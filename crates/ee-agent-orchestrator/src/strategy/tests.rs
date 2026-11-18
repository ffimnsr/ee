use std::sync::{Arc, Mutex};
use std::time::Duration;

use ee_agent_acp_server::server::OutboundEvent;
use ee_agent_acp_server::{ClientBridge, UpdateSink};
use ee_agent_protocol::{ContentBlock, SessionId, StopReason, TextContent};
use serde_json::json;
use tokio::sync::{mpsc, watch};

use super::*;
use crate::model::{ModelResponse, ModelRole};
use crate::policy::ToolPolicy;
use crate::reflection::ReflectionConfig;
use crate::test_support::{FakeModel, FakeTool};
use crate::tools::{ServerTool, SideEffectClass, ToolDefinition, ToolIntent, ToolResult};

// ── Selector tests ────────────────────────────────────────────────────

fn selector_context(prompt_text: &str) -> StrategyContext {
    StrategyContext {
        prompt_text: prompt_text.to_string(),
        has_code_changes: false,
        validation_tools_available: false,
        delegation_allowed: false,
        task_graph: TaskGraph::new(),
        tool_definitions: Vec::new(),
    }
}

fn decision_for(prompt_text: &str) -> StrategyDecision {
    StrategySelector.select(&selector_context(prompt_text))
}

#[test]
fn simple_answer_is_the_conservative_default() {
    let decision = decision_for("hello, what is the weather like?");
    assert_eq!(decision.strategy, TurnStrategy::SimpleAnswer);
    assert_eq!(decision.reason, StrategyReason::NoToolsRequested);
    assert_eq!(decision.reason.code(), "no-tools-requested");
    assert!(decision.required_capabilities.is_empty());
}

#[test]
fn web_research_guidance_requires_both_registered_web_tools() {
    let search = ToolDefinition::new("ee_web_search", "searches public web results");
    let fetch = ToolDefinition::new("ee_fetch_url", "fetches public remote text");
    assert!(!has_web_context_tools(std::slice::from_ref(&search)));
    assert!(!has_web_context_tools(std::slice::from_ref(&fetch)));
    assert!(has_web_context_tools(&[search, fetch]));
    assert!(WEB_RESEARCH_GUIDANCE.contains("never search merely because this turn began"));
    assert!(WEB_RESEARCH_GUIDANCE.contains("inspect local code before edits"));
    assert!(WEB_RESEARCH_GUIDANCE.contains("configured ee_web_search"));
    assert!(WEB_RESEARCH_GUIDANCE.contains("untrusted discovery evidence"));
    assert!(WEB_RESEARCH_GUIDANCE.contains("official or primary sources"));
}

#[test]
fn file_search_guidance_requires_a_registered_search_tool() {
    let regex = ToolDefinition::new("ee_search_text_regex", "regex search");
    let list = ToolDefinition::new("ee_list_directory", "lists a directory");
    assert!(has_file_search_tools(std::slice::from_ref(&regex)));
    assert!(has_file_search_tools(std::slice::from_ref(&ToolDefinition::new(
        "ee_search_text",
        "literal search"
    ))));
    assert!(!has_file_search_tools(std::slice::from_ref(&list)));
    assert!(SEARCH_GUIDANCE.contains("ee_search_text_regex"));
    assert!(SEARCH_GUIDANCE.contains("file_glob"));
    assert!(SEARCH_GUIDANCE.contains("directory traversal"));
}

#[test]
fn tool_loop_for_file_inspection() {
    let decision = decision_for("read the file /tmp/x and summarize it");
    assert_eq!(decision.strategy, TurnStrategy::ToolLoop);
    assert_eq!(decision.reason, StrategyReason::FileInspectionRequested);
    assert_eq!(decision.required_capabilities, vec!["fs:read"]);

    // Mentioning a registered tool also selects the tool loop.
    let mut ctx = selector_context("please use read_file on the config");
    ctx.tool_definitions = vec![ToolDefinition::new("read_file", "reads a file")];
    let decision = StrategySelector.select(&ctx);
    assert_eq!(decision.strategy, TurnStrategy::ToolLoop);
}

