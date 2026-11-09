//! Bridge between orchestrator boundaries and the `agent.*` event catalog.
//!
//! The bridge is a cheap cloneable handle. It never owns the host: it shares the
//! event bus, the runtime manager, the policy/context/strategy/session services,
//! and the effect queue.
//!
//! Event mapping comes from the existing [`OrchestratorEvent`] recorder. Attach
//! it with [`crate::runtime::OrchestratorRuntime::with_plugin_observer`] so
//! replay traces stay identical whether a plugin is registered or not.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ee_plugin_api::disposer::Disposer;
use ee_plugin_api::event::{Event, EventBus};
use ee_plugin_api::ids::PluginId;
use ee_plugin_runtime::manager::PluginManager;
use serde::Serialize;
use serde_json::Value;

use crate::events::OrchestratorEvent;
use crate::plugin_runtime::effects::{AgentEffect, AgentEffectQueue};
use crate::plugin_runtime::events::{
    self, AgentApprovalBefore, AgentContextCollect, AgentModelAfter, AgentModelBefore,
    AgentStrategySelect, AgentSubagentFinished, AgentSubagentStarted, AgentToolAfter,
    AgentToolBefore, AgentTurnAfter, AgentTurnBefore, AgentValidationCompleted,
};
use crate::plugin_runtime::services::context::ContextCollectRequest;
use crate::plugin_runtime::services::policy::ApprovalRequest;
use crate::plugin_runtime::services::{
    ContextCollection, ContextContribution, ContextService, PolicyService, SessionService,
    StrategyService,
};
use crate::policy::{PolicyContext, PolicyDecision};
use crate::strategy::{
    StrategyContext, StrategyDecision, StrategyReason, StrategySelector, TurnStrategy,
    required_capabilities_for,
};
use crate::tools::ToolDefinition;

/// One strategy proposal from a bail listener.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StrategyProposal {
    pub strategy: TurnStrategy,
    #[serde(default)]
    pub reason: Option<StrategyReason>,
}

/// Accumulator for `agent.context.collect` serial listeners.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextCollectAccumulator {
    pub contributions: Vec<ContextContribution>,
}

impl ContextCollectAccumulator {
    pub fn push(&mut self, contribution: ContextContribution) {
        self.contributions.push(contribution);
    }
}

/// Cloneable bridge handle.
#[derive(Clone)]
pub struct AgentBridge {
    events: EventBus,
    manager: Arc<Mutex<PluginManager>>,
    policy: Arc<PolicyService>,
    context: Arc<ContextService>,
    strategies: Arc<StrategyService>,
    sessions: Arc<SessionService>,
    effects: Arc<AgentEffectQueue>,
    revision: Arc<AtomicU64>,
}

