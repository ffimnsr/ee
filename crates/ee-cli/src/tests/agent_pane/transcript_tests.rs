//! `impl App` agents-pane tests: transcript_tests domain.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use super::*;

fn render_agents_rows(app: &App) -> Vec<String> {
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, app)).unwrap();
    let rows = terminal.backend().buffer();
    (0..20)
        .map(|y| (0..80).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect()
}

fn mouse_click_cell(app: &mut App, x: u16, y: u16) {
    app.handle_mouse_event_in_area(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        },
        Rect { x: 0, y: 0, width: 80, height: 20 },
    );
}

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
        wait_until(&mut app, &format!("turn completed {prompt}"), |app| {
            app.agents.threads[0].state == ThreadUiState::Ready
                && app.agents.threads[0].message_pairs().len() == index + 1
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
fn mouse_click_on_response_header_toggles_collapse() {
    let (mut app, _temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);
    let thread = &mut app.agents.threads[0];
    for (nick, text, kind) in [
        (String::from("fake"), String::from("answer"), MessageRenderKind::Assistant),
        (String::from("think"), String::from("hmm"), MessageRenderKind::Thought),
    ] {
        thread.transcript.push(TranscriptItem::Message {
            nick,
            text,
            kind,
            message_id: None,
            response_group: Some(1),
            at: SystemTime::UNIX_EPOCH,
        });
    }
    assert!(!app.agents.threads[0].collapsed_response_groups.contains(&1));

    // Click the expanded header row: collapses (same as Ctrl-R).
    let rows = render_agents_rows(&app);
    let header_y =
        rows.iter().position(|row| row.contains("[−]") && row.contains("response:")).unwrap();
    let marker_col = rows[header_y].find('[').unwrap() as u16;
    mouse_click_cell(&mut app, marker_col, header_y as u16);
    assert_eq!(app.agents.threads[0].selected_response_group, Some(1));
    assert!(app.agents.threads[0].collapsed_response_groups.contains(&1));

    // Click the collapsed header row: expands again.
    let rows = render_agents_rows(&app);
    let header_y =
        rows.iter().position(|row| row.contains("[+]") && row.contains("response:")).unwrap();
    let marker_col = rows[header_y].find('[').unwrap() as u16;
    mouse_click_cell(&mut app, marker_col, header_y as u16);
    assert!(!app.agents.threads[0].collapsed_response_groups.contains(&1));

    // Clicks on content rows (not headers) must not toggle.
    let rows = render_agents_rows(&app);
    let content_y = rows.iter().position(|row| row.contains("hmm")).unwrap();
    let content_col = rows[content_y].find('[').unwrap() as u16;
    mouse_click_cell(&mut app, content_col, content_y as u16);
    assert!(!app.agents.threads[0].collapsed_response_groups.contains(&1));
}

#[test]
fn mouse_click_on_tool_call_row_toggles_group_tool_detail() {
    let (mut app, _temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);
    app.agents.threads[0].transcript.push(TranscriptItem::ToolCall {
        id: String::from("call_1"),
        title: String::from("Run tests"),
        status: String::from("completed"),
        detail: String::from("detail: all green"),
        response_group: 1,
        at: SystemTime::UNIX_EPOCH,
    });
    assert!(!app.agents.threads[0].expanded_tool_details.contains(&1));

    // Click the tool-call row: expands the group's tool detail (same as Ctrl-E).
    let rows = render_agents_rows(&app);
    let tool_y = rows.iter().position(|row| row.contains("Run tests")).unwrap();
    let tool_col = rows[tool_y].find('[').unwrap() as u16;
    mouse_click_cell(&mut app, tool_col, tool_y as u16);
    assert_eq!(app.agents.threads[0].selected_response_group, Some(1));
    assert!(app.agents.threads[0].expanded_tool_details.contains(&1));
    let rows = render_agents_rows(&app);
    assert!(
        rows.iter().any(|row| row.contains("all green")),
        "expanded tool detail must render: {rows:#?}"
    );

    // Click the tool-call row again: collapses the detail.
    let tool_y = rows.iter().position(|row| row.contains("Run tests")).unwrap();
    let tool_col = rows[tool_y].find('[').unwrap() as u16;
    mouse_click_cell(&mut app, tool_col, tool_y as u16);
    assert!(!app.agents.threads[0].expanded_tool_details.contains(&1));
    let rows = render_agents_rows(&app);
    assert!(
        !rows.iter().any(|row| row.contains("all green")),
        "collapsed tool detail must hide: {rows:#?}"
    );

    // Header clicks still toggle collapse, independent of detail state.
    let header_y =
        rows.iter().position(|row| row.contains("[−]") && row.contains("response:")).unwrap();
    let header_col = rows[header_y].find('[').unwrap() as u16;
    mouse_click_cell(&mut app, header_col, header_y as u16);
    assert!(app.agents.threads[0].collapsed_response_groups.contains(&1));
    assert!(!app.agents.threads[0].expanded_tool_details.contains(&1));

    // Raw transcript mode always shows detail; tool clicks must not mutate state.
    app.agents.threads[0].collapsed_response_groups.clear();
    app.agents.threads[0].transcript_raw = true;
    let rows = render_agents_rows(&app);
    let tool_y = rows.iter().position(|row| row.contains("Run tests")).unwrap();
    let tool_col = rows[tool_y].find('[').unwrap() as u16;
    mouse_click_cell(&mut app, tool_col, tool_y as u16);
    assert!(!app.agents.threads[0].expanded_tool_details.contains(&1));
}

#[test]
fn mouse_click_on_header_respects_scroll_offset() {
    let (mut app, _temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);
    let thread = &mut app.agents.threads[0];
    for group in 1..=6u64 {
        thread.transcript.push(TranscriptItem::Message {
            nick: String::from("fake"),
            text: format!(
                "long answer number {group} with plenty of padding text to force wrapping onto a second line"
            ),
            kind: MessageRenderKind::Assistant,
            message_id: None,
            response_group: Some(group),
            at: SystemTime::UNIX_EPOCH,
        });
        thread.transcript.push(TranscriptItem::ToolCall {
            id: format!("call_{group}"),
            title: format!("tool {group}"),
            status: String::from("completed"),
            detail: String::new(),
            response_group: group,
            at: SystemTime::UNIX_EPOCH,
        });
    }
    // Header + 2-line answer + tool per group must overflow the 18-row
    // transcript viewport so the click math exercises the scroll offset.
    let rendered_line_count =
        crate::ui::agents_pane::agent_transcript_lines(&app, &app.agents.threads[0], 79).len();
    assert!(rendered_line_count >= 21, "fixture must overflow viewport: {rendered_line_count}");
    let thread = &mut app.agents.threads[0];
    thread.scroll = 4;
    thread.stick_to_bottom = false;
    assert!(thread.collapsed_response_groups.is_empty());

    // The first visible header is group 2 (group 1 is scrolled off); clicking
    // it must toggle group 2, not the group at the unscrolled position.
    let rows = render_agents_rows(&app);
    let header_y =
        rows.iter().position(|row| row.contains("[−]") && row.contains("response:")).unwrap();
    let header_col = rows[header_y].find('[').unwrap() as u16;
    mouse_click_cell(&mut app, header_col, header_y as u16);
    assert_eq!(app.agents.threads[0].selected_response_group, Some(2));
    assert_eq!(
        app.agents.threads[0].collapsed_response_groups.iter().copied().collect::<Vec<_>>(),
        vec![2],
        "click must hit the group at the scrolled row"
    );
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

#[test]
fn tool_call_completed_update_lands_even_when_sent_after_turn_end() {
    // Provider streams the final response and only then marks the tool
    // completed (Case B: late `tool_call_update`).
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call_1",
                "title": "Run tests",
                "kind": "execute",
                "status": "in_progress"
            }),
        ))
        .respond(json!({ "stopReason": "end_turn" }))
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_1",
                "status": "completed"
            }),
        ));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "completed update applied after turn end", |app| {
        app.agents.threads[0].transcript.iter().any(|item| {
            matches!(
                item,
                crate::app::TranscriptItem::ToolCall { id, status, .. }
                    if id == "call_1" && status == "completed"
            )
        })
    });
}

