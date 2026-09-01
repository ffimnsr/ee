//! Evidence-gated workspace fact candidate derivation.

use std::collections::{BTreeMap, BTreeSet};

use ee_agent_orchestrator::VerifiedSubagentReport;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::turn_evidence::{
    EvidenceCheck, EvidenceRecord, HostValidationRecord, PromptTerminalOutcome, TurnEvidence,
    TurnObservation, TurnTerminalStatus, reduce_terminal_state,
};

const VERIFIED_SOURCE_KIND: &str = "turn_evidence_validation";
const MAX_CANDIDATE_KEY_BYTES: usize = 128;
const MAX_CANDIDATE_VALUE_BYTES: usize = 512;

/// Fixed authority carried by evidence-derived workspace fact candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceVerifiedFactAuthority {
    HostVerified,
}

/// Fixed freshness carried by evidence-derived workspace fact candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceVerifiedFactFreshness {
    RevisionBound,
}

/// Narrow durable-fact candidate derived from sanitized host evidence.
///
/// Contains no evidence ledger, prompt, transcript, or terminal output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceVerifiedFactCandidate {
    pub key: String,
    pub value: String,
    pub authority: WorkspaceVerifiedFactAuthority,
    pub freshness: WorkspaceVerifiedFactFreshness,
    pub source_id: String,
    pub source_revision: String,
    pub source_fingerprint: String,
}

impl WorkspaceVerifiedFactCandidate {
    /// Derives parent-owned candidates after proof-carrying subagent report
    /// verification and independent successful parent validation.
    pub fn derive_parent_verified_subagent(
        child_evidence: &TurnEvidence,
        parent_evidence: &TurnEvidence,
        verified_report: &VerifiedSubagentReport,
    ) -> Result<Vec<Self>, WorkspaceVerifiedFactCandidateError> {
        derive_parent_verified_subagent_fact_candidates(
            child_evidence,
            parent_evidence,
            verified_report,
        )
    }
}

/// Opaque source identity observed by host lifecycle code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceVerifiedSourceIdentity {
    pub source_id: String,
    pub source_revision: String,
    pub source_fingerprint: String,
}

impl From<&WorkspaceVerifiedFactCandidate> for WorkspaceVerifiedSourceIdentity {
    fn from(candidate: &WorkspaceVerifiedFactCandidate) -> Self {
        Self {
            source_id: candidate.source_id.clone(),
            source_revision: candidate.source_revision.clone(),
            source_fingerprint: candidate.source_fingerprint.clone(),
        }
    }
}

/// Why host evidence cannot produce promotable candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceVerifiedFactCandidateError {
    TerminalEvidenceNotVerified,
    PromptNotCompleted,
    MissingCurrentRevision,
    MissingSelectedValidationRecord,
    InvalidSelectedValidationRecord,
    ChildTurnNotVerified,
    ParentTurnNotVerified,
    SubagentIdentityMismatch,
    ParentIdentityMismatch,
    RevisionMismatch,
    ParentDidNotVerifyClaim,
}

impl std::fmt::Display for WorkspaceVerifiedFactCandidateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::TerminalEvidenceNotVerified => "turn evidence is not completely verified",
            Self::PromptNotCompleted => "turn prompt did not complete successfully",
            Self::MissingCurrentRevision => "turn evidence has no current revision",
            Self::MissingSelectedValidationRecord => {
                "turn evidence has no selected successful validation record"
            }
            Self::InvalidSelectedValidationRecord => {
                "selected validation record cannot form a bounded candidate"
            }
            Self::ChildTurnNotVerified => "subagent turn did not complete with verified evidence",
            Self::ParentTurnNotVerified => "parent turn did not complete with verified evidence",
            Self::SubagentIdentityMismatch => {
                "verified subagent report does not identify the child evidence turn"
            }
            Self::ParentIdentityMismatch => "parent and subagent evidence identify the same agent",
            Self::RevisionMismatch => "parent and subagent evidence revisions do not match",
            Self::ParentDidNotVerifyClaim => {
                "parent did not independently verify any reported subagent fact"
            }
        };
        f.write_str(message)
    }
}

