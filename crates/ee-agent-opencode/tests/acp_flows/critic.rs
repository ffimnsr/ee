//! Rubber-duck second opinion flows.

use super::*;

// ── Critic (rubber-duck) second opinion ──────────────────────────────────

/// Serialized clean critique report the critic model must return.
fn clean_critique_report() -> String {
    let report = CritiqueReport::clean(CritiqueTarget::Implementation);
    serde_json::to_string(&report).expect("clean report serializes")
}

#[tokio::test]
async fn rubber_duck_second_opinion_runs_the_configured_critic() {
    for (root_model, critic_model) in [("gpt-5.5", "kimi-k3"), ("qwen3.7-max", "kimi-k3")] {
        let surface = OpenCodeSurface::Zen;
        let label = format!("{root_model} + {critic_model}");
        let dirs = TestDirs::new();
        let root_route = resolve_route(surface, root_model).expect("documented root route");
        let critic_route = resolve_route(surface, critic_model).expect("documented critic route");
        assert_ne!(root_route.dialect, critic_route.dialect, "{label} spans two dialects");

        let root_codec = Arc::new(ScriptedCodec::new(
            root_route.dialect,
            vec![ScriptedAnswer::text("synthesis after critique")],
        ));
        let critic_codec = Arc::new(ScriptedCodec::new(
            critic_route.dialect,
            vec![ScriptedAnswer::text(&clean_critique_report())],
        ));
        let mut config = test_config(surface, root_model);
        config.critic_model = Some(critic_model.to_string());

        let (provider, warning) = opencode_multi_model_provider_with_codecs(
            &config,
            dirs.state(),
            (root_route, root_codec.clone() as Arc<dyn ee_agent_opencode::adapter::DialectCodec>),
            Some((
                critic_route,
                critic_codec.clone() as Arc<dyn ee_agent_opencode::adapter::DialectCodec>,
            )),
        )
        .expect("provider builds with a critic");
        assert!(warning.is_none(), "{label}: {warning:?}");

        let (harness, task) = spawn_provider(provider, &dirs);
        initialize(&harness).await;
        let session_id = new_session(&harness, 2).await;

        harness.send(request(3, "session/prompt", prompt_params(&session_id, "/rubber-duck")));
        let (updates, result) = harness.collect_until_response(3).await;

        assert_eq!(result["stopReason"], "end_turn", "{label}");
        let selection = updates
            .iter()
            .find(|params| params["update"]["messageId"] == "rubber-duck-selected")
            .unwrap_or_else(|| panic!("{label} reports the selected critic: {updates:?}"));
        // The runtime reports registry identities: the critic is the
        // `rubber_duck` route, and the root is the `default` route.
        assert!(
            selection["update"]["content"]["text"].as_str().is_some_and(|text| {
                text.contains("rubber_duck") && text.contains("root default")
            }),
            "{label} reports the selected critic route: {selection}"
        );
        assert!(
            has_text_chunk(&updates, "agent_message_chunk", "synthesis after critique"),
            "{label} streams the root synthesis"
        );

        // The critic ran exactly once, on its own route and dialect, and the
        // root then synthesized once.
        assert_eq!(critic_codec.requests().len(), 1, "{label} critic call");
        assert_eq!(root_codec.requests().len(), 1, "{label} root synthesis call");
        let critic_request = critic_codec.requests()[0].json().to_string();
        assert!(
            critic_request.contains("rubber-duck critic"),
            "{label} critic received the critique contract: {critic_request}"
        );
        let root_request = root_codec.requests()[0].json().to_string();
        assert!(
            root_request.contains("read it") || root_request.contains("final decision owner"),
            "{label} root received the synthesis contract: {root_request}"
        );

        harness.shutdown(task).await;
    }
}

#[tokio::test]
async fn rubber_duck_without_a_critic_reports_contrast_unavailable() {
    let dirs = TestDirs::new();
    let (_codec, harness, task) = spawn_agent(
        OpenCodeSurface::Zen,
        OpenCodeDialect::OpenAiResponses,
        vec![ScriptedAnswer::text("unused")],
        &dirs,
    );
    initialize(&harness).await;
    let session_id = new_session(&harness, 2).await;

    harness.send(request(3, "session/prompt", prompt_params(&session_id, "/rubber-duck")));
    let (updates, result) = harness.collect_until_response(3).await;

    assert_eq!(result["stopReason"], "end_turn");
    let reported = updates
        .iter()
        .find(|params| params["update"]["messageId"] == "rubber-duck-result")
        .unwrap_or_else(|| {
            panic!("root-only agent explains why contrast is unavailable: {updates:?}")
        });
    assert!(
        reported["update"]["content"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("rubber duck skipped")),
        "the root-only agent explains why the critic is unavailable: {reported}"
    );

    harness.shutdown(task).await;
}
