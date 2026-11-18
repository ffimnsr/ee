use ee_agent_acp_server::PromptContext;
use ee_agent_protocol::{
    AudioContent, BlobResourceContents, ContentBlock, EmbeddedResource, EmbeddedResourceResource,
    ImageContent, ResourceLink, SessionId, TextContent, TextResourceContents,
};
use serde_json::json;
use tokio::sync::watch;

use super::message::prune_tool_result_text;
use super::*;
use crate::budget::BudgetSnapshot;
use crate::tasks::TaskId;
use crate::tasks::TaskNode;
use crate::test_support::FakeModel;
use crate::tools::ToolResult;
use crate::tools::{ToolDefinition, ToolIntent};

fn prompt(ctx_blocks: Vec<ContentBlock>) -> PromptContext {
    PromptContext::new(SessionId::new("s-1"), ctx_blocks)
}

fn sample_request() -> ModelRequest {
    ModelRequest {
        transcript: vec![ModelMessage::text(ModelRole::User, "hello")],
        tools: vec![ToolDefinition::new("read_file", "reads a file")],
        budget: BudgetSnapshot {
            iterations_used: 1,
            iterations_max: Some(16),
            model_calls_used: 1,
            model_calls_max: Some(16),
            tool_calls_used: 0,
            tool_calls_max: Some(32),
            subagents_used: 0,
            subagents_max: 8,
            output_bytes_used: 0,
            output_bytes_max: Some(1024 * 1024),
            input_tokens_used: None,
            input_tokens_max: None,
            output_tokens_used: None,
            output_tokens_max: None,
        },
        task: TaskNode::new(TaskId::new("task-1"), "hello", "hello"),
        available_models: Vec::new(),
        model_id: None,
        prompt_tokens: None,
        tool_control: ToolCallControl::auto(),
        response_format: None,
        allow_tool_list_updates: false,
        generation: None,
        session: None,
        continuation: None,
    }
}

#[test]
fn request_roundtrips_through_json() {
    let request = sample_request();
    let json = serde_json::to_string(&request).expect("serializes");
    let restored: ModelRequest = serde_json::from_str(&json).expect("parses");
    assert_eq!(restored, request);
}

#[test]
fn response_roundtrips_through_json() {
    let response = ModelResponse::new()
        .reasoning("think")
        .text("answer")
        .tool_intents(vec![ToolIntent::new("tc-1", "read_file", serde_json::json!({"path": "/a"}))])
        .completed();
    let json = serde_json::to_string(&response).expect("serializes");
    let restored: ModelResponse = serde_json::from_str(&json).expect("parses");
    assert_eq!(restored, response);
    assert!(restored.completed);
    assert!(!restored.is_empty());
}

#[test]
fn empty_response_detection_ignores_blank_reasoning() {
    assert!(ModelResponse::new().is_empty());
    assert!(ModelResponse::new().reasoning("").is_empty());
    assert!(!ModelResponse::new().text("x").is_empty());
}

#[test]
fn message_builders_set_role_and_content() {
    let message = ModelMessage::text(ModelRole::System, "be terse");
    assert_eq!(message.role, ModelRole::System);
    assert_eq!(message.content, vec![ModelContent::Text("be terse".into())]);
    assert_eq!(message.reasoning_summary, None);
}

#[test]
fn metadata_is_bounded_and_deterministic() {
    let entries: Vec<(String, String)> =
        (0..MAX_METADATA_ENTRIES + 4).map(|index| (format!("k{index}"), "v".into())).collect();
    let message = ModelMessage::new(ModelRole::System).with_metadata(entries.clone());
    assert_eq!(message.metadata.len(), MAX_METADATA_ENTRIES);
    for (key, _) in &entries[..MAX_METADATA_ENTRIES] {
        assert!(message.metadata.contains_key(key), "entry {key} kept");
    }
    assert!(!message.metadata.contains_key("k16"), "overflow entry dropped");
    let json = serde_json::to_value(&message.metadata).expect("serializes");
    assert!(json.is_object(), "metadata stays a flat object");
}

#[test]
fn prompt_text_converts_to_normalized_user_message() {
    let transcript =
        Transcript::from_prompt(&prompt(vec![ContentBlock::Text(TextContent::new("hello world"))]));
    assert_eq!(transcript.messages().len(), 1);
    let message = &transcript.messages()[0];
    assert_eq!(message.role, ModelRole::User);
    assert_eq!(message.content, vec![ModelContent::Text("hello world".into())]);
    assert_eq!(message.reasoning_summary, None);
    assert!(message.metadata.is_empty());
    assert!(transcript.truncation().is_none());
}

