//! Redaction and audit acceptance for R12.
//!
//! One scripted turn carries every seeded secret shape through a session
//! update, a tool call, and an approval payload; no outbound frame, replay, or
//! tool result may contain them. The same run must leave an attributable audit
//! trail for turn start, the approval decision, and turn interrupt.

use std::sync::Arc;
use std::time::Duration;

use ee_agent_host::fake::{FakeAgent, FakeAgentScript, FakeAgentTransport, wire};
use ee_agent_host::{AgentConnectionOptions, AgentProcessConfig, FakeTransportFactory};
use ee_agent_remote::dispatch::CallerIdentity;
use ee_agent_remote::events::Subscription;
use ee_agent_remote::protocol::Method;
use ee_agent_remote::{RemoteHost, RemoteHostConfig};
use serde_json::{Value, json};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Whole value of a configured env secret: generic shape detection cannot
/// infer it, so the host must seed it from resolution.
const CONFIGURED_SECRET: &str = "env-secret-value";
const SECRETS: [&str; 8] = [
    "sk-live-abcdef123456",
    "ghp_1234567890abcdefghij",
    "xoxb-1234567890-abcdef",
    "hunter2",
    "query-secret-value",
    "alice-password",
    "secret://prod/api-token",
    "eyJhbGciOiJIUzI1NiJ9.payload.signature",
];

#[derive(Clone)]
struct ScriptedFake(Arc<std::sync::Mutex<Option<FakeAgent>>>);

impl FakeTransportFactory for ScriptedFake {
    fn build(&self) -> FakeAgentTransport {
        let (agent, transport) = FakeAgent::spawn(script());
        *self.0.lock().expect("fake handle poisoned") = Some(agent);
        transport
    }
}

fn hostile_text() -> String {
    concat!(
        "Authorization: Bearer sk-live-abcdef123456\n",
        "token ghp_1234567890abcdefghij\n",
        "slack xoxb-1234567890-abcdef\n",
        "password=hunter2\n",
        "callback https://example.test/?access_token=query-secret-value\n",
        "fetch https://alice:alice-password@example.test/data\n",
        "reference secret://prod/api-token\n",
        "jwt eyJhbGciOiJIUzI1NiJ9.payload.signature\n"
    )
    .to_string()
}

fn script() -> FakeAgentScript {
    FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s-1" }))
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "s-1",
            wire::agent_message_chunk(
                "m-1",
                &format!("{}configured env value: {CONFIGURED_SECRET}\n", hostile_text()),
            ),
        ))
        .emit(wire::session_update(
            "s-1",
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-1",
                "title": "curl with Authorization: Bearer sk-live-abcdef123456",
                "kind": "execute",
                "status": "in_progress",
                "rawInput": { "api_key": "sk-live-abcdef123456", "command": "curl -H 'Authorization: Bearer sk-live-abcdef123456'" }
            }),
        ))
        .emit(json!({
            "jsonrpc": "2.0",
            "id": 100,
            "method": "session/request_permission",
            "params": {
                "sessionId": "s-1",
                "toolCall": {
                    "toolCallId": "call-1",
                    "title": "token ghp_1234567890abcdefghij",
                    "name": "bash",
                    "kind": "execute",
                    "rawInput": { "api_key": "sk-live-abcdef123456" }
                },
                "options": [
                    { "optionId": "allow_once", "name": "Allow", "kind": "allow_once" },
                    { "optionId": "reject_once", "name": "Reject", "kind": "reject_once" }
                ]
            }
        }))
        .wait_for_response(100)
        .respond(json!({ "stopReason": "end_turn" }))
        .wait_for("session/prompt")
        .delay(500)
        .respond(json!({ "stopReason": "end_turn" }))
}

fn assert_clean(text: &str, label: &str) {
    for secret in SECRETS {
        assert!(!text.contains(secret), "{label} leaked {secret}: {text}");
    }
    assert!(!text.contains(CONFIGURED_SECRET), "{label} leaked the configured secret: {text}");
}

async fn next_frame(receiver: &mut Subscription) -> Value {
    tokio::time::timeout(TEST_TIMEOUT, receiver.receiver.recv())
        .await
        .expect("timed out")
        .expect("closed")
}

