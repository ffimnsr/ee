//! `impl App` agents-pane tests: render_tests domain.
use super::*;

#[test]
fn agents_transcript_bottom_aligns_short_chat() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "answer")))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "question");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "chat lands", |app| app.agents.threads[0].message_pairs().len() >= 2);

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let rendered: Vec<String> = (0..20)
        .map(|y| (0..80).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();

    let first_transcript_row = rendered
        .iter()
        .position(|row| row.contains("session started"))
        .expect("first transcript row");
    let user_row = rendered.iter().position(|row| row.contains("question")).expect("user row");
    let agent_row = rendered.iter().position(|row| row.contains("answer")).expect("agent row");
    assert!(first_transcript_row >= 10, "short chat should sit near composer, rows={rendered:#?}");
    assert!(user_row < agent_row, "messages remain chronological");
}

#[test]
fn agents_transcript_preserves_agent_markdown_newlines() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "s1",
            wire::agent_message_chunk("m1", "first line\nsecond line"),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "question");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "multiline response lands", |app| {
        app.agents.threads[0]
            .message_pairs()
            .iter()
            .any(|(_, text)| text == "first line\nsecond line")
    });

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let rendered: Vec<String> = (0..20)
        .map(|y| (0..80).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();

    let first_row = rendered.iter().position(|row| row.contains("first line")).expect("first row");
    let second_row =
        rendered.iter().position(|row| row.contains("second line")).expect("second row");
    assert!(first_row < second_row, "newlines must render as separate rows: {rendered:#?}");
    assert!(
        rendered.iter().all(|row| !(row.contains("first line") && row.contains("second line"))),
        "newline collapsed onto one row: {rendered:#?}"
    );
    let first_col = rendered[first_row].find("first line").expect("first line column");
    let second_col = rendered[second_row].find("second line").expect("second line column");
    assert_eq!(first_col, second_col, "continuation rows must align: {rendered:#?}");
}

#[test]
fn agent_transcript_discards_blank_rendered_lines() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "s1",
            wire::agent_message_chunk("m1", "first line\n\nsecond line\n\n"),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "question");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "multiline response lands", |app| {
        app.agents.threads[0]
            .message_pairs()
            .iter()
            .any(|(_, text)| text == "first line\n\nsecond line\n\n")
    });

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let rendered: Vec<String> = (0..20)
        .map(|y| (0..80).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();
    let first_row = rendered.iter().position(|row| row.contains("first line")).expect("first row");
    let second_row =
        rendered.iter().position(|row| row.contains("second line")).expect("second row");

    assert_eq!(second_row, first_row + 1, "blank transcript rows must be trimmed: {rendered:#?}");
}

#[test]
fn long_agent_responses_scroll_by_visual_rows() {
    let response = "wrapped response ".repeat(200);
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", &response)))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);
    run_ex(&mut app, "agents_layout full");

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "long assistant response lands", |app| {
        app.agents.threads[0]
            .message_pairs()
            .iter()
            .any(|(nick, text)| nick == "fake" && text == &response)
    });

    let pane = crate::ui::agents_pane_rect_for(
        ratatui::layout::Rect { x: 0, y: 0, width: 40, height: 12 },
        &app,
    )
    .expect("full agents pane");
    let max_scroll = crate::ui::agents_transcript_scroll_max(&app, pane);
    let transcript_item_max = app.agents.threads[0].transcript.len().saturating_sub(1);
    assert!(
        max_scroll > transcript_item_max,
        "long wrapped response needs more visual scroll rows than transcript items"
    );

    let thread = &mut app.agents.threads[0];
    thread.scroll_by(1, max_scroll);
    assert_eq!(thread.scroll, 1, "one scroll step advances one rendered row");
    assert!(!thread.stick_to_bottom, "scrolling up unpins the view");
    thread.scroll_to(usize::MAX, max_scroll);
    assert_eq!(thread.scroll, max_scroll, "scroll stays within visual-row bounds");
    assert!(thread.stick_to_bottom, "last rendered row re-pins the view");
}

#[test]
fn scrollback_pins_to_bottom_until_user_scrolls_up() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "one")))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "assistant message lands", |app| {
        app.agents.threads[0].message_pairs().len() >= 2
    });

    assert!(app.agents.threads[0].stick_to_bottom, "new content must pin to bottom");

    press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
    assert!(!app.agents.threads[0].stick_to_bottom, "scrolling up unpins the view");
    let max_scroll = app.agents.threads[0].transcript.len().saturating_sub(1);
    assert!(
        app.agents.threads[0].scroll <= max_scroll,
        "scroll offset must stay within the transcript"
    );

    press(&mut app, KeyCode::End, KeyModifiers::NONE);
    assert!(app.agents.threads[0].stick_to_bottom, "End re-pins the view");

    press(&mut app, KeyCode::Home, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].scroll, 0);

    press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
    assert!(app.agents.threads[0].stick_to_bottom, "scrolling to the end re-pins");
}
