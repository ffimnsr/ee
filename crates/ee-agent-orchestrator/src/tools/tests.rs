use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ee_agent_acp_server::server::OutboundEvent;
use ee_agent_acp_server::{ClientBridge, UpdateSink};
use ee_agent_protocol::{
    RawJsonRpcMessage, RequestId, Response, SessionId, SessionUpdate, ToolCallStatus,
};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use super::*;
use crate::budget::BudgetTracker;
use crate::config::OrchestratorConfig;
use crate::error::OrchestratorError;
use crate::events::{EventRecorder, OrchestratorEvent};
use crate::policy::PolicyEngine;
use crate::tasks::{TaskId, TaskNode};
use crate::test_support::FakeTool;
use crate::tools::registry::{BridgeCall, ClientBridgeTool};
use crate::workspace_scope::WorkspaceScope;
use ee_agent_protocol::{ContentBlock, ToolCallContent};

fn plumbing() -> (UpdateSink, ClientBridge, mpsc::UnboundedReceiver<OutboundEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (
        UpdateSink::new_for_test(SessionId::new("s-1"), tx.clone()),
        ClientBridge::new_for_test(Duration::from_secs(5), tx),
        rx,
    )
}

fn harness(
    config: OrchestratorConfig,
) -> (Arc<Mutex<ToolRegistry>>, Arc<Mutex<BudgetTracker>>, ToolExecutor) {
    let tools = Arc::new(Mutex::new(ToolRegistry::new()));
    let budget = Arc::new(Mutex::new(BudgetTracker::new(&config)));
    let executor = ToolExecutor::new(
        config,
        tools.clone(),
        budget.clone(),
        PolicyEngine::default(),
        0,
        EventRecorder::new(),
    );
    (tools, budget, executor)
}

fn task_fixture() -> TaskNode {
    TaskNode::new(TaskId::new("task-1"), "t", "d")
}

/// Drains the next `session/update` notification from the outbound
/// channel, panicking on any other event.
async fn next_update(rx: &mut mpsc::UnboundedReceiver<OutboundEvent>) -> SessionUpdate {
    match rx.recv().await.expect("outbound event queued") {
        OutboundEvent::Update { update, .. } => *update,
        other => panic!("expected update event, got {other:?}"),
    }
}

async fn next_client_request(rx: &mut mpsc::UnboundedReceiver<OutboundEvent>) -> RawJsonRpcMessage {
    match rx.recv().await.expect("client request queued") {
        OutboundEvent::ClientRequest { frame } => frame,
        other => panic!("expected client request, got {other:?}"),
    }
}

/// A tool that returns after a fixed delay, for timeout tests.
struct SlowTool {
    definition: ToolDefinition,
    delay: Duration,
}

impl ServerTool for SlowTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn execute(
        &self,
        _arguments: Value,
        _client: ClientBridge,
        _cancel: watch::Receiver<bool>,
        _context: ToolCallContext,
    ) -> ToolFuture<ToolResult> {
        let delay = self.delay;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            ToolResult::success("slow done")
        })
    }
}

/// A tool that blocks until cancellation, for cancellation tests.
struct CancelObservingTool {
    definition: ToolDefinition,
}

impl ServerTool for CancelObservingTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn execute(
        &self,
        _arguments: Value,
        _client: ClientBridge,
        mut cancel: watch::Receiver<bool>,
        _context: ToolCallContext,
    ) -> ToolFuture<ToolResult> {
        Box::pin(async move {
            let _ = cancel.changed().await;
            ToolResult::failure(ToolErrorKind::Cancelled, "stopped by caller")
        })
    }
}

#[test]
fn register_and_lookup_roundtrip() {
    let mut registry = ToolRegistry::new();
    let tool = Arc::new(FakeTool::new(
        ToolDefinition::new("echo", "echoes").input_schema(serde_json::json!({ "type": "object" })),
        ToolResult::success("echo"),
    ));
    registry.register(tool.clone()).expect("registers");
    assert_eq!(registry.len(), 1);
    assert!(registry.get("echo").is_some());
    assert!(registry.get("missing").is_none());
}

#[test]
fn duplicate_tool_name_is_rejected() {
    let mut registry = ToolRegistry::new();
    let tool =
        Arc::new(FakeTool::new(ToolDefinition::new("echo", "one"), ToolResult::success("a")));
    registry.register(tool.clone()).expect("first registers");
    let second =
        Arc::new(FakeTool::new(ToolDefinition::new("echo", "two"), ToolResult::success("b")));
    let error = registry.register(second).expect_err("duplicate rejected");
    assert!(
        matches!(error, OrchestratorError::InvalidState(ref r) if r.contains("duplicate tool name: echo"))
    );
}

#[test]
fn empty_tool_name_is_rejected() {
    let mut registry = ToolRegistry::new();
    let tool =
        Arc::new(FakeTool::new(ToolDefinition::new("", "no name"), ToolResult::success("x")));
    let error = registry.register(tool).expect_err("empty name rejected");
    assert!(matches!(error, OrchestratorError::InvalidState(_)));
}

#[test]
fn definitions_are_sorted_by_name() {
    let mut registry = ToolRegistry::new();
    registry
        .register(Arc::new(FakeTool::new(
            ToolDefinition::new("zeta", "z"),
            ToolResult::success("z"),
        )))
        .expect("registers");
    registry
        .register(Arc::new(FakeTool::new(
            ToolDefinition::new("alpha", "a"),
            ToolResult::success("a"),
        )))
        .expect("registers");
    let names: Vec<String> = registry.definitions().into_iter().map(|d| d.name).collect();
    assert_eq!(names, vec!["alpha".to_string(), "zeta".to_string()]);
}

