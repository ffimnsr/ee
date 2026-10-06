//! Host-local workspace trust: workspace-layer `secret://` references and
//! always-allow persistent approvals.
//!
//! `secret://` references written in a workspace `.ee.toml` resolve only when
//! the user explicitly trusts that workspace (VS Code-style first-open
//! confirmation). The same decision gates always-allow approval candidates
//! and stored always-allow rules. The decision is a separate owner-only file
//! per canonical workspace under the ee state directory. It intentionally
//! mirrors the path, permission, identity, and atomic-write invariants of the
//! rule trust store in `policy::store` without extending that 1K-LOC document.
//!
//! Fail-closed rules:
//! - A missing decision is undecided and denies workspace references.
//! - Unreadable, malformed, wrong-identity, or unsafe decision files deny.
//! - Non-interactive startup never prompts and leaves the decision unrecorded.
//! - User-layer references are unaffected; they are user-owned config.

use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::config::EditorSettings;
use crate::policy::{TrustStoreError, WorkspaceIdentity};
use crate::secrets::resolve::{
    WorkspaceRefPolicy, WorkspaceSecretReference, workspace_secret_references,
};

/// Schema version of the workspace trust decision file.
pub(crate) const WORKSPACE_TRUST_SCHEMA_VERSION: u64 = 1;

const WORKSPACE_TRUST_DIR: &str = "workspace-trust";

/// Recorded workspace trust decision. `None` on disk means undecided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceTrustDecision {
    /// Workspace config may resolve its `secret://` references, and
    /// always-allow approvals may be offered, persisted, and matched.
    Trusted,
    /// Workspace config must never resolve `secret://` references, and
    /// always-allow approvals stay hidden while stored rules remain inert.
    Untrusted,
}

impl WorkspaceTrustDecision {
    fn as_str(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::Untrusted => "untrusted",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "trusted" => Some(Self::Trusted),
            "untrusted" => Some(Self::Untrusted),
            _ => None,
        }
    }
}

/// Owner-only decision store for one canonical workspace.
#[derive(Debug)]
pub(crate) struct WorkspaceTrustStore {
    path: PathBuf,
    workspace: WorkspaceIdentity,
}

impl WorkspaceTrustStore {
    /// Builds the store for `workspace_root` under `base_state_dir` (the `ee`
    /// state directory; the store lives in `<base>/workspace-trust/`).
    pub(crate) fn at(
        base_state_dir: &Path,
        workspace_root: &Path,
    ) -> Result<Self, TrustStoreError> {
        let workspace = canonical_workspace_identity(workspace_root)?;
        Ok(Self { path: decision_path_from(base_state_dir, &workspace), workspace })
    }

    /// The store under the platform state directory for `workspace_root`.
    pub(crate) fn default_for(workspace_root: &Path) -> Result<Self, TrustStoreError> {
        let state_dir = crate::logs::state_dir().ok_or(TrustStoreError::StateDirUnavailable)?;
        Self::at(&state_dir, workspace_root)
    }

    /// Exact host-local decision file; never inside the repository.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
    pub(crate) fn workspace(&self) -> &WorkspaceIdentity {
        &self.workspace
    }

