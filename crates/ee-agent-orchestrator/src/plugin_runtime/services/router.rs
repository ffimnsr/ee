//! JSON dispatch for `plugin/request` calls from native agent plugins.
//!
//! Permissions are checked before deserialization so a denied plugin cannot
//! probe argument shapes. Reads that expose no owner-scoped content need no
//! permission; registration and control methods require the manifest permission
//! from the v2 design document.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use ee_plugin_api::contribution::ContextProviderContribution;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::ids::{PluginId, ServiceId};
use serde_json::{Value, json};

use crate::plugin_runtime::services::{
    ContextService, ModelService, PERM_CONTEXT_CONTRIBUTE, PERM_MODELS_REGISTER, PERM_POLICY_GATE,
    PERM_POLICY_OBSERVE, PERM_SESSIONS_CONTROL, PERM_SESSIONS_READ, PERM_STRATEGIES_REGISTER,
    PERM_TOOLS_REGISTER, PERM_VALIDATION_CONTRIBUTE, PolicyService, SessionService,
    StrategyCandidate, StrategyService, ToolService, ValidationContribution, ValidationService,
    ids,
};
use crate::policy::{PolicyContext, PolicyDecision};
use crate::strategy::StrategyContext;
use crate::tools::ToolDefinition;

type PermissionMap = Arc<RwLock<BTreeMap<String, BTreeSet<String>>>>;

/// Routes external service calls to the `agent.*` services.
#[derive(Clone)]
pub struct AgentServiceRouter {
    pub tools: Arc<ToolService>,
    pub models: Arc<ModelService>,
    pub strategies: Arc<StrategyService>,
    pub policy: Arc<PolicyService>,
    pub context: Arc<ContextService>,
    pub sessions: Arc<SessionService>,
    pub validation: Arc<ValidationService>,
    permissions: PermissionMap,
    disposers: Arc<std::sync::Mutex<BTreeMap<String, Vec<ee_plugin_api::disposer::Disposer>>>>,
}

