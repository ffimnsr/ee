//! Reasoning-effort tests: strict parsing, exact per-dialect fields, and the
//! deliberate no-op for the Messages dialect.

use serde_json::json;

use super::*;

#[test]
fn effort_parses_only_the_documented_values() {
    assert_eq!(ReasoningEffort::parse("low"), Ok(ReasoningEffort::Low));
    assert_eq!(ReasoningEffort::parse(" MEDIUM "), Ok(ReasoningEffort::Medium));
    assert_eq!(ReasoningEffort::parse("High"), Ok(ReasoningEffort::High));

    for rejected in ["", "none", "ultra", "3", "low high"] {
        let error = ReasoningEffort::parse(rejected).expect_err("unsupported value");
        assert!(error.contains("expected \"low\", \"medium\", or \"high\""), "{error}");
        assert!(!error.contains("sk-"), "{error}");
    }
}

#[test]
fn responses_dialect_sends_the_documented_effort_field() {
    let extensions = request_extensions(OpenCodeDialect::OpenAiResponses, ReasoningEffort::High)
        .expect("responses documents an effort field");

    assert_eq!(extensions, json!({ "reasoning": { "effort": "high" } }));
}

#[test]
fn chat_completions_dialect_sends_the_documented_effort_field() {
    let extensions =
        request_extensions(OpenCodeDialect::OpenAiChatCompletions, ReasoningEffort::Low)
            .expect("chat completions documents an effort field");

    assert_eq!(extensions, json!({ "reasoning_effort": "low" }));
}

#[test]
fn messages_dialect_sends_nothing_and_explains_why() {
    assert!(!supports_effort(OpenCodeDialect::AnthropicMessages));
    assert_eq!(
        request_extensions(OpenCodeDialect::AnthropicMessages, ReasoningEffort::Medium),
        None
    );
    let note = unsupported_note(OpenCodeDialect::AnthropicMessages);
    assert!(note.contains("anthropic_messages"), "{note}");
    assert!(note.contains("Requests stay unchanged"), "{note}");
}

#[test]
fn every_supported_dialect_has_a_mapping_and_no_dialect_is_silently_ignored() {
    for dialect in [
        OpenCodeDialect::OpenAiResponses,
        OpenCodeDialect::AnthropicMessages,
        OpenCodeDialect::OpenAiChatCompletions,
    ] {
        let mapped = request_extensions(dialect, ReasoningEffort::Medium);
        assert_eq!(
            mapped.is_some(),
            supports_effort(dialect),
            "{} mapping must match its support statement",
            dialect.as_str()
        );
    }
}

#[test]
fn effort_fields_never_touch_reserved_request_keys() {
    for dialect in [OpenCodeDialect::OpenAiResponses, OpenCodeDialect::OpenAiChatCompletions] {
        let extensions = request_extensions(dialect, ReasoningEffort::Medium).expect("mapping");
        let keys = extensions.as_object().expect("object").keys().collect::<Vec<_>>();

        assert_eq!(keys.len(), 1, "{dialect:?} adds exactly one field");
        for reserved in
            ["model", "input", "messages", "tools", "system", "max_tokens", "store", "stream"]
        {
            assert!(
                !extensions.to_string().contains(&format!("\"{reserved}\"")),
                "{dialect:?} must not shape `{reserved}`"
            );
        }
    }
}
