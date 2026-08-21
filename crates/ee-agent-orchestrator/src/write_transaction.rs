//! Auditable, revision-bound write transactions.
//!
//! A host records one [`WriteTransaction`] for every mutation sequence. The
//! state machine enforces `read → preview → approval → apply → diagnostics →
//! final diff → selected validation → terminal state`; stale, ambiguous, and
//! conflicting revisions fail closed. This module never performs a write: it
//! records host-observed evidence and prevents that evidence from supporting a
//! verified final response until every required stage is fresh.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::completion::{CompletionReport, CompletionState};
use crate::final_response::{ValidationOutcome, ValidationRecord};

/// Stable lifecycle state for a write transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteTransactionState {
    /// Expected source revisions still need fresh reads.
    AwaitingRead,
    /// Read revisions are fresh; preview still needs recording.
    AwaitingPreview,
    /// Preview is recorded; mutation approval is required.
    AwaitingApproval,
    /// Approval passed; host may apply exactly previewed changes.
    AwaitingApply,
    /// All changes applied; fresh post-write diagnostics are required.
    AwaitingDiagnostics,
    /// Fresh diagnostics passed; final diff review is required.
    AwaitingFinalDiff,
    /// Fresh final diff passed; selected validation is required.
    AwaitingValidation,
    /// All required write evidence passed at current post-write revision.
    Verified,
    /// Sequence cannot continue safely without fresh user or host action.
    Blocked,
    /// Observation ended or was interrupted before required evidence existed.
    Unverified,
    /// Agent-owned changes were safely rolled back.
    RolledBack,
}

impl WriteTransactionState {
    /// Stable state spelling for protocol and user-facing output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AwaitingRead => "awaiting_read",
            Self::AwaitingPreview => "awaiting_preview",
            Self::AwaitingApproval => "awaiting_approval",
            Self::AwaitingApply => "awaiting_apply",
            Self::AwaitingDiagnostics => "awaiting_diagnostics",
            Self::AwaitingFinalDiff => "awaiting_final_diff",
            Self::AwaitingValidation => "awaiting_validation",
            Self::Verified => "verified",
            Self::Blocked => "blocked",
            Self::Unverified => "unverified",
            Self::RolledBack => "rolled_back",
        }
    }
}

/// Ownership of a source buffer at its observed revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BufferOwnership {
    /// Buffer has no unsaved changes.
    Clean,
    /// Unsaved changes were made by the agent transaction itself.
    Agent,
    /// Unsaved changes belong to user and must never be overwritten silently.
    User,
    /// Host cannot prove ownership; fail closed like a user-owned buffer.
    Unknown,
}

impl BufferOwnership {
    fn blocks_mutation(self) -> bool {
        matches!(self, Self::User | Self::Unknown)
    }
}

/// One expected source revision for a changed path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SourceRevision {
    /// Absolute changed path.
    pub path: String,
    /// Revision read before preview and approval.
    pub revision: String,
    /// Whether this source buffer contains unsaved content.
    pub dirty: bool,
    /// Owner of unsaved content when `dirty` is true.
    pub ownership: BufferOwnership,
}

impl SourceRevision {
    /// Creates a clean source revision.
    #[must_use]
    pub fn clean(path: impl Into<String>, revision: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            revision: revision.into(),
            dirty: false,
            ownership: BufferOwnership::Clean,
        }
    }
}

/// A bounded preview accepted by a user or trusted host approval flow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WritePreview {
    /// Path this preview changes.
    pub path: String,
    /// Revision the preview was generated against.
    pub expected_revision: String,
    /// Bounded human-readable mutation summary; no source contents required.
    pub summary: String,
}

/// Result of applying one changed path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AppliedWrite {
    /// Path host attempted to change.
    pub path: String,
    /// Revision expected by apply, copied from source read.
    pub expected_revision: String,
    /// Revision observed after successful apply.
    pub post_write_revision: Option<String>,
    /// Whether this mutation is owned by the agent transaction.
    pub agent_owned: bool,
    /// Whether host applied this path.
    pub applied: bool,
    /// Bounded failure reason when apply did not complete.
    pub failure_reason: Option<String>,
}

