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
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Serialize;

use super::manager::RuleMutation;
use super::rules::{
    RawCommandRule, RawFilesystemRule, RawMcpReadProfileRule, RawMcpReadRule, RawMcpRule,
    RawNetworkRule, RawProfileRule, RawReadPathRule, RawToolRule, RawWriteRule, validate_rule_id,
};
use super::{FallbackEffect, TrustCategory, TrustRule, WorkspaceIdentity};

/// Current trust-store schema version; unsupported versions load no
/// effective rules.
pub(crate) const TRUST_SCHEMA_VERSION: u64 = 2;
const LEGACY_TRUST_SCHEMA_VERSION: u64 = 1;

const TRUST_STATE_SUBDIR: &str = "trust";
const STORE_FILE_EXTENSION: &str = "toml";

fn default_load_time() -> SystemTime {
    #[cfg(test)]
    {
        crate::policy::clock::fixture_now()
    }
    #[cfg(not(test))]
    {
        SystemTime::now()
    }
}

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
    pub(crate) tool_defaults: Vec<ToolDefaultRule>,
    pub(crate) category_defaults: Vec<CategoryDefaultRule>,
    pub(crate) global_default: FallbackEffect,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ToolDefaultRule {
    pub(crate) tool: String,
    pub(crate) effect: FallbackEffect,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CategoryDefaultRule {
    pub(crate) category: TrustCategory,
    pub(crate) effect: FallbackEffect,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuleState {
    pub(crate) rule_id: String,
    pub(crate) enabled: bool,
    pub(crate) created_at: String,
    pub(crate) source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManagedTrustDocument {
    pub(crate) document: TrustStoreDocument,
    pub(crate) rule_states: Vec<RuleState>,
}

impl ManagedTrustDocument {
    pub(crate) fn state(&self, rule_id: &str) -> Option<&RuleState> {
        self.rule_states.iter().find(|state| state.rule_id == rule_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRuleTemplate {
    rule_id: String,
    template_id: String,
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

    /// Exact host-local file protected from agent mutation.
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
        self.load_at(default_load_time())
    }

    /// [`Self::load`] with injected time. Expired rules remain on disk but do
    /// not enter effective policy. Thirty-day/use ceilings apply only to
    /// authority-granting rules; deny may expire arbitrarily far in future.
    pub(crate) fn load_at(&self, now: SystemTime) -> Result<TrustStoreDocument, TrustStoreError> {
        let mut managed = self.load_for_management_at(now)?;
        let disabled = managed
            .rule_states
            .iter()
            .filter(|state| !state.enabled)
            .map(|state| state.rule_id.clone())
            .collect::<BTreeSet<_>>();
        managed
            .document
            .rules
            .retain(|rule| !disabled.contains(rule.id()) && scope_valid(rule, now));
        Ok(managed.document)
    }

    /// Strict unfiltered load for manager UI. Expired, exhausted, and disabled
    /// rules remain visible but never enter effective evaluation.
    pub(crate) fn load_for_management_at(
        &self,
        now: SystemTime,
    ) -> Result<ManagedTrustDocument, TrustStoreError> {
        let bytes = match read_verified_store(&self.path) {
            Ok(bytes) => bytes,
            Err(TrustStoreError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(ManagedTrustDocument {
                    document: TrustStoreDocument {
                        workspace: self.workspace,
                        workspace_enabled: false,
                        rules: Vec::new(),
                        tool_defaults: Vec::new(),
                        category_defaults: Vec::new(),
                        global_default: FallbackEffect::Confirm,
                    },
                    rule_states: Vec::new(),
                });
            }
            Err(error) => return Err(error),
        };
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| TrustStoreError::ParseFailure("trust store is not valid UTF-8".into()))?;
        let version = document_version(text)?;
        if version == LEGACY_TRUST_SCHEMA_VERSION {
            let migrated = migrate_v1(text, self.workspace)?;
            let document = parse_managed_document(&migrated, self.workspace)?;
            atomic_write(&self.path, migrated.as_bytes())?;
            return Ok(document);
        }
        let _ = now;
        parse_managed_document(text, self.workspace)
    }

    /// Fail-closed effective policy: any load problem yields an empty rule
    /// set with the workspace gate disabled, so every operation prompts.
    pub(crate) fn effective(&self) -> TrustStoreDocument {
        self.effective_at(default_load_time())
    }

    /// [`Self::effective`] with injected time (Phase 6).
    pub(crate) fn effective_at(&self, now: SystemTime) -> TrustStoreDocument {
        self.load_at(now).unwrap_or_else(|_| TrustStoreDocument {
            workspace: self.workspace,
            workspace_enabled: false,
            rules: Vec::new(),
            tool_defaults: Vec::new(),
            category_defaults: Vec::new(),
            global_default: FallbackEffect::Confirm,
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
            validate_rule_id(rule.id()).map_err(TrustStoreError::ValidationFailure)?;
            if !seen.insert(rule.id().to_string()) {
                return Err(TrustStoreError::ValidationFailure("duplicate rule id".into()));
            }
        }
        let text = serialize_document(document)?;
        parse_document_unfiltered(&text, self.workspace)?;
        atomic_write(&self.path, text.as_bytes())
    }

    /// Append-or-reuse: a rule whose stable id already exists is not
    /// duplicated; a new id is appended and persisted atomically.
    pub(crate) fn add_rule(&self, rule: TrustRule) -> Result<TrustStoreDocument, TrustStoreError> {
        let mut managed = self.load_for_management_at(default_load_time())?;
        if managed.document.rules.iter().any(|existing| existing.id() == rule.id()) {
            return Ok(managed.document);
        }
        let rule_id = rule.id().to_string();
        let source = rule
            .template_id()
            .map(|template| format!("template:{template}"))
            .unwrap_or_else(|| "user".into());
        managed.document.rules.push(rule);
        managed.rule_states.push(RuleState {
            rule_id,
            enabled: true,
            created_at: format_system_time(default_load_time()),
            source,
        });
        self.write_managed(&managed)?;
        Ok(managed.document)
    }

    pub(crate) fn mutate_rule_at(
        &self,
        rule_id: &str,
        mutation: RuleMutation,
        now: SystemTime,
    ) -> Result<ManagedTrustDocument, TrustStoreError> {
        validate_rule_id(rule_id).map_err(TrustStoreError::ValidationFailure)?;
        let mut managed = self.load_for_management_at(now)?;
        let position = managed
            .document
            .rules
            .iter()
            .position(|rule| rule.id() == rule_id)
            .ok_or_else(|| TrustStoreError::ValidationFailure("stale rule id".into()))?;
        match mutation {
            RuleMutation::Revoke => {
                managed.document.rules.remove(position);
                managed.rule_states.retain(|state| state.rule_id != rule_id);
            }
            RuleMutation::Disable | RuleMutation::Enable => {
                let enabled = mutation == RuleMutation::Enable;
                if let Some(state) =
                    managed.rule_states.iter_mut().find(|state| state.rule_id == rule_id)
                {
                    state.enabled = enabled;
                } else {
                    managed.rule_states.push(RuleState {
                        rule_id: rule_id.to_string(),
                        enabled,
                        created_at: "unknown".into(),
                        source: "legacy".into(),
                    });
                }
            }
        }
        self.write_managed(&managed)?;
        Ok(managed)
    }

    pub(crate) fn reset_at(
        &self,
        now: SystemTime,
    ) -> Result<ManagedTrustDocument, TrustStoreError> {
        let mut managed = self.load_for_management_at(now)?;
        managed.document.rules.clear();
        managed.document.tool_defaults.clear();
        managed.document.category_defaults.clear();
        managed.document.global_default = FallbackEffect::Confirm;
        managed.rule_states.clear();
        self.write_managed(&managed)?;
        Ok(managed)
    }

    fn write_managed(&self, managed: &ManagedTrustDocument) -> Result<(), TrustStoreError> {
        validate_managed(managed, self.workspace)?;
        let text = serialize_managed_document(managed)?;
        parse_managed_document(&text, self.workspace)?;
        atomic_write(&self.path, text.as_bytes())
    }
}

fn validate_managed(
    managed: &ManagedTrustDocument,
    workspace: WorkspaceIdentity,
) -> Result<(), TrustStoreError> {
    if managed.document.workspace != workspace {
        return Err(TrustStoreError::IdentityMismatch);
    }
    let mut rule_ids = BTreeSet::new();
    for rule in &managed.document.rules {
        if rule.scope().workspace != workspace {
            return Err(TrustStoreError::IdentityMismatch);
        }
        validate_rule_id(rule.id()).map_err(TrustStoreError::ValidationFailure)?;
        if !rule_ids.insert(rule.id()) {
            return Err(TrustStoreError::ValidationFailure("duplicate rule id".into()));
        }
    }
    let mut state_ids = BTreeSet::new();
    for state in &managed.rule_states {
        validate_rule_id(&state.rule_id).map_err(TrustStoreError::ValidationFailure)?;
        if !rule_ids.contains(state.rule_id.as_str()) || !state_ids.insert(state.rule_id.as_str()) {
            return Err(TrustStoreError::ValidationFailure(
                "rule state must reference one unique existing rule".into(),
            ));
        }
    }
    Ok(())
}

fn format_system_time(time: SystemTime) -> String {
    let datetime: chrono::DateTime<chrono::Utc> = time.into();
    datetime.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(unix)]
fn read_verified_store(path: &Path) -> Result<Vec<u8>, TrustStoreError> {
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
    verify_store_metadata(path, &metadata)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(TrustStoreError::Io)?;
    Ok(bytes)
}

#[cfg(not(unix))]
fn read_verified_store(_path: &Path) -> Result<Vec<u8>, TrustStoreError> {
    Err(TrustStoreError::PlatformAclUnsupported)
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

const V1_DOCUMENT_KEYS: [&str; 10] = [
    "schema_version",
    "workspace",
    "policy",
    "command_allow",
    "mcp_allow",
    "read_path_allow",
    "mcp_read_allow",
    "mcp_read_profile_allow",
    "profile_allow",
    "write_allow",
];

const DOCUMENT_KEYS: [&str; 17] = [
    "schema_version",
    "workspace",
    "policy",
    "command_rules",
    "mcp_rules",
    "read_path_rules",
    "mcp_read_rules",
    "mcp_read_profile_rules",
    "profile_rules",
    "write_rules",
    "network_rules",
    "filesystem_rules",
    "tool_rules",
    "tool_defaults",
    "category_defaults",
    "rule_templates",
    "rule_states",
];

fn parse_toml(text: &str) -> Result<toml::Value, TrustStoreError> {
    toml::from_str(text).map_err(|error| {
        TrustStoreError::ParseFailure(format!("invalid trust store TOML: {error}"))
    })
}

fn document_version(text: &str) -> Result<u64, TrustStoreError> {
    let value = parse_toml(text)?;
    let version = value
        .as_table()
        .and_then(|table| table.get("schema_version"))
        .and_then(toml::Value::as_integer)
        .ok_or_else(|| TrustStoreError::ValidationFailure("missing schema_version".into()))?;
    u64::try_from(version)
        .map_err(|_| TrustStoreError::ValidationFailure("invalid schema_version".into()))
}

fn migrate_v1(text: &str, expected: WorkspaceIdentity) -> Result<String, TrustStoreError> {
    let mut value = parse_toml(text)?;
    let table = value
        .as_table_mut()
        .ok_or_else(|| TrustStoreError::ParseFailure("trust store must be a TOML table".into()))?;
    for key in table.keys() {
        if !V1_DOCUMENT_KEYS.contains(&key.as_str()) {
            return Err(TrustStoreError::ValidationFailure(format!(
                "unknown or mixed version 1 field: {key}"
            )));
        }
    }
    if table.get("schema_version").and_then(toml::Value::as_integer)
        != Some(LEGACY_TRUST_SCHEMA_VERSION as i64)
    {
        return Err(TrustStoreError::UnsupportedSchemaVersion(document_version(text)?));
    }
    for (old, new) in [
        ("command_allow", "command_rules"),
        ("mcp_allow", "mcp_rules"),
        ("read_path_allow", "read_path_rules"),
        ("mcp_read_allow", "mcp_read_rules"),
        ("mcp_read_profile_allow", "mcp_read_profile_rules"),
        ("profile_allow", "profile_rules"),
        ("write_allow", "write_rules"),
    ] {
        if let Some(mut entries) = table.remove(old) {
            let array = entries.as_array_mut().ok_or_else(|| {
                TrustStoreError::ValidationFailure(format!("{old} must be an array of tables"))
            })?;
            for entry in array {
                let entry = entry.as_table_mut().ok_or_else(|| {
                    TrustStoreError::ValidationFailure(format!("{old} must contain tables"))
                })?;
                entry.insert("effect".into(), toml::Value::String("allow".into()));
            }
            table.insert(new.into(), entries);
        }
    }
    let policy = table
        .get_mut("policy")
        .and_then(toml::Value::as_table_mut)
        .ok_or_else(|| TrustStoreError::ValidationFailure("missing [policy] table".into()))?;
    if policy.len() != 1 || !policy.contains_key("workspace_enabled") {
        return Err(TrustStoreError::ValidationFailure(
            "version 1 [policy] must contain exactly workspace_enabled".into(),
        ));
    }
    policy.insert("global_default".into(), toml::Value::String("confirm".into()));
    table.insert("schema_version".into(), toml::Value::Integer(TRUST_SCHEMA_VERSION as i64));

    let candidate = toml::to_string(&value).map_err(|error| {
        TrustStoreError::ValidationFailure(format!(
            "cannot serialize migrated trust store: {error}"
        ))
    })?;
    let document = parse_document_unfiltered(&candidate, expected)?;
    serialize_document(&document)
}

fn parse_managed_document(
    text: &str,
    expected: WorkspaceIdentity,
) -> Result<ManagedTrustDocument, TrustStoreError> {
    let document = parse_document_unfiltered(text, expected)?;
    let value = parse_toml(text)?;
    let table = value
        .as_table()
        .ok_or_else(|| TrustStoreError::ParseFailure("trust store must be a TOML table".into()))?;
    let mut rule_states = Vec::new();
    let mut state_ids = BTreeSet::new();
    for entry in rule_entries(table, "rule_states")? {
        let state: RuleState = entry.clone().try_into().map_err(|error| {
            TrustStoreError::ValidationFailure(format!("invalid rule state: {error}"))
        })?;
        validate_rule_id(&state.rule_id).map_err(TrustStoreError::ValidationFailure)?;
        if !state_ids.insert(state.rule_id.clone()) {
            return Err(TrustStoreError::ValidationFailure("duplicate rule state id".into()));
        }
        if !document.rules.iter().any(|rule| rule.id() == state.rule_id) {
            return Err(TrustStoreError::ValidationFailure(
                "rule state references unknown rule id".into(),
            ));
        }
        if state.created_at.is_empty()
            || state.created_at.len() > 64
            || state.source.is_empty()
            || state.source.len() > 128
            || state.created_at.chars().chain(state.source.chars()).any(char::is_control)
        {
            return Err(TrustStoreError::ValidationFailure(
                "rule state metadata must be bounded and control-free".into(),
            ));
        }
        rule_states.push(state);
    }
    rule_states.sort_by(|left, right| left.rule_id.cmp(&right.rule_id));
    Ok(ManagedTrustDocument { document, rule_states })
}

fn parse_document_unfiltered(
    text: &str,
    expected: WorkspaceIdentity,
) -> Result<TrustStoreDocument, TrustStoreError> {
    let value = parse_toml(text)?;
    let table = value
        .as_table()
        .ok_or_else(|| TrustStoreError::ParseFailure("trust store must be a TOML table".into()))?;
    for key in table.keys() {
        if !DOCUMENT_KEYS.contains(&key.as_str()) {
            return Err(TrustStoreError::ValidationFailure(format!(
                "unknown or mixed version 2 field: {key}"
            )));
        }
    }
    let version = document_version(text)?;
    if version != TRUST_SCHEMA_VERSION {
        return Err(TrustStoreError::UnsupportedSchemaVersion(version));
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
    if policy_table.len() != 2
        || !policy_table.contains_key("workspace_enabled")
        || !policy_table.contains_key("global_default")
    {
        return Err(TrustStoreError::ValidationFailure(
            "[policy] must contain exactly workspace_enabled and global_default".into(),
        ));
    }
    let workspace_enabled =
        policy_table.get("workspace_enabled").and_then(toml::Value::as_bool).ok_or_else(|| {
            TrustStoreError::ValidationFailure("policy.workspace_enabled must be a boolean".into())
        })?;
    let global_default: FallbackEffect = policy_table
        .get("global_default")
        .cloned()
        .ok_or_else(|| TrustStoreError::ValidationFailure("missing global_default".into()))?
        .try_into()
        .map_err(|error| TrustStoreError::ValidationFailure(format!("invalid default: {error}")))?;

    let mut rules = Vec::new();
    parse_rules(table, "command_rules", &mut rules, |raw| {
        super::rules::CommandRule::from_raw(raw, identity).map(TrustRule::Command)
    })?;
    parse_rules(table, "mcp_rules", &mut rules, |raw| TrustRule::from_raw_mcp(raw, identity))?;
    parse_rules(table, "read_path_rules", &mut rules, |raw| {
        super::rules::ReadPathRule::from_raw(raw, identity).map(TrustRule::ReadPath)
    })?;
    parse_rules(table, "mcp_read_rules", &mut rules, |raw| {
        super::rules::McpReadRule::from_raw(raw, identity).map(TrustRule::McpRead)
    })?;
    parse_rules(table, "mcp_read_profile_rules", &mut rules, |raw| {
        super::rules::McpReadProfileRule::from_raw(raw, identity).map(TrustRule::McpReadProfile)
    })?;
    parse_rules(table, "profile_rules", &mut rules, |raw| {
        super::rules::ProfileRule::from_raw(raw, identity).map(TrustRule::Profile)
    })?;
    parse_rules(table, "write_rules", &mut rules, |raw| {
        super::rules::WriteRule::from_raw(raw, identity).map(TrustRule::Write)
    })?;
    parse_rules(table, "network_rules", &mut rules, |raw| {
        super::rules::NetworkRule::from_raw(raw, identity).map(TrustRule::Network)
    })?;
    parse_rules(table, "filesystem_rules", &mut rules, |raw| {
        super::rules::FilesystemRule::from_raw(raw, identity)
            .map(super::rules::FilesystemRule::into_trust_rule)
    })?;
    parse_rules(table, "tool_rules", &mut rules, |raw| {
        super::rules::ToolRule::from_raw(raw, identity).map(super::rules::ToolRule::into_trust_rule)
    })?;

    let mut templated_ids = BTreeSet::new();
    for entry in rule_entries(table, "rule_templates")? {
        let raw: RawRuleTemplate = entry.clone().try_into().map_err(|error| {
            TrustStoreError::ValidationFailure(format!("invalid rule template: {error}"))
        })?;
        validate_rule_id(&raw.rule_id).map_err(TrustStoreError::ValidationFailure)?;
        if raw.template_id.is_empty()
            || raw.template_id.chars().any(char::is_control)
            || !templated_ids.insert(raw.rule_id.clone())
        {
            return Err(TrustStoreError::ValidationFailure(
                "rule template must have unique rule_id and non-empty template_id".into(),
            ));
        }
        let position = rules.iter().position(|rule| rule.id() == raw.rule_id).ok_or_else(|| {
            TrustStoreError::ValidationFailure("rule template references unknown rule id".into())
        })?;
        let rule = rules.remove(position);
        rules.insert(
            position,
            TrustRule::with_template(raw.template_id, rule)
                .map_err(TrustStoreError::ValidationFailure)?,
        );
    }

    let mut ids = BTreeSet::new();
    for rule in &rules {
        validate_rule_id(rule.id()).map_err(TrustStoreError::ValidationFailure)?;
        if !ids.insert(rule.id().to_string()) {
            return Err(TrustStoreError::ValidationFailure("duplicate rule id".into()));
        }
    }

    let mut tool_defaults = Vec::new();
    let mut tools = BTreeSet::new();
    for entry in rule_entries(table, "tool_defaults")? {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawToolDefault {
            tool: String,
            effect: FallbackEffect,
        }
        let raw: RawToolDefault = entry.clone().try_into().map_err(|error| {
            TrustStoreError::ValidationFailure(format!("invalid tool default: {error}"))
        })?;
        validate_tool_default(&raw.tool).map_err(TrustStoreError::ValidationFailure)?;
        if !tools.insert(raw.tool.clone()) {
            return Err(TrustStoreError::ValidationFailure(
                "tool default must have unique tool".into(),
            ));
        }
        tool_defaults.push(ToolDefaultRule { tool: raw.tool, effect: raw.effect });
    }
    tool_defaults.sort_by(|left, right| left.tool.cmp(&right.tool));

    let mut category_defaults = Vec::new();
    let mut categories = BTreeSet::new();
    for entry in rule_entries(table, "category_defaults")? {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawCategoryDefault {
            category: TrustCategory,
            effect: FallbackEffect,
        }
        let raw: RawCategoryDefault = entry.clone().try_into().map_err(|error| {
            TrustStoreError::ValidationFailure(format!("invalid category default: {error}"))
        })?;
        if raw.category == TrustCategory::Unknown || !categories.insert(raw.category) {
            return Err(TrustStoreError::ValidationFailure(
                "category default must have unique known category".into(),
            ));
        }
        category_defaults.push(CategoryDefaultRule { category: raw.category, effect: raw.effect });
    }
    category_defaults.sort_by_key(|rule| rule.category);

    Ok(TrustStoreDocument {
        workspace: expected,
        workspace_enabled,
        rules,
        tool_defaults,
        category_defaults,
        global_default,
    })
}

const KNOWN_NATIVE_TOOL_DEFAULTS: [&str; 12] = [
    "terminal",
    "read",
    "write",
    "filesystem",
    "network",
    "fs/read_text_file",
    "fs/write_text_file",
    "fs/write_text_file_batch",
    "fs/create_directory",
    "fs/delete_path",
    "fs/copy_path",
    "fs/move_path",
];

fn validate_tool_default(tool: &str) -> Result<(), String> {
    if KNOWN_NATIVE_TOOL_DEFAULTS.contains(&tool) {
        return Ok(());
    }
    let Some(rest) = tool.strip_prefix("mcp:") else {
        return Err(format!("unknown native tool default: {tool}"));
    };
    let Some((server, mcp_tool)) = rest.split_once(':') else {
        return Err("MCP tool default must use mcp:<server>:<tool>".into());
    };
    if server.is_empty()
        || mcp_tool.is_empty()
        || server.contains(':')
        || mcp_tool.contains(':')
        || server.chars().any(char::is_control)
        || mcp_tool.chars().any(char::is_control)
    {
        return Err("MCP tool default must contain exact non-empty server and tool".into());
    }
    Ok(())
}

fn parse_rules<R, F>(
    table: &toml::Table,
    key: &str,
    rules: &mut Vec<TrustRule>,
    convert: F,
) -> Result<(), TrustStoreError>
where
    R: serde::de::DeserializeOwned,
    F: Fn(R) -> Result<TrustRule, String>,
{
    for entry in rule_entries(table, key)? {
        let raw: R = entry.clone().try_into().map_err(|error| {
            TrustStoreError::ValidationFailure(format!("invalid {key} entry: {error}"))
        })?;
        rules.push(convert(raw).map_err(|error| {
            TrustStoreError::ValidationFailure(format!("invalid {key} entry: {error}"))
        })?);
    }
    Ok(())
}

/// Whether one persisted scope is active and safe at `now`.
fn scope_valid(rule: &TrustRule, now: SystemTime) -> bool {
    let scope = rule.scope();
    match scope.expires_at {
        Some(expires) if expires <= now => return false, // past expiry
        Some(expires) if rule.effect() == super::TrustEffect::Allow => {
            let remaining = expires.duration_since(now);
            if remaining.map(|duration| duration > super::rules::MAX_RULE_DURATION).unwrap_or(true)
            {
                return false; // beyond maximum authority-grant duration
            }
        }
        Some(_) => {}
        None => {}
    }
    if rule.effect() == super::TrustEffect::Allow
        && scope.max_uses.is_some_and(|uses| uses > super::rules::MAX_RULE_MAX_USES)
    {
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    command_rules: Vec<RawCommandRule>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    mcp_rules: Vec<RawMcpRule>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    read_path_rules: Vec<RawReadPathRule>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    mcp_read_rules: Vec<RawMcpReadRule>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    mcp_read_profile_rules: Vec<RawMcpReadProfileRule>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    profile_rules: Vec<RawProfileRule>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    write_rules: Vec<RawWriteRule>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    network_rules: Vec<RawNetworkRule>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    filesystem_rules: Vec<RawFilesystemRule>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_rules: Vec<RawToolRule>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_defaults: Vec<ToolDefaultRule>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    category_defaults: Vec<CategoryDefaultRule>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    rule_templates: Vec<RawRuleTemplate>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    rule_states: Vec<RuleState>,
}

#[derive(Serialize)]
struct RawWorkspace {
    identity: String,
}

#[derive(Serialize)]
struct RawPolicy {
    workspace_enabled: bool,
    global_default: FallbackEffect,
}

fn serialize_document(document: &TrustStoreDocument) -> Result<String, TrustStoreError> {
    serialize_document_with_states(document, Vec::new())
}

fn serialize_managed_document(managed: &ManagedTrustDocument) -> Result<String, TrustStoreError> {
    serialize_document_with_states(&managed.document, managed.rule_states.clone())
}

fn serialize_document_with_states(
    document: &TrustStoreDocument,
    mut rule_states: Vec<RuleState>,
) -> Result<String, TrustStoreError> {
    let mut command_allow: Vec<RawCommandRule> = Vec::new();
    let mut mcp_allow: Vec<RawMcpRule> = Vec::new();
    let mut read_path_allow: Vec<RawReadPathRule> = Vec::new();
    let mut mcp_read_allow: Vec<RawMcpReadRule> = Vec::new();
    let mut mcp_read_profile_allow: Vec<RawMcpReadProfileRule> = Vec::new();
    let mut profile_allow: Vec<RawProfileRule> = Vec::new();
    let mut write_allow: Vec<RawWriteRule> = Vec::new();
    let mut network_rules: Vec<RawNetworkRule> = Vec::new();
    let mut filesystem_rules: Vec<RawFilesystemRule> = Vec::new();
    let mut tool_rules: Vec<RawToolRule> = Vec::new();
    let mut rule_templates = Vec::new();
    for rule in &document.rules {
        if let Some(template_id) = rule.template_id() {
            rule_templates.push(RawRuleTemplate {
                rule_id: rule.id().to_string(),
                template_id: template_id.to_string(),
            });
        }
        match rule.untemplated() {
            TrustRule::Command(rule) => command_allow.push(rule.into()),
            TrustRule::Mcp(rule) => mcp_allow.push(rule.into()),
            TrustRule::ReadPath(rule) => read_path_allow.push(rule.into()),
            TrustRule::McpRead(rule) => mcp_read_allow.push(rule.into()),
            TrustRule::McpReadProfile(rule) => mcp_read_profile_allow.push(rule.into()),
            TrustRule::Profile(rule) => profile_allow.push(rule.into()),
            TrustRule::Write(rule) => write_allow.push(rule.into()),
            TrustRule::Network(rule) => network_rules.push(rule.into()),
            TrustRule::McpDeny(rule) => mcp_allow.push(rule.into()),
            TrustRule::Filesystem(rule) => filesystem_rules.push(rule.into()),
            TrustRule::Tool(rule) => tool_rules.push(rule.into()),
            TrustRule::Template { .. } => unreachable!("untemplated rule"),
        }
    }
    command_allow.sort_by(|a, b| a.id.cmp(&b.id));
    mcp_allow.sort_by(|a, b| a.id.cmp(&b.id));
    read_path_allow.sort_by(|a, b| a.id.cmp(&b.id));
    mcp_read_allow.sort_by(|a, b| a.id.cmp(&b.id));
    mcp_read_profile_allow.sort_by(|a, b| a.id.cmp(&b.id));
    profile_allow.sort_by(|a, b| a.id.cmp(&b.id));
    write_allow.sort_by(|a, b| a.id.cmp(&b.id));
    network_rules.sort_by(|a, b| a.id.cmp(&b.id));
    filesystem_rules.sort_by(|a, b| a.id.cmp(&b.id));
    tool_rules.sort_by(|a, b| a.id.cmp(&b.id));
    let mut tool_defaults = document.tool_defaults.clone();
    tool_defaults.sort_by(|left, right| left.tool.cmp(&right.tool));
    let mut category_defaults = document.category_defaults.clone();
    category_defaults.sort_by_key(|rule| rule.category);
    rule_templates.sort_by(|left, right| left.rule_id.cmp(&right.rule_id));
    rule_states.sort_by(|left, right| left.rule_id.cmp(&right.rule_id));
    let raw = RawDocument {
        schema_version: TRUST_SCHEMA_VERSION,
        workspace: RawWorkspace { identity: document.workspace.as_string() },
        policy: RawPolicy {
            workspace_enabled: document.workspace_enabled,
            global_default: document.global_default,
        },
        command_rules: command_allow,
        mcp_rules: mcp_allow,
        read_path_rules: read_path_allow,
        mcp_read_rules: mcp_read_allow,
        mcp_read_profile_rules: mcp_read_profile_allow,
        profile_rules: profile_allow,
        write_rules: write_allow,
        network_rules,
        filesystem_rules,
        tool_rules,
        tool_defaults,
        category_defaults,
        rule_templates,
        rule_states,
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
        sync_parent_dir(dir);
        Ok(())
    })();
    if result.is_err() {
        // Best-effort cleanup: never leave a partial replacement behind.
        let _ = fs::remove_file(&temp);
    }
    result
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
    use crate::policy::rules::{CommandRule, MatchMode, RawCommandRule};
    use crate::policy::{TrustEffect, TrustRule};
    use std::time::Duration;

    #[test]
    fn persistent_deny_scope_allows_arbitrary_future_expiry_but_allow_does_not() {
        let now = default_load_time();
        let future = now + Duration::from_secs(365 * 24 * 60 * 60);
        let expiry: chrono::DateTime<chrono::Utc> = future.into();
        let raw = RawCommandRule {
            id: "deny-future".into(),
            effect: TrustEffect::Deny,
            agent: None,
            executable: "git".into(),
            match_mode: MatchMode::ArgvExact,
            argv: vec!["status".into()],
            expires_at: Some(expiry.to_rfc3339()),
            max_uses: None,
        };
        let workspace = WorkspaceIdentity::from_canonical_root_bytes(b"/phase9-scope");
        let deny = TrustRule::Command(CommandRule::from_raw(raw.clone(), workspace).unwrap());
        assert!(scope_valid(&deny, now));

        let allow = TrustRule::Command(
            CommandRule::from_raw(
                RawCommandRule {
                    id: "allow-future".into(),
                    effect: TrustEffect::Allow,
                    max_uses: Some(1),
                    ..raw
                },
                workspace,
            )
            .unwrap(),
        );
        assert!(!scope_valid(&allow, now));
    }
}
