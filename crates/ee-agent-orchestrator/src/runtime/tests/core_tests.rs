//! Runtime tests: core.
use super::*;

#[test]
fn usage_stats_tool_registers_only_with_session_logging() {
    let runtime = OrchestratorRuntime::new(
        OrchestratorConfig::default(),
        Arc::new(FakeModel::new(Vec::new())),
    );
    runtime
        .register_builtins(&ee_agent_protocol::SessionId::new("s-1"))
        .expect("builtins register");
    assert!(
        runtime.tool_names().contains(&String::from("usage_stats")),
        "session-log analytics reach the model as a read-only tool"
    );

    let mut config = OrchestratorConfig::default();
    config.session_log.enabled = false;
    let runtime = OrchestratorRuntime::new(config, Arc::new(FakeModel::new(Vec::new())));
    runtime
        .register_builtins(&ee_agent_protocol::SessionId::new("s-2"))
        .expect("builtins register");
    assert!(!runtime.tool_names().contains(&String::from("usage_stats")));
}

#[test]
fn user_agent_selection_sets_model_steps_and_permission_overlay() {
    let default = Arc::new(FakeModel::new(Vec::new()));
    let alternate = Arc::new(FakeModel::new(Vec::new()));
    let mut registry = crate::model_registry::ModelRegistry::new();
    registry
        .register(crate::model_registry::DEFAULT_MODEL_ID, default)
        .expect("default registered");
    registry.register("model-b", alternate).expect("model-b registered");
    let runtime = OrchestratorRuntime::with_shared_model_registry(
        OrchestratorConfig::default(),
        Arc::new(registry),
        PolicyEngine::default(),
    )
    .expect("runtime builds");
    let workspace = tempfile::tempdir().expect("workspace");
    let path = workspace.path().join(".agents/agents/reviewer.md");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &path,
        "---\nmode: primary\nmodel: model-b\nsteps: 2\npermission:\n  edit: deny\n---\nReview.\n",
    )
    .expect("write agent");
    let subagent = workspace.path().join(".agents/agents/helper.md");
    std::fs::write(&subagent, "---\nmode: subagent\n---\nHelp.\n").expect("write helper");

    runtime.reload_user_agents(workspace.path(), &[], None);
    runtime.select_user_agent(Some("reviewer")).expect("reviewer selected");
    assert_eq!(runtime.active_user_agent_name().as_deref(), Some("reviewer"));
    assert_eq!(runtime.selected_model().as_deref(), Some("model-b"));
    assert_eq!(runtime.config_for_turn().max_loop_iterations, Some(2));
    let tool = ToolDefinition::new("write_file", "write").side_effect_class(SideEffectClass::Write);
    assert!(
        !runtime.policy().check(&tool, Default::default()).allow,
        "the agent's deny overlay blocks writes before host approval"
    );
    runtime.set_policy(PolicyEngine::default());
    assert!(
        runtime.policy().policy().permission_rules.is_some(),
        "a policy refresh keeps the agent overlay"
    );

    let error = runtime.select_user_agent(Some("helper")).expect_err("subagent rejected");
    assert!(error.contains("subagent-only"), "unexpected error: {error}");
    runtime.select_user_agent(None).expect("clears selection");
    assert!(runtime.active_user_agent_name().is_none());
    assert!(runtime.policy().policy().permission_rules.is_none());
}

#[tokio::test]
async fn session_affinity_reaches_every_model_request() {
    let model = FakeModel::new(vec![ModelResponse::new().text("ok").completed()]);
    let (sink, client, _rx) = plumbing();
    let runtime = OrchestratorRuntime::new(OrchestratorConfig::default(), Arc::new(model.clone()));
    runtime.set_session_context(
        crate::session_context::SessionContext::new("s-child")
            .with_parent("s-parent")
            .with_project("proj_test")
            .with_client("ee"),
    );

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    runtime.run_turn(prompt("hello"), sink, client, cancel_rx).await.expect("turn succeeds");

    let request = model.requests().pop().expect("one request");
    let session = request.session.expect("affinity reaches the request");
    assert_eq!(session.session_id, "s-1", "the loop stamps the turn's session id");
    assert_eq!(session.parent_session_id.as_deref(), Some("s-parent"));
    assert_eq!(session.project_id.as_deref(), Some("proj_test"));
    assert_eq!(session.client_name.as_deref(), Some("ee"));
    assert_eq!(session.affinity(), "s-parent", "a child inherits the parent affinity");
}

