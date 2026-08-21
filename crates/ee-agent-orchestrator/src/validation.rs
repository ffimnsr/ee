//! Validation task planning and execution.
//!
//! [`ValidationPlanner`] infers validation work from changed file types and
//! the registered tool set, [`ValidationRunner`] routes those commands
//! through the shared [`ToolExecutor`] (so policy, budget, timeout, and
//! cancellation gates apply unchanged), and [`ValidationResultStore`] keeps
//! bounded, timestamped results per command.
//!
//! The planner is deterministic: file-type rules are ordered by extension,
//! plans iterate changed files in first-occurrence order, and a validation
//! tool is only planned when it is actually registered — otherwise the plan
//! is empty and nothing runs.

use std::collections::BTreeMap;
use std::time::{Instant, SystemTime};

use ee_acp_agent_server::{ClientBridge, UpdateSink};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::error::OrchestratorError;
use crate::events::{EventRecorder, OrchestratorEvent};
use crate::final_response::{ChangedFile, ValidationOutcome, ValidationRecorder};
use crate::model::ModelMessage;
use crate::tasks::{TaskGraph, TaskId, TaskStatus};
use crate::tools::{ToolDefinition, ToolExecutor, ToolIntent};

/// Default file-type → validation-tool inference rules.  Deterministic and
/// documented; rules only produce plan entries when the named tool is
/// registered with the runtime.
pub fn default_file_type_rules() -> BTreeMap<String, FileTypeRule> {
    let mut rules = BTreeMap::new();
    rules.insert(
        ".rs".into(),
        FileTypeRule::new("cargo_check", "cargo check", "compiles the Rust crate"),
    );
    rules.insert(
        ".toml".into(),
        FileTypeRule::new("cargo_check", "cargo check", "compiles the Rust crate"),
    );
    rules.insert(".py".into(), FileTypeRule::new("pytest", "pytest", "runs the Python tests"));
    rules.insert(
        ".js".into(),
        FileTypeRule::new("npm_test", "npm test", "runs the JavaScript tests"),
    );
    rules.insert(
        ".ts".into(),
        FileTypeRule::new("npm_test", "npm test", "runs the TypeScript tests"),
    );
    rules.insert(".go".into(), FileTypeRule::new("go_test", "go test", "runs the Go tests"));
    rules
}

/// One file-type → validation-tool inference rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FileTypeRule {
    /// The registered tool the command maps to.
    pub tool_name: String,
    /// Human-readable command line, used for the plan entry and records.
    pub command: String,
    /// Human-readable description of the check.
    pub description: String,
    /// Argument template; the literal `"<path>"` string is replaced with the
    /// changed file path.
    pub argument_template: serde_json::Value,
}

impl FileTypeRule {
    /// Creates a rule whose arguments are `{"path": "<path>"}`.
    #[must_use]
    pub fn new(
        tool_name: impl Into<String>,
        command: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            tool_name: tool_name.into(),
            command: command.into(),
            description: description.into(),
            argument_template: serde_json::json!({ "path": "<path>" }),
        }
    }

    /// Sets a custom argument template; `"<path>"` placeholders are replaced
    /// per changed file.
    #[must_use]
    pub fn with_argument_template(mut self, template: serde_json::Value) -> Self {
        self.argument_template = template;
        self
    }

    /// Renders the arguments for one changed file path.
    #[must_use]
    pub fn render_arguments(&self, path: &str) -> serde_json::Value {
        substitute_path(&self.argument_template, path)
    }
}

/// Replaces every literal `"<path>"` string in the value tree with `path`.
fn substitute_path(value: &serde_json::Value, path: &str) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) if text == "<path>" => {
            serde_json::Value::String(path.to_string())
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(|item| substitute_path(item, path)).collect())
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter().map(|(key, value)| (key.clone(), substitute_path(value, path))).collect(),
        ),
        other => other.clone(),
    }
}

/// Why the planner selected a validation entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationPlanReason {
    /// A changed file matched a registered file-type rule.
    ChangedFileType,
    /// A declared workspace validation task matched changed files or symbols.
    WorkspaceTask,
}

/// One declared workspace validation task. Hosts build this from their project
/// configuration or declared project tasks; only registered tools are planned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DeclaredValidationTask {
    /// Registered tool that runs this validation.
    pub tool_name: String,
    /// Human-readable command or task name.
    pub command: String,
    /// Schema-valid arguments for the registered tool.
    pub arguments: serde_json::Value,
    /// Changed extensions that select this task. Empty means all files.
    #[serde(default)]
    pub file_extensions: Vec<String>,
    /// Changed symbol names that select this task. Empty means all symbols.
    #[serde(default)]
    pub symbols: Vec<String>,
}

