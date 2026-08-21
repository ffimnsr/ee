//! Evidence-gated completion state for agent turns.
//!
//! [`CompletionEvidence`] holds only tool-observed facts.  [`derive_completion`]
//! never consumes model or reflection prose, so neither can turn missing,
//! failed, stale, skipped, or denied validation into a successful completion.

use serde::{Deserialize, Serialize};

use crate::final_response::{ChangedFile, ValidationOutcome, ValidationRecord};

/// Explicit terminal state for a turn's completion evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionState {
    /// All required fresh evidence, including selected validation, passed.
    Verified,
    /// Core evidence passed, but validation was explicitly unavailable or skipped.
    PartiallyVerified,
    /// Recorded evidence failed, was denied, or became stale.
    Blocked,
    /// Required evidence was never recorded.
    Unverified,
}

impl CompletionState {
    /// Stable lowercase state name for user-facing structured responses.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::PartiallyVerified => "partially_verified",
            Self::Blocked => "blocked",
            Self::Unverified => "unverified",
        }
    }
}

/// Freshness and result state for one non-validation evidence item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status", content = "reason")]
pub enum EvidenceStatus {
    /// Observation completed successfully against the current revision.
    Passed,
    /// Observation completed and found a problem.
    Failed(String),
    /// Observation was deliberately skipped with an actionable reason.
    Skipped(String),
    /// Observation was denied by policy or approval routing.
    Denied(String),
    /// Observation belongs to an older revision and cannot back current claims.
    Stale(String),
}

impl EvidenceStatus {
    fn blocker(&self, label: &str) -> Option<(String, String)> {
        match self {
            Self::Failed(reason) => Some((
                format!("{label} failed: {reason}"),
                format!("fix reported issue, then rerun {label}"),
            )),
            Self::Denied(reason) => Some((
                format!("{label} denied: {reason}"),
                format!("obtain required approval, then rerun {label}"),
            )),
            Self::Stale(reason) => Some((
                format!("{label} is stale: {reason}"),
                format!("refresh {label} against current revision"),
            )),
            Self::Passed | Self::Skipped(_) => None,
        }
    }
}

/// One tool-observed completion item with a stable identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CompletionEvidenceItem {
    /// Stable id cited by final responses.
    pub id: String,
    /// Tool or command that produced this observation.
    pub source: String,
    /// Revision observed by this item. `None` cannot satisfy verified completion.
    pub revision: Option<String>,
    /// Result and freshness state.
    pub status: EvidenceStatus,
}

impl CompletionEvidenceItem {
    /// Creates a fresh passing observation.
    #[must_use]
    pub fn passed(
        id: impl Into<String>,
        source: impl Into<String>,
        revision: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            source: source.into(),
            revision: Some(revision.into()),
            status: EvidenceStatus::Passed,
        }
    }
}

/// Tool-observed evidence required before a changed turn may be verified.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CompletionEvidence {
    /// Current workspace or buffer revision all evidence must match.
    pub revision: String,
    /// Recorded changed-file inventory.
    pub changed_file_inventory: Option<CompletionEvidenceItem>,
    /// Inventory paths in first-observed order.
    pub inventory_files: Vec<String>,
    /// Post-write diagnostics result.
    pub post_write_diagnostics: Option<CompletionEvidenceItem>,
    /// Final diff review result.
    pub final_diff_review: Option<CompletionEvidenceItem>,
}

impl CompletionEvidence {
    /// Creates evidence tied to `revision`.
    #[must_use]
    pub fn new(revision: impl Into<String>) -> Self {
        Self { revision: revision.into(), ..Self::default() }
    }

    /// Records the deduplicated changed-file inventory.
    pub fn record_changed_file_inventory(
        &mut self,
        item: CompletionEvidenceItem,
        files: impl IntoIterator<Item = impl Into<String>>,
    ) {
        self.changed_file_inventory = Some(item);
        self.inventory_files = deduplicate(files.into_iter().map(Into::into));
    }

    /// Records post-write diagnostics.
    pub fn record_post_write_diagnostics(&mut self, item: CompletionEvidenceItem) {
        self.post_write_diagnostics = Some(item);
    }

    /// Records final diff review.
    pub fn record_final_diff_review(&mut self, item: CompletionEvidenceItem) {
        self.final_diff_review = Some(item);
    }
}

/// Derived completion state, blockers, follow-up, and evidence citations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CompletionReport {
    /// Terminal state derived only from recorded evidence.
    pub state: CompletionState,
    /// Exact reason completion cannot be verified, when any.
    pub blocker: Option<String>,
    /// Safe next action for the user or agent, when any.
    pub safe_follow_up: Option<String>,
    /// Evidence ids backing status and claims.
    pub evidence_ids: Vec<String>,
}

