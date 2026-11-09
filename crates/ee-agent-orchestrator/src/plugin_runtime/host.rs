//! Agent plugin host.
//!
//! Owns discovery/activation (`PluginManager`), the `agent.*` service registry,
//! session and turn scopes, and the effect queue. Plugin callbacks never run
//! while the host holds a manager lock: effects are queued and applied by
//! [`AgentPluginHost::pump`] after the lock is released.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use ee_plugin_api::context::PluginContext;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::event::EventBus;
use ee_plugin_api::host::{HostAdapter, HostKind};
use ee_plugin_api::ids::PluginId;
use ee_plugin_api::service::ServiceRegistry;
use ee_plugin_api::wire::ServiceRequestParams;
use ee_plugin_runtime::diagnostics::Diagnostic;
use ee_plugin_runtime::discovery::DiscoveryOptions;
use ee_plugin_runtime::host_impl::{MemoryCommandRegistry, MemoryContributionSink};
use ee_plugin_runtime::manager::{ManagerStatus, PluginManager, PluginManagerConfig, PluginStatus};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::OrchestratorError;
use crate::plugin_runtime::bridge::AgentBridge;
use crate::plugin_runtime::effects::{
    AgentEffect, AgentEffectQueue, AgentEffectSink, ContextUpdateEffect, EffectReport,
    SessionControlEffect, ToolResultEffect,
};
use crate::plugin_runtime::services::{
    AgentServiceRouter, ContextService, ModelService, PolicyService, SessionHandle, SessionService,
    StrategyService, ToolService, TurnScope, ValidationService,
};
use crate::policy::PolicyEngine;

/// One queued notifier event.
#[derive(Debug, Clone)]
pub enum AgentNotifierEvent {
    Status(ManagerStatus),
    Health { plugin: String, state: String, reason: Option<String> },
    Alert(String),
}

/// Host-owned notification sink for frontend status and diagnostics.
pub trait AgentNotifier: Send + Sync + 'static {
    fn plugin_status(&self, status: &ManagerStatus);
    fn plugin_health(&self, plugin: &str, state: &str, reason: Option<&str>);
    fn alert(&self, message: &str);
}

/// Notifier used by tests and heads without a frontend.
#[derive(Default)]
pub struct NoopAgentNotifier;

impl AgentNotifier for NoopAgentNotifier {
    fn plugin_status(&self, _status: &ManagerStatus) {}
    fn plugin_health(&self, _plugin: &str, _state: &str, _reason: Option<&str>) {}
    fn alert(&self, _message: &str) {}
}

/// Thread-safe notifier that queues events for the owning process to drain.
#[derive(Default)]
pub struct QueuedAgentNotifier {
    events: Mutex<Vec<AgentNotifierEvent>>,
}

impl QueuedAgentNotifier {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn drain(&self) -> Vec<AgentNotifierEvent> {
        std::mem::take(&mut *self.events.lock().expect("notifier lock poisoned"))
    }
}

impl AgentNotifier for QueuedAgentNotifier {
    fn plugin_status(&self, status: &ManagerStatus) {
        self.events
            .lock()
            .expect("notifier lock poisoned")
            .push(AgentNotifierEvent::Status(status.clone()));
    }

    fn plugin_health(&self, plugin: &str, state: &str, reason: Option<&str>) {
        self.events.lock().expect("notifier lock poisoned").push(AgentNotifierEvent::Health {
            plugin: plugin.to_owned(),
            state: state.to_owned(),
            reason: reason.map(str::to_owned),
        });
    }

    fn alert(&self, message: &str) {
        self.events
            .lock()
            .expect("notifier lock poisoned")
            .push(AgentNotifierEvent::Alert(message.to_owned()));
    }
}

/// Serializable plugin status for frontend forwarding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPluginStatus {
    pub name: String,
    pub version: String,
    pub kind: String,
    pub active: bool,
    pub health: String,
    pub reason: Option<String>,
}

/// Bounded health/status snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentHealthReport {
    pub generation: u64,
    pub plugins: Vec<AgentPluginStatus>,
    pub diagnostics: usize,
}

