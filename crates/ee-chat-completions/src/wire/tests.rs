use std::time::Duration;

use ee_agent_orchestrator::{
    ModelMessage, ModelRole, SideEffectClass, ToolCallControl, ToolDefinition, ToolFormat,
    ToolResult,
};
use reqwest::header::HeaderMap;
use serde_json::json;

use super::*;
use crate::cache_policy::{
    CacheDirective, CacheFormat, CacheMessageStrategy, CachePolicy, CachePolicyObject,
    CacheSupport, CacheTargets,
};
use crate::profile::RetryPolicy;

#[test]
fn decodes_string_answer() {
    let value = json!({ "choices": [{ "message": { "content": "hi" } }] });
    assert_eq!(decode_message(&value).unwrap().content, "hi");
}

#[test]
fn decodes_reasoning_answer() {
    let value = json!({ "choices": [{ "message": { "reasoning": "check config first", "content": "answer" } }] });
    let message = decode_message(&value).unwrap();
    assert_eq!(message.reasoning, "check config first");
    assert_eq!(message.content, "answer");
}

#[test]
fn decodes_finish_reason() {
    let value = json!({ "choices": [{ "message": { "content": "hi" }, "finish_reason": "stop" }] });
    assert_eq!(decode_message(&value).unwrap().finish_reason.as_deref(), Some("stop"));
}

#[test]
fn decodes_content_parts_array() {
    let value = json!({
        "choices": [{ "message": { "content": [{ "type": "text", "text": "one" }, { "type": "text", "text": "two" }] } }]
    });
    assert_eq!(decode_message(&value).unwrap().content, "onetwo");
}

#[test]
fn missing_assistant_message_decodes_to_none() {
    assert_eq!(decode_message(&json!({ "choices": [] })), None);
}

#[test]
fn decodes_tool_call_arguments() {
    let value = json!({
        "choices": [{ "message": { "tool_calls": [{
            "id": "call_1", "type": "function",
            "function": { "name": "tool_read_file", "arguments": "{\"path\":\".ee.toml\"}" }
        }] } }]
    });
    let message = decode_message(&value).unwrap();
    assert_eq!(message.tool_calls[0].arguments["path"], ".ee.toml");
}

#[test]
fn malformed_tool_call_arguments_carry_a_parse_error() {
    let value = json!({
        "choices": [{ "message": { "tool_calls": [{
            "id": "call_1", "type": "function",
            "function": { "name": "tool_read_file", "arguments": "{not json" }
        }] } }]
    });
    let call = &decode_message(&value).unwrap().tool_calls[0];
    assert_eq!(call.arguments, Value::Null);
    let error = call.argument_error.as_deref().expect("parse failure recorded");
    assert!(error.contains("not valid JSON"), "{error}");
}

#[test]
fn extracts_usage_fields() {
    let value = json!({
        "choices": [{ "message": { "content": "hi" } }],
        "usage": {
            "prompt_tokens": 6120,
            "completion_tokens": 2311,
            "total_tokens": 8431,
        }
    });
    let usage = decode_message(&value).unwrap().usage.expect("usage parsed");
    assert_eq!(usage.input_tokens, Some(6120));
    assert_eq!(usage.output_tokens, Some(2311));
    assert_eq!(usage.total_tokens, Some(8431));
}

#[test]
fn provider_reported_cost_is_decoded_and_absent_cost_stays_unknown() {
    let openai = json!({
        "choices": [{ "message": { "content": "hi" } }],
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "cost": 0.001_234,
        }
    });
    let usage = decode_message(&openai).unwrap().usage.expect("usage parsed");
    assert_eq!(usage.cost_micros, Some(1_234));

    let responses = json!({ "input_tokens": 100, "output_tokens": 20 });
    let usage = usage_from_tokens(&responses).expect("usage parsed");
    assert_eq!(usage.cost_micros, None, "a missing cost is never zero-filled");
}

#[test]
fn cache_usage_is_parsed_from_both_provider_shapes() {
    let anthropic = json!({
        "usage": {
            "input_tokens": 100,
            "output_tokens": 20,
            "cache_read_input_tokens": 80,
            "cache_creation_input_tokens": 10,
        }
    });
    let usage = usage_from_tokens(&anthropic["usage"]).expect("usage parsed");
    assert_eq!(usage.cached_input_tokens, Some(80));
    assert_eq!(usage.cache_creation_input_tokens, Some(10));

    let openai = json!({
        "choices": [{ "message": { "content": "hi" } }],
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "prompt_tokens_details": { "cached_tokens": 60 },
        }
    });
    let usage = decode_message(&openai).unwrap().usage.expect("usage parsed");
    assert_eq!(usage.cached_input_tokens, Some(60));
    assert_eq!(usage.cache_creation_input_tokens, None);
}

#[test]
fn reasoning_tokens_are_parsed_from_every_dialect_shape() {
    let chat = json!({
        "choices": [{ "message": { "content": "hi" } }],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 20,
            "completion_tokens_details": { "reasoning_tokens": 7 },
        }
    });
    let usage = decode_message(&chat).unwrap().usage.expect("usage parsed");
    assert_eq!(usage.reasoning_tokens, Some(7));

    let responses = json!({
        "input_tokens": 10,
        "output_tokens": 20,
        "output_tokens_details": { "reasoning_tokens": 9 },
    });
    let usage = usage_from_tokens(&responses).expect("usage parsed");
    assert_eq!(usage.reasoning_tokens, Some(9));

    let anthropic = json!({
        "input_tokens": 10,
        "output_tokens": 20,
        "output_tokens_details": { "thinking_tokens": 5 },
    });
    let usage = usage_from_tokens(&anthropic).expect("usage parsed");
    assert_eq!(usage.reasoning_tokens, Some(5));

    let absent = json!({ "input_tokens": 10, "output_tokens": 20 });
    let usage = usage_from_tokens(&absent).expect("usage parsed");
    assert_eq!(usage.reasoning_tokens, None, "never zero-filled");
}

