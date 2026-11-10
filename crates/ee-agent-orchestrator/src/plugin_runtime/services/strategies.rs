//! `agent.strategies@1`: strategy candidates with host-owned selection.
//!
//! The cascade in [`StrategySelector`] remains authoritative. Plugins may
//! register a candidate that replaces the strategy for a reason the host already
//! selected, but only when the candidate's required capabilities are available
//! and the priority/id ordering is deterministic. A plugin can never select a
//! strategy the host would not have considered.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use ee_plugin_api::disposer::Disposer;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::ids::PluginId;
use ee_plugin_api::service::Service;
use serde::{Deserialize, Serialize};

use crate::strategy::{
    StrategyContext, StrategyDecision, StrategyReason, StrategySelector, TurnStrategy,
    required_capabilities_for,
};

/// Maximum registered strategy candidates.
pub const MAX_STRATEGY_CANDIDATES: usize = 32;

/// One strategy candidate proposed by a plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrategyCandidate {
    pub id: String,
    pub strategy: TurnStrategy,
    /// Reasons this candidate applies to; empty means every reason.
    #[serde(default)]
    pub applies_to: Vec<StrategyReason>,
    /// Higher priority wins; ties break by id.
    #[serde(default)]
    pub priority: i32,
}

struct CandidateEntry {
    plugin: PluginId,
    candidate: StrategyCandidate,
}

/// Strategy registry service.
pub struct StrategyService {
    candidates: Arc<RwLock<BTreeMap<String, CandidateEntry>>>,
    selector: StrategySelector,
}

impl Service for StrategyService {
    const ID: &'static str = "agent.strategies@1";
}

impl Default for StrategyService {
    fn default() -> Self {
        Self::new()
    }
}

impl StrategyService {
    #[must_use]
    pub fn new() -> Self {
        Self { candidates: Arc::new(RwLock::new(BTreeMap::new())), selector: StrategySelector }
    }

    /// Registers a candidate; ids are unique per host.
    pub fn register_candidate(
        &self,
        plugin: &PluginId,
        candidate: StrategyCandidate,
    ) -> Result<Disposer, PluginError> {
        let id = candidate.id.clone();
        if id.is_empty() {
            return Err(PluginError::Runtime {
                message: "strategy candidate id must not be empty".to_owned(),
            });
        }
        let mut candidates = self.candidates.write().expect("strategy candidate lock poisoned");
        if candidates.len() >= MAX_STRATEGY_CANDIDATES {
            return Err(PluginError::Resource {
                message: format!("strategy candidate limit of {MAX_STRATEGY_CANDIDATES} reached"),
            });
        }
        if candidates.contains_key(&id) {
            return Err(PluginError::Runtime {
                message: format!("duplicate strategy candidate `{id}`"),
            });
        }
        candidates.insert(id.clone(), CandidateEntry { plugin: plugin.clone(), candidate });
        drop(candidates);

        let handle = Arc::clone(&self.candidates);
        let plugin = plugin.clone();
        Ok(Disposer::new(move || {
            if let Ok(mut candidates) = handle.write()
                && candidates.get(&id).is_some_and(|entry| entry.plugin == plugin)
            {
                candidates.remove(&id);
            }
        }))
    }

    /// Selects a strategy with host authority.
    ///
    /// The base selector runs first; only then may a matching candidate replace
    /// the strategy, and only when every required capability is available.
    #[must_use]
    pub fn select(
        &self,
        context: &StrategyContext,
        available_capabilities: &[String],
    ) -> StrategyDecision {
        let base = self.selector.select(context);
        let mut candidates: Vec<StrategyCandidate> = self
            .candidates
            .read()
            .expect("strategy candidate lock poisoned")
            .values()
            .map(|entry| entry.candidate.clone())
            .collect();
        candidates
            .sort_by(|left, right| right.priority.cmp(&left.priority).then(left.id.cmp(&right.id)));
        for candidate in candidates {
            if !candidate.applies_to.is_empty() && !candidate.applies_to.contains(&base.reason) {
                continue;
            }
            let required = required_capabilities_for(candidate.strategy);
            if !required.iter().all(|capability| {
                available_capabilities.iter().any(|available| available == capability)
            }) {
                continue;
            }
            return StrategyDecision {
                strategy: candidate.strategy,
                reason: base.reason,
                required_capabilities: required.into_iter().map(str::to_string).collect(),
            };
        }
        base
    }

