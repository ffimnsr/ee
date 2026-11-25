//! v2 session continuity: the ee MCP proxy must survive session setup on
//! both `session/new` and `session/resume`.
//!
//! ACP v2 removed the client filesystem and terminal methods, so an armed ee
//! proxy that fails to advertise an MCP server would silently remove every
//! client tool. These tests pin the ACP-native route, the stdio fallback, the
//! fail-closed error, resume parity, and the explicitly disarmed case.

use super::v2_flows::{spawn_host_v2, spawn_host_v2_with_proxy, v2_base_script, v2_no_acp_script};
use super::*;

/// ACP-native MCP servers (the `ee` proxy on an MCP-over-ACP agent) keep
/// their transport identity on the v2 wire instead of being dropped.
#[tokio::test]
async fn v2_session_new_keeps_acp_native_ee_server() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 2,
            "info": { "name": "fake-agent-v2", "version": "1.0.0" },
            "capabilities": { "session": {} },
            "authMethods": [],
        }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1", "configOptions": [] }));
    let (fake, host) = spawn_host_v2(script, Arc::new(DenyAllHandler)).await;
    let connection = host.connection.clone();
    connection.wait_ready().await.expect("handshake succeeds");

    let server_id = connection.ee_proxy_server_id().expect("proxy armed");
    let server = ee_agent_protocol::McpServer::Acp(ee_agent_protocol::McpServerAcp::new(
        "ee",
        server_id.clone(),
    ));
    connection
        .new_session(vec![PathBuf::from("/work")], vec![server], None)
        .await
        .expect("v2 session/new succeeds");

    let new_session = fake.requests_by_method("session/new").pop().expect("session/new sent");
    assert_eq!(new_session["params"]["mcpServers"][0]["type"], "acp");
    assert_eq!(new_session["params"]["mcpServers"][0]["name"], "ee");
    assert_eq!(new_session["params"]["mcpServers"][0]["serverId"], server_id.0.as_ref());
    assert_eq!(
        new_session["params"]["mcpServers"].as_array().expect("mcpServers array").len(),
        1,
        "the ee entry must not be duplicated"
    );

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

/// A v2 session with an armed ee proxy must carry an MCP route; silently
/// creating it without client tools is a hard error.
#[tokio::test]
async fn v2_session_new_requires_client_tool_route_when_proxy_armed() {
    let script = v2_no_acp_script();
    let (fake, host) = spawn_host_v2(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let error = connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), None)
        .await
        .expect_err("v2 without an MCP route must fail closed");
    assert!(
        matches!(error, AgentError::InvalidParams(ref message) if message.contains("[mcp.proxy]")),
        "actionable error expected, got {error:?}"
    );
    assert!(
        fake.requests_by_method("session/new").is_empty(),
        "no session/new may be sent without a client-tool route"
    );

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

/// Without `session.mcp.acp`, the stdio `ee --mcp-proxy` fallback supplies
/// the v2 client-tool route.
#[tokio::test]
async fn v2_session_new_uses_stdio_fallback_when_agent_lacks_acp() {
    let script = v2_no_acp_script();
    let (fake, host) = spawn_host_v2(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let fallback = ee_agent_protocol::McpServerStdio::new("ee", "/usr/local/bin/ee")
        .args(vec![String::from("--mcp-proxy")]);

    let thread = connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(fallback))
        .await
        .expect("v2 stdio fallback session succeeds");
    assert_eq!(thread.proxy_mode(), ee_agent_host::EeProxyMode::StdioFallback);

    let request = fake.requests_by_method("session/new").pop().expect("session/new sent");
    assert_eq!(request["params"]["mcpServers"][0]["type"], "stdio");
    assert_eq!(request["params"]["mcpServers"][0]["name"], "ee");
    assert_eq!(request["params"]["mcpServers"][0]["command"], "/usr/local/bin/ee");
    assert_eq!(request["params"]["mcpServers"][0]["args"], json!(["--mcp-proxy"]));

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

/// An agent advertising `session.mcp.acp` gets the ACP-native `ee` route on
/// v2 without needing the stdio fallback.
#[tokio::test]
async fn v2_session_new_uses_acp_native_route_when_advertised() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 2,
            "info": { "name": "fake-agent-v2", "version": "1.0.0" },
            "capabilities": { "session": { "mcp": { "acp": {} } } },
            "authMethods": [],
        }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1", "configOptions": [] }));
    let (fake, host) = spawn_host_v2(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let thread = connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), None)
        .await
        .expect("v2 ACP-native session succeeds");
    assert_eq!(thread.proxy_mode(), ee_agent_host::EeProxyMode::AcpNative);

    let request = fake.requests_by_method("session/new").pop().expect("session/new sent");
    assert_eq!(request["params"]["mcpServers"][0]["type"], "acp");
    assert_eq!(request["params"]["mcpServers"][0]["name"], "ee");

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

