//! End-to-end ACP v2 flows over the in-memory transport: initialization with
//! the v2 surface, the v2 prompt lifecycle (acknowledgment + state updates),
//! v2-only method gating, and v2 cancellation.
//!
//! The v1 surface is unaffected and stays covered by `server_flows.rs`,
//! `client_requests.rs`, and the conformance/hardening suites.

#[allow(dead_code)]
mod common;

use ee_agent_protocol::RawJsonRpcMessage;
use serde_json::{Value, json};

use common::{
    FakeProvider, PromptBehavior, notification, prompt_params, raw_params_to_value, request,
    request_error, request_result, session_new_params, spawn_server,
};

/// A fully-shaped v2 `initialize` request (required `info` + `capabilities`).
fn v2_initialize_request() -> RawJsonRpcMessage {
    request(
        1,
        "initialize",
        json!({
            "protocolVersion": 2,
            "info": { "name": "test-client", "title": "Test Client", "version": "1.0.0" },
            "capabilities": {},
        }),
    )
}

/// Runs the v2 handshake over `handle`, asserting the agent answered with the
/// v2 surface, then creates a session and asserts the v2-shaped response
/// (session id, no `modes`).
async fn v2_handshake_and_session(handle: &common::Harness) -> String {
    handle.send(v2_initialize_request());
    let result = request_result(handle.next_frame().await);
    assert_eq!(result["protocolVersion"], 2);
    assert_eq!(result["info"]["name"], "fake-provider");

    handle.send(request(2, "session/new", session_new_params("/work")));
    let result = request_result(handle.next_frame().await);
    assert_eq!(result["sessionId"], "provider-session-1");
    assert!(result.get("modes").is_none(), "v2 responses must not carry modes: {result}");
    "provider-session-1".to_string()
}

/// The v1 client fs/terminal request surface is removed in v2 (guide): a
/// provider calling `client.read_text_file` on a v2 connection fails closed
/// before anything reaches the transport — no `fs/*` request may appear on
/// the v2 wire.
#[tokio::test(flavor = "current_thread")]
async fn v2_removed_client_surface_fails_closed_before_transport() {
    let (provider, log) = FakeProvider::new(&["provider-session-1"]);
    provider.set_prompt_behavior(
        "provider-session-1",
        PromptBehavior::ReadTextFileAndContinue { path: "/work/a.txt".to_string() },
    );
    let (handle, task) = spawn_server(provider).await;
    let session_id = v2_handshake_and_session(&handle).await;

    handle.send(request(5, "session/prompt", prompt_params(&session_id)));

    // The provider's read fails closed and the turn still completes.
    let calls = common::wait_for_log(&log, |calls| {
        calls.iter().any(|call| call.starts_with("client:read_text_file:err:"))
    })
    .await;
    assert!(
        calls.iter().any(|call| call.starts_with("client:read_text_file:err:")
            && call.contains("removed in ACP v2")),
        "provider must see the v2 removal error: {calls:?}"
    );

    // Drain the turn: every frame until the prompt response must be a
    // notification — never an `fs/*` client request.
    loop {
        let frame = handle.next_frame().await;
        match &frame {
            RawJsonRpcMessage::Request(request) => {
                panic!("no client request may reach the v2 wire: {}", request.method)
            }
            RawJsonRpcMessage::Response(_) => break,
            RawJsonRpcMessage::Notification(_) => {}
        }
    }

    handle.shutdown(task).await;
}

/// v2 initialize: role-agnostic `info`/`capabilities`, no v1 fields, no auth.
#[tokio::test(flavor = "current_thread")]
async fn v2_initialize_negotiates_agent_surface() {
    let (provider, _log) = FakeProvider::new(&[]);
    let (handle, task) = spawn_server(provider).await;

    handle.send(v2_initialize_request());
    let result = request_result(handle.next_frame().await);

    assert_eq!(result["protocolVersion"], 2);
    assert_eq!(result["info"]["name"], "fake-provider");
    assert!(result.get("agentInfo").is_none(), "v1 field must not appear");
    assert!(result.get("agentCapabilities").is_none(), "v1 field must not appear");
    assert!(result["capabilities"]["session"].is_object(), "session capabilities advertised");
    assert_eq!(
        result.get("authMethods").cloned().unwrap_or_else(|| json!([])),
        json!([]),
        "no auth surface advertised"
    );
    // Framework identity metadata is carried in `_meta` on both surfaces.
    assert_eq!(result["_meta"]["framework"]["name"], "ee-acp-agent-server");

    handle.shutdown(task).await;
}