#[test]
fn prompt_resource_link_converts_to_file_reference() {
    let transcript = Transcript::from_prompt(&prompt(vec![
        ContentBlock::Text(TextContent::new("read it")),
        ContentBlock::ResourceLink(ResourceLink::new("notes", "file:///work/notes.md")),
    ]));
    let content = &transcript.messages()[0].content;
    assert_eq!(content.len(), 2);
    assert_eq!(content[0], ModelContent::Text("read it".into()));
    assert_eq!(content[1], ModelContent::FileReference { path: "file:///work/notes.md".into() });
}

#[test]
fn multimodal_prompt_blocks_convert_without_metadata_fallback() {
    let transcript = Transcript::from_prompt(&prompt(vec![
        ContentBlock::Image(ImageContent::new("aW1n", "image/png").uri("file:///shot.png")),
        ContentBlock::Audio(AudioContent::new("YXVkaW8=", "audio/wav")),
        ContentBlock::Resource(EmbeddedResource::new(
            EmbeddedResourceResource::TextResourceContents(TextResourceContents::new(
                "resource body",
                "file:///notes.md",
            )),
        )),
        ContentBlock::Resource(EmbeddedResource::new(
            EmbeddedResourceResource::BlobResourceContents(
                BlobResourceContents::new("YmxvYg==", "file:///scan.png").mime_type("image/png"),
            ),
        )),
        ContentBlock::Text(TextContent::new("keep")),
    ]));
    let message = &transcript.messages()[0];
    assert_eq!(
        message.content[0],
        ModelContent::Image {
            data: "aW1n".into(),
            mime_type: "image/png".into(),
            uri: Some("file:///shot.png".into()),
        }
    );
    assert_eq!(
        message.content[1],
        ModelContent::Audio { data: "YXVkaW8=".into(), mime_type: "audio/wav".into() }
    );
    assert_eq!(
        message.content[2],
        ModelContent::EmbeddedResource {
            uri: "file:///notes.md".into(),
            mime_type: None,
            text: Some("resource body".into()),
            blob: None,
        }
    );
    assert_eq!(
        message.content[3],
        ModelContent::EmbeddedResource {
            uri: "file:///scan.png".into(),
            mime_type: Some("image/png".into()),
            text: None,
            blob: Some("YmxvYg==".into()),
        }
    );
    assert_eq!(message.content[4], ModelContent::Text("keep".into()));
    assert!(
        !message.metadata.contains_key("unsupported_blocks"),
        "supported multimodal blocks never enter diagnostic metadata"
    );
}

#[test]
fn reasoning_kept_separate_from_assistant_text() {
    let mut transcript = Transcript::new();
    transcript.push_assistant(&ModelResponse::new().reasoning("plan").text("answer"));
    assert_eq!(transcript.messages().len(), 1);
    let message = &transcript.messages()[0];
    assert_eq!(message.role, ModelRole::Assistant);
    assert_eq!(message.content, vec![ModelContent::Text("answer".into())]);
    assert_eq!(message.reasoning_summary.as_deref(), Some("plan"));
}

#[test]
fn push_assistant_skips_empty_responses() {
    let mut transcript = Transcript::new();
    transcript.push_assistant(&ModelResponse::new());
    transcript.push_assistant(&ModelResponse::new().reasoning(""));
    assert!(transcript.is_empty());
}

#[test]
fn aborted_tool_results_answer_every_unanswered_call() {
    let mut transcript = Transcript::new();
    transcript.push_assistant(&ModelResponse::new().tool_intents(vec![
        ToolIntent::new("tc-1", "read_file", serde_json::json!({})),
        ToolIntent::new("tc-2", "read_file", serde_json::json!({})),
    ]));
    transcript.push_tool_result("tc-1", ToolResult::success("ok"));
    assert_eq!(transcript.push_aborted_results_for_unanswered(), 1);
    assert_eq!(transcript.messages().len(), 3);
    let last = &transcript.messages()[2];
    let ModelContent::ToolResult { tool_call_id, result } = &last.content[0] else {
        panic!("expected a tool result");
    };
    assert_eq!(tool_call_id, "tc-2");
    assert_eq!(result.error_kind, Some(crate::tools::ToolErrorKind::Cancelled));
}

#[test]
fn tool_result_appends_stable_tool_call_id() {
    let mut transcript = Transcript::new();
    transcript.push_tool_result("tc-9", ToolResult::success("done"));
    assert_eq!(transcript.messages().len(), 1);
    let message = &transcript.messages()[0];
    assert_eq!(message.role, ModelRole::Tool);
    let ModelContent::ToolResult { tool_call_id, result } = &message.content[0] else {
        panic!("expected tool result content");
    };
    assert_eq!(tool_call_id, "tc-9");
    assert!(result.success);
}

