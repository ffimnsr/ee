//! `impl App` agents-pane tests: slash_tests domain.
use super::*;

#[test]
fn exit_slash_commands_work_without_a_configured_agent() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join(".ee.toml"), "[agents]\nenabled = true\n").unwrap();
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let mut app = App::from_path(None).unwrap();
    drop(_cwd_restore);
    drop(_cwd_lock);
    app.config.agents.servers.clear();
    app.config.agents.default_agent = None;

    run_ex(&mut app, "agents");
    assert_eq!(app.agents.layout, AgentPaneLayout::Full);
    assert!(app.agents.threads.is_empty());

    type_text(&mut app, "/quit");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert_eq!(app.agents.layout, AgentPaneLayout::Closed);
    assert_eq!(app.mode, Mode::Normal);
    assert!(app.agents.pending_draft.is_empty());
    assert!(app.agents.pending_sessions.is_empty());
    assert!(app.agents.threads.is_empty());

    run_ex(&mut app, "agents");
    type_text(&mut app, "/quit_full");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert!(app.should_quit);
    assert_eq!(app.agents.layout, AgentPaneLayout::Full);
    assert_eq!(app.mode, Mode::Agent);
    assert!(app.agents.pending_draft.is_empty());
    assert!(app.agents.pending_sessions.is_empty());
    assert!(app.agents.threads.is_empty());
}

#[test]
fn local_slash_commands_control_agent_tui_without_forwarding_prompts() {
    let script = base_script()
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s2" }))
        .wait_for("session/set_mode")
        .respond(json!({}));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/lay");
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].draft, "/layout");
    press(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);

    type_text(&mut app, "/new_thread");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "second thread ready", |app| {
        app.agents.threads.len() == 2 && app.agents.threads[1].state == ThreadUiState::Ready
    });
    assert_eq!(app.agents.active_thread, Some(1));

    type_text(&mut app, "/prev");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.active_thread, Some(0));
    type_text(&mut app, "/next");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.active_thread, Some(1));

    type_text(&mut app, "/layout right");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.layout, AgentPaneLayout::Right);

    type_text(&mut app, "/thoughts off");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(!app.agents.show_thoughts);

    type_text(&mut app, "/config");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.backend.status_message.as_deref(), Some("no session config options advertised"));

    type_text(&mut app, "/mcp");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        app.backend
            .status_message
            .as_deref()
            .is_some_and(|message| message.contains("no MCP servers"))
    );

    type_text(&mut app, "/stop");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.backend.status_message.as_deref(), Some("no running turn to stop"));

    assert!(!app.agents.threads[1].transcript.is_empty());
    type_text(&mut app, "/clear");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.agents.threads[1].transcript.is_empty());
    assert!(
        fake.agent().requests_by_method("session/prompt").is_empty(),
        "local slash commands must not forward prompt turns"
    );
}

#[test]
fn phase_one_slash_commands_are_local_and_safe() {
    let (mut app, _temp, fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/help");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let help = app.agents.threads[0].system_notices().join("\n");
    assert!(help.contains("Local slash commands:"));
    assert!(help.contains("/approval — default|autopilot|bypass; bypass keeps validation"));
    assert!(help.contains("Provider-owned slash commands"));

    type_text(&mut app, "/status");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let status = app.backend.status_message.as_deref().expect("status output");
    assert!(status.contains("session:s1"));
    assert!(status.contains("agent:fake"));
    assert!(status.contains("approval:default"));

    app.agents.threads[0].transcript.push(TranscriptItem::Message {
        nick: String::from("fake"),
        text: String::from("safe completed response"),
        kind: MessageRenderKind::Assistant,
        message_id: Some(String::from("assistant-1")),
        response_group: Some(1),
        at: SystemTime::now(),
    });
    type_text(&mut app, "/copy");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.registers.get(&RegisterName::Clipboard), "safe completed response");
    assert_eq!(app.backend.status_message.as_deref(), Some("copied assistant response 1"));

    type_text(&mut app, "/rename   Audit\u{200B}   run   ");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].session_name.as_deref(), Some("Audit run"));
    assert_eq!(app.agents.threads[0].display_name, "1.Audit run");

    type_text(&mut app, "/diff");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        app.backend
            .status_message
            .as_deref()
            .is_some_and(|message| message.contains("workspace diff unavailable"))
    );
    assert!(fake.agent().requests_by_method("session/prompt").is_empty());
}