impl From<&PluginStatus> for AgentPluginStatus {
    fn from(status: &PluginStatus) -> Self {
        Self {
            name: status.name.clone(),
            version: status.version.clone(),
            kind: status.kind.clone(),
            active: status.active,
            health: status.health.state.as_str().to_owned(),
            reason: status.health.reason.as_ref().map(|reason| reason.as_str().to_owned()),
        }
    }
}

/// Host configuration.
#[derive(Debug, Clone)]
pub struct AgentPluginHostConfig {
    pub manager: PluginManagerConfig,
    pub mandatory_policy: PolicyEngine,
    pub context_max_items: usize,
    pub context_max_bytes: usize,
}

impl AgentPluginHostConfig {
    /// Builds a config with no discovery roots (plugins must be added
    /// explicitly) and the default fail-closed policy.
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        let mut discovery = DiscoveryOptions::default();
        // Agent plugins are opt-in through an explicit environment path; the
        // host never reads the editor's user plugin tree from an agent process.
        if let Ok(paths) = std::env::var("EE_AGENT_PLUGIN_PATH") {
            discovery.push_env_paths(&paths);
        }
        Self {
            manager: PluginManagerConfig::new(
                HostKind::Agent,
                workspace.join(".ee-agent-plugins"),
                workspace,
            )
            .with_discovery(discovery),
            mandatory_policy: PolicyEngine::default(),
            context_max_items: crate::plugin_runtime::services::DEFAULT_CONTEXT_ITEMS,
            context_max_bytes: crate::plugin_runtime::services::DEFAULT_CONTEXT_BYTES,
        }
    }

    #[must_use]
    pub fn with_discovery(mut self, discovery: DiscoveryOptions) -> Self {
        self.manager.discovery = discovery;
        self
    }

    #[must_use]
    pub fn with_policy(mut self, policy: PolicyEngine) -> Self {
        self.mandatory_policy = policy;
        self
    }

    #[must_use]
    pub fn with_context_caps(mut self, max_items: usize, max_bytes: usize) -> Self {
        self.context_max_items = max_items.max(1);
        self.context_max_bytes = max_bytes.max(1);
        self
    }
}

/// Adapter handed to the runtime.
pub struct AgentHostAdapter {
    router: AgentServiceRouter,
    effects: Arc<AgentEffectQueue>,
}

impl AgentHostAdapter {
    #[must_use]
    pub fn new(router: AgentServiceRouter, effects: Arc<AgentEffectQueue>) -> Self {
        Self { router, effects }
    }
}

impl HostAdapter for AgentHostAdapter {
    fn host_kind(&self) -> HostKind {
        HostKind::Agent
    }

    fn submit_effects(
        &self,
        plugin: Option<&PluginId>,
        effects: Vec<ee_plugin_api::effect::Effect>,
    ) -> Result<(), PluginError> {
        for effect in effects {
            let effect = AgentEffect::from_plugin_effect(effect)?;
            self.effects.push(plugin.cloned(), effect);
        }
        Ok(())
    }

    fn handle_service_request(
        &self,
        plugin: &PluginId,
        request: &ServiceRequestParams,
    ) -> Result<Value, PluginError> {
        self.router.dispatch(plugin, &request.service, &request.method, request.params.clone())
    }
}

/// Agent plugin host.
pub struct AgentPluginHost {
    config: AgentPluginHostConfig,
    manager: Arc<Mutex<PluginManager>>,
    services: Arc<RwLock<ServiceRegistry>>,
    commands: Arc<MemoryCommandRegistry>,
    contributions: Arc<MemoryContributionSink>,
    tools: Arc<ToolService>,
    models: Arc<ModelService>,
    policy: Arc<PolicyService>,
    context: Arc<ContextService>,
    strategies: Arc<StrategyService>,
    sessions: Arc<SessionService>,
    validation: Arc<ValidationService>,
    router: AgentServiceRouter,
    bridge: AgentBridge,
    host_context: PluginContext,
    effects: Arc<AgentEffectQueue>,
    tool_results: Mutex<Vec<(Option<PluginId>, ToolResultEffect)>>,
    remote_providers: Mutex<BTreeSet<String>>,
    notifier: Arc<dyn AgentNotifier>,
    started: AtomicBool,
    shutdown: AtomicBool,
}

