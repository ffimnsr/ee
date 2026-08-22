//! OpenRouter provider end-to-end flows over the framework's in-memory
//! transport: initialization, session lifecycle, prompt updates (thoughts
//! and answers), file reads through the `ClientBridge`, and the bounded tool
//! loop.  The provider speaks to a scripted in-process OpenRouter HTTP
//! endpoint; there is no handrolled JSON-RPC or stdio in provider code.

mod common;

use std::time::Duration;

use common::{
    Harness, MockOpenRouter, answer_response, answer_response_with_usage, prompt_params,
    reasoning_response, request, request_error, request_result, respond_to, session_new_params,
    tool_call_response,
};
use ee_agent_protocol::{RawJsonRpcMessage, RawJsonRpcParams};
use ee_openrouter_agent::config::{Config, DEFAULT_CONTEXT_WINDOW_TOKENS};
use ee_openrouter_agent::provider::OpenRouterProvider;
use serde_json::{Value, json};

fn test_config(mock: &MockOpenRouter) -> Config {
    test_config_with(mock, 4, 2, 65_536)
}

fn test_config_with(
    mock: &MockOpenRouter,
    compact_min_messages: usize,
    compact_retained_tail: usize,
    compact_max_input_bytes: usize,
) -> Config {
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
        compact_min_messages,
        compact_retained_tail,
        compact_max_input_bytes,
        context_window: DEFAULT_CONTEXT_WINDOW_TOKENS,
        auto_compact_threshold_percent: 0,
        max_iterations: ee_agent_orchestrator::config::DEFAULT_MAX_LOOP_ITERATIONS,
        retry_max_attempts: ee_openrouter_agent::config::DEFAULT_RETRY_MAX_ATTEMPTS,
        retry_base_delay: Duration::from_millis(
            ee_openrouter_agent::config::DEFAULT_RETRY_BASE_DELAY_MS,
        ),
        retry_max_delay: Duration::from_millis(
            ee_openrouter_agent::config::DEFAULT_RETRY_MAX_DELAY_MS,
        ),
        checkpoint_dir: None,
    }
}

