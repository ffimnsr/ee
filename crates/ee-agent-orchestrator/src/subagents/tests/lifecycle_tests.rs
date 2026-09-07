//! Subagent tests: lifecycle.
use super::*;

#[tokio::test]
async fn parent_cancellation_cancels_children() {
    let model = FakeModel::new(Vec::new());
    let harness = manager_harness(OrchestratorConfig::default(), Arc::new(model.clone()));
    let manager = harness.manager;
    let tasks = harness.tasks;
    let root = tasks.lock().expect("task graph poisoned").create_root("root", "r");
    let events = EventRecorder::new();

    let (cancel_tx, cancel_rx) = watch::channel(false);
    let handle = tokio::spawn({
        let manager = manager.clone();
        let root_id = root.id.clone();
        let events = events.clone();
        async move {
            manager
                .spawn(
                    DelegationRequest {
                        parent_task_id: root_id,
                        role: SubagentRole::new("worker", "instructions"),
                        scoped_prompt: "prompt".into(),
                        context_snapshot: Vec::new(),
                        scope: None,
                        model_id: None,
                    },
                    bridge(),
                    cancel_rx,
                    events,
                )
                .await
                .expect("spawn succeeds")
        }
    });
    cancel_tx.send(true).expect("cancel receiver alive");

    let result = handle.await.expect("spawn task completes");
    assert_eq!(result.handoff.status, SubagentStatus::Cancelled);
    assert_eq!(model.call_count(), 0, "child never ran");

    let graph = tasks.lock().expect("task graph poisoned");
    let child = graph
        .list()
        .into_iter()
        .find(|node| node.parent == Some(root.id.clone()))
        .expect("child task node exists");
    assert_eq!(child.status, TaskStatus::Cancelled);
    drop(graph);

    let recorded = events.events();
    assert!(
        recorded.iter().any(|event| matches!(event, OrchestratorEvent::SubagentStarted { .. }))
    );
    assert!(recorded.iter().any(|event| {
        matches!(event, OrchestratorEvent::SubagentFinished { success: false, .. })
    }));
}

#[tokio::test]
async fn subagent_budget_denies_spawn_beyond_limit() {
    let model = FakeModel::new(vec![
        ModelResponse::new().text(handoff_output("first complete")).completed(),
    ]);
    let config = OrchestratorConfig { max_subagents: 1, ..OrchestratorConfig::default() };
    let harness = manager_harness(config, Arc::new(model.clone()));
    let manager = harness.manager;
    let tasks = harness.tasks;
    let budget = harness.budget;
    let root = tasks.lock().expect("task graph poisoned").create_root("root", "r");
    let events = EventRecorder::new();

    let first = manager
        .spawn(
            DelegationRequest {
                parent_task_id: root.id.clone(),
                role: BuiltinSubagentRole::Summarizer.role(),
                scoped_prompt: "one".into(),
                context_snapshot: Vec::new(),
                scope: None,
                model_id: None,
            },
            bridge(),
            watch::channel(false).1,
            events.clone(),
        )
        .await
        .expect("first spawn is within budget");
    assert_eq!(first.handoff.status, SubagentStatus::Completed);

    let second = manager
        .spawn(
            DelegationRequest {
                parent_task_id: root.id,
                role: BuiltinSubagentRole::Summarizer.role(),
                scoped_prompt: "two".into(),
                context_snapshot: Vec::new(),
                scope: None,
                model_id: None,
            },
            bridge(),
            watch::channel(false).1,
            events.clone(),
        )
        .await;
    assert!(
        matches!(second, Err(OrchestratorError::BudgetExceeded(ref r)) if r.contains("max subagents")),
        "second spawn denied before starting: {second:?}"
    );
    assert_eq!(
        budget.lock().expect("budget tracker poisoned").snapshot().subagents_used,
        1,
        "only the allowed spawn consumed budget"
    );
    assert_eq!(tasks.lock().expect("task graph poisoned").len(), 2, "root plus one child");
    let recorded = events.events();
    assert_eq!(
        recorded
            .iter()
            .filter(|event| matches!(event, OrchestratorEvent::SubagentStarted { .. }))
            .count(),
        1,
        "denied spawn was never started"
    );
    assert!(
        recorded.iter().any(|event| matches!(
            event,
            OrchestratorEvent::BudgetUpdated { subagents_used: 1, .. }
        ))
    );
}

