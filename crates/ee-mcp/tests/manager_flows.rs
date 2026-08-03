//! End-to-end MCP client manager tests against the in-process fake server.
//!
//! These cover: version pinning, per-request `_meta`, primitive listing and
//! namespacing, tool calls with content parsing, TTL caching, list-changed
//! notification refresh, MRTR elicitation retry with `inputResponses`,
//! secret field rejection, `subscriptions/listen`, request timeouts, and the
//! absence of deprecated features on the wire.
#![cfg(feature = "test-utils")]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ee_mcp::config::{McpServerConfig, McpServerKind, StdioMcpConfig};
use ee_mcp::events::{McpEvent, McpServerState};
use ee_mcp::fake::{FakeMcpScript, FakeMcpServer, FakeMcpTransport, FakeMcpTransportFactory};
use ee_mcp::{McpClientManager, McpError};
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// A fake server factory whose handle is retrievable after connect.
#[derive(Clone)]
struct ScriptedFake {
    script: FakeMcpScript,
    handle: Arc<Mutex<Option<FakeMcpServer>>>,
}

impl ScriptedFake {
    fn new(script: FakeMcpScript) -> Self {
        Self { script, handle: Arc::new(Mutex::new(None)) }
    }

    fn server(&self) -> FakeMcpServer {
        self.handle
            .lock()
            .expect("fake handle poisoned")
            .clone()
            .expect("fake server not spawned yet (start the manager first)")
    }
}

impl FakeMcpTransportFactory for ScriptedFake {
    fn build(&self) -> FakeMcpTransport {
        let (server, transport) = FakeMcpServer::spawn(self.script.clone());
        *self.handle.lock().expect("fake handle poisoned") = Some(server);
        transport
    }
}

