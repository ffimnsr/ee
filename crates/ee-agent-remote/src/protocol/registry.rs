//! Pinned remote method registry.
//!
//! Mirrors the `ee-agent-protocol` registry discipline: one enum, one name per
//! variant, no string literals at dispatch call sites, and a documented error
//! code table. Dispatcher routes resolve through [`Method`].
//!
//! # Wire-name aliases with ACP
//!
//! Several names intentionally mirror ACP (`initialize`, `initialized`,
//! `session/update`, and the `session/*` lifecycle family) so drivers that
//! already understand ACP terminology do not learn a second vocabulary. The
//! *params shapes* are remote-specific and validated against the types in
//! [`super::types`]. The collision test below pins that allowlist: any new
//! accidental overlap with an ACP wire name fails the build.

use crate::error::RemoteError;

/// `initialize` request.
pub const INITIALIZE: &str = "initialize";
/// `initialized` notification.
pub const INITIALIZED: &str = "initialized";
/// `session/list` request.
pub const SESSION_LIST: &str = "session/list";
/// `session/new` request.
pub const SESSION_NEW: &str = "session/new";
/// `session/resume` request.
pub const SESSION_RESUME: &str = "session/resume";
/// `session/close` request.
pub const SESSION_CLOSE: &str = "session/close";
/// `session/set_mode` request.
pub const SESSION_SET_MODE: &str = "session/set_mode";
/// `turn/start` request.
pub const TURN_START: &str = "turn/start";
/// `turn/interrupt` request.
pub const TURN_INTERRUPT: &str = "turn/interrupt";
/// `permissions/pending` request.
pub const PERMISSIONS_PENDING: &str = "permissions/pending";
/// `permissions/respond` request.
pub const PERMISSIONS_RESPOND: &str = "permissions/respond";
/// `history/tail` request.
pub const HISTORY_TAIL: &str = "history/tail";
/// `session/update` notification (ACP passthrough).
pub const SESSION_UPDATE: &str = "session/update";
/// `turn/completed` notification.
pub const TURN_COMPLETED: &str = "turn/completed";
/// `status/changed` notification.
pub const STATUS_CHANGED: &str = "status/changed";
/// `permission/requestApproval` server→client request.
pub const PERMISSION_REQUEST_APPROVAL: &str = "permission/requestApproval";

/// Wire direction of one registry entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodKind {
    /// Client→server request expecting a response.
    Request,
    /// Notification with no response.
    Notification,
    /// Server→client request expecting a result.
    ServerRequest,
}

/// Every remote registry entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Method {
    /// Handshake request.
    Initialize,
    /// Handshake completion notification.
    Initialized,
    /// List loaded sessions and configured agents.
    SessionList,
    /// Start a new session.
    SessionNew,
    /// Resume an existing session.
    SessionResume,
    /// Close a loaded session.
    SessionClose,
    /// Change the session mode.
    SessionSetMode,
    /// Start a turn; returns immediately.
    TurnStart,
    /// Interrupt the running turn.
    TurnInterrupt,
    /// List pending approvals.
    PermissionsPending,
    /// Answer a pending approval.
    PermissionsRespond,
    /// Replay buffered notifications after a cursor.
    HistoryTail,
    /// ACP update passthrough.
    SessionUpdate,
    /// Turn terminal outcome.
    TurnCompleted,
    /// Connection or session status change.
    StatusChanged,
    /// Ask the client for an approval decision.
    PermissionRequestApproval,
}

impl Method {
    /// All registry entries, in registry order.
    pub const ALL: [Method; 16] = [
        Method::Initialize,
        Method::Initialized,
        Method::SessionList,
        Method::SessionNew,
        Method::SessionResume,
        Method::SessionClose,
        Method::SessionSetMode,
        Method::TurnStart,
        Method::TurnInterrupt,
        Method::PermissionsPending,
        Method::PermissionsRespond,
        Method::HistoryTail,
        Method::SessionUpdate,
        Method::TurnCompleted,
        Method::StatusChanged,
        Method::PermissionRequestApproval,
    ];

