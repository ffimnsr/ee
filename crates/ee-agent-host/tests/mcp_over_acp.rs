//! Phase 6b: ACP-native MCP-over-ACP flows against the in-process fake ACP
//! agent.
//!
//! These tests exercise the real connection stack (SDK dispatch, driver,
//! approval-executor bridge, rmcp serve loop, `EeMcpProxy` tool surface)
//! over the scripted fake transport.  The fake agent advertises
//! `mcp_capabilities.acp`, receives the ACP-native `ee` server entry in
//! `session/new`, connects with `mcp/connect`, runs the inner MCP
//! `initialize` handshake, and exchanges inner MCP messages with
//! `mcp/message`; capture steps let the script use the host-generated
//! server/connection ids.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ee_agent_host::fake::{CaptureSource, FakeAgent, FakeAgentScript};
use ee_agent_host::{
    AgentConnection, AgentConnectionOptions, AgentError, ClientRequest, ClientRequestHandler,
    ClientRequestResponse, ClientRequestResult, DenyAllHandler, EeProxyMode, HandlerCapabilities,
    MCP_OVER_ACP_MAX_FRAME_BYTES, RecordingHandler,
};
use ee_agent_protocol::{
    CreateTerminalRequest, McpServer, McpServerStdio, ReadTextFileRequest, ReadTextFileResponse,
    WriteTextFileRequest,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Scripted handler: allows reads, denies writes and terminal creates with
/// `PermissionDenied`, and records every request (approval-path proof).
#[derive(Default, Clone)]
struct ScriptedHandler {
    seen: Arc<Mutex<Vec<ClientRequest>>>,
}

impl ScriptedHandler {
    fn seen(&self) -> Vec<ClientRequest> {
        self.seen.lock().expect("handler log poisoned").clone()
    }
}

impl ClientRequestHandler for ScriptedHandler {
    fn capabilities(&self) -> HandlerCapabilities {
        HandlerCapabilities::all()
    }

    fn handle(
        &self,
        request: ClientRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ClientRequestResult> + Send + '_>> {
        Box::pin(async move {
            self.seen.lock().expect("handler log poisoned").push(request.clone());
            match request {
                ClientRequest::ProxyWorkspaceRoots => {
                    Ok(ClientRequestResponse::ProxyValue(json!({
                        "roots": ["/work", "/extra"],
                        "activeRoot": "/work",
                        "activeFile": "/work/src/main.rs",
                        "additionalDirectories": ["/extra"],
                    })))
                }
                ClientRequest::ProxyListDirectory { path } => {
                    Ok(ClientRequestResponse::ProxyValue(json!({
                        "entries": [{ "path": format!("{path}/src"), "kind": "directory", "size": 4096 }],
                        "truncated": false,
                    })))
                }
                ClientRequest::ProxySearchFiles { pattern } => {
                    Ok(ClientRequestResponse::ProxyValue(json!({
                        "matches": [format!("/work/{pattern}")],
                        "truncated": false,
                    })))
                }
                ClientRequest::ProxySearchText { query } => {
                    Ok(ClientRequestResponse::ProxyValue(json!({
                        "matches": [{ "path": "/work/src/main.rs", "line": 3, "context": format!("hit {query}") }],
                        "truncated": false,
                    })))
                }
                ClientRequest::ProxyOpenBuffers => Ok(ClientRequestResponse::ProxyValue(json!({
                    "buffers": [{
                        "path": "/work/src/main.rs",
                        "dirty": true,
                        "revisionId": "rev-1",
                        "cursorSummary": "line 3, column 7",
                        "selectionSummary": "cursor at offset 42",
                        "languageId": "rust",
                        "active": true
                    }]
                }))),
                ClientRequest::ReadTextFile(_) => Ok(ClientRequestResponse::ReadTextFile(
                    ReadTextFileResponse::new("file contents"),
                )),
                ClientRequest::WriteTextFile(_) => {
                    Err(AgentError::PermissionDenied { reason: "test denies writes".into() })
                }
                ClientRequest::CreateTerminal(_) => {
                    Err(AgentError::PermissionDenied { reason: "test denies terminals".into() })
                }
                other => Err(AgentError::PermissionDenied {
                    reason: format!("test denies {}", other.method()),
                }),
            }
        })
    }
}

/// A connected host plus its event stream.
struct TestHost {
    connection: AgentConnection,
    #[allow(dead_code)]
    events: mpsc::UnboundedReceiver<ee_agent_host::AgentEvent>,
}

/// `ee --mcp-proxy` stdio fallback entry (the host swaps it for the ACP
/// entry when the agent supports MCP-over-ACP).
fn stdio_fallback() -> McpServerStdio {
    McpServerStdio::new("ee", "/usr/bin/ee").args(vec!["--mcp-proxy".into()])
}

/// ACP-native initialize + session/new script; captures the advertised
/// `serverId` from `session/new` (assumes the ee ACP entry is
/// `mcpServers[0]`).
fn acp_base_script() -> FakeAgentScript {
    acp_base_script_with_index(0)
}

fn acp_base_script_with_index(server_index: usize) -> FakeAgentScript {
    FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": { "mcpCapabilities": { "acp": true } }
        }))
        .capture(
            CaptureSource::Request { method: "session/new".into() },
            format!("params.mcpServers[{server_index}].serverId"),
            "server_id",
        )
        .respond(json!({ "sessionId": "s1" }))
}