#[test]
fn capability_guidance_downgrades_unavailable_strategies_without_granting_policy() {
    let mut context = selector_context("implement login across multiple files");
    let guidance = capability_aware_guidance(&context, &PolicyEngine::default());
    assert_eq!(guidance.decision.strategy, TurnStrategy::SimpleAnswer);
    assert!(guidance.tool_names.is_empty());
    assert!(guidance.text.contains("advertised tools=[none]"));

    context.tool_definitions = vec![
        ToolDefinition::new("read_file", "reads").side_effect_class(SideEffectClass::Read),
        ToolDefinition::new("write_file", "writes").side_effect_class(SideEffectClass::Write),
    ];
    let guidance = capability_aware_guidance(&context, &PolicyEngine::default());
    assert_eq!(guidance.decision.strategy, TurnStrategy::ToolLoop);
    assert_eq!(guidance.available_capabilities, vec!["fs:read"]);
    assert_eq!(guidance.tool_names, vec!["read_file"]);

    let policy = PolicyEngine::new(ToolPolicy { allow_write: true, ..ToolPolicy::default() });
    let guidance = capability_aware_guidance(&context, &policy);
    assert_eq!(guidance.decision.strategy, TurnStrategy::PlanThenExecute);
    assert_eq!(guidance.available_capabilities, vec!["fs:read", "fs:write"]);
}

#[test]
fn plan_then_execute_for_multi_file_implementation() {
    let decision = decision_for("implement login across multiple files");
    assert_eq!(decision.strategy, TurnStrategy::PlanThenExecute);
    assert_eq!(decision.reason, StrategyReason::MultiFileImplementation);
    assert_eq!(decision.required_capabilities, vec!["fs:read", "fs:write"]);
}

#[test]
fn research_then_edit_for_unknown_codebase_change() {
    let decision = decision_for("investigate why the build fails and fix it");
    assert_eq!(decision.strategy, TurnStrategy::ResearchThenEdit);
    assert_eq!(decision.reason, StrategyReason::UnknownCodebaseChange);
}

#[test]
fn validate_then_review_requires_changes_and_validation_tools() {
    let mut ctx = selector_context("run the checks");
    ctx.has_code_changes = true;
    ctx.validation_tools_available = true;
    let decision = StrategySelector.select(&ctx);
    assert_eq!(decision.strategy, TurnStrategy::ValidateThenReview);
    assert_eq!(decision.reason, StrategyReason::ChangesWithValidation);

    // Without a validation tool the same prompt falls through.
    ctx.validation_tools_available = false;
    let decision = StrategySelector.select(&ctx);
    assert_ne!(decision.strategy, TurnStrategy::ValidateThenReview);
}

#[test]
fn parallel_delegation_requires_independent_children_and_policy() {
    let mut ctx = selector_context("hello");
    let mut tasks = TaskGraph::new();
    let root = tasks.create_root("plan", "plan");
    tasks.create_child(&root.id, "a", "scope a").expect("child");
    tasks.create_child(&root.id, "b", "scope b").expect("child");
    ctx.task_graph = tasks;
    ctx.delegation_allowed = true;
    let decision = StrategySelector.select(&ctx);
    assert_eq!(decision.strategy, TurnStrategy::ParallelDelegation);
    assert_eq!(decision.reason, StrategyReason::ParallelIndependentWork);
    assert_eq!(decision.required_capabilities, vec!["subagent:spawn"]);

    // Denied delegation falls through to the conservative default.
    ctx.delegation_allowed = false;
    let decision = StrategySelector.select(&ctx);
    assert_eq!(decision.strategy, TurnStrategy::SimpleAnswer);
}

