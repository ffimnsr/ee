use super::*;

fn approve_until_repeated_validation_complete(
    app: &mut App,
    label: &str,
    max_approvals: usize,
) -> usize {
    let mut progress_deadline = Instant::now() + WAIT;
    let mut approvals = 0;
    let mut last_state = app.agents.threads[0].state;
    let mut last_evidence_count = 0;
    loop {
        app.pump_agents();
        let _ = app.backend.drain_events();
        let state = app.agents.threads[0].state;
        let evidence_count = app.agents.threads[0]
            .terminal_evidence
            .as_ref()
            .map_or(0, |summary| summary.evidence_ids.len());
        if state != last_state || evidence_count != last_evidence_count {
            progress_deadline = Instant::now() + WAIT;
            last_state = state;
            last_evidence_count = evidence_count;
        }
        if !app.agents.approvals.is_empty() {
            assert!(approvals < max_approvals, "{label} exceeded {max_approvals} approvals");
            press(app, KeyCode::Enter, KeyModifiers::NONE);
            approvals += 1;
            progress_deadline = Instant::now() + WAIT;
        } else if state == ThreadUiState::Ready
            && app.agents.threads[0].terminal_evidence.as_ref().is_some_and(|summary| {
                summary.status == TurnTerminalStatus::Blocked
                    && summary.blocker == Some(TurnBlocker::ValidationFailed)
            })
        {
            return approvals;
        }
        if Instant::now() >= progress_deadline {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "timed out waiting for {label}; approvals={approvals}; state={:?}; evidence={:?}; status={:?}",
        app.agents.threads[0].state,
        app.agents.threads[0].terminal_evidence,
        app.backend.status_message.as_deref()
    );
}

#[test]
fn phase_six_live_openrouter_pane_repeated_selected_validation_stops_repair() {
    let _live_lock = phase_six_live_lock();
    let workspace = tempfile::tempdir().expect("fixture workspace");
    let target = workspace.path().join("lib.rs");
    fs::write(workspace.path().join(".ee.toml"), AGENTS_TOML).expect("write agents config");
    fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"phase-six-validation\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\npath = \"lib.rs\"\n",
    )
    .expect("write cargo manifest");
    fs::write(&target, "pub fn phase_six() {}\n").expect("write baseline Rust file");
    commit_git_baseline(workspace.path());

    let scripted = ScriptedOpenRouterCompletion::new(vec![
        live_tool_response(
            "write-invalid-initial",
            "write_file",
            json!({ "path": target.display().to_string(), "content": "pub fn phase_six() {\n" }),
        ),
        live_completion_response("initial implementation complete"),
        live_tool_response(
            "write-invalid-repair",
            "write_file",
            json!({ "path": target.display().to_string(), "content": "pub fn phase_six_repaired() {\n" }),
        ),
        live_completion_response("repair attempted"),
    ]);
    let state = tempfile::tempdir().expect("fixture session state");
    let factory = LiveOpenRouterTransport::new(
        openrouter_fixture_config(),
        state.path().join("agent-sessions"),
        scripted.clone(),
    );
    let mut app = live_openrouter_app_in(workspace.path(), factory.clone());
    let buffer_id = app.backend.open_buffer(Some(target.clone())).expect("open target buffer");
    app.backend.switch_to_id(buffer_id).expect("focus target buffer");
    wait_until(&mut app, "target buffer loaded", |app| {
        app.backend.active().whole_text().as_deref() == Some("pub fn phase_six() {}\n")
    });
    open_pane_and_wait_ready(&mut app);
    select_live_write_mode(&mut app);

    type_text(&mut app, "write invalid Rust and exercise bounded repair");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let approval_count = approve_until_repeated_validation_complete(
        &mut app,
        "repeated validation provider completion",
        8,
    );
    assert!(
        approval_count >= 3,
        "two writes and selected validation need approval; got {approval_count}"
    );

    let thread = &app.agents.threads[0];
    assert_eq!(
        app.agents
            .action_log
            .iter()
            .filter(|action| format!("{action:?}").starts_with("Write {"))
            .count(),
        2,
        "initial write plus one repair write must execute"
    );
    assert_eq!(
        scripted.request_bodies().len(),
        4,
        "repair controller must stop before another model loop"
    );
    assert!(
        scripted.request_bodies().iter().any(|body| {
            body["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["content"]
                        .as_str()
                        .is_some_and(|text| text.contains("Repair controller request."))
                })
            })
        }),
        "repair request must use fresh production repair context"
    );
    assert_eq!(
        thread.terminal_evidence.as_ref().expect("failed validation evidence").blocker,
        Some(TurnBlocker::ValidationFailed)
    );

    app.shutdown_agents();
    factory.shutdown();
}

#[test]
fn url_elicitation_completion_clears_prompt_and_marks_complete() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(json!({
            "jsonrpc": "2.0",
            "id": 202,
            "method": "elicitation/create",
            "params": {
                "mode": "url",
                "sessionId": "s1",
                "elicitationId": "el-1",
                "url": "https://example.com/authorize?client=ee",
                "message": "authorize the agent"
            }
        }))
        .delay(50)
        .emit(elicitation_complete("el-1"))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "url elicitation completion handled", |app| {
        app.agents.elicitation().is_none() && fake.agent().response_with_id(202).is_some()
    });
    let response = fake.agent().response_with_id(202).expect("completion response");
    assert_eq!(response["result"]["action"], "accept");
    wait_until(&mut app, "completion notice lands", |app| {
        app.agents.threads[0]
            .system_notices()
            .iter()
            .any(|notice| notice.contains("elicitation completed: el-1"))
    });
}

