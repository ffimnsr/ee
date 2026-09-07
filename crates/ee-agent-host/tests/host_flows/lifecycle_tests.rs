//! Host-flow tests: lifecycle.
use super::*;

#[tokio::test]
async fn initialize_does_not_advertise_custom_standard_looking_capabilities() {
    let initialize = initialize_request_for_capabilities(HandlerCapabilities::all()).await;
    let capabilities =
        initialize["params"]["clientCapabilities"].as_object().expect("clientCapabilities object");
    assert!(
        !capabilities.contains_key("proxyDiscovery"),
        "unexpected custom capability: {initialize}"
    );
    assert!(!capabilities.contains_key("ee"), "unexpected custom capability: {initialize}");
}

#[tokio::test]
async fn happy_path_streams_updates_and_completes_turn() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "Hel")))
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "lo")))
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call_1",
                "title": "Run tests",
            }),
        ))
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "usage_update",
                "used": 10,
                "size": 100,
            }),
        ))
        .respond(json!({ "stopReason": "end_turn" }));

    let (fake, mut host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let thread = connection
        .new_session(vec![PathBuf::from("/work"), PathBuf::from("/extra")], Vec::new(), None)
        .await
        .expect("session/new succeeds");

    let response = thread
        .send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))])
        .await
        .expect("prompt completes");
    assert_eq!(response.stop_reason, StopReason::EndTurn);

    // Reduced state: optimistic user message + one assistant message with
    // two merged blocks; tool call and usage recorded.
    let snapshot = thread.snapshot();
    assert_eq!(snapshot.messages.len(), 2);
    assert_eq!(snapshot.messages[0].kind, MessageKind::User);
    assert_eq!(snapshot.messages[1].kind, MessageKind::Assistant);
    assert_eq!(snapshot.messages[1].message_id.as_deref(), Some("m1"));
    assert_eq!(snapshot.messages[1].blocks.len(), 2);
    assert_eq!(snapshot.tool_calls["call_1"].title, "Run tests");
    assert_eq!(snapshot.usage.as_ref().map(|usage| usage.used), Some(10));

    // Wire contract: method names, protocol version, cwd + additional dirs.
    assert_eq!(fake.requests_by_method("initialize").len(), 1);
    let initialize = &fake.requests_by_method("initialize")[0];
    assert_eq!(initialize["params"]["protocolVersion"], 1);
    assert_eq!(initialize["params"]["clientInfo"]["name"], "ee");
    let session_new = &fake.requests_by_method("session/new")[0];
    assert_eq!(session_new["params"]["cwd"], "/work");
    assert_eq!(session_new["params"]["additionalDirectories"], json!(["/extra"]));
    let prompt = &fake.requests_by_method("session/prompt")[0];
    assert_eq!(prompt["params"]["sessionId"], "s1");
    assert_eq!(prompt["params"]["prompt"][0]["text"], "hi");

    // Deterministic event stream.
    assert!(matches!(
        next_event(&mut host.events).await,
        AgentEvent::ConnectionStateChanged {
            state: ee_agent_host::AgentConnectionState::Ready { .. },
            ..
        }
    ));
    assert!(matches!(next_event(&mut host.events).await, AgentEvent::ThreadCreated { .. }));
    assert!(matches!(next_event(&mut host.events).await, AgentEvent::TurnStarted { .. }));
    assert!(matches!(next_event(&mut host.events).await, AgentEvent::SessionUpdate { .. }));
    assert!(matches!(next_event(&mut host.events).await, AgentEvent::SessionUpdate { .. }));
    assert!(matches!(next_event(&mut host.events).await, AgentEvent::SessionUpdate { .. }));
    assert!(matches!(next_event(&mut host.events).await, AgentEvent::SessionUpdate { .. }));
    assert!(matches!(
        next_event(&mut host.events).await,
        AgentEvent::TurnCompleted { stop_reason: StopReason::EndTurn, .. }
    ));

    connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn turn_completed_carries_elapsed_time_and_reported_tokens() {
    let script = base_script().wait_for("session/prompt").respond(json!({
        "stopReason": "end_turn",
        "usage": {
            "totalTokens": 8431,
            "inputTokens": 6120,
            "outputTokens": 2311,
        }
    }));
    let (fake, mut host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    thread
        .send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))])
        .await
        .expect("prompt completes");

    let metrics = loop {
        match next_event(&mut host.events).await {
            AgentEvent::TurnCompleted { metrics, .. } => break metrics,
            AgentEvent::TurnStarted { .. }
            | AgentEvent::SessionUpdate { .. }
            | AgentEvent::ConnectionStateChanged { .. }
            | AgentEvent::ThreadCreated { .. } => continue,
            other => panic!("unexpected event: {other:?}"),
        }
    };
    assert!(!metrics.elapsed.is_zero(), "elapsed must be measured");
    let tokens = metrics.tokens.expect("reported usage attached");
    assert_eq!(tokens.total_tokens, 8431);
    assert_eq!(tokens.input_tokens, 6120);
    assert_eq!(tokens.output_tokens, 2311);

    connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn cancelled_turn_carries_elapsed_time_and_no_tokens() {
    let script = base_script()
        .wait_for("session/prompt")
        // Agent never answers: the turn is cancelled locally.
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "thinking...")));
    let (fake, mut host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let prompt_thread = thread.clone();
    let prompt = tokio::spawn(async move {
        prompt_thread.send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))]).await
    });
    tokio::time::timeout(TEST_TIMEOUT, async {
        while !fake.log_contains("\"method\":\"session/prompt\"") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("prompt request observed");
    thread.cancel().await.expect("cancel succeeds");
    assert!(matches!(prompt.await.expect("prompt task joins"), Err(AgentError::Cancelled)));

    let metrics = loop {
        match next_event(&mut host.events).await {
            AgentEvent::TurnCancelled { metrics, .. } => break metrics,
            AgentEvent::TurnStarted { .. }
            | AgentEvent::SessionUpdate { .. }
            | AgentEvent::ConnectionStateChanged { .. }
            | AgentEvent::ThreadCreated { .. } => continue,
            other => panic!("unexpected event: {other:?}"),
        }
    };
    assert!(!metrics.elapsed.is_zero(), "elapsed must be measured");
    assert_eq!(metrics.tokens, None, "cancelled turns report no tokens");

    connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn unsupported_protocol_version_fails_closed() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 2, "agentCapabilities": {} }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;

    let error = host.connection.wait_ready().await.unwrap_err();
    assert!(matches!(
        error,
        AgentError::UnsupportedProtocolVersion { ref agent_id, .. } if agent_id == "fake"
    ));
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn rejected_initialize_fails_closed() {
    let script = FakeAgentScript::new().wait_for("initialize").respond_error(-32602, "bad request");
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;

    let error = host.connection.wait_ready().await.unwrap_err();
    assert!(matches!(error, AgentError::Rpc(_)));
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn malformed_json_from_agent_gets_parse_error_response() {
    let script = base_script()
        .emit_raw("{not valid json")
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }));

    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();
    thread
        .send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))])
        .await
        .expect("turn still works after malformed line");

    // The host answered the malformed line with a JSON-RPC parse error and
    // kept the connection alive.
    assert!(
        fake.log_contains("\"code\":-32700"),
        "expected parse-error response in fake log: {:?}",
        fake.log()
    );
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn unknown_custom_request_returns_method_not_found() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(json!({
            "jsonrpc": "2.0",
            "id": 104,
            "method": "_ee/unknown_request",
            "params": { "trace": "diag-only" }
        }))
        .respond(json!({ "stopReason": "end_turn" }));

    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    thread
        .send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))])
        .await
        .expect("prompt still completes");

    let response = await_response(&fake, 104).await;
    assert_method_not_found(&response);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn unknown_custom_notification_is_ignored() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(json!({
            "jsonrpc": "2.0",
            "method": "_ee/unknown_notification",
            "params": { "traceparent": "00-abc-def-01" }
        }))
        .respond(json!({ "stopReason": "end_turn" }));

    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let response = thread
        .send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))])
        .await
        .expect("prompt still completes after unknown notification");
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn oversized_agent_message_fails_the_connection_without_panic() {
    // Phase 7 resource limit: a line beyond `MAX_ACP_MESSAGE_BYTES` must
    // fail the connection as a typed transport error, never be parsed.
    let huge = "x".repeat(ee_agent_host::fake::MAX_ACP_MESSAGE_BYTES + 1);
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        // Give the client time to consume the initialize response before the
        // oversized line arrives (the cap error ends the incoming stream).
        .delay(150)
        .emit_raw(huge);
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;

    // The cap error surfaces as a connection failure (no panic, no hang).
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            match host.connection.state() {
                ee_agent_host::AgentConnectionState::Failed(_)
                | ee_agent_host::AgentConnectionState::Closed(_) => break,
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .expect("connection must fail within the test window");
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn agent_eof_mid_turn_resolves_prompt_with_typed_error() {
    let script = base_script().wait_for("session/prompt").close();
    let (fake, mut host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let error =
        thread.send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))]).await.unwrap_err();
    assert!(
        matches!(error, AgentError::ConnectionClosed { .. } | AgentError::Rpc(_)),
        "unexpected error: {error:?}"
    );
    assert!(!thread.is_turn_running());

    // The thread is reported closed; no prompt can start on a dead thread.
    let mut saw_thread_closed = false;
    while let Ok(Some(event)) = tokio::time::timeout(TEST_TIMEOUT, host.events.recv()).await {
        if matches!(event, AgentEvent::ThreadClosed { .. }) {
            saw_thread_closed = true;
            break;
        }
    }
    assert!(saw_thread_closed);
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn agent_eof_resolves_several_in_flight_prompts_without_hung_waiters() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s2" }))
        .capture(CaptureSource::Request { method: "session/prompt".into() }, "id", "prompt_a")
        .capture(CaptureSource::Request { method: "session/prompt".into() }, "id", "prompt_b")
        .close();
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let a = connection.new_session(vec![PathBuf::from("/a")], Vec::new(), None).await.unwrap();
    let b = connection.new_session(vec![PathBuf::from("/b")], Vec::new(), None).await.unwrap();

    let prompt_a = tokio::spawn(async move {
        a.send_prompt(vec![ContentBlock::Text(TextContent::new("A"))]).await
    });
    let prompt_b = tokio::spawn(async move {
        b.send_prompt(vec![ContentBlock::Text(TextContent::new("B"))]).await
    });
    for prompt in [prompt_a, prompt_b] {
        let error = tokio::time::timeout(TEST_TIMEOUT, prompt)
            .await
            .expect("prompt waiter resolves after EOF")
            .expect("prompt task")
            .unwrap_err();
        assert!(matches!(error, AgentError::ConnectionClosed { .. } | AgentError::Rpc(_)));
    }
    fake.join(TEST_TIMEOUT).await;
}