/// Starts a session, consumes the provider's `available_commands_update`
/// advertisement, and returns its id.
async fn new_session(harness: &Harness, id: i64) -> String {
    harness.send(request(id, "session/new", session_new_params("/work")));
    let result = request_result(harness.next_frame().await);
    let session_id = result["sessionId"].as_str().expect("session id").to_string();
    let advertisement = update_of(&harness.next_frame().await);
    assert_eq!(advertisement["sessionUpdate"], "available_commands_update");
    assert_eq!(advertisement["availableCommands"][0]["name"], "compact");
    session_id
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
    let frames = harness.next_frames(2).await;
    let first = request_result(frames[0].clone());
    assert_eq!(first["sessionId"], "openrouter-1");
    assert_eq!(update_of(&frames[1])["sessionUpdate"], "available_commands_update");

    harness.send(request(2, "session/new", session_new_params("/work")));
    let frames = harness.next_frames(2).await;
    let second = request_result(frames[0].clone());
    assert_eq!(second["sessionId"], "openrouter-2");
    assert_eq!(update_of(&frames[1])["sessionUpdate"], "available_commands_update");

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

#[tokio::test(flavor = "current_thread")]
async fn prompt_response_carries_reported_turn_token_usage() {
    let mock = MockOpenRouter::start(vec![answer_response_with_usage("done", 6120, 2311)]);
    let provider = OpenRouterProvider::new(test_config(&mock)).unwrap();
    let (harness, task) = Harness::spawn(provider).await;
    let session_id = new_session(&harness, 1).await;

    harness.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));

    // Answer chunk, usage_update notification, then the response.
    let frames = harness.next_frames(3).await;
    let result = request_result(frames[2].clone());
    assert_eq!(result["stopReason"], "end_turn");
    assert_eq!(result["usage"]["totalTokens"], 8431);
    assert_eq!(result["usage"]["inputTokens"], 6120);
    assert_eq!(result["usage"]["outputTokens"], 2311);

    harness.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn provider_emits_context_usage_update_with_used_and_window() {
    let mock = MockOpenRouter::start(vec![answer_response_with_usage("done", 6120, 2311)]);
    let provider = OpenRouterProvider::new(test_config(&mock)).unwrap();
    let (harness, task) = Harness::spawn(provider).await;
    let session_id = new_session(&harness, 1).await;

    harness.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));

    // Answer chunk, usage_update notification, then the response.
    let frames = harness.next_frames(3).await;
    let update = update_of(&frames[1]);
    assert_eq!(update["sessionUpdate"], "usage_update");
    assert_eq!(update["used"], 6120, "used is the current context sent to the model");
    assert_eq!(
        update["size"], DEFAULT_CONTEXT_WINDOW_TOKENS,
        "size is the configured context window"
    );

    harness.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_usage_emits_no_context_usage_update() {
    let mock = MockOpenRouter::start(vec![answer_response("done")]);
    let provider = OpenRouterProvider::new(test_config(&mock)).unwrap();
    let (harness, task) = Harness::spawn(provider).await;
    let session_id = new_session(&harness, 1).await;

    harness.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));

    let frames = harness.next_frames(2).await;
    assert_eq!(update_of(&frames[0])["sessionUpdate"], "agent_message_chunk");
    assert_eq!(request_result(frames[1].clone())["stopReason"], "end_turn");

    harness.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn tool_loop_turn_usage_aggregates_across_rounds() {
    let mock = MockOpenRouter::start(vec![
        tool_call_response("call_1", "/tmp/notes.txt"),
        answer_response_with_usage("read it", 100, 50),
    ]);
    let provider = OpenRouterProvider::new(test_config(&mock)).unwrap();
    let (harness, task) = Harness::spawn(provider).await;
    let session_id = new_session(&harness, 1).await;

    harness.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));

    // Tool in-progress update, then the agent → client fs request.
    let frames = harness.next_frames(2).await;
    let fs_request = &frames[1];
    harness.send(respond_to(fs_request, Ok(json!({ "content": "file contents" }))));

    // Tool completion, answer chunk, usage_update, then the response with
    // aggregated usage.
    let frames = harness.next_frames(4).await;
    let update = update_of(&frames[2]);
    assert_eq!(update["sessionUpdate"], "usage_update");
    assert_eq!(update["used"], 100, "context usage follows the last round");
    let result = request_result(frames[3].clone());
    assert_eq!(result["stopReason"], "end_turn");
    assert_eq!(result["usage"]["totalTokens"], 150);
    assert_eq!(result["usage"]["inputTokens"], 100);
    assert_eq!(result["usage"]["outputTokens"], 50);

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

#[tokio::test]
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

// ── /compact history compaction ───────────────────────────────────────

/// Runs one normal prompt turn to completion (answer only).
async fn run_normal_turn(harness: &Harness, session_id: &str, request_id: i64, text: &str) {
    harness.send(request(request_id, "session/prompt", prompt_params(session_id, text)));
    let frames = harness.next_frames(2).await;
    assert_eq!(update_of(&frames[0])["sessionUpdate"], "agent_message_chunk");
    let result = request_result(frames[1].clone());
    assert_eq!(result["stopReason"], "end_turn");
}

#[tokio::test]
async fn compact_noop_small_history_never_calls_the_model() {
    let mock = MockOpenRouter::start(vec![answer_response("first answer")]);
    let provider = OpenRouterProvider::new(test_config(&mock)).unwrap();
    let (harness, task) = Harness::spawn(provider).await;
    let session_id = new_session(&harness, 1).await;

    // One turn stores two messages; the configured minimum is four.
    run_normal_turn(&harness, &session_id, 2, "hello").await;
    assert_eq!(mock.request_bodies().len(), 1);

    harness.send(request(3, "session/prompt", prompt_params(&session_id, "/compact")));
    let frames = harness.next_frames(2).await;
    let notice = update_of(&frames[0]);
    assert_eq!(notice["sessionUpdate"], "agent_message_chunk");
    assert!(
        notice["content"]["text"].as_str().unwrap().contains("no compaction needed"),
        "{notice}"
    );
    let result = request_result(frames[1].clone());
    assert_eq!(result["stopReason"], "end_turn");
    assert_eq!(
        mock.request_bodies().len(),
        1,
        "a small history must not produce a compaction model call"
    );

    harness.shutdown(task).await;
}

