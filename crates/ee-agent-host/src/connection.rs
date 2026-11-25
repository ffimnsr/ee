//! Agent connection: one agent subprocess speaking ACP v1 over stdio.
//!
//! The connection owns:
//!
//! - the subprocess lifecycle (`command`/`args`/`env`/`cwd`, kill on drop);
//! - the ACP `initialize` handshake with strict v1-only negotiation and a
//!   bounded timeout;
//! - the session command driver (prompt, cancel, set-mode, authenticate,
//!   session/new, session/load, logout) with per-request timeouts and an
//!   explicit cancellation path for turns;
//! - concurrent prompt scheduling across sessions on one connection while
//!   preserving one active turn per session. Non-prompt commands retain FIFO
//!   driver ordering and remain responsive while prompt responses are pending;
//! - inbound dispatch: `session/update` notifications route to session
//!   threads, `session/request_permission` goes through the permission
//!   broker, and file/terminal/elicitation requests go to the registered
//!   [`ClientRequestHandler`] after a capability gate.
//!
//! The SDK's `Builder`/`ConnectionTo` machinery does the JSON-RPC framing,
//! request correlation, and response routing; this module only adapts it to
//! a tokio subprocess and the host lifecycle.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ee_agent_protocol::{
    Agent as AgentRole, AgentCapabilities, AuthenticateRequest, AuthenticateResponse,
    BooleanConfigOptionCapabilities, CancelNotification, CancelRequestNotification,
    Client as ClientRole, ClientCapabilities, ClientSessionCapabilities, CloseSessionRequest,
    CloseSessionResponse, CompleteElicitationNotification, ConnectMcpRequest, ConnectionTo,
    CreateElicitationRequest, CreateTerminalRequest, DeleteSessionRequest, DeleteSessionResponse,
    DisconnectMcpRequest, ElicitationCapabilities, ElicitationFormCapabilities,
    ElicitationUrlCapabilities, Error as RpcError, FileSystemCapabilities, Implementation,
    InitializeRequest, InitializeResponse, KillTerminalRequest, ListSessionsRequest,
    ListSessionsResponse, LoadSessionRequest, LoadSessionResponse, LogoutRequest, LogoutResponse,
    McpServer, McpServerAcpId, McpServerStdio, MessageMcpNotification, MessageMcpRequest,
    NewSessionRequest, NewSessionResponse, PromptDelivery, PromptRequest, PromptResponse,
    ProtocolVersion, ReadTextFileRequest, ReleaseTerminalRequest, RequestId,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    ResumeSessionRequest, ResumeSessionResponse, SessionConfigOption, SessionConfigOptionValue,
    SessionConfigOptionsCapabilities, SessionId, SessionNotification,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, SetSessionModeRequest,
    SetSessionModeResponse, StopReason, TerminalOutputRequest, WaitForTerminalExitRequest,
    WriteTextFileRequest, agent_capabilities_from_v2, on_receive_notification, on_receive_request,
    v2,
};
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::error::AgentError;
use crate::events::{AgentConnectionState, AgentEvent, ConnectionCloseReason, PermissionRequestId};
use crate::inbound::{ClientRequest, ClientRequestHandler, HandlerCapabilities};
use crate::mcp_over_acp::{EeProxyMode, EeProxyToolProfile, McpOverAcpRegistry};
use crate::permission::PermissionBroker;
use crate::permission_rules::PermissionRules;
use crate::process::{AgentProcess, AgentProcessConfig, spawn_stderr_reader};
use crate::session::{AgentThread, ThreadShared};
use crate::workspace_memory::WorkspaceMemoryHost;

/// Default timeout for the ACP `initialize` handshake.
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Default timeout for non-prompt ACP requests.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Default simultaneous prompt limit for sessions sharing one connection.
pub const DEFAULT_MAX_CONCURRENT_PROMPTS: usize = 4;

/// Connection tuning knobs (tests use tiny timeouts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentConnectionOptions {
    /// Timeout for `initialize` negotiation.
    pub handshake_timeout: Duration,
    /// Timeout for `session/new`, `session/load`, `session/set_mode`,
    /// `authenticate`, and `logout` requests.
    pub request_timeout: Duration,
    /// The ACP protocol version this connection attempts in `initialize`.
    ///
    /// `V2` is the draft v2 surface: the SDK pins one wire surface per
    /// connection, so a v2-mode connection requires a v2-answering agent;
    /// agents that answer v1 fail the handshake with a clear error (the
    /// sdk's `ClientProtocolConnector` — a future migration phase — can fall
    /// back to a fresh v1 connection instead).  Defaults to `V1`, which
    /// matches every pre-existing agent.
    pub protocol_version: ProtocolVersion,
    /// Whether the ee MCP proxy is configured (arms ACP-native MCP-over-ACP
    /// hosting for this connection; the agent still has to advertise
    /// `mcp_capabilities.acp` before anything is served). Enabled by default;
    /// hosts opt out explicitly when they do not want editor tools exposed.
    pub ee_proxy_enabled: bool,
    /// Maximum prompt requests sent concurrently on one agent connection.
    /// Additional cross-session prompts wait FIFO; same-session overlap rejects.
    pub max_concurrent_prompts: usize,
    /// Connection-owned ee MCP tool exposure profile.
    pub ee_proxy_tool_profile: EeProxyToolProfile,
}