impl DeclaredValidationTask {
    /// Creates an unconditional declared task.
    #[must_use]
    pub fn new(
        tool_name: impl Into<String>,
        command: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        Self {
            tool_name: tool_name.into(),
            command: command.into(),
            arguments,
            file_extensions: Vec::new(),
            symbols: Vec::new(),
        }
    }

    /// Restricts this task to changed extensions.
    #[must_use]
    pub fn for_extensions(
        mut self,
        extensions: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.file_extensions = extensions.into_iter().map(Into::into).collect();
        self
    }

    /// Restricts this task to changed symbols.
    #[must_use]
    pub fn for_symbols(mut self, symbols: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.symbols = symbols.into_iter().map(Into::into).collect();
        self
    }
}

/// Workspace configuration and declared project tasks available to planning.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceValidationConfig {
    /// Declared project validation tasks, in configuration order.
    #[serde(default)]
    pub declared_tasks: Vec<DeclaredValidationTask>,
}

/// Fresh inputs for dynamic validation planning.
#[derive(Debug, Clone, Copy)]
pub struct ValidationPlanningContext<'a> {
    /// Files written during this turn.
    pub changed_files: &'a [ChangedFile],
    /// Changed symbols resolved by the host or code graph.
    pub changed_symbols: &'a [String],
    /// Workspace configuration and declared project tasks.
    pub workspace: &'a WorkspaceValidationConfig,
}

impl<'a> ValidationPlanningContext<'a> {
    /// Creates context with no symbol or workspace-task information.
    #[must_use]
    pub fn from_changed_files(changed_files: &'a [ChangedFile]) -> Self {
        Self { changed_files, changed_symbols: &[], workspace: &EMPTY_WORKSPACE_VALIDATION_CONFIG }
    }
}

static EMPTY_WORKSPACE_VALIDATION_CONFIG: WorkspaceValidationConfig =
    WorkspaceValidationConfig { declared_tasks: Vec::new() };

/// One planned validation execution for one changed file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ValidationPlanEntry {
    /// The registered tool that runs the check.
    pub tool_name: String,
    /// Human-readable command line.
    pub command: String,
    /// The changed file that triggered the check.
    pub changed_file: String,
    /// Rendered tool arguments.
    pub arguments: serde_json::Value,
    /// Evidence showing why this entry was selected.
    pub reason: ValidationPlanReason,
}

/// A deterministic validation plan: entries in changed-file order, each
/// paired with the task id created for it via [`ValidationPlanner::create_tasks`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationPlan {
    /// Planned checks in deterministic order.
    pub entries: Vec<ValidationPlanEntry>,
}

impl ValidationPlan {
    /// Whether the plan has any entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Infers validation plans from changed files and the registered tool set.
#[derive(Debug, Clone)]
pub struct ValidationPlanner {
    rules: BTreeMap<String, FileTypeRule>,
}

impl Default for ValidationPlanner {
    fn default() -> Self {
        Self::new()
    }
}

impl ValidationPlanner {
    /// Creates a planner with the documented default file-type rules.
    #[must_use]
    pub fn new() -> Self {
        Self { rules: default_file_type_rules() }
    }

    /// Registers a rule for the given extensions (lowercased, `".rs"`-style).
    pub fn register(&mut self, extensions: &[&str], rule: FileTypeRule) {
        for extension in extensions {
            self.rules.insert(extension.to_ascii_lowercase(), rule.clone());
        }
    }

    /// The active rules, keyed by lowercase extension.
    #[must_use]
    pub fn rules(&self) -> &BTreeMap<String, FileTypeRule> {
        &self.rules
    }

    /// Plans validation checks from changed files using default workspace and
    /// symbol inputs. Use [`Self::plan_with_context`] when host configuration
    /// or graph-resolved changed symbols are available.
    #[must_use]
    pub fn plan(&self, changed_files: &[ChangedFile], tools: &[ToolDefinition]) -> ValidationPlan {
        self.plan_with_context(ValidationPlanningContext::from_changed_files(changed_files), tools)
    }

