//! End-to-end server flows over the in-memory transport: initialize
//! negotiation, session lifecycle, prompt execution, and error shaping.
//!
//! The fake provider records every call, so tests can prove that malformed
//! requests are rejected *before* any provider invocation.

// Shared harness serves multiple integration-test crates; each uses a different subset.
#[allow(dead_code)]
mod common;

use std::path::PathBuf;

use ee_agent_protocol::{
    AvailableCommand, AvailableCommandInput, InitializeResponse, ListSessionsResponse,
    LoadSessionResponse, NewSessionResponse, ProtocolVersion, RawJsonRpcMessage,
    ResumeSessionResponse, SessionId, SessionMode, SessionModeState, UnstructuredCommandInput,
};
use serde_json::json;

use common::{
    FakeProvider, PromptBehavior, error_reason, notification, prompt_params, raw_params_to_value,
    request, request_error, request_result, session_new_params, spawn_server, wait_for_log,
};

/// The command list advertised in the fake provider's session inits.
fn advertised_commands() -> Vec<AvailableCommand> {
    vec![
        AvailableCommand::new("compact", "Summarize the session history").input(
            AvailableCommandInput::Unstructured(UnstructuredCommandInput::new(
                "optional instructions",
            )),
        ),
        AvailableCommand::new("plan", "Create a plan"),
    ]
}

/// Asserts the next frame is an `available_commands_update` carrying exactly
/// the advertised commands for the given session.
fn expect_available_commands_update(frame: RawJsonRpcMessage, session_id: &str) {
    let RawJsonRpcMessage::Notification(notification) = frame else {
        panic!("expected an update notification, got {frame:?}");
    };
    assert_eq!(notification.method.as_ref(), "session/update");
    let params = common::raw_params_to_value(notification.params.clone());
    assert_eq!(params["sessionId"], session_id);
    assert_eq!(params["update"]["sessionUpdate"], "available_commands_update");
    let commands = params["update"]["availableCommands"].as_array().expect("commands array");
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0]["name"], "compact");
    assert_eq!(commands[0]["description"], "Summarize the session history");
    assert_eq!(commands[0]["input"]["hint"], "optional instructions");
    assert_eq!(commands[1]["name"], "plan");
    assert_eq!(commands[1]["description"], "Create a plan");
}

