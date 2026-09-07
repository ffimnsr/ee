//! Host-flow tests: concurrency.
use super::*;

#[tokio::test]
async fn recoverable_error_surfaces_as_paused_event_with_structured_info() {
    let script = base_script().wait_for("session/prompt").respond_error_with_data(
        -32603,
        "recoverable turn interruption: paused after 300s",
        json!({
            "recoverable": {
                "fault": "deadline",
                "detail": "paused after 300s",
                "cause": null,
                "safe_resume": true,
                "retry_after": null,
                "checkpoint_id": "s-1-0000000003",
                "completed_tool_calls": 4,
                "resumed_count": 0,
            }
        }),
    );
    let (fake, mut host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let error =
        thread.send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))]).await.unwrap_err();
    assert!(matches!(error, AgentError::Rpc(_)), "wire error stays an Rpc error: {error:?}");
    assert!(!thread.is_turn_running(), "the thread stays alive after a pause");

    let mut saw_paused_evidence = false;
    let paused = loop {
        match next_event(&mut host.events).await {
            AgentEvent::TurnPausedRecoverable { recoverable, .. } => break *recoverable,
            AgentEvent::TurnEvidenceUpdated { summary, .. } => {
                assert_eq!(summary.blocker, Some(TurnBlocker::PromptPausedRecoverable));
                assert_eq!(summary.safe_follow_up, SafeFollowUp::ResumeOrDiscard);
                saw_paused_evidence = true;
            }
            AgentEvent::TurnStarted { .. }
            | AgentEvent::SessionUpdate { .. }
            | AgentEvent::ConnectionStateChanged { .. }
            | AgentEvent::ThreadCreated { .. } => continue,
            other => panic!("unexpected event: {other:?}"),
        }
    };
    assert!(saw_paused_evidence);
    assert_eq!(paused.fault, "deadline");
    assert_eq!(paused.detail, "paused after 300s");
    assert!(paused.safe_resume);
    assert_eq!(paused.checkpoint_id.as_deref(), Some("s-1-0000000003"));
    assert_eq!(paused.completed_tool_calls, 4);
    assert_eq!(paused.resumed_count, 0);

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn plain_errors_still_surface_as_turn_failed() {
    let script = base_script().wait_for("session/prompt").respond_error(-32603, "backend exploded");
    let (fake, mut host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let error =
        thread.send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))]).await.unwrap_err();
    assert!(matches!(error, AgentError::Rpc(_)));

    let mut saw_failed = false;
    while let Ok(Some(event)) = tokio::time::timeout(TEST_TIMEOUT, host.events.recv()).await {
        if matches!(event, AgentEvent::TurnFailed { .. }) {
            saw_failed = true;
            break;
        }
    }
    assert!(saw_failed, "plain errors keep the TurnFailed path");
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn lifecycle_requests_on_one_connection_overlap_and_fail_independently() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .capture(CaptureSource::Request { method: "session/new".into() }, "id", "new_a_id")
        .capture(CaptureSource::Request { method: "session/new".into() }, "id", "new_b_id")
        .emit(json!({
            "jsonrpc": "2.0",
            "id": { "$capture": "new_b_id" },
            "result": { "sessionId": "s2" }
        }))
        .emit(json!({
            "jsonrpc": "2.0",
            "id": { "$capture": "new_a_id" },
            "error": { "code": -32603, "message": "first create failed" }
        }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let connection_a = connection.clone();
    let create_a = tokio::spawn(async move {
        connection_a.new_session(vec![PathBuf::from("/work/a")], Vec::new(), None).await
    });
    await_request_count(&fake, "session/new", 1).await;
    let connection_b = connection.clone();
    let create_b = tokio::spawn(async move {
        connection_b.new_session(vec![PathBuf::from("/work/b")], Vec::new(), None).await
    });

    let thread_b = tokio::time::timeout(TEST_TIMEOUT, create_b)
        .await
        .expect("second create completes")
        .expect("second create task")
        .expect("second create succeeds");
    assert_eq!(thread_b.session_id().0.as_ref(), "s2");
    let error_a = tokio::time::timeout(TEST_TIMEOUT, create_a)
        .await
        .expect("first create resolves")
        .expect("first create task")
        .unwrap_err();
    assert!(matches!(error_a, AgentError::Rpc(_)));
    assert_eq!(fake.requests_by_method("session/new").len(), 2);

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn prompts_in_two_sessions_on_one_connection_run_concurrently() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s2" }))
        .capture(CaptureSource::Request { method: "session/prompt".into() }, "id", "prompt_a_id")
        .capture(CaptureSource::Request { method: "session/prompt".into() }, "id", "prompt_b_id")
        .emit(json!({
            "jsonrpc": "2.0",
            "id": { "$capture": "prompt_b_id" },
            "result": { "stopReason": "end_turn" }
        }));
    let (fake, mut host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread_a =
        connection.new_session(vec![PathBuf::from("/work/a")], Vec::new(), None).await.unwrap();
    let thread_b =
        connection.new_session(vec![PathBuf::from("/work/b")], Vec::new(), None).await.unwrap();

    let prompt_thread_a = thread_a.clone();
    let mut prompt_a = tokio::spawn(async move {
        prompt_thread_a.send_prompt(vec![ContentBlock::Text(TextContent::new("A"))]).await
    });
    await_request_count(&fake, "session/prompt", 1).await;

    let prompt_thread_b = thread_b.clone();
    let prompt_b = tokio::spawn(async move {
        prompt_thread_b.send_prompt(vec![ContentBlock::Text(TextContent::new("B"))]).await
    });
    let response_b = tokio::time::timeout(TEST_TIMEOUT, prompt_b)
        .await
        .expect("session B completes while session A remains blocked")
        .expect("session B prompt task")
        .expect("session B prompt succeeds");
    assert_eq!(response_b.stop_reason, StopReason::EndTurn);
    assert!(!prompt_a.is_finished(), "session A must remain pending");
    assert!(thread_a.is_turn_running());
    assert!(!thread_b.is_turn_running());

    let prompts = fake.requests_by_method("session/prompt");
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0]["params"]["sessionId"], "s1");
    assert_eq!(prompts[1]["params"]["sessionId"], "s2");
    assert_eq!(prompts[0]["params"]["prompt"][0]["text"], "A");
    assert_eq!(prompts[1]["params"]["prompt"][0]["text"], "B");

    thread_a.cancel().await.expect("session A cancel succeeds");
    let error_a = tokio::time::timeout(TEST_TIMEOUT, &mut prompt_a)
        .await
        .expect("session A resolves after cancellation")
        .expect("session A prompt task")
        .unwrap_err();
    assert!(matches!(error_a, AgentError::Cancelled));
    assert!(!thread_a.is_turn_running());

    await_request_count(&fake, "session/cancel", 1).await;
    let session_cancels = fake.requests_by_method("session/cancel");
    assert_eq!(session_cancels.len(), 1);
    assert_eq!(session_cancels[0]["params"]["sessionId"], "s1");
    let prompt_a_id = prompts[0]["id"].clone();
    let prompt_b_id = prompts[1]["id"].clone();
    let request_cancels = fake.requests_by_method("$/cancel_request");
    assert!(request_cancels.iter().any(|cancel| cancel["params"]["requestId"] == prompt_a_id));
    assert!(!request_cancels.iter().any(|cancel| cancel["params"]["requestId"] == prompt_b_id));

    let mut completed_b = false;
    let mut cancelled_a = false;
    while !(completed_b && cancelled_a) {
        match next_event(&mut host.events).await {
            AgentEvent::TurnCompleted { session_id, .. } if session_id.0.as_ref() == "s2" => {
                completed_b = true;
            }
            AgentEvent::TurnCancelled { session_id, .. } if session_id.0.as_ref() == "s1" => {
                cancelled_a = true;
            }
            AgentEvent::TurnCancelled { session_id, .. }
            | AgentEvent::TurnFailed { session_id, .. }
                if session_id.0.as_ref() == "s2" =>
            {
                panic!("session B received wrong terminal event")
            }
            _ => {}
        }
    }

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn second_prompt_in_same_session_is_rejected_while_first_is_active() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
        .capture(CaptureSource::Request { method: "session/prompt".into() }, "id", "prompt_id");
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let first_thread = thread.clone();
    let first = tokio::spawn(async move {
        first_thread.send_prompt(vec![ContentBlock::Text(TextContent::new("first"))]).await
    });
    await_request_count(&fake, "session/prompt", 1).await;

    let second = thread.send_prompt(vec![ContentBlock::Text(TextContent::new("second"))]).await;
    assert!(matches!(second, Err(AgentError::TurnAlreadyRunning)));
    assert_eq!(fake.requests_by_method("session/prompt").len(), 1);

    thread.cancel().await.expect("cancel first prompt");
    assert!(matches!(first.await.expect("first task"), Err(AgentError::Cancelled)));
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn prompt_concurrency_limit_queues_cross_session_work_fifo() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s2" }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s3" }))
        .capture(CaptureSource::Request { method: "session/prompt".into() }, "id", "prompt_a")
        .capture(CaptureSource::Request { method: "session/prompt".into() }, "id", "prompt_b")
        .emit(json!({
            "jsonrpc": "2.0",
            "id": { "$capture": "prompt_a" },
            "result": { "stopReason": "end_turn" }
        }))
        .capture(CaptureSource::Request { method: "session/prompt".into() }, "id", "prompt_c")
        .emit(json!({
            "jsonrpc": "2.0",
            "id": { "$capture": "prompt_c" },
            "result": { "stopReason": "end_turn" }
        }))
        .emit(json!({
            "jsonrpc": "2.0",
            "id": { "$capture": "prompt_b" },
            "result": { "stopReason": "end_turn" }
        }));
    let (fake, host) = spawn_host_with_limit(script, Arc::new(DenyAllHandler), 2).await;
    let connection = ready_connection(&fake, &host).await;
    let a = connection.new_session(vec![PathBuf::from("/a")], Vec::new(), None).await.unwrap();
    let b = connection.new_session(vec![PathBuf::from("/b")], Vec::new(), None).await.unwrap();
    let c = connection.new_session(vec![PathBuf::from("/c")], Vec::new(), None).await.unwrap();

    let prompt_a = tokio::spawn(async move {
        a.send_prompt(vec![ContentBlock::Text(TextContent::new("work a"))]).await
    });
    await_request_count(&fake, "session/prompt", 1).await;
    let prompt_b = tokio::spawn(async move {
        b.send_prompt(vec![ContentBlock::Text(TextContent::new("work b"))]).await
    });
    await_request_count(&fake, "session/prompt", 2).await;
    let prompt_c = tokio::spawn(async move {
        c.send_prompt(vec![ContentBlock::Text(TextContent::new("work c"))]).await
    });

    for prompt in [prompt_a, prompt_b, prompt_c] {
        prompt.await.expect("prompt task").expect("prompt succeeds");
    }
    let requests = fake.requests_by_method("session/prompt");
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0]["params"]["sessionId"], "s1");
    assert_eq!(requests[1]["params"]["sessionId"], "s2");
    assert_eq!(requests[2]["params"]["sessionId"], "s3");

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn connection_commands_remain_responsive_while_prompt_is_pending() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": { "sessionCapabilities": { "list": {} } }
        }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
        .capture(CaptureSource::Request { method: "session/prompt".into() }, "id", "prompt_id")
        .wait_for("session/list")
        .respond(json!({ "sessions": [] }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let prompt_thread = thread.clone();
    let prompt = tokio::spawn(async move {
        prompt_thread.send_prompt(vec![ContentBlock::Text(TextContent::new("blocked"))]).await
    });
    await_request_count(&fake, "session/prompt", 1).await;

    let sessions = tokio::time::timeout(
        TEST_TIMEOUT,
        connection.list_sessions(Some(PathBuf::from("/work")), None),
    )
    .await
    .expect("session/list must not wait behind prompt")
    .expect("session/list succeeds");
    assert!(sessions.sessions.is_empty());
    assert!(!prompt.is_finished());

    thread.cancel().await.expect("cancel pending prompt");
    assert!(matches!(prompt.await.expect("prompt task"), Err(AgentError::Cancelled)));
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn connection_shutdown_resolves_all_in_flight_prompts() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s2" }))
        .capture(CaptureSource::Request { method: "session/prompt".into() }, "id", "prompt_a_id")
        .capture(CaptureSource::Request { method: "session/prompt".into() }, "id", "prompt_b_id");
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread_a =
        connection.new_session(vec![PathBuf::from("/work/a")], Vec::new(), None).await.unwrap();
    let thread_b =
        connection.new_session(vec![PathBuf::from("/work/b")], Vec::new(), None).await.unwrap();

    let prompt_thread_a = thread_a.clone();
    let prompt_a = tokio::spawn(async move {
        prompt_thread_a.send_prompt(vec![ContentBlock::Text(TextContent::new("A"))]).await
    });
    await_request_count(&fake, "session/prompt", 1).await;
    let prompt_thread_b = thread_b.clone();
    let prompt_b = tokio::spawn(async move {
        prompt_thread_b.send_prompt(vec![ContentBlock::Text(TextContent::new("B"))]).await
    });
    await_request_count(&fake, "session/prompt", 2).await;

    host.close().await;
    let error_a = prompt_a.await.expect("session A prompt task").unwrap_err();
    let error_b = prompt_b.await.expect("session B prompt task").unwrap_err();
    assert!(matches!(error_a, AgentError::ConnectionClosed { .. }));
    assert!(matches!(error_b, AgentError::ConnectionClosed { .. }));
    assert!(!thread_a.is_turn_running());
    assert!(!thread_b.is_turn_running());
    fake.join(TEST_TIMEOUT).await;
}
