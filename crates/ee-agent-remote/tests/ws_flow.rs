//! WebSocket transport acceptance for R10.
//!
//! Covers token auth, health endpoints and Origin rejection, a full turn over
//! WS, cursor replay after reconnect, static assets, and multi-client approval
//! fan-out with first-answer-wins.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ee_agent_host::fake::{FakeAgent, FakeAgentScript, FakeAgentTransport, wire};
use ee_agent_host::{AgentConnectionOptions, AgentProcessConfig, FakeTransportFactory};
use ee_agent_remote::events::EventBus;
use ee_agent_remote::transport::{WsServeOptions, WsServer};
use ee_agent_remote::{RemoteHost, RemoteHostConfig};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);
const TOKEN: &str = "test-token-0123456789abcdef";

#[derive(Clone)]
struct ScriptedFake {
    script: FakeAgentScript,
    agent: Arc<std::sync::Mutex<Option<FakeAgent>>>,
}

impl ScriptedFake {
    fn new(script: FakeAgentScript) -> Self {
        Self { script, agent: Arc::new(std::sync::Mutex::new(None)) }
    }

    fn agent(&self) -> FakeAgent {
        self.agent
            .lock()
            .expect("fake handle poisoned")
            .clone()
            .expect("fake agent not spawned yet")
    }
}

impl FakeTransportFactory for ScriptedFake {
    fn build(&self) -> FakeAgentTransport {
        let (agent, transport) = FakeAgent::spawn(self.script.clone());
        *self.agent.lock().expect("fake handle poisoned") = Some(agent);
        transport
    }
}

struct Harness {
    host: RemoteHost,
    fake: ScriptedFake,
    addr: SocketAddr,
    token_file: std::path::PathBuf,
}

async fn spawn(script: FakeAgentScript, root: &Path, static_dir: Option<&Path>) -> Harness {
    spawn_with_idle(script, root, static_dir, TEST_TIMEOUT).await
}

async fn spawn_with_idle(
    script: FakeAgentScript,
    root: &Path,
    static_dir: Option<&Path>,
    idle_timeout: Duration,
) -> Harness {
    let fake = ScriptedFake::new(script);
    let mut config = RemoteHostConfig {
        allowed_roots: vec![root.to_path_buf()],
        request_timeout: TEST_TIMEOUT,
        approval_timeout: TEST_TIMEOUT,
        max_outbound_queue: 64,
        session_log_dir: Some(root.join("history")),
        ..RemoteHostConfig::default()
    };
    config.agents.insert("fake".into(), AgentProcessConfig::new("unused"));
    config.fake_transports.insert("fake".into(), Arc::new(fake.clone()));
    let host = RemoteHost::with_options(
        config,
        Arc::new(ee_agent_host::DenyAllHandler),
        AgentConnectionOptions {
            handshake_timeout: TEST_TIMEOUT,
            request_timeout: TEST_TIMEOUT,
            ..AgentConnectionOptions::default()
        },
    );
    host.start().expect("pump");
    let token_file = root.join("token");
    write_token_file(&token_file, TOKEN);
    let mut options = WsServeOptions::new("127.0.0.1:0".parse().expect("addr"), token_file.clone());
    options.static_dir = static_dir.map(Path::to_path_buf);
    options.idle_timeout = idle_timeout;
    let server = WsServer::bind(options).await.expect("bind");
    let addr = server.local_addr().expect("addr");
    let serve_host = host.clone();
    tokio::spawn(async move {
        let _ = server.serve(serve_host).await;
    });
    Harness { host, fake, addr, token_file }
}

