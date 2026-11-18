//! Prompt inbox flows: mid-turn steer, queue promotion, cancellation of
//! not-yet-promoted items, and the disabled/bounded surfaces.

#[allow(dead_code)]
mod common;

use ee_agent_protocol::RawJsonRpcMessage;
use serde_json::{Value, json};

use common::{
    FakeProvider, PromptBehavior, error_reason, notification, request, request_error,
    request_result, session_new_params, spawn_server, spawn_server_with_config, wait_for_log,
};

fn text_prompt(session_id: &str, text: &str, delivery: Option<&str>) -> Value {
    let mut params = json!({
        "sessionId": session_id,
        "prompt": [{ "type": "text", "text": text }],
    });
    if let Some(delivery) = delivery {
        params["delivery"] = json!(delivery);
    }
    params
}

async fn handshake(handle: &common::Harness) -> String {
    handle.send(request(1, "initialize", json!({ "protocolVersion": 1 })));
    let _ = request_result(handle.next_frame().await);
    handle.send(request(2, "session/new", session_new_params("/work")));
    let result = request_result(handle.next_frame().await);
    result["sessionId"].as_str().expect("session id").to_string()
}

async fn next_prompt_response(handle: &common::Harness) -> Value {
    loop {
        let frame = handle.next_frame().await;
        if matches!(frame, RawJsonRpcMessage::Response(_)) {
            return request_result(frame);
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn steer_prompt_is_visible_to_the_running_turn() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    provider.set_prompt_behavior("session-a", PromptBehavior::AwaitSteerThenReturn);
    let (handle, task) = spawn_server(provider).await;
    let session_id = handshake(&handle).await;

    handle.send(request(3, "session/prompt", text_prompt(&session_id, "start work", None)));
    wait_for_log(&log, |calls| calls.iter().any(|call| call == "prompt:session-a:started")).await;

    handle.send(request(
        4,
        "session/prompt",
        text_prompt(&session_id, "steer middle", Some("steer")),
    ));

    let calls = wait_for_log(&log, |calls| {
        calls.iter().any(|call| call == "prompt:session-a:steer:steer middle")
    })
    .await;
    assert!(
        calls.iter().any(|call| call == "prompt:session-a:text:start work"),
        "first prompt text reached the provider: {calls:?}"
    );

    // Both requests answer from the joined turn.
    let first = next_prompt_response(&handle).await;
    let second = next_prompt_response(&handle).await;
    assert_eq!(first["stopReason"], "end_turn");
    assert_eq!(second["stopReason"], "end_turn");

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn queued_prompt_runs_after_the_turn_settles() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    provider.set_prompt_behavior("session-a", PromptBehavior::DelayThenReturn { millis: 50 });
    let (handle, task) = spawn_server(provider).await;
    let session_id = handshake(&handle).await;

    handle.send(request(3, "session/prompt", text_prompt(&session_id, "first", None)));
    wait_for_log(&log, |calls| calls.iter().any(|call| call == "prompt:session-a:started")).await;

    handle.send(request(4, "session/prompt", text_prompt(&session_id, "second", Some("queue"))));

    let calls =
        wait_for_log(&log, |calls| calls.iter().any(|call| call == "prompt:session-a:text:second"))
            .await;
    let started = calls.iter().filter(|call| *call == "prompt:session-a:started").count();
    assert_eq!(started, 2, "queued prompt starts its own turn: {calls:?}");

    let first = next_prompt_response(&handle).await;
    let second = next_prompt_response(&handle).await;
    assert_eq!(first["stopReason"], "end_turn");
    assert_eq!(second["stopReason"], "end_turn");

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn session_cancel_answers_not_yet_promoted_prompts() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    provider.set_prompt_behavior("session-a", PromptBehavior::AwaitCancelThenCancelled);
    let (handle, task) = spawn_server(provider).await;
    let session_id = handshake(&handle).await;

    handle.send(request(3, "session/prompt", text_prompt(&session_id, "running", None)));
    wait_for_log(&log, |calls| calls.iter().any(|call| call == "prompt:session-a:started")).await;
    handle.send(request(4, "session/prompt", text_prompt(&session_id, "queued", Some("queue"))));

    handle.send(notification("session/cancel", json!({ "sessionId": session_id })));

    let one = next_prompt_response(&handle).await;
    let two = next_prompt_response(&handle).await;
    let reasons = [one.clone(), two.clone()];
    assert!(
        reasons.iter().all(|response| response["stopReason"] == "cancelled"),
        "both the running and queued prompt answer cancelled: {reasons:?}"
    );
    let calls = log.calls();
    assert_eq!(
        calls.iter().filter(|call| *call == "prompt:session-a:started").count(),
        1,
        "queued prompt never starts: {calls:?}"
    );

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn queued_compact_command_executes_after_the_running_turn() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    provider.set_prompt_behavior("session-a", PromptBehavior::DelayThenReturn { millis: 40 });
    let (handle, task) = spawn_server(provider).await;
    let session_id = handshake(&handle).await;

    handle.send(request(3, "session/prompt", text_prompt(&session_id, "work", None)));
    wait_for_log(&log, |calls| calls.iter().any(|call| call == "prompt:session-a:started")).await;
    handle.send(request(4, "session/prompt", text_prompt(&session_id, "/compact", Some("queue"))));

    let calls = wait_for_log(&log, |calls| {
        calls.iter().any(|call| call == "prompt:session-a:text:/compact")
    })
    .await;
    assert!(
        calls.iter().any(|call| call == "prompt:session-a:text:work"),
        "control command arrives after ordinary work: {calls:?}"
    );

    let first = next_prompt_response(&handle).await;
    let second = next_prompt_response(&handle).await;
    assert_eq!(first["stopReason"], "end_turn");
    assert_eq!(second["stopReason"], "end_turn");

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn disabled_inbox_rejects_mid_turn_prompts() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    provider.set_prompt_behavior("session-a", PromptBehavior::DelayThenReturn { millis: 200 });
    let config = common::AcpAgentServerConfig {
        prompt_inbox_enabled: false,
        ..common::AcpAgentServerConfig::default()
    };
    let (handle, task) = spawn_server_with_config(provider, config).await;
    let session_id = handshake(&handle).await;

    handle.send(request(3, "session/prompt", text_prompt(&session_id, "running", None)));
    wait_for_log(&log, |calls| calls.iter().any(|call| call == "prompt:session-a:started")).await;
    handle.send(request(4, "session/prompt", text_prompt(&session_id, "rejected", Some("steer"))));

    let error = request_error(handle.next_frame().await);
    assert!(
        error_reason(&error).contains("already active"),
        "disabled inbox mirrors the concurrent-prompt error: {error:?}"
    );

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn full_queue_rejects_additional_prompts() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    provider.set_prompt_behavior("session-a", PromptBehavior::DelayThenReturn { millis: 200 });
    let config = common::AcpAgentServerConfig {
        max_pending_prompts: 1,
        ..common::AcpAgentServerConfig::default()
    };
    let (handle, task) = spawn_server_with_config(provider, config).await;
    let session_id = handshake(&handle).await;

    handle.send(request(3, "session/prompt", text_prompt(&session_id, "running", None)));
    wait_for_log(&log, |calls| calls.iter().any(|call| call == "prompt:session-a:started")).await;
    handle.send(request(4, "session/prompt", text_prompt(&session_id, "queued", Some("queue"))));
    handle.send(request(5, "session/prompt", text_prompt(&session_id, "overflow", Some("queue"))));

    let error = request_error(handle.next_frame().await);
    assert!(
        error_reason(&error).contains("already active"),
        "full inbox mirrors the concurrent-prompt error: {error:?}"
    );

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_delivery_value_is_rejected_before_the_provider() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    let (handle, task) = spawn_server(provider).await;
    let session_id = handshake(&handle).await;

    handle.send(request(
        3,
        "session/prompt",
        text_prompt(&session_id, "hello", Some("eventually")),
    ));
    let error = request_error(handle.next_frame().await);
    assert!(
        error_reason(&error).contains("eventually"),
        "error names the offending delivery value: {error:?}"
    );
    assert!(!log.has_call("prompt:session-a:started"), "provider never invoked");

    handle.shutdown(task).await;
}
