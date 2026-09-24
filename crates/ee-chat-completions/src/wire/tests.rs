use ee_agent_orchestrator::{SideEffectClass, ToolResult};
use serde_json::json;

use super::*;

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
fn malformed_tool_call_arguments_decode_to_empty_object() {
    let value = json!({
        "choices": [{ "message": { "tool_calls": [{
            "id": "call_1", "type": "function",
            "function": { "name": "tool_read_file", "arguments": "{not json" }
        }] } }]
    });
    assert_eq!(decode_message(&value).unwrap().tool_calls[0].arguments, json!({}));
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

    let body = request_body("m", &messages, &tools, true, Some(&extensions));

    assert_eq!(body["model"], "m");
    assert_eq!(body["stream"], true);
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["tools"][0]["function"]["name"], "read_file");
    assert_eq!(body["messages"][0]["content"], "hi");
    assert_eq!(body["reasoning"]["effort"], "medium");
}

#[test]
fn request_body_omits_tools_when_no_definitions_exist() {
    let body = request_body("m", &[], &[], false, None);
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
    assert_eq!(first, vec![StreamDelta::Reasoning(String::from("check "))]);
    assert_eq!(second, vec![StreamDelta::Text(String::from("working"))]);
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
    assert!(is_retryable(&ProviderError::RateLimited { retry_after: None, detail: "x".into() }));
    assert!(is_retryable(&ProviderError::Transient { retry_after: None, detail: "x".into() }));
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
    let policy = RetryPolicy {
        max_attempts: 2,
        base_delay: Duration::from_millis(100),
        max_delay: Duration::from_secs(10),
    };
    assert_eq!(retry_delay(&policy, 0, Some(Duration::from_secs(2))), Duration::from_secs(2));
    assert_eq!(retry_delay(&policy, 0, Some(Duration::from_secs(60))), Duration::from_secs(10));
}

#[test]
fn retry_delay_backs_off_exponentially_with_bounded_jitter() {
    let policy = RetryPolicy {
        max_attempts: 4,
        base_delay: Duration::from_millis(100),
        max_delay: Duration::from_secs(10),
    };
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
