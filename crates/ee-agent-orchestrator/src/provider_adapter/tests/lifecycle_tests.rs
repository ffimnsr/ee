//! Provider-adapter tests: lifecycle.
use super::*;

#[tokio::test]
async fn provider_adapter_initialize_metadata_through_memory_transport() {
    let model = Arc::new(FakeModel::new(Vec::new()));
    let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model);
    let (handle, task) = spawn_server(provider);

    handle.send(request(1, "initialize", json!({ "protocolVersion": 1 })));
    let result = request_result(handle.next_frame().await);
    assert_eq!(result["protocolVersion"], 1);
    assert_eq!(result["agentInfo"]["name"], DEFAULT_IMPLEMENTATION_NAME);
    assert_eq!(result["agentInfo"]["title"], DEFAULT_IMPLEMENTATION_TITLE);
    assert_eq!(
        result["agentInfo"]["version"],
        env!("CARGO_PKG_VERSION"),
        "version comes from the adapter crate"
    );
    assert_eq!(result["agentCapabilities"]["loadSession"], true);
    assert!(handle.outbound().is_empty(), "no further frames");

    handle.shutdown(task).await;
}

#[tokio::test]
async fn provider_adapter_new_session_creates_orchestrator_state() {
    let model = Arc::new(FakeModel::new(Vec::new()));
    let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model.clone());
    let probe = provider.clone();
    let (handle, task) = spawn_server(provider);

    handle.send(request(1, "session/new", session_new_params("/work")));
    let result = request_result(handle.next_frame().await);
    assert_eq!(result["sessionId"], "session-1", "monotonic provider id");
    assert_eq!(result["modes"]["currentModeId"], "ask");
    assert_eq!(
        result["modes"]["availableModes"]
            .as_array()
            .expect("mode list")
            .iter()
            .map(|mode| mode["id"].as_str().expect("mode id"))
            .collect::<Vec<_>>(),
        vec!["ask", "write", "plan"]
    );

    let (tasks, memory) = probe.session_state("session-1").expect("session state exists");
    assert_eq!(tasks.len(), 0, "fresh task graph");
    assert!(memory.is_empty(), "fresh memory store");

    handle.shutdown(task).await;
}

#[tokio::test]
async fn provider_adapter_mode_switch_enforces_effective_tool_policy() {
    use crate::tools::{SideEffectClass, ToolDefinition};

    let provider = OrchestratorProvider::new(
        OrchestratorProviderConfig::default(),
        Arc::new(FakeModel::new(Vec::new())),
    );
    let init =
        provider.new_session(NewSessionContext::new("/work")).await.expect("creates session");
    let session_id = init.session_id;
    let tool = |class| ToolDefinition::new("test", "test").side_effect_class(class);

    let (mode, policy) = provider.session_mode_policy(&session_id.to_string()).expect("state");
    assert_eq!(mode, SessionModeId::new(ASK_MODE_ID));
    assert!(
        policy.check(&tool(SideEffectClass::Read), Default::default()).allow,
        "ask allows reads"
    );
    for class in [SideEffectClass::Write, SideEffectClass::Execute, SideEffectClass::Delegate] {
        assert!(!policy.check(&tool(class), Default::default()).allow, "ask denies {class:?}");
    }
    assert!(
        !policy.check(&tool(SideEffectClass::Write).host_approval(), Default::default()).allow,
        "ask denies host-approved writes before the editor gate"
    );

    provider
        .set_mode(SetModeContext::new(session_id.clone(), PLAN_MODE_ID))
        .await
        .expect("switches to plan");
    let (mode, policy) = provider.session_mode_policy(&session_id.to_string()).expect("state");
    assert_eq!(mode, SessionModeId::new(PLAN_MODE_ID));
    assert!(policy.check(&tool(SideEffectClass::Read), Default::default()).allow);
    for class in [SideEffectClass::Write, SideEffectClass::Execute, SideEffectClass::Delegate] {
        assert!(!policy.check(&tool(class), Default::default()).allow, "plan denies {class:?}");
    }
    assert!(
        !policy.check(&tool(SideEffectClass::Execute).host_approval(), Default::default()).allow,
        "plan denies host-approved execution before the editor gate"
    );

    provider
        .set_mode(SetModeContext::new(session_id.clone(), WRITE_MODE_ID))
        .await
        .expect("switches to write");
    let (mode, policy) = provider.session_mode_policy(&session_id.to_string()).expect("state");
    assert_eq!(mode, SessionModeId::new(WRITE_MODE_ID));
    assert!(policy.check(&tool(SideEffectClass::Read), Default::default()).allow);
    assert!(
        policy.check(&tool(SideEffectClass::Write).host_approval(), Default::default()).allow,
        "write preserves host approval routing"
    );
}

