//! Host-flow tests: request.
use super::*;

#[tokio::test]
async fn permission_request_flows_through_broker_and_back() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "checking")))
        .emit(wire::request_permission(
            "s1",
            "call_1",
            "Run tests",
            json!([
                { "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" },
                { "optionId": "deny", "name": "Deny", "kind": "reject_once" }
            ]),
        ))
        .respond(json!({ "stopReason": "end_turn" }));

    let (fake, mut host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let prompt_thread = thread.clone();
    let prompt = tokio::spawn(async move {
        prompt_thread.send_prompt(vec![ContentBlock::Text(TextContent::new("run tests"))]).await
    });

    // The host surfaces the permission request to the UI.
    let request_id = loop {
        match next_event(&mut host.events).await {
            AgentEvent::PermissionRequested { session_id, request } => {
                assert_eq!(session_id, SessionId::new("s1"));
                assert_eq!(request.tool_call.fields.title.as_deref(), Some("Run tests"));
                assert_eq!(request.options.len(), 2);
                break request.request_id;
            }
            _ => continue,
        }
    };

    // The UI answers; the agent sees the selected outcome on the wire.
    let outcome = RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("allow_once"));
    assert!(thread.respond_permission(request_id, outcome.clone()));
    // Duplicate response is ignored.
    assert!(!thread.respond_permission(request_id, outcome));

    let response = prompt.await.expect("prompt task").expect("turn completes");
    assert_eq!(response.stop_reason, StopReason::EndTurn);

    // The fake agent recorded the host's permission response (request id 100
    // in the wire helpers).
    let permission_response = await_response(&fake, 100).await;
    assert_eq!(permission_response["result"]["outcome"]["outcome"], "selected");
    assert_eq!(permission_response["result"]["outcome"]["optionId"], "allow_once");

    // A PermissionResolved event was emitted.
    let mut resolved = false;
    while let Ok(Some(event)) = tokio::time::timeout(TEST_TIMEOUT, host.events.recv()).await {
        if matches!(event, AgentEvent::PermissionResolved { .. }) {
            resolved = true;
            break;
        }
    }
    assert!(resolved);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn cancel_resolves_pending_permissions_as_cancelled() {
    let script = base_script().wait_for("session/prompt").emit(wire::request_permission(
        "s1",
        "call_1",
        "Run tests",
        json!([{ "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" }]),
    ));
    let (fake, mut host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let prompt_thread = thread.clone();
    let prompt = tokio::spawn(async move {
        prompt_thread.send_prompt(vec![ContentBlock::Text(TextContent::new("go"))]).await
    });

    loop {
        match next_event(&mut host.events).await {
            AgentEvent::PermissionRequested { .. } => break,
            _ => continue,
        }
    }

    thread.cancel().await.expect("cancel succeeds");
    assert!(prompt.await.expect("prompt task").is_err());
    assert_eq!(connection.permission_broker().pending_count(), 0);

    // The agent saw a Cancelled outcome for the permission request.
    let permission_response = await_response(&fake, 100).await;
    assert_eq!(permission_response["result"]["outcome"]["outcome"], "cancelled");
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn fs_and_terminal_optional_requests_route_and_serialize_expected_shapes() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::read_text_file("s1", "/work/Cargo.toml"))
        .emit(wire::write_text_file("s1", "/work/Cargo.toml", "new"))
        .emit(wire::terminal_create("s1", "cargo test"))
        .emit(wire::terminal_output("s1", "term-1"))
        .emit(wire::terminal_wait_for_exit("s1", "term-1"))
        .emit(wire::terminal_kill("s1", "term-1"))
        .emit(wire::terminal_release("s1", "term-1"))
        .respond(json!({ "stopReason": "end_turn" }));

    let handler = ScriptedHandler::new(HandlerCapabilities {
        fs_read: true,
        fs_write: true,
        terminal: true,
        ..HandlerCapabilities::none()
    });
    let (fake, host) = spawn_host(script, Arc::new(handler.clone())).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    thread
        .send_prompt(vec![ContentBlock::Text(TextContent::new("go"))])
        .await
        .expect("turn completes");

    let seen = handler.seen();
    assert_eq!(
        seen.iter().map(ClientRequest::method).collect::<Vec<_>>(),
        vec![
            "fs/read_text_file",
            "fs/write_text_file",
            "terminal/create",
            "terminal/output",
            "terminal/wait_for_exit",
            "terminal/kill",
            "terminal/release",
        ]
    );

    assert_eq!(await_response(&fake, 101).await["result"]["content"], "file contents");
    assert!(await_response(&fake, 103).await.get("result").is_some());
    assert_eq!(await_response(&fake, 102).await["result"]["terminalId"], "term-scripted");
    assert_eq!(await_response(&fake, 104).await["result"]["output"], "stdout");
    assert_eq!(await_response(&fake, 104).await["result"]["truncated"], false);
    assert_eq!(await_response(&fake, 105).await["result"]["exitCode"], 0);
    assert!(await_response(&fake, 106).await.get("result").is_some());
    assert!(await_response(&fake, 107).await.get("result").is_some());
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn unadvertised_fs_write_is_rejected_before_handler_invocation() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::write_text_file("s1", "/work/Cargo.toml", "new"))
        .respond(json!({ "stopReason": "end_turn" }));
    let handler = RecordingHandler::new(HandlerCapabilities::none());
    let (fake, host) = spawn_host(script, Arc::new(handler.clone())).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();
    thread.send_prompt(vec![ContentBlock::Text(TextContent::new("go"))]).await.unwrap();

    assert!(handler.seen().is_empty());
    assert_method_not_found(&await_response(&fake, 103).await);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn unadvertised_terminal_requests_are_rejected_before_handler_invocation() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::terminal_output("s1", "term-1"))
        .emit(wire::terminal_wait_for_exit("s1", "term-1"))
        .emit(wire::terminal_kill("s1", "term-1"))
        .emit(wire::terminal_release("s1", "term-1"))
        .respond(json!({ "stopReason": "end_turn" }));
    let handler = RecordingHandler::new(HandlerCapabilities::none());
    let (fake, host) = spawn_host(script, Arc::new(handler.clone())).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();
    thread.send_prompt(vec![ContentBlock::Text(TextContent::new("go"))]).await.unwrap();

    assert!(handler.seen().is_empty());
    assert_method_not_found(&await_response(&fake, 104).await);
    assert_method_not_found(&await_response(&fake, 105).await);
    assert_method_not_found(&await_response(&fake, 106).await);
    assert_method_not_found(&await_response(&fake, 107).await);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn elicitation_form_routes_when_form_capability_is_advertised() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::elicitation_form(
            "s1",
            json!({
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }),
            "fill the form",
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let handler = ScriptedHandler::new(HandlerCapabilities {
        elicitation_form: true,
        ..HandlerCapabilities::none()
    });
    let (fake, host) = spawn_host(script, Arc::new(handler.clone())).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();
    thread.send_prompt(vec![ContentBlock::Text(TextContent::new("go"))]).await.unwrap();

    let seen = handler.seen();
    assert_eq!(seen.len(), 1);
    assert!(matches!(seen[0], ClientRequest::CreateElicitation(_)));
    let response = await_response(&fake, 108).await;
    assert_eq!(response["result"]["action"], "accept");
    assert_eq!(response["result"]["content"]["name"], "ed");
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn elicitation_url_routes_when_url_capability_is_advertised() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::elicitation_url("s1", "el-1", "https://example.com/authorize", "authorize"))
        .respond(json!({ "stopReason": "end_turn" }));
    let handler = ScriptedHandler::new(HandlerCapabilities {
        elicitation_url: true,
        ..HandlerCapabilities::none()
    });
    let (fake, host) = spawn_host(script, Arc::new(handler.clone())).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();
    thread.send_prompt(vec![ContentBlock::Text(TextContent::new("go"))]).await.unwrap();

    let seen = handler.seen();
    assert_eq!(seen.len(), 1);
    assert!(matches!(seen[0], ClientRequest::CreateElicitation(_)));
    let response = await_response(&fake, 109).await;
    assert_eq!(response["result"]["action"], "accept");
    assert!(response["result"].get("content").is_none());
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn unadvertised_elicitation_modes_are_rejected_before_handler_invocation() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::elicitation_form("s1", json!({ "type": "object", "properties": {} }), "fill"))
        .emit(wire::elicitation_url("s1", "el-1", "https://example.com/authorize", "open"))
        .respond(json!({ "stopReason": "end_turn" }));
    let handler = RecordingHandler::new(HandlerCapabilities::none());
    let (fake, host) = spawn_host(script, Arc::new(handler.clone())).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();
    thread.send_prompt(vec![ContentBlock::Text(TextContent::new("go"))]).await.unwrap();

    assert!(handler.seen().is_empty());
    assert_invalid_params(&await_response(&fake, 108).await);
    assert_invalid_params(&await_response(&fake, 109).await);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}
