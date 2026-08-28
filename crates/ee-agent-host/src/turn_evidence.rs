//! Host-owned, bounded evidence for ACP turn completion.
//!
//! ACP prompt completion only says that a transport request ended. This module
//! records editor-observed facts separately, then derives a completion state
//! without consulting model prose, plans, or ACP stop reasons.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::redact::is_secret_key;

/// Maximum changed-file names retained in one inventory observation.
pub const MAX_EVIDENCE_FILES: usize = 64;
/// Maximum observations retained for one turn.
pub const MAX_TURN_OBSERVATIONS: usize = 128;

/// Stable identity for one host turn.
///
/// `turn_id` is monotonic only within this `(agent_id, session_id)` pair.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TurnKey {
    agent_id: String,
    session_id: String,
    turn_id: u64,
}

impl TurnKey {
    #[must_use]
    pub(crate) fn new(agent_id: String, session_id: String, turn_id: u64) -> Self {
        Self {
            agent_id: sanitize_identifier(&agent_id),
            session_id: sanitize_identifier(&session_id),
            turn_id,
        }
    }

    #[must_use]
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    #[must_use]
    pub fn turn_id(&self) -> u64 {
        self.turn_id
    }
}

/// Opaque editor revision. This is a bounded identity, never file contents or
/// a user prompt.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EvidenceRevision(String);

impl EvidenceRevision {
    /// Creates a redacted, bounded revision identity.
    #[must_use]
    pub fn new(revision: impl AsRef<str>) -> Self {
        Self(sanitize_identifier(revision.as_ref()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Result of one host-observed check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceCheck {
    Passed,
    Failed,
    Skipped,
    Unavailable,
    Denied,
}

/// Approval and application outcome for a proposed write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteEvidenceOutcome {
    Approved,
    Applied,
    /// The approved request already matched the editor buffer. This is kept
    /// distinct from an applied mutation and cannot fabricate an inventory.
    NoOp,
    Denied,
    Failed,
    Conflicted,
}

/// Bounded validation record observed by the editor host. Identifiers are
/// redacted before storage; raw command output and free-form tool text are not
/// accepted by this type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostValidationRecord {
    /// Stable host-assigned identity for one selected validation lifecycle.
    /// Later observations for this same run replace provisional outcomes in
    /// reduction while preserving immutable audit records.
    #[serde(default = "default_validation_run_id")]
    pub run_id: String,
    pub command_id: String,
    pub command: String,
    pub tool: Option<String>,
    /// Host-derived command selector used to associate terminal lifecycle
    /// facts without storing terminal output.
    #[serde(default)]
    pub selector: Option<String>,
    pub outcome: EvidenceCheck,
    pub exit_status: Option<i32>,
    pub elapsed_ms: Option<u64>,
    pub affected_tests: Vec<String>,
    pub diagnostics_delta: i64,
    pub output_truncated: bool,
    pub skip_or_denial: Option<String>,
}

/// Host-observed stage for a revision-bound write transaction. This is an
/// audit fact only; it never grants the server ownership of editor buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteTransactionStage {
    Read,
    Preview,
    Approval,
    Apply,
    Diagnostics,
    FinalDiff,
    Validation,
    Interrupted,
    RollbackSafety,
}

/// Why a host turn stopped, recorded separately from the wire lifecycle
/// event. It contains no model or tool text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptTerminalOutcome {
    Completed,
    Cancelled,
    Failed,
    PausedRecoverable,
}

/// One bounded editor observation. Host storage sanitizes inputs, so callers
/// cannot persist prompt text or raw terminal output through this API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TurnObservation {
    /// Base or current editor/workspace revision. Later revisions invalidate
    /// earlier revision-bound observations; they are never rewritten.
    Revision { revision: EvidenceRevision },
    /// Complete changed-file inventory for a revision. A truncated inventory
    /// cannot prove verification.
    ChangedFiles { revision: EvidenceRevision, files: Vec<String>, truncated: bool },
    /// Host approval or write-application result.
    Write { revision: EvidenceRevision, outcome: WriteEvidenceOutcome },
    /// Diagnostics collected after writing.
    Diagnostics { revision: EvidenceRevision, outcome: EvidenceCheck },
    /// Final host/editor diff review.
    DiffReview { revision: EvidenceRevision, outcome: EvidenceCheck },
    /// A selected validation command/tool result. Only a selected passing
    /// validation can contribute to `verified`.
    Validation { revision: EvidenceRevision, selected: bool, outcome: EvidenceCheck },
    /// Complete bounded validation metadata for final-response and audit use.
    ValidationRecord { revision: EvidenceRevision, selected: bool, record: HostValidationRecord },
    /// Append-only write transaction lifecycle fact observed by the host.
    WriteTransaction {
        revision: EvidenceRevision,
        stage: WriteTransactionStage,
        outcome: EvidenceCheck,
    },
    /// Host-observed prompt lifecycle; does not use model-declared success.
    PromptTerminal { outcome: PromptTerminalOutcome },
}