/// Approval record for one write transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "result")]
pub enum WriteApproval {
    /// Explicit approval after preview.
    Approved { approval_id: String },
    /// User, host, or policy denied mutation.
    Denied { reason: String },
}

/// Fresh diagnostics evidence tied to post-write workspace revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TransactionDiagnostics {
    /// Stable diagnostics evidence id.
    pub evidence_id: String,
    /// Workspace revision observed after writes.
    pub workspace_revision: String,
    /// Error count before transaction writes.
    pub errors_before: u32,
    /// Error count after transaction writes.
    pub errors_after: u32,
    /// Warning count before transaction writes.
    pub warnings_before: u32,
    /// Warning count after transaction writes.
    pub warnings_after: u32,
}

impl TransactionDiagnostics {
    /// Whether this evidence shows a diagnostics regression.
    #[must_use]
    pub const fn regressed(&self) -> bool {
        self.errors_after > self.errors_before || self.warnings_after > self.warnings_before
    }

    /// Net error count change.
    #[must_use]
    pub const fn error_delta(&self) -> i64 {
        self.errors_after as i64 - self.errors_before as i64
    }

    /// Net warning count change.
    #[must_use]
    pub const fn warning_delta(&self) -> i64 {
        self.warnings_after as i64 - self.warnings_before as i64
    }
}

/// Fresh final-diff review evidence tied to post-write workspace revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TransactionFinalDiff {
    /// Stable final-diff evidence id.
    pub evidence_id: String,
    /// Workspace revision represented by this diff.
    pub workspace_revision: String,
    /// Ordered, deduplicated paths in reviewed diff.
    pub changed_paths: Vec<String>,
    /// Whether host found a final-diff safety problem.
    pub passed: bool,
    /// Bounded failure reason when `passed` is false.
    pub detail: Option<String>,
}

/// Validation evidence associated with one write transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TransactionValidation {
    /// Stable transaction id this record belongs to.
    pub transaction_id: String,
    /// Structured validation outcome, including command, elapsed time, output
    /// truncation, diagnostics delta, and revision.
    pub record: ValidationRecord,
}

/// Explicit rollback safety checks. Both checks are required because rollback
/// is another write and must not overwrite later user work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RollbackSafetyCheck {
    /// User or host explicitly approved rollback.
    pub approval_id: String,
    /// Host verified no later user edit exists at affected paths.
    pub no_later_user_edits: bool,
    /// Host revision that must equal current post-write workspace revision.
    pub workspace_revision: String,
}

/// Structured transaction failure. Callers can render `code` directly without
/// parsing prose and can preserve user work on every fail-closed path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WriteTransactionError {
    /// Stable machine-readable error class.
    pub code: String,
    /// Actionable, bounded explanation.
    pub message: String,
}

impl WriteTransactionError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into() }
    }
}

impl std::fmt::Display for WriteTransactionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for WriteTransactionError {}

/// Complete, serializable evidence for one mutation sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WriteTransaction {
    /// Stable transaction id assigned before source reads.
    pub transaction_id: String,
    /// Current transaction lifecycle state.
    pub state: WriteTransactionState,
    /// Expected source revisions, keyed by changed absolute path.
    pub expected_sources: BTreeMap<String, SourceRevision>,
    /// Paths changed by successful applies, in source order.
    pub changed_paths: Vec<String>,
    /// Approval result recorded after preview.
    pub approval: Option<WriteApproval>,
    /// Apply evidence for each planned source path.
    pub applies: Vec<AppliedWrite>,
    /// Post-write workspace revision. Required for downstream evidence.
    pub post_write_workspace_revision: Option<String>,
    /// Post-write diagnostics evidence.
    pub diagnostics: Option<TransactionDiagnostics>,
    /// Final diff evidence.
    pub final_diff: Option<TransactionFinalDiff>,
    /// Selected validation evidence.
    pub validation: Option<TransactionValidation>,
    /// Blocker or interruption reason when terminal state is not verified.
    pub terminal_reason: Option<String>,
    /// Rollback check recorded before a rollback can run.
    pub rollback_safety: Option<RollbackSafetyCheck>,
}