impl AgentPluginHost {
    /// Builds a host with the standard `agent.*` service set.
    pub fn new(
        config: AgentPluginHostConfig,
        notifier: Arc<dyn AgentNotifier>,
    ) -> Result<Self, OrchestratorError> {
        let events = EventBus::new();
        let services = Arc::new(RwLock::new(ServiceRegistry::new()));
        let tools = Arc::new(ToolService::new());
        let models = Arc::new(ModelService::new());
        let policy = Arc::new(PolicyService::new(config.mandatory_policy.clone(), events.clone()));
        let mut context_service = ContextService::new();
        context_service.set_caps(config.context_max_items, config.context_max_bytes);
        let context = Arc::new(context_service);
        let strategies = Arc::new(StrategyService::new());
        let sessions = Arc::new(SessionService::new(events.clone()));
        let validation = Arc::new(ValidationService::new());

        {
            let mut registry = services.write().expect("service lock poisoned");
            register(&mut registry, Arc::clone(&tools))?;
            register(&mut registry, Arc::clone(&models))?;
            register(&mut registry, Arc::clone(&policy))?;
            register(&mut registry, Arc::clone(&context))?;
            register(&mut registry, Arc::clone(&strategies))?;
            register(&mut registry, Arc::clone(&sessions))?;
            register(&mut registry, Arc::clone(&validation))?;
        }

        let router = AgentServiceRouter::new(
            Arc::clone(&tools),
            Arc::clone(&models),
            Arc::clone(&strategies),
            Arc::clone(&policy),
            Arc::clone(&context),
            Arc::clone(&sessions),
            Arc::clone(&validation),
        );
        let effects = Arc::new(AgentEffectQueue::new());
        let adapter = Arc::new(AgentHostAdapter::new(router.clone(), Arc::clone(&effects)));

        let contributions = Arc::new(MemoryContributionSink::new());
        let commands = Arc::new(MemoryCommandRegistry::new());
        let manager = Arc::new(Mutex::new(
            PluginManager::new(adapter.clone(), config.manager.clone())
                .with_services(Arc::clone(&services))
                .with_events(events.clone())
                .with_command_registry(commands.clone())
                .with_contribution_sink(contributions.clone()),
        ));
        let bridge = AgentBridge::new(
            events.clone(),
            Arc::clone(&manager),
            Arc::clone(&policy),
            Arc::clone(&context),
            Arc::clone(&strategies),
            Arc::clone(&sessions),
            Arc::clone(&effects),
        );
        let host_context = PluginContext::new("agent-host")
            .with_services(Arc::clone(&services))
            .with_events(events)
            .with_command_registry(commands.clone())
            .with_contribution_sink(contributions.clone());

        Ok(Self {
            config,
            manager,
            services,
            commands,
            contributions,
            tools,
            models,
            policy,
            context,
            strategies,
            sessions,
            validation,
            router,
            bridge,
            host_context,
            effects,
            tool_results: Mutex::new(Vec::new()),
            remote_providers: Mutex::new(BTreeSet::new()),
            notifier,
            started: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
        })
    }

    /// Starts discovery and activation once.
    pub fn start(&self) -> Result<(), OrchestratorError> {
        if self.started.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        if let Err(error) = self.start_inner() {
            self.started.store(false, Ordering::SeqCst);
            return Err(error);
        }
        Ok(())
    }

    fn start_inner(&self) -> Result<(), OrchestratorError> {
        {
            let mut manager = self.manager.lock().expect("manager lock poisoned");
            manager.start().map_err(|error| OrchestratorError::InvalidState(error.to_string()))?;
        }
        self.sync_permissions();
        self.publish_status();
        Ok(())
    }

