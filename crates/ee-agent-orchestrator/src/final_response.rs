//! Structured final responses built from observed state.
//!
//! [`FinalResponse`] is the typed, user-facing summary a strategic turn
//! produces: changed files, validation commands and outcomes, unresolved
//! risks, follow-up suggestions, and a deterministic prose summary assembled
//! by [`FinalResponseBuilder`].  The builder only ever claims what was
//! recorded — in particular, validation success is asserted exclusively from
//! [`ValidationRecorder`] records, so a turn can never claim `passed` without
//! a recorded passing command.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::memory::MemoryStore;
use crate::tasks::{TaskGraph, TaskId, TaskStatus};
use crate::tools::{SideEffectClass, ToolExecutionLogEntry};

/// Outcome of one recorded validation command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ValidationOutcome {
    /// The command ran and reported success.
    Passed,
    /// The command ran and reported failure.
    Failed,
    /// The command was not run.
    Skipped,
}

/// One recorded validation command and its outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ValidationRecord {
    /// The command that ran (tool name or command line).
    pub command: String,
    /// The recorded outcome.
    pub outcome: ValidationOutcome,
    /// Bounded detail (output summary or error).
    pub detail: Option<String>,
    /// The task that ran the command, when known.
    pub source_task: Option<TaskId>,
}

/// Ordered, append-only store of validation records for one turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationRecorder {
    records: Vec<ValidationRecord>,
}

impl ValidationRecorder {
    /// Creates an empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one command outcome in order.
    pub fn record(
        &mut self,
        command: impl Into<String>,
        outcome: ValidationOutcome,
        detail: Option<String>,
        source_task: Option<TaskId>,
    ) {
        self.records.push(ValidationRecord {
            command: command.into(),
            outcome,
            detail,
            source_task,
        });
    }

    /// All records in recording order.
    #[must_use]
    pub fn records(&self) -> &[ValidationRecord] {
        &self.records
    }

    /// Commands that recorded a passing outcome, in order.
    #[must_use]
    pub fn passed_commands(&self) -> Vec<String> {
        self.records
            .iter()
            .filter(|record| record.outcome == ValidationOutcome::Passed)
            .map(|record| record.command.clone())
            .collect()
    }

    /// Commands that recorded a failing outcome, in order.
    #[must_use]
    pub fn failed_commands(&self) -> Vec<String> {
        self.records
            .iter()
            .filter(|record| record.outcome == ValidationOutcome::Failed)
            .map(|record| record.command.clone())
            .collect()
    }

    /// Whether any command recorded a passing outcome.
    #[must_use]
    pub fn has_passed(&self) -> bool {
        self.records.iter().any(|record| record.outcome == ValidationOutcome::Passed)
    }

    /// Whether any command recorded a failing outcome.
    #[must_use]
    pub fn has_failed(&self) -> bool {
        self.records.iter().any(|record| record.outcome == ValidationOutcome::Failed)
    }
}

/// One file the turn changed, attributed to the task that changed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ChangedFile {
    /// The changed path.
    pub path: String,
    /// The task that changed it, when known.
    pub source_task: Option<TaskId>,
}

/// Structured final response built from observed state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FinalResponse {
    /// Deterministic prose summary assembled from observed state.
    pub summary: String,
    /// Files changed by write-class tool executions.
    pub changed_files: Vec<ChangedFile>,
    /// Recorded validation commands and outcomes.
    pub validation: Vec<ValidationRecord>,
    /// Risks the turn could not resolve.
    pub unresolved_risks: Vec<String>,
    /// Suggested follow-up work.
    pub follow_up_suggestions: Vec<String>,
    /// Provenance entries backing the claims above.
    pub provenance: Vec<String>,
    /// Whether the turn may claim completion: false when required tasks
    /// remain failed or blocked.
    pub can_finish: bool,
}

impl FinalResponse {
    /// Whether any recorded validation command passed.
    #[must_use]
    pub fn validation_passed(&self) -> bool {
        self.validation.iter().any(|record| record.outcome == ValidationOutcome::Passed)
    }

    /// Number of changed files.
    #[must_use]
    pub fn changed_file_count(&self) -> usize {
        self.changed_files.len()
    }
}

impl fmt::Display for FinalResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.summary)
    }
}