/// v2 session lifecycle: `session/load` and `session/set_mode` are removed in
/// v2 and must fail closed with method-not-found on v2 connections.
#[tokio::test(flavor = "current_thread")]
async fn v2_removed_methods_fail_closed() {
    let (provider, _log) = FakeProvider::new(&["provider-session-1"]);
    let (handle, task) = spawn_server(provider).await;

    let session_id = v2_handshake_and_session(&handle).await;

    handle.send(request(
        3,
        "session/load",
        json!({
            "sessionId": session_id,
            "cwd": "/work",
            "additionalDirectories": [],
            "mcpServers": [],
        }),
    ));
    let error = request_error(handle.next_frame().await);
    assert_eq!(i32::from(error.code), -32601, "session/load is not a v2 method");

    handle.send(request(
        4,
        "session/set_mode",
        json!({
            "sessionId": session_id,
            "modeId": "ask",
        }),
    ));
    let error = request_error(handle.next_frame().await);
    assert_eq!(i32::from(error.code), -32601, "session/set_mode is not a v2 method");

    handle.shutdown(task).await;
}

/// v2 prompt lifecycle: the response is an insertion acknowledgment carrying
/// the agent-owned `messageId`; the user message is reported with the same
/// id, foreground work reports `running`, streamed chunks carry message ids,
/// and turn completion arrives as an idle `state_update` with the stop reason
/// — never in the prompt response.
#[tokio::test(flavor = "current_thread")]
async fn v2_prompt_lifecycle_acknowledges_then_reports_state() {
    let (provider, _log) = FakeProvider::new(&["provider-session-1"]);
    provider.set_prompt_behavior("provider-session-1", PromptBehavior::EmitMessageThenReturn);
    let (handle, task) = spawn_server(provider).await;
    let session_id = v2_handshake_and_session(&handle).await;

    handle.send(request(5, "session/prompt", prompt_params(&session_id)));
    let ack = request_result(handle.next_frame().await);
    let message_id = ack["messageId"]
        .as_str()
        .expect("prompt response must carry an agent-owned messageId")
        .to_string();
    assert!(message_id.starts_with("msg_user-"), "agent-owned message id: {message_id}");
    assert!(ack.get("stopReason").is_none(), "stop reason lives in state updates now");

    // The user message acknowledgment (same id), running state, the streamed
    // agent chunk, and the idle state update with the stop reason follow in
    // FIFO order.
    let user = expect_v2_update(handle.next_frame().await, &session_id);
    assert_eq!(user["sessionUpdate"], "user_message");
    assert_eq!(user["messageId"], message_id);
    assert_eq!(user["content"][0]["type"], "text");

    let running = expect_v2_update(handle.next_frame().await, &session_id);
    assert_eq!(running["sessionUpdate"], "state_update");
    assert_eq!(running["state"], "running");

    let chunk = expect_v2_update(handle.next_frame().await, &session_id);
    assert_eq!(chunk["sessionUpdate"], "agent_message_chunk");
    assert_eq!(chunk["content"]["type"], "text");
    assert_eq!(chunk["content"]["text"], "hello from provider");
    assert_eq!(chunk["messageId"], "m-1", "streamed chunks carry their message id");

    let idle = expect_v2_update(handle.next_frame().await, &session_id);
    assert_eq!(idle["sessionUpdate"], "state_update");
    assert_eq!(idle["state"], "idle");
    assert_eq!(idle["stopReason"], "end_turn");

    // The prompt response was the ack; nothing else follows the lifecycle.
    assert!(handle.outbound().is_empty(), "no stray frames: {:?}", handle.outbound());

    handle.shutdown(task).await;
}

