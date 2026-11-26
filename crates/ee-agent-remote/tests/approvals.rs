//! Approval round-trip acceptance for R5.
//!
//! Uses a hermetic fake ACP agent plus the real headless handler so both
//! approval sources (ACP permission events and headless write gates) go
//! through the same registry.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ee_agent_host::fake::{FakeAgent, FakeAgentScript, FakeAgentTransport, wire};
use ee_agent_host::{
    AgentConnectionOptions, AgentProcessConfig, FakeTransportFactory, PermissionEffect,
    PermissionRule, PermissionRules, PermissionRuleset,
};
use ee_agent_remote::dispatch::CallerIdentity;
use ee_agent_remote::events::Subscription;
use ee_agent_remote::protocol::Method;
use ee_agent_remote::{RemoteHost, RemoteHostConfig};
use serde_json::{Value, json};
use tokio::sync::mpsc;

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

struct Harness {
    host: RemoteHost,
    fake: ScriptedFake,
    notifications: Subscription,
    audit_path: std::path::PathBuf,
}

async fn harness(
    script: FakeAgentScript,
    root: &Path,
    approval_timeout: Duration,
    rules: PermissionRules,
) -> Harness {
    let fake = ScriptedFake::new(script);
    let audit_path = root.join("audit/decisions.jsonl");
    let mut config = RemoteHostConfig {
        allowed_roots: vec![root.to_path_buf()],
        approval_timeout,
        request_timeout: TEST_TIMEOUT,
        permission_rules: Arc::new(rules),
        audit_path: Some(audit_path.clone()),
        ..RemoteHostConfig::default()
    };
    config.agents.insert("fake".into(), AgentProcessConfig::new("unused"));
    config.fake_transports.insert("fake".into(), Arc::new(fake.clone()));
    let options = AgentConnectionOptions {
        handshake_timeout: TEST_TIMEOUT,
        request_timeout: TEST_TIMEOUT,
        ..AgentConnectionOptions::default()
    };
    let (host, _handler) =
        RemoteHost::headless_with_options(config, options).expect("headless host");
    let notifications = host.bus().subscribe();
    host.start().expect("start pump");
    Harness { host, fake, notifications, audit_path }
}

async fn initialize(host: &RemoteHost) -> String {
    host.dispatch(
        Method::Initialize,
        Some(json!({ "protocolVersion": ee_agent_remote::REMOTE_PROTOCOL_VERSION })),
    )
    .await
    .expect("initialize");
    let session = host
        .dispatch(
            Method::SessionNew,
            Some(json!({ "agentId": "fake", "cwd": host.config().allowed_roots[0].display().to_string() })),
        )
        .await
        .expect("session/new");
    session["sessionId"].as_str().expect("session id").to_string()
}

async fn next_frame(receiver: &mut mpsc::Receiver<Value>) -> Value {
    tokio::time::timeout(TEST_TIMEOUT, receiver.recv())
        .await
        .expect("timed out waiting for a frame")
        .expect("frame channel closed")
}

async fn next_approval(receiver: &mut mpsc::Receiver<Value>) -> Value {
    loop {
        let frame = next_frame(receiver).await;
        if frame["method"] == ee_agent_remote::protocol::PERMISSION_REQUEST_APPROVAL {
            return frame;
        }
        if frame["method"] == ee_agent_remote::protocol::TURN_COMPLETED {
            panic!("turn completed before an approval request: {frame}");
        }
    }
}

async fn collect_turn_completed(receiver: &mut mpsc::Receiver<Value>) -> Value {
    loop {
        let frame = next_frame(receiver).await;
        if frame["method"] == ee_agent_remote::protocol::TURN_COMPLETED {
            return frame;
        }
    }
}

fn script_with_write(path: &Path) -> FakeAgentScript {
    FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s-1" }))
        .wait_for("session/prompt")
        .emit(wire::write_text_file("s-1", &path.display().to_string(), "data"))
        .wait_for_response(103)
        .respond(json!({ "stopReason": "end_turn" }))
}

fn acp_permission(value: Value) -> Value {
    json!({
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
            ],
            "_meta": value
        }
    })
}

