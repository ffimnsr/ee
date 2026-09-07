//! Host-flow tests: mode.
use super::*;

#[tokio::test]
async fn close_session_cancels_local_work_and_releases_thread_state() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": { "sessionCapabilities": { "close": {} } }
        }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
        .wait_for("session/prompt")
        .wait_for("session/close")
        .respond(json!({}));
    let (fake, mut host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let prompt_thread = thread.clone();
    let prompt = tokio::spawn(async move {
        prompt_thread.send_prompt(vec![ContentBlock::Text(TextContent::new("go"))]).await
    });
    tokio::time::timeout(TEST_TIMEOUT, async {
        while !fake.log_contains("\"method\":\"session/prompt\"") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("prompt observed");

    connection.close_session(SessionId::new("s1")).await.expect("close_session");

    let error = prompt.await.expect("prompt task").unwrap_err();
    assert!(matches!(error, AgentError::Cancelled));
    assert!(!thread.is_turn_running());
    assert_eq!(connection.permission_broker().pending_count(), 0);
    assert_eq!(fake.requests_by_method("session/close").len(), 1);

    let mut saw_thread_closed = false;
    for _ in 0..8 {
        match next_event(&mut host.events).await {
            AgentEvent::ThreadClosed { session_id, reason, .. } => {
                assert_eq!(session_id.0.as_ref(), "s1");
                assert_eq!(reason, ThreadCloseReason::HostClosed);
                saw_thread_closed = true;
                break;
            }
            AgentEvent::TurnStarted { .. }
            | AgentEvent::TurnCancelled { .. }
            | AgentEvent::ThreadCreated { .. }
            | AgentEvent::ConnectionStateChanged { .. } => continue,
            _ => continue,
        }
    }
    assert!(saw_thread_closed, "thread close event must be observed");

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn close_session_requires_advertised_capability() {
    let script = base_script();
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let error = connection.close_session(SessionId::new("s1")).await.unwrap_err();
    assert!(matches!(
        error,
        AgentError::CapabilityUnsupported { ref method } if method == "session/close"
    ));
    assert_eq!(fake.requests_by_method("session/close").len(), 0);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn set_mode_updates_shared_mode_state() {
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
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let modes = thread.advertised_modes().expect("modes advertised");
    assert_eq!(modes.available_modes.len(), 2);

    thread.set_mode(SessionModeId::new("plan")).await.expect("set_mode");
    assert_eq!(fake.requests_by_method("session/set_mode").len(), 1);
    assert_eq!(thread.snapshot().current_mode, Some(SessionModeId::new("plan")));
    assert_eq!(
        thread.advertised_modes().expect("modes advertised").current_mode_id,
        SessionModeId::new("plan")
    );

    // Unknown mode id is rejected locally.
    let error = thread.set_mode(SessionModeId::new("code")).await.unwrap_err();
    assert!(matches!(error, AgentError::InvalidParams(_)));
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn ensure_mode_negotiates_explicit_default_when_agent_omits_mode_state() {
    let script = base_script().wait_for("session/set_mode").respond(json!({}));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    thread.ensure_mode(SessionModeId::new("ask")).await.expect("default mode negotiated");

    let requests = fake.requests_by_method("session/set_mode");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["params"]["sessionId"], "s1");
    assert_eq!(requests[0]["params"]["modeId"], "ask");
    assert_eq!(thread.snapshot().current_mode, Some(SessionModeId::new("ask")));
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn set_mode_without_advertised_modes_fails_closed() {
    let script = base_script();
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let error = thread.set_mode(SessionModeId::new("ask")).await.unwrap_err();
    assert!(matches!(
        error,
        AgentError::CapabilityUnsupported { ref method } if method == "session/set_mode"
    ));
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn set_config_options_are_stored_and_mode_category_overrides_legacy_modes() {
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
                    { "id": "agent", "name": "Agent" },
                    { "id": "plan", "name": "Plan" }
                ]
            },
            "configOptions": [
                {
                    "id": "mode",
                    "name": "Mode",
                    "category": "mode",
                    "type": "select",
                    "currentValue": "agent",
                    "options": [
                        { "value": "ask", "name": "Ask" },
                        { "value": "agent", "name": "Agent" },
                        { "value": "plan", "name": "Plan" }
                    ]
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
                        { "value": "agent", "name": "Agent" },
                        { "value": "plan", "name": "Plan" }
                    ]
                }
            ]
        }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let snapshot = thread.snapshot();
    assert_eq!(snapshot.current_mode, Some(SessionModeId::new("agent")));
    assert_eq!(snapshot.config_options.len(), 1);
    assert_eq!(thread.config_options()[0].id.0.as_ref(), "mode");

    thread.set_mode(SessionModeId::new("plan")).await.expect("config-backed mode change");
    assert_eq!(fake.requests_by_method("session/set_mode").len(), 0);
    let request = &fake.requests_by_method("session/set_config_option")[0];
    assert_eq!(request["params"]["configId"], "mode");
    assert_eq!(request["params"]["value"], "plan");

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn set_config_option_validates_select_values_locally() {
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
                        { "value": "edit", "name": "Edit" }
                    ]
                }
            ]
        }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let error = thread
        .set_config_option(SessionConfigId::new("mode"), SessionConfigOptionValue::value_id("plan"))
        .await
        .unwrap_err();
    assert!(matches!(error, AgentError::InvalidParams(_)));
    assert_eq!(fake.requests_by_method("session/set_config_option").len(), 0);

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn set_config_option_boolean_requires_advertised_client_support() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({
            "sessionId": "s1",
            "configOptions": [
                {
                    "id": "confirmEdits",
                    "name": "Confirm edits",
                    "type": "boolean",
                    "currentValue": false
                }
            ]
        }));
    let handler = RecordingHandler::new(HandlerCapabilities::none());
    let (fake, host) = spawn_host(script, Arc::new(handler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let error = thread
        .set_config_option(
            SessionConfigId::new("confirmEdits"),
            SessionConfigOptionValue::boolean(true),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        AgentError::CapabilityUnsupported { ref method } if method == "session/set_config_option"
    ));
    assert_eq!(fake.requests_by_method("session/set_config_option").len(), 0);

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn set_config_option_boolean_round_trips_when_client_support_is_advertised() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({
            "sessionId": "s1",
            "configOptions": [
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
                    "id": "confirmEdits",
                    "name": "Confirm edits",
                    "type": "boolean",
                    "currentValue": true
                }
            ]
        }));
    let handler = RecordingHandler::new(HandlerCapabilities {
        session_config_boolean: true,
        ..HandlerCapabilities::none()
    });
    let (fake, host) = spawn_host(script, Arc::new(handler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    thread
        .set_config_option(
            SessionConfigId::new("confirmEdits"),
            SessionConfigOptionValue::boolean(true),
        )
        .await
        .expect("boolean config option routed");
    let request = &fake.requests_by_method("session/set_config_option")[0];
    assert_eq!(request["params"]["configId"], "confirmEdits");
    assert_eq!(request["params"]["type"], "boolean");
    assert_eq!(request["params"]["value"], true);

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn config_option_update_replaces_state_and_current_mode_update_keeps_mode_in_sync() {
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
                    { "id": "agent", "name": "Agent" },
                    { "id": "plan", "name": "Plan" }
                ]
            },
            "configOptions": [
                {
                    "id": "mode",
                    "name": "Mode",
                    "category": "mode",
                    "type": "select",
                    "currentValue": "ask",
                    "options": [
                        { "value": "ask", "name": "Ask" },
                        { "value": "agent", "name": "Agent" },
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
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "config_option_update",
                "configOptions": [
                    {
                        "id": "mode",
                        "name": "Mode",
                        "category": "mode",
                        "type": "select",
                        "currentValue": "agent",
                        "options": [
                            { "value": "ask", "name": "Ask" },
                            { "value": "agent", "name": "Agent" },
                            { "value": "plan", "name": "Plan" }
                        ]
                    }
                ]
            }),
        ))
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "current_mode_update",
                "currentModeId": "plan"
            }),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    thread.send_prompt(vec![ContentBlock::Text(TextContent::new("go"))]).await.unwrap();
    let snapshot = thread.snapshot();
    assert_eq!(snapshot.config_options.len(), 1, "config option updates replace whole state");
    assert_eq!(snapshot.current_mode, Some(SessionModeId::new("plan")));
    let config = serde_json::to_value(&snapshot.config_options[0]).unwrap();
    assert_eq!(config["currentValue"], "plan");

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn plan_updates_route_through_host_and_replace_wholesale() {
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
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    thread.send_prompt(vec![ContentBlock::Text(TextContent::new("go"))]).await.unwrap();
    let snapshot = thread.snapshot();
    assert_eq!(snapshot.plan.len(), 1);
    assert_eq!(snapshot.plan[0].content, "replacement");

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn available_commands_update_replaces_state_wholesale() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "available_commands_update",
                "availableCommands": [
                    { "name": "plan", "description": "Create plan" },
                    { "name": "edit", "description": "Edit code" }
                ]
            }),
        ))
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "available_commands_update",
                "availableCommands": [
                    { "name": "agent", "description": "Run agent mode" }
                ]
            }),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    thread.send_prompt(vec![ContentBlock::Text(TextContent::new("go"))]).await.unwrap();
    let snapshot = thread.snapshot();
    assert_eq!(snapshot.available_commands.len(), 1);
    assert_eq!(snapshot.available_commands[0].name, "agent");

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}