/// Derives the turn's changed files from the execution log: successful
/// write-class executions whose arguments carry a `path` string, deduplicated
/// by path in first-occurrence order.
#[must_use]
pub fn changed_files_from_log(
    log: &[ToolExecutionLogEntry],
    source_task: &TaskId,
) -> Vec<ChangedFile> {
    let mut seen = Vec::new();
    let mut files = Vec::new();
    for entry in log {
        if !entry.success || entry.side_effect_class != Some(SideEffectClass::Write) {
            continue;
        }
        let Some(path) = entry.arguments.get("path").and_then(Value::as_str) else {
            continue;
        };
        if seen.contains(&path.to_string()) {
            continue;
        }
        seen.push(path.to_string());
        files.push(ChangedFile { path: path.into(), source_task: Some(source_task.clone()) });
    }
    files
}

/// Builds a [`FinalResponse`] from observed state only.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct FinalResponseBuilder<'a> {
    /// Files changed during the turn.
    pub changed_files: Vec<ChangedFile>,
    /// Validation outcomes recorded during the turn.
    pub validation: &'a ValidationRecorder,
    /// Unresolved risks observed by the caller.
    pub unresolved_risks: Vec<String>,
    /// Follow-up suggestions observed by the caller.
    pub follow_up_suggestions: Vec<String>,
    /// The task graph the turn ran over.
    pub task_graph: &'a TaskGraph,
    /// The memory store the turn ran over (provenance only).
    pub memory: &'a MemoryStore,
    /// Optional progress score: shapes the confidence claim and `can_finish`.
    pub progress: Option<&'a crate::progress::ProgressScore>,
}

