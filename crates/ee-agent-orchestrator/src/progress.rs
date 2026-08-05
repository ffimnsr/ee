//! Progress scoring for one turn.
//!
//! [`ProgressTracker`] accumulates observed progress signals — completed
//! tool calls, recorded validation outcomes, review findings — and
//! [`ProgressTracker::score`] projects them (together with the task graph)
//! onto a deterministic [`ProgressScore`] with a 0.0–1.0 confidence value and
//! a `can_finish` flag.  `can_finish` is false whenever required tasks remain
//! failed or blocked, so a turn can never claim completion over broken task
//! state.
//!
//! The confidence weights are fixed and documented; unknown provider token
//! usage does not affect confidence (budgets already fail closed on it).

use serde::{Deserialize, Serialize};

use crate::final_response::{ValidationOutcome, ValidationRecorder};
use crate::tasks::{TaskGraph, TaskStatus};
use crate::tools::{ToolExecutionLogEntry, ToolResult};

/// Confidence gained per completed tool call.
const TOOL_WEIGHT: f64 = 0.15;
/// Cap on confidence gained from completed tool calls.
const TOOL_WEIGHT_CAP: f64 = 0.4;
/// Confidence gained when validation passed.
const VALIDATION_PASS_WEIGHT: f64 = 0.3;
/// Confidence lost when validation failed.
const VALIDATION_FAIL_WEIGHT: f64 = 0.3;
/// Confidence gained when the review found no issues.
const CLEAN_REVIEW_WEIGHT: f64 = 0.1;
/// Confidence lost when the review found issues.
const REVIEW_FINDINGS_WEIGHT: f64 = 0.1;

/// Accumulated progress observations for one turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProgressTracker {
    completed_tools: usize,
    validation_passed: bool,
    validation_failed: bool,
    review_performed: bool,
    review_findings: usize,
}

impl ProgressTracker {
    /// Creates an empty tracker (zero confidence).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one successful tool call.
    pub fn record_tool_success(&mut self) {
        self.completed_tools += 1;
    }

    /// Records one tool outcome; successes count toward confidence.
    pub fn record_tool_result(&mut self, result: &ToolResult) {
        if result.success {
            self.completed_tools += 1;
        }
    }

    /// Records one validation outcome.
    pub fn record_validation_outcome(&mut self, outcome: ValidationOutcome) {
        match outcome {
            ValidationOutcome::Passed => self.validation_passed = true,
            ValidationOutcome::Failed => self.validation_failed = true,
            ValidationOutcome::Skipped => {}
        }
    }

    /// Records one review outcome: `count` findings from the reflection
    /// pass.  A performed review with zero findings earns clean-review
    /// credit; findings lower confidence.
    pub fn record_review_findings(&mut self, count: usize) {
        self.review_performed = true;
        self.review_findings = count;
    }

    /// Builds a tracker from observed turn state: successful executions in
    /// the log, recorded validation outcomes, and the reflection outcome
    /// (`None` when no review ran, `Some(findings)` otherwise).
    #[must_use]
    pub fn from_execution_log(
        log: &[ToolExecutionLogEntry],
        validation: &ValidationRecorder,
        review: Option<usize>,
    ) -> Self {
        let mut tracker = Self::new();
        for entry in log {
            if entry.success {
                tracker.completed_tools += 1;
            }
        }
        for record in validation.records() {
            tracker.record_validation_outcome(record.outcome);
        }
        if let Some(findings) = review {
            tracker.record_review_findings(findings);
        }
        tracker
    }

    /// Number of recorded successful tool calls.
    #[must_use]
    pub fn completed_tools(&self) -> usize {
        self.completed_tools
    }

