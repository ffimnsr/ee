//! Host-local MCP server trust store and CLI.
//!
//! The store is a sibling of the workspace trust decision and rule store under
//! the platform state directory:
//! `<state>/mcp-trust/<workspace_hex_digest>.toml`.  It never lives inside the
//! repository, and every grant is pinned to the canonical workspace identity.
//!
//! Fail-closed rules mirror the existing trust stores:
//! - missing file is an empty grant set,
//! - unreadable, malformed, wrong-identity, symlinked, or non-owner-only files
//!   yield no effective grants,
//! - a grant is inert unless the workspace trust decision is `trusted`,
//! - unknown fields, duplicate grants, duplicate tools, non-manifest execute
//!   pins, network/unknown categories, and empty tool lists are rejected at
//!   load.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use crate::args::McpTrustTransport;
use crate::policy::{
    McpServerGrant, McpServerToolGrant, McpServerTrust, TrustCategory, TrustStoreError,
    WorkspaceIdentity, generate_mcp_server_grant_id, pinned_execution_tools, pinned_server_tools,
};
use crate::workspace_trust::{WorkspaceTrustDecision, WorkspaceTrustStore};

/// Schema version of the MCP server trust document.
pub(crate) const MCP_TRUST_SCHEMA_VERSION: u64 = 1;

