//! Deterministic replay harness (feature `test-utils`).
//!
//! A [`ReplayScript`] fixes the model responses in order, the canned tool
//! results, the expected [`OrchestratorEvent`] sequence, and the expected
//! final task graph.  [`run_replay`] executes the script through
//! [`OrchestratorRuntime`] with only [`FakeModel`] and [`FakeTool`] instances
//! registered — no real model provider, client bridge, or tool is ever
//! touched — and asserts the recorded events and task graph match the script.
//! This makes turns inspectable and regression-testable without network,
//! editor UI, or nondeterminism.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use ee_acp_agent_server::server::OutboundEvent;
use ee_acp_agent_server::{ClientBridge, PromptContext, PromptResult, UpdateSink};
use ee_agent_protocol::{ContentBlock, SessionId, TextContent};
use serde_json::Value;
use tokio::sync::{mpsc, watch};

use crate::config::OrchestratorConfig;
use crate::error::OrchestratorError;
use crate::events::{EventRecorder, OrchestratorEvent};
use crate::model::{ModelRequest, ModelResponse};
use crate::policy::{PolicyEngine, ToolPolicy};
use crate::runtime::OrchestratorRuntime;
use crate::tasks::{TaskGraph, TaskId, TaskNode, TaskStatus};
use crate::test_support::{
    FakeModel, FakeTool, delegate_then_answer_script, endless_tool_loop_script,
    simple_answer_script, tool_then_answer_script,
};
use crate::tools::{SideEffectClass, ToolDefinition, ToolResult};

/// One deterministic replay scenario.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ReplayScript {
    /// The user prompt text.
    pub prompt_text: String,
    /// The ACP session id the turn runs under.
    pub session_id: String,
    /// Model responses consumed in order by the fake model.
    pub model_responses: Vec<ModelResponse>,
    /// Canned results keyed by tool name; a fake tool is registered per name.
    pub tool_responses: Vec<(String, ToolResult)>,
    /// Whether delegation is policy-allowed (registers `delegate_task`).
    pub allow_delegate: bool,
    /// Config override; `None` uses [`OrchestratorConfig::default`].
    pub config: Option<OrchestratorConfig>,
    /// The exact expected event sequence.
    pub expected_events: Vec<OrchestratorEvent>,
    /// The expected final task graph (stable id order).
    pub expected_tasks: Vec<TaskNode>,
}

impl ReplayScript {
    /// Creates an empty script for `prompt_text` under `session_id`.
    #[must_use]
    pub fn new(prompt_text: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            prompt_text: prompt_text.into(),
            session_id: session_id.into(),
            model_responses: Vec::new(),
            tool_responses: Vec::new(),
            allow_delegate: false,
            config: None,
            expected_events: Vec::new(),
            expected_tasks: Vec::new(),
        }
    }

    /// Sets the scripted model responses.
    #[must_use]
    pub fn with_model_responses(mut self, responses: Vec<ModelResponse>) -> Self {
        self.model_responses = responses;
        self
    }

    /// Adds one canned tool result under `name`.
    #[must_use]
    pub fn with_tool_response(mut self, name: impl Into<String>, result: ToolResult) -> Self {
        self.tool_responses.push((name.into(), result));
        self
    }

    /// Enables delegation in the replay policy.
    #[must_use]
    pub fn with_allow_delegate(mut self) -> Self {
        self.allow_delegate = true;
        self
    }

    /// Overrides the config the replay runs under.
    #[must_use]
    pub fn with_config(mut self, config: OrchestratorConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Sets the exact expected event sequence.
    #[must_use]
    pub fn with_expected_events(mut self, events: Vec<OrchestratorEvent>) -> Self {
        self.expected_events = events;
        self
    }

    /// Sets the expected final task graph.
    #[must_use]
    pub fn with_expected_tasks(mut self, tasks: Vec<TaskNode>) -> Self {
        self.expected_tasks = tasks;
        self
    }
}

/// Everything a replay produced, for further assertions.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ReplayOutcome {
    /// Recorded events in order; equals the script's expected events.
    pub events: Vec<OrchestratorEvent>,
    /// Final task graph.
    pub tasks: TaskGraph,
    /// The turn's prompt result (or error).
    pub prompt_result: Result<PromptResult, OrchestratorError>,
    /// Every request the fake model received, proving the fake was used.
    pub model_requests: Vec<ModelRequest>,
    /// Tool arguments recorded per fake tool, proving the fakes were used.
    pub tool_calls: BTreeMap<String, Vec<Value>>,
    /// Every agent → client request the run emitted (must be empty: replay
    /// never touches the client bridge).
    pub client_requests: Vec<String>,
}

