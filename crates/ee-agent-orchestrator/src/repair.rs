//! Deterministic, bounded repair-attempt control.
//!
//! [`RepairController`] decides whether a failed diagnostics, diff, or selected
//! validation check may start another repair attempt. It records only bounded
//! fingerprints and typed summaries: prompts, terminal output, and model output
//! are deliberately outside this module's data model.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Default number of repair attempts allowed in one turn.
pub const DEFAULT_MAX_REPAIR_ATTEMPTS: usize = 2;
/// Absolute cap for configured repair attempts.
pub const MAX_REPAIR_ATTEMPTS: usize = 16;
/// Maximum characters retained for one fingerprint or evidence id.
pub const REPAIR_FINGERPRINT_MAX_CHARS: usize = 128;
/// Maximum prior evidence ids retained for one attempt.
pub const MAX_REPAIR_EVIDENCE_IDS: usize = 16;
/// Maximum tool-call fingerprints retained for one progress observation.
pub const MAX_REPAIR_TOOL_CALL_FINGERPRINTS: usize = 16;

/// Limits for deterministic repair within one turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairConfig {
    /// Maximum repair attempts allowed before the controller stops.
    pub max_attempts: usize,
}

impl Default for RepairConfig {
    fn default() -> Self {
        Self { max_attempts: DEFAULT_MAX_REPAIR_ATTEMPTS }
    }
}

impl RepairConfig {
    /// Returns a configuration with its attempt limit clamped to the hard cap.
    #[must_use]
    pub fn bounded(self) -> Self {
        Self { max_attempts: self.max_attempts.min(MAX_REPAIR_ATTEMPTS) }
    }
}

/// Failure source that may request a repair attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairReason {
    /// Current diagnostics contain a repairable failure.
    Diagnostics,
    /// Current diff review found a repairable failure.
    Diff,
    /// A validation result selected for completion failed.
    SelectedValidationFailure,
}

/// Typed, bounded failure facts for a repair request.
///
/// This enum intentionally has no raw command output, diagnostics text, prompt,
/// or model response field. Callers keep such data in their bounded evidence
/// stores and pass only its fingerprint or stable evidence id here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairFailureSummary {
    /// Aggregate diagnostics failure facts.
    Diagnostics {
        /// Number of current diagnostics included in the failure summary.
        diagnostic_count: usize,
        /// Stable fingerprint of the selected diagnostic facts.
        fingerprint: String,
    },
    /// Aggregate diff-review failure facts.
    Diff {
        /// Number of changed files included in the reviewed diff.
        changed_file_count: usize,
        /// Stable fingerprint of the reviewed diff facts.
        fingerprint: String,
    },
    /// A selected validation failure represented by stable evidence only.
    SelectedValidationFailure {
        /// Stable id of the selected validation evidence.
        evidence_id: String,
        /// Stable fingerprint of the selected validation failure facts.
        fingerprint: String,
    },
}

/// Bounded observations collected around one repair attempt.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RepairProgress {
    /// Current context revision. The first attempt trusts caller freshness; each
    /// later attempt must use a revision never used by a prior repair attempt.
    #[serde(default)]
    pub context_revision: String,
    /// Stable fingerprint for the current diff; never raw diff text.
    #[serde(default)]
    pub diff_fingerprint: String,
    /// Stable fingerprint for selected validation failure facts; never output.
    #[serde(default)]
    pub validation_fingerprint: String,
    /// Stable fingerprints of calls made during repair; never tool arguments.
    #[serde(default)]
    pub tool_call_fingerprints: Vec<String>,
    /// Whether this observation has a deterministic repair-progress signal.
    #[serde(default)]
    pub made_progress: bool,
    /// Whether an observed command or tool reached its terminal state.
    #[serde(default)]
    pub terminal: bool,
    /// Whether policy or approval denied required work.
    #[serde(default)]
    pub policy_denied: bool,
    /// Whether context, buffer, or workspace state was stale.
    #[serde(default)]
    pub stale: bool,
    /// Whether cancellation was observed.
    #[serde(default)]
    pub cancelled: bool,
    /// Whether a turn or repair budget was exhausted.
    #[serde(default)]
    pub budget_exhausted: bool,
    /// Whether required environment capability was unavailable.
    #[serde(default)]
    pub unavailable_environment: bool,
}

