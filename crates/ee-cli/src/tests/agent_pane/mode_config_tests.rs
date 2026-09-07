//! `impl App` agents-pane tests: mode_config_tests domain.
use super::*;

#[test]
fn slash_commands_are_discoverable_and_tab_inserts_prompt_text() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "available_commands_update",
                "availableCommands": [
                    { "name": "plan", "description": "Create plan", "input": { "hint": "goal" } },
                    { "name": "edit", "description": "Edit code" }
                ]
            }),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "commands arrive", |app| app.agents.threads[0].command_names().len() == 2);
    assert_eq!(
        app.agents.threads[0].command_names(),
        vec![String::from("plan"), String::from("edit")]
    );
    assert!(
        app.agents.threads[0]
            .system_notices()
            .iter()
            .all(|notice| !notice.starts_with("commands:")),
        "advertised commands must not add chat-area hints"
    );

    // Slash-prefixed drafts autocomplete by prefix; once completed, Tab and
    // Shift-Tab cycle through advertised commands.
    type_text(&mut app, "/e");
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].draft, "/edit");
    press(&mut app, KeyCode::BackTab, KeyModifiers::SHIFT);
    assert_eq!(app.agents.threads[0].draft, "/plan");
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].draft, "/edit");
    type_text(&mut app, " file");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "slash prompt sent", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 2
    });
    let prompt = &fake.agent().requests_by_method("session/prompt")[1];
    assert_eq!(prompt["params"]["prompt"][0]["text"], "/edit file");
}

#[test]
fn mode_slash_command_explains_when_agent_advertises_no_modes() {
    let (mut app, _temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/mode");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        app.agents.mode_selection.as_ref().is_some_and(|picker| picker.options.is_empty()),
        "an unsupported mode command must open an explanatory composer"
    );

    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let rendered: Vec<String> = (0..24)
        .map(|y| (0..120).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();
    assert!(
        rendered.iter().any(|row| row.contains("mode unavailable")),
        "unavailable mode heading missing: {rendered:#?}"
    );
    assert!(
        rendered.iter().any(|row| row.contains("did not advertise selectable ACP modes")),
        "unavailable mode reason missing: {rendered:#?}"
    );

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.agents.mode_selection.is_some(), "Enter cannot dismiss unavailable mode state");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.agents.mode_selection.is_none(), "Esc closes unavailable mode state");
}

#[test]
fn mode_slash_command_opens_expanded_composer_picker() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({
            "sessionId": "s1",
            "modes": {
                "currentModeId": "ask",
                "availableModes": [
                    { "id": "ask", "name": "Ask" },
                    { "id": "plan", "name": "Plan" }
                ]
            }
        }))
        .wait_for("session/set_mode")
        .respond(json!({}));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/mode");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let picker = app.agents.mode_selection.as_ref().expect("mode picker opens");
    assert_eq!(picker.options, vec![String::from("ask"), String::from("plan")]);
    assert_eq!(picker.selected, 0, "current mode starts selected");

    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let rendered: Vec<String> = (0..24)
        .map(|y| (0..120).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();
    assert!(
        rendered.iter().any(|row| row.contains("select mode")),
        "mode picker heading missing: {rendered:#?}"
    );
    assert!(
        rendered.iter().any(|row| row.contains("> ask")),
        "current mode must be selected: {rendered:#?}"
    );
    assert!(
        rendered.iter().any(|row| row.contains("  plan")),
        "other advertised modes must be visible: {rendered:#?}"
    );

    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.agents.mode_selection.as_ref().expect("picker remains open").selected, 1);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "mode change accepted", |app| {
        fake.agent().requests_by_method("session/set_mode").len() == 1
            && app.agents.threads[0]
                .host
                .snapshot()
                .current_mode
                .as_ref()
                .is_some_and(|mode| mode.0.as_ref() == "plan")
    });
    assert!(app.agents.mode_selection.is_none(), "picker closes after confirmation");

    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let rendered: Vec<String> = (0..24)
        .map(|y| (0..120).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();
    let composer_row =
        rendered.iter().position(|row| row.contains("prompt>")).expect("composer row");
    assert!(
        rendered[composer_row - 1].contains("mode:plan"),
        "footer must update after /mode plan: {rendered:#?}"
    );
}

#[test]
fn session_info_updates_display_name() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "session_info_update",
                "title": "Audit run",
                "updatedAt": "2026-08-04T12:00:00Z"
            }),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "thread title updates", |app| {
        app.agents.threads[0].display_name == "1.Audit run"
    });
    assert_eq!(app.agents.threads[0].session_title.as_deref(), Some("Audit run"));
    assert_eq!(app.agents.threads[0].session_updated_at.as_deref(), Some("2026-08-04T12:00:00Z"));
}

