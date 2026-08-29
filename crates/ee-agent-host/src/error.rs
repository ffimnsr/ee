//! Typed host errors for agents mode.
//!
//! `AgentError` is the single error type crossing the `ee-agent-host`
//! boundary.  Wire-level JSON-RPC failures keep their typed SDK error, and
//! every host-side failure (timeout, closed connection, denied permission)
//! maps to a distinct variant so the UI can react without string matching.

use std::fmt;

use ee_agent_protocol::{Error as RpcError, SessionId};

/// A typed failure in the agent host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentError {
    /// The agent id is not configured on this host.
    UnknownAgent(String),
    /// The agent subprocess could not be started.
    SpawnFailed { agent_id: String, message: String },
    /// The ACP handshake did not complete within the timeout.
    HandshakeTimeout { agent_id: String },
    /// The agent did not negotiate ACP v1.
    UnsupportedProtocolVersion { agent_id: String, version: String },
    /// The agent connection is closed; the subprocess is gone.
    ConnectionClosed { agent_id: String },
    /// A request did not complete within its timeout.
    RequestTimeout { method: String },
    /// The agent answered with a JSON-RPC error.
    Rpc(RpcError),
    /// A prompt was submitted while another turn was still running.
    TurnAlreadyRunning,
    /// No turn is running for this session.
    NoRunningTurn,
    /// The turn was cancelled locally (permission grants were resolved as
    /// cancelled and `session/cancel` was sent).
    Cancelled,
    /// The agent answered a prompt with an unknown stop reason shape.
    UnexpectedResponse(String),
    /// A permission decision was reported for an unknown or already-resolved
    /// permission request.
    UnknownPermissionRequest { request_id: u64 },
    /// The client declined a file, terminal, or elicitation operation.
    PermissionDenied { reason: String },
    /// Application-owned safeguard denied an operation before configurable
    /// policy or approval. Fields are stable, redacted identifiers.
    NonOverridableDenied { rule_id: String, category: String },
    /// The client does not advertise or implement the requested capability.
    CapabilityUnsupported { method: String },
    /// The agent sent an invalid session/update for the session's ordering.
    InvalidUpdate(String),
    /// A registered client request handler failed.
    HandlerError(String),
    /// Invalid local arguments (bad paths, empty commands, ...).
    InvalidParams(String),
    /// Session-related error.
    Session { session_id: SessionId, message: String },
    /// I/O or process error.
    Io(String),
}

impl AgentError {
    /// Creates [`AgentError::InvalidParams`] with the given message.
    #[must_use]
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::InvalidParams(message.into())
    }

    /// Converts this error into an ACP-compatible JSON-RPC error so the host
    /// can answer agent-to-client requests that failed locally.
    #[must_use]
    pub fn into_rpc(self) -> RpcError {
        match self {
            Self::Rpc(error) => error,
            Self::PermissionDenied { reason } | Self::InvalidParams(reason) => {
                RpcError::invalid_params().data(serde_json::json!({ "reason": reason }))
            }
            Self::NonOverridableDenied { rule_id, category } => RpcError::invalid_params().data(
                serde_json::json!({
                    "reason": "non-overridable safeguard denied operation",
                    "ruleId": rule_id,
                    "category": category,
                    "nonOverridable": true,
                }),
            ),
            Self::Cancelled => RpcError::request_cancelled(),
            Self::CapabilityUnsupported { method } => RpcError::method_not_found()
                .data(serde_json::json!({ "method": method, "reason": "client does not implement this capability" })),
            Self::UnknownPermissionRequest { request_id } => RpcError::invalid_params().data(
                serde_json::json!({ "requestId": request_id, "reason": "unknown permission request" }),
            ),
            other => RpcError::internal_error().data(serde_json::json!({ "reason": other.to_string() })),
        }
    }
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownAgent(agent_id) => write!(f, "unknown agent {agent_id:?}"),
            Self::SpawnFailed { agent_id, message } => {
                write!(f, "failed to spawn agent {agent_id:?}: {message}")
            }
            Self::HandshakeTimeout { agent_id } => {
                write!(f, "agent {agent_id:?} did not complete the ACP handshake in time")
            }
            Self::UnsupportedProtocolVersion { agent_id, version } => {
                write!(f, "agent {agent_id:?} negotiated unsupported ACP version {version:?}")
            }
            Self::ConnectionClosed { agent_id } => {
                write!(f, "agent connection {agent_id:?} is closed")
            }
            Self::RequestTimeout { method } => {
                write!(f, "ACP request {method:?} timed out")
            }
            Self::Rpc(error) => write!(f, "agent returned JSON-RPC error: {error}"),
            Self::TurnAlreadyRunning => write!(f, "a prompt turn is already running"),
            Self::NoRunningTurn => write!(f, "no prompt turn is running"),
            Self::Cancelled => write!(f, "turn cancelled"),
            Self::UnexpectedResponse(message) => write!(f, "unexpected agent response: {message}"),
            Self::UnknownPermissionRequest { request_id } => {
                write!(f, "unknown or stale permission request {request_id}")
            }
            Self::PermissionDenied { reason } => write!(f, "permission denied: {reason}"),
            Self::NonOverridableDenied { rule_id, category } => {
                write!(f, "operation denied by non-overridable safeguard {rule_id} ({category})")
            }
            Self::CapabilityUnsupported { method } => {
                write!(f, "client does not implement capability for {method:?}")
            }
            Self::InvalidUpdate(message) => write!(f, "invalid session update: {message}"),
            Self::HandlerError(message) => write!(f, "client request handler failed: {message}"),
            Self::InvalidParams(message) => write!(f, "invalid params: {message}"),
            Self::Session { session_id, message } => {
                write!(f, "session {session_id:?}: {message}")
            }
            Self::Io(message) => write!(f, "host I/O error: {message}"),
        }
    }
}