#[test]
fn tool_result_shapes() {
    let ok = ToolResult::success("done");
    assert!(ok.success);
    assert_eq!(ok.summary_text(), "done");

    let failed = ToolResult::failure(ToolErrorKind::Timeout, "slow");
    assert!(!failed.success);
    assert_eq!(failed.error_kind, Some(ToolErrorKind::Timeout));
    assert_eq!(failed.summary_text(), "timeout: slow");
}

#[test]
fn canonical_definitions_hide_bridge_read_and_write_duplicates() {
    let definition = |name: &str| ToolDefinition::new(name, "x");
    let names = |definitions: Vec<ToolDefinition>| {
        definitions.into_iter().map(|definition| definition.name).collect::<Vec<_>>()
    };
    let all = vec![
        definition("read_file"),
        definition("write_file"),
        definition("ee_read_text_file"),
        definition("ee_write_text_file"),
        definition("ee_replace_text"),
    ];
    assert_eq!(
        names(canonical_tool_definitions(all.clone(), None)),
        vec!["ee_read_text_file", "ee_write_text_file", "ee_replace_text"]
    );

    // Without the ee proxy tools, the bridge builtins stay visible.
    assert_eq!(
        names(canonical_tool_definitions(
            vec![definition("read_file"), definition("write_file")],
            None
        )),
        vec!["read_file", "write_file"]
    );

    // An explicit visible set keeps a named bridge tool (critic profiles).
    let visible = HashSet::from([String::from("read_file")]);
    assert_eq!(names(canonical_tool_definitions(all, Some(&visible))), vec!["read_file"]);
}

#[test]
fn builtin_tools_register_under_session() {
    let mut registry = ToolRegistry::new();
    registry.register_builtins(&SessionId::new("s-1")).expect("builtins register");
    let names: Vec<String> = registry.definitions().into_iter().map(|d| d.name).collect();
    assert_eq!(
        names,
        vec![
            "ask_user",
            "cargo_check",
            "create_terminal",
            "delete_memory",
            "kill_terminal",
            "load_experience",
            "load_memory",
            "load_skill",
            "read_file",
            "release_terminal",
            "save_memory",
            "terminal_output",
            "wait_for_terminal_exit",
            "write_file"
        ]
    );
    assert_eq!(registry.len(), 14);
}

#[tokio::test]
async fn todo_tool_replaces_plan_and_reports_statuses() {
    let mut registry = ToolRegistry::new();
    let plan = Arc::new(Mutex::new(crate::todo_plan::TodoPlan::new()));
    registry.register_todo_tool(plan.clone()).expect("registers");
    let tool = registry.get("todowrite").expect("todowrite registered");
    let (sink, client, _rx) = plumbing();
    drop(sink);
    let context = ToolCallContext {
        task: task_fixture(),
        transcript: Vec::new(),
        events: EventRecorder::new(),
        scope: None,
        model_id: None,
        terminal_wait: TerminalWaitLimits::default(),
        auto_approved: false,
    };

    let result = tool
        .execute(
            json!({
                "todos": [
                    { "content": "first", "status": "in_progress" },
                    { "content": "second", "status": "pending" },
                ]
            }),
            client.clone(),
            watch::channel(false).1,
            context.clone(),
        )
        .await;
    assert!(result.success, "{}", result.text_output);
    let stored = plan.lock().expect("plan").items();
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[0].content, "first");
    assert_eq!(stored[0].status, crate::todo_plan::TodoStatus::InProgress);
    assert_eq!(result.structured_output.expect("structured")["todos"][0]["status"], "in_progress");

    let invalid = tool
        .execute(
            json!({ "todos": [{ "content": "x", "status": "blocked" }] }),
            client,
            watch::channel(false).1,
            context,
        )
        .await;
    assert!(!invalid.success);
    assert_eq!(invalid.error_kind, Some(ToolErrorKind::InvalidArguments));
    assert_eq!(plan.lock().expect("plan").items().len(), 2, "failed update keeps prior plan");
}

#[tokio::test]
async fn load_skill_reads_a_scoped_manifest_and_rejects_traversal() {
    let temp = tempfile::tempdir().expect("tempdir");
    let skill_dir = temp.path().join(".agents/skills/atlas");
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: atlas\ndescription: Graph queries\n---\n\nUse atlas.\n",
    )
    .expect("write skill");
    std::fs::write(temp.path().join("outside.md"), "no").expect("outside file");
    let mut registry = ToolRegistry::new();
    registry.register_builtins(&SessionId::new("s-1")).expect("builtins register");
    let tool = registry.get("load_skill").expect("load_skill registered");
    let (sink, client, _rx) = plumbing();
    drop(sink);
    let context = ToolCallContext {
        task: task_fixture(),
        transcript: Vec::new(),
        events: EventRecorder::new(),
        scope: Some(WorkspaceScope::new(vec![temp.path().to_path_buf()], Vec::new())),
        model_id: None,
        terminal_wait: TerminalWaitLimits::default(),
        auto_approved: false,
    };

    let loaded = tool
        .execute(
            json!({ "name": "atlas" }),
            client.clone(),
            watch::channel(false).1,
            context.clone(),
        )
        .await;
    assert!(loaded.success, "{}", loaded.text_output);
    assert!(loaded.text_output.contains("Use atlas."));
    assert_eq!(loaded.structured_output.expect("structured")["scope"], "workspace");

    let traversal = tool
        .execute(json!({ "path": "../outside.md" }), client, watch::channel(false).1, context)
        .await;
    assert!(!traversal.success);
    assert_eq!(traversal.error_kind, Some(ToolErrorKind::InvalidArguments));
}

