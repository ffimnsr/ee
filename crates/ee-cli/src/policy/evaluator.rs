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
    DecisionReason, OperationIdentity, TrustCategory, TrustDecision, TrustOperation, UsageSnapshot,
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
}

/// Evaluates one operation and returns a redacted machine-readable decision
/// without mutating any state.
pub(crate) fn evaluate(input: &PolicyInput<'_>) -> TrustDecision {
    // Session deny precedes every session allow and persistent rule.
    if input.session.is_denied(input.session_id, input.fingerprint) {
        return TrustDecision::prompt(DecisionReason::SessionDeny);
    }
    // Session allow resolves silently (shared precedence contract).
    if input.session.is_allowed(input.session_id, input.fingerprint) {
        return TrustDecision::allow(DecisionReason::SessionAllow, None);
    }
    // Unknown operations never match a persistent rule.
    if input.operation.is_unknown() {
        return TrustDecision::prompt(DecisionReason::UnknownOperation);
    }
    // The workspace gate enables evaluation for read and curated-profile
    // operations; it alone never authorizes anything.
    if gate_required(input.operation) && !input.workspace_enabled {
        return TrustDecision::prompt(DecisionReason::WorkspaceDisabled);
    }
    // Persistent rules: scope checks first, domain matcher last.
    for rule in input.rules {
        if !scope_matches(rule, input) {
            continue;
        }
        if rule.matches(input.operation) {
            return TrustDecision::allow(
                DecisionReason::PersistentAllow,
                Some(rule.id().to_string()),
            );
        }
    }
    TrustDecision::prompt(DecisionReason::NoMatchingRule)
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
    if let Some(max_uses) = scope.max_uses
        && input.usage.used(rule.id()) >= max_uses
    {
        return false;
    }
    true
}
