//! Bounded decision log with stable reason codes.
//!
//! Every observable framework decision — strategy selection, tool policy
//! verdicts, model routing, subagent delegation — is recorded as a
//! [`DecisionEntry`] carrying a machine-readable reason code and a short,
//! redacted detail.  The log is bounded (oldest entries evicted first),
//! deterministic, and serializable.  It never stores model output, hidden
//! chain-of-thought, or sensitive content: details are passed through the
//! sensitive-data guard at record time and truncated to a fixed cap.

use serde::{Deserialize, Serialize};

use crate::policy::PolicyDecision;
use crate::sensitive_data::SensitiveDataGuard;
use crate::strategy::StrategyReason;

/// Default cap on retained decision entries.
pub const DEFAULT_MAX_DECISION_LOG_ENTRIES: usize = 256;
/// Cap on one detail's characters (UTF-8 chars, plus an ellipsis).
pub const DECISION_DETAIL_MAX_CHARS: usize = 200;

/// What kind of decision a log entry records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum DecisionKind {
    /// Turn strategy selection.
    Strategy,
    /// Tool policy verdict (allow/deny).
    ToolPolicy,
    /// Model route selection.
    Routing,
    /// Subagent delegation.
    Delegation,
}

/// One recorded decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DecisionEntry {
    /// What kind of decision this is.
    pub kind: DecisionKind,
    /// Stable machine-readable reason code (never free-form model text).
    pub reason_code: String,
    /// Short redacted detail (bounded, diagnostics-only).
    pub detail: String,
    /// Task the decision applied to, when known.
    pub task_id: Option<String>,
}

/// Bounded, redacted decision log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionLog {
    entries: Vec<DecisionEntry>,
    max_entries: usize,
}

impl Default for DecisionLog {
    fn default() -> Self {
        Self { entries: Vec::new(), max_entries: DEFAULT_MAX_DECISION_LOG_ENTRIES }
    }
}

impl DecisionLog {
    /// Creates a log with a custom entry cap (oldest evicted first).
    #[must_use]
    pub fn with_max_entries(max_entries: usize) -> Self {
        Self { entries: Vec::new(), max_entries }
    }

    /// Records a raw decision; `detail` is redacted and truncated.
    ///
    /// Reason codes are the only machine-readable channel — details are
    /// short diagnostics and must never carry model reasoning or secrets.
    pub fn record(
        &mut self,
        kind: DecisionKind,
        reason_code: impl Into<String>,
        detail: impl Into<String>,
    ) {
        let detail = SensitiveDataGuard::new().redact(&detail.into());
        let detail = truncate_chars(&detail, DECISION_DETAIL_MAX_CHARS);
        self.entries.push(DecisionEntry {
            kind,
            reason_code: reason_code.into(),
            detail,
            task_id: None,
        });
        if self.entries.len() > self.max_entries {
            let overflow = self.entries.len() - self.max_entries;
            self.entries.drain(..overflow);
        }
    }

    /// Records a strategy decision with its stable reason code.
    pub fn record_strategy(&mut self, reason: StrategyReason) {
        let code = reason.code().to_string();
        self.record(DecisionKind::Strategy, &code, format!("strategy {code}"));
    }

    /// Records a tool policy verdict; the reason code distinguishes allows
    /// from denials and carries the tool name in the detail.
    pub fn record_tool_policy(&mut self, tool_name: &str, decision: &PolicyDecision) {
        let code = if decision.allow { "policy-allow" } else { "policy-deny" };
        let detail = match &decision.reason {
            Some(reason) => format!("tool {tool_name}: {reason}"),
            None => format!("tool {tool_name}"),
        };
        self.record(DecisionKind::ToolPolicy, code, detail);
    }

    /// Records a model routing decision.
    pub fn record_routing(&mut self, route_id: &str, adapter_id: &str) {
        self.record(
            DecisionKind::Routing,
            format!("route:{route_id}"),
            format!("adapter {adapter_id}"),
        );
    }

    /// Records a subagent delegation decision (spawn or denial).
    pub fn record_delegation(&mut self, role: &str, depth: usize, allowed: bool) {
        let code = if allowed { "delegate-allowed" } else { "delegate-denied" };
        self.record(DecisionKind::Delegation, code, format!("role {role} at depth {depth}"));
    }

    /// Every entry in record order.
    #[must_use]
    pub fn entries(&self) -> &[DecisionEntry] {
        &self.entries
    }

    /// Number of recorded entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the log is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every reason code in record order, for evented assertions.
    #[must_use]
    pub fn reason_codes(&self) -> Vec<String> {
        self.entries.iter().map(|entry| entry.reason_code.clone()).collect()
    }

    /// Clears the log.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(max_chars).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{PolicyContext, PolicyDecision, PolicyEngine, ToolPolicy};
    use crate::strategy::{StrategyReason, TurnStrategy};
    use crate::tools::{SideEffectClass, ToolDefinition};

    #[test]
    fn strategy_decisions_record_reason_codes() {
        let mut log = DecisionLog::default();
        log.record_strategy(StrategyReason::NoToolsRequested);
        log.record_strategy(StrategyReason::MultiFileImplementation);
        assert_eq!(log.len(), 2);
        assert_eq!(log.reason_codes(), vec!["no-tools-requested", "multi-file-implementation"]);
        assert_eq!(log.entries()[0].kind, DecisionKind::Strategy);
        assert!(log.entries()[0].detail.contains("strategy no-tools-requested"));
    }

