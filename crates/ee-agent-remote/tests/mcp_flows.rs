//! Hermetic MCP flow tests over a duplex transport.
//!
//! Covers R7 (initialize → tools/list → tools/call, fail-closed arguments) and
//! R8 (full fake-agent turn through send/poll, typed busy/turn errors, durable
//! history on and off).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ee_agent_host::fake::{FakeAgent, FakeAgentScript, FakeAgentTransport, wire};
use ee_agent_host::{AgentConnectionOptions, AgentProcessConfig, FakeTransportFactory};
use ee_agent_remote::mcp::RemoteMcpServer;
use ee_agent_remote::{RemoteHost, RemoteHostConfig, RemotePermissionMode};
use rmcp::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, Implementation,
    InitializeRequestParams, JsonObject, ProtocolVersion,
};
use rmcp::service::{ClientLifecycleMode, RoleClient, RoleServer, RunningService};
use serde_json::{Value, json};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, Default)]
struct TestClientHandler;

impl ClientHandler for TestClientHandler {
    fn get_info(&self) -> InitializeRequestParams {
        InitializeRequestParams::new(
            ClientCapabilities::builder().build(),
            Implementation::new("mcp-test", "0.1"),
        )
        .with_protocol_version(ProtocolVersion::V_2026_07_28)
    }
}

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

fn arguments(value: Value) -> JsonObject {
    value.as_object().expect("arguments object").clone()
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

struct Harness {
    client: RunningService<RoleClient, TestClientHandler>,
    server: RunningService<RoleServer, RemoteMcpServer>,
    host: RemoteHost,
    fake: ScriptedFake,
}

async fn connect(script: FakeAgentScript, cwd: &std::path::Path, history: bool) -> Harness {
    connect_with(script, cwd, history, RemotePermissionMode::Ask, REQUEST_TIMEOUT).await
}

async fn connect_with(
    script: FakeAgentScript,
    cwd: &std::path::Path,
    history: bool,
    permission_mode: RemotePermissionMode,
    approval_timeout: Duration,
) -> Harness {
    let fake = ScriptedFake::new(script);
    let audit_path = cwd.join("audit/decisions.jsonl");
    let mut config = RemoteHostConfig {
        allowed_roots: vec![cwd.to_path_buf()],
        request_timeout: REQUEST_TIMEOUT,
        approval_timeout,
        permission_mode,
        audit_path: Some(audit_path),
        session_log_dir: history.then(|| cwd.join("history")),
        ..RemoteHostConfig::default()
    };
    config.agents.insert("fake".into(), AgentProcessConfig::new("unused"));
    config.fake_transports.insert("fake".into(), Arc::new(fake.clone()));
    let host = RemoteHost::with_options(
        config,
        Arc::new(ee_agent_host::DenyAllHandler),
        AgentConnectionOptions {
            handshake_timeout: REQUEST_TIMEOUT,
            request_timeout: REQUEST_TIMEOUT,
            ..AgentConnectionOptions::default()
        },
    );
    host.start().expect("pump");
    let (client_side, server_side) = tokio::io::duplex(64 * 1024);
    let server_task =
        tokio::spawn(rmcp::serve_server(RemoteMcpServer::new(host.clone()), server_side));
    let client = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        rmcp::serve_client_with_lifecycle(
            TestClientHandler,
            client_side,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        ),
    )
    .await
    .expect("client handshake timed out")
    .expect("client handshake failed");
    let server = tokio::time::timeout(HANDSHAKE_TIMEOUT, server_task)
        .await
        .expect("server handshake timed out")
        .expect("server task panicked")
        .expect("server handshake failed");
    Harness { client, server, host, fake }
}

fn shutdown(harness: &Harness) {
    harness.client.cancellation_token().cancel();
    harness.server.cancellation_token().cancel();
}

async fn try_call(
    client: &RunningService<RoleClient, TestClientHandler>,
    name: &str,
    args: Option<Value>,
) -> Result<CallToolResult, rmcp::ServiceError> {
    let mut request = CallToolRequestParams::new(name.to_string());
    if let Some(args) = args {
        request = request.with_arguments(arguments(args));
    }
    tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(request)).await.expect("call timed out")
}

async fn call(
    client: &RunningService<RoleClient, TestClientHandler>,
    name: &str,
    args: Option<Value>,
) -> CallToolResult {
    try_call(client, name, args).await.expect("call failed")
}

