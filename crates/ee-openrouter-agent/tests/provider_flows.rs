//! OpenRouter provider end-to-end flows over the framework's in-memory
//! transport: initialization, session lifecycle, prompt updates (thoughts
//! and answers), file reads through the `ClientBridge`, and the bounded tool
//! loop.  The provider speaks to a scripted in-process OpenRouter HTTP
//! endpoint; there is no handrolled JSON-RPC or stdio in provider code.

mod common;

use std::time::Duration;

use common::{
    Harness, MockOpenRouter, answer_response, prompt_params, reasoning_response, request,
    request_error, request_result, respond_to, session_new_params, tool_call_response,
};
use ee_agent_protocol::{RawJsonRpcMessage, RawJsonRpcParams};
use ee_openrouter_agent::config::Config;
use ee_openrouter_agent::provider::OpenRouterProvider;
use serde_json::{Value, json};

fn test_config(mock: &MockOpenRouter) -> Config {
    Config {
        model: String::from("test/model"),
        api_url: mock.api_url(),
        api_key: Some(String::from("sk-test")),
        site_url: None,
        app_title: String::from("ee-test"),
        timeout: Duration::from_secs(5),
        system_prompt: String::from("system"),
        reasoning_effort: None,
        orchestrated: false,
    }
}

/// Starts a session and returns its id.
async fn new_session(harness: &Harness, id: i64) -> String {
    harness.send(request(id, "session/new", session_new_params("/work")));
    let result = request_result(harness.next_frame().await);
    result["sessionId"].as_str().expect("session id").to_string()
}

/// Extracts the `update` value from a `session/update` notification.
fn update_of(frame: &RawJsonRpcMessage) -> Value {
    let RawJsonRpcMessage::Notification(notification) = frame else {
        panic!("expected a session/update notification, got {frame:?}");
    };
    assert_eq!(notification.method.as_ref(), "session/update");
    let RawJsonRpcParams::Object(params) = notification.params.as_ref().expect("params present")
    else {
        panic!("expected object params");
    };
    params.get("update").expect("update").clone()
}

/// Extracts the params object of a request frame.
fn request_params(frame: &RawJsonRpcMessage) -> Value {
    let RawJsonRpcMessage::Request(request) = frame else {
        panic!("expected a request frame, got {frame:?}");
    };
    let Some(RawJsonRpcParams::Object(params)) = request.params.clone() else {
        panic!("expected object params");
    };
    Value::Object(params)
}