impl RepairProgress {
    /// Builds a fresh repair observation from a context revision.
    #[must_use]
    pub fn at_revision(context_revision: impl Into<String>) -> Self {
        Self { context_revision: context_revision.into(), ..Self::default() }
    }

    /// Attaches a bounded diff fingerprint to this repair observation.
    #[must_use]
    pub fn with_diff_fingerprint(mut self, fingerprint: impl Into<String>) -> Self {
        self.diff_fingerprint = fingerprint.into();
        self
    }

    /// Attaches a bounded selected-validation fingerprint to this observation.
    #[must_use]
    pub fn with_validation_fingerprint(mut self, fingerprint: impl Into<String>) -> Self {
        self.validation_fingerprint = fingerprint.into();
        self
    }

    /// Records whether this observation contains a deterministic progress signal.
    #[must_use]
    pub fn with_progress(mut self, made_progress: bool) -> Self {
        self.made_progress = made_progress;
        self
    }

    fn bounded(mut self) -> Self {
        self.context_revision = bounded_string(&self.context_revision);
        self.diff_fingerprint = bounded_string(&self.diff_fingerprint);
        self.validation_fingerprint = bounded_string(&self.validation_fingerprint);
        self.tool_call_fingerprints = self
            .tool_call_fingerprints
            .into_iter()
            .take(MAX_REPAIR_TOOL_CALL_FINGERPRINTS)
            .map(|fingerprint| bounded_string(&fingerprint))
            .collect();
        self
    }
}

/// One recorded repair attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RepairAttempt {
    /// One-based repair attempt number.
    pub attempt_number: usize,
    /// Failure source that requested this repair.
    pub reason: RepairReason,
    /// Typed bounded failure facts that selected the repair.
    pub failure: RepairFailureSummary,
    /// Stable ids of prior evidence available to this repair.
    pub prior_evidence_ids: Vec<String>,
    /// Current bounded repair progress signal.
    pub progress: RepairProgress,
}

/// Deterministic reason no further repair attempt may start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairStopReason {
    /// An identical tool-call fingerprint repeated.
    RepeatedIdenticalToolCalls,
    /// No repair progress and no diff change occurred.
    UnchangedDiff,
    /// Same selected validation failure recurred.
    RepeatedValidationFailure,
    /// Repair produced no deterministic progress signal.
    NoProgress,
    /// Policy or approval denied required work.
    PolicyDenial,
    /// Context, buffer, or workspace state was stale.
    StaleState,
    /// Repair was cancelled.
    Cancellation,
    /// Turn or repair budget was exhausted.
    BudgetExhaustion,
    /// A required host, tool, or model operation timed out.
    Timeout,
    /// Required environment capability was unavailable.
    UnavailableEnvironment,
    /// Configured repair-attempt cap was reached.
    AttemptsExhausted,
}

/// Controller outcome for a requested repair attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairDecision {
    /// One bounded repair attempt may start.
    Attempt(RepairAttempt),
    /// No repair attempt may start.
    Stop(RepairStopReason),
}

/// Per-turn deterministic repair controller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairController {
    config: RepairConfig,
    attempts: Vec<RepairAttempt>,
    seen_context_revisions: BTreeSet<String>,
    stop_reason: Option<RepairStopReason>,
}

impl RepairController {
    /// Creates an empty controller for one turn.
    #[must_use]
    pub fn new(config: RepairConfig) -> Self {
        Self {
            config: config.bounded(),
            attempts: Vec::new(),
            seen_context_revisions: BTreeSet::new(),
            stop_reason: None,
        }
    }

