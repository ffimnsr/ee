//! Host-flow tests for the v2 client surface.
//!
//! The v2 mode (`AgentConnectionOptions.protocol_version = V2`) pins the
//! connection to the draft v2 wire: v2-shaped initialize, the v2 prompt
//! lifecycle (acknowledgment + idle state update), v2 session requests, and
//! the removed v1 methods failing closed.

use super::*;
use ee_agent_protocol::{MessageId, ToolCallId};

/// A scripted agent answering the v2 handshake and a v2 session/new.
fn v2_base_script() -> FakeAgentScript {
    FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 2,
            "info": { "name": "fake-agent-v2", "title": "Fake v2 Agent", "version": "1.0.0" },
            "capabilities": { "session": { "mcp": { "stdio": {} } } },
            "authMethods": [],
        }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1", "configOptions": [] }))
}

/// Spawns a host connection in v2 mode.
async fn spawn_host_v2(
    script: FakeAgentScript,
    handler: Arc<dyn ee_agent_host::ClientRequestHandler>,
) -> (FakeAgent, TestHost) {
    let (fake, transport) = FakeAgent::spawn(script);
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let options = AgentConnectionOptions {
        handshake_timeout: TEST_TIMEOUT,
        request_timeout: TEST_TIMEOUT,
        protocol_version: ProtocolVersion::V2,
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

/// A v2-shaped `session/update` notification for `session_id`.
fn v2_session_update(session_id: &str, update: Value) -> Value {
    wire::session_update(session_id, update)
}

/// A v2-shaped `session/request_permission` request.
fn v2_permission_request(session_id: &str, id: i64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/request_permission",
        "params": {
            "sessionId": session_id,
            "title": "Run the setup script?",
            "subject": {
                "type": "tool_call",
                "toolCall": {
                    "toolCallId": "call_1",
                    "title": "Execute setup",
                    "kind": "execute",
                    "status": "pending",
                },
            },
            "options": [
                { "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" },
            ],
        },
    })
}

/// v2 handshake: the host sends the v2-shaped initialize (role-agnostic
/// `info`/`capabilities`, no v1 client fields) and stores the agent's
/// session-nested capabilities in the v1-shaped state consumers read.
#[tokio::test]
async fn v2_handshake_with_v2_agent_succeeds() {
    let script = FakeAgentScript::new().wait_for("initialize").respond(json!({
        "protocolVersion": 2,
        "info": { "name": "fake-agent-v2", "version": "1.0.0" },
        "capabilities": {
            "session": {
                "prompt": { "image": {} },
                "mcp": { "stdio": {}, "http": {} },
            },
        },
        "authMethods": [],
    }));
    let (fake, host) = spawn_host_v2(script, Arc::new(DenyAllHandler)).await;

    let connection = &host.connection;
    connection.wait_ready().await.expect("v2 handshake succeeds");

    let initialize = fake.requests_by_method("initialize").pop().expect("initialize sent");
    assert_eq!(initialize["params"]["protocolVersion"], 2);
    assert_eq!(initialize["params"]["info"]["name"], "ee");
    assert!(
        initialize["params"].get("clientCapabilities").is_none(),
        "v1 clientCapabilities must not be sent on v2: {initialize}"
    );

    assert_eq!(connection.negotiated_protocol_version(), Some(ProtocolVersion::V2));
    let capabilities = connection.agent_capabilities().expect("capabilities stored");
    assert!(capabilities.prompt_capabilities.image, "v2 prompt image carried over");
    assert!(capabilities.mcp_capabilities.http, "v2 mcp http carried over");
    assert!(!capabilities.load_session, "v2 never advertises session/load");
    // v2 baseline: list/resume/close are always supported when `session` is
    // advertised — no marker probes.
    assert!(connection.supports_session_list());
    assert!(connection.supports_session_resume());
    assert!(connection.supports_session_close());
    assert!(connection.agent_info().is_some_and(|info| info.name == "fake-agent-v2"));

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

/// v2 mode with a v1-answering agent fails the handshake: the SDK pins one
/// wire surface per connection, so a v1 answer cannot be honored in place.
#[tokio::test]
async fn v2_mode_with_v1_agent_fails_closed() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }));
    let (fake, host) = spawn_host_v2(script, Arc::new(DenyAllHandler)).await;

    let error = host.connection.wait_ready().await.unwrap_err();
    let AgentError::Rpc(rpc_error) = &error else {
        panic!("expected a handshake rejection, got {error:?}");
    };
    assert!(
        rpc_error
            .data
            .as_ref()
            .and_then(|data| data.as_str())
            .is_some_and(|data| data.contains("negotiated 1"),),
        "SDK version guard must reject the v1 answer: {rpc_error:?}"
    );

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

