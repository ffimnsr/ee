//! Mid-session model switch (`session/set_model`): idle acceptance, running
//! rejection, and unknown-session handling.

#[allow(dead_code)]
mod common;

use serde_json::json;

use common::{
    FakeProvider, PromptBehavior, error_reason, request, request_error, request_result,
    session_new_params, spawn_server, wait_for_log,
};

async fn handshake(handle: &common::Harness) -> String {
    handle.send(request(1, "initialize", json!({ "protocolVersion": 1 })));
    let _ = request_result(handle.next_frame().await);
    handle.send(request(2, "session/new", session_new_params("/work")));
    let result = request_result(handle.next_frame().await);
    result["sessionId"].as_str().expect("session id").to_string()
}

#[tokio::test(flavor = "current_thread")]
async fn idle_session_model_switch_reaches_the_provider() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    let (handle, task) = spawn_server(provider).await;
    let session_id = handshake(&handle).await;

    handle.send(request(
        3,
        "session/set_model",
        json!({ "sessionId": session_id, "modelId": "model-b" }),
    ));
    let result = request_result(handle.next_frame().await);
    assert_eq!(result, json!({}));
    assert!(log.has_call("set_model:session-a:model-b"));

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn model_switch_is_rejected_while_a_turn_runs() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    provider.set_prompt_behavior("session-a", PromptBehavior::DelayThenReturn { millis: 200 });
    let (handle, task) = spawn_server(provider).await;
    let session_id = handshake(&handle).await;

    handle.send(request(
        3,
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": "work" }],
        }),
    ));
    wait_for_log(&log, |calls| calls.iter().any(|call| call == "prompt:session-a:started")).await;

    handle.send(request(
        4,
        "session/set_model",
        json!({ "sessionId": session_id, "modelId": "model-b" }),
    ));
    let error = request_error(handle.next_frame().await);
    assert!(
        error_reason(&error).contains("while a turn is running"),
        "running switch names the constraint: {error:?}"
    );
    assert!(!log.has_call("set_model:session-a:model-b"), "provider never saw the switch");

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn model_switch_rejects_an_unknown_session() {
    let (provider, log) = FakeProvider::new(&[]);
    let (handle, task) = spawn_server(provider).await;

    handle.send(request(
        1,
        "session/set_model",
        json!({ "sessionId": "ghost", "modelId": "model-b" }),
    ));
    let error = request_error(handle.next_frame().await);
    assert!(error.message.contains("unknown session"), "unknown session is named: {error:?}");
    assert!(!log.has_call("set_model:"));

    handle.shutdown(task).await;
}