#[test]
fn parallel_delegation_rejects_dependent_children() {
    let mut tasks = TaskGraph::new();
    let root = tasks.create_root("plan", "plan");
    let child = tasks.create_child(&root.id, "a", "scope a").expect("child");
    tasks.create_child(&root.id, "b", "scope b").expect("child");
    tasks.add_dependency(&child.id, &TaskId::new("task-3")).expect("depends on sibling");
    let ctx = StrategyContext {
        prompt_text: "hello".into(),
        has_code_changes: false,
        validation_tools_available: false,
        delegation_allowed: true,
        task_graph: tasks,
        tool_definitions: Vec::new(),
    };
    let decision = StrategySelector.select(&ctx);
    assert_ne!(decision.strategy, TurnStrategy::ParallelDelegation);
    assert!(!has_independent_children(&ctx.task_graph));
}

#[test]
fn strategy_types_serialize_deterministically() {
    for strategy in [
        TurnStrategy::SimpleAnswer,
        TurnStrategy::ToolLoop,
        TurnStrategy::PlanThenExecute,
        TurnStrategy::ResearchThenEdit,
        TurnStrategy::ValidateThenReview,
        TurnStrategy::ParallelDelegation,
    ] {
        let json = serde_json::to_string(&strategy).expect("serializes");
        let restored: TurnStrategy = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, strategy);
    }
    let decision =
        StrategyDecision::new(TurnStrategy::ToolLoop, StrategyReason::FileInspectionRequested);
    let json = serde_json::to_string(&decision).expect("serializes");
    let restored: StrategyDecision = serde_json::from_str(&json).expect("parses");
    assert_eq!(restored, decision);
}

#[test]
fn strategy_selection_emits_deterministic_event() {
    let events = EventRecorder::new();
    let decision = decision_for("hello");
    events.record(OrchestratorEvent::StrategySelected {
        strategy: decision.strategy,
        reason: decision.reason,
    });
    assert_eq!(
        events.events(),
        vec![OrchestratorEvent::StrategySelected {
            strategy: TurnStrategy::SimpleAnswer,
            reason: StrategyReason::NoToolsRequested,
        }]
    );
}

// ── Executor tests ────────────────────────────────────────────────────

type ExecutorStores =
    (StrategyExecutor, Arc<Mutex<ToolRegistry>>, Arc<Mutex<BudgetTracker>>, Arc<Mutex<TaskGraph>>);

fn plumbing() -> (UpdateSink, ClientBridge, mpsc::UnboundedReceiver<OutboundEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (
        UpdateSink::new_for_test(SessionId::new("s-1"), tx.clone()),
        ClientBridge::new_for_test(Duration::from_secs(5), tx),
        rx,
    )
}

fn task_fixture() -> TaskNode {
    TaskNode::new(TaskId::new("task-1"), "hello world", "hello world")
}

fn prompt(text: &str) -> PromptContext {
    PromptContext::new(SessionId::new("s-1"), vec![ContentBlock::Text(TextContent::new(text))])
}

fn executor_with(
    config: OrchestratorConfig,
    model: Arc<dyn ModelAdapter>,
    tool: Option<Arc<dyn ServerTool>>,
    policy: PolicyEngine,
    events: EventRecorder,
) -> ExecutorStores {
    let tools = Arc::new(Mutex::new(ToolRegistry::new()));
    if let Some(tool) = tool {
        tools.lock().expect("tool registry poisoned").register(tool).expect("registers tool");
    }
    let budget = Arc::new(Mutex::new(BudgetTracker::new(&config)));
    let tasks = Arc::new(Mutex::new(TaskGraph::new()));
    let executor = StrategyExecutor::new(
        config.clone(),
        model,
        tools.clone(),
        budget.clone(),
        policy,
        tasks.clone(),
        events,
    );
    (executor, tools, budget, tasks)
}

