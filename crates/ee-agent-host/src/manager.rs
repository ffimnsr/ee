//! Agent manager: the host entry point the editor integration (ee-cli) owns.
//!
//! The manager holds the resolved agent configurations, lazily starts
//! connections on first session creation, and never spawns a subprocess
//! unless a session is actually requested.  Dropping the manager closes
//! every connection (killing subprocesses and resolving pending work).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::connection::{AgentConnection, AgentConnectionOptions};
use crate::error::AgentError;
use crate::events::AgentEvent;
use crate::inbound::ClientRequestHandler;
use crate::process::AgentProcessConfig;
use crate::session::AgentThread;

#[cfg(feature = "test-utils")]
use crate::fake::FakeAgentTransport;

/// Builds one fake agent transport for a connection (test-utils only).
///
/// Implementations call [`crate::fake::FakeAgent::spawn`] and return the
/// transport; keeping the spawned [`crate::fake::FakeAgent`] handle is the
/// caller's responsibility (via interior mutability) so tests can assert on
/// the host's requests.
#[cfg(feature = "test-utils")]
pub trait FakeTransportFactory: Send + Sync + 'static {
    /// Builds the transport for one connection.
    fn build(&self) -> FakeAgentTransport;
}

/// Resolved agents-mode configuration the manager operates on.
#[derive(Clone, Default)]
pub struct AgentManagerConfig {
    /// Agent id → launch config.
    pub agents: BTreeMap<String, AgentProcessConfig>,
    /// Whether the ee MCP proxy is configured (arms ACP-native MCP-over-ACP
    /// hosting on every connection; the agent still has to advertise
    /// `mcp_capabilities.acp`).
    pub ee_proxy_enabled: bool,
    /// Test-only: agent id → fake transport factory.  When present for an
    /// agent, the manager connects over the fake transport instead of
    /// spawning a subprocess.
    #[cfg(feature = "test-utils")]
    pub fake_transports: BTreeMap<String, Arc<dyn FakeTransportFactory>>,
}

