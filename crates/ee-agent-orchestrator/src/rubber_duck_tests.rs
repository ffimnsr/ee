use std::future::pending;

use crate::config::OrchestratorConfig;
use crate::delegation_quality::FindingConfidence;
use crate::model::{ModelAdapter, ModelError, ModelFuture, ModelResponse};
use crate::model_registry::{DEFAULT_MODEL_ID, ModelFamily, ModelIdentity, ModelRegistration};
use crate::model_router::ModelTier;
use crate::test_support::{FakeModel, FakeTool};
use crate::tools::{SideEffectClass, ToolIntent, ToolResult};

use super::*;

struct Harness {
    runner: RubberDuckRunner,
    parent: TaskId,
    root: FakeModel,
    critic: FakeModel,
}

fn identity(
    model_id: &str,
    family: ModelFamily,
    capabilities: impl IntoIterator<Item = ModelCapability>,
) -> ModelIdentity {
    ModelIdentity::new(model_id, "test", family, model_id, capabilities).expect("identity")
}

fn report(target: CritiqueTarget, findings: Vec<CritiqueFinding>) -> String {
    serde_json::to_string(&crate::critique::CritiqueReport {
        schema_version: CRITIQUE_REPORT_SCHEMA_VERSION,
        target,
        findings,
    })
    .expect("report")
}

fn finding() -> CritiqueFinding {
    CritiqueFinding {
        key: "missing-test".into(),
        severity: CritiqueSeverity::Blocking,
        issue: "failure path lacks coverage".into(),
        impact: "regression may escape validation".into(),
        recommended_change: "add focused failure-path test".into(),
        confidence: FindingConfidence::High,
        evidence: vec![FindingEvidence::File("src/lib.rs".into())],
    }
}

fn harness(response: ModelResponse, mut config: OrchestratorConfig) -> Harness {
    config.max_subagents = config.max_subagents.max(1);
    config.max_model_calls = config.max_model_calls.max(2);
    let root = FakeModel::new(vec![]);
    let critic = FakeModel::new(vec![response]);
    let mut models = ModelRegistry::new();
    models
        .register_model(
            DEFAULT_MODEL_ID,
            Arc::new(root.clone()),
            ModelRegistration::new(identity(
                "root-model",
                ModelFamily::OpenAi,
                [ModelCapability::ChatCompletion, ModelCapability::Tools],
            )),
        )
        .expect("root model");
    models
        .register_model(
            "critic",
            Arc::new(critic.clone()),
            ModelRegistration::new(identity(
                "critic-model",
                ModelFamily::Anthropic,
                [ModelCapability::ChatCompletion, ModelCapability::Tools],
            ))
            .for_roles(&[RUBBER_DUCK_ROLE])
            .tier(ModelTier::Strong),
        )
        .expect("critic model");
    let tasks = Arc::new(Mutex::new(TaskGraph::new()));
    let parent = tasks.lock().expect("tasks").create_root("root", "root work").id;
    let tools = Arc::new(Mutex::new(ToolRegistry::new()));
    let budget = Arc::new(Mutex::new(BudgetTracker::new(&config)));
    let runner = RubberDuckRunner::with_config(
        Arc::new(models),
        tools,
        tasks,
        budget,
        EventRecorder::new(),
        config.rubber_duck.clone(),
    )
    .expect("rubber-duck config");
    Harness { runner, parent, root, critic }
}

fn request(parent: TaskId, target: CritiqueTarget) -> RubberDuckRequest {
    RubberDuckRequest {
        session_id: "session-1".into(),
        parent_task_id: parent,
        target,
        user_goal: "implement safe change".into(),
        active_task_or_plan: "edit implementation then validate".into(),
        active_model_id: DEFAULT_MODEL_ID.into(),
        user_question: None,
        revision: "rev-1".into(),
        observed_context: ReviewContext {
            changed_files: vec!["src/lib.rs".into()],
            revision: Some("rev-1".into()),
            ..ReviewContext::default()
        },
        observed_evidence: ReportEvidence {
            files: ["src/lib.rs".into()].into_iter().collect(),
            tools: BTreeSet::new(),
        },
        automatic: false,
    }
}