#[test]
fn phase_two_session_lifecycle_commands_keep_local_and_provider_state_distinct() {
    let script = base_script()
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s2" }))
        .wait_for("session/set_mode")
        .respond(json!({}));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    app.agents.threads[0].transcript.push(TranscriptItem::System {
        text: String::from("first transcript"),
        at: SystemTime::now(),
    });
    type_text(&mut app, "/clear");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.agents.threads[0].transcript.is_empty());
    assert_eq!(app.agents.threads[0].session_id, "s1", "clear must not create/reset session");
    assert!(
        app.backend
            .status_message
            .as_deref()
            .is_some_and(|message| message.contains("provider conversation remains intact"))
    );

    type_text(&mut app, "/new");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "new session ready", |app| {
        app.agents.threads.len() == 2 && app.agents.threads[1].state == ThreadUiState::Ready
    });
    assert_eq!(app.agents.threads[1].session_id, "s2");

    app.agents.threads[1].transcript.push(TranscriptItem::System {
        text: String::from("archived transcript"),
        at: SystemTime::now(),
    });
    type_text(&mut app, "/archive");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.threads.len(), 1);
    assert_eq!(app.agents.archived_threads.len(), 1);
    assert!(
        app.agents.archived_threads[0]
            .system_notices()
            .contains(&String::from("archived transcript"))
    );

    type_text(&mut app, "/archive restore 1");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.threads.len(), 2);
    assert!(app.agents.archived_threads.is_empty());
    assert_eq!(app.agents.active_thread, Some(1));

    type_text(&mut app, "/delete");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.agents.session_deletion_confirmation.is_some());
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.agents.session_deletion_confirmation.is_none());
    assert_eq!(app.agents.threads.len(), 2);

    type_text(&mut app, "/delete");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.threads.len(), 1);
    assert_eq!(app.agents.threads[0].session_id, "s1");
    assert!(
        app.backend
            .status_message
            .as_deref()
            .is_some_and(|message| message.contains("provider session unchanged"))
    );
    assert_eq!(fake.agent().requests_by_method("session/new").len(), 2);
}

#[test]
fn fork_and_branch_create_redacted_seeded_sessions_without_mutating_parent() {
    let script = base_script()
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s2" }))
        .wait_for("session/set_mode")
        .respond(json!({}))
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s3" }))
        .wait_for("session/set_mode")
        .respond(json!({}))
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);
    app.agents.threads[0].transcript.push(TranscriptItem::Message {
        nick: String::from("you"),
        text: String::from("parent context"),
        kind: MessageRenderKind::User,
        message_id: Some(String::from("parent-1")),
        response_group: None,
        at: SystemTime::now(),
    });

    type_text(&mut app, "/fork");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "fork ready", |app| app.agents.threads.len() == 2);
    assert_eq!(app.agents.active_thread, Some(0), "fork keeps parent active");
    assert_eq!(app.agents.threads[1].fork_parent_session_id.as_deref(), Some("s1"));
    assert!(app.agents.threads[1].context_files.is_empty(), "child attachments must be isolated");
    wait_until(&mut app, "fork seed sent", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 1
    });
    let prompts = fake.agent().requests_by_method("session/prompt");
    let seed = prompts[0]["params"]["prompt"][0]["text"].as_str().expect("fork seed text");
    assert!(seed.contains("parent context"));
    assert!(seed.contains("not provider-side session cloning"));

    type_text(&mut app, "/branch");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "branch ready", |app| app.agents.threads.len() == 3);
    assert_eq!(app.agents.active_thread, Some(2), "branch switches to child");
    assert_eq!(app.agents.threads[2].fork_parent_session_id.as_deref(), Some("s1"));
    assert!(app.agents.threads[2].context_files.is_empty());
}

