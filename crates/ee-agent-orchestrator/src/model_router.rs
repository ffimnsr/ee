//! Deterministic model routing.
//!
//! [`ModelRouter`] maps task kinds (and optional subagent role names) to
//! configured [`ModelRoute`] values.  Simple summaries and research go to the
//! cheap tier; implementation, review, and delegation go to the strong tier
//! when one is configured; subagent roles route to role-specific adapters
//! when configured.  Selection is fully deterministic (tier preference, then
//! route id order) and every decision is recorded as a `ModelRouted` event so
//! tests can assert the exact routing sequence.

use serde::{Deserialize, Serialize};

use crate::error::OrchestratorError;
use crate::events::{EventRecorder, OrchestratorEvent};

/// Cost/strength tier of one model route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ModelTier {
    /// Cheap/fast adapter for simple summaries and research.
    Cheap,
    /// Strong/capable adapter for implementation, review, and delegation.
    Strong,
}

/// What kind of work a model call serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum TaskKind {
    /// Simple answer or summary; no tool context required.
    Simple,
    /// Read-only research over workspace content.
    Research,
    /// Implementation work over one or more files.
    Implementation,
    /// Review or validation of existing work.
    Review,
    /// Delegation to subagents.
    Delegation,
}

/// One routing target: a model adapter serving selected task kinds and roles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelRoute {
    /// Stable route id used in events and diagnostics.
    pub route_id: String,
    /// The model adapter id this route resolves to.
    pub adapter_id: String,
    /// Task kinds this route serves; empty serves every kind.
    pub task_kinds: Vec<TaskKind>,
    /// Subagent role names this route serves; empty serves any role.
    pub roles: Vec<String>,
    /// Cost/strength tier.
    pub tier: ModelTier,
}

impl ModelRoute {
    /// Creates a catch-all route for one adapter.
    #[must_use]
    pub fn new(
        route_id: impl Into<String>,
        adapter_id: impl Into<String>,
        tier: ModelTier,
    ) -> Self {
        Self {
            route_id: route_id.into(),
            adapter_id: adapter_id.into(),
            task_kinds: Vec::new(),
            roles: Vec::new(),
            tier,
        }
    }

    /// Restricts the route to the given task kinds.
    #[must_use]
    pub fn for_kinds(mut self, kinds: &[TaskKind]) -> Self {
        self.task_kinds = kinds.to_vec();
        self
    }

    /// Restricts the route to the given subagent role names.
    #[must_use]
    pub fn for_roles(mut self, roles: &[&str]) -> Self {
        self.roles = roles.iter().map(|role| (*role).to_string()).collect();
        self
    }
}

/// Deterministic task → adapter router.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRouter {
    routes: Vec<ModelRoute>,
}

