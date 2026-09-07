//! Runtime tests: fixture.
use super::*;

#[tokio::test]
async fn fixture_simple_answer_produces_stable_event_sequence() {
    let model = Arc::new(FakeModel::new(simple_answer_script()));
    let runtime = OrchestratorRuntime::new(OrchestratorConfig::default(), model.clone());
    let (sink, client, _rx) = plumbing();
    let events = EventRecorder::new();
    let (_cancel_tx, cancel_rx) = watch::channel(false);

    let result = runtime
        .run_turn_recording(prompt("hello world"), sink, client, cancel_rx, events.clone())
        .await
        .expect("turn succeeds");
    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(
        events.events(),
        vec![
            OrchestratorEvent::TurnStarted { session_id: "s-1".into(), task_id: "task-1".into() },
            budget_event(1, 1, 0, 0, 0),
            OrchestratorEvent::ModelRequested { iteration: 1 },
            OrchestratorEvent::ModelResponded { iteration: 1 },
            budget_event(1, 1, 0, 0, 11), // "hello world"
            OrchestratorEvent::TurnStopped { stop_reason: "end_turn".into() },
        ]
    );
}

#[tokio::test]
async fn fixture_tool_then_answer_produces_stable_event_sequence() {
    let model = Arc::new(FakeModel::new(tool_then_answer_script()));
    let runtime = OrchestratorRuntime::new(OrchestratorConfig::default(), model.clone());
    runtime.register_tool(read_file_tool()).expect("registers read_file");
    let (sink, client, _rx) = plumbing();
    let events = EventRecorder::new();
    let (_cancel_tx, cancel_rx) = watch::channel(false);

    let result = runtime
        .run_turn_recording(prompt("read a file"), sink, client, cancel_rx, events.clone())
        .await
        .expect("turn succeeds");
    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(
        events.events(),
        vec![
            OrchestratorEvent::TurnStarted { session_id: "s-1".into(), task_id: "task-1".into() },
            budget_event(1, 1, 0, 0, 0),
            OrchestratorEvent::ModelRequested { iteration: 1 },
            OrchestratorEvent::ModelResponded { iteration: 1 },
            budget_event(1, 1, 0, 0, 0), // tool intent only
            OrchestratorEvent::ToolStarted {
                tool_call_id: "tc-1".into(),
                tool_name: "read_file".into(),
            },
            budget_event(1, 1, 1, 0, 0), // tool reservation
            OrchestratorEvent::ToolFinished {
                tool_call_id: "tc-1".into(),
                tool_name: "read_file".into(),
                success: true,
            },
            budget_event(2, 2, 1, 0, 0),
            OrchestratorEvent::ModelRequested { iteration: 2 },
            OrchestratorEvent::ModelResponded { iteration: 2 },
            budget_event(2, 2, 1, 0, 7), // "read it"
            OrchestratorEvent::TurnStopped { stop_reason: "end_turn".into() },
        ]
    );
}