/// v2 session/new carries the transport `type` discriminator on mcp servers
/// and the response flows back into the v1-shaped host API (no `modes`).
#[tokio::test]
async fn v2_session_new_sends_v2_wire_and_creates_thread() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 2,
            "info": { "name": "fake-agent-v2", "version": "1.0.0" },
            "capabilities": { "session": {} },
            "authMethods": [],
        }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1", "configOptions": [] }));
    let (fake, host) = spawn_host_v2(script, Arc::new(DenyAllHandler)).await;
    let connection = host.connection.clone();
    connection.wait_ready().await.expect("handshake succeeds");

    let thread = connection
        .new_session(
            vec![PathBuf::from("/work")],
            vec![ee_agent_protocol::McpServer::Stdio(ee_agent_protocol::McpServerStdio::new(
                "fs",
                "/usr/local/bin/mcp-fs",
            ))],
            None,
        )
        .await
        .expect("v2 session/new succeeds");
    assert_eq!(thread.session_id(), &SessionId::new("s1"));

    let new_session = fake.requests_by_method("session/new").pop().expect("session/new sent");
    assert_eq!(new_session["params"]["mcpServers"][0]["type"], "stdio");
    assert_eq!(new_session["params"]["mcpServers"][0]["name"], "fs");
    assert_eq!(new_session["params"]["mcpServers"][0]["command"], "/usr/local/bin/mcp-fs");

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

/// v2 prompt lifecycle: the response is an acknowledgment carrying the
/// agent-owned `messageId`; the turn keeps running and only ends when the
/// idle `state_update` with a stop reason arrives.  Streamed chunks reduce
/// into the v1-shaped session state.
#[tokio::test]
async fn v2_prompt_ends_turn_on_idle_state_update() {
    let script = v2_base_script()
        .wait_for("session/prompt")
        .respond(json!({ "messageId": "msg_user-1" }))
        .emit(v2_session_update(
            "s1",
            json!({
                "sessionUpdate": "agent_message_chunk",
                "messageId": "m-1",
                "content": { "type": "text", "text": "hello v2" },
            }),
        ))
        .emit(v2_session_update(
            "s1",
            json!({ "sessionUpdate": "state_update", "state": "idle", "stopReason": "end_turn" }),
        ));
    let (fake, mut host) = spawn_host_v2(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let prompt_thread = thread.clone();
    let prompt = tokio::spawn(async move {
        prompt_thread.send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))]).await
    });
    await_request_count(&fake, "session/prompt", 1).await;

    let mut saw_chunk = false;
    let turn_completed = loop {
        match next_event(&mut host.events).await {
            AgentEvent::SessionUpdate { session_id, update } => {
                assert_eq!(session_id, SessionId::new("s1"));
                if let ee_agent_protocol::SessionUpdate::AgentMessageChunk(chunk) = *update {
                    assert_eq!(chunk.message_id, Some(MessageId::new("m-1")));
                    saw_chunk = true;
                }
            }
            AgentEvent::TurnCompleted { session_id, stop_reason, .. } => {
                assert_eq!(session_id, SessionId::new("s1"));
                break stop_reason;
            }
            AgentEvent::TurnStarted { .. } | AgentEvent::ThreadCreated { .. } => continue,
            AgentEvent::ConnectionStateChanged { .. } => continue,
            other => panic!("unexpected event: {other:?}"),
        }
    };
    assert!(saw_chunk, "v2 agent chunk must be reduced before the turn ends");
    assert_eq!(turn_completed, StopReason::EndTurn);

    // The prompt future resolves with the same stop reason.
    let response = prompt.await.expect("prompt task").expect("turn completes");
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    // The ack's messageId never reaches the v1-typed response shape; the
    // only post-turn event is the host's own evidence summary.
    match host.events.try_recv() {
        Ok(AgentEvent::TurnEvidenceUpdated { .. }) => {}
        Ok(leftover) => panic!("unexpected leftover event after the turn: {leftover:?}"),
        Err(_) => {}
    }

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