// ── Initialization and session lifecycle ─────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn initialize_advertises_openrouter_agent() {
    let mock = MockOpenRouter::start(vec![]);
    let provider = OpenRouterProvider::new(test_config(&mock)).unwrap();
    let (harness, task) = Harness::spawn(provider).await;

    harness.send(request(1, "initialize", json!({ "protocolVersion": 1 })));

    let result = request_result(harness.next_frame().await);
    assert_eq!(result["protocolVersion"], 1);
    assert_eq!(result["agentInfo"]["name"], "ee-openrouter-agent");
    assert_eq!(result["agentInfo"]["title"], "OpenRouter");

    harness.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn session_new_returns_provider_session_ids() {
    let mock = MockOpenRouter::start(vec![]);
    let provider = OpenRouterProvider::new(test_config(&mock)).unwrap();
    let (harness, task) = Harness::spawn(provider).await;

    harness.send(request(1, "session/new", session_new_params("/work")));
    let first = request_result(harness.next_frame().await);
    assert_eq!(first["sessionId"], "openrouter-1");

    harness.send(request(2, "session/new", session_new_params("/work")));
    let second = request_result(harness.next_frame().await);
    assert_eq!(second["sessionId"], "openrouter-2");

    harness.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn session_load_is_unsupported() {
    let mock = MockOpenRouter::start(vec![]);
    let provider = OpenRouterProvider::new(test_config(&mock)).unwrap();
    let (harness, task) = Harness::spawn(provider).await;

    harness.send(request(
        1,
        "session/load",
        json!({
            "sessionId": "openrouter-1",
            "cwd": "/work",
            "additionalDirectories": [],
            "mcpServers": [],
        }),
    ));

    let error = request_error(harness.next_frame().await);
    assert_eq!(i32::from(error.code), -32600);
    assert!(error.message.contains("session loading is not supported"), "{error}");

    harness.shutdown(task).await;
}

// ── Prompt updates through the framework ─────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn prompt_without_api_key_returns_backend_error() {
    let mock = MockOpenRouter::start(vec![]);
    let mut config = test_config(&mock);
    config.api_key = None;
    let provider = OpenRouterProvider::new(config).unwrap();
    let (harness, task) = Harness::spawn(provider).await;
    let session_id = new_session(&harness, 1).await;

    harness.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));

    let error = request_error(harness.next_frame().await);
    assert_eq!(i32::from(error.code), -32603);
    assert!(error.message.contains("OPENROUTER_API_KEY"), "{error}");
    assert!(mock.request_bodies().is_empty(), "no HTTP call may happen without a key");

    harness.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn provider_emits_thought_update_through_framework() {
    let mock = MockOpenRouter::start(vec![reasoning_response("plan step", "answer text")]);
    let provider = OpenRouterProvider::new(test_config(&mock)).unwrap();
    let (harness, task) = Harness::spawn(provider).await;
    let session_id = new_session(&harness, 1).await;

    harness.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));

    // Thought chunk, answer chunk, then the prompt response, in order.
    let frames = harness.next_frames(3).await;
    let thought = update_of(&frames[0]);
    assert_eq!(thought["sessionUpdate"], "agent_thought_chunk");
    assert_eq!(thought["messageId"], "openrouter-thought-1");
    assert_eq!(thought["content"]["text"], "plan step");

    let answer = update_of(&frames[1]);
    assert_eq!(answer["sessionUpdate"], "agent_message_chunk");
    assert_eq!(answer["messageId"], "openrouter-message-2");
    assert_eq!(answer["content"]["text"], "answer text");

    let result = request_result(frames[2].clone());
    assert_eq!(result["stopReason"], "end_turn");

    harness.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn provider_emits_answer_update_through_framework() {
    let mock = MockOpenRouter::start(vec![answer_response("hi there")]);
    let provider = OpenRouterProvider::new(test_config(&mock)).unwrap();
    let (harness, task) = Harness::spawn(provider).await;
    let session_id = new_session(&harness, 1).await;

    harness.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));

    let frames = harness.next_frames(2).await;
    let answer = update_of(&frames[0]);
    assert_eq!(answer["sessionUpdate"], "agent_message_chunk");
    assert_eq!(answer["content"]["text"], "hi there");

    let result = request_result(frames[1].clone());
    assert_eq!(result["stopReason"], "end_turn");

    harness.shutdown(task).await;
}