    /// Requests a repair start using current, bounded evidence.
    ///
    /// First attempt accepts caller-declared fresh context. Every subsequent
    /// attempt requires a non-empty revision not seen by a prior attempt.
    /// Terminal flags and repeated evidence stop before an additional attempt
    /// can consume model calls, tools, approvals, or time.
    pub fn starts(
        &mut self,
        reason: RepairReason,
        failure: RepairFailureSummary,
        prior_evidence_ids: Vec<String>,
        progress: RepairProgress,
    ) -> RepairDecision {
        if let Some(reason) = self.stop_reason {
            return RepairDecision::Stop(reason);
        }

        let progress = progress.bounded();
        if let Some(reason) = stop_from_flags(&progress) {
            return self.stop(reason);
        }
        if self.attempts.len() >= self.config.max_attempts {
            return self.stop(RepairStopReason::AttemptsExhausted);
        }
        if !self.attempts.is_empty()
            && (progress.context_revision.is_empty()
                || self.seen_context_revisions.contains(&progress.context_revision))
        {
            return self.stop(RepairStopReason::StaleState);
        }
        if let Some(previous) = self.attempts.last() {
            if has_repeated_tool_calls(previous, &progress) {
                return self.stop(RepairStopReason::RepeatedIdenticalToolCalls);
            }
            if has_repeated_validation_failure(previous, &progress) {
                return self.stop(RepairStopReason::RepeatedValidationFailure);
            }
            if !progress.made_progress
                && !progress.diff_fingerprint.is_empty()
                && progress.diff_fingerprint == previous.progress.diff_fingerprint
            {
                return self.stop(RepairStopReason::UnchangedDiff);
            }
            if !progress.made_progress {
                return self.stop(RepairStopReason::NoProgress);
            }
        }

        self.seen_context_revisions.insert(progress.context_revision.clone());
        let attempt = RepairAttempt {
            attempt_number: self.attempts.len() + 1,
            reason,
            failure: bounded_failure(failure),
            prior_evidence_ids: prior_evidence_ids
                .into_iter()
                .take(MAX_REPAIR_EVIDENCE_IDS)
                .map(|id| bounded_string(&id))
                .collect(),
            progress,
        };
        self.attempts.push(attempt.clone());
        RepairDecision::Attempt(attempt)
    }

    /// Records a later observation for current repair attempt.
    ///
    /// Returns a terminal reason when new flags or repeated tool calls require
    /// immediate stop. Fresh-revision checks occur only in [`Self::starts`],
    /// because this observation belongs to already-started attempt.
    pub fn record_progress(&mut self, progress: RepairProgress) -> Option<RepairStopReason> {
        if let Some(reason) = self.stop_reason {
            return Some(reason);
        }
        let progress = progress.bounded();
        if let Some(reason) = stop_from_flags(&progress) {
            self.stop(reason);
            return Some(reason);
        }
        let attempt = self.attempts.last_mut()?;
        if has_duplicate_tool_calls(&progress.tool_call_fingerprints)
            || (!attempt.progress.tool_call_fingerprints.is_empty()
                && attempt.progress.tool_call_fingerprints == progress.tool_call_fingerprints)
        {
            self.stop_reason = Some(RepairStopReason::RepeatedIdenticalToolCalls);
            return self.stop_reason;
        }
        attempt.progress = progress;
        None
    }

    /// Configured bounded repair limit.
    #[must_use]
    pub fn config(&self) -> RepairConfig {
        self.config
    }

    /// Recorded attempts in start order.
    #[must_use]
    pub fn attempts(&self) -> &[RepairAttempt] {
        &self.attempts
    }

    /// Terminal stop reason, when repair can no longer continue.
    #[must_use]
    pub fn stop_reason(&self) -> Option<RepairStopReason> {
        self.stop_reason
    }

    fn stop(&mut self, reason: RepairStopReason) -> RepairDecision {
        self.stop_reason = Some(reason);
        RepairDecision::Stop(reason)
    }
}

impl Default for RepairController {
    fn default() -> Self {
        Self::new(RepairConfig::default())
    }
}

fn stop_from_flags(progress: &RepairProgress) -> Option<RepairStopReason> {
    if progress.policy_denied {
        Some(RepairStopReason::PolicyDenial)
    } else if progress.stale {
        Some(RepairStopReason::StaleState)
    } else if progress.cancelled {
        Some(RepairStopReason::Cancellation)
    } else if progress.budget_exhausted {
        Some(RepairStopReason::BudgetExhaustion)
    } else if progress.unavailable_environment {
        Some(RepairStopReason::UnavailableEnvironment)
    } else {
        None
    }
}