#[tokio::test]
async fn load_memory_loads_by_name_and_searches_by_tag() {
    let temp = tempfile::tempdir().expect("tempdir");
    let memory_dir = temp.path().join(".agents/memories");
    std::fs::create_dir_all(&memory_dir).expect("memory dir");
    std::fs::write(
        memory_dir.join("testing.md"),
        "---\nname: testing\ndescription: Test commands\ntags: [rust, ee-memory-tool-test]\nimportance: high\n---\n\nRun cargo test --quiet.\n",
    )
    .expect("write memory");
    std::fs::write(
        memory_dir.join("style.md"),
        "---\ndescription: Style rules\ntags: [rust, ee-memory-tool-test]\nimportance: low\n---\n\nUse rustfmt.\n",
    )
    .expect("write memory");
    let mut registry = ToolRegistry::new();
    registry.register_builtins(&SessionId::new("s-1")).expect("builtins register");
    let tool = registry.get("load_memory").expect("load_memory registered");
    let (sink, client, _rx) = plumbing();
    drop(sink);
    let context = ToolCallContext {
        task: task_fixture(),
        transcript: Vec::new(),
        events: EventRecorder::new(),
        scope: Some(WorkspaceScope::new(vec![temp.path().to_path_buf()], Vec::new())),
        model_id: None,
        terminal_wait: TerminalWaitLimits::default(),
        auto_approved: false,
    };

    let loaded = tool
        .execute(
            json!({ "name": "testing" }),
            client.clone(),
            watch::channel(false).1,
            context.clone(),
        )
        .await;
    assert!(loaded.success, "{}", loaded.text_output);
    assert!(loaded.text_output.contains("cargo test --quiet"));
    assert_eq!(loaded.structured_output.expect("structured")["importance"], "high");

    let invalid = tool
        .execute(
            json!({ "tags": "rust" }),
            client.clone(),
            watch::channel(false).1,
            context.clone(),
        )
        .await;
    assert!(!invalid.success);
    assert_eq!(invalid.error_kind, Some(ToolErrorKind::InvalidArguments));

    let searched = tool
        .execute(
            json!({ "query": "rustfmt", "tags": ["ee-memory-tool-test"], "limit": 4 }),
            client,
            watch::channel(false).1,
            context,
        )
        .await;
    assert!(searched.success, "{}", searched.text_output);
    assert!(searched.text_output.contains("Use rustfmt."));
    assert!(searched.text_output.contains("testing"), "tag match is included");
}

#[tokio::test]
async fn load_experience_loads_by_domain_and_sequence() {
    let temp = tempfile::tempdir().expect("tempdir");
    let dir = temp.path().join(".agents/experience/rust/entries/1-task");
    std::fs::create_dir_all(&dir).expect("experience dir");
    std::fs::write(
        dir.join("EXPERIENCE.md"),
        "---\nseq: 1\ndomain: rust\ntask: task\nconfidence: high\n---\n## Edits\ncomponent: tool_guidance.md\nchange: added rule\nobserved: candidate reopened outputs\nverdict: helped\n## Experience\ncue: cue\nlesson: lesson\nevidence: evidence\n",
    )
    .expect("write experience");
    let mut registry = ToolRegistry::new();
    registry.register_builtins(&SessionId::new("s-1")).expect("builtins register");
    let tool = registry.get("load_experience").expect("load_experience registered");
    let (sink, client, _rx) = plumbing();
    drop(sink);
    let context = ToolCallContext {
        task: task_fixture(),
        transcript: Vec::new(),
        events: EventRecorder::new(),
        scope: Some(WorkspaceScope::new(vec![temp.path().to_path_buf()], Vec::new())),
        model_id: None,
        terminal_wait: TerminalWaitLimits::default(),
        auto_approved: false,
    };

    let loaded = tool
        .execute(
            json!({ "domain": "rust" }),
            client.clone(),
            watch::channel(false).1,
            context.clone(),
        )
        .await;
    assert!(loaded.success, "{}", loaded.text_output);
    assert!(loaded.text_output.contains("lesson: lesson"));
    let structured = loaded.structured_output.expect("structured");
    assert_eq!(structured["seq"], 1);
    assert_eq!(structured["confidence"], "high");
    assert_eq!(structured["no_entry"], false);

    let missing =
        tool.execute(json!({}), client.clone(), watch::channel(false).1, context.clone()).await;
    assert_eq!(missing.error_kind, Some(ToolErrorKind::InvalidArguments));

    let unknown = tool
        .execute(json!({ "domain": "missing" }), client, watch::channel(false).1, context)
        .await;
    assert_eq!(unknown.error_kind, Some(ToolErrorKind::InvalidArguments));
}

#[tokio::test]
async fn save_memory_requires_confirmation_and_writes_the_entry() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut registry = ToolRegistry::new();
    registry.register_builtins(&SessionId::new("s-1")).expect("builtins register");
    let tool = registry.get("save_memory").expect("save_memory registered");
    let (sink, bridge, mut rx) = plumbing();
    drop(sink);
    let context = ToolCallContext {
        task: task_fixture(),
        transcript: Vec::new(),
        events: EventRecorder::new(),
        scope: Some(WorkspaceScope::new(vec![temp.path().to_path_buf()], Vec::new())),
        model_id: None,
        terminal_wait: TerminalWaitLimits::default(),
        auto_approved: false,
    };
    let task_bridge = bridge.clone();
    let task = tokio::spawn(async move {
        tool.execute(
            json!({
                "name": "testing",
                "content": "Run cargo test --quiet.",
                "description": "Test commands",
                "tags": ["rust"],
                "importance": "high",
            }),
            task_bridge,
            watch::channel(false).1,
            context,
        )
        .await
    });

    let RawJsonRpcMessage::Request(request) = next_client_request(&mut rx).await else {
        panic!("expected request frame");
    };
    assert_eq!(request.method.as_ref(), "elicitation/create");
    bridge.handle_response(Response::Result {
        id: RequestId::Number(1),
        result: json!({ "action": "accept" }),
    });

    let result = task.await.expect("task joins");
    assert!(result.success, "{}", result.text_output);
    let path = temp.path().join(".agents/memories/testing.md");
    let text = std::fs::read_to_string(&path).expect("memory written");
    assert!(text.contains("importance: high"));
    assert!(text.contains("Run cargo test --quiet."));
    assert_eq!(result.structured_output.expect("structured")["created"], true);
}