#[tokio::test]
async fn provider_adapter_mode_switch_preserves_model_conversation() {
    let model = Arc::new(FakeModel::new(vec![
        ModelResponse::new().text("I can create that file after write mode is enabled").completed(),
        ModelResponse::new().text("implemented").completed(),
    ]));
    let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model.clone());
    let (handle, task) = spawn_server(provider);
    let session_id = new_session(&handle, 1).await;

    handle.send(request(
        2,
        "session/prompt",
        prompt_params(&session_id, "create a long HTML file in test_assets"),
    ));
    let (frame, _) = next_response_with_updates(&handle).await;
    assert_eq!(request_result(frame)["stopReason"], "end_turn");

    handle.send(request(
        3,
        "session/set_mode",
        json!({ "sessionId": session_id, "modeId": WRITE_MODE_ID }),
    ));
    assert_eq!(request_result(handle.next_frame().await), json!({}));

    handle.send(request(4, "session/prompt", prompt_params(&session_id, "implement it")));
    let (frame, _) = next_response_with_updates(&handle).await;
    assert_eq!(request_result(frame)["stopReason"], "end_turn");

    let requests = model.requests();
    assert_eq!(requests.len(), 2, "one model call per completed prompt");
    let conversation = requests[1]
        .transcript
        .iter()
        .filter(|message| matches!(message.role, ModelRole::User | ModelRole::Assistant))
        .map(|message| (message.role, message.text_content()))
        .collect::<Vec<_>>();
    assert_eq!(
        conversation,
        vec![
            (ModelRole::User, "create a long HTML file in test_assets".to_string()),
            (
                ModelRole::Assistant,
                "I can create that file after write mode is enabled".to_string(),
            ),
            (ModelRole::User, "implement it".to_string()),
        ],
        "mode switch must keep prior user and assistant turns in model context"
    );

    handle.shutdown(task).await;
}

#[tokio::test]
async fn provider_adapter_prompt_runs_loop_and_emits_assistant_update() {
    let model =
        Arc::new(FakeModel::new(vec![ModelResponse::new().text("final answer").completed()]));
    let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model);
    let (handle, task) = spawn_server(provider);
    let session_id = new_session(&handle, 1).await;
    assert_eq!(session_id, "session-1");

    handle.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
    // MCP diagnostics, plan replacement, model text, host-derived final
    // evidence report, then the unchanged ACP response.
    drain_mcp_diagnostics(&handle).await;
    let frames = handle.next_frames(4).await;
    let RawJsonRpcMessage::Notification(plan) = &frames[0] else {
        panic!("first frame is the plan update, got {:?}", frames[0]);
    };
    assert_eq!(plan.method.as_ref(), "session/update");
    let plan_params = raw_params_to_value(plan.params.clone());
    assert_eq!(plan_params["sessionId"], "session-1");
    assert_eq!(plan_params["update"]["sessionUpdate"], "plan", "plan replacement update");

    let RawJsonRpcMessage::Notification(message) = &frames[1] else {
        panic!("second frame is the message update, got {:?}", frames[1]);
    };
    let message_params = raw_params_to_value(message.params.clone());
    assert_eq!(message_params["sessionId"], "session-1");
    assert_eq!(message_params["update"]["sessionUpdate"], "agent_message_chunk");
    assert!(
        message_params.to_string().contains("final answer"),
        "message chunk carries the assistant text: {message_params}"
    );

    let final_params = raw_params_to_value(match &frames[2] {
        RawJsonRpcMessage::Notification(update) => update.params.clone(),
        other => panic!("third frame is the final response, got {other:?}"),
    });
    assert_eq!(final_params["update"]["messageId"], "ee-final-response-1");
    assert!(
        final_params["update"]["content"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("completion: unverified")),
        "typed final completion report: {final_params}"
    );
    let result = request_result(frames[3].clone());
    assert_eq!(result["stopReason"], "end_turn");

    handle.shutdown(task).await;
}

