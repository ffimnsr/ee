//! Connection-scoped MCP-over-ACP registry.
use super::*;

///
/// Created for every connection; *armed* (accepting `mcp/connect`) only when
/// the host was configured with the ee proxy enabled.  All state is
/// connection-scoped because the `mcp/*` wire methods carry no session id.
pub(crate) struct McpOverAcpRegistry {
    /// The ACP `McpServer::Acp` server id this connection advertises, when
    /// the ee proxy is enabled for it.
    server_id: Option<McpServerAcpId>,
    connections: Mutex<HashMap<McpConnectionId, Arc<LogicalConnection>>>,
    next_connection_id: AtomicU64,
    /// Proxy tool call executor (runs on the host runtime).
    jobs: mpsc::UnboundedSender<ProxyJob>,
    /// Agent stderr capture for the `ee_diagnostics` tool.
    process: Arc<Mutex<Option<AgentProcess>>>,
    /// Active host session threads for this agent connection. MCP wire calls
    /// contain no session id, so evidence retrieval must resolve only through
    /// this connection-owned map.
    threads: Arc<Mutex<HashMap<SessionId, Arc<ThreadShared>>>>,
    agent_id: String,
    proxy_discovery: bool,
    workspace_memory: Arc<WorkspaceMemoryHost>,
    tool_profile: EeProxyToolProfile,
}

impl McpOverAcpRegistry {
    /// Creates the registry; `enabled` arms MCP-over-ACP for this
    /// connection.  The executor task is spawned on the current runtime and
    /// exits when the connection drops the jobs sender.
    pub(crate) fn new(
        enabled: bool,
        agent_id: &str,
        handler: Arc<dyn ClientRequestHandler>,
        process: Arc<Mutex<Option<AgentProcess>>>,
        threads: Arc<Mutex<HashMap<SessionId, Arc<ThreadShared>>>>,
        workspace_memory: Arc<WorkspaceMemoryHost>,
        tool_profile: EeProxyToolProfile,
    ) -> Self {
        let handler_capabilities = handler.capabilities();
        let (jobs_tx, jobs_rx) = mpsc::unbounded_channel();
        if enabled {
            tokio::spawn(proxy_executor(handler, handler_capabilities, jobs_rx));
        } else {
            drop(jobs_rx);
        }
        let server_id = enabled.then(|| McpServerAcpId::new(format!("ee-mcp-proxy:{agent_id}")));
        Self {
            server_id,
            connections: Mutex::new(HashMap::new()),
            next_connection_id: AtomicU64::new(0),
            jobs: jobs_tx,
            process,
            threads,
            agent_id: agent_id.to_owned(),
            proxy_discovery: handler_capabilities.proxy_discovery,
            workspace_memory,
            tool_profile,
        }
    }

    /// The advertised ACP server id, when the proxy is armed.
    pub(crate) fn server_id(&self) -> Option<&McpServerAcpId> {
        self.server_id.as_ref()
    }

    /// Whether `server_id` is the ee proxy server this connection hosts.
    pub(super) fn is_our_server(&self, server_id: &McpServerAcpId) -> bool {
        self.server_id.as_ref().is_some_and(|ours| ours == server_id)
    }

    /// Whether a logical connection with `connection_id` exists.
    pub(super) fn connection(
        &self,
        connection_id: &McpConnectionId,
    ) -> Option<Arc<LogicalConnection>> {
        self.connections.lock().expect("mcp connections poisoned").get(connection_id).cloned()
    }

