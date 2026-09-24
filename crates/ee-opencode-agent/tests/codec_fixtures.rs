//! Codec fixture suite: no live account and no network dependency.
//!
//! Every supported OpenCode route is exercised at the codec level: a request body
//! is built from a normalized transcript and a fixture response for that route's
//! dialect decodes into a normalized `ModelResponse`. The suite also proves the
//! dialects do not blur — a payload produced for one dialect is rejected by the
//! other dialects' decoders — and that a closed consumer stops a stream without
//! being retried.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ee_acp_agent_server::ProviderError;
use ee_agent_orchestrator::{
    ModelMessage, ModelResponse, ModelRole, SideEffectClass, ToolDefinition, ToolResult,
};
use ee_chat_completions::{
    ChunkFuture, ChunkSource, DeltaSink, RetryPolicy, StreamAttempt, StreamDelta, StreamFuture,
    drive_sse, with_stream_retry,
};
use ee_opencode_agent::messages;
use ee_opencode_agent::responses;
use ee_opencode_agent::routes::{self, OpenCodeDialect, OpenCodeRoute};
use serde_json::{Value, json};

const LABEL: &str = "OpenCode fixture";

fn transcript() -> Vec<ModelMessage> {
    vec![
        ModelMessage::text(ModelRole::System, "Memory facts:\ncwd: /work"),
        ModelMessage::text(ModelRole::User, "read .ee.toml"),
        ModelMessage::text(ModelRole::Assistant, "reading it"),
        ModelMessage::tool_result("call_1", ToolResult::success("theme = \"dark\"")),
    ]
}

fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition::new("read_file", "reads a file")
        .side_effect_class(SideEffectClass::Read)
        .input_schema(
            json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }),
        )]
}

/// Builds the dialect request for one resolved route.
fn encode(route: &OpenCodeRoute, stream: bool) -> Value {
    match route.dialect {
        OpenCodeDialect::OpenAiResponses => responses::request_body(
            route.model_id,
            "system",
            &transcript(),
            &definitions(),
            stream,
            None,
        ),
        OpenCodeDialect::AnthropicMessages => messages::request_body(
            route.model_id,
            "system",
            &transcript(),
            &definitions(),
            stream,
            None,
        ),
        OpenCodeDialect::OpenAiChatCompletions => ee_chat_completions::request_body(
            route.model_id,
            &ee_chat_completions::messages_from_transcript("system", &transcript()),
            &ee_chat_completions::tools_from_definitions(&definitions()),
            stream,
            None,
        ),
    }
}

/// One completed text fixture per dialect, shaped exactly like the endpoint.
fn text_fixture(dialect: OpenCodeDialect) -> Value {
    match dialect {
        OpenCodeDialect::OpenAiResponses => json!({
            "id": "resp_fixture",
            "object": "response",
            "status": "completed",
            "output": [{ "type": "message", "role": "assistant", "content": [
                { "type": "output_text", "text": "answer" }
            ] }],
            "usage": { "input_tokens": 3, "output_tokens": 2, "total_tokens": 5 }
        }),
        OpenCodeDialect::AnthropicMessages => json!({
            "id": "msg_fixture",
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "text", "text": "answer" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 3, "output_tokens": 2 }
        }),
        OpenCodeDialect::OpenAiChatCompletions => json!({
            "choices": [{ "message": { "content": "answer" }, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5 }
        }),
    }
}

/// One tool-call fixture per dialect, carrying the same call identity.
fn tool_fixture(dialect: OpenCodeDialect) -> Value {
    match dialect {
        OpenCodeDialect::OpenAiResponses => json!({
            "object": "response",
            "status": "completed",
            "output": [{ "type": "function_call", "call_id": "call_1", "name": "read_file", "arguments": "{\"path\":\"/x\"}" }]
        }),
        OpenCodeDialect::AnthropicMessages => json!({
            "type": "message",
            "content": [{ "type": "tool_use", "id": "call_1", "name": "read_file", "input": { "path": "/x" } }],
            "stop_reason": "tool_use"
        }),
        OpenCodeDialect::OpenAiChatCompletions => json!({
            "choices": [{
                "message": { "content": null, "tool_calls": [{
                    "id": "call_1", "type": "function",
                    "function": { "name": "read_file", "arguments": "{\"path\":\"/x\"}" }
                }] },
                "finish_reason": "tool_calls"
            }]
        }),
    }
}

/// Decodes one payload with the dialect's decoder.
fn decode(dialect: OpenCodeDialect, value: &Value) -> Result<ModelResponse, ProviderError> {
    match dialect {
        OpenCodeDialect::OpenAiResponses => responses::response_from_buffered(LABEL, value),
        OpenCodeDialect::AnthropicMessages => messages::response_from_buffered(LABEL, value),
        OpenCodeDialect::OpenAiChatCompletions => ee_chat_completions::decode_message(value)
            .map(ee_chat_completions::response_from_turn)
            .ok_or_else(|| {
                ProviderError::BackendFailure(String::from("fixture has no assistant message"))
            }),
    }
}

fn dialects() -> [OpenCodeDialect; 3] {
    [
        OpenCodeDialect::OpenAiResponses,
        OpenCodeDialect::AnthropicMessages,
        OpenCodeDialect::OpenAiChatCompletions,
    ]
}