/// v2 `tool_call_update` with a programmatic `name` reaches the reduced
/// state: v1 carries `name` on both create and update, so concrete names map
/// directly (the v2 `name: null` clear is lossy in v1, which treats `None` as
/// omission).
#[tokio::test]
async fn v2_tool_call_name_surfaces_in_snapshot() {
    let script = v2_base_script()
        .wait_for("session/prompt")
        .respond(json!({ "messageId": "msg_user-1" }))
        .emit(v2_session_update(
            "s1",
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_1",
                "title": "Run tests",
                "name": "test_runner",
                "kind": "execute",
                "status": "in_progress",
            }),
        ))
        .emit(v2_session_update(
            "s1",
            json!({ "sessionUpdate": "state_update", "state": "idle", "stopReason": "end_turn" }),
        ));
    let (fake, host) = spawn_host_v2(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let response = thread
        .send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))])
        .await
        .expect("turn completes");
    assert_eq!(response.stop_reason, StopReason::EndTurn);

    let tool_call = &thread.snapshot().tool_calls["call_1"];
    assert_eq!(tool_call.name.as_deref(), Some("test_runner"));
    assert_eq!(tool_call.title, "Run tests");
    assert_eq!(tool_call.status, ee_agent_protocol::ToolCallStatus::InProgress);

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

/// Host-initiated cancellation stays local on v2: `session/cancel` is sent
/// and the prompt resolves with `AgentError::Cancelled` without waiting for an
/// idle update (the agent may never answer).
#[tokio::test]
async fn v2_cancel_resolves_locally_without_idle_update() {
    let script = v2_base_script()
        .wait_for("session/prompt")
        .respond(json!({ "messageId": "msg_user-1" }))
        .wait_for("session/cancel");
    let (fake, mut host) = spawn_host_v2(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let prompt_thread = thread.clone();
    let prompt = tokio::spawn(async move {
        prompt_thread.send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))]).await
    });
    await_request_count(&fake, "session/prompt", 1).await;

    thread.cancel().await.expect("cancel succeeds");
    let error = prompt.await.expect("prompt task").unwrap_err();
    assert!(matches!(error, AgentError::Cancelled));
    assert!(fake.log_contains("\"method\":\"session/cancel\""));

    // The only events that follow are the host's own lifecycle events (the
    // v1-shaped TurnCancelled plus setup events still queued); no agent
    // session update or second completion may surface.
    let drain = tokio::time::timeout(Duration::from_millis(200), async {
        let mut saw_turn_cancelled = false;
        loop {
            match tokio::time::timeout(Duration::from_millis(50), host.events.recv()).await {
                Ok(Some(AgentEvent::TurnCancelled { .. })) => saw_turn_cancelled = true,
                Ok(Some(AgentEvent::TurnStarted { .. }))
                | Ok(Some(AgentEvent::ThreadCreated { .. }))
                | Ok(Some(AgentEvent::ConnectionStateChanged { .. }))
                | Ok(Some(AgentEvent::TurnEvidenceUpdated { .. })) => {}
                Ok(Some(AgentEvent::TurnCompleted { .. }))
                | Ok(Some(AgentEvent::TurnFailed { .. }))
                | Ok(Some(AgentEvent::SessionUpdate { .. })) => {
                    panic!("agent update surfaced after local cancel")
                }
                Ok(Some(other)) => panic!("unexpected event after local cancel: {other:?}"),
                Ok(None) => break,
                Err(_) => break,
            }
        }
        saw_turn_cancelled
    })
    .await
    .expect("event drain finishes");
    assert!(drain, "host must surface TurnCancelled after a local cancel");

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