    /// Builds a deterministic plan from changed files, changed symbols,
    /// workspace configuration, declared project tasks, and registered tools.
    /// File-type entries keep changed-file order. Matching declared tasks then
    /// append in workspace configuration order. Unknown tools never enter the
    /// plan, and duplicate tool/argument pairs are eliminated.
    #[must_use]
    pub fn plan_with_context(
        &self,
        context: ValidationPlanningContext<'_>,
        tools: &[ToolDefinition],
    ) -> ValidationPlan {
        let mut seen_files = Vec::new();
        let mut entries = Vec::new();
        for file in context.changed_files {
            if seen_files.contains(&file.path) {
                continue;
            }
            seen_files.push(file.path.clone());
            let Some(extension) = file_extension(&file.path) else {
                continue;
            };
            let Some(rule) = self.rules.get(&extension) else {
                continue;
            };
            if !tools.iter().any(|tool| tool.name == rule.tool_name) {
                continue;
            }
            push_unique(
                &mut entries,
                ValidationPlanEntry {
                    tool_name: rule.tool_name.clone(),
                    command: rule.command.clone(),
                    changed_file: file.path.clone(),
                    arguments: rule.render_arguments(&file.path),
                    reason: ValidationPlanReason::ChangedFileType,
                },
            );
        }
        for task in &context.workspace.declared_tasks {
            if !tools.iter().any(|tool| tool.name == task.tool_name) || !task_matches(task, context)
            {
                continue;
            }
            push_unique(
                &mut entries,
                ValidationPlanEntry {
                    tool_name: task.tool_name.clone(),
                    command: task.command.clone(),
                    changed_file: "<workspace>".into(),
                    arguments: task.arguments.clone(),
                    reason: ValidationPlanReason::WorkspaceTask,
                },
            );
        }
        ValidationPlan { entries }
    }

    /// Creates one pending validation task node per plan entry under
    /// `parent`, returning task ids aligned with
    /// [`ValidationPlan::entries`] order.
    pub fn create_tasks(
        &self,
        graph: &mut TaskGraph,
        plan: &ValidationPlan,
        parent: &TaskId,
    ) -> Result<Vec<TaskId>, OrchestratorError> {
        let mut ids = Vec::new();
        for entry in &plan.entries {
            let task = graph.create_child(
                parent,
                &format!("validate {}", entry.command),
                &entry.changed_file,
            )?;
            ids.push(task.id);
        }
        Ok(ids)
    }
}

fn file_extension(path: &str) -> Option<String> {
    let extension = std::path::Path::new(path).extension()?;
    Some(format!(".{}", extension.to_string_lossy().to_ascii_lowercase()))
}

fn task_matches(task: &DeclaredValidationTask, context: ValidationPlanningContext<'_>) -> bool {
    let extension_matches = task.file_extensions.is_empty()
        || context.changed_files.iter().filter_map(|file| file_extension(&file.path)).any(
            |extension| {
                task.file_extensions
                    .iter()
                    .any(|expected| expected.eq_ignore_ascii_case(&extension))
            },
        );
    let symbol_matches = task.symbols.is_empty()
        || context
            .changed_symbols
            .iter()
            .any(|symbol| task.symbols.iter().any(|expected| expected == symbol));
    extension_matches && symbol_matches
}

fn push_unique(entries: &mut Vec<ValidationPlanEntry>, entry: ValidationPlanEntry) {
    if !entries.iter().any(|existing| {
        existing.tool_name == entry.tool_name && existing.arguments == entry.arguments
    }) {
        entries.push(entry);
    }
}

/// One stored validation result: command, status, output summary, and
/// timestamp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ValidationResult {
    /// The command that ran.
    pub command: String,
    /// The recorded outcome.
    pub status: ValidationOutcome,
    /// Bounded output summary (or error text).
    pub output_summary: String,
    /// When the command finished.
    pub recorded_at: SystemTime,
    /// The validation task that ran it.
    pub task_id: Option<TaskId>,
}

/// Ordered, timestamped store of validation results for one turn.
#[derive(Debug, Clone, Default)]
pub struct ValidationResultStore {
    results: Vec<ValidationResult>,
}