impl WriteTransaction {
    /// Begins a transaction from declared expected source revisions.
    pub fn begin(
        transaction_id: impl Into<String>,
        sources: impl IntoIterator<Item = SourceRevision>,
    ) -> Result<Self, WriteTransactionError> {
        let transaction_id = transaction_id.into();
        if transaction_id.trim().is_empty() {
            return Err(WriteTransactionError::new(
                "invalid_transaction",
                "transaction id must not be empty",
            ));
        }
        let mut expected_sources = BTreeMap::new();
        for source in sources {
            validate_source(&source)?;
            if expected_sources.insert(source.path.clone(), source).is_some() {
                return Err(WriteTransactionError::new(
                    "ambiguous_revision",
                    "each changed path must have exactly one expected source revision",
                ));
            }
        }
        if expected_sources.is_empty() {
            return Err(WriteTransactionError::new(
                "invalid_transaction",
                "transaction requires at least one changed path",
            ));
        }
        Ok(Self {
            transaction_id,
            state: WriteTransactionState::AwaitingRead,
            expected_sources,
            changed_paths: Vec::new(),
            approval: None,
            applies: Vec::new(),
            post_write_workspace_revision: None,
            diagnostics: None,
            final_diff: None,
            validation: None,
            terminal_reason: None,
            rollback_safety: None,
        })
    }

    /// Records fresh reads immediately before preview. Dirty user or unknown
    /// buffers fail closed before an apply can start.
    pub fn record_read_revisions(
        &mut self,
        observed: impl IntoIterator<Item = SourceRevision>,
    ) -> Result<(), WriteTransactionError> {
        self.require_state(WriteTransactionState::AwaitingRead)?;
        let observed = revision_map(observed)?;
        if observed.len() != self.expected_sources.len()
            || observed.keys().ne(self.expected_sources.keys())
        {
            return self.block(
                "ambiguous_revision",
                "read revisions do not cover exactly planned changed paths",
            );
        }
        for (path, expected) in &self.expected_sources {
            let actual = &observed[path];
            if expected.revision != actual.revision {
                return self.block(
                    "stale_revision",
                    format!(
                        "{path} changed after transaction began; reread and re-preview before apply"
                    ),
                );
            }
            if actual.dirty && actual.ownership.blocks_mutation() {
                return self.block(
                    "dirty_user_buffer",
                    format!("{path} has dirty user or unknown content; preserve it and request explicit handoff"),
                );
            }
        }
        self.state = WriteTransactionState::AwaitingPreview;
        Ok(())
    }

    /// Records one preview per planned path. Previews must use source revisions
    /// observed by [`Self::record_read_revisions`].
    pub fn record_preview(
        &mut self,
        previews: impl IntoIterator<Item = WritePreview>,
    ) -> Result<(), WriteTransactionError> {
        self.require_state(WriteTransactionState::AwaitingPreview)?;
        let mut paths = BTreeSet::new();
        for preview in previews {
            let Some(source) = self.expected_sources.get(&preview.path) else {
                return self
                    .block("unexpected_path", "preview contains a path outside transaction scope");
            };
            if !paths.insert(preview.path.clone()) {
                return self
                    .block("ambiguous_preview", "preview contains the same path more than once");
            }
            if preview.expected_revision != source.revision {
                return self.block(
                    "stale_revision",
                    format!("preview for {} does not match fresh source revision", preview.path),
                );
            }
            if preview.summary.trim().is_empty() {
                return self.block("invalid_preview", "preview summary must not be empty");
            }
        }
        if paths.len() != self.expected_sources.len() {
            return self
                .block("incomplete_preview", "preview must cover every planned changed path");
        }
        self.state = WriteTransactionState::AwaitingApproval;
        Ok(())
    }

    /// Records explicit mutation approval. Denial blocks without an apply.
    pub fn record_approval(
        &mut self,
        approval: WriteApproval,
    ) -> Result<(), WriteTransactionError> {
        self.require_state(WriteTransactionState::AwaitingApproval)?;
        self.approval = Some(approval.clone());
        match approval {
            WriteApproval::Approved { approval_id } if !approval_id.trim().is_empty() => {
                self.state = WriteTransactionState::AwaitingApply;
                Ok(())
            }
            WriteApproval::Approved { .. } => {
                self.block("invalid_approval", "approval id must not be empty")
            }
            WriteApproval::Denied { reason } => self.block(
                "approval_denied",
                format!("mutation denied: {reason}; no changes were applied"),
            ),
        }
    }