#[test]
fn provider_tool_result_records_the_producing_route() {
    let mut transcript = Transcript::new();
    let intent = ToolIntent::new("ws-1", "web_search", serde_json::json!({}))
        .with_provider_executed(true)
        .with_provider("OpenCode");
    transcript.push_provider_tool_result(
        &intent,
        ToolResult::success_structured(r#"{"type":"web_search_call"}"#, serde_json::json!({})),
    );
    let message = &transcript.messages()[0];
    assert_eq!(message.role, ModelRole::Tool);
    assert_eq!(
        message
            .metadata
            .get(crate::model::PROVIDER_EXECUTED_RESULT_PROVIDER_KEY)
            .map(String::as_str),
        Some("OpenCode")
    );
    assert!(message.metadata.contains_key(crate::model::PROVIDER_EXECUTED_RESULT_KEY));
}

#[test]
fn provider_tool_result_without_a_route_stays_unstamped() {
    let mut transcript = Transcript::new();
    let intent =
        ToolIntent::new("ws-1", "web_search", serde_json::json!({})).with_provider_executed(true);
    transcript.push_provider_tool_result(&intent, ToolResult::success("provider executed"));
    assert!(
        !transcript.messages()[0]
            .metadata
            .contains_key(crate::model::PROVIDER_EXECUTED_RESULT_PROVIDER_KEY)
    );
}

#[test]
fn subagent_summary_appends_subagent_message() {
    let mut transcript = Transcript::new();
    transcript.push_subagent_summary("subagent finished");
    let message = &transcript.messages()[0];
    assert_eq!(message.role, ModelRole::Subagent);
    assert_eq!(message.content, vec![ModelContent::Text("subagent finished".into())]);
}

#[test]
fn transcript_truncation_preserves_newest_and_records_metadata() {
    let mut transcript = Transcript::new();
    for index in 0..6 {
        transcript.push_assistant(&ModelResponse::new().text(format!("message {index}")));
    }
    assert_eq!(transcript.messages().len(), 6);
    let newest_two =
        message_bytes(&transcript.messages()[4]) + message_bytes(&transcript.messages()[5]);

    transcript.enforce_memory_limit(newest_two);

    assert_eq!(transcript.messages().len(), 2);
    assert_eq!(
        transcript.messages()[0].content,
        vec![ModelContent::Text("message 4".into())],
        "oldest messages dropped first"
    );
    assert_eq!(transcript.messages()[1].content, vec![ModelContent::Text("message 5".into())]);
    let truncation = transcript.truncation().expect("records truncation");
    assert_eq!(truncation.removed_messages, 4);
    assert!(truncation.removed_bytes > 0);
    assert_eq!(truncation.retained_bytes, newest_two);
    assert_eq!(truncation.limit_bytes, newest_two);
}

#[test]
fn enforce_memory_limit_keeps_single_oversized_message() {
    let mut transcript = Transcript::new();
    transcript.push_assistant(&ModelResponse::new().text("x".repeat(10_000)));
    transcript.enforce_memory_limit(1);
    assert_eq!(transcript.messages().len(), 1, "newest message is never dropped");
    assert!(transcript.truncation().is_none());
}

#[test]
fn enforce_memory_limit_within_budget_clears_truncation() {
    let mut transcript = Transcript::new();
    transcript.push_assistant(&ModelResponse::new().text("hi"));
    transcript.enforce_memory_limit(1_000);
    assert!(transcript.truncation().is_none());
}

#[test]
fn tool_result_prune_keeps_head_marker_and_tail() {
    let text = format!(
        "{}{}{}",
        "h".repeat(TOOL_RESULT_PRUNE_HEAD),
        "m".repeat(TOOL_RESULT_PRUNE_THRESHOLD),
        "t".repeat(TOOL_RESULT_PRUNE_TAIL)
    );
    let pruned = prune_tool_result_text(&text).expect("oversized result prunes");
    assert!(pruned.starts_with(&"h".repeat(TOOL_RESULT_PRUNE_HEAD)));
    assert!(pruned.ends_with(&"t".repeat(TOOL_RESULT_PRUNE_TAIL)));
    assert!(pruned.contains(TOOL_RESULT_PRUNE_MARKER));
    assert!(!pruned.contains(&"m".repeat(10)), "middle removed");
}

#[test]
fn tool_result_prune_leaves_threshold_untouched() {
    let text = "x".repeat(TOOL_RESULT_PRUNE_THRESHOLD);
    assert!(prune_tool_result_text(&text).is_none());
    assert!(prune_tool_result_text("short").is_none());
}

#[test]
fn request_projection_does_not_mutate_transcript() {
    let mut transcript = Transcript::new();
    let oversized = format!("{}{}", "a".repeat(TOOL_RESULT_PRUNE_THRESHOLD), "b".repeat(10));
    transcript.push_tool_result("tc-1", ToolResult::success(&oversized));
    let projected = transcript.request_messages();
    let projected_text = match &projected[0].content[0] {
        ModelContent::ToolResult { result, .. } => result.text_output.as_str(),
        other => panic!("expected tool result, got {other:?}"),
    };
    assert!(projected_text.contains(TOOL_RESULT_PRUNE_MARKER));
    let durable_text = match &transcript.messages()[0].content[0] {
        ModelContent::ToolResult { result, .. } => result.text_output.as_str(),
        other => panic!("expected tool result, got {other:?}"),
    };
    assert_eq!(durable_text, oversized, "durable transcript keeps the full result");
}

#[test]
fn normalized_transcript_json_has_no_provider_specific_fields() {
    let mut transcript = Transcript::new();
    transcript.push_assistant(&ModelResponse::new().reasoning("r").text("a"));
    let json = serde_json::to_value(&transcript).expect("serializes");
    let message = &json["messages"][0];
    let mut keys: Vec<String> =
        message.as_object().expect("message object").keys().cloned().collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["content", "metadata", "reasoning_summary", "role", "trust"]);
    let serialized = serde_json::to_string(&transcript).expect("serializes");
    assert!(!serialized.contains("provider"), "no provider fields: {serialized}");
    let restored: Transcript = serde_json::from_str(&serialized).expect("parses");
    assert_eq!(restored, transcript);
}

