use ee_agent_orchestrator::{
    AppliedWrite, BufferOwnership, CompletionReport, CompletionState, RollbackSafetyCheck,
    SourceRevision, TransactionDiagnostics, TransactionFinalDiff, TransactionValidation,
    ValidationOutcome, ValidationRecord, WriteApproval, WritePreview, WriteTransaction,
    WriteTransactionState,
};
use serde::de::DeserializeOwned;
use serde_json::json;

fn decode<T: DeserializeOwned>(value: serde_json::Value) -> T {
    serde_json::from_value(value).expect("public transaction fixture decodes")
}

fn source(path: &str, revision: &str) -> SourceRevision {
    SourceRevision::clean(path, revision)
}

fn preview(path: &str, revision: &str) -> WritePreview {
    decode(
        json!({ "path": path, "expected_revision": revision, "summary": format!("preview {path}") }),
    )
}

fn applied(path: &str, expected_revision: &str, post_write_revision: Option<&str>) -> AppliedWrite {
    decode(json!({
        "path": path,
        "expected_revision": expected_revision,
        "post_write_revision": post_write_revision,
        "agent_owned": true,
        "applied": post_write_revision.is_some(),
        "failure_reason": if post_write_revision.is_some() { None } else { Some("concurrent edit") },
    }))
}

fn diagnostics(errors_after: u32) -> TransactionDiagnostics {
    decode(json!({
        "evidence_id": "diagnostics-2",
        "workspace_revision": "workspace-2",
        "errors_before": 0,
        "errors_after": errors_after,
        "warnings_before": 0,
        "warnings_after": 0,
    }))
}

fn approved_transaction(paths: &[(&str, &str)]) -> WriteTransaction {
    let sources = paths.iter().map(|(path, revision)| source(path, revision)).collect::<Vec<_>>();
    let mut transaction =
        WriteTransaction::begin("tx-integration", sources.clone()).expect("begin");
    transaction.record_read_revisions(sources).expect("read revisions");
    transaction
        .record_preview(paths.iter().map(|(path, revision)| preview(path, revision)))
        .expect("preview");
    transaction
        .record_approval(decode::<WriteApproval>(
            json!({ "result": "approved", "approval_id": "approval-1" }),
        ))
        .expect("approval");
    transaction
}

fn applied_transaction() -> WriteTransaction {
    let mut transaction = approved_transaction(&[("/work/a.rs", "source-1")]);
    transaction
        .record_apply([applied("/work/a.rs", "source-1", Some("path-2"))], "workspace-2")
        .expect("apply");
    transaction
}

#[test]
fn concurrent_user_edit_stops_before_mutation() {
    let mut transaction =
        WriteTransaction::begin("tx-integration", [source("/work/a.rs", "source-1")])
            .expect("begin");
    let dirty_user_source: SourceRevision = decode(json!({
        "path": "/work/a.rs",
        "revision": "source-1",
        "dirty": true,
        "ownership": BufferOwnership::User,
    }));
    let error = transaction
        .record_read_revisions([dirty_user_source])
        .expect_err("user-owned dirty buffer blocks");

    assert_eq!(error.code, "dirty_user_buffer");
    assert_eq!(transaction.state, WriteTransactionState::Blocked);
    assert!(transaction.applies.is_empty());
}

#[test]
fn denied_approval_leaves_no_apply_evidence() {
    let mut transaction =
        WriteTransaction::begin("tx-denied", [source("/work/a.rs", "source-1")]).expect("begin");
    transaction.record_read_revisions([source("/work/a.rs", "source-1")]).expect("read");
    transaction.record_preview([preview("/work/a.rs", "source-1")]).expect("preview");
    let error = transaction
        .record_approval(decode::<WriteApproval>(
            json!({ "result": "denied", "reason": "user declined" }),
        ))
        .expect_err("denial");

    assert_eq!(error.code, "approval_denied");
    assert_eq!(transaction.state, WriteTransactionState::Blocked);
    assert!(transaction.applies.is_empty());
}

