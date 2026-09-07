//! Runtime tests: shared fakes, plumbing, and fixture helpers.
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use ee_acp_agent_server::server::OutboundEvent;
use ee_acp_agent_server::{ClientBridge, UpdateSink};
use ee_agent_protocol::{ContentBlock, SessionId, SessionUpdate, StopReason, TextContent};
use serde_json::json;
use tokio::sync::{mpsc, watch};

use super::*;
use crate::events::OrchestratorEvent;
use crate::model::{ModelResponse, ModelRole};
use crate::model_registry::{
    ModelCapability, ModelFamily, ModelIdentity, ModelRegistration, RUBBER_DUCK_ROLE,
};
use crate::policy::{PolicyEngine, ToolPolicy};
use crate::strategy::StrategicInput;
use crate::test_support::{
    DELEGATED_HANDOFF_OUTPUT, FakeModel, FakeTool, delegate_then_answer_script,
    endless_tool_loop_script, simple_answer_script, tool_then_answer_script,
};
use crate::tools::{SideEffectClass, ToolDefinition, ToolIntent, ToolResult};

fn prompt(text: &str) -> PromptContext {
    PromptContext::new(SessionId::new("s-1"), vec![ContentBlock::Text(TextContent::new(text))])
}

fn plumbing() -> (UpdateSink, ClientBridge, mpsc::UnboundedReceiver<OutboundEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (
        UpdateSink::new_for_test(SessionId::new("s-1"), tx.clone()),
        ClientBridge::new_for_test(Duration::from_secs(5), tx),
        rx,
    )
}

/// Drains the next `session/update` notification from the outbound
/// channel, panicking on any other event.
async fn next_update(rx: &mut mpsc::UnboundedReceiver<OutboundEvent>) -> SessionUpdate {
    match rx.recv().await.expect("outbound event queued") {
        OutboundEvent::Update { update, .. } => *update,
        other => panic!("expected update event, got {other:?}"),
    }
}

fn echo_tool() -> Arc<FakeTool> {
    Arc::new(FakeTool::new(
        ToolDefinition::new("echo", "echoes its arguments")
            .side_effect_class(SideEffectClass::Read),
        ToolResult::success("echoed"),
    ))
}

fn repair_context_tool(name: &str, value: serde_json::Value) -> Arc<FakeTool> {
    Arc::new(FakeTool::new(
        ToolDefinition::new(name, "repair host context").side_effect_class(SideEffectClass::Read),
        ToolResult::success_structured(value.to_string(), value),
    ))
}
fn budget_event(
    iterations: usize,
    model_calls: usize,
    tools: usize,
    subagents: usize,
    output_bytes: usize,
) -> OrchestratorEvent {
    OrchestratorEvent::BudgetUpdated {
        iterations_used: iterations,
        model_calls_used: model_calls,
        tool_calls_used: tools,
        subagents_used: subagents,
        output_bytes_used: output_bytes,
    }
}

fn read_file_tool() -> Arc<FakeTool> {
    Arc::new(FakeTool::new(
        ToolDefinition::new("read_file", "reads a file").side_effect_class(SideEffectClass::Read),
        ToolResult::success("file contents"),
    ))
}
fn compact_runtime(config: OrchestratorConfig, model: Arc<FakeModel>) -> OrchestratorRuntime {
    let mut memory = MemoryStore::new(4_096);
    memory.insert(MemoryItem::new("decision:api", "use v2")).expect("inserts");
    memory
        .insert(MemoryItem::from_task("obs:file", "old read", TaskId::new("task-1")))
        .expect("inserts");
    memory
        .insert(MemoryItem::from_task("obs:file", "new read", TaskId::new("task-1")))
        .expect("inserts");
    memory.insert(MemoryItem::new("constraint:offline", "no network")).expect("inserts");
    memory.insert(MemoryItem::new("validation:tests", "all pass")).expect("inserts");
    let runtime = OrchestratorRuntime::with_state(
        config,
        model,
        crate::policy::PolicyEngine::default(),
        TaskGraph::new(),
        memory,
    );
    runtime.register_tool(echo_tool()).expect("registers echo");
    runtime
}
#[derive(Clone)]
struct DelayedModel {
    delays: Arc<Mutex<VecDeque<Duration>>>,
    default_delay: Duration,
    inner: FakeModel,
}

impl DelayedModel {
    fn new(delays: Vec<Duration>, default_delay: Duration, inner: FakeModel) -> Self {
        Self { delays: Arc::new(Mutex::new(delays.into())), default_delay, inner }
    }
}

impl ModelAdapter for DelayedModel {
    fn complete(
        &self,
        request: crate::model::ModelRequest,
        cancel: watch::Receiver<bool>,
    ) -> crate::model::ModelFuture<Result<crate::model::ModelResponse, crate::model::ModelError>>
    {
        let delay =
            self.delays.lock().expect("delays poisoned").pop_front().unwrap_or(self.default_delay);
        let inner = self.inner.clone();
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            if *cancel.borrow() {
                return Err(crate::model::ModelError::Cancelled);
            }
            inner.complete(request, cancel).await
        })
    }
}

fn recovery_config(turn_timeout: Duration) -> OrchestratorConfig {
    OrchestratorConfig {
        turn_timeout,
        recovery: crate::config::RecoveryConfig {
            enabled: true,
            ..crate::config::RecoveryConfig::default()
        },
        // Scripted text-only responses make no task-graph progress;
        // disable the no-progress rule for these tests.
        stuck: crate::stuck::StuckConfig {
            max_no_progress_iterations: 100,
            ..crate::stuck::StuckConfig::default()
        },
        ..OrchestratorConfig::default()
    }
}

mod compact_tests;
mod core_tests;
mod fixture_tests;
mod recovery_tests;
mod strategic_tests;