const MCP_TRUST_DIR: &str = "mcp-trust";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDocument {
    schema_version: u64,
    workspace: RawWorkspace,
    #[serde(default)]
    servers: Vec<RawGrant>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkspace {
    identity: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGrant {
    id: String,
    server: String,
    transport_identity: String,
    granted_at: String,
    tools: Vec<RawTool>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTool {
    tool: String,
    category: TrustCategory,
    tool_schema_version: u64,
}

/// Owner-only MCP server trust store for one canonical workspace.
#[derive(Debug)]
pub(crate) struct McpTrustStore {
    path: PathBuf,
    workspace: WorkspaceIdentity,
}

impl McpTrustStore {
    pub(crate) fn at(
        base_state_dir: &Path,
        workspace_root: &Path,
    ) -> Result<Self, TrustStoreError> {
        let workspace = canonical_workspace_identity(workspace_root)?;
        Ok(Self { path: trust_path_from(base_state_dir, &workspace), workspace })
    }

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

    /// Effective grants. A missing document is an empty grant set; every
    /// document-level problem is an error so callers fail closed.
    pub(crate) fn load(&self) -> Result<McpServerTrust, TrustStoreError> {
        let bytes = match read_verified_document(&self.path) {
            Ok(bytes) => bytes,
            Err(TrustStoreError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(McpServerTrust::empty());
            }
            Err(error) => return Err(error),
        };
        parse_document(&bytes, self.workspace)
    }

    /// Atomically replaces the document.
    pub(crate) fn save(&self, trust: &McpServerTrust) -> Result<(), TrustStoreError> {
        let text = serialize_document(self.workspace, trust)?;
        parse_document(text.as_bytes(), self.workspace)?;
        atomic_write(&self.path, text.as_bytes())
    }

    /// Adds one grant, replacing the previous grant for the same server and
    /// transport. The workspace identity is checked before anything is written.
    pub(crate) fn add_grant(&self, grant: McpServerGrant) -> Result<(), TrustStoreError> {
        if grant.workspace != self.workspace {
            return Err(TrustStoreError::IdentityMismatch);
        }
        let mut trust = self.load()?;
        let mut grants = trust.grants().to_vec();
        let mut grant = grant;
        if let Some(existing) = grants.iter().find(|existing| {
            existing.server == grant.server
                && existing.transport_identity == grant.transport_identity
        }) {
            grant.preserve_execute_pins(existing);
        }
        grants.retain(|existing| {
            !(existing.server == grant.server
                && existing.transport_identity == grant.transport_identity)
        });
        grants.push(grant);
        trust = McpServerTrust::new(grants);
        self.save(&trust)
    }

    /// Removes matching grants and returns the number removed.
    pub(crate) fn revoke(
        &self,
        server: &str,
        transport_identity: Option<&str>,
    ) -> Result<usize, TrustStoreError> {
        let trust = self.load()?;
        let before = trust.grants().len();
        let grants = trust
            .grants()
            .iter()
            .filter(|grant| {
                grant.server != server
                    || transport_identity
                        .is_some_and(|transport| grant.transport_identity != transport)
            })
            .cloned()
            .collect::<Vec<_>>();
        let removed = before.saturating_sub(grants.len());
        if removed > 0 {
            self.save(&McpServerTrust::new(grants))?;
        }
        Ok(removed)
    }

    /// Removes one grant by stable id and returns how many were removed.
    pub(crate) fn revoke_id(&self, id: &str) -> Result<usize, TrustStoreError> {
        let trust = self.load()?;
        let before = trust.grants().len();
        let grants =
            trust.grants().iter().filter(|grant| grant.id != id).cloned().collect::<Vec<_>>();
        let removed = before.saturating_sub(grants.len());
        if removed > 0 {
            self.save(&McpServerTrust::new(grants))?;
        }
        Ok(removed)
    }

    /// Removes terminal-execution pins for one server transport, preserving
    /// the rest of the grant. Returns how many pins were removed.
    pub(crate) fn revoke_execute(
        &self,
        server: &str,
        transport_identity: &str,
    ) -> Result<usize, TrustStoreError> {
        let trust = self.load()?;
        let mut grants = trust.grants().to_vec();
        let mut removed = 0usize;
        for grant in grants.iter_mut().filter(|grant| {
            grant.server == server && grant.transport_identity == transport_identity
        }) {
            let before = grant.tools.len();
            grant.tools.retain(|tool| tool.category != TrustCategory::Execute);
            removed = removed.saturating_add(before.saturating_sub(grant.tools.len()));
        }
        if removed > 0 {
            grants.retain(|grant| !grant.tools.is_empty());
            self.save(&McpServerTrust::new(grants))?;
        }
        Ok(removed)
    }

    /// Removes every grant document for this workspace. Used by explicit
    /// `/permissions reset confirm`.
    pub(crate) fn clear(&self) -> Result<(), TrustStoreError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(TrustStoreError::Io(error)),
        }
    }
}

fn canonical_workspace_identity(root: &Path) -> Result<WorkspaceIdentity, TrustStoreError> {
    let canonical = fs::canonicalize(root).map_err(TrustStoreError::Io)?;
    if !canonical.is_dir() {
        return Err(TrustStoreError::ValidationFailure("workspace root is not a directory".into()));
    }
    Ok(WorkspaceIdentity::from_canonical_root_bytes(canonical.as_os_str().as_encoded_bytes()))
}

fn trust_path_from(base_state_dir: &Path, workspace: &WorkspaceIdentity) -> PathBuf {
    base_state_dir.join(MCP_TRUST_DIR).join(format!("{}.toml", workspace.hex()))
}

fn serialize_document(
    workspace: WorkspaceIdentity,
    trust: &McpServerTrust,
) -> Result<String, TrustStoreError> {
    let document = RawDocument {
        schema_version: MCP_TRUST_SCHEMA_VERSION,
        workspace: RawWorkspace { identity: workspace.as_string() },
        servers: trust
            .grants()
            .iter()
            .map(|grant| RawGrant {
                id: grant.id.clone(),
                server: grant.server.clone(),
                transport_identity: grant.transport_identity.clone(),
                granted_at: grant.granted_at.clone(),
                tools: grant
                    .tools
                    .iter()
                    .map(|tool| RawTool {
                        tool: tool.tool.clone(),
                        category: tool.category,
                        tool_schema_version: tool.tool_schema_version,
                    })
                    .collect(),
            })
            .collect(),
    };
    toml::to_string(&document).map_err(|error| {
        TrustStoreError::ValidationFailure(format!("cannot serialize mcp trust document: {error}"))
    })
}

fn parse_document(
    bytes: &[u8],
    expected: WorkspaceIdentity,
) -> Result<McpServerTrust, TrustStoreError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| TrustStoreError::ParseFailure("mcp trust file is not UTF-8".into()))?;
    let document = toml::from_str::<RawDocument>(text).map_err(|error| {
        TrustStoreError::ParseFailure(format!("invalid mcp trust TOML: {error}"))
    })?;
    if document.schema_version != MCP_TRUST_SCHEMA_VERSION {
        return Err(TrustStoreError::UnsupportedSchemaVersion(document.schema_version));
    }
    let workspace = WorkspaceIdentity::parse(&document.workspace.identity).map_err(|error| {
        TrustStoreError::ValidationFailure(format!("invalid workspace identity: {error}"))
    })?;
    if workspace != expected {
        return Err(TrustStoreError::IdentityMismatch);
    }

    let mut grants = Vec::with_capacity(document.servers.len());
    let mut seen = std::collections::BTreeSet::new();
    let mut seen_ids = std::collections::BTreeSet::new();
    for raw in document.servers {
        if !seen.insert((raw.server.clone(), raw.transport_identity.clone())) {
            return Err(TrustStoreError::ValidationFailure(
                "duplicate mcp server grant for one transport".into(),
            ));
        }
        if !seen_ids.insert(raw.id.clone()) {
            return Err(TrustStoreError::ValidationFailure("duplicate mcp server grant id".into()));
        }
        let tools = raw
            .tools
            .into_iter()
            .map(|tool| McpServerToolGrant {
                tool: tool.tool,
                category: tool.category,
                tool_schema_version: tool.tool_schema_version,
            })
            .collect::<Vec<_>>();
        grants.push(
            McpServerGrant::new(
                raw.id,
                workspace,
                raw.server,
                raw.transport_identity,
                raw.granted_at,
                tools,
            )
            .map_err(TrustStoreError::ValidationFailure)?,
        );
    }
    Ok(McpServerTrust::new(grants))
}