impl ModelRouter {
    /// Creates a router; rejects an empty route list and duplicate route ids.
    pub fn new(routes: Vec<ModelRoute>) -> Result<Self, OrchestratorError> {
        if routes.is_empty() {
            return Err(OrchestratorError::InvalidState(
                "model router requires at least one route".into(),
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for route in &routes {
            if !seen.insert(route.route_id.clone()) {
                return Err(OrchestratorError::InvalidState(format!(
                    "duplicate model route id {}",
                    route.route_id
                )));
            }
        }
        Ok(Self { routes })
    }

    /// Single-route router: everything goes to one adapter.
    #[must_use]
    pub fn single(adapter_id: impl Into<String>) -> Self {
        Self { routes: vec![ModelRoute::new("default", adapter_id, ModelTier::Cheap)] }
    }

    /// Every configured route, in configured order.
    #[must_use]
    pub fn routes(&self) -> &[ModelRoute] {
        &self.routes
    }

    /// Deterministically selects the route for a task kind and optional
    /// subagent role, recording the decision as a `ModelRouted` event.
    ///
    /// Role-specific routes win when the role matches; otherwise the kind is
    /// matched (a catch-all route serves every kind).  Among candidates the
    /// tier preferred for the kind is chosen (`Cheap` for simple/research,
    /// `Strong` for implementation/review/delegation), falling back to any
    /// candidate; ties break by route id.  Fails closed when nothing matches.
    pub fn select(
        &self,
        kind: TaskKind,
        role: Option<&str>,
        events: &EventRecorder,
    ) -> Result<&ModelRoute, OrchestratorError> {
        let route = self.select_inner(kind, role)?;
        events.record(OrchestratorEvent::ModelRouted {
            route_id: route.route_id.clone(),
            adapter_id: route.adapter_id.clone(),
            task_kind: kind,
            role: role.map(str::to_string),
        });
        Ok(route)
    }

    fn select_inner<'a>(
        &'a self,
        kind: TaskKind,
        role: Option<&str>,
    ) -> Result<&'a ModelRoute, OrchestratorError> {
        // Role-specific routes win outright when the role matches; other
        // routes match by task kind (catch-all routes serve every kind).
        // Role-configured routes never serve general kind routing.
        let role_matches: Vec<&ModelRoute> = match role {
            Some(role) => self
                .routes
                .iter()
                .filter(|route| route.roles.iter().any(|name| name == role))
                .collect(),
            None => Vec::new(),
        };
        let candidates = if !role_matches.is_empty() {
            role_matches
        } else {
            self.routes
                .iter()
                .filter(|route| {
                    route.roles.is_empty()
                        && (route.task_kinds.is_empty() || route.task_kinds.contains(&kind))
                })
                .collect()
        };
        let preferred = preferred_tier(kind);
        let mut ranked = candidates;
        ranked.sort_by(|a, b| {
            let a_pref = a.tier == preferred;
            let b_pref = b.tier == preferred;
            // Preferred tier first; then lower tier; then stable route id.
            b_pref.cmp(&a_pref).then(a.tier.cmp(&b.tier)).then(a.route_id.cmp(&b.route_id))
        });
        ranked.into_iter().next().ok_or_else(|| {
            OrchestratorError::InvalidState(format!(
                "no model route for task kind {kind:?} and role {role:?}"
            ))
        })
    }
}

/// Tier preferred for a task kind: cheap for simple/research, strong for
/// implementation/review/delegation.
#[must_use]
pub fn preferred_tier(kind: TaskKind) -> ModelTier {
    match kind {
        TaskKind::Simple | TaskKind::Research => ModelTier::Cheap,
        TaskKind::Implementation | TaskKind::Review | TaskKind::Delegation => ModelTier::Strong,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routes() -> Vec<ModelRoute> {
        vec![
            ModelRoute::new("cheap", "cheap-model", ModelTier::Cheap),
            ModelRoute::new("strong", "strong-model", ModelTier::Strong).for_kinds(&[
                TaskKind::Implementation,
                TaskKind::Review,
                TaskKind::Delegation,
            ]),
            ModelRoute::new("researcher", "research-model", ModelTier::Cheap)
                .for_roles(&["researcher", "code_reader"]),
        ]
    }

    fn recorder() -> EventRecorder {
        EventRecorder::new()
    }

    #[test]
    fn simple_summaries_route_to_cheap_adapter() {
        let router = ModelRouter::new(routes()).expect("router");
        let events = recorder();
        let route = router.select(TaskKind::Simple, None, &events).expect("route");
        assert_eq!(route.adapter_id, "cheap-model");
        assert_eq!(route.tier, ModelTier::Cheap);
    }

    #[test]
    fn research_routes_to_cheap_adapter() {
        let router = ModelRouter::new(routes()).expect("router");
        let events = recorder();
        let route = router.select(TaskKind::Research, None, &events).expect("route");
        assert_eq!(route.adapter_id, "cheap-model");
    }

    #[test]
    fn implementation_and_review_route_to_strong_adapter() {
        let router = ModelRouter::new(routes()).expect("router");
        let events = recorder();
        for kind in [TaskKind::Implementation, TaskKind::Review, TaskKind::Delegation] {
            let route = router.select(kind, None, &events).expect("route");
            assert_eq!(route.adapter_id, "strong-model", "{kind:?} must use the strong tier");
        }
    }

    #[test]
    fn subagent_roles_route_to_role_specific_adapters() {
        let router = ModelRouter::new(routes()).expect("router");
        let events = recorder();
        let route =
            router.select(TaskKind::Implementation, Some("researcher"), &events).expect("route");
        assert_eq!(route.adapter_id, "research-model", "role route wins over kind route");
        let route = router.select(TaskKind::Simple, Some("code_reader"), &events).expect("route");
        assert_eq!(route.adapter_id, "research-model");
    }

    #[test]
    fn unknown_role_falls_back_to_kind_routing() {
        let router = ModelRouter::new(routes()).expect("router");
        let events = recorder();
        let route = router.select(TaskKind::Simple, Some("summarizer"), &events).expect("route");
        assert_eq!(route.adapter_id, "cheap-model");
    }

    #[test]
    fn single_route_serves_everything() {
        let router = ModelRouter::single("only-model");
        let events = recorder();
        for kind in [
            TaskKind::Simple,
            TaskKind::Research,
            TaskKind::Implementation,
            TaskKind::Review,
            TaskKind::Delegation,
        ] {
            let route = router.select(kind, None, &events).expect("route");
            assert_eq!(route.adapter_id, "only-model");
        }
    }

    #[test]
    fn selection_is_deterministic_across_repeats() {
        let router = ModelRouter::new(routes()).expect("router");
        let events = recorder();
        let first =
            router.select(TaskKind::Simple, None, &events).expect("route").adapter_id.clone();
        for _ in 0..10 {
            let again = router.select(TaskKind::Simple, None, &events).expect("route");
            assert_eq!(again.adapter_id, first);
        }
    }

    #[test]
    fn selection_is_recorded_as_event() {
        let router = ModelRouter::new(routes()).expect("router");
        let events = recorder();
        router.select(TaskKind::Implementation, Some("researcher"), &events).expect("route");
        let events = events.events();
        assert_eq!(events.len(), 1);
        match &events[0] {
            OrchestratorEvent::ModelRouted { route_id, adapter_id, task_kind, role } => {
                assert_eq!(route_id, "researcher");
                assert_eq!(adapter_id, "research-model");
                assert_eq!(*task_kind, TaskKind::Implementation);
                assert_eq!(role.as_deref(), Some("researcher"));
            }
            other => panic!("expected ModelRouted event, got {other:?}"),
        }
    }

    #[test]
    fn no_matching_route_fails_closed() {
        let router = ModelRouter::new(vec![
            ModelRoute::new("impl", "m", ModelTier::Strong).for_kinds(&[TaskKind::Implementation]),
        ])
        .expect("router");
        let events = recorder();
        let error = router.select(TaskKind::Simple, None, &events).expect_err("no match");
        assert!(matches!(error, OrchestratorError::InvalidState(_)));
    }

    #[test]
    fn empty_route_list_and_duplicate_ids_are_rejected() {
        let error = ModelRouter::new(Vec::new()).expect_err("empty rejected");
        assert!(matches!(error, OrchestratorError::InvalidState(_)));
        let duplicate = vec![
            ModelRoute::new("a", "m1", ModelTier::Cheap),
            ModelRoute::new("a", "m2", ModelTier::Strong),
        ];
        let error = ModelRouter::new(duplicate).expect_err("duplicate rejected");
        assert!(matches!(error, OrchestratorError::InvalidState(_)));
    }

    #[test]
    fn tiers_and_kinds_roundtrip_through_json() {
        let router = ModelRouter::new(routes()).expect("router");
        let json = serde_json::to_string(&router).expect("serializes");
        let restored: ModelRouter = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, router);
        for kind in [
            TaskKind::Simple,
            TaskKind::Research,
            TaskKind::Implementation,
            TaskKind::Review,
            TaskKind::Delegation,
        ] {
            let events = recorder();
            let a = router.select(kind, None, &events).expect("route");
            let b = restored.select(kind, None, &events).expect("route");
            assert_eq!(a, b, "{kind:?} routes identically after round-trip");
        }
    }

    #[test]
    fn preferred_tier_matches_documentation() {
        assert_eq!(preferred_tier(TaskKind::Simple), ModelTier::Cheap);
        assert_eq!(preferred_tier(TaskKind::Research), ModelTier::Cheap);
        assert_eq!(preferred_tier(TaskKind::Implementation), ModelTier::Strong);
        assert_eq!(preferred_tier(TaskKind::Review), ModelTier::Strong);
        assert_eq!(preferred_tier(TaskKind::Delegation), ModelTier::Strong);
    }
}
