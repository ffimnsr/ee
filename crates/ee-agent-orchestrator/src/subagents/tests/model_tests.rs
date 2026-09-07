//! Subagent tests: model.
use super::*;

#[tokio::test]
async fn delegate_model_explicit_selection_runs_child_on_selected_adapter() {
    let parent = FakeModel::new(vec![
        ModelResponse::new().tool_intents(vec![delegate_intent(json!({
            "prompt": "do the thing",
            "role_name": "researcher",
            "model": "strong",
        }))]),
        ModelResponse::new().text("parent done").completed(),
    ]);
    let strong =
        FakeModel::new(vec![ModelResponse::new().text(handoff_output("child answer")).completed()]);
    let (sink, client, _rx) = plumbing();
    let runtime = model_registry_runtime(
        OrchestratorConfig::default(),
        Arc::new(parent.clone()),
        Arc::new(strong.clone()),
    );
    let events = EventRecorder::new();
    let (_cancel_tx, cancel_rx) = watch::channel(false);

    runtime
        .run_turn_recording(prompt("delegate"), sink, client, cancel_rx, events.clone())
        .await
        .expect("turn succeeds");

    // The parent ran twice on the default adapter; the child ran once on
    // the selected strong adapter.
    assert_eq!(parent.requests().len(), 2, "parent adapter serves the parent loop");
    assert_eq!(strong.requests().len(), 1, "strong adapter serves the child loop");
    let parent_request = &parent.requests()[0];
    assert_eq!(parent_request.model_id.as_deref(), Some("default"));
    let child_request = &strong.requests()[0];
    assert_eq!(child_request.model_id.as_deref(), Some("strong"));
    assert_eq!(
        child_request.task.id,
        TaskId::new("task-2"),
        "child request carries the child task node"
    );

    // Both requests advertise the registry list without secrets.
    for request in [parent_request, child_request] {
        let ids: Vec<&str> = request.available_models.iter().map(|info| info.id.as_str()).collect();
        assert_eq!(ids, vec!["default", "strong"]);
        let strong = request
            .available_models
            .iter()
            .find(|info| info.id == "strong")
            .expect("strong advertised");
        assert_eq!(strong.display_name.as_deref(), Some("Strong Model"));
        assert_eq!(strong.capabilities, vec!["tools"]);
        assert!(
            request.available_models.iter().all(|info| info.id != "secret-provider-token"),
            "advertised list never leaks secrets"
        );
    }

    // The delegation event records the selected model.
    let started = events
        .events()
        .iter()
        .find_map(|event| match event {
            OrchestratorEvent::SubagentStarted { subagent_id, model_id, .. } => {
                Some((subagent_id.clone(), model_id.clone()))
            }
            _ => None,
        })
        .expect("subagent started");
    assert_eq!(started, ("task-2".to_string(), Some("strong".to_string())));

    // The child task node records the selected model.
    let tasks = runtime.tasks();
    let child = tasks.get(&TaskId::new("task-2")).expect("child task exists");
    assert_eq!(child.model_id.as_deref(), Some("strong"));
    assert_eq!(tasks.get(&TaskId::new("task-1")).expect("root").model_id, None);
}

#[tokio::test]
async fn role_router_drives_runtime_model_selection_and_telemetry() {
    let parent = FakeModel::new(vec![
        ModelResponse::new().tool_intents(vec![delegate_intent(json!({
            "prompt": "research the thing",
            "role_name": "researcher",
        }))]),
        ModelResponse::new().text("parent done").completed(),
    ]);
    let strong =
        FakeModel::new(vec![ModelResponse::new().text(handoff_output("child answer")).completed()]);
    let (sink, client, _rx) = plumbing();
    let runtime = model_registry_runtime(
        OrchestratorConfig::default(),
        Arc::new(parent.clone()),
        Arc::new(strong.clone()),
    );
    runtime
        .set_model_router(
            ModelRouter::new(vec![
                ModelRoute::new("default", "default", ModelTier::Cheap),
                ModelRoute::new("research", "strong", ModelTier::Strong).for_roles(&["researcher"]),
            ])
            .expect("valid router"),
        )
        .expect("registered routes");
    let events = EventRecorder::new();
    let (_cancel_tx, cancel_rx) = watch::channel(false);

    runtime
        .run_turn_recording(prompt("delegate"), sink, client, cancel_rx, events.clone())
        .await
        .expect("turn succeeds");

    assert_eq!(strong.requests().len(), 1, "role route serves child");
    assert_eq!(strong.requests()[0].model_id.as_deref(), Some("strong"));
    assert!(events.events().iter().any(|event| matches!(
        event,
        OrchestratorEvent::ModelRouted {
            route_id,
            adapter_id,
            role: Some(role),
            ..
        } if route_id == "research" && adapter_id == "strong" && role == "researcher"
    )));
    assert!(events.events().iter().any(|event| matches!(
        event,
        OrchestratorEvent::SubagentStarted {
            role,
            model_id: Some(model_id),
            ..
        } if role == "researcher" && model_id == "strong"
    )));
    assert_eq!(runtime.metrics_snapshot().subagent_spawns("researcher"), 1);
    assert!(runtime.decision_log_snapshot().entries().iter().any(|entry| {
        entry.kind == DecisionKind::Delegation && entry.reason_code == "delegate-allowed"
    }));
}