    /// Records apply outcomes and post-write workspace revision. Every planned
    /// path must succeed at its expected revision. Partial outcomes remain
    /// blocked; no broad automatic repair is attempted.
    pub fn record_apply(
        &mut self,
        outcomes: impl IntoIterator<Item = AppliedWrite>,
        post_write_workspace_revision: impl Into<String>,
    ) -> Result<(), WriteTransactionError> {
        self.require_state(WriteTransactionState::AwaitingApply)?;
        let workspace_revision = post_write_workspace_revision.into();
        if workspace_revision.trim().is_empty() {
            return self
                .block("ambiguous_revision", "post-write workspace revision must not be empty");
        }
        let mut seen = BTreeSet::new();
        let mut applied = Vec::new();
        for outcome in outcomes {
            let Some(source) = self.expected_sources.get(&outcome.path) else {
                return self.block(
                    "unexpected_path",
                    "apply result contains a path outside transaction scope",
                );
            };
            if !seen.insert(outcome.path.clone()) {
                return self.block(
                    "ambiguous_apply",
                    "apply result contains the same path more than once",
                );
            }
            if outcome.expected_revision != source.revision {
                return self.block(
                    "stale_revision",
                    format!(
                        "apply for {} used a stale or conflicting source revision",
                        outcome.path
                    ),
                );
            }
            if !outcome.applied || outcome.post_write_revision.as_deref().is_none_or(str::is_empty)
            {
                applied.push(outcome);
                self.applies = applied;
                self.changed_paths = self
                    .applies
                    .iter()
                    .filter(|item| item.applied)
                    .map(|item| item.path.clone())
                    .collect();
                return self.block(
                    "partial_apply",
                    "one or more paths did not apply; preserve current work, inspect outcomes, then recover explicitly",
                );
            }
            applied.push(outcome);
        }
        if seen.len() != self.expected_sources.len() {
            self.applies = applied;
            self.changed_paths = self
                .applies
                .iter()
                .filter(|item| item.applied)
                .map(|item| item.path.clone())
                .collect();
            return self.block(
                "partial_apply",
                "apply did not return every planned path; preserve current work and recover explicitly",
            );
        }
        self.changed_paths = self.expected_sources.keys().cloned().collect();
        self.applies = applied;
        self.post_write_workspace_revision = Some(workspace_revision);
        self.state = WriteTransactionState::AwaitingDiagnostics;
        Ok(())
    }

    /// Records diagnostics measured after all writes at post-write revision.
    /// Any diagnostic regression reopens work as blocked.
    pub fn record_diagnostics(
        &mut self,
        diagnostics: TransactionDiagnostics,
    ) -> Result<(), WriteTransactionError> {
        self.require_state(WriteTransactionState::AwaitingDiagnostics)?;
        if self.require_post_write_revision(&diagnostics.workspace_revision).is_err() {
            return self.block(
                "stale_revision",
                "diagnostics do not match post-write workspace revision; refresh diagnostics",
            );
        }
        if diagnostics.evidence_id.trim().is_empty() {
            return self.block("invalid_diagnostics", "diagnostics evidence id must not be empty");
        }
        let regressed = diagnostics.regressed();
        self.diagnostics = Some(diagnostics);
        if regressed {
            return self.block(
                "diagnostic_regression",
                "post-write diagnostics regressed; leave work blocked and investigate before repair",
            );
        }
        self.state = WriteTransactionState::AwaitingFinalDiff;
        Ok(())
    }

