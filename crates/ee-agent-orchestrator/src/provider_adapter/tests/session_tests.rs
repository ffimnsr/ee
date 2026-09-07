//! Provider-adapter tests: session.
use super::conversation::initial_commands;
use super::conversation::record_agent_message;
use super::conversation::record_user_message;
use super::*;

#[test]
fn orchestrated_sessions_advertise_rubber_duck_without_starting_models() {
    let commands = initial_commands(false);
    assert_eq!(
        commands.iter().map(|command| command.name.as_str()).collect::<Vec<_>>(),
        vec![COMPACT_COMMAND_NAME, RUBBER_DUCK_COMMAND_NAME]
    );
    let recovery_commands = initial_commands(true);
    assert_eq!(
        recovery_commands.iter().map(|command| command.name.as_str()).collect::<Vec<_>>(),
        vec![
            COMPACT_COMMAND_NAME,
            RUBBER_DUCK_COMMAND_NAME,
            DISCARD_COMMAND_NAME,
            ee_agent_protocol::RESUME_COMMAND_NAME,
        ]
    );
}

#[test]
fn provider_registry_requires_default_adapter() {
    let result = OrchestratorProvider::with_model_registry(
        OrchestratorProviderConfig::default(),
        crate::model_registry::ModelRegistry::new(),
        PolicyEngine::default(),
    );
    assert!(matches!(
        result,
        Err(ProviderError::BackendFailure(reason)) if reason.contains("no default adapter")
    ));
}
#[test]
fn streamed_agent_chunks_replay_as_one_message_per_turn() {
    let conversation = Arc::new(Mutex::new(Vec::new()));
    record_user_message(&conversation, "hello");
    record_agent_message(&conversation, "Hel");
    record_agent_message(&conversation, "lo");
    record_user_message(&conversation, "next");
    record_agent_message(&conversation, "Done");

    assert_eq!(
        *conversation.lock().expect("conversation"),
        vec![
            ConversationMessage { role: ConversationRole::User, text: "hello".to_string() },
            ConversationMessage { role: ConversationRole::Agent, text: "Hello".to_string() },
            ConversationMessage { role: ConversationRole::User, text: "next".to_string() },
            ConversationMessage { role: ConversationRole::Agent, text: "Done".to_string() },
        ]
    );
}
#[tokio::test]
async fn provider_adapter_surfaces_recoverable_interruption_and_manual_resume() {
    let model = Arc::new(DelayedModel::new(
        std::time::Duration::from_millis(5_000),
        std::time::Duration::from_millis(1),
        resume_script(),
    ));
    let provider = recovery_provider(model, 0);
    let (handle, task) = spawn_server(provider);
    let session_id = new_session(&handle, 1).await;

    // Prompt 1: the first model call hangs past the slice; the provider
    // answers with a JSON-RPC error carrying the recoverable payload.
    handle.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
    let frame = next_response_frame(&handle).await;
    let Response::Error { error, .. } = unwrap_response(frame.clone()) else {
        panic!("expected an error response, got {frame:?}");
    };
    let recoverable = &error.data.as_ref().expect("recoverable error carries data")["recoverable"];
    assert_eq!(recoverable["fault"], "deadline");
    assert_eq!(recoverable["safe_resume"], true);
    assert_eq!(recoverable["resumed_count"], 0);
    assert!(
        recoverable["checkpoint_id"].as_str().is_some(),
        "checkpoint id on the wire: {recoverable}"
    );

    // Prompt 2 with the same prompt resumes from the checkpoint and
    // completes; the pending checkpoint is cleared.
    handle.send(request(3, "session/prompt", prompt_params(&session_id, "hello")));
    let (frame, updates) = next_response_with_updates(&handle).await;
    let result = request_result(frame);
    assert_eq!(result["stopReason"], "end_turn", "resumed turn completes: {result}");
    let final_reports = updates
        .iter()
        .filter(|update| {
            update["update"]["messageId"]
                .as_str()
                .is_some_and(|id| id.starts_with("ee-final-response-"))
        })
        .collect::<Vec<_>>();
    assert_eq!(final_reports.len(), 1, "completed resume emits one final report");
    assert!(
        final_reports[0]["update"]["content"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("completion: unverified"))
    );

    handle.shutdown(task).await;
}