// ── CLI ──────────────────────────────────────────────────────────────────────

pub(crate) fn cmd_mcp_trust_grant(
    server: &str,
    transport: McpTrustTransport,
    execute: bool,
) -> io::Result<()> {
    let workspace = crate::workspace_trust::workspace_root_for_cli()?;
    require_trusted_workspace(&workspace)?;
    if server != "ee" {
        return Err(io::Error::other(
            "only the built-in ee MCP server has an application-owned pinned tool manifest",
        ));
    }
    let store = McpTrustStore::default_for(&workspace).map_err(io::Error::other)?;
    let existing = store.load().map_err(io::Error::other)?;
    for tool_transport in transport.selected() {
        let identity = transport_identity(tool_transport);
        let id = existing
            .find(server, identity)
            .map(|grant| grant.id.clone())
            .unwrap_or_else(generate_mcp_server_grant_id);
        let mut tools = pinned_server_tools(tool_transport);
        if execute {
            tools.extend(pinned_execution_tools(tool_transport));
            tools.sort_by(|left, right| left.tool.cmp(&right.tool));
            tools.dedup_by(|left, right| left.tool == right.tool);
        }
        let execute_granted = execute
            || existing
                .find(server, identity)
                .is_some_and(crate::policy::McpServerGrant::includes_execution);
        let grant = McpServerGrant::new(
            id,
            *store.workspace(),
            server.to_string(),
            identity.to_string(),
            crate::policy::mcp_server_trust::granted_at_now(),
            tools,
        )
        .map_err(io::Error::other)?;
        store.add_grant(grant).map_err(io::Error::other)?;
        println!(
            "mcp server trust granted: server={server} transport={identity} tools={} execute={execute_granted}",
            pinned_server_tools(tool_transport).len()
                + usize::from(execute_granted) * pinned_execution_tools(tool_transport).len()
        );
    }
    println!("host-local store: {}", store.path().display());
    Ok(())
}

