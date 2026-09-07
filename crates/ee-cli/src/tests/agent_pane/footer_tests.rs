//! `impl App` agents-pane tests: footer_tests domain.
use super::*;

#[test]
fn no_session_footer_uses_agent_status_background_without_mode_fallback() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join(".ee.toml"), "[agents]\nenabled = true\n").unwrap();
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let mut app = App::from_path(None).unwrap();
    drop(_cwd_restore);
    drop(_cwd_lock);

    run_ex(&mut app, "agents");
    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let rendered: Vec<String> = (0..24)
        .map(|y| (0..120).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();
    let footer_y = rendered
        .iter()
        .position(|row| row.contains("agents [no session] | mode:unset"))
        .expect("no-session footer row");

    let footer = &rendered[footer_y];
    assert!(
        (0..120).all(|x| {
            rows.cell((x, footer_y as u16)).unwrap().bg == crate::theme::ui::BG_AGENT_STATUS
        }),
        "no-session footer must use agent-status background: {rendered:#?}"
    );
    assert!(
        !footer.contains('/'),
        "no-session footer must not advertise slash commands: {rendered:#?}"
    );
    assert!(
        !footer.contains("Enter") && !footer.contains("Esc"),
        "no-session footer must not advertise keyboard hints: {rendered:#?}"
    );
}

#[test]
fn cold_launch_negotiates_ask_for_agent_without_mode_state() {
    let (mut app, _temp, fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);

    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let rendered: Vec<String> = (0..24)
        .map(|y| (0..120).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();
    let composer_row =
        rendered.iter().position(|row| row.contains("prompt>")).expect("composer row");

    assert!(
        rendered[composer_row - 1].contains("mode:ask"),
        "footer must show negotiated ask mode: {rendered:#?}"
    );
    let requests = fake.agent().requests_by_method("session/set_mode");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["params"]["modeId"], "ask");
}

#[test]
fn footer_renders_current_agent_mode() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({
            "sessionId": "s1",
            "modes": {
                "currentModeId": "plan",
                "availableModes": [
                    { "id": "ask", "name": "Ask" },
                    { "id": "plan", "name": "Plan" }
                ]
            }
        }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let rendered: Vec<String> = (0..24)
        .map(|y| (0..120).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();
    let composer_row =
        rendered.iter().position(|row| row.contains("prompt>")).expect("composer row");
    let footer_row = &rendered[composer_row - 1];

    assert!(
        footer_row.contains("mode:plan"),
        "footer must render current agent mode: {rendered:#?}"
    );
    assert!(
        !footer_row.contains("Ctrl-"),
        "footer must not render keyboard shortcuts: {rendered:#?}"
    );
}

#[test]
fn turn_completed_records_metrics_and_renders_tokens() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "s1",
            json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": "hmm" } }),
        ))
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "hello back")))
        .respond(json!({
            "stopReason": "end_turn",
            "usage": {
                "totalTokens": 8431,
                "inputTokens": 6120,
                "outputTokens": 2311,
            }
        }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "hello agent");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "turn metrics recorded", |app| {
        app.agents.threads[0].last_turn_metrics.is_some()
    });

    let thread = &app.agents.threads[0];
    let metrics = thread.last_turn_metrics.as_ref().expect("metrics recorded");
    let tokens = metrics.tokens.as_ref().expect("reported tokens attached");
    assert_eq!(tokens.total_tokens, 8431);
    assert_eq!(tokens.input_tokens, 6120);
    assert_eq!(tokens.output_tokens, 2311);
    assert!(!thread.turn_metrics.is_empty(), "metrics keyed by response group");
    assert!(thread.turn_started_at.is_none(), "start marker cleared");

    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let rendered: Vec<String> = (0..40)
        .map(|y| (0..120).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();
    let joined = rendered.join("\n");
    assert!(
        joined.contains("8,431 tokens (6,120 in / 2,311 out)"),
        "response header must render turn metrics: {rendered:#?}"
    );
    assert!(
        joined.contains("last:0.0s · 8,431 tokens"),
        "footer must render latest turn metrics: {rendered:#?}"
    );
}

#[test]
fn turn_without_usage_renders_elapsed_only() {
    let script =
        base_script().wait_for("session/prompt").respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "hello agent");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "turn metrics recorded", |app| {
        app.agents.threads[0].last_turn_metrics.is_some()
    });

    let metrics = app.agents.threads[0].last_turn_metrics.as_ref().expect("metrics recorded");
    assert_eq!(metrics.tokens, None, "unknown usage stays unknown, never zero");
    assert_eq!(crate::app::turn_metrics_label(metrics), "0.0s");
}