#[test]
fn partial_multi_file_apply_preserves_successful_path_and_blocks() {
    let mut transaction = approved_transaction(&[("/work/a.rs", "a-1"), ("/work/b.rs", "b-1")]);
    let mut failed = applied("/work/b.rs", "b-1", None);
    failed.agent_owned = false;
    let error = transaction
        .record_apply([applied("/work/a.rs", "a-1", Some("a-2")), failed], "workspace-2")
        .expect_err("partial apply");

    assert_eq!(error.code, "partial_apply");
    assert_eq!(transaction.changed_paths, vec!["/work/a.rs"]);
    assert_eq!(transaction.state, WriteTransactionState::Blocked);
}

#[test]
fn diagnostic_regression_blocks_pre_write_evidence_from_verifying() {
    let mut transaction = applied_transaction();
    let error = transaction.record_diagnostics(diagnostics(1)).expect_err("regression");
    assert_eq!(error.code, "diagnostic_regression");

    let mut false_success: CompletionReport = decode(json!({
        "state": CompletionState::Verified,
        "blocker": null,
        "safe_follow_up": null,
        "evidence_ids": ["pre-write-diagnostics", "pre-write-diff"],
    }));
    transaction.constrain_completion(&mut false_success);
    assert_eq!(false_success.state, CompletionState::Blocked);
    assert!(false_success.evidence_ids.iter().any(|id| id == "transaction:tx-integration"));
}

#[test]
fn interruption_preserves_evidence_for_manual_recovery_without_replay() {
    let mut transaction = applied_transaction();
    transaction.interrupt("transport interrupted after apply");

    assert_eq!(transaction.state, WriteTransactionState::Unverified);
    assert_eq!(transaction.applies.len(), 1);
    assert_eq!(transaction.post_write_workspace_revision.as_deref(), Some("workspace-2"));
    assert!(transaction.diagnostics.is_none());
    assert!(
        transaction.record_diagnostics(diagnostics(0)).is_err(),
        "recovery must restart fresh sequence"
    );
}

#[test]
fn rollback_requires_explicit_agent_owned_revision_safe_evidence() {
    let mut transaction = applied_transaction();
    transaction.interrupt("validation unavailable");
    let safety: RollbackSafetyCheck = decode(json!({
        "approval_id": "rollback-1",
        "no_later_user_edits": true,
        "workspace_revision": "workspace-2",
    }));
    transaction.prepare_rollback(safety).expect("safe rollback preparation");
    assert!(transaction.record_rollback("workspace-3").is_err(), "newer revision blocks rollback");
    transaction.record_rollback("workspace-2").expect("rollback exact revision");
    assert_eq!(transaction.state, WriteTransactionState::RolledBack);
}

#[test]
fn fresh_diff_and_selected_validation_complete_transaction() {
    let mut transaction = applied_transaction();
    transaction.record_diagnostics(diagnostics(0)).expect("diagnostics");
    let final_diff: TransactionFinalDiff = decode(json!({
        "evidence_id": "diff-2",
        "workspace_revision": "workspace-2",
        "changed_paths": ["/work/a.rs"],
        "passed": true,
        "detail": null,
    }));
    transaction.record_final_diff(final_diff).expect("diff");
    let record = ValidationRecord::evidence(
        "validation-2",
        "cargo test --quiet",
        ValidationOutcome::Passed,
        Some("terminal".into()),
        Some(0),
        Some(10),
        Vec::new(),
        0,
        false,
        None,
        Some("workspace-2".into()),
        true,
        false,
        None,
    );
    let validation: TransactionValidation = decode(json!({
        "transaction_id": "tx-integration",
        "record": record,
    }));
    transaction.record_validation(validation).expect("validation");

    assert_eq!(transaction.state, WriteTransactionState::Verified);
}