#[test]
fn every_catalog_route_builds_a_dialect_shaped_request_with_its_own_model() {
    for route in routes::catalog() {
        for stream in [false, true] {
            let body = encode(&route, stream);

            assert_eq!(body["model"], route.model_id, "{}", route.endpoint);
            assert_eq!(body["stream"], stream, "{}", route.endpoint);
            match route.dialect {
                OpenCodeDialect::OpenAiResponses => {
                    assert!(body["input"].is_array(), "{}", route.endpoint);
                    assert!(body["instructions"].is_string(), "{}", route.endpoint);
                }
                OpenCodeDialect::AnthropicMessages => {
                    assert!(body["max_tokens"].is_number(), "{}", route.endpoint);
                    assert_eq!(body["tool_choice"]["type"], "auto");
                    assert!(!body["messages"].as_array().expect("messages").is_empty());
                }
                OpenCodeDialect::OpenAiChatCompletions => {
                    assert_eq!(body["tool_choice"], "auto");
                    assert_eq!(body["messages"][0]["role"], "system");
                }
            }
        }
    }
}

#[test]
fn every_catalog_route_decodes_a_text_fixture() {
    for route in routes::catalog() {
        let response =
            decode(route.dialect, &text_fixture(route.dialect)).unwrap_or_else(|error| {
                panic!("{} {}: {error}", route.surface.as_str(), route.model_id)
            });

        assert_eq!(response.text, "answer", "{}", route.model_id);
        assert!(response.completed, "{}", route.model_id);
        assert!(response.tool_intents.is_empty());
        assert_eq!(response.usage.input_tokens, Some(3), "{}", route.model_id);
        assert_eq!(response.usage.output_tokens, Some(2));
    }
}

#[test]
fn every_catalog_route_decodes_a_tool_fixture_with_identity() {
    for route in routes::catalog() {
        let response =
            decode(route.dialect, &tool_fixture(route.dialect)).unwrap_or_else(|error| {
                panic!("{} {}: {error}", route.surface.as_str(), route.model_id)
            });

        assert_eq!(response.tool_intents.len(), 1, "{}", route.model_id);
        let intent = &response.tool_intents[0];
        assert_eq!(intent.tool_call_id, "call_1", "{}", route.model_id);
        assert_eq!(intent.name, "read_file");
        assert_eq!(intent.arguments["path"], "/x");
        assert!(!response.completed, "{}", route.model_id);
    }
}

#[test]
fn no_dialect_decoder_accepts_another_dialect_response() {
    for source in dialects() {
        for target in dialects() {
            if source == target {
                continue;
            }
            for fixture in [text_fixture(source), tool_fixture(source)] {
                assert!(
                    decode(target, &fixture).is_err(),
                    "{source:?} fixture was accepted by {target:?}: {fixture}"
                );
            }
        }
    }
}

#[test]
fn no_dialect_decoder_accepts_another_dialect_request() {
    let route_of = |dialect: OpenCodeDialect| {
        routes::catalog().find(|route| route.dialect == dialect).expect("dialect is routable")
    };
    for source in dialects() {
        let request = encode(&route_of(source), false);
        for target in dialects() {
            if source == target {
                continue;
            }
            assert!(
                decode(target, &request).is_err(),
                "{source:?} request was accepted by {target:?}: {request}"
            );
        }
    }
}

/// A chunk source replaying one scripted stream.
struct Chunks {
    chunks: Vec<Vec<u8>>,
}

impl ChunkSource for Chunks {
    fn next_chunk(&mut self) -> ChunkFuture<'_> {
        Box::pin(async move {
            if self.chunks.is_empty() {
                return Ok(None);
            }
            Ok(Some(self.chunks.remove(0)))
        })
    }
}

/// A streaming attempt that decodes a scripted Responses stream through the real
/// codec decoder, so cancellation and retry behavior are exercised end to end.
struct FixtureAttempt {
    chunks: Vec<Vec<u8>>,
    runs: Arc<AtomicUsize>,
}

impl StreamAttempt<ModelResponse> for FixtureAttempt {
    fn run<'a>(&'a mut self, sink: DeltaSink<'a>) -> StreamFuture<'a, ModelResponse> {
        Box::pin(async move {
            self.runs.fetch_add(1, Ordering::SeqCst);
            let mut source = Chunks { chunks: self.chunks.clone() };
            let mut decoder = responses::StreamDecoder::new(LABEL);
            drive_sse(LABEL, &mut source, |event| decoder.apply(event, sink)).await?;
            decoder.finish()
        })
    }
}

fn events(values: &[Value]) -> Vec<Vec<u8>> {
    values.iter().map(|value| format!("data: {value}\n\n").into_bytes()).collect()
}

#[tokio::test(start_paused = true)]
async fn a_closed_consumer_cancels_the_stream_and_is_never_retried() {
    let runs = Arc::new(AtomicUsize::new(0));
    let mut attempt = FixtureAttempt {
        chunks: events(&[
            json!({ "type": "response.output_text.delta", "delta": "first" }),
            json!({ "type": "response.output_text.delta", "delta": "second" }),
        ]),
        runs: runs.clone(),
    };
    let policy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(10),
        max_delay: Duration::from_millis(10),
    };
    let mut sink =
        |_delta: StreamDelta| -> Result<(), ProviderError> { Err(ProviderError::Cancellation) };

    let error = with_stream_retry(&policy, &mut sink, &mut attempt)
        .await
        .expect_err("a cancelled consumer stops the turn");

    assert!(matches!(error, ProviderError::Cancellation), "{error:?}");
    assert_eq!(runs.load(Ordering::SeqCst), 1, "a cancelled stream is never retried");
}