impl Default for AgentConnectionOptions {
    fn default() -> Self {
        Self {
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            protocol_version: ProtocolVersion::V1,
            ee_proxy_enabled: true,
            max_concurrent_prompts: DEFAULT_MAX_CONCURRENT_PROMPTS,
            ee_proxy_tool_profile: EeProxyToolProfile::Full,
        }
    }
}

/// Commands the session driver executes on the connection.
pub(crate) enum ConnectionCommand {
    NewSession {
        request: NewSessionRequest,
        tx: oneshot::Sender<Result<NewSessionResponse, AgentError>>,
    },
    LoadSession {
        request: LoadSessionRequest,
        tx: oneshot::Sender<Result<LoadSessionResponse, AgentError>>,
    },
    ListSessions {
        request: ListSessionsRequest,
        tx: oneshot::Sender<Result<ListSessionsResponse, AgentError>>,
    },
    DeleteSession {
        request: DeleteSessionRequest,
        tx: oneshot::Sender<Result<DeleteSessionResponse, AgentError>>,
    },
    ResumeSession {
        request: ResumeSessionRequest,
        tx: oneshot::Sender<Result<ResumeSessionResponse, AgentError>>,
    },
    CloseSession {
        request: CloseSessionRequest,
        tx: oneshot::Sender<Result<CloseSessionResponse, AgentError>>,
    },
    SetMode {
        request: SetSessionModeRequest,
        tx: oneshot::Sender<Result<SetSessionModeResponse, AgentError>>,
    },
    SetConfigOption {
        request: SetSessionConfigOptionRequest,
        tx: oneshot::Sender<Result<SetSessionConfigOptionResponse, AgentError>>,
    },
    SetModel {
        request: ee_agent_protocol::SetSessionModelRequest,
        tx: oneshot::Sender<Result<ee_agent_protocol::SetSessionModelResponse, AgentError>>,
    },
    Authenticate {
        request: AuthenticateRequest,
        tx: oneshot::Sender<Result<AuthenticateResponse, AgentError>>,
    },
    Logout {
        request: LogoutRequest,
        tx: oneshot::Sender<Result<LogoutResponse, AgentError>>,
    },
    Prompt {
        request: PromptRequest,
        cancel: watch::Receiver<bool>,
        tx: oneshot::Sender<Result<PromptResponse, AgentError>>,
    },
    /// Delivers a prompt to a running turn (or its queue) without taking the
    /// host's per-session turn reservation.
    SteerPrompt {
        request: PromptRequest,
        tx: oneshot::Sender<Result<PromptResponse, AgentError>>,
    },
    CancelSession {
        session_id: SessionId,
    },
    Close,
}

pub(crate) struct AgentConnectionInner {
    pub agent_id: String,
    pub state: watch::Sender<AgentConnectionState>,
    pub commands: mpsc::UnboundedSender<ConnectionCommand>,
    pub broker: PermissionBroker,
    pub events: mpsc::UnboundedSender<AgentEvent>,
    pub handler: Arc<dyn ClientRequestHandler>,
    pub handler_capabilities: HandlerCapabilities,
    pub workspace_memory: Arc<WorkspaceMemoryHost>,
    pub process: Arc<Mutex<Option<AgentProcess>>>,
    pub threads: Arc<Mutex<HashMap<SessionId, Arc<ThreadShared>>>>,
    /// ACP-native MCP-over-ACP hosting for the ee proxy.
    pub mcp: McpOverAcpRegistry,
    /// The protocol version negotiated at `initialize` (set once the
    /// handshake succeeds); v2 turns end through `v2_turn_ends`.
    pub negotiated_protocol_version: Mutex<Option<ProtocolVersion>>,
    /// Pending v2 turn-end waiters keyed by session: the driver registers a
    /// oneshot after the prompt acknowledgment, and the idle `state_update`
    /// resolves it with the agent's stop reason.
    pub v2_turn_ends: Mutex<HashMap<SessionId, oneshot::Sender<StopReason>>>,
    active_url_elicitations: Mutex<HashMap<String, Option<SessionId>>>,
    completed_url_elicitations: Mutex<HashSet<String>>,
    pending_client_requests: Mutex<HashMap<String, watch::Sender<bool>>>,
    shutdown: watch::Sender<bool>,
    closed_once: AtomicBool,
}

impl AgentConnectionInner {
    pub(crate) fn set_state(&self, state: AgentConnectionState) {
        let _ = self.state.send(state.clone());
        let _ = self
            .events
            .send(AgentEvent::ConnectionStateChanged { agent_id: self.agent_id.clone(), state });
    }