    /// Projects the observed progress onto a score for the given task graph.
    #[must_use]
    pub fn score(&self, graph: &TaskGraph) -> ProgressScore {
        let failed_tasks =
            graph.list().iter().filter(|task| task.status == TaskStatus::Failed).count();
        let blocked_tasks =
            graph.list().iter().filter(|task| task.status == TaskStatus::Blocked).count();

        let mut confidence = 0.0;
        confidence += (self.completed_tools as f64 * TOOL_WEIGHT).min(TOOL_WEIGHT_CAP);
        if self.validation_passed {
            confidence += VALIDATION_PASS_WEIGHT;
        }
        if self.validation_failed {
            confidence -= VALIDATION_FAIL_WEIGHT;
        }
        if self.review_performed {
            confidence += if self.review_findings == 0 {
                CLEAN_REVIEW_WEIGHT
            } else {
                -REVIEW_FINDINGS_WEIGHT
            };
        }
        confidence = confidence.clamp(0.0, 1.0);

        ProgressScore {
            confidence,
            completed_tools: self.completed_tools,
            validation_passed: self.validation_passed,
            validation_failed: self.validation_failed,
            review_findings: self.review_findings,
            failed_tasks,
            blocked_tasks,
            can_finish: failed_tasks == 0 && blocked_tasks == 0,
        }
    }
}