/// v2 cancellation: the response was already sent at acceptance, so
/// `session/cancel` ends with an idle `state_update` carrying the `cancelled`
/// stop reason.
#[tokio::test(flavor = "current_thread")]
async fn v2_cancel_confirms_with_idle_cancelled_state() {
    let (provider, _log) = FakeProvider::new(&["provider-session-1"]);
    provider.set_prompt_behavior("provider-session-1", PromptBehavior::AwaitCancelThenCancelled);
    let (handle, task) = spawn_server(provider).await;
    let session_id = v2_handshake_and_session(&handle).await;

    handle.send(request(5, "session/prompt", prompt_params(&session_id)));
    request_result(handle.next_frame().await); // ack

    // Consume the user_message + running updates queued at acceptance.
    expect_v2_update(handle.next_frame().await, &session_id);
    expect_v2_update(handle.next_frame().await, &session_id);

    handle.send(notification(
        "session/cancel",
        json!({
            "sessionId": session_id,
        }),
    ));

    let idle = expect_v2_update(handle.next_frame().await, &session_id);
    assert_eq!(idle["sessionUpdate"], "state_update");
    assert_eq!(idle["state"], "idle");
    assert_eq!(idle["stopReason"], "cancelled");

    assert!(handle.outbound().is_empty(), "no prompt response after cancellation");
    handle.shutdown(task).await;
}

/// v1 and v2 sessions coexist: after a v1 handshake everything stays on the
/// v1 surface (guard against cross-version leakage through shared state).
#[tokio::test(flavor = "current_thread")]
async fn v1_handshake_keeps_v1_surface() {
    let (provider, _log) = FakeProvider::new(&["provider-session-1"]);
    provider.set_prompt_behavior("provider-session-1", PromptBehavior::Return);
    let (handle, task) = spawn_server(provider).await;

    handle.send(request(1, "initialize", json!({ "protocolVersion": 1 })));
    let result = request_result(handle.next_frame().await);
    assert_eq!(result["protocolVersion"], 1);
    assert!(result.get("agentInfo").is_some(), "v1 field expected");

    handle.send(request(2, "session/new", session_new_params("/work")));
    let result = request_result(handle.next_frame().await);
    assert_eq!(result["sessionId"], "provider-session-1");
    assert!(
        result.get("capabilities").is_none(),
        "v1 responses must not carry the v2 capability shape"
    );

    handle.shutdown(task).await;
}

/// Extracts the `update` object of one `session/update` notification.
fn expect_v2_update(frame: RawJsonRpcMessage, session_id: &str) -> Value {
    let RawJsonRpcMessage::Notification(notification) = frame else {
        panic!("expected a session/update notification, got {frame:?}");
    };
    assert_eq!(notification.method.as_ref(), "session/update");
    let params = raw_params_to_value(notification.params.clone());
    assert_eq!(params["sessionId"], session_id);
    let update = params["update"].clone();
    assert!(update["sessionUpdate"].is_string(), "update must carry a discriminator: {update}");
    update
}

// ── Phase 3: auth/login + auth/logout ────────────────────────────────────

/// A single advertised v2 authentication method.
fn agent_auth_method() -> ee_agent_protocol::v2::AuthMethod {
    ee_agent_protocol::v2::AuthMethod::Agent(ee_agent_protocol::v2::AuthMethodAgent::new(
        "/users/me/credentials",
        "Agent credentials",
    ))
}