#[tokio::test(start_paused = true)]
async fn total_timeout_includes_waiting_for_subagent_permit() {
    let calls = Arc::new(Mutex::new(0usize));
    let model = Arc::new(CancelAwaitingModel { calls: calls.clone() });
    let config = OrchestratorConfig {
        max_parallel_subagents: 0,
        subagent_timeout: Duration::from_secs(10),
        subagent_stall_timeout: Duration::from_secs(60),
        ..OrchestratorConfig::default()
    };
    let harness = manager_harness(config, model);
    let root = harness.tasks.lock().expect("task graph poisoned").create_root("root", "r");
    let handle = tokio::spawn({
        let manager = harness.manager.clone();
        async move {
            manager
                .spawn(
                    DelegationRequest {
                        parent_task_id: root.id,
                        role: SubagentRole::new("worker", "instructions"),
                        scoped_prompt: "queued".into(),
                        context_snapshot: Vec::new(),
                        scope: None,
                        model_id: None,
                    },
                    bridge(),
                    watch::channel(false).1,
                    EventRecorder::new(),
                )
                .await
                .expect("supervised spawn returns result")
        }
    });
    wait_until(|| harness.children.snapshot(1).total == 1).await;

    tokio::time::advance(Duration::from_secs(11)).await;
    let result = handle.await.expect("spawn task completes");

    assert_eq!(result.handoff.status, SubagentStatus::Failed);
    assert!(
        result
            .error_summary
            .as_deref()
            .is_some_and(|error| error.contains("while waiting for a permit"))
    );
    assert_eq!(*calls.lock().expect("calls poisoned"), 0, "queued child never reached model");
    assert!(harness.manager.quarantine_snapshot().is_quarantined(&result.subagent_id));
    assert_eq!(harness.children.snapshot(1).children[0].state, ChildState::Failed);
}

#[tokio::test]
async fn cancellation_during_subagent_run_cancels_child_task() {
    let calls = Arc::new(Mutex::new(0usize));
    let model = Arc::new(CancelAwaitingModel { calls: calls.clone() });
    let harness = manager_harness(OrchestratorConfig::default(), model);
    let manager = harness.manager;
    let tasks = harness.tasks;
    let root = tasks.lock().expect("task graph poisoned").create_root("root", "r");
    let events = EventRecorder::new();
    let (cancel_tx, cancel_rx) = watch::channel(false);

    let handle = tokio::spawn({
        let manager = manager.clone();
        let root_id = root.id.clone();
        let events = events.clone();
        async move {
            manager
                .spawn(
                    DelegationRequest {
                        parent_task_id: root_id,
                        role: SubagentRole::new("worker", "instructions"),
                        scoped_prompt: "prompt".into(),
                        context_snapshot: Vec::new(),
                        scope: None,
                        model_id: None,
                    },
                    bridge(),
                    cancel_rx,
                    events,
                )
                .await
                .expect("spawn succeeds")
        }
    });

    // Wait until the child's model call is in flight, then cancel the
    // parent turn; the child must observe the token and stop.
    wait_until(|| *calls.lock().expect("calls poisoned") == 1).await;
    cancel_tx.send(true).expect("cancel receiver alive");

    let result = handle.await.expect("spawn task completes");
    assert_eq!(result.handoff.status, SubagentStatus::Cancelled);
    assert_eq!(*calls.lock().expect("calls poisoned"), 1, "child never called again");

    let graph = tasks.lock().expect("task graph poisoned");
    let child = graph
        .list()
        .into_iter()
        .find(|node| node.parent.as_ref() == Some(&root.id))
        .expect("child task node exists");
    assert_eq!(child.status, TaskStatus::Cancelled, "child task cleaned up");
    drop(graph);
}