#[test]
fn agents_config_commands_list_and_mutate_advertised_options() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({
            "sessionId": "s1",
            "configOptions": [
                {
                    "id": "mode",
                    "name": "Mode",
                    "category": "mode",
                    "type": "select",
                    "currentValue": "ask",
                    "options": [
                        { "value": "ask", "name": "Ask" },
                        { "value": "plan", "name": "Plan" }
                    ]
                },
                {
                    "id": "confirmEdits",
                    "name": "Confirm edits",
                    "type": "boolean",
                    "currentValue": false
                }
            ]
        }))
        .wait_for("session/set_config_option")
        .respond(json!({
            "configOptions": [
                {
                    "id": "mode",
                    "name": "Mode",
                    "category": "mode",
                    "type": "select",
                    "currentValue": "ask",
                    "options": [
                        { "value": "ask", "name": "Ask" },
                        { "value": "plan", "name": "Plan" }
                    ]
                },
                {
                    "id": "confirmEdits",
                    "name": "Confirm edits",
                    "type": "boolean",
                    "currentValue": true
                }
            ]
        }))
        .wait_for("session/set_config_option")
        .respond(json!({
            "configOptions": [
                {
                    "id": "mode",
                    "name": "Mode",
                    "category": "mode",
                    "type": "select",
                    "currentValue": "plan",
                    "options": [
                        { "value": "ask", "name": "Ask" },
                        { "value": "plan", "name": "Plan" }
                    ]
                },
                {
                    "id": "confirmEdits",
                    "name": "Confirm edits",
                    "type": "boolean",
                    "currentValue": true
                }
            ]
        }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/config");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let listed = app.backend.status_message.clone().unwrap_or_default();
    assert!(listed.contains("mode=ask"), "status: {listed}");
    assert!(listed.contains("confirmEdits=off"), "status: {listed}");

    type_text(&mut app, "/config toggle confirmEdits");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "boolean config sent", |_| {
        fake.agent().requests_by_method("session/set_config_option").len() == 1
    });
    let toggle = &fake.agent().requests_by_method("session/set_config_option")[0];
    assert_eq!(toggle["params"]["configId"], "confirmEdits");
    assert_eq!(toggle["params"]["type"], "boolean");
    assert_eq!(toggle["params"]["value"], true);

    type_text(&mut app, "/config set mode plan");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "select config sent", |_| {
        fake.agent().requests_by_method("session/set_config_option").len() == 2
    });
    let set_mode = &fake.agent().requests_by_method("session/set_config_option")[1];
    assert_eq!(set_mode["params"]["configId"], "mode");
    assert_eq!(set_mode["params"]["value"], "plan");
}

#[test]
fn provider_features_require_live_advertisement_or_advertised_config() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({
            "sessionId": "s1",
            "configOptions": [
                {
                    "id": "model",
                    "name": "Model",
                    "type": "select",
                    "currentValue": "small",
                    "options": [
                        { "value": "small", "name": "Small" },
                        { "value": "large", "name": "Large" }
                    ]
                },
                {
                    "id": "fast",
                    "name": "Fast mode",
                    "type": "boolean",
                    "currentValue": false
                }
            ]
        }))
        .wait_for("session/set_mode")
        .respond(json!({}))
        .wait_for("session/set_config_option")
        .respond(json!({}))
        .wait_for("session/set_config_option")
        .respond(json!({}))
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "available_commands_update",
                "availableCommands": [{ "name": "compact", "description": "Provider compaction" }]
            }),
        ))
        .respond(json!({ "stopReason": "end_turn" }))
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/model large");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "advertised model config sent", |_| {
        fake.agent().requests_by_method("session/set_config_option").len() == 1
    });
    let model = &fake.agent().requests_by_method("session/set_config_option")[0];
    assert_eq!(model["params"]["configId"], "model");
    assert_eq!(model["params"]["value"], "large");

    type_text(&mut app, "/fast");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "advertised fast config sent", |_| {
        fake.agent().requests_by_method("session/set_config_option").len() == 2
    });
    let fast = &fake.agent().requests_by_method("session/set_config_option")[1];
    assert_eq!(fast["params"]["configId"], "fast");
    assert_eq!(fast["params"]["value"], true);

    type_text(&mut app, "/effort high");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        app.backend
            .status_message
            .as_deref()
            .is_some_and(|status| status.contains("did not advertise config option effort")),
        "unadvertised config must remain local: {:?}",
        app.backend.status_message
    );
    assert_eq!(fake.agent().requests_by_method("session/set_config_option").len(), 2);

    type_text(&mut app, "/compact focus");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        app.backend
            .status_message
            .as_deref()
            .is_some_and(|status| status.contains("did not advertise it")),
        "unadvertised provider workflow must fail closed: {:?}",
        app.backend.status_message
    );
    assert!(fake.agent().requests_by_method("session/prompt").is_empty());

    type_text(&mut app, "sync commands");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "provider command advertisement", |app| {
        app.agents.threads[0].command_names() == vec![String::from("compact")]
    });

    type_text(&mut app, "/compact focus");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "advertised provider command forwarded", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 2
    });
    let compact = &fake.agent().requests_by_method("session/prompt")[1];
    assert_eq!(compact["params"]["prompt"][0]["text"], "/compact focus");
}
