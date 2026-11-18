//! Capability gates: sampling, hosted tools, affinity, compaction.

use super::*;

#[tokio::test]
async fn sampling_and_phase_budgets_reach_the_routed_profile() {
    let dirs = TestDirs::new();
    let mut config = test_config(OpenCodeSurface::Zen, "claude-sonnet-5");
    config.generation = ee_agent_orchestrator::GenerationOptions {
        temperature: Some(0.2),
        top_k: Some(40),
        ..ee_agent_orchestrator::GenerationOptions::none()
    };
    config.timeouts.chunk = std::time::Duration::from_secs(7);

    let profile = config.profile().expect("profile builds");
    assert_eq!(profile.generation.temperature, Some(0.2));
    assert_eq!(profile.generation.top_k, Some(40));
    assert_eq!(profile.timeouts.chunk, std::time::Duration::from_secs(7));
    assert_eq!(profile.timeouts.headers, ee_chat_completions::Timeouts::DEFAULT_HEADERS);

    let (codec, harness, task) =
        spawn_agent_with_config(config, vec![ScriptedAnswer::text("ok")], &dirs);
    initialize(&harness).await;
    let session_id = new_session(&harness, 2).await;
    harness.send(request(3, "session/prompt", prompt_params(&session_id, "hello")));
    drain_prompt_start(&harness).await;
    let (_updates, result) = harness.collect_until_response(3).await;

    assert_eq!(result["stopReason"], "end_turn");
    assert_eq!(codec.requests().len(), 1, "the turn still runs one model round");

    harness.shutdown(task).await;
}

#[tokio::test]
async fn provider_executed_tool_result_reaches_the_next_request() {
    let dirs = TestDirs::new();
    let (codec, harness, task) = spawn_agent(
        OpenCodeSurface::Zen,
        OpenCodeDialect::OpenAiResponses,
        vec![
            ScriptedAnswer::hosted_tool_call("hosted-1", "web_search", json!({ "answer": "ok" })),
            ScriptedAnswer::text("done"),
        ],
        &dirs,
    );
    initialize(&harness).await;
    let session_id = new_session(&harness, 2).await;
    harness.send(request(3, "session/prompt", prompt_params(&session_id, "search")));
    drain_prompt_start(&harness).await;
    let (_updates, result) = harness.collect_until_response(3).await;

    assert_eq!(result["stopReason"], "end_turn");
    let requests = codec.requests();
    assert_eq!(requests.len(), 2, "the hosted call still asks the model for a final answer");
    let provider_results: Vec<_> =
        requests[1].transcript.iter().filter(|message| message.role == ModelRole::Tool).collect();
    assert_eq!(provider_results.len(), 1, "the provider result appears exactly once");
    assert!(
        provider_results[0]
            .metadata
            .contains_key(ee_agent_orchestrator::PROVIDER_EXECUTED_RESULT_KEY)
    );
    // The hosted item is replayed verbatim, so the result records the route that
    // emitted it; replay gating compares that route with the next request.
    let producer = provider_results[0]
        .metadata
        .get(ee_agent_orchestrator::PROVIDER_EXECUTED_RESULT_PROVIDER_KEY)
        .expect("the producing route is recorded on the hosted result");
    assert!(producer.starts_with("OpenCode "), "route identity is the catalog label: {producer}");

    harness.shutdown(task).await;
}

#[tokio::test]
async fn session_affinity_reaches_the_routed_profile_headers() {
    let mut config = test_config(OpenCodeSurface::Zen, "claude-sonnet-5");
    config.session_headers = true;
    let profile = config.profile().expect("profile builds");

    assert_eq!(profile.session_header_style, ee_chat_completions::SessionHeaderStyle::OpenCode);
    assert!(profile.user_agent.starts_with("ee-agent-opencode/"));

    let context = ee_agent_orchestrator::SessionContext::new("child")
        .with_parent("parent")
        .with_project("project");
    let headers =
        ee_chat_completions::session_headers(&profile, &context).expect("affinity headers build");
    let value = |name: &str| {
        headers
            .iter()
            .find(|(header, _)| header.as_str() == name)
            .and_then(|(_, value)| value.to_str().ok())
    };
    assert_eq!(value("x-opencode-session-id"), Some("child"));
    assert_eq!(value("x-session-affinity"), Some("parent"));
    assert_eq!(value("x-opencode-session"), Some("parent"));
    assert_eq!(value("x-opencode-project"), Some("project"));
    assert!(value("user-agent").is_some(), "user agent is sent");
}

/// Gap 20: the adapter refuses provider compaction on a route whose catalog row
/// does not advertise it, before any request or credential header exists.
#[tokio::test]
async fn provider_compaction_is_refused_on_an_unsupported_route() {
    use ee_agent_orchestrator::{
        BudgetSnapshot, ModelAdapter, ModelError, ModelRequest, TaskId, TaskNode,
    };

    let config = test_config(OpenCodeSurface::Zen, "claude-sonnet-5");
    assert!(!config.profile().expect("profile").compaction);
    let adapter = ee_agent_opencode::adapter::OpenCodeModelAdapter::new(&config).expect("adapter");
    let request = ModelRequest::new(
        Vec::new(),
        Vec::new(),
        BudgetSnapshot::default(),
        TaskNode::new(TaskId::new("compact"), "compact", "compact"),
    );
    let (_cancel, receiver) = tokio::sync::watch::channel(false);

    let error = adapter.compact(request, receiver).await.expect_err("unsupported route");
    assert!(matches!(error, ModelError::UnsupportedCompaction(_)), "{error}");
    assert!(error.to_string().contains("claude-sonnet-5"), "{error}");
}

/// Gap 20: a catalog route that advertises compaction reaches the profile flag
/// the runtime gates on.
#[test]
fn supported_routes_advertise_compaction_in_the_profile() {
    let config = test_config(OpenCodeSurface::Zen, "gpt-5.5");
    assert!(config.route.supports_compaction());
    assert!(config.profile().expect("profile").compaction);
}
