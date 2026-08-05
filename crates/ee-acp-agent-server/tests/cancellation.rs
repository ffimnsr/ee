//! Prompt cancellation and active-prompt lifecycle over the in-memory
//! transport: `session/cancel`, `session/close` cancelling a running
//! prompt, concurrent-prompt rejection, parallel prompts across sessions,
//! and cleanup after success, provider error, and cancellation.

mod common;

use ee_agent_protocol::ListSessionsResponse;
use serde_json::json;

use common::{
    FakeProvider, PromptBehavior, notification, prompt_params, request, request_error,
    request_result, session_new_params, spawn_server, wait_for_log,
};

/// Starts a session and returns its id.
async fn new_session(handle: &common::Harness, id: i64) -> String {
    handle.send(request(id, "session/new", session_new_params("/work")));
    let result = request_result(handle.next_frame().await);
    result["sessionId"].as_str().expect("session id").to_string()
}

async fn list_session_ids(handle: &common::Harness, id: i64) -> Vec<String> {
    handle.send(request(id, "session/list", json!({})));
    let result = request_result(handle.next_frame().await);
    let response: ListSessionsResponse =
        serde_json::from_value(result).expect("parses as ListSessionsResponse");
    response.sessions.iter().map(|session| session.session_id.to_string()).collect()
}

// ── session/cancel ───────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn session_cancel_triggers_provider_cancellation_token() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    provider.set_prompt_behavior("session-a", PromptBehavior::AwaitCancelThenCancelled);
    let (handle, task) = spawn_server(provider).await;
    let session_id = new_session(&handle, 1).await;
    assert_eq!(session_id, "session-a");

    // Prompt starts and blocks on its cancellation receiver.
    handle.send(request(2, "session/prompt", prompt_params("session-a")));
    wait_for_log(&log, |calls| calls.iter().any(|c| c == "prompt:session-a:started")).await;

    // Cancel via the notification form.
    handle.send(notification("session/cancel", json!({ "sessionId": "session-a" })));
    wait_for_log(&log, |calls| calls.iter().any(|c| c == "prompt:session-a:cancelled")).await;

    // The cancelled prompt resolves to a deterministic `cancelled` result.
    let result = request_result(handle.next_frame().await);
    assert_eq!(result["stopReason"], "cancelled");

    // Provider cancellation hooks ran.
    assert!(log.has_call("cancel_session:session-a"));

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_prompt_removes_active_state() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    let behaviors = provider.behaviors.clone();
    provider.set_prompt_behavior("session-a", PromptBehavior::AwaitCancelThenCancelled);
    let (handle, task) = spawn_server(provider).await;
    new_session(&handle, 1).await;

    handle.send(request(2, "session/prompt", prompt_params("session-a")));
    wait_for_log(&log, |calls| calls.iter().any(|c| c == "prompt:session-a:started")).await;
    handle.send(notification("session/cancel", json!({ "sessionId": "session-a" })));
    let result = request_result(handle.next_frame().await);
    assert_eq!(result["stopReason"], "cancelled");

    // Active-prompt state is cleaned up: a new prompt is accepted.
    behaviors.lock().expect("behaviors").insert("session-a".to_string(), PromptBehavior::Return);
    handle.send(request(3, "session/prompt", prompt_params("session-a")));
    let result = request_result(handle.next_frame().await);
    assert_eq!(result["stopReason"], "end_turn");

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn session_cancel_without_active_prompt_is_noop() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    let (handle, task) = spawn_server(provider).await;
    new_session(&handle, 1).await;

    // No prompt is running; the notification must not error and the server
    // keeps serving.  The provider cancel hook still runs.
    handle.send(notification("session/cancel", json!({ "sessionId": "session-a" })));
    wait_for_log(&log, |calls| calls.iter().any(|c| c == "cancel_session:session-a")).await;

    handle.send(request(2, "session/prompt", prompt_params("session-a")));
    let result = request_result(handle.next_frame().await);
    assert_eq!(result["stopReason"], "end_turn");

    handle.shutdown(task).await;
}

// ── session/close cancels active prompt ──────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn session_close_cancels_active_prompt() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    provider.set_prompt_behavior("session-a", PromptBehavior::AwaitCancelThenCancelled);
    let (handle, task) = spawn_server(provider).await;
    new_session(&handle, 1).await;

    handle.send(request(2, "session/prompt", prompt_params("session-a")));
    wait_for_log(&log, |calls| calls.iter().any(|c| c == "prompt:session-a:started")).await;

    // Closing the session cancels the active prompt, awaits its cleanup
    // (bounded), removes prompt state, then closes.
    handle.send(request(3, "session/close", json!({ "sessionId": "session-a" })));
    let frames = handle.next_frames(2).await;
    let close_result = request_result(frames[0].clone());
    assert_eq!(close_result, json!({}));
    let prompt_result = request_result(frames[1].clone());
    assert_eq!(prompt_result["stopReason"], "cancelled");

    assert!(log.has_call("prompt:session-a:cancelled"));
    assert!(log.has_call("close_session:session-a"));
    assert!(list_session_ids(&handle, 4).await.is_empty(), "session removed");

    handle.shutdown(task).await;
}