    /// The exact wire name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Initialize => INITIALIZE,
            Self::Initialized => INITIALIZED,
            Self::SessionList => SESSION_LIST,
            Self::SessionNew => SESSION_NEW,
            Self::SessionResume => SESSION_RESUME,
            Self::SessionClose => SESSION_CLOSE,
            Self::SessionSetMode => SESSION_SET_MODE,
            Self::TurnStart => TURN_START,
            Self::TurnInterrupt => TURN_INTERRUPT,
            Self::PermissionsPending => PERMISSIONS_PENDING,
            Self::PermissionsRespond => PERMISSIONS_RESPOND,
            Self::HistoryTail => HISTORY_TAIL,
            Self::SessionUpdate => SESSION_UPDATE,
            Self::TurnCompleted => TURN_COMPLETED,
            Self::StatusChanged => STATUS_CHANGED,
            Self::PermissionRequestApproval => PERMISSION_REQUEST_APPROVAL,
        }
    }

    /// Wire direction.
    #[must_use]
    pub fn kind(self) -> MethodKind {
        match self {
            Self::Initialize
            | Self::SessionList
            | Self::SessionNew
            | Self::SessionResume
            | Self::SessionClose
            | Self::SessionSetMode
            | Self::TurnStart
            | Self::TurnInterrupt
            | Self::PermissionsPending
            | Self::PermissionsRespond
            | Self::HistoryTail => MethodKind::Request,
            Self::Initialized | Self::SessionUpdate | Self::TurnCompleted | Self::StatusChanged => {
                MethodKind::Notification
            }
            Self::PermissionRequestApproval => MethodKind::ServerRequest,
        }
    }

    /// Resolves a wire name, if it is registered.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|method| method.name() == name)
    }

    /// Resolves a wire name or returns the typed `method_not_found` error.
    ///
    /// # Errors
    ///
    /// Returns `method_not_found` for unregistered names.
    pub fn try_from_name(name: &str) -> Result<Self, RemoteError> {
        Self::from_name(name).ok_or_else(|| RemoteError::method_not_found(name))
    }

    /// The typed params type name, used in validation errors.
    #[must_use]
    pub fn params_type(self) -> &'static str {
        match self {
            Self::Initialize => "InitializeParams",
            Self::Initialized => "()",
            Self::SessionList => "SessionListParams",
            Self::SessionNew => "SessionNewParams",
            Self::SessionResume => "SessionResumeParams",
            Self::SessionClose => "SessionCloseParams",
            Self::SessionSetMode => "SessionSetModeParams",
            Self::TurnStart => "TurnStartParams",
            Self::TurnInterrupt => "TurnInterruptParams",
            Self::PermissionsPending => "PermissionsPendingParams",
            Self::PermissionsRespond => "PermissionsRespondParams",
            Self::HistoryTail => "HistoryTailParams",
            Self::SessionUpdate => "SessionUpdateNotification",
            Self::TurnCompleted => "TurnCompletedNotification",
            Self::StatusChanged => "StatusChangedNotification",
            Self::PermissionRequestApproval => "PermissionApprovalRequest",
        }
    }
}

/// One documented error code row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorCodeDoc {
    /// Numeric JSON-RPC code.
    pub code: i64,
    /// Stable class name.
    pub class: &'static str,
    /// Meaning drivers can rely on.
    pub meaning: &'static str,
}