impl ReplayOutcome {
    /// Asserts the recorded events and final task graph match the script,
    /// panicking with a diff on mismatch.
    pub fn assert_matches(&self, script: &ReplayScript) {
        assert_eq!(
            self.events, script.expected_events,
            "event sequence mismatch:\nexpected: {:#?}\nactual: {:#?}",
            script.expected_events, self.events
        );
        assert_eq!(
            self.tasks.list(),
            script.expected_tasks,
            "final task graph mismatch:\nexpected: {:#?}\nactual: {:#?}",
            script.expected_tasks,
            self.tasks.list()
        );
    }
}

/// Runs `script` against the runtime using only fake model and fake tools,
/// asserting the expected event order and final task graph.
pub async fn run_replay(script: ReplayScript) -> ReplayOutcome {
    let config = script.config.clone().unwrap_or_default();
    let model = Arc::new(FakeModel::new(script.model_responses.clone()));
    let runtime = if script.allow_delegate {
        let policy =
            PolicyEngine::new(ToolPolicy { allow_delegate: true, ..ToolPolicy::default() });
        OrchestratorRuntime::with_policy(config.clone(), model.clone(), policy)
    } else {
        OrchestratorRuntime::new(config.clone(), model.clone())
    };
    let mut tool_fakes: Vec<(String, Arc<FakeTool>)> = Vec::new();
    for (name, result) in &script.tool_responses {
        let fake = Arc::new(FakeTool::new(
            ToolDefinition::new(name, "replay tool").side_effect_class(SideEffectClass::Read),
            result.clone(),
        ));
        runtime.register_tool(fake.clone()).expect("registers replay tool");
        tool_fakes.push((name.clone(), fake));
    }

    let session = SessionId::new(script.session_id.clone());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let sink = UpdateSink::new_for_test(session.clone(), tx.clone());
    let client = ClientBridge::new_for_test(Duration::from_secs(5), tx);
    let ctx = PromptContext::new(
        session,
        vec![ContentBlock::Text(TextContent::new(script.prompt_text.clone()))],
    );
    let events = EventRecorder::new();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let prompt_result =
        runtime.run_turn_recording(ctx, sink, client, cancel_rx, events.clone()).await;

    let mut client_requests = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let OutboundEvent::ClientRequest { frame } = event {
            client_requests
                .push(serde_json::to_string(&frame).unwrap_or_else(|_| "<frame>".into()));
        }
    }
    let tool_calls =
        tool_fakes.into_iter().map(|(name, fake)| (name, fake.call_arguments())).collect();

    let outcome = ReplayOutcome {
        events: events.events(),
        tasks: runtime.tasks(),
        prompt_result,
        model_requests: model.requests(),
        tool_calls,
        client_requests,
    };
    outcome.assert_matches(&script);
    outcome
}

// ── Fixtures ──────────────────────────────────────────────────────────────

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

fn running_root(title: &str) -> TaskNode {
    TaskNode {
        id: TaskId::new("task-1"),
        title: title.into(),
        description: title.into(),
        parent: None,
        dependencies: Vec::new(),
        status: TaskStatus::Running,
        assigned_worker: None,
        result_summary: None,
        model_id: None,
    }
}

fn completed_child() -> TaskNode {
    TaskNode {
        id: TaskId::new("task-2"),
        title: "worker".into(),
        description: "do the work".into(),
        parent: Some(TaskId::new("task-1")),
        dependencies: Vec::new(),
        status: TaskStatus::Completed,
        assigned_worker: None,
        result_summary: None,
        model_id: Some("default".into()),
    }
}

/// Fixture: one completed assistant answer, no tools.
#[must_use]
pub fn simple_answer_replay() -> ReplayScript {
    ReplayScript::new("hello world", "s-1")
        .with_model_responses(simple_answer_script())
        .with_expected_events(vec![
            OrchestratorEvent::TurnStarted { session_id: "s-1".into(), task_id: "task-1".into() },
            budget_event(1, 1, 0, 0, 0),
            OrchestratorEvent::ModelRequested { iteration: 1 },
            OrchestratorEvent::ModelResponded { iteration: 1 },
            budget_event(1, 1, 0, 0, 11), // "hello world"
            OrchestratorEvent::TurnStopped { stop_reason: "end_turn".into() },
        ])
        .with_expected_tasks(vec![running_root("hello world")])
}