/// v2 `auth/login` + `auth/logout`: advertised methods are served, unknown
/// method ids are rejected before the provider call, and v1 connections fail
/// closed with method-not-found.
#[tokio::test(flavor = "current_thread")]
async fn v2_auth_login_and_logout_follow_advertised_methods() {
    let (provider, log) = FakeProvider::new(&["provider-session-1"]);
    let provider = provider.with_auth_methods(vec![agent_auth_method()]);
    let (handle, task) = spawn_server(provider).await;

    handle.send(v2_initialize_request());
    let result = request_result(handle.next_frame().await);
    assert_eq!(result["authMethods"][0]["methodId"], "/users/me/credentials");
    assert_eq!(result["authMethods"][0]["type"], "agent");

    // auth/login with the advertised method succeeds.
    handle.send(request(2, "auth/login", json!({ "methodId": "/users/me/credentials" })));
    request_result(handle.next_frame().await);
    assert!(log.calls().iter().any(|call| call == "login:/users/me/credentials"));

    // auth/logout succeeds when a non-empty authMethods list was advertised.
    handle.send(request(3, "auth/logout", json!({})));
    request_result(handle.next_frame().await);
    assert!(log.calls().iter().any(|call| call == "logout"));

    // An unadvertised method id is rejected before any provider call.
    handle.send(request(4, "auth/login", json!({ "methodId": "ghost" })));
    let error = request_error(handle.next_frame().await);
    assert_eq!(i32::from(error.code), -32602, "unadvertised method rejected: {error:?}");

    handle.shutdown(task).await;
}

/// With no advertised `authMethods` the v2 auth methods fail closed
/// (clients must not call them, and agents must not serve them).
#[tokio::test(flavor = "current_thread")]
async fn v2_auth_without_advertised_methods_fails_closed() {
    let (provider, _log) = FakeProvider::new(&["provider-session-1"]);
    let (handle, task) = spawn_server(provider).await;
    let session_id = v2_handshake_and_session(&handle).await;

    handle.send(request(3, "auth/login", json!({ "methodId": "any" })));
    let error = request_error(handle.next_frame().await);
    assert_eq!(i32::from(error.code), -32602, "no methods advertised: {error:?}");

    handle.send(request(4, "auth/logout", json!({})));
    let error = request_error(handle.next_frame().await);
    assert_eq!(i32::from(error.code), -32602, "logout without auth surface: {error:?}");
    assert!(session_id.starts_with("provider-session"), "session id sanity");

    handle.shutdown(task).await;
}

/// `auth/login` is a v2-only method; v1 connections must not see it.
#[tokio::test(flavor = "current_thread")]
async fn v1_connection_rejects_auth_login_as_unknown_method() {
    let (provider, _log) = FakeProvider::new(&["provider-session-1"]);
    let (handle, task) = spawn_server(provider).await;

    handle.send(request(1, "initialize", json!({ "protocolVersion": 1 })));
    request_result(handle.next_frame().await);

    handle.send(request(2, "auth/login", json!({ "methodId": "any" })));
    let error = request_error(handle.next_frame().await);
    assert_eq!(i32::from(error.code), -32601, "auth/login is not a v1 method");

    handle.shutdown(task).await;
}

// ── Phase 3: session/resume replayFrom ───────────────────────────────────

/// v2 `session/resume` with an explicit start cursor replays the history as
/// ordinary v2 updates BEFORE the deferred response, matching the v1
/// `session/load` ordering guarantee.
#[tokio::test(flavor = "current_thread")]
async fn v2_resume_with_replay_from_replays_history_before_response() {
    let (provider, _log) = FakeProvider::new(&["provider-session-1"]);
    let provider = provider.with_replay(vec![("user", "first message"), ("assistant", "echo")]);
    let (handle, task) = spawn_server(provider).await;
    let session_id = v2_handshake_and_session(&handle).await;

    handle.send(request(
        3,
        "session/resume",
        json!({
            "sessionId": session_id,
            "cwd": "/work",
            "replayFrom": { "type": "start" },
        }),
    ));

    // Replay updates first: user chunk then agent chunk, both with ids.
    let user = expect_v2_update(handle.next_frame().await, &session_id);
    assert_eq!(user["sessionUpdate"], "user_message_chunk");
    assert_eq!(user["messageId"], "replay-u-1");
    assert_eq!(user["content"]["text"], "first message");

    let agent = expect_v2_update(handle.next_frame().await, &session_id);
    assert_eq!(agent["sessionUpdate"], "agent_message_chunk");
    assert_eq!(agent["messageId"], "replay-a-2");
    assert_eq!(agent["content"]["text"], "echo");

    // Then the deferred resume response.
    request_result(handle.next_frame().await);
    assert!(handle.outbound().is_empty(), "no frames left after the resume response");

    handle.shutdown(task).await;
}