/// One turn's progress projection.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProgressScore {
    /// Deterministic 0.0–1.0 completion confidence.
    pub confidence: f64,
    /// Successful tool executions observed.
    pub completed_tools: usize,
    /// Whether any validation command recorded a pass.
    pub validation_passed: bool,
    /// Whether any validation command recorded a failure.
    pub validation_failed: bool,
    /// Review findings observed by the reflection pass.
    pub review_findings: usize,
    /// Tasks in a failed state.
    pub failed_tasks: usize,
    /// Tasks in a blocked state.
    pub blocked_tasks: usize,
    /// False when required tasks remain failed or blocked.
    pub can_finish: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::{TaskGraph, TaskStatus};

    fn graph_with(task_statuses: &[TaskStatus]) -> TaskGraph {
        let mut graph = TaskGraph::new();
        let root = graph.create_root("implement", "implement");
        for status in task_statuses {
            let child = graph.create_child(&root.id, "child", "child work").expect("child");
            graph.transition(&child.id, TaskStatus::Running).expect("running");
            graph.transition(&child.id, *status).expect("transition");
        }
        graph
    }

    #[test]
    fn progress_starts_at_zero_confidence() {
        let tracker = ProgressTracker::new();
        let score = tracker.score(&graph_with(&[]));
        assert_eq!(score.confidence, 0.0);
        assert_eq!(score.completed_tools, 0);
        assert!(score.can_finish);
    }

    #[test]
    fn progress_confidence_updates_from_completed_tools() {
        let mut tracker = ProgressTracker::new();
        tracker.record_tool_success();
        tracker.record_tool_success();
        let score = tracker.score(&graph_with(&[]));
        assert!((score.confidence - 0.3).abs() < 1e-9, "{}", score.confidence);
        assert_eq!(score.completed_tools, 2);
    }

    #[test]
    fn progress_tool_confidence_is_capped() {
        let mut tracker = ProgressTracker::new();
        for _ in 0..10 {
            tracker.record_tool_success();
        }
        let score = tracker.score(&graph_with(&[]));
        assert!((score.confidence - 0.4).abs() < 1e-9, "{}", score.confidence);
    }

    #[test]
    fn progress_validation_pass_raises_confidence() {
        let mut tracker = ProgressTracker::new();
        tracker.record_validation_outcome(ValidationOutcome::Passed);
        let score = tracker.score(&graph_with(&[]));
        assert!((score.confidence - 0.3).abs() < 1e-9, "{}", score.confidence);
        assert!(score.validation_passed);
    }

    #[test]
    fn progress_validation_failure_lowers_confidence() {
        let mut tracker = ProgressTracker::new();
        tracker.record_tool_success();
        tracker.record_validation_outcome(ValidationOutcome::Failed);
        let score = tracker.score(&graph_with(&[]));
        // 0.15 (tool) - 0.3 (failed validation) clamps to zero.
        assert!((score.confidence - 0.0).abs() < 1e-9, "clamped at zero: {}", score.confidence);
        assert!(score.validation_failed);
    }

    #[test]
    fn progress_review_findings_lower_confidence() {
        let mut clean = ProgressTracker::new();
        clean.record_tool_success();
        clean.record_review_findings(0);
        let mut with_findings = ProgressTracker::new();
        with_findings.record_tool_success();
        with_findings.record_review_findings(2);
        let clean_score = clean.score(&graph_with(&[]));
        let score = with_findings.score(&graph_with(&[]));
        assert!((clean_score.confidence - 0.25).abs() < 1e-9, "{}", clean_score.confidence);
        assert!((score.confidence - 0.05).abs() < 1e-9, "{}", score.confidence);
        assert!(clean_score.confidence > score.confidence);
    }

    #[test]
    fn progress_no_review_gets_no_clean_review_credit() {
        // Without a performed review there is neither credit nor penalty.
        let score = ProgressTracker::new().score(&graph_with(&[]));
        assert_eq!(score.confidence, 0.0);
        let mut reviewed = ProgressTracker::new();
        reviewed.record_review_findings(0);
        let reviewed_score = reviewed.score(&graph_with(&[]));
        assert!((reviewed_score.confidence - 0.1).abs() < 1e-9, "{}", reviewed_score.confidence);
    }

    #[test]
    fn progress_confidence_combines_all_signals() {
        let mut tracker = ProgressTracker::new();
        tracker.record_tool_success();
        tracker.record_tool_success();
        tracker.record_validation_outcome(ValidationOutcome::Passed);
        tracker.record_review_findings(0);
        let score = tracker.score(&graph_with(&[]));
        // 0.3 (tools) + 0.3 (validation pass) + 0.1 (clean review) = 0.7
        assert!((score.confidence - 0.7).abs() < 1e-9, "{}", score.confidence);
    }

    #[test]
    fn progress_can_finish_false_when_tasks_failed_or_blocked() {
        let score = ProgressTracker::new().score(&graph_with(&[TaskStatus::Failed]));
        assert!(!score.can_finish);
        assert_eq!(score.failed_tasks, 1);

        let score = ProgressTracker::new().score(&graph_with(&[TaskStatus::Blocked]));
        assert!(!score.can_finish);
        assert_eq!(score.blocked_tasks, 1);

        let score =
            ProgressTracker::new().score(&graph_with(&[TaskStatus::Completed, TaskStatus::Failed]));
        assert!(!score.can_finish, "one failed required task blocks completion");
    }

    #[test]
    fn progress_from_execution_log_counts_successes_and_validation() {
        use crate::final_response::ValidationRecorder;
        use crate::tools::{SideEffectClass, ToolErrorKind};

        let log = vec![
            ToolExecutionLogEntry {
                tool_call_id: "tc-1".into(),
                tool_name: "write_file".into(),
                side_effect_class: Some(SideEffectClass::Write),
                arguments: serde_json::json!({}),
                success: true,
                summary: "ok".into(),
            },
            ToolExecutionLogEntry {
                tool_call_id: "tc-2".into(),
                tool_name: "write_file".into(),
                side_effect_class: Some(SideEffectClass::Write),
                arguments: serde_json::json!({}),
                success: false,
                summary: ToolErrorKind::Backend.as_str().into(),
            },
        ];
        let mut validation = ValidationRecorder::new();
        validation.record("cargo check", ValidationOutcome::Passed, None, None);
        let tracker = ProgressTracker::from_execution_log(&log, &validation, Some(0));
        assert_eq!(tracker.completed_tools(), 1, "only successful executions count");
        let score = tracker.score(&graph_with(&[]));
        // 0.15 (tool) + 0.3 (validation pass) + 0.1 (clean review) = 0.55
        assert!((score.confidence - 0.55).abs() < 1e-9, "{}", score.confidence);
    }

    #[test]
    fn progress_scores_serialize_deterministically() {
        let score = ProgressTracker::new().score(&graph_with(&[TaskStatus::Failed]));
        let json = serde_json::to_string(&score).expect("serializes");
        let restored: ProgressScore = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored.confidence, score.confidence);
        assert!(!restored.can_finish);
    }
}