#[test]
fn session_call_accounting_fails_closed_at_capacity_without_evicting_quotas() {
    let mut calls = BTreeMap::new();
    for index in 0..MAX_RUBBER_DUCK_SESSION_COUNTERS {
        calls.insert(format!("session-{index:03}"), 1);
    }
    assert!(matches!(
        reserve_session_call(&mut calls, "new-session", 2),
        Err(RubberDuckUnavailable::CallAccountingCapacityReached { max_sessions })
            if max_sessions == MAX_RUBBER_DUCK_SESSION_COUNTERS
    ));
    assert_eq!(calls.len(), MAX_RUBBER_DUCK_SESSION_COUNTERS);
    assert!(matches!(
        reserve_session_call(&mut calls, "session-000", 1),
        Err(RubberDuckUnavailable::CallLimitReached { max_calls: 1 })
    ));
}

#[test]
fn unavailable_and_resolution_states_roundtrip() {
    let unavailable = RubberDuckUnavailable::CallLimitReached { max_calls: 2 };
    let json = serde_json::to_string(&unavailable).unwrap();
    assert_eq!(serde_json::from_str::<RubberDuckUnavailable>(&json).unwrap(), unavailable);

    let resolutions = [
        FindingResolution::Accepted { task_id: None },
        FindingResolution::Rejected {
            reason: "not reproduced".into(),
            evidence: vec![FindingEvidence::Tool("cargo test".into())],
        },
        FindingResolution::Deferred { reason: "follow-up".into() },
    ];
    for resolution in resolutions {
        let json = serde_json::to_string(&resolution).unwrap();
        assert_eq!(serde_json::from_str::<FindingResolution>(&json).unwrap(), resolution);
    }
}

#[tokio::test]
async fn uses_contrasting_adapter_and_injects_only_verified_evidence() {
    let response = ModelResponse::new()
        .text(report(CritiqueTarget::Implementation, vec![finding()]))
        .reasoning("hidden chain of thought")
        .completed();
    let harness = harness(response, OrchestratorConfig::default());
    let mut transcript = Transcript::new();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let outcome = harness
        .runner
        .run(
            request(harness.parent.clone(), CritiqueTarget::Implementation),
            &mut transcript,
            cancel_rx,
        )
        .await;
    let RubberDuckOutcome::Completed(completed) = outcome else {
        panic!("expected completed critique");
    };
    assert_eq!(harness.root.call_count(), 0);
    assert_eq!(harness.critic.call_count(), 1);
    assert_ne!(completed.active_model.identity.family, completed.critic_model.identity.family);
    assert_eq!(completed.critic_model.id, "critic");
    assert_eq!(transcript.messages.len(), 1);
    assert_eq!(transcript.messages[0].role, ModelRole::Subagent);
    assert_eq!(transcript.messages[0].reasoning_summary, None);
    assert!(!transcript.messages[0].text_content().contains("chain of thought"));
    assert_eq!(transcript.messages[0].metadata["active_model_id"], DEFAULT_MODEL_ID);
    assert_eq!(transcript.messages[0].metadata["critic_model_id"], "critic");
    assert!(matches!(
        harness.runner.critic_events().as_slice(),
        [CriticEvent::Started { .. }, CriticEvent::Completed { .. }]
    ));
}

#[tokio::test]
async fn config_off_call_limit_and_stale_revision_fail_closed() {
    let response =
        ModelResponse::new().text(report(CritiqueTarget::Implementation, vec![])).completed();
    let mut config = OrchestratorConfig::default();
    config.rubber_duck.mode = RubberDuckMode::Off;
    let disabled = harness(response.clone(), config);
    let (_tx, rx) = watch::channel(false);
    assert!(matches!(
        disabled
            .runner
            .run(
                request(disabled.parent.clone(), CritiqueTarget::Implementation),
                &mut Transcript::new(),
                rx,
            )
            .await,
        RubberDuckOutcome::Unavailable(RubberDuckUnavailable::Disabled)
    ));
    assert_eq!(disabled.critic.call_count(), 0);

    let mut config = OrchestratorConfig::default();
    config.rubber_duck.max_calls = 1;
    let limited = harness(response, config);
    let (_tx, rx) = watch::channel(false);
    assert!(matches!(
        limited
            .runner
            .run(
                request(limited.parent.clone(), CritiqueTarget::Implementation),
                &mut Transcript::new(),
                rx,
            )
            .await,
        RubberDuckOutcome::Completed(_)
    ));
    limited
        .runner
        .invalidate(&ContextInvalidation::DiagnosticsRevision { session_id: "session-1".into() });
    let (_tx, rx) = watch::channel(false);
    assert!(matches!(
        limited
            .runner
            .run(
                request(limited.parent.clone(), CritiqueTarget::Implementation),
                &mut Transcript::new(),
                rx,
            )
            .await,
        RubberDuckOutcome::Unavailable(RubberDuckUnavailable::CallLimitReached { max_calls: 1 })
    ));

    let mut stale = request(limited.parent, CritiqueTarget::Implementation);
    stale.observed_context.revision = Some("rev-0".into());
    assert!(validate_request(&stale, MAX_REVIEW_CONTEXT_BYTES).is_err());
}

