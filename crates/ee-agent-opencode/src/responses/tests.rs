use ee_agent_orchestrator::{ModelRole, SideEffectClass, ToolDefinition, ToolResult};
use ee_chat_completions::{ChunkFuture, ChunkSource, StreamDelta, drive_sse};
use serde_json::{Value, json};

use super::*;

const LABEL: &str = "OpenCode zen gpt-5.5";

fn transcript() -> Vec<ModelMessage> {
    vec![
        ModelMessage::text(ModelRole::System, "Memory facts:\ncwd: /work"),
        ModelMessage::text(ModelRole::User, "hello"),
        ModelMessage::text(ModelRole::Assistant, "hi there"),
        ModelMessage::tool_result("call_1", ToolResult::success("file contents")),
    ]
}

fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition::new("read_file", "reads a file")
        .side_effect_class(SideEffectClass::Read)
        .input_schema(
            json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }),
        )]
}

/// A chunk source replaying scripted SSE bytes.
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

fn event(value: Value) -> String {
    format!("data: {value}\n\n")
}

/// Builds SSE byte chunks from event payloads.
fn events(values: &[Value]) -> Vec<Vec<u8>> {
    values.iter().map(|value| event(value.clone()).into_bytes()).collect()
}

/// Builds one raw byte chunk.
fn raw(text: &str) -> Vec<u8> {
    text.as_bytes().to_vec()
}

/// Drives one scripted SSE byte stream through the decoder.
async fn decode_stream(
    chunks: Vec<Vec<u8>>,
) -> Result<(ModelResponse, Vec<StreamDelta>), ProviderError> {
    let mut source = Chunks { chunks };
    let mut decoder = StreamDecoder::new(LABEL);
    let deltas = {
        let mut deltas = Vec::new();
        {
            let mut sink = |delta: StreamDelta| -> Result<(), ProviderError> {
                deltas.push(delta);
                Ok(())
            };
            drive_sse(LABEL, &mut source, |event| decoder.apply(event, &mut sink)).await?;
        }
        deltas
    };
    Ok((decoder.finish()?, deltas))
}

#[test]
fn request_body_carries_instructions_input_items_and_tools() {
    let body = request_body("gpt-5.5", "system", &transcript(), &definitions(), false, None);

    assert_eq!(body["model"], "gpt-5.5");
    assert_eq!(body["instructions"], "system");
    assert_eq!(body["stream"], false);
    assert_eq!(body["store"], false);
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["input"].as_array().expect("input items").len(), 4);
    assert_eq!(body["input"][0]["type"], "message");
    assert_eq!(body["input"][0]["role"], "system");
    assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
    assert_eq!(body["input"][0]["content"][0]["text"], "Memory facts:\ncwd: /work");
    assert_eq!(body["input"][1]["role"], "user");
    assert_eq!(body["input"][2]["role"], "assistant");
    assert_eq!(body["input"][2]["content"][0]["type"], "output_text");
    assert_eq!(body["input"][3]["type"], "function_call_output");
    assert_eq!(body["input"][3]["call_id"], "call_1", "tool identity survives");
    assert_eq!(body["input"][3]["output"], "file contents");
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["name"], "read_file");
    assert_eq!(body["tools"][0]["parameters"]["required"][0], "path");
}

#[test]
fn request_body_streams_and_merges_extensions() {
    let extensions = json!({ "reasoning": { "effort": "low" } });

    let body = request_body("m", "system", &[], &[], true, Some(&extensions));

    assert_eq!(body["stream"], true);
    assert_eq!(body["reasoning"]["effort"], "low");
    assert_eq!(body["input"].as_array().expect("input items").len(), 0);
}

#[test]
fn request_body_omits_empty_text_and_empty_instructions() {
    let transcript =
        vec![ModelMessage::text(ModelRole::User, ""), ModelMessage::text(ModelRole::User, "kept")];

    let body = request_body("m", "", &transcript, &[], false, None);

    assert!(body.get("instructions").is_none());
    assert!(body.get("tools").is_none());
    assert_eq!(body["input"].as_array().expect("input items").len(), 1);
    assert_eq!(body["input"][0]["content"][0]["text"], "kept");
}

