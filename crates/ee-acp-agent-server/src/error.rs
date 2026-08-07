//! Framework errors and their JSON-RPC wire mapping.
//!
//! [`AcpServerError`] is the framework-level error used by transports and
//! (in later phases) request dispatch.  [`ProviderError`] separates
//! provider-owned failures from framework failures so provider code can
//! decide what to surface.  Both map to JSON-RPC error codes via
//! [`AcpServerError::jsonrpc_code`] / [`AcpServerError::jsonrpc_message`].

use std::fmt;
use std::time::Duration;

use ee_agent_protocol::recovery::RecoverableError;
use ee_agent_protocol::{Error as RpcError, RequestId, SessionId};

/// JSON-RPC code for a cancelled request (ACP/MCP cancellation convention).
/// Shared with the client-request bridge, which maps client errors back to
/// provider-visible errors.
pub(crate) const CODE_REQUEST_CANCELLED: i32 = -32800;
/// JSON-RPC server-error code used for permission denials.
pub(crate) const CODE_PERMISSION_DENIED: i32 = -32001;

/// Framework-level server error.
#[derive(Debug)]
pub enum AcpServerError {
    /// Underlying I/O failure (transport read/write).
    Io(std::io::Error),
    /// The transport delivered bytes that are not valid JSON.
    JsonParse {
        /// The raw frame bytes that failed to parse.
        raw: String,
        /// The underlying serde error.
        source: serde_json::Error,
    },
    /// The frame is not a valid JSON-RPC message (wrong shape, batch, or an
    /// oversized frame).
    Protocol(String),
    /// The client negotiated a protocol version this server does not support.
    UnsupportedVersion {
        /// The version the client asked for, as carried on the wire.
        version: String,
    },
    /// A request failed parameter validation (relative path, empty id, ...).
    InvalidParams {
        /// Human-readable reason for the rejection.
        reason: String,
    },
    /// A request referenced a session this server does not know.
    UnknownSession(SessionId),
    /// A request exceeded the configured request timeout.
    RequestTimeout {
        /// The id of the request that timed out.
        request_id: RequestId,
    },
    /// The transport closed before the exchange completed.
    TransportClosed,
    /// The provider backend rejected or failed a request.
    Provider(ProviderError),
}

/// Provider-owned failure, distinct from framework failures.
#[derive(Debug, Clone)]
pub enum ProviderError {
    /// The provider rejected the request as invalid for its backend.
    InvalidRequest(String),
    /// The provider backend failed while handling the request.
    BackendFailure(String),
    /// The request was cancelled.
    Cancellation,
    /// A request the provider made of the client (agent → client) failed.
    ClientRequestFailure(String),
    /// The provider denied the request on permission grounds.
    PermissionDenied(String),
    /// The turn stopped on a recoverable interruption: completed work is
    /// durable and the same session may resume without losing it.
    Recoverable(RecoverableError),
    /// The provider rate-limited the request; `retry_after` carries the
    /// server hint when one was provided.
    RateLimited {
        /// Server-provided retry hint, when present.
        retry_after: Option<Duration>,
        /// Human-readable detail.
        detail: String,
    },
    /// A transient provider/network failure that may succeed on retry
    /// (5xx, connection reset, ...).  Never used for side-effecting
    /// operations by providers that cannot prove the request did not land.
    Transient {
        /// Server-provided retry hint, when present.
        retry_after: Option<Duration>,
        /// Human-readable detail.
        detail: String,
    },
}

impl AcpServerError {
    /// JSON-RPC error code this error maps to.
    #[must_use]
    pub fn jsonrpc_code(&self) -> i32 {
        match self {
            Self::Io(_) | Self::RequestTimeout { .. } | Self::TransportClosed => -32603,
            Self::JsonParse { .. } => -32700,
            Self::Protocol(_) | Self::UnsupportedVersion { .. } => -32600,
            Self::InvalidParams { .. } | Self::UnknownSession(_) => -32602,
            Self::Provider(provider) => provider.jsonrpc_code(),
        }
    }

