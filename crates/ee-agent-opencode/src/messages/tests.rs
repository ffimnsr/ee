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
    let body = request_body(
        "kimi-k3",
        "system",
        &transcript(),
        &definitions(),
        false,
        DEFAULT_MAX_OUTPUT_TOKENS,
        None,
    );

    assert_eq!(body["model"], "kimi-k3");
    assert_eq!(body["system"][0]["text"], "system");
    assert_eq!(body["system"][1]["text"], "Memory facts:\ncwd: /work");
    assert!(
        body["system"][1].get("cache_control").is_none(),
        "the markerless builder places no breakpoints"
    );
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
    assert!(
        body["tools"][0].get("cache_control").is_none(),
        "the markerless builder places no breakpoints"
    );
}

/// Every `cache_control` marker in an assembled Messages body.
fn cache_markers(body: &Value) -> Vec<Value> {
    let mut markers = Vec::new();
    for part in body["system"].as_array().into_iter().flatten() {
        markers.extend(part.get("cache_control").cloned());
    }
    for tool in body["tools"].as_array().into_iter().flatten() {
        markers.extend(tool.get("cache_control").cloned());
    }
    for message in body["messages"].as_array().into_iter().flatten() {
        for block in message["content"].as_array().into_iter().flatten() {
            markers.extend(block.get("cache_control").cloned());
        }
    }
    markers
}

#[test]
fn automatic_policy_places_the_four_reference_breakpoints() {
    let tools: Vec<ToolDefinition> =
        (0..10).map(|index| ToolDefinition::new(format!("tool_{index}"), "t")).collect();
    let transcript = vec![
        ModelMessage::text(ModelRole::System, "project rules"),
        ModelMessage::text(ModelRole::User, "current request"),
    ];
    let cache = ee_chat_completions::CacheDirective::anthropic();
    let body = messages_body(&MessagesBody {
        model: "claude",
        provider: None,
        system_prompt: "system",
        transcript: &transcript,
        tools: &tools,
        stream: false,
        max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        extensions: None,
        tool_control: &ee_agent_orchestrator::ToolCallControl::auto(),
        cache: &cache,
        prompt_tokens: None,
        generation: None,
    });

    assert_eq!(cache_markers(&body).len(), 4, "the API allows four breakpoints: {body}");
    assert_eq!(body["tools"][9]["cache_control"]["type"], "ephemeral");
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(body["system"][1]["cache_control"]["type"], "ephemeral");
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"]["type"], "ephemeral",
        "the conversation tail is the fourth breakpoint"
    );
}

#[test]
fn manual_breakpoints_are_preserved_and_counted() {
    let transcript: Vec<ModelMessage> = (0..5)
        .map(|index| {
            ModelMessage::text(ModelRole::User, format!("step {index}")).with_cache_breakpoint()
        })
        .collect();
    let cache = ee_chat_completions::CacheDirective {
        policy: ee_chat_completions::CachePolicy::Explicit(
            ee_chat_completions::CachePolicyObject {
                tools: false,
                system: false,
                messages: None,
                ttl: None,
            },
        ),
        ..ee_chat_completions::CacheDirective::anthropic()
    };
    let body = messages_body(&MessagesBody {
        model: "claude",
        provider: None,
        system_prompt: "system",
        transcript: &transcript,
        tools: &[ToolDefinition::new("read_file", "reads a file")],
        stream: false,
        max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        extensions: None,
        tool_control: &ee_agent_orchestrator::ToolCallControl::auto(),
        cache: &cache,
        prompt_tokens: None,
        generation: None,
    });

    let markers = cache_markers(&body);
    assert_eq!(markers.len(), 4, "manual markers are capped at four: {body}");
    assert_eq!(body["messages"][1]["content"][0]["text"], "step 1", "text is never rewritten");
    assert_eq!(body["messages"][1]["content"][0]["cache_control"]["type"], "ephemeral");
    assert!(
        body["messages"][4]["content"][0].get("cache_control").is_none(),
        "the fifth manual marker is dropped instead of emitting an invalid request"
    );
}

#[test]
fn request_body_groups_consecutive_tool_results_into_one_turn() {
    let transcript = vec![
        ModelMessage::tool_result("call_1", ToolResult::success("one")),
        ModelMessage::tool_result("call_2", ToolResult::success("two")),
    ];

    let body = request_body("m", "", &transcript, &[], false, DEFAULT_MAX_OUTPUT_TOKENS, None);

    assert_eq!(body["messages"].as_array().expect("messages").len(), 1);
    assert_eq!(body["messages"][0]["content"].as_array().expect("blocks").len(), 2);
    assert_eq!(body["messages"][0]["content"][1]["tool_use_id"], "call_2");
    assert!(body.get("system").is_none(), "no system content means no system field");
}