    /// Handles `mcp/connect`: validates the server id and the agent's
    /// advertised capability, starts the rmcp serve thread for a fresh
    /// logical connection, and answers with the new connection id.
    ///
    /// Unknown server ids and connections for unadvertised capabilities fail
    /// closed with JSON-RPC invalid params.
    pub(crate) fn handle_connect(
        &self,
        request: ConnectMcpRequest,
        responder: Responder<ConnectMcpResponse>,
        supports_acp: bool,
    ) -> Result<(), RpcError> {
        if !supports_acp {
            return responder.respond_with_error(RpcError::invalid_params().data(
                serde_json::json!({ "reason": "agent did not advertise mcp_capabilities.acp" }),
            ));
        }
        if !self.is_our_server(&request.server_id) {
            return responder.respond_with_error(RpcError::invalid_params().data(
                serde_json::json!({ "reason": "unknown MCP server id", "serverId": request.server_id }),
            ));
        }

        let connection_id = McpConnectionId::new(format!(
            "ee-mcp:{}:{}",
            self.server_id.as_ref().expect("armed server id"),
            self.next_connection_id.fetch_add(1, Ordering::Relaxed),
        ));
        let (rx_tx, rx_rx) = mpsc::unbounded_channel::<RxJsonRpcMessage<RoleServer>>();
        let pending =
            Arc::new(Mutex::new(HashMap::<RequestId, oneshot::Sender<PendingReply>>::new()));
        let shutdown = CancellationToken::new();
        let connection = Arc::new(LogicalConnection {
            rx_tx,
            pending: pending.clone(),
            shutdown: shutdown.clone(),
            next_inner_id: AtomicI64::new(0),
        });

        // The serve thread runs its own current_thread runtime: proxy tool
        // calls block it synchronously while awaiting the host-side
        // approval round trip (never blocking the host runtime).
        let backend = HostProxyBackend {
            jobs: self.jobs.clone(),
            process: self.process.clone(),
            threads: self.threads.clone(),
            agent_id: self.agent_id.clone(),
            scope: connection_id.to_string(),
            workspace_memory: self.workspace_memory.clone(),
            shutdown: shutdown.clone(),
            supported_tools: match self.tool_profile {
                EeProxyToolProfile::Full => (!self.proxy_discovery).then(Vec::new),
                EeProxyToolProfile::CriticReadOnly => Some(
                    ee_mcp::critic_read_only_tool_names(ee_mcp::ToolTransport::Acp)
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                ),
            },
        };
        let proxy = EeMcpProxy::new(Arc::new(backend));
        let transport = McpOverAcpTransport { rx: rx_rx, pending: pending.clone() };
        let thread_connection = connection.clone();
        let thread_name = format!("ee-mcp-over-acp:{}", connection_id);
        let spawn_result = std::thread::Builder::new().name(thread_name).spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("mcp-over-acp serve runtime");
            runtime.block_on(async move {
                // `serve_server_with_ct` resolves after first-request
                // `server/discover` negotiation and returns running service;
                // await it so transport and rx channel stay alive until
                // connection cancellation or closure.
                if let Ok(running) =
                    rmcp::service::serve_server_with_ct(proxy, transport, shutdown.clone()).await
                {
                    let _ = running.waiting().await;
                }
                // Resolve any still-pending inner requests so awaiting
                // mcp/message responders never hang.
                thread_connection.close();
            });
        });
        if let Err(error) = spawn_result {
            return responder.respond_with_error(RpcError::internal_error().data(
                serde_json::json!({ "reason": format!("serve thread spawn failed: {error}") }),
            ));
        }
        self.connections
            .lock()
            .expect("mcp connections poisoned")
            .insert(connection_id.clone(), connection);
        responder.respond(ConnectMcpResponse::new(connection_id))
    }

    /// Handles an `mcp/message` request: forwards the inner MCP message into
    /// the logical connection's rmcp serve loop and answers with the inner
    /// response once rmcp produces it.
    ///
    /// Messages for unknown connections (including before `mcp/connect`),
    /// after disconnect, and oversized frames fail closed with invalid
    /// params; oversized frames also close the logical connection.
    pub(crate) fn handle_message(
        &self,
        request: MessageMcpRequest,
        responder: Responder<MessageMcpResponse>,
        cx: &ConnectionTo<AgentRole>,
        supports_acp: bool,
    ) -> Result<(), RpcError> {
        if !supports_acp {
            return responder.respond_with_error(RpcError::invalid_params().data(
                serde_json::json!({ "reason": "agent did not advertise mcp_capabilities.acp" }),
            ));
        }
        let Some(connection) = self.connection(&request.connection_id) else {
            return responder.respond_with_error(RpcError::invalid_params().data(
                serde_json::json!({
                    "reason": "unknown MCP connection (mcp/message before mcp/connect?)",
                    "connectionId": request.connection_id,
                }),
            ));
        };

        // Frame cap: the full `mcp/message` params payload the agent sent
        // (connection id + inner method + inner params), matching the stdio
        // proxy line cap.  Oversized frames fail closed and close the
        // logical connection (no partial parse).
        let frame = serde_json::json!({
            "connectionId": request.connection_id,
            "method": request.method,
            "params": request.params,
        });
        let frame_bytes = serde_json::to_string(&frame).map_or(0, |text| text.len());
        if frame_bytes > MCP_OVER_ACP_MAX_FRAME_BYTES {
            self.close_connection(&request.connection_id);
            return responder.respond_with_error(RpcError::invalid_params().data(
                serde_json::json!({
                    "reason": format!(
                        "mcp/message frame exceeds the {MCP_OVER_ACP_MAX_FRAME_BYTES}-byte cap"
                    ),
                    "connectionId": request.connection_id,
                }),
            ));
        }

        let inner_id = connection.next_inner_id.fetch_add(1, Ordering::Relaxed) + 1;
        let inner_message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": inner_id,
            "method": request.method,
            "params": request.params,
        });
        let Ok(message) = serde_json::from_value::<RxJsonRpcMessage<RoleServer>>(inner_message)
        else {
            return responder.respond_with_error(RpcError::invalid_params().data(
                serde_json::json!({
                    "reason": "inner MCP message is not a valid request",
                    "connectionId": request.connection_id,
                }),
            ));
        };

        let (reply_tx, reply_rx) = oneshot::channel();
        connection
            .pending
            .lock()
            .expect("pending map poisoned")
            .insert(RequestId::Number(inner_id), reply_tx);
        if connection.rx_tx.send(message).is_err() {
            connection
                .pending
                .lock()
                .expect("pending map poisoned")
                .remove(&RequestId::Number(inner_id));
            self.close_connection(&request.connection_id);
            return responder.respond_with_error(RpcError::invalid_params().data(
                serde_json::json!({
                    "reason": "MCP connection is closed",
                    "connectionId": request.connection_id,
                }),
            ));
        }

        let spawned = cx.spawn(async move {
            let outcome = match reply_rx.await {
                Ok(PendingReply::Message(item)) => *item,
                Ok(PendingReply::Closed) | Err(_) => {
                    return responder.respond_with_error(
                        RpcError::request_cancelled()
                            .data(serde_json::json!({ "reason": "MCP connection closed" })),
                    );
                }
            };
            match outcome {
                TxServerMessage::Response(response) => {
                    let value = serde_json::to_value(response.result)
                        .map_err(RpcError::into_internal_error)?;
                    let response = MessageMcpResponse::from_value("mcp/message", value)?;
                    responder.respond(response)
                }
                TxServerMessage::Error(error) => responder.respond_with_error(RpcError::new(
                    error.error.code.0,
                    error.error.message.to_string(),
                )),
                _ => responder.respond_with_error(
                    RpcError::internal_error()
                        .data(serde_json::json!({ "reason": "unexpected inner MCP message" })),
                ),
            }
        });
        if spawned.is_err() {
            // Connection is shutting down; the responder dies with it.
            tracing::debug!("mcp/message responder dropped: connection closing");
        }
        Ok(())
    }

    /// Handles an `mcp/message` notification (for example,
    /// `notifications/cancelled`). Unknown connections are dropped with a
    /// debug log (notifications carry no response channel); oversized frames
    /// fail closed and close the logical connection, like requests.
    pub(crate) fn handle_notification(&self, notification: MessageMcpNotification) {
        let Some(connection) = self.connection(&notification.connection_id) else {
            tracing::debug!(
                connection_id = %notification.connection_id.0,
                "dropping mcp/message notification for unknown connection"
            );
            return;
        };
        let frame = serde_json::json!({
            "connectionId": notification.connection_id,
            "method": notification.method,
            "params": notification.params,
        });
        if serde_json::to_string(&frame).map_or(0, |text| text.len()) > MCP_OVER_ACP_MAX_FRAME_BYTES
        {
            tracing::warn!(
                connection_id = %notification.connection_id.0,
                "closing mcp-over-acp connection: notification exceeds the frame cap"
            );
            self.close_connection(&notification.connection_id);
            return;
        }
        let inner = serde_json::json!({
            "jsonrpc": "2.0",
            "method": notification.method,
            "params": notification.params,
        });
        match serde_json::from_value::<RxJsonRpcMessage<RoleServer>>(inner) {
            Ok(message) => {
                if connection.rx_tx.send(message).is_err() {
                    tracing::debug!("dropping mcp/message notification: connection closed");
                }
            }
            Err(error) => {
                tracing::warn!(?error, "dropping invalid inner MCP notification");
            }
        }
    }

    /// Handles `mcp/disconnect`: closes the logical connection and answers
    /// with an empty response.  Unknown connection ids fail closed with
    /// invalid params.
    pub(crate) fn handle_disconnect(
        &self,
        request: DisconnectMcpRequest,
        responder: Responder<DisconnectMcpResponse>,
    ) -> Result<(), RpcError> {
        let connection = self
            .connections
            .lock()
            .expect("mcp connections poisoned")
            .remove(&request.connection_id);
        let Some(connection) = connection else {
            return responder.respond_with_error(RpcError::invalid_params().data(
                serde_json::json!({
                    "reason": "unknown MCP connection",
                    "connectionId": request.connection_id,
                }),
            ));
        };
        connection.close();
        responder.respond(DisconnectMcpResponse::new())
    }

    /// Closes one logical connection (disconnect and fail-closed paths).
    pub(super) fn close_connection(&self, connection_id: &McpConnectionId) {
        if let Some(connection) =
            self.connections.lock().expect("mcp connections poisoned").remove(connection_id)
        {
            connection.close();
        }
    }

    /// Closes every logical connection.  Called on turn cancel, session
    /// close, agent disconnect, and app shutdown; idempotent.
    pub(crate) fn close_all(&self) {
        let connections = {
            let mut guard = self.connections.lock().expect("mcp connections poisoned");
            guard.drain().map(|(_, connection)| connection).collect::<Vec<_>>()
        };
        for connection in connections {
            connection.close();
        }
    }
}
