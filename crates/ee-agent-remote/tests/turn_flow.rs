//! End-to-end remote turn flow against the in-process fake ACP agent.
//!
//! Covers R4 acceptance: initialize → session/new → turn/start → ordered
//! updates → turn/completed, the typed busy-turn error, interrupt, and full
//! cursor replay after a simulated reconnect.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ee_agent_host::fake::{FakeAgent, FakeAgentScript, FakeAgentTransport, wire};
use ee_agent_host::{AgentConnectionOptions, AgentProcessConfig, FakeTransportFactory};
use ee_agent_remote::events::Subscription;
use ee_agent_remote::protocol::Method;
use ee_agent_remote::{RemoteHost, RemoteHostConfig};
use serde_json::{Value, json};
use tokio::sync::mpsc;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct ScriptedFake {
    script: FakeAgentScript,
    agent: Arc<Mutex<Option<FakeAgent>>>,
}

impl ScriptedFake {
    fn new(script: FakeAgentScript) -> Self {
        Self { script, agent: Arc::new(Mutex::new(None)) }
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

struct TestHost {
    host: RemoteHost,
    fake: ScriptedFake,
    notifications: Subscription,
}

async fn spawn_host(script: FakeAgentScript, cwd: &std::path::Path) -> TestHost {
    let fake = ScriptedFake::new(script);
    let mut config = RemoteHostConfig {
        allowed_roots: vec![cwd.to_path_buf()],
        request_timeout: TEST_TIMEOUT,
        approval_timeout: TEST_TIMEOUT,
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
    let subscription = host.bus().subscribe();
    host.start().expect("start pump");
    TestHost { host, fake, notifications: subscription }
}

async fn initialize(host: &RemoteHost) -> Value {
    host.dispatch(
        Method::Initialize,
        Some(json!({ "protocolVersion": ee_agent_remote::REMOTE_PROTOCOL_VERSION })),
    )
    .await
    .expect("initialize")
}

async fn new_session(host: &RemoteHost, cwd: &std::path::Path) -> String {
    let result = host
        .dispatch(
            Method::SessionNew,
            Some(json!({ "agentId": "fake", "cwd": cwd.display().to_string() })),
        )
        .await
        .expect("session/new");
    result["sessionId"].as_str().expect("session id").to_string()
}

async fn next_frame(receiver: &mut mpsc::Receiver<Value>) -> Value {
    tokio::time::timeout(TEST_TIMEOUT, receiver.recv())
        .await
        .expect("timed out waiting for a notification")
        .expect("notification channel closed")
}

async fn collect_until_turn_completed(receiver: &mut mpsc::Receiver<Value>) -> Vec<Value> {
    let mut frames = Vec::new();
    loop {
        let frame = next_frame(receiver).await;
        let method = frame["method"].as_str().unwrap_or_default().to_string();
        frames.push(frame);
        if method == ee_agent_remote::protocol::TURN_COMPLETED {
            return frames;
        }
    }
}

fn cursor_frames(frames: &[Value]) -> Vec<(u64, String)> {
    frames
        .iter()
        .filter_map(|frame| {
            let cursor = frame["params"]["cursor"].as_u64()?;
            (cursor > 0).then(|| (cursor, frame["method"].as_str().unwrap_or_default().to_string()))
        })
        .collect()
}

async fn await_fake_request(fake: &FakeAgent, method: &str) {
    tokio::time::timeout(TEST_TIMEOUT, async {
        while fake.requests_by_method(method).is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {method}"));
}

fn streaming_script() -> FakeAgentScript {
    FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s-1" }))
        .wait_for("session/prompt")
        .emit(wire::session_update("s-1", wire::agent_message_chunk("m-1", "hello")))
        .emit(wire::session_update("s-1", wire::agent_message_chunk("m-1", " world")))
        .respond(json!({ "stopReason": "end_turn" }))
}

#[tokio::test]
async fn turn_streams_ordered_updates_then_completion_and_replays_from_cursor_zero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut harness = spawn_host(streaming_script(), dir.path()).await;
    initialize(&harness.host).await;
    let session_id = new_session(&harness.host, dir.path()).await;

    let started = harness
        .host
        .dispatch(Method::TurnStart, Some(json!({ "sessionId": session_id, "text": "hi" })))
        .await
        .expect("turn/start");
    assert_eq!(started["sessionId"], session_id);

    let frames = collect_until_turn_completed(&mut harness.notifications.receiver).await;
    let updates = frames
        .iter()
        .filter(|frame| frame["method"] == ee_agent_remote::protocol::SESSION_UPDATE)
        .count();
    assert_eq!(updates, 2, "both streamed chunks observed");
    assert_eq!(frames.last().expect("terminal frame")["params"]["status"], "completed");
    let cursors: Vec<u64> = cursor_frames(&frames).into_iter().map(|(cursor, _)| cursor).collect();
    let mut sorted = cursors.clone();
    sorted.sort_unstable();
    assert_eq!(cursors, sorted, "notification order matches cursor order");

    // Simulated reconnect: a fresh client replays from cursor 0 and must see
    // exactly the same buffered sequence, in the same order.
    let replay = harness
        .host
        .dispatch(Method::HistoryTail, Some(json!({ "sessionId": session_id, "fromCursor": 0 })))
        .await
        .expect("history/tail");
    let replayed: Vec<(u64, String)> = replay["notifications"]
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
    assert_eq!(cursor_frames(&frames), replayed);

    harness.host.shutdown().await;
    harness.fake.agent().join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn a_second_turn_while_active_is_typed_already_active_and_interrupt_cancels() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s-1" }))
        .wait_for("session/prompt")
        .delay(500)
        .respond(json!({ "stopReason": "end_turn" }));
    let mut harness = spawn_host(script, dir.path()).await;
    initialize(&harness.host).await;
    let session_id = new_session(&harness.host, dir.path()).await;

    harness
        .host
        .dispatch(Method::TurnStart, Some(json!({ "sessionId": session_id, "text": "hi" })))
        .await
        .expect("turn/start");
    await_fake_request(&harness.fake.agent(), "session/prompt").await;

    let error = harness
        .host
        .dispatch(Method::TurnStart, Some(json!({ "sessionId": session_id, "text": "again" })))
        .await
        .expect_err("busy session");
    assert_eq!(error.code().jsonrpc_code(), -32004);
    assert!(error.message().contains("turn/interrupt"));

    let interrupted = harness
        .host
        .dispatch(Method::TurnInterrupt, Some(json!({ "sessionId": session_id })))
        .await
        .expect("interrupt");
    assert_eq!(interrupted["interrupted"], true);

    let frames = collect_until_turn_completed(&mut harness.notifications.receiver).await;
    assert_eq!(frames.last().expect("terminal frame")["params"]["status"], "cancelled");

    // A second interrupt is a typed no-op, not an error.
    let again = harness
        .host
        .dispatch(Method::TurnInterrupt, Some(json!({ "sessionId": session_id })))
        .await
        .expect("second interrupt");
    assert_eq!(again["interrupted"], false);

    harness.host.shutdown().await;
    harness.fake.agent().join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn session_list_reports_loaded_sessions_and_configured_agents() {
    let dir = tempfile::tempdir().expect("tempdir");
    let harness = spawn_host(streaming_script(), dir.path()).await;
    initialize(&harness.host).await;
    let session_id = new_session(&harness.host, dir.path()).await;

    let listed =
        harness.host.dispatch(Method::SessionList, Some(json!({}))).await.expect("session/list");
    assert_eq!(listed["sessions"][0]["sessionId"], session_id);
    assert_eq!(listed["sessions"][0]["agentId"], "fake");
    assert_eq!(listed["agents"][0]["agentId"], "fake");
    assert_eq!(listed["agents"][0]["default"], true);

    harness.host.shutdown().await;
    harness.fake.agent().join(TEST_TIMEOUT).await;
}
