//! `impl App` agents-pane tests: permissions_tests domain.
use super::*;

#[test]
fn concurrent_permissions_are_session_scoped_and_fifo() {
    let script = two_session_script()
        .wait_for("session/prompt")
        .emit(permission_request(100, "s1", "call-a1", "A first"))
        .emit(permission_request(102, "s1", "call-a2", "A second"))
        .wait_for("session/prompt")
        .emit(permission_request(101, "s2", "call-b", "B"));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);
    open_second_thread(&mut app);

    app.focus_thread(0);
    type_text(&mut app, "session A");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "two session A permissions", |app| {
        app.agents.permissions.get("s1").is_some_and(|queue| queue.len() == 2)
    });
    assert_eq!(app.agents.threads[0].state, ThreadUiState::AwaitingPermission);

    app.focus_thread(1);
    assert!(app.agents.permission().is_none(), "session A modal must not leak into B");
    type_text(&mut app, "session B");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "session B permission", |app| {
        app.agents.permissions.get("s2").is_some_and(|queue| queue.len() == 1)
    });
    assert_eq!(app.agents.threads[0].state, ThreadUiState::AwaitingPermission);
    assert_eq!(app.agents.threads[1].state, ThreadUiState::AwaitingPermission);
    assert_eq!(fake.agent().requests_by_method("session/prompt").len(), 2);

    press(&mut app, KeyCode::Right, KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "session B permission response", |_| {
        fake.agent().response_with_id(101).is_some()
    });

    app.focus_thread(0);
    assert_eq!(app.agents.permission().expect("first A permission").tool_title, "A first");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "first session A permission response", |_| {
        fake.agent().response_with_id(100).is_some()
    });
    assert_eq!(app.agents.permission().expect("second A permission").tool_title, "A second");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "second session A permission response", |_| {
        fake.agent().response_with_id(102).is_some()
    });
    assert!(app.agents.permissions.is_empty());
    assert_eq!(
        fake.agent().response_with_id(101).expect("session B response")["result"]["outcome"]["optionId"],
        "deny"
    );
}
#[test]
fn permission_prompt_selection_resolves_host_request() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::request_permission(
            "s1",
            "call_1",
            "Run tests",
            json!([
                { "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" },
                { "optionId": "deny", "name": "Deny", "kind": "reject_once" }
            ]),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/approval bypass");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    type_text(&mut app, "run");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "permission prompt appears", |app| app.agents.permission().is_some());
    let permission = app.agents.permission().expect("permission present");
    assert_eq!(permission.options.len(), 2);
    assert_eq!(permission.selected, 0);
    assert_eq!(
        app.agents.threads[0].system_notices().iter().find(|n| n.starts_with("approval")),
        None,
        "no approval notice before confirmation"
    );

    press(&mut app, KeyCode::Right, KeyModifiers::NONE);
    assert_eq!(app.agents.permission().expect("prompt").selected, 1);

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.agents.permission().is_none(), "prompt clears after confirm");

    wait_until(&mut app, "host answered permission", |_| {
        fake.agent().response_with_id(100).is_some()
    });
    let response = fake.agent().response_with_id(100).expect("permission response");
    assert_eq!(response["result"]["outcome"]["outcome"], "selected");
    assert_eq!(response["result"]["outcome"]["optionId"], "deny");
    wait_until(&mut app, "approval notice lands", |app| {
        app.agents.threads[0]
            .system_notices()
            .iter()
            .any(|notice| notice.contains("approval: Deny (sent)"))
    });
}
#[test]
fn concurrent_elicitations_are_session_scoped_and_keep_identity() {
    let script = two_session_script()
        .wait_for("session/prompt")
        .emit(json!({
            "jsonrpc": "2.0",
            "id": 200,
            "method": "elicitation/create",
            "params": {
                "mode": "form",
                "sessionId": "s1",
                "requestedSchema": {
                    "type": "object",
                    "properties": { "name": { "type": "string", "title": "Name" } }
                },
                "message": "form A"
            }
        }))
        .wait_for("session/prompt")
        .emit(json!({
            "jsonrpc": "2.0",
            "id": 201,
            "method": "elicitation/create",
            "params": {
                "mode": "url",
                "sessionId": "s2",
                "elicitationId": "el-b",
                "url": "https://example.com/authorize",
                "message": "url B"
            }
        }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);
    open_second_thread(&mut app);

    app.focus_thread(0);
    type_text(&mut app, "session A");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "session A elicitation", |app| {
        app.agents.elicitation().is_some_and(|prompt| prompt.message == "form A")
    });
    assert_eq!(app.agents.threads[0].state, ThreadUiState::AwaitingElicitation);

    app.focus_thread(1);
    assert!(app.agents.elicitation().is_none(), "session A modal must not leak into B");
    type_text(&mut app, "session B");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "session B elicitation", |app| {
        app.agents.elicitation().is_some_and(|prompt| {
            prompt.message == "url B" && prompt.completion_id.as_deref() == Some("el-b")
        })
    });
    assert_eq!(app.agents.threads[0].state, ThreadUiState::AwaitingElicitation);
    assert_eq!(app.agents.threads[1].state, ThreadUiState::AwaitingElicitation);
    assert_eq!(fake.agent().requests_by_method("session/prompt").len(), 2);

    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    wait_until(&mut app, "session B elicitation response", |_| {
        fake.agent().response_with_id(201).is_some()
    });
    app.focus_thread(0);
    assert_eq!(app.agents.elicitation().expect("session A form").message, "form A");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    wait_until(&mut app, "session A elicitation response", |_| {
        fake.agent().response_with_id(200).is_some()
    });

    assert!(app.agents.elicitations.is_empty());
    assert_eq!(
        fake.agent().response_with_id(201).expect("B response")["result"]["action"],
        "decline"
    );
    assert_eq!(
        fake.agent().response_with_id(200).expect("A response")["result"]["action"],
        "cancel"
    );
}
#[test]
fn elicitation_widgets_resolve_form_requests() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(form_elicitation(
            200,
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "title": "Name" },
                    "debug": { "type": "boolean", "title": "Debug" },
                    "level": { "type": "string", "enum": ["low", "high"] }
                },
                "required": ["name"]
            }),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "elicitation prompt appears", |app| app.agents.elicitation().is_some());
    let elicitation = app.agents.elicitation().expect("prompt present");
    assert_eq!(elicitation.agent_label, "1.fake");
    assert_eq!(elicitation.message, "fill the form");
    assert_eq!(elicitation.fields.len(), 3);
    assert!(elicitation.unsupported_reason.is_none());
    // Schema properties arrive in sorted order (BTreeMap), so locate fields
    // by name instead of assuming insertion order.
    let field_index = |name: &str| {
        elicitation
            .fields
            .iter()
            .position(|field| field.name == name)
            .unwrap_or_else(|| panic!("missing form field {name:?}"))
    };
    let name_index = field_index("name");
    let debug_index = field_index("debug");
    let level_index = field_index("level");
    assert_eq!(elicitation.fields[name_index].label(), "Name");
    assert_eq!(elicitation.fields[debug_index].label(), "Debug");
    assert_eq!(elicitation.fields[level_index].label(), "level");

    // Drive the form: navigate to each field by Tab (fields cycle), fill it,
    // and accept.
    let mut steps = 0;
    while app.agents.elicitation().expect("prompt").selected_field != debug_index {
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        steps += 1;
        assert!(steps < 10, "Tab never reached the boolean field");
    }
    press(&mut app, KeyCode::Right, KeyModifiers::NONE); // toggle debug=true
    while app.agents.elicitation().expect("prompt").selected_field != level_index {
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        steps += 1;
        assert!(steps < 10, "Tab never reached the enum field");
    }
    press(&mut app, KeyCode::Right, KeyModifiers::NONE); // low → high
    while app.agents.elicitation().expect("prompt").selected_field != name_index {
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        steps += 1;
        assert!(steps < 10, "Tab never reached the name field");
    }
    type_text(&mut app, "ed");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // accept

    wait_until(&mut app, "elicitation answered on the wire", |_| {
        fake.agent().response_with_id(200).is_some()
    });
    let response = fake.agent().response_with_id(200).expect("elicitation response");
    assert_eq!(response["result"]["action"], "accept");
    assert_eq!(response["result"]["content"]["name"], "ed");
    assert_eq!(response["result"]["content"]["debug"], true);
    assert_eq!(response["result"]["content"]["level"], "high");
    assert!(app.agents.elicitation().is_none());
}
#[test]
fn elicitation_rejects_unsupported_schema_visibly_and_declines() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(form_elicitation(
            201,
            json!({
                "type": "object",
                "properties": {
                    "tags": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["a", "b"] }
                    }
                },
                "required": ["tags"]
            }),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "unsupported elicitation appears", |app| {
        app.agents.elicitation().is_some()
    });
    let reason = app
        .agents
        .elicitation()
        .and_then(|prompt| prompt.unsupported_reason.clone())
        .expect("unsupported reason must be visible");
    assert!(reason.contains("unsupported"), "reason: {reason}");

    // Enter (accept) fails locally and keeps prompt open until user declines/cancels.
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "unsupported elicitation stays local", |app| {
        app.agents.elicitation().is_some()
            && app
                .backend
                .status_message
                .as_deref()
                .is_some_and(|status| status.contains("elicitation blocked locally"))
    });
    assert!(fake.agent().response_with_id(201).is_none());

    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    wait_until(&mut app, "unsupported elicitation declined", |_app| {
        fake.agent().response_with_id(201).is_some()
    });
    let response = fake.agent().response_with_id(201).expect("decline response");
    assert_eq!(response["result"]["action"], "decline");
    assert!(app.agents.elicitation().is_none());
}
#[test]
fn elicitation_rejects_deep_schema_visibly_and_declines() {
    let mut nested = json!("leaf");
    for _ in 0..20 {
        nested = json!({ "child": nested });
    }
    let script = base_script()
        .wait_for("session/prompt")
        .emit(form_elicitation(
            203,
            json!({
                "type": "object",
                "properties": {
                    "deep": {
                        "type": "_future_widget",
                        "payload": nested
                    }
                }
            }),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "deep elicitation appears", |app| app.agents.elicitation().is_some());
    let reason = app
        .agents
        .elicitation()
        .and_then(|prompt| prompt.unsupported_reason.clone())
        .expect("unsupported reason must be visible");
    assert!(reason.contains("schema depth exceeds"), "reason: {reason}");

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "deep elicitation stays local", |app| {
        app.agents.elicitation().is_some()
            && app
                .backend
                .status_message
                .as_deref()
                .is_some_and(|status| status.contains("elicitation blocked locally"))
    });
    assert!(fake.agent().response_with_id(203).is_none());

    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    wait_until(&mut app, "deep elicitation cancelled", |_app| {
        fake.agent().response_with_id(203).is_some()
    });
    assert_eq!(fake.agent().response_with_id(203).expect("response")["result"]["action"], "cancel");
    assert!(app.agents.elicitation().is_none());
}
#[test]
fn url_elicitation_shows_full_url_host_and_choice() {
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
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "url elicitation appears", |app| {
        app.agents.elicitation().is_some_and(|prompt| prompt.url.is_some())
    });
    let prompt = app.agents.elicitation().expect("prompt");
    let url = prompt.url.clone().expect("url");
    assert_eq!(prompt.url_host.as_deref(), Some("example.com"));
    assert!(url.contains("https://example.com/authorize"), "full url shown: {url}");
    assert!(app.agents.threads[0].transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::Elicitation {
            agent,
            message,
            url: Some(url),
            url_host: Some(host),
            ..
        } if agent == "1.fake"
            && message == "authorize the agent"
            && host == "example.com"
            && url.contains("https://example.com/authorize?client=ee")
    )));

    // Left/Right cycles accept/decline/cancel; Ctrl-D declines without opening.
    press(&mut app, KeyCode::Left, KeyModifiers::NONE);
    assert_eq!(app.agents.elicitation().expect("prompt").selected_choice, 2);
    press(&mut app, KeyCode::Right, KeyModifiers::NONE);
    assert_eq!(app.agents.elicitation().expect("prompt").selected_choice, 0);
    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    wait_until(&mut app, "url elicitation declined", |_| {
        fake.agent().response_with_id(202).is_some()
    });
    assert_eq!(
        fake.agent().response_with_id(202).expect("response")["result"]["action"],
        "decline"
    );
}
#[test]
fn stale_url_elicitation_completion_is_ignored_without_clearing_prompt() {
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
        .emit(elicitation_complete("el-stale"))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "url elicitation remains open", |app| {
        app.agents.elicitation().is_some_and(|prompt| prompt.url.is_some())
    });
    std::thread::sleep(Duration::from_millis(100));
    app.pump_agents();
    assert!(
        fake.agent().response_with_id(202).is_none(),
        "stale completion must stay diagnostics-only and not answer request"
    );

    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    wait_until(&mut app, "url elicitation declined after stale completion", |_| {
        fake.agent().response_with_id(202).is_some()
    });
    assert_eq!(fake.agent().response_with_id(202).expect("response")["result"]["action"], "cancel");
}
#[test]
fn secret_like_elicitation_requests_are_blocked_locally() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(form_elicitation_with_message(
            204,
            json!({
                "type": "object",
                "properties": {
                    "api_key": { "type": "string", "title": "API key" }
                },
                "required": ["api_key"]
            }),
            "enter your password",
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "secretive elicitation appears", |app| app.agents.elicitation().is_some());
    let reason = app
        .agents
        .elicitation()
        .and_then(|prompt| prompt.unsupported_reason.clone())
        .expect("blocked reason visible");
    assert!(reason.contains("secret-like elicitation requests are blocked"), "reason: {reason}");

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "blocked elicitation remains local", |app| {
        app.agents.elicitation().is_some()
            && app
                .backend
                .status_message
                .as_deref()
                .is_some_and(|status| status.contains("elicitation blocked locally"))
    });
    assert!(fake.agent().response_with_id(204).is_none());

    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    wait_until(&mut app, "blocked elicitation declined", |_| {
        fake.agent().response_with_id(204).is_some()
    });
    assert_eq!(
        fake.agent().response_with_id(204).expect("response")["result"]["action"],
        "decline"
    );
}
#[test]
fn elicitation_validation_failure_stays_local_until_user_resolves_it() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(form_elicitation(
            205,
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "title": "Name" }
                },
                "required": ["name"]
            }),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "required-field elicitation appears", |app| {
        app.agents.elicitation().is_some()
    });
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "validation failure stays local", |app| {
        app.agents.elicitation().is_some()
            && app
                .backend
                .status_message
                .as_deref()
                .is_some_and(|status| status.contains("required field missing"))
    });
    assert!(fake.agent().response_with_id(205).is_none());

    type_text(&mut app, "ed");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "validation resolved", |_| fake.agent().response_with_id(205).is_some());
    assert_eq!(fake.agent().response_with_id(205).expect("response")["result"]["action"], "accept");
    assert_eq!(
        fake.agent().response_with_id(205).expect("response")["result"]["content"]["name"],
        "ed"
    );
}
#[test]
fn tool_call_details_stay_collapsed_until_toggled_during_active_turn() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call_1",
                "title": "Run tests",
                "kind": "execute",
                "status": "pending",
                "rawInput": { "token": "super-secret" }
            }),
        ))
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_1",
                "status": "completed",
                "content": [
                    { "type": "content", "content": { "type": "text", "text": "cargo test --quiet" } },
                    { "type": "diff", "path": "/tmp/src/lib.rs", "newText": "fn main() {}" },
                    { "type": "terminal", "terminalId": "term-1" }
                ],
                "locations": [
                    { "path": "/tmp/src/lib.rs", "line": 7 },
                    { "path": "/tmp/tests/lib.rs" }
                ],
                "rawOutput": { "password": "nope" }
            }),
        ))
        .wait_for("session/cancel");
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "tool call rendered", |app| {
        app.agents.threads[0].transcript.iter().any(|item| {
            matches!(
                item,
                TranscriptItem::ToolCall { status, detail, .. }
                    if status == "completed"
                        && detail.contains("kind: execute")
                        && detail.contains("content: cargo test --quiet")
                        && detail.contains("diff: new file /tmp/src/lib.rs")
                        && detail.contains("terminal: term-1")
                        && detail.contains("locations: /tmp/src/lib.rs:7, /tmp/tests/lib.rs")
                        && detail.contains("diagnostics: raw input/output captured")
                        && !detail.contains("super-secret")
                        && !detail.contains("nope")
            )
        })
    });
    let thread = &app.agents.threads[0];
    assert_eq!(thread.response_group_ids(), vec![1]);
    assert_eq!(thread.response_group_counts(1), (0, 1));
    assert_eq!(thread.selected_response_group, Some(1));
    assert_eq!(thread.state, ThreadUiState::Running);
    assert!(!thread.expanded_tool_details.contains(&1));

    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let collapsed: Vec<String> = (0..24)
        .map(|y| (0..120).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();
    assert!(collapsed.iter().any(|row| row.contains("Run tests [completed]")));
    assert!(
        !collapsed.iter().any(|row| row.contains("content: cargo test --quiet")),
        "tool detail must stay hidden while the turn is active: {collapsed:#?}"
    );

    press(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL);
    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let expanded: Vec<String> = (0..24)
        .map(|y| (0..120).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();
    assert!(
        expanded.iter().any(|row| row.contains("content: cargo test --quiet")),
        "expanded tool detail must render: {expanded:#?}"
    );

    press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let response_collapsed: Vec<String> = (0..24)
        .map(|y| (0..120).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();
    assert!(
        !response_collapsed.iter().any(|row| row.contains("Run tests [completed]")),
        "collapsed response must hide nested tool rows: {response_collapsed:#?}"
    );

    let export_base = tempfile::tempdir().unwrap();
    app.agents.test_export_base = Some(export_base.path().to_path_buf());
    type_text(&mut app, "/export");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    let export_dir = export_base.path().join("agent-exports");
    let export_path = fs::read_dir(&export_dir)
        .expect("export directory")
        .next()
        .expect("export file")
        .unwrap()
        .path();
    let exported = fs::read_to_string(&export_path).expect("exported transcript");
    assert!(exported.contains("# Agent session transcript"));
    assert!(exported.contains("Tool: Run tests"));
    assert!(exported.contains("#### Input"));
    assert!(exported.contains("#### Output"));
    assert!(exported.contains("\"token\": \"***\""));
    assert!(exported.contains("\"password\": \"***\""));
    assert!(!exported.contains("super-secret"));
    assert!(!exported.contains("nope"));
    assert!(exported.find("User (you)") < exported.find("Tool: Run tests"));
    assert!(app.agents.threads[0].draft.is_empty());
    assert!(
        app.backend.status_message.as_deref().is_some_and(|status| status.contains("exported"))
    );
}
