//! End-to-end host flows against the in-process fake ACP agent.
//!
//! These tests exercise the real connection stack (SDK transport, handshake,
//! driver, reducer, permission broker) over the scripted fake transport; no
//! external binaries are spawned.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ee_agent_host::fake::{FakeAgent, FakeAgentScript, wire};
use ee_agent_host::reducer::MessageKind;
use ee_agent_host::{
    AgentConnection, AgentConnectionOptions, AgentError, AgentEvent, AgentManager,
    AgentManagerConfig, AgentProcessConfig, ClientRequest, ClientRequestHandler,
    ClientRequestResponse, ClientRequestResult, DenyAllHandler, HandlerCapabilities,
    RecordingHandler, ThreadCloseReason,
};
use ee_agent_protocol::{
    AudioContent, ContentBlock, CreateElicitationResponse, CreateTerminalResponse,
    ElicitationAcceptAction, ElicitationAction, EmbeddedResource, EmbeddedResourceResource,
    ImageContent, KillTerminalResponse, ProtocolVersion, ReadTextFileResponse,
    ReleaseTerminalResponse, RequestPermissionOutcome, ResourceLink, SelectedPermissionOutcome,
    SessionConfigId, SessionConfigOptionValue, SessionId, SessionModeId, StopReason,
    TerminalExitStatus, TerminalId, TerminalOutputResponse, TextContent, TextResourceContents,
    WaitForTerminalExitResponse, WriteTextFileResponse,
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
    let options = AgentConnectionOptions {
        handshake_timeout: TEST_TIMEOUT,
        request_timeout: TEST_TIMEOUT,
        ..Default::default()
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

#[derive(Debug, Clone)]
struct ScriptedHandler {
    capabilities: HandlerCapabilities,
    seen: Arc<Mutex<Vec<ClientRequest>>>,
}

impl ScriptedHandler {
    fn new(capabilities: HandlerCapabilities) -> Self {
        Self { capabilities, seen: Arc::new(Mutex::new(Vec::new())) }
    }

    fn seen(&self) -> Vec<ClientRequest> {
        self.seen.lock().expect("scripted handler poisoned").clone()
    }
}

impl ClientRequestHandler for ScriptedHandler {
    fn capabilities(&self) -> HandlerCapabilities {
        self.capabilities
    }

    fn handle(
        &self,
        request: ClientRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ClientRequestResult> + Send + '_>> {
        Box::pin(async move {
            self.seen.lock().expect("scripted handler poisoned").push(request.clone());
            match request {
                ClientRequest::ReadTextFile(_) => Ok(ClientRequestResponse::ReadTextFile(
                    ReadTextFileResponse::new("file contents"),
                )),
                ClientRequest::WriteTextFile(_) => {
                    Ok(ClientRequestResponse::WriteTextFile(WriteTextFileResponse::new()))
                }
                ClientRequest::CreateTerminal(_) => Ok(ClientRequestResponse::CreateTerminal(
                    CreateTerminalResponse::new(TerminalId::new("term-scripted")),
                )),
                ClientRequest::TerminalOutput(_) => Ok(ClientRequestResponse::TerminalOutput(
                    TerminalOutputResponse::new("stdout", false),
                )),
                ClientRequest::WaitForTerminalExit(_) => Ok(
                    ClientRequestResponse::WaitForTerminalExit(WaitForTerminalExitResponse::new(
                        TerminalExitStatus::new().exit_code(Some(0)),
                    )),
                ),
                ClientRequest::KillTerminal(_) => {
                    Ok(ClientRequestResponse::KillTerminal(KillTerminalResponse::new()))
                }
                ClientRequest::ReleaseTerminal(_) => {
                    Ok(ClientRequestResponse::ReleaseTerminal(ReleaseTerminalResponse::new()))
                }
                ClientRequest::CreateElicitation(request) => {
                    let action = match request.mode {
                        ee_agent_protocol::ElicitationMode::Form(_) => {
                            ElicitationAction::Accept(ElicitationAcceptAction::new().content(
                                std::collections::BTreeMap::from([(
                                    String::from("name"),
                                    ee_agent_protocol::ElicitationContentValue::String(
                                        String::from("ed"),
                                    ),
                                )]),
                            ))
                        }
                        ee_agent_protocol::ElicitationMode::Url(_) => {
                            ElicitationAction::Accept(ElicitationAcceptAction::new())
                        }
                        _ => {
                            return Err(AgentError::invalid_params("unsupported elicitation mode"));
                        }
                    };
                    Ok(ClientRequestResponse::CreateElicitation(CreateElicitationResponse::new(
                        action,
                    )))
                }
                other => {
                    Err(AgentError::HandlerError(format!("unhandled request {}", other.method())))
                }
            }
        })
    }
}

fn assert_method_not_found(response: &Value) {
    assert_eq!(response["error"]["code"], -32601, "response: {response}");
}

fn assert_invalid_params(response: &Value) {
    assert_eq!(response["error"]["code"], -32602, "response: {response}");
}

fn cancel_request(id: i64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "$/cancel_request",
        "params": { "requestId": id }
    })
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

async fn initialize_request_for_capabilities(capabilities: HandlerCapabilities) -> Value {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }));
    let handler = Arc::new(ScriptedHandler::new(capabilities));
    let (fake, host) = spawn_host(script, handler).await;
    host.connection.wait_ready().await.expect("handshake succeeds");
    let initialize = fake.requests_by_method("initialize").pop().expect("initialize sent");
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
    initialize
}