    /// Recorded decision; a missing file is `Ok(None)` (undecided). Every
    /// document-level problem stays a typed error so callers fail closed.
    pub(crate) fn decision(&self) -> Result<Option<WorkspaceTrustDecision>, TrustStoreError> {
        let bytes = match read_verified_decision(&self.path) {
            Ok(bytes) => bytes,
            Err(TrustStoreError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        parse_decision_document(&bytes, self.workspace).map(Some)
    }

    /// Atomically records a decision, creating the owner-only directory on
    /// first use.
    pub(crate) fn set_decision(
        &self,
        decision: WorkspaceTrustDecision,
    ) -> Result<(), TrustStoreError> {
        let text = serialize_decision_document(self.workspace, decision, SystemTime::now())?;
        parse_decision_document(text.as_bytes(), self.workspace)?;
        atomic_write(&self.path, text.as_bytes())
    }
}

/// Workspace reference policy for one launch, fail closed: only an explicit
/// `Trusted` decision allows workspace-layer references to resolve.
pub(crate) fn workspace_secret_policy(workspace_root: &Path) -> WorkspaceRefPolicy {
    let Ok(store) = WorkspaceTrustStore::default_for(workspace_root) else {
        return WorkspaceRefPolicy::Deny;
    };
    store_policy(&store)
}

/// Decision-store variant of [`workspace_secret_policy`] used by callers that
/// already hold a store (including App test injection).
pub(crate) fn store_policy(store: &WorkspaceTrustStore) -> WorkspaceRefPolicy {
    match store.decision() {
        Ok(Some(WorkspaceTrustDecision::Trusted)) => WorkspaceRefPolicy::Resolve,
        _ => WorkspaceRefPolicy::Deny,
    }
}

/// Startup gate: when the merged config contains workspace-layer secret
/// references and no decision was recorded yet, ask once whether the
/// workspace is trusted. Non-interactive runs never prompt; references stay
/// blocked until `ee do trust grant` records a decision.
pub(crate) fn ensure_workspace_secret_trust(
    config: &EditorSettings,
    workspace_root: &Path,
) -> Result<(), String> {
    if !config.agents.enabled {
        return Ok(());
    }
    let references = workspace_secret_references(config);
    if references.is_empty() {
        return Ok(());
    }
    let store =
        WorkspaceTrustStore::default_for(workspace_root).map_err(|error| error.to_string())?;
    match store.decision().map_err(|error| error.to_string())? {
        Some(WorkspaceTrustDecision::Trusted) => Ok(()),
        Some(WorkspaceTrustDecision::Untrusted) => Ok(()),
        None => {
            if !io::stdin().is_terminal() {
                return Err(String::from(
                    "workspace secret references are blocked until this workspace is trusted; \
                     run `ee do trust grant` from the workspace",
                ));
            }
            println!("{}", trust_prompt_text(workspace_root, &references));
            let trusted = confirm("Trust this workspace? [y/N]: ")?;
            let decision = if trusted {
                WorkspaceTrustDecision::Trusted
            } else {
                WorkspaceTrustDecision::Untrusted
            };
            store.set_decision(decision).map_err(|error| error.to_string())?;
            match decision {
                WorkspaceTrustDecision::Trusted => {
                    println!("Workspace trusted. Change this later with `ee do trust revoke`.")
                }
                WorkspaceTrustDecision::Untrusted => println!(
                    "Workspace untrusted: its secret references stay blocked; \
                     use `ee do trust grant` to allow them."
                ),
            }
            Ok(())
        }
    }
}

/// Bounded prompt text for the startup decision. Shows reference names only;
/// stored values never appear.
pub(crate) fn trust_prompt_text(
    workspace_root: &Path,
    references: &[WorkspaceSecretReference],
) -> String {
    const MAX_LISTED: usize = 8;
    let mut text = String::new();
    let _ = writeln!(text, "Workspace {} defines secret references:", workspace_root.display());
    for reference in references.iter().take(MAX_LISTED) {
        let _ = writeln!(text, "  {} -> {}", reference.location, reference.reference);
    }
    if references.len() > MAX_LISTED {
        let _ = writeln!(text, "  ... and {} more", references.len() - MAX_LISTED);
    }
    let _ = write!(
        text,
        "Stored secret values stay encrypted and host-bound; trusting this workspace \
         lets its config hand them to the agent processes and MCP servers it defines."
    );
    text
}

fn confirm(prompt: &str) -> Result<bool, String> {
    print!("{prompt}");
    io::stdout().flush().map_err(|error| format!("cannot write trust prompt: {error}"))?;
    let mut line = String::new();
    let read = io::stdin()
        .read_line(&mut line)
        .map_err(|error| format!("cannot read trust input: {error}"))?;
    if read == 0 {
        return Err(String::from("trust input closed"));
    }
    Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

// ── CLI ──────────────────────────────────────────────────────────────────────

fn workspace_root_for_cli() -> io::Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    Ok(crate::config::find_git_root(&cwd).unwrap_or(cwd))
}

pub(crate) fn cmd_trust_grant() -> io::Result<()> {
    set_current_workspace_decision(WorkspaceTrustDecision::Trusted, "granted")
}

pub(crate) fn cmd_trust_revoke() -> io::Result<()> {
    set_current_workspace_decision(WorkspaceTrustDecision::Untrusted, "revoked")
}

fn set_current_workspace_decision(decision: WorkspaceTrustDecision, verb: &str) -> io::Result<()> {
    let workspace = workspace_root_for_cli()?;
    let store = WorkspaceTrustStore::default_for(&workspace).map_err(io::Error::other)?;
    store.set_decision(decision).map_err(io::Error::other)?;
    println!("workspace trust {verb} for {}", workspace.display());
    println!("host-local decision file: {}", store.path().display());
    Ok(())
}

pub(crate) fn cmd_trust_status() -> io::Result<()> {
    let workspace = workspace_root_for_cli()?;
    let store = WorkspaceTrustStore::default_for(&workspace).map_err(io::Error::other)?;
    let decision = store.decision().map_err(io::Error::other)?;
    let label = match decision {
        Some(WorkspaceTrustDecision::Trusted) => "trusted",
        Some(WorkspaceTrustDecision::Untrusted) => "untrusted",
        None => "undecided",
    };
    println!("workspace: {}", workspace.display());
    println!("workspace trust: {label}");
    println!("host-local decision file: {}", store.path().display());
    if decision.is_none() {
        println!(
            "workspace secret references stay blocked until the workspace is trusted \
             (`ee do trust grant`)"
        );
    }
    Ok(())
}

// ── Paths and document I/O ───────────────────────────────────────────────────

/// Canonical workspace identity: `SHA-256("ee.workspace.v1\0" +
/// canonical_workspace_root_path_bytes)`; the root must resolve to a
/// directory.
fn canonical_workspace_identity(root: &Path) -> Result<WorkspaceIdentity, TrustStoreError> {
    let canonical = fs::canonicalize(root).map_err(TrustStoreError::Io)?;
    if !canonical.is_dir() {
        return Err(TrustStoreError::ValidationFailure("workspace root is not a directory".into()));
    }
    Ok(WorkspaceIdentity::from_canonical_root_bytes(canonical.as_os_str().as_encoded_bytes()))
}

/// `<base_state_dir>/workspace-trust/<workspace_hex_digest>.toml`; the raw
/// workspace path never appears in the filename.
fn decision_path_from(base_state_dir: &Path, workspace: &WorkspaceIdentity) -> PathBuf {
    base_state_dir.join(WORKSPACE_TRUST_DIR).join(format!("{}.toml", workspace.hex()))
}

fn serialize_decision_document(
    workspace: WorkspaceIdentity,
    decision: WorkspaceTrustDecision,
    decided_at: SystemTime,
) -> Result<String, TrustStoreError> {
    let datetime: chrono::DateTime<chrono::Utc> = decided_at.into();
    let mut text = String::new();
    let _ = writeln!(text, "schema_version = {WORKSPACE_TRUST_SCHEMA_VERSION}");
    let _ = writeln!(text, "\n[workspace]");
    let _ = writeln!(text, "identity = \"{}\"", workspace.as_string());
    let _ = writeln!(text, "\n[trust]");
    let _ = writeln!(text, "decision = \"{}\"", decision.as_str());
    let _ = writeln!(
        text,
        "decided_at = \"{}\"",
        datetime.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    );
    Ok(text)
}

fn parse_decision_document(
    bytes: &[u8],
    expected: WorkspaceIdentity,
) -> Result<WorkspaceTrustDecision, TrustStoreError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| TrustStoreError::ParseFailure("workspace trust file is not UTF-8".into()))?;
    let value = toml::from_str::<toml::Value>(text).map_err(|error| {
        TrustStoreError::ParseFailure(format!("invalid workspace trust TOML: {error}"))
    })?;
    let table = value.as_table().ok_or_else(|| {
        TrustStoreError::ParseFailure("workspace trust document must be a table".into())
    })?;
    for key in table.keys() {
        if !["schema_version", "workspace", "trust"].contains(&key.as_str()) {
            return Err(TrustStoreError::ValidationFailure(format!(
                "unknown workspace trust field: {key}"
            )));
        }
    }
    let version = table
        .get("schema_version")
        .and_then(toml::Value::as_integer)
        .ok_or_else(|| TrustStoreError::ValidationFailure("missing schema_version".into()))?;
    if version != WORKSPACE_TRUST_SCHEMA_VERSION as i64 {
        return Err(TrustStoreError::UnsupportedSchemaVersion(version.max(0) as u64));
    }
    let workspace_table = table
        .get("workspace")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| TrustStoreError::ValidationFailure("missing [workspace] table".into()))?;
    if workspace_table.len() != 1 || !workspace_table.contains_key("identity") {
        return Err(TrustStoreError::ValidationFailure(
            "[workspace] must contain exactly identity".into(),
        ));
    }
    let identity_text =
        workspace_table.get("identity").and_then(toml::Value::as_str).ok_or_else(|| {
            TrustStoreError::ValidationFailure("workspace.identity must be a string".into())
        })?;
    let identity = WorkspaceIdentity::parse(identity_text).map_err(|error| {
        TrustStoreError::ValidationFailure(format!("invalid workspace identity: {error}"))
    })?;
    if identity != expected {
        return Err(TrustStoreError::IdentityMismatch);
    }
    let trust_table = table
        .get("trust")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| TrustStoreError::ValidationFailure("missing [trust] table".into()))?;
    if trust_table.len() != 2
        || !trust_table.contains_key("decision")
        || !trust_table.contains_key("decided_at")
    {
        return Err(TrustStoreError::ValidationFailure(
            "[trust] must contain exactly decision and decided_at".into(),
        ));
    }
    let decision_text =
        trust_table.get("decision").and_then(toml::Value::as_str).ok_or_else(|| {
            TrustStoreError::ValidationFailure("trust.decision must be a string".into())
        })?;
    let decision = WorkspaceTrustDecision::parse(decision_text).ok_or_else(|| {
        TrustStoreError::ValidationFailure("trust.decision must be trusted or untrusted".into())
    })?;
    let decided_at =
        trust_table.get("decided_at").and_then(toml::Value::as_str).ok_or_else(|| {
            TrustStoreError::ValidationFailure("trust.decided_at must be a string".into())
        })?;
    if decided_at.is_empty() || decided_at.len() > 64 || decided_at.chars().any(char::is_control) {
        return Err(TrustStoreError::ValidationFailure(
            "trust.decided_at must be bounded and control-free".into(),
        ));
    }
    Ok(decision)
}