fn write_token_file(path: &Path, token: &str) {
    std::fs::write(path, token).expect("token write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn streaming_script() -> FakeAgentScript {
    FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s-1" }))
        .wait_for("session/prompt")
        .emit(wire::session_update("s-1", wire::agent_message_chunk("m-1", "hello")))
        .respond(json!({ "stopReason": "end_turn" }))
}

fn permission_script() -> FakeAgentScript {
    FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s-1" }))
        .wait_for("session/prompt")
        .emit(json!({
            "jsonrpc": "2.0",
            "id": 100,
            "method": "session/request_permission",
            "params": {
                "sessionId": "s-1",
                "toolCall": {
                    "toolCallId": "call-1",
                    "title": "Run tests",
                    "name": "bash",
                    "kind": "execute",
                    "rawInput": { "command": "cargo test" }
                },
                "options": [
                    { "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" },
                    { "optionId": "reject_once", "name": "Reject", "kind": "reject_once" }
                ]
            }
        }))
        .wait_for_response(100)
        .respond(json!({ "stopReason": "end_turn" }))
}

async fn http_request(addr: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut buffer = Vec::new();
    let _ = tokio::time::timeout(TEST_TIMEOUT, stream.read_to_end(&mut buffer)).await;
    String::from_utf8_lossy(&buffer).to_string()
}

type Client = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

async fn connect(addr: SocketAddr, token: &str) -> Client {
    let url = format!("ws://{addr}/ws?token={token}");
    let (client, _response) =
        tokio::time::timeout(TEST_TIMEOUT, tokio_tungstenite::connect_async(url))
            .await
            .expect("ws connect timed out")
            .expect("ws connect failed");
    client
}

async fn send(client: &mut Client, value: Value) {
    client.send(Message::Text(value.to_string().into())).await.expect("send");
}

async fn recv(client: &mut Client) -> Value {
    loop {
        let message = tokio::time::timeout(TEST_TIMEOUT, client.next())
            .await
            .expect("recv timed out")
            .expect("stream ended")
            .expect("ws error");
        match message {
            Message::Text(text) => return serde_json::from_str(text.as_str()).expect("json"),
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => panic!("closed while waiting for a frame"),
            _ => continue,
        }
    }
}

async fn recv_response(client: &mut Client, id: &Value) -> Value {
    loop {
        let frame = recv(client).await;
        if frame.get("method").is_none() && frame.get("id") == Some(id) {
            return frame;
        }
    }
}

async fn handshake(client: &mut Client) {
    send(
        client,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": ee_agent_remote::REMOTE_PROTOCOL_VERSION, "clientId": "ws-test" }
        }),
    )
    .await;
    let _ = recv_response(client, &json!(1)).await;
    send(client, json!({ "jsonrpc": "2.0", "method": "initialized" })).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_endpoints_reject_origins_and_leak_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let harness = spawn(streaming_script(), dir.path(), None).await;

    let health =
        http_request(harness.addr, "GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
    assert!(health.starts_with("HTTP/1.1 200"), "{health}");
    assert!(health.contains("\"status\":\"ok\""));
    assert!(!health.contains("fake"), "health must not leak config");

    let origin = http_request(
        harness.addr,
        "GET /healthz HTTP/1.1\r\nHost: localhost\r\nOrigin: https://evil.test\r\n\r\n",
    )
    .await;
    assert!(origin.starts_with("HTTP/1.1 403"), "{origin}");

    let ready = http_request(harness.addr, "GET /readyz HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
    assert!(ready.starts_with("HTTP/1.1 200"), "{ready}");
    assert!(ready.contains("\"ready\":true"));

    let tokenless = http_request(
        harness.addr,
        "GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
    )
    .await;
    assert!(tokenless.starts_with("HTTP/1.1 401"), "{tokenless}");

    let wrong = tokio::time::timeout(
        TEST_TIMEOUT,
        tokio_tungstenite::connect_async(format!("ws://{}/ws?token=wrong", harness.addr)),
    )
    .await
    .expect("timed out")
    .expect_err("wrong token");
    let _ = wrong;
    let _ = &harness.token_file;
    let _ = EventBus::new(1, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_turn_and_reconnect_replay_over_websocket() {
    let dir = tempfile::tempdir().expect("tempdir");
    let harness = spawn(streaming_script(), dir.path(), None).await;
    let mut client = connect(harness.addr, TOKEN).await;
    handshake(&mut client).await;

    send(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": { "agentId": "fake", "cwd": dir.path().display().to_string() }
        }),
    )
    .await;
    let session = recv_response(&mut client, &json!(2)).await;
    assert_eq!(session["result"]["sessionId"], "s-1");

    send(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "turn/start",
            "params": { "sessionId": "s-1", "text": "hi" }
        }),
    )
    .await;

    let mut cursors = Vec::new();
    loop {
        let frame = recv(&mut client).await;
        if let Some(cursor) = frame["params"]["cursor"].as_u64()
            && cursor > 0
        {
            cursors.push((cursor, frame["method"].as_str().unwrap_or_default().to_string()));
        }
        if frame["method"] == "turn/completed" {
            break;
        }
    }
    drop(client);

    let mut reconnected = connect(harness.addr, TOKEN).await;
    handshake(&mut reconnected).await;
    send(
        &mut reconnected,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "history/tail",
            "params": { "sessionId": "s-1", "fromCursor": 0 }
        }),
    )
    .await;
    let replayed = recv_response(&mut reconnected, &json!(4)).await;
    let replayed: Vec<(u64, String)> = replayed["result"]["notifications"]
        .as_array()
        .expect("notifications")
        .iter()
        .map(|notification| {
            (
                notification["cursor"].as_u64().expect("cursor"),
                notification["method"].as_str().expect("method").to_string(),
            )
        })
        .collect();
    assert_eq!(cursors, replayed, "reconnect replays the same sequence");

    harness.host.shutdown().await;
    harness.fake.agent().join(TEST_TIMEOUT).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_see_the_approval_and_first_answer_wins() {
    let dir = tempfile::tempdir().expect("tempdir");
    let harness = spawn(permission_script(), dir.path(), None).await;
    let mut first = connect(harness.addr, TOKEN).await;
    let mut second = connect(harness.addr, TOKEN).await;
    handshake(&mut first).await;
    handshake(&mut second).await;

    send(
        &mut first,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": { "cwd": dir.path().display().to_string() }
        }),
    )
    .await;
    let _ = recv_response(&mut first, &json!(2)).await;
    send(
        &mut first,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "turn/start",
            "params": { "sessionId": "s-1", "text": "run" }
        }),
    )
    .await;

    let approval_first = recv_until_approval(&mut first).await;
    let approval_second = recv_until_approval(&mut second).await;
    assert_eq!(approval_first["id"], approval_second["id"]);
    let server_id = approval_first["id"].clone();
    let request_id = approval_first["params"]["requestId"].as_u64().expect("request id");

    // First client answers through the server→client request response.
    send(
        &mut first,
        json!({
            "jsonrpc": "2.0",
            "id": server_id.clone(),
            "result": { "decision": "allow" }
        }),
    )
    .await;

    // Wait until the first answer is applied before the late answer arrives,
    // so this assertion is deterministic.
    tokio::time::timeout(TEST_TIMEOUT, async {
        while harness.host.approvals().pending_count() != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("first answer applied");

    // Second client's poll-based answer arrives late and is typed.
    send(
        &mut second,
        json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "permissions/respond",
            "params": { "requestId": request_id, "decision": "deny" }
        }),
    )
    .await;
    let late = recv_response(&mut second, &json!(9)).await;
    assert_eq!(late["error"]["code"], -32005);

    let completed = recv_until_method(&mut first, "turn/completed").await;
    assert_eq!(completed["params"]["status"], "completed");
    let response = harness.fake.agent().response_with_id(100).expect("agent response");
    assert_eq!(response["result"]["outcome"]["optionId"], "allow_once");

    harness.host.shutdown().await;
    harness.fake.agent().join(TEST_TIMEOUT).await;
}

