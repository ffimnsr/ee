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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ee_agent_protocol::{
    Agent as AgentRole, AgentCapabilities, AuthenticateRequest, AuthenticateResponse,
    CancelNotification, Client as ClientRole, ClientCapabilities, ConnectionTo,
    CreateElicitationRequest, CreateTerminalRequest, ElicitationCapabilities,
    ElicitationFormCapabilities, ElicitationUrlCapabilities, Error as RpcError,
    FileSystemCapabilities, Implementation, InitializeRequest, KillTerminalRequest,
    LoadSessionRequest, LoadSessionResponse, LogoutRequest, LogoutResponse, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse, ProtocolVersion, ReadTextFileRequest,
    ReleaseTerminalRequest, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SessionId, SessionNotification, SetSessionModeRequest,
    SetSessionModeResponse, TerminalOutputRequest, WaitForTerminalExitRequest,
    WriteTextFileRequest, on_receive_notification, on_receive_request,
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::error::AgentError;
use crate::events::{AgentConnectionState, AgentEvent, ConnectionCloseReason, PermissionRequestId};
use crate::inbound::{ClientRequest, ClientRequestHandler, HandlerCapabilities};
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
}

impl Default for AgentConnectionOptions {
    fn default() -> Self {
        Self {
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
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
    SetMode {
        request: SetSessionModeRequest,
        tx: oneshot::Sender<Result<SetSessionModeResponse, AgentError>>,
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
    pub process: Mutex<Option<AgentProcess>>,
    pub threads: Mutex<HashMap<SessionId, Arc<ThreadShared>>>,
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

    /// Notifies all session threads and the broker that the connection is
    /// gone, so no pending work outlives the process.  Runs at most once.
    pub fn notify_connection_closed(&self, reason: ConnectionCloseReason) {
        if self.closed_once.swap(true, Ordering::SeqCst) {
            return;
        }
        self.broker.cancel_all();
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

        let inner = Arc::new(AgentConnectionInner {
            agent_id: agent_id.clone(),
            state: state_tx,
            commands: commands_tx,
            broker: broker.clone(),
            events: events.clone(),
            handler: handler.clone(),
            handler_capabilities,
            process: Mutex::new(process),
            threads: Mutex::new(HashMap::new()),
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
                .client_info(Implementation::new("ee", env!("CARGO_PKG_VERSION")))
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
                main_inner.agent_id.clone(),
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
    /// # Errors
    ///
    /// Fails when the connection is not ready, roots are invalid, or the
    /// agent rejects `session/new`.
    pub async fn new_session(
        &self,
        worktree_roots: Vec<PathBuf>,
        mcp_servers: Vec<ee_agent_protocol::McpServer>,
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
        let request =
            NewSessionRequest::new(cwd).additional_directories(additional).mcp_servers(mcp_servers);

        let response =
            self.send_command(|tx| ConnectionCommand::NewSession { request, tx }).await?;
        Ok(self.spawn_thread(response.session_id, response.modes))
    }

    /// Loads an existing session; only allowed when the agent advertises
    /// the `load_session` capability.
    ///
    /// # Errors
    ///
    /// Fails when the capability is missing or the agent rejects the load.
    pub async fn load_session(&self, session_id: SessionId) -> Result<AgentThread, AgentError> {
        self.wait_ready().await?;
        if !self.supports_load_session() {
            return Err(AgentError::CapabilityUnsupported { method: "session/load".into() });
        }
        let request = LoadSessionRequest::new(session_id.clone(), PathBuf::new());
        let response =
            self.send_command(|tx| ConnectionCommand::LoadSession { request, tx }).await?;
        // ACP v1 `session/load` responses carry no session id; the requested
        // id is the thread's identity.
        Ok(self.spawn_thread(session_id, response.modes))
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

    /// Sends `logout`; only meaningful when the agent advertised auth.
    ///
    /// # Errors
    ///
    /// Fails when the connection is not ready or the agent rejects logout.
    pub async fn logout(&self) -> Result<LogoutResponse, AgentError> {
        self.wait_ready().await?;
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
    ) -> AgentThread {
        let thread =
            AgentThread::new(self.agent_id.clone(), session_id.clone(), modes, self.clone());
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

    /// Closes the connection: stops the driver, kills the subprocess, and
    /// resolves pending work.
    pub async fn close(&self) {
        let _ = self.inner.commands.send(ConnectionCommand::Close);
        let _ = self.inner.shutdown.send(true);
        let process = self.inner.process.lock().expect("process poisoned").take();
        if let Some(process) = process {
            process.kill().await;
        }
        self.inner.notify_connection_closed(ConnectionCloseReason::Closed);
    }

    /// Deregisters a session thread.
    pub(crate) fn deregister_thread(&self, session_id: &SessionId) {
        self.inner.threads.lock().expect("threads poisoned").remove(session_id);
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
    agent_id: String,
) {
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
                    ConnectionCommand::SetMode { request, tx } => {
                        let result = request_with_timeout(&connection, request, request_timeout, "session/set_mode").await;
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
    if !inner.handler_capabilities.supports(&method) {
        return responder.respond_with_error(
            AgentError::CapabilityUnsupported { method: method.clone() }.into_rpc(),
        );
    }
    let _ = inner.events.send(AgentEvent::ClientRequestDispatched {
        session_id: session_id.clone(),
        method: method.clone(),
    });
    let handler = inner.handler.clone();
    let spawned = cx.spawn(async move {
        match handler.handle(request).await {
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
        }
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