impl AgentServiceRouter {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tools: Arc<ToolService>,
        models: Arc<ModelService>,
        strategies: Arc<StrategyService>,
        policy: Arc<PolicyService>,
        context: Arc<ContextService>,
        sessions: Arc<SessionService>,
        validation: Arc<ValidationService>,
    ) -> Self {
        Self {
            tools,
            models,
            strategies,
            policy,
            context,
            sessions,
            validation,
            permissions: Arc::new(RwLock::new(BTreeMap::new())),
            disposers: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
        }
    }

    /// Disposes every registration a plugin created through the wire path.
    pub fn dispose_plugin(&self, plugin: &PluginId) {
        let disposers =
            self.disposers.lock().expect("router disposer lock poisoned").remove(plugin.as_str());
        if let Some(disposers) = disposers {
            for disposer in disposers.into_iter().rev() {
                disposer.dispose();
            }
        }
    }

    fn retain(&self, plugin: &PluginId, disposer: ee_plugin_api::disposer::Disposer) {
        self.disposers
            .lock()
            .expect("router disposer lock poisoned")
            .entry(plugin.as_str().to_owned())
            .or_default()
            .push(disposer);
    }

    pub fn set_permissions(&self, plugin: &PluginId, permissions: BTreeSet<String>) {
        self.permissions
            .write()
            .expect("permission lock poisoned")
            .insert(plugin.as_str().to_owned(), permissions);
    }

    pub fn clear_permissions(&self) {
        self.permissions.write().expect("permission lock poisoned").clear();
    }

    pub fn permissions_for(&self, plugin: &PluginId) -> Option<BTreeSet<String>> {
        self.permissions.read().expect("permission lock poisoned").get(plugin.as_str()).cloned()
    }

    /// Dispatches one service method.
    pub fn dispatch(
        &self,
        plugin: &PluginId,
        service: &ServiceId,
        method: &str,
        params: Value,
    ) -> Result<Value, PluginError> {
        let name = service.as_str();
        match (name, method) {
            (ids::TOOLS, "list") => Ok(json!(self.tools.definitions())),
            (ids::TOOLS, "names") => Ok(json!(self.tools.names())),
            (ids::TOOLS, "scope") => {
                let name = string_param(&params, "name")?;
                Ok(json!(self.tools.scope_of(&name)))
            }
            (ids::TOOLS, "remove") => {
                self.require(plugin, PERM_TOOLS_REGISTER)?;
                let name = string_param(&params, "name")?;
                Ok(json!({"removed": self.tools.remove_owned(plugin, &name)}))
            }
            (ids::MODELS, "list") => Ok(json!(self.models.ids())),
            (ids::MODELS, "info") => {
                let id = string_param(&params, "id")?;
                Ok(json!(self.models.info(&id)))
            }
            (ids::MODELS, "resolve") => {
                let kind: crate::model_router::TaskKind = serde_json::from_value(
                    params.get("task_kind").cloned().ok_or_else(|| schema_error("task_kind"))?,
                )
                .map_err(|error| schema_error_message(&error.to_string()))?;
                let role = params.get("role").and_then(Value::as_str);
                let route = self
                    .models
                    .resolve(kind, role)
                    .map_err(|error| PluginError::Runtime { message: error.to_string() })?;
                Ok(json!(route))
            }
            (ids::MODELS, "register_route") => {
                self.require(plugin, PERM_MODELS_REGISTER)?;
                let route: crate::model_router::ModelRoute = serde_json::from_value(params)
                    .map_err(|error| schema_error_message(&error.to_string()))?;
                let disposer = self.models.register_route(plugin, route)?;
                self.retain(plugin, disposer);
                Ok(json!({"registered": true}))
            }
            (ids::STRATEGIES, "list") => Ok(json!(self.strategies.ids())),
            (ids::STRATEGIES, "register") => {
                self.require(plugin, PERM_STRATEGIES_REGISTER)?;
                let candidate: StrategyCandidate = serde_json::from_value(params)
                    .map_err(|error| schema_error_message(&error.to_string()))?;
                let id = candidate.id.clone();
                let disposer = self.strategies.register_candidate(plugin, candidate)?;
                self.retain(plugin, disposer);
                Ok(json!({"registered": id}))
            }
            (ids::POLICY, "check_tool") => {
                self.require(plugin, PERM_POLICY_OBSERVE)?;
                let tool: ToolDefinition = serde_json::from_value(
                    params.get("tool").cloned().ok_or_else(|| schema_error("tool"))?,
                )
                .map_err(|error| schema_error_message(&error.to_string()))?;
                let context: PolicyContext = params
                    .get("context")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|error| schema_error_message(&error.to_string()))?
                    .unwrap_or_default();
                let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
                let decision = self.policy.check_tool(&tool, context, &arguments);
                Ok(json!(decision))
            }
            (ids::POLICY, "check_approval") => {
                self.require(plugin, PERM_POLICY_OBSERVE)?;
                let request: crate::plugin_runtime::services::policy::ApprovalRequest =
                    serde_json::from_value(params)
                        .map_err(|error| schema_error_message(&error.to_string()))?;
                Ok(json!(self.policy.check_approval(&request)))
            }
            (ids::POLICY, "register_rule") => {
                self.require(plugin, PERM_POLICY_GATE)?;
                let id = string_param(&params, "id")?;
                let precedence =
                    params.get("precedence").and_then(Value::as_i64).unwrap_or(0) as i32;
                let pattern = params.get("tool_pattern").and_then(Value::as_str).map(str::to_owned);
                let allow = params.get("allow").and_then(Value::as_bool).unwrap_or(false);
                let decision = if allow {
                    PolicyDecision::allowed()
                } else {
                    PolicyDecision::denied(
                        params
                            .get("reason")
                            .and_then(Value::as_str)
                            .unwrap_or("plugin policy rule denied the tool"),
                    )
                };
                let disposer =
                    self.policy.register_gate_rule(plugin, id, precedence, pattern, decision)?;
                self.retain(plugin, disposer);
                Ok(json!({"registered": true}))
            }
            (ids::CONTEXT, "contribute") => {
                self.require(plugin, PERM_CONTEXT_CONTRIBUTE)?;
                let contribution: ContextProviderContribution = serde_json::from_value(params)
                    .map_err(|error| schema_error_message(&error.to_string()))?;
                let id = contribution.id.clone();
                let disposer = self.context.register_contribution(plugin, &contribution)?;
                self.retain(plugin, disposer);
                Ok(json!({"registered": id}))
            }
            (ids::CONTEXT, "collect") => {
                self.require(plugin, PERM_CONTEXT_CONTRIBUTE)?;
                let session_id = string_param(&params, "session_id")?;
                let turn = params.get("turn").and_then(Value::as_u64).unwrap_or(0);
                Ok(json!(self.context.collect(&session_id, turn)))
            }
            (ids::SESSIONS, "ids") => {
                self.require(plugin, PERM_SESSIONS_READ)?;
                Ok(json!(self.sessions.ids()))
            }
            (ids::SESSIONS, "state") => {
                self.require(plugin, PERM_SESSIONS_READ)?;
                let session_id = string_param(&params, "session_id")?;
                let state =
                    self.sessions.session(&session_id).map(|session| session.state()).ok_or_else(
                        || PluginError::Runtime {
                            message: format!("unknown agent session `{session_id}`"),
                        },
                    )?;
                Ok(json!(state))
            }
            (ids::SESSIONS, "cancel") => {
                self.require(plugin, PERM_SESSIONS_CONTROL)?;
                let session_id = string_param(&params, "session_id")?;
                let session =
                    self.sessions.session(&session_id).ok_or_else(|| PluginError::Runtime {
                        message: format!("unknown agent session `{session_id}`"),
                    })?;
                session.cancel();
                Ok(json!({"cancelled": true}))
            }
            (ids::SESSIONS, "checkpoint") => {
                self.require(plugin, PERM_SESSIONS_CONTROL)?;
                let session_id = string_param(&params, "session_id")?;
                let checkpoint_id = string_param(&params, "checkpoint_id")?;
                let session =
                    self.sessions.session(&session_id).ok_or_else(|| PluginError::Runtime {
                        message: format!("unknown agent session `{session_id}`"),
                    })?;
                session.checkpoint(checkpoint_id)?;
                Ok(json!({"saved": true}))
            }
            (ids::SESSIONS, "close") => {
                self.require(plugin, PERM_SESSIONS_CONTROL)?;
                let session_id = string_param(&params, "session_id")?;
                let reason = params.get("reason").and_then(Value::as_str).unwrap_or("plugin");
                let session =
                    self.sessions.session(&session_id).ok_or_else(|| PluginError::Runtime {
                        message: format!("unknown agent session `{session_id}`"),
                    })?;
                session.close(reason);
                Ok(json!({"closed": true}))
            }
            (ids::VALIDATION, "list") => Ok(json!(self.validation.ids())),
            (ids::VALIDATION, "register") => {
                self.require(plugin, PERM_VALIDATION_CONTRIBUTE)?;
                let contribution: ValidationContribution = serde_json::from_value(params)
                    .map_err(|error| schema_error_message(&error.to_string()))?;
                let id = contribution.id.clone();
                let disposer = self.validation.register_contribution(plugin, contribution)?;
                self.retain(plugin, disposer);
                Ok(json!({"registered": id}))
            }
            (ids::VALIDATION, "plan") => {
                let tools: Vec<ToolDefinition> = params
                    .get("tools")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|error| schema_error_message(&error.to_string()))?
                    .unwrap_or_default();
                let changed_files: Vec<crate::final_response::ChangedFile> = params
                    .get("changed_files")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|error| schema_error_message(&error.to_string()))?
                    .unwrap_or_default();
                let workspace: crate::validation::WorkspaceValidationConfig = params
                    .get("workspace")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|error| schema_error_message(&error.to_string()))?
                    .unwrap_or_default();
                let plan = self.validation.plan(
                    crate::validation::ValidationPlanningContext {
                        changed_files: &changed_files,
                        changed_symbols: &[],
                        workspace: &workspace,
                    },
                    &tools,
                );
                Ok(json!(plan))
            }
            (ids::VALIDATION, "workspace_config") => {
                let workspace: crate::validation::WorkspaceValidationConfig = params
                    .get("workspace")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|error| schema_error_message(&error.to_string()))?
                    .unwrap_or_default();
                Ok(json!(self.validation.workspace_config(&workspace)))
            }
            _ => Err(PluginError::Unsupported { feature: "unknown agent service method" }),
        }
    }

    /// Resolves a strategy through the host service for native callers.
    pub fn select_strategy(
        &self,
        context: &StrategyContext,
        capabilities: &[String],
    ) -> crate::strategy::StrategyDecision {
        self.strategies.select(context, capabilities)
    }

    fn require(&self, plugin: &PluginId, permission: &str) -> Result<(), PluginError> {
        let permissions = self.permissions.read().expect("permission lock poisoned");
        let allowed =
            permissions.get(plugin.as_str()).is_some_and(|granted| granted.contains(permission));
        if allowed {
            Ok(())
        } else {
            Err(PluginError::Permission { permission: permission.to_owned() })
        }
    }
}