#[test]
fn approval_slash_command_scopes_modes_to_active_session_and_confirms_bypass() {
    let (mut app, _temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/approval");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.backend.status_message.as_deref(), Some("tool approvals: default"));

    type_text(&mut app, "/approval autopilot");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.approval_modes.get("s1").map(|mode| mode.label()), Some("autopilot"));

    type_text(&mut app, "/approval bypass");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.agents.approval_mode_confirmation.is_some(), "bypass needs confirmation");

    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rendered: Vec<String> = (0..20)
        .map(|y| {
            (0..100).map(|x| terminal.backend().buffer().cell((x, y)).unwrap().symbol()).collect()
        })
        .collect();
    assert!(
        rendered.iter().any(|row| row.contains("enable bypass tool approvals")),
        "confirmation missing: {rendered:#?}"
    );

    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.agents.approval_mode_confirmation.is_none());
    assert_eq!(
        app.agents.approval_modes.get("s1").map(|mode| mode.label()),
        Some("autopilot"),
        "cancelling bypass preserves prior mode"
    );

    type_text(&mut app, "/approval bypass");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.approval_modes.get("s1").map(|mode| mode.label()), Some("bypass"));

    type_text(&mut app, "/approval default");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(!app.agents.approval_modes.contains_key("s1"), "default removes session override");
}

#[test]
fn draft_typed_before_session_ready_carries_into_first_thread() {
    let (mut app, _temp, _fake) = fake_agents_app(base_script());

    run_ex(&mut app, "agents");
    type_text(&mut app, "hello before ready");
    assert_eq!(app.agents.pending_draft, "hello before ready");

    wait_until(&mut app, "first agent thread ready", |app| {
        app.agents.threads.len() == 1 && app.agents.threads[0].state == ThreadUiState::Ready
    });

    assert!(app.agents.pending_draft.is_empty());
    assert_eq!(app.agents.threads[0].draft, "hello before ready");
}

#[test]
fn prompt_submission_appends_optimistic_you_and_sends_acp_prompt() {
    let script =
        base_script().wait_for("session/prompt").respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "hello agent");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    // Optimistic `you` message appears before any agent traffic.
    let pairs = app.agents.threads[0].message_pairs();
    assert_eq!(pairs, vec![(String::from("you"), String::from("hello agent"))]);
    assert!(app.agents.threads[0].draft.is_empty(), "draft must clear after submit");

    wait_until(&mut app, "prompt sent to agent", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 1
    });
    wait_until(&mut app, "turn completed notice", |app| {
        app.agents.threads[0].system_notices().iter().any(|n| n.contains("turn completed"))
    });
    assert_eq!(app.agents.threads[0].state, ThreadUiState::Ready);
}