pub(crate) fn cmd_mcp_trust_revoke(
    server: &str,
    transport: McpTrustTransport,
    execute: bool,
) -> io::Result<()> {
    let workspace = crate::workspace_trust::workspace_root_for_cli()?;
    let store = McpTrustStore::default_for(&workspace).map_err(io::Error::other)?;
    let mut removed = 0usize;
    for tool_transport in transport.selected() {
        if execute {
            removed += store
                .revoke_execute(server, transport_identity(tool_transport))
                .map_err(io::Error::other)?;
        } else {
            removed += store
                .revoke(server, Some(transport_identity(tool_transport)))
                .map_err(io::Error::other)?;
        }
    }
    let scope = if execute { "terminal execution" } else { "grants" };
    println!("mcp server trust revoked: server={server} {scope}={removed}");
    Ok(())
}

pub(crate) fn cmd_mcp_trust_status(server: Option<&str>) -> io::Result<()> {
    let workspace = crate::workspace_trust::workspace_root_for_cli()?;
    let decision = WorkspaceTrustStore::default_for(&workspace)
        .map_err(io::Error::other)?
        .decision()
        .map_err(io::Error::other)?;
    let store = McpTrustStore::default_for(&workspace).map_err(io::Error::other)?;
    let trust = store.load().map_err(io::Error::other)?;
    let label = match decision {
        Some(WorkspaceTrustDecision::Trusted) => "trusted",
        Some(WorkspaceTrustDecision::Untrusted) => "untrusted",
        None => "undecided",
    };
    println!("workspace: {}", workspace.display());
    println!("workspace trust: {label}");
    println!("mcp trust: {} grant(s)", trust.grants().len());
    for grant in trust.grants() {
        if server.is_some_and(|filter| filter != grant.server) {
            continue;
        }
        println!(
            "  {} server={} transport={} tools={} execute={} granted_at={}",
            grant.id,
            grant.server,
            grant.transport_identity,
            grant.tool_count(),
            grant.includes_execution(),
            grant.granted_at
        );
        println!(
            "    tools: {}",
            grant.tools.iter().map(|tool| tool.tool.as_str()).collect::<Vec<_>>().join(", ")
        );
    }
    println!("host-local store: {}", store.path().display());
    if decision != Some(WorkspaceTrustDecision::Trusted) {
        println!("server grants are inert until the workspace is trusted (`ee do trust grant`)");
    }
    Ok(())
}

fn require_trusted_workspace(workspace: &Path) -> io::Result<()> {
    let decision = WorkspaceTrustStore::default_for(workspace)
        .map_err(io::Error::other)?
        .decision()
        .map_err(io::Error::other)?;
    match decision {
        Some(WorkspaceTrustDecision::Trusted) => Ok(()),
        Some(WorkspaceTrustDecision::Untrusted) => Err(io::Error::other(
            "workspace is untrusted; run `ee do trust grant` before granting MCP server trust",
        )),
        None => Err(io::Error::other(
            "workspace trust is undecided; run `ee do trust grant` before granting MCP server trust",
        )),
    }
}

fn transport_identity(transport: ee_mcp::ToolTransport) -> &'static str {
    match transport {
        ee_mcp::ToolTransport::Stdio => "stdio:ee --mcp-proxy",
        ee_mcp::ToolTransport::Acp => "acp:ee",
    }
}

impl McpTrustTransport {
    pub(crate) fn selected(self) -> Vec<ee_mcp::ToolTransport> {
        match self {
            Self::Stdio => vec![ee_mcp::ToolTransport::Stdio],
            Self::Acp => vec![ee_mcp::ToolTransport::Acp],
            Self::All => vec![ee_mcp::ToolTransport::Stdio, ee_mcp::ToolTransport::Acp],
        }
    }
}

// ── Owner-only document I/O ──────────────────────────────────────────────────

#[cfg(unix)]
fn read_verified_document(path: &Path) -> Result<Vec<u8>, TrustStoreError> {
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
    verify_document_metadata(path, &metadata)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(TrustStoreError::Io)?;
    Ok(bytes)
}