#[tokio::test]
async fn provider_adapter_auto_resumes_once_and_completes() {
    let model = Arc::new(DelayedModel::new(
        std::time::Duration::from_millis(5_000),
        std::time::Duration::from_millis(1),
        resume_script(),
    ));
    let provider = recovery_provider(model, 1);
    let (handle, task) = spawn_server(provider);
    let session_id = new_session(&handle, 1).await;

    // One prompt: the first slice times out, the safe single auto-resume
    // continues from the checkpoint and completes.
    handle.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
    let (frame, updates) = next_response_with_updates(&handle).await;
    let result = request_result(frame);
    assert_eq!(result["stopReason"], "end_turn", "auto-resume completes: {result}");
    assert_eq!(
        updates
            .iter()
            .filter(|update| update["update"]["messageId"]
                .as_str()
                .is_some_and(|id| id.starts_with("ee-final-response-")))
            .count(),
        1,
        "auto-resume emits one final report only after terminal completion"
    );

    handle.shutdown(task).await;
}

#[tokio::test]
async fn provider_adapter_discard_command_clears_checkpoint() {
    let model = Arc::new(DelayedModel::new(
        std::time::Duration::from_millis(5_000),
        std::time::Duration::from_millis(1),
        resume_script(),
    ));
    let provider = recovery_provider(model, 0);
    let (handle, task) = spawn_server(provider);
    let session_id = new_session(&handle, 1).await;

    // Prompt 1 times out with a recoverable error (not asserted here;
    // the discard flow below is the point).
    handle.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
    let _ = next_response_frame(&handle).await;

    // `/discard` drops the pending checkpoint and answers end_turn.
    handle.send(request(3, "session/prompt", prompt_params(&session_id, "/discard")));
    let result = request_result(next_response_frame(&handle).await);
    assert_eq!(result["stopReason"], "end_turn", "discard answers end_turn: {result}");

    // A later prompt is a fresh turn, not a resume: the full script
    // replays inside the fresh slice.
    handle.send(request(4, "session/prompt", prompt_params(&session_id, "again")));
    let result = request_result(next_response_frame(&handle).await);
    assert_eq!(result["stopReason"], "end_turn", "fresh prompt after discard: {result}");

    handle.shutdown(task).await;
}

