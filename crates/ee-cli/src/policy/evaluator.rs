//! Pure shared trust-policy evaluator (Phase 1 foundation).
//!
//! The evaluator never reads files, spawns processes, touches transports or
//! UI, reads the clock, or mutates usage state.  Every input — effective
//! rule set, session policy state, current time, usage snapshot, and the
//! normalized operation — is injected by the caller.  Session deny is
//! evaluated before every session allow and persistent rule; malformed,
//! cross-workspace, cross-agent, expired, and exhausted rules are rejected
//! before any domain matcher runs.

use std::time::SystemTime;

use super::rules::TrustRule;
use super::session::SessionPolicy;
use super::{
    DecisionReason, FallbackEffect, OperationIdentity, SafeguardMatch, TrustCategory,
    TrustDecision, TrustEffect, TrustOperation, UsageSnapshot,
};

/// Immutable inputs for one policy evaluation.
pub(crate) struct PolicyInput<'a> {
    /// Session the operation belongs to (session-policy key).
    pub(crate) session_id: &'a str,
    /// Session-layer fingerprint (session-policy key); the session layer
    /// computes it from the operation kind and identity.
    pub(crate) fingerprint: &'a str,
    /// Normalized operation.
    pub(crate) operation: &'a TrustOperation,
    /// In-memory session allow/deny state.
    pub(crate) session: &'a SessionPolicy,
    /// Immutable effective persistent rule set (empty on any store failure).
    pub(crate) rules: &'a [TrustRule],
    /// Injected current time; the evaluator never reads the system clock.
    pub(crate) now: SystemTime,
    /// Injected session-local usage snapshot keyed by rule id; never
    /// mutated here.
    pub(crate) usage: &'a UsageSnapshot,
    /// Host-local workspace gate; required for read and curated-profile
    /// operations, and never sufficient by itself.
    pub(crate) workspace_enabled: bool,
    /// Application-owned, non-overridable safeguard match, if any.
    pub(crate) built_in_deny: Option<SafeguardMatch>,
    /// Effective exact-tool fallback, if configured.
    pub(crate) tool_default: Option<FallbackEffect>,
    /// Effective side-effect-category fallback, if configured.
    pub(crate) category_default: Option<FallbackEffect>,
    /// Effective global fallback, if configured.
    pub(crate) global_default: Option<FallbackEffect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TraceStatus {
    NoMatch,
    Matched,
    NotReached,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrecedenceTraceStep {
    pub(crate) layer: &'static str,
    pub(crate) status: TraceStatus,
    pub(crate) rule_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EvaluationResult {
    pub(crate) decision: TrustDecision,
    pub(crate) trace: Vec<PrecedenceTraceStep>,
}

const TRACE_LAYERS: [&str; 10] = [
    "built_in_deny",
    "persistent_deny",
    "session_deny",
    "mandatory_confirm",
    "workspace_gate",
    "session_allow",
    "bounded_persistent_allow",
    "tool_default",
    "category_default",
    "global_default",
];

/// Evaluates one operation and returns a redacted machine-readable decision
/// without mutating any state.
pub(crate) fn evaluate(input: &PolicyInput<'_>) -> TrustDecision {
    evaluate_with_trace(input).decision
}

/// Same pure evaluator with a complete redacted precedence trace. Tester UI
/// uses this directly, so synthetic checks cannot diverge from dispatch policy.
pub(crate) fn evaluate_with_trace(input: &PolicyInput<'_>) -> EvaluationResult {
    let mut trace = Vec::with_capacity(TRACE_LAYERS.len());
    macro_rules! no_match {
        ($layer:literal) => {
            trace.push(PrecedenceTraceStep {
                layer: $layer,
                status: TraceStatus::NoMatch,
                rule_id: None,
            })
        };
    }
    macro_rules! finish {
        ($layer:literal, $rule_id:expr, $decision:expr) => {{
            let rule_id = $rule_id;
            trace.push(PrecedenceTraceStep {
                layer: $layer,
                status: TraceStatus::Matched,
                rule_id: rule_id.clone(),
            });
            for layer in TRACE_LAYERS.iter().skip(trace.len()) {
                trace.push(PrecedenceTraceStep {
                    layer,
                    status: TraceStatus::NotReached,
                    rule_id: None,
                });
            }
            return EvaluationResult { decision: $decision, trace };
        }};
    }

    if let Some(safeguard) = input.built_in_deny {
        let id = Some(safeguard.rule_id.to_string());
        finish!("built_in_deny", id.clone(), TrustDecision::deny(DecisionReason::BuiltInDeny, id));
    }
    no_match!("built_in_deny");
    if let Some(rule) = matching_rule(input, TrustEffect::Deny) {
        let id = Some(rule.id().to_string());
        finish!(
            "persistent_deny",
            id.clone(),
            TrustDecision::deny(DecisionReason::PersistentDeny, id)
        );
    }
    no_match!("persistent_deny");
    if input.session.is_denied(input.session_id, input.fingerprint) {
        finish!("session_deny", None, TrustDecision::deny(DecisionReason::SessionDeny, None));
    }
    no_match!("session_deny");
    if input.operation.is_unknown() {
        no_match!("mandatory_confirm");
        no_match!("workspace_gate");
        no_match!("session_allow");
        no_match!("bounded_persistent_allow");
        if let Some(effect) = input.tool_default {
            finish!("tool_default", None, fallback_decision(effect, FallbackSource::Tool));
        }
        no_match!("tool_default");
        if let Some(effect) = input.category_default {
            finish!("category_default", None, fallback_decision(effect, FallbackSource::Category));
        }
        no_match!("category_default");
        let decision = unknown_decision(input);
        finish!("global_default", None, decision);
    }
    if let Some(rule) = matching_rule(input, TrustEffect::Confirm) {
        let id = Some(rule.id().to_string());
        finish!(
            "mandatory_confirm",
            id.clone(),
            TrustDecision::confirm(DecisionReason::MandatoryConfirm, id)
        );
    }
    no_match!("mandatory_confirm");
    if gate_required(input.operation) && !input.workspace_enabled {
        finish!(
            "workspace_gate",
            None,
            TrustDecision::confirm(DecisionReason::WorkspaceDisabled, None)
        );
    }
    no_match!("workspace_gate");
    if input.session.is_allowed(input.session_id, input.fingerprint) {
        finish!("session_allow", None, TrustDecision::allow(DecisionReason::SessionAllow, None));
    }
    no_match!("session_allow");
    if let Some(rule) = matching_rule(input, TrustEffect::Allow) {
        let id = Some(rule.id().to_string());
        finish!(
            "bounded_persistent_allow",
            id.clone(),
            TrustDecision::allow(DecisionReason::PersistentAllow, id)
        );
    }
    no_match!("bounded_persistent_allow");
    if let Some(effect) = input.tool_default {
        finish!("tool_default", None, fallback_decision(effect, FallbackSource::Tool));
    }
    no_match!("tool_default");
    if let Some(effect) = input.category_default {
        finish!("category_default", None, fallback_decision(effect, FallbackSource::Category));
    }
    no_match!("category_default");
    if let Some(effect) = input.global_default {
        finish!("global_default", None, fallback_decision(effect, FallbackSource::Global));
    }
    finish!("global_default", None, TrustDecision::confirm(DecisionReason::NoMatchingRule, None));
}

fn matching_rule<'a>(input: &PolicyInput<'a>, effect: TrustEffect) -> Option<&'a TrustRule> {
    input
        .rules
        .iter()
        .filter(|rule| rule.effect() == effect)
        .filter(|rule| scope_matches(rule, input))
        .filter(|rule| rule.matches(input.operation))
        .min_by(|left, right| left.id().cmp(right.id()))
}