#[test]
fn phase_six_live_openrouter_pane_diagnostics_regression_reports_blocked_evidence() {
    let _live_lock = phase_six_live_lock();
    let workspace = tempfile::tempdir().expect("fixture workspace");
    let target = workspace.path().join("diagnostics.txt");
    fs::write(workspace.path().join(".ee.toml"), AGENTS_TOML).expect("write agents config");
    fs::write(&target, "before\n").expect("write baseline file");
    commit_git_baseline(workspace.path());

    let scripted =
        live_write_script(&target, "write-live-diagnostics", "after\n", "diagnostics regressed");
    let state = tempfile::tempdir().expect("fixture session state");
    let factory = LiveOpenRouterTransport::new(
        openrouter_fixture_config(),
        state.path().join("agent-sessions"),
        scripted.clone(),
    );
    let mut app = live_openrouter_app_in(workspace.path(), factory.clone());
    let buffer_id = app.backend.open_buffer(Some(target.clone())).expect("open target buffer");
    app.backend.switch_to_id(buffer_id).expect("focus target buffer");
    wait_until(&mut app, "target buffer loaded", |app| {
        app.backend.active().whole_text().as_deref() == Some("before\n")
    });
    app.set_pre_write_verification_test_hook(|app| {
        app.backend.diagnostics = vec![Diagnostic {
            range: Range { start: 0, end: 5 },
            severity: DiagnosticSeverity::Error,
            message: String::from("fresh post-write diagnostic"),
            source: Some(String::from("test-lsp")),
            code: Some(String::from("E-POST-WRITE")),
        }];
    });
    open_pane_and_wait_ready(&mut app);
    select_live_write_mode(&mut app);

    type_text(&mut app, "trigger real diagnostic regression");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "diagnostic write approval", |app| app.agents.approvals.len() == 1);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "diagnostic provider turn completion", |app| {
        app.agents.threads[0].state == ThreadUiState::Ready
            && app.agents.threads[0].terminal_evidence.as_ref().is_some_and(|summary| {
                summary.status == TurnTerminalStatus::Blocked
                    && summary.blocker == Some(TurnBlocker::DiagnosticsFailed)
            })
    });

    let thread = &app.agents.threads[0];
    let summary = thread.terminal_evidence.as_ref().expect("diagnostics pane evidence");
    assert_eq!(summary.status, TurnTerminalStatus::Blocked);
    assert_eq!(summary.blocker, Some(TurnBlocker::DiagnosticsFailed));
    assert_eq!(summary.safe_follow_up, SafeFollowUp::RefreshEvidence);
    assert_eq!(fs::read_to_string(&target).expect("read diagnostics write"), "after\n");
    assert_eq!(app.backend.diagnostics.len(), 1, "post-write diagnostic reaches editor state");
    let bodies = scripted.request_bodies();
    assert_eq!(bodies.len(), 3, "diagnostic failure triggers one bounded repair model round");
    assert!(bodies.iter().any(|body| {
        body["messages"].as_array().is_some_and(|messages| {
            messages.iter().any(|message| {
                message["content"]
                    .as_str()
                    .is_some_and(|text| text.contains("Repair controller request."))
            })
        })
    }));

    app.shutdown_agents();
    factory.shutdown();
}

#[test]
fn phase_six_live_openrouter_pane_stale_revision_after_evidence_is_blocked() {
    let _live_lock = phase_six_live_lock();
    let workspace = tempfile::tempdir().expect("fixture workspace");
    let target = workspace.path().join("stale.txt");
    fs::write(workspace.path().join(".ee.toml"), AGENTS_TOML).expect("write agents config");
    fs::write(&target, "before\n").expect("write baseline file");
    commit_git_baseline(workspace.path());

    let scripted = live_write_script(&target, "write-stale", "after\n", "write complete");
    let state = tempfile::tempdir().expect("fixture session state");
    let factory = LiveOpenRouterTransport::new(
        openrouter_fixture_config(),
        state.path().join("agent-sessions"),
        scripted.clone(),
    );
    let mut app = live_openrouter_app_in(workspace.path(), factory.clone());
    let buffer_id = app.backend.open_buffer(Some(target.clone())).expect("open target buffer");
    app.backend.switch_to_id(buffer_id).expect("focus target buffer");
    wait_until(&mut app, "target buffer loaded", |app| {
        app.backend.active().whole_text().as_deref() == Some("before\n")
    });
    app.set_post_write_test_hook(|app| {
        app.backend
            .replace_line_range(0, 0, &[String::from("intervening editor mutation")])
            .expect("mutate buffer after evidence capture");
        app.backend.flush_all_pending_edits().expect("flush intervening editor mutation");
    });
    open_pane_and_wait_ready(&mut app);
    select_live_write_mode(&mut app);

    type_text(&mut app, "write then mutate editor after verification capture");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "stale write approval", |app| app.agents.approvals.len() == 1);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "stale revision evidence", |app| {
        app.agents.threads[0].state == ThreadUiState::Ready
            && app.agents.threads[0].terminal_evidence.as_ref().is_some_and(|summary| {
                summary.status == TurnTerminalStatus::Blocked
                    && summary.blocker == Some(TurnBlocker::StaleRevision)
                    && summary.safe_follow_up == SafeFollowUp::RefreshEvidence
            })
    });

    let thread = &app.agents.threads[0];
    assert_eq!(fs::read_to_string(&target).expect("read saved agent write"), "after\n");
    assert_eq!(app.backend.active().whole_text().as_deref(), Some("intervening editor mutation\n"));
    assert_eq!(
        thread.terminal_evidence.as_ref().expect("stale evidence").blocker,
        Some(TurnBlocker::StaleRevision)
    );
    assert_eq!(scripted.request_bodies().len(), 2);

    app.shutdown_agents();
    factory.shutdown();
}