#[tokio::test]
async fn discovery_is_read_only_and_tool_intents_are_quarantined_without_dispatch() {
    let response = ModelResponse::new().tool_intents(vec![ToolIntent::new(
        "call-1",
        "write_file",
        serde_json::json!({}),
    )]);
    let harness = harness(response, OrchestratorConfig::default());
    let read = FakeTool::new(ToolDefinition::new("read_file", "read"), ToolResult::success("ok"));
    let mut write_definition = ToolDefinition::new("write_file", "write");
    write_definition.side_effect_class = SideEffectClass::Write;
    let write = FakeTool::new(write_definition, ToolResult::success("bad"));
    harness.runner.tools.lock().expect("tools").register(Arc::new(read.clone())).expect("read");
    harness.runner.tools.lock().expect("tools").register(Arc::new(write.clone())).expect("write");
    let mut transcript = Transcript::new();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let outcome = harness
        .runner
        .run(
            request(harness.parent.clone(), CritiqueTarget::Implementation),
            &mut transcript,
            cancel_rx,
        )
        .await;
    assert!(matches!(outcome, RubberDuckOutcome::Quarantined { .. }));
    let requests = harness.critic.requests();
    assert_eq!(
        requests[0].tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(),
        vec!["read_file"]
    );
    assert_eq!(read.call_count(), 0);
    assert_eq!(write.call_count(), 0);
    assert!(transcript.messages.is_empty());
}

#[tokio::test]
async fn unavailable_contrast_and_budget_denial_create_no_child_task() {
    let config = OrchestratorConfig::default();
    let root = FakeModel::new(vec![]);
    let mut models = ModelRegistry::new();
    models
        .register_model(
            DEFAULT_MODEL_ID,
            Arc::new(root),
            ModelRegistration::new(identity(
                "root-only",
                ModelFamily::OpenAi,
                [ModelCapability::ChatCompletion, ModelCapability::Tools],
            )),
        )
        .expect("root");
    let tasks = Arc::new(Mutex::new(TaskGraph::new()));
    let parent = tasks.lock().expect("tasks").create_root("root", "work").id;
    let runner = RubberDuckRunner::new(
        Arc::new(models),
        Arc::new(Mutex::new(ToolRegistry::new())),
        tasks.clone(),
        Arc::new(Mutex::new(BudgetTracker::new(&config))),
        EventRecorder::new(),
    );
    let mut transcript = Transcript::new();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let outcome = runner
        .run(request(parent, CritiqueTarget::Implementation), &mut transcript, cancel_rx)
        .await;
    assert!(matches!(
        outcome,
        RubberDuckOutcome::Unavailable(RubberDuckUnavailable::Contrast(
            ContrastUnavailable::NoAlternative
        ))
    ));
    assert_eq!(tasks.lock().expect("tasks").len(), 1);

    let denied_config = OrchestratorConfig { max_subagents: 1, ..OrchestratorConfig::default() };
    let denied = harness(
        ModelResponse::new().text(report(CritiqueTarget::Implementation, vec![])),
        denied_config,
    );
    denied.runner.budget.lock().expect("budget").try_reserve_subagent().expect("first reservation");
    let before = denied.runner.tasks.lock().expect("tasks").len();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let outcome = denied
        .runner
        .run(
            request(denied.parent.clone(), CritiqueTarget::Implementation),
            &mut Transcript::new(),
            cancel_rx,
        )
        .await;
    assert!(matches!(
        outcome,
        RubberDuckOutcome::Unavailable(RubberDuckUnavailable::BudgetDenied { .. })
    ));
    assert_eq!(denied.runner.tasks.lock().expect("tasks").len(), before);
    assert_eq!(denied.critic.call_count(), 0);
}

