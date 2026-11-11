//! Agent → client request bridge over the in-memory transport: outbound
//! `fs/read_text_file` and `terminal/create` requests, response correlation,
//! error mapping, timeouts, transport-close cleanup, and validation that
//! relative paths never reach the transport.

// Shared harness serves multiple integration-test crates; each uses a different subset.
#[allow(dead_code)]
mod common;

use std::time::Duration;

use ee_agent_protocol::{Error as RpcError, ListSessionsResponse, RawJsonRpcMessage, RequestId};
use serde_json::{Value, json};

use common::{
    FakeProvider, PromptBehavior, prompt_params, request, request_error, request_result,
    session_new_params, spawn_server, spawn_server_with_config, wait_for_log,
};

/// Starts a session and returns its id.
async fn new_session(handle: &common::Harness, id: i64) -> String {
    handle.send(request(id, "session/new", session_new_params("/work")));
    let result = request_result(handle.next_frame().await);
    result["sessionId"].as_str().expect("session id").to_string()
}

/// Answers the captured request frame with the given result/error.
fn respond_to(frame: &RawJsonRpcMessage, response: Result<Value, RpcError>) -> RawJsonRpcMessage {
    let RawJsonRpcMessage::Request(request) = frame else {
        panic!("expected a request frame, got {frame:?}");
    };
    RawJsonRpcMessage::response(request.id.clone(), response)
}

/// Starts a prompt whose provider calls `read_text_file`, then returns the
/// outbound `fs/read_text_file` request frame.
async fn prompt_read_text_file(handle: &common::Harness) -> RawJsonRpcMessage {
    handle.send(request(2, "session/prompt", prompt_params("session-a")));
    let frames = handle.next_frames(1).await;
    let RawJsonRpcMessage::Request(request) = &frames[0] else {
        panic!("first frame must be the fs request, got {:?}", frames[0]);
    };
    assert_eq!(request.method.as_ref(), "fs/read_text_file");
    frames[0].clone()
}

// ── Outbound request emission and response correlation ───────────────────

#[tokio::test(flavor = "current_thread")]
async fn provider_read_text_file_emits_fs_request() {
    let (provider, _log) = FakeProvider::new(&["session-a"]);
    provider.set_prompt_behavior(
        "session-a",
        PromptBehavior::ReadTextFile { path: "/tmp/notes.txt".into() },
    );
    let (handle, task) = spawn_server(provider).await;
    new_session(&handle, 1).await;

    let frame = prompt_read_text_file(&handle).await;

    // The framework-owned request carries the typed params and its own id.
    let RawJsonRpcMessage::Request(request) = frame else {
        unreachable!("checked above");
    };
    let params = common::raw_params_to_value(request.params.clone());
    assert_eq!(params["sessionId"], "session-a");
    assert_eq!(params["path"], "/tmp/notes.txt");
    assert!(matches!(request.id, RequestId::Number(_)), "framework-owned numeric id");

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn matching_response_returns_content_to_provider() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    provider.set_prompt_behavior(
        "session-a",
        PromptBehavior::ReadTextFile { path: "/tmp/notes.txt".into() },
    );
    let (handle, task) = spawn_server(provider).await;
    new_session(&handle, 1).await;

    let frame = prompt_read_text_file(&handle).await;
    handle.send(respond_to(&frame, Ok(json!({ "content": "file contents" }))));

    // The provider sees the decoded typed response.
    wait_for_log(&log, |calls| calls.iter().any(|c| c == "client:read_text_file:ok:file contents"))
        .await;
    let result = request_result(handle.next_frame().await);
    assert_eq!(result["stopReason"], "end_turn");

    handle.shutdown(task).await;
}

// ── Error mapping ────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn jsonrpc_error_response_maps_to_client_request_failure() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    provider.set_prompt_behavior(
        "session-a",
        PromptBehavior::ReadTextFile { path: "/tmp/notes.txt".into() },
    );
    let (handle, task) = spawn_server(provider).await;
    new_session(&handle, 1).await;

    let frame = prompt_read_text_file(&handle).await;
    handle.send(respond_to(&frame, Err(RpcError::new(-32603, "client storage exploded"))));

    // The provider sees the mapped client-request failure.
    wait_for_log(&log, |calls| {
        calls.iter().any(|c| {
            c.contains("client:read_text_file:err:client request failed")
                && c.contains("client storage exploded")
        })
    })
    .await;
    let error = request_error(handle.next_frame().await);
    assert_eq!(i32::from(error.code), -32603);
    assert!(error.message.contains("client request failed"));

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn permission_denied_response_maps_to_provider_permission_denied() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    provider.set_prompt_behavior(
        "session-a",
        PromptBehavior::ReadTextFile { path: "/tmp/secret.txt".into() },
    );
    let (handle, task) = spawn_server(provider).await;
    new_session(&handle, 1).await;

    let frame = prompt_read_text_file(&handle).await;
    handle.send(respond_to(&frame, Err(RpcError::new(-32001, "user denied"))));

    wait_for_log(&log, |calls| {
        calls.iter().any(|c| c.contains("client:read_text_file:err:permission denied: user denied"))
    })
    .await;
    let error = request_error(handle.next_frame().await);
    assert_eq!(i32::from(error.code), -32001);

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_response_id_is_ignored() {
    let (provider, _log) = FakeProvider::new(&["session-a"]);
    let (handle, task) = spawn_server(provider).await;
    new_session(&handle, 1).await;

    // An unsolicited response with an unknown id must not error or respond.
    handle.send(RawJsonRpcMessage::response(
        RequestId::Number(4242),
        Ok(json!({ "content": "late" })),
    ));
    handle.send(request(2, "initialize", json!({ "protocolVersion": 1 })));
    let result = request_result(handle.next_frame().await);
    assert_eq!(result["protocolVersion"], 1);
    assert!(handle.outbound().is_empty(), "unknown response ids produce no output");

    handle.shutdown(task).await;
}

