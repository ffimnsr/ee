#![allow(unused_must_use)]

//! Phase 5 integration tests: replay determinism, prompt-scoped MCP tool
//! registration, remote transports, and provider construction.

use std::sync::Arc;

use ee_plugin_api::ids::PluginId;

use crate::plugin_runtime::host::{AgentPluginHost, AgentPluginHostConfig, NoopAgentNotifier};
use crate::plugin_runtime::services::{ToolScope, ToolService};
use crate::tools::{ServerTool, SideEffectClass, ToolDefinition, ToolResult};
use tokio::sync::watch;

fn workspace(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("ee-agent-phase5-{}-{name}", std::process::id()))
}

fn host(name: &str) -> AgentPluginHost {
    AgentPluginHost::new(AgentPluginHostConfig::new(workspace(name)), Arc::new(NoopAgentNotifier))
        .expect("host")
}

struct TestTool(&'static str);

impl ServerTool for TestTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(self.0, "test").side_effect_class(SideEffectClass::Read)
    }

    fn execute(
        &self,
        _arguments: serde_json::Value,
        _client: ee_agent_acp_server::ClientBridge,
        _cancel: watch::Receiver<bool>,
        _context: crate::tools::ToolCallContext,
    ) -> crate::tools::ToolFuture<ToolResult> {
        Box::pin(async { ToolResult::success("ok") })
    }
}

fn plugin() -> PluginId {
    PluginId::new("ee.mcp").expect("valid plugin id")
}

#[test]
fn prompt_scoped_mcp_tools_are_registered_and_disposed() {
    let service = ToolService::new();
    let scope = ToolScope::Turn { session_id: "session-1".to_owned(), turn: 1 };
    let disposer = service
        .register_for_session(
            &plugin(),
            scope.clone(),
            Some("session-1"),
            Arc::new(TestTool("mcp_read")),
        )
        .expect("registers");
    assert_eq!(service.scope_of("mcp_read"), Some(scope));
    disposer.dispose();
    assert!(!service.contains("mcp_read"));
}

#[test]
fn remote_providers_never_enter_the_in_process_tool_registry() {
    let host = host("remote");
    host.register_remote_provider("acp.remote-agent");
    assert_eq!(host.remote_providers(), vec!["acp.remote-agent".to_owned()]);
    assert!(host.tools().is_empty());
}

#[test]
fn host_registers_model_adapters_without_duplicating_route_authority() {
    let host = host("models");
    let registry = crate::model_registry::ModelRegistry::single(Arc::new(
        crate::test_support::FakeModel::new(Vec::new()),
    ));
    host.models().adopt_registry(&registry).expect("adopts");
    assert!(host.models().contains("default"));
}

#[test]
fn provider_constructs_with_an_explicit_plugin_host() {
    let host = Arc::new(host("provider"));
    let models = crate::model_registry::ModelRegistry::single(Arc::new(
        crate::test_support::FakeModel::new(Vec::new()),
    ));
    let provider = crate::provider_adapter::OrchestratorProvider::with_model_registry_and_host(
        crate::provider_adapter::OrchestratorProviderConfig::default(),
        models,
        crate::policy::default_agent_policy(),
        Arc::clone(&host),
    )
    .expect("provider");
    assert!(provider.plugin_host().is_started());
    assert!(Arc::ptr_eq(&provider.plugin_host(), &host));
}

#[cfg(feature = "test-utils")]
mod replay_determinism {
    use super::*;
    use crate::replay::{run_replay, simple_answer_replay};

    #[tokio::test]
    async fn observational_plugin_preserves_the_replay_trace() {
        let plain = run_replay(simple_answer_replay()).await;

        let host = host("replay");
        let bridge = host.bridge();
        let observer = bridge.observer("s-1");
        let observed = run_replay(simple_answer_replay().with_observer(observer)).await;

        assert_eq!(plain.events, observed.events);
        assert_eq!(plain.prompt_result, observed.prompt_result);
        assert_eq!(plain.tasks.list(), observed.tasks.list());
    }

    #[tokio::test]
    async fn context_provider_does_not_change_the_replay_trace() {
        let host = host("replay-context");
        host.context()
            .register_provider(
                &PluginId::new("demo").expect("plugin"),
                "demo.context",
                crate::trust::TrustLevel::ToolOutputUntrusted,
                1024,
                Arc::new(EmptyProvider),
            )
            .expect("provider");
        let bridge = host.bridge();
        let observer = bridge.observer("s-1");
        let plain = run_replay(simple_answer_replay()).await;
        let with_plugin = run_replay(simple_answer_replay().with_observer(observer)).await;
        assert_eq!(plain.events, with_plugin.events);
    }

    struct EmptyProvider;

    impl crate::plugin_runtime::ContextProvider for EmptyProvider {
        fn collect(
            &self,
            _request: &crate::plugin_runtime::services::ContextCollectRequest,
        ) -> Vec<crate::plugin_runtime::ContextContribution> {
            Vec::new()
        }
    }
}
