//! Unified host-local workspace trust policy.
//!
//! This module owns the shared trust contracts every persistent rule type
//! builds on (ISSUES.md "Unified Host-Local Workspace Trust Policy"):
//!
//! - [`TrustOperation`] / [`TrustCategory`] / [`OperationIdentity`]:
//!   validated, normalized operations.  Every operation begins with a
//!   canonical workspace identity; missing identity, unknown category,
//!   malformed config, invalid path, expired/exhausted rule, or tool
//!   metadata mismatch falls through to configured confirm-or-deny defaults.
//! - [`TrustRuleScope`]: common workspace / agent / expiration / use-budget
//!   scope carried by every rule variant.
//! - [`TrustDecision`]: allow/deny/confirm verdict with a redacted
//!   machine-readable reason and an optional stable rule id.
//! - [`SessionPolicy`]: in-memory session allow/deny state; session deny
//!   precedes every session allow and persistent rule.
//! - [`TrustStore`]: host-local per-workspace persistence.  Grants live only
//!   in application-owned state (`$XDG_STATE_HOME/ee/trust/` on Linux), never
//!   in repository `ee.toml`, XDG project config, system config, or
//!   agent-provided files, and are keyed by the canonical workspace identity
//!   digest so copying repository or trust files grants nothing.
//! - `evaluate`: the pure shared evaluator.  It performs no filesystem,
//!   process, transport, UI, clock, or counter mutation; time and usage are
//!   injected.

pub(crate) mod bounded;
pub(crate) mod clock;
pub(crate) mod command;
pub(crate) mod evaluator;
pub(crate) mod manager;
pub(crate) mod mcp;
pub(crate) mod paths;
pub(crate) mod profiles;
pub(crate) mod rules;
pub(crate) mod safeguards;
pub(crate) mod session;
pub(crate) mod store;
pub(crate) mod templates;
pub(crate) mod usage;

use std::collections::BTreeMap;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

// Re-exports feed the approval flow (`agents` feature), bin tests, and
// later phases; without `agents` the lib build uses none of them yet.
#[allow(unused_imports)]
pub(crate) use bounded::{
    BoundedRuleCandidate, BoundedRuleKind, BoundedRulePreview, EXECUTE_GRANT_DURATION,
    EXECUTE_GRANT_MAX_USES, NETWORK_GRANT_DURATION, NETWORK_GRANT_MAX_USES, WRITE_GRANT_DURATION,
    WRITE_GRANT_MAX_USES,
};
#[allow(unused_imports)]
pub(crate) use clock::PolicyClock;
#[allow(unused_imports)]
pub(crate) use command::{
    CommandInvocation, SHELL_WRAPPERS, generate_command_rule_id, is_shell_wrapper,
    resolve_command_cwd, validate_argv_tokens, validate_command_tokens, validate_executable,
};
#[allow(unused_imports)]
pub(crate) use evaluator::{
    EvaluationResult, PolicyInput, PrecedenceTraceStep, TraceStatus, evaluate, evaluate_with_trace,
};
#[allow(unused_imports)]
pub(crate) use mcp::{McpInvocation, generate_mcp_rule_id};
#[allow(unused_imports)]
pub(crate) use paths::is_protected_relative_path;
#[allow(unused_imports)]
pub(crate) use profiles::{
    CuratedProfile, EE_MCP_SAFE_READ_PROFILE, EE_MCP_SAFE_READ_TOOL_SCHEMA_VERSION,
    PROFILE_REGISTRY_VERSION, PROFILES, ProfileEntry, TERMINAL_READONLY_PROFILE,
    is_known_mcp_read_profile, is_known_profile, match_profile_entry, mcp_read_profile_matches,
};
#[allow(unused_imports)]
pub(crate) use rules::{
    CommandRule, FilesystemRule, HostMatchMode, MAX_WRITE_FILE_BYTES, MAX_WRITE_FILES,
    MAX_WRITE_TOTAL_BYTES, MatchMode, McpDenyRule, McpReadProfileRule, McpReadRule, McpRule,
    NetworkRule, PathPrefix, ProfileRule, ReadPathRule, ToolRule, ToolRuleIdentity, TrustRule,
    WriteOperationKind, WriteRule, generate_filesystem_rule_id, generate_network_rule_id,
    generate_tool_rule_id, generate_write_rule_id,
};
#[allow(unused_imports)]
pub(crate) use safeguards::{
    CATASTROPHIC_DELETE_RULE_ID, SAFEGUARD_REGISTRY_VERSION, SafeguardCategory, SafeguardMatch,
    inspect_path_escape, inspect_protected_state_path, inspect_special_file,
    inspect_terminal_command,
};
#[allow(unused_imports)]
pub(crate) use session::{SessionChoice, SessionPolicy};
#[allow(unused_imports)]
pub(crate) use store::{
    CategoryDefaultRule, ManagedTrustDocument, RuleState, ToolDefaultRule, TrustStore,
    TrustStoreDocument, TrustStoreError,
};
#[allow(unused_imports)]
pub(crate) use usage::UsageLedger;