impl std::error::Error for WorkspaceVerifiedFactCandidateError {}

/// Derives deterministic, bounded candidates only from complete current-revision evidence.
pub fn derive_workspace_verified_fact_candidates(
    evidence: &TurnEvidence,
) -> Result<Vec<WorkspaceVerifiedFactCandidate>, WorkspaceVerifiedFactCandidateError> {
    let summary = reduce_terminal_state(evidence);
    if summary.status != TurnTerminalStatus::Verified {
        return Err(WorkspaceVerifiedFactCandidateError::TerminalEvidenceNotVerified);
    }
    if !matches!(
        evidence.records().iter().rev().find_map(|record| match record.observation() {
            TurnObservation::PromptTerminal { outcome } => Some(*outcome),
            _ => None,
        }),
        Some(PromptTerminalOutcome::Completed)
    ) {
        return Err(WorkspaceVerifiedFactCandidateError::PromptNotCompleted);
    }
    let revision = evidence
        .current_revision()
        .ok_or(WorkspaceVerifiedFactCandidateError::MissingCurrentRevision)?;

    let mut latest_by_run = BTreeMap::<&str, (&EvidenceRecord, &HostValidationRecord)>::new();
    for record in evidence.records() {
        if let TurnObservation::ValidationRecord {
            revision: record_revision,
            selected: true,
            record: validation,
        } = record.observation()
            && record_revision == revision
        {
            latest_by_run.insert(validation.run_id.as_str(), (record, validation));
        }
    }

    let selected = latest_by_run
        .into_values()
        .filter(|(_, validation)| validation.outcome == EvidenceCheck::Passed)
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Err(WorkspaceVerifiedFactCandidateError::MissingSelectedValidationRecord);
    }

    let source_id = source_identity(
        b"ee.workspace-verified-fact.source.v1\0",
        "turn-evidence:sha256:",
        evidence,
        revision.as_str(),
    );
    let source_fingerprint = source_identity(
        b"ee.workspace-verified-fact.fingerprint.v1\0",
        "sha256:",
        evidence,
        revision.as_str(),
    );
    let mut candidates = selected
        .into_iter()
        .map(|(_, validation)| {
            candidate(
                source_id.as_str(),
                source_fingerprint.as_str(),
                revision.as_str(),
                validation,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    candidates.sort_by(|left, right| {
        left.key
            .cmp(&right.key)
            .then_with(|| left.source_fingerprint.cmp(&right.source_fingerprint))
    });
    candidates.dedup();
    Ok(candidates)
}

/// Promotes subagent validation claims into parent-derived candidates only after
/// deterministic parent verification.
///
/// Child evidence must be successful, report must be proof-carrying output from
/// `SubagentReportVerifier`, and report identity plus exact claim must match child
/// evidence. Parent must then independently rerun matching selected validation
/// at same revision. Returned candidates derive solely from parent evidence, so
/// existing promotion revalidation can use `parent_evidence` without trusting
/// child provenance.
pub fn derive_parent_verified_subagent_fact_candidates(
    child_evidence: &TurnEvidence,
    parent_evidence: &TurnEvidence,
    verified_report: &VerifiedSubagentReport,
) -> Result<Vec<WorkspaceVerifiedFactCandidate>, WorkspaceVerifiedFactCandidateError> {
    let child_candidates = derive_workspace_verified_fact_candidates(child_evidence)
        .map_err(|_| WorkspaceVerifiedFactCandidateError::ChildTurnNotVerified)?;
    let parent_candidates = derive_workspace_verified_fact_candidates(parent_evidence)
        .map_err(|_| WorkspaceVerifiedFactCandidateError::ParentTurnNotVerified)?;

    if verified_report.report().subagent_id != child_evidence.key().agent_id() {
        return Err(WorkspaceVerifiedFactCandidateError::SubagentIdentityMismatch);
    }
    if parent_evidence.key().agent_id() == child_evidence.key().agent_id() {
        return Err(WorkspaceVerifiedFactCandidateError::ParentIdentityMismatch);
    }
    if child_evidence.current_revision() != parent_evidence.current_revision() {
        return Err(WorkspaceVerifiedFactCandidateError::RevisionMismatch);
    }

    let reported_claims = verified_report
        .report()
        .findings
        .iter()
        .map(|finding| (finding.key.as_str(), finding.claim.as_str()))
        .collect::<BTreeSet<_>>();
    let reported_child_keys = child_candidates
        .iter()
        .filter(|candidate| {
            reported_claims.contains(&(candidate.key.as_str(), candidate.value.as_str()))
        })
        .map(|candidate| candidate.key.as_str())
        .collect::<BTreeSet<_>>();

    let child_validations = selected_passing_validations(child_evidence)
        .into_iter()
        .filter(|validation| {
            let key = validation_candidate_key(validation);
            reported_child_keys.contains(key.as_str())
        })
        .map(validation_identity)
        .collect::<BTreeSet<_>>();
    let verified_parent_keys = selected_passing_validations(parent_evidence)
        .into_iter()
        .filter(|validation| child_validations.contains(&validation_identity(validation)))
        .map(validation_candidate_key)
        .collect::<BTreeSet<_>>();

    let candidates = parent_candidates
        .into_iter()
        .filter(|candidate| verified_parent_keys.contains(&candidate.key))
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Err(WorkspaceVerifiedFactCandidateError::ParentDidNotVerifyClaim);
    }
    Ok(candidates)
}

fn selected_passing_validations(evidence: &TurnEvidence) -> Vec<&HostValidationRecord> {
    let Some(revision) = evidence.current_revision() else {
        return Vec::new();
    };
    let mut latest_by_run = BTreeMap::<&str, &HostValidationRecord>::new();
    for record in evidence.records() {
        if let TurnObservation::ValidationRecord {
            revision: record_revision,
            selected: true,
            record: validation,
        } = record.observation()
            && record_revision == revision
        {
            latest_by_run.insert(validation.run_id.as_str(), validation);
        }
    }
    latest_by_run
        .into_values()
        .filter(|validation| validation.outcome == EvidenceCheck::Passed)
        .collect()
}

fn validation_identity(validation: &HostValidationRecord) -> (&str, &str) {
    (validation.command_id.as_str(), validation.command.as_str())
}

fn validation_candidate_key(validation: &HostValidationRecord) -> String {
    let key_digest = digest_fields(
        b"ee.workspace-verified-fact.key.v1\0",
        [validation.command_id.as_str(), validation.run_id.as_str()],
    );
    format!("validation.{}", &key_digest[..32])
}

fn candidate(
    source_id: &str,
    source_fingerprint: &str,
    revision: &str,
    validation: &HostValidationRecord,
) -> Result<WorkspaceVerifiedFactCandidate, WorkspaceVerifiedFactCandidateError> {
    if validation.run_id == "unknown"
        || validation.command_id == "unknown"
        || validation.command == "unknown"
        || validation.exit_status.is_some_and(|status| status != 0)
        || validation.skip_or_denial.is_some()
    {
        return Err(WorkspaceVerifiedFactCandidateError::InvalidSelectedValidationRecord);
    }

    let key = validation_candidate_key(validation);
    let value =
        format!("selected validation {} passed for revision {}", validation.command, revision);
    if key.len() > MAX_CANDIDATE_KEY_BYTES || value.len() > MAX_CANDIDATE_VALUE_BYTES {
        return Err(WorkspaceVerifiedFactCandidateError::InvalidSelectedValidationRecord);
    }
    Ok(WorkspaceVerifiedFactCandidate {
        key,
        value,
        authority: WorkspaceVerifiedFactAuthority::HostVerified,
        freshness: WorkspaceVerifiedFactFreshness::RevisionBound,
        source_id: source_id.to_string(),
        source_revision: revision.to_string(),
        source_fingerprint: source_fingerprint.to_string(),
    })
}

fn source_identity(domain: &[u8], prefix: &str, evidence: &TurnEvidence, revision: &str) -> String {
    let turn_id = evidence.key().turn_id().to_string();
    let digest = digest_fields(
        domain,
        [evidence.key().agent_id(), evidence.key().session_id(), turn_id.as_str(), revision]
            .into_iter()
            .chain(evidence.records().iter().map(EvidenceRecord::id)),
    );
    format!("{prefix}{digest}")
}

fn digest_fields<'a>(domain: &[u8], fields: impl IntoIterator<Item = &'a str>) -> String {
    let mut digest = Sha256::new();
    digest.update(domain);
    for field in fields {
        digest.update(field.as_bytes());
        digest.update([0]);
    }
    format!("{:x}", digest.finalize())
}