#[test]
fn context_tokens_use_the_anthropic_breakdown_but_not_openai_subset_counts() {
    let anthropic = json!({
        "input_tokens": 100,
        "output_tokens": 20,
        "cache_read_input_tokens": 80,
        "cache_creation_input_tokens": 10,
    });
    let usage = usage_from_tokens(&anthropic).expect("usage parsed");
    assert_eq!(usage.context_tokens, Some(190), "input + cache read + cache write");

    let openai = json!({
        "input_tokens": 100,
        "output_tokens": 20,
        "input_tokens_details": { "cached_tokens": 60 },
    });
    let usage = usage_from_tokens(&openai).expect("usage parsed");
    assert_eq!(usage.context_tokens, None, "OpenAI cached tokens are a subset of input");
}

#[test]
fn reported_zero_cost_stays_zero() {
    let body = json!({
        "choices": [{ "message": { "content": "hi" } }],
        "usage": { "prompt_tokens": 10, "completion_tokens": 2, "cost": 0 }
    });
    let usage = decode_message(&body).unwrap().usage.expect("usage parsed");
    assert_eq!(usage.cost_micros, Some(0), "a free request is authoritative");
}

#[test]
fn cache_breakpoints_wrap_only_marked_messages_without_rewriting_text() {
    let transcript = vec![
        ModelMessage::text(ModelRole::User, "stable instructions").with_cache_breakpoint(),
        ModelMessage::text(ModelRole::User, "hello"),
    ];

    let plain = messages_from_transcript("system", &transcript);
    assert_eq!(plain[0]["content"], "system");
    assert_eq!(plain[1]["content"], "stable instructions");

    let cached = messages_from_transcript_with_cache("system", &transcript, true);
    assert_eq!(cached[0]["content"][0]["text"], "system");
    assert_eq!(cached[0]["content"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(cached[1]["content"][0]["text"], "stable instructions");
    assert_eq!(cached[1]["content"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(cached[2]["content"], "hello", "unmarked messages stay plain text");
}

#[test]
fn missing_usage_stays_unknown_not_zero() {
    let value = json!({ "choices": [{ "message": { "content": "hi" } }] });
    assert_eq!(decode_message(&value).unwrap().usage, None);
}

#[test]
fn partial_usage_keeps_only_known_fields() {
    let value = json!({
        "choices": [{ "message": { "content": "hi" } }],
        "usage": { "total_tokens": 100 }
    });
    let usage = decode_message(&value).unwrap().usage.expect("usage parsed");
    assert_eq!(usage.input_tokens, None);
    assert_eq!(usage.output_tokens, None);
    assert_eq!(usage.total_tokens, Some(100));
}

#[test]
fn request_body_carries_model_messages_tools_and_extensions() {
    let messages = vec![json!({ "role": "user", "content": "hi" })];
    let tools = tools_from_definitions(&[ToolDefinition::new("read_file", "reads")]);
    let extensions = json!({ "reasoning": { "effort": "medium" } });

    let body = request_body(
        "m",
        &messages,
        &tools,
        RequestBodyOptions {
            stream: true,
            extensions: Some(&extensions),
            ..RequestBodyOptions::default()
        },
    );

    assert_eq!(body["model"], "m");
    assert_eq!(body["stream"], true);
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["tools"][0]["function"]["name"], "read_file");
    assert_eq!(body["messages"][0]["content"], "hi");
    assert_eq!(body["reasoning"]["effort"], "medium");
}

#[test]
fn request_body_requests_stream_usage_and_output_bound() {
    let body = request_body(
        "m",
        &[],
        &[],
        RequestBodyOptions {
            stream: true,
            stream_usage: true,
            max_tokens: Some(1_234),
            max_tokens_field: crate::profile::MaxTokensField::MaxCompletionTokens,
            tool_control: None,
            generation: None,
            extensions: None,
        },
    );

    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(body["max_completion_tokens"], 1_234);
    assert!(body.get("max_tokens").is_none());
}

#[test]
fn request_body_omits_stream_usage_when_not_requested_or_buffered() {
    let streaming = request_body(
        "m",
        &[],
        &[],
        RequestBodyOptions { stream: true, ..RequestBodyOptions::default() },
    );
    assert!(streaming.get("stream_options").is_none());

    let buffered = request_body(
        "m",
        &[],
        &[],
        RequestBodyOptions { stream: false, stream_usage: true, ..RequestBodyOptions::default() },
    );
    assert!(buffered.get("stream_options").is_none());
}

#[test]
fn request_body_omits_tools_when_no_definitions_exist() {
    let body = request_body("m", &[], &[], RequestBodyOptions::default());
    assert_eq!(body["stream"], false);
    assert!(body.get("tools").is_none());
}

#[test]
fn transcript_converts_to_chat_messages() {
    let transcript = vec![
        ModelMessage::text(ModelRole::System, "Memory facts:\ncwd: /work"),
        ModelMessage::text(ModelRole::User, "hello"),
        ModelMessage::text(ModelRole::Assistant, "hi there"),
        ModelMessage::tool_result("call_1", ToolResult::success("file contents")),
    ];

    let messages = messages_from_transcript("system", &transcript);

    assert_eq!(messages.len(), 5, "system prompt plus four transcript messages");
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "system");
    assert_eq!(messages[1]["role"], "system");
    assert_eq!(messages[1]["content"], "Memory facts:\ncwd: /work");
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[3]["role"], "assistant");
    assert_eq!(messages[4]["role"], "tool");
    assert_eq!(messages[4]["tool_call_id"], "call_1");
    assert_eq!(messages[4]["content"], "file contents");
}

#[test]
fn subagent_summaries_map_to_user_content() {
    let transcript = vec![ModelMessage::text(ModelRole::Subagent, "summary")];
    let messages = messages_from_transcript("system", &transcript);
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "summary");
}