#[tokio::test]
async fn targeted_cancellation_leaves_sibling_running() {
    let calls = Arc::new(Mutex::new(0usize));
    let model = Arc::new(CancelAwaitingModel { calls: calls.clone() });
    let harness = manager_harness(OrchestratorConfig::default(), model);
    let root = harness.tasks.lock().expect("task graph poisoned").create_root("root", "r");
    let spawn_child = |prompt: &'static str| {
        let manager = harness.manager.clone();
        let root_id = root.id.clone();
        tokio::spawn(async move {
            manager
                .spawn(
                    DelegationRequest {
                        parent_task_id: root_id,
                        role: SubagentRole::new("worker", "instructions"),
                        scoped_prompt: prompt.into(),
                        context_snapshot: Vec::new(),
                        scope: None,
                        model_id: None,
                    },
                    bridge(),
                    watch::channel(false).1,
                    EventRecorder::new(),
                )
                .await
                .expect("spawn succeeds")
        })
    };

    let first = spawn_child("first");
    wait_until(|| *calls.lock().expect("calls poisoned") == 1).await;
    let second = spawn_child("second");
    wait_until(|| *calls.lock().expect("calls poisoned") == 2).await;

    assert_eq!(
        harness.children.cancel(&SubagentId::new("task-2")),
        crate::child_registry::ChildCancelResult::Requested
    );
    assert_eq!(first.await.expect("first task").handoff.status, SubagentStatus::Cancelled);
    assert!(!second.is_finished(), "targeted cancellation must not affect sibling");

    assert_eq!(
        harness.children.cancel(&SubagentId::new("task-3")),
        crate::child_registry::ChildCancelResult::Requested
    );
    assert_eq!(second.await.expect("second task").handoff.status, SubagentStatus::Cancelled);
}

#[tokio::test]
async fn silent_child_is_cancelled_and_quarantined_as_stalled() {
    let calls = Arc::new(Mutex::new(0usize));
    let model = Arc::new(CancelAwaitingModel { calls: calls.clone() });
    let config = OrchestratorConfig {
        subagent_stall_timeout: Duration::from_millis(10),
        subagent_timeout: Duration::from_secs(5),
        ..OrchestratorConfig::default()
    };
    let harness = manager_harness(config, model);
    let root = harness.tasks.lock().expect("task graph poisoned").create_root("root", "r");
    let result = harness
        .manager
        .spawn(
            DelegationRequest {
                parent_task_id: root.id,
                role: SubagentRole::new("worker", "instructions"),
                scoped_prompt: "silent".into(),
                context_snapshot: Vec::new(),
                scope: None,
                model_id: None,
            },
            bridge(),
            watch::channel(false).1,
            EventRecorder::new(),
        )
        .await
        .expect("supervised spawn returns result");

    assert_eq!(result.handoff.status, SubagentStatus::Cancelled);
    assert!(result.error_summary.as_deref().is_some_and(|error| error.contains("stalled")));
    assert!(harness.manager.quarantine_snapshot().is_quarantined(&result.subagent_id));
    let snapshot = harness.children.snapshot(8);
    assert_eq!(snapshot.children[0].state, ChildState::Stalled);
}

#[tokio::test]
async fn dropped_spawn_future_cleans_task_and_registry() {
    let calls = Arc::new(Mutex::new(0usize));
    let model = Arc::new(CancelAwaitingModel { calls: calls.clone() });
    let harness = manager_harness(OrchestratorConfig::default(), model);
    let root = harness.tasks.lock().expect("task graph poisoned").create_root("root", "r");
    let handle = tokio::spawn({
        let manager = harness.manager.clone();
        async move {
            manager
                .spawn(
                    DelegationRequest {
                        parent_task_id: root.id,
                        role: SubagentRole::new("worker", "instructions"),
                        scoped_prompt: "drop".into(),
                        context_snapshot: Vec::new(),
                        scope: None,
                        model_id: None,
                    },
                    bridge(),
                    watch::channel(false).1,
                    EventRecorder::new(),
                )
                .await
        }
    });
    wait_until(|| *calls.lock().expect("calls poisoned") == 1).await;
    handle.abort();
    let _ = handle.await;

    let snapshot = harness.children.snapshot(8);
    assert_eq!(snapshot.children[0].state, ChildState::Cancelled);
    let graph = harness.tasks.lock().expect("task graph poisoned");
    assert!(graph.list().iter().any(|task| task.status == TaskStatus::Cancelled));
}
