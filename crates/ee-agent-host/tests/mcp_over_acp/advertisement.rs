use super::*;

// ── session/new advertisement ────────────────────────────────────────────────

#[tokio::test]
async fn mcp_over_acp_session_new_advertises_acp_native_ee_server_when_supported() {
    let (fake, host) = spawn_host(acp_base_script(), Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread = connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    assert_eq!(thread.proxy_mode(), EeProxyMode::AcpNative);

    let session_new = fake.requests_by_method("session/new");
    let servers = session_new[0].get("params").and_then(|p| p.get("mcpServers"));
    let servers = servers.and_then(Value::as_array).expect("mcpServers present");
    let acp = servers
        .iter()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some("ee"))
        .expect("ee proxy entry forwarded");
    assert_eq!(acp.get("type").and_then(Value::as_str), Some("acp"));
    let expected_server_id = connection.ee_proxy_server_id().expect("armed server id");
    assert_eq!(acp.get("serverId"), Some(&json!(expected_server_id.0.as_ref())));

    // No stdio `ee --mcp-proxy` entry: the two modes are mutually exclusive.
    assert!(!servers.iter().any(|entry| {
        entry.get("type").and_then(Value::as_str) == Some("stdio")
            && entry.get("name").and_then(Value::as_str) == Some("ee")
    }));
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_session_new_uses_stdio_fallback_without_acp_capability() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread = connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    assert_eq!(thread.proxy_mode(), EeProxyMode::StdioFallback);

    let session_new = fake.requests_by_method("session/new");
    let servers = session_new[0].get("params").and_then(|p| p.get("mcpServers"));
    let servers = servers.and_then(Value::as_array).expect("mcpServers present");
    // `McpServer::Stdio` is an untagged schema variant: no `type` field.
    let stdio = servers
        .iter()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some("ee"))
        .expect("stdio fallback entry forwarded");
    assert!(stdio.get("type").is_none(), "stdio entries carry no type tag: {stdio}");
    assert_eq!(stdio.get("command").and_then(Value::as_str), Some("/usr/bin/ee"));
    let args = stdio.get("args").and_then(Value::as_array).expect("args");
    assert!(args.contains(&json!("--mcp-proxy")));
    assert!(!servers.iter().any(|entry| entry.get("type").and_then(Value::as_str) == Some("acp")));
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_session_new_uses_native_proxy_without_stdio_fallback_config() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": { "mcpCapabilities": { "acp": true } }
        }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread = connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), None)
        .await
        .expect("session starts");

    assert_eq!(thread.proxy_mode(), EeProxyMode::AcpNative);
    let session_new = fake.requests_by_method("session/new");
    let servers = session_new[0]
        .get("params")
        .and_then(|p| p.get("mcpServers"))
        .and_then(Value::as_array)
        .expect("native ee MCP server advertised");
    assert!(servers.iter().any(|entry| {
        entry.get("name").and_then(Value::as_str) == Some("ee")
            && entry.get("type").and_then(Value::as_str) == Some("acp")
    }));
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_direct_mcp_config_forwarding_works_independently() {
    // The user server lands at mcpServers[0]; the ee ACP entry at [1].
    let (fake, host) = spawn_host(acp_base_script_with_index(1), Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let user_server = McpServer::Stdio(
        McpServerStdio::new("filesystem", "/usr/bin/mcp-server").args(vec!["serve".into()]),
    );
    let thread = connection
        .new_session(vec![PathBuf::from("/work")], vec![user_server], Some(stdio_fallback()))
        .await
        .expect("session starts");
    assert_eq!(thread.proxy_mode(), EeProxyMode::AcpNative);

    let session_new = fake.requests_by_method("session/new");
    let servers = session_new[0].get("params").and_then(|p| p.get("mcpServers"));
    let servers = servers.and_then(Value::as_array).expect("mcpServers present");
    // User-configured server untouched...
    let filesystem = servers
        .iter()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some("filesystem"))
        .expect("user server forwarded");
    // `McpServer::Stdio` is an untagged schema variant: no `type` field.
    assert!(filesystem.get("type").is_none(), "stdio entries carry no type tag");
    assert_eq!(filesystem.get("command").and_then(Value::as_str), Some("/usr/bin/mcp-server"));
    // ...and the ACP-native ee entry alongside it.
    let acp = servers
        .iter()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some("ee"))
        .expect("ee acp entry forwarded");
    assert_eq!(acp.get("type").and_then(Value::as_str), Some("acp"));
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}
