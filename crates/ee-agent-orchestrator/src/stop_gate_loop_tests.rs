//! Loop-level stop-gate and session-log tests.
//!
//! Split from `loop_engine.rs` tests so the large engine file stays stable.

use std::sync::{Arc, Mutex};

use ee_acp_agent_server::{ClientBridge, PromptContext, UpdateSink};
use ee_agent_protocol::{ContentBlock, SessionId, StopReason, TextContent};
use serde_json::json;
use tokio::sync::{mpsc, watch};

use crate::budget::BudgetTracker;
use crate::config::OrchestratorConfig;
use crate::error::OrchestratorError;
use crate::events::{EventRecorder, OrchestratorEvent};
use crate::loop_engine::{LoopEngine, LoopOptions};
use crate::model::{ModelAdapter, ModelMessage, ModelResponse};
use crate::tasks::{TaskId, TaskNode};
use crate::test_support::{FakeModel, FakeTool, tool_then_answer_script};
use crate::tools::{
    ServerTool, SideEffectClass, ToolDefinition, ToolIntent, ToolRegistry, ToolResult,
};

fn prompt(text: &str) -> PromptContext {
    PromptContext::new(SessionId::new("s-1"), vec![ContentBlock::Text(TextContent::new(text))])
}

fn task() -> TaskNode {
    TaskNode::new(TaskId::new("task-1"), "hello world", "hello world")
}

fn plumbing()
-> (UpdateSink, ClientBridge, mpsc::UnboundedReceiver<ee_acp_agent_server::server::OutboundEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (
        UpdateSink::new_for_test(SessionId::new("s-1"), tx.clone()),
        ClientBridge::new_for_test(std::time::Duration::from_secs(5), tx),
        rx,
    )
}

fn engine_with_options(
    config: OrchestratorConfig,
    model: Arc<dyn ModelAdapter>,
    tools: Vec<Arc<dyn ServerTool>>,
    policy: crate::policy::PolicyEngine,
    events: EventRecorder,
    options: LoopOptions,
) -> LoopEngine {
    let registry = Arc::new(Mutex::new(ToolRegistry::new()));
    for tool in tools {
        registry.lock().expect("tool registry poisoned").register(tool).expect("registers tool");
    }
    let budget = Arc::new(Mutex::new(BudgetTracker::new(&config)));
    LoopEngine::new(config, model, registry, budget, policy, events, options)
}

fn echo_tool() -> Arc<dyn ServerTool> {
    Arc::new(FakeTool::new(
        ToolDefinition::new("echo", "echoes its arguments")
            .side_effect_class(SideEffectClass::Read),
        ToolResult::success("echoed"),
    ))
}

fn write_tool() -> Arc<dyn ServerTool> {
    Arc::new(FakeTool::new(
        ToolDefinition::new("write_file", "writes").side_effect_class(SideEffectClass::Write),
        ToolResult::success("written"),
    ))
}

fn read_tool(name: &str) -> Arc<dyn ServerTool> {
    Arc::new(FakeTool::new(
        ToolDefinition::new(name, "reads").side_effect_class(SideEffectClass::Read),
        ToolResult::success("ok"),
    ))
}

fn stop_gate_policy() -> crate::policy::PolicyEngine {
    crate::policy::PolicyEngine::new(crate::policy::ToolPolicy {
        allow_write: true,
        allow_execute: true,
        ..crate::policy::ToolPolicy::default()
    })
}