#[tokio::test]
async fn headless_write_surfaces_one_approval_and_allow_lets_it_proceed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("written.txt");
    let mut harness = harness(
        script_with_write(&target),
        dir.path(),
        TEST_TIMEOUT,
        PermissionRules::new(PermissionRuleset::empty()),
    )
    .await;
    let session_id = initialize(&harness.host).await;
    harness
        .host
        .dispatch(Method::TurnStart, Some(json!({ "sessionId": session_id, "text": "write" })))
        .await
        .expect("turn/start");

    let approval = next_approval(&mut harness.notifications.receiver).await;
    assert_eq!(approval["params"]["toolCall"]["name"], "headless");
    assert!(approval["params"]["summary"].as_str().expect("summary").contains("write"));
    let request_id = approval["params"]["requestId"].as_u64().expect("request id");

    let pending =
        harness.host.dispatch(Method::PermissionsPending, Some(json!({}))).await.expect("pending");
    assert_eq!(pending["approvals"].as_array().expect("approvals").len(), 1);

    let caller = CallerIdentity::new("hermes");
    harness
        .host
        .dispatch_as(
            &caller,
            Method::PermissionsRespond,
            Some(json!({ "requestId": request_id, "decision": "allow" })),
        )
        .await
        .expect("allow");

    let completed = collect_turn_completed(&mut harness.notifications.receiver).await;
    assert_eq!(completed["params"]["status"], "completed");
    assert_eq!(std::fs::read_to_string(&target).expect("written file"), "data");

    // A second answer is a typed already_resolved error.
    let error = harness
        .host
        .dispatch_as(
            &caller,
            Method::PermissionsRespond,
            Some(json!({ "requestId": request_id, "decision": "deny" })),
        )
        .await
        .expect_err("late answer");
    assert_eq!(error.code().jsonrpc_code(), -32005);

    let audit = std::fs::read_to_string(&harness.audit_path).expect("audit log");
    assert!(audit.contains("hermes"), "manual decision is attributed");
    assert!(audit.contains("\"decision\":\"allow\""));

    harness.host.shutdown().await;
    harness.fake.agent().join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn deny_fails_the_write_closed_and_is_audited() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("denied.txt");
    let mut harness = harness(
        script_with_write(&target),
        dir.path(),
        TEST_TIMEOUT,
        PermissionRules::new(PermissionRuleset::empty()),
    )
    .await;
    let session_id = initialize(&harness.host).await;
    harness
        .host
        .dispatch(Method::TurnStart, Some(json!({ "sessionId": session_id, "text": "write" })))
        .await
        .expect("turn/start");

    let approval = next_approval(&mut harness.notifications.receiver).await;
    let server_id = approval["id"].as_u64().expect("server request id");
    let request_id = approval["params"]["requestId"].as_u64().expect("request id");
    assert_eq!(server_id, ee_agent_remote::permissions::SERVER_REQUEST_ID_BASE + request_id);

    // Answer through the server→client request path (not permissions/respond).
    let caller = CallerIdentity::new("web");
    harness
        .host
        .resolve_approval_response(server_id, Some(&json!({ "decision": "deny" })), &caller)
        .expect("deny");

    let completed = collect_turn_completed(&mut harness.notifications.receiver).await;
    assert_eq!(completed["params"]["status"], "completed");
    assert!(!target.exists(), "denied writes must not touch disk");

    let response = harness.fake.agent().response_with_id(103).expect("write response");
    assert!(response.get("error").is_some(), "agent observed a typed denial");

    harness.host.shutdown().await;
    harness.fake.agent().join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn an_unanswered_approval_expires_to_deny() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("timeout.txt");
    let mut harness = harness(
        script_with_write(&target),
        dir.path(),
        Duration::from_millis(80),
        PermissionRules::new(PermissionRuleset::empty()),
    )
    .await;
    let session_id = initialize(&harness.host).await;
    harness
        .host
        .dispatch(Method::TurnStart, Some(json!({ "sessionId": session_id, "text": "write" })))
        .await
        .expect("turn/start");

    let approval = next_approval(&mut harness.notifications.receiver).await;
    let request_id = approval["params"]["requestId"].as_u64().expect("request id");

    let completed = collect_turn_completed(&mut harness.notifications.receiver).await;
    assert_eq!(completed["params"]["status"], "completed");
    assert!(!target.exists(), "expiry denies the write");
    let response = harness.fake.agent().response_with_id(103).expect("write response");
    assert!(response.get("error").is_some());

    let error = harness
        .host
        .dispatch(
            Method::PermissionsRespond,
            Some(json!({ "requestId": request_id, "decision": "allow" })),
        )
        .await
        .expect_err("expired");
    assert_eq!(error.code().jsonrpc_code(), -32005);

    let audit = std::fs::read_to_string(&harness.audit_path).expect("audit log");
    assert!(audit.contains("\"actor\":\"timeout\""));

    harness.host.shutdown().await;
    harness.fake.agent().join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn acp_request_id_answers_the_pending_entry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s-1" }))
        .wait_for("session/prompt")
        .emit(acp_permission(json!({ "case": "harness" })))
        .wait_for_response(100)
        .respond(json!({ "stopReason": "end_turn" }));
    let mut harness =
        harness(script, dir.path(), TEST_TIMEOUT, PermissionRules::new(PermissionRuleset::empty()))
            .await;
    let session_id = initialize(&harness.host).await;
    harness
        .host
        .dispatch(Method::TurnStart, Some(json!({ "sessionId": session_id, "text": "run" })))
        .await
        .expect("turn/start");

    let approval = next_approval(&mut harness.notifications.receiver).await;
    assert_eq!(approval["params"]["requestId"], 1, "remote clients see the registry's internal id");

    // The agent broker numbers ACP requests from zero, one below the internal
    // id. Answering with the raw ACP id used to miss the entry and expire.
    harness
        .host
        .approvals()
        .respond_acp(
            ee_agent_host::PermissionRequestId(0),
            ee_agent_remote::protocol::PermissionDecision::Allow,
            None,
            "harness",
        )
        .expect("ACP id must resolve the pending entry");

    let completed = collect_turn_completed(&mut harness.notifications.receiver).await;
    assert_eq!(completed["params"]["status"], "completed");
    let response = harness.fake.agent().response_with_id(100).expect("permission response");
    assert_eq!(response["result"]["outcome"]["outcome"], "selected");
    assert_eq!(response["result"]["outcome"]["optionId"], "allow_once");

    let audit = std::fs::read_to_string(&harness.audit_path).expect("audit log");
    assert!(audit.contains("\"actor\":\"harness\""), "{audit}");
    assert!(audit.contains("\"decision\":\"allow\""), "{audit}");

    harness.host.shutdown().await;
    harness.fake.agent().join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn declarative_rules_answer_without_a_remote_request_and_are_audited() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rules = PermissionRules::new(PermissionRuleset::new(vec![PermissionRule::new(
        "bash",
        "*",
        PermissionEffect::Allow,
    )]));
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s-1" }))
        .wait_for("session/prompt")
        .emit(acp_permission(json!({ "case": "rules" })))
        .wait_for_response(100)
        .respond(json!({ "stopReason": "end_turn" }));
    let mut harness = harness(script, dir.path(), TEST_TIMEOUT, rules).await;
    let session_id = initialize(&harness.host).await;
    harness
        .host
        .dispatch(Method::TurnStart, Some(json!({ "sessionId": session_id, "text": "run" })))
        .await
        .expect("turn/start");

    let completed = collect_turn_completed(&mut harness.notifications.receiver).await;
    assert_eq!(completed["params"]["status"], "completed");

    let response = harness.fake.agent().response_with_id(100).expect("permission response");
    assert_eq!(response["result"]["outcome"]["outcome"], "selected");
    assert_eq!(response["result"]["outcome"]["optionId"], "allow_once");

    let pending =
        harness.host.dispatch(Method::PermissionsPending, Some(json!({}))).await.expect("pending");
    assert_eq!(
        pending["approvals"].as_array().expect("approvals").len(),
        0,
        "rules-based allow never surfaces a remote approval"
    );

    let audit = std::fs::read_to_string(&harness.audit_path).expect("audit log");
    assert!(audit.contains("\"actor\":\"rules\""));
    assert!(audit.contains("\"decision\":\"allow\""));

    harness.host.shutdown().await;
    harness.fake.agent().join(TEST_TIMEOUT).await;
}