// ── File reads through the ClientBridge ──────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn read_file_tool_uses_client_bridge() {
    let mock = MockOpenRouter::start(vec![
        tool_call_response("call_1", "/tmp/notes.txt"),
        answer_response("done"),
    ]);
    let provider = OpenRouterProvider::new(test_config(&mock)).unwrap();
    let (harness, task) = Harness::spawn(provider).await;
    let session_id = new_session(&harness, 1).await;

    harness.send(request(2, "session/prompt", prompt_params(&session_id, "read notes")));

    // The framework emits the tool in-progress update, then the agent →
    // client fs request.
    let frames = harness.next_frames(2).await;
    let running = update_of(&frames[0]);
    assert_eq!(running["sessionUpdate"], "tool_call_update");
    assert_eq!(running["toolCallId"], "call_1");
    assert_eq!(running["status"], "in_progress");
    assert_eq!(running["title"], "read file");

    let fs_request = &frames[1];
    assert!(
        matches!(fs_request, RawJsonRpcMessage::Request(_)),
        "expected fs/read_text_file request, got {fs_request:?}"
    );
    let params = request_params(fs_request);
    assert_eq!(params["sessionId"], session_id);
    assert_eq!(params["path"], "/tmp/notes.txt");

    // Answer the bridge request; the tool completes and the turn ends.
    harness.send(respond_to(fs_request, Ok(json!({ "content": "file contents" }))));

    let frames = harness.next_frames(3).await;
    let completed = update_of(&frames[0]);
    assert_eq!(completed["sessionUpdate"], "tool_call_update");
    assert_eq!(completed["toolCallId"], "call_1");
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["content"][0]["content"]["text"], "read 13 bytes");

    let answer = update_of(&frames[1]);
    assert_eq!(answer["sessionUpdate"], "agent_message_chunk");
    assert_eq!(answer["content"]["text"], "done");

    let result = request_result(frames[2].clone());
    assert_eq!(result["stopReason"], "end_turn");

    // The tool result was appended to the conversation for the second round.
    let bodies = mock.request_bodies();
    assert_eq!(bodies.len(), 2);
    let messages = bodies[1]["messages"].as_array().expect("messages array");
    assert!(
        messages.iter().any(|message| message["role"] == "tool"
            && message["tool_call_id"] == "call_1"
            && message["content"] == "file contents"),
        "second request must carry the tool result: {messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|message| message["role"] == "assistant"
                && message["tool_calls"][0]["id"] == "call_1"),
        "second request must carry the assistant tool call: {messages:?}"
    );

    harness.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn relative_read_paths_resolve_against_session_cwd() {
    let mock = MockOpenRouter::start(vec![
        tool_call_response("call_1", ".ee.toml"),
        answer_response("ok"),
    ]);
    let provider = OpenRouterProvider::new(test_config(&mock)).unwrap();
    let (harness, task) = Harness::spawn(provider).await;
    let session_id = new_session(&harness, 1).await;

    harness.send(request(2, "session/prompt", prompt_params(&session_id, "read config")));

    let frames = harness.next_frames(2).await;
    let running = update_of(&frames[0]);
    assert_eq!(running["sessionUpdate"], "tool_call_update");
    assert_eq!(running["content"][0]["content"]["text"], "path: /work/.ee.toml");

    let fs_request = &frames[1];
    let params = request_params(fs_request);
    assert_eq!(params["path"], "/work/.ee.toml");

    harness.send(respond_to(fs_request, Ok(json!({ "content": "[agents]\nenabled = true\n" }))));

    let frames = harness.next_frames(3).await;
    assert_eq!(update_of(&frames[0])["status"], "completed");
    assert_eq!(update_of(&frames[1])["content"]["text"], "ok");
    let result = request_result(frames[2].clone());
    assert_eq!(result["stopReason"], "end_turn");

    harness.shutdown(task).await;
}

// ── Bounded tool loop ────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn tool_loop_max_rounds_maps_to_backend_error() {
    let mock = MockOpenRouter::start(
        (0..7).map(|round| tool_call_response(&format!("call_{round}"), "/tmp/data.txt")).collect(),
    );
    let provider = OpenRouterProvider::new(test_config(&mock)).unwrap();
    let (harness, task) = Harness::spawn(provider).await;
    let session_id = new_session(&harness, 1).await;

    harness.send(request(2, "session/prompt", prompt_params(&session_id, "loop")));

    // Six executed tool rounds: in-progress update + fs request, then a
    // completed update once the bridge answers.
    for round in 0..6 {
        let frames = harness.next_frames(2).await;
        assert_eq!(update_of(&frames[0])["sessionUpdate"], "tool_call_update");
        let fs_request = &frames[1];
        harness.send(respond_to(fs_request, Ok(json!({ "content": "data" }))));

        let frames = harness.next_frames(1).await;
        let completed = update_of(&frames[0]);
        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["toolCallId"], format!("call_{round}"));
    }

    // The seventh model response also requests a tool, which exceeds the
    // round budget: the prompt fails with a provider backend error.
    let error = request_error(harness.next_frame().await);
    assert_eq!(i32::from(error.code), -32603);
    assert!(error.message.contains("tool loop exceeded maximum rounds"), "{error}");
    assert_eq!(mock.request_bodies().len(), 7);

    harness.shutdown(task).await;
}
