//! `agent.policy@1`: mandatory-first tool and approval gates.
//!
//! Evaluation order is fixed:
//!
//! 1. mandatory host policy (workspace trust, class/subclass, scope, delegation,
//!    budget and write-transaction checks are performed by the caller/runtime),
//! 2. plugin gate rules in `(precedence, id)` order,
//! 3. registered `agent.tool.before` / `agent.approval.before` bail listeners.
//!
//! A plugin may deny an operation the host allowed. It can never allow one the
//! host denied: mandatory denial returns before plugin rules or listeners run.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, RwLock};

use ee_plugin_api::contribution::PolicyGate;
use ee_plugin_api::disposer::Disposer;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::event::EventBus;
use ee_plugin_api::ids::PluginId;
use ee_plugin_api::service::Service;
use serde::{Deserialize, Serialize};

use crate::plugin_runtime::events::{self, AgentApprovalBefore, AgentToolBefore};
use crate::policy::{PluginPolicyGate, PolicyContext, PolicyDecision, PolicyEngine};
use crate::tools::ToolDefinition;

/// Maximum plugin gate rules per host.
pub const MAX_POLICY_RULES: usize = 64;

/// A host approval decision request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub session_id: String,
    pub tool_name: String,
    pub tool_call_id: String,
    /// Whether the trusted editor host already approved the mutation.
    pub host_approved: bool,
}

/// Mandatory approval gate installed by the host.
pub trait MandatoryApprovalGate: Send + Sync + fmt::Debug {
    fn check(&self, request: &ApprovalRequest) -> PolicyDecision;
}

/// Default gate: host approval is mandatory; anything else is denied before
/// plugin listeners run.
#[derive(Debug, Default, Clone, Copy)]
pub struct ApprovalPolicy;

impl MandatoryApprovalGate for ApprovalPolicy {
    fn check(&self, request: &ApprovalRequest) -> PolicyDecision {
        if request.host_approved {
            PolicyDecision::allowed()
        } else {
            PolicyDecision::denied("host approval is required")
        }
    }
}

#[derive(Debug, Clone)]
struct PolicyRule {
    plugin: Option<PluginId>,
    precedence: i32,
    tool_pattern: Option<String>,
    decision: PolicyDecision,
}

/// Policy gate service.
pub struct PolicyService {
    mandatory: RwLock<PolicyEngine>,
    approval: RwLock<Option<Arc<dyn MandatoryApprovalGate>>>,
    rules: Arc<Mutex<BTreeMap<String, PolicyRule>>>,
    events: EventBus,
}

impl Service for PolicyService {
    const ID: &'static str = "agent.policy@1";
}

impl PolicyService {
    #[must_use]
    pub fn new(mandatory: PolicyEngine, events: EventBus) -> Self {
        Self {
            mandatory: RwLock::new(mandatory),
            approval: RwLock::new(None),
            rules: Arc::new(Mutex::new(BTreeMap::new())),
            events,
        }
    }

    /// Mandatory engine, never carrying a plugin gate itself.
    #[must_use]
    pub fn mandatory(&self) -> PolicyEngine {
        self.mandatory.read().expect("policy lock poisoned").clone()
    }

    /// Replaces the mandatory engine (mode changes) while keeping plugin rules.
    pub fn set_mandatory(&self, policy: PolicyEngine) {
        *self.mandatory.write().expect("policy lock poisoned") = policy;
    }

    /// Installs a mandatory approval gate.
    pub fn set_approval_gate(&self, gate: Arc<dyn MandatoryApprovalGate>) {
        *self.approval.write().expect("approval lock poisoned") = Some(gate);
    }

    /// Clears the mandatory approval gate; [`ApprovalPolicy`] remains implied.
    pub fn clear_approval_gate(&self) {
        *self.approval.write().expect("approval lock poisoned") = None;
    }

    /// Registers a gate rule. `tool_pattern` is an exact tool name or a
    /// `prefix*` matcher; `None` matches every tool.
    pub fn register_gate_rule(
        &self,
        plugin: &PluginId,
        id: impl Into<String>,
        precedence: i32,
        tool_pattern: Option<String>,
        decision: PolicyDecision,
    ) -> Result<Disposer, PluginError> {
        let id = id.into();
        let mut rules = self.rules.lock().expect("policy rule lock poisoned");
        if rules.len() >= MAX_POLICY_RULES {
            return Err(PluginError::Resource {
                message: format!("policy rule limit of {MAX_POLICY_RULES} reached"),
            });
        }
        if rules.contains_key(&id) {
            return Err(PluginError::Runtime { message: format!("duplicate policy rule `{id}`") });
        }
        rules.insert(
            id.clone(),
            PolicyRule { plugin: Some(plugin.clone()), precedence, tool_pattern, decision },
        );
        drop(rules);

        let handle = Arc::clone(&self.rules);
        let plugin = plugin.clone();
        Ok(Disposer::new(move || {
            if let Ok(mut rules) = handle.lock()
                && rules.get(&id).is_some_and(|rule| rule.plugin.as_ref() == Some(&plugin))
            {
                rules.remove(&id);
            }
        }))
    }