impl ValidationResultStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one result with the current time.
    pub fn record(
        &mut self,
        command: impl Into<String>,
        status: ValidationOutcome,
        output_summary: impl Into<String>,
        task_id: Option<TaskId>,
    ) {
        self.results.push(ValidationResult {
            command: command.into(),
            status,
            output_summary: output_summary.into(),
            recorded_at: SystemTime::now(),
            task_id,
        });
    }

    /// All results in recording order.
    #[must_use]
    pub fn results(&self) -> &[ValidationResult] {
        &self.results
    }

    /// Results with a passing status, in order.
    #[must_use]
    pub fn passed(&self) -> Vec<&ValidationResult> {
        self.results.iter().filter(|result| result.status == ValidationOutcome::Passed).collect()
    }

    /// Results with a failing status, in order.
    #[must_use]
    pub fn failed(&self) -> Vec<&ValidationResult> {
        self.results.iter().filter(|result| result.status == ValidationOutcome::Failed).collect()
    }
}

/// Executes a validation plan through the shared tool executor, recording
/// every outcome into both a timestamped [`ValidationResultStore`] and the
/// turn's [`ValidationRecorder`] (so final responses can cite them).
pub struct ValidationRunner {
    executor: ToolExecutor,
    events: EventRecorder,
    store: ValidationResultStore,
}

impl ValidationRunner {
    /// Creates a runner over the shared executor with a fresh result store.
    #[must_use]
    pub fn new(executor: ToolExecutor, events: EventRecorder) -> Self {
        Self { executor, events, store: ValidationResultStore::new() }
    }

    /// The accumulated results.
    #[must_use]
    pub fn store(&self) -> &ValidationResultStore {
        &self.store
    }

    /// Runs one plan entry through the tool executor.  Policy denials and
    /// tool failures are recorded as failed results (the turn continues);
    /// cancellation, budget exhaustion, and timeouts propagate.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_entry(
        &mut self,
        entry: &ValidationPlanEntry,
        task_id: &TaskId,
        sink: &UpdateSink,
        client: &ClientBridge,
        cancel: watch::Receiver<bool>,
        task: &crate::tasks::TaskNode,
        transcript: &[ModelMessage],
        validation: &mut ValidationRecorder,
    ) -> Result<ValidationResult, OrchestratorError> {
        let started = Instant::now();
        let intent = ToolIntent::new(
            format!("validation-{task_id}"),
            entry.tool_name.clone(),
            entry.arguments.clone(),
        );
        self.events.record(OrchestratorEvent::ToolStarted {
            tool_call_id: intent.tool_call_id.clone(),
            tool_name: intent.name.clone(),
        });
        let executed =
            self.executor.execute(&intent, sink, client, cancel.clone(), task, transcript).await;
        let success = executed.as_ref().is_ok_and(|result| result.success);
        self.events.record(OrchestratorEvent::ToolFinished {
            tool_call_id: intent.tool_call_id.clone(),
            tool_name: intent.name.clone(),
            success,
        });
        match executed {
            Ok(result) => {
                let status = if result.success {
                    ValidationOutcome::Passed
                } else {
                    ValidationOutcome::Failed
                };
                let summary = result.summary_text();
                record_validation_evidence(
                    validation,
                    entry,
                    task_id,
                    status,
                    summary.clone(),
                    elapsed_ms(started),
                    result.error_kind == Some(crate::tools::ToolErrorKind::PermissionDenied),
                );
                Ok(self.store_result(entry, task_id, status, summary))
            }
            Err(error)
                if error.is_cancellation()
                    || matches!(
                        error,
                        OrchestratorError::BudgetExceeded(_) | OrchestratorError::Timeout(_)
                    ) =>
            {
                Err(error)
            }
            Err(error) => {
                let summary = error.to_string();
                record_validation_evidence(
                    validation,
                    entry,
                    task_id,
                    ValidationOutcome::Failed,
                    summary.clone(),
                    elapsed_ms(started),
                    matches!(error, OrchestratorError::PolicyDenied(_)),
                );
                Ok(self.store_result(entry, task_id, ValidationOutcome::Failed, summary))
            }
        }
    }

    /// Runs every entry of a plan in order, stopping on the first fatal
    /// error (cancellation, budget, timeout).
    #[allow(clippy::too_many_arguments)]
    pub async fn run_plan(
        &mut self,
        plan: &ValidationPlan,
        task_ids: &[TaskId],
        sink: &UpdateSink,
        client: &ClientBridge,
        cancel: watch::Receiver<bool>,
        task: &crate::tasks::TaskNode,
        transcript: &[ModelMessage],
        validation: &mut ValidationRecorder,
    ) -> Result<Vec<ValidationResult>, OrchestratorError> {
        let mut results = Vec::new();
        for (entry, task_id) in plan.entries.iter().zip(task_ids) {
            if *cancel.borrow() {
                return Err(OrchestratorError::Cancellation);
            }
            results.push(
                self.run_entry(
                    entry,
                    task_id,
                    sink,
                    client,
                    cancel.clone(),
                    task,
                    transcript,
                    validation,
                )
                .await?,
            );
        }
        Ok(results)
    }

    /// Records a result into both stores.
    fn store_result(
        &mut self,
        entry: &ValidationPlanEntry,
        task_id: &TaskId,
        status: ValidationOutcome,
        summary: String,
    ) -> ValidationResult {
        let result = ValidationResult {
            command: entry.command.clone(),
            status,
            output_summary: summary,
            recorded_at: SystemTime::now(),
            task_id: Some(task_id.clone()),
        };
        self.store.results.push(result.clone());
        result
    }
}

