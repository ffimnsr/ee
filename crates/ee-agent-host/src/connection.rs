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
    NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, ProtocolVersion,
    ReadTextFileRequest, ReleaseTerminalRequest, RequestId, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, ResumeSessionRequest,
    ResumeSessionResponse, SessionConfigOption, SessionConfigOptionValue,
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
    /// `mcp_capabilities.acp` before anything is served).
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
            ee_proxy_enabled: false,
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

impl AgentConnection {
    /// Spawns the agent subprocess and starts the ACP v1 handshake.
    ///
    /// Returns immediately; the connection is ready once
    /// [`Self::wait_ready`] resolves.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::SpawnFailed`] when the subprocess cannot start.
    pub async fn connect(
        agent_id: String,
        config: AgentProcessConfig,
        handler: Arc<dyn ClientRequestHandler>,
        events: mpsc::UnboundedSender<AgentEvent>,
        options: AgentConnectionOptions,
    ) -> Result<Self, AgentError> {
        let mut process = AgentProcess::spawn(&config).await?;
        let stderr = process.take_stderr();
        let stderr_state = process.stderr_state();
        spawn_stderr_reader(stderr, stderr_state, agent_id.clone(), events.clone());

        // The host writes into the child's stdin and reads its stdout; both
        // directions are newline-delimited JSON-RPC.
        let transport = {
            let stdin = process.take_stdin();
            let stdout = process.take_stdout();
            ee_agent_protocol::ByteStreams::new(stdin.compat_write(), stdout.compat())
        };

        Self::start_connection(
            agent_id,
            Some(process),
            handler,
            events,
            options,
            WorkspaceMemoryHost::disabled(),
            transport,
        )
    }

    pub(crate) async fn connect_with_workspace_memory(
        agent_id: String,
        config: AgentProcessConfig,
        handler: Arc<dyn ClientRequestHandler>,
        events: mpsc::UnboundedSender<AgentEvent>,
        options: AgentConnectionOptions,
        workspace_memory: Arc<WorkspaceMemoryHost>,
    ) -> Result<Self, AgentError> {
        let mut process = AgentProcess::spawn(&config).await?;
        let stderr = process.take_stderr();
        let stderr_state = process.stderr_state();
        spawn_stderr_reader(stderr, stderr_state, agent_id.clone(), events.clone());
        let transport = ee_agent_protocol::ByteStreams::new(
            process.take_stdin().compat_write(),
            process.take_stdout().compat(),
        );
        Self::start_connection(
            agent_id,
            Some(process),
            handler,
            events,
            options,
            workspace_memory,
            transport,
        )
    }

    /// Connects over an injected transport instead of a subprocess (fake
    /// agent harness; test-utils only).
    #[cfg(feature = "test-utils")]
    #[allow(clippy::too_many_arguments)]
    pub fn connect_with_transport(
        agent_id: String,
        handler: Arc<dyn ClientRequestHandler>,
        events: mpsc::UnboundedSender<AgentEvent>,
        options: AgentConnectionOptions,
        transport: impl ee_agent_protocol::ConnectTo<ClientRole> + 'static,
    ) -> Result<Self, AgentError> {
        Self::start_connection(
            agent_id,
            None,
            handler,
            events,
            options,
            WorkspaceMemoryHost::disabled(),
            transport,
        )
    }

    #[cfg(feature = "test-utils")]
    pub(crate) fn connect_with_transport_and_workspace_memory(
        agent_id: String,
        handler: Arc<dyn ClientRequestHandler>,
        events: mpsc::UnboundedSender<AgentEvent>,
        options: AgentConnectionOptions,
        workspace_memory: Arc<WorkspaceMemoryHost>,
        transport: impl ee_agent_protocol::ConnectTo<ClientRole> + 'static,
    ) -> Result<Self, AgentError> {
        Self::start_connection(
            agent_id,
            None,
            handler,
            events,
            options,
            workspace_memory,
            transport,
        )
    }

    /// Shared connection bootstrap: channels, state, driver, and handshake.
    fn start_connection(
        agent_id: String,
        process: Option<AgentProcess>,
        handler: Arc<dyn ClientRequestHandler>,
        events: mpsc::UnboundedSender<AgentEvent>,
        options: AgentConnectionOptions,
        workspace_memory: Arc<WorkspaceMemoryHost>,
        transport: impl ee_agent_protocol::ConnectTo<ClientRole> + 'static,
    ) -> Result<Self, AgentError> {
        let (commands_tx, commands_rx) = mpsc::unbounded_channel();
        let (state_tx, _state_rx) = watch::channel(AgentConnectionState::Starting);
        let (terminate_tx, terminate_rx) = watch::channel(false);
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let handler_capabilities = handler.capabilities();
        let broker = PermissionBroker::new();
        let process = Arc::new(Mutex::new(process));
        let threads = Arc::new(Mutex::new(HashMap::new()));
        let mcp = McpOverAcpRegistry::new(
            options.ee_proxy_enabled,
            &agent_id,
            handler.clone(),
            process.clone(),
            threads.clone(),
            workspace_memory.clone(),
            options.ee_proxy_tool_profile,
        );

        let inner = Arc::new(AgentConnectionInner {
            agent_id: agent_id.clone(),
            state: state_tx,
            commands: commands_tx,
            broker: broker.clone(),
            events: events.clone(),
            handler: handler.clone(),
            handler_capabilities,
            workspace_memory,
            process,
            threads,
            mcp,
            active_url_elicitations: Mutex::new(HashMap::new()),
            completed_url_elicitations: Mutex::new(HashSet::new()),
            pending_client_requests: Mutex::new(HashMap::new()),
            shutdown: shutdown_tx,
            closed_once: AtomicBool::new(false),
            negotiated_protocol_version: Mutex::new(None),
            v2_turn_ends: Mutex::new(HashMap::new()),
        });

        let is_v2 = match options.protocol_version {
            ProtocolVersion::V1 => false,
            ProtocolVersion::V2 => true,
            other => {
                return Err(AgentError::invalid_params(format!(
                    "unsupported protocol version for a host connection: {}",
                    other.as_u16()
                )));
            }
        };

        // The main_fn closure runs the handshake, spawns the session driver,
        // and waits for shutdown/EOF.  It is a concrete async closure here so
        // the connection future stays `Send` for `tokio::spawn`.
        let main_inner = inner.clone();
        let main_fn = async move |connection: ConnectionTo<AgentRole>| -> Result<(), RpcError> {
            // The handshake serializes the typed response into a plain value
            // so one closure serves both version surfaces; the version-specific
            // parse happens after the timeout.
            let handshake = async {
                if is_v2 {
                    let initialize = v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("ee", env!("CARGO_PKG_VERSION")).title("ee"),
                    )
                    .capabilities(v2::ClientCapabilities::new());
                    match connection.send_request(initialize).block_task().await {
                        Ok(response) => {
                            serde_json::to_value(response).map_err(|_| RpcError::internal_error())
                        }
                        Err(error) => Err(error),
                    }
                } else {
                    let initialize = InitializeRequest::new(ProtocolVersion::V1)
                        .client_info(
                            Implementation::new("ee", env!("CARGO_PKG_VERSION")).title("ee"),
                        )
                        .client_capabilities(client_capabilities(&main_inner.handler_capabilities));
                    match connection.send_request(initialize).block_task().await {
                        Ok(response) => {
                            serde_json::to_value(response).map_err(|_| RpcError::internal_error())
                        }
                        Err(error) => Err(error),
                    }
                }
            };
            match tokio::time::timeout(options.handshake_timeout, handshake).await {
                Ok(Ok(response_value)) => {
                    if is_v2 {
                        let response: v2::InitializeResponse =
                            serde_json::from_value(response_value).map_err(|error| {
                                RpcError::internal_error()
                                    .data(format!("v2 initialize response did not parse: {error}"))
                            })?;
                        // v2 response: role-agnostic info + session-nested
                        // capabilities, converted to the v1-shaped state the
                        // rest of the host consumes.  The SDK compat layer
                        // already rejected a v1 answer before this point.
                        let info = implementation_to_v1(&response.info);
                        let capabilities = agent_capabilities_from_v2(&response.capabilities);
                        let auth_methods = response
                            .auth_methods
                            .iter()
                            .filter_map(|method| {
                                serde_json::from_value(serde_json::to_value(method).ok()?).ok()
                            })
                            .collect();
                        *main_inner
                            .negotiated_protocol_version
                            .lock()
                            .expect("negotiated version poisoned") = Some(ProtocolVersion::V2);
                        main_inner.set_state(AgentConnectionState::Ready {
                            agent_info: Some(Box::new(info)),
                            agent_capabilities: Box::new(capabilities),
                            auth_methods,
                        });
                    } else {
                        let response: InitializeResponse = serde_json::from_value(response_value)
                            .map_err(|error| {
                            RpcError::internal_error()
                                .data(format!("v1 initialize response did not parse: {error}"))
                        })?;
                        if response.protocol_version != ProtocolVersion::V1 {
                            let error = AgentError::UnsupportedProtocolVersion {
                                agent_id: main_inner.agent_id.clone(),
                                version: format!("{:?}", response.protocol_version),
                            };
                            main_inner.set_state(AgentConnectionState::Failed(error));
                            main_inner.notify_connection_closed(ConnectionCloseReason::Transport(
                                "unsupported protocol version".into(),
                            ));
                            return Ok(());
                        }
                        main_inner.set_state(AgentConnectionState::Ready {
                            agent_info: response.agent_info.map(Box::new),
                            agent_capabilities: Box::new(response.agent_capabilities),
                            auth_methods: response.auth_methods,
                        });
                    }
                }
                Ok(Err(error)) => {
                    main_inner.set_state(AgentConnectionState::Failed(AgentError::Rpc(error)));
                    main_inner.notify_connection_closed(ConnectionCloseReason::Transport(
                        "initialize rejected".into(),
                    ));
                    return Ok(());
                }
                Err(_) => {
                    let error =
                        AgentError::HandshakeTimeout { agent_id: main_inner.agent_id.clone() };
                    main_inner.set_state(AgentConnectionState::Failed(error));
                    main_inner.notify_connection_closed(ConnectionCloseReason::Transport(
                        "handshake timed out".into(),
                    ));
                    return Ok(());
                }
            }

            let driver_connection = connection.clone();
            tokio::spawn(driver_loop(
                driver_connection,
                commands_rx,
                terminate_rx,
                main_inner.state.subscribe(),
                options.request_timeout,
                options.max_concurrent_prompts.max(1),
                main_inner.clone(),
                is_v2,
            ));

            tokio::select! {
                _ = connection.incoming_closed() => {
                    let reason = match main_inner.child_exit_status() {
                        Some(status) => ConnectionCloseReason::ChildExited { status: status.code() },
                        None => ConnectionCloseReason::Transport("agent stdout closed".into()),
                    };
                    main_inner.set_state(AgentConnectionState::Closed(reason.clone()));
                    main_inner.notify_connection_closed(reason);
                }
                _ = shutdown_rx.changed() => {
                    main_inner.set_state(AgentConnectionState::Closed(ConnectionCloseReason::Closed));
                    main_inner.notify_connection_closed(ConnectionCloseReason::Closed);
                }
            }
            let _ = terminate_tx.send(true);
            Ok(())
        };

