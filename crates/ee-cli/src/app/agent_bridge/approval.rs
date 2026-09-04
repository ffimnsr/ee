//! Approval schema: kinds, choices, fingerprints, and session-decision mapping.

use std::collections::BTreeSet;
use std::path::PathBuf;

use ee_agent_protocol::CreateTerminalRequest;
use tokio_util::sync::CancellationToken;

use crate::app::agents_mcp::ProxyRoute;

use crate::policy::TrustRule;

// ── Policy constants ─────────────────────────────────────────────────────────

/// The persistent terminal approval option label.
pub(crate) const PERSISTENT_TERMINAL_OPTION_LABEL: &str = "Allow for 1 hour / 20 uses";

/// The persistent write approval option label.
pub(crate) const PERSISTENT_WRITE_OPTION_LABEL: &str = "Allow for 1 hour / 5 uses";

// ── Approval prompt ──────────────────────────────────────────────────────────

/// The operation awaiting an explicit user decision.
#[derive(Debug, Clone)]
pub(crate) enum WriteExpectation {
    Blind,
    MustNotExist,
    ExpectRevision(String),
}

/// How the approved write is answered to the requester.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteReplyKind {
    FsWrite,
    ProxyStructured,
}

/// One prepared text write awaiting approval.
#[derive(Debug)]
pub(crate) struct PreparedWrite {
    pub(crate) path: PathBuf,
    pub(crate) content: String,
    pub(crate) tool_call_id: Option<String>,
    pub(crate) expectation: WriteExpectation,
    pub(crate) reply_kind: WriteReplyKind,
    pub(crate) proxy_edit_count: u32,
}

#[derive(Debug)]
pub(super) struct ProxyWriteSpec {
    pub(super) title: String,
    pub(super) detail: String,
    pub(super) prepared: PreparedWrite,
}

pub(super) enum WebApprovalCall {
    Search { query: String },
    Fetch { url: String },
    BrowserRun { request: ee_mcp::BrowserRunRequest },
}

impl std::fmt::Debug for WebApprovalCall {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let action = match self {
            Self::Search { .. } => "search",
            Self::Fetch { .. } => "fetch",
            Self::BrowserRun { request } => request.action.as_str(),
        };
        formatter.debug_tuple("WebApprovalCall").field(&action).finish()
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum WorkspaceMemoryApprovalOperation {
    Remember,
    Verify,
    Forget,
    Retract,
    Clear,
    DisableDelete,
    Export,
    Import,
}

impl WorkspaceMemoryApprovalOperation {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Remember => "remember",
            Self::Verify => "verify",
            Self::Forget => "forget",
            Self::Retract => "retract",
            Self::Clear => "clear",
            Self::DisableDelete => "disable --delete",
            Self::Export => "export",
            Self::Import => "import",
        }
    }
}

pub(super) enum WorkspaceMemoryApprovalTarget {
    ApprovalOnly,
    Remember { value: String },
    Forget,
    Retract,
    RetractKey { key: String },
    Clear,
    DisableDelete { config_path: PathBuf },
    Export { include_values: bool },
    ExportValue { include_values: bool },
    Import { export: Box<ee_agent_host::WorkspaceMemoryExportDto> },
}

impl std::fmt::Debug for WorkspaceMemoryApprovalTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApprovalOnly => formatter.write_str("ApprovalOnly"),
            Self::Remember { .. } => formatter.write_str("Remember { value: [redacted] }"),
            Self::Forget => formatter.write_str("Forget"),
            Self::Retract => formatter.write_str("Retract"),
            Self::RetractKey { .. } => formatter.write_str("RetractKey { key: [redacted] }"),
            Self::Clear => formatter.write_str("Clear"),
            Self::DisableDelete { config_path } => {
                formatter.debug_struct("DisableDelete").field("config_path", config_path).finish()
            }
            Self::Export { .. } => formatter.write_str("Export { include_values: [redacted] }"),
            Self::ExportValue { .. } => {
                formatter.write_str("ExportValue { include_values: [redacted] }")
            }
            Self::Import { .. } => formatter.write_str("Import { export: [redacted] }"),
        }
    }
}

#[derive(Debug)]
pub(super) enum ApprovalKind {
    Write {
        path: PathBuf,
        content: String,
        tool_call_id: Option<String>,
        expectation: WriteExpectation,
        reply_kind: WriteReplyKind,
        proxy_edit_count: u32,
    },
    WriteBatch {
        writes: Vec<PreparedWrite>,
        total_edit_count: u32,
    },
    Filesystem {
        operation: crate::app::agent_filesystem::FilesystemOperation,
    },
    TerminalCreate {
        request: CreateTerminalRequest,
    },
    /// External network approval carries only host/route in visible or
    /// persisted session state. Query and URL remain private call payloads.
    WorkspaceMemoryApproval {
        operation: WorkspaceMemoryApprovalOperation,
        key: String,
        target: WorkspaceMemoryApprovalTarget,
    },
    Network {
        route: ProxyRoute,
        /// Canonical host at original tool invocation.
        requested_host: String,
        /// Canonical host about to receive the current request/redirect.
        current_host: String,
        call: WebApprovalCall,
        approved_hosts: BTreeSet<String>,
        cancellation: CancellationToken,
    },
}