#[derive(Clone)]
struct CancellingModel;

impl ModelAdapter for CancellingModel {
    fn complete(
        &self,
        _request: ModelRequest,
        mut cancel: watch::Receiver<bool>,
    ) -> ModelFuture<Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            if !*cancel.borrow() {
                let _ = cancel.changed().await;
            }
            Err(ModelError::Cancelled)
        })
    }
}

#[derive(Clone)]
struct NeverModel;

impl ModelAdapter for NeverModel {
    fn complete(
        &self,
        _request: ModelRequest,
        _cancel: watch::Receiver<bool>,
    ) -> ModelFuture<Result<ModelResponse, ModelError>> {
        Box::pin(pending())
    }
}

#[tokio::test]
async fn inherited_cancellation_stops_critic_without_transcript_merge() {
    let config = OrchestratorConfig::default();
    let root = FakeModel::new(vec![]);
    let mut models = ModelRegistry::new();
    models
        .register_model(
            DEFAULT_MODEL_ID,
            Arc::new(root),
            ModelRegistration::new(identity(
                "root-model",
                ModelFamily::OpenAi,
                [ModelCapability::ChatCompletion, ModelCapability::Tools],
            )),
        )
        .expect("root");
    models
        .register_model(
            "critic",
            Arc::new(CancellingModel),
            ModelRegistration::new(identity(
                "critic-model",
                ModelFamily::Anthropic,
                [ModelCapability::ChatCompletion, ModelCapability::Tools],
            )),
        )
        .expect("critic");
    let tasks = Arc::new(Mutex::new(TaskGraph::new()));
    let parent = tasks.lock().expect("tasks").create_root("root", "work").id;
    let runner = RubberDuckRunner::new(
        Arc::new(models),
        Arc::new(Mutex::new(ToolRegistry::new())),
        tasks,
        Arc::new(Mutex::new(BudgetTracker::new(&config))),
        EventRecorder::new(),
    );
    let mut transcript = Transcript::new();
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let outcome = {
        let run =
            runner.run(request(parent, CritiqueTarget::Implementation), &mut transcript, cancel_rx);
        tokio::pin!(run);
        tokio::select! {
            _ = &mut run => panic!("critic completed before cancellation"),
            _ = tokio::task::yield_now() => {}
        }
        cancel_tx.send(true).expect("cancel");
        run.as_mut().await
    };
    assert_eq!(outcome, RubberDuckOutcome::Cancelled);
    assert!(transcript.messages.is_empty());
}

#[tokio::test(start_paused = true)]
async fn dedicated_timeout_fails_without_transcript_merge() {
    let config = OrchestratorConfig::default();
    let root = FakeModel::new(vec![]);
    let mut models = ModelRegistry::new();
    models
        .register_model(
            DEFAULT_MODEL_ID,
            Arc::new(root),
            ModelRegistration::new(identity(
                "root-model",
                ModelFamily::OpenAi,
                [ModelCapability::ChatCompletion, ModelCapability::Tools],
            )),
        )
        .expect("root");
    models
        .register_model(
            "critic",
            Arc::new(NeverModel),
            ModelRegistration::new(identity(
                "critic-model",
                ModelFamily::Anthropic,
                [ModelCapability::ChatCompletion, ModelCapability::Tools],
            )),
        )
        .expect("critic");
    let tasks = Arc::new(Mutex::new(TaskGraph::new()));
    let parent = tasks.lock().expect("tasks").create_root("root", "work").id;
    let runner = RubberDuckRunner::new(
        Arc::new(models),
        Arc::new(Mutex::new(ToolRegistry::new())),
        tasks,
        Arc::new(Mutex::new(BudgetTracker::new(&config))),
        EventRecorder::new(),
    );
    let mut transcript = Transcript::new();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let outcome = {
        let run =
            runner.run(request(parent, CritiqueTarget::Implementation), &mut transcript, cancel_rx);
        tokio::pin!(run);
        tokio::select! {
            _ = &mut run => panic!("critic completed before timeout"),
            _ = tokio::task::yield_now() => {}
        }
        tokio::time::advance(RUBBER_DUCK_TIMEOUT + std::time::Duration::from_secs(1)).await;
        run.as_mut().await
    };
    assert_eq!(outcome, RubberDuckOutcome::Failed { reason: "rubber-duck timeout".into() });
    assert!(transcript.messages.is_empty());
}