// ── Timeout and transport close ──────────────────────────────────────────

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn timeout_fails_pending_request_and_server_stays_healthy() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    provider.set_prompt_behavior(
        "session-a",
        PromptBehavior::ReadTextFile { path: "/abs/no-answer.txt".into() },
    );
    let config = common::AcpAgentServerConfig {
        request_timeout: Duration::from_millis(50),
        ..Default::default()
    };
    let (handle, task) = spawn_server_with_config(provider, config).await;
    new_session(&handle, 1).await;

    let frame = prompt_read_text_file(&handle).await;

    // Never answer; advance past the request timeout.
    tokio::time::advance(Duration::from_millis(100)).await;
    wait_for_log(&log, |calls| {
        calls.iter().any(|c| c.contains("client:read_text_file:err:") && c.contains("timed out"))
    })
    .await;
    let error = request_error(handle.next_frame().await);
    assert_eq!(i32::from(error.code), -32603);
    assert!(error.message.contains("client request failed"));

    // A late response for the timed-out id is ignored; the server keeps
    // serving.
    handle.send(respond_to(&frame, Ok(json!({ "content": "late" }))));
    handle.send(request(3, "session/list", json!({})));
    let result = request_result(handle.next_frame().await);
    let response: ListSessionsResponse =
        serde_json::from_value(result).expect("parses as ListSessionsResponse");
    assert_eq!(response.sessions.len(), 1, "session-a still registered");

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn transport_close_fails_pending_entries() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    provider.set_prompt_behavior(
        "session-a",
        PromptBehavior::ReadTextFile { path: "/abs/interrupted.txt".into() },
    );
    let (handle, task) = spawn_server(provider).await;
    new_session(&handle, 1).await;

    let _frame = prompt_read_text_file(&handle).await;

    // The client never answers; the transport closes instead.  The blocked
    // provider must observe the transport-closed failure.
    handle.close();
    wait_for_log(&log, |calls| {
        calls
            .iter()
            .any(|c| c.contains("client:read_text_file:err:") && c.contains("transport closed"))
    })
    .await;
    task.await.expect("server task joins").expect("clean EOF shutdown");
}

// ── Path validation before writing ───────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn relative_outbound_read_path_fails_before_request_is_written() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    provider.set_prompt_behavior(
        "session-a",
        PromptBehavior::ReadTextFileAndContinue { path: "relative/notes.txt".into() },
    );
    let (handle, task) = spawn_server(provider).await;
    new_session(&handle, 1).await;

    handle.send(request(2, "session/prompt", prompt_params("session-a")));
    wait_for_log(&log, |calls| {
        calls.iter().any(|c| {
            c.contains("client:read_text_file:err:invalid request")
                && c.contains("path must be an absolute path")
        })
    })
    .await;

    // The first outbound frame after the prompt request is its response —
    // no `fs/read_text_file` request was ever written.
    let frame = handle.next_frame().await;
    assert!(
        matches!(frame, RawJsonRpcMessage::Response(_)),
        "no client request may be written for a relative path"
    );

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn terminal_create_rejects_relative_cwd() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    provider.set_prompt_behavior("session-a", PromptBehavior::CreateTerminalRelativeCwd);
    let (handle, task) = spawn_server(provider).await;
    new_session(&handle, 1).await;

    handle.send(request(2, "session/prompt", prompt_params("session-a")));
    wait_for_log(&log, |calls| {
        calls.iter().any(|c| {
            c.contains("client:create_terminal:err:invalid request")
                && c.contains("cwd must be an absolute path")
        })
    })
    .await;

    let frame = handle.next_frame().await;
    assert!(
        matches!(frame, RawJsonRpcMessage::Response(_)),
        "no terminal/create request may be written for a relative cwd"
    );

    handle.shutdown(task).await;
}