#[tokio::test]
async fn initialize_does_not_advertise_custom_standard_looking_capabilities() {
    let initialize = initialize_request_for_capabilities(HandlerCapabilities::all()).await;
    let capabilities =
        initialize["params"]["clientCapabilities"].as_object().expect("clientCapabilities object");
    assert!(
        !capabilities.contains_key("proxyDiscovery"),
        "unexpected custom capability: {initialize}"
    );
    assert!(!capabilities.contains_key("ee"), "unexpected custom capability: {initialize}");
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
        .new_session(vec![PathBuf::from("/work"), PathBuf::from("/extra")], Vec::new(), None)
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
async fn turn_completed_carries_elapsed_time_and_reported_tokens() {
    let script = base_script().wait_for("session/prompt").respond(json!({
        "stopReason": "end_turn",
        "usage": {
            "totalTokens": 8431,
            "inputTokens": 6120,
            "outputTokens": 2311,
        }
    }));
    let (fake, mut host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    thread
        .send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))])
        .await
        .expect("prompt completes");

    let metrics = loop {
        match next_event(&mut host.events).await {
            AgentEvent::TurnCompleted { metrics, .. } => break metrics,
            AgentEvent::TurnStarted { .. }
            | AgentEvent::SessionUpdate { .. }
            | AgentEvent::ConnectionStateChanged { .. }
            | AgentEvent::ThreadCreated { .. } => continue,
            other => panic!("unexpected event: {other:?}"),
        }
    };
    assert!(!metrics.elapsed.is_zero(), "elapsed must be measured");
    let tokens = metrics.tokens.expect("reported usage attached");
    assert_eq!(tokens.total_tokens, 8431);
    assert_eq!(tokens.input_tokens, 6120);
    assert_eq!(tokens.output_tokens, 2311);

    connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn cancelled_turn_carries_elapsed_time_and_no_tokens() {
    let script = base_script()
        .wait_for("session/prompt")
        // Agent never answers: the turn is cancelled locally.
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "thinking...")));
    let (fake, mut host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let prompt_thread = thread.clone();
    let prompt = tokio::spawn(async move {
        prompt_thread.send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))]).await
    });
    tokio::time::timeout(TEST_TIMEOUT, async {
        while !fake.log_contains("\"method\":\"session/prompt\"") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("prompt request observed");
    thread.cancel().await.expect("cancel succeeds");
    assert!(matches!(prompt.await.expect("prompt task joins"), Err(AgentError::Cancelled)));

    let metrics = loop {
        match next_event(&mut host.events).await {
            AgentEvent::TurnCancelled { metrics, .. } => break metrics,
            AgentEvent::TurnStarted { .. }
            | AgentEvent::SessionUpdate { .. }
            | AgentEvent::ConnectionStateChanged { .. }
            | AgentEvent::ThreadCreated { .. } => continue,
            other => panic!("unexpected event: {other:?}"),
        }
    };
    assert!(!metrics.elapsed.is_zero(), "elapsed must be measured");
    assert_eq!(metrics.tokens, None, "cancelled turns report no tokens");

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

    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();
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
async fn unknown_custom_request_returns_method_not_found() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(json!({
            "jsonrpc": "2.0",
            "id": 104,
            "method": "_ee/unknown_request",
            "params": { "trace": "diag-only" }
        }))
        .respond(json!({ "stopReason": "end_turn" }));

    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    thread
        .send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))])
        .await
        .expect("prompt still completes");

    let response = await_response(&fake, 104).await;
    assert_method_not_found(&response);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn unknown_custom_notification_is_ignored() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(json!({
            "jsonrpc": "2.0",
            "method": "_ee/unknown_notification",
            "params": { "traceparent": "00-abc-def-01" }
        }))
        .respond(json!({ "stopReason": "end_turn" }));

    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let response = thread
        .send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))])
        .await
        .expect("prompt still completes after unknown notification");
    assert_eq!(response.stop_reason, StopReason::EndTurn);
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
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

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
async fn recoverable_error_surfaces_as_paused_event_with_structured_info() {
    let script = base_script().wait_for("session/prompt").respond_error_with_data(
        -32603,
        "recoverable turn interruption: paused after 300s",
        json!({
            "recoverable": {
                "fault": "deadline",
                "detail": "paused after 300s",
                "cause": null,
                "safe_resume": true,
                "retry_after": null,
                "checkpoint_id": "s-1-0000000003",
                "completed_tool_calls": 4,
                "resumed_count": 0,
            }
        }),
    );
    let (fake, mut host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let error =
        thread.send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))]).await.unwrap_err();
    assert!(matches!(error, AgentError::Rpc(_)), "wire error stays an Rpc error: {error:?}");
    assert!(!thread.is_turn_running(), "the thread stays alive after a pause");

    let paused = loop {
        match next_event(&mut host.events).await {
            AgentEvent::TurnPausedRecoverable { recoverable, .. } => break *recoverable,
            AgentEvent::TurnStarted { .. }
            | AgentEvent::SessionUpdate { .. }
            | AgentEvent::ConnectionStateChanged { .. }
            | AgentEvent::ThreadCreated { .. } => continue,
            other => panic!("unexpected event: {other:?}"),
        }
    };
    assert_eq!(paused.fault, "deadline");
    assert_eq!(paused.detail, "paused after 300s");
    assert!(paused.safe_resume);
    assert_eq!(paused.checkpoint_id.as_deref(), Some("s-1-0000000003"));
    assert_eq!(paused.completed_tool_calls, 4);
    assert_eq!(paused.resumed_count, 0);

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn plain_errors_still_surface_as_turn_failed() {
    let script = base_script().wait_for("session/prompt").respond_error(-32603, "backend exploded");
    let (fake, mut host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let error =
        thread.send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))]).await.unwrap_err();
    assert!(matches!(error, AgentError::Rpc(_)));

    let mut saw_failed = false;
    while let Ok(Some(event)) = tokio::time::timeout(TEST_TIMEOUT, host.events.recv()).await {
        if matches!(event, AgentEvent::TurnFailed { .. }) {
            saw_failed = true;
            break;
        }
    }
    assert!(saw_failed, "plain errors keep the TurnFailed path");
    host.close().await;
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
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

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
    assert!(
        fake.log_contains("\"method\":\"$/cancel_request\""),
        "expected $/cancel_request in fake log: {:?}",
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
async fn late_session_updates_after_cancel_still_reduce_until_prompt_response_arrives() {
    let script = base_script()
        .wait_for("session/prompt")
        .delay(100)
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "late")));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let prompt_thread = thread.clone();
    let prompt = tokio::spawn(async move {
        prompt_thread.send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))]).await
    });
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

    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            let snapshot = thread.snapshot();
            if snapshot.messages.iter().any(|message| {
                message.kind == MessageKind::Assistant
                    && message.blocks.iter().any(|block| {
                        matches!(
                            block,
                            ContentBlock::Text(text) if text.text == "late"
                        )
                    })
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("late update reduced after cancel");

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
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

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
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

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
async fn fs_and_terminal_optional_requests_route_and_serialize_expected_shapes() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::read_text_file("s1", "/work/Cargo.toml"))
        .emit(wire::write_text_file("s1", "/work/Cargo.toml", "new"))
        .emit(wire::terminal_create("s1", "cargo test"))
        .emit(wire::terminal_output("s1", "term-1"))
        .emit(wire::terminal_wait_for_exit("s1", "term-1"))
        .emit(wire::terminal_kill("s1", "term-1"))
        .emit(wire::terminal_release("s1", "term-1"))
        .respond(json!({ "stopReason": "end_turn" }));

    let handler = ScriptedHandler::new(HandlerCapabilities {
        fs_read: true,
        fs_write: true,
        terminal: true,
        ..HandlerCapabilities::none()
    });
    let (fake, host) = spawn_host(script, Arc::new(handler.clone())).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    thread
        .send_prompt(vec![ContentBlock::Text(TextContent::new("go"))])
        .await
        .expect("turn completes");

    let seen = handler.seen();
    assert_eq!(
        seen.iter().map(ClientRequest::method).collect::<Vec<_>>(),
        vec![
            "fs/read_text_file",
            "fs/write_text_file",
            "terminal/create",
            "terminal/output",
            "terminal/wait_for_exit",
            "terminal/kill",
            "terminal/release",
        ]
    );

    assert_eq!(await_response(&fake, 101).await["result"]["content"], "file contents");
    assert!(await_response(&fake, 103).await.get("result").is_some());
    assert_eq!(await_response(&fake, 102).await["result"]["terminalId"], "term-scripted");
    assert_eq!(await_response(&fake, 104).await["result"]["output"], "stdout");
    assert_eq!(await_response(&fake, 104).await["result"]["truncated"], false);
    assert_eq!(await_response(&fake, 105).await["result"]["exitCode"], 0);
    assert!(await_response(&fake, 106).await.get("result").is_some());
    assert!(await_response(&fake, 107).await.get("result").is_some());
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn unadvertised_fs_write_is_rejected_before_handler_invocation() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::write_text_file("s1", "/work/Cargo.toml", "new"))
        .respond(json!({ "stopReason": "end_turn" }));
    let handler = RecordingHandler::new(HandlerCapabilities::none());
    let (fake, host) = spawn_host(script, Arc::new(handler.clone())).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();
    thread.send_prompt(vec![ContentBlock::Text(TextContent::new("go"))]).await.unwrap();

    assert!(handler.seen().is_empty());
    assert_method_not_found(&await_response(&fake, 103).await);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn unadvertised_terminal_requests_are_rejected_before_handler_invocation() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::terminal_output("s1", "term-1"))
        .emit(wire::terminal_wait_for_exit("s1", "term-1"))
        .emit(wire::terminal_kill("s1", "term-1"))
        .emit(wire::terminal_release("s1", "term-1"))
        .respond(json!({ "stopReason": "end_turn" }));
    let handler = RecordingHandler::new(HandlerCapabilities::none());
    let (fake, host) = spawn_host(script, Arc::new(handler.clone())).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();
    thread.send_prompt(vec![ContentBlock::Text(TextContent::new("go"))]).await.unwrap();

    assert!(handler.seen().is_empty());
    assert_method_not_found(&await_response(&fake, 104).await);
    assert_method_not_found(&await_response(&fake, 105).await);
    assert_method_not_found(&await_response(&fake, 106).await);
    assert_method_not_found(&await_response(&fake, 107).await);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn elicitation_form_routes_when_form_capability_is_advertised() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::elicitation_form(
            "s1",
            json!({
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }),
            "fill the form",
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let handler = ScriptedHandler::new(HandlerCapabilities {
        elicitation_form: true,
        ..HandlerCapabilities::none()
    });
    let (fake, host) = spawn_host(script, Arc::new(handler.clone())).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();
    thread.send_prompt(vec![ContentBlock::Text(TextContent::new("go"))]).await.unwrap();

    let seen = handler.seen();
    assert_eq!(seen.len(), 1);
    assert!(matches!(seen[0], ClientRequest::CreateElicitation(_)));
    let response = await_response(&fake, 108).await;
    assert_eq!(response["result"]["action"], "accept");
    assert_eq!(response["result"]["content"]["name"], "ed");
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn elicitation_url_routes_when_url_capability_is_advertised() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::elicitation_url("s1", "el-1", "https://example.com/authorize", "authorize"))
        .respond(json!({ "stopReason": "end_turn" }));
    let handler = ScriptedHandler::new(HandlerCapabilities {
        elicitation_url: true,
        ..HandlerCapabilities::none()
    });
    let (fake, host) = spawn_host(script, Arc::new(handler.clone())).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();
    thread.send_prompt(vec![ContentBlock::Text(TextContent::new("go"))]).await.unwrap();

    let seen = handler.seen();
    assert_eq!(seen.len(), 1);
    assert!(matches!(seen[0], ClientRequest::CreateElicitation(_)));
    let response = await_response(&fake, 109).await;
    assert_eq!(response["result"]["action"], "accept");
    assert!(response["result"].get("content").is_none());
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn unadvertised_elicitation_modes_are_rejected_before_handler_invocation() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::elicitation_form("s1", json!({ "type": "object", "properties": {} }), "fill"))
        .emit(wire::elicitation_url("s1", "el-1", "https://example.com/authorize", "open"))
        .respond(json!({ "stopReason": "end_turn" }));
    let handler = RecordingHandler::new(HandlerCapabilities::none());
    let (fake, host) = spawn_host(script, Arc::new(handler.clone())).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();
    thread.send_prompt(vec![ContentBlock::Text(TextContent::new("go"))]).await.unwrap();

    assert!(handler.seen().is_empty());
    assert_invalid_params(&await_response(&fake, 108).await);
    assert_invalid_params(&await_response(&fake, 109).await);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[derive(Debug, Clone)]