async fn recv_until_approval(client: &mut Client) -> Value {
    loop {
        let frame = recv(client).await;
        if frame["method"] == "permission/requestApproval" {
            return frame;
        }
    }
}

async fn recv_until_method(client: &mut Client, method: &str) -> Value {
    loop {
        let frame = recv(client).await;
        if frame["method"] == method {
            return frame;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_assets_are_served_with_mime_types_and_no_traversal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dist = dir.path().join("dist");
    std::fs::create_dir_all(&dist).expect("dist");
    std::fs::write(dist.join("index.html"), "<!doctype html>").expect("html");
    std::fs::write(dist.join("ee_web.wasm"), b"\0asm").expect("wasm");
    let harness = spawn(streaming_script(), dir.path(), Some(&dist)).await;

    let index = http_request(harness.addr, "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
    assert!(index.starts_with("HTTP/1.1 200"), "{index}");
    assert!(index.contains("text/html"));

    let wasm =
        http_request(harness.addr, "GET /ee_web.wasm HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
    assert!(wasm.starts_with("HTTP/1.1 200"), "{wasm}");
    assert!(wasm.contains("application/wasm"));

    let traversal =
        http_request(harness.addr, "GET /../token HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
    assert!(traversal.starts_with("HTTP/1.1 404"), "{traversal}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_disconnect_does_not_kill_an_in_flight_turn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s-1" }))
        .wait_for("session/prompt")
        .delay(300)
        .respond(json!({ "stopReason": "end_turn" }));
    let harness = spawn_with_idle(script, dir.path(), None, Duration::from_millis(80)).await;
    let mut client = connect(harness.addr, TOKEN).await;
    handshake(&mut client).await;
    send(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": { "cwd": dir.path().display().to_string() }
        }),
    )
    .await;
    let _ = recv_response(&mut client, &json!(2)).await;
    send(
        &mut client,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "turn/start",
            "params": { "sessionId": "s-1", "text": "slow" }
        }),
    )
    .await;

    // The connection idles out while the turn keeps running.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut reconnected = connect(harness.addr, TOKEN).await;
    handshake(&mut reconnected).await;
    let mut cursor = 0;
    let mut saw_completion = false;
    for _ in 0..100 {
        send(
            &mut reconnected,
            json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "history/tail",
                "params": { "sessionId": "s-1", "fromCursor": cursor }
            }),
        )
        .await;
        let tail = recv_response(&mut reconnected, &json!(5)).await;
        for notification in tail["result"]["notifications"].as_array().expect("notifications") {
            cursor = cursor.max(notification["cursor"].as_u64().expect("cursor"));
            if notification["method"] == "turn/completed" {
                saw_completion = true;
            }
        }
        if saw_completion {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(saw_completion, "the turn completed after the idle disconnect");
    let history_entries =
        std::fs::read_dir(dir.path().join("history")).expect("history dir").count();
    assert!(history_entries > 0, "the completed turn reached the durable log");

    harness.host.shutdown().await;
    harness.fake.agent().join(TEST_TIMEOUT).await;
}