    /// Human-readable JSON-RPC error message for this error.
    #[must_use]
    pub fn jsonrpc_message(&self) -> String {
        match self {
            Self::Io(source) => format!("I/O error: {source}"),
            Self::JsonParse { source, .. } => format!("parse error: {source}"),
            Self::Protocol(reason) => format!("invalid request: {reason}"),
            Self::UnsupportedVersion { version } => {
                format!("unsupported protocol version: {version}")
            }
            Self::InvalidParams { reason } => format!("invalid params: {reason}"),
            Self::UnknownSession(session_id) => format!("unknown session: {session_id}"),
            Self::RequestTimeout { request_id } => {
                format!("request timed out (id: {request_id})")
            }
            Self::TransportClosed => "transport closed".to_string(),
            Self::Provider(provider) => provider.jsonrpc_message(),
        }
    }

    /// Builds a JSON-RPC [`RpcError`] carrying this error's code and message.
    ///
    /// Structured payloads ride in `data` so clients can act on them without
    /// parsing messages: `InvalidParams` carries `reason`, recoverable turns
    /// carry `recoverable`, and rate limits carry `rate_limited`.
    #[must_use]
    pub fn into_rpc_error(&self) -> RpcError {
        match self {
            Self::InvalidParams { reason } => {
                RpcError::new(self.jsonrpc_code(), self.jsonrpc_message())
                    .data(serde_json::json!({ "reason": reason }))
            }
            Self::Provider(provider) => provider.into_rpc_error(),
            _ => RpcError::new(self.jsonrpc_code(), self.jsonrpc_message()),
        }
    }
}

impl ProviderError {
    /// JSON-RPC error code this error maps to.
    #[must_use]
    pub fn jsonrpc_code(&self) -> i32 {
        match self {
            Self::InvalidRequest(_) => -32600,
            Self::BackendFailure(_) | Self::ClientRequestFailure(_) => -32603,
            Self::Cancellation => CODE_REQUEST_CANCELLED,
            Self::PermissionDenied(_) => CODE_PERMISSION_DENIED,
            Self::Recoverable(_) | Self::RateLimited { .. } | Self::Transient { .. } => -32603,
        }
    }

    /// Human-readable JSON-RPC error message for this error.
    #[must_use]
    pub fn jsonrpc_message(&self) -> String {
        match self {
            Self::InvalidRequest(reason) => format!("invalid request: {reason}"),
            Self::BackendFailure(reason) => format!("provider backend failure: {reason}"),
            Self::Cancellation => "request cancelled".to_string(),
            Self::ClientRequestFailure(reason) => format!("client request failed: {reason}"),
            Self::PermissionDenied(reason) => format!("permission denied: {reason}"),
            Self::Recoverable(recoverable) => {
                format!("recoverable turn interruption: {}", recoverable.detail)
            }
            Self::RateLimited { detail, .. } => format!("provider rate limited: {detail}"),
            Self::Transient { detail, .. } => format!("transient provider failure: {detail}"),
        }
    }

    /// Builds a JSON-RPC [`RpcError`] carrying the structured `data` payload
    /// (`recoverable` / `rate_limited`) so clients can offer Resume/Discard
    /// without parsing messages.
    #[must_use]
    pub fn into_rpc_error(&self) -> RpcError {
        match self {
            Self::Recoverable(recoverable) => {
                RpcError::new(self.jsonrpc_code(), self.jsonrpc_message())
                    .data(serde_json::json!({ "recoverable": recoverable }))
            }
            Self::RateLimited { retry_after, .. } => {
                RpcError::new(self.jsonrpc_code(), self.jsonrpc_message()).data(
                    serde_json::json!({ "rate_limited": {
                        "retry_after_ms": retry_after.map(|duration| duration.as_millis() as u64),
                    } }),
                )
            }
            Self::Transient { retry_after, .. } => {
                RpcError::new(self.jsonrpc_code(), self.jsonrpc_message()).data(
                    serde_json::json!({ "transient": {
                        "retry_after_ms": retry_after.map(|duration| duration.as_millis() as u64),
                    } }),
                )
            }
            _ => RpcError::new(self.jsonrpc_code(), self.jsonrpc_message()),
        }
    }
}