fn run(
    config: OrchestratorConfig,
    model: Arc<FakeModel>,
    tool: Option<Arc<FakeTool>>,
    policy: PolicyEngine,
    strategy: TurnStrategy,
    cancellation: bool,
    graph: TaskGraph,
) -> (
    Result<PromptResult, OrchestratorError>,
    EventRecorder,
    ValidationRecorder,
    Vec<ToolExecutionLogEntry>,
    Arc<FakeModel>,
) {
    let events = EventRecorder::new();
    let (executor, _, _, _) = executor_with(
        config,
        model.clone(),
        tool.map(|t| t as Arc<dyn ServerTool>),
        policy,
        events.clone(),
    );
    let (sink, client, _rx) = plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(cancellation);
    let log: Arc<Mutex<Vec<ToolExecutionLogEntry>>> = Arc::new(Mutex::new(Vec::new()));
    let run = StrategyRun {
        task: task_fixture(),
        task_graph: graph,
        sink,
        client,
        cancel: cancel_rx,
        execution_log: log.clone(),
    };
    let mut validation = ValidationRecorder::new();
    // A dedicated current-thread runtime per call so the executor's tokio
    // timeouts run without sharing the test runtime.
    let executed = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("test runtime")
        .block_on(executor.execute(
            strategy,
            prompt("hello world"),
            None,
            None,
            run,
            &mut validation,
        ));
    let result = executed.map(|(prompt_result, _reflection)| prompt_result);
    let recorded_log = log.lock().expect("log poisoned").clone();
    (result, events, validation, recorded_log, model)
}

fn read_tool() -> Arc<FakeTool> {
    Arc::new(FakeTool::new(
        ToolDefinition::new("read_file", "reads a file").side_effect_class(SideEffectClass::Read),
        ToolResult::success("file contents"),
    ))
}

fn validation_tool(success: bool) -> Arc<FakeTool> {
    Arc::new(FakeTool::new(
        ToolDefinition::new("run_validation", "runs validation")
            .side_effect_class(SideEffectClass::Execute),
        if success {
            ToolResult::success("all checks green")
        } else {
            ToolResult::failure(crate::tools::ToolErrorKind::Backend, "checks failed")
        },
    ))
}

fn delegating_policy() -> PolicyEngine {
    PolicyEngine::new(ToolPolicy { allow_delegate: true, ..ToolPolicy::default() })
}

fn permissive_policy() -> PolicyEngine {
    PolicyEngine::new(ToolPolicy {
        allow_read: true,
        allow_write: true,
        allow_execute: true,
        ..ToolPolicy::default()
    })
}

#[test]
fn simple_answer_runs_one_call_without_tools() {
    let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("hi").completed()]));
    let (result, events, _, log, model) = run(
        OrchestratorConfig::default(),
        model,
        None,
        PolicyEngine::default(),
        TurnStrategy::SimpleAnswer,
        false,
        TaskGraph::new(),
    );
    let result = result.expect("simple answer succeeds");
    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(model.call_count(), 1);
    assert!(log.is_empty());
    assert!(
        events
            .events()
            .iter()
            .any(|e| matches!(e, OrchestratorEvent::ModelResponded { iteration: 1 }))
    );
}

#[test]
fn simple_answer_rejects_tool_intents() {
    let model = Arc::new(FakeModel::new(vec![
        ModelResponse::new()
            .tool_intents(vec![ToolIntent::new("tc-1", "read_file", json!({ "path": "/tmp/x" }))])
            .completed(),
    ]));
    let (result, _, _, log, _) = run(
        OrchestratorConfig::default(),
        model,
        Some(read_tool()),
        PolicyEngine::default(),
        TurnStrategy::SimpleAnswer,
        false,
        TaskGraph::new(),
    );
    assert!(
        matches!(result, Err(OrchestratorError::ModelFailure(ref reason)) if reason.contains("SimpleAnswer")),
        "{result:?}"
    );
    assert!(log.is_empty(), "no tool may execute under SimpleAnswer");
}

