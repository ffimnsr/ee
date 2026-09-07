//! `impl App` agents-pane tests: thread_tests domain.
use super::*;

#[test]
fn agents_stop_cancels_running_turn_and_updates_status() {
    let script = base_script().wait_for("session/prompt").wait_for("session/cancel");
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "long task");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "turn running", |app| {
        app.agents.threads[0].state == ThreadUiState::Running
    });

    run_ex(&mut app, "agents_stop");
    assert_eq!(app.backend.status_message.as_deref(), Some("cancelling turn…"));

    wait_until(&mut app, "cancel reply lands", |app| {
        app.backend.status_message.as_deref() == Some("turn cancelled")
    });
    wait_until(&mut app, "turn cancellation completed", |app| {
        app.agents.threads[0].state == ThreadUiState::Ready
            && app.agents.threads[0]
                .system_notices()
                .iter()
                .any(|notice| notice == "turn cancelled")
    });
    assert_eq!(app.agents.threads[0].state, ThreadUiState::Ready);
}

#[test]
fn steer_prioritizes_message_and_queue_runs_follow_up_after_turn_finishes() {
    let script = base_script()
        .wait_for("session/prompt")
        .wait_for("session/cancel")
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }))
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "original task");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "turn running", |app| {
        app.agents.threads[0].state == ThreadUiState::Running
    });
    wait_until(&mut app, "original prompt sent", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 1
    });

    type_text(&mut app, "/queue follow up after steer");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    type_text(&mut app, "/steer use additional user context");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].queued_prompts.len(), 2);
    assert_eq!(app.agents.threads[0].queued_prompts[0].text, "use additional user context");
    assert_eq!(app.agents.threads[0].queued_prompts[1].text, "follow up after steer");

    wait_until(&mut app, "steered and queued prompts complete", |app| {
        app.agents.threads[0].state == ThreadUiState::Ready
            && fake.agent().requests_by_method("session/prompt").len() == 3
    });
    let prompts = fake.agent().requests_by_method("session/prompt");
    assert_eq!(prompts[0]["params"]["prompt"][0]["text"], "original task");
    assert_eq!(prompts[1]["params"]["prompt"][0]["text"], "use additional user context");
    assert_eq!(prompts[2]["params"]["prompt"][0]["text"], "follow up after steer");
}

#[test]
fn queued_session_and_connection_saturation_render_distinctly() {
    let script = two_session_script().wait_for("session/prompt");
    let (mut app, _temp, fake) = fake_agents_app(script);
    app.config.agents.max_concurrent_prompts = 1;
    open_pane_and_wait_ready(&mut app);
    open_second_thread(&mut app);

    app.focus_thread(0);
    type_text(&mut app, "session A");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "session A dispatched", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 1
    });
    app.focus_thread(1);
    type_text(&mut app, "session B");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "session B queued", |app| {
        app.agents.threads[1].state == ThreadUiState::Queued
    });

    let backend = TestBackend::new(120, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rendered = (0..20)
        .map(|y| {
            (0..120)
                .map(|x| terminal.backend().buffer().cell((x, y)).unwrap().symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    assert!(
        rendered.iter().any(|row| row.contains("[queued]") && row.contains("conn:1/1 +1 queued")),
        "queued saturation missing: {rendered:#?}"
    );

    type_text(&mut app, "/stop");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    app.focus_thread(0);
    type_text(&mut app, "/stop");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "queued and active turns cancel", |app| {
        app.agents.pending_cancels.is_empty()
    });
}

#[test]
fn concurrent_cancellations_keep_session_scoped_results() {
    let script = two_session_script().wait_for("session/prompt").wait_for("session/prompt");
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);
    open_second_thread(&mut app);

    app.focus_thread(0);
    type_text(&mut app, "session A");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "session A prompt sent", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 1
    });
    app.focus_thread(1);
    type_text(&mut app, "session B");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "session B prompt sent", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 2
    });

    type_text(&mut app, "/stop");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    app.focus_thread(0);
    type_text(&mut app, "/stop");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.pending_cancels.len(), 2, "neither cancellation replaces the other");
    assert_eq!(app.agents.threads[0].state, ThreadUiState::Cancelling);
    assert_eq!(app.agents.threads[1].state, ThreadUiState::Cancelling);

    wait_until(&mut app, "both cancellations resolve", |app| app.agents.pending_cancels.is_empty());
    assert_eq!(
        fake.agent().requests_by_method("session/cancel").len(),
        2,
        "cancel notifications: {:?}",
        fake.agent().log()
    );
    let cancelled_request_ids: std::collections::BTreeSet<String> = fake
        .agent()
        .requests_by_method("$/cancel_request")
        .iter()
        .filter_map(|request| request["params"]["requestId"].as_str().map(str::to_string))
        .collect();
    assert_eq!(
        cancelled_request_ids.len(),
        2,
        "each prompt request must be targeted: {:?}",
        fake.agent().log()
    );
    for (index, session_id) in ["s1", "s2"].into_iter().enumerate() {
        assert_eq!(app.agents.threads[index].session_id, session_id);
        assert!(
            app.agents.threads[index]
                .system_notices()
                .iter()
                .any(|notice| notice == "turn cancelled"),
            "missing cancellation result for {session_id}"
        );
    }
}

