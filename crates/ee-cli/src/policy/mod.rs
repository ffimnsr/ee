//! Unified host-local workspace trust policy (Phase 1 foundation).
//!
//! This module owns the shared trust contracts every persistent rule type
//! builds on (ISSUES.md "Unified Host-Local Workspace Trust Policy"):
//!
//! - [`TrustOperation`] / [`TrustCategory`] / [`OperationIdentity`]:
//!   validated, normalized operations.  Every operation begins with a
//!   canonical workspace identity; missing identity, unknown category,
//!   malformed config, invalid path, expired/exhausted rule, or tool
//!   metadata mismatch returns a prompt.
//! - [`TrustRuleScope`]: common workspace / agent / expiration / use-budget
//!   scope carried by every rule variant.
//! - [`TrustDecision`]: allow/prompt verdict with a redacted machine-readable
//!   reason and an optional stable rule id.
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

pub(crate) mod clock;
pub(crate) mod command;
pub(crate) mod evaluator;
pub(crate) mod mcp;
pub(crate) mod paths;
pub(crate) mod profiles;
pub(crate) mod rules;
pub(crate) mod session;
pub(crate) mod store;
pub(crate) mod usage;

use std::collections::BTreeMap;
use std::time::SystemTime;

use sha2::{Digest as _, Sha256};

// Re-exports feed the approval flow (`agents` feature), bin tests, and
// later phases; without `agents` the lib build uses none of them yet.
#[allow(unused_imports)]
pub(crate) use clock::PolicyClock;
#[allow(unused_imports)]
pub(crate) use command::{
    CommandInvocation, SHELL_WRAPPERS, generate_command_rule_id, is_shell_wrapper,
    resolve_command_cwd, validate_command_tokens, validate_executable,
};
#[allow(unused_imports)]
pub(crate) use evaluator::{PolicyInput, evaluate};
#[allow(unused_imports)]
pub(crate) use mcp::{McpInvocation, generate_mcp_rule_id};
#[allow(unused_imports)]
pub(crate) use paths::is_protected_relative_path;
#[allow(unused_imports)]
pub(crate) use profiles::{
    CuratedProfile, PROFILE_REGISTRY_VERSION, PROFILES, ProfileEntry, is_known_profile,
    match_profile_entry,
};
#[allow(unused_imports)]
pub(crate) use rules::{
    CommandRule, MAX_WRITE_FILE_BYTES, MAX_WRITE_FILES, MAX_WRITE_TOTAL_BYTES, MatchMode,
    McpReadRule, McpRule, PathPrefix, ProfileRule, ReadPathRule, TrustRule, WriteOperationKind,
    WriteRule, generate_write_rule_id,
};
#[allow(unused_imports)]
pub(crate) use session::{SessionChoice, SessionPolicy};
#[allow(unused_imports)]
pub(crate) use store::{TrustStore, TrustStoreDocument, TrustStoreError};
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

/// Operation category; the schema names `read`, `write_create`,
/// `write_modify`, `execute`, and `unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrustCategory {
    Read,
    WriteCreate,
    WriteModify,
    Execute,
    Unknown,
}

impl TrustCategory {
    /// Machine-readable category name (redacted).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            TrustCategory::Read => "read",
            TrustCategory::WriteCreate => "write_create",
            TrustCategory::WriteModify => "write_modify",
            TrustCategory::Execute => "execute",
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

/// Validated operation-specific identity, matched only by the typed matcher
/// of the corresponding rule variant.  Operations that cannot be normalized
/// into one of these variants are `Unknown` and never match a persistent
/// rule.
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
    Unknown,
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

/// Allow/prompt verdict with a redacted machine-readable reason and the
/// stable id of the matched rule (persistent allows only).
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

    pub(crate) fn prompt(reason: DecisionReason) -> Self {
        Self { outcome: TrustOutcome::Prompt, reason, rule_id: None }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrustOutcome {
    Allow,
    Prompt,
}

/// Redacted, machine-readable decision reason; never carries paths, secrets,
/// environment values, or argument previews.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecisionReason {
    /// A recorded session deny matched the operation.
    SessionDeny,
    /// A recorded session allow matched the operation.
    SessionAllow,
    /// A validated persistent rule matched the operation.
    PersistentAllow,
    /// The operation has no valid normalized identity.
    UnknownOperation,
    /// The workspace gate is disabled and the operation requires it.
    WorkspaceDisabled,
    /// No session decision or validated rule matched.
    NoMatchingRule,
}

impl DecisionReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            DecisionReason::SessionDeny => "session_deny",
            DecisionReason::SessionAllow => "session_allow",
            DecisionReason::PersistentAllow => "persistent_allow",
            DecisionReason::UnknownOperation => "unknown_operation",
            DecisionReason::WorkspaceDisabled => "workspace_disabled",
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
