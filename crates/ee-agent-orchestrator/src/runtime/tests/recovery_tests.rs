//! Runtime tests: recovery.
use super::*;

#[tokio::test]
async fn repair_uses_fresh_host_context_and_stops_before_second_model_call_on_stale_revision() {
    let model =
        Arc::new(FakeModel::new(vec![ModelResponse::new().text("repair done").completed()]));
    let policy = PolicyEngine::new(ToolPolicy { allow_execute: true, ..ToolPolicy::default() });
    let runtime =
        OrchestratorRuntime::with_policy(OrchestratorConfig::default(), model.clone(), policy);
    for tool in [
        repair_context_tool("ee_project_instructions", json!({"sources": []})),
        repair_context_tool("ee_open_buffers", json!({"buffers": [{"revisionId": "rev-1"}]})),
        repair_context_tool("ee_get_diagnostics", json!({"diagnostics": [], "truncated": false})),
        repair_context_tool(
            "ee_changed_files",
            json!({"files": [{"path": "/work/src/lib.rs", "conflicted": false}], "truncated": false}),
        ),
        repair_context_tool(
            "ee_git_diff",
            json!({"diff": "diff", "bytesReturned": 4, "truncated": false}),
        ),
        repair_context_tool(
            "ee_review_context",
            json!({"diagnostics": {"truncated": false}, "changedFiles": {"truncated": false}}),
        ),
    ] {
        runtime.register_tool(tool).expect("register repair host tool");
    }
    runtime
        .register_tool(Arc::new(FakeTool::new(
            ToolDefinition::new("cargo_check", "fails validation")
                .side_effect_class(SideEffectClass::Execute),
            ToolResult::failure(crate::tools::ToolErrorKind::Backend, "compile failed"),
        )))
        .expect("register validation tool");
    let task = {
        let mut graph = runtime.tasks.lock().expect("task graph poisoned");
        graph.create_root("repair", "repair")
    };
    let mut records = ValidationRecorder::new();
    records.record_evidence(crate::final_response::ValidationRecord::evidence(
        "validation-cargo_check-task-1",
        "cargo check",
        ValidationOutcome::Failed,
        Some("cargo_check".into()),
        Some(1),
        Some(1),
        Vec::new(),
        0,
        false,
        None,
        None,
        false,
        false,
        Some("failed".into()),
    ));
    *runtime.last_turn_validation.lock().expect("validation poisoned") = records;
    let validation = PostWriteValidation {
        results: vec![ValidationResult {
            command_id: "cargo_check".into(),
            command: "cargo check".into(),
            status: ValidationOutcome::Failed,
            failure: Some(ValidationCommandFailure::CommandFailed),
            exit_status: Some(1),
            elapsed_ms: 1,
            test_ids: Vec::new(),
            diagnostics_delta: 0,
            output_redacted: false,
            output_truncated: false,
            attempts: 1,
            retry_reasons: Vec::new(),
            escalation: crate::command_intelligence::ValidationEscalation::Direct,
            output_summary: "failed".into(),
            recorded_at: std::time::SystemTime::now(),
            task_id: Some(task.id.clone()),
        }],
    };
    let log = Arc::new(Mutex::new(vec![ToolExecutionLogEntry {
        tool_call_id: "write-1".into(),
        tool_name: "write_file".into(),
        side_effect_class: Some(SideEffectClass::Write),
        arguments: json!({"path": "/work/src/lib.rs"}),
        success: true,
        summary: "written".into(),
    }]));
    let (sink, client, _rx) = plumbing();
    let (_cancel_tx, cancel) = watch::channel(false);
    runtime
        .run_bounded_repair(
            &prompt("repair"),
            &task,
            log,
            validation,
            sink,
            client,
            cancel,
            None,
            "s-1",
            "test",
        )
        .await
        .expect("repair controller returns terminal state");

    assert_eq!(model.call_count(), 1, "stale second snapshot must stop before a model call");
    assert_eq!(
        *runtime.last_repair_stop.lock().expect("repair stop poisoned"),
        Some(RepairStopReason::StaleState)
    );
    assert!(
        runtime.event_snapshot().iter().any(|event| matches!(
            event,
            OrchestratorEvent::RepairStarted { attempt_number: 1, .. }
        ))
    );
    assert!(runtime.event_snapshot().iter().any(|event| matches!(
        event,
        OrchestratorEvent::RepairStopped { reason } if reason == "stale_state"
    )));
}
#[tokio::test(start_paused = true)]
async fn deadline_stop_becomes_interrupted_outcome_with_durable_checkpoint() {
    // The first model call parks past the slice; the outer turn timeout
    // fires deterministically under the paused clock.
    let model = Arc::new(DelayedModel::new(
        vec![Duration::from_millis(5_000)],
        Duration::ZERO,
        FakeModel::new(vec![ModelResponse::new().text("one"), ModelResponse::new().text("two")]),
    ));
    let runtime =
        OrchestratorRuntime::new(recovery_config(Duration::from_millis(40)), model.clone());
    let (sink, client, _rx) = plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let outcome = tokio::join!(
        runtime.run_turn_recoverable(
            prompt("hello"),
            sink,
            client,
            cancel_rx,
            String::new(),
            "test-provider",
        ),
        async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            tokio::time::advance(Duration::from_millis(500)).await;
        },
    )
    .0;
    let interruption = match outcome.expect("recoverable run returns an outcome") {
        TurnOutcome::Interrupted(interruption) => interruption,
        other => panic!("expected interruption, got {other:?}"),
    };
    assert_eq!(interruption.fault, ee_agent_protocol::RecoverableFault::Deadline);
    assert!(interruption.safe_resume, "no in-flight tool; resuming is safe");
    assert_eq!(interruption.resumed_count, 0);
    assert!(interruption.checkpoint_id.is_some(), "checkpoint persisted");

    let store = runtime.checkpoint_store();
    let (id, checkpoint) = store.load_latest("s-1").expect("loads").expect("pending");
    assert_eq!(id, interruption.checkpoint_id.unwrap());
    assert_eq!(checkpoint.provider, "test-provider");
    let resume = checkpoint.resume.expect("resume state captured");
    assert!(resume.transcript.is_empty(), "durable checkpoint omits transcript content");
    assert!(runtime.event_snapshot().iter().any(|event| {
        matches!(
            event,
            OrchestratorEvent::TurnInterrupted { fault, .. } if fault == "deadline"
        )
    }));
}

