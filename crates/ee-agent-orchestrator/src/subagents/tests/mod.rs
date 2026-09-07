//! Subagent tests: harness.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ee_acp_agent_server::server::OutboundEvent;
use ee_acp_agent_server::{ClientBridge, PromptContext, UpdateSink};
use ee_agent_protocol::{ContentBlock, SessionId, SessionUpdate, StopReason, TextContent};
use serde_json::json;
use tokio::sync::{mpsc, watch};

use super::*;
use crate::config::OrchestratorConfig;
use crate::decision_log::DecisionKind;
use crate::model::{ModelContent, ModelError, ModelFuture, ModelRequest, ModelResponse, ModelRole};
use crate::model_router::{ModelRoute, ModelTier};
use crate::policy::{PolicyEngine, ToolPolicy};
use crate::runtime::OrchestratorRuntime;
use crate::tasks::TaskStatus;
use crate::test_support::{FakeModel, FakeTool};
use crate::tools::{ToolDefinition, ToolIntent, ToolResult};

/// A runtime whose policy allows delegation (reads allowed, writes and
/// executes still denied).
fn delegating_runtime(
    config: OrchestratorConfig,
    model: Arc<dyn ModelAdapter>,
) -> OrchestratorRuntime {
    let policy = PolicyEngine::new(ToolPolicy {
        allow_read: true,
        allow_write: false,
        allow_execute: false,
        allow_delegate: true,
        max_delegate_depth: config.max_subagent_depth,
        max_parallel_delegates: config.max_parallel_subagents,
        ..ToolPolicy::default()
    });
    OrchestratorRuntime::with_policy(config, model, policy)
}

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

async fn next_update(rx: &mut mpsc::UnboundedReceiver<OutboundEvent>) -> SessionUpdate {
    match rx.recv().await.expect("outbound event queued") {
        OutboundEvent::Update { update, .. } => *update,
        other => panic!("expected update event, got {other:?}"),
    }
}

fn delegate_intent(arguments: serde_json::Value) -> ToolIntent {
    ToolIntent::new("tc-1", "delegate_task", arguments)
}

fn handoff_output(summary: &str) -> String {
    json!({
        "schema_version": 1,
        "summary": summary,
        "findings": [],
        "citations": {"files": [], "tools": []},
        "unresolved": [],
        "recommended_actions": []
    })
    .to_string()
}

fn bridge() -> ClientBridge {
    let (tx, _rx) = mpsc::unbounded_channel();
    ClientBridge::new_for_test(Duration::from_secs(5), tx)
}

/// Test harness wiring a manager over fresh stores plus the shared
/// per-turn budget tracker.
struct ManagerHarness {
    manager: Arc<SubagentManager>,
    tasks: Arc<Mutex<TaskGraph>>,
    _memory: Arc<Mutex<MemoryStore>>,
    budget: Arc<Mutex<BudgetTracker>>,
    children: Arc<ChildRegistry>,
}

fn manager_harness(config: OrchestratorConfig, model: Arc<dyn ModelAdapter>) -> ManagerHarness {
    let tools = Arc::new(Mutex::new(ToolRegistry::new()));
    let tasks = Arc::new(Mutex::new(TaskGraph::new()));
    let memory = Arc::new(Mutex::new(MemoryStore::new(config.memory_limit_bytes)));
    let budget = Arc::new(Mutex::new(BudgetTracker::new(&config)));
    let children = Arc::new(ChildRegistry::default());
    let manager = Arc::new(SubagentManager::new(
        config,
        Arc::new(ModelRegistry::single(model)),
        tools,
        SubagentState::new(tasks.clone(), memory.clone(), budget.clone(), children.clone()),
        SubagentObservability::new(
            Arc::new(RwLock::new(None)),
            Arc::new(Mutex::new(OrchestratorMetrics::new())),
            Arc::new(Mutex::new(DecisionLog::default())),
        ),
    ));
    ManagerHarness { manager, tasks, _memory: memory, budget, children }
}
async fn wait_until(condition: impl Fn() -> bool) {
    for _ in 0..10_000 {
        if condition() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("condition never satisfied");
}

/// Model that blocks until the cancellation watch flips, then reports
/// cancellation; proves parent cancellation reaches a running child.
struct CancelAwaitingModel {
    calls: Arc<Mutex<usize>>,
}

impl ModelAdapter for CancelAwaitingModel {
    fn complete(
        &self,
        _request: ModelRequest,
        mut cancel: watch::Receiver<bool>,
    ) -> ModelFuture<Result<ModelResponse, ModelError>> {
        let calls = self.calls.clone();
        Box::pin(async move {
            *calls.lock().expect("calls poisoned") += 1;
            if *cancel.borrow() {
                return Err(ModelError::Cancelled);
            }
            let _ = cancel.changed().await;
            Err(ModelError::Cancelled)
        })
    }
}
/// Model probe counting concurrently active completions.
struct ConcurrencyProbe {
    active: Arc<Mutex<(usize, usize)>>,
}

impl ModelAdapter for ConcurrencyProbe {
    fn complete(
        &self,
        _request: ModelRequest,
        _cancel: watch::Receiver<bool>,
    ) -> ModelFuture<Result<ModelResponse, ModelError>> {
        let active = self.active.clone();
        Box::pin(async move {
            {
                let mut state = active.lock().expect("probe poisoned");
                state.0 += 1;
                state.1 = state.1.max(state.0);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            active.lock().expect("probe poisoned").0 -= 1;
            Ok(ModelResponse::new().text(handoff_output("done")).completed())
        })
    }
}

// ── Subagent model selection (phase 11) ───────────────────────────────

/// A delegating runtime sharing a registry of two fake adapters:
/// `default` (parent) and `strong` (selectable by delegation).
fn model_registry_runtime(
    config: OrchestratorConfig,
    parent: Arc<dyn ModelAdapter>,
    strong: Arc<dyn ModelAdapter>,
) -> OrchestratorRuntime {
    let mut registry = ModelRegistry::single(parent);
    registry
        .register_with_hints(
            "strong",
            strong,
            Some("Strong Model".to_string()),
            vec!["tools".to_string()],
        )
        .expect("registers strong model");
    let policy = PolicyEngine::new(ToolPolicy {
        allow_read: true,
        allow_write: false,
        allow_execute: false,
        allow_delegate: true,
        max_delegate_depth: config.max_subagent_depth,
        max_parallel_delegates: config.max_parallel_subagents,
        ..ToolPolicy::default()
    });
    OrchestratorRuntime::with_model_registry(config, registry, policy).expect("runtime")
}

mod delegate_tests;
mod lifecycle_tests;
mod memory_tests;
mod model_tests;
mod quarantine_tests;
mod roles_tests;
