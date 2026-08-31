//! Phase 10 guardrails: dependency-boundary assertions and default-policy
//! regression tests.
//!
//! The orchestrator must stay optional, server-side, and conservative:
//! `ee-agent-orchestrator` depends on the ACP server framework only, never on
//! the editor host or CLI crates, and the default policy allows reads while
//! denying writes, executes, and delegation unless explicitly configured.

use ee_agent_orchestrator::{
    OrchestratorConfig, OrchestratorProvider, PolicyContext, PolicyEngine, SideEffectClass,
    ToolDefinition, ToolPolicy,
};

// ── Dependency boundaries ────────────────────────────────────────────────

#[test]
fn manifest_depends_on_acp_server_not_host_or_cli() {
    let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .expect("orchestrator manifest exists");
    assert!(
        manifest.contains("ee-acp-agent-server"),
        "orchestrator depends on the ACP server framework"
    );
    assert!(
        !manifest.contains("ee-agent-host"),
        "orchestrator must never depend on the editor host crate"
    );
    assert!(!manifest.contains("ee-cli"), "orchestrator must never depend on the CLI crate");
}

#[test]
fn examples_and_tests_stay_network_free_by_default() {
    let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .expect("orchestrator manifest exists");
    // No HTTP/network client crates anywhere in the manifest (dependencies,
    // dev-dependencies), and no tokio `net` feature.
    for network_crate in ["reqwest", "ureq", "hyper", "isahc", "surf", "attohttpc"] {
        assert!(
            !manifest.contains(&format!("\n{network_crate}")),
            "orchestrator must not depend on network client {network_crate}"
        );
    }
    for line in manifest.lines() {
        if line.starts_with("tokio") || line.starts_with("tokio ") {
            assert!(
                !line.contains("\"net\""),
                "tokio dependency must not enable the network feature: {line}"
            );
        }
    }
}

#[test]
fn adapter_implements_framework_agent_provider() {
    fn assert_provider<P: ee_acp_agent_server::AgentProvider>() {}
    assert_provider::<OrchestratorProvider>();
}

// ── Default policy regression ────────────────────────────────────────────

fn tool(class: SideEffectClass) -> ToolDefinition {
    ToolDefinition::new("tool", "t").side_effect_class(class)
}

#[test]
fn default_policy_allows_read_tools() {
    let policy = PolicyEngine::default();
    let decision = policy.check(&tool(SideEffectClass::Read), PolicyContext::default());
    assert!(decision.allow, "reads stay available by default: {decision:?}");
}

#[test]
fn default_policy_denies_write_tools() {
    let policy = PolicyEngine::default();
    let decision = policy.check(&tool(SideEffectClass::Write), PolicyContext::default());
    assert!(!decision.allow, "writes fail closed by default");
}

#[test]
fn default_policy_denies_execute_tools() {
    let policy = PolicyEngine::default();
    let decision = policy.check(&tool(SideEffectClass::Execute), PolicyContext::default());
    assert!(!decision.allow, "executes fail closed by default");
}

#[test]
fn default_policy_denies_delegation() {
    let policy = PolicyEngine::default();
    let decision = policy.check(&tool(SideEffectClass::Delegate), PolicyContext::default());
    assert!(!decision.allow, "delegation fails closed by default");
}

#[test]
fn delegation_defaults_bound_depth_and_parallelism() {
    let policy = ToolPolicy::default();
    assert_eq!(policy.max_delegate_depth, 2, "delegation depth default");
    assert_eq!(policy.max_parallel_delegates, 4, "parallel delegation default");
    let config = OrchestratorConfig::default();
    assert_eq!(config.max_subagent_depth, 2, "subagent depth default");
    assert_eq!(config.max_parallel_subagents, 4, "parallel subagent default");
}