impl CompletionReport {
    /// Whether this report permits successful completion claims.
    #[must_use]
    pub const fn is_verified(&self) -> bool {
        matches!(self.state, CompletionState::Verified)
    }
}

/// Derives a terminal completion state from tool-observed evidence only.
///
/// `changed_files` must match the recorded inventory exactly (after stable
/// deduplication).  Successful validation must be selected and tied to the
/// same revision.  A skipped validation remains partially verified; an absent
/// validation remains unverified.
#[must_use]
pub fn derive_completion(
    changed_files: &[ChangedFile],
    evidence: &CompletionEvidence,
    validation: &[ValidationRecord],
) -> CompletionReport {
    let changed_paths = deduplicate(changed_files.iter().map(|file| file.path.clone()));
    let mut evidence_ids = evidence_ids(evidence, validation);

    for (label, item) in [
        ("changed-file inventory", evidence.changed_file_inventory.as_ref()),
        ("post-write diagnostics", evidence.post_write_diagnostics.as_ref()),
        ("final diff review", evidence.final_diff_review.as_ref()),
    ] {
        if let Some(item) = item {
            if let Some((blocker, safe_follow_up)) = item.status.blocker(label) {
                return blocked(blocker, safe_follow_up, evidence_ids);
            }
            if item.revision.as_deref() != Some(evidence.revision.as_str()) {
                return blocked(
                    format!("{label} is stale: revision does not match current workspace"),
                    format!("refresh {label} against current revision"),
                    evidence_ids,
                );
            }
        }
    }

    let Some(inventory) = evidence.changed_file_inventory.as_ref() else {
        return unverified("record changed-file inventory before completion", evidence_ids);
    };
    if !matches!(inventory.status, EvidenceStatus::Passed) {
        return unverified(
            "record successful changed-file inventory before completion",
            evidence_ids,
        );
    }
    if evidence.inventory_files != changed_paths {
        return blocked(
            "changed-file inventory does not match observed writes".into(),
            "refresh changed-file inventory and final diff review".into(),
            evidence_ids,
        );
    }

    for (label, item) in [
        ("post-write diagnostics", evidence.post_write_diagnostics.as_ref()),
        ("final diff review", evidence.final_diff_review.as_ref()),
    ] {
        let Some(item) = item else {
            return unverified(format!("record {label} before completion"), evidence_ids);
        };
        if !matches!(item.status, EvidenceStatus::Passed) {
            return unverified(
                format!("record successful {label} before completion"),
                evidence_ids,
            );
        }
    }

    let selected = validation.iter().filter(|record| record.selected).collect::<Vec<_>>();
    if selected.is_empty() {
        return unverified("record selected validation result before completion", evidence_ids);
    }
    if let Some(record) = selected.iter().find(|record| {
        record.revision.as_deref() != Some(evidence.revision.as_str())
            || record.outcome == ValidationOutcome::Failed
            || record.denied
    }) {
        let reason = if record.denied {
            "validation denied"
        } else if record.revision.as_deref() != Some(evidence.revision.as_str()) {
            "validation is stale"
        } else {
            "validation failed"
        };
        return blocked(
            format!("{reason}: {}", record.command),
            format!("resolve validation issue, then rerun {}", record.command),
            evidence_ids,
        );
    }
    if let Some(record) =
        selected.iter().find(|record| record.outcome == ValidationOutcome::Skipped)
    {
        return CompletionReport {
            state: CompletionState::PartiallyVerified,
            blocker: Some(format!(
                "validation skipped: {}",
                record.skip_reason.as_deref().unwrap_or(&record.command)
            )),
            safe_follow_up: Some(format!("run {} when blocker is cleared", record.command)),
            evidence_ids,
        };
    }
    if selected.iter().all(|record| record.outcome == ValidationOutcome::Passed) {
        evidence_ids.sort();
        evidence_ids.dedup();
        return CompletionReport {
            state: CompletionState::Verified,
            blocker: None,
            safe_follow_up: None,
            evidence_ids,
        };
    }

    unverified("record selected validation result before completion", evidence_ids)
}

fn blocked(
    blocker: String,
    safe_follow_up: String,
    mut evidence_ids: Vec<String>,
) -> CompletionReport {
    evidence_ids.sort();
    evidence_ids.dedup();
    CompletionReport {
        state: CompletionState::Blocked,
        blocker: Some(blocker),
        safe_follow_up: Some(safe_follow_up),
        evidence_ids,
    }
}

