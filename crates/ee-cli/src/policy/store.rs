//! Host-local per-workspace trust store (Phase 1 foundation).
//!
//! Persistent grants live only here: a versioned TOML document under the
//! platform state directory (`$XDG_STATE_HOME/ee/trust/` on Linux), keyed by
//! the canonical workspace identity digest.  The raw workspace path never
//! appears in the filename or the document.  Repository configuration
//! (`ee.toml`, project/XDG/system config) and agent-provided files can never
//! create effective grants: a copied document or file for a different
//! workspace fails identity validation and loads no rules.
//!
//! On Unix the trust directory is created with mode `0700` and the document
//! and temporary files with mode `0600`; broader modes, symlinks,
//! directories, and non-regular files fail closed.  Non-Unix platforms fail
//! closed until owner-only ACL verification is implemented.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Serialize;

use super::rules::{
    RawCommandRule, RawMcpReadRule, RawMcpRule, RawProfileRule, RawReadPathRule, RawWriteRule,
};
use super::{TrustRule, WorkspaceIdentity};

/// Current trust-store schema version; unsupported versions load no
/// effective rules.
pub(crate) const TRUST_SCHEMA_VERSION: u64 = 1;

const TRUST_STATE_SUBDIR: &str = "trust";
const STORE_FILE_EXTENSION: &str = "toml";

/// Host-local store for one canonical workspace.
#[derive(Debug)]
pub(crate) struct TrustStore {
    path: PathBuf,
    workspace: WorkspaceIdentity,
}

/// One parsed/loadable trust-store document: schema version, hashed
/// workspace identity, the workspace gate, and typed rule arrays.  Runtime
/// usage counters are session-local and never part of the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrustStoreDocument {
    pub(crate) workspace: WorkspaceIdentity,
    pub(crate) workspace_enabled: bool,
    pub(crate) rules: Vec<TrustRule>,
}

/// Typed store failure.  Callers fail closed: any error means no effective
/// rules and a prompt.
#[derive(Debug)]
pub(crate) enum TrustStoreError {
    Io(io::Error),
    StateDirUnavailable,
    /// Document identity does not match the workspace the store is keyed by.
    IdentityMismatch,
    UnsupportedSchemaVersion(u64),
    ParseFailure(String),
    ValidationFailure(String),
    /// Unsafe mode, symlink, directory, or non-regular store path.
    PermissionFailure(PathBuf),
    WriteFailure(io::Error),
    RenameFailure(io::Error),
    /// Owner-only ACL verification unavailable on this platform.
    PlatformAclUnsupported,
}

impl std::fmt::Display for TrustStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrustStoreError::Io(error) => write!(formatter, "trust store I/O error: {error}"),
            TrustStoreError::StateDirUnavailable => {
                formatter.write_str("trust state directory unavailable")
            }
            TrustStoreError::IdentityMismatch => formatter.write_str(
                "trust store workspace identity does not match the current workspace",
            ),
            TrustStoreError::UnsupportedSchemaVersion(version) => {
                write!(formatter, "unsupported trust store schema version {version}")
            }
            TrustStoreError::ParseFailure(message) => {
                write!(formatter, "trust store parse failure: {message}")
            }
            TrustStoreError::ValidationFailure(message) => {
                write!(formatter, "trust store validation failure: {message}")
            }
            TrustStoreError::PermissionFailure(path) => {
                write!(formatter, "unsafe trust store path: {}", path.display())
            }
            TrustStoreError::WriteFailure(error) => {
                write!(formatter, "trust store write failure: {error}")
            }
            TrustStoreError::RenameFailure(error) => {
                write!(formatter, "trust store rename failure: {error}")
            }
            TrustStoreError::PlatformAclUnsupported => formatter.write_str(
                "persistent trust requires verified owner-only access control, unavailable on this platform",
            ),
        }
    }
}

impl std::error::Error for TrustStoreError {}

impl TrustStore {
    /// Builds the store for `workspace_root` under `base_state_dir` (the
    /// `ee` state directory; the store lives in `<base>/trust/`).
    ///
    /// The workspace root is canonicalized first; the store filename and
    /// document identity derive from the digest of the canonical root bytes.
    pub(crate) fn at(
        base_state_dir: &Path,
        workspace_root: &Path,
    ) -> Result<Self, TrustStoreError> {
        let workspace = canonical_workspace_identity(workspace_root)?;
        Ok(Self { path: store_path_from(base_state_dir, &workspace), workspace })
    }