/// One immutable observation plus its generated opaque evidence id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRecord {
    id: String,
    observation: TurnObservation,
}

impl EvidenceRecord {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn observation(&self) -> &TurnObservation {
        &self.observation
    }
}

/// Immutable snapshot of evidence collected for one turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnEvidence {
    key: TurnKey,
    base_revision: Option<EvidenceRevision>,
    current_revision: Option<EvidenceRevision>,
    records: Vec<EvidenceRecord>,
    /// Set when a non-terminal observation was safely omitted to preserve the
    /// reserved prompt-terminal slot. It is intentionally visible in reduction.
    #[serde(default)]
    observation_capacity_exhausted: bool,
}

impl TurnEvidence {
    fn new(key: TurnKey) -> Self {
        Self {
            key,
            base_revision: None,
            current_revision: None,
            records: Vec::new(),
            observation_capacity_exhausted: false,
        }
    }

    #[must_use]
    pub fn key(&self) -> &TurnKey {
        &self.key
    }

    #[must_use]
    pub fn base_revision(&self) -> Option<&EvidenceRevision> {
        self.base_revision.as_ref()
    }

    #[must_use]
    pub fn current_revision(&self) -> Option<&EvidenceRevision> {
        self.current_revision.as_ref()
    }

    #[must_use]
    pub fn records(&self) -> &[EvidenceRecord] {
        &self.records
    }
}

/// Terminal verification state derived exclusively from [`TurnEvidence`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnTerminalStatus {
    Verified,
    PartiallyVerified,
    Blocked,
    Unverified,
}

/// Bounded machine-readable reason why evidence cannot establish verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnBlocker {
    PromptCancelled,
    PromptFailed,
    PromptPausedRecoverable,
    WriteDenied,
    WriteFailed,
    WriteConflicted,
    DiagnosticsFailed,
    DiffReviewFailed,
    ValidationFailed,
    StaleRevision,
    ConflictingEvidence,
    MissingRevision,
    MissingChangedFiles,
    MissingDiagnostics,
    MissingDiffReview,
    MissingSelectedValidation,
    ValidationSkipped,
    ValidationUnavailable,
    ValidationDenied,
    ObservationCapacityExhausted,
}

/// Safe next action for a UI, MCP surface, or final-response builder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafeFollowUp {
    CollectCurrentRevision,
    CollectChangedFiles,
    RunDiagnostics,
    ReviewDiff,
    RunSelectedValidation,
    RequestWriteApproval,
    ResolveWriteConflict,
    RefreshEvidence,
    ResumeOrDiscard,
    StartNewTurn,
    ReportEvidence,
}

/// Transport-safe completion payload. It can cross the host/MCP boundary but
/// is not an ACP wire type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnEvidenceSummary {
    pub key: TurnKey,
    pub status: TurnTerminalStatus,
    pub blocker: Option<TurnBlocker>,
    pub safe_follow_up: SafeFollowUp,
    pub evidence_ids: Vec<String>,
}

/// Append-only in-memory evidence ledger for one ACP session.
#[derive(Debug, Default)]
pub(crate) struct TurnEvidenceStore {
    next_turn_id: u64,
    turns: BTreeMap<u64, TurnEvidence>,
}

impl TurnEvidenceStore {
    pub(crate) fn start_turn(&mut self, agent_id: String, session_id: String) -> TurnKey {
        self.next_turn_id = self.next_turn_id.saturating_add(1);
        let key = TurnKey::new(agent_id, session_id, self.next_turn_id);
        self.turns.insert(key.turn_id(), TurnEvidence::new(key.clone()));
        key
    }