impl std::fmt::Debug for AgentManagerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentManagerConfig")
            .field("agents", &self.agents.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// Owns lazy agent connections and session creation.
#[derive(Clone)]
pub struct AgentManager {
    config: Arc<AgentManagerConfig>,
    handler: Arc<dyn ClientRequestHandler>,
    events: mpsc::UnboundedSender<AgentEvent>,
    options: AgentConnectionOptions,
    connections: Arc<Mutex<BTreeMap<String, AgentConnection>>>,
}

impl std::fmt::Debug for AgentManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentManager")
            .field("agents", &self.config.agents.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl AgentManager {
    /// Creates a manager over the resolved config.
    ///
    /// No subprocess is started here; connections start lazily on
    /// [`Self::new_session`].
    #[must_use]
    pub fn new(
        config: AgentManagerConfig,
        handler: Arc<dyn ClientRequestHandler>,
        events: mpsc::UnboundedSender<AgentEvent>,
    ) -> Self {
        Self::with_options(config, handler, events, AgentConnectionOptions::default())
    }

    /// Creates a manager with explicit connection tuning (tests).
    #[must_use]
    pub fn with_options(
        config: AgentManagerConfig,
        handler: Arc<dyn ClientRequestHandler>,
        events: mpsc::UnboundedSender<AgentEvent>,
        options: AgentConnectionOptions,
    ) -> Self {
        Self {
            config: Arc::new(config),
            handler,
            events,
            options,
            connections: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// The configured agent ids.
    #[must_use]
    pub fn agent_ids(&self) -> Vec<String> {
        self.config.agents.keys().cloned().collect()
    }

    /// Whether `agent_id` is configured.
    #[must_use]
    pub fn has_agent(&self, agent_id: &str) -> bool {
        self.config.agents.contains_key(agent_id)
    }

    /// The default agent id (`agents.default_agent` resolved by the caller),
    /// falling back to the single configured agent when unambiguous.
    #[must_use]
    pub fn resolve_default_agent(&self, default_agent: Option<&str>) -> Option<String> {
        if let Some(default) = default_agent {
            return self.config.agents.contains_key(default).then(|| default.to_string());
        }
        match self.config.agents.len() {
            1 => self.config.agents.keys().next().cloned(),
            _ => None,
        }
    }

    /// Returns the live connection for `agent_id`, starting it lazily when
    /// needed.
    ///
    /// # Errors
    ///
    /// Fails when the agent is not configured or the subprocess cannot
    /// start.
    pub async fn connection(&self, agent_id: &str) -> Result<AgentConnection, AgentError> {
        let mut options = self.options;
        options.ee_proxy_enabled = self.config.ee_proxy_enabled;
        self.connection_with_options(agent_id, options).await
    }

    /// Like [`Self::connection`] with explicit options (the config's
    /// `ee_proxy_enabled` still wins over the caller's options).
    async fn connection_with_options(
        &self,
        agent_id: &str,
        options: AgentConnectionOptions,
    ) -> Result<AgentConnection, AgentError> {
        let mut options = options;
        options.ee_proxy_enabled = self.config.ee_proxy_enabled;
        if let Some(connection) =
            self.connections.lock().expect("connections poisoned").get(agent_id)
        {
            return Ok(connection.clone());
        }
        let config = self
            .config
            .agents
            .get(agent_id)
            .cloned()
            .ok_or_else(|| AgentError::UnknownAgent(agent_id.to_string()))?;
        #[cfg(feature = "test-utils")]
        if let Some(factory) = self.config.fake_transports.get(agent_id) {
            let connection = AgentConnection::connect_with_transport(
                agent_id.to_string(),
                self.handler.clone(),
                self.events.clone(),
                options,
                factory.build(),
            )?;
            self.connections
                .lock()
                .expect("connections poisoned")
                .insert(agent_id.to_string(), connection.clone());
            return Ok(connection);
        }
        let connection = AgentConnection::connect(
            agent_id.to_string(),
            config,
            self.handler.clone(),
            self.events.clone(),
            options,
        )
        .await?;
        self.connections
            .lock()
            .expect("connections poisoned")
            .insert(agent_id.to_string(), connection.clone());
        Ok(connection)
    }

    /// Starts a new session on `agent_id` (lazily starting the connection).
    ///
    /// `worktree_roots` are absolute paths; the first root becomes the ACP
    /// `cwd` and the rest are forwarded as `additionalDirectories`.
    /// `mcp_servers` are forwarded as ACP `session/new` `mcpServers` (the
    /// agent connects to them directly; ee never starts them itself here).
    /// `ee_proxy_stdio_fallback` carries the stdio `ee --mcp-proxy` entry
    /// when proxy mode is configured; the host swaps it for an ACP-native
    /// [`ee_agent_protocol::McpServer::Acp`] entry when the agent supports
    /// MCP-over-ACP.
    ///
    /// # Errors
    ///
    /// Fails when the agent is unknown, the connection handshake fails, or
    /// the agent rejects `session/new`.
    pub async fn new_session(
        &self,
        agent_id: &str,
        worktree_roots: Vec<PathBuf>,
        mcp_servers: Vec<ee_agent_protocol::McpServer>,
        ee_proxy_stdio_fallback: Option<ee_agent_protocol::McpServerStdio>,
    ) -> Result<AgentThread, AgentError> {
        self.connection(agent_id)
            .await?
            .new_session(worktree_roots, mcp_servers, ee_proxy_stdio_fallback)
            .await
    }

    /// Loads an existing session; only when the agent advertises the
    /// `load_session` capability.
    ///
    /// # Errors
    ///
    /// Fails when the capability is missing, roots are invalid, or the agent
    /// rejects the load.
    pub async fn load_session(
        &self,
        agent_id: &str,
        session_id: ee_agent_protocol::SessionId,
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        mcp_servers: Vec<ee_agent_protocol::McpServer>,
    ) -> Result<AgentThread, AgentError> {
        self.connection(agent_id)
            .await?
            .load_session(session_id, cwd, additional_directories, mcp_servers)
            .await
    }

    /// Lists existing sessions on `agent_id`; only when the agent advertises
    /// `sessionCapabilities.list`.
    pub async fn list_sessions(
        &self,
        agent_id: &str,
        cwd: Option<PathBuf>,
        cursor: Option<String>,
    ) -> Result<ee_agent_protocol::ListSessionsResponse, AgentError> {
        self.connection(agent_id).await?.list_sessions(cwd, cursor).await
    }

    /// Deletes one existing session on `agent_id`; only when the agent
    /// advertises `sessionCapabilities.delete`.
    pub async fn delete_session(
        &self,
        agent_id: &str,
        session_id: ee_agent_protocol::SessionId,
    ) -> Result<ee_agent_protocol::DeleteSessionResponse, AgentError> {
        self.connection(agent_id).await?.delete_session(session_id).await
    }

    /// Resumes one existing session on `agent_id`; only when the agent
    /// advertises `sessionCapabilities.resume`.
    pub async fn resume_session(
        &self,
        agent_id: &str,
        session_id: ee_agent_protocol::SessionId,
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        mcp_servers: Vec<ee_agent_protocol::McpServer>,
    ) -> Result<AgentThread, AgentError> {
        self.connection(agent_id)
            .await?
            .resume_session(session_id, cwd, additional_directories, mcp_servers)
            .await
    }

    /// Closes one active session on `agent_id`; only when the agent
    /// advertises `sessionCapabilities.close`.
    pub async fn close_session(
        &self,
        agent_id: &str,
        session_id: ee_agent_protocol::SessionId,
    ) -> Result<ee_agent_protocol::CloseSessionResponse, AgentError> {
        self.connection(agent_id).await?.close_session(session_id).await
    }

    /// Closes the connection for `agent_id` (kills the subprocess, resolves
    /// pending work).  Idempotent.
    pub async fn close_agent(&self, agent_id: &str) {
        let connection = self.connections.lock().expect("connections poisoned").remove(agent_id);
        if let Some(connection) = connection {
            connection.close().await;
        }
    }

    /// Closes every connection.  Called on app shutdown.
    pub async fn shutdown(&self) {
        let connections = {
            let mut guard = self.connections.lock().expect("connections poisoned");
            std::mem::take(&mut *guard)
        };
        for (_, connection) in connections {
            connection.close().await;
        }
    }

    /// Number of live connections (tests and status lines).
    #[must_use]
    pub fn live_connection_count(&self) -> usize {
        self.connections.lock().expect("connections poisoned").len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbound::DenyAllHandler;
    use crate::process::AgentProcessConfig;

    fn manager() -> AgentManager {
        let mut config = AgentManagerConfig::default();
        config.agents.insert("primary".into(), AgentProcessConfig::new("echo"));
        config.agents.insert("secondary".into(), AgentProcessConfig::new("cat"));
        let (events, _rx) = mpsc::unbounded_channel();
        AgentManager::new(config, Arc::new(DenyAllHandler), events)
    }

    #[tokio::test]
    async fn manager_starts_with_no_connections() {
        let manager = manager();
        assert_eq!(manager.live_connection_count(), 0);
        assert_eq!(manager.agent_ids(), vec!["primary".to_string(), "secondary".to_string()]);
    }

    #[tokio::test]
    async fn resolve_default_agent_uses_explicit_or_singleton() {
        let manager = manager();
        assert_eq!(manager.resolve_default_agent(Some("secondary")), Some("secondary".into()));
        assert_eq!(manager.resolve_default_agent(Some("missing")), None);
        assert_eq!(manager.resolve_default_agent(None), None); // ambiguous

        let mut single = AgentManagerConfig::default();
        single.agents.insert("only".into(), AgentProcessConfig::new("echo"));
        let (events, _rx) = mpsc::unbounded_channel();
        let single_manager = AgentManager::new(single, Arc::new(DenyAllHandler), events);
        assert_eq!(single_manager.resolve_default_agent(None), Some("only".into()));
    }

    #[tokio::test]
    async fn unknown_agent_fails_typed() {
        let manager = manager();
        let error = manager
            .new_session("nope", vec![PathBuf::from("/tmp")], Vec::new(), None)
            .await
            .unwrap_err();
        assert!(matches!(error, AgentError::UnknownAgent(_)));
    }
}