impl FinalResponseBuilder<'_> {
    /// Assembles the final response.  Validation status is derived solely
    /// from recorded outcomes; an empty or failing recorder never claims
    /// success, and `can_finish` is false whenever the progress score reports
    /// failed or blocked required tasks.  Secret-like values in paths, risks,
    /// follow-ups, and commands are redacted before assembly.
    #[must_use]
    pub fn build(&self) -> FinalResponse {
        let guard = crate::sensitive_data::SensitiveDataGuard::new();
        let changed_files: Vec<ChangedFile> = self
            .changed_files
            .iter()
            .map(|file| ChangedFile {
                path: guard.redact(&file.path),
                source_task: file.source_task.clone(),
            })
            .collect();
        let unresolved_risks =
            self.unresolved_risks.iter().map(|risk| guard.redact(risk)).collect();
        let follow_up_suggestions =
            self.follow_up_suggestions.iter().map(|suggestion| guard.redact(suggestion)).collect();
        FinalResponse {
            summary: self.build_summary(&changed_files, &guard),
            changed_files,
            validation: self
                .validation
                .records()
                .iter()
                .map(|record| ValidationRecord {
                    command: guard.redact(&record.command),
                    outcome: record.outcome,
                    detail: record.detail.as_ref().map(|detail| guard.redact(detail)),
                    source_task: record.source_task.clone(),
                })
                .collect(),
            unresolved_risks,
            follow_up_suggestions,
            provenance: self.provenance(&guard),
            can_finish: self.progress.is_none_or(|progress| progress.can_finish),
        }
    }

    /// Whether the recorded validation warrants a "passed" claim.
    fn validation_status(&self) -> ValidationStatus {
        if self.validation.has_passed() {
            ValidationStatus::Passed
        } else if self.validation.has_failed() {
            ValidationStatus::Failed
        } else {
            ValidationStatus::None
        }
    }

    fn build_summary(
        &self,
        changed_files: &[ChangedFile],
        guard: &crate::sensitive_data::SensitiveDataGuard,
    ) -> String {
        let total = self.task_graph.len();
        let completed = self
            .task_graph
            .list()
            .iter()
            .filter(|task| task.status == TaskStatus::Completed)
            .count();
        let mut parts = vec![format!("planned tasks: {total}, completed: {completed}")];
        if changed_files.is_empty() {
            parts.push("no files changed".into());
        } else {
            let shown: Vec<&str> = changed_files.iter().take(8).map(|f| f.path.as_str()).collect();
            let mut label = format!("changed files: {}", shown.join(", "));
            if changed_files.len() > shown.len() {
                label.push_str(&format!(" (+{} more)", changed_files.len() - shown.len()));
            }
            parts.push(label);
        }
        match self.validation_status() {
            ValidationStatus::Passed => parts.push(format!(
                "validation passed (recorded): {}",
                self.validation
                    .passed_commands()
                    .iter()
                    .map(|command| guard.redact(command))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            ValidationStatus::Failed => parts.push(format!(
                "validation failed: {}",
                self.validation
                    .failed_commands()
                    .iter()
                    .map(|command| guard.redact(command))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            ValidationStatus::None => {}
        }
        if !self.unresolved_risks.is_empty() {
            parts.push(format!(
                "unresolved risks: {}",
                self.unresolved_risks
                    .iter()
                    .map(|risk| guard.redact(risk))
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
        if !self.follow_up_suggestions.is_empty() {
            parts.push(format!(
                "follow-ups: {}",
                self.follow_up_suggestions
                    .iter()
                    .map(|suggestion| guard.redact(suggestion))
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
        if let Some(progress) = self.progress {
            parts.push(format!("confidence: {:.2}", progress.confidence));
            if !progress.can_finish {
                parts.push(format!(
                    "incomplete: {} failed, {} blocked tasks",
                    progress.failed_tasks, progress.blocked_tasks
                ));
            }
        }
        parts.join("; ")
    }

    fn provenance(&self, guard: &crate::sensitive_data::SensitiveDataGuard) -> Vec<String> {
        let mut entries: Vec<String> = self
            .task_graph
            .list()
            .iter()
            .filter(|task| task.status == TaskStatus::Completed)
            .map(|task| format!("task:{}", task.id))
            .collect();
        entries.extend(self.memory.items().iter().map(|item| format!("memory:{}", item.key)));
        entries.extend(self.changed_files.iter().filter_map(|file| {
            file.source_task
                .as_ref()
                .map(|task| format!("change:{}:{}", guard.redact(&file.path), task))
        }));
        entries.sort();
        entries.dedup();
        entries
    }
}

/// How the recorded validation state reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValidationStatus {
    Passed,
    Failed,
    None,
}

#[cfg(test)]
mod tests {
    use crate::tasks::{TaskGraph, TaskId, TaskStatus};

    use super::*;

    fn write_entry(path: &str, success: bool) -> ToolExecutionLogEntry {
        ToolExecutionLogEntry {
            tool_call_id: "tc-1".into(),
            tool_name: "write_file".into(),
            side_effect_class: Some(SideEffectClass::Write),
            arguments: serde_json::json!({ "path": path, "content": "x" }),
            success,
            summary: if success { "file written".into() } else { "denied".into() },
        }
    }

    fn read_entry() -> ToolExecutionLogEntry {
        ToolExecutionLogEntry {
            tool_call_id: "tc-2".into(),
            tool_name: "read_file".into(),
            side_effect_class: Some(SideEffectClass::Read),
            arguments: serde_json::json!({ "path": "/tmp/x" }),
            success: true,
            summary: "file contents".into(),
        }
    }

    fn task_graph_with_completed_task() -> TaskGraph {
        let mut tasks = TaskGraph::new();
        let root = tasks.create_root("plan", "plan");
        tasks.transition(&root.id, TaskStatus::Completed).expect("completes");
        tasks
    }

    #[test]
    fn final_response_with_no_code_changes_claims_nothing() {
        let tasks = TaskGraph::new();
        let memory = MemoryStore::new(4096);
        let validation = ValidationRecorder::new();
        let response = FinalResponseBuilder {
            changed_files: Vec::new(),
            validation: &validation,
            unresolved_risks: Vec::new(),
            follow_up_suggestions: Vec::new(),
            task_graph: &tasks,
            memory: &memory,
            progress: None,
        }
        .build();
        assert!(response.changed_files.is_empty());
        assert!(!response.validation_passed());
        assert!(!response.summary.contains("changed files"), "{}", response.summary);
        assert!(
            !response.summary.to_ascii_lowercase().contains("validation"),
            "no recorded validation may be claimed: {}",
            response.summary
        );
        assert!(response.provenance.is_empty());
    }

    #[test]
    fn final_response_with_changed_files_and_passed_validation() {
        let tasks = task_graph_with_completed_task();
        let memory = MemoryStore::new(4096);
        let task_id = TaskId::new("task-1");
        let mut validation = ValidationRecorder::new();
        validation.record(
            "run_validation",
            ValidationOutcome::Passed,
            Some("all checks green".into()),
            Some(task_id.clone()),
        );
        let changed = changed_files_from_log(
            &[read_entry(), write_entry("/tmp/a.rs", true), write_entry("/tmp/a.rs", true)],
            &task_id,
        );
        let response = FinalResponseBuilder {
            changed_files: changed,
            validation: &validation,
            unresolved_risks: vec!["coverage below threshold".into()],
            follow_up_suggestions: vec!["add unit tests".into()],
            task_graph: &tasks,
            memory: &memory,
            progress: None,
        }
        .build();
        assert_eq!(response.changed_file_count(), 1, "duplicate path deduped");
        assert_eq!(response.changed_files[0].path, "/tmp/a.rs");
        assert!(response.validation_passed());
        assert!(response.summary.contains("changed files: /tmp/a.rs"), "{}", response.summary);
        assert!(
            response.summary.contains("validation passed (recorded): run_validation"),
            "{}",
            response.summary
        );
        assert!(response.summary.contains("unresolved risks"), "{}", response.summary);
        assert!(response.summary.contains("follow-ups"), "{}", response.summary);
        assert!(response.provenance.contains(&"task:task-1".to_string()));
        assert!(response.provenance.contains(&"change:/tmp/a.rs:task-1".to_string()));
    }

    #[test]
    fn final_response_with_failed_validation_never_claims_passed() {
        let tasks = task_graph_with_completed_task();
        let memory = MemoryStore::new(4096);
        let mut validation = ValidationRecorder::new();
        validation.record(
            "cargo test",
            ValidationOutcome::Failed,
            Some("2 tests failed".into()),
            None,
        );
        let response = FinalResponseBuilder {
            changed_files: Vec::new(),
            validation: &validation,
            unresolved_risks: Vec::new(),
            follow_up_suggestions: Vec::new(),
            task_graph: &tasks,
            memory: &memory,
            progress: None,
        }
        .build();
        assert!(!response.validation_passed());
        assert!(response.summary.contains("validation failed: cargo test"), "{}", response.summary);
        assert!(!response.summary.contains("passed"), "failed validation must not claim success");
    }

    #[test]
    fn changed_files_from_log_only_records_successful_writes_with_paths() {
        let task_id = TaskId::new("task-1");
        let log = vec![
            read_entry(),
            write_entry("/tmp/a.rs", true),
            write_entry("/tmp/b.rs", false),
            ToolExecutionLogEntry {
                tool_call_id: "tc-4".into(),
                tool_name: "write_file".into(),
                side_effect_class: Some(SideEffectClass::Write),
                arguments: serde_json::json!({ "content": "no path" }),
                success: true,
                summary: "file written".into(),
            },
            write_entry("/tmp/a.rs", true),
        ];
        let files = changed_files_from_log(&log, &task_id);
        assert_eq!(files.len(), 1, "failed, pathless, and duplicate writes excluded");
        assert_eq!(files[0].path, "/tmp/a.rs");
        assert_eq!(files[0].source_task, Some(task_id));
    }

    #[test]
    fn validation_recorder_tracks_outcomes_in_order() {
        let mut recorder = ValidationRecorder::new();
        recorder.record("cargo check", ValidationOutcome::Passed, None, None);
        recorder.record("cargo test", ValidationOutcome::Failed, Some("boom".into()), None);
        recorder.record("lint", ValidationOutcome::Skipped, None, None);
        assert_eq!(recorder.passed_commands(), vec!["cargo check"]);
        assert_eq!(recorder.failed_commands(), vec!["cargo test"]);
        assert!(recorder.has_passed());
        assert!(recorder.has_failed());
        assert_eq!(recorder.records().len(), 3);

        // A recorder with only failures can never report success.
        let mut failing = ValidationRecorder::new();
        failing.record("cargo test", ValidationOutcome::Failed, None, None);
        assert!(!failing.has_passed());
    }

    #[test]
    fn final_response_redacts_secret_like_values() {
        let tasks = task_graph_with_completed_task();
        let memory = MemoryStore::new(4096);
        let mut validation = ValidationRecorder::new();
        validation.record(
            "OPENROUTER_API_KEY=sk-live-123 cargo check",
            ValidationOutcome::Passed,
            Some("clean".into()),
            Some(TaskId::new("task-1")),
        );
        let response = FinalResponseBuilder {
            changed_files: vec![ChangedFile {
                path: "/work/sk-live-1234567890.rs".into(),
                source_task: Some(TaskId::new("task-1")),
            }],
            validation: &validation,
            unresolved_risks: vec!["key sk-live-1234567890 leaked".into()],
            follow_up_suggestions: vec!["rotate sk-live-1234567890".into()],
            task_graph: &tasks,
            memory: &memory,
            progress: None,
        }
        .build();
        let joined = format!(
            "{} {:?} {:?} {:?} {:?}",
            response.summary,
            response.changed_files,
            response.validation,
            response.unresolved_risks,
            response.follow_up_suggestions
        );
        assert!(!joined.contains("sk-live-1234567890"), "{joined}");
        assert!(!joined.contains("sk-live-123"), "{joined}");
        assert!(joined.contains("[redacted]"), "{joined}");
        assert!(response.changed_files[0].path.contains("[redacted]"));
    }

    #[test]
    fn final_response_serializes_deterministically() {
        let tasks = TaskGraph::new();
        let memory = MemoryStore::new(4096);
        let validation = ValidationRecorder::new();
        let response = FinalResponseBuilder {
            changed_files: vec![ChangedFile {
                path: "/tmp/a.rs".into(),
                source_task: Some(TaskId::new("task-1")),
            }],
            validation: &validation,
            unresolved_risks: Vec::new(),
            follow_up_suggestions: Vec::new(),
            task_graph: &tasks,
            memory: &memory,
            progress: None,
        }
        .build();
        let json = serde_json::to_string(&response).expect("serializes");
        let restored: FinalResponse = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, response);
    }

    #[test]
    fn unrecorded_passed_command_cannot_be_claimed() {
        // Even a "passing" summary line must come from the recorder: a
        // builder given a recorder without a Passed record cannot produce a
        // passed claim.
        let tasks = TaskGraph::new();
        let memory = MemoryStore::new(4096);
        let mut validation = ValidationRecorder::new();
        validation.record(
            "cargo test",
            ValidationOutcome::Failed,
            Some("1 test failed".into()),
            None,
        );
        validation.record("lint", ValidationOutcome::Skipped, None, None);
        let response = FinalResponseBuilder {
            changed_files: Vec::new(),
            validation: &validation,
            unresolved_risks: Vec::new(),
            follow_up_suggestions: Vec::new(),
            task_graph: &tasks,
            memory: &memory,
            progress: None,
        }
        .build();
        assert!(!response.validation_passed());
        assert!(
            !response.summary.to_ascii_lowercase().contains("passed"),
            "summary must not claim unrecorded success: {}",
            response.summary
        );
    }

    #[test]
    fn provenance_is_sorted_and_deduplicated() {
        let tasks = task_graph_with_completed_task();
        let mut memory = MemoryStore::new(4096);
        memory.insert(crate::memory::MemoryItem::new("cwd", "/work")).expect("inserts");
        memory.insert(crate::memory::MemoryItem::new("cwd", "/work")).expect("inserts");
        let validation = ValidationRecorder::new();
        let response = FinalResponseBuilder {
            changed_files: vec![ChangedFile {
                path: "/tmp/a.rs".into(),
                source_task: Some(TaskId::new("task-1")),
            }],
            validation: &validation,
            unresolved_risks: Vec::new(),
            follow_up_suggestions: Vec::new(),
            task_graph: &tasks,
            memory: &memory,
            progress: None,
        }
        .build();
        let mut expected = vec![
            "change:/tmp/a.rs:task-1".to_string(),
            "task:task-1".to_string(),
            "memory:cwd".to_string(),
        ];
        expected.sort();
        assert_eq!(response.provenance, expected);
    }

    #[test]
    fn final_response_with_progress_reports_confidence_and_can_finish() {
        let tasks = task_graph_with_completed_task();
        let memory = MemoryStore::new(4096);
        let validation = ValidationRecorder::new();
        let mut progress_tracker = crate::progress::ProgressTracker::new();
        progress_tracker.record_review_findings(0);
        let progress = progress_tracker.score(&tasks);
        let response = FinalResponseBuilder {
            changed_files: Vec::new(),
            validation: &validation,
            unresolved_risks: Vec::new(),
            follow_up_suggestions: Vec::new(),
            task_graph: &tasks,
            memory: &memory,
            progress: Some(&progress),
        }
        .build();
        assert!(response.can_finish, "completed tasks finish");
        assert!(response.summary.contains("confidence: 0.10"), "{}", response.summary);
    }

    #[test]
    fn final_response_prevents_finish_when_required_tasks_remain_failed_or_blocked() {
        let memory = MemoryStore::new(4096);
        let validation = ValidationRecorder::new();
        for (status, expected_fragment) in [
            (TaskStatus::Failed, "incomplete: 1 failed, 0 blocked tasks"),
            (TaskStatus::Blocked, "incomplete: 0 failed, 1 blocked tasks"),
        ] {
            let mut tasks = TaskGraph::new();
            let root = tasks.create_root("plan", "plan");
            let child = tasks.create_child(&root.id, "child", "child").expect("child");
            tasks.transition(&child.id, TaskStatus::Running).expect("running");
            tasks.transition(&child.id, status).expect("terminal");
            let progress = crate::progress::ProgressTracker::new().score(&tasks);
            let response = FinalResponseBuilder {
                changed_files: Vec::new(),
                validation: &validation,
                unresolved_risks: Vec::new(),
                follow_up_suggestions: Vec::new(),
                task_graph: &tasks,
                memory: &memory,
                progress: Some(&progress),
            }
            .build();
            assert!(!response.can_finish, "{status:?} must block completion");
            assert!(response.summary.contains(expected_fragment), "{}", response.summary);
        }
    }
}
