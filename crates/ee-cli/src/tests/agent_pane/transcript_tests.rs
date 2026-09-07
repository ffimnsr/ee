//! `impl App` agents-pane tests: transcript_tests domain.
use super::*;

#[test]
fn separate_user_turns_do_not_concatenate_without_message_ids() {
    let script = base_script()
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }))
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }))
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    for (index, prompt) in ["one", "two", "three"].into_iter().enumerate() {
        type_text(&mut app, prompt);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        wait_until(&mut app, "turn completed", |app| {
            app.agents.threads[0]
                .system_notices()
                .iter()
                .filter(|notice| notice.contains("turn completed"))
                .count()
                > index
        });
    }

    let pairs = app.agents.threads[0].message_pairs();
    assert_eq!(
        pairs,
        vec![
            (String::from("you"), String::from("one")),
            (String::from("you"), String::from("two")),
            (String::from("you"), String::from("three")),
        ]
    );
}

#[test]
fn assistant_chunks_with_reused_message_ids_stay_in_their_turn() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "first reply")))
        .respond(json!({ "stopReason": "end_turn" }))
        .wait_for("session/prompt")
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "second reply")))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "first question");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "first reply", |app| app.agents.threads[0].message_pairs().len() == 2);

    type_text(&mut app, "second question");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "second reply", |app| app.agents.threads[0].message_pairs().len() == 4);

    assert_eq!(
        app.agents.threads[0].message_pairs(),
        vec![
            (String::from("you"), String::from("first question")),
            (String::from("fake"), String::from("first reply")),
            (String::from("you"), String::from("second question")),
            (String::from("fake"), String::from("second reply")),
        ]
    );
}

#[test]
fn streamed_assistant_chunks_render_in_order_with_thoughts() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "hel")))
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "lo")))
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "agent_thought_chunk",
                "content": { "type": "text", "text": "hmm" }
            }),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "hi");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "chunks merged in order", |app| {
        app.agents.threads[0].message_pairs().len() == 3
    });
    assert!(app.agents.show_thoughts, "thought streaming must be visible by default");
    let pairs = app.agents.threads[0].message_pairs();
    assert_eq!(
        pairs,
        vec![
            (String::from("you"), String::from("hi")),
            (String::from("fake"), String::from("hello")),
            (String::from("think"), String::from("hmm")),
        ]
    );

    // Nick-column wrapping stays deterministic for the merged text.
    for (nick, _text) in &pairs {
        assert!(nick.chars().count() <= 10, "nick overflows the nick column: {nick:?}");
    }
    for line in wrap_text("hello", 8) {
        assert!(!line.is_empty());
    }

    let thread = &app.agents.threads[0];
    assert_eq!(thread.response_group_ids(), vec![1]);
    assert_eq!(thread.response_group_counts(1), (1, 0));
    assert_eq!(thread.selected_response_group, Some(1));
    assert!(thread.collapsed_response_groups.contains(&1), "completed turns collapse");

    press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert!(!app.agents.threads[0].collapsed_response_groups.contains(&1));
    press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert!(app.agents.threads[0].collapsed_response_groups.contains(&1));
}

#[test]
fn blank_agent_chunks_do_not_create_transcript_gaps() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update("s1", wire::agent_message_chunk("empty", "")))
        .emit(wire::session_update("s1", wire::agent_message_chunk("visible", "reply")));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "question");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "non-empty reply", |app| app.agents.threads[0].message_pairs().len() == 2);

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let rendered: Vec<String> = (0..20)
        .map(|y| (0..80).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();

    assert!(rendered.iter().any(|row| row.contains("reply")), "reply missing: {rendered:#?}");
    assert!(
        rendered.iter().all(|row| {
            !row.contains("assistant")
                || !row.split_once("assistant").is_some_and(|(_, text)| text.trim().is_empty())
        }),
        "blank assistant chunk rendered as a transcript row: {rendered:#?}"
    );
}

#[test]
fn agents_thoughts_command_toggles_visibility_without_dropping_transcript() {
    let (mut app, _temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);
    assert!(app.agents.show_thoughts);

    type_text(&mut app, "/thoughts off");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(!app.agents.show_thoughts);
    assert_eq!(app.backend.status_message.as_deref(), Some("agent thoughts hidden"));

    app.agents.threads[0].transcript.push(TranscriptItem::Message {
        nick: String::from("think"),
        text: String::from("private summary"),
        kind: crate::app::MessageRenderKind::Thought,
        message_id: Some(String::from("th-1")),
        response_group: Some(1),
        at: std::time::SystemTime::UNIX_EPOCH,
    });
    assert_eq!(
        app.agents.threads[0].message_pairs().len(),
        1,
        "toggle must not drop stored thought transcript"
    );

    type_text(&mut app, "/thoughts toggle");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.agents.show_thoughts);
    assert_eq!(app.backend.status_message.as_deref(), Some("agent thoughts visible"));

    type_text(&mut app, "/thoughts maybe");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.backend.status_message.as_deref(), Some("usage: /thoughts on|off|toggle"));
    assert!(app.agents.show_thoughts, "invalid input must not change visibility");
}

#[test]
fn plan_updates_stay_hidden_until_toggled_and_replace_wholesale_without_scrollback_append() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "plan",
                "entries": [
                    { "content": "first", "priority": "high", "status": "pending" },
                    { "content": "second", "priority": "low", "status": "in_progress" }
                ]
            }),
        ))
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "plan",
                "entries": [
                    { "content": "replacement", "priority": "medium", "status": "completed" }
                ]
            }),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "plan replacement lands", |app| {
        app.agents.threads[0].plan_entries() == vec![(String::from("[medium] replacement"), 'x')]
    });
    assert!(
        !app.agents.threads[0].plan_modal_open,
        "plan updates remain hidden until the user toggles visibility"
    );

    press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    assert!(app.agents.threads[0].plan_modal_open, "Ctrl-G opens plan modal");
    let transcript_debug = format!("{:?}", app.agents.threads[0].transcript);
    assert!(
        !transcript_debug.contains("replacement"),
        "plan content must not append into chat scrollback: {transcript_debug}"
    );

    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(!app.agents.threads[0].plan_modal_open, "Esc closes plan modal");
    assert_eq!(
        app.agents.threads[0].plan_entries(),
        vec![(String::from("[medium] replacement"), 'x')],
        "closing modal keeps latest plan snapshot"
    );
}