#[cfg(not(unix))]
fn read_verified_document(_path: &Path) -> Result<Vec<u8>, TrustStoreError> {
    Err(TrustStoreError::PlatformAclUnsupported)
}

#[cfg(unix)]
fn verify_document_metadata(path: &Path, meta: &fs::Metadata) -> Result<(), TrustStoreError> {
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

#[cfg(unix)]
fn ensure_document_dir(dir: &Path) -> Result<(), TrustStoreError> {
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
fn ensure_document_dir(_dir: &Path) -> Result<(), TrustStoreError> {
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

fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), TrustStoreError> {
    let dir = path.parent().ok_or_else(|| {
        TrustStoreError::WriteFailure(io::Error::new(
            io::ErrorKind::InvalidInput,
            "mcp trust path has no parent directory",
        ))
    })?;
    ensure_document_dir(dir)?;
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

fn unique_temp_path(dir: &Path) -> Result<PathBuf, TrustStoreError> {
    use rand::TryRngCore as _;
    let mut bytes = [0u8; 8];
    rand::rngs::OsRng.try_fill_bytes(&mut bytes).map_err(|_| {
        TrustStoreError::WriteFailure(io::Error::other("entropy source unavailable"))
    })?;
    let random_hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(dir.join(format!(".mcp-trust.tmp-{}-{random_hex}", std::process::id())))
}

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

    fn grant(workspace: WorkspaceIdentity, transport: &str) -> McpServerGrant {
        let tool_transport = match transport {
            "acp:ee" => ee_mcp::ToolTransport::Acp,
            _ => ee_mcp::ToolTransport::Stdio,
        };
        McpServerGrant::new(
            generate_mcp_server_grant_id(),
            workspace,
            "ee".to_string(),
            transport.to_string(),
            crate::policy::mcp_server_trust::granted_at_now(),
            pinned_server_tools(tool_transport),
        )
        .unwrap()
    }

    #[test]
    fn round_trip_and_missing_document_is_empty() {
        let temp = tempfile::tempdir().unwrap();
        let root = workspace_root(temp.path(), "repo");
        let base = temp.path().join("state");
        let store = McpTrustStore::at(&base, &root).unwrap();
        assert!(store.load().unwrap().is_empty());

        let first = grant(*store.workspace(), "stdio:ee --mcp-proxy");
        store.add_grant(first).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.grants().len(), 1);
        assert_eq!(loaded.grants()[0].server, "ee");
        assert_eq!(loaded.grants()[0].transport_identity, "stdio:ee --mcp-proxy");
        assert!(!loaded.grants()[0].tools.is_empty());
    }

    #[test]
    fn grant_file_and_directory_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = workspace_root(temp.path(), "repo");
        let base = temp.path().join("state");
        let store = McpTrustStore::at(&base, &root).unwrap();
        store.add_grant(grant(*store.workspace(), "stdio:ee --mcp-proxy")).unwrap();
        let file_mode = fs::metadata(store.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600);
        let dir_mode =
            fs::metadata(store.path().parent().unwrap()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
    }

    #[test]
    fn identity_mismatch_and_malformed_documents_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let root = workspace_root(temp.path(), "repo");
        let other = workspace_root(temp.path(), "other");
        let base = temp.path().join("state");
        let store = McpTrustStore::at(&base, &root).unwrap();
        store.add_grant(grant(*store.workspace(), "stdio:ee --mcp-proxy")).unwrap();
        let text = fs::read_to_string(store.path()).unwrap();

        let foreign_workspace = *McpTrustStore::at(&base, &other).unwrap().workspace();
        fs::write(
            store.path(),
            text.replace(&store.workspace().as_string(), &foreign_workspace.as_string()),
        )
        .unwrap();
        assert!(matches!(store.load(), Err(TrustStoreError::IdentityMismatch)));

        fs::write(store.path(), b"not a trust document").unwrap();
        assert!(matches!(store.load(), Err(TrustStoreError::ParseFailure(_))));
    }

    #[cfg(unix)]
    #[test]
    fn non_owner_only_documents_are_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = workspace_root(temp.path(), "repo");
        let base = temp.path().join("state");
        let store = McpTrustStore::at(&base, &root).unwrap();
        store.add_grant(grant(*store.workspace(), "stdio:ee --mcp-proxy")).unwrap();
        fs::set_permissions(store.path(), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(store.load(), Err(TrustStoreError::PermissionFailure(_))));
    }

    #[test]
    fn execute_pins_survive_retrust_and_revoke_separately() {
        let temp = tempfile::tempdir().unwrap();
        let root = workspace_root(temp.path(), "repo");
        let base = temp.path().join("state");
        let store = McpTrustStore::at(&base, &root).unwrap();
        let workspace = *store.workspace();
        let id = "mcp_server_exec".to_string();
        let mut tools = pinned_server_tools(ee_mcp::ToolTransport::Stdio);
        tools.extend(pinned_execution_tools(ee_mcp::ToolTransport::Stdio));
        let execution = McpServerGrant::new(
            id.clone(),
            workspace,
            "ee".to_string(),
            "stdio:ee --mcp-proxy".to_string(),
            crate::policy::mcp_server_trust::granted_at_now(),
            tools,
        )
        .unwrap();
        store.add_grant(execution).unwrap();
        assert!(store.load().unwrap().grants()[0].includes_execution());

        // Re-trusting from a general prompt refreshes tools but keeps execute.
        store.add_grant(grant(workspace, "stdio:ee --mcp-proxy")).unwrap();
        assert!(store.load().unwrap().grants()[0].includes_execution());

        assert_eq!(store.revoke_execute("ee", "stdio:ee --mcp-proxy").unwrap(), 1);
        let loaded = store.load().unwrap();
        assert_eq!(loaded.grants().len(), 1);
        assert!(!loaded.grants()[0].includes_execution());
        assert!(!loaded.grants()[0].tools.is_empty());
    }

    #[test]
    fn replace_and_revoke_are_scoped_to_server_transport() {
        let temp = tempfile::tempdir().unwrap();
        let root = workspace_root(temp.path(), "repo");
        let base = temp.path().join("state");
        let store = McpTrustStore::at(&base, &root).unwrap();
        let workspace = *store.workspace();
        store.add_grant(grant(workspace, "stdio:ee --mcp-proxy")).unwrap();
        store.add_grant(grant(workspace, "acp:ee")).unwrap();
        assert_eq!(store.load().unwrap().grants().len(), 2);

        // Re-granting one transport replaces that grant without duplicating it.
        store.add_grant(grant(workspace, "stdio:ee --mcp-proxy")).unwrap();
        assert_eq!(store.load().unwrap().grants().len(), 2);

        assert_eq!(store.revoke("ee", Some("stdio:ee --mcp-proxy")).unwrap(), 1);
        assert_eq!(store.load().unwrap().grants().len(), 1);
        let remaining = store.load().unwrap().grants()[0].id.clone();
        assert_eq!(store.revoke_id(&remaining).unwrap(), 1);
        assert!(store.load().unwrap().is_empty());

        // `/permissions reset` clears the whole document.
        store.add_grant(grant(workspace, "stdio:ee --mcp-proxy")).unwrap();
        store.clear().unwrap();
        assert!(store.load().unwrap().is_empty());
    }

    #[test]
    fn documents_with_execute_tools_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let root = workspace_root(temp.path(), "repo");
        let base = temp.path().join("state");
        let store = McpTrustStore::at(&base, &root).unwrap();
        store.add_grant(grant(*store.workspace(), "stdio:ee --mcp-proxy")).unwrap();
        let mut text = fs::read_to_string(store.path()).unwrap();
        text = text.replace("category = \"read\"", "category = \"execute\"");
        fs::write(store.path(), text).unwrap();
        assert!(matches!(store.load(), Err(TrustStoreError::ValidationFailure(_))));
    }
}