#[tokio::test]
async fn cache_is_exact_and_invalidated_by_session_state_change() {
    let response =
        ModelResponse::new().text(report(CritiqueTarget::Implementation, vec![])).completed();
    let harness = harness(response, OrchestratorConfig::default());
    let mut transcript = Transcript::new();
    let (_tx, rx) = watch::channel(false);
    let first = harness
        .runner
        .run(request(harness.parent.clone(), CritiqueTarget::Implementation), &mut transcript, rx)
        .await;
    assert!(matches!(
        first,
        RubberDuckOutcome::Completed(completed) if !completed.cached
    ));
    let (_tx, rx) = watch::channel(false);
    let second = harness
        .runner
        .run(request(harness.parent.clone(), CritiqueTarget::Implementation), &mut transcript, rx)
        .await;
    assert!(matches!(
        second,
        RubberDuckOutcome::Completed(completed) if completed.cached
    ));
    assert_eq!(harness.critic.call_count(), 1);
    harness
        .runner
        .invalidate(&ContextInvalidation::DiagnosticsRevision { session_id: "session-1".into() });
    let (_tx, rx) = watch::channel(false);
    let third = harness
        .runner
        .run(request(harness.parent, CritiqueTarget::Implementation), &mut transcript, rx)
        .await;
    assert!(!matches!(
        third,
        RubberDuckOutcome::Completed(completed) if completed.cached
    ));
}

#[tokio::test]
async fn root_reconciliation_controls_tasks_and_blocking_completion() {
    let response = ModelResponse::new()
        .text(report(CritiqueTarget::Implementation, vec![finding()]))
        .completed();
    let harness = harness(response, OrchestratorConfig::default());
    let (_tx, rx) = watch::channel(false);
    let outcome = harness
        .runner
        .run(
            request(harness.parent.clone(), CritiqueTarget::Implementation),
            &mut Transcript::new(),
            rx,
        )
        .await;
    assert!(matches!(outcome, RubberDuckOutcome::Completed(_)));
    let mut completion = CompletionReport {
        state: CompletionState::Verified,
        blocker: None,
        safe_follow_up: None,
        evidence_ids: vec!["host-validation".into()],
    };
    harness.runner.constrain_completion("rev-1", &mut completion);
    assert_eq!(completion.state, CompletionState::Blocked);
    assert_eq!(completion.evidence_ids, vec!["host-validation"]);

    let created = harness
        .runner
        .reconcile(
            &harness.parent,
            "session-1",
            "rev-1",
            &CritiqueTarget::Implementation,
            &[FindingDecision {
                key: "missing-test".into(),
                resolution: FindingResolution::Accepted { task_id: None },
            }],
            &ReportEvidence::default(),
        )
        .expect("root accepts finding");
    assert_eq!(created.len(), 1);
    let duplicate = harness
        .runner
        .reconcile(
            &harness.parent,
            "session-1",
            "rev-1",
            &CritiqueTarget::Implementation,
            &[FindingDecision {
                key: "missing-test".into(),
                resolution: FindingResolution::Accepted { task_id: None },
            }],
            &ReportEvidence::default(),
        )
        .expect("idempotent");
    assert!(duplicate.is_empty());
    {
        let mut tasks = harness.runner.tasks.lock().expect("tasks");
        tasks.transition(&created[0], TaskStatus::Running).expect("starts");
        tasks.transition(&created[0], TaskStatus::Completed).expect("completes");
    }
    let mut completion = CompletionReport {
        state: CompletionState::Verified,
        blocker: None,
        safe_follow_up: None,
        evidence_ids: vec!["host-validation".into()],
    };
    harness.runner.constrain_completion("rev-1", &mut completion);
    assert_eq!(completion.state, CompletionState::Verified);
}
