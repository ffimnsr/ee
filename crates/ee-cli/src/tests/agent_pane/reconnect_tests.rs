//! `impl App` agents-pane tests: reconnect_tests domain.
use super::*;

#[test]
fn recoverable_pause_discard_clears_state() {
    let script = base_script()
        .wait_for("session/prompt")
        .respond_error_with_data(
            -32603,
            "recoverable turn interruption: paused after 300s",
            json!({
                "recoverable": {
                    "fault": "deadline",
                    "detail": "paused after 300s",
                    "cause": null,
                    "safe_resume": true,
                    "retry_after": null,
                    "checkpoint_id": "s1-0000000001",
                    "completed_tool_calls": 0,
                    "resumed_count": 0,
                }
            }),
        )
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "hello agent");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "turn paused notice", |app| {
        app.agents.threads[0].state == ThreadUiState::PausedRecoverable
    });

    // `/discard` tells the agent to drop the checkpoint.
    type_text(&mut app, "/discard");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "discard prompt sent", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 2
    });
    wait_until(&mut app, "turn completed after discard", |app| {
        app.agents.threads[0].state == ThreadUiState::Ready
    });
    assert!(app.agents.threads[0].pending_recovery.is_none(), "discard clears the pause");
}

#[test]
fn agents_reconnect_loads_persisted_session_and_replays_conversation() {
    let state_dir = tempfile::tempdir().unwrap();
    let script = FakeAgentScript::new()
        // The restarted agent advertises load (replay) and resume.
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "loadSession": true,
                "sessionCapabilities": { "resume": {} }
            }
        }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
        .wait_for("session/set_mode")
        .respond(json!({}))
        .wait_for("session/prompt")
        .respond_error_with_data(
            -32603,
            "recoverable turn interruption: paused after 300s",
            json!({
                "recoverable": {
                    "fault": "deadline",
                    "detail": "paused after 300s",
                    "cause": null,
                    "safe_resume": true,
                    "retry_after": null,
                    "checkpoint_id": "s1-0000000001",
                    "completed_tool_calls": 0,
                    "resumed_count": 0,
                }
            }),
        )
        // The simulated restart answers `session/load` by replaying the
        // conversation, then responds (the SDK parses an empty object).
        .wait_for("session/load")
        .emit(wire::session_update("s1", wire::user_message_chunk("hello agent")))
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "first answer")))
        .respond(json!({}))
        .wait_for("session/set_mode")
        .respond(json!({}));
    let (mut app, _temp, fake) = fake_agents_app(script);
    app.agents.test_session_state_base = Some(state_dir.path().to_path_buf());
    open_pane_and_wait_ready(&mut app);

    // Submit a prompt that pauses: the persisted record now holds the
    // session id and the prompt text (client-persisted for the resend path).
    type_text(&mut app, "hello agent");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "turn paused notice", |app| {
        app.agents.threads[0].state == ThreadUiState::PausedRecoverable
    });

    // Reconnect: `session/load` (preferred over `session/resume`) restores
    // the session and replays the conversation; the existing thread is
    // rebound to the fresh connection.
    type_text(&mut app, "/reconnect");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let deadline = Instant::now() + WAIT;
    loop {
        app.pump_agents();
        let _ = app.backend.drain_events();
        if app.agents.threads.len() == 1
            && app.agents.threads[0].state == ThreadUiState::Ready
            && app.agents.threads[0].transcript.iter().any(|item| {
                matches!(
                    item,
                    TranscriptItem::Message {
                        kind: MessageRenderKind::User,
                        text,
                        ..
                    } if text == "hello agent"
                )
            })
            && app.agents.threads[0].transcript.iter().any(|item| {
                matches!(
                    item,
                    TranscriptItem::Message {
                        kind: MessageRenderKind::Assistant,
                        text,
                        ..
                    } if text == "first answer"
                )
            })
        {
            break;
        }
        if Instant::now() > deadline {
            panic!(
                "reconnect replay never applied; threads={} pending_sessions={} pending_replay={:?} fake_log={:?}",
                app.agents.threads.len(),
                app.agents.pending_sessions.len(),
                app.agents.pending_replay.keys().collect::<Vec<_>>(),
                fake.agent().log(),
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !fake.agent().requests_by_method("session/load").is_empty(),
        "reconnect sends session/load"
    );
    assert!(
        fake.agent().requests_by_method("session/resume").is_empty(),
        "load is preferred over resume"
    );
    // The persisted last prompt is restored for the resend path.
    let restored = app.agents.threads[0].last_prompt.as_ref().and_then(|blocks| {
        blocks.iter().find_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
    });
    assert_eq!(restored.as_deref(), Some("hello agent"));
}

#[test]
fn workspace_restart_restores_all_agent_threads_on_pane_open() {
    let workspace = tempfile::tempdir().unwrap();
    let state_dir = tempfile::tempdir().unwrap();
    let first_script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
        .wait_for("session/set_mode")
        .respond(json!({}))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s2" }))
        .wait_for("session/set_mode")
        .respond(json!({}));
    let (mut first_app, _first_fake) = fake_agents_app_in(workspace.path(), first_script);
    first_app.agents.test_session_state_base = Some(state_dir.path().to_path_buf());
    open_pane_and_wait_ready(&mut first_app);
    type_text(&mut first_app, "/new_thread");
    press(&mut first_app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut first_app, "second workspace thread ready", |app| {
        app.agents.threads.len() == 2 && app.agents.threads[1].state == ThreadUiState::Ready
    });
    assert_eq!(first_app.agents.active_thread, Some(1));
    first_app.shutdown_agents();
    drop(first_app);

    let restarted_script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": { "loadSession": true }
        }))
        // Both loads must arrive before either response. Complete them in
        // reverse order to verify operation attribution and stable UI ordering.
        .capture(CaptureSource::Request { method: "session/load".into() }, "id", "load_s1")
        .capture(CaptureSource::Request { method: "session/load".into() }, "id", "load_s2")
        .emit(wire::session_update("s2", wire::agent_message_chunk("s2-message", "second replay")))
        .emit(json!({ "jsonrpc": "2.0", "id": { "$capture": "load_s2" }, "result": {} }))
        .emit(wire::session_update("s1", wire::agent_message_chunk("s1-message", "first replay")))
        .emit(json!({ "jsonrpc": "2.0", "id": { "$capture": "load_s1" }, "result": {} }))
        .wait_for("session/set_mode")
        .respond(json!({}))
        .wait_for("session/set_mode")
        .respond(json!({}));
    let (mut restarted_app, restarted_fake) =
        fake_agents_app_in(workspace.path(), restarted_script);
    restarted_app.agents.test_session_state_base = Some(state_dir.path().to_path_buf());

    // Opening a fresh TUI pane restores workspace threads. It must not need
    // `/reconnect`, and it must not create a replacement `session/new`.
    run_ex(&mut restarted_app, "agents");
    wait_until(&mut restarted_app, "workspace threads restored", |app| {
        app.agents.threads.len() == 2
            && app.agents.threads.iter().all(|thread| thread.state == ThreadUiState::Ready)
            && app.agents.threads[0].transcript.iter().any(|item| {
                matches!(item, TranscriptItem::Message { text, .. } if text == "first replay")
            })
            && app.agents.threads[1].transcript.iter().any(|item| {
                matches!(item, TranscriptItem::Message { text, .. } if text == "second replay")
            })
    });

    assert_eq!(
        restarted_app
            .agents
            .threads
            .iter()
            .map(|thread| thread.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["s1", "s2"]
    );
    assert_eq!(restarted_app.agents.active_thread, Some(1));
    let loads = restarted_fake.agent().requests_by_method("session/load");
    assert_eq!(loads.len(), 2);
    let mut loaded_session_ids = loads
        .iter()
        .filter_map(|request| request["params"]["sessionId"].as_str())
        .collect::<Vec<_>>();
    loaded_session_ids.sort_unstable();
    assert_eq!(loaded_session_ids, vec!["s1", "s2"]);
    assert!(restarted_fake.agent().requests_by_method("session/new").is_empty());
    assert!(restarted_fake.agent().requests_by_method("session/resume").is_empty());
}

