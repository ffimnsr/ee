//! Session log tests.

use std::sync::Arc;

use crate::budget::BudgetSnapshot;
use crate::model::{ModelMessage, ModelRole};
use crate::session_log::*;
use crate::session_log_sink::is_content_hash;
use crate::tools::{ToolDefinition, ToolResult};

fn message(text: &str) -> ModelMessage {
    ModelMessage::text(ModelRole::User, text)
}

fn tool_message(id: &str, text: &str) -> ModelMessage {
    ModelMessage::tool_result(id, ToolResult::success(text))
}

fn sample_budget() -> BudgetSnapshot {
    BudgetSnapshot {
        iterations_used: 1,
        iterations_max: Some(8),
        model_calls_used: 1,
        model_calls_max: Some(8),
        tool_calls_used: 0,
        tool_calls_max: Some(16),
        subagents_used: 0,
        subagents_max: 4,
        output_bytes_used: 0,
        output_bytes_max: None,
        input_tokens_used: None,
        input_tokens_max: None,
        output_tokens_used: None,
        output_tokens_max: None,
    }
}

#[test]
fn message_bodies_are_content_addressed_and_replay_in_order() {
    let log = SessionLog::in_memory("s-1");
    let first = log.record_message(&message("hello"), None).expect("records");
    let second =
        log.record_message(&tool_message("tc-1", "result"), Some("tc-1")).expect("records");
    assert_eq!((first.as_str(), second.as_str()), ("msg-1", "msg-2"));

    let records = log.records().expect("records");
    let hashes: Vec<&str> = records
        .iter()
        .filter_map(|record| match record {
            SessionLogRecord::Message(message) => Some(message.content_hash.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(hashes.len(), 2);
    assert!(hashes.iter().all(|hash| is_content_hash(hash)));

    let replayed = log.replay_messages().expect("replays");
    assert_eq!(replayed.len(), 2);
    assert_eq!(replayed[0].1, message("hello"));
    assert_eq!(replayed[1].0, "msg-2");
    assert_eq!(replayed[1].1, tool_message("tc-1", "result"));
}

#[test]
fn model_request_records_ordered_ids_and_hashes() {
    let log = SessionLog::in_memory("s-1");
    log.begin_turn();
    log.set_iteration(2);
    let ids = vec![
        log.record_message(&message("hi"), None).expect("records"),
        log.record_message(&tool_message("tc-1", "out"), Some("tc-1")).expect("records"),
    ];
    let tools = vec![ToolDefinition::new("read_file", "reads")];
    let request = log
        .record_model_request(
            ids.clone(),
            &tools,
            projection_hash(&[message("hi")]),
            &sample_budget(),
            Some("default"),
        )
        .expect("records");
    assert_eq!(request, "req-1");

    let record = log
        .records()
        .expect("records")
        .into_iter()
        .find_map(|record| match record {
            SessionLogRecord::ModelRequest(request) => Some(*request),
            _ => None,
        })
        .expect("request record");
    assert_eq!(record.message_ids, ids);
    assert_eq!(record.turn, 1);
    assert_eq!(record.iteration, 2);
    assert_eq!(record.tool_schema_hash, tool_schema_hash(&tools));
    assert_eq!(record.model_id.as_deref(), Some("default"));
}

#[test]
fn truncation_is_recorded_without_deleting_messages() {
    let log = SessionLog::in_memory("s-1");
    log.record_message(&message("old"), None).expect("records");
    log.record_truncation(&crate::model::TranscriptTruncation {
        removed_messages: 1,
        removed_bytes: 7,
        retained_bytes: 11,
        limit_bytes: 11,
    });
    let records = log.records().expect("records");
    assert_eq!(
        records.iter().filter(|record| matches!(record, SessionLogRecord::Message(_))).count(),
        1
    );
    assert!(records.iter().any(|record| matches!(record, SessionLogRecord::Truncation(_))));
}

#[test]
fn bounded_replay_drops_oldest_but_keeps_newest() {
    let log = SessionLog::in_memory("s-1");
    log.record_message(&message("first"), None).expect("records");
    log.record_message(&message("second"), None).expect("records");
    let bounded = log.replay_messages_bounded(1).expect("replays");
    assert_eq!(bounded.len(), 1);
    assert_eq!(bounded[0].1, message("second"));
}

#[test]
fn export_redacts_secrets_but_reconstruction_keeps_them() {
    let log = SessionLog::in_memory("s-1");
    let secret = "sk-live-1234567890";
    log.record_message(&message(&format!("use {secret}")), None).expect("records");
    let export = log.export_redacted_jsonl().expect("exports");
    assert!(!export.contains(secret));
    assert!(export.contains("redacted_preview"));
    let replayed = log.replay_messages().expect("replays");
    assert!(replayed[0].1.text_content().contains(secret));
}

#[test]
fn retention_caps_fail_closed() {
    let config = SessionLogConfig { max_records: 1, ..SessionLogConfig::default() };
    let log = SessionLog::with_sink("s-1", Arc::new(InMemorySessionLogSink::new()), config);
    log.record_message(&message("one"), None).expect("first record");
    let error = log.record_message(&message("two"), None).expect_err("cap enforced");
    assert!(error.to_string().contains("record cap"));
    assert!(log.records().is_err(), "failed append marks the log unavailable");
}

#[test]
fn branch_replays_exact_prefix() {
    let log = SessionLog::in_memory("s-1");
    log.record_message(&message("before"), None).expect("records");
    log.record_checkpoint("s-1-0000000001", 1).expect("records");
    log.record_message(&message("after"), None).expect("records");

    let outcome = log.branch_from(1).expect("branches");
    assert!(outcome.conversation_replayed);
    assert_eq!(outcome.replayed_messages, 1);
    let branch = log.sink().records(&outcome.session_id).expect("records");
    assert_eq!(
        branch.iter().filter(|record| matches!(record, SessionLogRecord::Message(_))).count(),
        1
    );
    assert!(branch.iter().any(|record| matches!(
        record,
        SessionLogRecord::Branch { conversation_replayed: true, .. }
    )));
}

#[test]
fn branch_without_log_reports_conversation_unreplayed() {
    let log = SessionLog::in_memory("s-1");
    let outcome = log.branch_from(7).expect("branches");
    assert!(!outcome.conversation_replayed);
    assert_eq!(outcome.replayed_messages, 0);
    let branch = log.sink().records(&outcome.session_id).expect("records");
    assert!(branch.iter().any(|record| matches!(
        record,
        SessionLogRecord::Branch { conversation_replayed: false, .. }
    )));
}

#[test]
fn branch_rejects_missing_checkpoint() {
    let log = SessionLog::in_memory("s-1");
    log.record_message(&message("before"), None).expect("records");
    let error = log.branch_from(9).expect_err("missing checkpoint rejected");
    assert!(error.to_string().contains("no log record for checkpoint"));
}

#[test]
fn file_sink_is_owner_only_and_roundtrips() {
    let root = tempfile::tempdir().expect("log root");
    let config =
        SessionLogConfig { dir: Some(root.path().to_path_buf()), ..SessionLogConfig::default() };
    let store = SessionLogStore::from_config(&config).expect("store");
    let log = store.log_for("s-1");
    log.record_message(&message("hello"), None).expect("records");
    log.record_checkpoint("s-1-0000000001", 1).expect("records");
    let replayed = log.replay_messages().expect("replays");
    assert_eq!(replayed[0].1, message("hello"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = root.path().join("s-1").join(SESSION_LOG_RECORD_FILE);
        assert_eq!(std::fs::metadata(path).expect("metadata").permissions().mode() & 0o777, 0o600);
        let dir = root.path().join("s-1");
        assert_eq!(std::fs::metadata(dir).expect("metadata").permissions().mode() & 0o777, 0o700);
    }
}

#[test]
fn file_sink_body_hash_validation_rejects_traversal() {
    let root = tempfile::tempdir().expect("log root");
    let sink = FileSessionLogSink::new(root.path().to_path_buf(), 1024);
    let error = sink.put_body("s-1", "../escape", b"x").expect_err("invalid hash rejected");
    assert!(error.to_string().contains("64 lowercase hex"));
}

#[test]
fn disabled_config_yields_no_store() {
    let config = SessionLogConfig { enabled: false, ..SessionLogConfig::default() };
    assert!(SessionLogStore::from_config(&config).is_none());
}