    /// The store under the platform state directory for `workspace_root`.
    pub(crate) fn default_for(workspace_root: &Path) -> Result<Self, TrustStoreError> {
        let state_dir = crate::logs::state_dir().ok_or(TrustStoreError::StateDirUnavailable)?;
        Self::at(&state_dir, workspace_root)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn workspace(&self) -> &WorkspaceIdentity {
        &self.workspace
    }

    /// Strict load against the system clock.  A missing store is an empty
    /// document for this workspace; every document-level problem (identity
    /// mismatch, unsupported schema version, malformed structure, unsafe
    /// path) is a typed error.  Invalid rule entries are rejected
    /// individually — unique valid entries continue loading.
    pub(crate) fn load(&self) -> Result<TrustStoreDocument, TrustStoreError> {
        self.load_at(SystemTime::now())
    }

    /// [`Self::load`] with injected time (Phase 6): scope validation (past
    /// expiry, expiration beyond the maximum duration, use budget above the
    /// safety maximum) is evaluated against `now`, so tests are fully
    /// deterministic.  Expired rules remain stored on disk; they just never
    /// load into the effective set.
    pub(crate) fn load_at(&self, now: SystemTime) -> Result<TrustStoreDocument, TrustStoreError> {
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(TrustStoreDocument {
                    workspace: self.workspace,
                    workspace_enabled: false,
                    rules: Vec::new(),
                });
            }
            Err(error) => return Err(TrustStoreError::Io(error)),
        };
        verify_store_metadata(&self.path, &metadata)?;
        let bytes = fs::read(&self.path).map_err(TrustStoreError::Io)?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| TrustStoreError::ParseFailure("trust store is not valid UTF-8".into()))?;
        parse_document(text, self.workspace, now)
    }

    /// Fail-closed effective policy: any load problem yields an empty rule
    /// set with the workspace gate disabled, so every operation prompts.
    pub(crate) fn effective(&self) -> TrustStoreDocument {
        self.effective_at(SystemTime::now())
    }

    /// [`Self::effective`] with injected time (Phase 6).
    pub(crate) fn effective_at(&self, now: SystemTime) -> TrustStoreDocument {
        self.load_at(now).unwrap_or_else(|_| TrustStoreDocument {
            workspace: self.workspace,
            workspace_enabled: false,
            rules: Vec::new(),
        })
    }

    /// Atomically persists `document`, refusing documents bound to another
    /// workspace, rules scoped to another workspace, and documents with
    /// duplicate or empty rule ids.
    pub(crate) fn write(&self, document: &TrustStoreDocument) -> Result<(), TrustStoreError> {
        if document.workspace != self.workspace {
            return Err(TrustStoreError::IdentityMismatch);
        }
        let mut seen = BTreeSet::new();
        for rule in &document.rules {
            if rule.scope().workspace != self.workspace {
                return Err(TrustStoreError::IdentityMismatch);
            }
            if rule.id().is_empty() || !seen.insert(rule.id().to_string()) {
                return Err(TrustStoreError::ValidationFailure(
                    "duplicate or empty rule id".into(),
                ));
            }
        }
        let text = serialize_document(document)?;
        atomic_write(&self.path, text.as_bytes())
    }

    /// Append-or-reuse: a rule whose stable id already exists is not
    /// duplicated; a new id is appended and persisted atomically.
    pub(crate) fn add_rule(&self, rule: TrustRule) -> Result<TrustStoreDocument, TrustStoreError> {
        let mut document = self.load()?;
        if document.rules.iter().any(|existing| existing.id() == rule.id()) {
            return Ok(document);
        }
        document.rules.push(rule);
        self.write(&document)?;
        Ok(document)
    }
}

