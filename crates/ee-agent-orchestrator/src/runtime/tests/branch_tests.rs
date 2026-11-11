//! Runtime checkpoint-branch tests.
use super::*;

fn save_checkpoint(runtime: &OrchestratorRuntime, session_id: &str) -> String {
    let budget = runtime.budget.lock().expect("budget tracker poisoned").snapshot();
    let tasks = runtime.tasks.lock().expect("task graph poisoned").clone();
    let memory = runtime.memory.lock().expect("memory store poisoned").clone();
    let checkpoint = crate::checkpoint::OrchestratorCheckpoint::new(
        "branch-test",
        runtime.config().clone(),
        session_id,
        tasks,
        memory,
        crate::checkpoint::TranscriptSummary::from_transcript(&[]),
        budget,
        crate::checkpoint::SubagentTreeState::new(),
        crate::checkpoint::IdGeneratorState::new(),
    )
    .expect("checkpoint builds");
    runtime.checkpoint_store().save(session_id, &checkpoint).expect("checkpoint saves")
}

#[tokio::test]
async fn branch_rejects_missing_checkpoint() {
    let runtime =
        OrchestratorRuntime::new(OrchestratorConfig::default(), Arc::new(FakeModel::new(vec![])));
    let error =
        runtime.branch_from_checkpoint("s-1", 1).expect_err("missing checkpoint is rejected");
    assert!(error.to_string().contains("was not found"));
}

#[tokio::test]
async fn branch_without_log_records_reports_conversation_unreplayed() {
    let runtime =
        OrchestratorRuntime::new(OrchestratorConfig::default(), Arc::new(FakeModel::new(vec![])));
    save_checkpoint(&runtime, "s-1");

    let outcome = runtime.branch_from_checkpoint("s-1", 1).expect("branch is allowed");
    assert!(!outcome.conversation_replayed);
    assert_eq!(outcome.replayed_messages, 0);
    assert_eq!(outcome.source_session_id, "s-1");
}

#[tokio::test]
async fn branch_replays_log_prefix_up_to_the_checkpoint() {
    let runtime =
        OrchestratorRuntime::new(OrchestratorConfig::default(), Arc::new(FakeModel::new(vec![])));
    save_checkpoint(&runtime, "s-1");
    let store = runtime.session_logs.as_ref().expect("log store");
    let log = store.log_for("s-1");
    log.record_message(&ModelMessage::text(ModelRole::User, "before checkpoint"), None)
        .expect("message records");
    log.record_checkpoint("s-1-0000000001", 1).expect("checkpoint records");
    log.record_message(&ModelMessage::text(ModelRole::User, "after checkpoint"), None)
        .expect("message records");

    let outcome = runtime.branch_from_checkpoint("s-1", 1).expect("branch is allowed");
    assert!(outcome.conversation_replayed);
    assert_eq!(outcome.replayed_messages, 1);
    let replayed = store.log_for(&outcome.session_id).replay_messages().expect("branch replays");
    assert_eq!(replayed.len(), 1);
    assert_eq!(replayed[0].1.text_content(), "before checkpoint");
}
