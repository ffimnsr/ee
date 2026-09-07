//! Runtime tests: strategic.
use super::*;

#[tokio::test]
async fn manual_rubber_duck_uses_critic_then_one_no_tools_root_synthesis() {
    let root = FakeModel::new(vec![
        ModelResponse::new()
            .text("No blocking findings. Plan unchanged because review was clean.")
            .completed(),
    ]);
    let critic = FakeModel::new(vec![
        ModelResponse::new()
            .text(
                serde_json::to_string(&crate::CritiqueReport::clean(
                    CritiqueTarget::Implementation,
                ))
                .expect("report"),
            )
            .completed(),
    ]);
    let mut models = ModelRegistry::new();
    models
        .register_model(
            DEFAULT_MODEL_ID,
            Arc::new(root.clone()),
            ModelRegistration::new(
                ModelIdentity::new(
                    "root-model",
                    "test",
                    ModelFamily::OpenAi,
                    "Root",
                    [ModelCapability::ChatCompletion, ModelCapability::Tools],
                )
                .expect("root identity"),
            ),
        )
        .expect("root route");
    models
        .register_model(
            RUBBER_DUCK_ROLE,
            Arc::new(critic.clone()),
            ModelRegistration::new(
                ModelIdentity::new(
                    "critic-model",
                    "test",
                    ModelFamily::Anthropic,
                    "Critic",
                    [ModelCapability::ChatCompletion, ModelCapability::Tools],
                )
                .expect("critic identity"),
            )
            .for_roles(&[RUBBER_DUCK_ROLE]),
        )
        .expect("critic route");
    let runtime = OrchestratorRuntime::with_model_registry(
        OrchestratorConfig::default(),
        models,
        PolicyEngine::default(),
    )
    .expect("runtime");
    let (sink, _client, mut rx) = plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let turn = runtime
        .run_manual_rubber_duck("s-1", None, Vec::new(), sink, cancel_rx)
        .await
        .expect("manual critique succeeds");

    assert!(matches!(turn.critic_outcome, RubberDuckOutcome::Completed(_)));
    assert_eq!(critic.call_count(), 1);
    assert_eq!(root.call_count(), 1);
    assert!(turn.synthesis.as_deref().is_some_and(|text| text.contains("Plan unchanged")));
    assert!(root.requests()[0].tools.is_empty(), "root synthesis cannot mutate");
    let mut update_text = String::new();
    while let Ok(event) = rx.try_recv() {
        if let OutboundEvent::Update { update, .. } = event
            && let SessionUpdate::AgentMessageChunk(chunk) = *update
            && let ContentBlock::Text(text) = chunk.content
        {
            update_text.push_str(&text.text);
        }
    }
    assert!(update_text.contains("Plan unchanged"));
    assert!(!update_text.contains("schema_version"), "raw critic report stays hidden");
}

// ── Strategic turn path ────────────────────────────────────────────────

#[tokio::test]
async fn default_strategic_recovery_path_injects_bounded_capability_guidance_and_context() {
    let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("done").completed()]));
    let runtime = OrchestratorRuntime::new(OrchestratorConfig::default(), model.clone());
    let (sink, client, _rx) = plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let fresh = crate::context_planner::ContextFreshness::fresh("rev-1");
    let input = StrategicInput {
        context: Some(crate::context_planner::ContextPlanningInput {
            identity: crate::context_planner::ContextPlanIdentity {
                session_id: "s-1".into(),
                policy_revision: "policy-1".into(),
                workspace_revision: "workspace-1".into(),
                buffer_revision: "buffer-1".into(),
                diagnostics_revision: "diagnostics-1".into(),
                graph_revision: "graph-1".into(),
                checkout_revision: "checkout-1".into(),
            },
            candidates: vec![crate::context_planner::ContextCandidate::new(
                "diagnostic-1",
                crate::context_planner::ContextSource::Diagnostics,
                crate::context_planner::ContextTrustClass::RepositoryContent,
                fresh,
                "type mismatch",
            )],
        }),
        ..StrategicInput::default()
    };
    let outcome = runtime
        .run_turn_strategic_recoverable(
            prompt("inspect the active file"),
            sink,
            client,
            cancel_rx,
            StrategicRecoveryContext::new(input, "session facts", "provider"),
        )
        .await
        .expect("turn succeeds");
    assert!(matches!(outcome, StrategicTurnOutcome::Completed(_)));
    let request = &model.requests()[0];
    assert!(request.transcript[0].text_content().contains("Turn guidance:"));
    assert!(request.transcript.iter().any(|message| {
        message.metadata.get("context_source").map(String::as_str) == Some("diagnostics")
    }));
    assert!(
        runtime
            .event_snapshot()
            .iter()
            .any(|event| matches!(event, OrchestratorEvent::StrategySelected { .. }))
    );
}

