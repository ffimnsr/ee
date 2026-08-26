//! Deterministic host events consumed by the UI (Phase 3) and bridges.
//!
//! `ee-agent-host` stays UI-free: it never renders widgets.  Instead every
//! observable state change emits exactly one [`AgentEvent`] value; the UI
//! renders from [`crate::session::AgentThread::snapshot`] content and uses
//! events as change signals.
//!
//! Events never carry secrets: stderr lines are capped, and permission
//! events carry only the wire `PermissionOption` values the agent sent.

use ee_agent_protocol::{
    AgentCapabilities, AuthMethod, ElicitationId, Implementation, PermissionOption,
    RequestPermissionOutcome, SessionId, SessionUpdate, StopReason, ToolCallUpdate,
};
use serde::Deserialize;

use crate::error::AgentError;
use crate::turn_evidence::{TurnEvidenceSummary, TurnKey};

/// A unique id for one pending permission request on a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PermissionRequestId(pub u64);

impl std::fmt::Display for PermissionRequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Why an agent connection went away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionCloseReason {
    /// The connection was closed by the host (drop, shutdown, `:agents_stop`).
    Closed,
    /// The subprocess exited on its own.
    ChildExited { status: Option<i32> },
    /// The transport failed or the agent stopped speaking ACP.
    Transport(String),
}

/// Lifecycle state of one agent connection.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentConnectionState {
    /// The subprocess is starting.
    Starting,
    /// `initialize` is in flight.
    Initializing,
    /// ACP v1 negotiated; sessions can be created.
    Ready {
        /// Agent identity from `initialize` (when provided).
        agent_info: Option<Box<Implementation>>,
        /// Capabilities the agent advertised.
        agent_capabilities: Box<AgentCapabilities>,
        /// Authentication methods the agent advertised (empty = anonymous).
        auth_methods: Vec<AuthMethod>,
    },
    /// The connection failed before or during the handshake.
    Failed(AgentError),
    /// The connection is gone.
    Closed(ConnectionCloseReason),
}

/// View of a pending permission request for UI rendering.
///
/// Carries no response channel: decisions go through
/// [`crate::session::AgentThread::respond_permission`].
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequestInfo {
    pub request_id: PermissionRequestId,
    pub session_id: SessionId,
    /// The tool call needing approval (ACP `session/request_permission`).
    pub tool_call: ToolCallUpdate,
    /// Options the user may pick from.
    pub options: Vec<PermissionOption>,
}

/// Wall-clock and token metrics for one completed agent turn.
///
/// Counter-only: elapsed time plus token counts the agent reported.  Never
/// carries prompts, output, or any other content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnMetrics {
    /// Wall-clock duration from turn start to its terminal event.
    pub elapsed: std::time::Duration,
    /// Token usage reported by the agent for the whole turn, when known.
    /// Unknown (e.g. cancelled or failed turns) stays `None`, never zero.
    pub tokens: Option<ee_agent_protocol::Usage>,
}

/// Structured recoverable-turn payload deserialized from the agent's
/// JSON-RPC error `data.recoverable` field.  Mirrors the server-side wire
/// type; the host never synthesizes one.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RecoverableInfo {
    /// Fault class label (`deadline`, `transient`, `rate_limited`, ...).
    pub fault: String,
    /// Human-readable summary of why the turn paused.
    pub detail: String,
    /// Underlying cause, when the agent reported one.
    #[serde(default)]
    pub cause: Option<String>,
    /// Whether resuming is safe without user confirmation.
    pub safe_resume: bool,
    /// Server retry hint in milliseconds, when reported.
    #[serde(default)]
    pub retry_after: Option<u64>,
    /// Durable checkpoint identity, when the agent persisted one.
    #[serde(default)]
    pub checkpoint_id: Option<String>,
    /// Tool calls whose results are already durable.
    #[serde(default)]
    pub completed_tool_calls: u64,
    /// How many times this turn has been resumed already.
    #[serde(default)]
    pub resumed_count: u32,
}

/// Every observable host state change.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// The connection moved through its lifecycle.
    ConnectionStateChanged { agent_id: String, state: AgentConnectionState },
    /// A new session thread was created and is ready for prompts.
    ThreadCreated { agent_id: String, session_id: SessionId },
    /// A session thread was closed (host shutdown or connection loss).
    ThreadClosed { agent_id: String, session_id: SessionId, reason: ThreadCloseReason },
    /// A prompt turn started (optimistic user message already reduced).
    TurnStarted { session_id: SessionId, turn: TurnKey },
    /// One `session/update` notification was reduced into session state.
    ///
    /// The UI re-reads the thread snapshot; the raw update is included for
    /// diagnostics and Phase 3 regression tests.
    SessionUpdate { session_id: SessionId, update: Box<SessionUpdate> },
    /// The running ACP transport turn completed with a stop reason.
    ///
    /// This is protocol lifecycle only. It does not establish that editor
    /// changes were reviewed, validated, or verified; see
    /// [`AgentEvent::TurnEvidenceUpdated`].
    TurnCompleted { session_id: SessionId, stop_reason: StopReason, metrics: TurnMetrics },
    /// Host-derived evidence state changed for one turn.
    ///
    /// Payload is bounded and ACP-wire-independent, safe for UI and future
    /// MCP retrieval surfaces. It never includes prompt text, raw tool output,
    /// terminal output, or model-declared completion claims.
    TurnEvidenceUpdated { session_id: SessionId, summary: Box<TurnEvidenceSummary> },
    /// The running turn was cancelled locally or by the agent.
    TurnCancelled { session_id: SessionId, metrics: TurnMetrics },
    /// The running turn failed.
    TurnFailed { session_id: SessionId, error: AgentError, metrics: TurnMetrics },
    /// The running turn stopped on a recoverable interruption: completed
    /// work is durable and the same thread may resume (re-send the same
    /// prompt) or discard (send `/discard`).
    TurnPausedRecoverable {
        session_id: SessionId,
        metrics: TurnMetrics,
        recoverable: Box<RecoverableInfo>,
    },
    /// The agent requested permission for a tool call.
    PermissionRequested { session_id: SessionId, request: Box<PermissionRequestInfo> },
    /// A permission decision was recorded (from the user or cancellation).
    PermissionResolved {
        session_id: SessionId,
        request_id: PermissionRequestId,
        outcome: RequestPermissionOutcome,
    },
    /// An out-of-band URL elicitation finished.
    ///
    /// The UI may use this to clear pending URL prompts or mark transcript
    /// state complete. Unknown/stale ids stay diagnostics-only.
    ElicitationCompleted { elicitation_id: ElicitationId },
    /// An agent-to-client file/terminal/elicitation request was dispatched
    /// to the registered handler.
    ClientRequestDispatched { session_id: Option<SessionId>, method: String },
    /// A stderr line was captured from the agent subprocess.
    ///
    /// Bounded: the host keeps at most [`crate::process::STDERR_MAX_LINES`]
    /// lines and truncates each line.
    StderrLine { agent_id: String, line: String },
}

/// Why a session thread closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadCloseReason {
    /// The host closed the connection.
    HostClosed,
    /// The connection dropped before the session could be shut down.
    ConnectionLost,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_request_ids_display_as_numbers() {
        assert_eq!(PermissionRequestId(7).to_string(), "7");
    }
}