#[tokio::test]
async fn delete_memory_requires_confirmation_and_removes_the_entry() {
    let temp = tempfile::tempdir().expect("tempdir");
    let roots = vec![temp.path().to_path_buf()];
    crate::user_memory_write::save(
        temp.path(),
        &roots,
        crate::user_memory_write::MemorySaveRequest {
            name: "testing",
            description: Some("Test commands"),
            tags: None,
            importance: None,
            content: "Run tests.",
        },
    )
    .expect("seed memory");
    let mut registry = ToolRegistry::new();
    registry.register_builtins(&SessionId::new("s-1")).expect("builtins register");
    let tool = registry.get("delete_memory").expect("delete_memory registered");
    let (sink, bridge, mut rx) = plumbing();
    drop(sink);
    let context = ToolCallContext {
        task: task_fixture(),
        transcript: Vec::new(),
        events: EventRecorder::new(),
        scope: Some(WorkspaceScope::new(roots, Vec::new())),
        model_id: None,
        terminal_wait: TerminalWaitLimits::default(),
        auto_approved: false,
    };
    let task_bridge = bridge.clone();
    let task = tokio::spawn(async move {
        tool.execute(json!({ "name": "testing" }), task_bridge, watch::channel(false).1, context)
            .await
    });

    let RawJsonRpcMessage::Request(request) = next_client_request(&mut rx).await else {
        panic!("expected request frame");
    };
    assert_eq!(request.method.as_ref(), "elicitation/create");
    bridge.handle_response(Response::Result {
        id: RequestId::Number(1),
        result: json!({ "action": "accept" }),
    });

    let result = task.await.expect("task joins");
    assert!(result.success, "{}", result.text_output);
    assert!(!temp.path().join(".agents/memories/testing.md").exists());
    assert_eq!(result.structured_output.expect("structured")["deleted"], true);
}

#[tokio::test]
async fn delete_memory_declined_keeps_the_entry() {
    let temp = tempfile::tempdir().expect("tempdir");
    let roots = vec![temp.path().to_path_buf()];
    crate::user_memory_write::save(
        temp.path(),
        &roots,
        crate::user_memory_write::MemorySaveRequest {
            name: "testing",
            description: Some("Test commands"),
            tags: None,
            importance: None,
            content: "Run tests.",
        },
    )
    .expect("seed memory");
    let mut registry = ToolRegistry::new();
    registry.register_builtins(&SessionId::new("s-1")).expect("builtins register");
    let tool = registry.get("delete_memory").expect("delete_memory registered");
    let (sink, bridge, mut rx) = plumbing();
    drop(sink);
    let context = ToolCallContext {
        task: task_fixture(),
        transcript: Vec::new(),
        events: EventRecorder::new(),
        scope: Some(WorkspaceScope::new(roots, Vec::new())),
        model_id: None,
        terminal_wait: TerminalWaitLimits::default(),
        auto_approved: false,
    };
    let task_bridge = bridge.clone();
    let task = tokio::spawn(async move {
        tool.execute(json!({ "name": "testing" }), task_bridge, watch::channel(false).1, context)
            .await
    });

    let RawJsonRpcMessage::Request(_request) = next_client_request(&mut rx).await else {
        panic!("expected request frame");
    };
    bridge.handle_response(Response::Result {
        id: RequestId::Number(1),
        result: json!({ "action": "decline" }),
    });

    let result = task.await.expect("task joins");
    assert!(!result.success);
    assert_eq!(result.error_kind, Some(ToolErrorKind::PermissionDenied));
    assert!(temp.path().join(".agents/memories/testing.md").exists());
}

#[test]
fn register_builtins_conflicts_with_custom_tool() {
    let mut registry = ToolRegistry::new();
    registry
        .register(Arc::new(FakeTool::new(
            ToolDefinition::new("read_file", "custom"),
            ToolResult::success("custom"),
        )))
        .expect("custom read_file registers");
    let error = registry.register_builtins(&SessionId::new("s-1")).expect_err("conflict rejected");
    assert!(matches!(error, OrchestratorError::InvalidState(_)));
}

#[test]
fn validate_arguments_rejects_non_object_and_missing_required() {
    let definition = ToolDefinition::new("read_file", "reads")
        .input_schema(json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }));
    assert!(definition.validate_arguments(&json!([1, 2])).is_err());
    assert!(definition.validate_arguments(&json!({})).is_err());
    assert_eq!(
        definition.validate_arguments(&json!({})).unwrap_err(),
        "missing required argument: path"
    );
    assert!(definition.validate_arguments(&json!({ "path": "/a" })).is_ok());
    assert_eq!(
        definition.validate_arguments(&json!({ "path": 42 })).unwrap_err(),
        "argument path must be string"
    );
}

