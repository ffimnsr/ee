//! Pure shared trust-policy evaluator.
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
    DecisionReason, FallbackEffect, McpServerTrust, OperationIdentity, SafeguardMatch,
    TrustCategory, TrustDecision, TrustEffect, TrustOperation, UsageSnapshot, is_safe_read_profile,
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
    /// Injected host-local workspace trust decision (`ee do trust`): enables
    /// the built-in read-only safe allowlist for `safe_read` profile
    /// operations. The evaluator never reads the decision itself.
    pub(crate) safe_read_enabled: bool,
    /// Injected host-local workspace trust decision for native writes. The
    /// caller sets this only for a prompt whose every target canonicalizes
    /// under the primary workspace root, is not a protected path, and is not
    /// the secret store, so a write outside the workspace directory can
    /// never reach the built-in allow below.
    pub(crate) trusted_write_enabled: bool,
    /// Injected host-local workspace trust decision for native terminal
    /// execution. The caller sets this only for an explicitly granted,
    /// trusted workspace whose validated command has a canonical primary-
    /// workspace cwd, no shell syntax or wrapper, and no execution-affecting
    /// environment overrides.
    pub(crate) workspace_execute_enabled: bool,
    /// Injected host-local unrestricted terminal grant (`--yolo`). The caller
    /// sets this only for a trusted, explicitly granted, non-read-only native
    /// terminal request; explicit denies, mandatory confirms, and safeguards
    /// still run before this allowance.
    pub(crate) workspace_execute_unrestricted: bool,
    /// Effective pinned MCP server grants for this workspace. The caller
    /// supplies this only while the host-local workspace decision is
    /// `trusted`; a stale or malformed grant set is represented as `None` and
    /// therefore cannot authorize anything.
    pub(crate) mcp_server_trust: Option<&'a McpServerTrust>,
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

const TRACE_LAYERS: [&str; 12] = [
    "built_in_deny",
    "persistent_deny",
    "session_deny",
    "mandatory_confirm",
    "built_in_allow",
    "workspace_gate",
    "mcp_server_allow",
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
        // An unrestricted native-terminal grant deliberately covers command
        // text the strict parser cannot normalize (pipes, redirects, globs).
        // The caller only sets this for a trusted, explicitly granted,
        // non-read-only native terminal prompt, so it cannot widen other
        // unknown operations.
        if input.workspace_execute_unrestricted
            && input.operation.category == TrustCategory::Execute
        {
            finish!(
                "built_in_allow",
                None,
                TrustDecision::allow(DecisionReason::BuiltInAllow, None)
            );
        }
        no_match!("built_in_allow");
        no_match!("workspace_gate");
        no_match!("mcp_server_allow");
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
    // Built-in workspace-trusted read-only allowlist. Runs after every deny,
    // session deny, and mandatory-confirm layer, so it can never override an
    // explicit user deny or safeguard; the app injects `safe_read_enabled`
    // only for a `trusted` host-local workspace decision.
    if input.safe_read_enabled
        && let OperationIdentity::Profile { profile } = &input.operation.identity
        && is_safe_read_profile(profile)
    {
        finish!("built_in_allow", None, TrustDecision::allow(DecisionReason::BuiltInAllow, None));
    }
    no_match!("built_in_allow");
    // Built-in workspace-trusted native-write allow. Same precedence slot as
    // the read-only allowlist: denials, safeguards, and mandatory-confirm
    // rules above still win, and the caller only enables the flag for
    // canonical in-primary-workspace, non-protected, non-secret targets.
    if input.trusted_write_enabled
        && matches!(
            input.operation.category,
            TrustCategory::WriteCreate | TrustCategory::WriteModify
        )
        && matches!(input.operation.identity, OperationIdentity::Write { .. })
    {
        finish!("built_in_allow", None, TrustDecision::allow(DecisionReason::BuiltInAllow, None));
    }
    no_match!("built_in_allow");
    // Built-in workspace-trusted native terminal execution allow. Same
    // precedence slot as the write and read-only allows: denies, safeguards,
    // and mandatory-confirm rules above still win, and the caller only
    // enables the flag for a separately granted, trusted workspace with a
    // validated simple command rooted in the primary workspace.
    if input.workspace_execute_enabled
        && input.operation.category == TrustCategory::Execute
        && matches!(input.operation.identity, OperationIdentity::Command { .. })
    {
        finish!("built_in_allow", None, TrustDecision::allow(DecisionReason::BuiltInAllow, None));
    }
    no_match!("built_in_allow");
    if input.workspace_execute_unrestricted && input.operation.category == TrustCategory::Execute {
        finish!("built_in_allow", None, TrustDecision::allow(DecisionReason::BuiltInAllow, None));
    }
    no_match!("built_in_allow");
    if gate_required(input.operation) && !input.workspace_enabled {
        finish!(
            "workspace_gate",
            None,
            TrustDecision::confirm(DecisionReason::WorkspaceDisabled, None)
        );
    }
    no_match!("workspace_gate");
    // Pinned MCP server trust: a host-local grant generated from the
    // application-owned manifest. It sits after every deny, safeguard,
    // mandatory-confirm, and workspace gate layer, so it can never override
    // an explicit denial or an untrusted workspace.
    if let Some(trust) = input.mcp_server_trust
        && let Some(grant) = trust.allows(input.operation)
    {
        let id = Some(grant.id.clone());
        finish!(
            "mcp_server_allow",
            id.clone(),
            TrustDecision::allow(DecisionReason::McpServerAllow, id)
        );
    }
    no_match!("mcp_server_allow");
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