#[tokio::test(start_paused = true)]
async fn resumed_turn_completes_without_new_root_and_retains_counters() {
    let model = Arc::new(DelayedModel::new(
        vec![Duration::from_millis(5_000)],
        Duration::ZERO,
        FakeModel::new(vec![
            ModelResponse::new().text("one"),
            ModelResponse::new().text("two"),
            ModelResponse::new().text("three"),
            ModelResponse::new().text("four"),
            ModelResponse::new().text("five"),
            ModelResponse::new().text("six").completed(),
        ]),
    ));
    let runtime =
        OrchestratorRuntime::new(recovery_config(Duration::from_millis(40)), model.clone());
    let (sink, client, _rx) = plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let outcome = tokio::join!(
        runtime.run_turn_recoverable(
            prompt("hello"),
            sink,
            client,
            cancel_rx,
            String::new(),
            "test-provider",
        ),
        async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            tokio::time::advance(Duration::from_millis(500)).await;
        },
    )
    .0;
    assert!(
        matches!(outcome, Ok(TurnOutcome::Interrupted(_))),
        "first slice interrupts, got {outcome:?}"
    );
    assert_eq!(runtime.tasks().len(), 1, "one root task so far");
    assert_eq!(model.inner.call_count(), 0, "hung slice consumes no scripted responses");

    // Resume: same session, same prompt; the checkpoint is consumed.
    // The clock only advances enough to fire the scripted 1 ms model
    // sleeps; a 500 ms jump would land past the fresh slice deadline.
    let (sink, client, _rx) = plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let outcome = tokio::join!(
        runtime.resume_turn(
            prompt("hello"),
            sink,
            client,
            cancel_rx,
            String::new(),
            "test-provider",
        ),
        async {
            tokio::time::sleep(Duration::from_millis(1)).await;
            tokio::time::advance(Duration::from_millis(10)).await;
        },
    )
    .0;
    match outcome.expect("resume returns an outcome") {
        TurnOutcome::Completed(result) => {
            assert_eq!(result.stop_reason, StopReason::EndTurn);
        }
        other => panic!("resumed turn should complete, got {other:?}"),
    }
    // One root task survives; no second root was created by the resume.
    assert_eq!(runtime.tasks().len(), 1, "resume reuses the existing root task");
    // Cumulative counters retained: slice 1 reserved one model call
    // before its hang; the resume consumed the six scripted responses.
    assert_eq!(runtime.budget_snapshot().model_calls_used, 7);
    assert_eq!(model.inner.call_count(), 6);
    // Completed turns clear pending checkpoints.
    assert!(runtime.checkpoint_store().load_latest("s-1").expect("loads").is_none());
    assert!(runtime.event_snapshot().iter().any(|event| {
        matches!(event, OrchestratorEvent::TurnResumed { checkpoint_id, .. } if !checkpoint_id.is_empty())
    }));
}

