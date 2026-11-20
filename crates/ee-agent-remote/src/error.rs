//! Typed remote-surface errors.
//!
//! Every failure crossing the remote boundary carries one stable code so
//! drivers never string-match. `AgentError` variants keep their identity: a
//! busy turn is `already_active`, a stale session is `not_found`, and a
//! fail-closed denial is `denied`.

use std::fmt;

use ee_agent_host::AgentError;

/// Stable remote error classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RemoteErrorCode {
    /// Malformed JSON.
    ParseError,
    /// The JSON value is not a valid JSON-RPC request.
    InvalidRequest,
    /// No dispatcher route exists for the method.
    MethodNotFound,
    /// Params failed typed validation.
    InvalidParams,
    /// Unexpected host-side failure.
    Internal,
    /// A method was called before `initialize`/`initialized`.
    NotInitialized,
    /// Unknown session, command, or pending request.
    NotFound,
    /// A turn is already running for the session.
    AlreadyActive,
    /// The pending approval was answered by someone else first.
    AlreadyResolved,
    /// A bounded request deadline elapsed.
    Timeout,
    /// The outbound queue for this connection is full.
    Overload,
    /// The operation was refused by policy, rules, or a denial answer.
    Denied,
    /// The transport token check failed.
    Unauthorized,
}

impl RemoteErrorCode {
    /// The JSON-RPC error code for this class.
    #[must_use]
    pub fn jsonrpc_code(self) -> i64 {
        match self {
            Self::ParseError => -32700,
            Self::InvalidRequest => -32600,
            Self::MethodNotFound => -32601,
            Self::InvalidParams => -32602,
            Self::Internal => -32603,
            Self::Overload => -32001,
            Self::NotInitialized => -32002,
            Self::NotFound => -32003,
            Self::AlreadyActive => -32004,
            Self::AlreadyResolved => -32005,
            Self::Timeout => -32006,
            Self::Denied => -32007,
            Self::Unauthorized => -32008,
        }
    }

    /// Stable machine-readable class name used in error `data`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ParseError => "parse_error",
            Self::InvalidRequest => "invalid_request",
            Self::MethodNotFound => "method_not_found",
            Self::InvalidParams => "invalid_params",
            Self::Internal => "internal",
            Self::NotInitialized => "not_initialized",
            Self::NotFound => "not_found",
            Self::AlreadyActive => "already_active",
            Self::AlreadyResolved => "already_resolved",
            Self::Timeout => "timeout",
            Self::Overload => "overload",
            Self::Denied => "denied",
            Self::Unauthorized => "unauthorized",
        }
    }
}

/// One typed remote-surface failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteError {
    code: RemoteErrorCode,
    message: String,
    data: Option<serde_json::Value>,
}

impl RemoteError {
    /// Creates an error with a stable class, message, and optional data.
    #[must_use]
    pub fn new(
        code: RemoteErrorCode,
        message: impl Into<String>,
        data: Option<serde_json::Value>,
    ) -> Self {
        Self { code, message: message.into(), data }
    }

    /// The stable error class.
    #[must_use]
    pub fn code(&self) -> RemoteErrorCode {
        self.code
    }

    /// The human-readable message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Optional structured data (already redacted for outbound use).
    #[must_use]
    pub fn data(&self) -> Option<&serde_json::Value> {
        self.data.as_ref()
    }