struct DelayedUrlHandler {
    seen: Arc<Mutex<Vec<ClientRequest>>>,
    delay: Duration,
}

#[derive(Debug, Clone)]
struct DelayedReadHandler {
    seen: Arc<Mutex<Vec<ClientRequest>>>,
    delay: Duration,
}

impl DelayedUrlHandler {
    fn new(delay: Duration) -> Self {
        Self { seen: Arc::new(Mutex::new(Vec::new())), delay }
    }
}

impl DelayedReadHandler {
    fn new(delay: Duration) -> Self {
        Self { seen: Arc::new(Mutex::new(Vec::new())), delay }
    }
}

impl ClientRequestHandler for DelayedUrlHandler {
    fn capabilities(&self) -> HandlerCapabilities {
        HandlerCapabilities { elicitation_url: true, ..HandlerCapabilities::none() }
    }

    fn handle(
        &self,
        request: ClientRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ClientRequestResult> + Send + '_>> {
        Box::pin(async move {
            self.seen.lock().expect("delayed url handler poisoned").push(request.clone());
            match request {
                ClientRequest::CreateElicitation(_) => {
                    tokio::time::sleep(self.delay).await;
                    Ok(ClientRequestResponse::CreateElicitation(CreateElicitationResponse::new(
                        ElicitationAction::Accept(ElicitationAcceptAction::new()),
                    )))
                }
                other => {
                    Err(AgentError::HandlerError(format!("unhandled request {}", other.method())))
                }
            }
        })
    }
}

