//! `impl App` agents-pane tests: pane_tests domain.
use super::*;

#[test]
fn agents_disabled_path_opens_disabled_message_without_host() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join(".ee.toml"), "[agents]\nenabled = false\n").unwrap();
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let mut app = App::from_path(None).unwrap();
    drop(_cwd_restore);
    drop(_cwd_lock);

    run_ex(&mut app, "agents");

    let status = app.backend.status_message.clone().unwrap_or_default();
    assert!(status.contains("agents mode disabled"), "unexpected status: {status}");
    assert!(app.agents.host.is_none(), "disabled path must not start the host");
    assert_eq!(app.agents.layout, AgentPaneLayout::Closed);
    assert_eq!(app.mode, Mode::Normal);

    // The rest of the command surface stays inert too.
    run_ex(&mut app, "agents_layout");
    assert_eq!(app.agents.layout, AgentPaneLayout::Closed);
}

#[cfg(not(feature = "agents"))]
#[test]
fn agents_disabled_without_feature_stays_inert() {
    let mut app = App::from_path(None).unwrap();
    run_ex(&mut app, "agents");
    let status = app.backend.status_message.clone().unwrap_or_default();
    assert!(status.contains("compiled without `agents` feature"), "status: {status}");
}

// ── Enabled path + lazy start ────────────────────────────────────────────────

#[test]
fn agents_enabled_creates_pane_and_sends_lazy_session_new() {
    let (mut app, _temp, fake) = fake_agents_app(base_script());

    // Nothing starts until the user opens the pane.
    assert!(app.agents.host.is_none());
    assert_eq!(app.agents.layout, AgentPaneLayout::Closed);

    run_ex(&mut app, "agents");
    wait_until(&mut app, "first agent thread ready", |app| {
        app.agents.threads.len() == 1 && app.agents.threads[0].state == ThreadUiState::Ready
    });

    assert_eq!(app.agents.layout, AgentPaneLayout::Full);
    assert_eq!(app.mode, Mode::Agent);
    assert_eq!(app.agents.threads[0].session_id, "s1");
    assert_eq!(app.agents.threads[0].display_name, "1.fake");

    let agent = fake.agent();
    assert!(!agent.requests_by_method("initialize").is_empty(), "initialize must be sent");
    assert!(!agent.requests_by_method("session/new").is_empty(), "session/new must be sent");

    // Re-opening the pane must not create a second session.
    run_ex(&mut app, "agents");
    wait_until(&mut app, "session count stays one", |app| app.agents.threads.len() == 1);
    assert_eq!(agent.requests_by_method("session/new").len(), 1);
}

#[test]
fn explicit_agent_selection_starts_only_selected_process() {
    let (mut app, _temp, alpha, beta) = two_fake_agents_app();

    run_ex(&mut app, "agents_new beta");
    wait_until(&mut app, "beta thread ready", |app| {
        app.agents.threads.len() == 1 && app.agents.threads[0].state == ThreadUiState::Ready
    });

    assert_eq!(app.agents.threads[0].agent_id, "beta");
    assert_eq!(beta.agent().requests_by_method("initialize").len(), 1);
    assert_eq!(beta.agent().requests_by_method("session/new").len(), 1);
    assert!(alpha.handle.lock().expect("alpha handle poisoned").is_none());
}

#[test]
fn external_rubber_duck_uses_isolated_agent_then_root_synthesizes_verified_report() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join(".ee.toml"),
        r#"
root = true

[agents]
enabled = true

[agents.rubber_duck]
mode = "manual"
external_agent_id = "beta"
max_calls = 1
timeout_ms = 5000

[agents.servers.alpha]
command = "unused-alpha"