#[tokio::test]
async fn auto_compact_runs_before_the_next_prompt_after_near_limit_usage() {
    let mock = MockOpenRouter::start(vec![
        answer_response_with_usage("first answer", 80, 10),
        answer_response("SESSION SUMMARY"),
        answer_response("second answer"),
    ]);
    let mut config = test_config(&mock);
    config.context_window = 100;
    config.auto_compact_threshold_percent = 80;
    let provider = OpenRouterProvider::new(config).unwrap();
    let (harness, task) = Harness::spawn(provider).await;
    let session_id = new_session(&harness, 1).await;

    harness.send(request(2, "session/prompt", prompt_params(&session_id, "first question")));
    let frames = harness.next_frames(3).await;
    assert_eq!(update_of(&frames[0])["content"]["text"], "first answer");
    assert_eq!(update_of(&frames[1])["sessionUpdate"], "usage_update");
    assert_eq!(request_result(frames[2].clone())["stopReason"], "end_turn");

    // The stored history has only two messages, below the manual minimum of
    // four. Reported near-limit usage still forces safe automatic compaction.
    harness.send(request(3, "session/prompt", prompt_params(&session_id, "second question")));
    let frames = harness.next_frames(3).await;
    let status = update_of(&frames[0]);
    assert!(
        status["content"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("Session automatically compacted")),
        "{status}"
    );
    assert_eq!(update_of(&frames[1])["content"]["text"], "second answer");
    assert_eq!(request_result(frames[2].clone())["stopReason"], "end_turn");

    let bodies = mock.request_bodies();
    assert_eq!(bodies.len(), 3, "normal turn, automatic compaction, next turn");
    assert!(bodies[1].get("tools").is_none(), "automatic compaction has no tools");
    let next_messages = bodies[2]["messages"].as_array().expect("messages array");
    assert!(
        next_messages[1]["content"]
            .as_str()
            .is_some_and(|text| text.contains("Session summary:\nSESSION SUMMARY")),
        "next request receives the automatic summary: {next_messages:?}"
    );
    assert_eq!(next_messages.last().expect("new user prompt")["content"], "second question");

    harness.shutdown(task).await;
}

