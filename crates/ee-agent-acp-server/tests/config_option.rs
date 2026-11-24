//! `session/set_config_option`: advertised-option validation, provider
//! application, state refresh, and running-turn rejection.

#[allow(dead_code)]
mod common;

use ee_agent_protocol::{SessionConfigOption, SessionConfigSelectOption};
use serde_json::json;

use common::{
    FakeProvider, PromptBehavior, error_reason, request, request_error, request_result,
    session_new_params, spawn_server, wait_for_log,
};

fn effort_option() -> SessionConfigOption {
    SessionConfigOption::select(
        "effort",
        "Reasoning effort",
        "default",
        vec![
            SessionConfigSelectOption::new("default", "default: provider default"),
            SessionConfigSelectOption::new("low", "low"),
            SessionConfigSelectOption::new("high", "high"),
        ],
    )
}

async fn handshake(handle: &common::Harness) -> String {
    handle.send(request(1, "initialize", json!({ "protocolVersion": 1 })));
    let _ = request_result(handle.next_frame().await);
    handle.send(request(2, "session/new", session_new_params("/work")));
    let result = request_result(handle.next_frame().await);
    assert_eq!(
        result["configOptions"][0]["id"], "effort",
        "the provider-advertised option reaches the client: {result}"
    );
    assert_eq!(result["configOptions"][0]["currentValue"], "default");
    result["sessionId"].as_str().expect("session id").to_string()
}

#[tokio::test(flavor = "current_thread")]
async fn config_option_change_reaches_the_provider_and_refreshes_state() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    let provider = provider.with_config_options(vec![effort_option()]);
    let (handle, task) = spawn_server(provider).await;
    let session_id = handshake(&handle).await;

    handle.send(request(
        3,
        "session/set_config_option",
        json!({ "sessionId": session_id, "configId": "effort", "value": "high" }),
    ));
    let result = request_result(handle.next_frame().await);
    assert_eq!(result["configOptions"][0]["currentValue"], "high");
    assert!(log.has_call("set_config_option:session-a:effort:high"));

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_config_option_is_rejected_before_the_provider() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    let provider = provider.with_config_options(vec![effort_option()]);
    let (handle, task) = spawn_server(provider).await;
    let session_id = handshake(&handle).await;

    handle.send(request(
        3,
        "session/set_config_option",
        json!({ "sessionId": session_id, "configId": "speed", "value": "fast" }),
    ));
    let error = request_error(handle.next_frame().await);
    assert!(
        error_reason(&error).contains("was not advertised"),
        "unknown option is named: {error:?}"
    );
    assert!(!log.has_call("set_config_option:"), "provider never saw the option");

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn config_option_change_is_rejected_while_a_turn_runs() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    let provider = provider.with_config_options(vec![effort_option()]);
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
        "session/set_config_option",
        json!({ "sessionId": session_id, "configId": "effort", "value": "high" }),
    ));
    let error = request_error(handle.next_frame().await);
    assert!(
        error_reason(&error).contains("while a prompt is active"),
        "running change names the constraint: {error:?}"
    );
    assert!(!log.has_call("set_config_option:"), "provider never saw the option");

    handle.shutdown(task).await;
}