/// Domain separator for the workspace identity digest: the literal byte
/// sequence `ee.workspace.v1` followed by one NUL byte.
pub(crate) const WORKSPACE_IDENTITY_DOMAIN_SEPARATOR: &[u8] = b"ee.workspace.v1\0";

/// Canonical workspace identity: `SHA-256("ee.workspace.v1\0" +
/// canonical_workspace_root_path_bytes)`.
///
/// Canonical root bytes use platform-native path encoding and are never
/// serialized directly; only the domain-separated digest appears in store
/// filenames and documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct WorkspaceIdentity([u8; 32]);

impl WorkspaceIdentity {
    /// Digests the canonical root path bytes with the versioned domain
    /// separator.
    pub(crate) fn from_canonical_root_bytes(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(WORKSPACE_IDENTITY_DOMAIN_SEPARATOR);
        hasher.update(bytes);
        Self(hasher.finalize().into())
    }

    /// Lowercase hex digest; the store filename derives from this.
    pub(crate) fn hex(&self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// Document form: `sha256:<hex>`.
    pub(crate) fn as_string(&self) -> String {
        format!("sha256:{}", self.hex())
    }

    /// Parses the document form `sha256:<64 lowercase hex>`.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let hex = text
            .strip_prefix("sha256:")
            .ok_or_else(|| "identity must start with sha256:".to_string())?;
        if hex.len() != 64 {
            return Err("identity must be a 64-hex-char digest".to_string());
        }
        let mut digest = [0u8; 32];
        for (index, byte) in digest.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
                .map_err(|_| "identity contains invalid hex".to_string())?;
        }
        Ok(Self(digest))
    }
}

/// Closed side-effect category used by operation and deny scopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TrustCategory {
    Read,
    WriteCreate,
    WriteModify,
    Delete,
    Execute,
    Network,
    Unknown,
}

impl TrustCategory {
    /// Machine-readable category name (redacted).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            TrustCategory::Read => "read",
            TrustCategory::WriteCreate => "write_create",
            TrustCategory::WriteModify => "write_modify",
            TrustCategory::Delete => "delete",
            TrustCategory::Execute => "execute",
            TrustCategory::Network => "network",
            TrustCategory::Unknown => "unknown",
        }
    }
}

/// Transport an operation arrives through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransportKind {
    /// Native ACP session.
    Acp,
    /// MCP stdio proxy.
    McpStdio,
    /// MCP-over-ACP.
    McpAcp,
}

/// Explicit filesystem operation class for deny matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FilesystemOperationKind {
    Read,
    Create,
    Modify,
    Delete,
    Rename,
    Chmod,
    Symlink,
}

/// Closed normalized network scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NetworkScheme {
    Http,
    Https,
    Ws,
    Wss,
}

/// Closed network method class. Exact methods stay outside policy identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NetworkMethodClass {
    Read,
    Write,
    Connect,
}

/// Closed browser action class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BrowserActionClass {
    Navigate,
    Fetch,
    Download,
    Upload,
    WebSocket,
}

/// Validated operation-specific identity, matched only by corresponding typed
/// rule. Operations that cannot be normalized are `Unknown` and never match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OperationIdentity {
    Command {
        executable: String,
        argv: Vec<String>,
    },
    Mcp {
        server: String,
        transport_identity: String,
        tool: String,
        tool_schema_version: u64,
        /// Canonical compact JSON object (sorted keys, no duplicates).
        arguments_json: String,
    },
    ReadPath {
        /// Canonical workspace-relative slash-joined path.
        relative_path: String,
        byte_count: Option<u64>,
    },
    McpRead {
        server: String,
        transport_identity: String,
        tool: String,
        tool_schema_version: u64,
        relative_path: String,
        byte_count: Option<u64>,
    },
    Profile {
        profile: String,
    },
    Write {
        relative_path: String,
        file_count: u64,
        total_bytes: Option<u64>,
        max_file_bytes: Option<u64>,
    },
    Filesystem {
        operation: FilesystemOperationKind,
        /// Canonical workspace-relative source/primary path.
        source_path: Option<String>,
        /// Canonical workspace-relative destination path for rename/symlink.
        destination_path: Option<String>,
    },
    Network {
        scheme: NetworkScheme,
        /// Normalized lowercase exact host, without a trailing dot.
        host: String,
        port: u16,
        method: NetworkMethodClass,
        browser_action: BrowserActionClass,
    },
    /// Native tool identity used when operation-specific fields are absent.
    NativeTool {
        tool: String,
    },
    Unknown,
}