#[test]
fn usage_update_renders_context_window_right_aligned_above_composer() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "s1",
            json!({ "sessionUpdate": "usage_update", "used": 10_000, "size": 100_000 }),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "hello agent");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "usage update recorded", |app| app.agents.threads[0].usage.is_some());
    assert_eq!(app.agents.threads[0].usage.as_deref(), Some("10k/100k tokens"));

    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    assert!(
        (0..24).all(|y| rows.cell((0, y)).unwrap().symbol() != "│")
            && (0..24).all(|y| rows.cell((119, y)).unwrap().symbol() != "│"),
        "agents pane must not render an outer vertical border"
    );
    let rendered: Vec<String> = (0..24)
        .map(|y| (0..120).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();

    let composer_row =
        rendered.iter().position(|row| row.contains("prompt>")).expect("composer row");
    let footer_row = &rendered[composer_row - 1];
    assert!(
        footer_row.contains("10k/100k tokens"),
        "footer row above the composer must carry the context usage: {rendered:#?}"
    );
    assert!(
        (0..120).all(|x| {
            rows.cell((x, (composer_row - 1) as u16)).unwrap().bg
                == crate::theme::ui::BG_AGENT_STATUS
        }),
        "usage footer must paint its full row background: {rendered:#?}"
    );
    let label = "10k/100k tokens";
    // Byte offsets shift for multi-byte glyphs; compare in character columns.
    let label_end = footer_row
        .match_indices(label)
        .last()
        .map(|(byte_start, _)| footer_row[..byte_start].chars().count() + label.chars().count());
    assert_eq!(
        label_end,
        Some(120),
        "context usage must sit right-aligned at the row end: {rendered:#?}"
    );

    // Narrow panes: the left footer overflows, but the usage label must stay
    // pinned to the rightmost edge (not truncated away).
    let backend = TestBackend::new(60, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let rendered: Vec<String> = (0..24)
        .map(|y| (0..60).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();
    let composer_row =
        rendered.iter().position(|row| row.contains("prompt>")).expect("composer row");
    let footer_row = &rendered[composer_row - 1];
    assert!(
        footer_row.contains("10k/100k tokens"),
        "usage must survive narrow panes: {rendered:#?}"
    );
    let label_end = footer_row
        .match_indices(label)
        .last()
        .map(|(byte_start, _)| footer_row[..byte_start].chars().count() + label.chars().count());
    assert_eq!(
        label_end,
        Some(60),
        "usage must stay right-aligned at the edge in narrow panes: {rendered:#?}"
    );
}

#[test]
fn composer_uses_only_terminal_cursor() {
    let (mut app, _temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);
    type_text(&mut app, "hello");

    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();

    let cursor = terminal.get_cursor_position().unwrap();
    let rows = terminal.backend().buffer();
    let composer_row: String =
        (0..120).map(|x| rows.cell((x, cursor.y)).unwrap().symbol()).collect();
    let composer_start = composer_row.find("prompt> hello").expect("composer contents");
    let composer_start_col = composer_row[..composer_start].chars().count();

    assert!(
        !composer_row.contains('█'),
        "composer must not render a second cursor: {composer_row:?}"
    );
    assert_eq!(
        usize::from(cursor.x),
        composer_start_col + "prompt> hello".len(),
        "terminal cursor must sit directly after composer text"
    );
}

#[test]
fn turn_metrics_label_formats_duration_and_tokens() {
    use ee_agent_host::TurnMetrics;
    use ee_agent_protocol::Usage;
    use std::time::Duration;

    let plain = TurnMetrics { elapsed: Duration::from_millis(12_400), tokens: None };
    assert_eq!(crate::app::turn_metrics_label(&plain), "12.4s");

    let with_tokens = TurnMetrics {
        elapsed: Duration::from_secs(192),
        tokens: Some(Usage::new(8_431, 6_120, 2_311)),
    };
    assert_eq!(
        crate::app::turn_metrics_label(&with_tokens),
        "3m 12s · 8,431 tokens (6,120 in / 2,311 out)"
    );
    assert_eq!(crate::app::format_duration(Duration::from_millis(500)), "0.5s");
}