/// Owner-only read: rejects symlinks, non-regular files, unsafe modes, and
/// unsafe parent directories. Non-Unix platforms fail closed.
#[cfg(unix)]
fn read_verified_decision(path: &Path) -> Result<Vec<u8>, TrustStoreError> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file =
        OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path).map_err(
            |error| {
                if error.raw_os_error() == Some(libc::ELOOP) {
                    TrustStoreError::PermissionFailure(path.to_path_buf())
                } else {
                    TrustStoreError::Io(error)
                }
            },
        )?;
    let metadata = file.metadata().map_err(TrustStoreError::Io)?;
    verify_decision_metadata(path, &metadata)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(TrustStoreError::Io)?;
    Ok(bytes)
}

#[cfg(not(unix))]
fn read_verified_decision(_path: &Path) -> Result<Vec<u8>, TrustStoreError> {
    Err(TrustStoreError::PlatformAclUnsupported)
}

#[cfg(unix)]
fn verify_decision_metadata(path: &Path, meta: &fs::Metadata) -> Result<(), TrustStoreError> {
    use std::os::unix::fs::PermissionsExt;

    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(TrustStoreError::PermissionFailure(path.to_path_buf()));
    }
    if meta.permissions().mode() & 0o777 != 0o600 {
        return Err(TrustStoreError::PermissionFailure(path.to_path_buf()));
    }
    let Some(parent) = path.parent() else {
        return Err(TrustStoreError::PermissionFailure(path.to_path_buf()));
    };
    let parent_meta = fs::symlink_metadata(parent).map_err(TrustStoreError::Io)?;
    if parent_meta.file_type().is_symlink() || !parent_meta.is_dir() {
        return Err(TrustStoreError::PermissionFailure(parent.to_path_buf()));
    }
    if parent_meta.permissions().mode() & 0o777 != 0o700 {
        return Err(TrustStoreError::PermissionFailure(parent.to_path_buf()));
    }
    Ok(())
}