impl OperationIdentity {
    pub(crate) fn filesystem(
        operation: FilesystemOperationKind,
        source_path: Option<&str>,
        destination_path: Option<&str>,
    ) -> Result<Self, String> {
        let source_path = source_path.map(validate_identity_path).transpose()?;
        let destination_path = destination_path.map(validate_identity_path).transpose()?;
        match operation {
            FilesystemOperationKind::Rename | FilesystemOperationKind::Symlink
                if source_path.is_none() || destination_path.is_none() =>
            {
                return Err("rename and symlink identities require source and destination".into());
            }
            _ if source_path.is_none() && destination_path.is_none() => {
                return Err("filesystem identity requires at least one path".into());
            }
            _ => {}
        }
        Ok(Self::Filesystem { operation, source_path, destination_path })
    }

    pub(crate) fn network(
        scheme: NetworkScheme,
        host: &str,
        port: u16,
        method: NetworkMethodClass,
        browser_action: BrowserActionClass,
    ) -> Result<Self, String> {
        if port == 0 {
            return Err("port must be at least 1".into());
        }
        Ok(Self::Network {
            scheme,
            host: rules::normalize_host(host, rules::HostMatchMode::Exact)?,
            port,
            method,
            browser_action,
        })
    }

    pub(crate) fn native_tool(tool: &str) -> Result<Self, String> {
        if tool.is_empty() || tool.chars().any(char::is_control) {
            return Err("native tool identity must be non-empty and control-free".into());
        }
        Ok(Self::NativeTool { tool: tool.to_string() })
    }
}

fn validate_identity_path(raw: &str) -> Result<String, String> {
    if raw.is_empty() || raw.starts_with('/') || raw.starts_with('\\') || raw.contains(':') {
        return Err("filesystem path must be non-empty and workspace-relative".into());
    }
    for segment in raw.split('/') {
        if segment.is_empty()
            || matches!(segment, "." | "..")
            || segment.chars().any(|character| character.is_control() || character == '\\')
        {
            return Err("filesystem path must be canonical and traversal-free".into());
        }
    }
    Ok(raw.to_string())
}

/// One normalized operation fed to the evaluator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrustOperation {
    /// Canonical workspace identity the operation belongs to.
    pub(crate) workspace: WorkspaceIdentity,
    /// Optional configured agent id.
    pub(crate) agent: Option<String>,
    pub(crate) transport: TransportKind,
    pub(crate) category: TrustCategory,
    pub(crate) identity: OperationIdentity,
}

impl TrustOperation {
    /// Operations with a missing/unknown category or identity never match a
    /// persistent rule.
    pub(crate) fn is_unknown(&self) -> bool {
        self.category == TrustCategory::Unknown || self.identity == OperationIdentity::Unknown
    }

    /// Stable host-owned key used by tool-default policy.
    pub(crate) fn tool_key(&self) -> String {
        match &self.identity {
            OperationIdentity::Command { .. } => "terminal".to_string(),
            OperationIdentity::Mcp { server, tool, .. }
            | OperationIdentity::McpRead { server, tool, .. } => format!("mcp:{server}:{tool}"),
            OperationIdentity::ReadPath { .. } => "read".to_string(),
            OperationIdentity::Profile { profile } => format!("profile:{profile}"),
            OperationIdentity::Write { .. } => "write".to_string(),
            OperationIdentity::Filesystem { .. } => "filesystem".to_string(),
            OperationIdentity::Network { .. } => "network".to_string(),
            OperationIdentity::NativeTool { tool } => tool.clone(),
            OperationIdentity::Unknown => "unknown".to_string(),
        }
    }
}