/// Fixture: one `read_file` tool intent, then a completed answer.
#[must_use]
pub fn tool_then_answer_replay() -> ReplayScript {
    ReplayScript::new("read a file", "s-1")
        .with_model_responses(tool_then_answer_script())
        .with_tool_response("read_file", ToolResult::success("file contents"))
        .with_expected_events(vec![
            OrchestratorEvent::TurnStarted { session_id: "s-1".into(), task_id: "task-1".into() },
            budget_event(1, 1, 0, 0, 0),
            OrchestratorEvent::ModelRequested { iteration: 1 },
            OrchestratorEvent::ModelResponded { iteration: 1 },
            budget_event(1, 1, 0, 0, 0), // tool intent only
            OrchestratorEvent::ToolStarted {
                tool_call_id: "tc-1".into(),
                tool_name: "read_file".into(),
            },
            budget_event(1, 1, 1, 0, 0), // tool reservation
            OrchestratorEvent::ToolFinished {
                tool_call_id: "tc-1".into(),
                tool_name: "read_file".into(),
                success: true,
            },
            budget_event(2, 2, 1, 0, 0),
            OrchestratorEvent::ModelRequested { iteration: 2 },
            OrchestratorEvent::ModelResponded { iteration: 2 },
            budget_event(2, 2, 1, 0, 7), // "read it"
            OrchestratorEvent::TurnStopped { stop_reason: "end_turn".into() },
        ])
        .with_expected_tasks(vec![running_root("read a file")])
}

/// Fixture: a tool request denied by policy, followed by a completed answer.
/// The fake result models an editor/proxy denial without touching host state.
#[must_use]
pub fn denied_tool_replay() -> ReplayScript {
    let mut script = tool_then_answer_replay();
    script.tool_responses[0].1 = ToolResult::failure(
        crate::tools::ToolErrorKind::PermissionDenied,
        "fixture policy denied tool execution",
    );
    for event in &mut script.expected_events {
        if let OrchestratorEvent::ToolFinished { success, .. } = event {
            *success = false;
        }
    }
    script
}

/// Fixture: one `delegate_task` intent; the child consumes the fixture answer
/// and the parent finishes with one more response.
#[must_use]
pub fn delegate_then_answer_replay() -> ReplayScript {
    let mut responses = delegate_then_answer_script();
    responses.push(ModelResponse::new().text("parent answer").completed());
    ReplayScript::new("delegate work", "s-1")
        .with_allow_delegate()
        .with_model_responses(responses)
        .with_expected_events(vec![
            OrchestratorEvent::TurnStarted { session_id: "s-1".into(), task_id: "task-1".into() },
            budget_event(1, 1, 0, 0, 0),
            OrchestratorEvent::ModelRequested { iteration: 1 },
            OrchestratorEvent::ModelResponded { iteration: 1 },
            budget_event(1, 1, 0, 0, 0),
            OrchestratorEvent::ToolStarted {
                tool_call_id: "tc-1".into(),
                tool_name: "delegate_task".into(),
            },
            budget_event(1, 1, 1, 0, 0), // tool reservation
            budget_event(1, 1, 1, 1, 0), // subagent reservation
            OrchestratorEvent::SubagentStarted {
                subagent_id: "task-2".into(),
                model_id: Some("default".into()),
            },
            OrchestratorEvent::TurnStarted {
                session_id: "subagent".into(),
                task_id: "task-2".into(),
            },
            budget_event(1, 1, 0, 0, 0),
            OrchestratorEvent::ModelRequested { iteration: 1 },
            OrchestratorEvent::ModelResponded { iteration: 1 },
            budget_event(1, 1, 0, 0, 9), // "delegated"
            OrchestratorEvent::TurnStopped { stop_reason: "end_turn".into() },
            OrchestratorEvent::SubagentFinished { subagent_id: "task-2".into(), success: true },
            OrchestratorEvent::ToolFinished {
                tool_call_id: "tc-1".into(),
                tool_name: "delegate_task".into(),
                success: true,
            },
            budget_event(2, 2, 1, 1, 0),
            OrchestratorEvent::ModelRequested { iteration: 2 },
            OrchestratorEvent::ModelResponded { iteration: 2 },
            budget_event(2, 2, 1, 1, 13), // "parent answer"
            OrchestratorEvent::TurnStopped { stop_reason: "end_turn".into() },
        ])
        .with_expected_tasks(vec![running_root("delegate work"), completed_child()])
}

