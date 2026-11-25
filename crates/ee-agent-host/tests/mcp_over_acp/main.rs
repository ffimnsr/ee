//! Phase 6b: ACP-native MCP-over-ACP flows against the in-process fake ACP
//! agent.
//!
//! These tests exercise the real connection stack (SDK dispatch, driver,
//! approval-executor bridge, rmcp serve loop, `EeMcpProxy` tool surface)
//! over the scripted fake transport.  The fake agent advertises
//! `mcp_capabilities.acp`, receives ACP-native `ee` server entry in
//! `session/new`, connects with `mcp/connect`, negotiates through inner MCP
//! `server/discover`, and exchanges messages with `mcp/message`; capture steps
//! let the script use host-generated server/connection ids.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ee_agent_host::fake::{CaptureSource, FakeAgent, FakeAgentScript, FakeAgentTransport};
use ee_agent_host::reducer::MessageKind;
use ee_agent_host::{
    AgentConnection, AgentConnectionOptions, AgentError, AgentManager, AgentManagerConfig,
    AgentProcessConfig, ClientRequest, ClientRequestHandler, ClientRequestResponse,
    ClientRequestResult, DenyAllHandler, EeProxyMode, EeProxyToolProfile, FakeTransportFactory,
    HandlerCapabilities, MCP_OVER_ACP_MAX_FRAME_BYTES, RecordingHandler, WorkspaceMemoryHostConfig,
    WorkspaceMemoryMutationApproval,
};
use ee_agent_protocol::{
    ContentBlock, CreateTerminalRequest, McpServer, McpServerStdio, ReadTextFileRequest,
    ReadTextFileResponse, TextContent, WriteTextFileRequest,
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
                ClientRequest::ProxyMovePath { source_path, destination_path } => {
                    Ok(ClientRequestResponse::ProxyValue(json!({
                        "path": source_path,
                        "destinationPath": destination_path,
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
                ClientRequest::ApproveWorkspaceMemoryMutation { .. } => {
                    Ok(ClientRequestResponse::WorkspaceMemoryApproval { approved: true })
                }
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

struct OneTransportFactory(Mutex<Option<FakeAgentTransport>>);

impl FakeTransportFactory for OneTransportFactory {
    fn build(&self) -> FakeAgentTransport {
        self.0.lock().expect("transport factory poisoned").take().expect("single transport")
    }
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
    spawn_host_with_profile(script, handler, proxy_enabled, EeProxyToolProfile::Full).await
}

async fn spawn_host_with_profile(
    script: FakeAgentScript,
    handler: Arc<dyn ee_agent_host::ClientRequestHandler>,
    proxy_enabled: bool,
    profile: EeProxyToolProfile,
) -> (FakeAgent, TestHost) {
    let (fake, transport) = FakeAgent::spawn(script);
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let options = AgentConnectionOptions {
        handshake_timeout: TEST_TIMEOUT,
        request_timeout: TEST_TIMEOUT,
        ee_proxy_enabled: proxy_enabled,
        ee_proxy_tool_profile: profile,
        ..AgentConnectionOptions::default()
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

async fn spawn_manager_host_with_memory(
    script: FakeAgentScript,
    handler: Arc<dyn ee_agent_host::ClientRequestHandler>,
) -> (FakeAgent, TestHost, tempfile::TempDir) {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("workspace");
    std::fs::create_dir(&root).expect("workspace root");
    let (fake, host) = spawn_manager_host_with_memory_at(
        script,
        handler,
        "fake",
        root,
        temp.path().join("memory.sqlite3"),
    )
    .await;
    (fake, host, temp)
}

async fn spawn_manager_host_with_memory_at(
    script: FakeAgentScript,
    handler: Arc<dyn ee_agent_host::ClientRequestHandler>,
    agent_id: &str,
    root: PathBuf,
    database_path: PathBuf,
) -> (FakeAgent, TestHost) {
    let (fake, transport) = FakeAgent::spawn(script);
    let factory = Arc::new(OneTransportFactory(Mutex::new(Some(transport))));
    let config = AgentManagerConfig {
        agents: BTreeMap::from([(agent_id.to_string(), AgentProcessConfig::new("unused"))]),
        ee_proxy_enabled: true,
        workspace_memory: WorkspaceMemoryHostConfig {
            enabled: true,
            trusted_roots: vec![root],
            database_path: Some(database_path),
            ..Default::default()
        },
        permission_rules: Arc::new(ee_agent_host::PermissionRules::default()),
        fake_transports: BTreeMap::from([(
            agent_id.to_string(),
            factory as Arc<dyn FakeTransportFactory>,
        )]),
    };
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let manager = AgentManager::with_options(
        config,
        handler,
        events_tx,
        AgentConnectionOptions {
            handshake_timeout: TEST_TIMEOUT,
            request_timeout: TEST_TIMEOUT,
            ..Default::default()
        },
    );
    let connection = manager.connection(agent_id).await.expect("manager connection");
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

/// Required MCP `2026-07-28` client context carried by every request.
fn inner_request_meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": { "name": "fake-agent", "version": "0" },
        "io.modelcontextprotocol/clientCapabilities": {}
    })
}

/// An `mcp/message` request with captured `conn_id` and required request metadata.
fn emit_message(id: i64, method: &str, params: Option<Value>) -> Value {
    let mut params = params.unwrap_or_else(|| json!({}));
    params
        .as_object_mut()
        .expect("inner MCP request params must be an object")
        .insert(String::from("_meta"), inner_request_meta());
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "mcp/message",
        "params": {
            "connectionId": { "$capture": "conn_id" },
            "method": method,
            "params": params
        }
    })
}

/// An inner MCP notification with latest-protocol request metadata.
fn emit_notification(method: &str, params: Value) -> Value {
    let mut params = params;
    params
        .as_object_mut()
        .expect("inner MCP notification params must be an object")
        .insert(String::from("_meta"), inner_request_meta());
    json!({
        "jsonrpc": "2.0",
        "method": "mcp/message",
        "params": {
            "connectionId": { "$capture": "conn_id" },
            "method": method,
            "params": params
        }
    })
}

/// Script tail: connect, capture connection id, and negotiate inner MCP
/// `2026-07-28` through `server/discover` (ids 200/201). Follow-up requests
/// start at 202.
fn connect_and_discover_script() -> FakeAgentScript {
    acp_base_script()
        .emit(emit_connect(200))
        .capture(CaptureSource::Response { id: 200 }, "result.connectionId", "conn_id")
        .emit(emit_message(201, "server/discover", Some(json!({}))))
        .wait_for_response(201)
}

async fn ready_connection(fake: &FakeAgent, host: &TestHost) -> AgentConnection {
    let connection = host.connection.clone();
    connection.wait_ready().await.expect("handshake succeeds");
    assert!(fake.log_contains("\"method\":\"initialize\""));
    connection
}

mod advertisement;
mod approval_tools;
mod closure;
mod connect_flows;
mod connect_tools;