/// Rejects symlink, directory, group/world-writable, non-regular store
/// paths and unsafe trust directories; non-Unix platforms fail closed
/// until owner-only ACL verification exists.
fn verify_store_metadata(path: &Path, meta: &fs::Metadata) -> Result<(), TrustStoreError> {
    if meta.file_type().is_symlink() {
        return Err(TrustStoreError::PermissionFailure(path.to_path_buf()));
    }
    if !meta.is_file() {
        return Err(TrustStoreError::PermissionFailure(path.to_path_buf()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o777 != 0o600 {
            return Err(TrustStoreError::PermissionFailure(path.to_path_buf()));
        }
    }
    #[cfg(not(unix))]
    {
        return Err(TrustStoreError::PlatformAclUnsupported);
    }
    let Some(parent) = path.parent() else {
        return Err(TrustStoreError::PermissionFailure(path.to_path_buf()));
    };
    let parent_meta = fs::symlink_metadata(parent).map_err(TrustStoreError::Io)?;
    if parent_meta.file_type().is_symlink() || !parent_meta.is_dir() {
        return Err(TrustStoreError::PermissionFailure(parent.to_path_buf()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if parent_meta.permissions().mode() & 0o777 != 0o700 {
            return Err(TrustStoreError::PermissionFailure(parent.to_path_buf()));
        }
    }
    Ok(())
}

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

/// `<base_state_dir>/trust/<workspace_hex_digest>.toml`; the raw workspace
/// path never appears in the filename.
pub(crate) fn store_path_from(base_state_dir: &Path, workspace: &WorkspaceIdentity) -> PathBuf {
    base_state_dir
        .join(TRUST_STATE_SUBDIR)
        .join(format!("{}.{STORE_FILE_EXTENSION}", workspace.hex()))
}

// ── Parsing ──────────────────────────────────────────────────────────────────

const DOCUMENT_KEYS: [&str; 9] = [
    "schema_version",
    "workspace",
    "policy",
    "command_allow",
    "mcp_allow",
    "read_path_allow",
    "mcp_read_allow",
    "profile_allow",
    "write_allow",
];

fn parse_document(
    text: &str,
    expected: WorkspaceIdentity,
    now: SystemTime,
) -> Result<TrustStoreDocument, TrustStoreError> {
    let value: toml::Value = toml::from_str(text).map_err(|error| {
        TrustStoreError::ParseFailure(format!("invalid trust store TOML: {error}"))
    })?;
    let table = value
        .as_table()
        .ok_or_else(|| TrustStoreError::ParseFailure("trust store must be a TOML table".into()))?;

    for key in table.keys() {
        if !DOCUMENT_KEYS.contains(&key.as_str()) {
            return Err(TrustStoreError::ValidationFailure(format!(
                "unknown document field: {key}"
            )));
        }
    }

    let version = table
        .get("schema_version")
        .and_then(toml::Value::as_integer)
        .ok_or_else(|| TrustStoreError::ValidationFailure("missing schema_version".into()))?;
    if version != TRUST_SCHEMA_VERSION as i64 {
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

    let policy_table = table
        .get("policy")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| TrustStoreError::ValidationFailure("missing [policy] table".into()))?;
    if policy_table.len() != 1 || !policy_table.contains_key("workspace_enabled") {
        return Err(TrustStoreError::ValidationFailure(
            "[policy] must contain exactly workspace_enabled".into(),
        ));
    }
    let workspace_enabled =
        policy_table.get("workspace_enabled").and_then(toml::Value::as_bool).ok_or_else(|| {
            TrustStoreError::ValidationFailure("policy.workspace_enabled must be a boolean".into())
        })?;

    let mut rules = Vec::new();
    for entry in rule_entries(table, "command_allow")? {
        if let Ok(raw) = entry.clone().try_into::<RawCommandRule>()
            && let Ok(rule) = super::rules::CommandRule::from_raw(raw, identity)
        {
            rules.push(TrustRule::Command(rule));
        }
    }
    for entry in rule_entries(table, "mcp_allow")? {
        if let Ok(raw) = entry.clone().try_into::<RawMcpRule>()
            && let Ok(rule) = super::rules::McpRule::from_raw(raw, identity)
        {
            rules.push(TrustRule::Mcp(rule));
        }
    }
    for entry in rule_entries(table, "read_path_allow")? {
        if let Ok(raw) = entry.clone().try_into::<RawReadPathRule>()
            && let Ok(rule) = super::rules::ReadPathRule::from_raw(raw, identity)
        {
            rules.push(TrustRule::ReadPath(rule));
        }
    }
    for entry in rule_entries(table, "mcp_read_allow")? {
        if let Ok(raw) = entry.clone().try_into::<RawMcpReadRule>()
            && let Ok(rule) = super::rules::McpReadRule::from_raw(raw, identity)
        {
            rules.push(TrustRule::McpRead(rule));
        }
    }
    for entry in rule_entries(table, "profile_allow")? {
        if let Ok(raw) = entry.clone().try_into::<RawProfileRule>()
            && let Ok(rule) = super::rules::ProfileRule::from_raw(raw, identity)
        {
            rules.push(TrustRule::Profile(rule));
        }
    }
    for entry in rule_entries(table, "write_allow")? {
        if let Ok(raw) = entry.clone().try_into::<RawWriteRule>()
            && let Ok(rule) = super::rules::WriteRule::from_raw(raw, identity)
        {
            rules.push(TrustRule::Write(rule));
        }
    }

    // Conflicting duplicate ids invalidate every conflicting entry; unique
    // valid entries continue loading.
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for rule in &rules {
        *counts.entry(rule.id().to_string()).or_default() += 1;
    }
    rules.retain(|rule| counts.get(rule.id()) == Some(&1));

    // Phase 6 lifecycle: invalid persisted scope never loads.  Past expiry,
    // expiration beyond the maximum duration, and use budgets above the
    // safety maximum are rejected; the entries stay on disk but never become
    // effective authority.
    rules.retain(|rule| scope_valid(rule, now));

    Ok(TrustStoreDocument { workspace: expected, workspace_enabled, rules })
}

/// Whether one rule's persisted scope is valid at `now` (Phase 6).
fn scope_valid(rule: &TrustRule, now: SystemTime) -> bool {
    let scope = rule.scope();
    match scope.expires_at {
        Some(expires) if expires <= now => return false, // past expiry
        Some(expires) => {
            let remaining = expires.duration_since(now);
            if remaining.map(|duration| duration > super::rules::MAX_RULE_DURATION).unwrap_or(true)
            {
                return false; // beyond the maximum grant duration
            }
        }
        None => {}
    }
    if scope.max_uses.is_some_and(|uses| uses > super::rules::MAX_RULE_MAX_USES) {
        return false;
    }
    true
}

fn rule_entries<'a>(
    table: &'a toml::Table,
    key: &str,
) -> Result<&'a [toml::Value], TrustStoreError> {
    match table.get(key) {
        None => Ok(&[]),
        Some(toml::Value::Array(entries)) => Ok(entries),
        Some(_) => {
            Err(TrustStoreError::ValidationFailure(format!("{key} must be an array of tables")))
        }
    }
}

// ── Serialization (canonical schema order) ───────────────────────────────────

#[derive(Serialize)]
struct RawDocument {
    schema_version: u64,
    workspace: RawWorkspace,
    policy: RawPolicy,
    #[serde(rename = "command_allow", skip_serializing_if = "Vec::is_empty")]
    command_allow: Vec<RawCommandRule>,
    #[serde(rename = "mcp_allow", skip_serializing_if = "Vec::is_empty")]
    mcp_allow: Vec<RawMcpRule>,
    #[serde(rename = "read_path_allow", skip_serializing_if = "Vec::is_empty")]
    read_path_allow: Vec<RawReadPathRule>,
    #[serde(rename = "mcp_read_allow", skip_serializing_if = "Vec::is_empty")]
    mcp_read_allow: Vec<RawMcpReadRule>,
    #[serde(rename = "profile_allow", skip_serializing_if = "Vec::is_empty")]
    profile_allow: Vec<RawProfileRule>,
    #[serde(rename = "write_allow", skip_serializing_if = "Vec::is_empty")]
    write_allow: Vec<RawWriteRule>,
}

#[derive(Serialize)]
struct RawWorkspace {
    identity: String,
}

#[derive(Serialize)]
struct RawPolicy {
    workspace_enabled: bool,
}

fn serialize_document(document: &TrustStoreDocument) -> Result<String, TrustStoreError> {
    let mut command_allow: Vec<RawCommandRule> = Vec::new();
    let mut mcp_allow: Vec<RawMcpRule> = Vec::new();
    let mut read_path_allow: Vec<RawReadPathRule> = Vec::new();
    let mut mcp_read_allow: Vec<RawMcpReadRule> = Vec::new();
    let mut profile_allow: Vec<RawProfileRule> = Vec::new();
    let mut write_allow: Vec<RawWriteRule> = Vec::new();
    for rule in &document.rules {
        match rule {
            TrustRule::Command(rule) => command_allow.push(rule.into()),
            TrustRule::Mcp(rule) => mcp_allow.push(rule.into()),
            TrustRule::ReadPath(rule) => read_path_allow.push(rule.into()),
            TrustRule::McpRead(rule) => mcp_read_allow.push(rule.into()),
            TrustRule::Profile(rule) => profile_allow.push(rule.into()),
            TrustRule::Write(rule) => write_allow.push(rule.into()),
        }
    }
    command_allow.sort_by(|a, b| a.id.cmp(&b.id));
    mcp_allow.sort_by(|a, b| a.id.cmp(&b.id));
    read_path_allow.sort_by(|a, b| a.id.cmp(&b.id));
    mcp_read_allow.sort_by(|a, b| a.id.cmp(&b.id));
    profile_allow.sort_by(|a, b| a.id.cmp(&b.id));
    write_allow.sort_by(|a, b| a.id.cmp(&b.id));
    let raw = RawDocument {
        schema_version: TRUST_SCHEMA_VERSION,
        workspace: RawWorkspace { identity: document.workspace.as_string() },
        policy: RawPolicy { workspace_enabled: document.workspace_enabled },
        command_allow,
        mcp_allow,
        read_path_allow,
        mcp_read_allow,
        profile_allow,
        write_allow,
    };
    toml::to_string(&raw).map_err(|error| {
        TrustStoreError::ValidationFailure(format!("cannot serialize trust store: {error}"))
    })
}

// ── Atomic persistence ───────────────────────────────────────────────────────

/// Creates the trust directory with mode `0700` on Unix (when newly
/// created) and rejects symlink or non-directory parents.
#[cfg(unix)]
fn ensure_trust_dir(dir: &Path) -> Result<(), TrustStoreError> {
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
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(TrustStoreError::PermissionFailure(dir.to_path_buf()));
            }
            Ok(())
        }
    }
}