/// An agent-led cancellation on v2 resolves the turn as a completed prompt
/// whose `stop_reason` is `cancelled` (the v2 idle update, not a local error),
/// and no `session/cancel` is sent by the host.
#[tokio::test]
async fn v2_idle_cancelled_resolves_turn_with_cancelled_stop_reason() {
    let script = v2_base_script()
        .wait_for("session/prompt")
        .respond(json!({ "messageId": "msg_user-1" }))
        .emit(v2_session_update(
            "s1",
            json!({ "sessionUpdate": "state_update", "state": "idle", "stopReason": "cancelled" }),
        ));
    let (fake, mut host) = spawn_host_v2(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let prompt_thread = thread.clone();
    let prompt = tokio::spawn(async move {
        prompt_thread.send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))]).await
    });
    await_request_count(&fake, "session/prompt", 1).await;

    let response = prompt.await.expect("prompt task").expect("idle-cancelled completes the turn");
    assert_eq!(response.stop_reason, StopReason::Cancelled);
    let turn_completed = loop {
        match next_event(&mut host.events).await {
            AgentEvent::TurnCompleted { session_id, stop_reason, .. } => {
                assert_eq!(session_id, SessionId::new("s1"));
                break stop_reason;
            }
            AgentEvent::TurnStarted { .. }
            | AgentEvent::ThreadCreated { .. }
            | AgentEvent::ConnectionStateChanged { .. } => continue,
            other => panic!("unexpected event: {other:?}"),
        }
    };
    assert_eq!(turn_completed, StopReason::Cancelled);
    assert!(!fake.log_contains("\"method\":\"session/cancel\""));

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

