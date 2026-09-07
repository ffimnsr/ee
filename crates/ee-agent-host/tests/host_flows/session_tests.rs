//! Host-flow tests: session.
use super::*;

#[tokio::test]
async fn load_session_requires_advertised_capability() {
    let script = FakeAgentScript::new().wait_for("initialize").respond(json!({
        "protocolVersion": 1,
        "agentCapabilities": { "loadSession": false }
    }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = host.connection.clone();
    connection.wait_ready().await.expect("ready");

    let error = connection
        .load_session(SessionId::new("old"), PathBuf::from("/work"), Vec::new(), Vec::new())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        AgentError::CapabilityUnsupported { ref method } if method == "session/load"
    ));
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn load_session_routes_with_absolute_cwd_mcp_servers_and_optional_additional_directories() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "loadSession": true,
                "sessionCapabilities": { "additionalDirectories": {} }
            }
        }))
        .wait_for("session/load")
        .emit(wire::session_update("old", wire::agent_message_chunk("m1", "restored")))
        .respond(json!({
            "modes": {
                "currentModeId": "ask",
                "availableModes": [{ "id": "ask", "name": "Ask" }]
            },
            "configOptions": [
                {
                    "id": "mode",
                    "name": "Mode",
                    "category": "mode",
                    "type": "select",
                    "currentValue": "ask",
                    "options": [{ "value": "ask", "name": "Ask" }]
                }
            ]
        }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let thread = connection
        .load_session(
            SessionId::new("old"),
            PathBuf::from("/work"),
            vec![PathBuf::from("/extra")],
            vec![ee_agent_protocol::McpServer::Stdio(ee_agent_protocol::McpServerStdio::new(
                "tools",
                "agent-proxy",
            ))],
        )
        .await
        .expect("load_session");

    let request = &fake.requests_by_method("session/load")[0];
    assert_eq!(request["params"]["sessionId"], json!("old"));
    assert_eq!(request["params"]["cwd"], json!("/work"));
    assert_eq!(request["params"]["additionalDirectories"], json!(["/extra"]));
    assert_eq!(request["params"]["mcpServers"][0]["name"], json!("tools"));
    assert_eq!(request["params"]["mcpServers"][0]["command"], json!("agent-proxy"));
    assert!(thread.advertised_modes().is_some());
    assert_eq!(thread.config_options().len(), 1);
    assert_eq!(
        thread.snapshot().messages.len(),
        1,
        "streamed updates before load response must apply"
    );

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn load_session_omits_additional_directories_when_unadvertised() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": { "loadSession": true }
        }))
        .wait_for("session/load")
        .respond(json!({}));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    connection
        .load_session(
            SessionId::new("old"),
            PathBuf::from("/work"),
            vec![PathBuf::from("/extra")],
            Vec::new(),
        )
        .await
        .expect("load_session");

    let request = &fake.requests_by_method("session/load")[0];
    assert_eq!(request["params"].get("additionalDirectories"), None);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn load_session_rejects_relative_or_missing_roots() {
    let script = FakeAgentScript::new().wait_for("initialize").respond(json!({
        "protocolVersion": 1,
        "agentCapabilities": {
            "loadSession": true,
            "sessionCapabilities": { "additionalDirectories": {} }
        }
    }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let error = connection
        .load_session(SessionId::new("old"), PathBuf::from("relative"), Vec::new(), Vec::new())
        .await
        .unwrap_err();
    assert!(matches!(error, AgentError::InvalidParams(_)));

    let error = connection
        .load_session(
            SessionId::new("old"),
            PathBuf::from("/work"),
            vec![PathBuf::from("relative")],
            Vec::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, AgentError::InvalidParams(_)));
    assert_eq!(fake.requests_by_method("session/load").len(), 0);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn session_lifecycle_capabilities_reflect_advertised_flags() {
    let script = FakeAgentScript::new().wait_for("initialize").respond(json!({
        "protocolVersion": 1,
        "agentCapabilities": {
            "sessionCapabilities": {
                "list": {},
                "delete": {},
                "additionalDirectories": {},
                "resume": {},
                "close": {}
            }
        }
    }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    assert!(connection.supports_session_list());
    assert!(connection.supports_session_delete());
    assert!(connection.supports_session_resume());
    assert!(connection.supports_session_close());
    assert!(connection.supports_additional_directories());

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn list_sessions_routes_with_absolute_cwd_and_opaque_cursor() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": { "sessionCapabilities": { "list": {} } }
        }))
        .wait_for("session/list")
        .respond(json!({
            "sessions": [],
            "nextCursor": "opaque-next"
        }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let response = connection
        .list_sessions(Some(PathBuf::from("/work")), Some(String::from("opaque-prev")))
        .await
        .expect("list_sessions");
    assert_eq!(response.next_cursor.as_deref(), Some("opaque-next"));

    let request = &fake.requests_by_method("session/list")[0];
    assert_eq!(request["params"]["cwd"], json!("/work"));
    assert_eq!(request["params"]["cursor"], json!("opaque-prev"));
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn list_sessions_requires_advertised_capability_and_absolute_cwd() {
    let script = FakeAgentScript::new().wait_for("initialize").respond(json!({
        "protocolVersion": 1,
        "agentCapabilities": {}
    }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let error = connection.list_sessions(None, None).await.unwrap_err();
    assert!(matches!(
        error,
        AgentError::CapabilityUnsupported { ref method } if method == "session/list"
    ));
    assert_eq!(fake.requests_by_method("session/list").len(), 0);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;

    let script = FakeAgentScript::new().wait_for("initialize").respond(json!({
        "protocolVersion": 1,
        "agentCapabilities": { "sessionCapabilities": { "list": {} } }
    }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let error = connection.list_sessions(Some(PathBuf::from("relative")), None).await.unwrap_err();
    assert!(matches!(error, AgentError::InvalidParams(_)));
    assert_eq!(fake.requests_by_method("session/list").len(), 0);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn delete_session_routes_and_requires_capability() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": { "sessionCapabilities": { "delete": {} } }
        }))
        .wait_for("session/delete")
        .respond(json!({}));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    connection.delete_session(SessionId::new("dead")).await.expect("delete_session");
    assert_eq!(fake.requests_by_method("session/delete").len(), 1);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;

    let script = FakeAgentScript::new().wait_for("initialize").respond(json!({
        "protocolVersion": 1,
        "agentCapabilities": {}
    }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let error = connection.delete_session(SessionId::new("dead")).await.unwrap_err();
    assert!(matches!(
        error,
        AgentError::CapabilityUnsupported { ref method } if method == "session/delete"
    ));
    assert_eq!(fake.requests_by_method("session/delete").len(), 0);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn resume_session_routes_without_replayed_history() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "sessionCapabilities": {
                    "resume": {},
                    "additionalDirectories": {}
                }
            }
        }))
        .wait_for("session/resume")
        .respond(json!({
            "modes": {
                "currentModeId": "ask",
                "availableModes": [{ "id": "ask", "name": "Ask" }]
            },
            "configOptions": [
                {
                    "id": "mode",
                    "name": "Mode",
                    "category": "mode",
                    "type": "select",
                    "currentValue": "ask",
                    "options": [{ "value": "ask", "name": "Ask" }]
                }
            ]
        }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let thread = connection
        .resume_session(
            SessionId::new("s-resume"),
            PathBuf::from("/work"),
            vec![PathBuf::from("/extra")],
            Vec::new(),
        )
        .await
        .expect("resume_session");
    assert_eq!(thread.session_id().0.as_ref(), "s-resume");
    assert!(thread.snapshot().messages.is_empty(), "resume must not replay history");
    assert!(thread.advertised_modes().is_some());
    assert_eq!(thread.config_options().len(), 1);

    let request = &fake.requests_by_method("session/resume")[0];
    assert_eq!(request["params"]["cwd"], json!("/work"));
    assert_eq!(request["params"]["additionalDirectories"], json!(["/extra"]));
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn resume_session_requires_capabilities_and_absolute_paths() {
    let script = FakeAgentScript::new().wait_for("initialize").respond(json!({
        "protocolVersion": 1,
        "agentCapabilities": { "sessionCapabilities": { "resume": {} } }
    }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let error = connection
        .resume_session(
            SessionId::new("s1"),
            PathBuf::from("/work"),
            vec![PathBuf::from("/extra")],
            Vec::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        AgentError::CapabilityUnsupported { ref method } if method == "session/resume"
    ));
    assert_eq!(fake.requests_by_method("session/resume").len(), 0);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;

    let script = FakeAgentScript::new().wait_for("initialize").respond(json!({
        "protocolVersion": 1,
        "agentCapabilities": { "sessionCapabilities": { "resume": {}, "additionalDirectories": {} } }
    }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let error = connection
        .resume_session(SessionId::new("s1"), PathBuf::from("relative"), Vec::new(), Vec::new())
        .await
        .unwrap_err();
    assert!(matches!(error, AgentError::InvalidParams(_)));

    let error = connection
        .resume_session(
            SessionId::new("s1"),
            PathBuf::from("/work"),
            vec![PathBuf::from("relative")],
            Vec::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, AgentError::InvalidParams(_)));
    assert_eq!(fake.requests_by_method("session/resume").len(), 0);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}