#[test]
fn context_slash_command_attaches_snapshots_and_controls_session_files() {
    let script =
        base_script().wait_for("session/prompt").respond(json!({ "stopReason": "end_turn" }));
    let (mut app, temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);
    fs::write(temp.path().join("context.txt"), "context snapshot\n").unwrap();

    type_text(&mut app, "/context add context.txt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].context_files.len(), 1);
    assert_eq!(app.agents.threads[0].context_files[0].relative_path, "context.txt");
    assert_eq!(app.agents.threads[0].context_files[0].content, "context snapshot\n");
    assert_eq!(
        app.backend.status_message.as_deref(),
        Some("context attached: context.txt (17 bytes)")
    );

    type_text(&mut app, "/context");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        app.backend.status_message.as_deref(),
        Some("context files: context.txt (17 bytes)")
    );

    type_text(&mut app, "use selected context");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "context prompt sent", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 1
    });
    let prompt = &fake.agent().requests_by_method("session/prompt")[0]["params"]["prompt"];
    assert_eq!(prompt[0]["text"], "use selected context");
    let context = prompt[1]["text"].as_str().expect("context block text");
    assert!(context.contains("User-selected context file: `context.txt`"), "{context}");
    assert!(context.contains("context snapshot\n"), "{context}");
    assert!(
        !app.agents.threads[0]
            .system_notices()
            .iter()
            .any(|notice| notice.contains("context snapshot")),
        "context body must not enter transcript"
    );

    wait_until(&mut app, "context turn completes", |app| {
        app.agents.threads[0].state == ThreadUiState::Ready
    });
    type_text(&mut app, "/context remove context.txt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.agents.threads[0].context_files.is_empty());
    type_text(&mut app, "/context add context.txt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    type_text(&mut app, "/context clear");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.agents.threads[0].context_files.is_empty());
}

#[test]
fn context_status_and_one_turn_mentions_stay_bounded_and_session_local() {
    let script =
        base_script().wait_for("session/prompt").respond(json!({ "stopReason": "end_turn" }));
    let (mut app, temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);
    fs::write(temp.path().join("session.txt"), "session snapshot\n").unwrap();
    fs::write(temp.path().join("mention.txt"), "mention snapshot\n").unwrap();

    type_text(&mut app, "/context add session.txt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    type_text(&mut app, "/mention mention.txt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].context_files.len(), 1);
    assert_eq!(app.agents.threads[0].next_prompt_context_files.len(), 1);

    type_text(&mut app, "/context status");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let status = app.backend.status_message.clone().unwrap_or_default();
    assert!(status.contains("scope=session-only"), "status: {status}");
    assert!(status.contains("session.txt (17 bytes)"), "status: {status}");
    assert!(status.contains("mention.txt (17 bytes)"), "status: {status}");

    type_text(&mut app, "use both snapshots");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "mention prompt sent", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 1
    });
    let prompt = &fake.agent().requests_by_method("session/prompt")[0]["params"]["prompt"];
    assert_eq!(prompt.as_array().map(Vec::len), Some(3));
    assert!(prompt[2]["text"].as_str().is_some_and(|text| text.contains("One-turn user mention")));
    assert!(app.agents.threads[0].next_prompt_context_files.is_empty());
}

#[test]
fn composer_at_path_completion_completes_only_safe_unique_workspace_file() {
    let (mut app, temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);
    fs::write(temp.path().join("mention-target.txt"), "safe\n").unwrap();
    fs::write(temp.path().join(".env"), "TOKEN=secret\n").unwrap();

    type_text(&mut app, "review @mention-t");
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].draft, "review @mention-target.txt");

    press(&mut app, KeyCode::Char(' '), KeyModifiers::NONE);
    type_text(&mut app, "@.e");
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].draft, "review @mention-target.txt @.e\t");
}

#[test]
fn add_dir_requires_advertised_capability_and_explicit_confirmation() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": { "sessionCapabilities": { "additionalDirectories": {} } }
        }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
        .wait_for("session/set_mode")
        .respond(json!({}));
    let (mut app, temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);
    let extra = temp.path().join("extra-root");
    fs::create_dir(&extra).unwrap();

    type_text(&mut app, "/add-dir extra-root");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.agents.additional_directory_confirmation.is_some());
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.agents.additional_workspace_roots.is_empty());

    type_text(&mut app, "/add-dir extra-root");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.additional_workspace_roots.len(), 1);
    assert!(
        app.backend
            .status_message
            .as_deref()
            .is_some_and(|status| status.contains("current provider session unchanged"))
    );
}