/// v2 keeps `session/set_config_option`; the request goes out with the v2
/// typed value (`type: "id"`) and the response returns to the host API.
#[tokio::test]
async fn v2_set_config_option_sends_config_id_and_value_on_v2_wire() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 2,
            "info": { "name": "fake-agent-v2", "version": "1.0.0" },
            "capabilities": { "session": {} },
            "authMethods": [],
        }))
        .wait_for("session/new")
        .respond(json!({
            "sessionId": "s1",
            "configOptions": [
                {
                    "configId": "model",
                    "name": "Model",
                    "category": "mode",
                    "type": "select",
                    "options": [
                        { "value": "fast", "name": "Fast" },
                        { "value": "slow", "name": "Slow" },
                    ],
                    "currentValue": "fast",
                },
            ],
        }))
        .wait_for("session/set_config_option")
        .respond(json!({
            "configOptions": [
                {
                    "configId": "model",
                    "name": "Model",
                    "category": "mode",
                    "type": "select",
                    "options": [
                        { "value": "fast", "name": "Fast" },
                        { "value": "slow", "name": "Slow" },
                    ],
                    "currentValue": "slow",
                },
            ],
        }));
    let (fake, host) = spawn_host_v2(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();
    let options = thread.config_options();
    assert_eq!(options.len(), 1, "v2 configOptions must reach the v1 state");
    assert_eq!(options[0].id, ee_agent_protocol::SessionConfigId::new("model"));
    assert_eq!(
        options[0].category,
        Some(ee_agent_protocol::SessionConfigOptionCategory::Mode),
        "mode-like state renders from configOptions on v2"
    );

    thread
        .set_config_option(
            ee_agent_protocol::SessionConfigId::new("model"),
            ee_agent_protocol::SessionConfigOptionValue::value_id(
                ee_agent_protocol::SessionConfigValueId::new("slow"),
            ),
        )
        .await
        .expect("set_config_option succeeds on v2");

    let request = fake.requests_by_method("session/set_config_option").pop().expect("request sent");
    assert_eq!(request["params"]["configId"], "model");
    assert_eq!(request["params"]["type"], "id");
    assert_eq!(request["params"]["value"], "slow");

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

/// The removed v1 methods fail closed on v2 connections before any wire
/// traffic.
#[tokio::test]
async fn v2_removed_v1_methods_fail_closed() {
    let script = v2_base_script();
    let (fake, host) = spawn_host_v2(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    // session/load is removed in v2.
    assert!(!connection.supports_load_session());
    let error = connection
        .load_session(SessionId::new("s1"), PathBuf::from("/work"), Vec::new(), Vec::new())
        .await
        .expect_err("load must fail closed on v2");
    assert!(matches!(error, AgentError::CapabilityUnsupported { .. }));

    // session/set_mode is removed in v2; the thread falls back to the error.
    let error = thread.set_mode(SessionModeId::new("ask")).await.expect_err("set_mode must fail");
    assert!(matches!(error, AgentError::CapabilityUnsupported { .. }));

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

/// v2 `session/request_permission` flows through the same broker: the
/// tool-call subject is translated to the v1-shaped broker state and the
/// selected outcome returns on the v2 wire.
#[tokio::test]
async fn v2_permission_request_flows_through_broker() {
    let script = v2_base_script()
        .wait_for("session/prompt")
        .emit(v2_permission_request("s1", 42))
        .wait_for_response(42)
        .respond(json!({ "messageId": "msg_user-1" }))
        .emit(v2_session_update(
            "s1",
            json!({ "sessionUpdate": "state_update", "state": "idle", "stopReason": "end_turn" }),
        ));
    let (fake, mut host) = spawn_host_v2(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let prompt_thread = thread.clone();
    let prompt = tokio::spawn(async move {
        prompt_thread.send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))]).await
    });

    let request_id = loop {
        match next_event(&mut host.events).await {
            AgentEvent::PermissionRequested { session_id, request } => {
                assert_eq!(session_id, SessionId::new("s1"));
                assert_eq!(request.tool_call.fields.title.as_deref(), Some("Execute setup"));
                assert_eq!(request.tool_call.tool_call_id, ToolCallId::new("call_1"));
                assert_eq!(request.options.len(), 1);
                break request.request_id;
            }
            _ => continue,
        }
    };
    let outcome = RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("allow_once"));
    assert!(thread.respond_permission(request_id, outcome.clone()));

    let response = prompt.await.expect("prompt task").expect("turn completes");
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert!(fake.log_contains("\"outcome\":\"selected\""));

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

// ── Phase 2 gap closure: whole-message + terminal transcript fallback ─────

/// A v2 whole `agent_message` (no chunk streaming) renders as same-id chunks:
/// the reducer merges them into one message and the transcript appends, so
/// non-streaming agents stay visible.
#[tokio::test]
async fn v2_whole_agent_message_renders_as_same_id_chunks() {
    let script = v2_base_script()
        .wait_for("session/prompt")
        .respond(json!({ "messageId": "msg_user-1" }))
        .emit(v2_session_update(
            "s1",
            json!({
                "sessionUpdate": "agent_message",
                "messageId": "m-final",
                "content": [
                    { "type": "text", "text": "first part" },
                    { "type": "text", "text": ". second part" },
                ],
            }),
        ))
        .emit(v2_session_update(
            "s1",
            json!({ "sessionUpdate": "state_update", "state": "idle", "stopReason": "end_turn" }),
        ));
    let (fake, mut host) = spawn_host_v2(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let prompt_thread = thread.clone();
    let prompt = tokio::spawn(async move {
        prompt_thread.send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))]).await
    });
    await_request_count(&fake, "session/prompt", 1).await;

    let mut texts = Vec::new();
    loop {
        match next_event(&mut host.events).await {
            AgentEvent::SessionUpdate { session_id, update } => {
                assert_eq!(session_id, SessionId::new("s1"));
                if let ee_agent_protocol::SessionUpdate::AgentMessageChunk(chunk) = *update {
                    assert_eq!(chunk.message_id, Some(MessageId::new("m-final")));
                    let ee_agent_protocol::ContentBlock::Text(text) = &chunk.content else {
                        panic!("expected text block");
                    };
                    texts.push(text.text.clone());
                }
            }
            AgentEvent::TurnCompleted { .. } => break,
            AgentEvent::TurnStarted { .. }
            | AgentEvent::ThreadCreated { .. }
            | AgentEvent::ConnectionStateChanged { .. }
            | AgentEvent::TurnEvidenceUpdated { .. } => continue,
            other => panic!("unexpected event: {other:?}"),
        }
    }
    assert_eq!(texts, vec!["first part", ". second part"], "all blocks surface");
    let _ = prompt.await.expect("prompt task").expect("turn completes");

    // The reduced state holds ONE merged message, not two chunks.
    let snapshot = thread.snapshot();
    let merged = snapshot
        .messages
        .iter()
        .filter(|message| message.message_id.as_deref() == Some("m-final"))
        .collect::<Vec<_>>();
    assert_eq!(merged.len(), 1, "same-id chunks merge in the reducer");
    assert_eq!(merged[0].blocks.len(), 2);

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