#[test]
fn file_and_terminal_references_render_as_text_placeholders() {
    let transcript = vec![ModelMessage::new(ModelRole::User).with_content(vec![
        ModelContent::FileReference { path: "src/main.rs".into() },
        ModelContent::TerminalReference { terminal_id: "t1".into() },
    ])];

    let messages = messages_from_transcript("", &transcript);
    assert_eq!(messages[1]["content"], "[file:src/main.rs]\n[terminal:t1]");
}

#[test]
fn image_audio_and_binary_resources_become_multimodal_parts() {
    let transcript = vec![ModelMessage::new(ModelRole::User).with_content(vec![
        ModelContent::Text("look".into()),
        ModelContent::Image {
            data: "aW1n".into(),
            mime_type: "image/png".into(),
            uri: Some("file:///shot.png".into()),
        },
        ModelContent::Audio { data: "YXVkaW8=".into(), mime_type: "audio/wav".into() },
        ModelContent::EmbeddedResource {
            uri: "file:///scan.png".into(),
            mime_type: Some("image/png".into()),
            text: None,
            blob: Some("YmxvYg==".into()),
        },
    ])];

    let messages = messages_from_transcript("", &transcript);
    let parts = messages[1]["content"].as_array().expect("content parts array");
    assert_eq!(parts[0], json!({ "type": "text", "text": "look" }));
    assert_eq!(
        parts[1],
        json!({
            "type": "image_url",
            "image_url": { "url": "data:image/png;base64,aW1n" },
        })
    );
    assert_eq!(
        parts[2],
        json!({
            "type": "input_audio",
            "input_audio": { "data": "YXVkaW8=", "format": "wav" },
        })
    );
    assert_eq!(
        parts[3],
        json!({
            "type": "image_url",
            "image_url": { "url": "data:image/png;base64,YmxvYg==" },
        })
    );
}

#[test]
fn text_only_content_keeps_plain_string_messages() {
    let transcript = vec![ModelMessage::new(ModelRole::User).with_content(vec![
        ModelContent::Text("hello".into()),
        ModelContent::EmbeddedResource {
            uri: "file:///notes.md".into(),
            mime_type: None,
            text: Some("body".into()),
            blob: None,
        },
    ])];

    let messages = messages_from_transcript("", &transcript);
    assert_eq!(messages[1]["content"], "hello\n[resource:file:///notes.md]\nbody");
}

#[test]
fn definitions_convert_to_function_schema() {
    let definitions = vec![ToolDefinition::new("read_file", "reads a file")
        .side_effect_class(SideEffectClass::Read)
        .input_schema(json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }))];

    let tools = tools_from_definitions(&definitions);

    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["function"]["name"], "read_file");
    assert_eq!(tools[0]["function"]["description"], "reads a file");
    assert_eq!(tools[0]["function"]["parameters"]["required"][0], "path");
}

#[test]
fn tool_call_converts_to_normalized_tool_intent() {
    let value = json!({
        "choices": [{
            "message": {
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": { "name": "tool_read_file", "arguments": "{\"path\":\"/tmp/x\"}" }
                }]
            },
            "finish_reason": "tool_calls"
        }]
    });
    let response = response_from_turn(decode_message(&value).expect("decodes"));

    assert_eq!(response.tool_intents.len(), 1);
    let intent = &response.tool_intents[0];
    assert_eq!(intent.tool_call_id, "call_1");
    assert_eq!(intent.name, "read_file", "historical alias maps to the builtin");
    assert_eq!(intent.arguments["path"], "/tmp/x");
    assert!(!response.completed, "tool calls mean the turn continues");
    assert!(response.text.is_empty());
}

#[test]
fn reasoning_converts_to_normalized_reasoning() {
    let value = json!({
        "choices": [{
            "message": { "reasoning": "think first", "content": "answer" },
            "finish_reason": "stop"
        }]
    });
    let response = response_from_turn(decode_message(&value).expect("decodes"));

    assert_eq!(response.reasoning.as_deref(), Some("think first"));
    assert_eq!(response.text, "answer");
    assert!(response.completed);
}

#[test]
fn usage_converts_to_normalized_usage() {
    let response = response_from_turn(
        decode_message(&json!({
            "choices": [{ "message": { "content": "answer" }, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 6_120, "completion_tokens": 2_311 }
        }))
        .expect("decodes"),
    );

    assert_eq!(response.usage.input_tokens, Some(6_120));
    assert_eq!(response.usage.output_tokens, Some(2_311));
}

#[test]
fn stop_reason_maps_to_completion_signal() {
    let stopped = response_from_turn(
        decode_message(&json!({
            "choices": [{ "message": { "content": "done" }, "finish_reason": "stop" }]
        }))
        .expect("decodes"),
    );
    assert!(stopped.completed);
    assert!(stopped.tool_intents.is_empty());

    // Missing finish reason is treated as a completed stop (older APIs).
    let missing = response_from_turn(
        decode_message(&json!({ "choices": [{ "message": { "content": "done" } }] }))
            .expect("decodes"),
    );
    assert!(missing.completed);

    // Length-limited responses are not a completion signal.
    let truncated = response_from_turn(
        decode_message(&json!({
            "choices": [{ "message": { "content": "half" }, "finish_reason": "length" }]
        }))
        .expect("decodes"),
    );
    assert!(!truncated.completed);
}

#[test]
fn accumulator_keeps_latest_usage_chunk() {
    let mut accumulator = StreamAccumulator::new("Test");
    accumulator
        .apply(&json!({ "choices": [{ "delta": { "content": "hi" } }], "usage": {
            "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15
        } }))
        .unwrap();
    accumulator
        .apply(&json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }], "usage": {
            "prompt_tokens": 20, "completion_tokens": 9, "total_tokens": 29
        } }))
        .unwrap();
    let message = accumulator.finish().unwrap();
    let usage = message.usage.expect("usage parsed");
    assert_eq!(usage.input_tokens, Some(20), "later cumulative usage wins");
    assert_eq!(usage.output_tokens, Some(9));
    assert_eq!(usage.total_tokens, Some(29));
}