    /// Records a declarative manifest rule contribution for audit. Such a
    /// contribution carries no decision, so it never changes an outcome.
    pub fn note_contribution(
        &self,
        plugin: &PluginId,
        rule: &ee_plugin_api::contribution::PolicyRuleContribution,
    ) {
        let gate = match rule.gate {
            PolicyGate::Observe => "observe",
            PolicyGate::Gate => "gate",
        };
        tracing::debug!(
            plugin = %plugin,
            rule = %rule.id,
            gate,
            precedence = rule.precedence,
            "recorded agent policy contribution"
        );
    }

    /// Evaluates a tool after mandatory policy passed.
    pub fn check_tool(
        &self,
        tool: &ToolDefinition,
        context: PolicyContext,
        arguments: &serde_json::Value,
    ) -> PolicyDecision {
        let mandatory = self.mandatory();
        let decision = mandatory.check_with_arguments(tool, context, arguments);
        if !decision.allow {
            return decision;
        }
        self.check_tool_after_mandatory(tool, context, arguments)
    }

    /// Evaluates an approval request. Mandatory denial short-circuits plugin
    /// listeners.
    pub fn check_approval(&self, request: &ApprovalRequest) -> PolicyDecision {
        let gate = self.approval.read().expect("approval lock poisoned").clone();
        let mandatory = match gate {
            Some(gate) => gate.check(request),
            None => ApprovalPolicy.check(request),
        };
        if !mandatory.allow {
            return mandatory;
        }
        let event = AgentApprovalBefore {
            session_id: request.session_id.clone(),
            tool_name: request.tool_name.clone(),
            tool_call_id: request.tool_call_id.clone(),
            host_approved: request.host_approved,
        };
        match self.events.bail::<AgentApprovalBefore, PolicyDecision>(&event) {
            Ok(Some(decision)) => decision,
            Ok(None) => PolicyDecision::allowed(),
            Err(error) => PolicyDecision::denied(format!("approval listener failed: {error}")),
        }
    }

    /// Registers an observe-only `agent.tool.before` bail listener.
    pub fn on_tool_before(
        &self,
        listener: impl Fn(&AgentToolBefore) -> Option<PolicyDecision> + Send + Sync + 'static,
    ) -> Result<Disposer, PluginError> {
        Ok(self.events.on_bail::<AgentToolBefore, PolicyDecision>(listener)?)
    }

    /// Registers an observe-only `agent.approval.before` bail listener.
    pub fn on_approval_before(
        &self,
        listener: impl Fn(&AgentApprovalBefore) -> Option<PolicyDecision> + Send + Sync + 'static,
    ) -> Result<Disposer, PluginError> {
        Ok(self.events.on_bail::<AgentApprovalBefore, PolicyDecision>(listener)?)
    }

    /// Runs plugin rules then bail listeners. Callers must have already applied
    /// every mandatory check.
    pub fn check_tool_after_mandatory(
        &self,
        tool: &ToolDefinition,
        context: PolicyContext,
        arguments: &serde_json::Value,
    ) -> PolicyDecision {
        let mut rules: Vec<(i32, String, PolicyRule)> = self
            .rules
            .lock()
            .expect("policy rule lock poisoned")
            .iter()
            .map(|(id, rule)| (rule.precedence, id.clone(), rule.clone()))
            .collect();
        rules.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
        for (_, _, rule) in rules {
            if rule.tool_pattern.as_deref().is_none_or(|pattern| matches_tool(pattern, &tool.name))
                && !rule.decision.allow
            {
                return rule.decision;
            }
        }

        let event = events::tool_before(
            "",
            tool,
            context.subagent_depth,
            context.active_delegates,
            arguments,
        );
        match self.events.bail::<AgentToolBefore, PolicyDecision>(&event) {
            Ok(Some(decision)) => decision,
            Ok(None) => PolicyDecision::allowed(),
            Err(error) => PolicyDecision::denied(format!("tool listener failed: {error}")),
        }
    }
}

