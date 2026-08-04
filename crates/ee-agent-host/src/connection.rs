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
//! - inbound dispatch: `session/update` notifications route to session
//!   threads, `session/request_permission` goes through the permission
//!   broker, and file/terminal/elicitation requests go to the registered
//!   [`ClientRequestHandler`] after a capability gate.
//!
//! The SDK's `Builder`/`ConnectionTo` machinery does the JSON-RPC framing,
//! request correlation, and response routing; this module only adapts it to
//! a tokio subprocess and the host lifecycle.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
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
    InitializeRequest, KillTerminalRequest, ListSessionsRequest, ListSessionsResponse,
    LoadSessionRequest, LoadSessionResponse, LogoutRequest, LogoutResponse, McpServer,
    McpServerAcpId, McpServerStdio, MessageMcpNotification, MessageMcpRequest, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse, ProtocolVersion, ReadTextFileRequest,
    ReleaseTerminalRequest, RequestId, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, ResumeSessionRequest, ResumeSessionResponse, SessionConfigOption,
    SessionConfigOptionValue, SessionConfigOptionsCapabilities, SessionId, SessionNotification,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, SetSessionModeRequest,
    SetSessionModeResponse, TerminalOutputRequest, WaitForTerminalExitRequest,
    WriteTextFileRequest, on_receive_notification, on_receive_request,
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::error::AgentError;
use crate::events::{AgentConnectionState, AgentEvent, ConnectionCloseReason, PermissionRequestId};
use crate::inbound::{ClientRequest, ClientRequestHandler, HandlerCapabilities};
use crate::mcp_over_acp::{EeProxyMode, McpOverAcpRegistry};
use crate::permission::PermissionBroker;
use crate::process::{AgentProcess, AgentProcessConfig, spawn_stderr_reader};
use crate::session::{AgentThread, ThreadShared};