fn structured(result: &CallToolResult) -> &Value {
    result
        .structured_content
        .as_ref()
        .unwrap_or_else(|| panic!("expected structured content: {result:?}"))
}

fn error_code(result: &CallToolResult) -> String {
    let text = serde_json::to_value(result).expect("serialize");
    text.to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_list_and_status_round_trip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let harness = connect(streaming_script(), dir.path(), false).await;
    let tools = tokio::time::timeout(REQUEST_TIMEOUT, harness.client.list_tools(None))
        .await
        .expect("list timed out")
        .expect("list failed");
    let mut names: Vec<String> = tools.tools.iter().map(|tool| tool.name.to_string()).collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "ee_permission_respond",
            "ee_permissions_pending",
            "ee_session_history",
            "ee_session_new",
            "ee_sessions_list",
            "ee_status",
            "ee_task_send",
            "ee_turn_interrupt",
            "ee_updates_poll",
        ]
    );

    let result = call(&harness.client, "ee_status", None).await;
    assert_eq!(structured(&result)["agents"][0]["agentId"], "fake");
    assert_eq!(structured(&result)["permissionMode"], "ask");
    assert_eq!(structured(&result)["history"]["enabled"], false);

    shutdown(&harness);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_arguments_fail_closed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let harness = connect(streaming_script(), dir.path(), false).await;
    let rejected = tokio::time::timeout(
        REQUEST_TIMEOUT,
        harness.client.call_tool(
            CallToolRequestParams::new("ee_status")
                .with_arguments(arguments(json!({ "unexpected": true }))),
        ),
    )
    .await
    .expect("call timed out");
    assert!(rejected.is_err(), "extra arguments must be rejected");

    let unknown = call(&harness.client, "ee_missing", None).await;
    assert_eq!(unknown.is_error, Some(true));

    let missing = try_call(&harness.client, "ee_task_send", Some(json!({}))).await;
    assert!(missing.is_err(), "missing required arguments fail validation");

    shutdown(&harness);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_send_poll_and_completion_work_end_to_end() {
    let dir = tempfile::tempdir().expect("tempdir");
    let harness = connect(streaming_script(), dir.path(), false).await;

    let created = call(
        &harness.client,
        "ee_session_new",
        Some(json!({ "agentId": "fake", "cwd": dir.path().display().to_string() })),
    )
    .await;
    let session_id = structured(&created)["sessionId"].as_str().expect("session id").to_string();

    let sent = call(
        &harness.client,
        "ee_task_send",
        Some(json!({ "sessionId": session_id, "text": "hi" })),
    )
    .await;
    let turn_id = structured(&sent)["turnId"].as_u64().expect("turn id");
    assert!(turn_id >= 1);

    let mut cursor = 0_u64;
    let mut seen_cursors = Vec::new();
    let mut saw_completion = false;
    for _ in 0..200 {
        let polled = call(
            &harness.client,
            "ee_updates_poll",
            Some(json!({ "sessionId": session_id, "cursor": cursor })),
        )
        .await;
        let structured = structured(&polled).clone();
        for update in structured["updates"].as_array().expect("updates") {
            let update_cursor = update["cursor"].as_u64().expect("cursor");
            assert!(update_cursor > cursor, "cursors must move forward");
            seen_cursors.push(update_cursor);
            if update["method"] == "turn/completed" {
                saw_completion = true;
            }
        }
        cursor = structured["cursor"].as_u64().expect("cursor");
        if saw_completion && structured["turnRunning"] == false {
            assert!(
                structured["textTail"].as_str().expect("tail").contains("hello"),
                "assistant tail: {structured}"
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(saw_completion, "turn completion observed through polling");
    let mut sorted = seen_cursors.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted, seen_cursors, "no missed or duplicated updates");

    let idle = call(
        &harness.client,
        "ee_updates_poll",
        Some(json!({ "sessionId": session_id, "cursor": cursor })),
    )
    .await;
    assert_eq!(
        structured(&idle)["updates"].as_array().expect("updates").len(),
        0,
        "an idle session returns an empty delta, not an error"
    );

    let listed = call(&harness.client, "ee_sessions_list", None).await;
    assert_eq!(structured(&listed)["sessions"][0]["sessionId"], session_id);

    let missing =
        call(&harness.client, "ee_session_history", Some(json!({ "sessionId": session_id }))).await;
    assert_eq!(missing.is_error, Some(true));
    assert!(error_code(&missing).contains("history_disabled"));

    harness.host.shutdown().await;
    harness.fake.agent().join(REQUEST_TIMEOUT).await;
    shutdown(&harness);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_history_returns_completed_turns_when_enabled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let harness = connect(streaming_script(), dir.path(), true).await;
    let created = call(
        &harness.client,
        "ee_session_new",
        Some(json!({ "cwd": dir.path().display().to_string() })),
    )
    .await;
    let session_id = structured(&created)["sessionId"].as_str().expect("session id").to_string();
    call(&harness.client, "ee_task_send", Some(json!({ "sessionId": session_id, "text": "hi" })))
        .await;
    tokio::time::timeout(REQUEST_TIMEOUT, async {
        loop {
            let polled = call(
                &harness.client,
                "ee_updates_poll",
                Some(json!({ "sessionId": session_id, "cursor": 0 })),
            )
            .await;
            let structured = structured(&polled).clone();
            if structured["turnRunning"] == false
                && structured["updates"]
                    .as_array()
                    .is_some_and(|updates| updates.iter().any(|u| u["method"] == "turn/completed"))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("turn completion");

    let history =
        call(&harness.client, "ee_session_history", Some(json!({ "sessionId": session_id }))).await;
    let entries = structured(&history)["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["status"], "completed");
    assert!(entries[0]["assistantTail"].as_str().expect("tail").contains("hello"));

    harness.host.shutdown().await;
    harness.fake.agent().join(REQUEST_TIMEOUT).await;
    shutdown(&harness);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_turn_is_typed_and_interrupt_ends_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s-1" }))
        .wait_for("session/prompt")
        .delay(500)
        .respond(json!({ "stopReason": "end_turn" }));
    let harness = connect(script, dir.path(), false).await;
    let created = call(
        &harness.client,
        "ee_session_new",
        Some(json!({ "cwd": dir.path().display().to_string() })),
    )
    .await;
    let session_id = structured(&created)["sessionId"].as_str().expect("session id").to_string();
    call(&harness.client, "ee_task_send", Some(json!({ "sessionId": session_id, "text": "slow" })))
        .await;
    tokio::time::timeout(REQUEST_TIMEOUT, async {
        while harness.fake.agent().requests_by_method("session/prompt").is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("prompt sent");

    let busy = call(
        &harness.client,
        "ee_task_send",
        Some(json!({ "sessionId": session_id, "text": "again" })),
    )
    .await;
    assert_eq!(busy.is_error, Some(true));
    assert!(error_code(&busy).contains("already_active"));

    let interrupted =
        call(&harness.client, "ee_turn_interrupt", Some(json!({ "sessionId": session_id }))).await;
    assert_eq!(structured(&interrupted)["interrupted"], true);

    // A stale session id is a typed tool error, never a panic.
    let stale =
        call(&harness.client, "ee_updates_poll", Some(json!({ "sessionId": "missing" }))).await;
    assert_eq!(stale.is_error, Some(true));
    assert!(error_code(&stale).contains("not_found"));

    harness.host.shutdown().await;
    harness.fake.agent().join(REQUEST_TIMEOUT).await;
    shutdown(&harness);
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

async fn wait_for_completion(harness: &Harness, session_id: &str) -> Value {
    tokio::time::timeout(REQUEST_TIMEOUT, async {
        loop {
            let polled = call(
                &harness.client,
                "ee_updates_poll",
                Some(json!({ "sessionId": session_id, "cursor": 0 })),
            )
            .await;
            let structured = structured(&polled).clone();
            if structured["turnRunning"] == false
                && structured["updates"].as_array().is_some_and(|updates| {
                    updates.iter().any(|update| update["method"] == "turn/completed")
                })
            {
                return structured;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("turn completion")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ask_mode_surfaces_pending_and_answers_end_to_end() {
    let dir = tempfile::tempdir().expect("tempdir");
    let harness = connect_with(
        permission_script(),
        dir.path(),
        false,
        RemotePermissionMode::Ask,
        REQUEST_TIMEOUT,
    )
    .await;
    let created = call(
        &harness.client,
        "ee_session_new",
        Some(json!({ "cwd": dir.path().display().to_string() })),
    )
    .await;
    let session_id = structured(&created)["sessionId"].as_str().expect("session").to_string();
    call(&harness.client, "ee_task_send", Some(json!({ "sessionId": session_id, "text": "run" })))
        .await;

    let request_id = tokio::time::timeout(REQUEST_TIMEOUT, async {
        loop {
            let pending = call(&harness.client, "ee_permissions_pending", None).await;
            let approvals = structured(&pending)["approvals"].as_array().expect("approvals");
            if let Some(approval) = approvals.first() {
                return approval["requestId"].as_u64().expect("request id");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("pending approval");

    let answered = call(
        &harness.client,
        "ee_permission_respond",
        Some(json!({ "requestId": request_id, "decision": "allow" })),
    )
    .await;
    assert_eq!(structured(&answered)["resolved"], true);

    wait_for_completion(&harness, &session_id).await;
    let response = harness.fake.agent().response_with_id(100).expect("permission response");
    assert_eq!(response["result"]["outcome"]["outcome"], "selected");
    assert_eq!(response["result"]["outcome"]["optionId"], "allow_once");

    let late = call(
        &harness.client,
        "ee_permission_respond",
        Some(json!({ "requestId": request_id, "decision": "deny" })),
    )
    .await;
    assert_eq!(late.is_error, Some(true));
    assert!(error_code(&late).contains("already_resolved"));

    let audit = std::fs::read_to_string(dir.path().join("audit/decisions.jsonl")).expect("audit");
    assert!(audit.contains("\"actor\":\"mcp\""));
    assert!(audit.contains("\"mode\":\"ask\""));

    harness.host.shutdown().await;
    harness.fake.agent().join(REQUEST_TIMEOUT).await;
    shutdown(&harness);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rules_mode_hides_pending_and_expires_to_deny() {
    let dir = tempfile::tempdir().expect("tempdir");
    let harness = connect_with(
        permission_script(),
        dir.path(),
        false,
        RemotePermissionMode::Rules,
        Duration::from_millis(80),
    )
    .await;
    let created = call(
        &harness.client,
        "ee_session_new",
        Some(json!({ "cwd": dir.path().display().to_string() })),
    )
    .await;
    let session_id = structured(&created)["sessionId"].as_str().expect("session").to_string();
    call(&harness.client, "ee_task_send", Some(json!({ "sessionId": session_id, "text": "run" })))
        .await;

    tokio::time::sleep(Duration::from_millis(20)).await;
    let pending = call(&harness.client, "ee_permissions_pending", None).await;
    assert_eq!(
        structured(&pending)["approvals"].as_array().expect("approvals").len(),
        0,
        "rules mode never surfaces answerable approvals"
    );
    let answer = call(
        &harness.client,
        "ee_permission_respond",
        Some(json!({ "requestId": 1, "decision": "allow" })),
    )
    .await;
    assert_eq!(answer.is_error, Some(true));

    wait_for_completion(&harness, &session_id).await;
    let response = harness.fake.agent().response_with_id(100).expect("permission response");
    assert_eq!(response["result"]["outcome"]["optionId"], "reject_once");

    let audit = std::fs::read_to_string(dir.path().join("audit/decisions.jsonl")).expect("audit");
    assert!(audit.contains("\"actor\":\"timeout\""));
    assert!(audit.contains("\"mode\":\"rules\""));

    harness.host.shutdown().await;
    harness.fake.agent().join(REQUEST_TIMEOUT).await;
    shutdown(&harness);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deny_mode_refuses_without_any_pending_entry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let harness = connect_with(
        permission_script(),
        dir.path(),
        false,
        RemotePermissionMode::Deny,
        REQUEST_TIMEOUT,
    )
    .await;
    let created = call(
        &harness.client,
        "ee_session_new",
        Some(json!({ "cwd": dir.path().display().to_string() })),
    )
    .await;
    let session_id = structured(&created)["sessionId"].as_str().expect("session").to_string();
    call(&harness.client, "ee_task_send", Some(json!({ "sessionId": session_id, "text": "run" })))
        .await;

    wait_for_completion(&harness, &session_id).await;
    let pending = call(&harness.client, "ee_permissions_pending", None).await;
    assert_eq!(structured(&pending)["approvals"].as_array().expect("approvals").len(), 0);
    let response = harness.fake.agent().response_with_id(100).expect("permission response");
    assert_eq!(response["result"]["outcome"]["optionId"], "reject_once");

    let audit = std::fs::read_to_string(dir.path().join("audit/decisions.jsonl")).expect("audit");
    assert!(audit.contains("\"mode\":\"deny\""));

    harness.host.shutdown().await;
    harness.fake.agent().join(REQUEST_TIMEOUT).await;
    shutdown(&harness);
}