// ── available_commands_update advertisement ──────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn session_new_emits_available_commands_update_after_response() {
    let (provider, _log) = FakeProvider::new(&["provider-session-1"]);
    let provider = provider.with_commands(advertised_commands());
    let (handle, task) = spawn_server(provider).await;

    handle.send(request(1, "session/new", session_new_params("/work")));
    let frames = handle.next_frames(2).await;

    // Response first (the session must be registered client-side before the
    // update references it), then the command advertisement.
    let response: NewSessionResponse =
        serde_json::from_value(request_result(frames[0].clone())).expect("parses response");
    assert_eq!(response.session_id, SessionId::new("provider-session-1"));
    expect_available_commands_update(frames[1].clone(), "provider-session-1");

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn session_load_emits_available_commands_update_before_response() {
    let (provider, _log) = FakeProvider::new(&[]);
    let provider = provider.with_commands(advertised_commands());
    let (handle, task) = spawn_server(provider).await;

    let params = json!({
        "sessionId": "loaded-session",
        "cwd": "/work",
        "additionalDirectories": [],
        "mcpServers": [],
    });
    handle.send(request(1, "session/load", params));
    let frames = handle.next_frames(2).await;

    // `session/load` is deferred: every update the provider queues (the
    // command advertisement included) streams before the response, matching
    // the ACP v1 replay-then-respond contract.
    expect_available_commands_update(frames[0].clone(), "loaded-session");
    let _response: LoadSessionResponse =
        serde_json::from_value(request_result(frames[1].clone())).expect("parses response");

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn session_new_without_commands_emits_no_available_commands_update() {
    let (provider, _log) = FakeProvider::new(&["provider-session-1"]);
    let (handle, task) = spawn_server(provider).await;

    handle.send(request(1, "session/new", session_new_params("/work")));
    let _ = request_result(handle.next_frame().await);

    // No command advertisement follows: the provider exposed no commands,
    // and the server keeps serving further requests.
    handle.send(request(2, "session/list", json!({})));
    let result = request_result(handle.next_frame().await);
    let response: ListSessionsResponse =
        serde_json::from_value(result).expect("parses as ListSessionsResponse");
    assert_eq!(response.sessions.len(), 1);
    assert!(handle.outbound().is_empty(), "no update frames queued: {:?}", handle.outbound());

    handle.shutdown(task).await;
}

// ── initialize ───────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn initialize_with_protocol_v1_succeeds() {
    let (provider, _log) = FakeProvider::new(&[]);
    let (handle, task) = spawn_server(provider).await;

    handle.send(request(1, "initialize", json!({ "protocolVersion": 1 })));
    let result = request_result(handle.next_frame().await);

    let response: InitializeResponse =
        serde_json::from_value(result).expect("response parses as InitializeResponse");
    assert_eq!(response.protocol_version, ProtocolVersion::V1);
    assert_eq!(response.agent_info.expect("agent info").name, "fake-provider");
    assert!(response.agent_capabilities.load_session);
    let framework =
        response.meta.expect("framework metadata").get("framework").expect("framework key").clone();
    assert_eq!(framework["name"], "ee-acp-agent-server");

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn initialize_with_protocol_v0_fails_closed() {
    let (provider, _log) = FakeProvider::new(&[]);
    let (handle, task) = spawn_server(provider).await;

    handle.send(request(1, "initialize", json!({ "protocolVersion": 0 })));
    let error = request_error(handle.next_frame().await);
    assert_eq!(i32::from(error.code), -32600);
    assert!(error.message.contains("unsupported protocol version: 0"));

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn initialize_with_protocol_v2_fails_closed() {
    let (provider, _log) = FakeProvider::new(&[]);
    let (handle, task) = spawn_server(provider).await;

    handle.send(request(1, "initialize", json!({ "protocolVersion": 2 })));
    let error = request_error(handle.next_frame().await);
    assert_eq!(i32::from(error.code), -32600);
    assert!(error.message.contains("unsupported protocol version: 2"));

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn initialize_with_malformed_params_fails() {
    let (provider, _log) = FakeProvider::new(&[]);
    let (handle, task) = spawn_server(provider).await;

    handle.send(request(1, "initialize", json!({ "nope": true })));
    let error = request_error(handle.next_frame().await);
    assert_eq!(i32::from(error.code), -32602);

    handle.shutdown(task).await;
}

// ── session/new ──────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn session_new_stores_session_and_returns_provider_id() {
    let (provider, log) = FakeProvider::new(&["provider-session-1"]);
    let (handle, task) = spawn_server(provider).await;

    handle.send(request(1, "session/new", session_new_params("/work")));
    let result = request_result(handle.next_frame().await);
    let response: NewSessionResponse =
        serde_json::from_value(result).expect("parses as NewSessionResponse");
    assert_eq!(response.session_id, SessionId::new("provider-session-1"));

    // The registered session is visible through session/list.
    handle.send(request(2, "session/list", json!({})));
    let result = request_result(handle.next_frame().await);
    let response: ListSessionsResponse =
        serde_json::from_value(result).expect("parses as ListSessionsResponse");
    assert_eq!(response.sessions.len(), 1);
    assert_eq!(response.sessions[0].session_id, SessionId::new("provider-session-1"));
    assert_eq!(response.sessions[0].cwd, PathBuf::from("/work"));
    assert_eq!(response.sessions[0].title.as_deref(), Some("Test Session"));
    assert!(log.has_call("new_session:/work"));

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn session_new_rejects_relative_cwd_before_provider_call() {
    let (provider, log) = FakeProvider::new(&["never-called"]);
    let (handle, task) = spawn_server(provider).await;

    handle.send(request(1, "session/new", session_new_params("relative/dir")));
    let error = request_error(handle.next_frame().await);
    assert_eq!(i32::from(error.code), -32602);
    assert!(error_reason(&error).contains("cwd must be an absolute path"));

    // Criterion: provider records prove the provider was never invoked.
    assert!(!log.has_call("new_session"));
    assert!(
        log.calls().is_empty(),
        "provider must not be called for malformed requests: {:?}",
        log.calls()
    );

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn session_new_rejects_relative_additional_directory_before_provider_call() {
    let (provider, log) = FakeProvider::new(&["never-called"]);
    let (handle, task) = spawn_server(provider).await;

    let params = json!({
        "cwd": "/work",
        "additionalDirectories": ["/abs", "relative/extra"],
        "mcpServers": [],
    });
    handle.send(request(1, "session/new", params));
    let error = request_error(handle.next_frame().await);
    assert_eq!(i32::from(error.code), -32602);
    assert!(error_reason(&error).contains("additional directory must be an absolute path"));
    assert!(!log.has_call("new_session"));

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn session_new_with_malformed_params_fails_before_provider_call() {
    let (provider, log) = FakeProvider::new(&["never-called"]);
    let (handle, task) = spawn_server(provider).await;

    // Array params do not match the NewSessionRequest object shape.
    handle.send(request(1, "session/new", json!([1, 2, 3])));
    let error = request_error(handle.next_frame().await);
    assert_eq!(i32::from(error.code), -32602);
    assert!(!log.has_call("new_session"));

    handle.shutdown(task).await;
}

// ── session/load ─────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn session_load_stores_session_and_returns_loaded_id() {
    let (provider, log) = FakeProvider::new(&[]);
    let (handle, task) = spawn_server(provider).await;

    let params = json!({
        "sessionId": "loaded-session",
        "cwd": "/work",
        "additionalDirectories": [],
        "mcpServers": [],
    });
    handle.send(request(1, "session/load", params));
    let result = request_result(handle.next_frame().await);
    let _response: LoadSessionResponse =
        serde_json::from_value(result).expect("parses as LoadSessionResponse");

    handle.send(request(2, "session/list", json!({})));
    let result = request_result(handle.next_frame().await);
    let response: ListSessionsResponse =
        serde_json::from_value(result).expect("parses as ListSessionsResponse");
    assert_eq!(response.sessions.len(), 1);
    assert_eq!(response.sessions[0].session_id, SessionId::new("loaded-session"));
    assert!(log.has_call("load_session:loaded-session"));

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn session_load_rejects_relative_cwd_before_provider_call() {
    let (provider, log) = FakeProvider::new(&[]);
    let (handle, task) = spawn_server(provider).await;

    let params = json!({
        "sessionId": "loaded-session",
        "cwd": "relative/dir",
        "additionalDirectories": [],
        "mcpServers": [],
    });
    handle.send(request(1, "session/load", params));
    let error = request_error(handle.next_frame().await);
    assert_eq!(i32::from(error.code), -32602);
    assert!(!log.has_call("load_session"));

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn session_load_streams_conversation_replay_before_response() {
    // ACP v1: `session/load` replays the whole conversation as
    // `session/update` notifications and only then responds `null`.
    let (provider, _log) = FakeProvider::new(&[]);
    let provider = provider.with_replay(vec![
        ("user", "what's the capital of France?"),
        ("agent", "Paris."),
        ("user", "and of Spain?"),
        ("agent", "Madrid."),
    ]);
    let (handle, task) = spawn_server(provider).await;

    let params = json!({
        "sessionId": "loaded-session",
        "cwd": "/work",
        "additionalDirectories": [],
        "mcpServers": [],
    });
    handle.send(request(1, "session/load", params));
    let frames = handle.next_frames(5).await;

    // The four replayed messages stream first, in order, with deterministic
    // ids; the load response follows last.
    let expected = [
        ("user_message_chunk", "replay-u-1", "what's the capital of France?"),
        ("agent_message_chunk", "replay-a-2", "Paris."),
        ("user_message_chunk", "replay-u-3", "and of Spain?"),
        ("agent_message_chunk", "replay-a-4", "Madrid."),
    ];
    for (index, (kind, message_id, text)) in expected.iter().enumerate() {
        let RawJsonRpcMessage::Notification(update) = &frames[index] else {
            panic!("expected replay update {}, got {:?}", index, frames[index]);
        };
        let params = raw_params_to_value(update.params.clone());
        assert_eq!(params["sessionId"], "loaded-session");
        assert_eq!(params["update"]["sessionUpdate"], *kind);
        assert_eq!(params["update"]["messageId"], *message_id);
        assert_eq!(params["update"]["content"]["text"], *text);
    }
    let _response: LoadSessionResponse =
        serde_json::from_value(request_result(frames[4].clone())).expect("parses response");

    // The loaded session is registered and promptable afterwards.
    handle.send(request(2, "session/list", json!({})));
    let result = request_result(handle.next_frame().await);
    let response: ListSessionsResponse =
        serde_json::from_value(result).expect("parses as ListSessionsResponse");
    assert_eq!(response.sessions.len(), 1);
    assert_eq!(response.sessions[0].session_id, SessionId::new("loaded-session"));

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn session_load_failure_removes_provisional_session() {
    let (provider, _log) = FakeProvider::new(&[]);
    let provider = provider.with_load_error("no persisted orchestrator state");
    let (handle, task) = spawn_server(provider).await;

    let params = json!({
        "sessionId": "ghost-session",
        "cwd": "/work",
        "additionalDirectories": [],
        "mcpServers": [],
    });
    handle.send(request(1, "session/load", params));
    let error = request_error(handle.next_frame().await);
    assert!(error.message.contains("no persisted orchestrator state"), "{error:?}");

    // The failed load left no session behind: a prompt is rejected.
    handle.send(request(
        2,
        "session/prompt",
        json!({
            "sessionId": "ghost-session",
            "prompt": [{ "type": "text", "text": "hello" }],
        }),
    ));
    let error = request_error(handle.next_frame().await);
    assert!(error.message.contains("session"), "unknown session rejected: {error:?}");

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn session_load_rejects_already_registered_session() {
    let (provider, _log) = FakeProvider::new(&[]);
    let (handle, task) = spawn_server(provider).await;

    let params = json!({
        "sessionId": "dupe-session",
        "cwd": "/work",
        "additionalDirectories": [],
        "mcpServers": [],
    });
    handle.send(request(1, "session/load", params.clone()));
    let _ = request_result(handle.next_frame().await);
    // A second load of the same id is a duplicate; reconnecting clients use
    // `session/resume`.
    handle.send(request(2, "session/load", params));
    let error = request_error(handle.next_frame().await);
    assert!(
        error_reason(&error).contains("already registered"),
        "duplicate load rejected: {error:?}"
    );

    handle.shutdown(task).await;
}

// ── session/resume ────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn session_resume_registers_session_and_returns_empty_result() {
    let (provider, log) = FakeProvider::new(&[]);
    let provider = provider.with_commands(advertised_commands());
    let (handle, task) = spawn_server(provider).await;

    let params = json!({
        "sessionId": "resumed-session",
        "cwd": "/work",
        "additionalDirectories": [],
        "mcpServers": [],
    });
    handle.send(request(1, "session/resume", params));
    let frames = handle.next_frames(2).await;

    let _response: ResumeSessionResponse =
        serde_json::from_value(request_result(frames[0].clone())).expect("parses response");
    expect_available_commands_update(frames[1].clone(), "resumed-session");
    assert!(log.has_call("load_session:resumed-session"), "default resume delegates to load");

    // The resumed session is registered and promptable.
    handle.send(request(
        2,
        "session/prompt",
        json!({
            "sessionId": "resumed-session",
            "prompt": [{ "type": "text", "text": "hello" }],
        }),
    ));
    let result = request_result(handle.next_frame().await);
    assert_eq!(result["stopReason"], "end_turn");

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn session_resume_rejects_relative_cwd_before_provider_call() {
    let (provider, log) = FakeProvider::new(&[]);
    let (handle, task) = spawn_server(provider).await;

    let params = json!({
        "sessionId": "resumed-session",
        "cwd": "relative/dir",
        "additionalDirectories": [],
        "mcpServers": [],
    });
    handle.send(request(1, "session/resume", params));
    let error = request_error(handle.next_frame().await);
    assert_eq!(i32::from(error.code), -32602);
    assert!(!log.has_call("load_session"));

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn session_resume_without_pending_state_is_rejected() {
    let (provider, _log) = FakeProvider::new(&[]);
    let provider =
        provider.with_load_error("no pending checkpoint for session x; nothing to resume");
    let (handle, task) = spawn_server(provider).await;

    let params = json!({
        "sessionId": "ghost-session",
        "cwd": "/work",
        "additionalDirectories": [],
        "mcpServers": [],
    });
    handle.send(request(1, "session/resume", params));
    let error = request_error(handle.next_frame().await);
    assert!(error.message.contains("nothing to resume"), "provider rejection surfaces: {error:?}");
    // No session was registered.
    handle.send(request(2, "session/list", json!({})));
    let result = request_result(handle.next_frame().await);
    let response: ListSessionsResponse =
        serde_json::from_value(result).expect("parses as ListSessionsResponse");
    assert!(response.sessions.is_empty(), "failed resume registers nothing");

    handle.shutdown(task).await;
}

// ── session/set_mode ─────────────────────────────────────────────────────

fn advertised_modes() -> SessionModeState {
    SessionModeState::new(
        "ask",
        vec![SessionMode::new("ask", "Ask"), SessionMode::new("plan", "Plan")],
    )
}

fn set_mode_params(session_id: &str, mode_id: &str) -> serde_json::Value {
    json!({ "sessionId": session_id, "modeId": mode_id })
}

#[tokio::test(flavor = "current_thread")]
async fn session_set_mode_rejects_unadvertised_mode_before_provider_call() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    let provider = provider.with_modes(advertised_modes());
    let (handle, task) = spawn_server(provider).await;

    handle.send(request(1, "session/new", session_new_params("/work")));
    let _ = request_result(handle.next_frame().await);

    handle.send(request(2, "session/set_mode", set_mode_params("session-a", "agent")));
    let error = request_error(handle.next_frame().await);
    assert_eq!(i32::from(error.code), -32602);
    assert!(error_reason(&error).contains("not advertised"), "{error:?}");
    assert!(!log.has_call("set_mode:"), "unadvertised mode reached provider: {:?}", log.calls());

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn session_set_mode_calls_provider_after_advertised_mode_validation() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    let provider = provider.with_modes(advertised_modes());
    let (handle, task) = spawn_server(provider).await;

    handle.send(request(1, "session/new", session_new_params("/work")));
    let _ = request_result(handle.next_frame().await);

    handle.send(request(2, "session/set_mode", set_mode_params("session-a", "plan")));
    assert_eq!(request_result(handle.next_frame().await), json!({}));
    assert!(log.has_call("set_mode:session-a:plan"), "provider received mode change");

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn session_set_mode_provider_failure_keeps_prior_selected_mode() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    let provider = provider.with_modes(advertised_modes());
    let (handle, task) = spawn_server(provider.clone()).await;

    handle.send(request(1, "session/new", session_new_params("/work")));
    let _ = request_result(handle.next_frame().await);

    // First change succeeds, establishing `plan` as selected mode. The next
    // provider failure must leave that prior framework mode intact.
    handle.send(request(2, "session/set_mode", set_mode_params("session-a", "plan")));
    let _ = request_result(handle.next_frame().await);
    provider.set_mode_error(Some("fake mode provider boom"));

    handle.send(request(3, "session/set_mode", set_mode_params("session-a", "ask")));
    let error = request_error(handle.next_frame().await);
    assert!(error.message.contains("fake mode provider boom"), "{error:?}");
    wait_for_log(&log, |calls| calls.iter().any(|call| call == "set_mode:session-a:ask:failed"))
        .await;
    assert_eq!(
        log.calls().iter().filter(|call| call.as_str() == "set_mode:session-a:plan").count(),
        1,
        "only prior successful mode change is recorded: {:?}",
        log.calls()
    );

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn session_set_mode_is_rejected_while_prompt_is_active() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    let provider = provider.with_modes(advertised_modes());
    provider.set_prompt_behavior("session-a", PromptBehavior::AwaitCancelThenCancelled);
    let (handle, task) = spawn_server(provider).await;

    handle.send(request(1, "session/new", session_new_params("/work")));
    let _ = request_result(handle.next_frame().await);
    handle.send(request(2, "session/prompt", prompt_params("session-a")));
    wait_for_log(&log, |calls| calls.iter().any(|call| call == "prompt:session-a:started")).await;

    handle.send(request(3, "session/set_mode", set_mode_params("session-a", "plan")));
    let error = request_error(handle.next_frame().await);
    assert_eq!(i32::from(error.code), -32602);
    assert!(error_reason(&error).contains("is active"), "{error:?}");
    assert!(!log.has_call("set_mode:"), "mode change reached provider while prompt active");

    handle.send(notification("session/cancel", json!({ "sessionId": "session-a" })));
    let result = request_result(handle.next_frame().await);
    assert_eq!(result["stopReason"], "cancelled");

    handle.shutdown(task).await;
}

// ── session/list ─────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn session_list_returns_stable_sorted_order() {
    let (provider, log) = FakeProvider::new(&["zeta-session", "alpha-session"]);
    let (handle, task) = spawn_server(provider).await;

    // Insert out of sort order on purpose: zeta first, then alpha.
    handle.send(request(1, "session/new", session_new_params("/work")));
    let _ = request_result(handle.next_frame().await);
    handle.send(request(2, "session/new", session_new_params("/other")));
    let _ = request_result(handle.next_frame().await);

    handle.send(request(3, "session/list", json!({})));
    let result = request_result(handle.next_frame().await);
    let response: ListSessionsResponse =
        serde_json::from_value(result).expect("parses as ListSessionsResponse");
    let ids: Vec<String> =
        response.sessions.iter().map(|session| session.session_id.to_string()).collect();
    assert_eq!(ids, vec!["alpha-session", "zeta-session"]);

    // Both sessions went through the provider; listing itself is served
    // from the framework store.
    assert_eq!(log.calls().iter().filter(|call| call.starts_with("new_session")).count(), 2);

    handle.shutdown(task).await;
}

// ── session/close ────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn session_close_removes_session_and_calls_provider() {
    let (provider, log) = FakeProvider::new(&["provider-session-1"]);
    let (handle, task) = spawn_server(provider).await;

    handle.send(request(1, "session/new", session_new_params("/work")));
    let _ = request_result(handle.next_frame().await);

    handle.send(request(2, "session/close", json!({ "sessionId": "provider-session-1" })));
    let result = request_result(handle.next_frame().await);
    assert_eq!(result, json!({}));
    assert!(log.has_call("close_session:provider-session-1"));

    handle.send(request(3, "session/list", json!({})));
    let result = request_result(handle.next_frame().await);
    let response: ListSessionsResponse =
        serde_json::from_value(result).expect("parses as ListSessionsResponse");
    assert!(response.sessions.is_empty(), "session must be removed");

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn session_close_unknown_session_returns_invalid_params() {
    let (provider, log) = FakeProvider::new(&[]);
    let (handle, task) = spawn_server(provider).await;

    handle.send(request(1, "session/close", json!({ "sessionId": "ghost" })));
    let error = request_error(handle.next_frame().await);
    assert_eq!(i32::from(error.code), -32602);
    assert!(error.message.contains("unknown session: ghost"));
    assert!(!log.has_call("close_session"));

    handle.shutdown(task).await;
}

// ── session/prompt ───────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn prompt_returns_stop_reason() {
    let (provider, _log) = FakeProvider::new(&["session-a"]);
    let (handle, task) = spawn_server(provider).await;

    handle.send(request(1, "session/new", session_new_params("/work")));
    let _ = request_result(handle.next_frame().await);

    handle.send(request(2, "session/prompt", prompt_params("session-a")));
    let result = request_result(handle.next_frame().await);
    assert_eq!(result["stopReason"], "end_turn");

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_on_unknown_session_is_rejected_before_provider_call() {
    let (provider, log) = FakeProvider::new(&[]);
    let (handle, task) = spawn_server(provider).await;

    handle.send(request(1, "session/prompt", prompt_params("ghost")));
    let error = request_error(handle.next_frame().await);
    assert_eq!(i32::from(error.code), -32602);
    assert!(error.message.contains("unknown session: ghost"));
    assert!(
        !log.calls().iter().any(|call| call.starts_with("prompt:")),
        "provider prompt must not run for unknown sessions"
    );

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn update_is_emitted_before_prompt_response() {
    let (provider, _log) = FakeProvider::new(&["session-a"]);
    provider.set_prompt_behavior("session-a", PromptBehavior::EmitMessageThenReturn);
    let (handle, task) = spawn_server(provider).await;

    handle.send(request(1, "session/new", session_new_params("/work")));
    let _ = request_result(handle.next_frame().await);

    handle.send(request(2, "session/prompt", prompt_params("session-a")));
    let frames = handle.next_frames(2).await;

    // First: the session/update notification.
    let RawJsonRpcMessage::Notification(update_notification) = &frames[0] else {
        panic!("first frame must be the update notification, got {:?}", frames[0]);
    };
    assert_eq!(update_notification.method.as_ref(), "session/update");
    let update = common::raw_params_to_value(update_notification.params.clone());
    assert_eq!(update["sessionId"], "session-a");
    assert_eq!(update["update"]["sessionUpdate"], "agent_message_chunk");
    assert_eq!(update["update"]["content"]["text"], "hello from provider");

    // Second: the prompt response, after the update.
    let result = request_result(frames[1].clone());
    assert_eq!(result["stopReason"], "end_turn");

    handle.shutdown(task).await;
}

// ── Unknown methods and notifications ────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn unknown_request_returns_method_not_found() {
    let (provider, _log) = FakeProvider::new(&[]);
    let (handle, task) = spawn_server(provider).await;

    handle.send(request(1, "bogus/method", json!({})));
    let error = request_error(handle.next_frame().await);
    assert_eq!(i32::from(error.code), -32601);

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_notification_is_ignored() {
    let (provider, _log) = FakeProvider::new(&[]);
    let (handle, task) = spawn_server(provider).await;

    handle.send(notification("bogus/notification", json!({})));

    // The notification must not produce a response; the server keeps
    // serving requests afterwards.
    handle.send(request(1, "initialize", json!({ "protocolVersion": 1 })));
    let result = request_result(handle.next_frame().await);
    let response: InitializeResponse =
        serde_json::from_value(result).expect("parses as InitializeResponse");
    assert_eq!(response.protocol_version, ProtocolVersion::V1);
    assert!(handle.outbound().is_empty(), "notification must not produce a response");

    handle.shutdown(task).await;
}
