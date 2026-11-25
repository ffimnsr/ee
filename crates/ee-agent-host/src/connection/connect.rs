use super::*;

use super::driver::{driver_loop, implementation_to_v1};
use super::protocol::{build_client_builder, build_client_builder_v2, client_capabilities};

/// Non-option host stores threaded into one connection bootstrap.
struct ConnectionStores {
    process: Option<AgentProcess>,
    workspace_memory: Arc<WorkspaceMemoryHost>,
    permission_rules: Arc<PermissionRules>,
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
            ConnectionStores {
                process: Some(process),
                workspace_memory: WorkspaceMemoryHost::disabled(),
                permission_rules: Arc::new(PermissionRules::default()),
            },
            handler,
            events,
            options,
            transport,
        )
    }

    pub(crate) async fn connect_with_workspace_memory_and_rules(
        agent_id: String,
        config: AgentProcessConfig,
        handler: Arc<dyn ClientRequestHandler>,
        events: mpsc::UnboundedSender<AgentEvent>,
        options: AgentConnectionOptions,
        workspace_memory: Arc<WorkspaceMemoryHost>,
        permission_rules: Arc<PermissionRules>,
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
            ConnectionStores { process: Some(process), workspace_memory, permission_rules },
            handler,
            events,
            options,
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
            ConnectionStores {
                process: None,
                workspace_memory: WorkspaceMemoryHost::disabled(),
                permission_rules: Arc::new(PermissionRules::default()),
            },
            handler,
            events,
            options,
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
        permission_rules: Arc<PermissionRules>,
        transport: impl ee_agent_protocol::ConnectTo<ClientRole> + 'static,
    ) -> Result<Self, AgentError> {
        Self::start_connection(
            agent_id,
            ConnectionStores { process: None, workspace_memory, permission_rules },
            handler,
            events,
            options,
            transport,
        )
    }

    /// Shared connection bootstrap: channels, state, driver, and handshake.
    fn start_connection(
        agent_id: String,
        stores: ConnectionStores,
        handler: Arc<dyn ClientRequestHandler>,
        events: mpsc::UnboundedSender<AgentEvent>,
        options: AgentConnectionOptions,
        transport: impl ee_agent_protocol::ConnectTo<ClientRole> + 'static,
    ) -> Result<Self, AgentError> {
        let ConnectionStores { process, workspace_memory, permission_rules } = stores;
        let (commands_tx, commands_rx) = mpsc::unbounded_channel();
        let (state_tx, _state_rx) = watch::channel(AgentConnectionState::Starting);
        let (terminate_tx, terminate_rx) = watch::channel(false);
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let handler_capabilities = handler.capabilities();
        let broker = PermissionBroker::with_rules(permission_rules);
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
}
