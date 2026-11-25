use super::*;

impl AgentConnection {
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

    /// Delivers a prompt to a running turn or its queue, bypassing this
    /// connection's one-at-a-time session reservation.
    pub(crate) async fn steer_prompt(
        &self,
        session_id: SessionId,
        prompt: Vec<ee_agent_protocol::ContentBlock>,
        delivery: PromptDelivery,
    ) -> Result<PromptResponse, AgentError> {
        // The SDK prompt type cannot carry a top-level extension field, so the
        // delivery rides in `_meta`; the agent reads both forms.
        let mut request = PromptRequest::new(session_id, prompt);
        request.meta = Some(delivery.attach_to_meta(request.meta));
        self.send_command(|tx| ConnectionCommand::SteerPrompt { request, tx }).await
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
    /// Sends one session config option change through `session/set_config_option`.
    pub(crate) async fn set_config_option(
        &self,
        session_id: SessionId,
        config_id: ee_agent_protocol::SessionConfigId,
        value: SessionConfigOptionValue,
    ) -> Result<SetSessionConfigOptionResponse, AgentError> {
        let request = SetSessionConfigOptionRequest::new(session_id, config_id, value);
        self.send_command(|tx| ConnectionCommand::SetConfigOption { request, tx }).await
    }

    /// Sends `session/set_model` for an idle session.
    pub(crate) async fn set_model(
        &self,
        session_id: SessionId,
        model_id: String,
    ) -> Result<ee_agent_protocol::SetSessionModelResponse, AgentError> {
        let request = ee_agent_protocol::SetSessionModelRequest::new(session_id, model_id);
        self.send_command(|tx| ConnectionCommand::SetModel { request, tx }).await
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

    pub(super) fn prepare_local_thread_for_close(&self, session_id: &SessionId) {
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

    pub(super) fn close_local_thread(&self, session_id: &SessionId) {
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
