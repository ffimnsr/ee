use ee_agent_orchestrator::{ModelRole, SideEffectClass, ToolDefinition, ToolResult};
use ee_chat_completions::{ChunkFuture, ChunkSource, StreamDelta, drive_sse};
use serde_json::{Value, json};

use super::*;

const LABEL: &str = "OpenCode go kimi-k3";

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
fn request_body_hoists_system_and_correlates_tool_results() {
    let body = request_body("kimi-k3", "system", &transcript(), &definitions(), false, None);

    assert_eq!(body["model"], "kimi-k3");
    assert_eq!(body["system"], "system\n\nMemory facts:\ncwd: /work");
    assert_eq!(body["max_tokens"], DEFAULT_MAX_OUTPUT_TOKENS);
    assert_eq!(body["stream"], false);
    assert_eq!(body["tool_choice"]["type"], "auto");
    assert_eq!(body["messages"].as_array().expect("messages").len(), 3);
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"][0]["type"], "text");
    assert_eq!(body["messages"][0]["content"][0]["text"], "hello");
    assert_eq!(body["messages"][1]["role"], "assistant");
    let tool_turn = &body["messages"][2];
    assert_eq!(tool_turn["role"], "user", "tool results arrive as a user turn");
    assert_eq!(tool_turn["content"][0]["type"], "tool_result");
    assert_eq!(tool_turn["content"][0]["tool_use_id"], "call_1", "identity survives");
    assert_eq!(tool_turn["content"][0]["content"], "file contents");
    assert_eq!(body["tools"][0]["name"], "read_file");
    assert_eq!(body["tools"][0]["input_schema"]["required"][0], "path");
    assert_eq!(body["tools"][0]["description"], "reads a file");
}

#[test]
fn request_body_groups_consecutive_tool_results_into_one_turn() {
    let transcript = vec![
        ModelMessage::tool_result("call_1", ToolResult::success("one")),
        ModelMessage::tool_result("call_2", ToolResult::success("two")),
    ];

    let body = request_body("m", "", &transcript, &[], false, None);

    assert_eq!(body["messages"].as_array().expect("messages").len(), 1);
    assert_eq!(body["messages"][0]["content"].as_array().expect("blocks").len(), 2);
    assert_eq!(body["messages"][0]["content"][1]["tool_use_id"], "call_2");
    assert!(body.get("system").is_none(), "no system content means no system field");
}

#[test]
fn request_body_extension_overrides_max_tokens_and_streams() {
    let extensions = json!({ "max_tokens": 256 });

    let body = request_body("m", "system", &[], &[], true, Some(&extensions));

    assert_eq!(body["stream"], true);
    assert_eq!(body["max_tokens"], 256);
}

#[test]
fn request_body_never_carries_a_credential() {
    let body = request_body("m", "system", &transcript(), &definitions(), true, None).to_string();

    assert!(!body.contains("api_key"), "{body}");
    assert!(!body.contains("authorization"), "{body}");
    assert!(!body.contains("Bearer"), "{body}");
}

#[test]
fn buffered_text_thinking_and_tool_use_map_to_normalized_response() {
    let value = json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "content": [
            { "type": "thinking", "thinking": "check first" },
            { "type": "text", "text": "done" },
            { "type": "tool_use", "id": "toolu_1", "name": "read_file", "input": { "path": "/x" } }
        ],
        "stop_reason": "tool_use",
        "usage": { "input_tokens": 10, "output_tokens": 4 }
    });

    let response = response_from_buffered(LABEL, &value).expect("decodes");

    assert_eq!(response.text, "done");
    assert_eq!(response.reasoning.as_deref(), Some("check first"));
    assert_eq!(response.tool_intents.len(), 1);
    assert_eq!(response.tool_intents[0].tool_call_id, "toolu_1");
    assert_eq!(response.tool_intents[0].name, "read_file");
    assert_eq!(response.tool_intents[0].arguments["path"], "/x");
    assert!(!response.completed, "tool_use continues the turn");
    assert_eq!(response.usage.input_tokens, Some(10));
    assert_eq!(response.usage.output_tokens, Some(4));
}