/// Converts an externally observed timeout into a deterministic repair stop.
#[must_use]
pub const fn timeout_stop_reason() -> RepairStopReason {
    RepairStopReason::Timeout
}

fn has_repeated_tool_calls(previous: &RepairAttempt, current: &RepairProgress) -> bool {
    has_duplicate_tool_calls(&current.tool_call_fingerprints)
        || (!previous.progress.tool_call_fingerprints.is_empty()
            && previous.progress.tool_call_fingerprints == current.tool_call_fingerprints)
}

fn has_duplicate_tool_calls(fingerprints: &[String]) -> bool {
    let mut seen = BTreeSet::new();
    fingerprints.iter().any(|fingerprint| !seen.insert(fingerprint))
}

fn has_repeated_validation_failure(previous: &RepairAttempt, current: &RepairProgress) -> bool {
    !current.validation_fingerprint.is_empty()
        && current.validation_fingerprint == previous.progress.validation_fingerprint
}

fn bounded_failure(failure: RepairFailureSummary) -> RepairFailureSummary {
    match failure {
        RepairFailureSummary::Diagnostics { diagnostic_count, fingerprint } => {
            RepairFailureSummary::Diagnostics {
                diagnostic_count,
                fingerprint: bounded_string(&fingerprint),
            }
        }
        RepairFailureSummary::Diff { changed_file_count, fingerprint } => {
            RepairFailureSummary::Diff {
                changed_file_count,
                fingerprint: bounded_string(&fingerprint),
            }
        }
        RepairFailureSummary::SelectedValidationFailure { evidence_id, fingerprint } => {
            RepairFailureSummary::SelectedValidationFailure {
                evidence_id: bounded_string(&evidence_id),
                fingerprint: bounded_string(&fingerprint),
            }
        }
    }
}