    /// Stops every plugin and closes live session scopes.
    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.started.store(false, Ordering::SeqCst);
        self.sessions.close_all("host shutdown");
        let plugins = self.plugin_ids();
        for plugin in plugins {
            self.router.dispose_plugin(&plugin);
        }
        {
            let mut manager = self.manager.lock().expect("manager lock poisoned");
            manager.stop();
        }
        self.publish_status();
    }

    #[must_use]
    pub fn is_started(&self) -> bool {
        self.started.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Drains native messages and applies queued effects. Returns plugin
    /// failures as `(plugin, message)` pairs.
    pub fn pump(&self) -> Vec<(String, String)> {
        let failures = {
            let mut manager = self.manager.lock().expect("manager lock poisoned");
            manager.pump().into_iter().map(|(plugin, error)| (plugin, error.to_string())).collect()
        };
        let queued = self.effects.drain();
        let report = crate::plugin_runtime::effects::apply_effects(self, queued);
        for (index, error) in &report.errors {
            tracing::warn!(index, error = %error, "agent effect failed");
        }
        if !report.is_ok() {
            self.publish_status();
        }
        failures
    }

    /// Creates the plugin scope for one agent session.
    pub fn session_scope(&self, session_id: &str) -> Result<SessionHandle, OrchestratorError> {
        self.sessions
            .create(&self.host_context, session_id)
            .map_err(|error| OrchestratorError::InvalidState(error.to_string()))
    }

    /// Creates a child scope for one prompt turn.
    pub fn turn_scope(&self, session_id: &str, turn: u64) -> Result<TurnScope, OrchestratorError> {
        let session = self.sessions.session(session_id).ok_or_else(|| {
            OrchestratorError::InvalidState(format!("unknown session {session_id}"))
        })?;
        session.turn_scope(turn).map_err(|error| OrchestratorError::InvalidState(error.to_string()))
    }

    /// Bridge handle used by providers and the runtime observer.
    #[must_use]
    pub fn bridge(&self) -> AgentBridge {
        self.bridge.clone()
    }

    /// Mandatory policy with the plugin gate installed.
    #[must_use]
    pub fn effective_policy(&self) -> PolicyEngine {
        self.policy.mandatory().with_plugin_gate(self.policy.clone())
    }

    /// Installs the plugin gate on a policy derived from host mode changes.
    #[must_use]
    pub fn gate_policy(&self, policy: PolicyEngine) -> PolicyEngine {
        policy.with_plugin_gate(self.policy.clone())
    }

    /// Replaces the mandatory policy engine.
    pub fn set_mandatory_policy(&self, policy: PolicyEngine) {
        self.policy.set_mandatory(policy);
    }

    #[must_use]
    pub fn tools(&self) -> &Arc<ToolService> {
        &self.tools
    }

    #[must_use]
    pub fn models(&self) -> &Arc<ModelService> {
        &self.models
    }

    #[must_use]
    pub fn policy(&self) -> &Arc<PolicyService> {
        &self.policy
    }

    #[must_use]
    pub fn context(&self) -> &Arc<ContextService> {
        &self.context
    }

    #[must_use]
    pub fn strategies(&self) -> &Arc<StrategyService> {
        &self.strategies
    }

    #[must_use]
    pub fn sessions(&self) -> &Arc<SessionService> {
        &self.sessions
    }

    #[must_use]
    pub fn validation(&self) -> &Arc<ValidationService> {
        &self.validation
    }

    #[must_use]
    pub fn router(&self) -> &AgentServiceRouter {
        &self.router
    }

    #[must_use]
    pub fn services(&self) -> Arc<RwLock<ServiceRegistry>> {
        Arc::clone(&self.services)
    }

    #[must_use]
    pub fn commands(&self) -> &Arc<MemoryCommandRegistry> {
        &self.commands
    }

    #[must_use]
    pub fn contributions(&self) -> &Arc<MemoryContributionSink> {
        &self.contributions
    }

    #[must_use]
    pub fn config(&self) -> &AgentPluginHostConfig {
        &self.config
    }

    /// Current manager status.
    #[must_use]
    pub fn status(&self) -> ManagerStatus {
        self.manager.lock().expect("manager lock poisoned").status()
    }

    /// Current diagnostics.
    #[must_use]
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        self.manager.lock().expect("manager lock poisoned").diagnostics().to_vec()
    }

    /// Bounded health snapshot for frontend forwarding.
    #[must_use]
    pub fn health_report(&self) -> AgentHealthReport {
        let manager = self.manager.lock().expect("manager lock poisoned");
        let status = manager.status();
        let diagnostics = manager.diagnostics().len();
        AgentHealthReport {
            generation: status.generation,
            plugins: status.plugins.iter().map(AgentPluginStatus::from).collect(),
            diagnostics,
        }
    }

    /// Takes structured tool results produced by plugins since the last call.
    pub fn take_tool_results(&self) -> Vec<(Option<PluginId>, ToolResultEffect)> {
        std::mem::take(&mut *self.tool_results.lock().expect("tool result lock poisoned"))
    }

    /// Records a remote ACP/MCP provider transport.
    ///
    /// Remote providers contribute tools/models over their own transport; they
    /// are never registered as in-process `ServerTool` values here, so host
    /// policy, approval, redaction, and manifest gating stay authoritative.
    pub fn register_remote_provider(&self, id: impl Into<String>) {
        self.remote_providers.lock().expect("remote provider lock poisoned").insert(id.into());
    }

    /// Sorted remote provider ids.
    #[must_use]
    pub fn remote_providers(&self) -> Vec<String> {
        self.remote_providers
            .lock()
            .expect("remote provider lock poisoned")
            .iter()
            .cloned()
            .collect()
    }

    /// Applies one batch of queued effects immediately.
    pub fn apply_pending_effects(&self) -> EffectReport {
        crate::plugin_runtime::effects::apply_effects(self, self.effects.drain())
    }

    fn sync_permissions(&self) {
        let permissions: Vec<(String, BTreeSet<String>)> = {
            let manager = self.manager.lock().expect("manager lock poisoned");
            manager
                .manifests()
                .iter()
                .map(|(name, manifest)| {
                    let granted = manifest
                        .permissions
                        .0
                        .iter()
                        .filter(|(_, enabled)| **enabled)
                        .map(|(permission, _)| permission.clone())
                        .collect::<BTreeSet<_>>();
                    (name.clone(), granted)
                })
                .collect()
        };
        for (name, granted) in permissions {
            if let Ok(plugin) = PluginId::new(name) {
                self.router.set_permissions(&plugin, granted);
            }
        }
    }

    fn publish_status(&self) {
        let status = self.status();
        self.notifier.plugin_status(&status);
        for plugin in &status.plugins {
            self.notifier.plugin_health(
                &plugin.name,
                plugin.health.state.as_str(),
                plugin.health.reason.as_ref().map(|reason| reason.as_str()),
            );
        }
        for diagnostic in self.diagnostics() {
            if diagnostic.plugin.is_none() {
                self.notifier.alert(&diagnostic.message);
            }
        }
    }

    fn plugin_ids(&self) -> Vec<PluginId> {
        let manager = self.manager.lock().expect("manager lock poisoned");
        manager.manifests().keys().filter_map(|name| PluginId::new(name.clone()).ok()).collect()
    }
}