        let task_inner = inner.clone();
        let mut transport = Some(transport);
        tokio::spawn(async move {
            // Each version surface builds its own typed handler chain; only
            // one branch runs per connection.
            let result = if is_v2 {
                let transport = transport.take().expect("transport taken once");
                build_client_builder_v2(task_inner.clone()).connect_with(transport, main_fn).await
            } else {
                let transport = transport.take().expect("transport taken once");
                build_client_builder(task_inner.clone()).connect_with(transport, main_fn).await
            };
            match result {
                Ok(()) => {
                    // main_fn already moved the state to Failed/Closed.
                    let state = task_inner.state.borrow().clone();
                    if matches!(state, AgentConnectionState::Ready { .. }) {
                        task_inner
                            .set_state(AgentConnectionState::Closed(ConnectionCloseReason::Closed));
                        task_inner.notify_connection_closed(ConnectionCloseReason::Closed);
                    }
                }
                Err(error) => {
                    let state = AgentConnectionState::Failed(AgentError::Rpc(error));
                    task_inner.set_state(state);
                    task_inner.notify_connection_closed(ConnectionCloseReason::Transport(
                        "connection failed".into(),
                    ));
                }
            }
        });

        Ok(Self { agent_id, inner })
    }

    /// The configured agent id.
    #[must_use]
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Current connection state.
    #[must_use]
    pub fn state(&self) -> AgentConnectionState {
        self.inner.state.borrow().clone()
    }

    /// Agent implementation metadata from initialize, once ready.
    #[must_use]
    pub fn agent_info(&self) -> Option<ee_agent_protocol::Implementation> {
        match self.state() {
            AgentConnectionState::Ready { agent_info, .. } => {
                agent_info.map(|info| (*info).clone())
            }
            _ => None,
        }
    }

    /// The agent's negotiated capabilities, once ready.
    #[must_use]
    pub fn agent_capabilities(&self) -> Option<AgentCapabilities> {
        match self.state() {
            AgentConnectionState::Ready { agent_capabilities, .. } => {
                Some((*agent_capabilities).clone())
            }
            _ => None,
        }
    }

    /// Authentication methods the agent advertised, once ready.
    #[must_use]
    pub fn auth_methods(&self) -> Vec<ee_agent_protocol::AuthMethod> {
        match self.state() {
            AgentConnectionState::Ready { auth_methods, .. } => auth_methods,
            _ => Vec::new(),
        }
    }

    /// Whether the agent advertises `session/load`.
    #[must_use]
    pub fn supports_load_session(&self) -> bool {
        self.agent_capabilities().is_some_and(|capabilities| capabilities.load_session)
    }

    /// The protocol version negotiated at `initialize`, once the handshake
    /// succeeded.
    #[must_use]
    pub fn negotiated_protocol_version(&self) -> Option<ProtocolVersion> {
        *self.inner.negotiated_protocol_version.lock().expect("negotiated version poisoned")
    }

    /// Whether the agent advertises `session/list` (v2 baseline: `session`
    /// support commits the agent to list, resume, and close).
    #[must_use]
    pub fn supports_session_list(&self) -> bool {
        if self.negotiated_protocol_version() == Some(ProtocolVersion::V2) {
            return true;
        }
        self.agent_capabilities()
            .is_some_and(|capabilities| capabilities.session_capabilities.list.is_some())
    }

    /// Whether the agent advertises `session/delete`.
    #[must_use]
    pub fn supports_session_delete(&self) -> bool {
        self.agent_capabilities()
            .is_some_and(|capabilities| capabilities.session_capabilities.delete.is_some())
    }

    /// Whether the agent advertises `session/resume` (v2 baseline: `session`
    /// support commits the agent to list, resume, and close).
    #[must_use]
    pub fn supports_session_resume(&self) -> bool {
        if self.negotiated_protocol_version() == Some(ProtocolVersion::V2) {
            return true;
        }
        self.agent_capabilities()
            .is_some_and(|capabilities| capabilities.session_capabilities.resume.is_some())
    }

    /// Whether the agent advertises `session/close` (v2 baseline: `session`
    /// support commits the agent to list, resume, and close).
    #[must_use]
    pub fn supports_session_close(&self) -> bool {
        if self.negotiated_protocol_version() == Some(ProtocolVersion::V2) {
            return true;
        }
        self.agent_capabilities()
            .is_some_and(|capabilities| capabilities.session_capabilities.close.is_some())
    }

    /// Whether the agent advertises `additionalDirectories` on supported
    /// session lifecycle methods.
    #[must_use]
    pub fn supports_additional_directories(&self) -> bool {
        self.agent_capabilities().is_some_and(|capabilities| {
            capabilities.session_capabilities.additional_directories.is_some()
        })
    }

    /// Whether this client advertised boolean session config option support.
    #[must_use]
    pub fn supports_boolean_session_config_options(&self) -> bool {
        self.inner.handler_capabilities.session_config_boolean
    }

    /// Whether the agent advertises prompt image support.
    #[must_use]
    pub fn supports_prompt_images(&self) -> bool {
        self.agent_capabilities().is_some_and(|capabilities| capabilities.prompt_capabilities.image)
    }

    /// Whether the agent advertises prompt audio support.
    #[must_use]
    pub fn supports_prompt_audio(&self) -> bool {
        self.agent_capabilities().is_some_and(|capabilities| capabilities.prompt_capabilities.audio)
    }

    /// Whether the agent advertises embedded prompt context support.
    #[must_use]
    pub fn supports_prompt_embedded_context(&self) -> bool {
        self.agent_capabilities()
            .is_some_and(|capabilities| capabilities.prompt_capabilities.embedded_context)
    }

    /// Whether the agent advertises `logout` support.
    #[must_use]
    pub fn supports_logout(&self) -> bool {
        self.agent_capabilities().is_some_and(|capabilities| capabilities.auth.logout.is_some())
    }

    /// Retained stderr diagnostics for the debug pane.
    #[must_use]
    pub fn stderr_diagnostics(&self) -> Vec<String> {
        self.inner
            .process
            .lock()
            .expect("process poisoned")
            .as_ref()
            .map_or_else(Vec::new, AgentProcess::stderr_snapshot)
    }

    /// The permission broker shared by this connection.
    #[must_use]
    pub fn permission_broker(&self) -> PermissionBroker {
        self.inner.broker.clone()
    }

    /// Waits until the connection is ready or failed.
    ///
    /// # Errors
    ///
    /// Returns the handshake failure or [`AgentError::ConnectionClosed`].
    pub async fn wait_ready(&self) -> Result<(), AgentError> {
        let mut rx = self.inner.state.subscribe();
        loop {
            let state = rx.borrow().clone();
            match state {
                AgentConnectionState::Ready { .. } => return Ok(()),
                AgentConnectionState::Failed(error) => return Err(error),
                AgentConnectionState::Closed(_) => {
                    return Err(AgentError::ConnectionClosed { agent_id: self.agent_id.clone() });
                }
                AgentConnectionState::Starting | AgentConnectionState::Initializing => {
                    if rx.changed().await.is_err() {
                        return Err(AgentError::ConnectionClosed {
                            agent_id: self.agent_id.clone(),
                        });
                    }
                }
            }
        }
    }

    /// Creates a new session on this connection.
    ///
    /// `worktree_roots` must be absolute; the first root becomes the ACP
    /// `cwd` and the rest are forwarded as `additionalDirectories`.
    /// `mcp_servers` are forwarded as ACP `mcpServers` (MCP configuration the
    /// agent may connect to directly); ee never interprets them itself.
    ///
    /// `ee_proxy_stdio_fallback` carries the stdio `ee --mcp-proxy` entry
    /// when proxy mode is configured.  When the agent advertised
    /// `mcp_capabilities.acp` and this connection hosts MCP-over-ACP, the
    /// entry is replaced by an ACP-native [`McpServer::Acp`] `ee` entry
    /// instead; the two modes are mutually exclusive for the `ee` server id.
    /// The resolved mode is available on the returned thread.
    ///
    /// # Errors
    ///
    /// Fails when the connection is not ready, roots are invalid, or the
    /// agent rejects `session/new`.
    pub async fn new_session(
        &self,
        worktree_roots: Vec<PathBuf>,
        mcp_servers: Vec<ee_agent_protocol::McpServer>,
        ee_proxy_stdio_fallback: Option<McpServerStdio>,
    ) -> Result<AgentThread, AgentError> {
        self.wait_ready().await?;
        if worktree_roots.is_empty() {
            return Err(AgentError::invalid_params(
                "session/new requires at least one absolute worktree root (cwd)",
            ));
        }
        for root in &worktree_roots {
            if !root.is_absolute() {
                return Err(AgentError::invalid_params(format!(
                    "worktree root must be absolute, got {}",
                    root.display()
                )));
            }
        }
        let mut roots = worktree_roots.into_iter();
        let cwd = roots.next().expect("non-empty roots");
        let additional = roots.collect::<Vec<_>>();
        let mut request =
            NewSessionRequest::new(cwd).additional_directories(additional).mcp_servers(mcp_servers);
        let proxy_mode =
            self.append_ee_proxy_entry(&mut request.mcp_servers, ee_proxy_stdio_fallback);

        let response =
            self.send_command(|tx| ConnectionCommand::NewSession { request, tx }).await?;
        Ok(self.spawn_thread(
            response.session_id,
            response.modes,
            response.config_options,
            proxy_mode,
        ))
    }

    /// Appends the ee proxy advertisement to a session setup request.
    ///
    /// Returns the mode actually used: ACP-native `McpServer::Acp` when this
    /// connection hosts MCP-over-ACP and the agent advertised `acp` support,
    /// the stdio fallback entry otherwise, or nothing at all.
    fn append_ee_proxy_entry(
        &self,
        mcp_servers: &mut Vec<McpServer>,
        ee_proxy_stdio_fallback: Option<McpServerStdio>,
    ) -> EeProxyMode {
        if let Some(server_id) = self.inner.mcp.server_id()
            && self.agent_capabilities().is_some_and(|caps| caps.mcp_capabilities.acp)
        {
            mcp_servers.push(ee_agent_protocol::ee_proxy_acp_entry(server_id.clone()));
            return EeProxyMode::AcpNative;
        }
        if let Some(fallback) = ee_proxy_stdio_fallback {
            mcp_servers.push(McpServer::Stdio(fallback));
            EeProxyMode::StdioFallback
        } else {
            EeProxyMode::Disabled
        }
    }

    /// Loads an existing session; only allowed when the agent advertises
    /// the `load_session` capability.
    ///
    /// `cwd` and every `additional_directories` entry must be absolute.
    /// `additionalDirectories` is forwarded only when the agent advertises
    /// `sessionCapabilities.additionalDirectories`.
    ///
    /// # Errors
    ///
    /// Fails when the capability is missing, roots are invalid, or the agent
    /// rejects the load.
    pub async fn load_session(
        &self,
        session_id: SessionId,
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        mcp_servers: Vec<McpServer>,
    ) -> Result<AgentThread, AgentError> {
        self.wait_ready().await?;
        if !self.supports_load_session() {
            return Err(AgentError::CapabilityUnsupported { method: "session/load".into() });
        }
        if !cwd.is_absolute() {
            return Err(AgentError::invalid_params(format!(
                "session/load cwd must be absolute, got {}",
                cwd.display()
            )));
        }
        for directory in &additional_directories {
            if !directory.is_absolute() {
                return Err(AgentError::invalid_params(format!(
                    "additional directory must be absolute, got {}",
                    directory.display()
                )));
            }
        }
        let request = LoadSessionRequest::new(session_id.clone(), cwd)
            .mcp_servers(mcp_servers)
            .additional_directories(if self.supports_additional_directories() {
                additional_directories
            } else {
                Vec::new()
            });
        // Register the thread before awaiting the load response so streamed
        // `session/update` notifications can be reduced immediately; ACP does
        // not replay history after `session/load` completes.
        let thread = AgentThread::new(
            self.agent_id.clone(),
            session_id.clone(),
            None,
            None,
            self.clone(),
            EeProxyMode::Disabled,
        );
        self.inner
            .threads
            .lock()
            .expect("threads poisoned")
            .insert(session_id.clone(), thread.shared.clone());
        let response =
            match self.send_command(|tx| ConnectionCommand::LoadSession { request, tx }).await {
                Ok(response) => response,
                Err(error) => {
                    self.deregister_thread(&session_id);
                    return Err(error);
                }
            };
        *thread.shared.modes.lock().expect("modes poisoned") = response.modes;
        thread.set_initial_config_options(response.config_options);
        let _ = self
            .inner
            .events
            .send(AgentEvent::ThreadCreated { agent_id: self.agent_id.clone(), session_id });
        Ok(thread)
    }

    /// Lists existing sessions; only allowed when the agent advertises
    /// `sessionCapabilities.list`.
    ///
    /// `cwd`, when provided, must be absolute. `cursor` stays opaque and is
    /// forwarded unchanged.
    pub async fn list_sessions(
        &self,
        cwd: Option<PathBuf>,
        cursor: Option<String>,
    ) -> Result<ListSessionsResponse, AgentError> {
        self.wait_ready().await?;
        if !self.supports_session_list() {
            return Err(AgentError::CapabilityUnsupported { method: "session/list".into() });
        }
        if let Some(cwd) = cwd.as_ref()
            && !cwd.is_absolute()
        {
            return Err(AgentError::invalid_params(format!(
                "session/list cwd must be absolute, got {}",
                cwd.display()
            )));
        }
        let request = ListSessionsRequest::new().cwd(cwd).cursor(cursor);
        self.send_command(|tx| ConnectionCommand::ListSessions { request, tx }).await
    }

    /// Deletes one existing session; only allowed when the agent advertises
    /// `sessionCapabilities.delete`.
    pub async fn delete_session(
        &self,
        session_id: SessionId,
    ) -> Result<DeleteSessionResponse, AgentError> {
        self.wait_ready().await?;
        if !self.supports_session_delete() {
            return Err(AgentError::CapabilityUnsupported { method: "session/delete".into() });
        }
        let request = DeleteSessionRequest::new(session_id);
        self.send_command(|tx| ConnectionCommand::DeleteSession { request, tx }).await
    }

    /// Resumes an existing session; only allowed when the agent advertises
    /// `sessionCapabilities.resume`.
    ///
    /// `cwd` and every `additional_directories` entry must be absolute.
    /// Non-empty `additional_directories` also require the
    /// `sessionCapabilities.additionalDirectories` capability.
    pub async fn resume_session(
        &self,
        session_id: SessionId,
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        mcp_servers: Vec<McpServer>,
    ) -> Result<AgentThread, AgentError> {
        self.wait_ready().await?;
        if !self.supports_session_resume() {
            return Err(AgentError::CapabilityUnsupported { method: "session/resume".into() });
        }
        if !cwd.is_absolute() {
            return Err(AgentError::invalid_params(format!(
                "session/resume cwd must be absolute, got {}",
                cwd.display()
            )));
        }
        if !additional_directories.is_empty() && !self.supports_additional_directories() {
            return Err(AgentError::CapabilityUnsupported { method: "session/resume".into() });
        }
        for directory in &additional_directories {
            if !directory.is_absolute() {
                return Err(AgentError::invalid_params(format!(
                    "additional directory must be absolute, got {}",
                    directory.display()
                )));
            }
        }
        let request = ResumeSessionRequest::new(session_id.clone(), cwd)
            .additional_directories(additional_directories)
            .mcp_servers(mcp_servers);
        let response =
            self.send_command(|tx| ConnectionCommand::ResumeSession { request, tx }).await?;
        Ok(self.spawn_thread(
            session_id,
            response.modes,
            response.config_options,
            EeProxyMode::Disabled,
        ))
    }

    /// Closes one active session; only allowed when the agent advertises
    /// `sessionCapabilities.close`.
    ///
    /// Local pending work is cancelled and the thread state is released after
    /// the agent acknowledges the close.
    pub async fn close_session(
        &self,
        session_id: SessionId,
    ) -> Result<CloseSessionResponse, AgentError> {
        self.wait_ready().await?;
        if !self.supports_session_close() {
            return Err(AgentError::CapabilityUnsupported { method: "session/close".into() });
        }
        self.prepare_local_thread_for_close(&session_id);
        let request = CloseSessionRequest::new(session_id.clone());
        let response =
            self.send_command(|tx| ConnectionCommand::CloseSession { request, tx }).await?;
        self.close_local_thread(&session_id);
        Ok(response)
    }

    /// Sends `authenticate` for one of the advertised auth methods.
    ///
    /// # Errors
    ///
    /// Fails when the connection is not ready or the agent rejects the
    /// method.
    pub async fn authenticate(
        &self,
        method_id: ee_agent_protocol::AuthMethodId,
    ) -> Result<AuthenticateResponse, AgentError> {
        self.wait_ready().await?;
        let request = AuthenticateRequest::new(method_id);
        self.send_command(|tx| ConnectionCommand::Authenticate { request, tx }).await
    }

    /// Sends `logout`; only allowed when the agent advertised `auth.logout`.
    ///
    /// # Errors
    ///
    /// Fails when the capability is missing or the agent rejects logout.
    pub async fn logout(&self) -> Result<LogoutResponse, AgentError> {
        self.wait_ready().await?;
        if !self.supports_logout() {
            return Err(AgentError::CapabilityUnsupported { method: "logout".into() });
        }
        let request = LogoutRequest::new();
        self.send_command(|tx| ConnectionCommand::Logout { request, tx }).await
    }

    /// Sends one command to the driver and awaits its typed result.
    async fn send_command<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<T, AgentError>>) -> ConnectionCommand,
    ) -> Result<T, AgentError> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .commands
            .send(build(tx))
            .map_err(|_| AgentError::ConnectionClosed { agent_id: self.agent_id.clone() })?;
        rx.await.map_err(|_| AgentError::ConnectionClosed { agent_id: self.agent_id.clone() })?
    }

    /// Creates the session thread handle for a fresh or loaded session.
    fn spawn_thread(
        &self,
        session_id: SessionId,
        modes: Option<ee_agent_protocol::SessionModeState>,
        config_options: Option<Vec<SessionConfigOption>>,
        proxy_mode: EeProxyMode,
    ) -> AgentThread {
        let thread = AgentThread::new(
            self.agent_id.clone(),
            session_id.clone(),
            modes,
            config_options,
            self.clone(),
            proxy_mode,
        );
        self.inner
            .threads
            .lock()
            .expect("threads poisoned")
            .insert(session_id.clone(), thread.shared.clone());
        let _ = self
            .inner
            .events
            .send(AgentEvent::ThreadCreated { agent_id: self.agent_id.clone(), session_id });
        thread
    }

    /// Sends a prompt for a session turn (used by [`AgentThread`]).
    pub(crate) async fn send_prompt(
        &self,
        session_id: SessionId,
        prompt: Vec<ee_agent_protocol::ContentBlock>,
        cancel: watch::Receiver<bool>,
    ) -> Result<PromptResponse, AgentError> {
        let request = PromptRequest::new(session_id, prompt);
        self.send_command(|tx| ConnectionCommand::Prompt { request, cancel, tx }).await
    }

    /// Sends the `session/cancel` notification for a session.
    pub(crate) fn send_session_cancel(&self, session_id: SessionId) {
        let _ = self.inner.commands.send(ConnectionCommand::CancelSession { session_id });
    }

    /// Sends `session/set_mode` for a session (used by [`AgentThread`]).
    pub(crate) async fn set_mode(
        &self,
        session_id: SessionId,
        mode_id: ee_agent_protocol::SessionModeId,
    ) -> Result<SetSessionModeResponse, AgentError> {
        let request = SetSessionModeRequest::new(session_id, mode_id);
        self.send_command(|tx| ConnectionCommand::SetMode { request, tx }).await
    }

    /// Sends `session/set_config_option` for a session (used by [`AgentThread`]).
    pub(crate) async fn set_config_option(
        &self,
        session_id: SessionId,
        config_id: ee_agent_protocol::SessionConfigId,
        value: SessionConfigOptionValue,
    ) -> Result<SetSessionConfigOptionResponse, AgentError> {
        let request = SetSessionConfigOptionRequest::new(session_id, config_id, value);
        self.send_command(|tx| ConnectionCommand::SetConfigOption { request, tx }).await
    }

    /// Resolves a pending permission request; returns `false` for stale or
    /// unknown ids (duplicate-response guard).
    pub fn respond_permission(
        &self,
        request_id: PermissionRequestId,
        outcome: RequestPermissionOutcome,
    ) -> bool {
        self.inner.broker.respond(request_id, outcome)
    }

    /// Cancels every pending permission for a session.
    pub(crate) fn cancel_session_permissions(&self, session_id: &SessionId) -> usize {
        self.inner.broker.cancel_session(session_id)
    }

    /// The advertised ACP server id for the ee proxy, when this connection
    /// hosts ACP-native MCP-over-ACP (proxy mode configured).
    #[must_use]
    pub fn ee_proxy_server_id(&self) -> Option<McpServerAcpId> {
        self.inner.mcp.server_id().cloned()
    }

    /// Closes the connection: stops the driver, kills the subprocess, and
    /// resolves pending work.
    pub async fn close(&self) {
        let _ = self.inner.commands.send(ConnectionCommand::Close);
        let _ = self.inner.shutdown.send(true);
        self.inner.mcp.close_all();
        let process = self.inner.process.lock().expect("process poisoned").take();
        if let Some(process) = process {
            process.kill().await;
        }
        self.inner.notify_connection_closed(ConnectionCloseReason::Closed);
    }

    /// Deregisters a session thread (closing it also closes every logical
    /// MCP connection on this agent connection).
    pub(crate) fn deregister_thread(&self, session_id: &SessionId) {
        self.inner.threads.lock().expect("threads poisoned").remove(session_id);
        self.inner.mcp.close_all();
    }

    fn prepare_local_thread_for_close(&self, session_id: &SessionId) {
        let thread = self.inner.threads.lock().expect("threads poisoned").get(session_id).cloned();
        if let Some(thread) = thread
            && let Some(cancel) = thread
                .turn
                .lock()
                .expect("turn state poisoned")
                .take()
                .map(crate::session::RunningTurn::cancel)
        {
            let _ = cancel.send(true);
        }
        self.cancel_session_permissions(session_id);
    }

    fn close_local_thread(&self, session_id: &SessionId) {
        let thread = self.inner.threads.lock().expect("threads poisoned").remove(session_id);
        let Some(_thread) = thread else {
            self.inner.mcp.close_all();
            return;
        };
        self.inner.mcp.close_all();
        let _ = self.inner.events.send(AgentEvent::ThreadClosed {
            agent_id: self.agent_id.clone(),
            session_id: session_id.clone(),
            reason: crate::events::ThreadCloseReason::HostClosed,
        });
    }
}