#[test]
fn decoder_handles_fragmented_utf8_and_multiple_events() {
    let mut decoder = SseDecoder::new("Test");
    assert!(decoder.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"").unwrap().is_empty());
    assert!(decoder.push("\u{00e9}".as_bytes()).unwrap().is_empty());
    let events =
        decoder.push(b"\"}}]}\n\ndata: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n").unwrap();
    assert_eq!(events.len(), 2);
    assert!(events[0].contains('é'));
}

#[test]
fn decoder_ignores_comments_and_accepts_crlf_and_multiline_data() {
    let mut decoder = SseDecoder::new("Test");
    let events = decoder.push(b": PROCESSING\r\n\r\ndata: first\r\ndata: second\r\n\r\n").unwrap();
    assert_eq!(events, vec![String::from("first\nsecond")]);
}

#[test]
fn decoder_finishes_trailing_event_without_terminator() {
    let mut decoder = SseDecoder::new("Test");
    assert!(decoder.push(b"data: {\"a\":1}").unwrap().is_empty());
    assert_eq!(decoder.finish().unwrap(), vec![String::from("{\"a\":1}")]);
}

#[test]
fn accumulator_emits_deltas_and_reassembles_tool_calls() {
    let mut accumulator = StreamAccumulator::new("Test");
    let first = accumulator
        .apply(&json!({ "choices": [{ "delta": {
            "reasoning_content": "check ",
            "tool_calls": [{ "index": 0, "id": "call_1", "function": { "name": "tool_read_file", "arguments": "{\"path\":\"" } }]
        } }] }))
        .unwrap();
    let second = accumulator
        .apply(&json!({ "choices": [{ "delta": {
            "content": "working",
            "tool_calls": [{ "index": 0, "function": { "arguments": "Cargo.toml\"}" } }]
        }, "finish_reason": "tool_calls" }] }))
        .unwrap();
    assert_eq!(first[0], StreamDelta::Reasoning(String::from("check ")));
    assert!(matches!(first[1], StreamDelta::ToolInputStart { .. }));
    assert!(matches!(first[2], StreamDelta::ToolInputDelta { .. }));
    assert_eq!(second[0], StreamDelta::Text(String::from("working")));
    assert!(
        second
            .iter()
            .any(|delta| matches!(delta, StreamDelta::ToolInputEnd { id } if id == "call_1"))
    );
    let message = accumulator.finish().unwrap();
    assert_eq!(message.content, "working");
    assert_eq!(message.reasoning, "check ");
    assert_eq!(message.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(message.tool_calls[0].arguments["path"], "Cargo.toml");
    assert_eq!(message.raw["tool_calls"][0]["function"]["arguments"], "{\"path\":\"Cargo.toml\"}");
}

#[test]
fn accumulator_recovers_malformed_tool_arguments_as_invalid_input() {
    let mut accumulator = StreamAccumulator::new("Test");
    accumulator
        .apply(&json!({ "choices": [{ "delta": {
            "tool_calls": [{ "index": 0, "id": "call_1", "function": {
                "name": "tool_read_file", "arguments": "{\"path\":\"Cargo"
            } }]
        }, "finish_reason": "tool_calls" }] }))
        .unwrap();

    let message = accumulator.finish().unwrap();
    assert_eq!(message.tool_calls.len(), 1);
    assert_eq!(message.tool_calls[0].id, "call_1");
    assert_eq!(message.tool_calls[0].name, "tool_read_file");
    assert_eq!(message.tool_calls[0].arguments, Value::Null);
    assert_eq!(message.raw["tool_calls"][0]["function"]["arguments"], "null");
}

#[test]
fn accumulator_rejects_incomplete_tool_calls() {
    let mut accumulator = StreamAccumulator::new("Test");
    accumulator
        .apply(&json!({ "choices": [{ "delta": { "tool_calls": [{ "index": 0 }] } }] }))
        .unwrap();
    assert!(accumulator.finish().is_err());
}

#[test]
fn accumulator_rejects_stream_error_events() {
    let mut accumulator = StreamAccumulator::new("Test");
    let error =
        accumulator.apply(&json!({ "error": { "message": "upstream failed" } })).unwrap_err();
    assert!(error.to_string().contains("upstream failed"), "{error}");
}

#[test]
fn http_errors_extract_message_with_the_provider_label() {
    let value = json!({ "error": { "message": "rate limited" } });
    assert_eq!(http_error_message("OpenRouter", 429, &value), "OpenRouter HTTP 429: rate limited");
    assert_eq!(http_error_message("OpenCode", 500, &json!("boom")), "OpenCode HTTP 500: \"boom\"");
}

#[test]
fn classifies_http_errors_into_retry_decisions() {
    enum ExpectedError {
        RateLimited,
        Transient,
        BackendFailure,
        InvalidRequest,
    }

    let hint = Some(Duration::from_secs(3));
    let cases = [
        (429, ExpectedError::RateLimited),
        (408, ExpectedError::Transient),
        (500, ExpectedError::Transient),
        (502, ExpectedError::Transient),
        (503, ExpectedError::Transient),
        (504, ExpectedError::Transient),
        (599, ExpectedError::Transient),
        (401, ExpectedError::BackendFailure),
        (403, ExpectedError::BackendFailure),
        (400, ExpectedError::InvalidRequest),
        (404, ExpectedError::InvalidRequest),
    ];
    for (status, expected) in cases {
        let error = classify_http_error(status, hint, format!("HTTP {status}"));
        assert!(
            matches!(
                (&expected, &error),
                (ExpectedError::RateLimited, ProviderError::RateLimited { .. })
                    | (ExpectedError::Transient, ProviderError::Transient { .. })
                    | (ExpectedError::BackendFailure, ProviderError::BackendFailure(_))
                    | (ExpectedError::InvalidRequest, ProviderError::InvalidRequest(_))
            ),
            "status {status} classified wrong: {error:?}"
        );
    }
    match classify_http_error(429, hint, "slow".into()) {
        ProviderError::RateLimited { retry_after, .. } => assert_eq!(retry_after, hint),
        other => panic!("expected rate limited, got {other:?}"),
    }
    match classify_http_error(503, hint, "down".into()) {
        ProviderError::Transient { retry_after, .. } => assert_eq!(retry_after, hint),
        other => panic!("expected transient, got {other:?}"),
    }
    assert!(is_retryable(&ProviderError::RateLimited {
        retry_after: None,
        rate_limit: None,
        retryable: true,
        detail: "x".into(),
    }));
    assert!(is_retryable(&ProviderError::Transient {
        retry_after: None,
        retryable: true,
        detail: "x".into(),
    }));
    assert!(!is_retryable(&ProviderError::BackendFailure("x".into())));
    assert!(!is_retryable(&ProviderError::InvalidRequest("x".into())));
    assert!(!is_retryable(&ProviderError::Cancellation));
}

#[test]
fn parses_retry_after_seconds_and_ignores_dates() {
    let mut headers = HeaderMap::new();
    assert_eq!(parse_retry_after(&headers), None);
    headers.insert("retry-after", "120".parse().expect("header"));
    assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(120)));
    headers.insert("retry-after", "Tue, 15 Nov 1994 08:12:31 GMT".parse().expect("header"));
    assert_eq!(parse_retry_after(&headers), None, "HTTP dates are rejected, not mis-parsed");
    headers.insert("retry-after", "abc".parse().expect("header"));
    assert_eq!(parse_retry_after(&headers), None);
}