/// Explicitly disarming the proxy removes the requirement: no ee entry is
/// advertised and the session is created without an error.
#[tokio::test]
async fn v2_session_new_allows_disarmed_proxy() {
    let script = v2_base_script();
    let (fake, host) = spawn_host_v2_with_proxy(script, Arc::new(DenyAllHandler), false).await;
    let connection = ready_connection(&fake, &host).await;

    let thread = connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), None)
        .await
        .expect("disarmed proxy does not require an MCP route");
    assert_eq!(thread.proxy_mode(), ee_agent_host::EeProxyMode::Disabled);

    let request = fake.requests_by_method("session/new").pop().expect("session/new sent");
    assert!(
        request["params"].get("mcpServers").is_none(),
        "disarmed proxy must not advertise mcpServers: {request}"
    );

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

/// Resume re-advertises the ee proxy on v2 and reports the resolved mode.
#[tokio::test]
async fn v2_resume_advertises_ee_proxy_and_reports_mode() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 2,
            "info": { "name": "fake-agent-v2", "version": "1.0.0" },
            "capabilities": { "session": {} },
            "authMethods": [],
        }))
        .wait_for("session/resume")
        .respond(json!({ "configOptions": [] }));
    let (fake, host) = spawn_host_v2(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let fallback = ee_agent_protocol::McpServerStdio::new("ee", "/usr/local/bin/ee")
        .args(vec![String::from("--mcp-proxy")]);

    let thread = connection
        .resume_session(
            SessionId::new("s1"),
            PathBuf::from("/work"),
            Vec::new(),
            Vec::new(),
            Some(fallback),
        )
        .await
        .unwrap_or_else(|error| panic!("v2 resume failed: {error:?}\nlog: {:?}", fake.log()));
    assert_eq!(thread.proxy_mode(), ee_agent_host::EeProxyMode::StdioFallback);

    let request = fake.requests_by_method("session/resume").pop().expect("session/resume sent");
    assert_eq!(request["params"]["mcpServers"][0]["type"], "stdio");
    assert_eq!(request["params"]["mcpServers"][0]["name"], "ee");

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

/// Resume with an armed proxy and no MCP route fails closed exactly like
/// `session/new`.
#[tokio::test]
async fn v2_resume_requires_client_tool_route_when_proxy_armed() {
    let script = FakeAgentScript::new().wait_for("initialize").respond(json!({
        "protocolVersion": 2,
        "info": { "name": "fake-agent-v2", "version": "1.0.0" },
        "capabilities": { "session": {} },
        "authMethods": [],
    }));
    let (fake, host) = spawn_host_v2(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let error = connection
        .resume_session(SessionId::new("s1"), PathBuf::from("/work"), Vec::new(), Vec::new(), None)
        .await
        .expect_err("v2 resume without an MCP route must fail closed");
    assert!(
        matches!(error, AgentError::InvalidParams(ref message) if message.contains("[mcp.proxy]")),
        "actionable error expected, got {error:?}"
    );
    assert!(fake.requests_by_method("session/resume").is_empty());

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}