/// Default timeout for the ACP `initialize` handshake.
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Default timeout for non-prompt ACP requests.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Connection tuning knobs (tests use tiny timeouts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentConnectionOptions {
    /// Timeout for `initialize` negotiation.
    pub handshake_timeout: Duration,
    /// Timeout for `session/new`, `session/load`, `session/set_mode`,
    /// `authenticate`, and `logout` requests.
    pub request_timeout: Duration,
    /// Whether the ee MCP proxy is configured (arms ACP-native MCP-over-ACP
    /// hosting for this connection; the agent still has to advertise
    /// `mcp_capabilities.acp` before anything is served).
    pub ee_proxy_enabled: bool,
}

impl Default for AgentConnectionOptions {
    fn default() -> Self {
        Self {
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            ee_proxy_enabled: false,
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
    pub process: Arc<Mutex<Option<AgentProcess>>>,
    pub threads: Mutex<HashMap<SessionId, Arc<ThreadShared>>>,
    /// ACP-native MCP-over-ACP hosting for the ee proxy (Phase 6b).
    pub mcp: McpOverAcpRegistry,
    active_url_elicitations: Mutex<HashSet<String>>,
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

    fn register_url_elicitation(&self, elicitation_id: &str) {
        self.active_url_elicitations
            .lock()
            .expect("active url elicitations poisoned")
            .insert(elicitation_id.to_string());
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

    fn complete_url_elicitation(&self, elicitation_id: &str) -> bool {
        let removed = self
            .active_url_elicitations
            .lock()
            .expect("active url elicitations poisoned")
            .remove(elicitation_id);
        if removed {
            self.completed_url_elicitations
                .lock()
                .expect("completed url elicitations poisoned")
                .insert(elicitation_id.to_string());
        }
        removed
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
            .is_some_and(|tx| tx.send(true).is_ok())
    }

    fn finish_client_request(&self, request_id: &RequestId) {
        let key = request_id_key(request_id);
        self.pending_client_requests.lock().expect("pending client requests poisoned").remove(&key);
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

        Self::start_connection(agent_id, Some(process), handler, events, options, transport)
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
        Self::start_connection(agent_id, None, handler, events, options, transport)
    }

    /// Shared connection bootstrap: channels, state, driver, and handshake.
    fn start_connection(
        agent_id: String,
        process: Option<AgentProcess>,
        handler: Arc<dyn ClientRequestHandler>,
        events: mpsc::UnboundedSender<AgentEvent>,
        options: AgentConnectionOptions,
        transport: impl ee_agent_protocol::ConnectTo<ClientRole> + 'static,
    ) -> Result<Self, AgentError> {
        let (commands_tx, commands_rx) = mpsc::unbounded_channel();
        let (state_tx, _state_rx) = watch::channel(AgentConnectionState::Starting);
        let (terminate_tx, terminate_rx) = watch::channel(false);
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let handler_capabilities = handler.capabilities();
        let broker = PermissionBroker::new();
        let process = Arc::new(Mutex::new(process));
        let mcp = McpOverAcpRegistry::new(
            options.ee_proxy_enabled,
            &agent_id,
            handler.clone(),
            handler_capabilities,
            process.clone(),
        );

        let inner = Arc::new(AgentConnectionInner {
            agent_id: agent_id.clone(),
            state: state_tx,
            commands: commands_tx,
            broker: broker.clone(),
            events: events.clone(),
            handler: handler.clone(),
            handler_capabilities,
            process,
            threads: Mutex::new(HashMap::new()),
            mcp,
            active_url_elicitations: Mutex::new(HashSet::new()),
            completed_url_elicitations: Mutex::new(HashSet::new()),
            pending_client_requests: Mutex::new(HashMap::new()),
            shutdown: shutdown_tx,
            closed_once: AtomicBool::new(false),
        });

        let client_builder = build_client_builder(inner.clone());

        // The main_fn closure runs the handshake, spawns the session driver,
        // and waits for shutdown/EOF.  It is a concrete async closure here so
        // the connection future stays `Send` for `tokio::spawn`.
        let main_inner = inner.clone();
        let main_fn = async move |connection: ConnectionTo<AgentRole>| -> Result<(), RpcError> {
            let initialize = InitializeRequest::new(ProtocolVersion::V1)
                .client_info(Implementation::new("ee", env!("CARGO_PKG_VERSION")).title("ee"))
                .client_capabilities(client_capabilities(&main_inner.handler_capabilities));
            let handshake = async { connection.send_request(initialize).block_task().await };
            match tokio::time::timeout(options.handshake_timeout, handshake).await {
                Ok(Ok(response)) => {
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
                main_inner.clone(),
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
        tokio::spawn(async move {
            let result = client_builder.connect_with(transport, main_fn).await;
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

    /// Whether the agent advertises `session/list`.
    #[must_use]
    pub fn supports_session_list(&self) -> bool {
        self.agent_capabilities()
            .is_some_and(|capabilities| capabilities.session_capabilities.list.is_some())
    }

    /// Whether the agent advertises `session/delete`.
    #[must_use]
    pub fn supports_session_delete(&self) -> bool {
        self.agent_capabilities()
            .is_some_and(|capabilities| capabilities.session_capabilities.delete.is_some())
    }

    /// Whether the agent advertises `session/resume`.
    #[must_use]
    pub fn supports_session_resume(&self) -> bool {
        self.agent_capabilities()
            .is_some_and(|capabilities| capabilities.session_capabilities.resume.is_some())
    }

    /// Whether the agent advertises `session/close`.
    #[must_use]
    pub fn supports_session_close(&self) -> bool {
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
        let Some(fallback) = ee_proxy_stdio_fallback else {
            return EeProxyMode::Disabled;
        };
        match self.inner.mcp.server_id() {
            Some(server_id)
                if self.agent_capabilities().is_some_and(|caps| caps.mcp_capabilities.acp) =>
            {
                mcp_servers.push(ee_agent_protocol::ee_proxy_acp_entry(server_id.clone()));
                EeProxyMode::AcpNative
            }
            _ => {
                mcp_servers.push(McpServer::Stdio(fallback));
                EeProxyMode::StdioFallback
            }
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
            && let Some(cancel) = thread.turn.lock().expect("turn state poisoned").take()
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
        // Phase 6b: ACP-native MCP-over-ACP for the ee proxy.  These use the
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

/// Executes connection commands against the SDK connection.  Runs until the
/// connection terminates or every command sender is dropped; pending
/// requests resolve with typed errors either way.
async fn driver_loop(
    connection: ConnectionTo<AgentRole>,
    mut rx: mpsc::UnboundedReceiver<ConnectionCommand>,
    mut terminate: watch::Receiver<bool>,
    state_rx: watch::Receiver<AgentConnectionState>,
    request_timeout: Duration,
    inner: Arc<AgentConnectionInner>,
) {
    let agent_id = inner.agent_id.clone();
    loop {
        tokio::select! {
            _ = terminate.changed() => break,
            command = rx.recv() => {
                let Some(command) = command else { break };
                match command {
                    ConnectionCommand::NewSession { request, tx } => {
                        let result = request_with_timeout(&connection, request, request_timeout, "session/new").await;
                        let _ = tx.send(result);
                    }
                    ConnectionCommand::LoadSession { request, tx } => {
                        let result = request_with_timeout(&connection, request, request_timeout, "session/load").await;
                        let _ = tx.send(result);
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
                        let result = request_with_timeout(&connection, request, request_timeout, "session/resume").await;
                        let _ = tx.send(result);
                    }
                    ConnectionCommand::CloseSession { request, tx } => {
                        let result = request_with_timeout(&connection, request, request_timeout, "session/close").await;
                        let _ = tx.send(result);
                    }
                    ConnectionCommand::SetMode { request, tx } => {
                        let result = request_with_timeout(&connection, request, request_timeout, "session/set_mode").await;
                        let _ = tx.send(result);
                    }
                    ConnectionCommand::SetConfigOption { request, tx } => {
                        let result = request_with_timeout(&connection, request, request_timeout, "session/set_config_option").await;
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
                    ConnectionCommand::Prompt { request, mut cancel, tx } => {
                        let sent = connection.send_request(request);
                        let request_id = sent.id().clone();
                        // A user cancel signals `true`; teardown (sender
                        // dropped, or a non-true value) leaves the future
                        // pending so the EOF/terminate arms own the
                        // connection-closed semantics.
                        let cancelled = async {
                            match cancel.changed().await {
                                Ok(()) if *cancel.borrow() => (),
                                Ok(()) | Err(_) => std::future::pending().await,
                            }
                        };
                        let result = tokio::select! {
                            response = sent.block_task() => {
                                match response {
                                    Ok(response) => Ok(response),
                                    // The SDK fails pending requests when the
                                    // transport closes; map that onto the
                                    // host's connection-closed error so
                                    // shutdown resolution is deterministic.
                                    Err(_error) if matches!(*state_rx.borrow(), AgentConnectionState::Closed(_)) => {
                                        Err(AgentError::ConnectionClosed { agent_id: agent_id.clone() })
                                    }
                                    Err(error) => {
                                        tracing::warn!(agent_id, ?error, "prompt block_task failed");
                                        Err(AgentError::Rpc(error))
                                    }
                                }
                            }
                            _ = cancelled => {
                                let _ = connection.send_cancel_request(request_id);
                                Err(AgentError::Cancelled)
                            }
                            _ = terminate.changed() => {
                                let _ = connection.send_cancel_request(request_id);
                                Err(AgentError::ConnectionClosed { agent_id: agent_id.clone() })
                            }
                        };
                        let _ = tx.send(result);
                    }
                    ConnectionCommand::CancelSession { session_id } => {
                        let _ = connection.send_notification(CancelNotification::new(session_id));
                        // Turn cancel closes every logical MCP connection on
                        // this connection (Phase 6b lifecycle rule).
                        inner.mcp.close_all();
                    }
                    ConnectionCommand::Close => break,
                }
            }
        }
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
    if inner.complete_url_elicitation(&elicitation_id) {
        let _ = inner
            .events
            .send(AgentEvent::ElicitationCompleted { elicitation_id: notification.elicitation_id });
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

fn dispatch_client_request(
    inner: &Arc<AgentConnectionInner>,
    request: ClientRequest,
    responder: ee_agent_protocol::Responder<serde_json::Value>,
    cx: &ConnectionTo<AgentRole>,
) -> Result<(), RpcError> {
    let method = request.method().to_string();
    let session_id = request.session_id().cloned();
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
        inner.register_url_elicitation(mode.elicitation_id.0.as_ref());
    }
    let _ = inner.events.send(AgentEvent::ClientRequestDispatched {
        session_id: session_id.clone(),
        method: method.clone(),
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