    pub(crate) fn observe(
        &mut self,
        turn_id: u64,
        observation: TurnObservation,
    ) -> Result<TurnEvidenceSummary, TurnEvidenceError> {
        let evidence =
            self.turns.get_mut(&turn_id).ok_or(TurnEvidenceError::UnknownTurn(turn_id))?;
        let observation = sanitize_observation(observation);
        // Keep one slot for the ACP prompt terminal fact. Omitted bridge facts
        // set a durable blocker instead of silently disappearing; prompt
        // completion remains auditable even when a noisy turn exhausts budget.
        let is_prompt_terminal = matches!(observation, TurnObservation::PromptTerminal { .. });
        if evidence.records.len() >= MAX_TURN_OBSERVATIONS
            || (!is_prompt_terminal && evidence.records.len() >= MAX_TURN_OBSERVATIONS - 1)
        {
            evidence.observation_capacity_exhausted = true;
            return Ok(reduce_terminal_state(evidence));
        }
        if let TurnObservation::Revision { revision } = &observation {
            if evidence.base_revision.is_none() {
                evidence.base_revision = Some(revision.clone());
            }
            evidence.current_revision = Some(revision.clone());
        }
        let record_number = evidence.records.len() + 1;
        evidence.records.push(EvidenceRecord {
            id: format!("turn:{}:evidence:{record_number}", turn_id),
            observation,
        });
        Ok(reduce_terminal_state(evidence))
    }

    pub(crate) fn snapshot(&self, turn_id: u64) -> Option<TurnEvidence> {
        self.turns.get(&turn_id).cloned()
    }

    pub(crate) fn summary(&self, turn_id: u64) -> Option<TurnEvidenceSummary> {
        self.turns.get(&turn_id).map(reduce_terminal_state)
    }
}

/// Error from an evidence-store operation. Does not expose untrusted content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEvidenceError {
    UnknownTurn(u64),
    ObservationLimitExceeded { turn_id: u64 },
}

impl std::fmt::Display for TurnEvidenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTurn(turn_id) => write!(f, "unknown evidence turn {turn_id}"),
            Self::ObservationLimitExceeded { turn_id } => {
                write!(f, "evidence observation limit exceeded for turn {turn_id}")
            }
        }
    }
}

impl std::error::Error for TurnEvidenceError {}