    /// Attaches structured data while keeping the code and message.
    #[must_use]
    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
        self
    }

    /// Attaches a stable class name to the data payload.
    #[must_use]
    pub fn with_class(mut self) -> Self {
        let mut data = self.data.take().unwrap_or_else(|| serde_json::json!({}));
        if let serde_json::Value::Object(map) = &mut data {
            map.insert("class".into(), self.code.as_str().into());
        } else {
            data = serde_json::json!({ "class": self.code.as_str(), "detail": data });
        }
        self.data = Some(data);
        self
    }

    /// `parse_error`.
    #[must_use]
    pub fn parse_error(message: impl Into<String>) -> Self {
        Self::new(RemoteErrorCode::ParseError, message, None)
    }

    /// `invalid_request`.
    #[must_use]
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(RemoteErrorCode::InvalidRequest, message, None)
    }

    /// `method_not_found`.
    #[must_use]
    pub fn method_not_found(method: &str) -> Self {
        Self::new(
            RemoteErrorCode::MethodNotFound,
            format!("unknown remote method {method:?}"),
            Some(serde_json::json!({ "method": method })),
        )
    }

    /// `invalid_params`.
    #[must_use]
    pub fn invalid_params(reason: impl Into<String>) -> Self {
        Self::new(RemoteErrorCode::InvalidParams, reason, None)
    }

    /// `internal`.
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(RemoteErrorCode::Internal, message, None)
    }

    /// `not_initialized`.
    #[must_use]
    pub fn not_initialized() -> Self {
        Self::new(
            RemoteErrorCode::NotInitialized,
            "initialize/initialized handshake required before this method",
            None,
        )
    }

    /// `not_found`.
    #[must_use]
    pub fn not_found(what: impl Into<String>) -> Self {
        Self::new(RemoteErrorCode::NotFound, format!("{} not found", what.into()), None)
    }

    /// `already_active`: a turn already owns the session.
    #[must_use]
    pub fn already_active(session_id: &str) -> Self {
        Self::new(
            RemoteErrorCode::AlreadyActive,
            "a turn is already running for this session; wait for turn/completed or call turn/interrupt",
            Some(serde_json::json!({ "sessionId": session_id })),
        )
    }

    /// `already_resolved`: another client answered the approval first.
    #[must_use]
    pub fn already_resolved(request_id: u64) -> Self {
        Self::new(
            RemoteErrorCode::AlreadyResolved,
            "permission request was already resolved",
            Some(serde_json::json!({ "requestId": request_id })),
        )
    }

    /// `timeout`.
    #[must_use]
    pub fn timeout(method: &str, timeout_ms: u64) -> Self {
        Self::new(
            RemoteErrorCode::Timeout,
            format!("remote method {method:?} timed out after {timeout_ms} ms"),
            Some(serde_json::json!({ "method": method, "timeoutMs": timeout_ms })),
        )
    }

    /// `overload`.
    #[must_use]
    pub fn overload() -> Self {
        Self::new(
            RemoteErrorCode::Overload,
            "outbound queue is full; retry after draining notifications",
            Some(serde_json::json!({ "retryable": true })),
        )
    }

    /// `denied`.
    #[must_use]
    pub fn denied(reason: impl Into<String>) -> Self {
        Self::new(RemoteErrorCode::Denied, reason, None)
    }

    /// `unauthorized`.
    #[must_use]
    pub fn unauthorized() -> Self {
        Self::new(RemoteErrorCode::Unauthorized, "invalid or missing bearer token", None)
    }
}

impl fmt::Display for RemoteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for RemoteError {}

impl From<AgentError> for RemoteError {
    fn from(error: AgentError) -> Self {
        match error {
            AgentError::TurnAlreadyRunning => Self::new(
                RemoteErrorCode::AlreadyActive,
                "a turn is already running for this session; wait for turn/completed or call turn/interrupt",
                None,
            ),
            AgentError::NoRunningTurn => {
                Self::new(RemoteErrorCode::NotFound, "no turn is running for this session", None)
            }
            AgentError::UnknownAgent(agent_id) => Self::new(
                RemoteErrorCode::NotFound,
                format!("unknown agent {agent_id:?}"),
                Some(serde_json::json!({ "agentId": agent_id })),
            ),
            AgentError::UnknownPermissionRequest { request_id } => {
                Self::already_resolved(request_id)
            }
            AgentError::RequestTimeout { method } => Self::new(
                RemoteErrorCode::Timeout,
                format!("agent request {method:?} timed out"),
                Some(serde_json::json!({ "method": method })),
            ),
            AgentError::PermissionDenied { reason } | AgentError::InvalidParams(reason) => {
                Self::new(RemoteErrorCode::Denied, reason, None)
            }
            AgentError::Cancelled => Self::new(RemoteErrorCode::Denied, "turn cancelled", None),
            other => Self::new(RemoteErrorCode::Internal, other.to_string(), None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonrpc_codes_match_the_documented_table() {
        assert_eq!(RemoteErrorCode::ParseError.jsonrpc_code(), -32700);
        assert_eq!(RemoteErrorCode::Overload.jsonrpc_code(), -32001);
        assert_eq!(RemoteErrorCode::NotInitialized.jsonrpc_code(), -32002);
        assert_eq!(RemoteErrorCode::NotFound.jsonrpc_code(), -32003);
        assert_eq!(RemoteErrorCode::AlreadyActive.jsonrpc_code(), -32004);
        assert_eq!(RemoteErrorCode::AlreadyResolved.jsonrpc_code(), -32005);
        assert_eq!(RemoteErrorCode::Timeout.jsonrpc_code(), -32006);
        assert_eq!(RemoteErrorCode::Denied.jsonrpc_code(), -32007);
        assert_eq!(RemoteErrorCode::Unauthorized.jsonrpc_code(), -32008);
    }

    #[test]
    fn busy_agent_turn_maps_to_already_active() {
        let error = RemoteError::from(AgentError::TurnAlreadyRunning);
        assert_eq!(error.code(), RemoteErrorCode::AlreadyActive);
        assert!(error.message().contains("turn/interrupt"));
    }

    #[test]
    fn class_is_added_to_structured_data() {
        let error = RemoteError::already_active("s-1").with_class();
        assert_eq!(error.data().and_then(|data| data.get("class")), Some(&"already_active".into()));
    }
}