#[test]
fn request_body_extension_overrides_max_tokens_and_streams() {
    let extensions = json!({ "max_tokens": 256 });

    let body =
        request_body("m", "system", &[], &[], true, DEFAULT_MAX_OUTPUT_TOKENS, Some(&extensions));

    assert_eq!(body["stream"], true);
    assert_eq!(body["max_tokens"], 256);
}

#[test]
fn request_body_never_carries_a_credential() {
    let body = request_body(
        "m",
        "system",
        &transcript(),
        &definitions(),
        true,
        DEFAULT_MAX_OUTPUT_TOKENS,
        None,
    )
    .to_string();

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
        deltas[..3],
        [
            StreamDelta::Reasoning(String::from("hmm ")),
            StreamDelta::Text(String::from("ans")),
            StreamDelta::Text(String::from("wer")),
        ]
    );
    assert!(matches!(deltas.last(), Some(StreamDelta::Usage(_))));
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

    assert!(
        deltas.iter().all(|delta| matches!(
            delta,
            StreamDelta::ToolInputStart { .. }
                | StreamDelta::ToolInputDelta { .. }
                | StreamDelta::ToolInputEnd { .. }
        )),
        "only display-only tool-input events stream before the call completes: {deltas:?}"
    );
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

// ── Gap 4: thinking-block continuity ─────────────────────────────────────

#[test]
fn second_turn_replays_the_signed_thinking_block() {
    let decoded = response_from_buffered(
        LABEL,
        &json!({
            "type": "message",
            "content": [
                { "type": "thinking", "thinking": "consider", "signature": "sig-123" },
                { "type": "text", "text": "answer" }
            ],
            "stop_reason": "end_turn"
        }),
    )
    .expect("decodes");
    let decoded = ee_chat_completions::seal_response(decoded, "claude-x");
    assert!(decoded.native.is_some(), "decoded thinking state must be replayable");

    let mut history = ee_agent_orchestrator::Transcript::new();
    history.push_assistant(&decoded);
    let body = request_body("claude-x", "sys", history.messages(), &[], false, 4_096, None);

    let content = body["messages"][0]["content"].as_array().expect("content blocks");
    assert_eq!(content[0]["type"], "thinking");
    assert_eq!(content[0]["thinking"], "consider");
    assert_eq!(content[0]["signature"], "sig-123", "signature must replay byte-identically");
    assert_eq!(content[1]["type"], "text");
}

#[test]
fn provider_scoped_thinking_blocks_require_the_same_provider() {
    let decoded = response_from_buffered(
        LABEL,
        &json!({
            "type": "message",
            "content": [
                { "type": "thinking", "thinking": "consider", "signature": "sig-123" },
                { "type": "text", "text": "answer" }
            ],
            "stop_reason": "end_turn"
        }),
    )
    .expect("decodes");
    let decoded =
        ee_chat_completions::seal_response_with_provider(decoded, Some("OpenCode"), "claude-x");
    let mut history = ee_agent_orchestrator::Transcript::new();
    history.push_assistant(&decoded);
    let message = &history.messages[0];
    assert!(replay_thinking_blocks(message, Some("OpenCode"), "claude-x").is_some());
    assert!(replay_thinking_blocks(message, Some("OpenRouter"), "claude-x").is_none());
    assert!(replay_thinking_blocks(message, None, "claude-x").is_none());
}

#[test]
fn thinking_blocks_never_replay_to_another_model() {
    let decoded = response_from_buffered(
        LABEL,
        &json!({
            "type": "message",
            "content": [
                { "type": "thinking", "thinking": "consider", "signature": "sig-123" },
                { "type": "text", "text": "answer" }
            ],
            "stop_reason": "end_turn"
        }),
    )
    .expect("decodes");
    let decoded = ee_chat_completions::seal_response(decoded, "claude-x");

    let mut history = ee_agent_orchestrator::Transcript::new();
    history.push_assistant(&decoded);
    let body = request_body("claude-y", "sys", history.messages(), &[], false, 4_096, None);

    let content = body["messages"][0]["content"].as_array().expect("content blocks");
    assert_eq!(content.len(), 1, "only text may be replayed across models");
    assert_eq!(content[0]["type"], "text");
}