/// Reduces immutable host observations into a terminal verification state.
#[must_use]
pub fn reduce_terminal_state(evidence: &TurnEvidence) -> TurnEvidenceSummary {
    let current_revision = evidence.current_revision.as_ref();
    let ids = evidence.records.iter().map(|record| record.id.clone()).collect();
    let summary = |status, blocker, safe_follow_up| TurnEvidenceSummary {
        key: evidence.key.clone(),
        status,
        blocker,
        safe_follow_up,
        evidence_ids: ids,
    };

    if evidence.observation_capacity_exhausted {
        return summary(
            TurnTerminalStatus::Blocked,
            Some(TurnBlocker::ObservationCapacityExhausted),
            SafeFollowUp::StartNewTurn,
        );
    }

    // Resume keeps one append-only evidence ledger. Earlier pause/failure facts
    // remain auditable, but only newest prompt lifecycle fact describes current
    // transport state.
    let latest_prompt_terminal = evidence.records.iter().rev().find_map(|record| {
        if let TurnObservation::PromptTerminal { outcome } = record.observation {
            Some(outcome)
        } else {
            None
        }
    });
    match latest_prompt_terminal {
        Some(PromptTerminalOutcome::Cancelled) => {
            return summary(
                TurnTerminalStatus::Blocked,
                Some(TurnBlocker::PromptCancelled),
                SafeFollowUp::StartNewTurn,
            );
        }
        Some(PromptTerminalOutcome::Failed) => {
            return summary(
                TurnTerminalStatus::Blocked,
                Some(TurnBlocker::PromptFailed),
                SafeFollowUp::StartNewTurn,
            );
        }
        Some(PromptTerminalOutcome::PausedRecoverable) => {
            return summary(
                TurnTerminalStatus::Blocked,
                Some(TurnBlocker::PromptPausedRecoverable),
                SafeFollowUp::ResumeOrDiscard,
            );
        }
        Some(PromptTerminalOutcome::Completed) | None => {}
    }

    let Some(current_revision) = current_revision else {
        return summary(
            TurnTerminalStatus::Unverified,
            Some(TurnBlocker::MissingRevision),
            SafeFollowUp::CollectCurrentRevision,
        );
    };

    let mut changed_files = Vec::new();
    let mut diagnostics = Vec::new();
    let mut diff_reviews = Vec::new();
    let mut validations = Vec::new();
    let mut validation_runs = BTreeMap::new();
    let mut has_stale_verification_evidence = false;

    for record in &evidence.records {
        match &record.observation {
            TurnObservation::ChangedFiles { revision, .. }
            | TurnObservation::Diagnostics { revision, .. }
            | TurnObservation::DiffReview { revision, .. }
            | TurnObservation::Validation { revision, .. }
            | TurnObservation::ValidationRecord { revision, .. }
                if revision != current_revision =>
            {
                has_stale_verification_evidence = true;
            }
            TurnObservation::ChangedFiles { revision, files, truncated }
                if revision == current_revision =>
            {
                changed_files.push((files, *truncated))
            }
            TurnObservation::Diagnostics { revision, outcome } if revision == current_revision => {
                diagnostics.push(*outcome);
            }
            TurnObservation::DiffReview { revision, outcome } if revision == current_revision => {
                diff_reviews.push(*outcome);
            }
            TurnObservation::Validation { revision, selected, outcome }
                if revision == current_revision && *selected =>
            {
                validations.push(*outcome)
            }
            TurnObservation::ValidationRecord { revision, selected, record }
                if revision == current_revision && *selected =>
            {
                // A selected run commonly starts as pending/skipped, then gets
                // a terminal result. Only its latest lifecycle fact decides.
                validation_runs.insert(record.run_id.clone(), record.outcome);
            }
            TurnObservation::WriteTransaction { revision, stage, outcome }
                if revision == current_revision
                    && matches!(stage, WriteTransactionStage::Apply)
                    && matches!(outcome, EvidenceCheck::Failed | EvidenceCheck::Denied) =>
            {
                return summary(
                    TurnTerminalStatus::Blocked,
                    Some(TurnBlocker::WriteFailed),
                    SafeFollowUp::RefreshEvidence,
                );
            }
            TurnObservation::Write { revision, outcome } if revision == current_revision => {
                match outcome {
                    WriteEvidenceOutcome::Denied => {
                        return summary(
                            TurnTerminalStatus::Blocked,
                            Some(TurnBlocker::WriteDenied),
                            SafeFollowUp::RequestWriteApproval,
                        );
                    }
                    WriteEvidenceOutcome::Failed => {
                        return summary(
                            TurnTerminalStatus::Blocked,
                            Some(TurnBlocker::WriteFailed),
                            SafeFollowUp::RefreshEvidence,
                        );
                    }
                    WriteEvidenceOutcome::Conflicted => {
                        return summary(
                            TurnTerminalStatus::Blocked,
                            Some(TurnBlocker::WriteConflicted),
                            SafeFollowUp::ResolveWriteConflict,
                        );
                    }
                    WriteEvidenceOutcome::Approved
                    | WriteEvidenceOutcome::Applied
                    | WriteEvidenceOutcome::NoOp => {}
                }
            }
            _ => {}
        }
    }

    validations.extend(validation_runs.into_values());

    if changed_files.len() > 1 && changed_files.windows(2).any(|window| window[0] != window[1]) {
        return summary(
            TurnTerminalStatus::Blocked,
            Some(TurnBlocker::ConflictingEvidence),
            SafeFollowUp::RefreshEvidence,
        );
    }
    if diagnostics.contains(&EvidenceCheck::Failed) {
        return summary(
            TurnTerminalStatus::Blocked,
            Some(TurnBlocker::DiagnosticsFailed),
            SafeFollowUp::RefreshEvidence,
        );
    }
    if diff_reviews.contains(&EvidenceCheck::Failed) {
        return summary(
            TurnTerminalStatus::Blocked,
            Some(TurnBlocker::DiffReviewFailed),
            SafeFollowUp::RefreshEvidence,
        );
    }
    if validations.contains(&EvidenceCheck::Failed) {
        return summary(
            TurnTerminalStatus::Blocked,
            Some(TurnBlocker::ValidationFailed),
            SafeFollowUp::RefreshEvidence,
        );
    }
    if validations.contains(&EvidenceCheck::Skipped) {
        return summary(
            TurnTerminalStatus::Blocked,
            Some(TurnBlocker::ValidationSkipped),
            SafeFollowUp::RunSelectedValidation,
        );
    }
    if validations.contains(&EvidenceCheck::Unavailable) {
        return summary(
            TurnTerminalStatus::Blocked,
            Some(TurnBlocker::ValidationUnavailable),
            SafeFollowUp::RunSelectedValidation,
        );
    }
    if validations.contains(&EvidenceCheck::Denied) {
        return summary(
            TurnTerminalStatus::Blocked,
            Some(TurnBlocker::ValidationDenied),
            SafeFollowUp::RequestWriteApproval,
        );
    }

    let changed_files_current = changed_files.first();
    let diagnostics_passed = diagnostics.contains(&EvidenceCheck::Passed);
    let diff_review_passed = diff_reviews.contains(&EvidenceCheck::Passed);
    let validation_passed = validations.contains(&EvidenceCheck::Passed);
    let positive_evidence =
        changed_files_current.is_some() || diagnostics_passed || diff_review_passed;
    let current_evidence_complete = changed_files_current.is_some_and(|(_, truncated)| !*truncated)
        && diagnostics_passed
        && diff_review_passed
        && validation_passed;

    if has_stale_verification_evidence && !current_evidence_complete {
        return summary(
            TurnTerminalStatus::Blocked,
            Some(TurnBlocker::StaleRevision),
            SafeFollowUp::RefreshEvidence,
        );
    }

    if changed_files_current.is_none() {
        return summary(
            if positive_evidence {
                TurnTerminalStatus::PartiallyVerified
            } else {
                TurnTerminalStatus::Unverified
            },
            Some(if has_stale_verification_evidence {
                TurnBlocker::StaleRevision
            } else {
                TurnBlocker::MissingChangedFiles
            }),
            SafeFollowUp::CollectChangedFiles,
        );
    }
    if changed_files_current.is_some_and(|(_, truncated)| *truncated) {
        return summary(
            TurnTerminalStatus::PartiallyVerified,
            Some(TurnBlocker::MissingChangedFiles),
            SafeFollowUp::CollectChangedFiles,
        );
    }
    if !diagnostics_passed {
        return summary(
            TurnTerminalStatus::PartiallyVerified,
            Some(if has_stale_verification_evidence {
                TurnBlocker::StaleRevision
            } else {
                TurnBlocker::MissingDiagnostics
            }),
            SafeFollowUp::RunDiagnostics,
        );
    }
    if !diff_review_passed {
        return summary(
            TurnTerminalStatus::PartiallyVerified,
            Some(if has_stale_verification_evidence {
                TurnBlocker::StaleRevision
            } else {
                TurnBlocker::MissingDiffReview
            }),
            SafeFollowUp::ReviewDiff,
        );
    }
    if !validation_passed {
        return summary(
            TurnTerminalStatus::PartiallyVerified,
            Some(if has_stale_verification_evidence {
                TurnBlocker::StaleRevision
            } else {
                TurnBlocker::MissingSelectedValidation
            }),
            SafeFollowUp::RunSelectedValidation,
        );
    }

    summary(TurnTerminalStatus::Verified, None, SafeFollowUp::ReportEvidence)
}

