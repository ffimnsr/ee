//! Subagent tests: quarantine.
use super::*;

#[tokio::test]
async fn failed_child_output_is_quarantined_not_merged() {
    // The child loop rejects subagent intents — a provider error that
    // fails the child.  Failed output must be quarantined by default and
    // never reach parent memory.
    let model = FakeModel::new(vec![
        ModelResponse::new().subagent_intents(vec![SubagentIntent::new("break")]),
    ]);
    let harness = manager_harness(OrchestratorConfig::default(), Arc::new(model.clone()));
    let root = harness.tasks.lock().expect("task graph poisoned").create_root("parent", "p");
    let request = DelegationRequest {
        parent_task_id: root.id.clone(),
        role: SubagentRole::new("researcher", "research"),
        scoped_prompt: "do it".into(),
        context_snapshot: Vec::new(),
        scope: None,
        model_id: None,
    };
    let (tx, _rx) = mpsc::unbounded_channel();
    let client = ClientBridge::new_for_test(Duration::from_secs(5), tx);
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let result = harness
        .manager
        .spawn(request, client, cancel_rx, EventRecorder::new())
        .await
        .expect("spawn succeeds");
    assert_eq!(result.handoff.status, SubagentStatus::Failed);
    assert!(
        harness._memory.lock().expect("memory store poisoned").is_empty(),
        "failed child memory never merges"
    );
    assert!(
        harness.manager.quarantine_snapshot().is_quarantined(&result.subagent_id),
        "failed child output is quarantined by default"
    );
}

#[tokio::test]
async fn malformed_generic_handoff_is_failed_and_quarantined() {
    let model = FakeModel::new(vec![
        ModelResponse::new().text("not JSON [file:/work/private.rs]").completed(),
    ]);
    let harness = manager_harness(OrchestratorConfig::default(), Arc::new(model));
    let root = harness.tasks.lock().expect("task graph poisoned").create_root("parent", "p");
    let request = DelegationRequest {
        parent_task_id: root.id,
        role: BuiltinSubagentRole::Summarizer.role(),
        scoped_prompt: "summarize".into(),
        context_snapshot: Vec::new(),
        scope: None,
        model_id: None,
    };
    let result = harness
        .manager
        .spawn(request, bridge(), watch::channel(false).1, EventRecorder::new())
        .await
        .expect("spawn completes with rejected handoff");

    assert_eq!(result.handoff.status, SubagentStatus::Failed);
    assert_eq!(result.handoff.output_format, HandoffOutputFormat::RejectedMalformed);
    assert!(result.handoff.summary.is_empty(), "raw output stays quarantined");
    assert!(result.error_summary.as_deref().is_some_and(|error| error.contains("malformed")));
    assert!(harness.manager.quarantine_snapshot().is_quarantined(&result.subagent_id));
    assert!(harness._memory.lock().expect("memory store poisoned").is_empty());
}

#[tokio::test]
async fn malformed_rubber_duck_report_is_failed_and_quarantined() {
    let model = FakeModel::new(vec![ModelResponse::new().text("not JSON").completed()]);
    let harness = manager_harness(OrchestratorConfig::default(), Arc::new(model));
    let root = harness.tasks.lock().expect("task graph poisoned").create_root("parent", "p");
    let request = DelegationRequest {
        parent_task_id: root.id,
        role: BuiltinSubagentRole::RubberDuck.role(),
        scoped_prompt: "review".into(),
        context_snapshot: Vec::new(),
        scope: None,
        model_id: None,
    };
    let result = harness
        .manager
        .spawn(request, bridge(), watch::channel(false).1, EventRecorder::new())
        .await
        .expect("spawn completes with failed report");
    assert_eq!(result.handoff.status, SubagentStatus::Failed);
    assert!(
        result
            .error_summary
            .as_deref()
            .expect("rejection reason")
            .contains("malformed critique JSON")
    );
    assert!(harness.manager.quarantine_snapshot().is_quarantined(&result.subagent_id));
    assert!(harness._memory.lock().expect("memory store poisoned").is_empty());
}

#[tokio::test]
async fn subagent_depth_limit_is_enforced_before_spawn() {
    let model = FakeModel::new(Vec::new());
    let harness = manager_harness(OrchestratorConfig::default(), Arc::new(model.clone()));
    let (_root, _child, grandchild) = {
        let mut graph = harness.tasks.lock().expect("task graph poisoned");
        let root = graph.create_root("root", "r");
        let child = graph.create_child(&root.id, "c1", "d").expect("child");
        let grandchild = graph.create_child(&child.id, "c2", "d").expect("child");
        (root, child, grandchild)
    };

    let error = harness
        .manager
        .spawn(
            DelegationRequest {
                parent_task_id: grandchild.id.clone(),
                role: SubagentRole::new("worker", "instructions"),
                scoped_prompt: "prompt".into(),
                context_snapshot: Vec::new(),
                scope: None,
                model_id: None,
            },
            bridge(),
            watch::channel(false).1,
            EventRecorder::new(),
        )
        .await
        .expect_err("depth limit");
    assert!(
        matches!(error, OrchestratorError::InvalidState(ref message) if message.contains("depth limit"))
    );
    assert_eq!(model.call_count(), 0, "no child may run");
    assert_eq!(harness.tasks.lock().expect("task graph poisoned").len(), 3, "no new node created");
}

#[tokio::test]
async fn subagent_parallel_limit_bounds_concurrency() {
    let probe_state: Arc<Mutex<(usize, usize)>> = Arc::new(Mutex::new((0, 0)));
    let model = Arc::new(ConcurrencyProbe { active: probe_state.clone() });
    let config = OrchestratorConfig { max_parallel_subagents: 2, ..OrchestratorConfig::default() };
    let harness = manager_harness(config, model);
    let root = harness.tasks.lock().expect("task graph poisoned").create_root("root", "r");
    let events = EventRecorder::new();
    let manager = harness.manager;

    let mut handles = Vec::new();
    for index in 0..4 {
        let manager = manager.clone();
        let root_id = root.id.clone();
        let role = BuiltinSubagentRole::Summarizer.role();
        let events = events.clone();
        handles.push(tokio::spawn(async move {
            manager
                .spawn(
                    DelegationRequest {
                        parent_task_id: root_id,
                        role,
                        scoped_prompt: format!("task {index}"),
                        context_snapshot: Vec::new(),
                        scope: None,
                        model_id: None,
                    },
                    bridge(),
                    watch::channel(false).1,
                    events,
                )
                .await
                .expect("spawn succeeds")
        }));
    }
    let results: Vec<SubagentResult> = futures::future::join_all(handles)
        .await
        .into_iter()
        .collect::<Result<_, _>>()
        .expect("spawn tasks complete");
    assert_eq!(results.len(), 4);
    assert!(results.iter().all(|result| result.handoff.status == SubagentStatus::Completed));

    let (current, max) = *probe_state.lock().expect("probe state poisoned");
    assert_eq!(current, 0, "all probes finished");
    assert_eq!(max, 2, "at most max_parallel_subagents children ran concurrently");
    assert_eq!(
        harness.tasks.lock().expect("task graph poisoned").len(),
        5,
        "root plus four children"
    );
}