    /// Sorted candidate ids.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.candidates.read().expect("strategy candidate lock poisoned").keys().cloned().collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.candidates.read().expect("strategy candidate lock poisoned").len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for StrategyService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StrategyService")
            .field("candidates", &self.ids())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    #![allow(unused_must_use)]
    use super::*;
    use crate::tasks::TaskGraph;

    fn plugin() -> PluginId {
        PluginId::new("demo").expect("valid plugin id")
    }

    fn context() -> StrategyContext {
        // Inspection prompt so the host cascade selects `ToolLoop`; the
        // candidate tests assert replacement of that host-owned baseline.
        StrategyContext {
            prompt_text: "inspect the parser".to_owned(),
            has_code_changes: false,
            validation_tools_available: false,
            delegation_allowed: false,
            task_graph: TaskGraph::new(),
            tool_definitions: Vec::new(),
        }
    }

    fn capabilities(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn host_selection_runs_without_candidates() {
        let service = StrategyService::new();
        let decision = service.select(&context(), &capabilities(&["fs:read"]));
        assert_eq!(decision.strategy, TurnStrategy::ToolLoop);
    }

    #[test]
    fn candidate_replaces_strategy_when_capabilities_are_available() {
        let service = StrategyService::new();
        service
            .register_candidate(
                &plugin(),
                StrategyCandidate {
                    id: "demo.plan".to_owned(),
                    strategy: TurnStrategy::PlanThenExecute,
                    applies_to: vec![StrategyReason::FileInspectionRequested],
                    priority: 10,
                },
            )
            .expect("candidate");
        let decision = service.select(&context(), &capabilities(&["fs:read", "fs:write"]));
        assert_eq!(decision.strategy, TurnStrategy::PlanThenExecute);
        assert_eq!(decision.reason, StrategyReason::FileInspectionRequested);
    }

    #[test]
    fn candidate_without_capabilities_is_ignored() {
        let service = StrategyService::new();
        service
            .register_candidate(
                &plugin(),
                StrategyCandidate {
                    id: "demo.plan".to_owned(),
                    strategy: TurnStrategy::PlanThenExecute,
                    applies_to: Vec::new(),
                    priority: 10,
                },
            )
            .expect("candidate");
        let decision = service.select(&context(), &capabilities(&["fs:read"]));
        assert_eq!(decision.strategy, TurnStrategy::ToolLoop);
    }

    #[test]
    fn priority_then_id_is_deterministic() {
        let service = StrategyService::new();
        for (id, priority) in [("demo.b", 5), ("demo.a", 5), ("demo.c", 1)] {
            service
                .register_candidate(
                    &plugin(),
                    StrategyCandidate {
                        id: id.to_owned(),
                        strategy: TurnStrategy::PlanThenExecute,
                        applies_to: Vec::new(),
                        priority,
                    },
                )
                .expect("candidate");
        }
        let ids = service.ids();
        assert_eq!(ids, vec!["demo.a".to_owned(), "demo.b".to_owned(), "demo.c".to_owned()]);
    }

    #[test]
    fn removal_restores_host_selection() {
        let service = StrategyService::new();
        let disposer = service
            .register_candidate(
                &plugin(),
                StrategyCandidate {
                    id: "demo.plan".to_owned(),
                    strategy: TurnStrategy::PlanThenExecute,
                    applies_to: Vec::new(),
                    priority: 10,
                },
            )
            .expect("candidate");
        disposer.dispose();
        assert!(service.is_empty());
        let decision = service.select(&context(), &capabilities(&["fs:read", "fs:write"]));
        assert_eq!(decision.strategy, TurnStrategy::ToolLoop);
    }
}