impl std::error::Error for AgentError {}

impl From<RpcError> for AgentError {
    fn from(error: RpcError) -> Self {
        Self::Rpc(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ee_agent_protocol::ErrorCode;

    #[test]
    fn display_messages_are_stable() {
        let cases = [
            (AgentError::UnknownAgent("a".into()), "unknown agent \"a\""),
            (
                AgentError::HandshakeTimeout { agent_id: "a".into() },
                "agent \"a\" did not complete the ACP handshake in time",
            ),
            (AgentError::PermissionDenied { reason: "nope".into() }, "permission denied: nope"),
        ];
        for (error, expected) in cases {
            assert_eq!(error.to_string(), expected);
        }
    }

    #[test]
    fn into_rpc_preserves_agent_rpc_errors() {
        let rpc = RpcError::resource_not_found(Some("x".into()));
        let converted = AgentError::Rpc(rpc.clone()).into_rpc();
        assert_eq!(converted.code, rpc.code);
        assert_eq!(converted, rpc);
    }

    #[test]
    fn into_rpc_maps_denial_to_invalid_params() {
        let converted = AgentError::PermissionDenied { reason: "blocked".into() }.into_rpc();
        assert_eq!(converted.code, ErrorCode::InvalidParams);
        assert_eq!(converted.data, Some(serde_json::json!({ "reason": "blocked" })));
    }

    #[test]
    fn into_rpc_preserves_non_overridable_denial_metadata() {
        let converted = AgentError::NonOverridableDenied {
            rule_id: "builtin.v1.test".into(),
            category: "catastrophic_deletion".into(),
        }
        .into_rpc();
        assert_eq!(converted.code, ErrorCode::InvalidParams);
        assert_eq!(
            converted.data,
            Some(serde_json::json!({
                "reason": "non-overridable safeguard denied operation",
                "ruleId": "builtin.v1.test",
                "category": "catastrophic_deletion",
                "nonOverridable": true,
            }))
        );
    }

    #[test]
    fn into_rpc_maps_cancelled_to_request_cancelled() {
        let converted = AgentError::Cancelled.into_rpc();
        assert_eq!(converted.code, ErrorCode::RequestCancelled);
    }

    #[test]
    fn into_rpc_maps_unsupported_capability_to_method_not_found() {
        let converted =
            AgentError::CapabilityUnsupported { method: "fs/read_text_file".into() }.into_rpc();
        assert_eq!(converted.code, ErrorCode::MethodNotFound);
    }
}
