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
            step_event(1, 1),
            OrchestratorEvent::ModelRequested { iteration: 1 },
            OrchestratorEvent::ModelResponded { iteration: 1 },
            budget_event(1, 1, 0, 0, 11), // "hello world"
            stop_gate_event(),
            OrchestratorEvent::TurnStopped { stop_reason: "end_turn".into() },
        ]
    );
}

#[tokio::test]
async fn session_rename_emits_a_session_info_update() {
    let script = vec![
        ModelResponse::new().tool_intents(vec![ToolIntent::new(
            "tc-1",
            "session_rename",
            json!({ "title": "Audit run" }),
        )]),
        ModelResponse::new().text("renamed").completed(),
    ];
    let model = Arc::new(FakeModel::new(script));
    let runtime = OrchestratorRuntime::new(OrchestratorConfig::default(), model);
    runtime.register_builtins(&SessionId::new("s-1")).expect("registers session tools");
    assert!(runtime.tool_names().iter().any(|name| name == "models"));
    let (sink, client, mut rx) = plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);

    let result = runtime
        .run_turn_recording(
            prompt("rename this session"),
            sink,
            client,
            cancel_rx,
            EventRecorder::new(),
        )
        .await
        .expect("turn succeeds");
    assert_eq!(result.stop_reason, StopReason::EndTurn);

    let mut titles = Vec::new();
    while let Ok(OutboundEvent::Update { update, .. }) = rx.try_recv() {
        if let SessionUpdate::SessionInfoUpdate(info) = *update {
            titles.extend(info.title.value().cloned());
        }
    }
    assert_eq!(titles, vec!["Audit run".to_string()]);
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
            step_event(1, 2),
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
            step_event(2, 2),
            OrchestratorEvent::ModelRequested { iteration: 2 },
            OrchestratorEvent::ModelResponded { iteration: 2 },
            budget_event(2, 2, 1, 0, 7), // "read it"
            stop_gate_event(),
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
            step_event(1, 1),
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
            subagent_step_event(1, 1),
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
            step_event(2, 1),
            OrchestratorEvent::ModelRequested { iteration: 2 },
            OrchestratorEvent::ModelResponded { iteration: 2 },
            budget_event(2, 2, 1, 1, 13), // "parent answer"
            stop_gate_event(),
            OrchestratorEvent::TurnStopped { stop_reason: "end_turn".into() },
        ]
    );
}

#[tokio::test]
async fn fixture_endless_tool_loop_stops_via_iteration_budget() {
    let model = Arc::new(FakeModel::new(endless_tool_loop_script(6)));
    let config =
        OrchestratorConfig { max_loop_iterations: Some(3), ..OrchestratorConfig::default() };
    let runtime = OrchestratorRuntime::new(config, model.clone());
    runtime.register_tool(read_file_tool()).expect("registers read_file");
    let (sink, client, _rx) = plumbing();
    let events = EventRecorder::new();
    let (_cancel_tx, cancel_rx) = watch::channel(false);

    let result = runtime
        .run_turn_recording(prompt("loop forever"), sink, client, cancel_rx, events.clone())
        .await
        .expect("the cap produces a summary instead of an error");
    assert_eq!(result.stop_reason, ee_agent_protocol::StopReason::MaxTurnRequests);
    assert_eq!(model.call_count(), 4, "three steps plus one summary call");
    let recorded = events.events();
    assert_eq!(
        recorded[0],
        OrchestratorEvent::TurnStarted { session_id: "s-1".into(), task_id: "task-1".into() }
    );
    assert_eq!(recorded[1], budget_event(1, 1, 0, 0, 0));
    // Every iteration reserves, calls the model, and executes the tool.
    assert_eq!(
        recorded.iter().filter(|e| matches!(e, OrchestratorEvent::ModelRequested { .. })).count(),
        4
    );
    assert_eq!(
        recorded.iter().filter(|e| matches!(e, OrchestratorEvent::ToolStarted { .. })).count(),
        3
    );
    assert_eq!(
        recorded.last(),
        Some(&OrchestratorEvent::TurnStopped { stop_reason: "max_iterations".into() })
    );
}