impl AgentEffectSink for AgentPluginHost {
    fn effect_tool_result(
        &self,
        owner: Option<&PluginId>,
        result: &ToolResultEffect,
    ) -> Result<(), PluginError> {
        self.tool_results
            .lock()
            .expect("tool result lock poisoned")
            .push((owner.cloned(), result.clone()));
        Ok(())
    }

    fn effect_context_update(
        &self,
        _owner: Option<&PluginId>,
        update: &ContextUpdateEffect,
    ) -> Result<(), PluginError> {
        self.context.apply_update(update)
    }

    fn effect_session_control(
        &self,
        _owner: Option<&PluginId>,
        control: &SessionControlEffect,
    ) -> Result<(), PluginError> {
        let session_id = control.session_id();
        let session = self.sessions.session(session_id).ok_or_else(|| PluginError::Runtime {
            message: format!("unknown agent session `{session_id}`"),
        })?;
        match control {
            SessionControlEffect::Cancel { .. } => session.cancel(),
            SessionControlEffect::Checkpoint { checkpoint_id, .. } => {
                session.checkpoint(checkpoint_id.clone())?;
            }
            SessionControlEffect::Close { reason, .. } => session.close(reason.clone()),
        }
        Ok(())
    }
}

impl std::fmt::Debug for AgentPluginHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentPluginHost")
            .field("started", &self.is_started())
            .field("sessions", &self.sessions.ids())
            .finish_non_exhaustive()
    }
}

fn register<T>(registry: &mut ServiceRegistry, service: Arc<T>) -> Result<(), OrchestratorError>
where
    T: ee_plugin_api::service::Service,
{
    registry.register(service).map_err(|error| OrchestratorError::InvalidState(error.to_string()))
}

