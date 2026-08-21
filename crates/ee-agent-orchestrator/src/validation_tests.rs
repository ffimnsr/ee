use std::collections::VecDeque;
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
use crate::retries::{BackoffStrategy, RetryPolicy};
use crate::test_support::FakeTool;
use crate::tools::{
    ServerTool, SideEffectClass, ToolCallContext, ToolErrorKind, ToolFuture, ToolRegistry,
    ToolResult,
};

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
        declared_commands: Vec::new(),
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
    assert_eq!(evidence.evidence_id, "validation-cargo_check-task-2");
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
    let mut runner =
        runner_with_policy(tool.clone(), OrchestratorConfig::default(), PolicyEngine::default());
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
    assert_eq!(result.failure, Some(ValidationCommandFailure::PolicyDenied));
    assert_eq!(result.attempts, 1, "policy denial is never retried");
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
    assert_eq!(graph.get(&ids[0]).expect("stored").result_summary.as_deref(), Some("checks green"));
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
    runner.store.record(
        "cargo check",
        ValidationOutcome::Passed,
        "green",
        Some(TaskId::new("task-2")),
    );
    assert_eq!(runner.store().results().len(), 1);
    assert_eq!(runner.store().passed().len(), 1);
    assert!(runner.store().failed().is_empty());
}

fn workspace_command(
    tool_name: &str,
    command: &str,
    command_id: &str,
    scope: ValidationScope,
) -> DeclaredValidationCommand {
    DeclaredValidationCommand::new(
        DeclaredValidationTask::new(tool_name, command, json!({})),
        command_id,
    )
    .with_metadata(
        ValidationCommandMetadata::targeted(command_id)
            .with_scope(scope)
            .with_test_ids([format!("{command_id}::test")]),
    )
}

fn runner_with_tools(tools_to_register: Vec<Arc<dyn ServerTool>>) -> ValidationRunner {
    let config = OrchestratorConfig::default();
    let tools = Arc::new(Mutex::new(ToolRegistry::new()));
    for tool in tools_to_register {
        tools.lock().expect("registry poisoned").register(tool).expect("registers");
    }
    let events = EventRecorder::new();
    ValidationRunner::new(
        ToolExecutor::new(
            config.clone(),
            tools,
            Arc::new(Mutex::new(BudgetTracker::new(&config))),
            PolicyEngine::new(ToolPolicy { allow_execute: true, ..ToolPolicy::default() }),
            0,
            events.clone(),
        ),
        events,
    )
}

#[derive(Clone)]
struct ScriptedTool {
    definition: ToolDefinition,
    results: Arc<Mutex<VecDeque<ToolResult>>>,
    calls: Arc<Mutex<usize>>,
}

impl ScriptedTool {
    fn new(name: &str, results: impl IntoIterator<Item = ToolResult>) -> Self {
        Self {
            definition: ToolDefinition::new(name, "scripted validation")
                .side_effect_class(SideEffectClass::Execute),
            results: Arc::new(Mutex::new(results.into_iter().collect())),
            calls: Arc::new(Mutex::new(0)),
        }
    }

    fn call_count(&self) -> usize {
        *self.calls.lock().expect("calls poisoned")
    }
}

impl ServerTool for ScriptedTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn execute(
        &self,
        _arguments: serde_json::Value,
        _client: ClientBridge,
        _cancel: watch::Receiver<bool>,
        _context: ToolCallContext,
    ) -> ToolFuture<ToolResult> {
        let results = self.results.clone();
        let calls = self.calls.clone();
        Box::pin(async move {
            *calls.lock().expect("calls poisoned") += 1;
            results
                .lock()
                .expect("results poisoned")
                .pop_front()
                .unwrap_or_else(|| ToolResult::failure(ToolErrorKind::Backend, "unexpected call"))
        })
    }
}