#[test]
fn tool_loop_executes_tools_and_continues() {
    let model = Arc::new(FakeModel::new(vec![
        ModelResponse::new().tool_intents(vec![ToolIntent::new(
            "tc-1",
            "read_file",
            json!({ "path": "/tmp/x" }),
        )]),
        ModelResponse::new().text("read it").completed(),
    ]));
    let (result, events, _, log, _) = run(
        OrchestratorConfig::default(),
        model,
        Some(read_tool()),
        PolicyEngine::default(),
        TurnStrategy::ToolLoop,
        false,
        TaskGraph::new(),
    );
    assert_eq!(result.expect("tool loop succeeds").stop_reason, StopReason::EndTurn);
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].tool_name, "read_file");
    assert!(log[0].success);
    assert_eq!(log[0].side_effect_class, Some(SideEffectClass::Read));
    assert!(events.events().iter().any(|e| matches!(e, OrchestratorEvent::ToolFinished { tool_name, .. } if tool_name == "read_file")));
}

#[test]
fn research_then_edit_runs_reads_before_writes() {
    let model = Arc::new(FakeModel::new(vec![
        ModelResponse::new().tool_intents(vec![
            ToolIntent::new("tc-1", "write_file", json!({ "path": "/tmp/x", "content": "y" })),
            ToolIntent::new("tc-2", "read_file", json!({ "path": "/tmp/x" })),
        ]),
        ModelResponse::new().text("done").completed(),
    ]));
    let (result, events, _, _, _) = run(
        OrchestratorConfig::default(),
        model,
        Some(read_tool()),
        permissive_policy(),
        TurnStrategy::ResearchThenEdit,
        false,
        TaskGraph::new(),
    );
    assert_eq!(result.expect("research then edit succeeds").stop_reason, StopReason::EndTurn);
    let recorded = events.events();
    let started: Vec<&str> = recorded
        .iter()
        .filter_map(|e| match e {
            OrchestratorEvent::ToolStarted { tool_name, .. } => Some(tool_name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(started, vec!["read_file", "write_file"], "reads must run before writes");
}

#[test]
fn validate_then_review_records_validation_after_edits() {
    let model = Arc::new(FakeModel::new(vec![
        ModelResponse::new().text("edits done").completed(),
        ModelResponse::new().tool_intents(vec![ToolIntent::new(
            "tc-1",
            "run_validation",
            json!({}),
        )]),
    ]));
    let (result, events, validation, _, _) = run(
        OrchestratorConfig::default(),
        model,
        Some(validation_tool(true)),
        permissive_policy(),
        TurnStrategy::ValidateThenReview,
        false,
        TaskGraph::new(),
    );
    assert_eq!(result.expect("review turn succeeds").stop_reason, StopReason::EndTurn);
    assert!(validation.has_passed(), "validation outcome must be recorded");
    assert_eq!(validation.passed_commands(), vec!["run_validation"]);
    let events = events.events();
    let stopped = events
        .iter()
        .position(|e| matches!(e, OrchestratorEvent::TurnStopped { .. }))
        .expect("main loop stops");
    assert!(
        events[stopped..].iter().any(|e| matches!(e, OrchestratorEvent::ToolStarted { tool_name, .. } if tool_name == "run_validation")),
        "validation must run after the edit loop stops"
    );
}

#[test]
fn validate_then_review_records_failed_validation_but_completes() {
    let model = Arc::new(FakeModel::new(vec![
        ModelResponse::new().text("edits done").completed(),
        ModelResponse::new().tool_intents(vec![ToolIntent::new(
            "tc-1",
            "run_validation",
            json!({}),
        )]),
    ]));
    let (result, _, validation, _, _) = run(
        OrchestratorConfig::default(),
        model,
        Some(validation_tool(false)),
        permissive_policy(),
        TurnStrategy::ValidateThenReview,
        false,
        TaskGraph::new(),
    );
    assert_eq!(result.expect("review turn succeeds").stop_reason, StopReason::EndTurn);
    assert!(!validation.has_passed());
    assert!(validation.has_failed());
    assert_eq!(validation.failed_commands(), vec!["run_validation"]);
}

#[test]
fn validate_then_review_rejects_non_validation_tools() {
    let model = Arc::new(FakeModel::new(vec![
        ModelResponse::new().text("edits done").completed(),
        ModelResponse::new().tool_intents(vec![ToolIntent::new(
            "tc-1",
            "read_file",
            json!({ "path": "/tmp/x" }),
        )]),
    ]));
    let (result, _, validation, _, _) = run(
        OrchestratorConfig::default(),
        model,
        Some(read_tool()),
        PolicyEngine::default(),
        TurnStrategy::ValidateThenReview,
        false,
        TaskGraph::new(),
    );
    assert!(
        matches!(result, Err(OrchestratorError::ModelFailure(ref reason)) if reason.contains("validation tools")),
        "{result:?}"
    );
    assert!(!validation.has_passed());
}

#[test]
fn parallel_delegation_requires_independent_children() {
    let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("done").completed()]));
    let (result, _, _, _, _) = run(
        OrchestratorConfig::default(),
        model.clone(),
        None,
        delegating_policy(),
        TurnStrategy::ParallelDelegation,
        false,
        TaskGraph::new(),
    );
    assert!(matches!(result, Err(OrchestratorError::InvalidState(_))), "{result:?}");

    let mut graph = TaskGraph::new();
    let root = graph.create_root("plan", "plan");
    graph.create_child(&root.id, "a", "scope a").expect("child");
    graph.create_child(&root.id, "b", "scope b").expect("child");
    let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("done").completed()]));
    let (result, _, _, _, _) = run(
        OrchestratorConfig::default(),
        model,
        None,
        delegating_policy(),
        TurnStrategy::ParallelDelegation,
        false,
        graph,
    );
    assert_eq!(result.expect("parallel delegation runs").stop_reason, StopReason::EndTurn);
}