/// Creates the decision directory with mode `0700` on Unix (when newly
/// created) and rejects symlink or non-directory parents.
#[cfg(unix)]
fn ensure_decision_dir(dir: &Path) -> Result<(), TrustStoreError> {
    use std::os::unix::fs::PermissionsExt;
    match fs::symlink_metadata(dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(dir).map_err(TrustStoreError::Io)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
                .map_err(TrustStoreError::Io)?;
            Ok(())
        }
        Err(error) => Err(TrustStoreError::Io(error)),
        Ok(meta) => {
            if meta.file_type().is_symlink()
                || !meta.is_dir()
                || meta.permissions().mode() & 0o777 != 0o700
            {
                return Err(TrustStoreError::PermissionFailure(dir.to_path_buf()));
            }
            Ok(())
        }
    }
}

#[cfg(not(unix))]
fn ensure_decision_dir(_dir: &Path) -> Result<(), TrustStoreError> {
    Err(TrustStoreError::PlatformAclUnsupported)
}

#[cfg(unix)]
fn open_private_temp(path: &Path) -> Result<fs::File, TrustStoreError> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(TrustStoreError::WriteFailure)
}

#[cfg(not(unix))]
fn open_private_temp(_path: &Path) -> Result<fs::File, TrustStoreError> {
    Err(TrustStoreError::PlatformAclUnsupported)
}

