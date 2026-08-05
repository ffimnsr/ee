//! Bounded self-review after tool/edit loops.
//!
//! When [`ReflectionConfig::enabled`], the strategy executor runs one
//! reflection pass after a tool/edit loop: it builds a review request from
//! observed evidence only (changed files, recorded validation results, task
//! state), performs a bounded number of review model calls, converts findings
//! into task-graph items, and runs at most the configured number of fix
//! loops.  The pass never retries blindly: each review call costs a
//! model-call budget slot, each fix loop a loop-iteration budget slot, and
//! both are capped by [`ReflectionConfig`].
//!
//! This module holds the config, evidence assembly, review request
//! construction, finding parsing, and task-graph conversion — all pure and
//! unit-testable.  The loop orchestration itself lives in the strategy
//! executor, which owns the model/budget/tool handles.

use serde::{Deserialize, Serialize};

use crate::budget::BudgetSnapshot;
use crate::final_response::{ValidationRecorder, changed_files_from_log};
use crate::model::{ModelMessage, ModelRequest, ModelResponse, ModelRole, Transcript};
use crate::tasks::{TaskGraph, TaskId, TaskStatus};
use crate::tools::ToolExecutionLogEntry;

/// Maximum review findings converted from one review response.
pub const MAX_REVIEW_FINDINGS: usize = 16;
/// Maximum characters kept per finding.
pub const MAX_FINDING_CHARS: usize = 200;
/// Maximum changed files cited in a review request.
const MAX_CITED_FILES: usize = 8;
/// Maximum validation results cited in a review request.
const MAX_CITED_VALIDATION: usize = 8;
/// Maximum task states cited in a review request.
const MAX_CITED_TASKS: usize = 16;

/// Bounded reflection configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReflectionConfig {
    /// Whether the reflection pass runs after tool/edit loops.
    pub enabled: bool,
    /// Maximum review model calls per turn.
    pub max_review_iterations: usize,
    /// Maximum fix tool-loops per turn.
    pub max_fix_iterations: usize,
}

impl Default for ReflectionConfig {
    fn default() -> Self {
        Self { enabled: false, max_review_iterations: 1, max_fix_iterations: 1 }
    }
}

/// One review finding: a line of evidence from the review response, tied to
/// the task-graph item it was converted into (when it was).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ReviewFinding {
    /// The finding text.
    pub detail: String,
    /// The task item created for this finding, when converted.
    pub task_id: Option<TaskId>,
}

/// Outcome of one reflection pass; all counts are bounded by config.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ReflectionOutcome {
    /// Review model calls performed.
    pub review_calls: usize,
    /// Fix tool-loops run.
    pub fix_loops: usize,
    /// All findings from every review call, in order.
    pub findings: Vec<ReviewFinding>,
    /// Task ids created for findings, in order.
    pub finding_task_ids: Vec<TaskId>,
}

/// Observed evidence fed to a review model call.  Everything here is derived
/// from persisted/recorded state — never fabricated claims.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ReviewContext {
    /// Changed file paths (deduplicated, bounded).
    pub changed_files: Vec<String>,
    /// One line per recorded validation outcome (command, outcome, detail).
    pub validation_summaries: Vec<String>,
    /// One line per task (`id: title (status)`, bounded).
    pub task_state: Vec<String>,
}

/// Builds the review evidence context from the execution log, recorded
/// validation outcomes, and task graph.  All lists are deduplicated and
/// bounded so the review request stays compact.
#[must_use]
pub fn build_review_context(
    log: &[ToolExecutionLogEntry],
    validation: &ValidationRecorder,
    tasks: &TaskGraph,
) -> ReviewContext {
    let mut changed_files = Vec::new();
    for file in changed_files_from_log(log, &TaskId::new("review")) {
        if changed_files.len() >= MAX_CITED_FILES {
            break;
        }
        if !changed_files.contains(&file.path) {
            changed_files.push(file.path);
        }
    }
    let validation_summaries: Vec<String> = validation
        .records()
        .iter()
        .take(MAX_CITED_VALIDATION)
        .map(|record| {
            let outcome = match record.outcome {
                crate::final_response::ValidationOutcome::Passed => "passed",
                crate::final_response::ValidationOutcome::Failed => "failed",
                crate::final_response::ValidationOutcome::Skipped => "skipped",
            };
            match &record.detail {
                Some(detail) => format!("{}: {} — {detail}", record.command, outcome),
                None => format!("{}: {}", record.command, outcome),
            }
        })
        .collect();
    let task_state: Vec<String> = tasks
        .list()
        .iter()
        .take(MAX_CITED_TASKS)
        .map(|task| format!("{}: {} ({:?})", task.id, task.title, task.status))
        .collect();
    ReviewContext { changed_files, validation_summaries, task_state }
}