/// Whole-message upserts REPLACE (or clear with `null`/`[]`) the content
/// stored for their `messageId`; chunks always append (v2 upsert semantics).
#[tokio::test]
async fn v2_whole_messages_replace_and_clear_by_id() {
    let script = v2_base_script()
        .wait_for("session/prompt")
        .respond(json!({ "messageId": "msg_user-1" }))
        .emit(v2_session_update(
            "s1",
            json!({
                "sessionUpdate": "agent_message_chunk",
                "messageId": "m-1",
                "content": { "type": "text", "text": "part one" },
            }),
        ))
        .emit(v2_session_update(
            "s1",
            json!({
                "sessionUpdate": "agent_message",
                "messageId": "m-1",
                "content": [{ "type": "text", "text": "replacement" }],
            }),
        ))
        .emit(v2_session_update(
            "s1",
            json!({
                "sessionUpdate": "agent_message",
                "messageId": "m-1",
                "content": [],
            }),
        ))
        .emit(v2_session_update(
            "s1",
            json!({
                "sessionUpdate": "agent_thought",
                "messageId": "m-2",
                "content": [{ "type": "text", "text": "the idea" }],
            }),
        ))
        .emit(v2_session_update(
            "s1",
            json!({ "sessionUpdate": "state_update", "state": "idle", "stopReason": "end_turn" }),
        ));
    let (fake, host) = spawn_host_v2(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    thread
        .send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))])
        .await
        .expect("turn completes");

    let snapshot = thread.snapshot();
    // The whole `agent_message` replaced the chunk-accumulated content, then
    // `content: []` cleared the message entirely.
    assert!(
        snapshot.messages.iter().all(|message| message.message_id.as_deref() != Some("m-1")),
        "cleared message must be gone: {:?}",
        snapshot.messages
    );
    let thought = snapshot
        .messages
        .iter()
        .find(|message| message.message_id.as_deref() == Some("m-2"))
        .expect("thought message retained");
    assert_eq!(thought.kind, ee_agent_host::MessageKind::Thought);
    assert_eq!(thought.blocks.len(), 1);
    let ee_agent_protocol::ContentBlock::Text(text) = &thought.blocks[0] else {
        panic!("expected text block");
    };
    assert_eq!(text.text, "the idea");

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