#[tokio::test]
async fn strategic_turn_selects_strategy_and_emits_decision_event() {
    let model =
        Arc::new(FakeModel::new(vec![ModelResponse::new().text("implemented").completed()]));
    let runtime = OrchestratorRuntime::new(OrchestratorConfig::default(), model);
    let (sink, client, _rx) = plumbing();
    let events = EventRecorder::new();
    let (_cancel_tx, cancel_rx) = watch::channel(false);

    let turn = runtime
        .run_turn_strategic_recording(
            prompt("implement login across multiple files"),
            StrategicInput::default(),
            sink,
            client,
            cancel_rx,
            events.clone(),
        )
        .await
        .expect("strategic turn succeeds");
    assert_eq!(turn.prompt_result.stop_reason, StopReason::EndTurn);
    assert_eq!(turn.strategy.strategy, crate::strategy::TurnStrategy::PlanThenExecute);
    assert_eq!(
        events.events()[0],
        OrchestratorEvent::StrategySelected {
            strategy: crate::strategy::TurnStrategy::PlanThenExecute,
            reason: crate::strategy::StrategyReason::MultiFileImplementation,
        }
    );
    assert!(turn.final_response.summary.contains("no files changed"));
    assert!(!turn.final_response.validation_passed());
}

#[tokio::test]
async fn strategic_turn_uses_editor_context_before_terminal_evidence() {
    let model =
        Arc::new(FakeModel::new(vec![ModelResponse::new().text("implemented").completed()]));
    let runtime = OrchestratorRuntime::new(OrchestratorConfig::default(), model.clone());
    let (sink, client, _rx) = plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let fresh = crate::context_planner::ContextFreshness::fresh("rev-1");
    let context = crate::context_planner::ContextPlanningInput {
        identity: crate::context_planner::ContextPlanIdentity {
            session_id: "s-1".to_string(),
            policy_revision: "policy-1".to_string(),
            workspace_revision: "workspace-1".to_string(),
            buffer_revision: "buffer-1".to_string(),
            diagnostics_revision: "diagnostics-1".to_string(),
            graph_revision: "graph-1".to_string(),
            checkout_revision: "checkout-1".to_string(),
        },
        candidates: vec![
            crate::context_planner::ContextCandidate::new(
                "terminal:1",
                crate::context_planner::ContextSource::TerminalOutput,
                crate::context_planner::ContextTrustClass::TerminalOutput,
                fresh.clone(),
                "terminal probe result",
            ),
            crate::context_planner::ContextCandidate::new(
                "diagnostic:1",
                crate::context_planner::ContextSource::Diagnostics,
                crate::context_planner::ContextTrustClass::RepositoryContent,
                fresh.clone(),
                "type mismatch",
            ),
            crate::context_planner::ContextCandidate::new(
                "buffer:src/lib.rs",
                crate::context_planner::ContextSource::DirtyBuffer,
                crate::context_planner::ContextTrustClass::RepositoryContent,
                fresh,
                "unsaved edit",
            ),
        ],
    };
    let turn = runtime
        .run_turn_strategic(
            prompt("fix the active file"),
            StrategicInput { context: Some(context), ..StrategicInput::default() },
            sink,
            client,
            cancel_rx,
        )
        .await
        .expect("strategic turn succeeds");

    let plan = turn.context_plan.expect("host context planned");
    assert_eq!(
        plan.items.iter().map(|item| item.source).collect::<Vec<_>>(),
        vec![
            crate::context_planner::ContextSource::DirtyBuffer,
            crate::context_planner::ContextSource::Diagnostics,
            crate::context_planner::ContextSource::TerminalOutput,
        ]
    );
    let requests = model.requests();
    assert_eq!(requests.len(), 1, "planned context does not trigger terminal probing");
    let context_messages = requests[0]
        .transcript
        .iter()
        .filter(|message| message.metadata.contains_key("context_source"))
        .collect::<Vec<_>>();
    assert_eq!(context_messages.len(), 3);
    assert_eq!(context_messages[0].metadata["context_source"], "dirty_buffer");
    assert_eq!(context_messages[1].metadata["context_source"], "diagnostics");
    assert_eq!(context_messages[2].metadata["context_source"], "terminal_output");
    assert!(context_messages.iter().all(|message| message.trust.is_untrusted()));
}