fn stdio_config(id: &str, timeout_ms: u64) -> McpServerConfig {
    McpServerConfig {
        id: id.to_string(),
        kind: McpServerKind::Stdio(StdioMcpConfig {
            command: "unused".to_string(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
            stderr_cap: 1024,
        }),
        timeout_ms,
    }
}

fn capabilities() -> Value {
    json!({ "tools": {}, "prompts": {}, "resources": {} })
}

fn discover(capabilities: Value) -> Value {
    json!({
        "resultType": "complete",
        "supportedVersions": ["2026-07-28"],
        "capabilities": capabilities,
        "ttlMs": 0,
        "cacheScope": "private",
    })
}

fn tools_result(tools: Value, ttl_ms: u64) -> Value {
    json!({
        "tools": tools,
        "resultType": "complete",
        "ttlMs": ttl_ms,
        "cacheScope": "private",
    })
}

fn tool(name: &str) -> Value {
    json!({ "name": name, "description": format!("{name} tool"), "inputSchema": { "type": "object", "properties": {} } })
}

/// Builds a manager with one fake `srv` server.
async fn manager_with(
    script: FakeMcpScript,
) -> (McpClientManager, mpsc::UnboundedReceiver<McpEvent>, ScriptedFake) {
    let mut configs = BTreeMap::new();
    configs.insert("srv".to_string(), stdio_config("srv", 5000));
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let manager = McpClientManager::new(configs, events_tx);
    let fake = ScriptedFake::new(script);
    manager.install_fake_transport("srv", Arc::new(fake.clone())).await;
    (manager, events_rx, fake)
}

async fn next_event(rx: &mut mpsc::UnboundedReceiver<McpEvent>, label: &str) -> McpEvent {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for event {label}"))
        .unwrap_or_else(|| panic!("event channel closed while waiting for {label}"))
}

async fn drain_until_state(rx: &mut mpsc::UnboundedReceiver<McpEvent>, wanted: McpServerState) {
    loop {
        match next_event(rx, "state").await {
            McpEvent::ServerState { state, .. } if state == wanted => return,
            _ => {}
        }
    }
}

// ── Discovery ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn discovery_rejects_unsupported_protocol_version() {
    let script = FakeMcpScript::new().respond(
        "server/discover",
        json!({
            "resultType": "complete",
            "supportedVersions": ["2025-11-25"],
            "capabilities": {},
            "ttlMs": 0,
            "cacheScope": "private",
        }),
    );
    let (manager, mut events, fake) = manager_with(script).await;
    let error = manager.start("srv").await.expect_err("unsupported version rejected");
    assert!(error.is_unsupported_version(), "error: {error}");
    assert!(matches!(
        error,
        McpError::UnsupportedProtocolVersion { ref server_supported }
            if server_supported == &vec!["2025-11-25".to_string()]
    ));
    drain_until_state(&mut events, McpServerState::Failed).await;
    // The server keeps retrying; stop the manager to end the loop.
    manager.shutdown().await;
    let _ = fake.server().join(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn start_reaches_ready_and_publishes_discovery_snapshot() {
    let script = FakeMcpScript::new()
        .discover_2026_07_28(json!({ "tools": {}, "resources": { "listChanged": true } }));
    let (manager, _events, fake) = manager_with(script).await;
    manager.start("srv").await.expect("ready");
    let snapshot = manager.discovery("srv").await.expect("discovery cached");
    assert_eq!(snapshot.supported_versions, vec!["2026-07-28"]);
    assert!(snapshot.capabilities.tools);
    assert!(snapshot.capabilities.resources_list_changed);
    manager.shutdown().await;
    let _ = fake.server().join(Duration::from_secs(5)).await;
}

// ── _meta ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn every_request_carries_2026_07_28_meta() {
    let script = FakeMcpScript::new()
        .discover_2026_07_28(capabilities())
        .respond("tools/list", tools_result(json!([tool("read")]), 0));
    let (manager, _events, fake) = manager_with(script).await;
    manager.start("srv").await.expect("ready");
    let tools = manager.list_tools("srv").await.expect("tools");
    assert_eq!(tools.len(), 1);
    manager.shutdown().await;

    let log = fake.server().log();
    assert!(!log.is_empty(), "client must have sent requests");
    for line in &log {
        let value: Value = serde_json::from_str(line).expect("client line parses");
        let Some(method) = value.get("method").and_then(Value::as_str) else {
            continue; // responses carry no _meta
        };
        let meta = value.pointer("/params/_meta").expect("request carries _meta");
        assert_eq!(
            meta["io.modelcontextprotocol/protocolVersion"], "2026-07-28",
            "request {method} missing protocol version meta: {line}"
        );
        assert!(
            meta.get("io.modelcontextprotocol/clientInfo").is_some(),
            "request {method} missing client info: {line}"
        );
        assert!(
            meta.get("io.modelcontextprotocol/clientCapabilities").is_some(),
            "request {method} missing client capabilities: {line}"
        );
    }
    let _ = fake.server().join(Duration::from_secs(5)).await;
}

// ── Primitives ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn tools_are_namespaced_and_callable() {
    let script = FakeMcpScript::new()
        .discover_2026_07_28(capabilities())
        .respond("tools/list", tools_result(json!([tool("read"), tool("write")]), 0))
        .respond(
            "tools/call",
            json!({ "resultType": "complete", "content": [ { "type": "text", "text": "ok" } ] }),
        );
    let (manager, _events, fake) = manager_with(script).await;
    manager.start("srv").await.expect("ready");
    let tools = manager.list_tools("srv").await.expect("tools");
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].key, "srv/read");
    assert_eq!(tools[1].key, "srv/write");

    let result = manager.call_tool("srv", "srv/read", json!({})).await.expect("call");
    assert_eq!(result.content.len(), 1);

    // The wire call uses the raw tool name, not the namespaced key.
    let calls = fake.server().requests_by_method("tools/call");
    assert_eq!(calls[0]["params"]["name"], "read");
    manager.shutdown().await;
    let _ = fake.server().join(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn call_tool_parses_content_blocks_and_is_error() {
    let script = FakeMcpScript::new()
        .discover_2026_07_28(capabilities())
        .respond(
            "tools/call",
            json!({
                "resultType": "complete",
                "isError": true,
                "content": [
                    { "type": "text", "text": "hello" },
                    { "type": "image", "data": "aGk=", "mimeType": "image/png" },
                    { "type": "audio", "data": "aGk=", "mimeType": "audio/wav" },
                    { "type": "resource", "resource": { "uri": "file:///tmp/x", "text": "body", "mimeType": "text/plain" } },
                ],
            }),
        );
    let (manager, _events, fake) = manager_with(script).await;
    manager.start("srv").await.expect("ready");
    let result = manager.call_tool("srv", "srv/any", json!({})).await.expect("call");
    assert!(result.is_error.unwrap_or(false), "isError parsed");
    assert_eq!(result.content.len(), 4);
    assert!(matches!(result.content[0], rmcp::model::ContentBlock::Text(_)));
    assert!(matches!(result.content[1], rmcp::model::ContentBlock::Image(_)));
    assert!(matches!(result.content[2], rmcp::model::ContentBlock::Audio(_)));
    assert!(matches!(result.content[3], rmcp::model::ContentBlock::Resource(_)));
    manager.shutdown().await;
    let _ = fake.server().join(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn unknown_server_and_namespaced_tool_fail_closed() {
    let script = FakeMcpScript::new().discover_2026_07_28(capabilities());
    let (manager, _events, _fake) = manager_with(script).await;
    manager.start("srv").await.expect("ready");
    assert!(manager.list_tools("missing").await.is_err());
    assert!(manager.call_tool("srv", "other/name", json!({})).await.is_err());
    manager.shutdown().await;
}

#[tokio::test]
async fn prompts_resources_and_templates_are_listed() {
    let script = FakeMcpScript::new()
        .discover_2026_07_28(capabilities())
        .respond("prompts/list", json!({ "prompts": [ { "name": "greet", "description": "g", "arguments": [ { "name": "who", "required": true } ] } ], "resultType": "complete", "ttlMs": 0, "cacheScope": "private" }))
        .respond("resources/list", json!({ "resources": [ { "uri": "file:///tmp/a", "name": "a" } ], "resultType": "complete", "ttlMs": 0, "cacheScope": "private" }))
        .respond("resources/templates/list", json!({ "resourceTemplates": [ { "uriTemplate": "file:///{path}", "name": "t" } ], "resultType": "complete", "ttlMs": 0, "cacheScope": "private" }))
        .respond("resources/read", json!({ "resultType": "complete", "contents": [ { "uri": "file:///tmp/a", "text": "body", "mimeType": "text/plain" } ] }))
        .respond("prompts/get", json!({ "resultType": "complete", "description": "g", "messages": [ { "role": "user", "content": { "type": "text", "text": "hi" } } ] }));
    let (manager, _events, fake) = manager_with(script).await;
    manager.start("srv").await.expect("ready");

    let prompts = manager.list_prompts("srv").await.expect("prompts");
    assert_eq!(prompts[0].key, "srv/greet");

    let resources = manager.list_resources("srv").await.expect("resources");
    assert_eq!(resources[0].resource.uri, "file:///tmp/a");

    let templates = manager.list_resource_templates("srv").await.expect("templates");
    assert_eq!(templates[0].template.uri_template, "file:///{path}");

    let read = manager.read_resource("srv", "file:///tmp/a").await.expect("read");
    assert_eq!(read.contents.len(), 1);

    let prompt =
        manager.get_prompt("srv", "srv/greet", Some(json!({ "who": "ed" }))).await.expect("get");
    assert_eq!(prompt.messages.len(), 1);
    manager.shutdown().await;
    let _ = fake.server().join(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn tool_list_pagination_follows_cursors() {
    let script = FakeMcpScript::new()
        .discover_2026_07_28(capabilities())
        .respond_once(
            "tools/list",
            json!({
                "tools": [tool("alpha")],
                "nextCursor": "page-2",
                "resultType": "complete",
                "ttlMs": 0,
                "cacheScope": "private",
            }),
        )
        .respond_once(
            "tools/list",
            json!({ "tools": [tool("beta")], "resultType": "complete", "ttlMs": 0, "cacheScope": "private" }),
        );
    let (manager, _events, fake) = manager_with(script).await;
    manager.start("srv").await.expect("ready");
    let tools = manager.list_tools("srv").await.expect("all pages");
    let names: Vec<&str> = tools.iter().map(|t| t.tool.name.as_ref()).collect();
    assert_eq!(names, vec!["alpha", "beta"], "cursor pagination must collect every page");
    let requests = fake.server().requests_by_method("tools/list");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1]["params"]["cursor"], "page-2", "second page carries the cursor");
    manager.shutdown().await;
    let _ = fake.server().join(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn primitive_list_is_ttl_cached() {
    let script = FakeMcpScript::new()
        .discover_2026_07_28(capabilities())
        // ttlMs 60s: repeated listing must not hit the wire.
        .respond("tools/list", tools_result(json!([tool("read")]), 60_000));
    let (manager, _events, fake) = manager_with(script).await;
    manager.start("srv").await.expect("ready");
    let first = manager.list_tools("srv").await.expect("first");
    assert_eq!(first.len(), 1);
    let second = manager.list_tools("srv").await.expect("second");
    assert_eq!(second.len(), 1);
    assert_eq!(
        fake.server().requests_by_method("tools/list").len(),
        1,
        "second list must be served from the TTL cache"
    );
    manager.shutdown().await;
    let _ = fake.server().join(Duration::from_secs(5)).await;
}

// ── Notifications ────────────────────────────────────────────────────────────

#[tokio::test]
async fn tool_list_changed_notification_refreshes_registry() {
    let script = FakeMcpScript::new()
        .discover_2026_07_28(capabilities())
        .respond_once("tools/list", tools_result(json!([tool("alpha")]), 60_000))
        .emit(json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }))
        .respond_once("tools/list", tools_result(json!([tool("alpha"), tool("beta")]), 60_000));
    let (manager, mut events, fake) = manager_with(script).await;
    manager.start("srv").await.expect("ready");

    let first = manager.list_tools("srv").await.expect("first");
    assert_eq!(first.len(), 1);

    // The notification arrives asynchronously; consume it, then refresh.
    loop {
        match tokio::time::timeout(Duration::from_secs(3), events.recv()).await {
            Ok(Some(McpEvent::ToolListChanged { server_id })) if server_id == "srv" => break,
            Ok(Some(other)) => eprintln!("DBG event: {other:?}"),
            Ok(None) => panic!("channel closed"),
            Err(_) => {
                eprintln!("DBG fake log: {:?}", fake.server().log());
                panic!("timed out waiting for tool list changed");
            }
        }
    }
    manager.refresh_registry("srv").await.expect("refresh");

    let second = manager.list_tools("srv").await.expect("second");
    assert_eq!(second.len(), 2, "registry must reflect the refreshed tool list");
    assert_eq!(
        fake.server().requests_by_method("tools/list").len(),
        2,
        "list-changed must force a real re-list"
    );
    manager.shutdown().await;
    let _ = fake.server().join(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn resource_and_prompt_list_changed_events_are_emitted() {
    // Sequence the notifications after both discover rounds so the client's
    // serve loop is running when they arrive.
    let script = FakeMcpScript::new()
        .respond_once("server/discover", discover(capabilities()))
        .respond_once("server/discover", discover(capabilities()))
        .emit(json!({ "jsonrpc": "2.0", "method": "notifications/resources/list_changed" }))
        .emit(json!({ "jsonrpc": "2.0", "method": "notifications/prompts/list_changed" }));
    let (manager, mut events, fake) = manager_with(script).await;
    manager.start("srv").await.expect("ready");
    let mut saw_resource = false;
    let mut saw_prompt = false;
    for _ in 0..10 {
        match next_event(&mut events, "list-changed").await {
            McpEvent::ResourceListChanged { server_id } if server_id == "srv" => {
                saw_resource = true
            }
            McpEvent::PromptListChanged { server_id } if server_id == "srv" => saw_prompt = true,
            _ => {}
        }
        if saw_resource && saw_prompt {
            break;
        }
    }
    assert!(saw_resource && saw_prompt, "both list-changed events must be emitted");
    manager.shutdown().await;
    let _ = fake.server().join(Duration::from_secs(5)).await;
}

// ── Elicitation ──────────────────────────────────────────────────────────────

fn input_required_with_elicitation(elicitation: Value, key: &str, request_state: &str) -> Value {
    json!({
        "resultType": "input_required",
        "inputRequests": {
            key: {
                "jsonrpc": "2.0",
                "id": 7,
                "method": "elicitation/create",
                "params": elicitation,
            }
        },
        "requestState": request_state,
    })
}

fn form_elicitation(message: &str, properties: Value) -> Value {
    json!({
        "mode": "form",
        "message": message,
        "requestedSchema": {
            "type": "object",
            "properties": properties,
        },
    })
}

#[tokio::test]
async fn elicitation_round_trips_input_responses_and_retries() {
    let script = FakeMcpScript::new()
        .discover_2026_07_28(capabilities())
        .respond_once(
            "tools/call",
            input_required_with_elicitation(
                form_elicitation("confirm?", json!({ "name": { "type": "string" } })),
                "k1",
                "st-1",
            ),
        )
        .respond(
            "tools/call",
            json!({ "resultType": "complete", "content": [ { "type": "text", "text": "done" } ] }),
        );
    let (manager, mut events, fake) = manager_with(script).await;
    manager.start("srv").await.expect("ready");
    let host = tokio::spawn(async move {
        loop {
            if let McpEvent::Elicitation(handle) = next_event(&mut events, "elicitation").await {
                assert_eq!(handle.server_id, "srv");
                let rmcp::model::ElicitRequestParams::FormElicitationParams {
                    message,
                    requested_schema,
                    ..
                } = handle.request
                else {
                    panic!("expected form elicitation");
                };
                assert_eq!(message, "confirm?");
                assert!(requested_schema.properties.contains_key("name"));
                let _ = handle.reply.send(Ok(rmcp::model::ElicitResult::new(
                    rmcp::model::ElicitationAction::Accept,
                )
                .with_content(serde_json::json!({ "name": "ed" }))));
                return;
            }
        }
    });

    let result = manager.call_tool("srv", "srv/any", json!({})).await.expect("call completes");
    host.await.expect("host answered");

    // The retried request carries inputResponses and the echoed requestState.
    let calls = fake.server().requests_by_method("tools/call");
    assert_eq!(calls.len(), 2);
    let retry = &calls[1];
    assert_eq!(retry["params"]["requestState"], "st-1");
    assert!(retry["params"]["inputResponses"]["k1"].is_object(), "input responses echoed");
    assert_eq!(
        retry["params"]["inputResponses"]["k1"]["content"]["name"], "ed",
        "accepted content in the retry"
    );
    assert_eq!(result.content[0].as_text().map(|t| t.text.as_str()), Some("done"));
    manager.shutdown().await;
    let _ = fake.server().join(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn secret_form_fields_are_rejected_without_reaching_host() {
    let script = FakeMcpScript::new()
        .discover_2026_07_28(capabilities())
        .respond_once(
            "tools/call",
            input_required_with_elicitation(
                form_elicitation("give me your key", json!({ "apiKey": { "type": "string" } })),
                "k1",
                "st-2",
            ),
        )
        .respond("tools/call", json!({ "resultType": "complete", "content": [] }));
    let (manager, mut events, fake) = manager_with(script).await;
    manager.start("srv").await.expect("ready");

    // No Elicitation event may be emitted; a diagnostics event is expected.
    let host = tokio::spawn(async move {
        for _ in 0..10 {
            match next_event(&mut events, "diagnostics").await {
                McpEvent::Diagnostics { message, .. } if message.contains("secret-like") => {
                    return;
                }
                McpEvent::Elicitation(_) => panic!("secret elicitation must not reach the host"),
                _ => {}
            }
        }
        panic!("expected a secret-rejection diagnostics event");
    });

    let error = manager.call_tool("srv", "srv/any", json!({})).await.expect_err("rejected");
    assert!(matches!(error, McpError::Protocol(_)), "error: {error}");
    host.await.expect("diagnostics emitted");
    manager.shutdown().await;
    let _ = fake.server().join(Duration::from_secs(5)).await;
}

// ── subscriptions/listen ─────────────────────────────────────────────────────

#[tokio::test]
async fn subscriptions_listen_acknowledges_and_streams() {
    use rmcp::model::SubscriptionFilter;
    let script = FakeMcpScript::new()
        .discover_2026_07_28(capabilities())
        .respond("subscriptions/listen", json!({ "resultType": "complete" }));
    let (manager, _events, fake) = manager_with(script).await;
    manager.start("srv").await.expect("ready");
    let mut filter = SubscriptionFilter::new();
    filter.tools_list_changed = Some(true);
    let mut subscription = manager.subscribe("srv", filter).await.expect("listen acknowledged");
    assert_eq!(subscription.subscription.acknowledged().tools_list_changed, Some(true));
    let _ = subscription.subscription.cancel().await;
    manager.shutdown().await;
    let _ = fake.server().join(Duration::from_secs(5)).await;
}

// ── Timeout / lifecycle ──────────────────────────────────────────────────────

#[tokio::test]
async fn unresponsive_server_hits_configured_request_timeout() {
    let script = FakeMcpScript::new()
        .discover_2026_07_28(capabilities())
        // No tools/list response: the request must time out.
        .delay(10_000);
    let mut configs = BTreeMap::new();
    configs.insert("srv".to_string(), stdio_config("srv", 200));
    let (events_tx, _events_rx) = mpsc::unbounded_channel();
    let manager = McpClientManager::new(configs, events_tx);
    let fake = ScriptedFake::new(script);
    manager.install_fake_transport("srv", Arc::new(fake.clone())).await;
    manager.start("srv").await.expect("ready");
    let error = manager.list_tools("srv").await.expect_err("timeout");
    assert!(matches!(error, McpError::Timeout { .. }), "error: {error}");
    manager.shutdown().await;
    let _ = fake.server().join(Duration::from_secs(5)).await;
}

// ── Deprecated features absent ───────────────────────────────────────────────

#[tokio::test]
async fn deprecated_features_are_never_sent_or_answered() {
    let script = FakeMcpScript::new()
        .discover_2026_07_28(capabilities())
        .respond("tools/list", tools_result(json!([tool("read")]), 0))
        .respond("tools/call", json!({ "resultType": "complete", "content": [] }));
    let (manager, _events, fake) = manager_with(script).await;
    manager.start("srv").await.expect("ready");
    let _ = manager.list_tools("srv").await.expect("tools");
    let _ = manager.call_tool("srv", "srv/read", json!({})).await.expect("call");
    manager.shutdown().await;

    let log = fake.server().log();
    for forbidden in [
        "roots/list",
        "sampling/createMessage",
        "logging/setLevel",
        "notifications/initialized",
        "clients/register",
        "dynamic",
    ] {
        assert!(
            !log.iter().any(|line| line.contains(forbidden)),
            "deprecated feature request must never be emitted: {forbidden:?} in {log:?}"
        );
    }
    let _ = fake.server().join(Duration::from_secs(5)).await;
}

// ── HTTP transport ───────────────────────────────────────────────────────────

/// A minimal raw-TCP Streamable HTTP endpoint: answers POST bodies with a
/// JSON-RPC error (echoing the request id) and keeps GET streams open as
/// empty SSE.
async fn spawn_raw_http_endpoint() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let url = format!("http://{addr}/mcp");
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                // Read the request head + body (bounded).
                loop {
                    let n = match socket.read(&mut chunk).await {
                        Ok(0) => return,
                        Ok(n) => n,
                        Err(_) => return,
                    };
                    buffer.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buffer);
                    if let Some(head_end) = text.find("\r\n\r\n") {
                        let head = &text[..head_end];
                        let content_length = head
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if buffer.len() >= head_end + 4 + content_length {
                            break;
                        }
                    }
                    if buffer.len() > 64 * 1024 {
                        return;
                    }
                }
                let text = String::from_utf8_lossy(&buffer);
                if text.starts_with("GET") {
                    // Notification stream: hold open as empty SSE.
                    let _ = socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n",
                        )
                        .await;
                    let _ = socket.flush().await;
                    let mut sink = [0u8; 1024];
                    loop {
                        if socket.read(&mut sink).await.unwrap_or(0) == 0 {
                            return;
                        }
                    }
                }
                // POST: echo a JSON-RPC error for the request id.
                let body = text.split("\r\n\r\n").nth(1);
                let parsed = body.and_then(|b| serde_json::from_str::<Value>(b).ok());
                if let (Some(_body), Some(value)) = (body, parsed) {
                    let id = value.get("id").cloned().unwrap_or(Value::Null);
                    let response = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": "method not found" }
                    });
                    let body = response.to_string();
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(body.as_bytes()).await;
                    let _ = socket.flush().await;
                }
            });
        }
    });
    (url, handle)
}