#[tokio::test]
async fn tool_executor_builtin_read_file_uses_client_bridge() {
    let (sink, bridge, mut rx) = plumbing();
    let (tools, _budget, executor) = harness(OrchestratorConfig::default());
    tools.lock().expect("registry").register_builtins(&SessionId::new("s-1")).expect("builtins");

    let intent = ToolIntent::new("tc-1", "read_file", json!({ "path": "/a" }));
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let task_bridge = bridge.clone();
    let task = tokio::spawn(async move {
        executor.execute(&intent, &sink, &task_bridge, cancel_rx, &task_fixture(), &[]).await
    });

    // Lifecycle: pending → in-progress → client request.
    assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCall(_)));
    assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCallUpdate(_)));
    let RawJsonRpcMessage::Request(request) = next_client_request(&mut rx).await else {
        panic!("expected request frame");
    };
    assert_eq!(request.method.as_ref(), "fs/read_text_file");
    assert_eq!(request.params.clone().expect("params").into_value()["path"], "/a");

    bridge.handle_response(Response::Result {
        id: RequestId::Number(1),
        result: json!({ "content": "file contents" }),
    });

    let result = task.await.expect("task joins").expect("executes");
    assert!(result.success);
    assert_eq!(result.text_output, "file contents");
    assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCallUpdate(_)));
    assert!(rx.try_recv().is_err(), "no further outbound events");
}

#[tokio::test]
async fn tool_executor_builtin_ask_user_uses_elicitation() {
    let (sink, bridge, mut rx) = plumbing();
    let (tools, _budget, executor) = harness(OrchestratorConfig::default());
    tools.lock().expect("registry").register_builtins(&SessionId::new("s-1")).expect("builtins");

    let intent = ToolIntent::new("tc-1", "ask_user", json!({ "message": "pick one" }));
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let task_bridge = bridge.clone();
    let task = tokio::spawn(async move {
        executor.execute(&intent, &sink, &task_bridge, cancel_rx, &task_fixture(), &[]).await
    });

    assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCall(_)));
    assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCallUpdate(_)));
    let RawJsonRpcMessage::Request(request) = next_client_request(&mut rx).await else {
        panic!("expected request frame");
    };
    assert_eq!(request.method.as_ref(), "elicitation/create");

    bridge.handle_response(Response::Result {
        id: RequestId::Number(1),
        result: json!({ "action": "cancel" }),
    });

    let result = task.await.expect("task joins").expect("executes");
    assert!(result.success);
    assert_eq!(result.text_output, "elicitation created");
}

#[test]
fn native_gated_tools_are_classifier_eligible() {
    let tools = super::registry::builtin_tools(&SessionId::new("s-1"));
    let definition = |name: &str| {
        tools
            .iter()
            .find(|tool| tool.definition().name == name)
            .unwrap_or_else(|| panic!("{name} registered"))
            .definition()
    };
    for name in ["create_terminal", "write_file", "cargo_check"] {
        assert!(definition(name).classifier_eligible, "{name} must reach the classifier");
        assert!(!definition(name).host_approval, "{name} keeps native policy semantics");
    }
    assert!(!definition("read_file").classifier_eligible);
}

#[tokio::test]
async fn bridge_tool_forwards_classifier_auto_approval_meta() {
    let (_sink, bridge, mut rx) = plumbing();

    let task_bridge = bridge.clone();
    let terminal = tokio::spawn(async move {
        ClientBridgeTool::run(
            BridgeCall::CreateTerminal,
            SessionId::new("s-1"),
            json!({ "command": "cargo test 2>&1 | tail" }),
            &task_bridge,
            TerminalWaitLimits::default(),
            true,
        )
        .await
    });
    let RawJsonRpcMessage::Request(request) = next_client_request(&mut rx).await else {
        panic!("expected terminal create request");
    };
    assert_eq!(request.method.as_ref(), "terminal/create");
    let params = request.params.clone().expect("params").into_value();
    assert_eq!(params["_meta"]["ee/approval_classifier"]["auto_approved"], json!(true));
    bridge.handle_response(Response::Result {
        id: request.id,
        result: json!({ "terminalId": "term-1" }),
    });
    terminal.await.expect("task joins").expect("terminal created");

    let task_bridge = bridge.clone();
    let write = tokio::spawn(async move {
        ClientBridgeTool::run(
            BridgeCall::WriteTextFile,
            SessionId::new("s-1"),
            json!({ "path": "/work/src/lib.rs", "content": "fn main() {}\n" }),
            &task_bridge,
            TerminalWaitLimits::default(),
            true,
        )
        .await
    });
    let RawJsonRpcMessage::Request(request) = next_client_request(&mut rx).await else {
        panic!("expected write request");
    };
    assert_eq!(request.method.as_ref(), "fs/write_text_file");
    let params = request.params.clone().expect("params").into_value();
    assert_eq!(params["_meta"]["ee/approval_classifier"]["auto_approved"], json!(true));
    bridge.handle_response(Response::Result { id: request.id, result: json!({}) });
    write.await.expect("task joins").expect("file written");
}

#[tokio::test(start_paused = true)]
async fn terminal_wait_timeout_kills_captures_output_and_releases() {
    let (_sink, bridge, mut rx) = plumbing();
    let task_bridge = bridge.clone();
    let task = tokio::spawn(async move {
        ClientBridgeTool::run(
            BridgeCall::WaitForTerminalExit,
            SessionId::new("s-1"),
            json!({ "terminal_id": "term-1", "timeout_ms": 1 }),
            &task_bridge,
            TerminalWaitLimits::default(),
            false,
        )
        .await
    });

    let RawJsonRpcMessage::Request(wait) = next_client_request(&mut rx).await else {
        panic!("expected terminal wait request");
    };
    assert_eq!(wait.method.as_ref(), "terminal/wait_for_exit");

    tokio::time::advance(Duration::from_millis(1)).await;
    let RawJsonRpcMessage::Request(kill) = next_client_request(&mut rx).await else {
        panic!("expected terminal kill request");
    };
    assert_eq!(kill.method.as_ref(), "terminal/kill");
    bridge.handle_response(Response::Result { id: kill.id, result: json!({}) });

    let RawJsonRpcMessage::Request(output) = next_client_request(&mut rx).await else {
        panic!("expected terminal output request");
    };
    assert_eq!(output.method.as_ref(), "terminal/output");
    bridge.handle_response(Response::Result {
        id: output.id,
        result: json!({
            "output": "partial output",
            "truncated": false,
            "exitStatus": { "exitCode": null, "signal": "9" },
        }),
    });

    let RawJsonRpcMessage::Request(release) = next_client_request(&mut rx).await else {
        panic!("expected terminal release request");
    };
    assert_eq!(release.method.as_ref(), "terminal/release");
    bridge.handle_response(Response::Result { id: release.id, result: json!({}) });

    let result = task.await.expect("task joins").expect("timeout workflow succeeds");
    assert!(result.success);
    assert_eq!(result.structured_output.as_ref().expect("structured output")["timed_out"], true);
    assert_eq!(
        result.structured_output.as_ref().expect("structured output")["output"],
        "partial output"
    );
    assert_eq!(result.structured_output.as_ref().expect("structured output")["released"], true);
}