#[tokio::test(start_paused = true)]
async fn manual_resume_preserves_ambiguous_write_and_never_replays_it() {
    let write = || ToolIntent::new("tc-1", "write_file", json!({ "path": "/tmp/x" }));
    let tool = Arc::new(FakeTool::new(
        ToolDefinition::new("write_file", "writes a file")
            .side_effect_class(SideEffectClass::Write),
        ToolResult::success("written"),
    ));
    // Slice 1: one instant write, then a hang that trips the outer
    // timeout.  Slice 2 (resume): the model asks for the identical write
    // again, then completes.
    let model = Arc::new(DelayedModel::new(
        vec![Duration::ZERO, Duration::from_millis(5_000)],
        Duration::ZERO,
        FakeModel::new(vec![
            ModelResponse::new().text("write now").tool_intents(vec![write()]),
            ModelResponse::new().text("one"),
            ModelResponse::new().text("write again").tool_intents(vec![write()]),
            ModelResponse::new().text("done").completed(),
        ]),
    ));
    let runtime = OrchestratorRuntime::with_policy(
        recovery_config(Duration::from_millis(40)),
        model,
        PolicyEngine::new(ToolPolicy { allow_write: true, ..ToolPolicy::default() }),
    );
    runtime.register_tool(tool.clone()).expect("registers write_file");
    let (sink, client, _rx) = plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let outcome = tokio::join!(
        runtime.run_turn_recoverable(
            prompt("hello"),
            sink,
            client,
            cancel_rx,
            String::new(),
            "test-provider",
        ),
        async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            tokio::time::advance(Duration::from_millis(500)).await;
        },
    )
    .0;
    assert!(
        matches!(outcome, Ok(TurnOutcome::Interrupted(_))),
        "first slice interrupts, got {outcome:?}"
    );
    assert_eq!(tool.call_count(), 1, "the first slice executed the write once");

    // Simulate a process stop after an approved write began but before
    // completion reached the checkpoint. The marker must survive resume.
    let store = runtime.checkpoint_store();
    let (_, mut checkpoint) = store.load_latest("s-1").expect("loads").expect("pending");
    let resume = checkpoint.resume.as_mut().expect("resume state");
    resume.in_flight = Some(crate::checkpoint::InFlightOperation {
        tool_call_id: "tc-ambiguous".into(),
        tool_name: "write_file".into(),
        arguments_fingerprint: crate::checkpoint::tool_call_fingerprint(
            "write_file",
            &json!({ "path": "/tmp/x" }),
        )
        .expect("fingerprint"),
        started_at_millis: current_unix_millis(),
    });
    store.save("s-1", &checkpoint).expect("persists ambiguous marker");

    let (sink, client, _rx) = plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let error = runtime
        .resume_turn(prompt("hello"), sink, client, cancel_rx, String::new(), "test-provider")
        .await
        .expect_err("ambiguous write blocks manual resume");
    assert!(
        matches!(error, OrchestratorError::PolicyDenied(ref reason) if reason.contains("explicitly abandon")),
        "{error}"
    );
    assert_eq!(tool.call_count(), 1, "resume must not replay ambiguous write");
    let (_, retained) = store.load_latest("s-1").expect("loads").expect("still pending");
    assert!(retained.resume.expect("resume state").in_flight.is_some());
}

#[tokio::test(start_paused = true)]
async fn cancellation_clears_pending_checkpoints() {
    // The model parks briefly so the bumper can flip the cancel watch
    // before the slice's outer timeout fires.
    let model = Arc::new(DelayedModel::new(
        vec![Duration::from_millis(10)],
        Duration::from_millis(10),
        FakeModel::new(vec![ModelResponse::new().text("one")]),
    ));
    // A long slice: cancellation (not the outer timeout) must end the
    // turn deterministically.
    let runtime = OrchestratorRuntime::new(recovery_config(Duration::from_secs(10)), model);
    let (sink, client, _rx) = plumbing();
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let outcome = tokio::join!(
        runtime.run_turn_recoverable(
            prompt("hello"),
            sink,
            client,
            cancel_rx,
            String::new(),
            "test-provider",
        ),
        async {
            tokio::time::sleep(Duration::from_millis(1)).await;
            cancel_tx.send(true).expect("cancels");
            tokio::time::advance(Duration::from_millis(20)).await;
        },
    )
    .0;
    assert!(matches!(outcome, Err(OrchestratorError::Cancellation)), "got {outcome:?}");
    assert!(
        runtime.checkpoint_store().load_latest("s-1").expect("loads").is_none(),
        "a cancelled turn never leaves a stale pending checkpoint"
    );
}