impl PluginPolicyGate for PolicyService {
    fn check_tool(
        &self,
        tool: &ToolDefinition,
        context: PolicyContext,
        arguments: &serde_json::Value,
    ) -> PolicyDecision {
        // The engine only calls this after mandatory checks passed.
        self.check_tool_after_mandatory(tool, context, arguments)
    }
}

impl fmt::Debug for PolicyService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PolicyService")
            .field("rules", &self.rules.lock().map(|rules| rules.len()).unwrap_or(0))
            .finish_non_exhaustive()
    }
}

fn matches_tool(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::ToolPolicy;
    use crate::tools::SideEffectClass;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn tool() -> ToolDefinition {
        ToolDefinition::new("read_file", "reads").side_effect_class(SideEffectClass::Read)
    }

    fn plugin() -> PluginId {
        PluginId::new("demo").expect("valid plugin id")
    }

    fn read_only() -> PolicyEngine {
        PolicyEngine::new(ToolPolicy::default())
    }

    #[test]
    fn plugin_listener_can_deny_an_allowed_tool() {
        let service = PolicyService::new(read_only(), EventBus::new());
        let _listener = service
            .on_tool_before(|_event| Some(PolicyDecision::denied("plugin says no")))
            .expect("listener");
        let decision =
            service.check_tool(&tool(), PolicyContext::default(), &serde_json::json!({}));
        assert!(!decision.allow);
        assert_eq!(decision.reason.as_deref(), Some("plugin says no"));
    }

    #[test]
    fn plugin_listener_can_allow() {
        let service = PolicyService::new(read_only(), EventBus::new());
        let _listener =
            service.on_tool_before(|_event| Some(PolicyDecision::allowed())).expect("listener");
        let decision =
            service.check_tool(&tool(), PolicyContext::default(), &serde_json::json!({}));
        assert!(decision.allow);
    }

    #[test]
    fn mandatory_denial_short_circuits_plugin_listeners() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = PolicyService::new(
            PolicyEngine::new(ToolPolicy { allow_read: false, ..ToolPolicy::default() }),
            EventBus::new(),
        );
        let counter = Arc::clone(&calls);
        let _listener = service
            .on_tool_before(move |_event| {
                counter.fetch_add(1, Ordering::SeqCst);
                Some(PolicyDecision::allowed())
            })
            .expect("listener");
        let decision =
            service.check_tool(&tool(), PolicyContext::default(), &serde_json::json!({}));
        assert!(!decision.allow);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn plugin_rule_denial_is_deterministic() {
        let service = PolicyService::new(read_only(), EventBus::new());
        let _rule = service
            .register_gate_rule(
                &plugin(),
                "demo.deny-reads",
                0,
                Some("read_*".to_owned()),
                PolicyDecision::denied("rule denied"),
            )
            .expect("rule");
        let decision =
            service.check_tool(&tool(), PolicyContext::default(), &serde_json::json!({}));
        assert!(!decision.allow);
    }

    #[test]
    fn approval_listener_cannot_bypass_mandatory_denial() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = PolicyService::new(read_only(), EventBus::new());
        service.set_approval_gate(Arc::new(ApprovalPolicy));
        let counter = Arc::clone(&calls);
        let _listener = service
            .on_approval_before(move |_event| {
                counter.fetch_add(1, Ordering::SeqCst);
                Some(PolicyDecision::allowed())
            })
            .expect("listener");
        let request = ApprovalRequest {
            session_id: "session-1".to_owned(),
            tool_name: "write_file".to_owned(),
            tool_call_id: "call-1".to_owned(),
            host_approved: false,
        };
        let decision = service.check_approval(&request);
        assert!(!decision.allow);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn approval_listener_can_deny_host_approved_request() {
        let service = PolicyService::new(read_only(), EventBus::new());
        service.set_approval_gate(Arc::new(ApprovalPolicy));
        let _listener = service
            .on_approval_before(|_event| Some(PolicyDecision::denied("plugin veto")))
            .expect("listener");
        let request = ApprovalRequest {
            session_id: "session-1".to_owned(),
            tool_name: "write_file".to_owned(),
            tool_call_id: "call-1".to_owned(),
            host_approved: true,
        };
        let decision = service.check_approval(&request);
        assert!(!decision.allow);
        assert_eq!(decision.reason.as_deref(), Some("plugin veto"));
    }

    #[test]
    fn host_approved_request_passes_without_plugin_denial() {
        let service = PolicyService::new(read_only(), EventBus::new());
        let request = ApprovalRequest {
            session_id: "session-1".to_owned(),
            tool_name: "write_file".to_owned(),
            tool_call_id: "call-1".to_owned(),
            host_approved: true,
        };
        assert!(service.check_approval(&request).allow);
    }
}