#[test]
fn ps_tasks_and_terminal_stop_stay_scoped_to_active_agent_session() {
    let (mut app, _temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);
    let request = ee_agent_protocol::CreateTerminalRequest::new(
        ee_agent_protocol::SessionId::new("s1"),
        "sleep",
    )
    .args(vec![String::from("30")]);
    let created =
        app.agents.terminals.spawn(&request, Some("fake")).expect("owned terminal spawns");
    let terminal_id = created.terminal_id.0.to_string();

    type_text(&mut app, "/ps");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        app.backend.status_message.as_deref().is_some_and(|status| status.contains(&terminal_id))
    );

    type_text(&mut app, "/tasks");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        app.backend
            .status_message
            .as_deref()
            .is_some_and(|status| status.contains("subagent tasks: unavailable"))
    );

    type_text(&mut app, &format!("/stop {terminal_id}"));
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        app.backend
            .status_message
            .as_deref()
            .is_some_and(|status| status.contains("stop requested"))
    );
    app.agents.terminals.kill_all();
}

#[test]
fn phase_four_history_queue_transcript_and_draft_controls_stay_local_and_safe() {
    let script = base_script()
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }))
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }))
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }))
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }))
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "first prompt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].state, ThreadUiState::Running);
    type_text(&mut app, "second prompt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    type_text(&mut app, "third prompt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].queued_prompts.len(), 2);

    type_text(&mut app, "/queue move 2 1");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    type_text(&mut app, "/queue edit 1 revised third prompt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "queued prompts dispatch", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 3
    });
    let prompts = fake.agent().requests_by_method("session/prompt");
    assert_eq!(prompts[0]["params"]["prompt"][0]["text"], "first prompt");
    assert_eq!(prompts[1]["params"]["prompt"][0]["text"], "revised third prompt");
    assert_eq!(prompts[2]["params"]["prompt"][0]["text"], "second prompt");
    wait_until(&mut app, "queued turns complete", |app| {
        app.agents.threads[0].state == ThreadUiState::Ready
    });

    type_text(&mut app, "duplicate prompt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "first duplicate completes", |app| {
        app.agents.threads[0].state == ThreadUiState::Ready
    });
    type_text(&mut app, "duplicate prompt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "second duplicate completes", |app| {
        app.agents.threads[0].state == ThreadUiState::Ready
    });
    assert_eq!(
        app.agents.threads[0]
            .prompt_history
            .iter()
            .filter(|prompt| prompt.as_str() == "duplicate prompt")
            .count(),
        1,
        "adjacent duplicate prompts collapse in history"
    );

    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].draft, "duplicate prompt");
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert!(app.agents.threads[0].draft.is_empty());
    type_text(&mut app, "second");
    press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert_eq!(app.agents.threads[0].draft, "second prompt");
    press(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);

    type_text(&mut app, "/details on");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.agents.threads[0].transcript_detail);
    app.key_bindings.insert(
        crate::keymap::BindingKey {
            mode: Mode::Agent,
            key: KeyCode::Char('x'),
            modifiers: KeyModifiers::CONTROL,
            prefix: None,
        },
        crate::keymap::Action::AgentToggleTranscriptRaw,
    );
    press(&mut app, KeyCode::Char('x'), KeyModifiers::CONTROL);
    assert!(app.agents.threads[0].transcript_raw, "configured Agent-mode keymap action dispatches");
    app.agents.threads[0].transcript.push(TranscriptItem::ToolCall {
        id: String::from("diff-1"),
        title: String::from("edit"),
        status: String::from("completed"),
        detail: String::from("kind: edit · content: diff: src/example.rs"),
        response_group: 77,
        at: SystemTime::now(),
    });
    assert_eq!(
        app.agents.threads[0].response_group_change_summary(77).as_deref(),
        Some("changes: src/example.rs")
    );

    type_text(&mut app, "long local draft");
    press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert!(app.agents.threads[0].draft.is_empty());
    press(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
    assert_eq!(app.agents.threads[0].draft, "long local draft");
    press(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL | KeyModifiers::SHIFT);
    let request = app.take_agent_external_editor_request().expect("external editor request queued");
    assert_eq!(request.session_id.as_deref(), Some("s1"));
    assert_eq!(request.draft, "long local draft");
    assert!(fake.agent().requests_by_method("session/prompt").len() == 5);
}