fn unverified(blocker: impl Into<String>, mut evidence_ids: Vec<String>) -> CompletionReport {
    evidence_ids.sort();
    evidence_ids.dedup();
    CompletionReport {
        state: CompletionState::Unverified,
        blocker: Some(blocker.into()),
        safe_follow_up: Some("collect required tool evidence before claiming completion".into()),
        evidence_ids,
    }
}

fn evidence_ids(evidence: &CompletionEvidence, validation: &[ValidationRecord]) -> Vec<String> {
    let mut ids = [
        evidence.changed_file_inventory.as_ref(),
        evidence.post_write_diagnostics.as_ref(),
        evidence.final_diff_review.as_ref(),
    ]
    .into_iter()
    .flatten()
    .map(|item| item.id.clone())
    .collect::<Vec<_>>();
    ids.extend(
        validation.iter().filter(|record| record.selected).map(|record| record.evidence_id.clone()),
    );
    ids
}

fn deduplicate(input: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut values = Vec::new();
    for value in input {
        if !values.contains(&value) {
            values.push(value);
        }
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str) -> ChangedFile {
        ChangedFile { path: path.into(), source_task: None }
    }

    fn validation(outcome: ValidationOutcome) -> ValidationRecord {
        ValidationRecord::evidence(
            "validation-1",
            "cargo test --quiet",
            outcome,
            Some("terminal".into()),
            Some(0),
            Some(42),
            vec!["crate::tests::smoke".into()],
            0,
            false,
            None,
            Some("rev-1".into()),
            true,
            false,
            None,
        )
    }

    fn complete_evidence() -> CompletionEvidence {
        let mut evidence = CompletionEvidence::new("rev-1");
        evidence.record_changed_file_inventory(
            CompletionEvidenceItem::passed("inventory-1", "git diff --name-only", "rev-1"),
            ["src/lib.rs"],
        );
        evidence.record_post_write_diagnostics(CompletionEvidenceItem::passed(
            "diagnostics-1",
            "editor diagnostics",
            "rev-1",
        ));
        evidence.record_final_diff_review(CompletionEvidenceItem::passed(
            "diff-1",
            "git diff --check",
            "rev-1",
        ));
        evidence
    }

    #[test]
    fn verified_requires_all_fresh_tool_evidence() {
        let report = derive_completion(
            &[file("src/lib.rs")],
            &complete_evidence(),
            &[validation(ValidationOutcome::Passed)],
        );
        assert_eq!(report.state, CompletionState::Verified);
        assert_eq!(
            report.evidence_ids,
            vec!["diagnostics-1", "diff-1", "inventory-1", "validation-1"]
        );
    }

    #[test]
    fn model_claim_cannot_replace_missing_validation_evidence() {
        let report = derive_completion(&[file("src/lib.rs")], &complete_evidence(), &[]);
        assert_eq!(report.state, CompletionState::Unverified);
        assert_eq!(
            report.blocker.as_deref(),
            Some("record selected validation result before completion")
        );
    }

    #[test]
    fn stale_diagnostics_blocks_completion() {
        let mut evidence = complete_evidence();
        evidence.record_post_write_diagnostics(CompletionEvidenceItem::passed(
            "diagnostics-1",
            "editor diagnostics",
            "rev-old",
        ));
        let report = derive_completion(
            &[file("src/lib.rs")],
            &evidence,
            &[validation(ValidationOutcome::Passed)],
        );
        assert_eq!(report.state, CompletionState::Blocked);
        assert!(report.blocker.expect("blocker").contains("stale"));
    }

    #[test]
    fn skipped_validation_is_only_partially_verified() {
        let report = derive_completion(
            &[file("src/lib.rs")],
            &complete_evidence(),
            &[validation(ValidationOutcome::Skipped)],
        );
        assert_eq!(report.state, CompletionState::PartiallyVerified);
        assert!(report.safe_follow_up.expect("follow-up").contains("cargo test --quiet"));
    }

    #[test]
    fn failed_validation_blocks_completion() {
        let report = derive_completion(
            &[file("src/lib.rs")],
            &complete_evidence(),
            &[validation(ValidationOutcome::Failed)],
        );
        assert_eq!(report.state, CompletionState::Blocked);
        assert!(report.blocker.expect("blocker").contains("validation failed"));
    }

    #[test]
    fn mismatched_inventory_blocks_completion() {
        let report = derive_completion(
            &[file("src/main.rs")],
            &complete_evidence(),
            &[validation(ValidationOutcome::Passed)],
        );
        assert_eq!(report.state, CompletionState::Blocked);
        assert!(report.blocker.expect("blocker").contains("does not match"));
    }
}