#[tokio::test]
async fn strategic_turn_plan_update_precedes_tool_updates() {
    let model = Arc::new(FakeModel::new(vec![
        ModelResponse::new().tool_intents(vec![ToolIntent::new(
            "tc-1",
            "read_file",
            json!({ "path": "/tmp/x" }),
        )]),
        ModelResponse::new().text("read it").completed(),
    ]));
    let runtime = OrchestratorRuntime::new(OrchestratorConfig::default(), model);
    runtime.register_tool(read_file_tool()).expect("registers read_file");
    let (sink, client, mut rx) = plumbing();
    let events = EventRecorder::new();
    let (_cancel_tx, cancel_rx) = watch::channel(false);

    let turn = runtime
        .run_turn_strategic_recording(
            prompt("read the file"),
            StrategicInput::default(),
            sink,
            client,
            cancel_rx,
            events,
        )
        .await
        .expect("strategic turn succeeds");
    assert_eq!(turn.strategy.strategy, crate::strategy::TurnStrategy::ToolLoop);
    // The plan update lands before any tool lifecycle update.
    assert!(matches!(next_update(&mut rx).await, SessionUpdate::Plan(_)));
    assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCall(_)));
    assert_eq!(turn.final_response.changed_file_count(), 0);
}

#[tokio::test]
async fn strategic_turn_builds_final_response_from_observed_writes() {
    let model = Arc::new(FakeModel::new(vec![
        ModelResponse::new().tool_intents(vec![ToolIntent::new(
            "tc-1",
            "write_file",
            json!({ "path": "/tmp/out.rs", "content": "fn main() {}" }),
        )]),
        ModelResponse::new().text("done").completed(),
    ]));
    let policy = crate::policy::PolicyEngine::new(crate::policy::ToolPolicy {
        allow_write: true,
        ..crate::policy::ToolPolicy::default()
    });
    let runtime = OrchestratorRuntime::with_policy(OrchestratorConfig::default(), model, policy);
    runtime
        .register_tool(Arc::new(FakeTool::new(
            crate::tools::ToolDefinition::new("write_file", "writes a file")
                .side_effect_class(crate::tools::SideEffectClass::Write),
            crate::tools::ToolResult::success("file written"),
        )))
        .expect("registers write_file");
    let (sink, client, _rx) = plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);

    let turn = runtime
        .run_turn_strategic(
            prompt("read the file"),
            StrategicInput::default(),
            sink,
            client,
            cancel_rx,
        )
        .await
        .expect("strategic turn succeeds");
    assert_eq!(turn.final_response.changed_file_count(), 1);
    assert_eq!(turn.final_response.changed_files[0].path, "/tmp/out.rs");
    assert!(turn.final_response.summary.contains("changed files: /tmp/out.rs"));
    assert!(turn.final_response.provenance.contains(&"change:/tmp/out.rs:task-1".to_string()));
}

#[tokio::test]
async fn strategic_turn_respects_cancellation() {
    let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("x").completed()]));
    let runtime = OrchestratorRuntime::new(OrchestratorConfig::default(), model.clone());
    let (sink, client, _rx) = plumbing();
    let events = EventRecorder::new();
    let (_cancel_tx, cancel_rx) = watch::channel(true);

    let error = runtime
        .run_turn_strategic_recording(
            prompt("hello world"),
            StrategicInput::default(),
            sink,
            client,
            cancel_rx,
            events.clone(),
        )
        .await
        .expect_err("pre-cancelled strategic turn stops");
    assert_eq!(error, OrchestratorError::Cancellation);
    assert_eq!(model.call_count(), 0);
    // The decision was still recorded before execution was blocked.
    assert!(matches!(events.events()[0], OrchestratorEvent::StrategySelected { .. }));
}