impl std::fmt::Debug for AgentServiceRouter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("AgentServiceRouter").finish_non_exhaustive()
    }
}

fn string_param(params: &Value, field: &str) -> Result<String, PluginError> {
    params.get(field).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| schema_error(field))
}

fn schema_error(field: &str) -> PluginError {
    PluginError::Schema { message: format!("missing string parameter `{field}`") }
}

fn schema_error_message(message: &str) -> PluginError {
    PluginError::Schema { message: message.to_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ee_plugin_api::event::EventBus;

    fn router() -> AgentServiceRouter {
        let events = EventBus::new();
        AgentServiceRouter::new(
            Arc::new(ToolService::new()),
            Arc::new(ModelService::new()),
            Arc::new(StrategyService::new()),
            Arc::new(PolicyService::new(crate::policy::PolicyEngine::default(), events.clone())),
            Arc::new(ContextService::new()),
            Arc::new(SessionService::new(events)),
            Arc::new(ValidationService::new()),
        )
    }

    fn plugin() -> PluginId {
        PluginId::new("demo").expect("valid plugin id")
    }

    fn service(id: &str) -> ServiceId {
        id.parse().expect("valid service id")
    }

    #[test]
    fn read_methods_need_no_permission() {
        let router = router();
        let ids = router
            .dispatch(&plugin(), &service(ids::SESSIONS), "ids", json!({}))
            .expect_err("denied");
        assert!(matches!(ids, PluginError::Permission { .. }));

        let strategies = router
            .dispatch(&plugin(), &service(ids::STRATEGIES), "list", json!({}))
            .expect("read allowed");
        assert_eq!(strategies, json!([]));
    }

    #[test]
    fn registration_requires_manifest_permission() {
        let router = router();
        let denied = router
            .dispatch(
                &plugin(),
                &service(ids::STRATEGIES),
                "register",
                json!({"id": "demo.plan", "strategy": "ToolLoop"}),
            )
            .expect_err("denied");
        assert!(matches!(denied, PluginError::Permission { .. }));

        let mut granted = BTreeSet::new();
        granted.insert(PERM_STRATEGIES_REGISTER.to_owned());
        router.set_permissions(&plugin(), granted);
        router
            .dispatch(
                &plugin(),
                &service(ids::STRATEGIES),
                "register",
                json!({"id": "demo.plan", "strategy": "ToolLoop"}),
            )
            .expect("allowed");
    }

    #[test]
    fn unknown_service_method_fails_closed() {
        let router = router();
        let error = router
            .dispatch(&plugin(), &service(ids::TOOLS), "execute", json!({}))
            .expect_err("unsupported");
        assert!(matches!(error, PluginError::Unsupported { .. }));
    }

    #[test]
    fn session_methods_require_read_and_control_permissions() {
        let router = router();
        let root = ee_plugin_api::context::PluginContext::new("agent-host");
        let session = router.sessions.create(&root, "session-1").expect("creates");
        assert!(session.context().is_some());

        let mut granted = BTreeSet::new();
        granted.insert(PERM_SESSIONS_CONTROL.to_owned());
        router.set_permissions(&plugin(), granted);
        router
            .dispatch(
                &plugin(),
                &service(ids::SESSIONS),
                "cancel",
                json!({"session_id": "session-1"}),
            )
            .expect("allowed");
        assert!(router.sessions.session("session-1").expect("live").is_cancelled());
    }

    #[test]
    fn tool_removal_is_owner_scoped() {
        let router = router();
        let mut granted = BTreeSet::new();
        granted.insert(PERM_TOOLS_REGISTER.to_owned());
        router.set_permissions(&plugin(), granted);
        let response = router
            .dispatch(&plugin(), &service(ids::TOOLS), "remove", json!({"name": "missing"}))
            .expect("allowed");
        assert_eq!(response, json!({"removed": false}));
    }
}
