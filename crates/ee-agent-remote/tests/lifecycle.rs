//! Lifecycle hardening acceptance for R13.
//!
//! Covers typed request timeouts, interrupt propagation to ACP
//! `session/cancel`, shutdown resolving turns and approvals, and completion
//! consistency when interrupt races completion.

use std::sync::Arc;
use std::time::Duration;

use ee_agent_host::fake::{FakeAgent, FakeAgentScript, FakeAgentTransport, wire};
use ee_agent_host::{AgentConnectionOptions, AgentProcessConfig, FakeTransportFactory};
use ee_agent_remote::events::Subscription;
use ee_agent_remote::protocol::Method;
use ee_agent_remote::transport::serve_stream;
use ee_agent_remote::{RemoteHost, RemoteHostConfig};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

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

fn options() -> AgentConnectionOptions {
    AgentConnectionOptions {
        handshake_timeout: TEST_TIMEOUT,
        request_timeout: TEST_TIMEOUT,
        ..AgentConnectionOptions::default()
    }
}

fn host(
    script: FakeAgentScript,
    dir: &std::path::Path,
    request_timeout: Duration,
) -> (RemoteHost, ScriptedFake) {
    let fake = ScriptedFake::new(script);
    let mut config = RemoteHostConfig {
        allowed_roots: vec![dir.to_path_buf()],
        request_timeout,
        approval_timeout: TEST_TIMEOUT,
        audit_path: Some(dir.join("audit.jsonl")),
        ..RemoteHostConfig::default()
    };
    config.agents.insert("fake".into(), AgentProcessConfig::new("unused"));
    config.fake_transports.insert("fake".into(), Arc::new(fake.clone()));
    let host = RemoteHost::with_options(config, Arc::new(ee_agent_host::DenyAllHandler), options());
    host.start().expect("pump");
    (host, fake)
}

async fn initialize(host: &RemoteHost) -> String {
    host.dispatch(
        Method::Initialize,
        Some(json!({ "protocolVersion": ee_agent_remote::REMOTE_PROTOCOL_VERSION })),
    )
    .await
    .expect("initialize");
    let cwd = host.config().allowed_roots[0].display().to_string();
    host.dispatch(Method::SessionNew, Some(json!({ "agentId": "fake", "cwd": cwd })))
        .await
        .expect("session/new")["sessionId"]
        .as_str()
        .expect("session id")
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_timeouts_are_typed_and_bounded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .delay(600)
        .respond(json!({ "sessionId": "s-1" }));
    let fake = ScriptedFake::new(script);
    let mut config = RemoteHostConfig {
        request_timeout: Duration::from_millis(60),
        ..RemoteHostConfig::default()
    };
    config.agents.insert("fake".into(), AgentProcessConfig::new("unused"));
    config.fake_transports.insert("fake".into(), Arc::new(fake.clone()));
    let host = RemoteHost::with_options(config, Arc::new(ee_agent_host::DenyAllHandler), options());
    host.start().expect("pump");

    let (client_writer, server_reader) = tokio::io::duplex(64 * 1024);
    let (server_writer, client_reader) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(serve_stream(host.clone(), server_reader, server_writer));
    let mut client = LineClient { writer: client_writer, reader: BufReader::new(client_reader) };

    client
        .send(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": ee_agent_remote::REMOTE_PROTOCOL_VERSION }
        }))
        .await;
    let _ = client.recv_response(&json!(1)).await;
    client.send(json!({ "jsonrpc": "2.0", "method": "initialized" })).await;

    let started = std::time::Instant::now();
    client
        .send(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": { "agentId": "fake", "cwd": dir.path().display().to_string() }
        }))
        .await;
    let response = client.recv_response(&json!(2)).await;
    assert_eq!(response["error"]["code"], -32006, "{response}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "timeout must be bounded, took {:?}",
        started.elapsed()
    );

    drop(client.writer);
    let _ = tokio::time::timeout(TEST_TIMEOUT, server).await;
    fake.agent().join(TEST_TIMEOUT).await;
}

struct LineClient {
    writer: DuplexStream,
    reader: BufReader<DuplexStream>,
}

impl LineClient {
    async fn send(&mut self, value: Value) {
        let mut line = serde_json::to_vec(&value).expect("frame");
        line.push(b'\n');
        self.writer.write_all(&line).await.expect("write");
        self.writer.flush().await.expect("flush");
    }

