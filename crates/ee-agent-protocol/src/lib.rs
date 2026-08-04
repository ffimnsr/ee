//! ACP (Agent Client Protocol) v1 wire protocol facade for ee agents mode.
//!
//! Per repository policy, protocol code prefers official SDK crates over
//! handrolled wire code.  All ACP wire structs, method metadata, and routing
//! enums come from the official [`agent-client-protocol`] SDK (Apache-2.0)
//! and are re-exported here so that `ee-cli` and `xi-core-lib` never
//! duplicate protocol types.  This crate owns only the ee-specific
//! boundaries the SDK does not cover:
//!
//! - [`version`] — strict v1-only version negotiation (fail closed).
//! - [`validate`] — absolute-path and 1-based line invariants at the protocol
//!   boundary; editor-relative coordinates are converted only inside editors.
//! - [`ordering`] — session-update ordering checks (SDK gap: documented in
//!   module docs and tests).
//! - [`capabilities`] — unknown-capability capture for diagnostics that never
//!   enables unsupported behavior.
//! - [`registry`] — typed JSON-RPC method registry whose constants are
//!   derived from the SDK's `AGENT_METHOD_NAMES` / `CLIENT_METHOD_NAMES`;
//!   per-method params validation stays local because the SDK's untagged
//!   routing enums cannot validate params by method (documented in tests).
//!
//! Target protocol: ACP v1 only (the latest stable major version).  Draft v2
//! and legacy v0 fail closed everywhere.
//!
//! [`agent-client-protocol`]: https://docs.rs/agent-client-protocol

pub mod capabilities;
pub mod mcp_over_acp;
pub mod ordering;
pub mod registry;
pub mod validate;
pub mod version;

// Convenience re-exports of the boundary helpers at the crate root.
pub use capabilities::*;
pub use ordering::*;
pub use registry::*;
pub use validate::*;
pub use version::*;

// The MCP-over-ACP facade (Phase 6b) lives in its own module: its runtime
// [`McpServer`](mcp_over_acp::McpServer) re-export shares the `McpServer`
// name with the schema enum re-exported below, so it is never glob-imported.
pub use mcp_over_acp::{EE_PROXY_SERVER_NAME, ee_proxy_acp_entry};

// ── Re-exports from the official SDK ──────────────────────────────────────────

/// All ACP v1 wire types (requests, responses, notifications, updates,
/// content blocks, capabilities, and shared identifiers).
pub use agent_client_protocol::schema::v1::*;

/// The ACP protocol version marker type (shared across v1/v2 schema).
pub use agent_client_protocol::schema::ProtocolVersion;

/// JSON-RPC error type, error codes, and convenience result alias.
pub use agent_client_protocol::{Error, ErrorCode, Result};

/// JSON-RPC 2.0 envelope types.
pub use agent_client_protocol::schema::v1::{
    JsonRpcBatch, JsonRpcMessage, Notification, Request, RequestId, Response,
};

/// High-level SDK entry points (role traits, stdio transport, handlers).
pub use agent_client_protocol::{
    AcpAgent, Agent, Builder, ByteStreams, Channel, Client, ConnectTo, ConnectionTo,
    HandleConnectionClose, HandleDispatchFrom, JsonRpcNotification, JsonRpcRequest,
    JsonRpcResponse, Lines, Responder, Role, RunWithConnectionTo, SentRequest, Stdio,
};

/// Handler-registration glue macros used with [`Builder::on_receive_request`]
/// and [`Builder::on_receive_notification`].
pub use agent_client_protocol::{on_receive_notification, on_receive_request};

/// String form of the supported ACP version, used for user-facing status.
///
/// The wire representation is the numeric [`ProtocolVersion`]; this string
/// form exists for messages like the `:agents` status line.
pub const SUPPORTED_ACP_VERSION: &str = "1";
