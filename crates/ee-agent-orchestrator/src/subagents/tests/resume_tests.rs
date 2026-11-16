//! Subagent tests: retained transcript resume and context forks.
use super::*;

fn manager_with(
    config: OrchestratorConfig,
    model: Arc<dyn ModelAdapter>,
    transcripts: Arc<Mutex<crate::subagent_transcripts::ChildTranscriptStore>>,
) -> (Arc<SubagentManager>, Arc<Mutex<TaskGraph>>) {
    let tools = Arc::new(Mutex::new(ToolRegistry::new()));
    let tasks = Arc::new(Mutex::new(TaskGraph::new()));
    let memory = Arc::new(Mutex::new(MemoryStore::new(config.memory_limit_bytes)));
    let budget = Arc::new(Mutex::new(BudgetTracker::new(&config)));
    let children = Arc::new(ChildRegistry::default());
    let manager = Arc::new(SubagentManager::new(
        config,
        Arc::new(ModelRegistry::single(model)),
        tools,
        SubagentState::new(tasks.clone(), memory, budget, children).with_transcripts(transcripts),
        SubagentObservability::new(
            Arc::new(RwLock::new(None)),
            Arc::new(Mutex::new(OrchestratorMetrics::new())),
            Arc::new(Mutex::new(DecisionLog::default())),
        ),
    ));
    (manager, tasks)
}

fn researcher_request(
    parent_task_id: TaskId,
    prompt: &str,
    resume_child_id: Option<String>,
    fork_context: bool,
    context_snapshot: Vec<ModelMessage>,
) -> DelegationRequest {
    DelegationRequest {
        parent_task_id,
        role: BuiltinSubagentRole::Researcher.role(),
        scoped_prompt: prompt.to_string(),
        context_snapshot,
        scope: None,
        model_id: None,
        resume_child_id,
        fork_context,
    }
}

#[tokio::test]
async fn resumed_child_continues_the_retained_transcript() {
    let model = FakeModel::new(vec![ModelResponse::new().text("follow-up answer").completed()]);
    let transcripts =
        Arc::new(Mutex::new(crate::subagent_transcripts::ChildTranscriptStore::default()));
    let (manager, tasks) =
        manager_with(OrchestratorConfig::default(), Arc::new(model.clone()), transcripts.clone());
    let root = tasks.lock().expect("task graph").create_root("root", "root");
    transcripts.lock().expect("transcripts").insert(
        crate::subagent_transcripts::retained_transcript(
            "child-1",
            "researcher",
            vec![
                ModelMessage::text(ModelRole::User, "prior question"),
                ModelMessage::text(ModelRole::Assistant, "prior answer"),
            ],
            3,
        ),
    );

    let result = manager
        .spawn(
            researcher_request(
                root.id.clone(),
                "follow up",
                Some(String::from("child-1")),
                false,
                Vec::new(),
            ),
            bridge(),
            watch::channel(false).1,
            EventRecorder::new(),
        )
        .await
        .expect("resume spawn runs");

    let requests = model.requests();
    assert_eq!(requests.len(), 1);
    let texts = requests[0].transcript.iter().map(ModelMessage::text_content).collect::<Vec<_>>();
    assert!(texts.iter().any(|text| text == "prior question"), "{texts:?}");
    assert!(texts.iter().any(|text| text == "prior answer"), "{texts:?}");
    assert!(texts.iter().any(|text| text == "follow up"), "{texts:?}");
    assert_eq!(result.tool_call_count, 0, "the follow-up turn starts with a fresh tool-call count");
}

#[tokio::test]
async fn resume_rejects_unknown_ids_and_role_mismatches_without_running() {
    let model = FakeModel::new(Vec::new());
    let transcripts =
        Arc::new(Mutex::new(crate::subagent_transcripts::ChildTranscriptStore::default()));
    let (manager, tasks) =
        manager_with(OrchestratorConfig::default(), Arc::new(model.clone()), transcripts.clone());
    let root = tasks.lock().expect("task graph").create_root("root", "root");

    let unknown = manager
        .spawn(
            researcher_request(
                root.id.clone(),
                "follow up",
                Some(String::from("missing")),
                false,
                Vec::new(),
            ),
            bridge(),
            watch::channel(false).1,
            EventRecorder::new(),
        )
        .await;
    assert!(
        matches!(unknown, Err(OrchestratorError::InvalidState(ref reason)) if reason.contains("unknown or expired"))
    );

    transcripts.lock().expect("transcripts").insert(
        crate::subagent_transcripts::retained_transcript(
            "child-1",
            "researcher",
            vec![ModelMessage::text(ModelRole::User, "prior")],
            0,
        ),
    );
    let mut request =
        researcher_request(root.id, "follow up", Some(String::from("child-1")), false, Vec::new());
    request.role = BuiltinSubagentRole::CodeReader.role();
    let mismatch =
        manager.spawn(request, bridge(), watch::channel(false).1, EventRecorder::new()).await;
    assert!(
        matches!(mismatch, Err(OrchestratorError::InvalidState(ref reason)) if reason.contains("cannot resume"))
    );
    assert_eq!(model.call_count(), 0, "rejected resumes never call the model");
}

#[tokio::test]
async fn fork_context_compacts_parent_snapshot() {
    let model = FakeModel::new(vec![ModelResponse::new().text("fork answer").completed()]);
    let transcripts =
        Arc::new(Mutex::new(crate::subagent_transcripts::ChildTranscriptStore::default()));
    let (manager, tasks) =
        manager_with(OrchestratorConfig::default(), Arc::new(model.clone()), transcripts);
    let root = tasks.lock().expect("task graph").create_root("root", "root");
    let snapshot = vec![
        ModelMessage::text(ModelRole::User, "parent turn"),
        ModelMessage::tool_result(
            "tc-1",
            crate::tools::ToolResult::success("large tool output sentinel"),
        ),
    ];

    manager
        .spawn(
            researcher_request(root.id, "plan next", None, true, snapshot),
            bridge(),
            watch::channel(false).1,
            EventRecorder::new(),
        )
        .await
        .expect("forked spawn runs");

    let requests = model.requests();
    let texts = requests[0]
        .transcript
        .iter()
        .map(ModelMessage::text_content)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(texts.contains("compacted for this forked subagent"), "{texts}");
    assert!(texts.contains("parent turn"), "{texts}");
    assert!(!texts.contains("large tool output sentinel"), "{texts}");
}

#[tokio::test]
async fn fork_context_rejects_non_read_only_roles() {
    let model = FakeModel::new(Vec::new());
    let transcripts =
        Arc::new(Mutex::new(crate::subagent_transcripts::ChildTranscriptStore::default()));
    let (manager, tasks) =
        manager_with(OrchestratorConfig::default(), Arc::new(model.clone()), transcripts);
    let root = tasks.lock().expect("task graph").create_root("root", "root");
    let mut request = researcher_request(root.id, "edit", None, true, Vec::new());
    request.role = BuiltinSubagentRole::Implementer.role();

    let result =
        manager.spawn(request, bridge(), watch::channel(false).1, EventRecorder::new()).await;
    assert!(
        matches!(result, Err(OrchestratorError::InvalidState(ref reason)) if reason.contains("fork_context"))
    );
    assert_eq!(model.call_count(), 0);
}