// ── Concurrency rules ────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn second_same_session_prompt_is_rejected_while_first_is_active() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    let behaviors = provider.behaviors.clone();
    provider.set_prompt_behavior("session-a", PromptBehavior::AwaitCancelThenCancelled);
    let (handle, task) = spawn_server(provider).await;
    new_session(&handle, 1).await;

    handle.send(request(2, "session/prompt", prompt_params("session-a")));
    wait_for_log(&log, |calls| calls.iter().any(|c| c == "prompt:session-a:started")).await;

    // Second prompt on the same session is rejected with a clear error.
    handle.send(request(3, "session/prompt", prompt_params("session-a")));
    let error = request_error(handle.next_frame().await);
    assert_eq!(i32::from(error.code), -32602);
    assert!(common::error_reason(&error).contains("already active"));

    // Exactly one provider prompt ran for the session.
    assert_eq!(log.calls().iter().filter(|c| c == &"prompt:session-a:started").count(), 1);

    // Cleanup: after cancellation a new prompt is accepted.
    handle.send(notification("session/cancel", json!({ "sessionId": "session-a" })));
    let result = request_result(handle.next_frame().await);
    assert_eq!(result["stopReason"], "cancelled");
    behaviors.lock().expect("behaviors").insert("session-a".to_string(), PromptBehavior::Return);
    handle.send(request(4, "session/prompt", prompt_params("session-a")));
    let result = request_result(handle.next_frame().await);
    assert_eq!(result["stopReason"], "end_turn");

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn prompts_in_two_sessions_run_concurrently() {
    let (provider, log) = FakeProvider::new(&["session-a", "session-b"]);
    provider.set_prompt_behavior("session-a", PromptBehavior::AwaitCancelThenCancelled);
    let (handle, task) = spawn_server(provider).await;
    new_session(&handle, 1).await;
    new_session(&handle, 2).await;

    // Prompt A blocks; prompt B returns while A is still active.
    handle.send(request(3, "session/prompt", prompt_params("session-a")));
    wait_for_log(&log, |calls| calls.iter().any(|c| c == "prompt:session-a:started")).await;
    handle.send(request(4, "session/prompt", prompt_params("session-b")));
    let result = request_result(handle.next_frame().await);
    assert_eq!(result["stopReason"], "end_turn", "session B completes while A is active");

    // Session A is still running and can still be cancelled.
    handle.send(notification("session/cancel", json!({ "sessionId": "session-a" })));
    let result = request_result(handle.next_frame().await);
    assert_eq!(result["stopReason"], "cancelled");

    assert_eq!(log.calls().iter().filter(|c| c == &"prompt:session-a:started").count(), 1);
    assert_eq!(log.calls().iter().filter(|c| c == &"prompt:session-b:started").count(), 1);

    handle.shutdown(task).await;
}

// ── Cleanup after success and provider error ─────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn prompt_state_cleaned_after_success_and_provider_error() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    let behaviors = provider.behaviors.clone();
    let (handle, task) = spawn_server(provider).await;
    new_session(&handle, 1).await;

    // Success first.
    handle.send(request(2, "session/prompt", prompt_params("session-a")));
    let result = request_result(handle.next_frame().await);
    assert_eq!(result["stopReason"], "end_turn");

    // Provider error: the framework answers with a JSON-RPC error.
    behaviors.lock().expect("behaviors").insert("session-a".to_string(), PromptBehavior::Fail);
    handle.send(request(3, "session/prompt", prompt_params("session-a")));
    let error = request_error(handle.next_frame().await);
    assert_eq!(i32::from(error.code), -32603);
    assert!(error.message.contains("provider backend failure"));

    // Cleanup after both: a success prompt is accepted again.
    behaviors.lock().expect("behaviors").insert("session-a".to_string(), PromptBehavior::Return);
    handle.send(request(4, "session/prompt", prompt_params("session-a")));
    let result = request_result(handle.next_frame().await);
    assert_eq!(result["stopReason"], "end_turn");
    assert_eq!(log.calls().iter().filter(|c| c == &"prompt:session-a:started").count(), 3);

    handle.shutdown(task).await;
}