/// The v2 `user_message` echo of the host's own prompt must not duplicate the
/// transcript (the host renders the submitted prompt optimistically).
#[tokio::test]
async fn v2_user_message_echo_does_not_duplicate_transcript() {
    let script = v2_base_script()
        .wait_for("session/prompt")
        .respond(json!({ "messageId": "msg_user-1" }))
        .emit(v2_session_update(
            "s1",
            json!({
                "sessionUpdate": "user_message",
                "messageId": "msg_user-1",
                "content": [{ "type": "text", "text": "hi" }],
            }),
        ))
        .emit(v2_session_update(
            "s1",
            json!({ "sessionUpdate": "state_update", "state": "idle", "stopReason": "end_turn" }),
        ));
    let (fake, mut host) = spawn_host_v2(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let prompt_thread = thread.clone();
    let prompt = tokio::spawn(async move {
        prompt_thread.send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))]).await
    });
    await_request_count(&fake, "session/prompt", 1).await;

    loop {
        match next_event(&mut host.events).await {
            AgentEvent::TurnCompleted { .. } => break,
            AgentEvent::SessionUpdate { .. } => {
                panic!("the user_message echo must not surface as a session update")
            }
            AgentEvent::TurnStarted { .. }
            | AgentEvent::ThreadCreated { .. }
            | AgentEvent::ConnectionStateChanged { .. }
            | AgentEvent::TurnEvidenceUpdated { .. } => continue,
            other => panic!("unexpected event: {other:?}"),
        }
    }
    let _ = prompt.await.expect("prompt task").expect("turn completes");
    // Only the optimistic user message exists in state.
    let snapshot = thread.snapshot();
    let user_messages = snapshot
        .messages
        .iter()
        .filter(|message| message.kind == ee_agent_host::MessageKind::User)
        .count();
    assert_eq!(user_messages, 1, "no duplicate user line");

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

/// Agent-owned terminal output takes the sanitized transcript fallback:
/// decoded, printable output lines render as same-id agent chunks under a
/// per-terminal message id (no terminal pane needed).
#[tokio::test]
async fn v2_terminal_output_renders_sanitized_transcript_fallback() {
    use base64::Engine as _;
    let encoded =
        base64::engine::general_purpose::STANDARD.encode("cargo test\r\n\u{1b}[32mok\u{1b}[0m\n");
    let script = v2_base_script()
        .wait_for("session/prompt")
        .respond(json!({ "messageId": "msg_user-1" }))
        .emit(v2_session_update(
            "s1",
            json!({
                "sessionUpdate": "terminal_output_chunk",
                "terminalId": "term-1",
                "data": encoded,
            }),
        ))
        .emit(v2_session_update(
            "s1",
            json!({ "sessionUpdate": "state_update", "state": "idle", "stopReason": "end_turn" }),
        ));
    let (fake, mut host) = spawn_host_v2(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let prompt_thread = thread.clone();
    let prompt = tokio::spawn(async move {
        prompt_thread.send_prompt(vec![ContentBlock::Text(TextContent::new("hi"))]).await
    });
    await_request_count(&fake, "session/prompt", 1).await;

    let mut rendered = None;
    loop {
        match next_event(&mut host.events).await {
            AgentEvent::SessionUpdate { session_id, update } => {
                assert_eq!(session_id, SessionId::new("s1"));
                if let ee_agent_protocol::SessionUpdate::AgentMessageChunk(chunk) = *update {
                    assert_eq!(chunk.message_id, Some(MessageId::new("terminal-term-1")));
                    let ee_agent_protocol::ContentBlock::Text(text) = &chunk.content else {
                        panic!("expected text block");
                    };
                    rendered = Some(text.text.clone());
                }
            }
            AgentEvent::TurnCompleted { .. } => break,
            AgentEvent::TurnStarted { .. }
            | AgentEvent::ThreadCreated { .. }
            | AgentEvent::ConnectionStateChanged { .. }
            | AgentEvent::TurnEvidenceUpdated { .. } => continue,
            other => panic!("unexpected event: {other:?}"),
        }
    }
    let _ = prompt.await.expect("prompt task").expect("turn completes");
    let rendered = rendered.expect("terminal output must render in the transcript");
    assert!(
        rendered.contains("cargo test") && rendered.contains("ok"),
        "printable output survives: {rendered:?}"
    );
    assert!(!rendered.contains('\u{1b}'), "ANSI escape noise is stripped: {rendered:?}");

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}
