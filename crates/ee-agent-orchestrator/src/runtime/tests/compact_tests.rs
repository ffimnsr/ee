//! Runtime tests: compact.
use super::*;

#[tokio::test]
async fn compact_turn_preserves_protected_memory_and_invokes_no_tools() {
    let model =
        Arc::new(FakeModel::new(vec![ModelResponse::new().text("SUMMARY TEXT").completed()]));
    let runtime = compact_runtime(OrchestratorConfig::default(), model.clone());
    let (sink, client, mut rx) = plumbing();
    let events = EventRecorder::new();
    let (_cancel_tx, cancel_rx) = watch::channel(false);

    let result = runtime
        .run_turn_recording(prompt("/compact"), sink, client, cancel_rx, events.clone())
        .await
        .expect("compaction turn succeeds");
    assert_eq!(result.stop_reason, StopReason::EndTurn);

    // Exactly one model call, no tools, compaction prompt in system.
    let calls = model.requests();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].tools.is_empty(), "no tools may be exposed during compaction");
    let crate::model::ModelContent::Text(system) = &calls[0].transcript[0].content[0] else {
        panic!("expected system text");
    };
    assert!(system.contains("Write a compact continuation summary"), "{system}");

    // The summary is stored as model-derived session memory; protected
    // keys survive; the deterministic pass merged the duplicates.
    let memory = runtime.memory();
    assert_eq!(memory.query("summary:session").expect("stored").value, "SUMMARY TEXT");
    assert_eq!(memory.query("decision:api").expect("kept").value, "use v2");
    assert_eq!(memory.query("constraint:offline").expect("kept").value, "no network");
    assert_eq!(memory.query("validation:tests").expect("kept").value, "all pass");
    assert_eq!(memory.query("obs:file").expect("kept").value, "new read", "merged");
    assert_eq!(memory.query_prefix("obs:").len(), 1, "duplicates merged");

    // No tools ran, no plan was emitted, and the report message carries
    // the deterministic counts.
    let recorded = events.events();
    assert!(
        !recorded.iter().any(|event| matches!(event, OrchestratorEvent::ToolStarted { .. })),
        "no tool lifecycle events: {recorded:?}"
    );
    let report = next_update(&mut rx).await;
    let SessionUpdate::AgentMessageChunk(chunk) = report else {
        panic!("expected the compaction report message");
    };
    let ContentBlock::Text(text) = chunk.content else {
        panic!("expected text content");
    };
    assert!(text.text.contains("Session compacted:"), "{}", text.text);
    assert!(text.text.contains("merged 1 duplicate facts"), "{}", text.text);
    assert!(text.text.contains("preserved 3 protected items"), "{}", text.text);
    assert!(text.text.contains("stored 27 summary bytes"), "{}", text.text);
}

#[tokio::test]
async fn compact_turn_context_is_byte_bounded() {
    let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("SUMMARY").completed()]));
    let mut memory = MemoryStore::new(4_096);
    for index in 0..50 {
        memory.insert(MemoryItem::new(format!("obs:{index}"), "v".repeat(20))).expect("inserts");
    }
    let config = OrchestratorConfig {
        compaction: crate::compaction::CompactionConfig {
            max_input_bytes: 300,
            ..crate::compaction::CompactionConfig::default()
        },
        ..OrchestratorConfig::default()
    };
    let runtime = OrchestratorRuntime::with_state(
        config,
        model.clone(),
        crate::policy::PolicyEngine::default(),
        TaskGraph::new(),
        memory,
    );
    let (sink, client, _rx) = plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);

    runtime
        .run_turn(prompt("/compact"), sink, client, cancel_rx)
        .await
        .expect("compaction turn succeeds");

    let calls = model.requests();
    assert_eq!(calls.len(), 1);
    let crate::model::ModelContent::Text(system) = &calls[0].transcript[0].content[0] else {
        panic!("expected system text");
    };
    // The compaction context itself is bounded to 300 bytes; the prompt
    // text is separate, so the system message stays well under 1 KiB.
    assert!(system.len() < 1_024, "system message bounded: {} bytes", system.len());
    assert!(system.contains("obs:49"), "newest memory retained: {system}");
    assert!(!system.contains("obs:0"), "oldest memory dropped for the bound: {system}");
}

#[tokio::test]
async fn compact_turn_rejects_empty_summary_without_memory_changes() {
    let model = Arc::new(FakeModel::new(vec![ModelResponse::new().completed()]));
    let runtime = compact_runtime(OrchestratorConfig::default(), model.clone());
    let memory_before = runtime.memory();
    let (sink, client, _rx) = plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);

    let error = runtime
        .run_turn(prompt("/compact"), sink, client, cancel_rx)
        .await
        .expect_err("empty summaries reject");
    assert!(
        matches!(&error, OrchestratorError::InvalidState(reason)
            if reason.contains("compaction summary was empty")),
        "{error}"
    );
    assert_eq!(
        runtime.memory(),
        memory_before,
        "failed compaction must not commit duplicate merging or decay"
    );
    assert_eq!(model.call_count(), 1);
}

#[tokio::test]
async fn compact_turn_respects_cancellation_before_model_call() {
    let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("SUMMARY").completed()]));
    let runtime = compact_runtime(OrchestratorConfig::default(), model.clone());
    let (sink, client, _rx) = plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(true);

    let error = runtime
        .run_turn(prompt("/compact"), sink, client, cancel_rx)
        .await
        .expect_err("pre-cancelled compaction stops");
    assert_eq!(error, OrchestratorError::Cancellation);
    assert_eq!(model.call_count(), 0, "no model call after cancellation");
    assert!(runtime.memory().query("summary:session").is_none());
}

#[tokio::test]
async fn compact_prefix_collision_runs_the_normal_loop() {
    let model =
        Arc::new(FakeModel::new(vec![ModelResponse::new().text("normal answer").completed()]));
    let runtime = compact_runtime(OrchestratorConfig::default(), model.clone());
    let (sink, client, _rx) = plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);

    let result = runtime
        .run_turn(prompt("/compactness"), sink, client, cancel_rx)
        .await
        .expect("prefix collisions are ordinary prompts");
    assert_eq!(result.stop_reason, StopReason::EndTurn);

    let calls = model.requests();
    assert_eq!(calls.len(), 1);
    // The loop prepends a system message with memory facts; the user
    // message carries the original prompt verbatim.
    assert_eq!(calls[0].transcript[0].role, ModelRole::System);
    assert_eq!(calls[0].transcript[1].role, ModelRole::User);
    assert_eq!(
        calls[0].transcript[1].content,
        vec![crate::model::ModelContent::Text("/compactness".into())]
    );
    assert!(!calls[0].tools.is_empty(), "normal loop advertises tools");
    assert_eq!(runtime.tasks().len(), 1, "root task created by the normal loop");
    assert!(runtime.memory().query("summary:session").is_none());
}
