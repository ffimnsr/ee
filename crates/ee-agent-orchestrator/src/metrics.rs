//! Structured usage metrics.
//!
//! [`OrchestratorMetrics`] summarizes model calls, tool calls by
//! side-effect class, subagent spawns by role, cancellations, policy
//! denials, budget stops, and bytes/tokens where the adapter reports them.
//! The store keeps **counters only** — never content, paths, prompts, or
//! model output — so metrics cannot leak sensitive data.  Counter maps are
//! `BTreeMap`-backed and fully serializable for deterministic snapshots and
//! future persistence.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::model::ModelUsage;
use crate::tools::SideEffectClass;

/// Counter-only usage metrics; never holds content.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OrchestratorMetrics {
    model_calls: u64,
    tool_calls: BTreeMap<SideEffectClass, u64>,
    subagent_spawns: BTreeMap<String, u64>,
    cancellations: u64,
    policy_denials: u64,
    budget_stops: u64,
    input_bytes: u64,
    output_bytes: u64,
    input_tokens: u64,
    output_tokens: u64,
}

impl OrchestratorMetrics {
    /// Creates an empty metrics set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Counts one model adapter call.
    pub fn record_model_call(&mut self) {
        self.model_calls += 1;
    }

    /// Counts one tool execution by its side-effect class.
    pub fn record_tool_call(&mut self, class: SideEffectClass) {
        *self.tool_calls.entry(class).or_default() += 1;
    }

    /// Counts one subagent spawn by role name.
    pub fn record_subagent_spawn(&mut self, role: &str) {
        *self.subagent_spawns.entry(role.to_string()).or_default() += 1;
    }

    /// Counts one cancellation (turn, tool, or subagent).
    pub fn record_cancellation(&mut self) {
        self.cancellations += 1;
    }

    /// Counts one policy-denied action.
    pub fn record_policy_denial(&mut self) {
        self.policy_denials += 1;
    }

    /// Counts one budget-exceeded stop.
    pub fn record_budget_stop(&mut self) {
        self.budget_stops += 1;
    }

    /// Accumulates known input/output bytes.
    pub fn record_bytes(&mut self, input_bytes: u64, output_bytes: u64) {
        self.input_bytes = self.input_bytes.saturating_add(input_bytes);
        self.output_bytes = self.output_bytes.saturating_add(output_bytes);
    }

    /// Accumulates token usage where the adapter reported it; unknown fields
    /// (`None`) are skipped, never counted as zero.
    pub fn record_usage(&mut self, usage: &ModelUsage) {
        if let Some(tokens) = usage.input_tokens {
            self.input_tokens = self.input_tokens.saturating_add(tokens as u64);
        }
        if let Some(tokens) = usage.output_tokens {
            self.output_tokens = self.output_tokens.saturating_add(tokens as u64);
        }
    }

    /// Total model adapter calls.
    #[must_use]
    pub fn model_calls(&self) -> u64 {
        self.model_calls
    }

    /// Tool executions of one side-effect class.
    #[must_use]
    pub fn tool_calls(&self, class: SideEffectClass) -> u64 {
        self.tool_calls.get(&class).copied().unwrap_or_default()
    }

    /// Observed side-effect classes with at least one execution, in stable
    /// (derived) order.
    #[must_use]
    pub fn tool_call_classes(&self) -> Vec<SideEffectClass> {
        self.tool_calls.keys().copied().collect()
    }

    /// Total tool executions across all classes.
    #[must_use]
    pub fn tool_calls_total(&self) -> u64 {
        self.tool_calls.values().sum()
    }

    /// Subagent spawns of one role.
    #[must_use]
    pub fn subagent_spawns(&self, role: &str) -> u64 {
        self.subagent_spawns.get(role).copied().unwrap_or_default()
    }

    /// Roles with at least one spawn, in stable (id) order.
    #[must_use]
    pub fn subagent_roles(&self) -> Vec<String> {
        self.subagent_spawns.keys().cloned().collect()
    }

    /// Total subagent spawns across all roles.
    #[must_use]
    pub fn subagent_spawns_total(&self) -> u64 {
        self.subagent_spawns.values().sum()
    }

    /// Cancellations counted.
    #[must_use]
    pub fn cancellations(&self) -> u64 {
        self.cancellations
    }

    /// Policy-denied actions counted.
    #[must_use]
    pub fn policy_denials(&self) -> u64 {
        self.policy_denials
    }

    /// Budget-exceeded stops counted.
    #[must_use]
    pub fn budget_stops(&self) -> u64 {
        self.budget_stops
    }