pub(crate) fn verified_source_kind() -> &'static str {
    VERIFIED_SOURCE_KIND
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn_evidence::{
        EvidenceRevision, TurnEvidenceStore, TurnObservation, WriteEvidenceOutcome,
    };
    use crate::workspace_memory::{
        WorkspaceMemoryHost, WorkspaceMemoryHostConfig, WorkspaceMemoryHostErrorCode,
        WorkspaceMemoryMutationApproval,
    };
    use ee_agent_orchestrator::{ReportEvidence, SubagentReport, SubagentReportVerifier};
    use serde_json::json;
    use tempfile::tempdir;

    fn revision(value: &str) -> EvidenceRevision {
        EvidenceRevision::new(value)
    }

    fn validation(
        revision: &EvidenceRevision,
        selected: bool,
        outcome: EvidenceCheck,
    ) -> TurnObservation {
        TurnObservation::ValidationRecord {
            revision: revision.clone(),
            selected,
            record: HostValidationRecord {
                run_id: "validation-run".to_string(),
                command_id: "cargo-test".to_string(),
                command: "cargo test --quiet".to_string(),
                tool: Some("terminal".to_string()),
                selector: Some("cargo-test".to_string()),
                outcome,
                exit_status: (outcome == EvidenceCheck::Passed).then_some(0),
                elapsed_ms: Some(10),
                affected_tests: vec!["host".to_string()],
                diagnostics_delta: 0,
                output_truncated: false,
                skip_or_denial: None,
            },
        }
    }

    fn evidence_with(observations: Vec<TurnObservation>) -> TurnEvidence {
        evidence_with_identity("agent", observations)
    }

    fn evidence_with_identity(
        agent_id: &str,
        mut observations: Vec<TurnObservation>,
    ) -> TurnEvidence {
        let mut store = TurnEvidenceStore::default();
        let key = store.start_turn(agent_id.to_string(), "session".to_string());
        for observation in observations.drain(..) {
            store.observe(key.turn_id(), observation).expect("known turn");
        }
        store.snapshot(key.turn_id()).expect("snapshot")
    }

    fn verified_report_for(
        candidate: &WorkspaceVerifiedFactCandidate,
        subagent_id: &str,
    ) -> VerifiedSubagentReport {
        let mut report = SubagentReport::new("investigator", subagent_id);
        report.findings = serde_json::from_value(json!([{
            "key": candidate.key,
            "claim": candidate.value,
            "kind": "observed",
            "evidence": [{ "tool": "terminal" }],
            "confidence": "high",
            "rejected_alternatives": [],
            "recommended_next_action": "rerun selected validation"
        }]))
        .expect("valid finding fixture");
        let mut observed = ReportEvidence::default();
        observed.tools.insert("terminal".to_string());
        SubagentReportVerifier.verify_and_accept(report, &observed).expect("report verified")
    }

    fn complete_observations() -> Vec<TurnObservation> {
        let current = revision("rev-1");
        vec![
            TurnObservation::Revision { revision: current.clone() },
            TurnObservation::Write {
                revision: current.clone(),
                outcome: WriteEvidenceOutcome::Applied,
            },
            TurnObservation::ChangedFiles {
                revision: current.clone(),
                files: vec!["src/lib.rs".to_string()],
                truncated: false,
            },
            TurnObservation::Diagnostics {
                revision: current.clone(),
                outcome: EvidenceCheck::Passed,
            },
            TurnObservation::DiffReview {
                revision: current.clone(),
                outcome: EvidenceCheck::Passed,
            },
            validation(&current, true, EvidenceCheck::Passed),
            TurnObservation::PromptTerminal { outcome: PromptTerminalOutcome::Completed },
        ]
    }

    #[test]
    fn complete_verified_evidence_derives_bounded_deterministic_candidate() {
        let evidence = evidence_with(complete_observations());
        let first = derive_workspace_verified_fact_candidates(&evidence).expect("candidate");
        let second = derive_workspace_verified_fact_candidates(&evidence).expect("candidate");

        assert_eq!(first, second);
        assert_eq!(first.len(), 1);
        let candidate = &first[0];
        assert_eq!(candidate.authority, WorkspaceVerifiedFactAuthority::HostVerified);
        assert_eq!(candidate.freshness, WorkspaceVerifiedFactFreshness::RevisionBound);
        assert_eq!(candidate.source_revision, revision("rev-1").as_str());
        assert!(candidate.key.len() <= MAX_CANDIDATE_KEY_BYTES);
        assert!(candidate.value.len() <= MAX_CANDIDATE_VALUE_BYTES);
        assert!(!candidate.value.contains("src/lib.rs"));
    }

    #[test]
    fn failed_cancelled_and_paused_turns_derive_nothing() {
        for outcome in [
            PromptTerminalOutcome::Failed,
            PromptTerminalOutcome::Cancelled,
            PromptTerminalOutcome::PausedRecoverable,
        ] {
            let mut observations = complete_observations();
            observations.pop();
            observations.push(TurnObservation::PromptTerminal { outcome });
            assert_eq!(
                derive_workspace_verified_fact_candidates(&evidence_with(observations)),
                Err(WorkspaceVerifiedFactCandidateError::TerminalEvidenceNotVerified)
            );
        }
    }

    #[test]
    fn missing_or_stale_revision_derives_nothing() {
        let current = revision("rev-1");
        let mut missing = complete_observations();
        missing.remove(0);
        assert!(derive_workspace_verified_fact_candidates(&evidence_with(missing)).is_err());

        let mut stale = complete_observations();
        stale[5] = validation(&revision("rev-old"), true, EvidenceCheck::Passed);
        assert!(derive_workspace_verified_fact_candidates(&evidence_with(stale)).is_err());
        assert_ne!(current, revision("rev-old"));
    }

    #[test]
    fn truncated_files_and_missing_diagnostics_or_diff_derive_nothing() {
        let mut truncated = complete_observations();
        if let TurnObservation::ChangedFiles { truncated, .. } = &mut truncated[2] {
            *truncated = true;
        }
        assert!(derive_workspace_verified_fact_candidates(&evidence_with(truncated)).is_err());

        for removed_index in [4, 3] {
            let mut observations = complete_observations();
            observations.remove(removed_index);
            assert!(
                derive_workspace_verified_fact_candidates(&evidence_with(observations)).is_err()
            );
        }
    }

    #[test]
    fn unselected_skipped_and_failed_validation_derive_nothing() {
        for (selected, outcome) in [
            (false, EvidenceCheck::Passed),
            (true, EvidenceCheck::Skipped),
            (true, EvidenceCheck::Failed),
        ] {
            let mut observations = complete_observations();
            observations[5] = validation(&revision("rev-1"), selected, outcome);
            assert!(
                derive_workspace_verified_fact_candidates(&evidence_with(observations)).is_err()
            );
        }
    }

    #[test]
    fn verified_without_explicit_prompt_completion_derives_nothing() {
        let mut observations = complete_observations();
        observations.pop();
        let evidence = evidence_with(observations);
        assert_eq!(reduce_terminal_state(&evidence).status, TurnTerminalStatus::Verified);
        assert_eq!(
            derive_workspace_verified_fact_candidates(&evidence),
            Err(WorkspaceVerifiedFactCandidateError::PromptNotCompleted)
        );
    }

    #[test]
    fn approved_promotion_is_compatible_and_source_change_marks_fact_stale() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("workspace");
        std::fs::create_dir_all(&root).expect("workspace root");
        let host = WorkspaceMemoryHost::new(&WorkspaceMemoryHostConfig {
            enabled: true,
            trusted_roots: vec![root],
            database_path: Some(temp.path().join("memory.sqlite3")),
            ..WorkspaceMemoryHostConfig::default()
        });
        let evidence = evidence_with(complete_observations());
        let candidate =
            derive_workspace_verified_fact_candidates(&evidence).expect("candidate").remove(0);

        let first = host
            .promote_verified_primary_approved(
                candidate.clone(),
                &evidence,
                WorkspaceMemoryMutationApproval::Approved,
            )
            .expect("approved promotion");
        let repeated = host
            .promote_verified_primary_approved(
                candidate.clone(),
                &evidence,
                WorkspaceMemoryMutationApproval::Approved,
            )
            .expect("compatible repeat");
        assert_eq!(
            first.fact.as_ref().map(|fact| &fact.id),
            repeated.fact.as_ref().map(|fact| &fact.id)
        );
        assert_eq!(first.fact.as_ref().map(|fact| fact.authority.as_str()), Some("host_verified"));
        assert_eq!(first.fact.as_ref().map(|fact| fact.freshness.as_str()), Some("revision_bound"));

        let unchanged = host
            .invalidate_verified_source(WorkspaceVerifiedSourceIdentity::from(&candidate))
            .expect("matching source remains current");
        assert_eq!(unchanged.affected, 0);

        let mut changed = WorkspaceVerifiedSourceIdentity::from(&candidate);
        changed.source_fingerprint = format!("sha256:{}", "0".repeat(64));
        let stale = host.invalidate_verified_source(changed).expect("source invalidated");
        assert_eq!(stale.affected, 1);
        assert_eq!(
            host.read_primary(candidate.key.clone()).expect_err("stale excluded").code,
            WorkspaceMemoryHostErrorCode::NotFound
        );
        assert!(host.recall_primary(candidate.key, 10).expect("recall").facts.is_empty());
    }

    #[test]
    fn parent_verified_subagent_fact_uses_parent_evidence_for_promotion() {
        let child = evidence_with_identity("child-agent", complete_observations());
        let child_candidate =
            derive_workspace_verified_fact_candidates(&child).expect("child candidate").remove(0);
        let report = verified_report_for(&child_candidate, child.key().agent_id());
        let parent = evidence_with_identity("parent-agent", complete_observations());

        let promoted = WorkspaceVerifiedFactCandidate::derive_parent_verified_subagent(
            &child, &parent, &report,
        )
        .expect("parent independently verified child claim");
        let parent_candidates =
            derive_workspace_verified_fact_candidates(&parent).expect("parent candidates");
        assert_eq!(promoted, parent_candidates);
        assert_ne!(promoted[0].source_id, child_candidate.source_id);

        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("workspace");
        std::fs::create_dir_all(&root).expect("workspace root");
        let host = WorkspaceMemoryHost::new(&WorkspaceMemoryHostConfig {
            enabled: true,
            trusted_roots: vec![root],
            database_path: Some(temp.path().join("memory.sqlite3")),
            ..WorkspaceMemoryHostConfig::default()
        });
        host.promote_verified_primary_approved(
            promoted[0].clone(),
            &parent,
            WorkspaceMemoryMutationApproval::Approved,
        )
        .expect("parent-derived proof remains promotable");
    }

    #[test]
    fn failed_or_cancelled_child_and_parent_turns_never_pass_parent_gate() {
        for outcome in [PromptTerminalOutcome::Failed, PromptTerminalOutcome::Cancelled] {
            let successful_child = evidence_with_identity("child-agent", complete_observations());
            let child_candidate = derive_workspace_verified_fact_candidates(&successful_child)
                .expect("child candidate")
                .remove(0);
            let report = verified_report_for(&child_candidate, successful_child.key().agent_id());

            let mut failed_observations = complete_observations();
            failed_observations.pop();
            failed_observations.push(TurnObservation::PromptTerminal { outcome });
            let failed_child = evidence_with_identity("child-agent", failed_observations.clone());
            let successful_parent = evidence_with_identity("parent-agent", complete_observations());
            assert_eq!(
                derive_parent_verified_subagent_fact_candidates(
                    &failed_child,
                    &successful_parent,
                    &report,
                ),
                Err(WorkspaceVerifiedFactCandidateError::ChildTurnNotVerified)
            );

            let failed_parent = evidence_with_identity("parent-agent", failed_observations);
            assert_eq!(
                derive_parent_verified_subagent_fact_candidates(
                    &successful_child,
                    &failed_parent,
                    &report,
                ),
                Err(WorkspaceVerifiedFactCandidateError::ParentTurnNotVerified)
            );
        }
    }

    #[test]
    fn parent_gate_rejects_wrong_child_identity_revision_and_unmatched_validation() {
        let child = evidence_with_identity("child-agent", complete_observations());
        let child_candidate =
            derive_workspace_verified_fact_candidates(&child).expect("child candidate").remove(0);
        let wrong_report = verified_report_for(&child_candidate, "other-child");
        let parent = evidence_with_identity("parent-agent", complete_observations());
        assert_eq!(
            derive_parent_verified_subagent_fact_candidates(&child, &parent, &wrong_report),
            Err(WorkspaceVerifiedFactCandidateError::SubagentIdentityMismatch)
        );

        let report = verified_report_for(&child_candidate, child.key().agent_id());
        let mut other_revision = complete_observations();
        for observation in &mut other_revision {
            match observation {
                TurnObservation::Revision { revision }
                | TurnObservation::Write { revision, .. }
                | TurnObservation::ChangedFiles { revision, .. }
                | TurnObservation::Diagnostics { revision, .. }
                | TurnObservation::DiffReview { revision, .. }
                | TurnObservation::ValidationRecord { revision, .. } => {
                    *revision = self::revision("rev-2");
                }
                _ => {}
            }
        }
        let other_revision_parent = evidence_with_identity("parent-agent", other_revision);
        assert_eq!(
            derive_parent_verified_subagent_fact_candidates(
                &child,
                &other_revision_parent,
                &report,
            ),
            Err(WorkspaceVerifiedFactCandidateError::RevisionMismatch)
        );

        let mut unmatched = complete_observations();
        if let TurnObservation::ValidationRecord { record, .. } = &mut unmatched[5] {
            record.command_id = "cargo-clippy".to_string();
            record.command = "cargo clippy".to_string();
        }
        let unmatched_parent = evidence_with_identity("parent-agent", unmatched);
        assert_eq!(
            derive_parent_verified_subagent_fact_candidates(&child, &unmatched_parent, &report),
            Err(WorkspaceVerifiedFactCandidateError::ParentDidNotVerifyClaim)
        );
    }

    #[test]
    fn promotion_revalidates_proof_and_never_overwrites_conflict() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("workspace");
        std::fs::create_dir_all(&root).expect("workspace root");
        let host = WorkspaceMemoryHost::new(&WorkspaceMemoryHostConfig {
            enabled: true,
            trusted_roots: vec![root],
            database_path: Some(temp.path().join("memory.sqlite3")),
            ..WorkspaceMemoryHostConfig::default()
        });
        let evidence = evidence_with(complete_observations());
        let candidate =
            derive_workspace_verified_fact_candidates(&evidence).expect("candidate").remove(0);

        let mut tampered = candidate.clone();
        tampered.value.push_str(" tampered");
        assert_eq!(
            host.promote_verified_primary_approved(
                tampered,
                &evidence,
                WorkspaceMemoryMutationApproval::Approved,
            )
            .expect_err("proof mismatch")
            .code,
            WorkspaceMemoryHostErrorCode::InvalidFact
        );

        host.remember_primary_approved(
            candidate.key.clone(),
            "conflicting user assertion".to_string(),
            WorkspaceMemoryMutationApproval::Approved,
        )
        .expect("existing fact");
        assert_eq!(
            host.promote_verified_primary_approved(
                candidate.clone(),
                &evidence,
                WorkspaceMemoryMutationApproval::Approved,
            )
            .expect_err("conflict")
            .code,
            WorkspaceMemoryHostErrorCode::Conflict
        );
        assert_eq!(
            host.read_primary(candidate.key).expect("existing preserved").value,
            "conflicting user assertion"
        );
    }
}