    #[test]
    fn tool_policy_verdicts_record_allow_and_deny_codes() {
        let mut log = DecisionLog::default();
        let allowed = PolicyEngine::default()
            .check(&ToolDefinition::new("read", "reads"), PolicyContext::default());
        let denied = PolicyEngine::default().check(
            &ToolDefinition::new("write", "writes").side_effect_class(SideEffectClass::Write),
            PolicyContext::default(),
        );
        log.record_tool_policy("read", &allowed);
        log.record_tool_policy("write", &denied);

        assert_eq!(log.reason_codes(), vec!["policy-allow", "policy-deny"]);
        assert_eq!(log.entries()[0].kind, DecisionKind::ToolPolicy);
        assert!(log.entries()[1].detail.contains("tool write"));
    }

    #[test]
    fn routing_decisions_record_route_and_adapter() {
        let mut log = DecisionLog::default();
        log.record_routing("cheap", "cheap-model");
        log.record_routing("strong", "strong-model");
        assert_eq!(log.reason_codes(), vec!["route:cheap", "route:strong"]);
        assert_eq!(log.entries()[0].detail, "adapter cheap-model");
    }

    #[test]
    fn delegation_decisions_record_role_depth_and_verdict() {
        let mut log = DecisionLog::default();
        log.record_delegation("researcher", 0, true);
        log.record_delegation("researcher", 2, false);
        assert_eq!(log.reason_codes(), vec!["delegate-allowed", "delegate-denied"]);
        assert!(log.entries()[1].detail.contains("role researcher at depth 2"));
    }

    #[test]
    fn sensitive_details_are_redacted_at_record_time() {
        let mut log = DecisionLog::default();
        log.record(DecisionKind::ToolPolicy, "policy-deny", "the key is sk-live-1234567890");
        let detail = &log.entries()[0].detail;
        assert!(!detail.contains("sk-live-1234567890"), "secret redacted: {detail}");
        assert!(detail.contains("[redacted]"), "marker present: {detail}");
    }

    #[test]
    fn details_are_bounded_so_chain_of_thought_cannot_accumulate() {
        let mut log = DecisionLog::default();
        let reasoning = "chain of thought: ".repeat(500); // ~8k chars
        log.record(DecisionKind::Strategy, "no-tools-requested", reasoning.clone());
        let detail = &log.entries()[0].detail;
        assert!(
            detail.chars().count() <= DECISION_DETAIL_MAX_CHARS + 1,
            "detail bounded to {} chars, got {}",
            DECISION_DETAIL_MAX_CHARS + 1,
            detail.chars().count()
        );
        assert!(detail.ends_with('…'), "truncation marker present");
        assert!(!detail.contains(&reasoning), "full reasoning text never stored");
    }

    #[test]
    fn log_is_bounded_and_evicts_oldest_first() {
        let mut log = DecisionLog::with_max_entries(3);
        for index in 0..5 {
            log.record(DecisionKind::Routing, format!("route:{index}"), format!("r{index}"));
        }
        assert_eq!(log.len(), 3);
        assert_eq!(log.reason_codes(), vec!["route:2", "route:3", "route:4"]);
    }

    #[test]
    fn log_roundtrips_through_json() {
        let mut log = DecisionLog::default();
        log.record_strategy(StrategyReason::FileInspectionRequested);
        log.record_delegation("researcher", 0, true);
        let json = serde_json::to_string(&log).expect("serializes");
        let restored: DecisionLog = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, log);
    }

    #[test]
    fn strategy_reason_codes_are_stable() {
        // Pin the mapping so reason codes never drift silently.
        assert_eq!(StrategyReason::NoToolsRequested.code(), "no-tools-requested");
        assert_eq!(StrategyReason::FileInspectionRequested.code(), "file-inspection-requested");
        assert_eq!(StrategyReason::MultiFileImplementation.code(), "multi-file-implementation");
        assert_eq!(StrategyReason::UnknownCodebaseChange.code(), "unknown-codebase-change");
        assert_eq!(StrategyReason::ChangesWithValidation.code(), "changes-with-validation");
        assert_eq!(StrategyReason::ParallelIndependentWork.code(), "parallel-independent-work");
    }

    #[test]
    fn policy_denial_decision_carries_reason() {
        let decision = PolicyDecision::denied("write tools are denied by the default policy");
        assert!(!decision.allow);
        assert_eq!(
            decision.reason.as_deref(),
            Some("write tools are denied by the default policy")
        );
        let allowed = PolicyDecision::allowed();
        assert!(allowed.allow);
        assert_eq!(allowed.reason, None);
    }

    #[test]
    fn tool_policy_default_denies_writes() {
        let policy = ToolPolicy::default();
        assert!(!policy.allow_write);
        assert!(!policy.allow_execute);
        assert!(!policy.allow_delegate);
        assert!(policy.allow_read);
        assert_eq!(policy.max_delegate_depth, 2);
    }

    #[test]
    fn clear_resets_the_log() {
        let mut log = DecisionLog::default();
        log.record_strategy(StrategyReason::NoToolsRequested);
        log.clear();
        assert!(log.is_empty());
        assert_eq!(log.len(), 0);
    }

    #[test]
    fn strategy_reason_roundtrips_through_json() {
        let reason = StrategyReason::MultiFileImplementation;
        let json = serde_json::to_string(&reason).expect("serializes");
        let restored: StrategyReason = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, reason);
    }

    #[test]
    fn turn_strategy_variant_is_serializable() {
        // Strategy entries reference the strategy enum in events; keep the
        // serialization contract pinned.
        let strategy = TurnStrategy::ToolLoop;
        let json = serde_json::to_string(&strategy).expect("serializes");
        assert!(json.contains("ToolLoop"));
    }
}