#[test]
fn retry_delay_prefers_capped_server_hint() {
    let policy = RetryPolicy::new(2, Duration::from_millis(100), Duration::from_secs(10));
    let delay = retry_delay(&policy, 0, Some(Duration::from_secs(2))).as_millis() as u64;
    assert!((2_000..=2_400).contains(&delay), "hint widened the gap: {delay}");
    // The server-hint cap is independent of the backoff cap, so a long hint is
    // honoured up to 15 minutes rather than truncated to `max_delay`.
    let delay = retry_delay(&policy, 0, Some(Duration::from_secs(3_600))).as_millis() as u64;
    let cap = 15 * 60 * 1_000;
    assert!((cap..=cap + cap / 5).contains(&delay), "hint was not capped: {delay}");
}

#[test]
fn retry_delay_backs_off_exponentially_with_bounded_jitter() {
    let policy = RetryPolicy::new(4, Duration::from_millis(100), Duration::from_secs(10));
    for attempt in 0..5 {
        let delay = retry_delay(&policy, attempt, None).as_millis() as u64;
        let expected = 100u64 << attempt.min(10);
        let ceiling = expected + expected / 5 + 1;
        assert!(
            (expected..=ceiling).contains(&delay),
            "attempt {attempt}: {delay} outside [{expected}, {ceiling}]"
        );
    }
    let delay = retry_delay(&policy, 20, None).as_millis() as u64;
    assert!(delay <= 10_000 + 10_000 / 5 + 1, "{delay}");
}

// ── Gap 4: reasoning continuity and replay ──────────────────────────────

fn assistant_with_native(model: &str, state: serde_json::Value) -> ModelMessage {
    ModelMessage::text(ModelRole::Assistant, "answer")
        .with_native(crate::native::seal(state, model))
}

#[test]
fn reasoning_details_are_replayed_verbatim_with_index() {
    let details = json!([
        { "type": "reasoning.text", "text": "step", "index": 0, "signature": "sig" },
        { "type": "reasoning.summary", "summary": "sum", "index": 1 }
    ]);
    let transcript =
        vec![assistant_with_native("model-a", json!({ "reasoning_details": details }))];

    let messages = messages_from_transcript_replaying("sys", &transcript, false, "model-a", None);
    assert_eq!(messages[1]["reasoning_details"], details);
    assert_eq!(messages[1]["reasoning_details"][0]["index"], 0);

    let other = messages_from_transcript_replaying("sys", &transcript, false, "model-b", None);
    assert!(
        other[1].get("reasoning_details").is_none(),
        "provider state is model-scoped and must not replay to another model"
    );
}

#[test]
fn configured_reasoning_field_carries_the_reasoning_text() {
    let message = assistant_with_native(
        "model-a",
        json!({ "reasoning_details": [{ "type": "reasoning.summary", "summary": "thinking" }] }),
    )
    .with_reasoning_summary("thinking");

    let messages = messages_from_transcript_replaying(
        "",
        &[message],
        false,
        "model-a",
        Some("reasoning_content"),
    );
    assert_eq!(messages[1]["reasoning_content"], "thinking");
    assert!(messages[1]["reasoning_details"].is_array());
}