#[test]
fn unsigned_thinking_blocks_are_not_replayed() {
    let decoded = response_from_buffered(
        LABEL,
        &json!({
            "type": "message",
            "content": [
                { "type": "thinking", "thinking": "consider" },
                { "type": "text", "text": "answer" }
            ],
            "stop_reason": "end_turn"
        }),
    )
    .expect("decodes");
    let decoded = ee_chat_completions::seal_response(decoded, "claude-x");

    let mut history = ee_agent_orchestrator::Transcript::new();
    history.push_assistant(&decoded);
    let body = request_body("claude-x", "sys", history.messages(), &[], false, 4_096, None);

    let content = body["messages"][0]["content"].as_array().expect("content blocks");
    assert_eq!(content.len(), 1, "an unsigned thinking block is rejected by Anthropic");
    assert_eq!(content[0]["type"], "text");
}

#[test]
fn streamed_thinking_signature_survives_into_the_next_request() {
    let mut decoder = StreamDecoder::new(LABEL);
    let mut sink = |_delta: StreamDelta| Ok(());
    for event in [
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "thinking", "thinking": "" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "thinking_delta", "thinking": "plan" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "signature_delta", "signature": "sig-stream" } }),
        json!({ "type": "content_block_stop", "index": 0 }),
        json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "text", "text": "" } }),
        json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "text_delta", "text": "answer" } }),
        // Anthropic closes a stream with `message_delta.stop_reason`; without it
        // the turn counts as cut and its tail state is never replayed.
        json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" } }),
        json!({ "type": "message_stop" }),
    ] {
        decoder.apply(&event.to_string(), &mut sink).expect("applies");
    }
    let decoded = decoder.finish().expect("finishes");
    let decoded = ee_chat_completions::seal_response(decoded, "claude-x");

    let mut history = ee_agent_orchestrator::Transcript::new();
    history.push_assistant(&decoded);
    let body = request_body("claude-x", "sys", history.messages(), &[], false, 4_096, None);
    let content = body["messages"][0]["content"].as_array().expect("content blocks");
    assert_eq!(content[0]["type"], "thinking");
    assert_eq!(content[0]["signature"], "sig-stream");
}

#[test]
fn assistant_tool_call_and_thinking_replay_before_its_result() {
    let decoded = response_from_buffered(
        LABEL,
        &json!({
            "type": "message",
            "content": [
                { "type": "thinking", "thinking": "plan", "signature": "sig-tool" },
                { "type": "tool_use", "id": "tu-1", "name": "read_file", "input": { "path": "a" } }
            ],
            "stop_reason": "tool_use"
        }),
    )
    .expect("decodes");
    let decoded = ee_chat_completions::seal_response(decoded, "claude-x");

    let mut history = ee_agent_orchestrator::Transcript::new();
    history.push_assistant(&decoded);
    history.push_tool_result("tu-1", ToolResult::success("file body"));

    let body = request_body("claude-x", "sys", history.messages(), &[], false, 4_096, None);
    let messages = body["messages"].as_array().expect("messages");
    let assistant = messages[0]["content"].as_array().expect("assistant blocks");
    assert_eq!(assistant[0]["type"], "thinking");
    assert_eq!(assistant[0]["signature"], "sig-tool");
    assert_eq!(assistant[1]["type"], "tool_use");
    assert_eq!(assistant[1]["id"], "tu-1");
    assert_eq!(assistant[1]["name"], "read_file");
    assert_eq!(assistant[1]["input"]["path"], "a");

    let observation = messages[1]["content"].as_array().expect("tool result blocks");
    assert_eq!(observation[0]["type"], "tool_result");
    assert_eq!(observation[0]["tool_use_id"], "tu-1", "the result must answer a replayed call");
}

#[test]
fn tool_choice_lowers_to_the_anthropic_shape() {
    let cache = ee_chat_completions::CacheDirective::disabled();
    let build = |control: &ee_agent_orchestrator::ToolCallControl| {
        messages_body(&MessagesBody {
            model: "claude",
            provider: None,
            system_prompt: "system",
            transcript: &transcript(),
            tools: &definitions(),
            stream: false,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            extensions: None,
            tool_control: control,
            cache: &cache,
            prompt_tokens: None,
            generation: None,
        })
    };

    assert_eq!(
        build(&ee_agent_orchestrator::ToolCallControl::auto())["tool_choice"]["type"],
        "auto"
    );
    assert_eq!(
        build(&ee_agent_orchestrator::ToolCallControl::required())["tool_choice"]["type"],
        "any"
    );
    let named = build(&ee_agent_orchestrator::ToolCallControl::named("read_file"));
    assert_eq!(named["tool_choice"]["type"], "tool");
    assert_eq!(named["tool_choice"]["name"], "read_file");

    let parallel =
        build(&ee_agent_orchestrator::ToolCallControl::auto().with_parallel(Some(false)));
    assert_eq!(parallel["tool_choice"]["disable_parallel_tool_use"], true);
}