/// Builds the review `ModelRequest`: the current transcript plus one user
/// message carrying the observed evidence and the finding format.
#[must_use]
pub fn build_review_request(
    transcript: &Transcript,
    context: &ReviewContext,
    tools: Vec<crate::tools::ToolDefinition>,
    budget: BudgetSnapshot,
    task: crate::tasks::TaskNode,
) -> ModelRequest {
    let mut messages = transcript.messages().to_vec();
    messages.push(ModelMessage::text(ModelRole::User, review_prompt(context)));
    // Review requests run through the injection guard too: tool observations
    // in the transcript are untrusted and must be labeled and bounded.
    let prepared = crate::prompt_injection::prepare_request(&messages);
    ModelRequest::new(prepared.messages, tools, budget, task)
}

/// The deterministic review prompt built from observed evidence only.
fn review_prompt(context: &ReviewContext) -> String {
    let mut lines = vec![
        "Review the completed work below. Cite observed evidence only;".to_string(),
        "do not fabricate results.".to_string(),
    ];
    lines.push("Changed files:".into());
    if context.changed_files.is_empty() {
        lines.push("- none".into());
    } else {
        for path in &context.changed_files {
            lines.push(format!("- {path}"));
        }
    }
    lines.push("Validation results:".into());
    if context.validation_summaries.is_empty() {
        lines.push("- none".into());
    } else {
        for summary in &context.validation_summaries {
            lines.push(format!("- {summary}"));
        }
    }
    lines.push("Task state:".into());
    if context.task_state.is_empty() {
        lines.push("- none".into());
    } else {
        for state in &context.task_state {
            lines.push(format!("- {state}"));
        }
    }
    lines.push(String::new());
    lines.push("Report findings as a list, one per line, each starting with \"- \".".into());
    lines.join("\n")
}

/// Parses findings from a review response: non-empty lines starting with
/// `- `, bounded and truncated.  Deterministic and format-documented; any
/// other text is ignored so free-form prose never becomes task items.
#[must_use]
pub fn findings_from_response(response: &ModelResponse) -> Vec<String> {
    let mut findings = Vec::new();
    for line in response.text.lines() {
        let trimmed = line.trim();
        if let Some(detail) = trimmed.strip_prefix("- ").map(str::trim) {
            if detail.is_empty() {
                continue;
            }
            findings.push(crate::tasks::truncate(detail, MAX_FINDING_CHARS));
            if findings.len() >= MAX_REVIEW_FINDINGS {
                break;
            }
        }
    }
    findings
}

/// Converts findings into pending child task items under `parent`, returning
/// the created task ids in the same order as the findings.
pub fn create_finding_tasks(
    graph: &mut TaskGraph,
    parent: &TaskId,
    findings: &[String],
) -> Result<Vec<TaskId>, crate::error::OrchestratorError> {
    let mut ids = Vec::new();
    for finding in findings {
        let task = graph.create_child(parent, "review finding", finding)?;
        ids.push(task.id);
    }
    Ok(ids)
}