[agents.servers.beta]
command = "unused-beta"
"#,
    )
    .unwrap();
    fs::write(temp.path().join("lib.rs"), "pub fn value() -> u8 { 1 }\n").unwrap();
    commit_git_baseline(temp.path());
    fs::write(temp.path().join("lib.rs"), "pub fn value() -> u8 { 2 }\n").unwrap();

    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let mut app = App::from_path(None).unwrap();
    drop(_cwd_restore);
    drop(_cwd_lock);

    let alpha = ScriptedFake::new(
        FakeAgentScript::new()
            .wait_for("initialize")
            .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
            .wait_for("session/new")
            .respond(json!({ "sessionId": "alpha-root" }))
            .wait_for("session/set_mode")
            .respond(json!({}))
            .wait_for("session/prompt")
            .emit(wire::session_update(
                "alpha-root",
                wire::agent_message_chunk("root-synthesis", "Root synthesis: no findings."),
            ))
            .respond(json!({ "stopReason": "end_turn" })),
    );
    let report = r#"{"schema_version":1,"target":{"kind":"implementation"},"findings":[]}"#;
    let beta = ScriptedFake::new(
        FakeAgentScript::new()
            .wait_for("initialize")
            .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
            .wait_for("session/new")
            .respond(json!({ "sessionId": "beta-critic" }))
            .wait_for("session/prompt")
            .emit(wire::session_update(
                "beta-critic",
                wire::agent_message_chunk("critic-report", report),
            ))
            .respond(json!({ "stopReason": "end_turn" })),
    );
    app.agents.test_fake_transports.insert(String::from("alpha"), Arc::new(alpha.clone()));
    app.agents.test_fake_transports.insert(String::from("beta"), Arc::new(beta.clone()));

    run_ex(&mut app, "agents_new alpha");
    wait_until(&mut app, "root thread ready", |app| {
        app.agents.threads.len() == 1 && app.agents.threads[0].state == ThreadUiState::Ready
    });
    assert!(beta.handle.lock().unwrap().is_none(), "critic must remain lazy before invocation");

    type_text(&mut app, "/rubber-duck");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "root synthesis completed", |app| {
        app.agents.threads.len() == 1
            && app.agents.threads[0].state == ThreadUiState::Ready
            && app.agents.threads[0]
                .message_pairs()
                .iter()
                .any(|(_, text)| text.contains("Root synthesis: no findings"))
    });

    assert_eq!(alpha.agent().requests_by_method("initialize").len(), 1);
    assert_eq!(beta.agent().requests_by_method("initialize").len(), 1);
    assert_eq!(beta.agent().requests_by_method("session/new").len(), 1);
    assert_eq!(beta.agent().requests_by_method("session/prompt").len(), 1);
    assert_eq!(alpha.agent().requests_by_method("session/prompt").len(), 1);
    assert_eq!(app.agents.threads.len(), 1, "ephemeral critic must not become an editor thread");

    let critic_prompt = beta.agent().requests_by_method("session/prompt")[0].to_string();
    assert!(critic_prompt.contains("<untrusted_review_context>"));
    assert!(critic_prompt.contains("schema_version 1"));
    let synthesis_prompt = alpha.agent().requests_by_method("session/prompt")[0].to_string();
    assert!(synthesis_prompt.contains("<verified_external_critique>"));
    assert!(synthesis_prompt.contains("not validation, approval, completion evidence"));
    assert!(
        app.agents.threads[0]
            .system_notices()
            .iter()
            .any(|notice| notice.contains("host-forwarded read-only only"))
    );
}

#[test]
fn ambiguous_new_session_uses_lazy_picker_then_local_new_accepts_agent_id() {
    let (mut app, _temp, alpha, beta) = two_fake_agents_app();

    run_ex(&mut app, "agents_new");
    let picker = app.picker.as_ref().expect("ambiguous selection opens picker");
    assert_eq!(picker.kind, crate::picker::PickerKind::AgentServers);
    assert_eq!(picker.visible_count(), 2);
    assert!(picker.visible_items_range(0, 2)[0].contains("Alpha Agent (alpha)"));
    assert!(alpha.handle.lock().expect("alpha handle poisoned").is_none());
    assert!(beta.handle.lock().expect("beta handle poisoned").is_none());

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "alpha picker thread ready", |app| {
        app.agents.threads.len() == 1 && app.agents.threads[0].state == ThreadUiState::Ready
    });
    assert_eq!(app.agents.threads[0].agent_id, "alpha");
    assert!(beta.handle.lock().expect("beta handle poisoned").is_none());

    type_text(&mut app, "/new beta");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "beta local-new thread ready", |app| {
        app.agents.threads.len() == 2 && app.agents.threads[1].state == ThreadUiState::Ready
    });
    assert_eq!(app.agents.threads[1].agent_id, "beta");
    assert_eq!(alpha.agent().requests_by_method("session/new").len(), 1);
    assert_eq!(beta.agent().requests_by_method("session/new").len(), 1);
}