#[tokio::test]
async fn completed_turn_clears_pending_checkpoints() {
    let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("done").completed()]));
    let runtime = OrchestratorRuntime::new(recovery_config(Duration::from_secs(30)), model);
    let (sink, client, _rx) = plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let outcome = runtime
        .run_turn_recoverable(
            prompt("hello"),
            sink,
            client,
            cancel_rx,
            String::new(),
            "test-provider",
        )
        .await
        .expect("outcome");
    assert!(matches!(outcome, TurnOutcome::Completed(_)));
    assert!(
        runtime.checkpoint_store().load_latest("s-1").expect("loads").is_none(),
        "completed turns clear milestone checkpoints"
    );
}

#[tokio::test(start_paused = true)]
async fn deadline_is_reanchored_per_turn_not_per_session() {
    // A runtime may idle between turns for longer than one timeout; the
    // next turn must get a fresh slice instead of failing instantly on
    // the session-creation deadline.
    let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("done").completed()]));
    let runtime = OrchestratorRuntime::new(recovery_config(Duration::from_secs(30)), model);
    tokio::time::advance(Duration::from_secs(120)).await;
    let (sink, client, _rx) = plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let outcome = runtime
        .run_turn_recoverable(
            prompt("hello"),
            sink,
            client,
            cancel_rx,
            String::new(),
            "test-provider",
        )
        .await
        .expect("outcome");
    assert!(
        matches!(outcome, TurnOutcome::Completed(_)),
        "idle time must not consume the next turn's slice: {outcome:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn resume_rejects_provider_mismatch_and_expired_sessions() {
    let model = Arc::new(DelayedModel::new(
        vec![Duration::from_millis(5_000)],
        Duration::ZERO,
        FakeModel::new(vec![ModelResponse::new().text("one")]),
    ));
    let runtime = OrchestratorRuntime::new(recovery_config(Duration::from_millis(40)), model);
    let (sink, client, _rx) = plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let outcome = tokio::join!(
        runtime.run_turn_recoverable(
            prompt("hello"),
            sink,
            client,
            cancel_rx,
            String::new(),
            "test-provider",
        ),
        async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            tokio::time::advance(Duration::from_millis(500)).await;
        },
    )
    .0;
    assert!(matches!(outcome, Ok(TurnOutcome::Interrupted(_))));

    // A different provider identity must never restore the checkpoint.
    let (sink, client, _rx) = plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let error = runtime
        .resume_turn(prompt("hello"), sink, client, cancel_rx, String::new(), "other-provider")
        .await
        .expect_err("provider mismatch rejects restore");
    assert!(
        matches!(error, OrchestratorError::PolicyDenied(ref reason) if reason.contains("provider")),
        "{error}"
    );

    // A checkpoint beyond the cumulative session cap is discarded.
    let runtime = OrchestratorRuntime::new(
        OrchestratorConfig {
            turn_timeout: Duration::from_millis(40),
            recovery: crate::config::RecoveryConfig {
                enabled: true,
                session_timeout: Some(Duration::from_secs(1)),
                ..crate::config::RecoveryConfig::default()
            },
            ..OrchestratorConfig::default()
        },
        Arc::new(DelayedModel::new(
            vec![Duration::from_millis(5_000)],
            Duration::ZERO,
            FakeModel::new(vec![ModelResponse::new().text("one")]),
        )),
    );
    let (sink, client, _rx) = plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let outcome = tokio::join!(
        runtime.run_turn_recoverable(
            prompt("hello"),
            sink,
            client,
            cancel_rx,
            String::new(),
            "test-provider",
        ),
        async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            tokio::time::advance(Duration::from_millis(500)).await;
        },
    )
    .0;
    assert!(matches!(outcome, Ok(TurnOutcome::Interrupted(_))));
    // Age the checkpoint past the one-second session cap.
    {
        let store = runtime.checkpoint_store();
        let (_, mut checkpoint) = store.load_latest("s-1").expect("loads").expect("pending");
        checkpoint.created_at_millis = crate::checkpoint::current_unix_millis();
        if let Some(resume) = checkpoint.resume.as_mut() {
            resume.first_started_at_millis =
                crate::checkpoint::current_unix_millis().saturating_sub(2_000);
        }
        store.save("s-1", &checkpoint).expect("rewrites aged checkpoint");
    }
    let (sink, client, _rx) = plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let error = runtime
        .resume_turn(prompt("hello"), sink, client, cancel_rx, String::new(), "test-provider")
        .await
        .expect_err("session cap rejects resume");
    assert!(error.to_string().contains("cumulative timeout"), "{error}");
    assert!(
        runtime.checkpoint_store().load_latest("s-1").expect("loads").is_none(),
        "expired checkpoint discarded"
    );
}