#[test]
fn user_attachments_route_images_and_pdfs_to_native_blocks() {
    let transcript = vec![ModelMessage::new(ModelRole::User).with_content(vec![
        ModelContent::Text("look".into()),
        ModelContent::Image { data: "aW1n".into(), mime_type: "image/png".into(), uri: None },
        ModelContent::EmbeddedResource {
            uri: "file:///scan.pdf".into(),
            mime_type: Some("application/pdf".into()),
            text: None,
            blob: Some("JVBERi0=".into()),
        },
    ])];
    let cache = CacheDirective::disabled();
    let body = messages_body(&MessagesBody {
        model: "claude",
        provider: None,
        system_prompt: "system",
        transcript: &transcript,
        tools: &[],
        stream: false,
        max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        extensions: None,
        tool_control: &ee_agent_orchestrator::ToolCallControl::auto(),
        cache: &cache,
        prompt_tokens: None,
        generation: None,
    });
    let blocks = body["messages"][0]["content"].as_array().expect("content blocks");
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[1]["type"], "image");
    assert_eq!(blocks[1]["source"]["media_type"], "image/png");
    assert_eq!(blocks[2]["type"], "document");
    assert_eq!(blocks[2]["source"]["media_type"], "application/pdf");
    assert_eq!(blocks[2]["source"]["data"], "JVBERi0=");
}

fn body_with(max_output_tokens: u64, extensions: Value) -> Value {
    let control = ee_agent_orchestrator::ToolCallControl::auto();
    let cache = CacheDirective::disabled();
    messages_body(&MessagesBody {
        model: "claude",
        provider: None,
        system_prompt: "system",
        transcript: &transcript(),
        tools: &definitions(),
        stream: false,
        max_output_tokens,
        extensions: Some(&extensions),
        tool_control: &control,
        cache: &cache,
        prompt_tokens: None,
        generation: None,
    })
}

#[test]
fn thinking_budget_is_clamped_below_the_output_bound() {
    let extensions = json!({ "thinking": { "type": "enabled", "budget_tokens": 16_000 } });
    let body = body_with(8_000, extensions.clone());
    assert_eq!(body["thinking"]["budget_tokens"], 7_999);

    let body = body_with(32_000, extensions);
    assert_eq!(body["thinking"]["budget_tokens"], 16_000, "a fitting budget is untouched");
}

#[test]
fn thinking_is_dropped_when_no_valid_budget_fits() {
    let body =
        body_with(1_024, json!({ "thinking": { "type": "enabled", "budget_tokens": 16_000 } }));
    assert!(body.get("thinking").is_none(), "no budget above the 1024 floor fits");
}

#[test]
fn disabled_thinking_passes_through_unchanged() {
    let body = body_with(8_000, json!({ "thinking": { "type": "disabled" } }));
    assert_eq!(body["thinking"]["type"], "disabled");
}

// ── Sampling knobs ─────────────────────────────────────────────────

fn sampling_body(generation: &ee_agent_orchestrator::GenerationOptions) -> Value {
    let transcript = vec![ModelMessage::text(ModelRole::User, "hello")];
    let cache = ee_chat_completions::CacheDirective::disabled();
    messages_body(&MessagesBody {
        model: "claude",
        provider: None,
        system_prompt: "system",
        transcript: &transcript,
        tools: &[],
        stream: false,
        max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        extensions: None,
        tool_control: &ee_agent_orchestrator::ToolCallControl::auto(),
        cache: &cache,
        prompt_tokens: None,
        generation: Some(generation),
    })
}

#[test]
fn no_sampling_key_appears_when_nothing_is_configured() {
    let body = sampling_body(&ee_agent_orchestrator::GenerationOptions::none());
    for key in ["temperature", "top_p", "top_k", "stop_sequences", "seed", "stop"] {
        assert!(body.get(key).is_none(), "{key} must stay absent");
    }
}

#[test]
fn messages_lowers_its_sampling_subset_and_renames_stop() {
    let generation = ee_agent_orchestrator::GenerationOptions {
        temperature: Some(0.3),
        top_p: Some(0.8),
        top_k: Some(25),
        seed: Some(5),
        stop: Some(vec![String::from("END")]),
        ..ee_agent_orchestrator::GenerationOptions::none()
    };
    let body = sampling_body(&generation);
    assert_eq!(body["temperature"], 0.3);
    assert_eq!(body["top_p"], 0.8);
    assert_eq!(body["top_k"], 25);
    assert_eq!(body["stop_sequences"], json!(["END"]));
    assert!(body.get("seed").is_none(), "Messages documents no seed");
    assert!(body.get("stop").is_none(), "stop is renamed, never sent raw");
}