#[tokio::test]
async fn fixture_delegate_then_answer_produces_stable_event_sequence() {
    // The child subagent consumes the fixture's answer; the parent gets
    // one more response after the delegation returns.
    let mut script = delegate_then_answer_script();
    script.push(ModelResponse::new().text("parent answer").completed());
    let model = Arc::new(FakeModel::new(script));
    let policy = PolicyEngine::new(ToolPolicy { allow_delegate: true, ..ToolPolicy::default() });
    let runtime =
        OrchestratorRuntime::with_policy(OrchestratorConfig::default(), model.clone(), policy);
    let (sink, client, _rx) = plumbing();
    let events = EventRecorder::new();
    let (_cancel_tx, cancel_rx) = watch::channel(false);

    let result = runtime
        .run_turn_recording(prompt("delegate work"), sink, client, cancel_rx, events.clone())
        .await
        .expect("turn succeeds");
    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(
        events.events(),
        vec![
            OrchestratorEvent::TurnStarted { session_id: "s-1".into(), task_id: "task-1".into() },
            budget_event(1, 1, 0, 0, 0),
            OrchestratorEvent::ModelRequested { iteration: 1 },
            OrchestratorEvent::ModelResponded { iteration: 1 },
            budget_event(1, 1, 0, 0, 0),
            OrchestratorEvent::ToolStarted {
                tool_call_id: "tc-1".into(),
                tool_name: "delegate_task".into(),
            },
            budget_event(1, 1, 1, 0, 0), // tool reservation
            budget_event(1, 1, 1, 1, 0), // subagent reservation
            OrchestratorEvent::SubagentStarted {
                subagent_id: "task-2".into(),
                role: "summarizer".into(),
                model_id: Some("default".into()),
            },
            // The child runs its own loop over the shared script.
            OrchestratorEvent::TurnStarted {
                session_id: "subagent".into(),
                task_id: "task-2".into(),
            },
            budget_event(1, 1, 0, 0, 0),
            OrchestratorEvent::ModelRequested { iteration: 1 },
            OrchestratorEvent::ModelResponded { iteration: 1 },
            budget_event(1, 1, 0, 0, DELEGATED_HANDOFF_OUTPUT.len()),
            OrchestratorEvent::TurnStopped { stop_reason: "end_turn".into() },
            OrchestratorEvent::SubagentFinished { subagent_id: "task-2".into(), success: true },
            OrchestratorEvent::ToolFinished {
                tool_call_id: "tc-1".into(),
                tool_name: "delegate_task".into(),
                success: true,
            },
            budget_event(2, 2, 1, 1, 0),
            OrchestratorEvent::ModelRequested { iteration: 2 },
            OrchestratorEvent::ModelResponded { iteration: 2 },
            budget_event(2, 2, 1, 1, 13), // "parent answer"
            OrchestratorEvent::TurnStopped { stop_reason: "end_turn".into() },
        ]
    );
}

#[tokio::test]
async fn fixture_endless_tool_loop_stops_via_iteration_budget() {
    let model = Arc::new(FakeModel::new(endless_tool_loop_script(6)));
    let config = OrchestratorConfig { max_loop_iterations: 3, ..OrchestratorConfig::default() };
    let runtime = OrchestratorRuntime::new(config, model.clone());
    runtime.register_tool(read_file_tool()).expect("registers read_file");
    let (sink, client, _rx) = plumbing();
    let events = EventRecorder::new();
    let (_cancel_tx, cancel_rx) = watch::channel(false);

    let error = runtime
        .run_turn_recording(prompt("loop forever"), sink, client, cancel_rx, events.clone())
        .await
        .expect_err("iteration budget stops the loop");
    assert!(matches!(error, OrchestratorError::BudgetExceeded(_)));
    assert_eq!(model.call_count(), 3, "exactly the allowed iterations ran");
    let recorded = events.events();
    assert_eq!(
        recorded[0],
        OrchestratorEvent::TurnStarted { session_id: "s-1".into(), task_id: "task-1".into() }
    );
    assert_eq!(recorded[1], budget_event(1, 1, 0, 0, 0));
    // Every iteration reserves, calls the model, and executes the tool.
    assert_eq!(
        recorded.iter().filter(|e| matches!(e, OrchestratorEvent::ModelRequested { .. })).count(),
        3
    );
    assert_eq!(
        recorded.iter().filter(|e| matches!(e, OrchestratorEvent::ToolStarted { .. })).count(),
        3
    );
    assert_eq!(recorded[recorded.len() - 3], budget_event(3, 3, 3, 0, 0));
    assert_eq!(
        recorded[recorded.len() - 2],
        OrchestratorEvent::ToolFinished {
            tool_call_id: "tc-2".into(),
            tool_name: "read_file".into(),
            success: true,
        }
    );
    assert_eq!(
        recorded[recorded.len() - 1],
        OrchestratorEvent::TurnStopped { stop_reason: "budget_exceeded".into() }
    );
}
