//! Dispatcher tests: one resolved route, one dialect codec, one set of
//! orchestrator guarantees (credential gate, cancellation, stream ordering,
//! error mapping). Every test runs without a network.

use std::path::PathBuf;
use std::sync::Arc;

use ee_agent_orchestrator::{
    BudgetTracker, ModelAdapter, ModelError, ModelMessage, ModelRequest, ModelRole,
    OrchestratorConfig, SideEffectClass, TaskId, TaskNode, ToolDefinition, ToolResult,
    stream_channel,
};
use serde_json::json;
use tokio::sync::watch;

use super::test_support::{
    ScriptedAnswer, ScriptedCodec, TEST_API_KEY, missing_token_source, test_config,
    test_token_source,
};
use super::*;
use crate::routes::{OpenCodeSurface, catalog};

/// First documented route for one surface and dialect.
///
/// Fixtures read the catalog instead of hard-coding ids, so a catalog refresh
/// never breaks dispatcher tests, and a missing dialect fails loudly.
fn first_route(surface: OpenCodeSurface, dialect: OpenCodeDialect) -> OpenCodeRoute {
    catalog()
        .find(|route| route.surface == surface && route.dialect == dialect)
        .expect("the catalog documents at least one route for this surface and dialect")
}

fn sample_transcript() -> Vec<ModelMessage> {
    vec![
        ModelMessage::text(ModelRole::System, "Memory facts:\ncwd: /work"),
        ModelMessage::text(ModelRole::User, "hello"),
        ModelMessage::text(ModelRole::Assistant, "hi there"),
        ModelMessage::tool_result("call_1", ToolResult::success("file contents")),
    ]
}

fn sample_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::new("read_file", "reads a file")
            .side_effect_class(SideEffectClass::Read)
            .input_schema(json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
            })),
    ]
}

fn sample_request() -> ModelRequest {
    ModelRequest::new(
        sample_transcript(),
        sample_definitions(),
        BudgetTracker::new(&OrchestratorConfig::default()).snapshot(),
        TaskNode::new(TaskId::new("task-1"), "hello", "hello"),
    )
}

