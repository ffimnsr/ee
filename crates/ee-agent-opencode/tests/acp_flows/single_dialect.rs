//! Detailed single-dialect ACP flows.

use super::*;

#[tokio::test]
async fn initialize_reports_the_opencode_agent_identity() {
    let dirs = TestDirs::new();
    let (_codec, harness, task) = spawn_agent(
        OpenCodeSurface::Zen,
        OpenCodeDialect::OpenAiResponses,
        vec![ScriptedAnswer::text("unused")],
        &dirs,
    );

    initialize(&harness).await;

    harness.shutdown(task).await;
}

#[tokio::test]
async fn prompt_streams_thought_then_answer_for_the_configured_route() {
    let dirs = TestDirs::new();
    let (codec, harness, task) = spawn_agent(
        OpenCodeSurface::Go,
        OpenCodeDialect::AnthropicMessages,
        vec![ScriptedAnswer::reasoning("weighing options", "the answer")],
        &dirs,
    );
    initialize(&harness).await;
    let session_id = new_session(&harness, 2).await;

    harness.send(request(3, "session/prompt", prompt_params(&session_id, "hello")));
    drain_prompt_start(&harness).await;
    let (updates, result) = harness.collect_until_response(3).await;

    assert_eq!(result["stopReason"], "end_turn");
    assert_eq!(update_text(&updates, "agent_thought_chunk"), "weighing options");
    assert!(has_text_chunk(&updates, "agent_message_chunk", "the answer"));
    // The host appends its own completion report after the model's answer.
    assert!(update_text(&updates, "agent_message_chunk").contains("the answer"));
    let recorded = codec.requests();
    assert_eq!(recorded.len(), 1, "one model round for one answer");
    assert!(!recorded[0].tools.is_empty(), "the routed tools reach the codec");

    harness.shutdown(task).await;
}

#[tokio::test]
async fn tool_call_runs_through_the_client_bridge_and_continues_the_turn() {
    let dirs = TestDirs::new();
    let (codec, harness, task) = spawn_agent(
        OpenCodeSurface::Zen,
        OpenCodeDialect::OpenAiChatCompletions,
        vec![
            ScriptedAnswer::tool_call("call_1", "read_file", json!({ "path": "/tmp/notes.txt" })),
            ScriptedAnswer::text("read it"),
        ],
        &dirs,
    );
    initialize(&harness).await;
    let session_id = new_session(&harness, 2).await;

    harness.send(request(3, "session/prompt", prompt_params(&session_id, "read a file")));
    drain_prompt_start(&harness).await;
    let (updates, bridge_request) = harness.collect_until_client_request("fs/read_text_file").await;

    // The normalized tool call reached the client before the tool ran.
    assert!(update_kinds(&updates).contains(&String::from("tool_call")));
    let params = raw_params_to_value(match &bridge_request {
        RawJsonRpcMessage::Request(request) => request.params.clone(),
        other => panic!("expected the bridge request, got {other:?}"),
    });
    assert_eq!(params["sessionId"], session_id);
    assert_eq!(params["path"], "/tmp/notes.txt");

    let RawJsonRpcMessage::Request(request) = &bridge_request else {
        panic!("expected the bridge request");
    };
    harness.send(RawJsonRpcMessage::response(
        request.id.clone(),
        Ok(json!({ "content": "notes from the workspace" })),
    ));
    let (updates, result) = harness.collect_until_response(3).await;

    assert_eq!(result["stopReason"], "end_turn");
    assert!(update_kinds(&updates).contains(&String::from("tool_call_update")));
    assert!(has_text_chunk(&updates, "agent_message_chunk", "read it"));

    // The second model round carried the tool observation, with the model tool
    // call id preserved for correlation.
    let recorded = codec.requests();
    assert_eq!(recorded.len(), 2, "the turn continued after the tool result");
    let second = recorded[1].json();
    let transcript = second["transcript"].to_string();
    assert!(transcript.contains("notes from the workspace"), "{transcript}");
    assert!(transcript.contains("call_1"), "tool-call identity is preserved: {transcript}");

    harness.shutdown(task).await;
}

#[tokio::test]
async fn session_cancel_stops_a_pending_model_turn() {
    let dirs = TestDirs::new();
    let (codec, harness, task) = spawn_agent(
        OpenCodeSurface::Zen,
        OpenCodeDialect::OpenAiResponses,
        vec![ScriptedAnswer::Pending],
        &dirs,
    );
    initialize(&harness).await;
    let session_id = new_session(&harness, 2).await;

    harness.send(request(3, "session/prompt", prompt_params(&session_id, "wait")));
    drain_prompt_start(&harness).await;
    wait_for_requests(&codec, 1).await;
    harness.send(notification("session/cancel", json!({ "sessionId": session_id })));
    let (updates, result) = harness.collect_until_response(3).await;

    assert_eq!(result["stopReason"], "cancelled");
    assert_eq!(codec.requests().len(), 1, "the call started once and was not retried");
    assert!(
        !update_kinds(&updates).contains(&String::from("agent_message_chunk")),
        "a cancelled turn emits no answer"
    );

    harness.shutdown(task).await;
}

#[tokio::test]
async fn missing_credential_fails_the_prompt_without_a_model_request() {
    let dirs = TestDirs::new();
    // The credential now comes from the configuration, exactly as in production.
    let mut config = test_config(OpenCodeSurface::Go, "kimi-k3");
    config.api_key = None;
    let (codec, harness, task) =
        spawn_agent_with_config(config, vec![ScriptedAnswer::text("never sent")], &dirs);
    initialize(&harness).await;
    let session_id = new_session(&harness, 2).await;

    harness.send(request(3, "session/prompt", prompt_params(&session_id, "hello")));
    drain_prompt_start(&harness).await;
    let (updates, error) = harness.collect_until_error(3).await;

    assert!(error.message.contains("OPENCODE_API_KEY is not set"), "{}", error.message);
    assert!(
        !update_kinds(&updates).contains(&String::from("agent_message_chunk")),
        "a failed turn emits no answer"
    );
    assert!(codec.requests().is_empty(), "no request exists without a credential");

    harness.shutdown(task).await;
}

#[tokio::test]
async fn session_close_ends_the_session_and_the_server_exits_on_eof() {
    let dirs = TestDirs::new();
    let (_codec, harness, task) = spawn_agent(
        OpenCodeSurface::Zen,
        OpenCodeDialect::OpenAiResponses,
        vec![ScriptedAnswer::text("unused")],
        &dirs,
    );
    initialize(&harness).await;
    let session_id = new_session(&harness, 2).await;

    harness.send(request(3, "session/close", json!({ "sessionId": session_id })));
    assert_eq!(result_of(harness.next_frame().await), json!({}));

    harness.shutdown(task).await;
}
