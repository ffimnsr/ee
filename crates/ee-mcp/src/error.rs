//! Typed errors for the ee MCP client manager.
//!
//! Everything here either wraps an [`rmcp`] error (transport, handshake,
//! protocol) or represents ee policy enforcement (version pinning,
//! capability rejection, invalid primitive shapes).  No JSON-RPC-level error
//! types are duplicated: protocol errors are [`rmcp::Error`].

use rmcp::service::{ClientInitializeError, ServiceError};

/// Errors produced by the ee MCP client.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum McpError {
    /// The server did not offer `2026-07-28` during `server/discover`.
    #[error(
        "unsupported MCP protocol version; server supports {server_supported:?}, ee requires {MCP_PROTOCOL_VERSION}"
    )]
    UnsupportedProtocolVersion {
        /// Versions the server advertised.
        server_supported: Vec<String>,
    },
    /// A server capability is deprecated and rejected by ee policy.
    #[error("deprecated MCP capability rejected: {capability}")]
    UnsupportedCapability {
        /// The capability name (e.g. `roots`, `sampling`, `logging`).
        capability: String,
    },
    /// The underlying stdio/HTTP transport failed.
    #[error("MCP transport failure: {0}")]
    Transport(String),
    /// The rmcp service task ended (transport closed, server exited).
    #[error("MCP connection closed: {0}")]
    ConnectionClosed(String),
    /// A protocol-level error surfaced by rmcp (JSON-RPC error, unexpected
    /// response type, handshake rejection).
    #[error("MCP protocol error: {0}")]
    Protocol(String),
    /// A primitive response failed ee shape validation.
    #[error("invalid MCP primitive result: {0}")]
    InvalidPrimitiveResult(String),
    /// A request exceeded its configured timeout.
    #[error("MCP request timed out after {timeout_ms}ms")]
    Timeout {
        /// The timeout that was enforced.
        timeout_ms: u64,
    },
    /// The server or request was cancelled.
    #[error("MCP request cancelled")]
    Cancelled,
    /// The server id or namespaced primitive does not exist.
    #[error("MCP resource not found: {0}")]
    NotFound(String),
    /// Local spawn/IO failure (stdio transport).
    #[error("MCP io error: {0}")]
    Io(String),
    /// A required behavior is missing from `rmcp` and has no local
    /// implementation; every variant must be justified by an SDK-gap test.
    #[error("rmcp SDK gap: {0}")]
    SdkGap(String),
}

impl McpError {
    /// The server rejected the `2026-07-28` handshake.
    pub fn is_unsupported_version(&self) -> bool {
        matches!(self, McpError::UnsupportedProtocolVersion { .. })
    }
}

impl From<ClientInitializeError> for McpError {
    fn from(error: ClientInitializeError) -> Self {
        match error {
            ClientInitializeError::NoCompatibleProtocolVersion {
                client_supported,
                server_supported,
            } => McpError::UnsupportedProtocolVersion {
                server_supported: server_supported.iter().map(ToString::to_string).collect(),
            }
            .into_protocol_note(client_supported),
            other => McpError::Protocol(other.to_string()),
        }
    }
}

impl McpError {
    fn into_protocol_note(self, _client_supported: Vec<rmcp::model::ProtocolVersion>) -> Self {
        self
    }
}

impl From<ServiceError> for McpError {
    fn from(error: ServiceError) -> Self {
        match error {
            ServiceError::TransportClosed => {
                McpError::ConnectionClosed("transport closed".to_string())
            }
            ServiceError::Timeout { timeout } => {
                McpError::Timeout { timeout_ms: timeout.as_millis().max(1) as u64 }
            }
            ServiceError::Cancelled { .. } => McpError::Cancelled,
            other => McpError::Protocol(other.to_string()),
        }
    }
}

impl From<std::io::Error> for McpError {
    fn from(error: std::io::Error) -> Self {
        McpError::Io(error.to_string())
    }
}

/// Protocol version constant used in error messages (kept in sync with
/// `crate::MCP_PROTOCOL_VERSION`).
const MCP_PROTOCOL_VERSION: &str = crate::MCP_PROTOCOL_VERSION;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_version_error_reports_server_versions() {
        let error = McpError::UnsupportedProtocolVersion {
            server_supported: vec!["2025-11-25".to_string()],
        };
        assert!(error.is_unsupported_version());
        assert!(error.to_string().contains("2025-11-25"));
        assert!(error.to_string().contains("2026-07-28"));
    }

    #[test]
    fn transport_and_io_errors_round_trip() {
        let error = McpError::Io("spawn failed".to_string());
        assert!(error.to_string().contains("spawn failed"));
        let error: McpError = std::io::Error::other("boom").into();
        assert!(error.to_string().contains("boom"));
    }
}