#[derive(Clone, Copy)]
enum FallbackSource {
    Tool,
    Category,
    Global,
}

fn fallback_decision(effect: FallbackEffect, source: FallbackSource) -> TrustDecision {
    match (effect, source) {
        (FallbackEffect::Deny, FallbackSource::Tool) => {
            TrustDecision::deny(DecisionReason::ToolDefaultDeny, None)
        }
        (FallbackEffect::Confirm, FallbackSource::Tool) => {
            TrustDecision::confirm(DecisionReason::ToolDefaultConfirm, None)
        }
        (FallbackEffect::Deny, FallbackSource::Category) => {
            TrustDecision::deny(DecisionReason::CategoryDefaultDeny, None)
        }
        (FallbackEffect::Confirm, FallbackSource::Category) => {
            TrustDecision::confirm(DecisionReason::CategoryDefaultConfirm, None)
        }
        (FallbackEffect::Deny, FallbackSource::Global) => {
            TrustDecision::deny(DecisionReason::GlobalDefaultDeny, None)
        }
        (FallbackEffect::Confirm, FallbackSource::Global) => {
            TrustDecision::confirm(DecisionReason::GlobalDefaultConfirm, None)
        }
    }
}

fn unknown_decision(input: &PolicyInput<'_>) -> TrustDecision {
    if let Some(effect) = input.tool_default {
        return fallback_decision(effect, FallbackSource::Tool);
    }
    if let Some(effect) = input.category_default {
        return fallback_decision(effect, FallbackSource::Category);
    }
    if input.global_default == Some(FallbackEffect::Deny) {
        return fallback_decision(FallbackEffect::Deny, FallbackSource::Global);
    }
    TrustDecision::confirm(DecisionReason::UnknownOperation, None)
}

/// Operations that need the workspace gate before any persistent rule can
/// match.
fn gate_required(operation: &TrustOperation) -> bool {
    operation.category == TrustCategory::Read
        || matches!(operation.identity, OperationIdentity::Profile { .. })
}

/// Common scope gate: workspace identity, agent scope, expiry, and use
/// budget.  Any mismatch rejects the rule before domain matching.
fn scope_matches(rule: &TrustRule, input: &PolicyInput<'_>) -> bool {
    let scope = rule.scope();
    if scope.workspace != input.operation.workspace {
        return false;
    }
    let agent_ok = match &scope.agent {
        None => true,
        Some(agent) => input.operation.agent.as_deref() == Some(agent.as_str()),
    };
    if !agent_ok {
        return false;
    }
    if scope.expires_at.is_some_and(|expires| expires <= input.now) {
        return false;
    }
    if rule.effect() == TrustEffect::Allow
        && let Some(max_uses) = scope.max_uses
        && input.usage.used(rule.id()) >= max_uses
    {
        return false;
    }
    true
}