#[tokio::test]
async fn tool_intent_parsing_from_fake_model_response() {
    let model = FakeModel::new(vec![ModelResponse::new().tool_intents(vec![ToolIntent::new(
        "tc-1",
        "read_file",
        json!({"path": "/a"}),
    )])]);
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let response = model.complete(sample_request(), cancel_rx).await.expect("fake succeeds");
    assert_eq!(response.tool_intents.len(), 1);
    let intent = &response.tool_intents[0];
    assert_eq!(intent.tool_call_id, "tc-1");
    assert_eq!(intent.name, "read_file");
    assert_eq!(intent.arguments, json!({"path": "/a"}));
}

#[test]
fn usage_accumulates_and_reports_prompt_cache_tokens() {
    let first = ModelUsage::new()
        .with_input_tokens(100)
        .with_output_tokens(20)
        .with_cached_input_tokens(80);
    let second = ModelUsage::new().with_cache_creation_input_tokens(5);
    let total = first.accumulate(&second);
    assert_eq!(total.input_tokens, Some(100), "unknown fields never zero a known count");
    assert_eq!(total.cached_input_tokens, Some(80));
    assert_eq!(total.cache_creation_input_tokens, Some(5));

    let acp = total.acp_usage().expect("input and output are known");
    assert_eq!(acp.cached_read_tokens, Some(80));
    assert_eq!(acp.cached_write_tokens, Some(5));
}

#[tokio::test]
async fn empty_script_exhaustion_returns_empty_response() {
    let model = FakeModel::new(Vec::new());
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let response = model.complete(sample_request(), cancel_rx).await.expect("fake succeeds");
    assert!(response.is_empty());
    assert!(!response.completed);
}

#[test]
fn output_limit_caps_fits_and_floors_at_the_provider_minimum() {
    let limits = ModelLimits::new(200_000, 500_000);
    assert_eq!(fit_output_limit(Some(limits), None), Some(OUTPUT_LIMIT_MAX));

    let fitted = fit_output_limit(Some(limits), Some(150_000)).expect("fits");
    assert!(fitted >= OUTPUT_LIMIT_MIN, "fitted {fitted}");
    assert!(
        fitted + 150_000 * 116 / 100 + 2_000 <= 200_000,
        "output plus prompt margin must fit the window: {fitted}"
    );

    let small = ModelLimits::new(200_000, 4_096);
    assert_eq!(fit_output_limit(Some(small), Some(1_000)), Some(4_096));
    assert_eq!(
        fit_output_limit(Some(small), Some(199_999)),
        Some(OUTPUT_LIMIT_MIN),
        "a full window still asks for the provider minimum, never zero or one"
    );
    assert_eq!(fit_output_limit(None, Some(1_000)), None);
}