    /// Records final diff review at current post-write revision.
    pub fn record_final_diff(
        &mut self,
        diff: TransactionFinalDiff,
    ) -> Result<(), WriteTransactionError> {
        self.require_state(WriteTransactionState::AwaitingFinalDiff)?;
        if self.require_post_write_revision(&diff.workspace_revision).is_err() {
            return self.block(
                "stale_revision",
                "final diff does not match post-write workspace revision; refresh final diff",
            );
        }
        if diff.evidence_id.trim().is_empty() {
            return self.block("invalid_final_diff", "final diff evidence id must not be empty");
        }
        if deduplicate_paths(&diff.changed_paths) != self.changed_paths {
            return self.block(
                "stale_final_diff",
                "final diff paths differ from applied transaction paths; refresh final diff",
            );
        }
        let passed = diff.passed;
        let detail = diff.detail.clone();
        self.final_diff = Some(diff);
        if !passed {
            return self.block(
                "final_diff_failed",
                detail.unwrap_or_else(|| "final diff review reported a safety issue".into()),
            );
        }
        self.state = WriteTransactionState::AwaitingValidation;
        Ok(())
    }

    /// Records selected validation evidence. Validation must run after fresh
    /// diagnostics and final diff, be selected, pass, and carry post-write
    /// workspace revision.
    pub fn record_validation(
        &mut self,
        validation: TransactionValidation,
    ) -> Result<(), WriteTransactionError> {
        self.require_state(WriteTransactionState::AwaitingValidation)?;
        if validation.transaction_id != self.transaction_id {
            return self.block(
                "wrong_transaction",
                "validation evidence belongs to a different transaction",
            );
        }
        if self
            .require_post_write_revision(validation.record.revision.as_deref().unwrap_or_default())
            .is_err()
        {
            return self.block(
                "stale_revision",
                "validation does not match post-write workspace revision; rerun validation",
            );
        }
        if !validation.record.selected {
            return self.block(
                "unselected_validation",
                "validation must be explicitly selected for completion",
            );
        }
        if validation.record.denied {
            return self.block(
                "validation_denied",
                "validation was denied; obtain approval then rerun it",
            );
        }
        if validation.record.outcome == ValidationOutcome::Skipped {
            self.validation = Some(validation);
            self.state = WriteTransactionState::Unverified;
            self.terminal_reason =
                Some("selected validation was skipped; run it against current revision".into());
            return Ok(());
        }
        if validation.record.outcome != ValidationOutcome::Passed {
            return self.block(
                "validation_failed",
                "selected validation failed; resolve it then rerun validation",
            );
        }
        self.validation = Some(validation);
        self.state = WriteTransactionState::Verified;
        self.terminal_reason = None;
        Ok(())
    }

    /// Marks an interrupted sequence as unverified. Existing evidence remains
    /// intact for recovery; no mutation is replayed automatically.
    pub fn interrupt(&mut self, reason: impl Into<String>) {
        if !matches!(
            self.state,
            WriteTransactionState::Verified | WriteTransactionState::RolledBack
        ) {
            self.state = WriteTransactionState::Unverified;
            self.terminal_reason = Some(reason.into());
        }
    }

    /// Starts rollback only when all observed changes are agent-owned, post-write
    /// revision is still current, and explicit safety confirmation exists.
    pub fn prepare_rollback(
        &mut self,
        safety: RollbackSafetyCheck,
    ) -> Result<(), WriteTransactionError> {
        if !matches!(self.state, WriteTransactionState::Blocked | WriteTransactionState::Unverified)
        {
            return Err(WriteTransactionError::new(
                "invalid_rollback_state",
                "rollback is only available for blocked or unverified transactions",
            ));
        }
        self.require_post_write_revision(&safety.workspace_revision)?;
        if safety.approval_id.trim().is_empty() || !safety.no_later_user_edits {
            return Err(WriteTransactionError::new(
                "rollback_safety_check_failed",
                "rollback requires explicit approval and proof no later user edits exist",
            ));
        }
        if self.applies.is_empty()
            || self.applies.iter().any(|item| !item.applied || !item.agent_owned)
        {
            return Err(WriteTransactionError::new(
                "rollback_not_agent_owned",
                "rollback is allowed only for successfully applied agent-owned changes",
            ));
        }
        self.rollback_safety = Some(safety);
        Ok(())
    }