#[test]
fn request_body_never_carries_a_credential() {
    let body = request_body("m", "system", &transcript(), &definitions(), true, None).to_string();

    assert!(!body.contains("api_key"), "{body}");
    assert!(!body.contains("authorization"), "{body}");
    assert!(!body.contains("Bearer"), "{body}");
}

#[test]
fn buffered_text_only_completes_without_usage() {
    let value = json!({
        "id": "resp_1",
        "object": "response",
        "status": "completed",
        "output": [
            { "type": "message", "role": "assistant", "content": [
                { "type": "output_text", "text": "answer" }
            ] }
        ]
    });

    let response = response_from_buffered(LABEL, &value).expect("decodes");

    assert_eq!(response.text, "answer");
    assert!(response.completed);
    assert!(response.tool_intents.is_empty());
    assert_eq!(response.usage.input_tokens, None, "unknown usage stays unknown");
    assert_eq!(response.usage.output_tokens, None);
}

#[test]
fn buffered_reasoning_summary_maps_to_reasoning() {
    let value = json!({
        "object": "response",
        "status": "completed",
        "output": [
            { "type": "reasoning", "summary": [{ "type": "summary_text", "text": "check first" }] },
            { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "done" }] }
        ],
        "usage": { "input_tokens": 10, "output_tokens": 4, "total_tokens": 14 }
    });

    let response = response_from_buffered(LABEL, &value).expect("decodes");

    assert_eq!(response.reasoning.as_deref(), Some("check first"));
    assert_eq!(response.text, "done");
    assert_eq!(response.usage.input_tokens, Some(10));
    assert_eq!(response.usage.output_tokens, Some(4));
}

#[test]
fn buffered_function_call_maps_to_tool_intent() {
    let value = json!({
        "object": "response",
        "status": "completed",
        "output": [
            { "type": "function_call", "call_id": "call_1", "name": "tool_read_file", "arguments": "{\"path\":\"/x\"}" }
        ]
    });

    let response = response_from_buffered(LABEL, &value).expect("decodes");

    assert_eq!(response.tool_intents.len(), 1);
    assert_eq!(response.tool_intents[0].tool_call_id, "call_1");
    assert_eq!(response.tool_intents[0].name, "read_file", "legacy alias maps to the builtin");
    assert_eq!(response.tool_intents[0].arguments["path"], "/x");
    assert!(!response.completed, "tool calls continue the turn");
}

#[test]
fn buffered_multiple_function_calls_keep_their_identity() {
    let value = json!({
        "object": "response",
        "status": "completed",
        "output": [
            { "type": "function_call", "call_id": "call_1", "name": "read_file", "arguments": "{\"path\":\"/a\"}" },
            { "type": "function_call", "call_id": "call_2", "name": "read_file", "arguments": "{\"path\":\"/b\"}" }
        ]
    });

    let response = response_from_buffered(LABEL, &value).expect("decodes");

    assert_eq!(response.tool_intents.len(), 2);
    assert_eq!(response.tool_intents[0].tool_call_id, "call_1");
    assert_eq!(response.tool_intents[1].tool_call_id, "call_2");
    assert_eq!(response.tool_intents[1].arguments["path"], "/b");
}

#[test]
fn buffered_malformed_arguments_keep_identity_with_null_arguments() {
    let value = json!({
        "object": "response",
        "status": "completed",
        "output": [
            { "type": "function_call", "call_id": "call_1", "name": "read_file", "arguments": "{\"path\":" }
        ]
    });

    let response = response_from_buffered(LABEL, &value).expect("decodes");

    assert_eq!(response.tool_intents.len(), 1);
    assert_eq!(response.tool_intents[0].tool_call_id, "call_1");
    assert_eq!(response.tool_intents[0].arguments, Value::Null);
    assert!(!response.completed);
}

