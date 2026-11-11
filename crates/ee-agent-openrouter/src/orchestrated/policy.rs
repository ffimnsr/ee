use ee_agent_orchestrator::PolicyEngine;

/// Builds default policy for orchestrated OpenRouter sessions.
///
/// The policy itself is provider-neutral and owned by `ee-agent-orchestrator`
/// (`default_agent_policy`); this wrapper keeps the OpenRouter-facing name and
/// documents the intent for this provider. It admits the same tool classes as
/// every production ee agent and never performs a side effect itself.
#[must_use]
pub fn openrouter_orchestrated_policy() -> PolicyEngine {
    ee_agent_orchestrator::default_agent_policy()
}