/// Unknown `replayFrom` cursors are rejected rather than guessed (the v2 spec
/// requires receivers to reject cursors they do not understand).
#[tokio::test(flavor = "current_thread")]
async fn v2_resume_unknown_replay_cursor_is_rejected() {
    let (provider, _log) = FakeProvider::new(&["provider-session-1"]);
    let (handle, task) = spawn_server(provider).await;
    let session_id = v2_handshake_and_session(&handle).await;

    handle.send(request(
        3,
        "session/resume",
        json!({
            "sessionId": session_id,
            "cwd": "/work",
            "replayFrom": { "type": "message", "messageId": "m-9" },
        }),
    ));
    let error = request_error(handle.next_frame().await);
    assert_eq!(i32::from(error.code), -32602, "unknown cursors must be rejected: {error:?}");
    assert!(common::error_reason(&error).contains("replayFrom"));

    handle.shutdown(task).await;
}

// ── Phase 3: requires_action foreground state ────────────────────────────

/// While an agent → client request is pending on a v2 connection the
/// foreground state reports `requires_action`; once the client answers the
/// turn reports `running` again before completing with idle.  Uses
/// `mcp/message` — the v2-native agent → client request — because the
/// fs/terminal client surface is removed in v2.
#[tokio::test(flavor = "current_thread")]
async fn v2_pending_client_request_reports_requires_action_then_running() {
    let (provider, _log) = FakeProvider::new(&["provider-session-1"]);
    provider.set_prompt_behavior(
        "provider-session-1",
        PromptBehavior::McpMessageAndContinue { connection_id: "conn-1".to_string() },
    );
    let (handle, task) = spawn_server(provider).await;
    let session_id = v2_handshake_and_session(&handle).await;

    handle.send(request(5, "session/prompt", prompt_params(&session_id)));
    request_result(handle.next_frame().await); // ack
    expect_v2_update(handle.next_frame().await, &session_id); // user_message
    let running = expect_v2_update(handle.next_frame().await, &session_id);
    assert_eq!(running["state"], "running");

    // The pending client request flips foreground state.
    let blocked = expect_v2_update(handle.next_frame().await, &session_id);
    assert_eq!(blocked["sessionUpdate"], "state_update");
    assert_eq!(blocked["state"], "requires_action");

    // The outbound client request follows, then the client answers it.
    let RawJsonRpcMessage::Request(request) = handle.next_frame().await else {
        panic!("expected the outbound mcp/message frame");
    };
    assert_eq!(request.method.as_ref(), "mcp/message");
    handle.send(RawJsonRpcMessage::response(request.id, Ok(json!({ "resultType": "complete" }))));

    // Foreground work resumes, then the turn completes idle.
    let resumed = expect_v2_update(handle.next_frame().await, &session_id);
    assert_eq!(resumed["state"], "running");
    let idle = expect_v2_update(handle.next_frame().await, &session_id);
    assert_eq!(idle["sessionUpdate"], "state_update");
    assert_eq!(idle["state"], "idle");
    assert_eq!(idle["stopReason"], "end_turn");

    handle.shutdown(task).await;
}

// ── Phase 3: tool-call chunk + provider-keyed plan streaming ──────────────