#[test]
fn buffered_incomplete_status_is_not_completed() {
    let value = json!({
        "object": "response",
        "status": "incomplete",
        "incomplete_details": { "reason": "max_output_tokens" },
        "output": [{ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "half" }] }]
    });

    let response = response_from_buffered(LABEL, &value).expect("decodes");

    assert_eq!(response.text, "half");
    assert!(!response.completed, "a truncated response is not a finished turn");
}

#[test]
fn buffered_error_payload_fails_closed() {
    let value = json!({ "error": { "code": "invalid_request", "message": "bad model" } });

    let error = response_from_buffered(LABEL, &value).expect_err("fails");

    assert!(error.to_string().contains("bad model"), "{error}");
    assert!(error.to_string().contains(LABEL), "{error}");
}

#[test]
fn buffered_failed_status_fails_closed() {
    let value = json!({ "object": "response", "status": "failed", "error": { "message": "upstream down" } });

    let error = response_from_buffered(LABEL, &value).expect_err("fails");

    assert!(error.to_string().contains("upstream down"), "{error}");
}

#[test]
fn buffered_function_call_without_identity_fails_closed() {
    for output in [
        json!([{ "type": "function_call", "name": "read_file", "arguments": "{}" }]),
        json!([{ "type": "function_call", "call_id": "call_1", "arguments": "{}" }]),
    ] {
        let value = json!({ "object": "response", "status": "completed", "output": output });
        assert!(response_from_buffered(LABEL, &value).is_err(), "{value}");
    }
}

#[test]
fn buffered_payload_from_another_dialect_fails_closed() {
    let messages_shaped = json!({
        "type": "message",
        "content": [{ "type": "text", "text": "answer" }],
        "stop_reason": "end_turn"
    });
    let chat_shaped = json!({ "choices": [{ "message": { "content": "answer" } }] });

    assert!(response_from_buffered(LABEL, &messages_shaped).is_err());
    assert!(response_from_buffered(LABEL, &chat_shaped).is_err());
}

#[tokio::test]
async fn stream_text_and_reasoning_deltas_reach_the_sink_in_order() {
    let stream = events(&[
        json!({ "type": "response.created" }),
        json!({ "type": "response.reasoning_summary_text.delta", "delta": "think " }),
        json!({ "type": "response.output_text.delta", "delta": "ans" }),
        json!({ "type": "response.output_text.delta", "delta": "wer" }),
        json!({ "type": "response.completed", "response": {
            "status": "completed",
            "usage": { "input_tokens": 7, "output_tokens": 3 }
        } }),
    ]);

    let (response, deltas) = decode_stream(stream).await.expect("decodes");

    assert_eq!(
        deltas,
        vec![
            StreamDelta::Reasoning(String::from("think ")),
            StreamDelta::Text(String::from("ans")),
            StreamDelta::Text(String::from("wer")),
        ]
    );
    assert_eq!(response.text, "answer");
    assert_eq!(response.reasoning.as_deref(), Some("think "));
    assert!(response.completed);
    assert_eq!(response.usage.input_tokens, Some(7));
    assert_eq!(response.usage.output_tokens, Some(3));
}

#[tokio::test]
async fn stream_fragmented_call_is_reassembled_across_byte_boundaries() {
    let stream = vec![
        raw("data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\
             \"function_call\",\"call_id\":\"call_1\",\"name\":\"read_file\",\"arguments\":\"\"}}\n\n\
             data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\
             \"{\\\"path\\\":\\\"caf"),
        raw("é.txt\\\"}\"}\n\n"),
        event(json!({ "type": "response.function_call_arguments.done", "output_index": 0, "arguments": "{\"path\":\"café.txt\"}" }))
            .into_bytes(),
    ];

    let (response, deltas) = decode_stream(stream).await.expect("decodes");

    assert!(deltas.is_empty(), "argument deltas never reach the sink before the call is complete");
    assert_eq!(response.tool_intents.len(), 1);
    assert_eq!(response.tool_intents[0].tool_call_id, "call_1");
    assert_eq!(response.tool_intents[0].arguments["path"], "café.txt");
    assert!(!response.completed);
}