#[tokio::test]
async fn every_surface_redacts_and_the_session_is_audited() {
    let dir = tempfile::tempdir().expect("tempdir");
    let audit_path = dir.path().join("audit/decisions.jsonl");
    let fake = ScriptedFake(Arc::new(std::sync::Mutex::new(None)));
    let mut config = RemoteHostConfig {
        allowed_roots: vec![dir.path().to_path_buf()],
        audit_path: Some(audit_path.clone()),
        session_log_dir: Some(dir.path().join("history")),
        secret_values: vec![String::from(CONFIGURED_SECRET)],
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
    let mut subscription = host.bus().subscribe();
    host.start().expect("pump");
    let caller = CallerIdentity::new("hermes");

    host.dispatch_as(
        &caller,
        Method::Initialize,
        Some(json!({ "protocolVersion": ee_agent_remote::REMOTE_PROTOCOL_VERSION })),
    )
    .await
    .expect("initialize");
    let session = host
        .dispatch_as(
            &caller,
            Method::SessionNew,
            Some(json!({ "cwd": dir.path().display().to_string() })),
        )
        .await
        .expect("session/new");
    assert_clean(&session.to_string(), "session/new response");
    let session_id = session["sessionId"].as_str().expect("session id").to_string();

    host.dispatch_as(
        &caller,
        Method::TurnStart,
        Some(json!({ "sessionId": session_id, "text": "run" })),
    )
    .await
    .expect("turn/start");

    let mut frames = Vec::new();
    loop {
        let frame = next_frame(&mut subscription).await;
        assert_clean(&frame.to_string(), "outbound notification");
        frames.push(frame.clone());
        if frame["method"] == "permission/requestApproval" {
            let request_id = frame["params"]["requestId"].as_u64().expect("request id");
            host.dispatch_as(
                &caller,
                Method::PermissionsRespond,
                Some(json!({ "requestId": request_id, "decision": "allow" })),
            )
            .await
            .expect("approval");
        }
        if frame["method"] == "turn/completed" {
            break;
        }
    }
    assert!(
        frames.iter().any(|frame| frame["method"] == "permission/requestApproval"),
        "approval was surfaced"
    );

    // Cursor replay is stored already-redacted.
    let replay = host
        .dispatch_as(
            &caller,
            Method::HistoryTail,
            Some(json!({ "sessionId": session_id, "fromCursor": 0 })),
        )
        .await
        .expect("replay");
    assert_clean(&replay.to_string(), "history/tail");

    // A second turn is interrupted to exercise the interrupt audit path.
    let fake_agent = fake.0.lock().expect("fake").as_ref().cloned().expect("spawned");
    host.dispatch_as(
        &caller,
        Method::TurnStart,
        Some(json!({ "sessionId": session_id, "text": "second" })),
    )
    .await
    .expect("second turn");
    tokio::time::timeout(TEST_TIMEOUT, async {
        while fake_agent.requests_by_method("session/prompt").len() < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("second prompt sent");
    let interrupted = host
        .dispatch_as(&caller, Method::TurnInterrupt, Some(json!({ "sessionId": session_id })))
        .await
        .expect("interrupt");
    assert_eq!(interrupted["interrupted"], true);

    let audit = std::fs::read_to_string(&audit_path).expect("audit log");
    assert_clean(&audit, "audit log");
    assert!(audit.contains("\"event\":\"turn_start\""), "{audit}");
    assert!(audit.contains("\"actor\":\"hermes\""), "{audit}");
    assert!(audit.contains("\"event\":\"permission_decision\""), "{audit}");
    assert!(audit.contains("\"event\":\"turn_interrupt\""), "{audit}");
    assert!(audit.contains("\"mode\":\"ask\""), "{audit}");

    // Durable history is written already redacted, including configured env
    // values that no shape detector could recognize.
    let history_dir = dir.path().join("history");
    let history = std::fs::read_dir(&history_dir)
        .expect("history dir")
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .collect::<String>();
    assert!(history.contains("assistantTail"), "{history}");
    assert_clean(&history, "durable history");

    host.shutdown().await;
    fake_agent.join(TEST_TIMEOUT).await;
}

#[test]
fn mcp_result_caps_apply_after_redaction() {
    let response = ee_agent_remote::mcp::complete_json(json!({
        "text": hostile_text(),
        "api_key": "sk-live-abcdef123456",
    }));
    let rmcp::model::CallToolResponse::Complete(result) = response else {
        panic!("expected a complete tool result");
    };
    let text = serde_json::to_string(&result).expect("serialize");
    assert_clean(&text, "mcp result");
}

#[test]
fn redaction_happens_before_ring_buffer_insertion() {
    use ee_agent_remote::events::EventBus;
    let bus = EventBus::new(8, 8);
    bus.publish(
        "s-1",
        "session/update",
        json!({ "text": "Authorization: Bearer sk-live-abcdef123456" }),
    );
    let tail = bus.tail("s-1", 0, 10);
    assert_clean(&serde_json::to_string(&tail).expect("serialize"), "buffered replay");
}