/// Providers stream tool-call content item-by-item via
/// `tool_call_content_chunk` and key plans by their own ids; both ride the
/// v2 update surface.
#[tokio::test(flavor = "current_thread")]
async fn v2_streamed_tool_call_chunk_and_provider_plan_update() {
    let (provider, _log) = FakeProvider::new(&["provider-session-1"]);
    provider
        .set_prompt_behavior("provider-session-1", PromptBehavior::EmitV2ChunkAndPlanThenReturn);
    let (handle, task) = spawn_server(provider).await;
    let session_id = v2_handshake_and_session(&handle).await;

    handle.send(request(5, "session/prompt", prompt_params(&session_id)));
    request_result(handle.next_frame().await); // ack
    expect_v2_update(handle.next_frame().await, &session_id); // user_message
    expect_v2_update(handle.next_frame().await, &session_id); // running

    let tool_call = expect_v2_update(handle.next_frame().await, &session_id);
    assert_eq!(tool_call["sessionUpdate"], "tool_call_update");
    assert_eq!(tool_call["toolCallId"], "tc-1");

    let chunk = expect_v2_update(handle.next_frame().await, &session_id);
    assert_eq!(chunk["sessionUpdate"], "tool_call_content_chunk");
    assert_eq!(chunk["toolCallId"], "tc-1");
    assert_eq!(chunk["content"]["type"], "content");
    assert_eq!(chunk["content"]["content"]["type"], "text");
    assert_eq!(chunk["content"]["content"]["text"], "streamed");

    let plan = expect_v2_update(handle.next_frame().await, &session_id);
    assert_eq!(plan["sessionUpdate"], "plan_update");
    assert_eq!(plan["plan"]["type"], "items");
    assert_eq!(plan["plan"]["planId"], "plan-42", "provider-chosen plan id is preserved");
    assert_eq!(plan["plan"]["entries"][0]["content"], "pipeline");

    let idle = expect_v2_update(handle.next_frame().await, &session_id);
    assert_eq!(idle["sessionUpdate"], "state_update");
    assert_eq!(idle["state"], "idle");
    assert_eq!(idle["stopReason"], "end_turn");

    handle.shutdown(task).await;
}

// ── Phase 3: MCP capability reconciliation + command input discriminator ──

/// The v1 MCP capability flags reconcile onto the v2 `session.mcp` transport
/// table: stdio baseline, http/acp carried, removed sse never advertised.
#[tokio::test(flavor = "current_thread")]
async fn v2_initialize_advertises_mcp_transports_from_v1_caps() {
    let (provider, _log) = FakeProvider::new(&["provider-session-1"]);
    let provider = provider.with_mcp_http().with_mcp_acp();
    let (handle, task) = spawn_server(provider).await;

    handle.send(v2_initialize_request());
    let result = request_result(handle.next_frame().await);
    let mcp = &result["capabilities"]["session"]["mcp"];
    assert!(mcp.get("stdio").is_some(), "stdio baseline must be advertised: {mcp}");
    assert!(mcp.get("http").is_some(), "v1 http carried over: {mcp}");
    assert!(mcp.get("acp").is_some(), "v1 acp (mcp-over-acp) carried over: {mcp}");
    assert!(mcp.get("sse").is_none(), "removed sse must never appear: {mcp}");

    handle.shutdown(task).await;
}

/// Slash commands emitted as `available_commands_update` carry the required
/// `type: "text"` input discriminator on the v2 wire.
#[tokio::test(flavor = "current_thread")]
async fn v2_available_commands_carry_text_input_discriminator() {
    use ee_agent_protocol::{AvailableCommand, AvailableCommandInput, UnstructuredCommandInput};
    let commands = vec![AvailableCommand::new("compact", "Summarize the session").input(
        AvailableCommandInput::Unstructured(UnstructuredCommandInput::new("optional notes")),
    )];
    let (provider, _log) = FakeProvider::new(&["provider-session-1"]);
    let provider = provider.with_commands(commands);
    let (handle, task) = spawn_server(provider).await;

    handle.send(v2_initialize_request());
    request_result(handle.next_frame().await);
    handle.send(request(2, "session/new", session_new_params("/work")));
    request_result(handle.next_frame().await);

    let commands_update = expect_v2_update(handle.next_frame().await, "provider-session-1");
    assert_eq!(commands_update["sessionUpdate"], "available_commands_update");
    assert_eq!(commands_update["availableCommands"][0]["name"], "compact");
    assert_eq!(
        commands_update["availableCommands"][0]["input"]["type"], "text",
        "slash-command inputs need the type discriminator: {commands_update}"
    );
    assert_eq!(commands_update["availableCommands"][0]["input"]["hint"], "optional notes");

    handle.shutdown(task).await;
}