#[test]
fn workspace_restart_restores_local_transcript_when_agent_load_has_no_replay() {
    let workspace = tempfile::tempdir().unwrap();
    let state_dir = tempfile::tempdir().unwrap();
    let first_script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
        .wait_for("session/set_mode")
        .respond(json!({}));
    let (mut first_app, _first_fake) = fake_agents_app_in(workspace.path(), first_script);
    first_app.agents.test_session_state_base = Some(state_dir.path().to_path_buf());
    open_pane_and_wait_ready(&mut first_app);
    first_app.agents.threads[0].transcript.push(TranscriptItem::Message {
        nick: String::from("you"),
        text: String::from("persisted question"),
        kind: MessageRenderKind::User,
        message_id: Some(String::from("user-1")),
        response_group: None,
        at: SystemTime::now(),
    });
    first_app.agents.threads[0].transcript.push(TranscriptItem::Message {
        nick: String::from("assistant"),
        text: String::from("persisted answer"),
        kind: MessageRenderKind::Assistant,
        message_id: Some(String::from("assistant-1")),
        response_group: Some(1),
        at: SystemTime::now(),
    });
    first_app.shutdown_agents();
    drop(first_app);

    let restarted_script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": { "loadSession": true }
        }))
        .wait_for("session/load")
        .respond(json!({}))
        .wait_for("session/set_mode")
        .respond(json!({}));
    let (mut restarted_app, restarted_fake) =
        fake_agents_app_in(workspace.path(), restarted_script);
    restarted_app.agents.test_session_state_base = Some(state_dir.path().to_path_buf());

    run_ex(&mut restarted_app, "agents");
    wait_until(&mut restarted_app, "local transcript restored", |app| {
        app.agents.threads.first().is_some_and(|thread| {
            thread.state == ThreadUiState::Ready
                && thread.transcript.iter().any(|item| {
                    matches!(
                        item,
                        TranscriptItem::Message {
                            kind: MessageRenderKind::User,
                            text,
                            ..
                        } if text == "persisted question"
                    )
                })
                && thread.transcript.iter().any(|item| {
                    matches!(
                        item,
                        TranscriptItem::Message {
                            kind: MessageRenderKind::Assistant,
                            text,
                            ..
                        } if text == "persisted answer"
                    )
                })
        })
    });

    assert_eq!(restarted_fake.agent().requests_by_method("session/load").len(), 1);
    assert!(restarted_fake.agent().requests_by_method("session/new").is_empty());
}

#[test]
fn agents_reconnect_without_persisted_session_reports_error() {
    // A fresh state directory and no session ever created: there is no
    // persisted record to reconnect.
    let state_dir = tempfile::tempdir().unwrap();
    let (mut app, _temp, _fake) = fake_agents_app(base_script());
    app.agents.test_session_state_base = Some(state_dir.path().to_path_buf());

    run_ex(&mut app, "agents_reconnect");
    assert!(
        app.agents
            .error
            .as_deref()
            .is_some_and(|error| error.contains("no persisted agent session"))
    );
}