#[tokio::test]
async fn stop_gate_repairs_once_then_blocks_unverified_changed_turn() {
    let model = FakeModel::new(vec![
        ModelResponse::new().tool_intents(vec![ToolIntent::new(
            "tc-1",
            "write_file",
            json!({"path": "/tmp/x.rs"}),
        )]),
        ModelResponse::new().text("done").completed(),
        ModelResponse::new().text("done again").completed(),
    ]);
    let (sink, client, _rx) = plumbing();
    let events = EventRecorder::new();
    let execution_log = Arc::new(Mutex::new(Vec::new()));
    let engine = engine_with_options(
        OrchestratorConfig::default(),
        Arc::new(model.clone()),
        vec![write_tool()],
        stop_gate_policy(),
        events.clone(),
        LoopOptions {
            execution_log: Some(execution_log),
            stop_gate: Some(crate::stop_gate::StopGateConfig::default()),
            ..LoopOptions::default()
        },
    );

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let result = engine
        .run(prompt("hello"), sink, client, cancel_rx, task(), None)
        .await
        .expect("turn runs to a gated stop");
    assert_eq!(result.stop_reason, StopReason::Refusal);
    assert_eq!(model.call_count(), 3, "one tool round plus one bounded repair");
    assert!(model.requests()[2].transcript.iter().any(|message| {
        message.metadata.get("ee_prompt").map(String::as_str) == Some("stop_gate_repair")
    }));
    assert!(events.events().iter().any(|event| matches!(
        event,
        OrchestratorEvent::StopGateEvaluated { changed_file_count: 1, .. }
    )));
}

#[tokio::test]
async fn stop_gate_allows_verified_changed_turn() {
    let intent = |id: &str, name: &str| {
        ModelResponse::new().tool_intents(vec![ToolIntent::new(
            id,
            name,
            json!({"path": "/tmp/x.rs"}),
        )])
    };
    let model = FakeModel::new(vec![
        intent("tc-1", "write_file"),
        intent("tc-2", "ee_diagnostics"),
        intent("tc-3", "ee_diff"),
        intent("tc-4", "cargo_test"),
        ModelResponse::new().text("done").completed(),
    ]);
    let (sink, client, _rx) = plumbing();
    let events = EventRecorder::new();
    let engine = engine_with_options(
        OrchestratorConfig::default(),
        Arc::new(model.clone()),
        vec![
            write_tool(),
            read_tool("ee_diagnostics"),
            read_tool("ee_diff"),
            Arc::new(FakeTool::new(
                ToolDefinition::new("cargo_test", "tests")
                    .side_effect_class(SideEffectClass::Execute),
                ToolResult::success("ok"),
            )),
        ],
        stop_gate_policy(),
        events,
        LoopOptions {
            execution_log: Some(Arc::new(Mutex::new(Vec::new()))),
            stop_gate: Some(crate::stop_gate::StopGateConfig::default()),
            ..LoopOptions::default()
        },
    );

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let result = engine
        .run(prompt("hello"), sink, client, cancel_rx, task(), None)
        .await
        .expect("verified turn completes");
    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(model.call_count(), 5);
}

#[tokio::test]
async fn stop_veto_denies_an_allowed_stop() {
    let intent = |id: &str, name: &str| {
        ModelResponse::new().tool_intents(vec![ToolIntent::new(
            id,
            name,
            json!({"path": "/tmp/x.rs"}),
        )])
    };
    let model = FakeModel::new(vec![
        intent("tc-1", "write_file"),
        intent("tc-2", "ee_diagnostics"),
        intent("tc-3", "ee_diff"),
        intent("tc-4", "cargo_test"),
        ModelResponse::new().text("done").completed(),
    ]);
    let (sink, client, _rx) = plumbing();
    let engine = engine_with_options(
        OrchestratorConfig::default(),
        Arc::new(model.clone()),
        vec![
            write_tool(),
            read_tool("ee_diagnostics"),
            read_tool("ee_diff"),
            Arc::new(FakeTool::new(
                ToolDefinition::new("cargo_test", "tests")
                    .side_effect_class(SideEffectClass::Execute),
                ToolResult::success("ok"),
            )),
        ],
        stop_gate_policy(),
        EventRecorder::new(),
        LoopOptions {
            execution_log: Some(Arc::new(Mutex::new(Vec::new()))),
            stop_gate: Some(crate::stop_gate::StopGateConfig::default()),
            stop_veto: Some(crate::stop_gate::StopVeto::new(|_| true)),
            ..LoopOptions::default()
        },
    );

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let result = engine
        .run(prompt("hello"), sink, client, cancel_rx, task(), None)
        .await
        .expect("vetoed stop is a refusal, not an error");
    assert_eq!(result.stop_reason, StopReason::Refusal);
}