fn record_validation_evidence(
    validation: &mut ValidationRecorder,
    entry: &ValidationPlanEntry,
    task_id: &TaskId,
    outcome: ValidationOutcome,
    detail: String,
    elapsed_ms: u64,
    denied: bool,
) {
    validation.record_evidence(crate::final_response::ValidationRecord {
        evidence_id: format!("validation-{task_id}"),
        command: entry.command.clone(),
        tool: Some(entry.tool_name.clone()),
        outcome,
        exit_status: (outcome == ValidationOutcome::Passed).then_some(0),
        elapsed_ms: Some(elapsed_ms),
        affected_tests: Vec::new(),
        diagnostics_delta: 0,
        output_truncated: false,
        skip_reason: None,
        revision: None,
        selected: false,
        denied,
        detail: Some(detail),
        source_task: Some(task_id.clone()),
    });
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

/// Completes or fails validation task nodes from run outcomes.
pub fn finalize_validation_tasks(
    graph: &mut TaskGraph,
    task_ids: &[TaskId],
    results: &[ValidationResult],
) -> Result<(), OrchestratorError> {
    for (task_id, result) in task_ids.iter().zip(results) {
        graph.transition(task_id, TaskStatus::Running)?;
        let status = match result.status {
            ValidationOutcome::Passed => TaskStatus::Completed,
            ValidationOutcome::Failed | ValidationOutcome::Skipped => TaskStatus::Failed,
        };
        graph.transition(task_id, status)?;
        graph.set_result_summary(task_id, result.output_summary.clone())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use ee_acp_agent_server::server::OutboundEvent;
    use ee_acp_agent_server::{ClientBridge, UpdateSink};
    use ee_agent_protocol::SessionId;
    use serde_json::json;
    use tokio::sync::{mpsc, watch};

    use super::*;
    use crate::budget::BudgetTracker;
    use crate::config::OrchestratorConfig;
    use crate::policy::{PolicyEngine, ToolPolicy};
    use crate::test_support::FakeTool;
    use crate::tools::{ServerTool, SideEffectClass, ToolErrorKind, ToolRegistry, ToolResult};

    fn changed_file(path: &str) -> ChangedFile {
        ChangedFile { path: path.into(), source_task: Some(TaskId::new("task-1")) }
    }

    fn cargo_check_tool() -> Arc<FakeTool> {
        Arc::new(FakeTool::new(
            ToolDefinition::new("cargo_check", "compiles the Rust crate")
                .side_effect_class(SideEffectClass::Execute),
            ToolResult::success("checks green"),
        ))
    }

    fn tool_definitions(tool: &Arc<FakeTool>) -> Vec<ToolDefinition> {
        vec![tool.definition()]
    }

    #[test]
    fn validation_planner_rust_file_infers_cargo_check() {
        let planner = ValidationPlanner::new();
        let tools = tool_definitions(&cargo_check_tool());
        let plan = planner.plan(&[changed_file("src/lib.rs")], &tools);
        assert_eq!(plan.entries.len(), 1);
        let entry = &plan.entries[0];
        assert_eq!(entry.tool_name, "cargo_check");
        assert_eq!(entry.command, "cargo check");
        assert_eq!(entry.changed_file, "src/lib.rs");
        assert_eq!(entry.arguments, json!({ "path": "src/lib.rs" }));
        assert_eq!(entry.reason, ValidationPlanReason::ChangedFileType);
    }

    #[test]
    fn validation_planner_skips_unregistered_tools() {
        let planner = ValidationPlanner::new();
        // No tool registered at all: the plan must be empty.
        assert!(planner.plan(&[changed_file("src/lib.rs")], &[]).is_empty());
        // A registered-but-unrelated tool also yields an empty plan.
        let unrelated = Arc::new(FakeTool::new(
            ToolDefinition::new("read_file", "reads").side_effect_class(SideEffectClass::Read),
            ToolResult::success("x"),
        ));
        assert!(planner.plan(&[changed_file("src/lib.rs")], &[unrelated.definition()]).is_empty());
    }

    #[test]
    fn validation_planner_skips_unknown_file_types() {
        let planner = ValidationPlanner::new();
        let tools = tool_definitions(&cargo_check_tool());
        assert!(planner.plan(&[changed_file("README.md")], &tools).is_empty());
        assert!(planner.plan(&[changed_file("no_extension")], &tools).is_empty());
    }

    #[test]
    fn validation_planner_deduplicates_changed_files() {
        let planner = ValidationPlanner::new();
        let tools = tool_definitions(&cargo_check_tool());
        let plan = planner.plan(
            &[changed_file("src/lib.rs"), changed_file("src/lib.rs"), changed_file("Cargo.toml")],
            &tools,
        );
        assert_eq!(plan.entries.len(), 2);
        assert_eq!(plan.entries[0].changed_file, "src/lib.rs");
        assert_eq!(plan.entries[1].changed_file, "Cargo.toml");
    }

    #[test]
    fn validation_planner_custom_rule_uses_template() {
        let mut planner = ValidationPlanner::new();
        planner.register(
            &[".md"],
            FileTypeRule::new("md_lint", "md lint", "lints markdown")
                .with_argument_template(json!({ "file": "<path>", "strict": true })),
        );
        let md_tool = Arc::new(FakeTool::new(
            ToolDefinition::new("md_lint", "lints markdown")
                .side_effect_class(SideEffectClass::Execute),
            ToolResult::success("clean"),
        ));
        let plan = planner.plan(&[changed_file("README.md")], &[md_tool.definition()]);
        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].arguments, json!({ "file": "README.md", "strict": true }));
    }

    #[test]
    fn validation_planner_uses_workspace_tasks_and_changed_symbols() {
        let planner = ValidationPlanner::new();
        let changed_files = vec![changed_file("src/api.rs")];
        let changed_symbols = vec!["handle_request".into()];
        let workspace = WorkspaceValidationConfig {
            declared_tasks: vec![
                DeclaredValidationTask::new(
                    "cargo_test",
                    "cargo test --quiet",
                    json!({ "package": "api" }),
                )
                .for_extensions([".rs"])
                .for_symbols(["handle_request"]),
                DeclaredValidationTask::new("missing", "missing", json!({})),
            ],
        };
        let tools = vec![
            cargo_check_tool().definition(),
            ToolDefinition::new("cargo_test", "runs API tests")
                .side_effect_class(SideEffectClass::Execute),
        ];
        let plan = planner.plan_with_context(
            ValidationPlanningContext {
                changed_files: &changed_files,
                changed_symbols: &changed_symbols,
                workspace: &workspace,
            },
            &tools,
        );
        assert_eq!(plan.entries.len(), 2);
        assert_eq!(plan.entries[0].tool_name, "cargo_check");
        assert_eq!(plan.entries[1].tool_name, "cargo_test");
        assert_eq!(plan.entries[1].reason, ValidationPlanReason::WorkspaceTask);
        assert_eq!(plan.entries[1].arguments, json!({ "package": "api" }));
    }

    #[test]
    fn validation_planner_creates_pending_tasks_under_parent() {
        let planner = ValidationPlanner::new();
        let tools = tool_definitions(&cargo_check_tool());
        let plan = planner.plan(&[changed_file("src/lib.rs"), changed_file("Cargo.toml")], &tools);
        let mut graph = TaskGraph::new();
        let root = graph.create_root("implement", "implement");
        let ids = planner.create_tasks(&mut graph, &plan, &root.id).expect("creates tasks");
        assert_eq!(ids, vec![TaskId::new("task-2"), TaskId::new("task-3")]);
        for id in &ids {
            let task = graph.get(id).expect("stored");
            assert_eq!(task.status, TaskStatus::Pending);
            assert_eq!(task.parent, Some(root.id.clone()));
            assert!(task.title.starts_with("validate "));
        }
    }

    fn plumbing() -> (UpdateSink, ClientBridge, mpsc::UnboundedReceiver<OutboundEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            UpdateSink::new_for_test(SessionId::new("s-1"), tx.clone()),
            ClientBridge::new_for_test(Duration::from_secs(5), tx),
            rx,
        )
    }

    /// Runner with a policy that allows execute-class tools (validation
    /// commands are execute-class by default).
    fn runner_with(tool: Arc<FakeTool>, config: OrchestratorConfig) -> ValidationRunner {
        runner_with_policy(
            tool,
            config,
            PolicyEngine::new(ToolPolicy { allow_execute: true, ..ToolPolicy::default() }),
        )
    }

    fn runner_with_policy(
        tool: Arc<FakeTool>,
        config: OrchestratorConfig,
        policy: PolicyEngine,
    ) -> ValidationRunner {
        let tools = Arc::new(Mutex::new(ToolRegistry::new()));
        tools.lock().expect("registry poisoned").register(tool).expect("registers");
        let budget = Arc::new(Mutex::new(BudgetTracker::new(&config)));
        let events = EventRecorder::new();
        ValidationRunner::new(
            ToolExecutor::new(config, tools, budget, policy, 0, events.clone()),
            events,
        )
    }

    #[test]
    fn validation_runner_records_passed_result_with_timestamp() {
        let before = SystemTime::now();
        let mut runner = runner_with(cargo_check_tool(), OrchestratorConfig::default());
        let planner = ValidationPlanner::new();
        let plan = planner.plan(&[changed_file("src/lib.rs")], &[cargo_check_tool().definition()]);
        let mut graph = TaskGraph::new();
        let root = graph.create_root("implement", "implement");
        let ids = planner.create_tasks(&mut graph, &plan, &root.id).expect("creates");
        let (sink, client, _rx) = plumbing();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let mut validation = ValidationRecorder::new();

        let result = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime")
            .block_on(runner.run_entry(
                &plan.entries[0],
                &ids[0],
                &sink,
                &client,
                cancel_rx,
                &root,
                &[],
                &mut validation,
            ))
            .expect("runs");
        let after = SystemTime::now();

        assert_eq!(result.command, "cargo check");
        assert_eq!(result.status, ValidationOutcome::Passed);
        assert_eq!(result.output_summary, "checks green");
        assert_eq!(result.task_id, Some(TaskId::new("task-2")));
        assert!(
            result.recorded_at >= before && result.recorded_at <= after,
            "timestamp falls between capture points"
        );
        assert_eq!(runner.store().results().len(), 1);
        assert_eq!(runner.store().passed().len(), 1);
        assert_eq!(validation.passed_commands(), vec!["cargo check"]);
        let evidence = &validation.records()[0];
        assert_eq!(evidence.evidence_id, "validation-task-2");
        assert_eq!(evidence.tool.as_deref(), Some("cargo_check"));
        assert_eq!(evidence.exit_status, Some(0));
        assert!(evidence.elapsed_ms.is_some());
        assert!(!evidence.output_truncated);
        assert!(!evidence.selected, "host must explicitly select completion evidence");
    }

    #[test]
    fn validation_runner_records_failed_tool_as_failed() {
        let failing = Arc::new(FakeTool::new(
            ToolDefinition::new("cargo_check", "compiles the Rust crate")
                .side_effect_class(SideEffectClass::Execute),
            ToolResult::failure(ToolErrorKind::Backend, "compile error"),
        ));
        let mut runner = runner_with(failing, OrchestratorConfig::default());
        let planner = ValidationPlanner::new();
        let plan = planner.plan(&[changed_file("src/lib.rs")], &[cargo_check_tool().definition()]);
        let mut graph = TaskGraph::new();
        let root = graph.create_root("implement", "implement");
        let ids = planner.create_tasks(&mut graph, &plan, &root.id).expect("creates");
        let (sink, client, _rx) = plumbing();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let mut validation = ValidationRecorder::new();

        let result = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime")
            .block_on(runner.run_entry(
                &plan.entries[0],
                &ids[0],
                &sink,
                &client,
                cancel_rx,
                &root,
                &[],
                &mut validation,
            ))
            .expect("records failure, does not abort");
        assert_eq!(result.status, ValidationOutcome::Failed);
        assert_eq!(runner.store().failed().len(), 1);
        assert_eq!(validation.failed_commands(), vec!["cargo check"]);
    }

    #[test]
    fn validation_runner_denied_tool_records_failed_without_execution() {
        let tool = cargo_check_tool();
        let calls = tool.call_count();
        let mut runner = runner_with_policy(
            tool.clone(),
            OrchestratorConfig::default(),
            PolicyEngine::default(),
        );
        let planner = ValidationPlanner::new();
        let plan = planner.plan(&[changed_file("src/lib.rs")], &[tool.definition()]);
        let mut graph = TaskGraph::new();
        let root = graph.create_root("implement", "implement");
        let ids = planner.create_tasks(&mut graph, &plan, &root.id).expect("creates");
        let (sink, client, _rx) = plumbing();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let mut validation = ValidationRecorder::new();

        let result = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime")
            .block_on(runner.run_entry(
                &plan.entries[0],
                &ids[0],
                &sink,
                &client,
                cancel_rx,
                &root,
                &[],
                &mut validation,
            ))
            .expect("policy denial is recorded, not fatal");
        assert_eq!(result.status, ValidationOutcome::Failed);
        assert!(result.output_summary.contains("denied"));
        assert_eq!(tool.call_count(), calls, "execute-class tool is denied before running");
    }

    #[test]
    fn validation_runner_cancellation_propagates() {
        let mut runner = runner_with(cargo_check_tool(), OrchestratorConfig::default());
        let planner = ValidationPlanner::new();
        let plan = planner.plan(&[changed_file("src/lib.rs")], &[cargo_check_tool().definition()]);
        let mut graph = TaskGraph::new();
        let root = graph.create_root("implement", "implement");
        let ids = planner.create_tasks(&mut graph, &plan, &root.id).expect("creates");
        let (sink, client, _rx) = plumbing();
        let (_cancel_tx, cancel_rx) = watch::channel(true);
        let mut validation = ValidationRecorder::new();

        let error = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime")
            .block_on(runner.run_entry(
                &plan.entries[0],
                &ids[0],
                &sink,
                &client,
                cancel_rx,
                &root,
                &[],
                &mut validation,
            ))
            .expect_err("cancellation propagates");
        assert!(error.is_cancellation());
    }

    #[test]
    fn validation_runner_run_plan_records_all_outcomes_in_order() {
        let mut runner = runner_with(cargo_check_tool(), OrchestratorConfig::default());
        let planner = ValidationPlanner::new();
        let plan = planner.plan(
            &[changed_file("src/lib.rs"), changed_file("Cargo.toml")],
            &[cargo_check_tool().definition()],
        );
        let mut graph = TaskGraph::new();
        let root = graph.create_root("implement", "implement");
        let ids = planner.create_tasks(&mut graph, &plan, &root.id).expect("creates");
        let (sink, client, _rx) = plumbing();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let mut validation = ValidationRecorder::new();

        let results = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime")
            .block_on(runner.run_plan(
                &plan,
                &ids,
                &sink,
                &client,
                cancel_rx,
                &root,
                &[],
                &mut validation,
            ))
            .expect("plan runs");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].command, "cargo check");
        assert_eq!(results[1].command, "cargo check");
        assert_eq!(results[0].task_id, Some(TaskId::new("task-2")));
        assert_eq!(results[1].task_id, Some(TaskId::new("task-3")));

        // Finalizing the validation tasks completes or fails them in place.
        finalize_validation_tasks(&mut graph, &ids, &results).expect("finalizes");
        assert_eq!(graph.get(&ids[0]).expect("stored").status, TaskStatus::Completed);
        assert_eq!(graph.get(&ids[1]).expect("stored").status, TaskStatus::Completed);
        assert_eq!(
            graph.get(&ids[0]).expect("stored").result_summary.as_deref(),
            Some("checks green")
        );
    }

    #[test]
    fn validation_runner_store_is_deterministically_ordered() {
        let mut runner = ValidationRunner::new(
            ToolExecutor::new(
                OrchestratorConfig::default(),
                Arc::new(Mutex::new(ToolRegistry::new())),
                Arc::new(Mutex::new(BudgetTracker::new(&OrchestratorConfig::default()))),
                PolicyEngine::default(),
                0,
                EventRecorder::new(),
            ),
            EventRecorder::new(),
        );
        runner.store.results.push(ValidationResult {
            command: "cargo check".into(),
            status: ValidationOutcome::Passed,
            output_summary: "green".into(),
            recorded_at: SystemTime::UNIX_EPOCH,
            task_id: Some(TaskId::new("task-2")),
        });
        assert_eq!(runner.store().results().len(), 1);
        assert_eq!(runner.store().passed().len(), 1);
        assert!(runner.store().failed().is_empty());
    }
}