/// Marks finding tasks completed with a bounded summary.  Tasks must be
/// pending; the transition runs `Pending → Running → Completed`.
pub fn mark_finding_tasks(
    graph: &mut TaskGraph,
    ids: &[TaskId],
    summary: &str,
) -> Result<(), crate::error::OrchestratorError> {
    for id in ids {
        graph.transition(id, TaskStatus::Running)?;
        graph.transition(id, TaskStatus::Completed)?;
        graph.set_result_summary(id, summary)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::final_response::ValidationOutcome;
    use crate::tasks::truncate;
    use crate::tools::SideEffectClass;

    #[test]
    fn reflection_config_defaults_are_conservative() {
        let config = ReflectionConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.max_review_iterations, 1);
        assert_eq!(config.max_fix_iterations, 1);
    }

    #[test]
    fn reflection_context_is_built_from_observed_evidence() {
        let log = vec![ToolExecutionLogEntry {
            tool_call_id: "tc-1".into(),
            tool_name: "write_file".into(),
            side_effect_class: Some(SideEffectClass::Write),
            arguments: serde_json::json!({ "path": "/tmp/out.rs" }),
            success: true,
            summary: "file written".into(),
        }];
        let mut validation = ValidationRecorder::new();
        validation.record("cargo check", ValidationOutcome::Passed, Some("clean".into()), None);
        let mut tasks = TaskGraph::new();
        tasks.create_root("implement", "implement the change");
        let context = build_review_context(&log, &validation, &tasks);
        assert_eq!(context.changed_files, vec!["/tmp/out.rs"]);
        assert_eq!(context.validation_summaries, vec!["cargo check: passed — clean"]);
        assert_eq!(context.task_state, vec!["task-1: implement (Running)"]);
    }

    #[test]
    fn reflection_context_without_evidence_is_empty() {
        let log = Vec::new();
        let validation = ValidationRecorder::new();
        let tasks = TaskGraph::new();
        let context = build_review_context(&log, &validation, &tasks);
        assert!(context.changed_files.is_empty());
        assert!(context.validation_summaries.is_empty());
        assert!(context.task_state.is_empty());
    }

    #[test]
    fn reflection_request_appends_review_prompt_and_cites_files() {
        let mut transcript = Transcript::new();
        transcript.prepend_system("facts");
        let context = ReviewContext {
            changed_files: vec!["/tmp/out.rs".into()],
            validation_summaries: vec!["cargo check: passed — clean".into()],
            task_state: vec!["task-1: implement (Running)".into()],
        };
        let request = build_review_request(
            &transcript,
            &context,
            Vec::new(),
            BudgetSnapshot {
                iterations_used: 0,
                iterations_max: 16,
                model_calls_used: 0,
                model_calls_max: 16,
                tool_calls_used: 0,
                tool_calls_max: 32,
                subagents_used: 0,
                subagents_max: 8,
                output_bytes_used: 0,
                output_bytes_max: 1024 * 1024,
                input_tokens_used: None,
                input_tokens_max: None,
                output_tokens_used: None,
                output_tokens_max: None,
            },
            crate::tasks::TaskNode::new(TaskId::new("task-1"), "implement", "implement"),
        );
        let messages = request.transcript;
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].role, ModelRole::User);
        let text = messages[1].text_content();
        assert!(text.contains("Changed files:"));
        assert!(text.contains("- /tmp/out.rs"));
        assert!(text.contains("cargo check: passed — clean"));
        assert!(text.contains("task-1: implement"));
        assert!(text.contains("each starting with \"- \""));
    }

    #[test]
    fn reflection_findings_parse_dash_prefixed_lines_only() {
        let response = ModelResponse::new()
            .text("- missing error handling\nfree-form prose\n-  add tests\n\n-   \n- last");
        let findings = findings_from_response(&response);
        assert_eq!(findings, vec!["missing error handling", "add tests", "last"]);
    }

    #[test]
    fn reflection_findings_are_bounded_and_truncated() {
        let mut lines: Vec<String> =
            (0..MAX_REVIEW_FINDINGS - 1).map(|index| format!("- finding {index}")).collect();
        let long = format!("- {}", "x".repeat(500));
        lines.push(long.clone());
        let findings = findings_from_response(&ModelResponse::new().text(lines.join("\n")));
        assert_eq!(findings.len(), MAX_REVIEW_FINDINGS, "extra findings are dropped");
        assert_eq!(findings[0], "finding 0");
        assert_eq!(
            findings[MAX_REVIEW_FINDINGS - 1],
            truncate(&"x".repeat(500), MAX_FINDING_CHARS)
        );
        assert_eq!(findings[MAX_REVIEW_FINDINGS - 1].chars().count(), MAX_FINDING_CHARS + 1);
    }

    #[test]
    fn reflection_findings_without_dash_prefix_are_ignored() {
        let response = ModelResponse::new().text("everything looks fine here");
        assert!(findings_from_response(&response).is_empty());
    }

    #[test]
    fn reflection_finding_tasks_convert_to_graph_items() {
        let mut graph = TaskGraph::new();
        let root = graph.create_root("implement", "implement");
        let findings = vec!["missing error handling".to_string(), "add tests".to_string()];
        let ids = create_finding_tasks(&mut graph, &root.id, &findings).expect("creates tasks");
        assert_eq!(ids, vec![TaskId::new("task-2"), TaskId::new("task-3")]);
        for id in &ids {
            let task = graph.get(id).expect("stored");
            assert_eq!(task.status, TaskStatus::Pending);
            assert_eq!(task.parent, Some(root.id.clone()));
        }
        assert_eq!(graph.get(&ids[0]).expect("stored").title, "review finding");
    }

    #[test]
    fn reflection_marking_findings_completes_pending_tasks() {
        let mut graph = TaskGraph::new();
        let root = graph.create_root("implement", "implement");
        let ids =
            create_finding_tasks(&mut graph, &root.id, &["fix it".to_string()]).expect("creates");
        mark_finding_tasks(&mut graph, &ids, "addressed in fix loop").expect("marks completed");
        let task = graph.get(&ids[0]).expect("stored");
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.result_summary.as_deref(), Some("addressed in fix loop"));
    }

    #[test]
    fn reflection_marking_unknown_finding_task_fails_closed() {
        let mut graph = TaskGraph::new();
        graph.create_root("implement", "implement");
        let error = mark_finding_tasks(&mut graph, &[TaskId::new("task-99")], "x")
            .expect_err("unknown task rejected");
        assert!(matches!(error, crate::error::OrchestratorError::InvalidState(_)));
    }

    #[test]
    fn reflection_outcome_serializes_deterministically() {
        let outcome = ReflectionOutcome {
            review_calls: 1,
            fix_loops: 1,
            findings: vec![ReviewFinding {
                detail: "missing error handling".into(),
                task_id: Some(TaskId::new("task-2")),
            }],
            finding_task_ids: vec![TaskId::new("task-2")],
        };
        let json = serde_json::to_string(&outcome).expect("serializes");
        let restored: ReflectionOutcome = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, outcome);
    }
}