    async fn recv_response(&mut self, id: &Value) -> Value {
        loop {
            let mut line = String::new();
            tokio::time::timeout(TEST_TIMEOUT, self.reader.read_line(&mut line))
                .await
                .expect("timed out")
                .expect("read");
            let frame: Value = serde_json::from_str(&line).expect("json");
            if frame.get("method").is_none() && frame.get("id") == Some(id) {
                return frame;
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_propagates_session_cancel_and_shutdown_leaves_no_zombie() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s-1" }))
        .wait_for("session/prompt")
        .wait_for("session/cancel")
        .close();
    let (host, fake) = host(script, dir.path(), TEST_TIMEOUT);
    let session_id = initialize(&host).await;
    host.dispatch(Method::TurnStart, Some(json!({ "sessionId": session_id, "text": "slow" })))
        .await
        .expect("turn/start");
    tokio::time::timeout(TEST_TIMEOUT, async {
        while fake.agent().requests_by_method("session/prompt").is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("prompt sent");

    let interrupted = host
        .dispatch(Method::TurnInterrupt, Some(json!({ "sessionId": session_id })))
        .await
        .expect("interrupt");
    assert_eq!(interrupted["interrupted"], true);
    tokio::time::timeout(TEST_TIMEOUT, async {
        while !fake.agent().log_contains("session/cancel") {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("ACP session/cancel reached the agent");

    host.shutdown().await;
    fake.agent().join(TEST_TIMEOUT).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_denies_pending_approvals() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("written.txt");
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s-1" }))
        .wait_for("session/prompt")
        .emit(wire::write_text_file("s-1", &target.display().to_string(), "data"))
        .wait_for_response(103)
        .respond(json!({ "stopReason": "end_turn" }));
    let fake = ScriptedFake::new(script);
    let mut config = RemoteHostConfig {
        allowed_roots: vec![dir.path().to_path_buf()],
        request_timeout: TEST_TIMEOUT,
        approval_timeout: Duration::from_secs(60),
        audit_path: Some(dir.path().join("audit.jsonl")),
        ..RemoteHostConfig::default()
    };
    config.agents.insert("fake".into(), AgentProcessConfig::new("unused"));
    config.fake_transports.insert("fake".into(), Arc::new(fake.clone()));
    let (host, _handler) =
        RemoteHost::headless_with_options(config, options()).expect("headless host");
    let mut subscription = host.bus().subscribe();
    host.start().expect("pump");
    let session_id = initialize(&host).await;
    host.dispatch(Method::TurnStart, Some(json!({ "sessionId": session_id, "text": "write" })))
        .await
        .expect("turn/start");

    wait_for_approval(&mut subscription).await;
    assert_eq!(host.approvals().pending_count(), 1);
    host.shutdown().await;
    assert_eq!(
        host.approvals().pending_count(),
        0,
        "shutdown denies pending approvals instead of leaking them"
    );
    let audit = std::fs::read_to_string(dir.path().join("audit.jsonl")).expect("audit");
    assert!(audit.contains("\"actor\":\"shutdown\""), "{audit}");
    fake.agent().join(TEST_TIMEOUT).await;
}

async fn wait_for_approval(subscription: &mut Subscription) {
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            let frame = subscription.receiver.recv().await.expect("frame");
            if frame["method"] == "permission/requestApproval" {
                return;
            }
        }
    })
    .await
    .expect("approval surfaced");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_racing_completion_produces_exactly_one_terminal_event() {
    let dir = tempfile::tempdir().expect("tempdir");
    for _ in 0..5 {
        let script = FakeAgentScript::new()
            .wait_for("initialize")
            .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
            .wait_for("session/new")
            .respond(json!({ "sessionId": "s-1" }))
            .wait_for("session/prompt")
            .respond(json!({ "stopReason": "end_turn" }));
        let (host, fake) = host(script, dir.path(), TEST_TIMEOUT);
        let mut subscription = host.bus().subscribe();
        let session_id = initialize(&host).await;
        host.dispatch(Method::TurnStart, Some(json!({ "sessionId": session_id, "text": "fast" })))
            .await
            .expect("turn/start");
        let _ =
            host.dispatch(Method::TurnInterrupt, Some(json!({ "sessionId": session_id }))).await;

        tokio::time::timeout(TEST_TIMEOUT, async {
            loop {
                let frame = subscription.receiver.recv().await.expect("frame");
                if frame["method"] == "turn/completed" {
                    return;
                }
            }
        })
        .await
        .expect("terminal event");
        tokio::time::sleep(Duration::from_millis(80)).await;
        let tail = host.bus().tail(&session_id, 0, 100);
        let terminals = tail
            .notifications
            .iter()
            .filter(|notification| notification.method == "turn/completed")
            .count();
        assert_eq!(terminals, 1, "exactly one terminal event per turn");

        host.shutdown().await;
        fake.agent().join(TEST_TIMEOUT).await;
    }
}