#[tokio::test]
async fn runtime_runs_one_complete_turn_with_fake_model() {
    let model = FakeModel::new(vec![
        ModelResponse::new().reasoning("thinking hard").text("final answer").completed(),
    ]);
    let (sink, client, mut rx) = plumbing();
    let runtime = OrchestratorRuntime::new(OrchestratorConfig::default(), Arc::new(model.clone()));
    runtime.register_tool(echo_tool()).expect("registers echo");

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let result = runtime
        .run_turn(prompt("hello world"), sink, client, cancel_rx)
        .await
        .expect("turn succeeds");
    assert_eq!(result.stop_reason, StopReason::EndTurn);

    // One model call with the prompt as the user message, the root task,
    // the registered tool schema, and the budget snapshot.
    let calls = model.requests();
    assert_eq!(calls.len(), 1);
    let request = &calls[0];
    assert_eq!(request.transcript.len(), 1);
    assert_eq!(request.transcript[0].role, ModelRole::User);
    assert_eq!(
        request.transcript[0].content,
        vec![crate::model::ModelContent::Text("hello world".into())]
    );
    assert_eq!(request.task.title, "hello world");
    assert!(request.task.id.as_str().starts_with("task-"));
    assert!(request.tools.iter().any(|tool| tool.name == "echo"));
    assert_eq!(request.budget.iterations_used, 1);
    assert_eq!(request.budget.iterations_max, None);

    // Updates stream in order: plan, thought chunk, then message chunk.
    assert!(matches!(next_update(&mut rx).await, SessionUpdate::Plan(_)));
    assert!(matches!(next_update(&mut rx).await, SessionUpdate::AgentThoughtChunk(_)));
    let update = next_update(&mut rx).await;
    assert!(matches!(update, SessionUpdate::AgentMessageChunk(_)));
    assert!(rx.try_recv().is_err(), "no further outbound events");
}

#[tokio::test]
async fn runtime_executes_tool_intent_and_continues_loop() {
    let tool = echo_tool();
    let model = FakeModel::new(vec![
        ModelResponse::new().tool_intents(vec![ToolIntent::new(
            "tc-1",
            "echo",
            json!({ "text": "x" }),
        )]),
        ModelResponse::new().text("done").completed(),
    ]);
    let (sink, client, mut rx) = plumbing();
    let runtime = OrchestratorRuntime::new(OrchestratorConfig::default(), Arc::new(model.clone()));
    runtime.register_tool(tool.clone()).expect("registers echo");

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let result = runtime
        .run_turn(prompt("hello world"), sink, client, cancel_rx)
        .await
        .expect("turn succeeds");
    assert_eq!(result.stop_reason, StopReason::EndTurn);

    // The tool ran exactly once with the model's arguments.
    assert_eq!(tool.call_count(), 1);
    assert_eq!(tool.call_arguments(), vec![json!({ "text": "x" })]);

    // The initial plan update precedes the tool lifecycle updates.
    assert!(matches!(next_update(&mut rx).await, SessionUpdate::Plan(_)));
    // Tool updates precede the final message chunk.
    assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCall(_)));
    assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCallUpdate(_)));
    assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCallUpdate(_)));
    assert!(matches!(next_update(&mut rx).await, SessionUpdate::AgentMessageChunk(_)));

    // The second model call sees the tool observation in the transcript.
    let calls = model.requests();
    assert_eq!(calls.len(), 2);
    let transcript = &calls[1].transcript;
    let tool_message = transcript
        .iter()
        .find(|message| message.role == ModelRole::Tool)
        .expect("tool observation appended");
    assert_eq!(tool_message.content.len(), 1);
}

#[tokio::test]
async fn runtime_denies_write_tool_by_default_policy() {
    let tool = Arc::new(FakeTool::new(
        ToolDefinition::new("write_file", "writes").side_effect_class(SideEffectClass::Write),
        ToolResult::success("written"),
    ));
    let model = FakeModel::new(vec![
        ModelResponse::new().tool_intents(vec![ToolIntent::new("tc-1", "write_file", json!({}))]),
        ModelResponse::new().text("done").completed(),
    ]);
    let (sink, client, mut rx) = plumbing();
    let runtime = OrchestratorRuntime::new(OrchestratorConfig::default(), Arc::new(model.clone()));
    runtime.register_tool(tool.clone()).expect("registers write_file");

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let result = runtime
        .run_turn(prompt("hello world"), sink, client, cancel_rx)
        .await
        .expect("turn succeeds");
    assert_eq!(result.stop_reason, StopReason::EndTurn);

    // The tool never executed; the client saw a failed tool-call update.
    assert_eq!(tool.call_count(), 0);
    assert!(matches!(next_update(&mut rx).await, SessionUpdate::Plan(_)));
    let update = next_update(&mut rx).await;
    let SessionUpdate::ToolCallUpdate(failed) = update else {
        panic!("expected tool call update, got {update:?}");
    };
    assert_eq!(failed.tool_call_id, ee_agent_protocol::ToolCallId::new("tc-1"));
    assert_eq!(failed.fields.status, Some(ee_agent_protocol::ToolCallStatus::Failed));
    assert!(matches!(next_update(&mut rx).await, SessionUpdate::AgentMessageChunk(_)));

    // The model saw the denial as a failed tool observation.
    let calls = model.requests();
    assert_eq!(calls.len(), 2);
    let transcript = &calls[1].transcript;
    let tool_message = transcript
        .iter()
        .find(|message| message.role == ModelRole::Tool)
        .expect("tool observation appended");
    let crate::model::ModelContent::ToolResult { result, .. } = &tool_message.content[0] else {
        panic!("expected tool result content");
    };
    assert!(!result.success);
    assert_eq!(result.error_kind, Some(crate::tools::ToolErrorKind::PermissionDenied));
}