    /// Accumulated input bytes.
    #[must_use]
    pub fn input_bytes(&self) -> u64 {
        self.input_bytes
    }

    /// Accumulated output bytes.
    #[must_use]
    pub fn output_bytes(&self) -> u64 {
        self.output_bytes
    }

    /// Accumulated input tokens (reported usage only).
    #[must_use]
    pub fn input_tokens(&self) -> u64 {
        self.input_tokens
    }

    /// Accumulated output tokens (reported usage only).
    #[must_use]
    pub fn output_tokens(&self) -> u64 {
        self.output_tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_increment_per_call() {
        let mut metrics = OrchestratorMetrics::new();
        metrics.record_model_call();
        metrics.record_model_call();
        metrics.record_cancellation();
        metrics.record_policy_denial();
        metrics.record_policy_denial();
        metrics.record_budget_stop();

        assert_eq!(metrics.model_calls(), 2);
        assert_eq!(metrics.cancellations(), 1);
        assert_eq!(metrics.policy_denials(), 2);
        assert_eq!(metrics.budget_stops(), 1);
    }

    #[test]
    fn tool_calls_are_counted_by_side_effect_class() {
        let mut metrics = OrchestratorMetrics::new();
        metrics.record_tool_call(SideEffectClass::Read);
        metrics.record_tool_call(SideEffectClass::Read);
        metrics.record_tool_call(SideEffectClass::Write);
        metrics.record_tool_call(SideEffectClass::Execute);

        assert_eq!(metrics.tool_calls(SideEffectClass::Read), 2);
        assert_eq!(metrics.tool_calls(SideEffectClass::Write), 1);
        assert_eq!(metrics.tool_calls(SideEffectClass::Execute), 1);
        assert_eq!(metrics.tool_calls(SideEffectClass::Delegate), 0, "never called");
        assert_eq!(metrics.tool_calls_total(), 4);
        assert_eq!(
            metrics.tool_call_classes(),
            vec![SideEffectClass::Read, SideEffectClass::Write, SideEffectClass::Execute]
        );
    }

    #[test]
    fn subagent_spawns_are_counted_by_role() {
        let mut metrics = OrchestratorMetrics::new();
        metrics.record_subagent_spawn("researcher");
        metrics.record_subagent_spawn("researcher");
        metrics.record_subagent_spawn("implementer");

        assert_eq!(metrics.subagent_spawns("researcher"), 2);
        assert_eq!(metrics.subagent_spawns("implementer"), 1);
        assert_eq!(metrics.subagent_spawns("summarizer"), 0);
        assert_eq!(metrics.subagent_spawns_total(), 3);
        assert_eq!(metrics.subagent_roles(), vec!["implementer", "researcher"], "stable order");
    }

    #[test]
    fn tokens_are_counted_only_where_known() {
        let mut metrics = OrchestratorMetrics::new();
        metrics.record_usage(&ModelUsage::new().with_input_tokens(100).with_output_tokens(50));
        metrics.record_usage(&ModelUsage::new().with_input_tokens(25));
        metrics.record_usage(&ModelUsage::new()); // unknown output — skipped

        assert_eq!(metrics.input_tokens(), 125);
        assert_eq!(metrics.output_tokens(), 50, "unknown usage is never counted as zero");
    }

    #[test]
    fn bytes_accumulate() {
        let mut metrics = OrchestratorMetrics::new();
        metrics.record_bytes(10, 20);
        metrics.record_bytes(5, 7);
        assert_eq!(metrics.input_bytes(), 15);
        assert_eq!(metrics.output_bytes(), 27);
    }

    #[test]
    fn metrics_roundtrip_through_json() {
        let mut metrics = OrchestratorMetrics::new();
        metrics.record_model_call();
        metrics.record_tool_call(SideEffectClass::Read);
        metrics.record_subagent_spawn("researcher");
        metrics.record_cancellation();
        metrics.record_usage(&ModelUsage::new().with_output_tokens(9));

        let json = serde_json::to_string(&metrics).expect("serializes");
        let restored: OrchestratorMetrics = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, metrics);
    }

    #[test]
    fn counters_never_hold_content() {
        // The metrics type is counters only: recording never takes text or
        // paths, so there is nothing sensitive to leak.  This test pins the
        // serialized shape to counts only.
        let mut metrics = OrchestratorMetrics::new();
        metrics.record_tool_call(SideEffectClass::Read);
        let json = serde_json::to_string(&metrics).expect("serializes");
        assert!(json.contains("\"model_calls\":0"));
        assert!(json.contains("\"tool_calls\""));
        assert!(!json.contains("sk-"), "no secret-shaped payload can appear");
    }
}