/// Bounded wait for the scripted codec to record `count` requests.
async fn wait_for_requests(codec: &ScriptedCodec, count: usize) {
    for _ in 0..5_000 {
        if codec.requests().len() >= count {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("timed out waiting for {count} recorded codec request(s)");
}

#[test]
fn every_dialect_route_dispatches_to_its_own_codec() {
    for dialect in [
        OpenCodeDialect::OpenAiResponses,
        OpenCodeDialect::AnthropicMessages,
        OpenCodeDialect::OpenAiChatCompletions,
    ] {
        let route = first_route(OpenCodeSurface::Zen, dialect);
        let codec = Arc::new(ScriptedCodec::new(dialect, Vec::new()));
        let adapter = codec.adapter(route);

        assert_eq!(adapter.dialect(), dialect);
        assert_eq!(adapter.route().model_id, route.model_id);
        assert_eq!(adapter.route().endpoint, route.endpoint);
    }
}

#[test]
#[should_panic(expected = "the injected codec must speak the routed dialect")]
fn injected_codec_must_speak_the_routed_dialect() {
    let route = first_route(OpenCodeSurface::Zen, OpenCodeDialect::OpenAiResponses);
    let codec = Arc::new(ScriptedCodec::new(OpenCodeDialect::AnthropicMessages, Vec::new()));

    let _ = OpenCodeModelAdapter::with_codec(route, test_token_source(), codec);
}

#[tokio::test]
async fn buffered_completion_forwards_transcript_and_tools_unchanged() {
    let route = first_route(OpenCodeSurface::Go, OpenCodeDialect::AnthropicMessages);
    let codec = Arc::new(ScriptedCodec::new(route.dialect, vec![ScriptedAnswer::text("done")]));
    let adapter = codec.adapter(route);
    let (_cancel_tx, cancel) = watch::channel(false);

    let response = adapter.complete(sample_request(), cancel).await.expect("scripted completion");

    assert_eq!(response.text, "done");
    assert!(response.completed);
    let recorded = codec.requests();
    assert_eq!(recorded.len(), 1);
    let request = recorded[0].json();
    // The adapter forwards the normalized transcript and tool definitions
    // unchanged, so a dialect codec only ever re-encodes protocol shape.
    assert_eq!(
        request,
        json!({ "transcript": &sample_transcript(), "tools": &sample_definitions() })
    );
    assert_eq!(request["transcript"][0]["role"], json!(ModelRole::System));
    assert_eq!(request["transcript"][3]["role"], json!(ModelRole::Tool));
    assert!(request.to_string().contains("hello"), "user text reaches the codec");
    assert_eq!(request["tools"][0]["name"], "read_file");
    assert_eq!(request["tools"][0]["input_schema"]["required"][0], "path");
}

#[tokio::test]
async fn missing_credential_fails_before_the_codec_sees_a_request() {
    let route = first_route(OpenCodeSurface::Zen, OpenCodeDialect::OpenAiChatCompletions);
    let codec =
        Arc::new(ScriptedCodec::new(route.dialect, vec![ScriptedAnswer::text("never sent")]));
    let adapter = OpenCodeModelAdapter::with_codec(route, missing_token_source(), codec.clone());
    let (_cancel_tx, cancel) = watch::channel(false);

    let error = adapter.complete(sample_request(), cancel).await.expect_err("no credential");

    assert!(error.to_string().contains("OPENCODE_API_KEY is not set"), "{error}");
    assert!(codec.requests().is_empty(), "no request may exist without a credential");
}

#[tokio::test]
async fn new_builds_a_transport_for_a_documented_route_and_redacts_the_key() {
    let config = test_config(OpenCodeSurface::Zen, "gpt-5.5");
    let adapter = OpenCodeModelAdapter::new(&config).expect("documented route builds");
    let debug = format!("{:?}", config);

    // The key resolves for the adapter and stays out of every diagnostic view.
    assert!(config.has_api_key());
    assert!(!debug.contains(TEST_API_KEY), "config debug must redact the key");
    assert_eq!(adapter.route().endpoint, "https://opencode.ai/zen/v1/responses");
}

#[tokio::test]
async fn cancel_before_start_returns_cancelled_without_a_request() {
    let route = first_route(OpenCodeSurface::Go, OpenCodeDialect::OpenAiResponses);
    let codec = Arc::new(ScriptedCodec::new(route.dialect, vec![ScriptedAnswer::text("late")]));
    let adapter = codec.adapter(route);
    let (cancel_tx, cancel) = watch::channel(false);
    cancel_tx.send(true).expect("cancel flag set");

    let error = adapter.complete(sample_request(), cancel).await.expect_err("pre-cancelled");

    assert!(matches!(error, ModelError::Cancelled));
    assert!(codec.requests().is_empty(), "a cancelled call must not reach the codec");
}

#[tokio::test]
async fn in_flight_model_call_returns_cancelled_promptly() {
    let route = first_route(OpenCodeSurface::Go, OpenCodeDialect::OpenAiChatCompletions);
    let codec = Arc::new(ScriptedCodec::pending(route.dialect));
    let adapter = Arc::new(codec.adapter(route));
    let (cancel_tx, cancel) = watch::channel(false);

    let call = {
        let adapter = Arc::clone(&adapter);
        tokio::spawn(async move { adapter.complete(sample_request(), cancel).await })
    };
    wait_for_requests(&codec, 1).await;
    cancel_tx.send(true).expect("cancel delivered");

    let error = call.await.expect("call task joins").expect_err("cancelled mid-flight");

    assert!(matches!(error, ModelError::Cancelled));
}

#[tokio::test]
async fn streamed_updates_reach_the_sink_before_the_response_returns() {
    let route = first_route(OpenCodeSurface::Zen, OpenCodeDialect::OpenAiChatCompletions);
    let codec = Arc::new(ScriptedCodec::new(
        route.dialect,
        vec![ScriptedAnswer::reasoning("plan step", "final answer")],
    ));
    let adapter = codec.adapter(route);
    let (sink, receiver) = stream_channel();
    let consumer = receiver.into_consumer();
    let (_cancel_tx, cancel) = watch::channel(false);

    let (response, turn) = tokio::join!(
        adapter.complete_streaming(sample_request(), cancel.clone(), sink),
        consumer.run(None, &cancel, "message-1"),
    );

    let response = response.expect("streaming completes");
    assert_eq!(response.text, "final answer");
    assert_eq!(response.reasoning.as_deref(), Some("plan step"));
    assert!(response.completed);
    assert_eq!(turn.text, "final answer");
    assert_eq!(turn.reasoning, "plan step");
    assert!(!turn.cancelled, "the sink stayed open for the whole stream");
}

#[tokio::test]
async fn rejected_stream_updates_fail_the_call_before_it_returns() {
    let route = first_route(OpenCodeSurface::Zen, OpenCodeDialect::OpenAiResponses);
    let codec = Arc::new(ScriptedCodec::new(
        route.dialect,
        vec![ScriptedAnswer::reasoning("plan step", "final answer")],
    ));
    let adapter = codec.adapter(route);
    let (sink, receiver) = stream_channel();
    // The consumer is gone, so the adapter must observe the closed sink instead
    // of returning a response the client never saw.
    drop(receiver);
    let (_cancel_tx, cancel) = watch::channel(false);

    let error = adapter
        .complete_streaming(sample_request(), cancel, sink)
        .await
        .expect_err("closed sink is a failure");

    assert!(error.to_string().contains("streamed update was rejected"), "{error}");
}

#[tokio::test]
async fn adapter_failure_keeps_codec_detail_and_never_the_credential() {
    let route = first_route(OpenCodeSurface::Go, OpenCodeDialect::AnthropicMessages);
    let codec = Arc::new(ScriptedCodec::new(
        route.dialect,
        vec![ScriptedAnswer::Failure(String::from("OpenCode Go messages request failed"))],
    ));
    let adapter = codec.adapter(route);
    let (_cancel_tx, cancel) = watch::channel(false);

    let error = adapter.complete(sample_request(), cancel).await.expect_err("codec failed");

    let rendered = error.to_string();
    assert!(rendered.contains("OpenCode Go messages request failed"), "{rendered}");
    assert!(!rendered.contains(TEST_API_KEY), "{rendered}");
}

#[tokio::test]
async fn codec_cancellation_maps_to_orchestrator_cancellation() {
    let route = first_route(OpenCodeSurface::Zen, OpenCodeDialect::OpenAiResponses);
    let codec = Arc::new(ScriptedCodec::new(route.dialect, vec![ScriptedAnswer::Cancellation]));
    let adapter = codec.adapter(route);
    let (_cancel_tx, cancel) = watch::channel(false);

    let error = adapter.complete(sample_request(), cancel).await.expect_err("cancelled");

    assert!(matches!(error, ModelError::Cancelled));
}

#[test]
fn orchestrator_config_carries_opencode_identity_budgets_and_recovery() {
    let mut config = test_config(OpenCodeSurface::Zen, "gpt-5.5");
    config.max_iterations = 7;
    config.context_window = 123_456;
    let session_state_dir = PathBuf::from("/tmp/ee-opencode-agent-config-test");

    let provider_config = opencode_orchestrator_config(&config, session_state_dir.clone());

    assert_eq!(provider_config.implementation.name, "ee-opencode-agent");
    assert_eq!(provider_config.orchestrator.max_loop_iterations, 7);
    assert_eq!(provider_config.orchestrator.max_model_calls, 7);
    assert_eq!(provider_config.orchestrator.context_window_tokens, 123_456);
    assert_eq!(provider_config.session_state_dir.as_ref(), Some(&session_state_dir));
    assert!(
        !provider_config.orchestrator.recovery.is_durable(),
        "recovery must not claim durability without a checkpoint directory"
    );

    config.checkpoint_dir = Some(PathBuf::from("/tmp/ee-opencode-agent-checkpoints-test"));
    let durable = opencode_orchestrator_config(&config, session_state_dir);
    assert!(durable.orchestrator.recovery.is_durable());
    assert_eq!(
        durable.orchestrator.recovery.checkpoint_dir.as_deref(),
        Some(std::path::Path::new("/tmp/ee-opencode-agent-checkpoints-test"))
    );
}

#[test]
fn reasoning_effort_shapes_only_the_dialect_that_documents_it() {
    let mut chat = test_config(OpenCodeSurface::Zen, "kimi-k3");
    chat.reasoning_effort = Some(crate::reasoning::ReasoningEffort::High);
    let profile = chat.profile().expect("profile builds");
    assert_eq!(profile.extensions, Some(json!({ "reasoning_effort": "high" })));
    assert_eq!(chat.reasoning_effort_note(), None);

    let mut responses = test_config(OpenCodeSurface::Zen, "gpt-5.5");
    responses.reasoning_effort = Some(crate::reasoning::ReasoningEffort::Low);
    let profile = responses.profile().expect("profile builds");
    assert_eq!(profile.extensions, Some(json!({ "reasoning": { "effort": "low" } })));
    assert_eq!(responses.reasoning_effort_note(), None);

    let mut messages = test_config(OpenCodeSurface::Zen, "claude-opus-4-5");
    messages.reasoning_effort = Some(crate::reasoning::ReasoningEffort::Medium);
    assert_eq!(messages.profile().expect("profile builds").extensions, None);
    let note = messages.reasoning_effort_note().expect("unsupported dialect explains itself");
    assert!(note.contains("anthropic_messages"), "{note}");

    // Without the knob, every dialect keeps its request shape.
    for model_id in ["gpt-5.5", "claude-opus-4-5", "kimi-k3"] {
        let config = test_config(OpenCodeSurface::Zen, model_id);
        assert_eq!(config.profile().expect("profile builds").extensions, None, "{model_id}");
        assert_eq!(config.reasoning_effort_note(), None, "{model_id}");
    }
}