#[test]
fn context_slash_command_rejects_protected_outside_and_oversized_files() {
    let (mut app, temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);
    fs::write(temp.path().join(".env"), "TOKEN=secret\n").unwrap();
    fs::write(temp.path().join("too-large.txt"), "x".repeat(64 * 1024 + 1)).unwrap();

    type_text(&mut app, "/context add .env");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        app.backend.status_message.as_deref().is_some_and(|message| message.contains("protected"))
    );
    type_text(&mut app, "/context add /tmp/outside-context.txt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        app.backend.status_message.as_deref(),
        Some("context path must be workspace-relative")
    );
    type_text(&mut app, "/context add too-large.txt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.backend.status_message.as_deref().is_some_and(|message| message.contains("limit")));
    assert!(app.agents.threads[0].context_files.is_empty());

    fs::write(temp.path().join("first.txt"), "a".repeat(64 * 1024)).unwrap();
    fs::write(temp.path().join("second.txt"), "b".repeat(64 * 1024)).unwrap();
    fs::write(temp.path().join("third.txt"), "c").unwrap();
    type_text(&mut app, "/context add first.txt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    type_text(&mut app, "/context add second.txt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    type_text(&mut app, "/context add third.txt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.backend.status_message.as_deref().is_some_and(|message| message.contains("total")));
    assert_eq!(app.agents.threads[0].context_files.len(), 2);
}

#[cfg(unix)]
#[test]
fn context_and_mentions_reject_workspace_symlink_escapes_before_reading() {
    let (mut app, temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), "outside-secret-value\n").unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.txt"), temp.path().join("escape.txt"))
        .unwrap();

    type_text(&mut app, "/context add escape.txt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        app.backend.status_message.as_deref(),
        Some("context file outside primary workspace")
    );
    assert!(app.agents.threads[0].context_files.is_empty());

    type_text(&mut app, "/mention escape.txt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        app.backend.status_message.as_deref(),
        Some("context file outside primary workspace")
    );
    assert!(app.agents.threads[0].next_prompt_context_files.is_empty());
    assert!(
        !app.agents.threads[0]
            .system_notices()
            .iter()
            .any(|notice| notice.contains("outside-secret-value")),
        "rejected target content must never reach transcript notices"
    );
}

#[test]
fn context_slash_command_uses_unsaved_open_buffer_snapshot() {
    let script =
        base_script().wait_for("session/prompt").respond(json!({ "stopReason": "end_turn" }));
    let (mut app, temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);
    let path = temp.path().join("dirty.txt");
    fs::write(&path, "disk text\n").unwrap();
    let id = app.backend.open_buffer(Some(path.clone())).unwrap();
    app.backend.switch_to_id(id).unwrap();
    wait_until(&mut app, "context buffer open", |app| {
        app.backend.active().path.as_deref() == Some(path.as_path())
    });
    app.backend.replace_line_range(0, 0, &[String::from("unsaved text")]).unwrap();
    app.backend.flush_all_pending_edits().unwrap();
    wait_until(&mut app, "context buffer dirty", |app| {
        app.backend
            .active()
            .whole_text()
            .as_deref()
            .is_some_and(|text| text.starts_with("unsaved text"))
    });

    type_text(&mut app, "/context add dirty.txt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.agents.threads[0].context_files[0].content.starts_with("unsaved text"));
    type_text(&mut app, "use dirty context");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "dirty context prompt sent", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 1
    });
    let prompts = fake.agent().requests_by_method("session/prompt");
    let context = prompts[0]["params"]["prompt"][1]["text"].as_str().expect("context block text");
    assert!(context.contains("unsaved text"), "{context}");
    assert!(!context.contains("disk text"), "{context}");
}