#[tokio::test]
async fn compact_replaces_history_with_summary_and_retained_tail() {
    let mock = MockOpenRouter::start(vec![
        answer_response("answer one"),
        answer_response("answer two"),
        answer_response("answer three"),
        answer_response("SESSION SUMMARY CONTENT"),
        answer_response("next answer"),
    ]);
    let provider = OpenRouterProvider::new(test_config(&mock)).unwrap();
    let (harness, task) = Harness::spawn(provider).await;
    let session_id = new_session(&harness, 1).await;

    // Three turns store six messages (user/assistant each).
    run_normal_turn(&harness, &session_id, 2, "remember sk-live-1234567890").await;
    run_normal_turn(&harness, &session_id, 3, "second question").await;
    run_normal_turn(&harness, &session_id, 4, "third question").await;
    assert_eq!(mock.request_bodies().len(), 3);

    harness.send(request(5, "session/prompt", prompt_params(&session_id, "/compact keep API v2")));
    let frames = harness.next_frames(2).await;
    let status = update_of(&frames[0]);
    assert_eq!(status["sessionUpdate"], "agent_message_chunk");
    let status_text = status["content"]["text"].as_str().expect("status text");
    assert!(status_text.contains("Session compacted: 6 messages"), "{status_text}");
    assert!(status_text.contains("2 tail messages"), "{status_text}");
    let result = request_result(frames[1].clone());
    assert_eq!(result["stopReason"], "end_turn");

    // The compaction request: no tools, instructions preserved, and the
    // stored secret redacted before it reaches the wire.
    let bodies = mock.request_bodies();
    assert_eq!(bodies.len(), 4);
    let compaction_body = &bodies[3];
    assert!(compaction_body.get("tools").is_none(), "no tools during compaction");
    let compaction_text = compaction_body.to_string();
    assert!(compaction_text.contains("keep API v2"), "instructions preserved: {compaction_text}");
    assert!(
        !compaction_text.contains("sk-live-1234567890"),
        "secret redacted from the compaction request: {compaction_text}"
    );
    assert!(compaction_text.contains("[redacted]"), "{compaction_text}");
    assert!(!status_text.contains("sk-live-1234567890"), "secret redacted from the status text");

    // The next turn sees the compacted history: summary message first,
    // then the two retained tail messages.
    run_normal_turn(&harness, &session_id, 6, "next question").await;
    let bodies = mock.request_bodies();
    let messages = bodies[4]["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 5, "system + summary + two tail + new user");
    assert!(messages[1]["content"].as_str().unwrap().contains("SESSION SUMMARY CONTENT"));
    assert!(messages[1]["content"].as_str().unwrap().contains("Session summary:"));
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[2]["content"], "third question");
    assert_eq!(messages[3]["role"], "assistant");
    assert_eq!(messages[3]["content"], "answer three");
    assert_eq!(messages[4]["role"], "user");
    assert_eq!(messages[4]["content"], "next question");
    assert!(
        !messages[1].to_string().contains("sk-live-1234567890"),
        "summary storage redacts secrets"
    );

    harness.shutdown(task).await;
}

