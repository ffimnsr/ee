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

use crate::command_intelligence::{
    ValidationApprovalClass, ValidationCommandFailure, ValidationCommandMetadata,
    ValidationEscalation, ValidationScope,
};
use crate::error::OrchestratorError;
use crate::events::{EventRecorder, OrchestratorEvent};
use crate::final_response::{ChangedFile, ValidationOutcome, ValidationRecorder};
use crate::model::ModelMessage;
use crate::retries::RetryPolicy;
use crate::sensitive_data::redact;
use crate::tasks::{TaskGraph, TaskId, TaskStatus};
use crate::tools::{ToolDefinition, ToolErrorKind, ToolExecutor, ToolIntent, ToolResult};

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

/// A workspace task with stable command identity, scope, prerequisites,
/// approval routing, and affected test identifiers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DeclaredValidationCommand {
    /// Existing task definition and selection predicates.
    pub task: DeclaredValidationTask,
    /// Stable command metadata. Invalid metadata is rejected from plans.
    pub metadata: ValidationCommandMetadata,
}

impl DeclaredValidationCommand {
    /// Creates a command declaration from an existing task definition.
    #[must_use]
    pub fn new(task: DeclaredValidationTask, command_id: impl Into<String>) -> Self {
        Self { task, metadata: ValidationCommandMetadata::targeted(command_id) }
    }

    /// Replaces metadata with an explicitly declared version.
    #[must_use]
    pub fn with_metadata(mut self, metadata: ValidationCommandMetadata) -> Self {
        self.metadata = metadata;
        self
    }
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
    /// Legacy declared project validation tasks, in configuration order.
    /// They remain supported and receive targeted metadata keyed by tool name.
    #[serde(default)]
    pub declared_tasks: Vec<DeclaredValidationTask>,
    /// Versioned command declarations with stable ids and command policy metadata.
    #[serde(default)]
    pub declared_commands: Vec<DeclaredValidationCommand>,
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
    WorkspaceValidationConfig { declared_tasks: Vec::new(), declared_commands: Vec::new() };

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
    /// Stable workspace command metadata, including id and prerequisites.
    pub metadata: ValidationCommandMetadata,
    /// Whether this is focused work or a justified broader escalation.
    pub escalation: ValidationEscalation,
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
    /// Focused file and symbol checks always precede broader workspace checks.
    /// Broader checks are marked as escalations and run only after focused
    /// checks pass. Unknown tools, invalid metadata, and duplicate command-id/
    /// argument pairs never enter the plan.
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
                    metadata: ValidationCommandMetadata::targeted(rule.tool_name.clone()),
                    escalation: ValidationEscalation::Direct,
                },
            );
        }
        for task in &context.workspace.declared_tasks {
            let metadata = ValidationCommandMetadata::targeted(task.tool_name.clone());
            push_declared_task(&mut entries, task, metadata, context, tools);
        }
        for command in &context.workspace.declared_commands {
            push_declared_task(
                &mut entries,
                &command.task,
                command.metadata.clone(),
                context,
                tools,
            );
        }
        mark_workspace_escalations(&mut entries);
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

fn push_declared_task(
    entries: &mut Vec<ValidationPlanEntry>,
    task: &DeclaredValidationTask,
    metadata: ValidationCommandMetadata,
    context: ValidationPlanningContext<'_>,
    tools: &[ToolDefinition],
) {
    let Some(definition) = tools.iter().find(|tool| tool.name == task.tool_name) else {
        return;
    };
    if !metadata.is_valid()
        || !task_matches(task, context)
        || (metadata.approval == ValidationApprovalClass::Host && !definition.host_approval)
    {
        return;
    }
    push_unique(
        entries,
        ValidationPlanEntry {
            tool_name: task.tool_name.clone(),
            command: task.command.clone(),
            changed_file: "<workspace>".into(),
            arguments: task.arguments.clone(),
            reason: ValidationPlanReason::WorkspaceTask,
            metadata,
            escalation: ValidationEscalation::Direct,
        },
    );
}

