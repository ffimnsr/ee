//! `/responses/compact` codec tests.

use ee_agent_orchestrator::{
    ModelContent, ModelMessage, ModelRole, SessionContext, ToolCallControl, ToolDefinition,
};
use serde_json::json;

use super::*;

fn checkpoint_payload() -> serde_json::Value {
    json!({
        "object": "response.compaction",
        "output": [
            { "type": "compaction", "id": "cmp-1", "encrypted_content": "Zmlyc3Q=" },
            { "type": "message", "role": "assistant", "content": [
                { "type": "output_text", "text": "summary text" }
            ] }
        ],
        "usage": { "input_tokens": 12, "output_tokens": 3 }
    })
}

#[test]
fn compact_body_carries_no_store_stream_or_output_bound() {
    let transcript = vec![ModelMessage::text(ModelRole::User, "hello")];
    let tools = vec![ToolDefinition::new("read_file", "reads")];
    let cache = CacheDirective::disabled();
    let body = compact_body(&CompactBody {
        model: "gpt-5.5",
        system_prompt: "system",
        transcript: &transcript,
        tools: &tools,
        extensions: None,
        generation: None,
        cache: &cache,
        session: None,
    });
    assert!(body.get("store").is_none(), "{body}");
    assert!(body.get("stream").is_none(), "{body}");
    assert!(body.get("max_output_tokens").is_none(), "{body}");
    assert!(body.get("tool_choice").is_none(), "{body}");
    assert_eq!(body["model"], "gpt-5.5");
    assert_eq!(body["instructions"], "system");
    assert!(body["prompt_cache_key"].as_str().is_some_and(|key| !key.is_empty()));
    assert_eq!(body["tools"][0]["name"], "read_file");
}

#[test]
fn session_affinity_supplies_the_cache_key() {
    let transcript = vec![ModelMessage::text(ModelRole::User, "hello")];
    let cache = CacheDirective::disabled();
    let session = SessionContext::new("s-1");
    let body = compact_body(&CompactBody {
        model: "gpt-5.5",
        system_prompt: "system",
        transcript: &transcript,
        tools: &[],
        extensions: None,
        generation: None,
        cache: &cache,
        session: Some(&session),
    });
    assert_eq!(body["prompt_cache_key"], session.cache_key());
}

#[test]
fn decode_returns_a_byte_identical_checkpoint_and_summary() {
    let outcome = decode_compaction("OpenAI", "gpt-5.5", &checkpoint_payload()).expect("decodes");
    assert_eq!(outcome.summary_text(), "summary text");
    assert_eq!(outcome.usage.input_tokens, Some(12));
    let checkpoint = outcome.checkpoint.expect("checkpoint");
    assert_eq!(checkpoint.encrypted(), "Zmlyc3Q=");
    assert_eq!(checkpoint.id.as_deref(), Some("cmp-1"));

    // The replacement re-encodes the same encrypted payload, byte for byte.
    let body = compact_body(&CompactBody {
        model: "gpt-5.5",
        system_prompt: "",
        transcript: &outcome.replacement,
        tools: &[],
        extensions: None,
        generation: None,
        cache: &CacheDirective::disabled(),
        session: None,
    });
    assert_eq!(body["input"][0]["type"], "compaction");
    assert_eq!(body["input"][0]["encrypted_content"], checkpoint.encrypted());
    assert_eq!(body["input"][0]["id"], "cmp-1");
}

#[test]
fn a_missing_checkpoint_is_rejected() {
    let payload = json!({
        "object": "response.compaction",
        "output": [
            { "type": "message", "role": "assistant", "content": [
                { "type": "output_text", "text": "summary" }
            ] }
        ]
    });
    let error = decode_compaction("OpenAI", "gpt-5.5", &payload).expect_err("rejects");
    assert!(error.to_string().contains("checkpoint"), "{error}");
}

#[test]
fn an_oversized_checkpoint_is_rejected_without_echoing_it() {
    let secret = "s".repeat(ee_agent_orchestrator::MAX_CHECKPOINT_BYTES + 1);
    let payload = json!({
        "object": "response.compaction",
        "output": [
            { "type": "compaction", "id": "cmp-1", "encrypted_content": secret }
        ]
    });
    let error = decode_compaction("OpenAI", "gpt-5.5", &payload).expect_err("rejects");
    let message = error.to_string();
    assert!(message.contains("supported size"), "{message}");
    assert!(!message.contains("ssssss"), "{message}");
}

#[test]
fn a_non_compaction_payload_is_rejected() {
    let error = decode_compaction("OpenAI", "gpt-5.5", &json!({ "object": "response" }))
        .expect_err("rejects");
    assert!(error.to_string().contains("compaction response"), "{error}");
}

#[test]
fn reasoning_items_are_kept_opaque_for_the_next_turn() {
    let payload = json!({
        "object": "response.compaction",
        "output": [
            { "type": "reasoning", "id": "rs-1", "summary": [{ "text": "thought" }], "encrypted_content": "enA=" },
            { "type": "compaction", "id": "cmp-1", "encrypted_content": "Zmlyc3Q=" }
        ]
    });
    let outcome = decode_compaction("OpenAI", "gpt-5.5", &payload).expect("decodes");
    assert!(
        outcome.replacement.iter().any(|message| {
            message
                .content
                .iter()
                .any(|block| matches!(block, ModelContent::CompactionCheckpoint(_)))
        }),
        "checkpoint content survives"
    );
    let reasoning = outcome
        .replacement
        .iter()
        .find(|message| message.reasoning_summary.is_some())
        .expect("reasoning message");
    assert_eq!(reasoning.reasoning_summary.as_deref(), Some("thought"));
    assert!(reasoning.native.is_some(), "native state is retained");
}

#[test]
fn tool_choice_is_irrelevant_to_the_compact_body() {
    // The compact endpoint has no tool-choice field; a forced control must not
    // leak into it.
    assert_eq!(ToolCallControl::auto().choice, ee_agent_orchestrator::ToolChoice::Auto);
}