/// The complete remote error code table.
pub const ERROR_CODES: [ErrorCodeDoc; 13] = [
    ErrorCodeDoc { code: -32700, class: "parse_error", meaning: "frame is not valid JSON" },
    ErrorCodeDoc {
        code: -32600,
        class: "invalid_request",
        meaning: "not a valid JSON-RPC request (batches rejected)",
    },
    ErrorCodeDoc {
        code: -32601,
        class: "method_not_found",
        meaning: "method is not in the registry",
    },
    ErrorCodeDoc {
        code: -32602,
        class: "invalid_params",
        meaning: "params failed typed validation",
    },
    ErrorCodeDoc { code: -32603, class: "internal", meaning: "unexpected host-side failure" },
    ErrorCodeDoc { code: -32001, class: "overload", meaning: "outbound queue full; retry later" },
    ErrorCodeDoc {
        code: -32002,
        class: "not_initialized",
        meaning: "handshake not completed on this connection",
    },
    ErrorCodeDoc { code: -32003, class: "not_found", meaning: "unknown session or request" },
    ErrorCodeDoc {
        code: -32004,
        class: "already_active",
        meaning: "a turn already owns the session",
    },
    ErrorCodeDoc {
        code: -32005,
        class: "already_resolved",
        meaning: "another client answered the approval first",
    },
    ErrorCodeDoc { code: -32006, class: "timeout", meaning: "request deadline elapsed" },
    ErrorCodeDoc { code: -32007, class: "denied", meaning: "refused by policy or rules" },
    ErrorCodeDoc { code: -32008, class: "unauthorized", meaning: "bearer token rejected" },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Names that deliberately mirror ACP semantics. Everything else must be
    /// disjoint so debugging both surfaces cannot silently confuse them.
    const INTENTIONAL_ACP_ALIASES: [&str; 8] = [
        INITIALIZE,
        INITIALIZED,
        SESSION_LIST,
        SESSION_NEW,
        SESSION_RESUME,
        SESSION_CLOSE,
        SESSION_SET_MODE,
        SESSION_UPDATE,
    ];

    fn acp_wire_names() -> Vec<&'static str> {
        use ee_agent_protocol::registry as acp;
        vec![
            acp::INITIALIZE_METHOD_NAME,
            acp::AUTHENTICATE_METHOD_NAME,
            acp::LOGOUT_METHOD_NAME,
            acp::SESSION_NEW_METHOD_NAME,
            acp::SESSION_LOAD_METHOD_NAME,
            acp::SESSION_LIST_METHOD_NAME,
            acp::SESSION_DELETE_METHOD_NAME,
            acp::SESSION_RESUME_METHOD_NAME,
            acp::SESSION_CLOSE_METHOD_NAME,
            acp::SESSION_SET_MODE_METHOD_NAME,
            acp::SESSION_SET_CONFIG_OPTION_METHOD_NAME,
            acp::SESSION_PROMPT_METHOD_NAME,
            acp::SESSION_CANCEL_NOTIFICATION,
            acp::SESSION_REQUEST_PERMISSION_METHOD_NAME,
            acp::SESSION_UPDATE_NOTIFICATION,
            acp::FS_READ_TEXT_FILE_METHOD_NAME,
            acp::FS_WRITE_TEXT_FILE_METHOD_NAME,
            acp::TERMINAL_CREATE_METHOD_NAME,
            acp::TERMINAL_OUTPUT_METHOD_NAME,
            acp::TERMINAL_RELEASE_METHOD_NAME,
            acp::TERMINAL_WAIT_FOR_EXIT_METHOD_NAME,
            acp::TERMINAL_KILL_METHOD_NAME,
            acp::ELICITATION_CREATE_METHOD_NAME,
            acp::ELICITATION_COMPLETE_NOTIFICATION,
        ]
    }

    #[test]
    fn no_unintentional_acp_wire_name_collisions() {
        let acp = acp_wire_names();
        for method in Method::ALL {
            let name = method.name();
            if acp.contains(&name) {
                assert!(
                    INTENTIONAL_ACP_ALIASES.contains(&name),
                    "{name} collides with an ACP wire name without being a documented alias"
                );
            }
        }
    }

    #[test]
    fn every_name_resolves_back_to_its_variant() {
        for method in Method::ALL {
            assert_eq!(Method::from_name(method.name()), Some(method));
        }
    }

    #[test]
    fn names_are_unique() {
        let mut names: Vec<&str> = Method::ALL.iter().map(|method| method.name()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate registry names");
    }

    #[test]
    fn unknown_methods_return_the_typed_error() {
        let error = Method::try_from_name("turn/steer").expect_err("unknown");
        assert_eq!(error.code(), crate::error::RemoteErrorCode::MethodNotFound);
        assert_eq!(error.data().and_then(|data| data.get("method")), Some(&"turn/steer".into()));
    }

    #[test]
    fn direction_partition_matches_the_spec() {
        let requests =
            Method::ALL.into_iter().filter(|method| method.kind() == MethodKind::Request).count();
        let notifications = Method::ALL
            .into_iter()
            .filter(|method| method.kind() == MethodKind::Notification)
            .count();
        let server_requests = Method::ALL
            .into_iter()
            .filter(|method| method.kind() == MethodKind::ServerRequest)
            .count();
        assert_eq!((requests, notifications, server_requests), (11, 4, 1));
    }

    #[test]
    fn error_code_table_matches_the_error_enum() {
        use crate::error::RemoteErrorCode;
        let all = [
            RemoteErrorCode::ParseError,
            RemoteErrorCode::InvalidRequest,
            RemoteErrorCode::MethodNotFound,
            RemoteErrorCode::InvalidParams,
            RemoteErrorCode::Internal,
            RemoteErrorCode::Overload,
            RemoteErrorCode::NotInitialized,
            RemoteErrorCode::NotFound,
            RemoteErrorCode::AlreadyActive,
            RemoteErrorCode::AlreadyResolved,
            RemoteErrorCode::Timeout,
            RemoteErrorCode::Denied,
            RemoteErrorCode::Unauthorized,
        ];
        for code in all {
            let row = ERROR_CODES
                .iter()
                .find(|row| row.code == code.jsonrpc_code())
                .unwrap_or_else(|| panic!("missing documented row for {code:?}"));
            assert_eq!(row.class, code.as_str());
        }
    }
}