/// Builds the plugin-id → permission map for diagnostics/tests.
#[must_use]
pub fn permission_map(host: &AgentPluginHost) -> BTreeMap<String, BTreeSet<String>> {
    let manager = host.manager.lock().expect("manager lock poisoned");
    manager
        .manifests()
        .iter()
        .map(|(name, manifest)| {
            (
                name.clone(),
                manifest
                    .permissions
                    .0
                    .iter()
                    .filter(|(_, enabled)| **enabled)
                    .map(|(permission, _)| permission.clone())
                    .collect(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(unused_must_use)]
    use super::*;
    use ee_plugin_api::service::Service;
    use serde_json::json;

    struct ScopedService;

    impl Service for ScopedService {
        const ID: &'static str = "test.host.scoped@1";
    }

    fn host() -> AgentPluginHost {
        let workspace =
            std::env::temp_dir().join(format!("ee-agent-host-{}-{}", std::process::id(), unique()));
        AgentPluginHost::new(AgentPluginHostConfig::new(workspace), Arc::new(NoopAgentNotifier))
            .expect("host")
    }

    fn unique() -> u128 {
        use std::sync::atomic::AtomicU64;
        use std::sync::atomic::Ordering;
        static NEXT: AtomicU64 = AtomicU64::new(1);
        u128::from(NEXT.fetch_add(1, Ordering::SeqCst))
    }

    #[test]
    fn start_and_stop_are_idempotent_with_no_roots() {
        let host = host();
        host.start().expect("starts");
        assert!(host.is_started());
        host.start().expect("second start is a no-op");
        host.stop();
        assert!(!host.is_started());
        assert!(host.is_shutdown());
        host.stop();
    }

    #[test]
    fn session_scope_disposes_contributions() {
        let host = host();
        host.start().expect("starts");
        let session = host.session_scope("session-1").expect("session scope");
        let context = session.context().expect("active");
        context.register_service(Arc::new(ScopedService)).expect("registers");
        assert!(
            host.services()
                .read()
                .expect("services")
                .contains(&ee_plugin_api::ids::ServiceId::from_static(ScopedService::ID))
        );
        session.close("done");
        assert!(
            !host
                .services()
                .read()
                .expect("services")
                .contains(&ee_plugin_api::ids::ServiceId::from_static(ScopedService::ID))
        );
    }

    #[test]
    fn turn_scope_disposes_on_finish() {
        let host = host();
        host.start().expect("starts");
        let session = host.session_scope("session-1").expect("session scope");
        let turn = host.turn_scope("session-1", 1).expect("turn scope");
        turn.context().register_service(Arc::new(ScopedService)).expect("registers");
        assert!(session.context().expect("active").service::<ScopedService>().is_some());
        turn.finish();
        assert!(session.context().expect("active").service::<ScopedService>().is_none());
        session.close("done");
    }

    #[test]
    fn session_control_effects_apply_in_order() {
        let host = host();
        host.start().expect("starts");
        let session = host.session_scope("session-1").expect("session scope");
        host.effects.push(
            None,
            crate::plugin_runtime::AgentEffect::SessionControl(
                crate::plugin_runtime::SessionControlEffect::Checkpoint {
                    session_id: "session-1".to_owned(),
                    checkpoint_id: "cp-1".to_owned(),
                },
            ),
        );
        host.effects.push(
            None,
            crate::plugin_runtime::AgentEffect::SessionControl(
                crate::plugin_runtime::SessionControlEffect::Cancel {
                    session_id: "session-1".to_owned(),
                },
            ),
        );
        let report = host.apply_pending_effects();
        assert!(report.is_ok());
        assert_eq!(session.checkpoints(), vec!["cp-1".to_owned()]);
        assert!(session.is_cancelled());
    }

    #[test]
    fn health_report_is_bounded_and_serializable() {
        let host = host();
        host.start().expect("starts");
        let report = host.health_report();
        let json = serde_json::to_value(&report).expect("serializes");
        assert_eq!(json["generation"], json!(report.generation));
    }

    #[test]
    fn plugin_status_conversion_preserves_health_state() {
        let status = PluginStatus {
            name: "demo".to_owned(),
            version: "1.0.0".to_owned(),
            kind: "builtin".to_owned(),
            origin: None,
            origin_root: None,
            origin_hash: None,
            active: true,
            health: ee_plugin_runtime::health::HealthTracker::default().snapshot(),
        };
        let converted = AgentPluginStatus::from(&status);
        assert_eq!(converted.name, "demo");
        assert_eq!(converted.health, "loaded");
    }
}