#[cfg(not(unix))]
fn ensure_trust_dir(_dir: &Path) -> Result<(), TrustStoreError> {
    Err(TrustStoreError::PlatformAclUnsupported)
}

/// Unique sibling temporary file (pid plus OS-random bytes).
fn unique_temp_path(dir: &Path) -> Result<PathBuf, TrustStoreError> {
    use rand::TryRngCore as _;
    let mut bytes = [0u8; 8];
    rand::rngs::OsRng.try_fill_bytes(&mut bytes).map_err(|_| {
        TrustStoreError::WriteFailure(io::Error::other("entropy source unavailable"))
    })?;
    let random_hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(dir.join(format!(".trust.tmp-{}-{random_hex}", std::process::id())))
}

/// Opens the temporary file with mode `0600` on Unix.
#[cfg(unix)]
fn open_private_temp(path: &Path) -> Result<fs::File, TrustStoreError> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(TrustStoreError::WriteFailure)
}

#[cfg(not(unix))]
fn open_private_temp(_path: &Path) -> Result<fs::File, TrustStoreError> {
    Err(TrustStoreError::PlatformAclUnsupported)
}

/// Replaces `path` atomically: unique same-directory temp file with mode
/// `0600`, flush, rename, then best-effort parent flush.  Failed writes
/// remove the temp file and leave any previous store intact.
fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), TrustStoreError> {
    let dir = path.parent().ok_or_else(|| {
        TrustStoreError::WriteFailure(io::Error::new(
            io::ErrorKind::InvalidInput,
            "store path has no parent directory",
        ))
    })?;
    ensure_trust_dir(dir)?;
    let temp = unique_temp_path(dir)?;

    let result = (|| -> Result<(), TrustStoreError> {
        let mut file = open_private_temp(&temp)?;
        file.write_all(contents).map_err(TrustStoreError::WriteFailure)?;
        file.sync_all().map_err(TrustStoreError::WriteFailure)?;
        drop(file);
        fs::rename(&temp, path).map_err(TrustStoreError::RenameFailure)?;
        #[cfg(unix)]
        enforce_private_file_mode(path)?;
        sync_parent_dir(dir);
        Ok(())
    })();
    if result.is_err() {
        // Best-effort cleanup: never leave a partial replacement behind.
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Re-asserts owner-only file mode after replacement.
#[cfg(unix)]
fn enforce_private_file_mode(path: &Path) -> Result<(), TrustStoreError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(TrustStoreError::Io)
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