#[test]
fn buffered_stop_reasons_map_to_completion_state() {
    let cases = [
        ("end_turn", true),
        ("stop_sequence", true),
        ("refusal", true),
        ("tool_use", false),
        ("max_tokens", false),
        ("pause_turn", false),
    ];
    for (stop_reason, completed) in cases {
        let value = json!({
            "type": "message",
            "content": [{ "type": "text", "text": "answer" }],
            "stop_reason": stop_reason,
        });

        let response = response_from_buffered(LABEL, &value).expect("decodes");

        assert_eq!(response.completed, completed, "stop_reason {stop_reason}");
    }

    let missing = json!({ "type": "message", "content": [{ "type": "text", "text": "answer" }] });
    assert!(response_from_buffered(LABEL, &missing).expect("decodes").completed);
}

#[test]
fn buffered_usage_omission_stays_unknown() {
    let value = json!({ "type": "message", "content": [{ "type": "text", "text": "answer" }] });

    let response = response_from_buffered(LABEL, &value).expect("decodes");

    assert_eq!(response.usage.input_tokens, None);
    assert_eq!(response.usage.output_tokens, None);
}

#[test]
fn buffered_error_payload_fails_closed() {
    let value = json!({ "type": "error", "error": { "type": "overloaded_error", "message": "overloaded" } });

    let error = response_from_buffered(LABEL, &value).expect_err("fails");

    assert!(error.to_string().contains("overloaded"), "{error}");
    assert!(error.to_string().contains(LABEL), "{error}");
}

#[test]
fn buffered_tool_use_without_identity_fails_closed() {
    for block in [
        json!({ "type": "tool_use", "name": "read_file", "input": {} }),
        json!({ "type": "tool_use", "id": "toolu_1", "input": {} }),
    ] {
        let value = json!({ "type": "message", "content": [block] });
        assert!(response_from_buffered(LABEL, &value).is_err(), "{value}");
    }
}

#[test]
fn buffered_malformed_tool_input_keeps_identity_with_null_arguments() {
    let value = json!({
        "type": "message",
        "content": [{ "type": "tool_use", "id": "toolu_1", "name": "read_file", "input": "[1]" }],
        "stop_reason": "tool_use"
    });

    let response = response_from_buffered(LABEL, &value).expect("decodes");

    assert_eq!(response.tool_intents.len(), 1);
    assert_eq!(response.tool_intents[0].arguments, Value::Null);
}

#[test]
fn buffered_payload_from_another_dialect_fails_closed() {
    let responses_shaped = json!({ "object": "response", "status": "completed", "output": [] });
    let chat_shaped = json!({ "choices": [{ "message": { "content": "answer" } }] });

    assert!(response_from_buffered(LABEL, &responses_shaped).is_err());
    assert!(response_from_buffered(LABEL, &chat_shaped).is_err());
}

#[tokio::test]
async fn stream_lifecycle_maps_deltas_usage_and_stop_reason() {
    let stream = events(&[
        json!({ "type": "message_start", "message": { "usage": { "input_tokens": 9 } } }),
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "thinking" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "thinking_delta", "thinking": "hmm " } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "ans" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "wer" } }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 3 } }),
        json!({ "type": "message_stop" }),
    ]);

    let (response, deltas) = decode_stream(stream).await.expect("decodes");

    assert_eq!(
        deltas,
        vec![
            StreamDelta::Reasoning(String::from("hmm ")),
            StreamDelta::Text(String::from("ans")),
            StreamDelta::Text(String::from("wer")),
        ]
    );
    assert_eq!(response.text, "answer");
    assert_eq!(response.reasoning.as_deref(), Some("hmm "));
    assert!(response.completed);
    assert_eq!(response.usage.input_tokens, Some(9));
    assert_eq!(response.usage.output_tokens, Some(3));
}