#[test]
fn parallel_delegation_denied_without_policy() {
    let mut graph = TaskGraph::new();
    let root = graph.create_root("plan", "plan");
    graph.create_child(&root.id, "a", "scope a").expect("child");
    graph.create_child(&root.id, "b", "scope b").expect("child");
    let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("done").completed()]));
    let (result, _, _, _, _) = run(
        OrchestratorConfig::default(),
        model,
        None,
        PolicyEngine::default(),
        TurnStrategy::ParallelDelegation,
        false,
        graph,
    );
    assert!(matches!(result, Err(OrchestratorError::PolicyDenied(_))), "{result:?}");
}

#[test]
fn every_wrapper_respects_cancellation() {
    let mut parallel_graph = TaskGraph::new();
    let root = parallel_graph.create_root("plan", "plan");
    parallel_graph.create_child(&root.id, "a", "scope a").expect("child");
    parallel_graph.create_child(&root.id, "b", "scope b").expect("child");
    for (strategy, graph, policy) in [
        (TurnStrategy::SimpleAnswer, TaskGraph::new(), PolicyEngine::default()),
        (TurnStrategy::ToolLoop, TaskGraph::new(), PolicyEngine::default()),
        (TurnStrategy::PlanThenExecute, TaskGraph::new(), PolicyEngine::default()),
        (TurnStrategy::ResearchThenEdit, TaskGraph::new(), PolicyEngine::default()),
        (TurnStrategy::ValidateThenReview, TaskGraph::new(), PolicyEngine::default()),
        (TurnStrategy::ParallelDelegation, parallel_graph.clone(), delegating_policy()),
    ] {
        let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("x").completed()]));
        let (result, _, _, _, model) =
            run(OrchestratorConfig::default(), model, None, policy, strategy, true, graph);
        assert_eq!(result, Err(OrchestratorError::Cancellation), "{strategy:?}");
        assert_eq!(
            model.call_count(),
            0,
            "{strategy:?} must not call the model after cancellation"
        );
    }
}