#[tokio::test]
async fn tool_executor_rejects_missing_arguments_without_execution() {
    let (sink, bridge, mut rx) = plumbing();
    let (tools, _budget, executor) = harness(OrchestratorConfig::default());
    tools.lock().expect("registry").register_builtins(&SessionId::new("s-1")).expect("builtins");

    let intent = ToolIntent::new("tc-1", "read_file", json!({}));
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let result = executor
        .execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[])
        .await
        .expect("executes");

    assert_eq!(result.error_kind, Some(ToolErrorKind::InvalidArguments));
    assert!(result.text_output.contains("missing required argument: path"));
    let SessionUpdate::ToolCallUpdate(failed) = next_update(&mut rx).await else {
        panic!("expected failed tool update");
    };
    assert_eq!(failed.fields.status, Some(ToolCallStatus::Failed));
    assert!(rx.try_recv().is_err(), "no client request may be sent for invalid arguments");
}

#[tokio::test]
async fn tool_executor_rejects_non_object_arguments_without_execution() {
    let (sink, bridge, mut rx) = plumbing();
    let (tools, _budget, executor) = harness(OrchestratorConfig::default());
    tools.lock().expect("registry").register_builtins(&SessionId::new("s-1")).expect("builtins");

    let intent = ToolIntent::new("tc-1", "read_file", serde_json::Value::Null);
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let result = executor
        .execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[])
        .await
        .expect("executes");

    assert_eq!(result.error_kind, Some(ToolErrorKind::InvalidArguments));
    assert_eq!(result.text_output, "tool arguments must be a JSON object");
    let SessionUpdate::ToolCallUpdate(failed) = next_update(&mut rx).await else {
        panic!("expected failed tool update");
    };
    assert_eq!(failed.fields.status, Some(ToolCallStatus::Failed));
    assert!(rx.try_recv().is_err(), "no client request may be sent for invalid arguments");
}

#[tokio::test]
async fn tool_executor_denies_write_tool_by_default_policy() {
    let (sink, bridge, mut rx) = plumbing();
    let (tools, _budget, executor) = harness(OrchestratorConfig::default());
    tools.lock().expect("registry").register_builtins(&SessionId::new("s-1")).expect("builtins");

    let intent = ToolIntent::new("tc-1", "write_file", json!({ "path": "/a", "content": "x" }));
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let result = executor
        .execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[])
        .await
        .expect("executes");

    assert_eq!(result.error_kind, Some(ToolErrorKind::PermissionDenied));
    let SessionUpdate::ToolCallUpdate(failed) = next_update(&mut rx).await else {
        panic!("expected failed tool update");
    };
    assert_eq!(failed.fields.status, Some(ToolCallStatus::Failed));
    let denied_reason_carried = failed
        .fields
        .content
        .as_ref()
        .expect("content")
        .iter()
        .any(|block| {
            matches!(block, ToolCallContent::Content(content) if matches!(&content.content, ContentBlock::Text(text) if text.text.contains("explicit policy allowance")))
        });
    assert!(denied_reason_carried);
    assert!(rx.try_recv().is_err(), "denied write tool must not reach the bridge");
}

#[tokio::test]
async fn tool_executor_denies_execute_tool_by_default_policy() {
    let (sink, bridge, mut rx) = plumbing();
    let (tools, _budget, executor) = harness(OrchestratorConfig::default());
    tools.lock().expect("registry").register_builtins(&SessionId::new("s-1")).expect("builtins");

    let intent = ToolIntent::new("tc-1", "create_terminal", json!({ "command": "ls" }));
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let result = executor
        .execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[])
        .await
        .expect("executes");

    assert_eq!(result.error_kind, Some(ToolErrorKind::PermissionDenied));
    let SessionUpdate::ToolCallUpdate(failed) = next_update(&mut rx).await else {
        panic!("expected failed tool update");
    };
    assert_eq!(failed.fields.status, Some(ToolCallStatus::Failed));
    assert!(rx.try_recv().is_err(), "denied execute tool must not reach the bridge");
}

#[tokio::test]
async fn tool_executor_runs_custom_tool_with_structured_output() {
    let (sink, bridge, mut rx) = plumbing();
    let (tools, _budget, executor) = harness(OrchestratorConfig::default());
    let tool = Arc::new(FakeTool::new(
        ToolDefinition::new("stats", "counts things"),
        ToolResult::success_structured("3 items", json!({ "count": 3 })),
    ));
    tools.lock().expect("registry").register(tool.clone()).expect("registers");

    let intent = ToolIntent::new("tc-1", "stats", json!({}));
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let result = executor
        .execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[])
        .await
        .expect("executes");

    assert_eq!(tool.call_count(), 1);
    assert!(result.success);
    assert_eq!(result.structured_output, Some(json!({ "count": 3 })));
    assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCall(_)));
    assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCallUpdate(_)));
    assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCallUpdate(_)));
    assert!(rx.try_recv().is_err());
}