#[test]
fn streamed_reasoning_details_merge_into_one_replayable_entry() {
    let mut accumulator = StreamAccumulator::new("test");
    for text in ["a", "b"] {
        let chunk = json!({ "choices": [{ "delta": { "reasoning_details": [
            { "type": "reasoning.text", "text": text, "index": 0 }
        ] } }] });
        accumulator.apply(&chunk).expect("applies");
    }

    let turn = accumulator.finish().expect("finishes");
    let native = turn.native_reasoning.expect("native reasoning");
    assert_eq!(native["reasoning_details"][0]["text"], "ab");
    assert_eq!(native["reasoning_details"][0]["index"], 0);
}

#[test]
fn streamed_scalar_reasoning_is_replayed_under_its_own_field() {
    let mut accumulator = StreamAccumulator::new("test");
    for text in ["one", " two"] {
        let chunk = json!({ "choices": [{ "delta": { "reasoning_content": text } }] });
        accumulator.apply(&chunk).expect("applies");
    }

    let turn = accumulator.finish().expect("finishes");
    let native = turn.native_reasoning.expect("native reasoning");
    assert_eq!(native["reasoning_content"], "one two");
}

// ── Gap 5: error taxonomy, rate-limit metadata, retry budget ────────────

#[test]
fn classifies_overflow_payload_policy_and_quota() {
    let empty = HeaderMap::new();
    let overflow = json!({ "error": { "code": "context_length_exceeded", "message": "too long" } });
    assert!(matches!(
        classify_status_error("x", 400, &empty, &overflow),
        ProviderError::ContextOverflow(_)
    ));
    let large = json!({ "error": { "message": "nope" } });
    assert!(matches!(
        classify_status_error("x", 413, &empty, &large),
        ProviderError::PayloadTooLarge(_)
    ));
    let quota = json!({ "error": { "message": "payment required" } });
    assert!(matches!(
        classify_status_error("x", 402, &empty, &quota),
        ProviderError::QuotaExceeded { .. }
    ));
    let auth = json!({ "error": { "message": "no" } });
    assert!(matches!(
        classify_status_error("x", 401, &empty, &auth),
        ProviderError::BackendFailure(_)
    ));
    let policy = json!({ "error": { "message": "blocked by the content policy" } });
    assert!(matches!(
        classify_status_error("x", 400, &empty, &policy),
        ProviderError::ContentPolicy(_)
    ));
}

#[test]
fn a_throttle_is_never_a_context_overflow() {
    let body = json!({ "error": { "message": "too many tokens processed" } });
    let error = classify_status_error("x", 429, &HeaderMap::new(), &body);
    assert!(matches!(error, ProviderError::RateLimited { .. }), "{error:?}");
}

#[test]
fn x_should_retry_false_suppresses_the_retry() {
    let mut headers = HeaderMap::new();
    headers.insert("x-should-retry", "false".parse().expect("header"));
    let body = json!({ "error": { "message": "slow down" } });
    let error = classify_status_error("x", 429, &headers, &body);
    assert!(!is_retryable(&error), "{error:?}");
    assert!(matches!(error, ProviderError::RateLimited { retryable: false, .. }));
}

#[test]
fn rate_limit_headers_are_exposed() {
    let mut headers = HeaderMap::new();
    headers.insert("retry-after", "30".parse().expect("header"));
    headers.insert("x-ratelimit-limit", "100".parse().expect("header"));
    headers.insert("x-ratelimit-remaining", "0".parse().expect("header"));
    headers.insert("x-ratelimit-reset", "60".parse().expect("header"));
    let body = json!({ "error": { "message": "slow down" } });

    match classify_status_error("x", 429, &headers, &body) {
        ProviderError::RateLimited { retry_after, rate_limit: Some(info), .. } => {
            assert_eq!(retry_after, Some(Duration::from_secs(30)));
            assert_eq!(info.limit, Some(100));
            assert_eq!(info.remaining, Some(0));
            assert_eq!(info.reset, Some(Duration::from_secs(60)));
        }
        other => panic!("expected rate limit metadata, got {other:?}"),
    }
}

#[test]
fn ambiguous_transport_failures_are_not_retried() {
    use ee_agent_acp_server::Delivery;

    let ambiguous = ProviderError::Transport {
        delivery: Delivery::Ambiguous,
        code: None,
        detail: "connection dropped".into(),
    };
    assert!(!is_retryable(&ambiguous), "a sent request may already have run");

    let not_sent = ProviderError::Transport {
        delivery: Delivery::NotSent,
        code: Some("ENOTFOUND".into()),
        detail: "dns".into(),
    };
    assert!(is_retryable(&not_sent));
    assert!(is_unreachable(&not_sent));

    let refused = ProviderError::Transport {
        delivery: Delivery::NotSent,
        code: Some("ECONNREFUSED".into()),
        detail: "refused".into(),
    };
    assert!(!is_unreachable(&refused), "something answered, so it is the server's state");
}

#[test]
fn timeout_transport_failures_are_recognised() {
    let timeout = ProviderError::Transport {
        delivery: ee_agent_acp_server::Delivery::Ambiguous,
        code: Some("Timeout".into()),
        detail: "timed out".into(),
    };
    assert!(is_timeout(&timeout));
    assert!(!is_unreachable(&timeout));
}

#[test]
fn assistant_tool_calls_replay_before_their_observations() {
    let mut transcript = vec![
        ModelMessage::text(ModelRole::User, "read it"),
        ModelMessage::tool_call("call_1", "read_file", json!({ "path": "a" }))
            .with_native(crate::native::seal(
                json!({ "reasoning_details": [{ "type": "reasoning.text", "text": "plan", "index": 0 }] }),
                "model-a",
            )),
    ];
    transcript.push(ModelMessage::tool_result("call_1", ToolResult::success("body")));

    let messages = messages_from_transcript_replaying("sys", &transcript, false, "model-a", None);
    let assistant = &messages[2];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["content"], Value::Null, "a tool-only turn has null content");
    assert_eq!(assistant["tool_calls"][0]["id"], "call_1");
    assert_eq!(assistant["tool_calls"][0]["function"]["name"], "read_file");
    assert_eq!(
        assistant["tool_calls"][0]["function"]["arguments"], "{\"path\":\"a\"}",
        "arguments replay as a JSON string"
    );
    assert_eq!(assistant["reasoning_details"][0]["index"], 0);
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[3]["tool_call_id"], "call_1");
}