#[test]
fn prompt_estimate_is_positive_for_empty_and_populated_requests() {
    assert!(estimate_prompt_tokens(&[], &[], "") >= 1);
    let transcript = vec![ModelMessage::text(ModelRole::User, "hello")];
    let tools = vec![ToolDefinition::new("read_file", "reads a file")];
    let estimate = estimate_prompt_tokens(&transcript, &tools, "system");
    assert!(estimate >= 2, "estimate {estimate}");
}

#[test]
fn response_carries_finish_reason() {
    let response = ModelResponse::new().with_finish_reason(Some(String::from("length")));
    assert_eq!(response.finish_reason.as_deref(), Some("length"));
    let json = serde_json::to_string(&response).expect("serializes");
    let restored: ModelResponse = serde_json::from_str(&json).expect("parses");
    assert_eq!(restored.finish_reason.as_deref(), Some("length"));
}

#[test]
fn cut_turn_drops_native_state_but_keeps_reasoning_text() {
    let native = OpaqueProviderState::new(serde_json::json!({
        "thinking_blocks": [{ "type": "thinking", "signature": "sig" }],
    }))
    .expect("fits");
    let response = ModelResponse::new()
        .reasoning("partial reasoning")
        .with_native(Some(native))
        .with_finish_reason(Some(String::from("error")));
    let mut transcript = Transcript::new();
    transcript.push_assistant(&response);
    let message = transcript.messages.first().expect("assistant message");
    assert!(message.native.is_none(), "a cut turn must not replay provider state");
    assert_eq!(message.reasoning_summary.as_deref(), Some("partial reasoning"));
}

#[test]
fn a_turn_without_a_finish_reason_is_treated_as_cut() {
    let native = OpaqueProviderState::new(serde_json::json!({
        "reasoning_items": [{ "id": "rs_1", "encrypted_content": "blob" }],
    }))
    .expect("fits");
    let response = ModelResponse::new().with_native(Some(native)).with_finish_reason(None);
    let mut transcript = Transcript::new();
    transcript.push_assistant(&response);
    let message = transcript.messages.first().expect("assistant message");
    assert!(
        message.native.is_none(),
        "a stream that stopped without a finish marker must not replay its tail state"
    );
}

#[test]
fn opaque_provider_state_is_redacted_and_bounded() {
    let state = OpaqueProviderState::new(serde_json::json!({
        "thinking": "secret reasoning text",
        "signature": "sig",
    }))
    .expect("fits");
    let debug = format!("{state:?}");
    assert_eq!(debug, "OpaqueProviderState([opaque])");
    assert!(!debug.contains("secret reasoning text"));
    assert_eq!(state.as_value()["signature"], "sig");

    let huge = OpaqueProviderState::new(serde_json::json!({
        "blob": "x".repeat(OpaqueProviderState::MAX_BYTES),
    }));
    assert!(huge.is_none(), "oversized state is dropped, never replayed truncated");
}

#[test]
fn shrink_oldest_drops_half_the_removable_messages_and_keeps_the_system_prefix() {
    let mut transcript = Transcript::new();
    transcript.messages.push(ModelMessage::text(ModelRole::System, "policy"));
    transcript.log_ids.push(None);
    for index in 0..5 {
        transcript.messages.push(ModelMessage::text(ModelRole::User, format!("message {index}")));
        transcript.log_ids.push(None);
    }
    assert!(transcript.shrink_oldest());
    assert_eq!(transcript.messages[0].role, ModelRole::System);
    // Five removable messages, half dropped (2), newest kept.
    assert_eq!(transcript.messages.len(), 4);
    assert_eq!(transcript.truncation.as_ref().map(|t| t.removed_messages), Some(2));
    assert_eq!(transcript.log_ids.len(), transcript.messages.len());
}

#[test]
fn shrink_oldest_never_starts_with_an_orphan_tool_result() {
    let mut transcript = Transcript::new();
    transcript.messages.push(ModelMessage::text(ModelRole::User, "read it"));
    transcript.log_ids.push(None);
    transcript.messages.push(ModelMessage::tool_call("call_1", "read_file", serde_json::json!({})));
    transcript.log_ids.push(None);
    transcript.messages.push(ModelMessage::tool_result("call_1", ToolResult::success("body")));
    transcript.log_ids.push(None);
    transcript.messages.push(ModelMessage::text(ModelRole::User, "next"));
    transcript.log_ids.push(None);

    assert!(transcript.shrink_oldest());
    assert_ne!(
        transcript.messages.first().map(|message| message.role),
        Some(ModelRole::Tool),
        "the dropped call must take its observation with it"
    );
    assert_eq!(transcript.log_ids.len(), transcript.messages.len());
}