#[tokio::test]
async fn stream_multiple_tool_calls_stay_separate() {
    let stream = events(&[
        json!({ "type": "response.output_item.added", "output_index": 0, "item": {
            "type": "function_call", "call_id": "call_1", "name": "read_file", "arguments": ""
        } }),
        json!({ "type": "response.output_item.added", "output_index": 1, "item": {
            "type": "function_call", "call_id": "call_2", "name": "read_file", "arguments": ""
        } }),
        json!({ "type": "response.function_call_arguments.delta", "output_index": 0, "delta": "{\"path\":\"/a\"}" }),
        json!({ "type": "response.function_call_arguments.delta", "output_index": 1, "delta": "{\"path\":\"/b\"}" }),
    ]);

    let (response, _deltas) = decode_stream(stream).await.expect("decodes");

    assert_eq!(response.tool_intents.len(), 2);
    assert_eq!(response.tool_intents[0].tool_call_id, "call_1");
    assert_eq!(response.tool_intents[0].arguments["path"], "/a");
    assert_eq!(response.tool_intents[1].tool_call_id, "call_2");
    assert_eq!(response.tool_intents[1].arguments["path"], "/b");
}

#[tokio::test]
async fn stream_incomplete_function_call_fails_closed() {
    let stream = events(&[
        json!({ "type": "response.function_call_arguments.delta", "output_index": 0, "delta": "{}" }),
    ]);

    let error = decode_stream(stream).await.expect_err("fails");

    assert!(error.to_string().contains("missing its call id"), "{error}");
}

#[tokio::test]
async fn stream_failure_and_error_events_fail_closed() {
    let failed = events(&[json!({ "type": "response.failed", "response": {
        "status": "failed",
        "error": { "message": "upstream down" }
    } })]);
    let error = decode_stream(failed).await.expect_err("fails");
    assert!(error.to_string().contains("upstream down"), "{error}");

    let terminal = events(&[json!({ "type": "error", "code": "server_error", "message": "boom" })]);
    let error = decode_stream(terminal).await.expect_err("fails");
    assert!(error.to_string().contains("boom"), "{error}");
}

#[tokio::test]
async fn stream_output_text_done_fills_missing_deltas_and_unknown_events_are_ignored() {
    let stream = events(&[
        json!({ "type": "response.in_progress" }),
        json!({ "type": "response.some_future_event", "delta": "ignored" }),
        json!({ "type": "response.output_text.done", "text": "final text" }),
        json!({ "type": "response.completed", "response": { "status": "completed" } }),
    ]);

    let (response, deltas) = decode_stream(stream).await.expect("decodes");

    assert_eq!(response.text, "final text");
    assert_eq!(deltas, vec![StreamDelta::Text(String::from("final text"))]);
    assert_eq!(response.usage.input_tokens, None, "omitted usage stays unknown");
}

#[tokio::test]
async fn stream_incomplete_status_is_not_completed() {
    let stream = events(&[json!({ "type": "response.incomplete", "response": {
        "status": "incomplete",
        "incomplete_details": { "reason": "max_output_tokens" }
    } })]);

    let (response, _deltas) = decode_stream(stream).await.expect("decodes");

    assert!(!response.completed);
}

#[tokio::test]
async fn stream_stops_when_the_sink_is_closed() {
    let stream = events(&[
        json!({ "type": "response.output_text.delta", "delta": "first" }),
        json!({ "type": "response.output_text.delta", "delta": "second" }),
    ]);
    let mut source = Chunks { chunks: stream };
    let mut decoder = StreamDecoder::new(LABEL);
    let mut sink =
        |_delta: StreamDelta| -> Result<(), ProviderError> { Err(ProviderError::Cancellation) };

    let error = drive_sse(LABEL, &mut source, |event| decoder.apply(event, &mut sink))
        .await
        .expect_err("a closed consumer stops the stream");

    assert!(matches!(error, ProviderError::Cancellation), "{error:?}");
    assert_eq!(source.chunks.len(), 1, "later chunks are never read");
}