impl AgentBridge {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        events: EventBus,
        manager: Arc<Mutex<PluginManager>>,
        policy: Arc<PolicyService>,
        context: Arc<ContextService>,
        strategies: Arc<StrategyService>,
        sessions: Arc<SessionService>,
        effects: Arc<AgentEffectQueue>,
    ) -> Self {
        Self {
            events,
            manager,
            policy,
            context,
            strategies,
            sessions,
            effects,
            revision: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Event bus shared with builtin plugins.
    #[must_use]
    pub fn events(&self) -> EventBus {
        self.events.clone()
    }

    /// Emits one typed event to in-process listeners and active native plugins.
    pub fn emit<E: Event + Serialize>(&self, event: &E) {
        let _ = self.events.emit(event);
        let revision = self.revision.fetch_add(1, Ordering::SeqCst);
        if let Ok(payload) = serde_json::to_value(event)
            && let Ok(mut manager) = self.manager.lock()
        {
            manager.emit_event(E::NAME, revision, payload);
        }
    }

    /// Emits a turn-before observation.
    pub fn begin_turn(&self, session_id: &str, task_id: &str, prompt_bytes: usize) {
        self.emit(&AgentTurnBefore {
            session_id: session_id.to_owned(),
            task_id: task_id.to_owned(),
            prompt_bytes,
        });
    }

    /// Emits a turn-after observation.
    pub fn end_turn(&self, session_id: &str, task_id: &str, stop_reason: &str) {
        self.emit(&AgentTurnAfter {
            session_id: session_id.to_owned(),
            task_id: task_id.to_owned(),
            stop_reason: stop_reason.to_owned(),
        });
    }

    /// Emits a validation-completed observation.
    pub fn validation_completed(&self, session_id: &str, command: &str, status: &str) {
        self.emit(&AgentValidationCompleted {
            session_id: session_id.to_owned(),
            command: command.to_owned(),
            status: status.to_owned(),
        });
    }

    /// Applies mandatory-first tool policy, then plugin rules and listeners.
    #[must_use]
    pub fn check_tool(
        &self,
        session_id: &str,
        definition: &ToolDefinition,
        context: PolicyContext,
        arguments: &Value,
    ) -> PolicyDecision {
        let mandatory = self.policy.mandatory();
        let decision = mandatory.check_with_arguments(definition, context, arguments);
        if !decision.allow {
            return decision;
        }
        self.emit(&events::tool_before(
            session_id,
            definition,
            context.subagent_depth,
            context.active_delegates,
            arguments,
        ));
        self.policy.check_tool_after_mandatory(definition, context, arguments)
    }

    /// Applies mandatory approval policy, then plugin listeners.
    #[must_use]
    pub fn check_approval(&self, request: &ApprovalRequest) -> PolicyDecision {
        self.policy.check_approval(request)
    }

    /// Collects bounded context from registered providers plus serial listeners.
    #[must_use]
    pub fn collect_context(
        &self,
        session_id: &str,
        turn: u64,
        max_items: usize,
        max_bytes: usize,
    ) -> ContextCollection {
        let event =
            AgentContextCollect { session_id: session_id.to_owned(), turn, max_items, max_bytes };
        let accumulated = self
            .events
            .serial::<AgentContextCollect, ContextCollectAccumulator>(
                &event,
                ContextCollectAccumulator::default(),
            )
            .unwrap_or_default();
        let mut collection = self.context.collect_with(ContextCollectRequest {
            session_id: session_id.to_owned(),
            turn,
            max_items,
            max_bytes,
        });
        merge_context(&mut collection, accumulated.contributions, max_items, max_bytes);
        collection
    }

    /// Registers a serial context listener; the listener appends bounded items.
    pub fn on_context_collect(
        &self,
        listener: impl Fn(&AgentContextCollect, &mut ContextCollectAccumulator) + Send + Sync + 'static,
    ) -> Result<Disposer, ee_plugin_api::error::PluginError> {
        Ok(self.events.on_serial::<AgentContextCollect, ContextCollectAccumulator>(listener)?)
    }

    /// Selects a strategy with host authority.
    ///
    /// Order: deterministic cascade, declarative candidates, then a single bail
    /// proposal. A proposal is accepted only when every required capability is
    /// available; otherwise the host decision stands.
    #[must_use]
    pub fn select_strategy(
        &self,
        session_id: &str,
        context: &StrategyContext,
        available_capabilities: &[String],
    ) -> StrategyDecision {
        let base = StrategySelector.select(context);
        let selected = self.strategies.select(context, available_capabilities);
        let event = AgentStrategySelect {
            session_id: session_id.to_owned(),
            base_strategy: base.strategy,
            base_reason: base.reason,
            available_capabilities: available_capabilities.to_vec(),
        };
        let proposal =
            self.events.bail::<AgentStrategySelect, StrategyProposal>(&event).ok().flatten();
        let Some(proposal) = proposal else {
            return selected;
        };
        let required = required_capabilities_for(proposal.strategy);
        if !required.iter().all(|capability| {
            available_capabilities.iter().any(|available| available == capability)
        }) {
            return selected;
        }
        StrategyDecision {
            strategy: proposal.strategy,
            reason: proposal.reason.unwrap_or(selected.reason),
            required_capabilities: required.into_iter().map(str::to_string).collect(),
        }
    }

    /// Registers a `agent.strategy.select` bail listener.
    pub fn on_strategy_select(
        &self,
        listener: impl Fn(&AgentStrategySelect) -> Option<StrategyProposal> + Send + Sync + 'static,
    ) -> Result<Disposer, ee_plugin_api::error::PluginError> {
        Ok(self.events.on_bail::<AgentStrategySelect, StrategyProposal>(listener)?)
    }

    /// Queues a structured plugin effect.
    pub fn submit(&self, owner: Option<PluginId>, effect: AgentEffect) {
        self.effects.push(owner, effect);
    }

    /// Session service handle.
    #[must_use]
    pub fn sessions(&self) -> &Arc<SessionService> {
        &self.sessions
    }

    /// Maps one orchestrator event for `session_id`.
    ///
    /// This is observe-only: it never changes loop decisions, so replay traces
    /// remain deterministic with and without plugin registration.
    pub fn observe_orchestrator_event(&self, session_id: &str, event: &OrchestratorEvent) {
        match event {
            OrchestratorEvent::TurnStarted { task_id, .. } => {
                self.emit(&AgentTurnBefore {
                    session_id: session_id.to_owned(),
                    task_id: task_id.clone(),
                    prompt_bytes: 0,
                });
            }
            OrchestratorEvent::TurnStopped { stop_reason } => {
                self.emit(&AgentTurnAfter {
                    session_id: session_id.to_owned(),
                    task_id: String::new(),
                    stop_reason: stop_reason.clone(),
                });
            }
            OrchestratorEvent::ModelRequested { iteration } => {
                self.emit(&AgentModelBefore {
                    session_id: session_id.to_owned(),
                    model_id: None,
                    task_kind: None,
                    role: None,
                    iteration: *iteration,
                });
            }
            OrchestratorEvent::ModelResponded { iteration } => {
                self.emit(&AgentModelAfter {
                    session_id: session_id.to_owned(),
                    model_id: None,
                    task_kind: None,
                    iteration: *iteration,
                });
            }
            OrchestratorEvent::ToolFinished { tool_call_id, tool_name, success } => {
                self.emit(&AgentToolAfter {
                    session_id: session_id.to_owned(),
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    success: *success,
                });
            }
            OrchestratorEvent::ApprovalRequested { tool_call_id, tool_name } => {
                self.emit(&AgentApprovalBefore {
                    session_id: session_id.to_owned(),
                    tool_name: tool_name.clone(),
                    tool_call_id: tool_call_id.clone(),
                    host_approved: false,
                });
            }
            OrchestratorEvent::SubagentStarted { subagent_id, role, model_id } => {
                self.emit(&AgentSubagentStarted {
                    session_id: session_id.to_owned(),
                    subagent_id: subagent_id.clone(),
                    role: role.clone(),
                    model_id: model_id.clone(),
                });
            }
            OrchestratorEvent::SubagentFinished { subagent_id, success } => {
                self.emit(&AgentSubagentFinished {
                    session_id: session_id.to_owned(),
                    subagent_id: subagent_id.clone(),
                    success: *success,
                });
            }
            OrchestratorEvent::ModelRouted { adapter_id, task_kind, role, .. } => {
                self.emit(&AgentModelBefore {
                    session_id: session_id.to_owned(),
                    model_id: Some(adapter_id.clone()),
                    task_kind: Some(*task_kind),
                    role: role.clone(),
                    iteration: 0,
                });
            }
            OrchestratorEvent::StrategySelected { strategy, reason } => {
                self.emit(&AgentStrategySelect {
                    session_id: session_id.to_owned(),
                    base_strategy: *strategy,
                    base_reason: *reason,
                    available_capabilities: Vec::new(),
                });
            }
            _ => {}
        }
    }

    /// Observer closure for [`crate::runtime::OrchestratorRuntime`].
    pub fn observer(&self, session_id: impl Into<String>) -> crate::events::EventObserver {
        let bridge = self.clone();
        let session_id = session_id.into();
        Arc::new(move |event| bridge.observe_orchestrator_event(&session_id, event))
    }

    /// Tool-before event builder re-exported for callers.
    #[must_use]
    pub fn tool_before(
        &self,
        session_id: &str,
        definition: &ToolDefinition,
        context: PolicyContext,
        arguments: &Value,
    ) -> AgentToolBefore {
        events::tool_before(
            session_id,
            definition,
            context.subagent_depth,
            context.active_delegates,
            arguments,
        )
    }
}

fn merge_context(
    collection: &mut ContextCollection,
    mut contributions: Vec<ContextContribution>,
    max_items: usize,
    max_bytes: usize,
) {
    contributions.sort_by(|left, right| {
        left.source
            .cmp(&right.source)
            .then(left.id.cmp(&right.id))
            .then(left.revision.cmp(&right.revision))
    });
    for contribution in contributions {
        if collection.items.len() >= max_items {
            collection.omitted += 1;
            continue;
        }
        if collection
            .items
            .iter()
            .any(|item| item.source == contribution.source && item.id == contribution.id)
        {
            collection.omitted += 1;
            continue;
        }
        let excerpt = crate::plugin_runtime::services::context::bound_excerpt(
            &contribution.excerpt,
            max_bytes.saturating_sub(collection.bytes),
        );
        if excerpt.is_empty() {
            collection.omitted += 1;
            continue;
        }
        collection.bytes += excerpt.len();
        collection.items.push(ContextContribution { excerpt, ..contribution });
    }
}

impl std::fmt::Debug for AgentBridge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("AgentBridge").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    #![allow(unused_must_use)]
    use super::*;
    use crate::plugin_runtime::host::AgentPluginHost;
    use crate::plugin_runtime::host::AgentPluginHostConfig;

    fn bridge() -> AgentBridge {
        let host = AgentPluginHost::new(
            AgentPluginHostConfig::new("memory"),
            Arc::new(crate::plugin_runtime::host::NoopAgentNotifier),
        )
        .expect("host");
        host.bridge()
    }

    fn tool() -> ToolDefinition {
        ToolDefinition::new("read_file", "reads")
    }

    #[test]
    fn observer_maps_core_events_without_changing_them() {
        let bridge = bridge();
        let seen = Arc::new(Mutex::new(Vec::new()));
        bridge
            .events()
            .on::<AgentTurnBefore>({
                let captured = Arc::clone(&seen);
                move |event| captured.lock().expect("capture lock").push(event.task_id.clone())
            })
            .expect("listener");
        bridge.observe_orchestrator_event(
            "session-1",
            &OrchestratorEvent::TurnStarted {
                session_id: "session-1".to_owned(),
                task_id: "task-1".to_owned(),
            },
        );
        assert_eq!(*seen.lock().expect("capture lock"), vec!["task-1".to_owned()]);
    }

    #[test]
    fn strategy_proposal_cannot_grant_missing_capabilities() {
        let bridge = bridge();
        let _listener = bridge
            .on_strategy_select(|_event| {
                Some(StrategyProposal {
                    strategy: TurnStrategy::ParallelDelegation,
                    reason: Some(StrategyReason::ParallelIndependentWork),
                })
            })
            .expect("listener");
        let context = StrategyContext {
            prompt_text: "do it".to_owned(),
            has_code_changes: false,
            validation_tools_available: false,
            delegation_allowed: false,
            task_graph: crate::tasks::TaskGraph::new(),
            tool_definitions: Vec::new(),
        };
        let decision = bridge.select_strategy("session-1", &context, &["fs:read".to_owned()]);
        assert_ne!(decision.strategy, TurnStrategy::ParallelDelegation);
    }

    #[test]
    fn mandatory_policy_is_checked_before_plugin_gate() {
        let bridge = bridge();
        let decision = bridge.check_tool(
            "session-1",
            &tool(),
            PolicyContext::default(),
            &serde_json::json!({}),
        );
        assert!(decision.allow);
    }

    #[test]
    fn context_listener_contributions_are_capped() {
        let bridge = bridge();
        let _listener = bridge
            .on_context_collect(|_event, accumulator| {
                accumulator.push(ContextContribution {
                    id: "demo".to_owned(),
                    source: "plugin".to_owned(),
                    trust: crate::trust::TrustLevel::ToolOutputUntrusted,
                    revision: "rev".to_owned(),
                    excerpt: "abcdefghij".to_owned(),
                });
            })
            .expect("listener");
        let collection = bridge.collect_context("session-1", 1, 1, 4);
        assert_eq!(collection.items.len(), 1);
        assert_eq!(collection.bytes, 4);
        assert_eq!(collection.omitted, 0);
    }
}