/// Session-local tool approval behavior selected from the agents TUI.
///
/// This controls only whether the UI approval dialog is shown. It never
/// bypasses request validation, workspace boundaries, revision checks, or ACP
/// permission and elicitation prompts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ToolApprovalMode {
    #[default]
    Default,
    Autopilot,
    Bypass,
}

impl ToolApprovalMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Autopilot => "autopilot",
            Self::Bypass => "bypass",
        }
    }
}

/// One approval decision the user can pick (Phase 2 policy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalChoice {
    /// Allow this operation only; identical future operations ask again.
    AllowOnce,
    /// Allow this and every identical operation for the rest of the session.
    AllowSession,
    /// Deny this operation only.
    DenyOnce,
    /// Deny this and every identical operation for the rest of the session.
    DenySession,
    /// Preview and persist the exact bounded candidate using default limits.
    AllowPersistent,
    /// Preview and persist the exact bounded candidate using shorter fixed limits.
    AllowPersistentShort,
    /// Preview and persist a command argv prefix ending at selected token boundary.
    AllowPersistentPrefix(usize),
    /// Preview and persist a command argv prefix using shorter fixed limits.
    AllowPersistentPrefixShort(usize),
    /// Preview and persist a narrow host-local deny rule before denying.
    DenyPersistent,
}

impl ApprovalChoice {
    pub(super) fn label(self) -> &'static str {
        match self {
            ApprovalChoice::AllowOnce => "Allow once",
            ApprovalChoice::AllowSession => "Allow session",
            ApprovalChoice::DenyOnce => "Deny",
            ApprovalChoice::DenySession => "Deny session",
            ApprovalChoice::AllowPersistent => PERSISTENT_TERMINAL_OPTION_LABEL,
            ApprovalChoice::AllowPersistentShort => "Allow for 10 minutes / 5 uses",
            ApprovalChoice::AllowPersistentPrefix(_) => "Allow structured command prefix",
            ApprovalChoice::AllowPersistentPrefixShort(_) => {
                "Allow structured command prefix for 10 minutes"
            }
            ApprovalChoice::DenyPersistent => "Deny for this workspace",
        }
    }

    pub(super) fn allows(self) -> bool {
        matches!(
            self,
            ApprovalChoice::AllowOnce
                | ApprovalChoice::AllowSession
                | ApprovalChoice::AllowPersistent
                | ApprovalChoice::AllowPersistentShort
                | ApprovalChoice::AllowPersistentPrefix(_)
                | ApprovalChoice::AllowPersistentPrefixShort(_)
        )
    }
}

/// Session-scoped approval policy (shared precedence contract, Phase 1
/// foundation).
///
/// `allow_once` / `deny_once` decisions are resolved by the approval UI
/// layer and never recorded; `allow_session` / `deny_session` decisions are
/// remembered per session, keyed by action kind and fingerprint (path for
/// writes, command+args fingerprint for terminals), and invalidated when the
/// session closes.  Allow-always persistence is deliberately not
/// implemented: persistent grants live only in the host-local trust store,
/// and the option does not exist at the schema level.
pub(crate) use crate::policy::session::{SessionChoice, SessionPolicy as ApprovalPolicy};

/// Fingerprint for one approval operation: action kind + stable identity.
pub(super) fn approval_fingerprint(kind: &ApprovalKind) -> String {
    match kind {
        ApprovalKind::Write { path, .. } => format!("write:{}", path.display()),
        ApprovalKind::WriteBatch { writes, .. } => format!(
            "write-batch:{}",
            writes
                .iter()
                .map(|write| write.path.display().to_string())
                .collect::<Vec<_>>()
                .join("|")
        ),
        ApprovalKind::Filesystem { operation } => operation.fingerprint(),
        ApprovalKind::WorkspaceMemoryApproval { operation, key, .. } => {
            format!("workspace-memory:{operation:?}:{key}")
        }
        ApprovalKind::TerminalCreate { request } => {
            let command = [request.command.clone()]
                .into_iter()
                .chain(request.args.clone())
                .collect::<Vec<_>>()
                .join("\u{1f}");
            format!("terminal:{command}")
        }
        ApprovalKind::Network { route, current_host, call, .. } => {
            let action = match call {
                WebApprovalCall::Search { .. } => "search",
                WebApprovalCall::Fetch { .. } => "fetch",
                WebApprovalCall::BrowserRun { request } => request.action.as_str(),
            };
            format!("network:{}:{action}:{current_host}", route.transport_identity())
        }
    }
}