    /// Whether the agent advertised `mcp_capabilities.acp` (MCP-over-ACP
    /// support) during `initialize`.
    pub(crate) fn agent_advertises_acp(&self) -> bool {
        matches!(
            &*self.state.borrow(),
            AgentConnectionState::Ready { agent_capabilities, .. }
                if agent_capabilities.mcp_capabilities.acp
        )
    }

    /// Notifies all session threads and the broker that the connection is
    /// gone, so no pending work outlives the process.  Runs at most once.
    pub fn notify_connection_closed(&self, reason: ConnectionCloseReason) {
        if self.closed_once.swap(true, Ordering::SeqCst) {
            return;
        }
        self.broker.cancel_all();
        self.cancel_all_client_requests();
        self.mcp.close_all();
        let threads: Vec<Arc<ThreadShared>> =
            self.threads.lock().expect("threads poisoned").values().cloned().collect();
        for thread in threads {
            thread.notify_connection_lost(reason.clone());
        }
    }

    fn child_exit_status(&self) -> Option<std::process::ExitStatus> {
        self.process
            .lock()
            .expect("process poisoned")
            .as_mut()
            .and_then(|process| process.child().try_wait().ok().flatten())
    }

    fn register_url_elicitation(&self, elicitation_id: &str, session_id: Option<SessionId>) {
        self.active_url_elicitations
            .lock()
            .expect("active url elicitations poisoned")
            .insert(elicitation_id.to_string(), session_id);
    }

    fn finish_url_elicitation(&self, elicitation_id: &str) {
        self.active_url_elicitations
            .lock()
            .expect("active url elicitations poisoned")
            .remove(elicitation_id);
        self.completed_url_elicitations
            .lock()
            .expect("completed url elicitations poisoned")
            .insert(elicitation_id.to_string());
    }

    fn complete_url_elicitation(&self, elicitation_id: &str) -> Option<Option<SessionId>> {
        let session_id = self
            .active_url_elicitations
            .lock()
            .expect("active url elicitations poisoned")
            .remove(elicitation_id)?;
        self.completed_url_elicitations
            .lock()
            .expect("completed url elicitations poisoned")
            .insert(elicitation_id.to_string());
        Some(session_id)
    }

    fn register_client_request(&self, request_id: &RequestId) -> watch::Receiver<bool> {
        let key = request_id_key(request_id);
        let (tx, rx) = watch::channel(false);
        self.pending_client_requests
            .lock()
            .expect("pending client requests poisoned")
            .insert(key, tx);
        rx
    }

    fn cancel_client_request(&self, request_id: &RequestId) -> bool {
        let key = request_id_key(request_id);
        self.pending_client_requests
            .lock()
            .expect("pending client requests poisoned")
            .remove(&key)
            .is_some_and(|cancel| cancel.send(true).is_ok())
    }

    fn finish_client_request(&self, request_id: &RequestId) {
        let key = request_id_key(request_id);
        self.pending_client_requests.lock().expect("pending client requests poisoned").remove(&key);
    }

    fn cancel_all_client_requests(&self) {
        let pending = std::mem::take(
            &mut *self.pending_client_requests.lock().expect("pending client requests poisoned"),
        );
        for cancel in pending.into_values() {
            let _ = cancel.send(true);
        }
    }
}

fn request_id_key(request_id: &RequestId) -> String {
    serde_json::to_string(request_id).unwrap_or_else(|_| format!("{request_id:?}"))
}

/// A handle to one agent connection (cloneable; the last drop kills the
/// subprocess and resolves pending requests).
#[derive(Clone)]
pub struct AgentConnection {
    pub(crate) agent_id: String,
    pub(crate) inner: Arc<AgentConnectionInner>,
}

impl std::fmt::Debug for AgentConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentConnection").field("agent_id", &self.agent_id).finish_non_exhaustive()
    }
}

mod connect;
mod driver;
mod inbound_dispatch;
mod introspect;
mod protocol;
mod sessions;
mod threads;

#[cfg(test)]
mod tests {
    use super::protocol::client_capabilities;
    use super::*;

    #[test]
    fn default_options_are_bounded() {
        let options = AgentConnectionOptions::default();
        assert_eq!(options.handshake_timeout, DEFAULT_HANDSHAKE_TIMEOUT);
        assert_eq!(options.request_timeout, DEFAULT_REQUEST_TIMEOUT);
    }

    #[test]
    fn client_capabilities_reflect_handler_capabilities() {
        let caps = client_capabilities(&HandlerCapabilities::all());
        assert!(caps.fs.read_text_file);
        assert!(caps.fs.write_text_file);
        assert!(caps.terminal);
        assert!(caps.elicitation.is_some());

        let none = client_capabilities(&HandlerCapabilities::none());
        assert!(!none.fs.read_text_file);
        assert!(!none.fs.write_text_file);
        assert!(!none.terminal);
        assert!(none.elicitation.is_none());
    }
}