    /// Records completed rollback only after [`Self::prepare_rollback`].
    pub fn record_rollback(
        &mut self,
        current_workspace_revision: impl AsRef<str>,
    ) -> Result<(), WriteTransactionError> {
        let Some(safety) = &self.rollback_safety else {
            return Err(WriteTransactionError::new(
                "rollback_safety_check_missing",
                "record rollback safety checks before applying rollback",
            ));
        };
        if safety.workspace_revision != current_workspace_revision.as_ref() {
            return Err(WriteTransactionError::new(
                "stale_revision",
                "workspace changed after rollback safety check; reread before rollback",
            ));
        }
        self.state = WriteTransactionState::RolledBack;
        self.terminal_reason = Some("agent-owned transaction changes rolled back".into());
        Ok(())
    }

    /// Prevents a caller-provided completion report from claiming verified work
    /// when this transaction lacks fresh post-write evidence.
    pub fn constrain_completion(&self, report: &mut CompletionReport) {
        if !report.is_verified() || self.state == WriteTransactionState::Verified {
            return;
        }
        let (state, blocker, follow_up) = match self.state {
            WriteTransactionState::Blocked => (
                CompletionState::Blocked,
                self.terminal_reason
                    .clone()
                    .unwrap_or_else(|| "write transaction is blocked".into()),
                "resolve transaction blocker, refresh revisions, then collect fresh evidence"
                    .into(),
            ),
            WriteTransactionState::RolledBack => (
                CompletionState::Unverified,
                "transaction changes were rolled back".into(),
                "start a fresh transaction before claiming completion".into(),
            ),
            _ => (
                CompletionState::Unverified,
                self.terminal_reason
                    .clone()
                    .unwrap_or_else(|| format!("write transaction is {}", self.state.as_str())),
                "complete fresh transaction evidence before claiming completion".into(),
            ),
        };
        report.state = state;
        report.blocker = Some(blocker);
        report.safe_follow_up = Some(follow_up);
        report.evidence_ids.push(format!("transaction:{}", self.transaction_id));
        report.evidence_ids.sort();
        report.evidence_ids.dedup();
    }

    fn require_state(&self, expected: WriteTransactionState) -> Result<(), WriteTransactionError> {
        if self.state == expected {
            return Ok(());
        }
        Err(WriteTransactionError::new(
            "invalid_transaction_sequence",
            format!("expected {}, found {}", expected.as_str(), self.state.as_str()),
        ))
    }

    fn require_post_write_revision(&self, revision: &str) -> Result<(), WriteTransactionError> {
        if self.post_write_workspace_revision.as_deref() == Some(revision) {
            Ok(())
        } else {
            Err(WriteTransactionError::new(
                "stale_revision",
                "evidence revision does not match post-write workspace revision",
            ))
        }
    }

    fn block<T>(
        &mut self,
        code: &'static str,
        message: impl Into<String>,
    ) -> Result<T, WriteTransactionError> {
        let error = WriteTransactionError::new(code, message);
        self.state = WriteTransactionState::Blocked;
        self.terminal_reason = Some(error.message.clone());
        Err(error)
    }
}

fn validate_source(source: &SourceRevision) -> Result<(), WriteTransactionError> {
    if !Path::new(&source.path).is_absolute() {
        return Err(WriteTransactionError::new(
            "invalid_path",
            "transaction paths must be absolute",
        ));
    }
    if source.revision.trim().is_empty() {
        return Err(WriteTransactionError::new(
            "ambiguous_revision",
            "source revision must not be empty",
        ));
    }
    if !source.dirty && source.ownership != BufferOwnership::Clean {
        return Err(WriteTransactionError::new(
            "ambiguous_buffer_ownership",
            "clean source buffers must use clean ownership",
        ));
    }
    Ok(())
}

fn revision_map(
    revisions: impl IntoIterator<Item = SourceRevision>,
) -> Result<BTreeMap<String, SourceRevision>, WriteTransactionError> {
    let mut map = BTreeMap::new();
    for revision in revisions {
        validate_source(&revision)?;
        if map.insert(revision.path.clone(), revision).is_some() {
            return Err(WriteTransactionError::new(
                "ambiguous_revision",
                "each observed path must have exactly one revision",
            ));
        }
    }
    Ok(map)
}

fn deduplicate_paths(paths: &[String]) -> Vec<String> {
    let mut paths = paths.to_vec();
    paths.sort();
    paths.dedup();
    paths
}
