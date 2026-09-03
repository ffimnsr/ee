use ee_agent_orchestrator::{PolicyEngine, SideEffectSubclass, ToolPolicy};

/// Builds default policy for orchestrated OpenRouter sessions.
///
/// Read, execute, and delegate tools are available because orchestrated mode is
/// production OpenRouter path. Writes and external-network reads are admitted
/// only so trusted ee proxy tools can reach existing host approval and scope
/// checks. Policy never performs either side effect itself.
#[must_use]
pub fn openrouter_orchestrated_policy() -> PolicyEngine {
    PolicyEngine::new(
        ToolPolicy {
            allow_read: true,
            allow_write: true,
            allow_execute: true,
            allow_delegate: true,
            ..ToolPolicy::default()
        }
        .allow_side_effect_subclass(SideEffectSubclass::Overwrite)
        .allow_side_effect_subclass(SideEffectSubclass::ExternalNetwork),
    )
}

#[cfg(test)]
mod tests {
    use ee_agent_orchestrator::{
        PolicyContext, SideEffectClass, SideEffectSubclass, ToolDefinition,
    };

    use super::openrouter_orchestrated_policy;

    #[test]
    fn policy_admits_only_host_approved_external_network_reads() {
        let policy = openrouter_orchestrated_policy();
        let context = PolicyContext::default();
        let network_tool = |class| {
            ToolDefinition::new("ee_fetch_url", "fetches HTTPS content")
                .side_effect_class(class)
                .side_effect_subclass(SideEffectSubclass::ExternalNetwork)
        };

        assert!(
            policy.check(&network_tool(SideEffectClass::Read).host_approval(), context).allow,
            "trusted ee web reads must reach host network approval"
        );
        assert!(
            !policy.check(&network_tool(SideEffectClass::Read), context).allow,
            "external MCP tools cannot borrow ee network allowance"
        );
        assert!(
            !policy.check(&network_tool(SideEffectClass::Execute).host_approval(), context).allow,
            "network allowance must not permit command execution"
        );
    }
}
