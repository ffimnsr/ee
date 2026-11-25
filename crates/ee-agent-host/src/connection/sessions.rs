use super::*;

impl AgentConnection {
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
    pub(super) async fn send_command<T>(
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
}