impl ClientRequestHandler for DelayedReadHandler {
    fn capabilities(&self) -> HandlerCapabilities {
        HandlerCapabilities { fs_read: true, ..HandlerCapabilities::none() }
    }

    fn handle(
        &self,
        request: ClientRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ClientRequestResult> + Send + '_>> {
        Box::pin(async move {
            self.seen.lock().expect("delayed read handler poisoned").push(request.clone());
            match request {
                ClientRequest::ReadTextFile(_) => {
                    tokio::time::sleep(self.delay).await;
                    Ok(ClientRequestResponse::ReadTextFile(ReadTextFileResponse::new("late")))
                }
                other => {
                    Err(AgentError::HandlerError(format!("unhandled request {}", other.method())))
                }
            }
        })
    }
}

#[tokio::test]
async fn incoming_cancel_request_aborts_long_running_client_request() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::read_text_file("s1", "/work/Cargo.toml"))
        .delay(50)
        .emit(cancel_request(101))
        .respond(json!({ "stopReason": "end_turn" }));

    let handler = Arc::new(DelayedReadHandler::new(Duration::from_secs(1)));
    let (fake, host) = spawn_host(script, handler).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    thread.send_prompt(vec![ContentBlock::Text(TextContent::new("go"))]).await.unwrap();

    let response = await_response(&fake, 101).await;
    assert_eq!(response["error"]["code"], -32800, "response: {response}");
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn url_elicitation_completion_is_connection_scoped_and_idempotent() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::elicitation_url("s1", "el-1", "https://example.com/authorize", "authorize"))
        .emit(wire::elicitation_complete("el-1"))
        .emit(wire::elicitation_complete("el-1"))
        .emit(wire::elicitation_complete("el-stale"))
        .respond(json!({ "stopReason": "end_turn" }));

    let handler = Arc::new(DelayedUrlHandler::new(Duration::from_millis(100)));
    let (fake, mut host) = spawn_host(script, handler).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    thread
        .send_prompt(vec![ContentBlock::Text(TextContent::new("go"))])
        .await
        .expect("turn completes");

    let mut completions = Vec::new();
    let mut saw_turn_completed = false;
    while !saw_turn_completed {
        match next_event(&mut host.events).await {
            AgentEvent::ElicitationCompleted { elicitation_id } => {
                completions.push(elicitation_id.0.to_string());
            }
            AgentEvent::TurnCompleted { .. } => {
                saw_turn_completed = true;
            }
            AgentEvent::TurnStarted { .. }
            | AgentEvent::ThreadCreated { .. }
            | AgentEvent::ConnectionStateChanged { .. }
            | AgentEvent::ClientRequestDispatched { .. }
            | AgentEvent::SessionUpdate { .. } => {}
            _ => {}
        }
    }
    assert_eq!(completions, vec![String::from("el-1")]);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn authenticate_and_logout_round_trip() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "auth": { "logout": {} }
            },
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
    assert!(connection.supports_logout());

    connection.authenticate(auth_methods[0].id().clone()).await.expect("authenticate");
    connection.logout().await.expect("logout");

    assert_eq!(fake.requests_by_method("authenticate").len(), 1);
    assert_eq!(fake.requests_by_method("logout").len(), 1);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn logout_without_advertised_support_fails_locally() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": {},
            "authMethods": [{ "id": "device", "name": "Device auth" }]
        }))
        .wait_for("authenticate")
        .respond(json!({}));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let auth_methods = connection.auth_methods();
    assert_eq!(auth_methods.len(), 1);
    assert!(!connection.supports_logout());

    connection.authenticate(auth_methods[0].id().clone()).await.expect("authenticate");
    let error = connection.logout().await.unwrap_err();
    assert!(
        matches!(error, AgentError::CapabilityUnsupported { ref method } if method == "logout")
    );

    assert_eq!(fake.requests_by_method("authenticate").len(), 1);
    assert_eq!(fake.requests_by_method("logout").len(), 0);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn initialize_request_includes_client_title() {
    let initialize = initialize_request_for_capabilities(HandlerCapabilities::none()).await;
    assert_eq!(initialize["params"]["clientInfo"]["name"], "ee");
    assert_eq!(initialize["params"]["clientInfo"]["title"], "ee");
}

#[tokio::test]
async fn initialize_snapshot_fs_read_only() {
    let initialize = initialize_request_for_capabilities(HandlerCapabilities {
        fs_read: true,
        ..HandlerCapabilities::none()
    })
    .await;
    assert_eq!(
        initialize["params"]["clientCapabilities"],
        json!({
            "fs": {
                "readTextFile": true,
                "writeTextFile": false,
            },
            "terminal": false,
        })
    );
}

#[tokio::test]
async fn initialize_snapshot_fs_write_only() {
    let initialize = initialize_request_for_capabilities(HandlerCapabilities {
        fs_write: true,
        ..HandlerCapabilities::none()
    })
    .await;
    assert_eq!(
        initialize["params"]["clientCapabilities"],
        json!({
            "fs": {
                "readTextFile": false,
                "writeTextFile": true,
            },
            "terminal": false,
        })
    );
}

#[tokio::test]
async fn initialize_snapshot_fs_read_and_write() {
    let initialize = initialize_request_for_capabilities(HandlerCapabilities {
        fs_read: true,
        fs_write: true,
        ..HandlerCapabilities::none()
    })
    .await;
    assert_eq!(
        initialize["params"]["clientCapabilities"],
        json!({
            "fs": {
                "readTextFile": true,
                "writeTextFile": true,
            },
            "terminal": false,
        })
    );
}

#[tokio::test]
async fn initialize_snapshot_terminal_support() {
    let initialize = initialize_request_for_capabilities(HandlerCapabilities {
        terminal: true,
        ..HandlerCapabilities::none()
    })
    .await;
    assert_eq!(
        initialize["params"]["clientCapabilities"],
        json!({
            "fs": {
                "readTextFile": false,
                "writeTextFile": false,
            },
            "terminal": true,
        })
    );
}

#[tokio::test]
async fn initialize_snapshot_boolean_session_config_support() {
    let initialize = initialize_request_for_capabilities(HandlerCapabilities {
        session_config_boolean: true,
        ..HandlerCapabilities::none()
    })
    .await;
    assert_eq!(
        initialize["params"]["clientCapabilities"],
        json!({
            "fs": {
                "readTextFile": false,
                "writeTextFile": false,
            },
            "terminal": false,
            "session": {
                "configOptions": {
                    "boolean": {},
                }
            }
        })
    );
}