#[test]
fn closing_pane_preserves_thread_state_and_session() {
    let (mut app, _temp, fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);
    let session_count_before_close = app.agents.threads.len();

    run_ex(&mut app, "agents_close");
    assert_eq!(app.agents.layout, AgentPaneLayout::Closed);
    assert_eq!(app.mode, Mode::Normal, "focus returns to the editor");
    assert_eq!(app.agents.threads.len(), session_count_before_close, "thread survives close");

    // Reopening reuses the running session: no second session/new.
    run_ex(&mut app, "agents");
    wait_until(&mut app, "pane reopened", |app| app.agents_focused());
    assert_eq!(fake.agent().requests_by_method("session/new").len(), 1);
    assert_eq!(app.agents.threads.len(), 1);
}

#[test]
fn esc_and_ctrl_c_do_not_close_or_blur_agents_pane() {
    let (mut app, _temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);

    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.agents.layout, AgentPaneLayout::Full);
    assert_eq!(app.mode, Mode::Agent);

    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(app.agents.layout, AgentPaneLayout::Full);
    assert_eq!(app.mode, Mode::Agent);
}

#[test]
fn quit_slash_command_closes_agents_pane_locally() {
    let (mut app, _temp, fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/quit");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert_eq!(app.agents.layout, AgentPaneLayout::Closed);
    assert_eq!(app.mode, Mode::Normal);
    assert!(app.agents.threads[0].draft.is_empty());
    assert!(fake.agent().requests_by_method("session/prompt").is_empty());
}

#[test]
fn quit_full_slash_command_exits_editor_locally() {
    let (mut app, _temp, fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/quit_full");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert!(app.should_quit);
    assert_eq!(app.agents.layout, AgentPaneLayout::Full);
    assert_eq!(app.mode, Mode::Agent);
    assert!(app.agents.threads[0].draft.is_empty());
    assert!(fake.agent().requests_by_method("session/prompt").is_empty());
}

#[test]
fn new_thread_slash_command_starts_and_focuses_thread_locally() {
    let script = FakeAgentScript::new()
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
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/new_thread");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "new-thread slash-command thread ready", |app| {
        app.agents.threads.len() == 2 && app.agents.threads[1].state == ThreadUiState::Ready
    });
    assert_eq!(app.agents.active_thread, Some(1));
    assert!(app.agents.threads[0].draft.is_empty());
    assert!(app.agents.threads[1].draft.is_empty());
    assert_eq!(fake.agent().requests_by_method("session/new").len(), 2);
    assert!(fake.agent().requests_by_method("session/prompt").is_empty());
}

#[test]
fn new_slash_command_rejects_unknown_agent_without_forwarding_prompt() {
    let (mut app, _temp, fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/new missing");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert_eq!(
        app.backend.status_message.as_deref(),
        Some("unknown agent `missing`; configured: fake")
    );
    assert_eq!(fake.agent().requests_by_method("session/new").len(), 1);
    assert!(fake.agent().requests_by_method("session/prompt").is_empty());
}