#[tokio::test]
async fn compact_tail_keeps_tool_call_result_pairs_consistent() {
    // A tool round then one plain round: stored history is
    // [user, assistant(tool_call), tool result, assistant, user, assistant].
    // The retained tail of 4 starts at the tool result, so the pair rule
    // must pull the assistant tool call back into the tail.
    let mock = MockOpenRouter::start(vec![
        tool_call_response("call_1", "/tmp/notes.txt"),
        answer_response("answer one"),
        answer_response("answer two"),
        answer_response("SUMMARY"),
        answer_response("final"),
    ]);
    let config = test_config_with(&mock, 4, 4, 65_536);
    let provider = OpenRouterProvider::new(config).unwrap();
    let (harness, task) = Harness::spawn(provider).await;
    let session_id = new_session(&harness, 1).await;

    // Turn 1: a tool round answered through the client bridge.
    harness.send(request(2, "session/prompt", prompt_params(&session_id, "read notes")));
    let frames = harness.next_frames(2).await;
    assert_eq!(update_of(&frames[0])["sessionUpdate"], "tool_call_update");
    let fs_request = &frames[1];
    harness.send(respond_to(fs_request, Ok(json!({ "content": "file contents" }))));
    let frames = harness.next_frames(3).await;
    assert_eq!(update_of(&frames[0])["status"], "completed");
    assert_eq!(update_of(&frames[1])["content"]["text"], "answer one");
    assert_eq!(request_result(frames[2].clone())["stopReason"], "end_turn");
    // Turn 2: a plain answer round.
    run_normal_turn(&harness, &session_id, 3, "second question").await;

    harness.send(request(4, "session/prompt", prompt_params(&session_id, "/compact")));
    let frames = harness.next_frames(2).await;
    let status = update_of(&frames[0]);
    assert!(status["content"]["text"].as_str().unwrap().contains("5 tail messages"));
    let result = request_result(frames[1].clone());
    assert_eq!(result["stopReason"], "end_turn");

    // The next turn's request shows the compacted history: summary plus
    // the pair-consistent tail (assistant tool call + its tool result).
    run_normal_turn(&harness, &session_id, 5, "next").await;
    let bodies = mock.request_bodies();
    let messages = bodies[4]["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 8, "system + summary + five tail + new user");
    assert!(messages[1]["content"].as_str().unwrap().contains("SUMMARY"));
    let tail = &messages[2..7];
    assert!(
        tail[0].get("tool_calls").is_some(),
        "assistant tool call retained with its result: {tail:?}"
    );
    assert_eq!(tail[0]["tool_calls"][0]["id"], "call_1");
    assert_eq!(tail[1]["role"], "tool");
    assert_eq!(tail[1]["tool_call_id"], "call_1");
    assert_eq!(tail[2]["content"], "answer one");
    assert_eq!(tail[3]["role"], "user");
    assert_eq!(tail[3]["content"], "second question");
    assert_eq!(tail[4]["role"], "assistant");
    assert_eq!(tail[4]["content"], "answer two");

    harness.shutdown(task).await;
}

#[tokio::test]
async fn compact_bounds_the_serialized_request_input() {
    let mock = MockOpenRouter::start(vec![
        answer_response("answer one"),
        answer_response("answer two"),
        answer_response("answer three"),
        answer_response("SUMMARY"),
        answer_response("final"),
    ]);
    // A tight bound forces front trimming while keeping the request
    // serialized size at or under the configured maximum.
    let config = test_config_with(&mock, 4, 2, 500);
    let provider = OpenRouterProvider::new(config).unwrap();
    let (harness, task) = Harness::spawn(provider).await;
    let session_id = new_session(&harness, 1).await;

    run_normal_turn(&harness, &session_id, 2, "alpha oldest question").await;
    run_normal_turn(&harness, &session_id, 3, "beta second question").await;
    run_normal_turn(&harness, &session_id, 4, "gamma third question").await;

    harness.send(request(5, "session/prompt", prompt_params(&session_id, "/compact")));
    let frames = harness.next_frames(2).await;
    assert_eq!(update_of(&frames[0])["sessionUpdate"], "agent_message_chunk");
    assert_eq!(request_result(frames[1].clone())["stopReason"], "end_turn");

    let bodies = mock.request_bodies();
    let compaction_body = &bodies[3];
    let serialized = compaction_body["messages"].to_string();
    assert!(serialized.len() <= 500, "compaction request bounded: {} bytes", serialized.len());
    assert!(
        !compaction_body.to_string().contains("alpha oldest question")
            && !compaction_body.to_string().contains("beta second question"),
        "oldest messages trimmed for the bound: {compaction_body}"
    );
    assert!(
        compaction_body.to_string().contains("gamma third question"),
        "newest history stays inside the bound"
    );

    harness.shutdown(task).await;
}

#[tokio::test]
async fn compact_rejects_empty_summary_keeping_history_unchanged() {
    let mock = MockOpenRouter::start(vec![
        answer_response("answer one"),
        answer_response("answer two"),
        answer_response(""),
        answer_response("after failure"),
    ]);
    let provider = OpenRouterProvider::new(test_config(&mock)).unwrap();
    let (harness, task) = Harness::spawn(provider).await;
    let session_id = new_session(&harness, 1).await;

    run_normal_turn(&harness, &session_id, 2, "first question").await;
    run_normal_turn(&harness, &session_id, 3, "second question").await;

    harness.send(request(4, "session/prompt", prompt_params(&session_id, "/compact")));
    let error = request_error(harness.next_frame().await);
    assert_eq!(i32::from(error.code), -32603);
    assert!(error.message.contains("empty compaction summary"), "{error}");

    // The stored history is untouched: the next turn still sends all four
    // original messages (no summary message inserted).
    run_normal_turn(&harness, &session_id, 5, "third question").await;
    let bodies = mock.request_bodies();
    let messages = bodies[3]["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 6, "system + four original messages + new user");
    assert_eq!(messages[1]["content"], "first question");
    assert_eq!(messages[2]["content"], "answer one");
    assert_eq!(messages[3]["content"], "second question");
    assert_eq!(messages[4]["content"], "answer two");
    assert!(
        !messages.iter().any(|message| message["content"]
            .as_str()
            .is_some_and(|text| text.contains("Session summary:"))),
        "no summary may be stored after an empty-summary rejection"
    );

    harness.shutdown(task).await;
}