#[test]
fn every_wrapper_respects_model_call_budget() {
    for strategy in
        [TurnStrategy::ToolLoop, TurnStrategy::PlanThenExecute, TurnStrategy::ResearchThenEdit]
    {
        let config =
            OrchestratorConfig { max_model_calls: Some(1), ..OrchestratorConfig::default() };
        let model = Arc::new(FakeModel::new(vec![
            ModelResponse::new().text("first"),
            ModelResponse::new().text("second").completed(),
        ]));
        let (result, _, _, _, model) =
            run(config, model, None, PolicyEngine::default(), strategy, false, TaskGraph::new());
        assert!(
            matches!(result, Err(OrchestratorError::BudgetExceeded(_))),
            "{strategy:?}: {result:?}"
        );
        assert!(model.call_count() <= 1, "{strategy:?} exceeded its model-call budget");
    }

    // SimpleAnswer fits its single call inside the same budget cap.
    let config = OrchestratorConfig { max_model_calls: Some(1), ..OrchestratorConfig::default() };
    let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("first")]));
    let (result, _, _, _, model) = run(
        config,
        model,
        None,
        PolicyEngine::default(),
        TurnStrategy::SimpleAnswer,
        false,
        TaskGraph::new(),
    );
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(model.call_count(), 1);
}

// ── Reflection pass tests ─────────────────────────────────────────────

fn write_tool() -> Arc<FakeTool> {
    Arc::new(FakeTool::new(
        ToolDefinition::new("write_file", "writes a file")
            .side_effect_class(SideEffectClass::Write),
        ToolResult::success("file written"),
    ))
}

fn write_intent() -> ToolIntent {
    ToolIntent::new("tc-1", "write_file", json!({ "path": "/tmp/out.rs" }))
}

/// Runs a ToolLoop strategy with reflection enabled through the executor
/// and returns the executed result, the executor's task store, and the
/// fake model for request inspection.
type ReflectionRunOutcome = (
    Result<(PromptResult, crate::reflection::ReflectionOutcome), OrchestratorError>,
    Arc<Mutex<TaskGraph>>,
    Arc<FakeModel>,
);

fn reflection_run(
    config: OrchestratorConfig,
    model: Arc<FakeModel>,
    tool: Arc<FakeTool>,
) -> ReflectionRunOutcome {
    let events = EventRecorder::new();
    let (executor, _, _, tasks) = executor_with(
        config,
        model.clone(),
        Some(tool as Arc<dyn ServerTool>),
        permissive_policy(),
        events,
    );
    // Mirror the runtime: the root task exists in the shared store before
    // the turn runs, so finding tasks can attach to it.
    tasks.lock().expect("task graph poisoned").create_root("implement", "implement a fix");
    let (sink, client, _rx) = plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let log: Arc<Mutex<Vec<ToolExecutionLogEntry>>> = Arc::new(Mutex::new(Vec::new()));
    let run = StrategyRun {
        task: task_fixture(),
        task_graph: TaskGraph::new(),
        sink,
        client,
        cancel: cancel_rx,
        execution_log: log.clone(),
    };
    let mut validation = ValidationRecorder::new();
    let executed = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("test runtime")
        .block_on(executor.execute(
            TurnStrategy::ToolLoop,
            prompt("implement a fix"),
            None,
            None,
            run,
            &mut validation,
        ));
    (executed, tasks, model)
}

#[test]
fn reflection_one_review_pass_finds_issue_and_fixes() {
    let model = Arc::new(FakeModel::new(vec![
        ModelResponse::new().tool_intents(vec![write_intent()]),
        ModelResponse::new().text("done").completed(),
        ModelResponse::new().text("- missing error handling"),
        ModelResponse::new().text("fixed").completed(),
    ]));
    let config = OrchestratorConfig {
        reflection: ReflectionConfig {
            enabled: true,
            max_review_iterations: 1,
            max_fix_iterations: 1,
        },
        ..OrchestratorConfig::default()
    };
    let (executed, tasks, model) = reflection_run(config, model, write_tool());
    let (result, outcome) = executed.expect("turn succeeds");
    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(model.call_count(), 4);
    assert_eq!(outcome.review_calls, 1);
    assert_eq!(outcome.fix_loops, 1);
    assert_eq!(outcome.findings.len(), 1);
    assert_eq!(outcome.findings[0].detail, "missing error handling");
    assert_eq!(outcome.findings[0].task_id, Some(TaskId::new("task-2")));

    // Trusted review instructions stay separate from bounded untrusted
    // evidence; injection preparation wraps only evidence.
    let review_request = &model.requests()[2];
    let user_messages: Vec<_> = review_request
        .transcript
        .iter()
        .filter(|message| message.role == ModelRole::User)
        .collect();
    assert!(
        user_messages
            .iter()
            .any(|message| message.text_content().contains("Review completed work"))
    );
    let evidence = user_messages
        .iter()
        .find(|message| message.trust == crate::trust::TrustLevel::ToolOutputUntrusted)
        .expect("untrusted review evidence");
    assert!(evidence.text_content().contains("- /tmp/out.rs"));

    // The finding became a completed task item after the fix loop.
    let graph = tasks.lock().expect("task graph poisoned");
    let finding = graph.get(&TaskId::new("task-2")).expect("finding task");
    assert_eq!(finding.status, TaskStatus::Completed);
    assert_eq!(finding.result_summary.as_deref(), Some("addressed in fix loop"));
}