fn bounded_string(text: &str) -> String {
    if text.chars().count() <= REPAIR_FINGERPRINT_MAX_CHARS {
        return text.to_string();
    }
    text.chars().take(REPAIR_FINGERPRINT_MAX_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary() -> RepairFailureSummary {
        RepairFailureSummary::Diagnostics { diagnostic_count: 1, fingerprint: "diag-a".into() }
    }

    fn progress(
        revision: &str,
        diff: &str,
        validation: &str,
        made_progress: bool,
    ) -> RepairProgress {
        RepairProgress {
            context_revision: revision.into(),
            diff_fingerprint: diff.into(),
            validation_fingerprint: validation.into(),
            made_progress,
            ..RepairProgress::default()
        }
    }

    fn starts(controller: &mut RepairController, progress: RepairProgress) -> RepairDecision {
        controller.starts(RepairReason::Diagnostics, summary(), vec!["evidence-1".into()], progress)
    }

    #[test]
    fn repair_attempts_are_capped_at_two_by_default() {
        let mut controller = RepairController::default();
        assert!(matches!(
            starts(&mut controller, progress("rev-1", "diff-1", "", true)),
            RepairDecision::Attempt(_)
        ));
        assert!(matches!(
            starts(&mut controller, progress("rev-2", "diff-2", "", true)),
            RepairDecision::Attempt(_)
        ));
        assert_eq!(
            starts(&mut controller, progress("rev-3", "diff-3", "", true)),
            RepairDecision::Stop(RepairStopReason::AttemptsExhausted)
        );
        assert_eq!(controller.attempts().len(), 2);
    }

    #[test]
    fn repeated_context_revision_stops_as_stale() {
        let mut controller = RepairController::default();
        let initial = progress("revision-1", "diff-1", "", true);
        assert!(matches!(starts(&mut controller, initial.clone()), RepairDecision::Attempt(_)));
        assert_eq!(
            starts(&mut controller, initial),
            RepairDecision::Stop(RepairStopReason::StaleState)
        );
    }

    #[test]
    fn repeated_tool_calls_stop_before_second_attempt() {
        let mut controller = RepairController::default();
        let mut first = progress("rev-1", "diff-1", "", true);
        first.tool_call_fingerprints = vec!["tool:abc".into()];
        assert!(matches!(starts(&mut controller, first), RepairDecision::Attempt(_)));

        let mut second = progress("rev-2", "diff-2", "", true);
        second.tool_call_fingerprints = vec!["tool:abc".into()];
        assert_eq!(
            starts(&mut controller, second),
            RepairDecision::Stop(RepairStopReason::RepeatedIdenticalToolCalls)
        );
    }

    #[test]
    fn unchanged_diff_stops_after_no_progress() {
        let mut controller = RepairController::default();
        assert!(matches!(
            starts(&mut controller, progress("rev-1", "diff-1", "", true)),
            RepairDecision::Attempt(_)
        ));
        assert_eq!(
            starts(&mut controller, progress("rev-2", "diff-1", "", false)),
            RepairDecision::Stop(RepairStopReason::UnchangedDiff)
        );
    }

    #[test]
    fn repeated_selected_validation_failure_stops() {
        let mut controller = RepairController::default();
        assert!(matches!(
            starts(&mut controller, progress("rev-1", "diff-1", "validation-a", true)),
            RepairDecision::Attempt(_)
        ));
        assert_eq!(
            starts(&mut controller, progress("rev-2", "diff-2", "validation-a", true)),
            RepairDecision::Stop(RepairStopReason::RepeatedValidationFailure)
        );
    }

    #[test]
    fn policy_denial_stops_before_any_attempt() {
        let mut controller = RepairController::default();
        let mut denied = progress("rev-1", "diff-1", "", false);
        denied.policy_denied = true;
        assert_eq!(
            starts(&mut controller, denied),
            RepairDecision::Stop(RepairStopReason::PolicyDenial)
        );
        assert!(controller.attempts().is_empty());
    }

    #[test]
    fn record_progress_stops_on_duplicate_calls_within_attempt() {
        let mut controller = RepairController::default();
        assert!(matches!(
            starts(&mut controller, progress("rev-1", "diff-1", "", true)),
            RepairDecision::Attempt(_)
        ));
        let mut observed = progress("rev-1", "diff-1", "", false);
        observed.tool_call_fingerprints = vec!["tool:a".into(), "tool:a".into()];
        assert_eq!(
            controller.record_progress(observed),
            Some(RepairStopReason::RepeatedIdenticalToolCalls)
        );
    }

    #[test]
    fn recorded_strings_and_collections_are_bounded() {
        let mut controller = RepairController::default();
        let long = "x".repeat(REPAIR_FINGERPRINT_MAX_CHARS + 10);
        let progress = RepairProgress {
            context_revision: "rev-1".into(),
            diff_fingerprint: long.clone(),
            tool_call_fingerprints: (0..MAX_REPAIR_TOOL_CALL_FINGERPRINTS + 2)
                .map(|index| format!("tool-{index}"))
                .collect(),
            made_progress: true,
            ..RepairProgress::default()
        };
        let decision = controller.starts(
            RepairReason::Diagnostics,
            RepairFailureSummary::Diagnostics { diagnostic_count: 1, fingerprint: long },
            (0..MAX_REPAIR_EVIDENCE_IDS + 2).map(|index| format!("evidence-{index}")).collect(),
            progress,
        );
        let RepairDecision::Attempt(attempt) = decision else { panic!("attempt expected") };
        assert_eq!(attempt.prior_evidence_ids.len(), MAX_REPAIR_EVIDENCE_IDS);
        assert_eq!(
            attempt.progress.tool_call_fingerprints.len(),
            MAX_REPAIR_TOOL_CALL_FINGERPRINTS
        );
        assert_eq!(attempt.progress.diff_fingerprint.chars().count(), REPAIR_FINGERPRINT_MAX_CHARS);
        let RepairFailureSummary::Diagnostics { fingerprint, .. } = attempt.failure else {
            panic!("diagnostic summary expected")
        };
        assert_eq!(fingerprint.chars().count(), REPAIR_FINGERPRINT_MAX_CHARS);
    }
}