/// Builds the SDK client with typed handlers for every agent-to-client
/// request ACP v1 defines.
/// Builds the SDK client with typed handlers for every agent-to-client
/// request ACP v1 defines.
fn build_client_builder(
    inner: Arc<AgentConnectionInner>,
) -> ee_agent_protocol::Builder<
    ClientRole,
    impl ee_agent_protocol::HandleDispatchFrom<AgentRole>,
    impl ee_agent_protocol::RunWithConnectionTo<AgentRole>,
    impl ee_agent_protocol::HandleConnectionClose<AgentRole>,
> {
    ClientRole
        .builder()
        .name(format!("ee-agent-host:{}", inner.agent_id))
        .on_receive_notification(
            {
                let inner = inner.clone();
                async move |notification: SessionNotification, _cx| {
                    handle_session_notification(notification, &inner);
                    Ok(())
                }
            },
            on_receive_notification!(),
        )
        .on_receive_notification(
            {
                let inner = inner.clone();
                async move |notification: CompleteElicitationNotification, _cx| {
                    handle_elicitation_complete(notification, &inner);
                    Ok(())
                }
            },
            on_receive_notification!(),
        )
        .on_receive_notification(
            {
                let inner = inner.clone();
                async move |notification: CancelRequestNotification, _cx| {
                    let cancelled = inner.cancel_client_request(&notification.request_id);
                    tracing::debug!(
                        agent_id = %inner.agent_id,
                        request_id = ?notification.request_id,
                        cancelled,
                        "received $/cancel_request for client request"
                    );
                    Ok(())
                }
            },
            on_receive_notification!(),
        )
        .on_receive_request(
            {
                let inner = inner.clone();
                async move |request: RequestPermissionRequest, responder, cx| {
                    handle_permission_request(request, responder, &cx, &inner)
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let inner = inner.clone();
                async move |request: ReadTextFileRequest, responder, cx| {
                    dispatch_client_request(
                        &inner,
                        ClientRequest::ReadTextFile(request),
                        responder.erase_to_json(),
                        &cx,
                    )
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let inner = inner.clone();
                async move |request: WriteTextFileRequest, responder, cx| {
                    dispatch_client_request(
                        &inner,
                        ClientRequest::WriteTextFile(request),
                        responder.erase_to_json(),
                        &cx,
                    )
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let inner = inner.clone();
                async move |request: CreateTerminalRequest, responder, cx| {
                    dispatch_client_request(
                        &inner,
                        ClientRequest::CreateTerminal(request),
                        responder.erase_to_json(),
                        &cx,
                    )
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let inner = inner.clone();
                async move |request: TerminalOutputRequest, responder, cx| {
                    dispatch_client_request(
                        &inner,
                        ClientRequest::TerminalOutput(request),
                        responder.erase_to_json(),
                        &cx,
                    )
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let inner = inner.clone();
                async move |request: WaitForTerminalExitRequest, responder, cx| {
                    dispatch_client_request(
                        &inner,
                        ClientRequest::WaitForTerminalExit(request),
                        responder.erase_to_json(),
                        &cx,
                    )
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let inner = inner.clone();
                async move |request: KillTerminalRequest, responder, cx| {
                    dispatch_client_request(
                        &inner,
                        ClientRequest::KillTerminal(request),
                        responder.erase_to_json(),
                        &cx,
                    )
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let inner = inner.clone();
                async move |request: ReleaseTerminalRequest, responder, cx| {
                    dispatch_client_request(
                        &inner,
                        ClientRequest::ReleaseTerminal(request),
                        responder.erase_to_json(),
                        &cx,
                    )
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let inner = inner.clone();
                async move |request: CreateElicitationRequest, responder, cx| {
                    dispatch_client_request(
                        &inner,
                        ClientRequest::CreateElicitation(request),
                        responder.erase_to_json(),
                        &cx,
                    )
                }
            },
            on_receive_request!(),
        )
        // ACP-native MCP-over-ACP for the ee proxy.  These use the
        // official SDK request types (method metadata from
        // `CLIENT_METHOD_NAMES`); strict ordering/identity rules live in
        // `crate::mcp_over_acp`.
        .on_receive_request(
            {
                let inner = inner.clone();
                async move |request: ConnectMcpRequest, responder, cx| {
                    let _ = cx;
                    inner.mcp.handle_connect(request, responder, inner.agent_advertises_acp())
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let inner = inner.clone();
                async move |request: MessageMcpRequest, responder, cx| {
                    inner.mcp.handle_message(request, responder, &cx, inner.agent_advertises_acp())
                }
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            {
                let inner = inner.clone();
                async move |notification: MessageMcpNotification, _cx| {
                    inner.mcp.handle_notification(notification);
                    Ok(())
                }
            },
            on_receive_notification!(),
        )
        .on_receive_request(
            {
                let inner = inner.clone();
                async move |request: DisconnectMcpRequest, responder, cx| {
                    let _ = cx;
                    inner.mcp.handle_disconnect(request, responder)
                }
            },
            on_receive_request!(),
        )
}

/// Builds the SDK client for a v2 connection.
///
/// v2 removes the client fs/terminal/elicitation request surface, so none of
/// those handlers are registered (a conforming v2 agent never sends them; a
/// non-conforming one gets a method-not-found response).  Only the v2-typed
/// session surface is handled: updates (including the turn-ending idle
/// `state_update`), permission requests, and `$/cancel_request`.
fn build_client_builder_v2(
    inner: Arc<AgentConnectionInner>,
) -> ee_agent_protocol::Builder<
    ClientRole,
    impl ee_agent_protocol::HandleDispatchFrom<AgentRole>,
    impl ee_agent_protocol::RunWithConnectionTo<AgentRole>,
    impl ee_agent_protocol::HandleConnectionClose<AgentRole>,
> {
    // SDK >= 2.1: `Role::v2()` yields a `V2Builder` whose callbacks receive
    // version-typed `V2ConnectionTo` contexts.  The host driver is written
    // against the version-neutral [`ConnectionTo`]; `with_v2_protocol_guard`
    // applies the same wire validation while retaining raw callback contexts.
    ClientRole
        .builder()
        .with_v2_protocol_guard()
        .name(format!("ee-agent-host:{}:v2", inner.agent_id))
        .on_receive_notification(
            {
                let inner = inner.clone();
                async move |notification: v2::UpdateSessionNotification, _cx| {
                    handle_v2_session_notification(notification, &inner);
                    Ok(())
                }
            },
            on_receive_notification!(),
        )
        .on_receive_notification(
            {
                let inner = inner.clone();
                async move |notification: v2::CancelRequestNotification, _cx| {
                    let cancelled = inner.cancel_client_request(&notification.request_id);
                    tracing::debug!(
                        agent_id = %inner.agent_id,
                        request_id = ?notification.request_id,
                        cancelled,
                        "received $/cancel_request for client request (v2)"
                    );
                    Ok(())
                }
            },
            on_receive_notification!(),
        )
        .on_receive_request(
            {
                let inner = inner.clone();
                async move |request: v2::RequestPermissionRequest, responder, cx| {
                    handle_permission_request_v2(request, responder, &cx, &inner)
                }
            },
            on_receive_request!(),
        )
}

/// The client-side capabilities advertised during `initialize`, derived from
/// the registered handler so nothing unsupported is ever advertised.
fn client_capabilities(handler_capabilities: &HandlerCapabilities) -> ClientCapabilities {
    let mut capabilities = ClientCapabilities::new();
    if handler_capabilities.fs_read || handler_capabilities.fs_write {
        capabilities = capabilities.fs(FileSystemCapabilities::new()
            .read_text_file(handler_capabilities.fs_read)
            .write_text_file(handler_capabilities.fs_write));
    }
    if handler_capabilities.terminal {
        capabilities = capabilities.terminal(true);
    }
    if handler_capabilities.session_config_boolean {
        capabilities = capabilities.session(ClientSessionCapabilities::new().config_options(
            SessionConfigOptionsCapabilities::new().boolean(BooleanConfigOptionCapabilities::new()),
        ));
    }
    if handler_capabilities.elicitation_form || handler_capabilities.elicitation_url {
        let mut elicitation = ElicitationCapabilities::new();
        if handler_capabilities.elicitation_form {
            elicitation = elicitation.form(ElicitationFormCapabilities::new());
        }
        if handler_capabilities.elicitation_url {
            elicitation = elicitation.url(ElicitationUrlCapabilities::new());
        }
        capabilities = capabilities.elicitation(elicitation);
    }
    capabilities
}

type PromptTask = Pin<Box<dyn Future<Output = PromptTaskCompletion> + Send>>;
type LifecycleTask = Pin<Box<dyn Future<Output = ()> + Send>>;

struct PendingPrompt {
    request: PromptRequest,
    cancel: watch::Receiver<bool>,
    tx: oneshot::Sender<Result<PromptResponse, AgentError>>,
}

struct PromptTaskCompletion {
    session_id: SessionId,
    request_id: RequestId,
    result: Result<PromptResponse, AgentError>,
    tx: oneshot::Sender<Result<PromptResponse, AgentError>>,
}

/// Executes connection commands against the SDK connection.
///
/// Prompt requests are tracked separately and polled concurrently, keyed by
/// session and JSON-RPC request id. Independent create/load/resume requests are
/// also polled concurrently; SDK request ids preserve response attribution.
/// Connection-wide and mutating session controls remain FIFO. Shutdown closes
/// intake, resolves bounded tasks, then rejects queued commands.
#[allow(clippy::too_many_arguments)]
async fn driver_loop(
    connection: ConnectionTo<AgentRole>,
    mut rx: mpsc::UnboundedReceiver<ConnectionCommand>,
    mut terminate: watch::Receiver<bool>,
    state_rx: watch::Receiver<AgentConnectionState>,
    request_timeout: Duration,
    max_concurrent_prompts: usize,
    inner: Arc<AgentConnectionInner>,
    is_v2: bool,
) {
    let agent_id = inner.agent_id.clone();
    let (prompt_shutdown_tx, prompt_shutdown_rx) = watch::channel(false);
    let (lifecycle_shutdown_tx, lifecycle_shutdown_rx) = watch::channel(false);
    let mut prompt_tasks = FuturesUnordered::<PromptTask>::new();
    let mut lifecycle_tasks = FuturesUnordered::<LifecycleTask>::new();
    let mut active_prompts = HashMap::<SessionId, RequestId>::new();
    let mut queued_prompts = VecDeque::<PendingPrompt>::new();
    let mut queued_sessions = HashSet::<SessionId>::new();
    loop {
        tokio::select! {
            _ = terminate.changed() => break,
            Some(completion) = prompt_tasks.next(), if !prompt_tasks.is_empty() => {
                finish_prompt_task(completion, &mut active_prompts);
                dispatch_queued_prompts(
                    &connection,
                    &mut prompt_tasks,
                    &mut active_prompts,
                    &mut queued_prompts,
                    &mut queued_sessions,
                    max_concurrent_prompts,
                    &prompt_shutdown_rx,
                    &state_rx,
                    &agent_id,
                    &inner,
                    is_v2,
                );
            }
            Some(()) = lifecycle_tasks.next(), if !lifecycle_tasks.is_empty() => {}
            command = rx.recv() => {
                let Some(command) = command else { break };
                match command {
                    ConnectionCommand::NewSession { request, tx } => {
                        let connection = connection.clone();
                        let shutdown = lifecycle_shutdown_rx.clone();
                        let agent_id = agent_id.clone();
                        // v2 wire: mcpServers carry a `type` discriminator;
                        // the v1-typed request is converted before sending and
                        // the v2 response is rounded back to the v1 shape the
                        // host API exposes.
                        let v2_request = is_v2.then(|| v2_new_session_request(&request));
                        lifecycle_tasks.push(Box::pin(async move {
                            let result = tokio::select! {
                                () = wait_for_true(shutdown) => {
                                    Err(AgentError::ConnectionClosed { agent_id })
                                }
                                result = async {
                                    if let Some(v2_request) = &v2_request {
                                        let response: v2::NewSessionResponse =
                                            request_with_timeout(
                                                &connection,
                                                v2_request.clone(),
                                                request_timeout,
                                                "session/new",
                                            )
                                            .await?;
                                        let mut converted = ee_agent_protocol::NewSessionResponse::new(
                                            response.session_id.0,
                                        );
                                        let options =
                                            crate::v2_updates::config_options_to_v1(
                                                response.config_options,
                                            );
                                        if !options.is_empty() {
                                            converted = converted.config_options(options);
                                        }
                                        Ok::<_, AgentError>(converted)
                                    } else {
                                        request_with_timeout(
                                            &connection,
                                            request,
                                            request_timeout,
                                            "session/new",
                                        )
                                        .await
                                    }
                                } => result,
                            };
                            let _ = tx.send(result);
                        }));
                    }
                    ConnectionCommand::LoadSession { request, tx } => {
                        if is_v2 {
                            // v2 removed `session/load`; `session/resume` with
                            // optional `replayFrom` replaces it.
                            let _ = tx.send(Err(AgentError::CapabilityUnsupported {
                                method: "session/load".into(),
                            }));
                            continue;
                        }
                        let connection = connection.clone();
                        let shutdown = lifecycle_shutdown_rx.clone();
                        let agent_id = agent_id.clone();
                        lifecycle_tasks.push(Box::pin(async move {
                            let result = tokio::select! {
                                () = wait_for_true(shutdown) => {
                                    Err(AgentError::ConnectionClosed { agent_id })
                                }
                                result = request_with_timeout(
                                    &connection,
                                    request,
                                    request_timeout,
                                    "session/load",
                                ) => result,
                            };
                            let _ = tx.send(result);
                        }));
                    }
                    ConnectionCommand::ListSessions { request, tx } => {
                        let result = request_with_timeout(&connection, request, request_timeout, "session/list").await;
                        let _ = tx.send(result);
                    }
                    ConnectionCommand::DeleteSession { request, tx } => {
                        let result = request_with_timeout(&connection, request, request_timeout, "session/delete").await;
                        let _ = tx.send(result);
                    }
                    ConnectionCommand::ResumeSession { request, tx } => {
                        let connection = connection.clone();
                        let shutdown = lifecycle_shutdown_rx.clone();
                        let agent_id = agent_id.clone();
                        // v2 wire: same conversion as session/new; `replayFrom`
                        // stays unset (no replay), which equals the v1 resume
                        // behavior.
                        let v2_request = is_v2.then(|| v2_resume_session_request(&request));
                        lifecycle_tasks.push(Box::pin(async move {
                            let result = tokio::select! {
                                () = wait_for_true(shutdown) => {
                                    Err(AgentError::ConnectionClosed { agent_id })
                                }
                                result = async {
                                    if let Some(v2_request) = &v2_request {
                                        let response: v2::ResumeSessionResponse =
                                            request_with_timeout(
                                                &connection,
                                                v2_request.clone(),
                                                request_timeout,
                                                "session/resume",
                                            )
                                            .await?;
                                        let mut converted = ee_agent_protocol::ResumeSessionResponse::new();
                                        let options =
                                            crate::v2_updates::config_options_to_v1(
                                                response.config_options,
                                            );
                                        if !options.is_empty() {
                                            converted = converted.config_options(options);
                                        }
                                        Ok::<_, AgentError>(converted)
                                    } else {
                                        request_with_timeout(
                                            &connection,
                                            request,
                                            request_timeout,
                                            "session/resume",
                                        )
                                        .await
                                    }
                                } => result,
                            };
                            let _ = tx.send(result);
                        }));
                    }
                    ConnectionCommand::CloseSession { request, tx } => {
                        let result = request_with_timeout(&connection, request, request_timeout, "session/close").await;
                        let _ = tx.send(result);
                    }
                    ConnectionCommand::SetMode { request, tx } => {
                        if is_v2 {
                            // v2 removed `session/set_mode`; modes are config
                            // options (see `AgentThread::set_mode`).
                            let _ = tx.send(Err(AgentError::CapabilityUnsupported {
                                method: "session/set_mode".into(),
                            }));
                            continue;
                        }
                        let result = request_with_timeout(&connection, request, request_timeout, "session/set_mode").await;
                        let _ = tx.send(result);
                    }
                    ConnectionCommand::SetConfigOption { request, tx } => {
                        let result = if is_v2 {
                            // v2 keeps `session/set_config_option` (modes are
                            // config options now); the v1-typed request and
                            // response convert to the v2 types (identical wire
                            // shapes).
                            match v2_set_config_option_request(&request) {
                                Err(error) => Err(error),
                                Ok(v2_request) => {
                                    match request_with_timeout(
                                        &connection,
                                        v2_request,
                                        request_timeout,
                                        "session/set_config_option",
                                    )
                                    .await
                                    {
                                        Err(error) => Err(error),
                                        Ok(response) => serde_json::to_value(response)
                                            .map_err(|error| {
                                                AgentError::UnexpectedResponse(error.to_string())
                                            })
                                            .and_then(|value| {
                                                serde_json::from_value(value).map_err(|error| {
                                                    AgentError::UnexpectedResponse(error.to_string())
                                                })
                                            }),
                                    }
                                }
                            }
                        } else {
                            request_with_timeout(
                                &connection,
                                request,
                                request_timeout,
                                "session/set_config_option",
                            )
                            .await
                        };
                        let _ = tx.send(result);
                    }
                    ConnectionCommand::Authenticate { request, tx } => {
                        let result = request_with_timeout(&connection, request, request_timeout, "authenticate").await;
                        let _ = tx.send(result);
                    }
                    ConnectionCommand::Logout { request, tx } => {
                        let result = request_with_timeout(&connection, request, request_timeout, "logout").await;
                        let _ = tx.send(result);
                    }
                    ConnectionCommand::Prompt { request, cancel, tx } => {
                        let session_id = request.session_id.clone();
                        if active_prompts.contains_key(&session_id)
                            || queued_sessions.contains(&session_id)
                        {
                            let _ = tx.send(Err(AgentError::TurnAlreadyRunning));
                            continue;
                        }
                        if active_prompts.len() >= max_concurrent_prompts {
                            queued_sessions.insert(session_id.clone());
                            queued_prompts.push_back(PendingPrompt { request, cancel, tx });
                            let _ = inner.events.send(AgentEvent::TurnQueued {
                                session_id,
                                position: queued_prompts.len(),
                            });
                            continue;
                        }
                        start_prompt_task(
                            &connection,
                            &mut prompt_tasks,
                            &mut active_prompts,
                            PendingPrompt { request, cancel, tx },
                            &prompt_shutdown_rx,
                            &state_rx,
                            &agent_id,
                            is_v2,
                            &inner,
                        );
                    }
                    ConnectionCommand::CancelSession { session_id } => {
                        if let Some(position) = queued_prompts
                            .iter()
                            .position(|pending| pending.request.session_id == session_id)
                        {
                            if let Some(pending) = queued_prompts.remove(position) {
                                queued_sessions.remove(&session_id);
                                let _ = pending.tx.send(Err(AgentError::Cancelled));

                            }
                            continue;
                        }
                        let _ = connection.send_notification(CancelNotification::new(session_id));
                        // Turn cancel closes every logical MCP connection on
                        // this connection.
                        inner.mcp.close_all();
                    }
                    ConnectionCommand::Close => break,
                }
            }
        }
    }

    rx.close();
    let _ = prompt_shutdown_tx.send(true);
    let _ = lifecycle_shutdown_tx.send(true);
    while let Some(pending) = queued_prompts.pop_front() {
        let _ = pending.tx.send(Err(AgentError::ConnectionClosed { agent_id: agent_id.clone() }));
    }
    queued_sessions.clear();

    while let Some(completion) = prompt_tasks.next().await {
        finish_prompt_task(completion, &mut active_prompts);
    }
    while lifecycle_tasks.next().await.is_some() {}
    while let Some(command) = rx.recv().await {
        reject_command_after_shutdown(command, &agent_id);
    }
}

/*
 * Prompt tasks.  v1: the prompt response ends the turn and carries the stop
 * reason.  v2: the response is an insertion acknowledgment; the turn ends when
 * the idle `state_update` arrives (registered through `inner.v2_turn_ends`),
 * and the stop reason is taken from that update.
 */

#[allow(clippy::too_many_arguments)]
fn start_prompt_task(
    connection: &ConnectionTo<AgentRole>,
    prompt_tasks: &mut FuturesUnordered<PromptTask>,
    active_prompts: &mut HashMap<SessionId, RequestId>,
    pending: PendingPrompt,
    prompt_shutdown: &watch::Receiver<bool>,
    state_rx: &watch::Receiver<AgentConnectionState>,
    agent_id: &str,
    is_v2: bool,
    inner: &Arc<AgentConnectionInner>,
) {
    let session_id = pending.request.session_id.clone();
    if is_v2 {
        let v2_request = v2_prompt_request(&pending.request);
        // The turn-end oneshot is registered *before* the request goes out:
        // the SDK may dispatch follow-up notifications (chunks, the idle
        // `state_update`) before the prompt response itself is delivered to
        // the blocked task, so waiting for the ack first would miss a fast
        // idle and hang the turn.
        let (end_tx, end_rx) = oneshot::channel();
        let replace_session = session_id.clone();
        if let Some(previous) = inner
            .v2_turn_ends
            .lock()
            .expect("v2 turn ends poisoned")
            .insert(replace_session, end_tx)
        {
            // One active prompt per session is enforced by the driver; a
            // duplicate registration means a bug.
            drop(previous);
            tracing::warn!(%session_id, "duplicate v2 turn-end registration for one session");
        }
        let sent = connection.send_request(v2_request);
        let request_id = sent.id().clone();
        active_prompts.insert(session_id.clone(), request_id.clone());
        prompt_tasks.push(Box::pin(run_prompt_task_v2(
            sent,
            session_id,
            request_id,
            pending.cancel,
            prompt_shutdown.clone(),
            state_rx.clone(),
            agent_id.to_string(),
            connection.clone(),
            inner.clone(),
            end_rx,
            pending.tx,
        )));
        return;
    }
    let sent = connection.send_request(pending.request);
    let request_id = sent.id().clone();
    active_prompts.insert(session_id.clone(), request_id.clone());
    prompt_tasks.push(Box::pin(run_prompt_task(
        sent,
        session_id,
        request_id,
        pending.cancel,
        prompt_shutdown.clone(),
        state_rx.clone(),
        agent_id.to_string(),
        connection.clone(),
        pending.tx,
    )));
}

#[allow(clippy::too_many_arguments)]
fn dispatch_queued_prompts(
    connection: &ConnectionTo<AgentRole>,
    prompt_tasks: &mut FuturesUnordered<PromptTask>,
    active_prompts: &mut HashMap<SessionId, RequestId>,
    queued_prompts: &mut VecDeque<PendingPrompt>,
    queued_sessions: &mut HashSet<SessionId>,
    max_concurrent_prompts: usize,
    prompt_shutdown: &watch::Receiver<bool>,
    state_rx: &watch::Receiver<AgentConnectionState>,
    agent_id: &str,
    inner: &Arc<AgentConnectionInner>,
    is_v2: bool,
) {
    while active_prompts.len() < max_concurrent_prompts {
        let Some(pending) = queued_prompts.pop_front() else {
            break;
        };
        queued_sessions.remove(&pending.request.session_id);
        let _ = inner
            .events
            .send(AgentEvent::TurnDispatched { session_id: pending.request.session_id.clone() });
        start_prompt_task(
            connection,
            prompt_tasks,
            active_prompts,
            pending,
            prompt_shutdown,
            state_rx,
            agent_id,
            is_v2,
            inner,
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_prompt_task(
    sent: ee_agent_protocol::SentRequest<PromptResponse>,
    session_id: SessionId,
    request_id: RequestId,
    cancel: watch::Receiver<bool>,
    prompt_shutdown: watch::Receiver<bool>,
    state_rx: watch::Receiver<AgentConnectionState>,
    agent_id: String,
    connection: ConnectionTo<AgentRole>,
    tx: oneshot::Sender<Result<PromptResponse, AgentError>>,
) -> PromptTaskCompletion {
    let cancelled = wait_for_true(cancel);
    let shutdown = wait_for_true(prompt_shutdown);
    let result = tokio::select! {
        biased;
        () = shutdown => {
            let _ = connection.send_cancel_request(request_id.clone());
            Err(AgentError::ConnectionClosed { agent_id: agent_id.clone() })
        }
        () = cancelled => {
            let _ = connection.send_cancel_request(request_id.clone());
            Err(AgentError::Cancelled)
        }
        response = sent.block_task() => {
            match response {
                Ok(response) => Ok(response),
                Err(_error) if matches!(*state_rx.borrow(), AgentConnectionState::Closed(_)) => {
                    Err(AgentError::ConnectionClosed { agent_id: agent_id.clone() })
                }
                Err(error) => {
                    tracing::warn!(agent_id, ?error, "prompt block_task failed");
                    Err(AgentError::Rpc(error))
                }
            }
        }
    };
    PromptTaskCompletion { session_id, request_id, result, tx }
}

/// v2 prompt: the acknowledgment response is not the end of the turn.  Once
/// the ack arrives, the turn stays open — and `active_prompts` stays
/// occupied — until the idle `state_update` resolves `inner.v2_turn_ends`
/// with the agent's stop reason.
///
/// The response and notifications share one transport and are processed in
/// stream order, so the ack always resolves before the idle update that
/// follows it in-band; a defensive debug log covers the impossible case where
/// a stop reason raced ahead of registration.
#[allow(clippy::too_many_arguments)]
async fn run_prompt_task_v2(
    sent: ee_agent_protocol::SentRequest<v2::PromptResponse>,
    session_id: SessionId,
    request_id: RequestId,
    cancel: watch::Receiver<bool>,
    prompt_shutdown: watch::Receiver<bool>,
    state_rx: watch::Receiver<AgentConnectionState>,
    agent_id: String,
    connection: ConnectionTo<AgentRole>,
    inner: Arc<AgentConnectionInner>,
    // Resolves with the turn's stop reason when the idle `state_update`
    // arrives; created in `start_prompt_task` before the request is sent /
    // notifications can be dispatched.
    end_rx: oneshot::Receiver<StopReason>,
    tx: oneshot::Sender<Result<PromptResponse, AgentError>>,
) -> PromptTaskCompletion {
    let cancelled = wait_for_true(cancel.clone());
    let shutdown = wait_for_true(prompt_shutdown.clone());
    // Fresh futures for the post-ack wait: the outer select consumed its own
    // copies above.
    let stop_cancelled = wait_for_true(cancel);
    let stop_shutdown = wait_for_true(prompt_shutdown);
    let result = tokio::select! {
        biased;
        () = shutdown => {
            inner.v2_turn_ends.lock().expect("v2 turn ends poisoned").remove(&session_id);
            let _ = connection.send_cancel_request(request_id.clone());
            Err(AgentError::ConnectionClosed { agent_id: agent_id.clone() })
        }
        () = cancelled => {
            inner.v2_turn_ends.lock().expect("v2 turn ends poisoned").remove(&session_id);
            let _ = connection.send_cancel_request(request_id.clone());
            Err(AgentError::Cancelled)
        }
        response = sent.block_task() => {
            match response {
                Ok(_ack) => {
                    // Turn accepted; the turn ends when the inbound idle
                    // `state_update` fires the oneshot registered before the
                    // request was sent.
                    let stop = tokio::select! {
                        () = stop_shutdown => {
                            inner.v2_turn_ends.lock().expect("v2 turn ends poisoned").remove(&session_id);
                            let _ = connection.send_cancel_request(request_id.clone());
                            Err(AgentError::ConnectionClosed { agent_id: agent_id.clone() })
                        }
                        () = stop_cancelled => {
                            inner.v2_turn_ends.lock().expect("v2 turn ends poisoned").remove(&session_id);
                            Err(AgentError::Cancelled)
                        }
                        reason = end_rx => {
                            match reason {
                                Ok(stop_reason) => {
                                    // v1 stop reasons and v2 stop reasons share
                                    // their variant set; the host API stays on
                                    // the v1-typed PromptResponse.
                                    Ok(PromptResponse::new(stop_reason))
                                }
                                Err(_) => {
                                    tracing::warn!(%session_id, "v2 idle state update never arrived");
                                    Err(AgentError::ConnectionClosed { agent_id: agent_id.clone() })
                                }
                            }
                        }
                    };
                    stop
                }
                Err(error) => {
                    inner.v2_turn_ends.lock().expect("v2 turn ends poisoned").remove(&session_id);
                    if matches!(*state_rx.borrow(), AgentConnectionState::Closed(_)) {
                        Err(AgentError::ConnectionClosed { agent_id: agent_id.clone() })
                    } else {
                        tracing::warn!(agent_id, ?error, "v2 prompt block_task failed");
                        Err(AgentError::Rpc(error))
                    }
                }
            }
        }
    };
    PromptTaskCompletion { session_id, request_id, result, tx }
}

/// v1 session/new request → v2 wire shape (mcp servers gain the `type`
/// discriminator).
fn v2_new_session_request(request: &NewSessionRequest) -> v2::NewSessionRequest {
    let mut converted = v2::NewSessionRequest::new(v2::AbsolutePath::new(request.cwd.clone()))
        .additional_directories(request.additional_directories.clone());
    if !request.mcp_servers.is_empty() {
        converted = converted.mcp_servers(mcp_servers_to_v2(&request.mcp_servers));
    }
    match &request.meta {
        Some(meta) => converted.meta(meta.clone()),
        None => converted,
    }
}

/// v1 session/resume request → v2 wire shape (`replayFrom` stays unset:
/// no replay, matching the v1 resume behavior).
fn v2_resume_session_request(request: &ResumeSessionRequest) -> v2::ResumeSessionRequest {
    let mut converted = v2::ResumeSessionRequest::new(
        v2::SessionId::new(request.session_id.0.clone()),
        v2::AbsolutePath::new(request.cwd.clone()),
    )
    .additional_directories(request.additional_directories.clone());
    if !request.mcp_servers.is_empty() {
        converted = converted.mcp_servers(mcp_servers_to_v2(&request.mcp_servers));
    }
    match &request.meta {
        Some(meta) => converted.meta(meta.clone()),
        None => converted,
    }
}

/// v1 prompt request → v2 typed request (identical wire shape; the response
/// type differs between versions, so the typed request must too).
fn v2_prompt_request(request: &PromptRequest) -> v2::PromptRequest {
    let prompt = request
        .prompt
        .iter()
        .filter_map(|block| serde_json::from_value(serde_json::to_value(block).ok()?).ok())
        .collect::<Vec<v2::ContentBlock>>();
    if prompt.len() != request.prompt.len() {
        tracing::warn!("dropped v1 prompt blocks with no v2 representation");
    }
    let converted =
        v2::PromptRequest::new(v2::SessionId::new(request.session_id.0.clone()), prompt);
    match &request.meta {
        Some(meta) => converted.meta(meta.clone()),
        None => converted,
    }
}

/// v1 `session/set_config_option` request → v2 typed request (identical wire
/// shape; the v1 `ValueId` maps to the v2 `Id` value).  Fails closed for v1
/// values without a v2 representation.
fn v2_set_config_option_request(
    request: &SetSessionConfigOptionRequest,
) -> Result<v2::SetSessionConfigOptionRequest, AgentError> {
    let value = match &request.value {
        ee_agent_protocol::SessionConfigOptionValue::ValueId { value } => {
            v2::SessionConfigOptionValue::Id {
                value: v2::SessionConfigValueId::new(value.0.clone()),
            }
        }
        ee_agent_protocol::SessionConfigOptionValue::Boolean { value } => {
            v2::SessionConfigOptionValue::Boolean { value: *value }
        }
        other => {
            return Err(AgentError::invalid_params(format!(
                "config option value has no v2 representation: {other:?}"
            )));
        }
    };
    Ok(v2::SetSessionConfigOptionRequest::new(
        v2::SessionId::new(request.session_id.0.clone()),
        request.config_id.0.clone(),
        value,
    ))
}

/// Maps the host's v1 MCP server configs onto the v2 wire (every server
/// needs a transport `type`).  Configs that cannot be represented are
/// dropped with a warning rather than failing the session request.
fn mcp_servers_to_v2(servers: &[McpServer]) -> Vec<v2::McpServer> {
    servers
        .iter()
        .filter_map(|server| match server {
            McpServer::Stdio(stdio) => {
                let env = stdio
                    .env
                    .iter()
                    .filter_map(|variable| {
                        serde_json::from_value(serde_json::to_value(variable).ok()?).ok()
                    })
                    .collect::<Vec<v2::EnvVariable>>();
                let mut converted = v2::McpServerStdio::new(
                    stdio.name.clone(),
                    v2::AbsolutePath::new(stdio.command.clone()),
                )
                .args(stdio.args.clone())
                .env(env);
                if let Some(meta) = &stdio.meta {
                    converted = converted.meta(meta.clone());
                }
                Some(v2::McpServer::Stdio(converted))
            }
            McpServer::Http(http) => Some(v2::McpServer::Http(
                serde_json::from_value(serde_json::to_value(http).ok()?).ok()?,
            )),
            other => {
                tracing::warn!(?other, "dropping MCP server config with no v2 representation");
                None
            }
        })
        .collect()
}

/// v2 `Implementation` → the v1-typed implementation the host stores.  The
/// field set is identical across versions.
fn implementation_to_v1(implementation: &v2::Implementation) -> Implementation {
    let mut converted =
        Implementation::new(implementation.name.clone(), implementation.version.clone());
    if let Some(title) = &implementation.title {
        converted = converted.title(title.clone());
    }
    converted
}

/// Maps the v2 stop reason onto the v1-typed value the host API uses.
fn stop_reason_to_v1(reason: v2::StopReason) -> StopReason {
    match reason {
        v2::StopReason::EndTurn => StopReason::EndTurn,
        v2::StopReason::MaxTokens => StopReason::MaxTokens,
        v2::StopReason::MaxTurnRequests => StopReason::MaxTurnRequests,
        v2::StopReason::Refusal => StopReason::Refusal,
        v2::StopReason::Cancelled => StopReason::Cancelled,
        other => {
            tracing::warn!(?other, "unknown v2 stop reason mapped to refusal");
            StopReason::Refusal
        }
    }
}

async fn wait_for_true(mut signal: watch::Receiver<bool>) {
    if *signal.borrow() {
        return;
    }
    loop {
        match signal.changed().await {
            Ok(()) if *signal.borrow() => return,
            Ok(()) => {}
            Err(_) => std::future::pending().await,
        }
    }
}

fn finish_prompt_task(
    completion: PromptTaskCompletion,
    active_prompts: &mut HashMap<SessionId, RequestId>,
) {
    if active_prompts.get(&completion.session_id) == Some(&completion.request_id) {
        active_prompts.remove(&completion.session_id);
    }
    let _ = completion.tx.send(completion.result);
}

fn reject_command_after_shutdown(command: ConnectionCommand, agent_id: &str) {
    let error = || AgentError::ConnectionClosed { agent_id: agent_id.to_string() };
    match command {
        ConnectionCommand::NewSession { tx, .. } => {
            let _ = tx.send(Err(error()));
        }
        ConnectionCommand::LoadSession { tx, .. } => {
            let _ = tx.send(Err(error()));
        }
        ConnectionCommand::ListSessions { tx, .. } => {
            let _ = tx.send(Err(error()));
        }
        ConnectionCommand::DeleteSession { tx, .. } => {
            let _ = tx.send(Err(error()));
        }
        ConnectionCommand::ResumeSession { tx, .. } => {
            let _ = tx.send(Err(error()));
        }
        ConnectionCommand::CloseSession { tx, .. } => {
            let _ = tx.send(Err(error()));
        }
        ConnectionCommand::SetMode { tx, .. } => {
            let _ = tx.send(Err(error()));
        }
        ConnectionCommand::SetConfigOption { tx, .. } => {
            let _ = tx.send(Err(error()));
        }
        ConnectionCommand::Authenticate { tx, .. } => {
            let _ = tx.send(Err(error()));
        }
        ConnectionCommand::Logout { tx, .. } => {
            let _ = tx.send(Err(error()));
        }
        ConnectionCommand::Prompt { tx, .. } => {
            let _ = tx.send(Err(error()));
        }
        ConnectionCommand::CancelSession { .. } | ConnectionCommand::Close => {}
    }
}

/// Runs one non-prompt ACP request with a bounded timeout.
async fn request_with_timeout<T: ee_agent_protocol::JsonRpcRequest>(
    connection: &ConnectionTo<AgentRole>,
    request: T,
    timeout: Duration,
    method: &str,
) -> Result<T::Response, AgentError> {
    let sent = connection.send_request(request);
    tokio::time::timeout(timeout, sent.block_task())
        .await
        .map_err(|_| AgentError::RequestTimeout { method: method.to_string() })?
        .map_err(AgentError::Rpc)
}

fn handle_session_notification(notification: SessionNotification, inner: &AgentConnectionInner) {
    let Some(thread) =
        inner.threads.lock().expect("threads poisoned").get(&notification.session_id).cloned()
    else {
        tracing::warn!(
            session_id = %notification.session_id.0,
            "session/update for unknown session"
        );
        return;
    };
    thread.apply_update(notification.update);
}

fn handle_elicitation_complete(
    notification: CompleteElicitationNotification,
    inner: &AgentConnectionInner,
) {
    let elicitation_id = notification.elicitation_id.0.to_string();
    if let Some(session_id) = inner.complete_url_elicitation(&elicitation_id) {
        let _ = inner.events.send(AgentEvent::ElicitationCompleted {
            agent_id: inner.agent_id.clone(),
            session_id,
            elicitation_id: notification.elicitation_id,
        });
    } else {
        tracing::debug!(
            agent_id = %inner.agent_id,
            elicitation_id,
            "ignored stale or unknown elicitation completion"
        );
    }
}

fn handle_permission_request(
    request: RequestPermissionRequest,
    responder: ee_agent_protocol::Responder<RequestPermissionResponse>,
    cx: &ConnectionTo<AgentRole>,
    inner: &Arc<AgentConnectionInner>,
) -> Result<(), RpcError> {
    let session_id = request.session_id.clone();
    let (request_id, info, rx) =
        inner.broker.request(session_id.clone(), request.tool_call, request.options);
    let _ = inner.events.send(AgentEvent::PermissionRequested {
        session_id: session_id.clone(),
        request: Box::new(info),
    });
    let events = inner.events.clone();
    let spawned = cx.spawn(async move {
        let outcome = match rx.await {
            Ok(outcome) => outcome,
            // Broker dropped the sender (cancel or connection close): answer
            // cancelled so the agent never hangs on an unanswered approval.
            Err(_) => RequestPermissionOutcome::Cancelled,
        };
        let _ = events.send(AgentEvent::PermissionResolved {
            session_id: session_id.clone(),
            request_id,
            outcome: outcome.clone(),
        });
        responder.respond(RequestPermissionResponse::new(outcome))
    });
    if spawned.is_err() {
        // Connection is shutting down; the responder is dropped with the
        // task and the agent is gone anyway.
        tracing::debug!(agent_id = %inner.agent_id, "permission request dropped: connection closing");
    }
    Ok(())
}

/// Routes a v2 `session/request_permission` (required `title`, optional
/// `subject`) through the same permission broker the v1 surface uses.
///
/// The broker is v1-shaped (it holds a [`ToolCallUpdate`]); the v2 tool-call
/// subject becomes that value, a command subject renders with the command
/// text in the title and command/cwd in `rawInput`, and an absent or unknown
/// subject becomes a title-only placeholder, so the UI has something
/// deterministic to show.
fn handle_permission_request_v2(
    request: v2::RequestPermissionRequest,
    responder: ee_agent_protocol::Responder<v2::RequestPermissionResponse>,
    cx: &ConnectionTo<AgentRole>,
    inner: &Arc<AgentConnectionInner>,
) -> Result<(), RpcError> {
    use crate::v2_updates::permission_subject_tool_call;
    let session_id = ee_agent_protocol::SessionId::new(request.session_id.0.clone());
    let tool_call = permission_subject_tool_call(
        &request.title,
        request.subject.as_ref(),
        &format!("permission-{}", request.session_id.0),
    );
    let options = request
        .options
        .iter()
        .filter_map(|option| serde_json::from_value(serde_json::to_value(option).ok()?).ok())
        .collect();
    let (request_id, info, rx) = inner.broker.request(session_id.clone(), tool_call, options);
    let _ = inner.events.send(AgentEvent::PermissionRequested {
        session_id: session_id.clone(),
        request: Box::new(info),
    });
    let events = inner.events.clone();
    let spawned = cx.spawn(async move {
        let outcome = match rx.await {
            Ok(outcome) => serde_json::from_value(serde_json::to_value(outcome).expect("outcome"))
                .expect("permission outcome maps between versions"),
            // Broker dropped the sender (cancel or connection close): answer
            // cancelled so the agent never hangs on an unanswered approval.
            Err(_) => ee_agent_protocol::RequestPermissionOutcome::Cancelled,
        };
        let _ = events.send(AgentEvent::PermissionResolved {
            session_id: session_id.clone(),
            request_id,
            outcome: outcome.clone(),
        });
        // The v1-typed broker outcome must be serialized back onto the v2 wire.
        let v2_outcome: v2::RequestPermissionOutcome =
            serde_json::from_value(serde_json::to_value(outcome).expect("outcome"))
                .expect("permission outcome maps between versions");
        responder.respond(v2::RequestPermissionResponse::new(v2_outcome))
    });
    if spawned.is_err() {
        tracing::debug!(agent_id = %inner.agent_id, "v2 permission request dropped: connection closing");
    }
    Ok(())
}

/// Routes one v2 `session/update` notification: the idle `state_update`
/// resolves the pending turn's stop reason (see `run_prompt_task_v2`); every
/// other representable update is translated to the v1-typed shape and
/// reduced by the session thread.
fn handle_v2_session_notification(
    notification: v2::UpdateSessionNotification,
    inner: &AgentConnectionInner,
) {
    let session_id = ee_agent_protocol::SessionId::new(notification.session_id.0.clone());
    if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) = &notification.update {
        if let Some(end_tx) =
            inner.v2_turn_ends.lock().expect("v2 turn ends poisoned").remove(&session_id)
        {
            let stop_reason =
                idle.stop_reason.clone().map(stop_reason_to_v1).unwrap_or(StopReason::EndTurn);
            let _ = end_tx.send(stop_reason);
        }
        // Idle without a pending turn (background work, or a turn the host
        // already resolved locally) is expected and ignored.
        return;
    }
    let Some(thread) = inner.threads.lock().expect("threads poisoned").get(&session_id).cloned()
    else {
        tracing::debug!(
            session_id = %session_id.0,
            "v2 session/update for unknown session"
        );
        return;
    };
    crate::v2_updates::apply_v2_update(&thread, &notification.update);
}

fn dispatch_client_request(
    inner: &Arc<AgentConnectionInner>,
    request: ClientRequest,
    responder: ee_agent_protocol::Responder<serde_json::Value>,
    cx: &ConnectionTo<AgentRole>,
) -> Result<(), RpcError> {
    let method = request.method().to_string();
    let session_id = request.session_id().cloned();
    let target = ClientRequest::client_request_target(&request);
    // Fail closed: never invoke a handler for a capability we did not
    // advertise during initialize.
    if !inner.handler_capabilities.supports_request(&request) {
        let error = match &request {
            ClientRequest::CreateElicitation(_) => {
                AgentError::invalid_params("elicitation mode was not advertised by the client")
            }
            _ => AgentError::CapabilityUnsupported { method: method.clone() },
        };
        return responder.respond_with_error(error.into_rpc());
    }
    if let ClientRequest::CreateElicitation(request) = &request
        && let ee_agent_protocol::ElicitationMode::Url(mode) = &request.mode
    {
        inner.register_url_elicitation(mode.elicitation_id.0.as_ref(), session_id.clone());
    }
    let _ = inner.events.send(AgentEvent::ClientRequestDispatched {
        session_id: session_id.clone(),
        method: method.clone(),
        target,
    });
    let request_id = responder.id().clone();
    let handler = inner.handler.clone();
    let cancellation = responder.cancellation();
    let mut cancel_rx = inner.register_client_request(&request_id);
    let inner_for_spawn = inner.clone();
    let spawned = cx.spawn(async move {
        let url_elicitation_id = match &request {
            ClientRequest::CreateElicitation(request) => match &request.mode {
                ee_agent_protocol::ElicitationMode::Url(mode) => {
                    Some(mode.elicitation_id.0.to_string())
                }
                _ => None,
            },
            _ => None,
        };
        let result = tokio::select! {
            _ = cancellation.cancelled() => responder.respond_with_error(RpcError::request_cancelled()),
            changed = cancel_rx.changed() => match changed {
                Ok(()) if *cancel_rx.borrow() => responder.respond_with_error(RpcError::request_cancelled()),
                Ok(()) | Err(_) => responder.respond_with_error(RpcError::request_cancelled()),
            },
            result = handler.handle(request) => match result {
                Ok(response) => {
                    let value = response.into_value().map_err(|error| {
                        AgentError::HandlerError(format!(
                            "handler response serialization failed: {error}"
                        ))
                        .into_rpc()
                    })?;
                    responder.respond_with_result(Ok(value))
                }
                Err(error) => responder.respond_with_error(error.into_rpc()),
            },
        };
        inner_for_spawn.finish_client_request(&request_id);
        if let Some(elicitation_id) = url_elicitation_id {
            inner_for_spawn.finish_url_elicitation(&elicitation_id);
        }
        result
    });
    if spawned.is_err() {
        // Connection is shutting down; the responder is dropped with the
        // task and the agent is gone anyway.
        tracing::debug!(agent_id = %inner.agent_id, "client request dropped: connection closing");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
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