#[tokio::test]
async fn provider_adapter_resume_command_requires_fresh_prompt_after_durable_redaction() {
    let model = Arc::new(DelayedModel::new(
        std::time::Duration::from_millis(5_000),
        std::time::Duration::from_millis(1),
        resume_script(),
    ));
    let provider = recovery_provider(model.clone(), 0);
    let (handle, task) = spawn_server(provider);
    let session_id = new_session(&handle, 1).await;

    // Prompt 1 times out and leaves a pending checkpoint.
    handle.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
    let frame = next_response_frame(&handle).await;
    let Response::Error { .. } = unwrap_response(frame) else {
        panic!("expected a recoverable error");
    };

    // `/resume` carries no user content. Durable checkpoints omit the
    // original transcript, so recovery fails closed until caller supplies
    // a fresh prompt or explicitly abandons with `/discard`.
    handle.send(request(3, "session/prompt", prompt_params(&session_id, "/resume")));
    let error = request_error(next_response_frame(&handle).await);
    assert!(error.message.contains("omits transcript content"), "{error:?}");
    assert!(model.inner.requests().is_empty(), "resume must not call model blindly");

    handle.shutdown(task).await;
}
#[tokio::test]
async fn provider_adapter_resume_command_without_pending_is_ordinary_prompt() {
    let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("ok").completed()]));
    let provider = recovery_provider(
        Arc::new(DelayedModel::new(
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
            (*model).clone(),
        )),
        0,
    );
    let (handle, task) = spawn_server(provider);
    let session_id = new_session(&handle, 1).await;

    handle.send(request(2, "session/prompt", prompt_params(&session_id, "/resume")));
    let result = request_result(next_response_frame(&handle).await);
    assert_eq!(result["stopReason"], "end_turn", "ordinary turn runs: {result}");
    let requests = model.requests();
    assert!(
        requests.iter().any(|request| {
            request.transcript.iter().any(|message| message.text_content() == "/resume")
        }),
        "without a pending checkpoint /resume reaches the model as text"
    );

    handle.shutdown(task).await;
}
#[tokio::test]
async fn provider_adapter_session_resume_restores_pending_turn_without_replay() {
    let dir = tempfile::TempDir::new().expect("checkpoint dir");
    let model = Arc::new(DelayedModel::new(
        std::time::Duration::from_millis(5_000),
        std::time::Duration::from_millis(1),
        resume_script(),
    ));
    // Provider A pauses the turn and persists a durable checkpoint.
    let provider_a = durable_recovery_provider(model.clone(), dir.path());
    let (handle_a, task_a) = spawn_server(provider_a);
    let session_id = new_session(&handle_a, 1).await;
    handle_a.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
    let frame = next_response_frame(&handle_a).await;
    let Response::Error { .. } = unwrap_response(frame) else {
        panic!("expected a recoverable error");
    };
    handle_a.shutdown(task_a).await;

    // Provider B (fresh process) restores via session/resume: no replay
    // updates precede its mode advertisement response.
    let provider_b = durable_recovery_provider(model, dir.path());
    let (handle_b, task_b) = spawn_server(provider_b);
    handle_b.send(request(
        1,
        "session/resume",
        json!({ "sessionId": session_id, "cwd": "/work", "mcpServers": [] }),
    ));
    let result = request_result(handle_b.next_frame().await);
    assert_eq!(result["modes"]["currentModeId"], ASK_MODE_ID, "resume mode: {result}");
    assert!(handle_b.outbound().is_empty(), "no replay updates before the resume response");

    // The next prompt continues the paused turn from the checkpoint.
    handle_b.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
    let result = request_result(next_response_frame(&handle_b).await);
    assert_eq!(result["stopReason"], "end_turn", "paused turn resumes after session/resume");

    handle_b.shutdown(task_b).await;
}

#[tokio::test]
async fn provider_adapter_session_resume_without_pending_state_is_rejected() {
    let dir = tempfile::TempDir::new().expect("checkpoint dir");
    let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("done").completed()]));
    // Provider A completes the turn: the checkpoint is cleared.
    let provider_a = durable_recovery_provider(
        Arc::new(DelayedModel::new(
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
            (*model).clone(),
        )),
        dir.path(),
    );
    let (handle_a, task_a) = spawn_server(provider_a);
    let session_id = new_session(&handle_a, 1).await;
    handle_a.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
    let result = request_result(next_response_frame(&handle_a).await);
    assert_eq!(result["stopReason"], "end_turn");
    handle_a.shutdown(task_a).await;

    // Provider B has no in-memory state and no pending checkpoint.
    let provider_b = durable_recovery_provider(
        Arc::new(DelayedModel::new(
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
            (*model).clone(),
        )),
        dir.path(),
    );
    let (handle_b, task_b) = spawn_server(provider_b);
    handle_b.send(request(
        1,
        "session/resume",
        json!({ "sessionId": session_id, "cwd": "/work", "mcpServers": [] }),
    ));
    let error = request_error(handle_b.next_frame().await);
    assert!(
        error.message.contains("no pending checkpoint"),
        "resume without pending state is rejected: {error:?}"
    );

    handle_b.shutdown(task_b).await;
}