#[test]
fn validation_planner_selects_targeted_before_workspace_escalation() {
    let planner = ValidationPlanner::new();
    let changed_files = vec![changed_file("docs/guide.txt")];
    let changed_symbols = vec!["parse_guide".into()];
    let focused = workspace_command(
        "focused_test",
        "cargo test parse_guide",
        "focused",
        ValidationScope::Targeted,
    );
    let broad = workspace_command(
        "workspace_test",
        "cargo test --quiet",
        "workspace",
        ValidationScope::Workspace,
    )
    .with_metadata(
        ValidationCommandMetadata::targeted("workspace")
            .with_scope(ValidationScope::Workspace)
            .with_prerequisites(["focused"])
            .with_test_ids(["workspace::all"]),
    );
    let workspace = WorkspaceValidationConfig {
        declared_tasks: Vec::new(),
        declared_commands: vec![focused, broad],
    };
    let tools = vec![
        ToolDefinition::new("focused_test", "focused").side_effect_class(SideEffectClass::Execute),
        ToolDefinition::new("workspace_test", "workspace")
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
    assert_eq!(plan.entries[0].metadata.command_id, "focused");
    assert_eq!(plan.entries[0].escalation, ValidationEscalation::Direct);
    assert_eq!(plan.entries[1].metadata.command_id, "workspace");
    assert_eq!(plan.entries[1].escalation, ValidationEscalation::AfterFocusedPass);
    assert_eq!(plan.entries[1].metadata.prerequisites, vec!["focused"]);
    assert_eq!(plan.entries[1].metadata.test_ids, vec!["workspace::all"]);
}

#[test]
fn validation_runner_blocks_broader_escalation_after_target_failure() {
    let planner = ValidationPlanner::new();
    let focused_tool = Arc::new(ScriptedTool::new(
        "focused_test",
        [ToolResult::failure(ToolErrorKind::Backend, "target failure")],
    ));
    let broad_tool =
        Arc::new(ScriptedTool::new("workspace_test", [ToolResult::success("must not run")]));
    let workspace = WorkspaceValidationConfig {
        declared_tasks: Vec::new(),
        declared_commands: vec![
            workspace_command("focused_test", "focused", "focused", ValidationScope::Targeted),
            workspace_command(
                "workspace_test",
                "workspace",
                "workspace",
                ValidationScope::Workspace,
            ),
        ],
    };
    let changed_files = vec![changed_file("docs/guide.txt")];
    let plan = planner.plan_with_context(
        ValidationPlanningContext {
            changed_files: &changed_files,
            changed_symbols: &[],
            workspace: &workspace,
        },
        &[focused_tool.definition(), broad_tool.definition()],
    );
    let mut graph = TaskGraph::new();
    let root = graph.create_root("implement", "implement");
    let ids = planner.create_tasks(&mut graph, &plan, &root.id).expect("creates");
    let mut runner = runner_with_tools(vec![focused_tool.clone(), broad_tool.clone()]);
    let (sink, client, _rx) = plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let mut validation = ValidationRecorder::new();

    let results = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime")
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
        .expect("target failure records without aborting plan");

    assert_eq!(results[0].failure, Some(ValidationCommandFailure::CommandFailed));
    assert_eq!(results[1].status, ValidationOutcome::Skipped);
    assert_eq!(results[1].failure, Some(ValidationCommandFailure::MissingDependency));
    assert_eq!(broad_tool.call_count(), 0, "broader command must not execute");
}

#[test]
fn validation_runner_retries_only_bounded_transient_failures_and_records_reason() {
    let planner = ValidationPlanner::new();
    let tool = Arc::new(ScriptedTool::new(
        "cargo_check",
        [
            ToolResult::failure(ToolErrorKind::Backend, "temporary failure"),
            ToolResult::failure(ToolErrorKind::Backend, "still failing"),
            ToolResult::success("must not run"),
        ],
    ));
    let plan = planner.plan(&[changed_file("src/lib.rs")], &[tool.definition()]);
    let mut graph = TaskGraph::new();
    let root = graph.create_root("implement", "implement");
    let ids = planner.create_tasks(&mut graph, &plan, &root.id).expect("creates");
    let mut runner = runner_with_tools(vec![tool.clone()]).with_retry_policy(RetryPolicy::new(
        1,
        BackoffStrategy::new(Duration::ZERO, Duration::ZERO, 1),
    ));
    let (sink, client, _rx) = plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let mut validation = ValidationRecorder::new();

    let result = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime")
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
        .expect("failure records");

    assert_eq!(tool.call_count(), 2, "max_retries=1 means two total attempts");
    assert_eq!(result.attempts, 2);
    assert_eq!(result.retry_reasons, vec!["backend"]);
    assert_eq!(result.failure, Some(ValidationCommandFailure::CommandFailed));
}

#[test]
fn validation_runner_redacts_and_caps_command_output() {
    let output = format!("API_KEY=sk-{}\n{}", "a".repeat(40), "x".repeat(9_000));
    let tool = Arc::new(FakeTool::new(
        ToolDefinition::new("cargo_check", "compiles").side_effect_class(SideEffectClass::Execute),
        ToolResult::success(output),
    ));
    let planner = ValidationPlanner::new();
    let plan = planner.plan(&[changed_file("src/lib.rs")], &[tool.definition()]);
    let mut graph = TaskGraph::new();
    let root = graph.create_root("implement", "implement");
    let ids = planner.create_tasks(&mut graph, &plan, &root.id).expect("creates");
    let mut runner = runner_with(tool, OrchestratorConfig::default());
    let (sink, client, _rx) = plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let mut validation = ValidationRecorder::new();

    let result = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime")
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

    assert!(result.output_redacted);
    assert!(result.output_truncated);
    assert!(result.output_summary.contains("[redacted]"));
    assert!(!result.output_summary.contains("sk-"));
    assert!(result.output_summary.len() <= VALIDATION_COMMAND_OUTPUT_MAX_BYTES + 32);
}
