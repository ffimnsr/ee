//! Recorded-request fixtures for the OpenRouter provider.
//!
//! Each test drives the real `OpenRouterModelAdapter` (profile construction,
//! catalog gates, headers, and codec) against the scripted in-process endpoint,
//! then asserts both the recorded request body and the decoded response for one
//! dialect. No live account and no network dependency.

use std::time::Duration;

use ee_agent_openrouter::orchestrated::OpenRouterModelAdapter;
use ee_agent_orchestrator::{
    BudgetSnapshot, ModelAdapter, ModelMessage, ModelRequest, ModelRole, TaskId, TaskNode,
};

use super::*;

/// A Responses-shaped answer, matching the documented non-streaming body.
fn responses_answer(text: &str) -> Value {
    json!({
        "id": "resp_1",
        "status": "completed",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": text }]
        }],
        "usage": {
            "input_tokens": 12,
            "output_tokens": 3,
            "total_tokens": 15
        }
    })
}

/// A configuration pointing at the scripted endpoint for one API kind.
fn fixture_config(
    mock: &MockOpenRouter,
    model: &str,
    api_kind: ee_agent_openrouter::config::OpenRouterApiKind,
) -> Config {
    let mut config = test_config(mock);
    config.model = model.to_string();
    config.api_kind = api_kind;
    config.responses_url = mock.responses_url();
    config.system_prompt = String::from("fixture system");
    config.timeouts = ee_chat_completions::Timeouts::new().with_total(Some(Duration::from_secs(5)));
    config
}

/// One recorded prompt/answer exchange.
fn fixture_request() -> ModelRequest {
    ModelRequest::new(
        vec![
            ModelMessage::text(ModelRole::User, "recorded prompt"),
            ModelMessage::text(ModelRole::Assistant, "recorded answer"),
        ],
        Vec::new(),
        BudgetSnapshot::default(),
        TaskNode::new(TaskId::new("fixture"), "fixture", "fixture"),
    )
}

async fn complete(adapter: &OpenRouterModelAdapter) -> ee_agent_orchestrator::ModelResponse {
    let (_tx, rx) = tokio::sync::watch::channel(false);
    adapter.complete(fixture_request(), rx).await.expect("fixture completes")
}

#[tokio::test]
async fn chat_request_and_response_are_recorded() {
    let mock = MockOpenRouter::start(vec![answer_response("recorded answer")]);
    let adapter = OpenRouterModelAdapter::new(fixture_config(
        &mock,
        "qwen/qwen3-coder",
        ee_agent_openrouter::config::OpenRouterApiKind::Chat,
    ))
    .expect("chat adapter builds");

    let response = complete(&adapter).await;
    assert_eq!(response.text, "recorded answer");

    let bodies = mock.request_bodies();
    assert_eq!(bodies.len(), 1, "one recorded chat request");
    let body = &bodies[0];
    assert_eq!(body["model"], "qwen/qwen3-coder");
    assert_eq!(body["stream"], false, "the buffered path requests a JSON body");
    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages[0]["role"], "system");
    // A cache-declaring route carries the system prompt as a marked part array.
    let system_text = match &messages[0]["content"] {
        Value::String(text) => text.clone(),
        parts => parts
            .as_array()
            .expect("system parts")
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    };
    assert_eq!(system_text, "fixture system");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "recorded prompt");
    assert!(body.get("tools").is_none(), "no tools were advertised");
}

#[tokio::test]
async fn responses_request_and_response_are_recorded() {
    let mock = MockOpenRouter::start(vec![responses_answer("recorded answer")]);
    let adapter = OpenRouterModelAdapter::new(fixture_config(
        &mock,
        "anthropic/claude-haiku-4.5",
        ee_agent_openrouter::config::OpenRouterApiKind::Responses,
    ))
    .expect("responses adapter builds");

    let response = complete(&adapter).await;
    assert_eq!(response.text, "recorded answer");
    assert_eq!(response.usage.input_tokens, Some(12));
    assert_eq!(response.response_id.as_deref(), Some("resp_1"));

    let bodies = mock.request_bodies();
    assert_eq!(bodies.len(), 1, "one recorded Responses request");
    let body = &bodies[0];
    assert_eq!(body["model"], "anthropic/claude-haiku-4.5");
    assert_eq!(body["stream"], false);
    assert_eq!(body["instructions"], "fixture system");
    assert_eq!(body["store"], false, "no continuation anchor was supplied");
    assert!(body.get("previous_response_id").is_none());
    let include: Vec<&str> = body["include"]
        .as_array()
        .expect("include array")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(include.contains(&"reasoning.encrypted_content"), "{include:?}");
    let items = body["input"].as_array().expect("input items");
    assert!(
        items.iter().any(|item| item["role"] == "user" && item["type"] == "message"),
        "the prompt reaches the input items: {items:?}"
    );
}

#[tokio::test]
async fn recorded_usage_round_trips_into_the_normalized_response() {
    let mut answer = answer_response("usage answer");
    answer["usage"] = json!({
        "prompt_tokens": 20,
        "completion_tokens": 5,
        "total_tokens": 25,
        "prompt_tokens_details": { "cached_tokens": 8 }
    });
    let mock = MockOpenRouter::start(vec![answer]);
    let adapter = OpenRouterModelAdapter::new(fixture_config(
        &mock,
        "qwen/qwen3-coder",
        ee_agent_openrouter::config::OpenRouterApiKind::Chat,
    ))
    .expect("chat adapter builds");

    let response = complete(&adapter).await;
    assert_eq!(response.usage.input_tokens, Some(20));
    assert_eq!(response.usage.output_tokens, Some(5));
    assert_eq!(response.usage.cached_input_tokens, Some(8));
    assert_eq!(response.usage.reasoning_tokens, None, "unknown stays unknown");
    assert!(
        response.usage.cost_micros.is_some(),
        "the catalog prices a known row when the provider reports no cost"
    );
}
