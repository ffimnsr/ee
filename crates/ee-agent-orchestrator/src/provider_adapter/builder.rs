//! `impl OrchestratorProvider`: construction, accessors, session state.
use super::*;

impl OrchestratorProvider {
    /// Creates an adapter with the default fail-closed policy (reads only).
    #[must_use]
    pub fn new(config: OrchestratorProviderConfig, model: Arc<dyn ModelAdapter>) -> Self {
        Self::with_policy(config, model, PolicyEngine::default())
    }
    /// Creates an adapter with a custom policy engine.
    #[must_use]
    pub fn with_policy(
        config: OrchestratorProviderConfig,
        model: Arc<dyn ModelAdapter>,
        policy: PolicyEngine,
    ) -> Self {
        Self::with_model_registry(
            config,
            crate::model_registry::ModelRegistry::single(model),
            policy,
        )
        .expect("single-adapter registry always contains default")
    }
    /// Creates a provider owning one validated process-wide model registry.
    /// Every new or restored session shares its adapters without serializing
    /// adapters, clients, or credentials.
    pub fn with_model_registry(
        config: OrchestratorProviderConfig,
        models: crate::model_registry::ModelRegistry,
        policy: PolicyEngine,
    ) -> Result<Self, ProviderError> {
        let workspace = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let plugins = crate::plugin_runtime::AgentPluginHost::new(
            crate::plugin_runtime::AgentPluginHostConfig::new(workspace),
            std::sync::Arc::new(crate::plugin_runtime::NoopAgentNotifier),
        )
        .map_err(ProviderError::from)?;
        Self::with_model_registry_and_host(config, models, policy, std::sync::Arc::new(plugins))
    }

    /// Creates a provider with an explicitly constructed agent plugin host.
    ///
    /// Providers register their model adapters into the host before calling
    /// this constructor; the host mirrors the registry for plugin metadata and
    /// deterministic routing hints.
    pub fn with_model_registry_and_host(
        config: OrchestratorProviderConfig,
        models: crate::model_registry::ModelRegistry,
        policy: PolicyEngine,
        plugins: std::sync::Arc<crate::plugin_runtime::AgentPluginHost>,
    ) -> Result<Self, ProviderError> {
        models.default_adapter().map_err(ProviderError::from)?;
        plugins.models().adopt_registry(&models).map_err(|error| {
            ProviderError::BackendFailure(format!("failed to mirror model registry: {error}"))
        })?;
        // Mandatory policy runs without the plugin gate; the gate is installed on
        // the effective engine the loop uses.
        plugins.policy().set_mandatory(policy.clone());
        let policy = plugins.gate_policy(policy);
        plugins.start().map_err(ProviderError::from)?;
        let session_store = Arc::new(SessionStateStore::new(
            config.session_state_dir.clone(),
            config.max_session_state_bytes,
        ));
        // Invalid user telemetry caps fail closed by disabling telemetry rather
        // than preventing the ACP provider from starting.
        let telemetry = TelemetryRecorder::new(config.telemetry.clone()).unwrap_or_default();
        Ok(Self {
            config,
            models: Arc::new(models),
            policy,
            plugins,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            persisted: Arc::new(Mutex::new(HashMap::new())),
            session_store,
            next_session: Arc::new(AtomicU64::new(1)),
            next_final_response: Arc::new(AtomicU64::new(1)),
            telemetry: Arc::new(Mutex::new(telemetry)),
            next_telemetry_turn: Arc::new(AtomicU64::new(1)),
        })
    }
    /// Non-secret model metadata owned by this provider, in stable route order.
    #[must_use]
    pub fn registered_models(&self) -> Vec<crate::model_registry::ModelInfo> {
        self.models.advertised()
    }

    /// Process-scoped agent plugin host.
    #[must_use]
    pub fn plugin_host(&self) -> std::sync::Arc<crate::plugin_runtime::AgentPluginHost> {
        self.plugins.clone()
    }

    /// Bounded plugin health/status snapshot for host forwarding.
    #[must_use]
    pub fn plugin_health(&self) -> crate::plugin_runtime::AgentHealthReport {
        self.plugins.health_report()
    }
    /// Exports retained privacy-safe telemetry as local JSONL. This never
    /// writes files or sends data over the network; caller owns destination.
    pub fn export_telemetry_jsonl(&self) -> Result<String, crate::observability::TelemetryError> {
        self.telemetry.lock().expect("telemetry recorder poisoned").export_jsonl()
    }
    /// Snapshot of the live task/memory state for one session, when the
    /// session exists.
    #[cfg(test)]
    pub(crate) fn session_state(&self, session_id: &str) -> Option<(TaskGraph, MemoryStore)> {
        let sessions = self.sessions.lock().expect("adapter sessions poisoned");
        let runtime = &sessions.get(session_id)?.runtime;
        Some((runtime.tasks(), runtime.memory()))
    }
    /// Names of the tools currently registered for one session (tests).
    #[cfg(test)]
    pub(crate) fn session_tool_names(&self, session_id: &str) -> Vec<String> {
        let sessions = self.sessions.lock().expect("adapter sessions poisoned");
        sessions.get(session_id).map(|session| session.runtime.tool_names()).unwrap_or_default()
    }
    /// The redacted MCP server descriptors of one session (tests).
    #[cfg(test)]
    pub(crate) fn session_mcp_servers(&self, session_id: &str) -> Vec<McpServerDescriptor> {
        let sessions = self.sessions.lock().expect("adapter sessions poisoned");
        sessions.get(session_id).map(|session| session.mcp_servers.clone()).unwrap_or_default()
    }
    /// Current session mode and its effective policy (tests).
    #[cfg(test)]
    pub(crate) fn session_mode_policy(
        &self,
        session_id: &str,
    ) -> Option<(SessionModeId, PolicyEngine)> {
        let sessions = self.sessions.lock().expect("adapter sessions poisoned");
        let session = sessions.get(session_id)?;
        Some((session.mode.clone(), session.runtime.policy()))
    }
    /// Whether a session's serialized state is still held for `session/load`.
    #[cfg(test)]
    pub(crate) fn has_persisted_state(&self, session_id: &str) -> bool {
        self.persisted.lock().expect("adapter persisted poisoned").contains_key(session_id)
    }
    /// Registers a deterministic server tool in an existing session for
    /// hermetic ACP replay fixtures. Production providers register tools only
    /// through built-ins and per-prompt MCP discovery.
    #[cfg(feature = "test-utils")]
    pub fn register_test_tool_for_session(
        &self,
        session_id: &str,
        tool: Arc<dyn crate::tools::ServerTool>,
    ) -> Result<(), ProviderError> {
        let sessions = self.sessions.lock().expect("adapter sessions poisoned");
        let session = sessions.get(session_id).ok_or_else(|| {
            ProviderError::InvalidRequest(format!("unknown orchestrator session: {session_id}"))
        })?;
        session.runtime.register_tool(tool).map_err(ProviderError::from)
    }
}