/// Session-scoped counterpart of an approval choice; once-only choices are
/// never recorded (shared precedence contract, Phase 1 foundation), and
/// persistent grants are host-local rules, not session decisions.
pub(super) fn session_decision(choice: ApprovalChoice) -> Option<SessionChoice> {
    match choice {
        ApprovalChoice::AllowOnce
        | ApprovalChoice::DenyOnce
        | ApprovalChoice::AllowPersistent
        | ApprovalChoice::AllowPersistentShort
        | ApprovalChoice::AllowPersistentPrefix(_)
        | ApprovalChoice::AllowPersistentPrefixShort(_)
        | ApprovalChoice::DenyPersistent => None,
        ApprovalChoice::AllowSession => Some(SessionChoice::Allow),
        ApprovalChoice::DenySession => Some(SessionChoice::Deny),
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DenyScopePreview {
    pub(crate) workspace: String,
    pub(crate) agent: String,
    pub(crate) matcher_fields: Vec<(String, String)>,
    pub(crate) expires: String,
}

#[derive(Debug)]
pub(super) struct PersistentDenyCandidate {
    pub(super) rule: TrustRule,
    pub(super) preview: DenyScopePreview,
}

#[derive(Debug, Clone)]
pub(crate) struct MandatoryConfirmation {
    pub(crate) rule_id: String,
    pub(crate) template_id: Option<String>,
}

#[cfg(test)]
#[cfg(test)]
#[cfg(test)]
use ee_agent_protocol::SessionId;

#[cfg(test)]
#[cfg(test)]
mod tests {
    use super::*;

    // ── Approval policy (Phase 1 foundation) ────────────────────────────────
    // Session state lives in `crate::policy::session`; these tests pin the
    // once/session precedence contract the shared evaluator consumes.

    fn write_kind(path: &str) -> ApprovalKind {
        ApprovalKind::Write {
            path: PathBuf::from(path),
            content: String::new(),
            tool_call_id: None,
            expectation: WriteExpectation::Blind,
            reply_kind: WriteReplyKind::FsWrite,
            proxy_edit_count: 0,
        }
    }

    #[test]
    fn once_choices_are_never_recorded() {
        let policy = ApprovalPolicy::default();
        let fp = approval_fingerprint(&write_kind("/work/a.txt"));
        assert_eq!(session_decision(ApprovalChoice::AllowOnce), None);
        assert_eq!(session_decision(ApprovalChoice::DenyOnce), None);
        assert!(policy.lookup("s1", &fp).is_none(), "once decisions must not persist");
    }

    #[test]
    fn session_choices_map_to_shared_policy_state() {
        assert_eq!(session_decision(ApprovalChoice::AllowSession), Some(SessionChoice::Allow));
        assert_eq!(session_decision(ApprovalChoice::DenySession), Some(SessionChoice::Deny));
    }

    #[test]
    fn policy_session_allow_and_deny_are_scoped_and_invalidated() {
        let mut policy = ApprovalPolicy::default();
        let fp = approval_fingerprint(&write_kind("/work/a.txt"));
        policy.record("s1", &fp, SessionChoice::Allow);
        assert_eq!(policy.lookup("s1", &fp), Some(SessionChoice::Allow));
        // A different session is unaffected.
        assert!(policy.lookup("s2", &fp).is_none());

        policy.record("s1", &fp, SessionChoice::Deny);
        // Deny wins over allow for the same key.
        assert_eq!(policy.lookup("s1", &fp), Some(SessionChoice::Deny));

        policy.invalidate_session("s1");
        assert!(policy.lookup("s1", &fp).is_none(), "policy dies with the session");
    }

    #[test]
    fn policy_deny_wins_over_allow_for_same_fingerprint() {
        let mut policy = ApprovalPolicy::default();
        let fp = approval_fingerprint(&write_kind("/work/a.txt"));
        policy.record("s1", &fp, SessionChoice::Allow);
        policy.record("s1", &fp, SessionChoice::Deny);
        assert_eq!(policy.lookup("s1", &fp), Some(SessionChoice::Deny));
    }

    #[test]
    fn approval_fingerprints_differ_by_kind_and_identity() {
        let write = approval_fingerprint(&write_kind("/work/a.txt"));
        let other = approval_fingerprint(&write_kind("/work/b.txt"));
        assert_ne!(write, other);
        let mut request = CreateTerminalRequest::new(SessionId::new("s1"), "cargo");
        request.args = vec![String::from("test")];
        let terminal = approval_fingerprint(&ApprovalKind::TerminalCreate { request });
        assert_ne!(write, terminal);
    }
}
