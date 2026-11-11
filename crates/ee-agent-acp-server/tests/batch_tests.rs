//! JSON-RPC 2.0 batch support over the in-memory transport.
//!
//! The ACP v2 migration guide requires stdio servers to accept batch arrays:
//! the codec materializes per-entry `-32600` error responses for invalid
//! members, and the server dispatches every valid member, answers request
//! members with one batch response array in source order, never answers
//! notifications, and rejects lifecycle-sensitive methods per member
//! (`initialize`, `auth/login`, `session/new`, `session/resume`,
//! `session/prompt`, `session/load` — the guide's "don't batch" carve-out,
//! because those change which later messages are valid and several complete
//! through the deferred outbound path).
//!
//! Per-entry error materialization for *invalid* members happens in the
//! transport codec (bounded by `max_frame_bytes`), so it is covered by the
//! transport unit tests in `src/transport.rs`; the memory transport injects
//! already-parsed frames, which cannot carry invalid members.

#[allow(dead_code)]
mod common;

use std::time::Duration;

use ee_agent_protocol::RawJsonRpcMessage;
use serde_json::Value;

use common::{
    FakeProvider, notification, prompt_params, request, request_result, session_new_params,
    spawn_server,
};

/// A fully-shaped v2 `initialize` request (required `info` + `capabilities`).
fn v2_initialize_request() -> RawJsonRpcMessage {
    request(
        1,
        "initialize",
        serde_json::json!({
            "protocolVersion": 2,
            "info": { "name": "test-client", "title": "Test Client", "version": "1.0.0" },
            "capabilities": {},
        }),
    )
}

/// Runs the v2 handshake and returns the created session id.
async fn v2_handshake_and_session(handle: &common::Harness) -> String {
    handle.send(v2_initialize_request());
    let result = request_result(handle.next_frame().await);
    assert_eq!(result["protocolVersion"], 2);

    handle.send(request(2, "session/new", session_new_params("/work")));
    let result = request_result(handle.next_frame().await);
    result["sessionId"].as_str().expect("session id").to_string()
}

/// Asserts no outbound frame appears within a bounded window (used after an
/// operation that must produce *no* response frame at all).
async fn assert_quiet(handle: &common::Harness) {
    let quiet = tokio::time::timeout(Duration::from_millis(150), handle.next_frame()).await;
    assert!(quiet.is_err(), "expected no response frame, got {:?}", quiet.ok());
}

/// Batch `[session/list, session/close]` answers with ONE batch response
/// array, entries in source order.
#[tokio::test(flavor = "current_thread")]
async fn batch_of_immediate_methods_answers_one_array_in_order() {
    let (provider, log) = FakeProvider::new(&["provider-session-1"]);
    let (handle, task) = spawn_server(provider).await;
    let session_id = v2_handshake_and_session(&handle).await;

    handle.send_batch(vec![
        request(3, "session/list", serde_json::json!({})),
        request(4, "session/close", serde_json::json!({ "sessionId": session_id })),
    ]);

    let entries = handle.next_batch().await;
    assert_eq!(entries.len(), 2, "one batch response array with both entries");
    let first: Value = serde_json::to_value(&entries[0]).expect("entry serializes");
    assert_eq!(first["id"], 3);
    assert_eq!(first["result"]["sessions"][0]["sessionId"], session_id);
    let second: Value = serde_json::to_value(&entries[1]).expect("entry serializes");
    assert_eq!(second["id"], 4);
    assert!(second["result"].is_object(), "session/close answered in the same array");

    // The provider really handled the close.
    common::wait_for_log(&log, |calls| calls.iter().any(|call| call.starts_with("close_session:")))
        .await;

    handle.shutdown(task).await;
}

/// Notification members are dispatched but never answered: the batch response
/// array carries only the request member's entry.
#[tokio::test(flavor = "current_thread")]
async fn batch_notification_members_produce_no_response_entries() {
    let (provider, _log) = FakeProvider::new(&["provider-session-1"]);
    let (handle, task) = spawn_server(provider).await;
    let session_id = v2_handshake_and_session(&handle).await;

    handle.send_batch(vec![
        request(5, "session/list", serde_json::json!({})),
        notification("notifications/cancelled", serde_json::json!({ "sessionId": session_id })),
    ]);

    let entries = handle.next_batch().await;
    assert_eq!(entries.len(), 1, "notifications are never answered");
    let only: Value = serde_json::to_value(&entries[0]).expect("entry serializes");
    assert_eq!(only["id"], 5);
    assert_eq!(only["result"]["sessions"].as_array().map(Vec::len), Some(1));

    handle.shutdown(task).await;
}

/// Lifecycle-sensitive methods are rejected per member with `-32600`; the
/// prompt never starts, and the unaffected member still answers.
#[tokio::test(flavor = "current_thread")]
async fn lifecycle_sensitive_methods_are_rejected_inside_batches() {
    let (provider, _log) = FakeProvider::new(&["provider-session-1"]);
    let (handle, task) = spawn_server(provider).await;
    let session_id = v2_handshake_and_session(&handle).await;

    handle.send_batch(vec![
        request(6, "session/prompt", prompt_params(&session_id)),
        request(7, "session/list", serde_json::json!({})),
    ]);

    let entries = handle.next_batch().await;
    assert_eq!(entries.len(), 2);
    let first: Value = serde_json::to_value(&entries[0]).expect("entry serializes");
    assert_eq!(first["id"], 6);
    assert_eq!(first["error"]["code"].as_i64(), Some(-32600));
    let message = first["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("session/prompt must not be sent inside a batch"),
        "unexpected per-member message: {message}"
    );
    let second: Value = serde_json::to_value(&entries[1]).expect("entry serializes");
    assert_eq!(second["id"], 7);
    assert_eq!(second["result"]["sessions"].as_array().map(Vec::len), Some(1));

    // The rejected prompt never started: no ack or state updates follow.
    assert_quiet(&handle).await;

    handle.shutdown(task).await;
}

/// A batch of only notifications produces no response frame at all.
#[tokio::test(flavor = "current_thread")]
async fn batch_of_pure_notifications_produces_no_response() {
    let (provider, _log) = FakeProvider::new(&["provider-session-1"]);
    let (handle, task) = spawn_server(provider).await;
    let session_id = v2_handshake_and_session(&handle).await;

    handle.send_batch(vec![
        notification("notifications/cancelled", serde_json::json!({ "sessionId": session_id })),
        notification("notifications/unknown-member", serde_json::json!({})),
    ]);

    // Both members are dispatched (cancel is a known notification, the other
    // is ignored), but a batch of notifications is answered with nothing.
    assert_quiet(&handle).await;

    handle.shutdown(task).await;
}