#[tokio::test]
async fn http_json_rpc_error_surfaces_as_protocol_error() {
    let (url, server) = spawn_raw_http_endpoint().await;
    let mut configs = BTreeMap::new();
    configs.insert(
        "http-srv".to_string(),
        McpServerConfig {
            id: "http-srv".to_string(),
            kind: McpServerKind::StreamableHttp(ee_mcp::config::StreamableHttpConfig {
                url,
                headers: BTreeMap::new(),
            }),
            timeout_ms: 3000,
        },
    );
    let (events_tx, _events_rx) = mpsc::unbounded_channel();
    let manager = McpClientManager::new(configs, events_tx);
    let error = manager.start("http-srv").await.expect_err("json-rpc error surfaces");
    assert!(matches!(error, McpError::Protocol(_)), "error: {error}");
    assert!(
        error.to_string().contains("method not found") || error.to_string().contains("-32601"),
        "error text carries the server error: {error}"
    );
    manager.shutdown().await;
    server.abort();
}

// ── State machine ────────────────────────────────────────────────────────────

#[tokio::test]
async fn manager_starts_lazily_and_tracks_states() {
    let script = FakeMcpScript::new().discover_2026_07_28(capabilities());
    let (manager, _events, fake) = manager_with(script).await;
    // Nothing started until asked.
    assert!(manager.state("srv").await.is_none());
    manager.start("srv").await.expect("ready");
    assert_eq!(manager.state("srv").await, Some(McpServerState::Ready));
    // Idempotent start.
    manager.start("srv").await.expect("already running");
    manager.shutdown().await;
    assert!(manager.state("srv").await.is_none());
    let _ = fake.server().join(Duration::from_secs(5)).await;
}