#[test]
fn unknown_agent_id_is_bounded_and_starts_nothing() {
    let (mut app, _temp, alpha, beta) = two_fake_agents_app();

    run_ex(&mut app, "agents_new missing");

    assert_eq!(
        app.backend.status_message.as_deref(),
        Some("unknown agent `missing`; configured: alpha, beta")
    );
    assert!(alpha.handle.lock().expect("alpha handle poisoned").is_none());
    assert!(beta.handle.lock().expect("beta handle poisoned").is_none());
}

#[test]
fn agents_mode_advertises_editor_backed_optional_client_capabilities() {
    let (mut app, _temp, fake) = fake_agents_app(base_script());

    open_pane_and_wait_ready(&mut app);

    let initialize = fake
        .agent()
        .requests_by_method("initialize")
        .into_iter()
        .next()
        .expect("initialize request sent");
    let client_capabilities = &initialize["params"]["clientCapabilities"];
    assert_eq!(client_capabilities["fs"]["readTextFile"], true);
    assert_eq!(client_capabilities["fs"]["writeTextFile"], true);
    assert_eq!(client_capabilities["terminal"], true);
    assert_eq!(client_capabilities["elicitation"]["form"], json!({}));
    assert_eq!(client_capabilities["elicitation"]["url"], json!({}));
}

#[test]
fn agents_close_restores_mode_that_opened_command_line() {
    let (mut app, _temp, _fake) = fake_agents_app(base_script());

    app.mode = Mode::CommandLine;
    app.command_mode_origin = Some(Mode::Insert);
    type_text(&mut app, "agents");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "first agent thread ready", |app| {
        app.agents.threads.len() == 1 && app.agents.threads[0].state == ThreadUiState::Ready
    });
    assert_eq!(app.mode, Mode::Agent);

    run_ex(&mut app, "agents_close");

    assert_eq!(app.mode, Mode::Insert, "focus returns to command-line origin mode");
    assert_eq!(app.agents.layout, AgentPaneLayout::Closed, "explicit close hides agents pane");
}

#[test]
fn colon_in_agents_pane_stays_in_agent_draft() {
    let (mut app, _temp, _fake) = fake_agents_app(base_script());

    open_pane_and_wait_ready(&mut app);
    press(&mut app, KeyCode::Char(':'), KeyModifiers::NONE);

    assert_eq!(
        app.mode,
        Mode::Agent,
        "colon must not open ee command line while agents pane has focus"
    );
    assert_eq!(app.command_buffer, "", "editor command buffer stays untouched");
    assert_eq!(app.agents.threads[0].draft, ":", "colon is regular agent prompt input");
}

#[test]
fn pane_startup_is_inert_and_editor_modes_unchanged_while_closed() {
    let (mut app, _temp, _fake) = fake_agents_app(base_script());

    assert_eq!(app.agents.layout, AgentPaneLayout::Closed);
    assert_eq!(app.mode, Mode::Normal);

    // Normal editing works while the pane is closed.
    press(&mut app, KeyCode::Char('i'), KeyModifiers::NONE);
    type_text(&mut app, "ab");
    wait_until(&mut app, "insert text lands", |app| app.backend.lines == vec![String::from("ab")]);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::Normal);

    // Command line still opens.
    press(&mut app, KeyCode::Char(':'), KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::CommandLine);
}

// ── Prompt submission ────────────────────────────────────────────────────────

#[test]
fn enter_without_config_reports_needed_server_and_keeps_draft() {
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
    type_text(&mut app, "qwe");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    let status = app.backend.status_message.clone().unwrap_or_default();
    assert!(status.contains("no agent configured"), "unexpected status: {status}");
    assert_eq!(app.agents.pending_draft, "qwe");
    assert!(app.agents.threads.is_empty());
}