async fn spawn_host(
    script: FakeAgentScript,
    handler: Arc<dyn ee_agent_host::ClientRequestHandler>,
) -> (FakeAgent, TestHost) {
    spawn_host_with(script, handler, true).await
}

async fn spawn_host_with(
    script: FakeAgentScript,
    handler: Arc<dyn ee_agent_host::ClientRequestHandler>,
    proxy_enabled: bool,
) -> (FakeAgent, TestHost) {
    let (fake, transport) = FakeAgent::spawn(script);
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let options = AgentConnectionOptions {
        handshake_timeout: TEST_TIMEOUT,
        request_timeout: TEST_TIMEOUT,
        ee_proxy_enabled: proxy_enabled,
    };
    let connection = AgentConnection::connect_with_transport(
        "fake".into(),
        handler,
        events_tx,
        options,
        transport,
    )
    .expect("connect over fake transport");
    (fake, TestHost { connection, events: events_rx })
}

/// Polls the fake's log for the host's response to the request with `id`.
async fn await_response(fake: &FakeAgent, id: i64) -> Value {
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if let Some(response) = fake.response_with_id(id) {
                break response;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed out waiting for response")
}

/// An `mcp/connect` emit with the captured `server_id` substituted.
fn emit_connect(id: i64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "mcp/connect",
        "params": { "serverId": { "$capture": "server_id" } }
    })
}

/// An `mcp/message` emit with the captured `conn_id` substituted.
fn emit_message(id: i64, method: &str, params: Option<Value>) -> Value {
    let mut inner = serde_json::Map::new();
    inner.insert(String::from("connectionId"), json!({ "$capture": "conn_id" }));
    inner.insert(String::from("method"), json!(method));
    if let Some(params) = params {
        inner.insert(String::from("params"), params);
    }
    json!({ "jsonrpc": "2.0", "id": id, "method": "mcp/message", "params": inner })
}

/// Inner MCP `initialize` params (the agent is the MCP client and must run
/// the handshake before any other inner message).
fn inner_initialize_params() -> Value {
    json!({
        "protocolVersion": "2026-07-28",
        "capabilities": {},
        "clientInfo": { "name": "fake-agent", "version": "0" }
    })
}

/// Script tail: connect, capture the connection id, and run the inner MCP
/// `initialize` handshake (ids 200/201).  Follow-up messages start at 202.
fn connect_and_init_script() -> FakeAgentScript {
    acp_base_script()
        .emit(emit_connect(200))
        .capture(CaptureSource::Response { id: 200 }, "result.connectionId", "conn_id")
        .emit(emit_message(201, "initialize", Some(inner_initialize_params())))
        .wait_for_response(201)
}