impl fmt::Display for AcpServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => write!(f, "I/O error: {source}"),
            Self::JsonParse { raw, source } => {
                write!(f, "invalid JSON frame {raw:?}: {source}")
            }
            Self::Protocol(reason) => write!(f, "protocol error: {reason}"),
            Self::UnsupportedVersion { version } => {
                write!(f, "unsupported protocol version: {version}")
            }
            Self::InvalidParams { reason } => write!(f, "invalid params: {reason}"),
            Self::UnknownSession(session_id) => write!(f, "unknown session: {session_id}"),
            Self::RequestTimeout { request_id } => {
                write!(f, "request timed out (id: {request_id})")
            }
            Self::TransportClosed => f.write_str("transport closed"),
            Self::Provider(provider) => write!(f, "provider error: {provider}"),
        }
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(reason) => write!(f, "invalid request: {reason}"),
            Self::BackendFailure(reason) => write!(f, "provider backend failure: {reason}"),
            Self::Cancellation => f.write_str("request cancelled"),
            Self::ClientRequestFailure(reason) => write!(f, "client request failed: {reason}"),
            Self::PermissionDenied(reason) => write!(f, "permission denied: {reason}"),
            Self::Recoverable(recoverable) => {
                write!(f, "recoverable turn interruption: {}", recoverable.detail)
            }
            Self::RateLimited { detail, .. } => write!(f, "provider rate limited: {detail}"),
            Self::Transient { detail, .. } => write!(f, "transient provider failure: {detail}"),
        }
    }
}

impl std::error::Error for AcpServerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::JsonParse { source, .. } => Some(source),
            Self::Provider(provider) => Some(provider),
            _ => None,
        }
    }
}

impl std::error::Error for ProviderError {}

impl From<std::io::Error> for AcpServerError {
    fn from(source: std::io::Error) -> Self {
        Self::Io(source)
    }
}

