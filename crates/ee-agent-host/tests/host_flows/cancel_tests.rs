//! Host-flow tests: cancel.
use super::*;

#[tokio::test]
async fn cancel_sends_session_cancel_and_resolves_prompt() {
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
    // Wait until the agent saw the prompt request.
    tokio::time::timeout(TEST_TIMEOUT, async {
        while !fake.log_contains("\"method\":\"session/prompt\"") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("prompt request observed");

    thread.cancel().await.expect("cancel succeeds");

    let error = prompt.await.expect("prompt task").unwrap_err();
    assert!(matches!(error, AgentError::Cancelled));
    assert!(!thread.is_turn_running());

    // `session/cancel` went out on the wire.
    assert!(
        fake.log_contains("\"method\":\"session/cancel\""),
        "expected session/cancel in fake log: {:?}",
        fake.log()
    );
    assert!(
        fake.log_contains("\"method\":\"$/cancel_request\""),
        "expected $/cancel_request in fake log: {:?}",
        fake.log()
    );

    // Exactly one terminal event for the turn.
    let mut terminal_events = Vec::new();
    while let Ok(Some(event)) = tokio::time::timeout(TEST_TIMEOUT, host.events.recv()).await {
        if matches!(
            event,
            AgentEvent::TurnCompleted { .. }
                | AgentEvent::TurnCancelled { .. }
                | AgentEvent::TurnFailed { .. }
        ) {
            terminal_events.push(event);
            break;
        }
    }
    assert!(matches!(terminal_events[0], AgentEvent::TurnCancelled { .. }));
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn cancellation_keeps_turn_reservation_until_old_prompt_future_resolves() {
    let script = base_script()
        .wait_for("session/prompt")
        .delay(100)
        .respond(json!({ "stopReason": "end_turn" }))
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let old_thread = thread.clone();
    let old_prompt = tokio::spawn(async move {
        old_thread.send_prompt(vec![ContentBlock::Text(TextContent::new("old"))]).await
    });
    tokio::time::timeout(TEST_TIMEOUT, async {
        while !fake.log_contains("\"method\":\"session/prompt\"") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("old prompt request observed");

    thread.cancel().await.expect("cancel succeeds");
    assert!(thread.is_turn_running(), "cancelling turn retains reservation");
    assert!(matches!(
        thread.send_prompt(vec![ContentBlock::Text(TextContent::new("replacement"))]).await,
        Err(AgentError::TurnAlreadyRunning)
    ));

    assert!(matches!(old_prompt.await.expect("old prompt task joins"), Err(AgentError::Cancelled)));
    assert!(!thread.is_turn_running(), "old completion releases only its reservation");
    thread
        .send_prompt(vec![ContentBlock::Text(TextContent::new("replacement"))])
        .await
        .expect("replacement prompt starts after old future resolves");

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn late_session_updates_after_cancel_still_reduce_until_prompt_response_arrives() {
    let script = base_script()
        .wait_for("session/prompt")
        .delay(100)
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "late")));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
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
    let error = prompt.await.expect("prompt task").unwrap_err();
    assert!(matches!(error, AgentError::Cancelled));

    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            let snapshot = thread.snapshot();
            if snapshot.messages.iter().any(|message| {
                message.kind == MessageKind::Assistant
                    && message.blocks.iter().any(|block| {
                        matches!(
                            block,
                            ContentBlock::Text(text) if text.text == "late"
                        )
                    })
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("late update reduced after cancel");

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}