#[derive(Debug)]
struct FixedRisk(crate::approval_classifier::RiskLevel);

impl crate::approval_classifier::ApprovalRiskClassifier for FixedRisk {
    fn model_id(&self) -> &str {
        "test-fast"
    }

    fn classify(
        &self,
        _candidate: &crate::approval_classifier::ApprovalCandidate,
    ) -> crate::model::ModelFuture<
        Result<
            crate::approval_classifier::ClassifierVerdict,
            crate::approval_classifier::ApprovalClassifierError,
        >,
    > {
        let risk = self.0;
        Box::pin(async move {
            Ok(crate::approval_classifier::ClassifierVerdict {
                risk,
                reason: String::from("test verdict"),
            })
        })
    }
}

fn classifier_executor(
    risk: crate::approval_classifier::RiskLevel,
) -> (
    Arc<Mutex<ToolRegistry>>,
    Arc<Mutex<crate::decision_log::DecisionLog>>,
    EventRecorder,
    ToolExecutor,
) {
    let config = OrchestratorConfig::default();
    let tools = Arc::new(Mutex::new(ToolRegistry::new()));
    let budget = Arc::new(Mutex::new(BudgetTracker::new(&config)));
    let decisions = Arc::new(Mutex::new(crate::decision_log::DecisionLog::default()));
    let events = EventRecorder::new();
    let executor = ToolExecutor::new(
        config,
        tools.clone(),
        budget,
        PolicyEngine::default(),
        0,
        events.clone(),
    )
    .with_approval_classifier(Some(Arc::new(crate::approval_classifier::ApprovalClassifier::new(
        Arc::new(FixedRisk(risk)),
        Duration::from_secs(1),
    ))))
    .with_decisions(Some(decisions.clone()));
    (tools, decisions, events, executor)
}

fn host_approval_write_tool(tools: &Arc<Mutex<ToolRegistry>>) -> Arc<FakeTool> {
    let tool = Arc::new(FakeTool::new(
        ToolDefinition::new("write_file", "writes")
            .side_effect_class(SideEffectClass::Write)
            .host_approval(),
        ToolResult::success("wrote"),
    ));
    tools.lock().expect("registry").register(tool.clone()).expect("registers");
    tool
}

fn approval_codes(decisions: &Arc<Mutex<crate::decision_log::DecisionLog>>) -> Vec<String> {
    decisions
        .lock()
        .expect("decisions")
        .entries()
        .iter()
        .map(|entry| entry.reason_code.clone())
        .collect()
}

#[tokio::test]
async fn approval_classifier_hard_block_denies_without_executing() {
    let (sink, bridge, _rx) = plumbing();
    let (tools, decisions, _events, executor) =
        classifier_executor(crate::approval_classifier::RiskLevel::Low);
    let tool = host_approval_write_tool(&tools);

    let intent = ToolIntent::new("tc-1", "write_file", json!({ "path": ".env" }));
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let result = executor
        .execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[])
        .await
        .expect("executes");

    assert_eq!(result.error_kind, Some(ToolErrorKind::PermissionDenied));
    assert_eq!(tool.call_count(), 0, "hard-blocked calls never execute");
    assert_eq!(approval_codes(&decisions), vec![String::from("approval-denied")]);
}

#[tokio::test]
async fn approval_classifier_low_risk_executes_and_audits() {
    let (sink, bridge, _rx) = plumbing();
    let (tools, decisions, events, executor) =
        classifier_executor(crate::approval_classifier::RiskLevel::Low);
    let tool = host_approval_write_tool(&tools);

    let intent =
        ToolIntent::new("tc-1", "write_file", json!({ "path": "src/lib.rs", "content": "x" }));
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let result = executor
        .execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[])
        .await
        .expect("executes");

    assert!(result.success);
    assert_eq!(tool.call_count(), 1);
    assert_eq!(approval_codes(&decisions), vec![String::from("approval-auto-approved")]);
    assert!(
        !events
            .events()
            .iter()
            .any(|event| matches!(event, OrchestratorEvent::ApprovalRequested { .. })),
        "auto-approved calls skip the approval stage"
    );
}

#[tokio::test]
async fn approval_classifier_medium_risk_keeps_the_host_approval_stage() {
    let (sink, bridge, _rx) = plumbing();
    let (tools, decisions, events, executor) =
        classifier_executor(crate::approval_classifier::RiskLevel::Medium);
    let tool = host_approval_write_tool(&tools);

    let intent =
        ToolIntent::new("tc-1", "write_file", json!({ "path": "src/lib.rs", "content": "x" }));
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let result = executor
        .execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[])
        .await
        .expect("executes");

    assert!(result.success);
    assert_eq!(tool.call_count(), 1);
    assert_eq!(approval_codes(&decisions), vec![String::from("approval-needs-host")]);
    assert!(
        events
            .events()
            .iter()
            .any(|event| matches!(event, OrchestratorEvent::ApprovalRequested { .. })),
        "non-low risk must still report the approval stage"
    );
}

#[tokio::test]
async fn tool_executor_unknown_tool_returns_backend_failure() {
    let (sink, bridge, mut rx) = plumbing();
    let (_tools, _budget, executor) = harness(OrchestratorConfig::default());

    let intent = ToolIntent::new("tc-1", "missing", json!({}));
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let result = executor
        .execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[])
        .await
        .expect("executes");

    assert_eq!(result.error_kind, Some(ToolErrorKind::Backend));
    assert!(result.text_output.contains("unknown tool: missing"));
    let SessionUpdate::ToolCallUpdate(failed) = next_update(&mut rx).await else {
        panic!("expected failed tool update");
    };
    assert_eq!(failed.fields.status, Some(ToolCallStatus::Failed));
}