impl From<ProviderError> for AcpServerError {
    fn from(provider: ProviderError) -> Self {
        Self::Provider(provider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider_samples() -> Vec<(ProviderError, i32, &'static str)> {
        vec![
            (ProviderError::InvalidRequest("bad param".into()), -32600, "invalid request"),
            (ProviderError::BackendFailure("boom".into()), -32603, "provider backend failure"),
            (ProviderError::Cancellation, -32800, "request cancelled"),
            (ProviderError::ClientRequestFailure("nope".into()), -32603, "client request failed"),
            (ProviderError::PermissionDenied("locked".into()), -32001, "permission denied"),
            (
                ProviderError::Recoverable(
                    RecoverableError::new(ee_agent_protocol::RecoverableFault::Deadline, "paused")
                        .with_checkpoint_id("ck-1")
                        .with_counts(4, 1),
                ),
                -32603,
                "recoverable turn interruption",
            ),
            (
                ProviderError::RateLimited {
                    retry_after: Some(std::time::Duration::from_secs(30)),
                    detail: "429".into(),
                },
                -32603,
                "provider rate limited",
            ),
            (
                ProviderError::Transient { retry_after: None, detail: "503".into() },
                -32603,
                "transient provider failure",
            ),
        ]
    }

    #[test]
    fn server_errors_map_to_jsonrpc_codes() {
        let cases: Vec<(AcpServerError, i32)> = vec![
            (AcpServerError::Io(std::io::Error::other("io")), -32603),
            (
                AcpServerError::JsonParse {
                    raw: "{".to_string(),
                    source: serde_json::from_str::<serde_json::Value>("{").unwrap_err(),
                },
                -32700,
            ),
            (AcpServerError::Protocol("bad shape".into()), -32600),
            (AcpServerError::UnsupportedVersion { version: "2".into() }, -32600),
            (AcpServerError::InvalidParams { reason: "bad param".into() }, -32602),
            (AcpServerError::UnknownSession(SessionId::new("s-1")), -32602),
            (AcpServerError::RequestTimeout { request_id: RequestId::Number(7) }, -32603),
            (AcpServerError::TransportClosed, -32603),
        ];
        for (error, expected) in cases {
            assert_eq!(error.jsonrpc_code(), expected, "{error:?}");
            assert!(!error.jsonrpc_message().is_empty(), "{error:?}");
        }
    }

    #[test]
    fn provider_errors_map_to_jsonrpc_codes() {
        for (error, expected_code, expected_fragment) in provider_samples() {
            let server = AcpServerError::Provider(error);
            assert_eq!(server.jsonrpc_code(), expected_code);
            let message = server.jsonrpc_message();
            assert!(message.contains(expected_fragment), "{message}");
        }
    }

    #[test]
    fn into_rpc_error_carries_code_and_message() {
        let error = AcpServerError::UnknownSession(SessionId::new("s-1"));
        let rpc = error.into_rpc_error();
        assert_eq!(i32::from(rpc.code), -32602);
        assert!(rpc.message.contains("unknown session: s-1"));
    }

    #[test]
    fn invalid_params_error_carries_reason_in_data() {
        let error = AcpServerError::InvalidParams { reason: "cwd must be an absolute path".into() };
        assert_eq!(error.jsonrpc_code(), -32602);
        let rpc = error.into_rpc_error();
        assert_eq!(i32::from(rpc.code), -32602);
        assert!(rpc.message.contains("invalid params: cwd must be an absolute path"));
        let data = rpc.data.expect("invalid params carries data");
        assert_eq!(data["reason"], "cwd must be an absolute path");
    }

    #[test]
    fn recoverable_error_carries_structured_data() {
        let error = ProviderError::Recoverable(
            RecoverableError::new(ee_agent_protocol::RecoverableFault::Deadline, "turn paused")
                .with_checkpoint_id("ck-7")
                .with_counts(4, 1)
                .with_retry_after(5_000),
        );
        let rpc = error.into_rpc_error();
        assert_eq!(i32::from(rpc.code), -32603);
        let data = rpc.data.expect("recoverable carries data");
        assert_eq!(data["recoverable"]["fault"], "deadline");
        assert_eq!(data["recoverable"]["checkpoint_id"], "ck-7");
        assert_eq!(data["recoverable"]["completed_tool_calls"], 4);
        assert_eq!(data["recoverable"]["retry_after"], 5_000);
        // The payload round-trips into the shared wire type.
        let parsed: ee_agent_protocol::RecoverableError =
            serde_json::from_value(data["recoverable"].clone()).expect("parses");
        assert_eq!(parsed.fault, ee_agent_protocol::RecoverableFault::Deadline);
        assert!(parsed.safe_resume);
    }

    #[test]
    fn rate_limited_error_carries_retry_hint() {
        let error = ProviderError::RateLimited {
            retry_after: Some(std::time::Duration::from_secs(30)),
            detail: "slow down".into(),
        };
        let rpc = error.into_rpc_error();
        let data = rpc.data.expect("rate limited carries data");
        assert_eq!(data["rate_limited"]["retry_after_ms"], 30_000);
    }

    #[test]
    fn transient_error_carries_retry_hint() {
        let error = ProviderError::Transient {
            retry_after: Some(std::time::Duration::from_secs(5)),
            detail: "upstream down".into(),
        };
        let rpc = error.into_rpc_error();
        let data = rpc.data.expect("transient carries data");
        assert_eq!(data["transient"]["retry_after_ms"], 5_000);
    }

    #[test]
    fn ambiguous_fault_is_never_safe_to_resume() {
        let error = ProviderError::Recoverable(
            RecoverableError::new(ee_agent_protocol::RecoverableFault::AmbiguousTool, "paused")
                .with_safe_resume(false),
        );
        let rpc = error.into_rpc_error();
        let data = rpc.data.expect("recoverable carries data");
        let parsed: ee_agent_protocol::RecoverableError =
            serde_json::from_value(data["recoverable"].clone()).expect("parses");
        assert!(!parsed.safe_resume);
        assert!(!parsed.fault.is_safe_to_resume());
    }

    #[test]
    fn provider_wraps_and_converts() {
        let error: AcpServerError = ProviderError::Cancellation.into();
        assert_eq!(error.jsonrpc_code(), -32800);
        assert!(matches!(error, AcpServerError::Provider(ProviderError::Cancellation)));
    }

    #[test]
    fn display_and_source_are_populated() {
        let io_error =
            AcpServerError::Io(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe"));
        assert!(io_error.to_string().contains("pipe"));
        assert!(std::error::Error::source(&io_error).is_some());

        let parse_error = AcpServerError::JsonParse {
            raw: "{".to_string(),
            source: serde_json::from_str::<serde_json::Value>("{").unwrap_err(),
        };
        assert!(parse_error.to_string().contains("invalid JSON frame"));
        assert!(std::error::Error::source(&parse_error).is_some());

        let provider = AcpServerError::Provider(ProviderError::BackendFailure("x".into()));
        assert!(provider.to_string().contains("provider error"));
        assert!(std::error::Error::source(&provider).is_some());
    }
}