fn sanitize_observation(observation: TurnObservation) -> TurnObservation {
    match observation {
        TurnObservation::Revision { revision } => {
            TurnObservation::Revision { revision: EvidenceRevision::new(revision.as_str()) }
        }
        TurnObservation::ChangedFiles { revision, files, truncated } => {
            let input_was_truncated = files.len() > MAX_EVIDENCE_FILES;
            let mut files = files
                .into_iter()
                .take(MAX_EVIDENCE_FILES)
                .map(|file| sanitize_identifier(&file))
                .collect::<Vec<_>>();
            files.sort();
            files.dedup();
            TurnObservation::ChangedFiles {
                revision: EvidenceRevision::new(revision.as_str()),
                truncated: truncated || input_was_truncated,
                files,
            }
        }
        TurnObservation::Write { revision, outcome } => {
            TurnObservation::Write { revision: EvidenceRevision::new(revision.as_str()), outcome }
        }
        TurnObservation::Diagnostics { revision, outcome } => TurnObservation::Diagnostics {
            revision: EvidenceRevision::new(revision.as_str()),
            outcome,
        },
        TurnObservation::DiffReview { revision, outcome } => TurnObservation::DiffReview {
            revision: EvidenceRevision::new(revision.as_str()),
            outcome,
        },
        TurnObservation::Validation { revision, selected, outcome } => {
            TurnObservation::Validation {
                revision: EvidenceRevision::new(revision.as_str()),
                selected,
                outcome,
            }
        }
        TurnObservation::ValidationRecord { revision, selected, record } => {
            TurnObservation::ValidationRecord {
                revision: EvidenceRevision::new(revision.as_str()),
                selected,
                record: HostValidationRecord {
                    run_id: sanitize_identifier(&record.run_id),
                    command_id: sanitize_identifier(&record.command_id),
                    command: sanitize_identifier(&record.command),
                    tool: record.tool.as_deref().map(sanitize_identifier),
                    selector: record.selector.as_deref().map(sanitize_identifier),
                    outcome: record.outcome,
                    exit_status: record.exit_status,
                    elapsed_ms: record.elapsed_ms,
                    affected_tests: record
                        .affected_tests
                        .into_iter()
                        .take(MAX_EVIDENCE_FILES)
                        .map(|test| sanitize_identifier(&test))
                        .collect(),
                    diagnostics_delta: record.diagnostics_delta,
                    output_truncated: record.output_truncated,
                    skip_or_denial: record.skip_or_denial.as_deref().map(sanitize_identifier),
                },
            }
        }
        TurnObservation::WriteTransaction { revision, stage, outcome } => {
            TurnObservation::WriteTransaction {
                revision: EvidenceRevision::new(revision.as_str()),
                stage,
                outcome,
            }
        }
        TurnObservation::PromptTerminal { outcome } => TurnObservation::PromptTerminal { outcome },
    }
}