/// Common scope shared by every rule variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrustRuleScope {
    /// Workspace identity the rule was created for.
    pub(crate) workspace: WorkspaceIdentity,
    /// Optional agent id; `None` scopes the rule to any configured agent in
    /// the matching workspace.
    pub(crate) agent: Option<String>,
    /// Absolute UTC expiration; expired rules evaluate as prompt.
    pub(crate) expires_at: Option<SystemTime>,
    /// Maximum successful uses; exhausted rules evaluate as prompt.
    pub(crate) max_uses: Option<u64>,
}

/// Policy effect carried by every persistent rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TrustEffect {
    Allow,
    Deny,
    Confirm,
}

/// Restricted fallback effect. Defaults never grant authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FallbackEffect {
    Deny,
    Confirm,
}

/// Allow/deny/confirm verdict with redacted reason and optional stable rule id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrustDecision {
    pub(crate) outcome: TrustOutcome,
    pub(crate) reason: DecisionReason,
    pub(crate) rule_id: Option<String>,
}

impl TrustDecision {
    pub(crate) fn allow(reason: DecisionReason, rule_id: Option<String>) -> Self {
        Self { outcome: TrustOutcome::Allow, reason, rule_id }
    }

    pub(crate) fn deny(reason: DecisionReason, rule_id: Option<String>) -> Self {
        Self { outcome: TrustOutcome::Deny, reason, rule_id }
    }

    pub(crate) fn confirm(reason: DecisionReason, rule_id: Option<String>) -> Self {
        Self { outcome: TrustOutcome::Confirm, reason, rule_id }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrustOutcome {
    Allow,
    Deny,
    Confirm,
}

/// Redacted, machine-readable decision reason; never carries paths, secrets,
/// environment values, or argument previews.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecisionReason {
    /// A non-overridable application safeguard denied the operation.
    BuiltInDeny,
    /// A persistent deny rule matched the operation.
    PersistentDeny,
    /// A recorded session deny matched the operation.
    SessionDeny,
    /// A persistent mandatory-confirm rule matched the operation.
    MandatoryConfirm,
    /// A recorded session allow matched the operation.
    SessionAllow,
    /// A validated persistent rule matched the operation.
    PersistentAllow,
    /// The operation has no valid normalized identity.
    UnknownOperation,
    /// The workspace gate is disabled and the operation requires it.
    WorkspaceDisabled,
    /// A tool-specific fallback denied the operation.
    ToolDefaultDeny,
    /// A tool-specific fallback requires confirmation.
    ToolDefaultConfirm,
    /// A side-effect category fallback denied the operation.
    CategoryDefaultDeny,
    /// A side-effect category fallback requires confirmation.
    CategoryDefaultConfirm,
    /// Global fallback denied the operation.
    GlobalDefaultDeny,
    /// Global fallback requires confirmation.
    GlobalDefaultConfirm,
    /// No session decision, validated rule, or injected fallback matched.
    NoMatchingRule,
}

impl DecisionReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            DecisionReason::BuiltInDeny => "built_in_deny",
            DecisionReason::PersistentDeny => "persistent_deny",
            DecisionReason::SessionDeny => "session_deny",
            DecisionReason::MandatoryConfirm => "mandatory_confirm",
            DecisionReason::SessionAllow => "session_allow",
            DecisionReason::PersistentAllow => "persistent_allow",
            DecisionReason::UnknownOperation => "unknown_operation",
            DecisionReason::WorkspaceDisabled => "workspace_disabled",
            DecisionReason::ToolDefaultDeny => "tool_default_deny",
            DecisionReason::ToolDefaultConfirm => "tool_default_confirm",
            DecisionReason::CategoryDefaultDeny => "category_default_deny",
            DecisionReason::CategoryDefaultConfirm => "category_default_confirm",
            DecisionReason::GlobalDefaultDeny => "global_default_deny",
            DecisionReason::GlobalDefaultConfirm => "global_default_confirm",
            DecisionReason::NoMatchingRule => "no_matching_rule",
        }
    }
}

/// Session-local usage snapshot keyed by stable rule id.  Runtime counters
/// are session-local and are never written into the trust-store document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct UsageSnapshot {
    used: BTreeMap<String, u64>,
}

impl UsageSnapshot {
    pub(crate) fn new(used: BTreeMap<String, u64>) -> Self {
        Self { used }
    }

    /// Successful uses recorded for `rule_id`.
    pub(crate) fn used(&self, rule_id: &str) -> u64 {
        self.used.get(rule_id).copied().unwrap_or(0)
    }
}