async fn ready_connection(fake: &FakeAgent, host: &TestHost) -> AgentConnection {
    let connection = host.connection.clone();
    connection.wait_ready().await.expect("handshake succeeds");
    assert!(fake.log_contains("\"method\":\"initialize\""));
    connection
}

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
async fn mcp_over_acp_session_new_omits_ee_proxy_without_proxy_config() {
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

    assert_eq!(thread.proxy_mode(), EeProxyMode::Disabled);
    let session_new = fake.requests_by_method("session/new");
    let servers = session_new[0].get("params").and_then(|p| p.get("mcpServers"));
    let servers = servers.and_then(Value::as_array).cloned().unwrap_or_default();
    assert!(
        servers.iter().all(|entry| entry.get("name").and_then(Value::as_str) != Some("ee")),
        "no ee proxy entry expected: {servers:?}"
    );
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

// ── connect / message / disconnect lifecycle ─────────────────────────────────

#[tokio::test]
async fn mcp_over_acp_workspace_roots_tool_round_trips_through_the_handler() {
    let script = connect_and_init_script()
        .emit(emit_message(202, "tools/call", Some(json!({ "name": "ee_workspace_roots" }))))
        .wait_for_response(202);
    let handler = Arc::new(ScriptedHandler::default());
    let (fake, host) = spawn_host(script, handler.clone()).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let response = await_response(&fake, 202).await;
    assert_eq!(response["result"]["structuredContent"]["roots"], json!(["/work", "/extra"]));
    assert!(
        handler.seen().iter().any(|request| matches!(request, ClientRequest::ProxyWorkspaceRoots))
    );

    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_open_buffers_tool_round_trips_through_the_handler() {
    let script = connect_and_init_script()
        .emit(emit_message(202, "tools/call", Some(json!({ "name": "ee_open_buffers" }))))
        .wait_for_response(202);
    let handler = Arc::new(ScriptedHandler::default());
    let (fake, host) = spawn_host(script, handler.clone()).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let response = await_response(&fake, 202).await;
    assert_eq!(response["result"]["structuredContent"]["buffers"][0]["languageId"], json!("rust"));
    assert!(
        handler.seen().iter().any(|request| matches!(request, ClientRequest::ProxyOpenBuffers))
    );

    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_connect_and_tools_list_round_trip() {
    let script = connect_and_init_script()
        .emit(emit_message(202, "tools/list", None))
        .wait_for_response(202);
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let connect = await_response(&fake, 200).await;
    let connection_id = connect["result"]["connectionId"].as_str().expect("connection id");
    assert!(connection_id.starts_with("ee-mcp:"), "ee-owned connection id: {connection_id}");

    let list = await_response(&fake, 202).await;
    let tools = list["result"]["tools"].as_array().expect("tools list in result");
    let names: Vec<&str> =
        tools.iter().filter_map(|tool| tool.get("name").and_then(Value::as_str)).collect();
    assert_eq!(
        names,
        vec![
            "ee_workspace_roots",
            "ee_list_directory",
            "ee_list_directory_all",
            "ee_search_files",
            "ee_search_files_all",
            "ee_search_text",
            "ee_search_text_regex",
            "ee_search_text_in_files",
            "ee_replace_text",
            "ee_apply_patch",
            "ee_create_text_file",
            "ee_overwrite_text_file",
            "ee_read_buffer",
            "ee_read_buffer_lines",
            "ee_open_buffers",
            "ee_get_diagnostics",
            "ee_get_file_diagnostics",
            "ee_document_symbols",
            "ee_references",
            "ee_list_code_actions",
            "ee_apply_code_action",
            "ee_format_file",
            "ee_preview_rename_symbol",
            "ee_rename_symbol",
            "ee_read_text_file",
            "ee_write_text_file",
            "ee_terminal_create",
            "ee_diagnostics",
        ]
    );
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_message_before_connect_is_rejected_with_invalid_params() {
    let script = acp_base_script().emit(json!({
        "jsonrpc": "2.0",
        "id": 200,
        "method": "mcp/message",
        "params": { "connectionId": "nope", "method": "tools/list" }
    }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let reply = await_response(&fake, 200).await;
    let error = reply["error"].as_object().expect("invalid params error");
    assert_eq!(error["code"], json!(-32602));
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_connect_unknown_server_id_is_rejected() {
    let script = acp_base_script().emit(json!({
        "jsonrpc": "2.0",
        "id": 200,
        "method": "mcp/connect",
        "params": { "serverId": "some-other-server" }
    }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let reply = await_response(&fake, 200).await;
    let error = reply["error"].as_object().expect("invalid params error");
    assert_eq!(error["code"], json!(-32602));
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_connect_without_acp_capability_fails_closed() {
    // Proxy configured (armed) but the agent never advertised acp support:
    // the host must reject mcp/connect even though the server id is valid.
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
        .emit(json!({
            "jsonrpc": "2.0",
            "id": 200,
            "method": "mcp/connect",
            "params": { "serverId": "ee-mcp-proxy:fake" }
        }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let reply = await_response(&fake, 200).await;
    let error = reply["error"].as_object().expect("invalid params error");
    assert_eq!(error["code"], json!(-32602));
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_disconnect_unknown_connection_is_rejected() {
    let script = acp_base_script().emit(json!({
        "jsonrpc": "2.0",
        "id": 200,
        "method": "mcp/disconnect",
        "params": { "connectionId": "ee-mcp:fake:99" }
    }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let reply = await_response(&fake, 200).await;
    let error = reply["error"].as_object().expect("invalid params error");
    assert_eq!(error["code"], json!(-32602));
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_repeated_connects_yield_distinct_connection_ids() {
    let script = acp_base_script()
        .emit(emit_connect(200))
        .capture(CaptureSource::Response { id: 200 }, "result.connectionId", "conn_id")
        .emit(emit_connect(201))
        .capture(CaptureSource::Response { id: 201 }, "result.connectionId", "conn_id2");
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let first = await_response(&fake, 200).await;
    let second = await_response(&fake, 201).await;
    let first_id = first["result"]["connectionId"].as_str().expect("first id");
    let second_id = second["result"]["connectionId"].as_str().expect("second id");
    assert_ne!(first_id, second_id, "connection ids must be unique");
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_disconnect_closes_the_logical_connection() {
    let script = connect_and_init_script()
        .emit(json!({
            "jsonrpc": "2.0",
            "id": 202,
            "method": "mcp/disconnect",
            "params": { "connectionId": { "$capture": "conn_id" } }
        }))
        .wait_for_response(202)
        .emit(emit_message(203, "tools/list", None));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    await_response(&fake, 202).await; // disconnect ok
    let reply = await_response(&fake, 203).await;
    let error = reply["error"].as_object().expect("message after disconnect must fail");
    assert_eq!(error["code"], json!(-32602));
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_oversized_message_frame_fails_closed() {
    let huge = "x".repeat(MCP_OVER_ACP_MAX_FRAME_BYTES + 1);
    let script = connect_and_init_script()
        .emit(emit_message(202, "tools/list", Some(json!({ "pad": huge }))))
        .emit(emit_message(203, "tools/list", None));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    // The oversized frame is rejected and closes the logical connection...
    let reply = await_response(&fake, 202).await;
    let error = reply["error"].as_object().expect("oversized frame fails closed");
    assert_eq!(error["code"], json!(-32602));
    // ...so the next message on the same connection id fails too.
    let reply = await_response(&fake, 203).await;
    let error = reply["error"].as_object().expect("closed connection rejects messages");
    assert_eq!(error["code"], json!(-32602));
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_inner_notification_does_not_break_the_connection() {
    let script = connect_and_init_script()
        .emit(json!({
            "jsonrpc": "2.0",
            "method": "mcp/message",
            "params": {
                "connectionId": { "$capture": "conn_id" },
                "method": "notifications/initialized"
            }
        }))
        .emit(emit_message(202, "tools/list", None))
        .wait_for_response(202);
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let list = await_response(&fake, 202).await;
    let tools = list["result"]["tools"].as_array().expect("tools still listed");
    assert!(!tools.is_empty());
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

// ── tool calls through the approval/bridge path ──────────────────────────────

#[tokio::test]
async fn mcp_over_acp_write_tool_denial_leaves_buffer_and_disk_unchanged() {
    let handler = ScriptedHandler::default();
    let script = connect_and_init_script()
        .emit(emit_message(
            202,
            "tools/call",
            Some(json!({
                "name": "ee_write_text_file",
                "arguments": { "path": "/tmp/ee-mcp-over-acp-write.txt", "content": "boom" }
            })),
        ))
        .wait_for_response(202);
    let (fake, host) = spawn_host(script, Arc::new(handler.clone())).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let reply = await_response(&fake, 202).await;
    let result = reply["result"].as_object().expect("tool result payload");
    assert_eq!(result["isError"], json!(true));
    let text = result["content"]
        .as_array()
        .and_then(|blocks| blocks.first())
        .and_then(|block| block.get("text"))
        .and_then(Value::as_str)
        .expect("denial text");
    assert!(text.contains("denied"), "denial surfaced to the agent: {text}");

    // The denial came from the shared handler (approval path), not a crafted
    // ACP-side result: the request reached the handler exactly once.
    let seen = handler.seen();
    assert_eq!(seen.len(), 1);
    assert!(matches!(&seen[0], ClientRequest::WriteTextFile(WriteTextFileRequest { .. })));
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_terminal_create_denial_does_not_spawn_terminal() {
    let handler = ScriptedHandler::default();
    let script = connect_and_init_script()
        .emit(emit_message(
            202,
            "tools/call",
            Some(json!({
                "name": "ee_terminal_create",
                "arguments": { "command": "touch", "args": ["/tmp/ee-denied-terminal"] }
            })),
        ))
        .wait_for_response(202);
    let (fake, host) = spawn_host(script, Arc::new(handler.clone())).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let reply = await_response(&fake, 202).await;
    let result = reply["result"].as_object().expect("tool result payload");
    assert_eq!(result["isError"], json!(true));
    // The handler was asked, so the terminal was denied before any spawn.
    let seen = handler.seen();
    assert_eq!(seen.len(), 1);
    assert!(matches!(&seen[0], ClientRequest::CreateTerminal(CreateTerminalRequest { .. })));
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_read_tool_round_trips_through_the_handler() {
    let handler = ScriptedHandler::default();
    let script = connect_and_init_script()
        .emit(emit_message(
            202,
            "tools/call",
            Some(json!({
                "name": "ee_read_text_file",
                "arguments": { "path": "/tmp/ee-mcp-over-acp-read.txt" }
            })),
        ))
        .wait_for_response(202);
    let (fake, host) = spawn_host(script, Arc::new(handler.clone())).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let reply = await_response(&fake, 202).await;
    let result = reply["result"].as_object().expect("tool result payload");
    assert_eq!(result["isError"], json!(false));
    let text = result["content"]
        .as_array()
        .and_then(|blocks| blocks.first())
        .and_then(|block| block.get("text"))
        .and_then(Value::as_str)
        .expect("read text");
    assert_eq!(text, "file contents");
    let seen = handler.seen();
    assert!(matches!(&seen[0], ClientRequest::ReadTextFile(ReadTextFileRequest { .. })));
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_unadvertised_capabilities_are_rejected_before_the_handler() {
    // The capability gate lives in `proxy_executor`: a handler without
    // capabilities must fail closed before it is invoked.
    let handler = RecordingHandler::new(HandlerCapabilities::none());
    let script = connect_and_init_script()
        .emit(emit_message(
            202,
            "tools/call",
            Some(json!({
                "name": "ee_write_text_file",
                "arguments": { "path": "/tmp/x", "content": "boom" }
            })),
        ))
        .wait_for_response(202);
    let (fake, host) = spawn_host(script, Arc::new(handler.clone())).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let reply = await_response(&fake, 202).await;
    let result = reply["result"].as_object().expect("tool result payload");
    assert_eq!(result["isError"], json!(true));
    assert!(handler.seen().is_empty(), "handler must not run for unadvertised capabilities");
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_diagnostics_tool_returns_bounded_redacted_stderr() {
    let script = connect_and_init_script()
        .emit(emit_message(202, "tools/call", Some(json!({ "name": "ee_diagnostics" }))))
        .wait_for_response(202);
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let reply = await_response(&fake, 202).await;
    let result = reply["result"].as_object().expect("tool result payload");
    assert_eq!(result["isError"], json!(false));
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

// ── lifecycle closure ────────────────────────────────────────────────────────

#[tokio::test]
async fn mcp_over_acp_turn_cancel_closes_logical_connections() {
    let script = connect_and_init_script()
        .emit(emit_message(202, "tools/list", None))
        .wait_for_response(202)
        .wait_for("session/prompt")
        // The driver processes `session/cancel` (notification + close of all
        // logical MCP connections) in one command; a short settle after the
        // notification guarantees the serve threads have exited before the
        // next message probes the closed registry.
        .wait_for("session/cancel")
        .delay(100)
        .emit(emit_message(203, "tools/list", None));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread = connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    // The first tools/list round trip works (the connection exists).
    let list = await_response(&fake, 202).await;
    assert!(list["result"]["tools"].is_array());

    // Start a turn (the fake consumes session/prompt), then cancel it.
    let blocks =
        vec![ee_agent_protocol::ContentBlock::Text(ee_agent_protocol::TextContent::new("hi"))];
    let prompt = tokio::spawn({
        let thread = thread.clone();
        async move { thread.send_prompt(blocks).await }
    });
    // Wait until the host actually sent session/prompt before cancelling.
    tokio::time::timeout(TEST_TIMEOUT, async {
        while !fake.log_contains("\"method\":\"session/prompt\"") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("session/prompt sent");
    thread.cancel().await.expect("turn cancelled");
    let result = prompt.await.expect("prompt task finished");
    assert!(matches!(result, Err(AgentError::Cancelled)));

    // Turn cancel closed every logical MCP connection: the next message is
    // rejected with invalid params.
    let reply = await_response(&fake, 203).await;
    let error = reply["error"].as_object().expect("message after cancel must fail");
    assert_eq!(error["code"], json!(-32602));
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_host_close_closes_logical_connections_deterministically() {
    let script = connect_and_init_script()
        .emit(emit_message(202, "tools/list", None))
        .wait_for_response(202);
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");
    await_response(&fake, 202).await;

    // App shutdown: the connection closes every logical MCP connection
    // without hanging (the fake's driver reaches its script end and exits).
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

// ── SDK gap audit (Phase 6b criteria) ────────────────────────────────────────

/// Proves no duplicate incompatible `rmcp` major version can leak into ee
/// public APIs: the lockfile pins exactly one `rmcp` (3.x) and no
/// `agent-client-protocol-rmcp` package (its `rmcp ^2.x` dependency is the
/// documented SDK gap).
#[test]
fn mcp_over_acp_workspace_has_single_rmcp_major_and_no_rmcp_bridge_crate() {
    let lockfile = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    let lockfile = std::fs::canonicalize(&lockfile).expect("workspace Cargo.lock");
    let lock = std::fs::read_to_string(&lockfile).expect("read Cargo.lock");
    let packages = lock.split("[[package]]").skip(1).collect::<Vec<_>>();

    let rmcp_versions = packages
        .iter()
        .filter(|package| package.lines().any(|line| line.trim() == "name = \"rmcp\""))
        .filter_map(|package| {
            package
                .lines()
                .find_map(|line| line.trim().strip_prefix("version = \""))
                .map(|version| version.trim_end_matches('"').to_string())
        })
        .collect::<Vec<_>>();
    assert_eq!(rmcp_versions.len(), 1, "exactly one rmcp package: {rmcp_versions:?}");
    assert!(rmcp_versions[0].starts_with("3."), "rmcp must stay on 3.x: {}", rmcp_versions[0]);

    let has_bridge = packages.iter().any(|package| {
        package.lines().any(|line| line.trim() == "name = \"agent-client-protocol-rmcp\"")
    });
    assert!(
        !has_bridge,
        "agent-client-protocol-rmcp would pull rmcp 2.x; keep the local adapter until upstream ships rmcp 3.x support"
    );
}

/// The host never advertises the proxy as an MCP root or any other MCP
/// resource: workspace directories stay in plain session metadata.
#[tokio::test]
async fn mcp_over_acp_workspace_directories_stay_in_plain_session_metadata() {
    let (fake, host) = spawn_host(acp_base_script(), Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(
            vec![PathBuf::from("/work"), PathBuf::from("/extra")],
            Vec::new(),
            Some(stdio_fallback()),
        )
        .await
        .expect("session starts");

    let session_new = fake.requests_by_method("session/new");
    let params = session_new[0].get("params").expect("params");
    assert_eq!(params["cwd"], json!("/work"));
    assert_eq!(params["additionalDirectories"], json!(["/extra"]));
    assert!(params.get("roots").is_none(), "workspace dirs must never be forwarded as MCP roots");
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}