fn default_validation_run_id() -> String {
    String::from("legacy")
}

fn sanitize_identifier(value: &str) -> String {
    if is_opaque_identifier(value) {
        return value.to_string();
    }
    if is_secret_key(value) {
        return String::from("***");
    }
    if value.trim().is_empty() {
        return String::from("unknown");
    }
    // Evidence needs stable equality, not raw identifiers. Hash all caller
    // supplied ids so a malicious ACP session id or repository file name
    // cannot become persisted prompt-like content.
    let digest = Sha256::digest(value.as_bytes());
    format!("sha256:{digest:x}")
}

fn is_opaque_identifier(value: &str) -> bool {
    value == "***"
        || value == "unknown"
        || (value.len() == 71
            && value.starts_with("sha256:")
            && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn revision(value: &str) -> EvidenceRevision {
        EvidenceRevision::new(value)
    }

    fn verified_store() -> TurnEvidenceStore {
        let mut store = TurnEvidenceStore::default();
        let key = store.start_turn(String::from("agent"), String::from("session"));
        let current = revision("rev-2");
        for observation in [
            TurnObservation::Revision { revision: revision("rev-1") },
            TurnObservation::Revision { revision: current.clone() },
            TurnObservation::ChangedFiles {
                revision: current.clone(),
                files: vec![String::from("src/lib.rs")],
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
            TurnObservation::Validation {
                revision: current,
                selected: true,
                outcome: EvidenceCheck::Passed,
            },
        ] {
            store.observe(key.turn_id(), observation).expect("known turn");
        }
        store
    }

    #[test]
    fn matching_editor_evidence_is_verified() {
        let store = verified_store();
        let summary = store.summary(1).expect("summary");
        assert_eq!(summary.status, TurnTerminalStatus::Verified);
        assert_eq!(summary.blocker, None);
        assert_eq!(summary.evidence_ids.len(), 6);
    }

    #[test]
    fn completed_resume_supersedes_historical_recoverable_pause() {
        let mut store = verified_store();
        for outcome in [PromptTerminalOutcome::PausedRecoverable, PromptTerminalOutcome::Completed]
        {
            store.observe(1, TurnObservation::PromptTerminal { outcome }).expect("known turn");
        }

        let summary = store.summary(1).expect("summary");
        assert_eq!(summary.status, TurnTerminalStatus::Verified);
        assert_eq!(summary.blocker, None);
        assert_eq!(summary.safe_follow_up, SafeFollowUp::ReportEvidence);
        assert_eq!(summary.evidence_ids.len(), 8, "both lifecycle facts remain auditable");
    }

    #[test]
    fn stale_transaction_lifecycle_does_not_block_fresh_verification() {
        let mut store = verified_store();
        store
            .observe(
                1,
                TurnObservation::WriteTransaction {
                    revision: revision("rev-1"),
                    stage: WriteTransactionStage::Preview,
                    outcome: EvidenceCheck::Passed,
                },
            )
            .expect("known turn");

        let summary = store.summary(1).expect("summary");
        assert_eq!(summary.status, TurnTerminalStatus::Verified);
    }

    #[test]
    fn stale_validation_cannot_verify_new_revision() {
        let mut store = verified_store();
        store
            .observe(1, TurnObservation::Revision { revision: revision("rev-3") })
            .expect("known turn");

        let summary = store.summary(1).expect("summary");
        assert_eq!(summary.status, TurnTerminalStatus::Blocked);
        assert_eq!(summary.blocker, Some(TurnBlocker::StaleRevision));
    }

    #[test]
    fn failed_or_denied_evidence_blocks_completion() {
        let mut store = verified_store();
        store
            .observe(
                1,
                TurnObservation::Write {
                    revision: revision("rev-2"),
                    outcome: WriteEvidenceOutcome::Denied,
                },
            )
            .expect("known turn");

        let summary = store.summary(1).expect("summary");
        assert_eq!(summary.status, TurnTerminalStatus::Blocked);
        assert_eq!(summary.blocker, Some(TurnBlocker::WriteDenied));
    }

    #[test]
    fn no_op_write_does_not_fabricate_changed_file_evidence() {
        let mut store = TurnEvidenceStore::default();
        let key = store.start_turn(String::from("agent"), String::from("session"));
        store
            .observe(key.turn_id(), TurnObservation::Revision { revision: revision("rev-1") })
            .expect("known turn");
        store
            .observe(
                key.turn_id(),
                TurnObservation::Write {
                    revision: revision("rev-1"),
                    outcome: WriteEvidenceOutcome::NoOp,
                },
            )
            .expect("known turn");

        let summary = store.summary(key.turn_id()).expect("summary");
        assert_eq!(summary.status, TurnTerminalStatus::Unverified);
        assert_eq!(summary.blocker, Some(TurnBlocker::MissingChangedFiles));
    }

    #[test]
    fn current_write_flow_evidence_reaches_verified_after_selected_validation() {
        let mut store = TurnEvidenceStore::default();
        let key = store.start_turn(String::from("agent"), String::from("session"));
        let current = revision("write-revision");
        for observation in [
            TurnObservation::Revision { revision: current.clone() },
            TurnObservation::Write {
                revision: current.clone(),
                outcome: WriteEvidenceOutcome::Applied,
            },
            TurnObservation::ChangedFiles {
                revision: current.clone(),
                files: vec![String::from("src/lib.rs")],
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
            TurnObservation::ValidationRecord {
                revision: current,
                selected: true,
                record: HostValidationRecord {
                    run_id: String::from("terminal-42"),
                    command_id: String::from("terminal-42"),
                    command: String::from("cargo test --quiet selected_test"),
                    tool: Some(String::from("terminal")),
                    selector: Some(String::from("selected_test")),
                    outcome: EvidenceCheck::Passed,
                    exit_status: Some(0),
                    elapsed_ms: Some(1),
                    affected_tests: vec![String::from("selected_test")],
                    diagnostics_delta: 0,
                    output_truncated: false,
                    skip_or_denial: None,
                },
            },
        ] {
            store.observe(key.turn_id(), observation).expect("current write flow fact");
        }

        let summary = store.summary(key.turn_id()).expect("summary");
        assert_eq!(summary.status, TurnTerminalStatus::Verified);
        assert_eq!(summary.blocker, None);
    }

    #[test]
    fn later_result_for_same_validation_run_replaces_provisional_skip() {
        let mut store = TurnEvidenceStore::default();
        let key = store.start_turn(String::from("agent"), String::from("session"));
        let current = revision("rev-1");
        for observation in [
            TurnObservation::Revision { revision: current.clone() },
            TurnObservation::ChangedFiles {
                revision: current.clone(),
                files: vec![String::from("src/lib.rs")],
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
        ] {
            store.observe(key.turn_id(), observation).expect("known turn");
        }
        for outcome in [EvidenceCheck::Skipped, EvidenceCheck::Passed] {
            store
                .observe(
                    key.turn_id(),
                    TurnObservation::ValidationRecord {
                        revision: current.clone(),
                        selected: true,
                        record: HostValidationRecord {
                            run_id: String::from("terminal-1"),
                            command_id: String::from("terminal-1"),
                            command: String::from("cargo test --quiet"),
                            tool: Some(String::from("terminal")),
                            selector: Some(String::from("cargo test --quiet")),
                            outcome,
                            exit_status: if matches!(outcome, EvidenceCheck::Passed) {
                                Some(0)
                            } else {
                                None
                            },
                            elapsed_ms: None,
                            affected_tests: Vec::new(),
                            diagnostics_delta: 0,
                            output_truncated: false,
                            skip_or_denial: None,
                        },
                    },
                )
                .expect("known turn");
        }

        assert_eq!(
            store.summary(key.turn_id()).expect("summary").status,
            TurnTerminalStatus::Verified
        );
    }

    #[test]
    fn selected_skipped_validation_is_blocked_not_success() {
        let mut store = verified_store();
        store
            .observe(
                1,
                TurnObservation::Validation {
                    revision: revision("rev-2"),
                    selected: true,
                    outcome: EvidenceCheck::Skipped,
                },
            )
            .expect("known turn");

        let summary = store.summary(1).expect("summary");
        assert_eq!(summary.status, TurnTerminalStatus::Blocked);
        assert_eq!(summary.blocker, Some(TurnBlocker::ValidationSkipped));
    }

    #[test]
    fn validation_record_is_bounded_redacted_and_can_verify() {
        let mut store = TurnEvidenceStore::default();
        let key = store.start_turn(String::from("agent"), String::from("session"));
        let current = revision("rev-1");
        for observation in [
            TurnObservation::Revision { revision: current.clone() },
            TurnObservation::ChangedFiles {
                revision: current.clone(),
                files: vec![String::from("src/lib.rs")],
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
            TurnObservation::ValidationRecord {
                revision: current,
                selected: true,
                record: HostValidationRecord {
                    run_id: String::from("run-1"),
                    command_id: String::from("cargo_check"),
                    command: String::from("cargo check --quiet"),
                    tool: Some(String::from("cargo_check")),
                    selector: Some(String::from("selected")),
                    outcome: EvidenceCheck::Passed,
                    exit_status: Some(0),
                    elapsed_ms: Some(42),
                    affected_tests: vec![String::from("API_TOKEN")],
                    diagnostics_delta: 0,
                    output_truncated: false,
                    skip_or_denial: None,
                },
            },
        ] {
            store.observe(key.turn_id(), observation).expect("known turn");
        }
        let snapshot = store.snapshot(key.turn_id()).expect("snapshot");
        let EvidenceRecord {
            observation: TurnObservation::ValidationRecord { record, .. }, ..
        } = snapshot.records().last().expect("record")
        else {
            panic!("validation record");
        };
        assert_eq!(record.affected_tests, vec![String::from("***")]);
        assert_eq!(
            store.summary(key.turn_id()).expect("summary").status,
            TurnTerminalStatus::Verified
        );
    }

    #[test]
    fn evidence_storage_redacts_and_bounds_untrusted_identifiers() {
        let mut store = TurnEvidenceStore::default();
        let key = store.start_turn(String::from("agent"), String::from("session"));
        store
            .observe(
                key.turn_id(),
                TurnObservation::ChangedFiles {
                    revision: revision("API_TOKEN"),
                    files: (0..80).map(|index| format!("src/{index}.rs\nignored")).collect(),
                    truncated: false,
                },
            )
            .expect("known turn");
        let snapshot = store.snapshot(key.turn_id()).expect("snapshot");
        let EvidenceRecord {
            observation: TurnObservation::ChangedFiles { revision, files, truncated },
            ..
        } = &snapshot.records()[0]
        else {
            panic!("changed files evidence");
        };
        assert_eq!(revision.as_str(), "***");
        assert_eq!(files.len(), MAX_EVIDENCE_FILES);
        assert!(*truncated);
        assert!(files.iter().all(|file| file.starts_with("sha256:")));
        assert!(files.iter().all(|file| !file.contains("ignored")));
        assert!(snapshot.key().agent_id().starts_with("sha256:"));
        assert!(snapshot.key().session_id().starts_with("sha256:"));
    }

    #[test]
    fn observation_capacity_preserves_prompt_terminal_and_blocks_visibly() {
        let mut store = TurnEvidenceStore::default();
        let key = store.start_turn(String::from("agent"), String::from("session"));
        for index in 0..(MAX_TURN_OBSERVATIONS - 1) {
            store
                .observe(
                    key.turn_id(),
                    TurnObservation::Revision { revision: revision(&format!("rev-{index}")) },
                )
                .expect("reserved non-terminal capacity");
        }
        store
            .observe(
                key.turn_id(),
                TurnObservation::PromptTerminal { outcome: PromptTerminalOutcome::Completed },
            )
            .expect("reserved terminal capacity");
        let summary = store
            .observe(
                key.turn_id(),
                TurnObservation::Diagnostics {
                    revision: revision("rev-overflow"),
                    outcome: EvidenceCheck::Passed,
                },
            )
            .expect("capacity exhaustion is visible in summary");

        assert_eq!(summary.status, TurnTerminalStatus::Blocked);
        assert_eq!(summary.blocker, Some(TurnBlocker::ObservationCapacityExhausted));
        let snapshot = store.snapshot(key.turn_id()).expect("snapshot");
        assert!(matches!(
            snapshot.records().last().map(EvidenceRecord::observation),
            Some(TurnObservation::PromptTerminal { outcome: PromptTerminalOutcome::Completed })
        ));
    }

    #[test]
    fn observations_are_append_only_and_snapshots_are_immutable() {
        let mut store = TurnEvidenceStore::default();
        let key = store.start_turn(String::from("agent"), String::from("session"));
        store
            .observe(key.turn_id(), TurnObservation::Revision { revision: revision("rev-1") })
            .expect("known turn");
        let first = store.snapshot(key.turn_id()).expect("first snapshot");
        store
            .observe(key.turn_id(), TurnObservation::Revision { revision: revision("rev-2") })
            .expect("known turn");
        let second = store.snapshot(key.turn_id()).expect("second snapshot");

        assert_eq!(first.records().len(), 1);
        assert_eq!(second.records().len(), 2);
        assert_eq!(first.current_revision(), Some(&revision("rev-1")));
        assert_eq!(second.current_revision(), Some(&revision("rev-2")));
    }
}