#[test]
fn tool_call_update_for_unknown_id_is_dropped_and_original_stays_in_progress() {
    // Provider completes under a different `toolCallId` with no title
    // (Case C): the ordering tracker rejects the constructible-less update
    // fail-closed, so the announced call stays `in_progress` forever.
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call_1",
                "title": "Run tests",
                "kind": "execute",
                "status": "in_progress"
            }),
        ))
        .emit(wire::session_update(
            "s1",
            json!({ "sessionUpdate": "tool_call_update", "toolCallId": "call_OTHER", "status": "completed" }),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "turn done with tool still in progress", |app| {
        app.agents.threads[0].state == ThreadUiState::Ready
            && app.agents.threads[0].transcript.iter().any(|item| {
                matches!(
                    item,
                    crate::app::TranscriptItem::ToolCall { id, status, .. }
                        if id == "call_1" && status == "in_progress"
                )
            })
    });
    assert!(
        !app.agents.threads[0].transcript.iter().any(|item| {
            matches!(
                item,
                crate::app::TranscriptItem::ToolCall { id, .. } if id == "call_OTHER"
            )
        }),
        "rejected update must not create a ghost tool row"
    );
}

#[test]
fn tool_call_update_without_status_field_keeps_in_progress_label() {
    // Provider streams output content but never sends a status transition
    // (Case D): the field merge leaves the announced status untouched.
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call_1",
                "title": "Run tests",
                "kind": "execute",
                "status": "in_progress"
            }),
        ))
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_1",
                "content": [
                    { "type": "content", "content": { "type": "text", "text": "result landed" } }
                ]
            }),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "content applied while label stuck in progress", |app| {
        app.agents.threads[0].transcript.iter().any(|item| {
            matches!(
                item,
                crate::app::TranscriptItem::ToolCall { id, status, detail, .. }
                    if id == "call_1"
                        && status == "in_progress"
                        && detail.contains("content: result landed")
            )
        })
    });
}
