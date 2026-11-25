use super::*;

impl AgentConnection {
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
}
