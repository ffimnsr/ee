//! End-to-end host flows against the in-process fake ACP agent.
//!
//! These tests exercise the real connection stack (SDK transport, handshake,
//! driver, reducer, permission broker) over the scripted fake transport; no
//! external binaries are spawned.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ee_agent_host::fake::{FakeAgent, FakeAgentScript, wire};
use ee_agent_host::reducer::MessageKind;
use ee_agent_host::{
    AgentConnection, AgentConnectionOptions, AgentError, AgentEvent, AgentManager,
    AgentManagerConfig, AgentProcessConfig, DenyAllHandler, HandlerCapabilities, RecordingHandler,
};
use ee_agent_protocol::{
    ContentBlock, ProtocolVersion, RequestPermissionOutcome, SelectedPermissionOutcome, SessionId,
    SessionModeId, StopReason, TextContent,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

/// A connected host plus its event stream.
struct TestHost {
    connection: AgentConnection,
    events: mpsc::UnboundedReceiver<AgentEvent>,
}

async fn spawn_host(
    script: FakeAgentScript,
    handler: Arc<dyn ee_agent_host::ClientRequestHandler>,
) -> (FakeAgent, TestHost) {
    let (fake, transport) = FakeAgent::spawn(script);
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let options =
        AgentConnectionOptions { handshake_timeout: TEST_TIMEOUT, request_timeout: TEST_TIMEOUT };
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

async fn next_event(rx: &mut mpsc::UnboundedReceiver<AgentEvent>) -> AgentEvent {
    tokio::time::timeout(TEST_TIMEOUT, rx.recv())
        .await
        .expect("timed out waiting for host event")
        .expect("event channel closed")
}

impl TestHost {
    /// Closes the connection so the fake's driver can finish (the transport
    /// stays open while the connection lives).
    async fn close(&self) {
        self.connection.close().await;
    }
}

/// Polls the fake's log for the host's response to the request with `id`
/// (the responder task writes asynchronously).
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

/// initialize + session/new happy-path responses.
fn base_script() -> FakeAgentScript {
    FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
}

async fn ready_connection(fake: &FakeAgent, host: &TestHost) -> AgentConnection {
    let connection = host.connection.clone();
    connection.wait_ready().await.expect("handshake succeeds");
    assert!(fake.log_contains("\"method\":\"initialize\""));
    connection
}

#[tokio::test]
async fn happy_path_streams_updates_and_completes_turn() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "Hel")))
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "lo")))
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call_1",
                "title": "Run tests",
            }),
        ))
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "usage_update",
                "used": 10,
                "size": 100,
            }),
        ))
        .respond(json!({ "stopReason": "end_turn" }));

    let (fake, mut host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let thread = connection
        .new_session(vec![PathBuf::from("/work"), PathBuf::from("/extra")], Vec::new())
        .await
        .expect("session/new succeeds");

    let response = thread
        .send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))])
        .await
        .expect("prompt completes");
    assert_eq!(response.stop_reason, StopReason::EndTurn);

    // Reduced state: optimistic user message + one assistant message with
    // two merged blocks; tool call and usage recorded.
    let snapshot = thread.snapshot();
    assert_eq!(snapshot.messages.len(), 2);
    assert_eq!(snapshot.messages[0].kind, MessageKind::User);
    assert_eq!(snapshot.messages[1].kind, MessageKind::Assistant);
    assert_eq!(snapshot.messages[1].message_id.as_deref(), Some("m1"));
    assert_eq!(snapshot.messages[1].blocks.len(), 2);
    assert_eq!(snapshot.tool_calls["call_1"].title, "Run tests");
    assert_eq!(snapshot.usage.as_ref().map(|usage| usage.used), Some(10));

    // Wire contract: method names, protocol version, cwd + additional dirs.
    assert_eq!(fake.requests_by_method("initialize").len(), 1);
    let initialize = &fake.requests_by_method("initialize")[0];
    assert_eq!(initialize["params"]["protocolVersion"], 1);
    assert_eq!(initialize["params"]["clientInfo"]["name"], "ee");
    let session_new = &fake.requests_by_method("session/new")[0];
    assert_eq!(session_new["params"]["cwd"], "/work");
    assert_eq!(session_new["params"]["additionalDirectories"], json!(["/extra"]));
    let prompt = &fake.requests_by_method("session/prompt")[0];
    assert_eq!(prompt["params"]["sessionId"], "s1");
    assert_eq!(prompt["params"]["prompt"][0]["text"], "hi");

    // Deterministic event stream.
    assert!(matches!(
        next_event(&mut host.events).await,
        AgentEvent::ConnectionStateChanged {
            state: ee_agent_host::AgentConnectionState::Ready { .. },
            ..
        }
    ));
    assert!(matches!(next_event(&mut host.events).await, AgentEvent::ThreadCreated { .. }));
    assert!(matches!(next_event(&mut host.events).await, AgentEvent::TurnStarted { .. }));
    assert!(matches!(next_event(&mut host.events).await, AgentEvent::SessionUpdate { .. }));
    assert!(matches!(next_event(&mut host.events).await, AgentEvent::SessionUpdate { .. }));
    assert!(matches!(next_event(&mut host.events).await, AgentEvent::SessionUpdate { .. }));
    assert!(matches!(next_event(&mut host.events).await, AgentEvent::SessionUpdate { .. }));
    assert!(matches!(
        next_event(&mut host.events).await,
        AgentEvent::TurnCompleted { stop_reason: StopReason::EndTurn, .. }
    ));

    connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn unsupported_protocol_version_fails_closed() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 2, "agentCapabilities": {} }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;

    let error = host.connection.wait_ready().await.unwrap_err();
    assert!(matches!(
        error,
        AgentError::UnsupportedProtocolVersion { ref agent_id, .. } if agent_id == "fake"
    ));
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn rejected_initialize_fails_closed() {
    let script = FakeAgentScript::new().wait_for("initialize").respond_error(-32602, "bad request");
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;

    let error = host.connection.wait_ready().await.unwrap_err();
    assert!(matches!(error, AgentError::Rpc(_)));
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn malformed_json_from_agent_gets_parse_error_response() {
    let script = base_script()
        .emit_raw("{not valid json")
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }));

    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let thread = connection.new_session(vec![PathBuf::from("/work")], Vec::new()).await.unwrap();
    thread
        .send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))])
        .await
        .expect("turn still works after malformed line");

    // The host answered the malformed line with a JSON-RPC parse error and
    // kept the connection alive.
    assert!(
        fake.log_contains("\"code\":-32700"),
        "expected parse-error response in fake log: {:?}",
        fake.log()
    );
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn oversized_agent_message_fails_the_connection_without_panic() {
    // Phase 7 resource limit: a line beyond `MAX_ACP_MESSAGE_BYTES` must
    // fail the connection as a typed transport error, never be parsed.
    let huge = "x".repeat(ee_agent_host::fake::MAX_ACP_MESSAGE_BYTES + 1);
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        // Give the client time to consume the initialize response before the
        // oversized line arrives (the cap error ends the incoming stream).
        .delay(150)
        .emit_raw(huge);
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;

    // The cap error surfaces as a connection failure (no panic, no hang).
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            match host.connection.state() {
                ee_agent_host::AgentConnectionState::Failed(_)
                | ee_agent_host::AgentConnectionState::Closed(_) => break,
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .expect("connection must fail within the test window");
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn agent_eof_mid_turn_resolves_prompt_with_typed_error() {
    let script = base_script().wait_for("session/prompt").close();
    let (fake, mut host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread = connection.new_session(vec![PathBuf::from("/work")], Vec::new()).await.unwrap();

    let error =
        thread.send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))]).await.unwrap_err();
    assert!(
        matches!(error, AgentError::ConnectionClosed { .. } | AgentError::Rpc(_)),
        "unexpected error: {error:?}"
    );
    assert!(!thread.is_turn_running());

    // The thread is reported closed; no prompt can start on a dead thread.
    let mut saw_thread_closed = false;
    while let Ok(Some(event)) = tokio::time::timeout(TEST_TIMEOUT, host.events.recv()).await {
        if matches!(event, AgentEvent::ThreadClosed { .. }) {
            saw_thread_closed = true;
            break;
        }
    }
    assert!(saw_thread_closed);
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn cancel_sends_session_cancel_and_resolves_prompt() {
    let script = base_script()
        .wait_for("session/prompt")
        // Agent never answers: the turn is cancelled locally.
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "thinking...")));
    let (fake, mut host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread = connection.new_session(vec![PathBuf::from("/work")], Vec::new()).await.unwrap();

    let prompt_thread = thread.clone();
    let prompt = tokio::spawn(async move {
        prompt_thread.send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))]).await
    });
    // Wait until the agent saw the prompt request.
    tokio::time::timeout(TEST_TIMEOUT, async {
        while !fake.log_contains("\"method\":\"session/prompt\"") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("prompt request observed");

    thread.cancel().await.expect("cancel succeeds");

    let error = prompt.await.expect("prompt task").unwrap_err();
    assert!(matches!(error, AgentError::Cancelled));
    assert!(!thread.is_turn_running());

    // `session/cancel` went out on the wire.
    assert!(
        fake.log_contains("\"method\":\"session/cancel\""),
        "expected session/cancel in fake log: {:?}",
        fake.log()
    );

    // Exactly one terminal event for the turn.
    let mut terminal_events = Vec::new();
    while let Ok(Some(event)) = tokio::time::timeout(TEST_TIMEOUT, host.events.recv()).await {
        if matches!(
            event,
            AgentEvent::TurnCompleted { .. }
                | AgentEvent::TurnCancelled { .. }
                | AgentEvent::TurnFailed { .. }
        ) {
            terminal_events.push(event);
            break;
        }
    }
    assert!(matches!(terminal_events[0], AgentEvent::TurnCancelled { .. }));
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn permission_request_flows_through_broker_and_back() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "checking")))
        .emit(wire::request_permission(
            "s1",
            "call_1",
            "Run tests",
            json!([
                { "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" },
                { "optionId": "deny", "name": "Deny", "kind": "reject_once" }
            ]),
        ))
        .respond(json!({ "stopReason": "end_turn" }));

    let (fake, mut host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread = connection.new_session(vec![PathBuf::from("/work")], Vec::new()).await.unwrap();

    let prompt_thread = thread.clone();
    let prompt = tokio::spawn(async move {
        prompt_thread.send_prompt(vec![ContentBlock::Text(TextContent::new("run tests"))]).await
    });

    // The host surfaces the permission request to the UI.
    let request_id = loop {
        match next_event(&mut host.events).await {
            AgentEvent::PermissionRequested { session_id, request } => {
                assert_eq!(session_id, SessionId::new("s1"));
                assert_eq!(request.tool_call.fields.title.as_deref(), Some("Run tests"));
                assert_eq!(request.options.len(), 2);
                break request.request_id;
            }
            _ => continue,
        }
    };

    // The UI answers; the agent sees the selected outcome on the wire.
    let outcome = RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("allow_once"));
    assert!(thread.respond_permission(request_id, outcome.clone()));
    // Duplicate response is ignored.
    assert!(!thread.respond_permission(request_id, outcome));

    let response = prompt.await.expect("prompt task").expect("turn completes");
    assert_eq!(response.stop_reason, StopReason::EndTurn);

    // The fake agent recorded the host's permission response (request id 100
    // in the wire helpers).
    let permission_response = await_response(&fake, 100).await;
    assert_eq!(permission_response["result"]["outcome"]["outcome"], "selected");
    assert_eq!(permission_response["result"]["outcome"]["optionId"], "allow_once");

    // A PermissionResolved event was emitted.
    let mut resolved = false;
    while let Ok(Some(event)) = tokio::time::timeout(TEST_TIMEOUT, host.events.recv()).await {
        if matches!(event, AgentEvent::PermissionResolved { .. }) {
            resolved = true;
            break;
        }
    }
    assert!(resolved);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn cancel_resolves_pending_permissions_as_cancelled() {
    let script = base_script().wait_for("session/prompt").emit(wire::request_permission(
        "s1",
        "call_1",
        "Run tests",
        json!([{ "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" }]),
    ));
    let (fake, mut host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread = connection.new_session(vec![PathBuf::from("/work")], Vec::new()).await.unwrap();

    let prompt_thread = thread.clone();
    let prompt = tokio::spawn(async move {
        prompt_thread.send_prompt(vec![ContentBlock::Text(TextContent::new("go"))]).await
    });

    loop {
        match next_event(&mut host.events).await {
            AgentEvent::PermissionRequested { .. } => break,
            _ => continue,
        }
    }

    thread.cancel().await.expect("cancel succeeds");
    assert!(prompt.await.expect("prompt task").is_err());
    assert_eq!(connection.permission_broker().pending_count(), 0);

    // The agent saw a Cancelled outcome for the permission request.
    let permission_response = await_response(&fake, 100).await;
    assert_eq!(permission_response["result"]["outcome"]["outcome"], "cancelled");
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn fs_and_terminal_requests_dispatch_to_handler_with_capability_gate() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::read_text_file("s1", "/work/Cargo.toml"))
        .emit(wire::terminal_create("s1", "cargo test"))
        .respond(json!({ "stopReason": "end_turn" }));

    let handler = RecordingHandler::new(HandlerCapabilities::all());
    let (fake, host) = spawn_host(script, Arc::new(handler.clone())).await;
    let connection = ready_connection(&fake, &host).await;
    let thread = connection.new_session(vec![PathBuf::from("/work")], Vec::new()).await.unwrap();

    thread
        .send_prompt(vec![ContentBlock::Text(TextContent::new("go"))])
        .await
        .expect("turn completes");

    // Both requests reached the handler, in order.
    let seen = handler.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].method(), "fs/read_text_file");
    assert_eq!(seen[1].method(), "terminal/create");
    // The recording handler denies: the agent saw denial responses.
    await_response(&fake, 101).await;
    await_response(&fake, 102).await;
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn unadvertised_capabilities_are_rejected_before_the_handler() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::read_text_file("s1", "/work/Cargo.toml"))
        .respond(json!({ "stopReason": "end_turn" }));

    // RecordingHandler with no capabilities advertises nothing; the host
    // must reject the request without invoking a handler at all.
    let handler = RecordingHandler::new(HandlerCapabilities::none());
    let (fake, host) = spawn_host(script, Arc::new(handler.clone())).await;
    let connection = ready_connection(&fake, &host).await;
    let thread = connection.new_session(vec![PathBuf::from("/work")], Vec::new()).await.unwrap();
    thread
        .send_prompt(vec![ContentBlock::Text(TextContent::new("go"))])
        .await
        .expect("turn completes");

    assert!(handler.seen().is_empty(), "handler must not be invoked");
    let response = await_response(&fake, 101).await;
    assert_eq!(response["error"]["code"], -32601);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn authenticate_and_logout_round_trip() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": {},
            "authMethods": [{ "id": "device", "name": "Device auth" }]
        }))
        .wait_for("authenticate")
        .respond(json!({}))
        .wait_for("logout")
        .respond(json!({}));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let auth_methods = connection.auth_methods();
    assert_eq!(auth_methods.len(), 1);
    assert_eq!(auth_methods[0].id().0.as_ref(), "device");

    connection.authenticate(auth_methods[0].id().clone()).await.expect("authenticate");
    connection.logout().await.expect("logout");

    assert_eq!(fake.requests_by_method("authenticate").len(), 1);
    assert_eq!(fake.requests_by_method("logout").len(), 1);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn load_session_requires_advertised_capability() {
    let script = FakeAgentScript::new().wait_for("initialize").respond(json!({
        "protocolVersion": 1,
        "agentCapabilities": { "loadSession": false }
    }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = host.connection.clone();
    connection.wait_ready().await.expect("ready");

    let error = connection.load_session(SessionId::new("old")).await.unwrap_err();
    assert!(matches!(
        error,
        AgentError::CapabilityUnsupported { ref method } if method == "session/load"
    ));
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn set_mode_requires_advertised_modes() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({
            "sessionId": "s1",
            "modes": {
                "currentModeId": "ask",
                "availableModes": [{ "id": "ask", "name": "Ask" }]
            }
        }))
        .wait_for("session/set_mode")
        .respond(json!({}));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread = connection.new_session(vec![PathBuf::from("/work")], Vec::new()).await.unwrap();

    let modes = thread.advertised_modes().expect("modes advertised");
    assert_eq!(modes.available_modes.len(), 1);

    thread.set_mode(SessionModeId::new("ask")).await.expect("set_mode");
    assert_eq!(fake.requests_by_method("session/set_mode").len(), 1);

    // Unknown mode id is rejected locally.
    let error = thread.set_mode(SessionModeId::new("code")).await.unwrap_err();
    assert!(matches!(error, AgentError::InvalidParams(_)));
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn set_mode_without_advertised_modes_fails_closed() {
    let script = base_script();
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread = connection.new_session(vec![PathBuf::from("/work")], Vec::new()).await.unwrap();

    let error = thread.set_mode(SessionModeId::new("ask")).await.unwrap_err();
    assert!(matches!(
        error,
        AgentError::CapabilityUnsupported { ref method } if method == "session/set_mode"
    ));
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn close_kills_connection_and_resolves_pending_work() {
    let script = base_script().wait_for("session/prompt");
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread = connection.new_session(vec![PathBuf::from("/work")], Vec::new()).await.unwrap();

    let prompt_thread = thread.clone();
    let prompt = tokio::spawn(async move {
        prompt_thread.send_prompt(vec![ContentBlock::Text(TextContent::new("go"))]).await
    });
    tokio::time::timeout(TEST_TIMEOUT, async {
        while !fake.log_contains("\"method\":\"session/prompt\"") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("prompt observed");

    connection.close().await;

    let error = prompt.await.expect("prompt task").unwrap_err();
    assert!(matches!(error, AgentError::ConnectionClosed { .. } | AgentError::Cancelled));
    assert!(!thread.is_turn_running());
    assert_eq!(connection.permission_broker().pending_count(), 0);
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn session_new_without_roots_fails_closed() {
    let script = base_script();
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let error = connection.new_session(Vec::new(), Vec::new()).await.unwrap_err();
    assert!(matches!(error, AgentError::InvalidParams(_)));

    let error =
        connection.new_session(vec![PathBuf::from("relative")], Vec::new()).await.unwrap_err();
    assert!(matches!(error, AgentError::InvalidParams(_)));
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn request_timeout_produces_typed_error() {
    // The agent never answers session/new: the request timeout fires.
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new");
    let (fake, transport) = FakeAgent::spawn(script);
    let (events_tx, _events_rx) = mpsc::unbounded_channel();
    let connection = AgentConnection::connect_with_transport(
        "fake".into(),
        Arc::new(DenyAllHandler),
        events_tx,
        AgentConnectionOptions {
            handshake_timeout: TEST_TIMEOUT,
            request_timeout: Duration::from_millis(100),
        },
        transport,
    )
    .expect("connect");

    connection.wait_ready().await.expect("handshake ok");
    let error = connection.new_session(vec![PathBuf::from("/work")], Vec::new()).await.unwrap_err();
    assert!(matches!(error, AgentError::RequestTimeout { ref method } if method == "session/new"));
    connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn manager_resolves_default_agent_and_lists_ids() {
    let mut config = AgentManagerConfig::default();
    config.agents.insert("primary".into(), AgentProcessConfig::new("unused"));
    config.agents.insert("secondary".into(), AgentProcessConfig::new("unused"));
    let (events_tx, _events_rx) = mpsc::unbounded_channel();
    let manager = AgentManager::new(config, Arc::new(DenyAllHandler), events_tx);

    assert_eq!(manager.agent_ids().len(), 2);
    assert!(manager.has_agent("primary"));
    assert!(!manager.has_agent("missing"));
    assert_eq!(manager.resolve_default_agent(Some("secondary")), Some("secondary".into()));
    assert_eq!(manager.resolve_default_agent(Some("missing")), None);
    assert_eq!(manager.resolve_default_agent(None), None); // ambiguous
    assert_eq!(manager.live_connection_count(), 0);
    let _ = ProtocolVersion::V1;
}