#[test]
fn model_router_rejects_unknown_adapter_before_installation() {
    let runtime = model_registry_runtime(
        OrchestratorConfig::default(),
        Arc::new(FakeModel::new(Vec::new())),
        Arc::new(FakeModel::new(Vec::new())),
    );
    let router =
        ModelRouter::new(vec![ModelRoute::new("missing", "unregistered", ModelTier::Strong)])
            .expect("structurally valid router");

    let error = runtime.set_model_router(router).expect_err("unknown adapter rejected");
    assert!(error.to_string().contains("unknown adapter unregistered"));
}

#[tokio::test]
async fn delegate_model_unset_selection_falls_back_to_parent_adapter() {
    let parent = FakeModel::new(vec![
        ModelResponse::new().tool_intents(vec![delegate_intent(json!({
            "prompt": "do the thing",
        }))]),
        ModelResponse::new().text("parent answer").completed(),
    ]);
    let strong = FakeModel::new(Vec::new());
    let (sink, client, _rx) = plumbing();
    let runtime = model_registry_runtime(
        OrchestratorConfig::default(),
        Arc::new(parent.clone()),
        Arc::new(strong.clone()),
    );
    let events = EventRecorder::new();
    let (_cancel_tx, cancel_rx) = watch::channel(false);

    runtime
        .run_turn_recording(prompt("delegate"), sink, client, cancel_rx, events.clone())
        .await
        .expect("turn succeeds");

    // The child consumed the next parent script response on the parent
    // adapter; the strong adapter never ran.
    assert!(strong.requests().is_empty(), "unselected adapter never runs");
    let parent_requests = parent.requests();
    let child_call = parent_requests
        .iter()
        .find(|request| request.task.id == TaskId::new("task-2"))
        .expect("child request went to the parent adapter");
    assert_eq!(child_call.model_id.as_deref(), Some("default"), "fallback resolved");

    // The delegation event and child node record the fallback selection.
    let started = events
        .events()
        .iter()
        .find_map(|event| match event {
            OrchestratorEvent::SubagentStarted { subagent_id, model_id, .. } => {
                Some((subagent_id.clone(), model_id.clone()))
            }
            _ => None,
        })
        .expect("subagent started");
    assert_eq!(started, ("task-2".to_string(), Some("default".to_string())));
    let tasks = runtime.tasks();
    assert_eq!(
        tasks.get(&TaskId::new("task-2")).expect("child task").model_id.as_deref(),
        Some("default")
    );
}

#[tokio::test]
async fn delegate_model_unknown_selection_never_creates_child_task() {
    let parent = FakeModel::new(vec![
        ModelResponse::new().tool_intents(vec![delegate_intent(json!({
            "prompt": "do the thing",
            "model": "nope",
        }))]),
        ModelResponse::new().text("parent done").completed(),
    ]);
    let strong = FakeModel::new(Vec::new());
    let (sink, client, _rx) = plumbing();
    let runtime = model_registry_runtime(
        OrchestratorConfig::default(),
        Arc::new(parent.clone()),
        Arc::new(strong.clone()),
    );
    let events = EventRecorder::new();
    let (_cancel_tx, cancel_rx) = watch::channel(false);

    runtime
        .run_turn_recording(prompt("delegate"), sink, client, cancel_rx, events.clone())
        .await
        .expect("turn succeeds with the failure fed back");

    // No child task node was created and no subagent started.
    let tasks = runtime.tasks();
    assert_eq!(tasks.len(), 1, "only the root task exists");
    assert_eq!(tasks.get(&TaskId::new("task-1")).expect("root").model_id, None);
    assert!(
        !events
            .events()
            .iter()
            .any(|event| matches!(event, OrchestratorEvent::SubagentStarted { .. })),
        "unknown model never starts a subagent"
    );
    assert!(strong.requests().is_empty());

    // The delegate tool failed with the deterministic rejection, and the
    // parent loop recovered.
    assert!(events.events().iter().any(|event| matches!(
        event,
        OrchestratorEvent::ToolFinished {
            tool_name,
            success: false,
            ..
        } if tool_name == "delegate_task"
    )));
    assert_eq!(parent.requests().len(), 2, "parent recovered after the failure");
}