#[tokio::test]
async fn provider_adapter_load_after_crash_replays_checkpoint_transcript() {
    let dir = tempfile::TempDir::new().expect("checkpoint dir");
    let model = Arc::new(DelayedModel::new(
        std::time::Duration::from_millis(5_000),
        std::time::Duration::from_millis(1),
        resume_script(),
    ));
    // Provider A pauses the turn; the checkpoint holds the transcript
    // tail (the user message).
    let provider_a = durable_recovery_provider(model.clone(), dir.path());
    let (handle_a, task_a) = spawn_server(provider_a);
    let session_id = new_session(&handle_a, 1).await;
    handle_a.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
    let frame = next_response_frame(&handle_a).await;
    let Response::Error { .. } = unwrap_response(frame) else {
        panic!("expected a recoverable error");
    };
    handle_a.shutdown(task_a).await;

    // Provider B loads from the checkpoint store: the pending turn's
    // transcript tail is replayed as the crash-restore conversation.
    let provider_b = durable_recovery_provider(model, dir.path());
    let (handle_b, task_b) = spawn_server(provider_b);
    handle_b.send(request(
        1,
        "session/load",
        json!({ "sessionId": session_id, "cwd": "/work", "mcpServers": [] }),
    ));
    let mut replayed_user_texts = Vec::new();
    let mut commands_seen = false;
    let mut response = None;
    for _ in 0..10 {
        let frame = handle_b.next_frame_real().await;
        match &frame {
            RawJsonRpcMessage::Notification(update) => {
                let params = raw_params_to_value(update.params.clone());
                match params["update"]["sessionUpdate"].as_str() {
                    Some("user_message_chunk") => {
                        replayed_user_texts.push(
                            params["update"]["content"]["text"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string(),
                        );
                    }
                    Some("available_commands_update") => commands_seen = true,
                    _ => {}
                }
            }
            RawJsonRpcMessage::Response(_) => {
                response = Some(frame);
                break;
            }
            _ => {}
        }
    }
    assert!(
        replayed_user_texts.is_empty(),
        "crash restore must not replay durable transcript text: {replayed_user_texts:?}"
    );
    assert!(commands_seen, "loaded providers re-advertise their commands");
    let result = request_result(response.expect("load response arrives after replay"));
    assert_eq!(result["modes"]["currentModeId"], ASK_MODE_ID, "load mode: {result}");

    // The loaded session resumes the paused turn on the next prompt.
    handle_b.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
    let result = request_result(next_response_frame(&handle_b).await);
    assert_eq!(result["stopReason"], "end_turn", "paused turn resumes after crash load");

    handle_b.shutdown(task_b).await;
}

#[tokio::test]
async fn provider_adapter_initialize_advertises_resume_capability_only_with_durable_recovery() {
    let plain = Arc::new(FakeModel::new(Vec::new()));
    let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), plain.clone());
    let (handle, task) = spawn_server(provider);
    handle.send(request(1, "initialize", json!({ "protocolVersion": 1 })));
    let result = request_result(handle.next_frame().await);
    assert_eq!(
        result["agentCapabilities"]["sessionCapabilities"]["resume"],
        Value::Null,
        "no resume advertisement without recovery"
    );
    handle.shutdown(task).await;

    let recovered = Arc::new(FakeModel::new(Vec::new()));
    let provider = recovery_provider(
        Arc::new(DelayedModel::new(
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
            (*recovered).clone(),
        )),
        0,
    );
    let (handle, task) = spawn_server(provider);
    handle.send(request(1, "initialize", json!({ "protocolVersion": 1 })));
    let result = request_result(handle.next_frame().await);
    assert_eq!(
        result["agentCapabilities"]["sessionCapabilities"]["resume"],
        Value::Null,
        "memory-only recovery never implies crash-resumable ACP state"
    );
    handle.shutdown(task).await;

    let dir = tempfile::TempDir::new().expect("checkpoint dir");
    let provider = durable_recovery_provider(
        Arc::new(DelayedModel::new(
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
            FakeModel::new(Vec::new()),
        )),
        dir.path(),
    );
    let (handle, task) = spawn_server(provider);
    handle.send(request(1, "initialize", json!({ "protocolVersion": 1 })));
    let result = request_result(handle.next_frame().await);
    assert_eq!(
        result["agentCapabilities"]["sessionCapabilities"]["resume"],
        json!({}),
        "durable recovery advertises session/resume"
    );
    handle.shutdown(task).await;
}