#[tokio::test]
async fn runtime_stops_when_loop_iteration_budget_exhausted() {
    let tool = echo_tool();
    let model = FakeModel::new(vec![
        ModelResponse::new().tool_intents(vec![ToolIntent::new("tc-1", "echo", json!({}))]),
        ModelResponse::new().tool_intents(vec![ToolIntent::new("tc-2", "echo", json!({}))]),
        ModelResponse::new().tool_intents(vec![ToolIntent::new("tc-3", "echo", json!({}))]),
    ]);
    let config =
        OrchestratorConfig { max_loop_iterations: Some(2), ..OrchestratorConfig::default() };
    let (sink, client, _rx) = plumbing();
    let runtime = OrchestratorRuntime::new(config, Arc::new(model.clone()));
    runtime.register_tool(tool).expect("registers echo");

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let result = runtime
        .run_turn(prompt("hello world"), sink, client, cancel_rx)
        .await
        .expect("the cap produces a summary instead of an error");
    assert_eq!(result.stop_reason, ee_agent_protocol::StopReason::MaxTurnRequests);
    // Two iterations ran, then one final text-only summary call.
    assert_eq!(model.requests().len(), 3);
}

#[tokio::test]
async fn runtime_cancels_before_first_model_call() {
    let model = FakeModel::new(vec![ModelResponse::new().text("never").completed()]);
    let (sink, client, _rx) = plumbing();
    let runtime = OrchestratorRuntime::new(OrchestratorConfig::default(), Arc::new(model.clone()));

    let (_cancel_tx, cancel_rx) = watch::channel(true);
    let error = runtime
        .run_turn(prompt("hello world"), sink, client, cancel_rx)
        .await
        .expect_err("pre-cancelled turn stops");
    assert_eq!(error, OrchestratorError::Cancellation);
    assert_eq!(model.requests().len(), 0, "no model call may start after cancellation");
}

#[tokio::test]
async fn runtime_stops_after_two_empty_model_responses() {
    let model = FakeModel::new(vec![ModelResponse::new(), ModelResponse::new()]);
    let (sink, client, mut rx) = plumbing();
    let runtime = OrchestratorRuntime::new(OrchestratorConfig::default(), Arc::new(model.clone()));

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let result = runtime
        .run_turn(prompt("hello world"), sink, client, cancel_rx)
        .await
        .expect("empty responses stop deterministically");
    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(model.requests().len(), 2);
    // Only the initial plan update is emitted; empty responses stream nothing.
    assert!(matches!(next_update(&mut rx).await, SessionUpdate::Plan(_)));
    assert!(rx.try_recv().is_err(), "empty responses emit no further updates");
}
#[test]
fn task_summary_derives_bounded_title_and_description() {
    let ctx = prompt("first line");
    let (title, description) = task_summary(&ctx);
    assert_eq!(title, "first line");
    assert_eq!(description, "first line");

    let long = "x".repeat(4_500);
    let ctx = prompt(&long);
    let (title, description) = task_summary(&ctx);
    assert_eq!(title.chars().count(), MAX_TASK_TITLE_CHARS + 1);
    assert!(title.ends_with('…'));
    assert_eq!(description.chars().count(), MAX_TASK_DESCRIPTION_CHARS + 1);
}

#[test]
fn task_summary_falls_back_for_prompt_without_text() {
    let ctx = PromptContext::new(SessionId::new("s-1"), Vec::new());
    let (title, description) = task_summary(&ctx);
    assert_eq!(title, UNTITLED_TASK);
    assert_eq!(description, UNTITLED_TASK);
}