#[tokio::test]
async fn background_terminal_jobs_register_and_clear() {
    let jobs = Arc::new(crate::background_jobs::BackgroundJobRegistry::new());
    let (_tools, _budget, executor) = harness(OrchestratorConfig::default());
    let executor = executor.with_background_jobs(Some(jobs.clone()));

    let create = ToolDefinition::new("create_terminal", "creates");
    let create_intent =
        ToolIntent::new("tc-1", "create_terminal", json!({ "command": "cargo build" }));
    executor.track_background_job(
        &create,
        &create_intent,
        &ToolResult::success_structured("ok", json!({ "terminal_id": "term-1" })),
    );
    assert!(jobs.snapshot_text().expect("job tracked").contains("cargo build"));

    let wait = ToolDefinition::new("wait_for_terminal_exit", "waits");
    let wait_intent =
        ToolIntent::new("tc-2", "wait_for_terminal_exit", json!({ "terminal_id": "term-1" }));
    executor.track_background_job(&wait, &wait_intent, &ToolResult::success("exited"));
    assert!(jobs.is_empty(), "foreground wait clears the background job");
}

#[tokio::test]
async fn tool_executor_timeout_emits_failed_update() {
    let (sink, bridge, mut rx) = plumbing();
    let config = OrchestratorConfig {
        tool_timeouts: crate::config::ToolTimeoutPolicy {
            fast_local_read: Duration::from_millis(50),
            search_and_mcp: Duration::from_millis(50),
            write_and_edit: Duration::from_millis(50),
            terminal_default: Duration::from_millis(50),
            terminal_max: Duration::from_millis(50),
        },
        ..OrchestratorConfig::default()
    };
    let (tools, _budget, executor) = harness(config);
    tools
        .lock()
        .expect("registry")
        .register(Arc::new(SlowTool {
            definition: ToolDefinition::new("slow", "sleeps"),
            delay: Duration::from_millis(200),
        }))
        .expect("registers");

    let intent = ToolIntent::new("tc-1", "slow", json!({}));
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let result = executor
        .execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[])
        .await
        .expect("executes");

    assert_eq!(result.error_kind, Some(ToolErrorKind::Timeout));
    assert!(result.text_output.contains("timed out"));
    assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCall(_)));
    assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCallUpdate(_)));
    let SessionUpdate::ToolCallUpdate(failed) = next_update(&mut rx).await else {
        panic!("expected failed tool update");
    };
    assert_eq!(failed.fields.status, Some(ToolCallStatus::Failed));
    let timeout_reason_carried = failed
        .fields
        .content
        .as_ref()
        .expect("content")
        .iter()
        .any(|block| {
            matches!(block, ToolCallContent::Content(content) if matches!(&content.content, ContentBlock::Text(text) if text.text.contains("timed out")))
        });
    assert!(timeout_reason_carried);
}

#[tokio::test]
async fn tool_executor_cancelled_result_shapes_failed_update() {
    let (sink, bridge, mut rx) = plumbing();
    let (tools, _budget, executor) = harness(OrchestratorConfig::default());
    tools
        .lock()
        .expect("registry")
        .register(Arc::new(FakeTool::new(
            ToolDefinition::new("cancel_me", "cancels"),
            ToolResult::failure(ToolErrorKind::Cancelled, "backend refused"),
        )))
        .expect("registers");

    let intent = ToolIntent::new("tc-1", "cancel_me", json!({}));
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let result = executor
        .execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[])
        .await
        .expect("executes");

    assert_eq!(result.error_kind, Some(ToolErrorKind::Cancelled));
    assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCall(_)));
    assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCallUpdate(_)));
    let SessionUpdate::ToolCallUpdate(failed) = next_update(&mut rx).await else {
        panic!("expected failed tool update");
    };
    assert_eq!(failed.fields.status, Some(ToolCallStatus::Failed));
}

#[tokio::test]
async fn tool_executor_cancel_during_run_stops_loop() {
    let (sink, bridge, mut rx) = plumbing();
    let (tools, _budget, executor) = harness(OrchestratorConfig::default());
    tools
        .lock()
        .expect("registry")
        .register(Arc::new(CancelObservingTool {
            definition: ToolDefinition::new("blocking", "blocks until cancelled"),
        }))
        .expect("registers");

    let intent = ToolIntent::new("tc-1", "blocking", json!({}));
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        executor.execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[]).await
    });

    assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCall(_)));
    assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCallUpdate(_)));
    cancel_tx.send(true).expect("cancels");

    let error = task.await.expect("task joins").expect_err("cancellation stops the loop");
    assert_eq!(error, OrchestratorError::Cancellation);
    assert!(rx.try_recv().is_err(), "no outcome update after cancellation");
}

#[tokio::test]
async fn tool_executor_relative_read_path_fails_without_request() {
    let (sink, bridge, mut rx) = plumbing();
    let (tools, _budget, executor) = harness(OrchestratorConfig::default());
    tools.lock().expect("registry").register_builtins(&SessionId::new("s-1")).expect("builtins");

    let intent = ToolIntent::new("tc-1", "read_file", json!({ "path": "rel/file" }));
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let result = executor
        .execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[])
        .await
        .expect("executes");

    assert_eq!(result.error_kind, Some(ToolErrorKind::InvalidArguments));
    assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCall(_)));
    assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCallUpdate(_)));
    assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCallUpdate(_)));
    assert!(rx.try_recv().is_err(), "relative path must not reach the transport");
}