// ── Cache policy ─────────────────────────────────────────────────────

fn system(text: &str) -> ModelMessage {
    ModelMessage::text(ModelRole::System, text)
}

fn user(text: &str) -> ModelMessage {
    ModelMessage::text(ModelRole::User, text)
}

fn definitions(count: usize) -> Vec<ToolDefinition> {
    (0..count).map(|index| ToolDefinition::new(format!("tool_{index}"), "t")).collect()
}

fn chat_parts(
    transcript: &[ModelMessage],
    tools: &[ToolDefinition],
    cache: &CacheDirective,
) -> (Vec<Value>, Vec<Value>) {
    chat_request_parts(&ChatRequestParts {
        system_prompt: "system",
        transcript,
        definitions: tools,
        cache,
        model: "model-a",
        reasoning_field: None,
        tools_strict: false,
        freeform: &[],
        prompt_tokens: None,
    })
}

fn markers(messages: &[Value], tools: &[Value]) -> usize {
    let mut count = 0;
    for message in messages {
        if let Some(parts) = message["content"].as_array() {
            count += parts.iter().filter(|part| part.get("cache_control").is_some()).count();
        }
    }
    count + tools.iter().filter(|tool| tool.get("cache_control").is_some()).count()
}

#[test]
fn automatic_policy_places_exactly_four_breakpoints() {
    let transcript = vec![system("project rules"), user("current request")];
    let tools = definitions(10);
    let (messages, tools) = chat_parts(&transcript, &tools, &CacheDirective::anthropic());

    assert_eq!(markers(&messages, &tools), 4);
    assert_eq!(tools[9]["cache_control"]["type"], "ephemeral");
    assert!(tools[8].get("cache_control").is_none());
    assert_eq!(messages[0]["content"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(messages[1]["content"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(messages[2]["content"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(messages[2]["content"][0]["text"], "current request");
}

#[test]
fn alibaba_routes_skip_tool_breakpoints() {
    let transcript = vec![user("current request")];
    let tools = definitions(3);
    let cache = CacheDirective {
        policy: CachePolicy::Explicit(CachePolicyObject {
            tools: true,
            system: true,
            messages: Some(CacheMessageStrategy::Tail(1)),
            ttl: None,
        }),
        support: CacheSupport {
            targets: CacheTargets::SYSTEM_AND_MESSAGES,
            ..CacheSupport::ANTHROPIC
        },
        format: CacheFormat::Alibaba,
    };
    let (messages, tools) = chat_parts(&transcript, &tools, &cache);

    assert!(tools.iter().all(|tool| tool.get("cache_control").is_none()));
    assert_eq!(messages[0]["content"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(messages[1]["content"][0]["cache_control"]["type"], "ephemeral");
}

#[test]
fn a_policy_without_breakpoints_places_none() {
    let transcript = vec![user("current request")];
    let (messages, tools) = chat_parts(&transcript, &definitions(2), &CacheDirective::disabled());
    assert_eq!(markers(&messages, &tools), 0);
}

#[test]
fn configured_ttl_lands_on_every_marker() {
    let cache = CacheDirective {
        policy: CachePolicy::Explicit(CachePolicyObject::auto().with_ttl(Some(String::from("1h")))),
        ..CacheDirective::anthropic()
    };
    let (messages, tools) = chat_parts(&[user("hi")], &definitions(1), &cache);

    assert_eq!(tools[0]["cache_control"]["ttl"], "1h");
    assert_eq!(messages[1]["content"][0]["cache_control"]["ttl"], "1h");
    assert_eq!(messages[0]["content"][0]["cache_control"]["ttl"], "1h");
}

// ── Tool-call control and structured tools ───────────────────────────

fn body_with(control: &ToolCallControl, tools: &[Value]) -> Value {
    request_body(
        "m",
        &[],
        tools,
        RequestBodyOptions {
            stream: false,
            stream_usage: false,
            max_tokens: None,
            max_tokens_field: crate::profile::MaxTokensField::MaxTokens,
            tool_control: Some(control),
            generation: None,
            extensions: None,
        },
    )
}

#[test]
fn tool_choice_and_parallel_are_only_sent_with_tools() {
    let tools = tools_from_definitions(&definitions(1));
    let body = body_with(&ToolCallControl::auto(), &tools);
    assert_eq!(body["tool_choice"], "auto");
    assert!(body.get("parallel_tool_calls").is_none(), "unset by default");

    let body = body_with(&ToolCallControl::auto().with_parallel(Some(false)), &tools);
    assert_eq!(body["parallel_tool_calls"], false);

    let body = body_with(&ToolCallControl::required(), &tools);
    assert_eq!(body["tool_choice"], "required");

    let body = body_with(&ToolCallControl::named("read_file"), &tools);
    assert_eq!(body["tool_choice"]["function"]["name"], "read_file");

    let empty = body_with(&ToolCallControl::named("read_file"), &[]);
    assert!(empty.get("tool_choice").is_none(), "never without tools");
    assert!(empty.get("parallel_tool_calls").is_none());
}

#[test]
fn strict_and_freeform_tools_follow_route_capabilities() {
    let strict = ToolDefinition::new("a", "a").strict();
    let grammar = ToolDefinition::new("b", "b")
        .format(ToolFormat::Lark { definition: String::from("start: /x/") });

    let unsupported = tools_from_definitions_with(&[strict.clone(), grammar.clone()], false, &[]);
    assert!(unsupported[0]["function"].get("strict").is_none());
    assert_eq!(unsupported[1]["type"], "function", "a grammar tool degrades to a function");

    let supported = tools_from_definitions_with(&[strict, grammar], true, &[String::from("lark")]);
    assert_eq!(supported[0]["function"]["strict"], true);
    assert_eq!(supported[1]["type"], "custom");
    assert_eq!(supported[1]["custom"]["format"]["grammar"]["syntax"], "lark");
}

// ── Sampling knobs and nested extension merge ─────────────────────────

fn generated(generation: &ee_agent_orchestrator::GenerationOptions) -> Value {
    request_body(
        "m",
        &[],
        &[],
        RequestBodyOptions {
            stream: false,
            stream_usage: false,
            max_tokens: None,
            max_tokens_field: crate::profile::MaxTokensField::MaxTokens,
            tool_control: None,
            generation: Some(generation),
            extensions: None,
        },
    )
}

#[test]
fn no_sampling_key_appears_when_nothing_is_configured() {
    let body = generated(&ee_agent_orchestrator::GenerationOptions::none());
    for key in
        ["temperature", "top_p", "top_k", "frequency_penalty", "presence_penalty", "seed", "stop"]
    {
        assert!(body.get(key).is_none(), "{key} must stay absent");
    }
}

#[test]
fn configured_sampling_knobs_round_trip_with_native_types() {
    let generation = ee_agent_orchestrator::GenerationOptions {
        temperature: Some(0.2),
        top_p: Some(0.9),
        top_k: Some(12),
        frequency_penalty: Some(0.5),
        presence_penalty: Some(-0.5),
        seed: Some(7),
        stop: Some(vec![String::from("END")]),
        ..ee_agent_orchestrator::GenerationOptions::none()
    };
    let body = generated(&generation);

    assert_eq!(body["temperature"], 0.2);
    assert_eq!(body["top_p"], 0.9);
    assert_eq!(body["top_k"], 12);
    assert_eq!(body["frequency_penalty"], 0.5);
    assert_eq!(body["presence_penalty"], -0.5);
    assert_eq!(body["seed"], 7);
    assert!(body["seed"].is_i64(), "seed stays an integer");
    assert_eq!(body["stop"], json!(["END"]));
}

#[test]
fn extensions_deep_merge_over_the_native_body() {
    let extensions = json!({ "reasoning": { "effort": "high" }, "metadata": { "a": "b" } });
    let body = request_body(
        "m",
        &[],
        &[],
        RequestBodyOptions { extensions: Some(&extensions), ..RequestBodyOptions::default() },
    );
    assert_eq!(body["model"], "m", "the native body survives the merge");
    assert_eq!(body["reasoning"]["effort"], "high");
    assert_eq!(body["metadata"]["a"], "b");
}

#[test]
fn nested_extension_keeps_a_sibling_key_written_by_the_provider() {
    let mut extensions = json!({ "reasoning": { "effort": "high" } });
    crate::deep_merge(&mut extensions, &json!({ "reasoning": { "summary": "auto" } }));
    assert_eq!(extensions, json!({ "reasoning": { "effort": "high", "summary": "auto" } }));
}

#[test]
fn streamed_tool_arguments_emit_start_delta_and_end() {
    let mut accumulator = StreamAccumulator::new("test");
    let first = accumulator
        .apply(&json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "function": { "name": "echo", "arguments": "{\"text\":" }
                    }]
                }
            }]
        }))
        .expect("first chunk decodes");
    assert!(matches!(first[0], StreamDelta::ToolInputStart { .. }));
    let StreamDelta::ToolInputDelta { text, parsed, .. } = &first[1] else {
        panic!("expected a tool input delta, got {first:?}");
    };
    assert_eq!(text, "{\"text\":");
    assert_eq!(parsed.as_ref(), Some(&json!({ "text": null })));

    let second = accumulator
        .apply(&json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": { "arguments": "\"hi\"}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }))
        .expect("second chunk decodes");
    assert!(
        second
            .iter()
            .any(|delta| matches!(delta, StreamDelta::ToolInputEnd { id } if id == "call_1"))
    );
    let turn = accumulator.finish().expect("finishes");
    assert_eq!(turn.tool_calls.len(), 1);
    assert_eq!(turn.tool_calls[0].arguments, json!({ "text": "hi" }));
    assert!(turn.tool_calls[0].argument_error.is_none());
}

#[test]
fn malformed_tool_arguments_carry_a_bounded_error() {
    let mut accumulator = StreamAccumulator::new("test");
    accumulator
        .apply(&json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "function": { "name": "echo", "arguments": "{oops" }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }))
        .expect("chunk decodes");
    let turn = accumulator.finish().expect("finishes with the call identity");
    assert_eq!(turn.tool_calls[0].arguments, serde_json::Value::Null);
    let error = turn.tool_calls[0].argument_error.as_deref().expect("parse failure recorded");
    assert!(error.contains("not valid JSON"), "{error}");
    assert!(error.contains("{oops"), "{error}");
}

#[test]
fn usage_chunk_emits_a_usage_delta() {
    let mut accumulator = StreamAccumulator::new("test");
    let deltas = accumulator
        .apply(&json!({
            "choices": [{ "delta": {} }],
            "usage": { "prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10 }
        }))
        .expect("usage chunk decodes");
    assert_eq!(
        deltas,
        vec![StreamDelta::Usage(Usage {
            input_tokens: Some(7),
            output_tokens: Some(3),
            total_tokens: Some(10),
            ..Default::default()
        })]
    );
}