#[tokio::test]
async fn provider_telemetry_is_opt_in_local_and_records_terminal_state() {
    let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("done").completed()]));
    let provider = OrchestratorProvider::new(
        OrchestratorProviderConfig {
            telemetry: TelemetryConfig {
                enabled: true,
                max_turns: 2,
                max_events_per_turn: 16,
                max_bytes_per_turn: 4_096,
            },
            ..OrchestratorProviderConfig::default()
        },
        model,
    );
    let probe = provider.clone();
    let (handle, task) = spawn_server(provider);
    let session_id = new_session(&handle, 1).await;
    handle.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
    let result = request_result(next_response_frame(&handle).await);
    assert_eq!(result["stopReason"], "end_turn");

    let records = probe.export_telemetry_jsonl().expect("exports telemetry");
    assert_eq!(records.lines().count(), 1);
    let record: Value =
        serde_json::from_str(records.lines().next().expect("record")).expect("json");
    assert_eq!(record["turnId"], "turn-1");
    assert_eq!(record["outcome"], "succeeded");
    assert_eq!(record["terminalState"], "unverified");
    assert_eq!(record["summary"]["modelCalls"], 1);
    assert!(!records.contains(&session_id), "telemetry IDs must not contain ACP sessions");
    assert_eq!(record["attribution"]["transport"], "acp");

    handle.shutdown(task).await;
}

#[tokio::test]
async fn provider_adapter_prompt_includes_workspace_system_context() {
    let model =
        Arc::new(FakeModel::new(vec![ModelResponse::new().text(plan_response("ok")).completed()]));
    let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model.clone());
    let probe = provider.clone();
    let (handle, task) = spawn_server(provider);

    handle.send(request(
        1,
        "session/new",
        session_new_params_with_additional("/work/project", &["/shared/lib"]),
    ));
    let session_id = request_result(handle.next_frame().await)["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    // The provider advertises its initial slash commands after the
    // session/new response; drain the update before the prompt flows.
    let frame = handle.next_frame().await;
    let RawJsonRpcMessage::Notification(update) = &frame else {
        panic!("expected the available_commands_update, got {frame:?}");
    };
    assert_eq!(
        raw_params_to_value(update.params.clone())["update"]["sessionUpdate"],
        "available_commands_update"
    );
    handle.send(request(
        2,
        "session/set_mode",
        json!({ "sessionId": session_id, "modeId": PLAN_MODE_ID }),
    ));
    assert_eq!(request_result(handle.next_frame().await), json!({}));
    handle.send(request(3, "session/prompt", prompt_params(&session_id, "read .ee.toml")));
    drain_mcp_diagnostics(&handle).await;
    let frames = handle.next_frames(5).await;
    assert_eq!(request_result(frames[4].clone())["stopReason"], "end_turn");

    let requests = model.requests();
    let context = requests[0].transcript[0].text_content();
    assert!(context.contains("current_working_directory: /work/project"), "{context}");
    assert!(context.contains("- /shared/lib"), "{context}");
    assert!(context.contains("require absolute paths"), "{context}");
    assert!(context.contains("Resolve relative paths"), "{context}");
    assert!(context.contains("Agent mode: plan"), "{context}");
    assert!(context.contains("concrete implementation plan"), "{context}");
    assert!(context.contains("## Plan"), "{context}");
    assert!(context.contains("## Validation"), "{context}");
    assert!(context.contains("## Open questions"), "{context}");
    assert!(context.contains("file or symbol"), "{context}");
    assert!(context.contains("observable success criterion"), "{context}");
    assert!(context.contains(PLAN_PAYLOAD_MARKER), "{context}");

    let tasks = probe.session_state(&session_id).expect("session state").0.list();
    assert_eq!(tasks.len(), 2, "compiled plan replaces prompt-only root");
    assert_eq!(tasks[0].title, "plan");
    assert_eq!(tasks[1].title, "Inspect implementation");

    handle.shutdown(task).await;
}