#[tokio::test]
async fn stream_fragmented_tool_input_is_reassembled_across_byte_boundaries() {
    let start = event(json!({ "type": "content_block_start", "index": 0, "content_block": {
        "type": "tool_use", "id": "toolu_1", "name": "read_file"
    } }))
    .into_bytes();
    let delta = event(json!({ "type": "content_block_delta", "index": 0, "delta": {
        "type": "input_json_delta", "partial_json": "{\"path\":\"café.txt\"}"
    } }))
    .into_bytes();
    // Split the event in the middle of the multi-byte character, so framing
    // proves it never decodes half a UTF-8 sequence.
    let split = delta
        .windows(2)
        .position(|window| window == "é".as_bytes())
        .expect("multi-byte character is present")
        + 1;
    let stream = vec![
        start,
        delta[..split].to_vec(),
        delta[split..].to_vec(),
        event(json!({ "type": "content_block_stop", "index": 0 })).into_bytes(),
        event(json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" } }))
            .into_bytes(),
        event(json!({ "type": "message_stop" })).into_bytes(),
    ];

    let (response, deltas) = decode_stream(stream).await.expect("decodes");

    assert!(deltas.is_empty(), "tool input never reaches the sink before the call is complete");
    assert_eq!(response.tool_intents.len(), 1);
    assert_eq!(response.tool_intents[0].tool_call_id, "toolu_1");
    assert_eq!(response.tool_intents[0].arguments["path"], "café.txt");
    assert!(!response.completed);
}

#[tokio::test]
async fn stream_multiple_tool_blocks_stay_separate() {
    let stream = events(&[
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": "toolu_1", "name": "read_file" } }),
        json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "tool_use", "id": "toolu_2", "name": "read_file" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "{\"path\":\"/a\"}" } }),
        json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": "{\"path\":\"/b\"}" } }),
    ]);

    let (response, _deltas) = decode_stream(stream).await.expect("decodes");

    assert_eq!(response.tool_intents.len(), 2);
    assert_eq!(response.tool_intents[0].tool_call_id, "toolu_1");
    assert_eq!(response.tool_intents[0].arguments["path"], "/a");
    assert_eq!(response.tool_intents[1].tool_call_id, "toolu_2");
    assert_eq!(response.tool_intents[1].arguments["path"], "/b");
}

#[tokio::test]
async fn stream_tool_use_without_identity_fails_closed() {
    let stream = events(&[json!({ "type": "content_block_start", "index": 0, "content_block": {
        "type": "tool_use", "name": "read_file"
    } })]);

    let error = decode_stream(stream).await.expect_err("fails");

    assert!(error.to_string().contains("missing its id"), "{error}");
}

#[tokio::test]
async fn stream_error_event_fails_closed() {
    let stream =
        events(&[json!({ "type": "error", "error": { "type": "api_error", "message": "boom" } })]);

    let error = decode_stream(stream).await.expect_err("fails");

    assert!(error.to_string().contains("boom"), "{error}");
}

#[tokio::test]
async fn stream_ignores_ping_and_unknown_events() {
    let stream = events(&[
        json!({ "type": "ping" }),
        json!({ "type": "content_block_stop", "index": 5 }),
        json!({ "type": "some_future_event", "delta": { "type": "text_delta", "text": "ignored" } }),
        json!({ "type": "message_stop" }),
    ]);

    let (response, deltas) = decode_stream(stream).await.expect("decodes");

    assert_eq!(response.text, "");
    assert!(deltas.is_empty());
}

#[tokio::test]
async fn stream_without_stop_reason_is_treated_as_a_finished_turn() {
    let stream = events(&[
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "answer" } }),
        json!({ "type": "message_stop" }),
    ]);

    let (response, _deltas) = decode_stream(stream).await.expect("decodes");

    assert_eq!(response.text, "answer");
    assert!(response.completed);
    assert_eq!(response.usage.input_tokens, None, "omitted usage stays unknown");
}