#[test]
fn reflection_disabled_skips_review_call() {
    let model = Arc::new(FakeModel::new(vec![
        ModelResponse::new().tool_intents(vec![write_intent()]),
        ModelResponse::new().text("done").completed(),
        ModelResponse::new().text("extra").completed(),
    ]));
    let (executed, tasks, model) =
        reflection_run(OrchestratorConfig::default(), model, write_tool());
    let (result, outcome) = executed.expect("turn succeeds");
    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(model.call_count(), 2, "no review call with reflection disabled");
    assert_eq!(outcome.review_calls, 0);
    assert_eq!(outcome.fix_loops, 0);
    assert!(outcome.findings.is_empty());
    let graph = tasks.lock().expect("task graph poisoned");
    assert_eq!(graph.len(), 1, "no finding tasks were created");
}

#[test]
fn reflection_cannot_exceed_configured_iterations() {
    let model = Arc::new(FakeModel::new(vec![
        ModelResponse::new().tool_intents(vec![write_intent()]),
        ModelResponse::new().text("done").completed(),
        ModelResponse::new().text("- issue one"),
        ModelResponse::new().text("fixed").completed(),
        ModelResponse::new().text("- issue two"),
    ]));
    let config = OrchestratorConfig {
        reflection: ReflectionConfig {
            enabled: true,
            max_review_iterations: 2,
            max_fix_iterations: 1,
        },
        ..OrchestratorConfig::default()
    };
    let (executed, tasks, model) = reflection_run(config, model, write_tool());
    let (result, outcome) = executed.expect("turn succeeds");
    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(model.call_count(), 5);
    assert_eq!(outcome.review_calls, 2, "review calls capped at config");
    assert_eq!(outcome.fix_loops, 1, "fix loops capped at config");
    assert_eq!(outcome.findings.len(), 2);
    assert_eq!(outcome.findings[1].task_id, Some(TaskId::new("task-3")));

    let graph = tasks.lock().expect("task graph poisoned");
    // First finding was fixed; the second arrived after the fix budget
    // was spent and stays pending (never blindly retried).
    assert_eq!(
        graph.get(&TaskId::new("task-2")).expect("first finding").status,
        TaskStatus::Completed
    );
    assert_eq!(
        graph.get(&TaskId::new("task-3")).expect("second finding").status,
        TaskStatus::Pending
    );
}

#[test]
fn reflection_without_edits_stays_silent() {
    // A turn with no changed files and no validation records gets no
    // review call even when reflection is enabled.
    let model = Arc::new(FakeModel::new(vec![
        ModelResponse::new().text("answer").completed(),
        ModelResponse::new().text("- fabricated finding"),
    ]));
    let config = OrchestratorConfig {
        reflection: ReflectionConfig { enabled: true, ..ReflectionConfig::default() },
        ..OrchestratorConfig::default()
    };
    let (executed, tasks, model) = reflection_run(config, model, write_tool());
    let (result, outcome) = executed.expect("turn succeeds");
    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(model.call_count(), 1, "no evidence means no review call");
    assert_eq!(outcome.review_calls, 0);
    let graph = tasks.lock().expect("task graph poisoned");
    assert_eq!(graph.len(), 1);
}