/// Fixture: an unbounded tool loop that the iteration budget must terminate.
/// `calls` is the scripted response count; at least 3 responses are needed so
/// the loop runs all three allowed iterations before the fourth is denied.
#[must_use]
pub fn endless_loop_replay(calls: usize) -> ReplayScript {
    assert!(calls >= 3, "endless replay needs at least 3 scripted responses");
    let mut events =
        vec![OrchestratorEvent::TurnStarted { session_id: "s-1".into(), task_id: "task-1".into() }];
    for iteration in 1..=3usize {
        events.push(budget_event(iteration, iteration, iteration - 1, 0, 0));
        events.push(OrchestratorEvent::ModelRequested { iteration });
        events.push(OrchestratorEvent::ModelResponded { iteration });
        events.push(budget_event(iteration, iteration, iteration - 1, 0, 0));
        events.push(OrchestratorEvent::ToolStarted {
            tool_call_id: format!("tc-{}", iteration - 1),
            tool_name: "read_file".into(),
        });
        events.push(budget_event(iteration, iteration, iteration, 0, 0));
        events.push(OrchestratorEvent::ToolFinished {
            tool_call_id: format!("tc-{}", iteration - 1),
            tool_name: "read_file".into(),
            success: true,
        });
    }
    events.push(OrchestratorEvent::TurnStopped { stop_reason: "budget_exceeded".into() });
    ReplayScript::new("loop forever", "s-1")
        .with_config(OrchestratorConfig { max_loop_iterations: 3, ..OrchestratorConfig::default() })
        .with_model_responses(endless_tool_loop_script(calls))
        .with_tool_response("read_file", ToolResult::success("file contents"))
        .with_expected_events(events)
        .with_expected_tasks(vec![running_root("loop forever")])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn replay_simple_answer_uses_fake_model_and_no_client() {
        let outcome = run_replay(simple_answer_replay()).await;
        assert_eq!(outcome.model_requests.len(), 1, "fake model must serve the turn");
        assert!(outcome.prompt_result.is_ok());
        assert!(outcome.tool_calls.is_empty());
        assert!(
            outcome.client_requests.is_empty(),
            "replay must never touch the client bridge: {:?}",
            outcome.client_requests
        );
    }

    #[tokio::test]
    async fn replay_tool_then_answer_uses_fake_tool_and_no_client() {
        let outcome = run_replay(tool_then_answer_replay()).await;
        assert_eq!(outcome.model_requests.len(), 2);
        assert_eq!(
            outcome.tool_calls.get("read_file").map(Vec::len),
            Some(1),
            "fake tool must serve the read intent"
        );
        assert!(outcome.prompt_result.is_ok());
        assert!(
            outcome.client_requests.is_empty(),
            "replay must never touch the client bridge: {:?}",
            outcome.client_requests
        );
    }

    #[tokio::test]
    async fn replay_delegate_then_answer_uses_fake_subagent_flow() {
        let outcome = run_replay(delegate_then_answer_replay()).await;
        assert_eq!(outcome.model_requests.len(), 3, "child consumes one response, parent two");
        assert!(outcome.prompt_result.is_ok());
        assert!(outcome.client_requests.is_empty());
        // The child's summary fact was merged into the parent store.
        let tasks = outcome.tasks;
        assert_eq!(tasks.list().len(), 2);
    }

    #[tokio::test]
    async fn replay_endless_loop_stops_via_iteration_budget() {
        let outcome = run_replay(endless_loop_replay(6)).await;
        assert!(
            matches!(outcome.prompt_result, Err(OrchestratorError::BudgetExceeded(_))),
            "loop must terminate via the iteration budget"
        );
        assert_eq!(outcome.model_requests.len(), 3, "exactly the allowed iterations ran");
        assert_eq!(
            outcome.tool_calls.get("read_file").map(Vec::len),
            Some(3),
            "every allowed iteration executed its tool"
        );
        assert!(outcome.client_requests.is_empty());
    }

    #[tokio::test]
    async fn every_fixture_asserts_stable_event_order() {
        for script in [
            simple_answer_replay(),
            tool_then_answer_replay(),
            delegate_then_answer_replay(),
            endless_loop_replay(6),
        ] {
            let outcome = run_replay(script.clone()).await;
            assert_eq!(outcome.events, script.expected_events);
            assert_eq!(outcome.tasks.list(), script.expected_tasks);
        }
    }
}