/// Replaces `path` atomically: unique same-directory temp file with mode
/// `0600`, flush, rename, then best-effort parent flush. Failed writes remove
/// the temp file and leave any previous decision intact.
fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), TrustStoreError> {
    let dir = path.parent().ok_or_else(|| {
        TrustStoreError::WriteFailure(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace trust path has no parent directory",
        ))
    })?;
    ensure_decision_dir(dir)?;
    let temp = unique_temp_path(dir)?;
    let result = (|| -> Result<(), TrustStoreError> {
        let mut file = open_private_temp(&temp)?;
        file.write_all(contents).map_err(TrustStoreError::WriteFailure)?;
        file.sync_all().map_err(TrustStoreError::WriteFailure)?;
        drop(file);
        fs::rename(&temp, path).map_err(TrustStoreError::RenameFailure)?;
        sync_parent_dir(dir);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Unique sibling temporary file (pid plus OS-random bytes).
fn unique_temp_path(dir: &Path) -> Result<PathBuf, TrustStoreError> {
    use rand::TryRngCore as _;
    let mut bytes = [0u8; 8];
    rand::rngs::OsRng.try_fill_bytes(&mut bytes).map_err(|_| {
        TrustStoreError::WriteFailure(io::Error::other("entropy source unavailable"))
    })?;
    let random_hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(dir.join(format!(".workspace-trust.tmp-{}-{random_hex}", std::process::id())))
}

/// Best-effort directory flush so the rename is durable where supported.
#[cfg(unix)]
fn sync_parent_dir(dir: &Path) {
    if let Ok(dir_file) = fs::File::open(dir) {
        let _ = dir_file.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_parent_dir(_dir: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_root(base: &Path, name: &str) -> PathBuf {
        let root = base.join(name);
        fs::create_dir_all(&root).expect("workspace dir");
        root
    }

    fn store_for(base: &Path, root: &Path) -> WorkspaceTrustStore {
        WorkspaceTrustStore::at(base, root).expect("store")
    }

    #[test]
    fn decision_round_trips_and_missing_is_undecided() {
        let temp = tempfile::tempdir().unwrap();
        let root = workspace_root(temp.path(), "repo");
        let base = temp.path().join("state");
        let store = store_for(&base, &root);

        assert_eq!(store.decision().unwrap(), None, "missing file is undecided");

        store.set_decision(WorkspaceTrustDecision::Trusted).unwrap();
        assert_eq!(store.decision().unwrap(), Some(WorkspaceTrustDecision::Trusted));

        store.set_decision(WorkspaceTrustDecision::Untrusted).unwrap();
        assert_eq!(store.decision().unwrap(), Some(WorkspaceTrustDecision::Untrusted));
    }

    #[test]
    fn decision_file_and_directory_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = workspace_root(temp.path(), "repo");
        let base = temp.path().join("state");
        let store = store_for(&base, &root);
        store.set_decision(WorkspaceTrustDecision::Trusted).unwrap();

        let file_mode = fs::metadata(store.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600);
        let dir_mode =
            fs::metadata(store.path().parent().unwrap()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
    }

    #[test]
    fn identity_mismatch_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let root = workspace_root(temp.path(), "repo");
        let other = workspace_root(temp.path(), "other");
        let base = temp.path().join("state");
        let store = store_for(&base, &root);
        store.set_decision(WorkspaceTrustDecision::Trusted).unwrap();

        let foreign = WorkspaceTrustStore {
            path: store.path().to_path_buf(),
            workspace: *store_for(&base, &other).workspace(),
        };
        assert!(matches!(foreign.decision(), Err(TrustStoreError::IdentityMismatch)));
    }

    #[test]
    fn malformed_and_unknown_documents_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let root = workspace_root(temp.path(), "repo");
        let base = temp.path().join("state");
        let store = store_for(&base, &root);
        store.set_decision(WorkspaceTrustDecision::Trusted).unwrap();
        let path = store.path().to_path_buf();

        let identity = store.workspace().as_string();
        let cases = [
            format!(
                "schema_version = 9\n[workspace]\nidentity = \"{identity}\"\n[trust]\ndecision = \"trusted\"\ndecided_at = \"2026-01-01T00:00:00Z\"\n"
            ),
            format!(
                "schema_version = 1\nextra = true\n[workspace]\nidentity = \"{identity}\"\n[trust]\ndecision = \"trusted\"\ndecided_at = \"2026-01-01T00:00:00Z\"\n"
            ),
            format!(
                "schema_version = 1\n[workspace]\nidentity = \"{identity}\"\n[trust]\ndecision = \"maybe\"\ndecided_at = \"2026-01-01T00:00:00Z\"\n"
            ),
            format!(
                "schema_version = 1\n[workspace]\nidentity = \"{identity}\"\n[trust]\ndecision = \"trusted\"\n"
            ),
        ];
        for text in cases {
            fs::write(&path, text).unwrap();
            let error = store.decision().expect_err("invalid document must fail closed");
            assert!(
                matches!(
                    error,
                    TrustStoreError::ValidationFailure(_)
                        | TrustStoreError::UnsupportedSchemaVersion(_)
                ),
                "unexpected error: {error}"
            );
        }

        // The parse failures above must not have replaced the file.
        fs::write(
            &path,
            serialize_decision_document(
                *store.workspace(),
                WorkspaceTrustDecision::Untrusted,
                SystemTime::UNIX_EPOCH,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(store.decision().unwrap(), Some(WorkspaceTrustDecision::Untrusted));
    }

    #[test]
    fn policy_is_fail_closed_and_only_trust_opens_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = workspace_root(temp.path(), "repo");
        let base = temp.path().join("state");
        let store = store_for(&base, &root);

        // No decision file: deny.
        assert_eq!(store_policy(&store), WorkspaceRefPolicy::Deny);
        store.set_decision(WorkspaceTrustDecision::Trusted).unwrap();
        assert_eq!(store_policy(&store), WorkspaceRefPolicy::Resolve);
        store.set_decision(WorkspaceTrustDecision::Untrusted).unwrap();
        assert_eq!(store_policy(&store), WorkspaceRefPolicy::Deny);

        // A malformed decision file denies even when it was previously trusted.
        fs::write(store.path(), b"not a trust document").unwrap();
        assert_eq!(store_policy(&store), WorkspaceRefPolicy::Deny);
    }

    #[test]
    fn startup_gate_is_silent_without_enabled_workspace_references() {
        use crate::config::{AgentEnvValue, AgentServerSettings};
        use std::collections::BTreeMap;

        let temp = tempfile::tempdir().unwrap();
        let root = workspace_root(temp.path(), "repo");
        let mut config = EditorSettings::default();
        let mut env = BTreeMap::new();
        env.insert(
            String::from("OPENROUTER_API_KEY"),
            AgentEnvValue {
                layer: crate::config::ConfigLayerKind::Ancestor,
                raw: String::from("secret://alpha-key"),
            },
        );
        config.agents.servers.insert(
            String::from("alpha"),
            AgentServerSettings {
                label: None,
                command: String::from("unused"),
                args: Vec::new(),
                env,
                cwd: None,
            },
        );

        // Agents mode off: no trust prompt, no store access.
        assert!(!config.agents.enabled);
        assert!(ensure_workspace_secret_trust(&config, &root).is_ok());

        // Agents mode on but no workspace-layer references: still silent.
        config.agents.enabled = true;
        let server = config.agents.servers.get_mut("alpha").expect("server");
        server.env.insert(
            String::from("OPENROUTER_API_KEY"),
            AgentEnvValue {
                layer: crate::config::ConfigLayerKind::UserXdg,
                raw: String::from("secret://alpha-key"),
            },
        );
        assert!(ensure_workspace_secret_trust(&config, &root).is_ok());
    }

    #[test]
    fn prompt_text_lists_references_without_values() {
        let references = vec![
            WorkspaceSecretReference {
                location: String::from("agents.servers.alpha.env.OPENROUTER_API_KEY"),
                reference: String::from("secret://alpha-key"),
            },
            WorkspaceSecretReference {
                location: String::from("mcp.servers.beta.headers.Authorization"),
                reference: String::from("Bearer secret://beta-token"),
            },
        ];
        let text = trust_prompt_text(Path::new("/repo"), &references);
        assert!(text.contains("/repo"));
        assert!(text.contains("agents.servers.alpha.env.OPENROUTER_API_KEY -> secret://alpha-key"));
        assert!(
            text.contains("mcp.servers.beta.headers.Authorization -> Bearer secret://beta-token")
        );
        assert!(!text.contains("sk-"));
    }

    #[test]
    fn prompt_text_bounds_large_reference_sets() {
        let references = (0..12)
            .map(|index| WorkspaceSecretReference {
                location: format!("agents.servers.server{index}.env.KEY"),
                reference: format!("secret://key-{index}"),
            })
            .collect::<Vec<_>>();
        let text = trust_prompt_text(Path::new("/repo"), &references);
        assert!(text.contains("server7"));
        assert!(!text.contains("server8"));
        assert!(text.contains("... and 4 more"));
    }
}