#[tokio::test]
async fn provider_adapter_prompt_executes_tool_through_client_bridge() {
    let model = Arc::new(FakeModel::new(vec![
        ModelResponse::new().tool_intents(vec![crate::tools::ToolIntent::new(
            "tc-1",
            "read_file",
            json!({ "path": "/tmp/notes.txt" }),
        )]),
        ModelResponse::new().text(plan_response("read it")).completed(),
    ]));
    let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model);
    let (handle, task) = spawn_server(provider);
    let session_id = new_session(&handle, 1).await;

    handle.send(request(
        2,
        "session/set_mode",
        json!({ "sessionId": session_id, "modeId": PLAN_MODE_ID }),
    ));
    assert_eq!(request_result(handle.next_frame().await), json!({}));
    handle.send(request(3, "session/prompt", prompt_params(&session_id, "read a file")));
    // MCP diagnostics, plan, pending tool-call, in-progress tool-call,
    // then the framework-owned fs request.
    drain_mcp_diagnostics(&handle).await;
    let frames = handle.next_frames(4).await;
    let RawJsonRpcMessage::Request(fs_request) = &frames[3] else {
        panic!("fourth frame is the fs request, got {:?}", frames[3]);
    };
    assert_eq!(fs_request.method.as_ref(), "fs/read_text_file");
    let fs_params = raw_params_to_value(fs_request.params.clone());
    assert_eq!(fs_params["path"], "/tmp/notes.txt");
    assert!(matches!(fs_request.id, RequestId::Number(_)), "framework-owned numeric id");

    // Answer the bridge call; the loop appends the observation and asks
    // the model again, streaming the completed tool-call update, then the
    // assistant update, concrete plan replacement, host final response,
    // then the unchanged ACP response.
    handle.send(RawJsonRpcMessage::response(
        fs_request.id.clone(),
        Ok(json!({ "content": "file contents" })),
    ));
    let frames = handle.next_frames(5).await;
    assert_eq!(
        raw_params_to_value(match &frames[0] {
            RawJsonRpcMessage::Notification(update) => update.params.clone(),
            other => panic!("expected completed tool-call update, got {other:?}"),
        })["update"]["sessionUpdate"],
        "tool_call_update",
        "completion update streamed before the response"
    );
    let RawJsonRpcMessage::Notification(message) = &frames[1] else {
        panic!("expected the message update, got {:?}", frames[1]);
    };
    let message_params = raw_params_to_value(message.params.clone());
    assert_eq!(message_params["update"]["sessionUpdate"], "agent_message_chunk");
    assert!(
        message_params.to_string().contains("read it"),
        "message chunk carries the assistant text: {message_params}"
    );
    let plan = raw_params_to_value(match &frames[2] {
        RawJsonRpcMessage::Notification(update) => update.params.clone(),
        other => panic!("expected concrete plan update, got {other:?}"),
    });
    assert_eq!(plan["update"]["sessionUpdate"], "plan");
    assert_eq!(plan["update"]["entries"][1]["content"], "Inspect implementation");
    let final_params = raw_params_to_value(match &frames[3] {
        RawJsonRpcMessage::Notification(update) => update.params.clone(),
        other => panic!("expected final completion update, got {other:?}"),
    });
    assert_eq!(final_params["update"]["messageId"], "ee-final-response-1");
    let result = request_result(frames[4].clone());
    assert_eq!(result["stopReason"], "end_turn");

    handle.shutdown(task).await;
}