fn mark_workspace_escalations(entries: &mut [ValidationPlanEntry]) {
    let has_focused = entries.iter().any(|entry| entry.metadata.scope == ValidationScope::Targeted);
    if !has_focused {
        return;
    }
    for entry in entries {
        if entry.metadata.scope == ValidationScope::Workspace {
            entry.escalation = ValidationEscalation::AfterFocusedPass;
        }
    }
}

fn push_unique(entries: &mut Vec<ValidationPlanEntry>, entry: ValidationPlanEntry) {
    if !entries.iter().any(|existing| {
        existing.metadata.command_id == entry.metadata.command_id
            && existing.arguments == entry.arguments
    }) {
        entries.push(entry);
    }
}

/// Maximum bytes retained from a validation command's redacted output.
pub const VALIDATION_COMMAND_OUTPUT_MAX_BYTES: usize = 8 * 1024;

/// One structured validation command result. Output is redacted and capped
/// before recording, while command identity, attempts, policy failures, and
/// selected tests remain available as Phase 9 completion evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ValidationResult {
    /// Stable workspace command id.
    pub command_id: String,
    /// The command that ran.
    pub command: String,
    /// The recorded outcome.
    pub status: ValidationOutcome,
    /// Typed failure classification when the command did not pass.
    pub failure: Option<ValidationCommandFailure>,
    /// Process exit status when the host supplies one; success is `0`.
    pub exit_status: Option<i32>,
    /// Elapsed execution time across all attempts.
    pub elapsed_ms: u64,
    /// Stable affected test or check identifiers.
    pub test_ids: Vec<String>,
    /// Net diagnostics change observed by the host, when known. Runner starts
    /// at zero; write transactions attach revision-bound diagnostics separately.
    pub diagnostics_delta: i64,
    /// Whether secret-like content was redacted from output.
    pub output_redacted: bool,
    /// Whether output exceeded [`VALIDATION_COMMAND_OUTPUT_MAX_BYTES`].
    pub output_truncated: bool,
    /// Number of attempts, including initial dispatch.
    pub attempts: u32,
    /// Explicit transient failure reasons that triggered retries.
    pub retry_reasons: Vec<String>,
    /// Why broader validation could run, when applicable.
    pub escalation: ValidationEscalation,
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
        let command = command.into();
        self.results.push(ValidationResult {
            command_id: command.clone(),
            command,
            status,
            failure: (status != ValidationOutcome::Passed)
                .then_some(ValidationCommandFailure::CommandFailed),
            exit_status: (status == ValidationOutcome::Passed).then_some(0),
            elapsed_ms: 0,
            test_ids: Vec::new(),
            diagnostics_delta: 0,
            output_redacted: false,
            output_truncated: false,
            attempts: 0,
            retry_reasons: Vec::new(),
            escalation: ValidationEscalation::Direct,
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
    retry_policy: RetryPolicy,
}

impl ValidationRunner {
    /// Creates a runner over the shared executor with a fresh result store and
    /// the bounded default transient-retry policy.
    #[must_use]
    pub fn new(executor: ToolExecutor, events: EventRecorder) -> Self {
        Self {
            executor,
            events,
            store: ValidationResultStore::new(),
            retry_policy: RetryPolicy::default(),
        }
    }

    /// Replaces the explicit transient-retry policy for future commands.
    #[must_use]
    pub fn with_retry_policy(mut self, retry_policy: RetryPolicy) -> Self {
        self.retry_policy = retry_policy;
        self
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
        let executed = self
            .execute_with_retries(&intent, sink, client, cancel.clone(), task, transcript)
            .await;
        let success = executed.as_ref().is_ok_and(|result| result.result.success);
        self.events.record(OrchestratorEvent::ToolFinished {
            tool_call_id: intent.tool_call_id.clone(),
            tool_name: intent.name.clone(),
            success,
        });
        match executed {
            Ok(executed) => {
                let status = if executed.result.success {
                    ValidationOutcome::Passed
                } else {
                    ValidationOutcome::Failed
                };
                let failure = validation_failure_from_tool(&executed.result);
                let (summary, output_redacted, output_truncated) =
                    bounded_redacted_output(&executed.result.summary_text());
                let result = self.store_result(
                    entry,
                    task_id,
                    status,
                    failure,
                    summary,
                    elapsed_ms(started),
                    output_redacted,
                    output_truncated,
                    executed.attempts,
                    executed.retry_reasons,
                );
                record_validation_evidence(validation, entry, task_id, &result);
                Ok(result)
            }
            Err(error) => {
                let failure = validation_failure_from_error(&error);
                let (summary, output_redacted, output_truncated) =
                    bounded_redacted_output(&error.to_string());
                let result = self.store_result(
                    entry,
                    task_id,
                    ValidationOutcome::Failed,
                    Some(failure),
                    summary,
                    elapsed_ms(started),
                    output_redacted,
                    output_truncated,
                    1,
                    Vec::new(),
                );
                record_validation_evidence(validation, entry, task_id, &result);
                if error.is_cancellation()
                    || matches!(
                        error,
                        OrchestratorError::BudgetExceeded(_) | OrchestratorError::Timeout(_)
                    )
                {
                    Err(error)
                } else {
                    Ok(result)
                }
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
            if let Some(reason) = execution_blocker(entry, plan, &results) {
                let result = self.store_result(
                    entry,
                    task_id,
                    ValidationOutcome::Skipped,
                    Some(ValidationCommandFailure::MissingDependency),
                    reason,
                    0,
                    false,
                    false,
                    0,
                    Vec::new(),
                );
                record_validation_evidence(validation, entry, task_id, &result);
                results.push(result);
                continue;
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

    async fn execute_with_retries(
        &self,
        intent: &ToolIntent,
        sink: &UpdateSink,
        client: &ClientBridge,
        cancel: watch::Receiver<bool>,
        task: &crate::tasks::TaskNode,
        transcript: &[ModelMessage],
    ) -> Result<RetriedValidationToolResult, OrchestratorError> {
        let mut attempts = 0_u32;
        let mut retry_reasons = Vec::new();
        loop {
            let result = self
                .executor
                .execute(intent, sink, client, cancel.clone(), task, transcript)
                .await?;
            attempts = attempts.saturating_add(1);
            let Some(kind) = result.error_kind else {
                return Ok(RetriedValidationToolResult { result, attempts, retry_reasons });
            };
            if result.success
                || !self.retry_policy.is_transient(&result)
                || attempts > self.retry_policy.max_retries as u32
            {
                return Ok(RetriedValidationToolResult { result, attempts, retry_reasons });
            }
            retry_reasons.push(kind.as_str().to_string());
            self.retry_policy.backoff.sleep_for((attempts - 1) as usize).await;
        }
    }

    /// Records a result into both stores.
    #[allow(clippy::too_many_arguments)]
    fn store_result(
        &mut self,
        entry: &ValidationPlanEntry,
        task_id: &TaskId,
        status: ValidationOutcome,
        failure: Option<ValidationCommandFailure>,
        summary: String,
        elapsed_ms: u64,
        output_redacted: bool,
        output_truncated: bool,
        attempts: u32,
        retry_reasons: Vec<String>,
    ) -> ValidationResult {
        let result = ValidationResult {
            command_id: entry.metadata.command_id.clone(),
            command: entry.command.clone(),
            status,
            failure,
            exit_status: (status == ValidationOutcome::Passed).then_some(0),
            elapsed_ms,
            test_ids: entry.metadata.test_ids.clone(),
            diagnostics_delta: 0,
            output_redacted,
            output_truncated,
            attempts,
            retry_reasons,
            escalation: entry.escalation,
            output_summary: summary,
            recorded_at: SystemTime::now(),
            task_id: Some(task_id.clone()),
        };
        self.store.results.push(result.clone());
        result
    }
}

struct RetriedValidationToolResult {
    result: ToolResult,
    attempts: u32,
    retry_reasons: Vec<String>,
}

fn validation_failure_from_tool(result: &ToolResult) -> Option<ValidationCommandFailure> {
    (!result.success).then_some(match result.error_kind {
        Some(ToolErrorKind::Timeout) => ValidationCommandFailure::Timeout,
        Some(ToolErrorKind::Cancelled) => ValidationCommandFailure::Cancelled,
        Some(ToolErrorKind::PermissionDenied) => ValidationCommandFailure::PolicyDenied,
        Some(ToolErrorKind::InvalidArguments) => ValidationCommandFailure::InvalidArguments,
        Some(ToolErrorKind::Backend) | None => ValidationCommandFailure::CommandFailed,
    })
}

fn validation_failure_from_error(error: &OrchestratorError) -> ValidationCommandFailure {
    if error.is_cancellation() {
        ValidationCommandFailure::Cancelled
    } else if matches!(error, OrchestratorError::Timeout(_)) {
        ValidationCommandFailure::Timeout
    } else if matches!(error, OrchestratorError::PolicyDenied(_)) {
        ValidationCommandFailure::PolicyDenied
    } else {
        ValidationCommandFailure::UnavailableEnvironment
    }
}

fn bounded_redacted_output(output: &str) -> (String, bool, bool) {
    let redacted = redact(output);
    let output_redacted = redacted != output;
    if redacted.len() <= VALIDATION_COMMAND_OUTPUT_MAX_BYTES {
        return (redacted, output_redacted, false);
    }
    let mut end = VALIDATION_COMMAND_OUTPUT_MAX_BYTES;
    while !redacted.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}… [truncated]", &redacted[..end]), output_redacted, true)
}

fn execution_blocker(
    entry: &ValidationPlanEntry,
    plan: &ValidationPlan,
    completed: &[ValidationResult],
) -> Option<String> {
    let mut prior = plan.entries.iter().zip(completed);
    for prerequisite in &entry.metadata.prerequisites {
        let Some((_, result)) =
            prior.clone().find(|(planned, _)| planned.metadata.command_id == *prerequisite)
        else {
            return Some(format!("missing prerequisite command: {prerequisite}"));
        };
        if result.status != ValidationOutcome::Passed {
            return Some(format!("prerequisite command did not pass: {prerequisite}"));
        }
    }
    if entry.escalation == ValidationEscalation::AfterFocusedPass
        && prior.any(|(planned, result)| {
            planned.metadata.scope == ValidationScope::Targeted
                && result.status != ValidationOutcome::Passed
        })
    {
        return Some("broader validation requires prior focused commands to pass".into());
    }
    None
}

fn record_validation_evidence(
    validation: &mut ValidationRecorder,
    entry: &ValidationPlanEntry,
    task_id: &TaskId,
    result: &ValidationResult,
) {
    validation.record_evidence(crate::final_response::ValidationRecord {
        evidence_id: format!("validation-{}-{task_id}", result.command_id),
        command: entry.command.clone(),
        tool: Some(entry.tool_name.clone()),
        outcome: result.status,
        exit_status: result.exit_status,
        elapsed_ms: Some(result.elapsed_ms),
        affected_tests: result.test_ids.clone(),
        diagnostics_delta: result.diagnostics_delta,
        output_truncated: result.output_truncated,
        skip_reason: (result.status == ValidationOutcome::Skipped)
            .then(|| result.output_summary.clone()),
        revision: None,
        selected: false,
        denied: result.failure == Some(ValidationCommandFailure::PolicyDenied),
        detail: Some(result.output_summary.clone()),
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
#[path = "validation_tests.rs"]
mod tests;