#[tokio::test]
async fn delegate_model_nested_child_falls_back_to_parent_adapter() {
    // Root delegates to a child on `strong`; that child delegates again
    // without a selection, so the grandchild must fall back to `strong`
    // (its parent adapter), not the registry default.
    let parent = FakeModel::new(vec![
        ModelResponse::new().tool_intents(vec![delegate_intent(json!({
            "prompt": "level one",
            "model": "strong",
            "allowed_tool_classes": ["delegate"],
        }))]),
        ModelResponse::new().text("root done").completed(),
    ]);
    let strong = FakeModel::new(vec![
        ModelResponse::new().tool_intents(vec![delegate_intent(json!({
            "prompt": "level two",
            "allowed_tool_classes": ["delegate"],
        }))]),
        ModelResponse::new().text(handoff_output("child done")).completed(),
    ]);
    let (sink, client, _rx) = plumbing();
    let runtime = model_registry_runtime(
        OrchestratorConfig { max_subagent_depth: 2, ..OrchestratorConfig::default() },
        Arc::new(parent.clone()),
        Arc::new(strong.clone()),
    );
    let events = EventRecorder::new();
    let (_cancel_tx, cancel_rx) = watch::channel(false);

    runtime
        .run_turn_recording(prompt("delegate"), sink, client, cancel_rx, events.clone())
        .await
        .expect("turn succeeds");

    // Both children ran on `strong`.
    let started: Vec<(String, Option<String>)> = events
        .events()
        .into_iter()
        .filter_map(|event| match event {
            OrchestratorEvent::SubagentStarted { subagent_id, model_id, .. } => {
                Some((subagent_id.clone(), model_id.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(started.len(), 2);
    assert_eq!(started[0], ("task-2".to_string(), Some("strong".to_string())));
    assert_eq!(started[1], ("task-3".to_string(), Some("strong".to_string())));

    // The grandchild request carried `strong` as its diagnostic model id
    // and the strong adapter served the child and grandchild loops.
    let strong_requests = strong.requests();
    assert_eq!(strong_requests.len(), 4, "child loop (3 calls) + grandchild loop (1 call)");
    assert!(strong_requests.iter().all(|request| request.model_id.as_deref() == Some("strong")));
    let tasks = runtime.tasks();
    assert_eq!(
        tasks.get(&TaskId::new("task-3")).expect("grandchild").model_id.as_deref(),
        Some("strong")
    );
}

#[tokio::test]
async fn delegate_model_schema_advertises_registry_ids() {
    let parent = FakeModel::new(vec![ModelResponse::new().text("done").completed()]);
    let strong = FakeModel::new(Vec::new());
    let (sink, client, _rx) = plumbing();
    let runtime = model_registry_runtime(
        OrchestratorConfig::default(),
        Arc::new(parent.clone()),
        Arc::new(strong.clone()),
    );
    let (_cancel_tx, cancel_rx) = watch::channel(false);

    runtime.run_turn(prompt("delegate"), sink, client, cancel_rx).await.expect("turn");

    let parent_requests = parent.requests();
    let delegate = parent_requests[0]
        .tools
        .iter()
        .find(|tool| tool.name == "delegate_task")
        .expect("delegate_task advertised");
    assert!(
        delegate.description.contains("Available models: default, strong"),
        "description advertises ids: {}",
        delegate.description
    );
    let model_schema = &delegate.input_schema["properties"]["model"];
    assert_eq!(model_schema["type"], json!("string"));
    assert_eq!(model_schema["enum"], json!(["default", "strong"]));
}

#[test]
fn delegate_model_role_builder_and_roundtrip() {
    let role = SubagentRole::new("researcher", "instructions").with_model("strong");
    assert_eq!(role.model.as_deref(), Some("strong"));
    let json = serde_json::to_string(&role).expect("serializes");
    let restored: SubagentRole = serde_json::from_str(&json).expect("parses");
    assert_eq!(restored, role);
    assert_eq!(SubagentRole::new("worker", "x").model, None);
}

#[test]
fn delegate_model_registry_requires_default_adapter() {
    let mut registry = ModelRegistry::new();
    registry.register("strong", Arc::new(FakeModel::new(Vec::new()))).expect("registers");
    let error = match OrchestratorRuntime::with_model_registry(
        OrchestratorConfig::default(),
        registry,
        PolicyEngine::default(),
    ) {
        Ok(_) => panic!("registry without a default adapter must fail closed"),
        Err(error) => error,
    };
    assert!(
        matches!(error, OrchestratorError::InvalidState(reason) if reason.contains("no default adapter"))
    );
}
