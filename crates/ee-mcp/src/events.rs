//! Host-facing events emitted by the MCP client manager.
//!
//! `ee-mcp` is UI-free: it emits [`McpEvent`]s on an unbounded channel and
//! never renders anything itself.  Elicitation requests carry a reply sender
//! so the host (agents pane) can answer them; if the host drops the reply
//! the client declines the elicitation.

use std::fmt;

use rmcp::model::{ElicitRequestParams, ElicitResult};
use tokio::sync::oneshot;

use crate::McpError;
use crate::discovery::DiscoverySnapshot;

/// Lifecycle state of one MCP server connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpServerState {
    /// Never started (config exists but no connection requested yet).
    Disabled,
    /// Connection is being established (spawn/HTTP + `server/discover`).
    Starting,
    /// `server/discover` succeeded and the connection is usable.
    Ready,
    /// The connection failed (spawn, handshake, transport, or reconnect).
    Failed,
    /// A registry refresh (list-changed notification or TTL) is in flight.
    Refreshing,
}

impl fmt::Display for McpServerState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            McpServerState::Disabled => "disabled",
            McpServerState::Starting => "starting",
            McpServerState::Ready => "ready",
            McpServerState::Failed => "failed",
            McpServerState::Refreshing => "refreshing",
        };
        f.write_str(label)
    }
}

/// An elicitation request forwarded from an MCP server to the host.
#[derive(Debug)]
pub struct ElicitationHandle {
    /// The server that asked for input.
    pub server_id: String,
    /// The elicitation (form or URL mode).
    pub request: ElicitRequestParams,
    /// Where the host sends its answer.
    pub reply: oneshot::Sender<Result<ElicitResult, McpError>>,
}

/// Events the MCP client manager emits.
#[derive(Debug)]
#[non_exhaustive]
pub enum McpEvent {
    /// A server connection changed state.
    ServerState {
        /// Server id.
        server_id: String,
        /// New state.
        state: McpServerState,
    },
    /// `server/discover` produced a fresh capability snapshot.
    Discovery {
        /// Server id.
        server_id: String,
        /// The parsed snapshot (protocol version already pinned).
        snapshot: DiscoverySnapshot,
    },
    /// A server asked for user input (`elicitation/create`).  Reply via
    /// [`ElicitationHandle::reply`]; dropping the sender declines.
    Elicitation(ElicitationHandle),
    /// Deprecated protocol `logging` message received (diagnostics only).
    Diagnostics {
        /// Server id.
        server_id: String,
        /// The log message (already treated as untrusted diagnostics).
        message: String,
    },
    /// The server notified a tool list change; the registry was refreshed.
    ToolListChanged {
        /// Server id.
        server_id: String,
    },
    /// The server notified a resource list change.
    ResourceListChanged {
        /// Server id.
        server_id: String,
    },
    /// The server notified a prompt list change.
    PromptListChanged {
        /// Server id.
        server_id: String,
    },
}