#[tokio::test]
async fn provider_adapter_cancel_stops_active_turn() {
    let calls = Arc::new(Mutex::new(0usize));
    let model = Arc::new(CancelAwaitingModel { calls: calls.clone() });
    let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model);
    let (handle, task) = spawn_server(provider);
    let session_id = new_session(&handle, 1).await;

    handle.send(request(2, "session/prompt", prompt_params(&session_id, "blocking prompt")));
    drain_mcp_diagnostics(&handle).await;
    let _plan = handle.next_frame().await; // plan update proves the turn started
    wait_until(|| *calls.lock().expect("calls poisoned") == 1).await;

    handle.send(notification("session/cancel", json!({ "sessionId": session_id })));
    let result = request_result(handle.next_frame().await);
    assert_eq!(result["stopReason"], "cancelled", "deterministic cancelled result");
    assert_eq!(*calls.lock().expect("calls poisoned"), 1, "no second model call");
    assert!(handle.outbound().is_empty(), "no updates after cancellation");

    handle.shutdown(task).await;
}

#[tokio::test]
async fn provider_adapter_close_removes_session_state() {
    let model = Arc::new(FakeModel::new(Vec::new()));
    let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model);
    let probe = provider.clone();
    let (handle, task) = spawn_server(provider);
    let session_id = new_session(&handle, 1).await;
    assert!(probe.session_state(&session_id).is_some());

    handle.send(request(2, "session/close", json!({ "sessionId": session_id })));
    let result = request_result(handle.next_frame().await);
    assert_eq!(result, json!({}));
    assert!(probe.session_state(&session_id).is_none(), "live state removed");
    assert!(probe.has_persisted_state(&session_id), "state persisted for load");

    // The framework store no longer knows the session either.
    handle.send(request(3, "session/prompt", prompt_params(&session_id, "hello")));
    let error = request_error(handle.next_frame().await);
    assert!(error.message.contains("session"), "unknown session rejected");

    handle.shutdown(task).await;
}

#[tokio::test]
async fn provider_adapter_load_session_restores_persisted_state() {
    let model = Arc::new(FakeModel::new(vec![
        ModelResponse::new().text("first turn").completed(),
        ModelResponse::new().text("second turn").completed(),
    ]));
    let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model);
    let probe = provider.clone();
    let (handle, task) = spawn_server(provider);
    let session_id = new_session(&handle, 1).await;

    // One turn creates a root task; closing persists the task graph.
    handle.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
    drain_mcp_diagnostics(&handle).await;
    handle.next_frames(4).await; // plan, model message, final response, response
    assert_eq!(probe.session_state(&session_id).expect("state").0.len(), 1);
    handle.send(request(3, "session/close", json!({ "sessionId": session_id })));
    let _ = request_result(handle.next_frame().await);

    // Durable load restores only mode metadata. Transcript, task text,
    // memory, and tool data are not persisted or replayed.
    handle.send(request(
        4,
        "session/load",
        json!({
            "sessionId": session_id,
            "cwd": "/work",
            "additionalDirectories": [],
            "mcpServers": [],
        }),
    ));
    let frames = handle.next_frames(2).await;
    let RawJsonRpcMessage::Notification(commands) = &frames[0] else {
        panic!("expected available commands update, got {:?}", frames[0]);
    };
    assert_eq!(
        raw_params_to_value(commands.params.clone())["update"]["sessionUpdate"],
        "available_commands_update"
    );
    let result = request_result(frames[1].clone());
    assert_eq!(result["modes"]["currentModeId"], ASK_MODE_ID);
    assert_eq!(probe.session_state(&session_id).expect("restored state").0.len(), 0);

    handle.send(request(5, "session/prompt", prompt_params(&session_id, "again")));
    drain_mcp_diagnostics(&handle).await;
    let frames = handle.next_frames(4).await;
    assert_eq!(request_result(frames[3].clone())["stopReason"], "end_turn");
    assert_eq!(
        probe.session_state(&session_id).expect("state").0.len(),
        1,
        "new turn starts with a fresh durable-session runtime"
    );

    handle.shutdown(task).await;
}