#[test]
fn new_thread_slash_command_allows_parallel_requests_while_session_starts() {
    let script = base_script().wait_for("session/new").wait_for("session/new");
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/new_thread");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "new-thread session request pending", |_| {
        fake.agent().requests_by_method("session/new").len() == 2
    });

    type_text(&mut app, "/new_thread");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "parallel new-thread request pending", |_| {
        fake.agent().requests_by_method("session/new").len() == 3
    });
    assert_eq!(app.backend.status_message.as_deref(), Some("starting new agent session…"));
    assert_eq!(app.agents.pending_sessions.len(), 2);
}

#[test]
fn agents_clear_wipes_scrollback_only_when_idle() {
    let script = base_script().wait_for("session/prompt");
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    // No turn running: clearing works.
    run_ex(&mut app, "agents_clear");
    assert!(app.agents.threads[0].transcript.is_empty());
    assert_eq!(
        app.backend.status_message.as_deref(),
        Some("visible scrollback cleared; provider conversation remains intact")
    );

    // While a turn is running the clear is refused.
    type_text(&mut app, "keep");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "turn running", |app| {
        app.agents.threads[0].state == ThreadUiState::Running
    });
    run_ex(&mut app, "agents_clear");
    assert_eq!(
        app.backend.status_message.as_deref(),
        Some("cannot clear scrollback while a turn is running")
    );
    assert!(!app.agents.threads[0].transcript.is_empty());
}

// ── Thread switching ─────────────────────────────────────────────────────────

#[test]
fn agents_threads_opens_picker_and_focuses_selected_session() {
    let script = FakeAgentScript::new()
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
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);
    run_ex(&mut app, "agents_new");
    wait_until(&mut app, "second thread ready", |app| {
        app.agents.threads.len() == 2 && app.agents.threads[1].state == ThreadUiState::Ready
    });
    assert_eq!(app.agents.active_thread, Some(1));

    run_ex(&mut app, "agents_threads");
    let picker = app.picker.as_ref().expect("agent thread picker should open");
    assert_eq!(picker.kind, crate::picker::PickerKind::AgentThreads);
    assert_eq!(picker.title, "Agent Sessions");
    assert_eq!(picker.visible_count(), 2);
    assert_eq!(picker.selected, 1, "active thread preselected");

    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.active_thread, Some(0));
    assert_eq!(app.mode, Mode::Agent);
    assert!(app.picker.is_none(), "picker closes after confirm");
}

#[test]
fn sessions_slash_command_opens_agent_thread_picker_locally() {
    let script = FakeAgentScript::new()
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
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);
    run_ex(&mut app, "agents_new");
    wait_until(&mut app, "second thread ready", |app| {
        app.agents.threads.len() == 2 && app.agents.threads[1].state == ThreadUiState::Ready
    });

    type_text(&mut app, "/sessions");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    let picker = app.picker.as_ref().expect("/sessions should open agent thread picker");
    assert_eq!(picker.kind, crate::picker::PickerKind::AgentThreads);
    assert_eq!(picker.title, "Agent Sessions");
    assert_eq!(picker.selected, 1, "active thread preselected");
    assert!(fake.agent().requests_by_method("session/prompt").is_empty());
}

#[test]
fn ctrl_t_opens_agent_thread_picker() {
    let script = FakeAgentScript::new()
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
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);
    run_ex(&mut app, "agents_new");
    wait_until(&mut app, "second thread ready", |app| {
        app.agents.threads.len() == 2 && app.agents.threads[1].state == ThreadUiState::Ready
    });

    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);

    let picker = app.picker.as_ref().expect("ctrl-t should open agent thread picker");
    assert_eq!(picker.kind, crate::picker::PickerKind::AgentThreads);
    assert_eq!(picker.selected, 1);
}