#[tokio::test]
async fn provider_adapter_new_session_avoids_durable_checkpoint_collision() {
    let dir = tempfile::TempDir::new().expect("checkpoint dir");
    let model = Arc::new(DelayedModel::new(
        std::time::Duration::from_millis(5_000),
        std::time::Duration::from_millis(1),
        resume_script(),
    ));
    // Provider A creates `session-1` and pauses it (durable checkpoint).
    let provider_a = durable_recovery_provider(model.clone(), dir.path());
    let (handle_a, task_a) = spawn_server(provider_a);
    let session_id = new_session(&handle_a, 1).await;
    assert_eq!(session_id, "session-1");
    handle_a.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
    let frame = next_response_frame(&handle_a).await;
    let Response::Error { .. } = unwrap_response(frame) else {
        panic!("expected a recoverable error");
    };
    handle_a.shutdown(task_a).await;

    // Provider B (fresh process) must not shadow the reconnected
    // `session-1` when creating a new session.
    let provider_b = durable_recovery_provider(model, dir.path());
    let (handle_b, task_b) = spawn_server(provider_b);
    handle_b.send(request(1, "session/new", session_new_params("/work")));
    let result = request_result(handle_b.next_frame().await);
    assert_eq!(result["sessionId"], "session-2", "fresh id skips durable ids: {result}");
    // Subsequent allocations stay monotonic past the durable base (drain
    // the first session's command advertisement first).
    let frame = handle_b.next_frame().await;
    let RawJsonRpcMessage::Notification(update) = &frame else {
        panic!("expected the available_commands_update, got {frame:?}");
    };
    assert_eq!(
        raw_params_to_value(update.params.clone())["update"]["sessionUpdate"],
        "available_commands_update"
    );
    handle_b.send(request(2, "session/new", session_new_params("/work")));
    let result = request_result(handle_b.next_frame().await);
    assert_eq!(result["sessionId"], "session-3", "ids stay monotonic: {result}");

    handle_b.shutdown(task_b).await;
}

#[tokio::test]
async fn provider_adapter_load_after_process_restart_restores_durable_session() {
    let state_dir = tempfile::TempDir::new().expect("session state dir");
    let workspace = tempfile::TempDir::new().expect("workspace");
    let workspace = workspace.path().to_string_lossy().into_owned();
    let provider_a = OrchestratorProvider::new(
        OrchestratorProviderConfig {
            session_state_dir: Some(state_dir.path().to_path_buf()),
            ..OrchestratorProviderConfig::default()
        },
        Arc::new(FakeModel::new(vec![ModelResponse::new().text("done").completed()])),
    );
    let (handle_a, task_a) = spawn_server(provider_a);
    handle_a.send(request(1, "session/new", session_new_params(&workspace)));
    let session_id = request_result(handle_a.next_frame().await)["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    let _ = handle_a.next_frame().await; // available commands update
    handle_a.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
    let result = request_result(next_response_frame(&handle_a).await);
    assert_eq!(result["stopReason"], "end_turn");
    // EOF drops provider A without ACP `session/close`, mirroring
    // `/quit_full` stopping its child agent process.
    handle_a.shutdown(task_a).await;

    let provider_b = OrchestratorProvider::new(
        OrchestratorProviderConfig {
            session_state_dir: Some(state_dir.path().to_path_buf()),
            ..OrchestratorProviderConfig::default()
        },
        Arc::new(FakeModel::new(Vec::new())),
    );
    let (handle_b, task_b) = spawn_server(provider_b);
    handle_b.send(request(1, "session/new", session_new_params(&workspace)));
    let new_session = request_result(handle_b.next_frame().await);
    assert_eq!(new_session["sessionId"], "session-2");
    let _ = handle_b.next_frame().await; // available commands update
    handle_b.send(request(
        2,
        "session/load",
        json!({ "sessionId": session_id, "cwd": workspace, "mcpServers": [] }),
    ));
    let mut replayed = Vec::new();
    let mut response = None;
    for _ in 0..10 {
        let frame = handle_b.next_frame_real().await;
        match &frame {
            RawJsonRpcMessage::Notification(update) => {
                let params = raw_params_to_value(update.params.clone());
                let kind = params["update"]["sessionUpdate"].as_str();
                if matches!(kind, Some("user_message_chunk") | Some("agent_message_chunk")) {
                    replayed.push(params["update"]["content"]["text"].clone());
                }
            }
            RawJsonRpcMessage::Response(_) => {
                response = Some(frame);
                break;
            }
            _ => {}
        }
    }
    let result = request_result(response.expect("load response"));
    assert_eq!(result["modes"]["currentModeId"], ASK_MODE_ID);
    assert!(replayed.is_empty(), "durable session load must not replay transcript: {replayed:?}");
    handle_b.shutdown(task_b).await;
}