#[tokio::test]
async fn session_log_write_failure_prevents_model_call() {
    #[derive(Debug)]
    struct FailingSink;
    impl crate::session_log::SessionLogSink for FailingSink {
        fn append(
            &self,
            _session_id: &str,
            _record: &crate::session_log::SessionLogRecord,
        ) -> Result<(), OrchestratorError> {
            Err(OrchestratorError::Serialization("disk full".into()))
        }
        fn records(
            &self,
            _session_id: &str,
        ) -> Result<Vec<crate::session_log::SessionLogRecord>, OrchestratorError> {
            Ok(Vec::new())
        }
        fn put_body(
            &self,
            _session_id: &str,
            _content_hash: &str,
            _body: &[u8],
        ) -> Result<(), OrchestratorError> {
            Ok(())
        }
        fn body(
            &self,
            _session_id: &str,
            _content_hash: &str,
        ) -> Result<Option<Vec<u8>>, OrchestratorError> {
            Ok(None)
        }
    }

    let log = crate::session_log::SessionLog::with_sink(
        "s-1",
        Arc::new(FailingSink),
        crate::session_log::SessionLogConfig::default(),
    );
    let model = FakeModel::new(vec![ModelResponse::new().text("never").completed()]);
    let (sink, client, _rx) = plumbing();
    let engine = engine_with_options(
        OrchestratorConfig::default(),
        Arc::new(model.clone()),
        vec![echo_tool()],
        crate::policy::PolicyEngine::default(),
        EventRecorder::new(),
        LoopOptions { session_log: Some(Arc::new(log)), ..LoopOptions::default() },
    );

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let error = engine
        .run(prompt("hello"), sink, client, cancel_rx, task(), None)
        .await
        .expect_err("log failure fails the turn closed");
    assert!(error.to_string().contains("disk full"));
    assert_eq!(model.call_count(), 0, "model never sees an unlogged request");
}

#[tokio::test]
async fn every_model_request_is_reconstructable_from_the_session_log() {
    let model = FakeModel::new(tool_then_answer_script());
    let (sink, client, _rx) = plumbing();
    let log = Arc::new(crate::session_log::SessionLog::in_memory("s-1"));
    let engine = engine_with_options(
        OrchestratorConfig::default(),
        Arc::new(model.clone()),
        vec![read_tool("read_file")],
        crate::policy::PolicyEngine::default(),
        EventRecorder::new(),
        LoopOptions { session_log: Some(log.clone()), ..LoopOptions::default() },
    );

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    engine
        .run(prompt("hello"), sink, client, cancel_rx, task(), None)
        .await
        .expect("turn completes");

    let records = log.records().expect("records");
    let replayed: std::collections::HashMap<String, ModelMessage> =
        log.replay_messages().expect("replays").into_iter().collect();
    let requests: Vec<_> = records
        .into_iter()
        .filter_map(|record| match record {
            crate::session_log::SessionLogRecord::ModelRequest(request) => Some(request),
            _ => None,
        })
        .collect();
    assert_eq!(requests.len(), model.call_count());
    for request in &requests {
        let messages: Vec<ModelMessage> = request
            .message_ids
            .iter()
            .map(|id| replayed.get(id).cloned().expect("message reconstructable"))
            .collect();
        assert_eq!(messages.len(), request.message_ids.len());
    }
    let first: Vec<ModelMessage> = requests[0]
        .message_ids
        .iter()
        .map(|id| replayed.get(id).cloned().expect("message reconstructable"))
        .collect();
    assert_eq!(crate::session_log::projection_hash(&first), requests[0].projection_hash);
}