#[test]
fn thread_switching_preserves_drafts_scroll_unread_and_activity() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
        .wait_for("session/set_mode")
        .respond(json!({}))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s2" }))
        .wait_for("session/set_mode")
        .respond(json!({}))
        // After s2 is fully registered, a prompt on s2 lets the fake emit
        // content for s1 while s2 is the focused thread (deterministic
        // unread bump).
        .wait_for("session/prompt")
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "ping")))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "first draft");
    assert_eq!(app.agents.threads[0].draft, "first draft");

    run_ex(&mut app, "agents_new");
    wait_until(&mut app, "second thread ready", |app| {
        app.agents.threads.len() == 2 && app.agents.threads[1].state == ThreadUiState::Ready
    });
    assert_eq!(app.agents.active_thread, Some(1));
    assert_eq!(app.agents.threads[1].draft, "", "new thread starts with an empty draft");

    type_text(&mut app, "second draft");
    assert_eq!(app.agents.threads[1].draft, "second draft");

    // Submitting on s2 lets the fake stream s1 content while s2 is focused.
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    // s1 had unread content from the emit while s2 was focused.
    wait_until(&mut app, "unread bumped on inactive thread", |app| {
        app.agents.threads[0].unread > 0 && app.agents.threads[0].activity
    });
    let unread_before_focus = app.agents.threads[0].unread;
    assert!(unread_before_focus > 0);

    // Switch back to s1: draft preserved, unread reset by focus.
    run_ex(&mut app, "agents_prev");
    assert_eq!(app.agents.active_thread, Some(0));
    assert_eq!(app.agents.threads[0].draft, "first draft");
    assert_eq!(app.agents.threads[0].unread, 0, "focusing resets unread");

    // Scroll offset is per-thread and survives switching.
    press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
    let s1_scroll = app.agents.threads[0].scroll;
    assert!(!app.agents.threads[0].stick_to_bottom);
    run_ex(&mut app, "agents_next");
    run_ex(&mut app, "agents_prev");
    assert_eq!(app.agents.threads[0].scroll, s1_scroll);
    assert!(!app.agents.threads[0].stick_to_bottom);

    // Next wraps around to the first thread.
    run_ex(&mut app, "agents_next");
    assert_eq!(app.agents.active_thread, Some(1));
}

// ── Layout command ───────────────────────────────────────────────────────────

#[test]
fn agents_layout_changes_split_and_opens_pane() {
    let (mut app, _temp, _fake) = fake_agents_app(base_script());

    run_ex(&mut app, "agents_layout bottom");
    assert_eq!(app.agents.layout, AgentPaneLayout::Bottom);
    assert_eq!(app.mode, Mode::Agent);
    wait_until(&mut app, "thread ready after layout open", |app| {
        app.agents.threads.len() == 1 && app.agents.threads[0].state == ThreadUiState::Ready
    });

    run_ex(&mut app, "agents_layout full");
    assert_eq!(app.agents.layout, AgentPaneLayout::Full);

    run_ex(&mut app, "agents_layout left");
    assert_eq!(
        app.backend.status_message.as_deref(),
        Some("usage: :agents_layout right|bottom|full")
    );
    assert_eq!(app.agents.layout, AgentPaneLayout::Full, "invalid argument leaves layout");
}

#[test]
fn phase_five_init_and_doctor_stay_local_owned_and_safe() {
    let script =
        base_script().wait_for("session/prompt").respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/doctor");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let doctor = app.agents.threads[0].system_notices().join("\n");
    assert!(doctor.contains("Agents TUI doctor (read-only)"));
    assert!(doctor.contains("feature: agents mode enabled"));
    assert!(doctor.contains("configured agent command: fake: unused"));
    assert!(doctor.contains("redaction:"));
    assert!(fake.agent().requests_by_method("session/prompt").is_empty());

    type_text(&mut app, "/init");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "init workflow prompt sent", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 1
    });
    let prompt = &fake.agent().requests_by_method("session/prompt")[0]["params"]["prompt"];
    let text = prompt[0]["text"].as_str().expect("init workflow text");
    assert!(text.contains("ee_project_instructions"));
    assert!(text.contains("ee_create_text_file"));
    assert!(text.contains("do not overwrite"));
    assert!(text.contains("normal file-write approval"));
    let transcript = app.agents.threads[0].message_pairs();
    assert!(transcript.iter().any(|(_, text)| text.contains("EE local /init request sent")));
    assert!(!transcript.iter().any(|(_, text)| text.contains("ee_create_text_file")));
}