#[tokio::test]
async fn initialize_snapshot_elicitation_form_support() {
    let initialize = initialize_request_for_capabilities(HandlerCapabilities {
        elicitation_form: true,
        ..HandlerCapabilities::none()
    })
    .await;
    assert_eq!(
        initialize["params"]["clientCapabilities"],
        json!({
            "fs": {
                "readTextFile": false,
                "writeTextFile": false,
            },
            "terminal": false,
            "elicitation": {
                "form": {},
            }
        })
    );
}

#[tokio::test]
async fn initialize_snapshot_elicitation_url_support() {
    let initialize = initialize_request_for_capabilities(HandlerCapabilities {
        elicitation_url: true,
        ..HandlerCapabilities::none()
    })
    .await;
    assert_eq!(
        initialize["params"]["clientCapabilities"],
        json!({
            "fs": {
                "readTextFile": false,
                "writeTextFile": false,
            },
            "terminal": false,
            "elicitation": {
                "url": {},
            }
        })
    );
}

#[tokio::test]
async fn initialize_snapshot_no_capabilities() {
    let initialize = initialize_request_for_capabilities(HandlerCapabilities::none()).await;
    assert_eq!(
        initialize["params"]["clientCapabilities"],
        json!({
            "fs": {
                "readTextFile": false,
                "writeTextFile": false,
            },
            "terminal": false,
        })
    );
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

    let error = connection
        .load_session(SessionId::new("old"), PathBuf::from("/work"), Vec::new(), Vec::new())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        AgentError::CapabilityUnsupported { ref method } if method == "session/load"
    ));
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn load_session_routes_with_absolute_cwd_mcp_servers_and_optional_additional_directories() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "loadSession": true,
                "sessionCapabilities": { "additionalDirectories": {} }
            }
        }))
        .wait_for("session/load")
        .emit(wire::session_update("old", wire::agent_message_chunk("m1", "restored")))
        .respond(json!({
            "modes": {
                "currentModeId": "ask",
                "availableModes": [{ "id": "ask", "name": "Ask" }]
            },
            "configOptions": [
                {
                    "id": "mode",
                    "name": "Mode",
                    "category": "mode",
                    "type": "select",
                    "currentValue": "ask",
                    "options": [{ "value": "ask", "name": "Ask" }]
                }
            ]
        }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let thread = connection
        .load_session(
            SessionId::new("old"),
            PathBuf::from("/work"),
            vec![PathBuf::from("/extra")],
            vec![ee_agent_protocol::McpServer::Stdio(ee_agent_protocol::McpServerStdio::new(
                "tools",
                "agent-proxy",
            ))],
        )
        .await
        .expect("load_session");

    let request = &fake.requests_by_method("session/load")[0];
    assert_eq!(request["params"]["sessionId"], json!("old"));
    assert_eq!(request["params"]["cwd"], json!("/work"));
    assert_eq!(request["params"]["additionalDirectories"], json!(["/extra"]));
    assert_eq!(request["params"]["mcpServers"][0]["name"], json!("tools"));
    assert_eq!(request["params"]["mcpServers"][0]["command"], json!("agent-proxy"));
    assert!(thread.advertised_modes().is_some());
    assert_eq!(thread.config_options().len(), 1);
    assert_eq!(
        thread.snapshot().messages.len(),
        1,
        "streamed updates before load response must apply"
    );

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn load_session_omits_additional_directories_when_unadvertised() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": { "loadSession": true }
        }))
        .wait_for("session/load")
        .respond(json!({}));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    connection
        .load_session(
            SessionId::new("old"),
            PathBuf::from("/work"),
            vec![PathBuf::from("/extra")],
            Vec::new(),
        )
        .await
        .expect("load_session");

    let request = &fake.requests_by_method("session/load")[0];
    assert_eq!(request["params"].get("additionalDirectories"), None);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn load_session_rejects_relative_or_missing_roots() {
    let script = FakeAgentScript::new().wait_for("initialize").respond(json!({
        "protocolVersion": 1,
        "agentCapabilities": {
            "loadSession": true,
            "sessionCapabilities": { "additionalDirectories": {} }
        }
    }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let error = connection
        .load_session(SessionId::new("old"), PathBuf::from("relative"), Vec::new(), Vec::new())
        .await
        .unwrap_err();
    assert!(matches!(error, AgentError::InvalidParams(_)));

    let error = connection
        .load_session(
            SessionId::new("old"),
            PathBuf::from("/work"),
            vec![PathBuf::from("relative")],
            Vec::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, AgentError::InvalidParams(_)));
    assert_eq!(fake.requests_by_method("session/load").len(), 0);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn session_lifecycle_capabilities_reflect_advertised_flags() {
    let script = FakeAgentScript::new().wait_for("initialize").respond(json!({
        "protocolVersion": 1,
        "agentCapabilities": {
            "sessionCapabilities": {
                "list": {},
                "delete": {},
                "additionalDirectories": {},
                "resume": {},
                "close": {}
            }
        }
    }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    assert!(connection.supports_session_list());
    assert!(connection.supports_session_delete());
    assert!(connection.supports_session_resume());
    assert!(connection.supports_session_close());
    assert!(connection.supports_additional_directories());

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn list_sessions_routes_with_absolute_cwd_and_opaque_cursor() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": { "sessionCapabilities": { "list": {} } }
        }))
        .wait_for("session/list")
        .respond(json!({
            "sessions": [],
            "nextCursor": "opaque-next"
        }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let response = connection
        .list_sessions(Some(PathBuf::from("/work")), Some(String::from("opaque-prev")))
        .await
        .expect("list_sessions");
    assert_eq!(response.next_cursor.as_deref(), Some("opaque-next"));

    let request = &fake.requests_by_method("session/list")[0];
    assert_eq!(request["params"]["cwd"], json!("/work"));
    assert_eq!(request["params"]["cursor"], json!("opaque-prev"));
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn list_sessions_requires_advertised_capability_and_absolute_cwd() {
    let script = FakeAgentScript::new().wait_for("initialize").respond(json!({
        "protocolVersion": 1,
        "agentCapabilities": {}
    }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let error = connection.list_sessions(None, None).await.unwrap_err();
    assert!(matches!(
        error,
        AgentError::CapabilityUnsupported { ref method } if method == "session/list"
    ));
    assert_eq!(fake.requests_by_method("session/list").len(), 0);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;

    let script = FakeAgentScript::new().wait_for("initialize").respond(json!({
        "protocolVersion": 1,
        "agentCapabilities": { "sessionCapabilities": { "list": {} } }
    }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let error = connection.list_sessions(Some(PathBuf::from("relative")), None).await.unwrap_err();
    assert!(matches!(error, AgentError::InvalidParams(_)));
    assert_eq!(fake.requests_by_method("session/list").len(), 0);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn delete_session_routes_and_requires_capability() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": { "sessionCapabilities": { "delete": {} } }
        }))
        .wait_for("session/delete")
        .respond(json!({}));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    connection.delete_session(SessionId::new("dead")).await.expect("delete_session");
    assert_eq!(fake.requests_by_method("session/delete").len(), 1);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;

    let script = FakeAgentScript::new().wait_for("initialize").respond(json!({
        "protocolVersion": 1,
        "agentCapabilities": {}
    }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let error = connection.delete_session(SessionId::new("dead")).await.unwrap_err();
    assert!(matches!(
        error,
        AgentError::CapabilityUnsupported { ref method } if method == "session/delete"
    ));
    assert_eq!(fake.requests_by_method("session/delete").len(), 0);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn resume_session_routes_without_replayed_history() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "sessionCapabilities": {
                    "resume": {},
                    "additionalDirectories": {}
                }
            }
        }))
        .wait_for("session/resume")
        .respond(json!({
            "modes": {
                "currentModeId": "ask",
                "availableModes": [{ "id": "ask", "name": "Ask" }]
            },
            "configOptions": [
                {
                    "id": "mode",
                    "name": "Mode",
                    "category": "mode",
                    "type": "select",
                    "currentValue": "ask",
                    "options": [{ "value": "ask", "name": "Ask" }]
                }
            ]
        }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let thread = connection
        .resume_session(
            SessionId::new("s-resume"),
            PathBuf::from("/work"),
            vec![PathBuf::from("/extra")],
            Vec::new(),
        )
        .await
        .expect("resume_session");
    assert_eq!(thread.session_id().0.as_ref(), "s-resume");
    assert!(thread.snapshot().messages.is_empty(), "resume must not replay history");
    assert!(thread.advertised_modes().is_some());
    assert_eq!(thread.config_options().len(), 1);

    let request = &fake.requests_by_method("session/resume")[0];
    assert_eq!(request["params"]["cwd"], json!("/work"));
    assert_eq!(request["params"]["additionalDirectories"], json!(["/extra"]));
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn resume_session_requires_capabilities_and_absolute_paths() {
    let script = FakeAgentScript::new().wait_for("initialize").respond(json!({
        "protocolVersion": 1,
        "agentCapabilities": { "sessionCapabilities": { "resume": {} } }
    }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let error = connection
        .resume_session(
            SessionId::new("s1"),
            PathBuf::from("/work"),
            vec![PathBuf::from("/extra")],
            Vec::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        AgentError::CapabilityUnsupported { ref method } if method == "session/resume"
    ));
    assert_eq!(fake.requests_by_method("session/resume").len(), 0);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;

    let script = FakeAgentScript::new().wait_for("initialize").respond(json!({
        "protocolVersion": 1,
        "agentCapabilities": { "sessionCapabilities": { "resume": {}, "additionalDirectories": {} } }
    }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let error = connection
        .resume_session(SessionId::new("s1"), PathBuf::from("relative"), Vec::new(), Vec::new())
        .await
        .unwrap_err();
    assert!(matches!(error, AgentError::InvalidParams(_)));

    let error = connection
        .resume_session(
            SessionId::new("s1"),
            PathBuf::from("/work"),
            vec![PathBuf::from("relative")],
            Vec::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, AgentError::InvalidParams(_)));
    assert_eq!(fake.requests_by_method("session/resume").len(), 0);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn close_session_cancels_local_work_and_releases_thread_state() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": { "sessionCapabilities": { "close": {} } }
        }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
        .wait_for("session/prompt")
        .wait_for("session/close")
        .respond(json!({}));
    let (fake, mut host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

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

    connection.close_session(SessionId::new("s1")).await.expect("close_session");

    let error = prompt.await.expect("prompt task").unwrap_err();
    assert!(matches!(error, AgentError::Cancelled));
    assert!(!thread.is_turn_running());
    assert_eq!(connection.permission_broker().pending_count(), 0);
    assert_eq!(fake.requests_by_method("session/close").len(), 1);

    let mut saw_thread_closed = false;
    for _ in 0..8 {
        match next_event(&mut host.events).await {
            AgentEvent::ThreadClosed { session_id, reason, .. } => {
                assert_eq!(session_id.0.as_ref(), "s1");
                assert_eq!(reason, ThreadCloseReason::HostClosed);
                saw_thread_closed = true;
                break;
            }
            AgentEvent::TurnStarted { .. }
            | AgentEvent::TurnCancelled { .. }
            | AgentEvent::ThreadCreated { .. }
            | AgentEvent::ConnectionStateChanged { .. } => continue,
            _ => continue,
        }
    }
    assert!(saw_thread_closed, "thread close event must be observed");

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn close_session_requires_advertised_capability() {
    let script = base_script();
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let error = connection.close_session(SessionId::new("s1")).await.unwrap_err();
    assert!(matches!(
        error,
        AgentError::CapabilityUnsupported { ref method } if method == "session/close"
    ));
    assert_eq!(fake.requests_by_method("session/close").len(), 0);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn set_mode_updates_shared_mode_state() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({
            "sessionId": "s1",
            "modes": {
                "currentModeId": "ask",
                "availableModes": [
                    { "id": "ask", "name": "Ask" },
                    { "id": "plan", "name": "Plan" }
                ]
            }
        }))
        .wait_for("session/set_mode")
        .respond(json!({}));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let modes = thread.advertised_modes().expect("modes advertised");
    assert_eq!(modes.available_modes.len(), 2);

    thread.set_mode(SessionModeId::new("plan")).await.expect("set_mode");
    assert_eq!(fake.requests_by_method("session/set_mode").len(), 1);
    assert_eq!(thread.snapshot().current_mode, Some(SessionModeId::new("plan")));
    assert_eq!(
        thread.advertised_modes().expect("modes advertised").current_mode_id,
        SessionModeId::new("plan")
    );

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
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let error = thread.set_mode(SessionModeId::new("ask")).await.unwrap_err();
    assert!(matches!(
        error,
        AgentError::CapabilityUnsupported { ref method } if method == "session/set_mode"
    ));
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn set_config_options_are_stored_and_mode_category_overrides_legacy_modes() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({
            "sessionId": "s1",
            "modes": {
                "currentModeId": "ask",
                "availableModes": [
                    { "id": "ask", "name": "Ask" },
                    { "id": "agent", "name": "Agent" },
                    { "id": "plan", "name": "Plan" }
                ]
            },
            "configOptions": [
                {
                    "id": "mode",
                    "name": "Mode",
                    "category": "mode",
                    "type": "select",
                    "currentValue": "agent",
                    "options": [
                        { "value": "ask", "name": "Ask" },
                        { "value": "agent", "name": "Agent" },
                        { "value": "plan", "name": "Plan" }
                    ]
                }
            ]
        }))
        .wait_for("session/set_config_option")
        .respond(json!({
            "configOptions": [
                {
                    "id": "mode",
                    "name": "Mode",
                    "category": "mode",
                    "type": "select",
                    "currentValue": "plan",
                    "options": [
                        { "value": "ask", "name": "Ask" },
                        { "value": "agent", "name": "Agent" },
                        { "value": "plan", "name": "Plan" }
                    ]
                }
            ]
        }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let snapshot = thread.snapshot();
    assert_eq!(snapshot.current_mode, Some(SessionModeId::new("agent")));
    assert_eq!(snapshot.config_options.len(), 1);
    assert_eq!(thread.config_options()[0].id.0.as_ref(), "mode");

    thread.set_mode(SessionModeId::new("plan")).await.expect("config-backed mode change");
    assert_eq!(fake.requests_by_method("session/set_mode").len(), 0);
    let request = &fake.requests_by_method("session/set_config_option")[0];
    assert_eq!(request["params"]["configId"], "mode");
    assert_eq!(request["params"]["value"], "plan");

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn set_config_option_validates_select_values_locally() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({
            "sessionId": "s1",
            "configOptions": [
                {
                    "id": "mode",
                    "name": "Mode",
                    "category": "mode",
                    "type": "select",
                    "currentValue": "ask",
                    "options": [
                        { "value": "ask", "name": "Ask" },
                        { "value": "edit", "name": "Edit" }
                    ]
                }
            ]
        }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let error = thread
        .set_config_option(SessionConfigId::new("mode"), SessionConfigOptionValue::value_id("plan"))
        .await
        .unwrap_err();
    assert!(matches!(error, AgentError::InvalidParams(_)));
    assert_eq!(fake.requests_by_method("session/set_config_option").len(), 0);

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn set_config_option_boolean_requires_advertised_client_support() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({
            "sessionId": "s1",
            "configOptions": [
                {
                    "id": "confirmEdits",
                    "name": "Confirm edits",
                    "type": "boolean",
                    "currentValue": false
                }
            ]
        }));
    let handler = RecordingHandler::new(HandlerCapabilities::none());
    let (fake, host) = spawn_host(script, Arc::new(handler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let error = thread
        .set_config_option(
            SessionConfigId::new("confirmEdits"),
            SessionConfigOptionValue::boolean(true),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        AgentError::CapabilityUnsupported { ref method } if method == "session/set_config_option"
    ));
    assert_eq!(fake.requests_by_method("session/set_config_option").len(), 0);

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn set_config_option_boolean_round_trips_when_client_support_is_advertised() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({
            "sessionId": "s1",
            "configOptions": [
                {
                    "id": "confirmEdits",
                    "name": "Confirm edits",
                    "type": "boolean",
                    "currentValue": false
                }
            ]
        }))
        .wait_for("session/set_config_option")
        .respond(json!({
            "configOptions": [
                {
                    "id": "confirmEdits",
                    "name": "Confirm edits",
                    "type": "boolean",
                    "currentValue": true
                }
            ]
        }));
    let handler = RecordingHandler::new(HandlerCapabilities {
        session_config_boolean: true,
        ..HandlerCapabilities::none()
    });
    let (fake, host) = spawn_host(script, Arc::new(handler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    thread
        .set_config_option(
            SessionConfigId::new("confirmEdits"),
            SessionConfigOptionValue::boolean(true),
        )
        .await
        .expect("boolean config option routed");
    let request = &fake.requests_by_method("session/set_config_option")[0];
    assert_eq!(request["params"]["configId"], "confirmEdits");
    assert_eq!(request["params"]["type"], "boolean");
    assert_eq!(request["params"]["value"], true);

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn config_option_update_replaces_state_and_current_mode_update_keeps_mode_in_sync() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({
            "sessionId": "s1",
            "modes": {
                "currentModeId": "ask",
                "availableModes": [
                    { "id": "ask", "name": "Ask" },
                    { "id": "agent", "name": "Agent" },
                    { "id": "plan", "name": "Plan" }
                ]
            },
            "configOptions": [
                {
                    "id": "mode",
                    "name": "Mode",
                    "category": "mode",
                    "type": "select",
                    "currentValue": "ask",
                    "options": [
                        { "value": "ask", "name": "Ask" },
                        { "value": "agent", "name": "Agent" },
                        { "value": "plan", "name": "Plan" }
                    ]
                },
                {
                    "id": "confirmEdits",
                    "name": "Confirm edits",
                    "type": "boolean",
                    "currentValue": false
                }
            ]
        }))
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "config_option_update",
                "configOptions": [
                    {
                        "id": "mode",
                        "name": "Mode",
                        "category": "mode",
                        "type": "select",
                        "currentValue": "agent",
                        "options": [
                            { "value": "ask", "name": "Ask" },
                            { "value": "agent", "name": "Agent" },
                            { "value": "plan", "name": "Plan" }
                        ]
                    }
                ]
            }),
        ))
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "current_mode_update",
                "currentModeId": "plan"
            }),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    thread.send_prompt(vec![ContentBlock::Text(TextContent::new("go"))]).await.unwrap();
    let snapshot = thread.snapshot();
    assert_eq!(snapshot.config_options.len(), 1, "config option updates replace whole state");
    assert_eq!(snapshot.current_mode, Some(SessionModeId::new("plan")));
    let config = serde_json::to_value(&snapshot.config_options[0]).unwrap();
    assert_eq!(config["currentValue"], "plan");

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn plan_updates_route_through_host_and_replace_wholesale() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "plan",
                "entries": [
                    { "content": "first", "priority": "high", "status": "pending" },
                    { "content": "second", "priority": "low", "status": "in_progress" }
                ]
            }),
        ))
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "plan",
                "entries": [
                    { "content": "replacement", "priority": "medium", "status": "completed" }
                ]
            }),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    thread.send_prompt(vec![ContentBlock::Text(TextContent::new("go"))]).await.unwrap();
    let snapshot = thread.snapshot();
    assert_eq!(snapshot.plan.len(), 1);
    assert_eq!(snapshot.plan[0].content, "replacement");

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn available_commands_update_replaces_state_wholesale() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "available_commands_update",
                "availableCommands": [
                    { "name": "plan", "description": "Create plan" },
                    { "name": "edit", "description": "Edit code" }
                ]
            }),
        ))
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "available_commands_update",
                "availableCommands": [
                    { "name": "agent", "description": "Run agent mode" }
                ]
            }),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    thread.send_prompt(vec![ContentBlock::Text(TextContent::new("go"))]).await.unwrap();
    let snapshot = thread.snapshot();
    assert_eq!(snapshot.available_commands.len(), 1);
    assert_eq!(snapshot.available_commands[0].name, "agent");

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn prompt_allows_text_and_resource_links_without_prompt_capabilities() {
    let script =
        base_script().wait_for("session/prompt").respond(json!({ "stopReason": "end_turn" }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let prompt = vec![
        ContentBlock::Text(TextContent::new("hi")),
        ContentBlock::ResourceLink(
            ResourceLink::new("readme", "file:///work/README.md")
                .title("README")
                .meta(Some(serde_json::from_value(json!({ "source": "local-test" })).unwrap())),
        ),
    ];
    thread.send_prompt(prompt).await.expect("text and resource link stay allowed");

    let request = &fake.requests_by_method("session/prompt")[0];
    assert_eq!(request["params"]["prompt"][0]["type"], "text");
    assert_eq!(request["params"]["prompt"][1]["type"], "resource_link");
    assert_eq!(request["params"]["prompt"][1]["_meta"]["source"], "local-test");

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn prompt_rejects_unsupported_rich_content_locally_before_session_prompt() {
    let script = base_script();
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let image_error = thread
        .send_prompt(vec![ContentBlock::Image(ImageContent::new("ZmFrZQ==", "image/png"))])
        .await
        .unwrap_err();
    assert!(matches!(
        image_error,
        AgentError::InvalidParams(ref reason)
            if reason.contains("promptCapabilities.image")
    ));

    let audio_error = thread
        .send_prompt(vec![ContentBlock::Audio(AudioContent::new("ZmFrZQ==", "audio/wav"))])
        .await
        .unwrap_err();
    assert!(matches!(
        audio_error,
        AgentError::InvalidParams(ref reason)
            if reason.contains("promptCapabilities.audio")
    ));

    let resource_error = thread
        .send_prompt(vec![ContentBlock::Resource(EmbeddedResource::new(
            EmbeddedResourceResource::TextResourceContents(TextResourceContents::new(
                "hello",
                "file:///work/readme.md",
            )),
        ))])
        .await
        .unwrap_err();
    assert!(matches!(
        resource_error,
        AgentError::InvalidParams(ref reason)
            if reason.contains("promptCapabilities.embeddedContext")
    ));
    assert_eq!(fake.requests_by_method("session/prompt").len(), 0);

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn prompt_allows_advertised_rich_content_and_preserves_meta() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "promptCapabilities": {
                    "image": true,
                    "audio": true,
                    "embeddedContext": true,
                    "_meta": { "diagnosticOnly": true }
                }
            }
        }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let resource = ContentBlock::Resource(
        EmbeddedResource::new(EmbeddedResourceResource::TextResourceContents(
            TextResourceContents::new("hello", "file:///work/readme.md")
                .meta(Some(serde_json::from_value(json!({ "kind": "inline" })).unwrap())),
        ))
        .meta(Some(serde_json::from_value(json!({ "scope": "prompt" })).unwrap())),
    );
    let prompt = vec![
        ContentBlock::Image(
            ImageContent::new("ZmFrZQ==", "image/png")
                .meta(Some(serde_json::from_value(json!({ "slot": "preview" })).unwrap())),
        ),
        ContentBlock::Audio(
            AudioContent::new("ZmFrZQ==", "audio/wav")
                .meta(Some(serde_json::from_value(json!({ "slot": "clip" })).unwrap())),
        ),
        resource,
    ];
    thread.send_prompt(prompt).await.expect("advertised rich content accepted");

    let request = &fake.requests_by_method("session/prompt")[0];
    assert_eq!(request["params"]["prompt"][0]["_meta"]["slot"], "preview");
    assert_eq!(request["params"]["prompt"][1]["_meta"]["slot"], "clip");
    assert_eq!(request["params"]["prompt"][2]["_meta"]["scope"], "prompt");
    assert_eq!(request["params"]["prompt"][2]["resource"]["_meta"]["kind"], "inline");

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn prompt_capability_meta_stays_diagnostic_only() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "promptCapabilities": {
                    "_meta": {
                        "image": true,
                        "audio": true,
                        "embeddedContext": true
                    }
                }
            }
        }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    assert!(!connection.supports_prompt_images());
    assert!(!connection.supports_prompt_audio());
    assert!(!connection.supports_prompt_embedded_context());

    let error = thread
        .send_prompt(vec![ContentBlock::Image(ImageContent::new("ZmFrZQ==", "image/png"))])
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        AgentError::InvalidParams(ref reason)
            if reason.contains("promptCapabilities.image")
    ));
    assert_eq!(fake.requests_by_method("session/prompt").len(), 0);

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn unknown_capability_fields_stay_diagnostic_only() {
    let script = FakeAgentScript::new().wait_for("initialize").respond(json!({
        "protocolVersion": 1,
        "agentCapabilities": {
            "sessionCapabilities": { "mysteryLifecycle": {} },
            "promptCapabilities": { "mysteryPrompt": true }
        }
    }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    assert!(!connection.supports_session_list());
    assert!(!connection.supports_session_delete());
    assert!(!connection.supports_session_resume());
    assert!(!connection.supports_session_close());
    assert!(!connection.supports_prompt_images());
    assert!(!connection.supports_prompt_audio());
    assert!(!connection.supports_prompt_embedded_context());

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn close_kills_connection_and_resolves_pending_work() {
    let script = base_script().wait_for("session/prompt");
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

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

    let error = connection.new_session(Vec::new(), Vec::new(), None).await.unwrap_err();
    assert!(matches!(error, AgentError::InvalidParams(_)));

    let error = connection
        .new_session(vec![PathBuf::from("relative")], Vec::new(), None)
        .await
        .unwrap_err();
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
            ..Default::default()
        },
        transport,
    )
    .expect("connect");

    connection.wait_ready().await.expect("handshake ok");
    let error =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap_err();
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
