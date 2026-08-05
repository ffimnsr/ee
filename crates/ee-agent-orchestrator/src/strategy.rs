//! Deterministic turn strategies and their execution wrappers.
//!
//! [`StrategySelector`] picks a [`TurnStrategy`] from observed context
//! (prompt text, task graph shape, tool set, delegation policy, recorded
//! changes) with a deterministic [`StrategyReason`] code, and the runtime
//! records the decision as an [`OrchestratorEvent::StrategySelected`].  The
//! default is conservative: a prompt that needs no workspace context runs
//! [`TurnStrategy::SimpleAnswer`] (one model call, no tools).  Every strategy
//! wrapper runs through the same budget, policy, and cancellation gates as
//! the standard loop — strategies never bypass them.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use ee_acp_agent_server::{ClientBridge, PromptContext, PromptResult, UpdateSink};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::budget::BudgetTracker;
use crate::config::OrchestratorConfig;
use crate::error::OrchestratorError;
use crate::events::{EventRecorder, OrchestratorEvent};
use crate::final_response::{
    FinalResponse, ValidationOutcome, ValidationRecorder, changed_files_from_log,
};
use crate::loop_engine::{LoopEngine, LoopMode, LoopOptions};
use crate::model::{ModelAdapter, ModelRequest, Transcript};
use crate::policy::PolicyEngine;
use crate::reflection::{
    ReflectionOutcome, ReviewFinding, build_review_context, build_review_request,
    create_finding_tasks, findings_from_response, mark_finding_tasks,
};
use crate::tasks::{TaskGraph, TaskId, TaskNode, TaskStatus};
use crate::tools::{ToolDefinition, ToolExecutionLogEntry, ToolExecutor, ToolIntent, ToolRegistry};

/// Which deterministic turn strategy to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TurnStrategy {
    /// One model call, no tool execution.
    SimpleAnswer,
    /// Standard bounded model → tool loop.
    ToolLoop,
    /// Task graph plan emitted before any tool runs.
    PlanThenExecute,
    /// Read-class tools execute before write-class tools.
    ResearchThenEdit,
    /// Validation and review run after the edit loop.
    ValidateThenReview,
    /// Independent child tasks run as parallel delegations.
    ParallelDelegation,
}

/// Deterministic reason code for a strategy choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StrategyReason {
    /// Prompt needs no workspace or tool context.
    NoToolsRequested,
    /// Prompt asks for file inspection or tool use.
    FileInspectionRequested,
    /// Prompt asks for implementation over multiple files.
    MultiFileImplementation,
    /// Prompt asks for a change to an unknown codebase area.
    UnknownCodebaseChange,
    /// Recorded code changes with validation tools available.
    ChangesWithValidation,
    /// Task graph holds independent child work and delegation is allowed.
    ParallelIndependentWork,
}

impl StrategyReason {
    /// Stable machine-readable code.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::NoToolsRequested => "no-tools-requested",
            Self::FileInspectionRequested => "file-inspection-requested",
            Self::MultiFileImplementation => "multi-file-implementation",
            Self::UnknownCodebaseChange => "unknown-codebase-change",
            Self::ChangesWithValidation => "changes-with-validation",
            Self::ParallelIndependentWork => "parallel-independent-work",
        }
    }
}

/// Capability flags a strategy requires from the client/provider.
#[must_use]
pub fn required_capabilities_for(strategy: TurnStrategy) -> Vec<&'static str> {
    match strategy {
        TurnStrategy::SimpleAnswer => Vec::new(),
        TurnStrategy::ToolLoop => vec!["fs:read"],
        TurnStrategy::PlanThenExecute => vec!["fs:read", "fs:write"],
        TurnStrategy::ResearchThenEdit => vec!["fs:read", "fs:write"],
        TurnStrategy::ValidateThenReview => vec!["fs:write", "terminal:run"],
        TurnStrategy::ParallelDelegation => vec!["subagent:spawn"],
    }
}

/// One strategy choice: the strategy, its deterministic reason, and the
/// capabilities it needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StrategyDecision {
    /// Selected strategy.
    pub strategy: TurnStrategy,
    /// Deterministic reason code.
    pub reason: StrategyReason,
    /// Capability flags the strategy requires.
    pub required_capabilities: Vec<String>,
}

impl StrategyDecision {
    fn new(strategy: TurnStrategy, reason: StrategyReason) -> Self {
        Self {
            strategy,
            reason,
            required_capabilities: required_capabilities_for(strategy)
                .into_iter()
                .map(str::to_string)
                .collect(),
        }
    }
}

/// Observed inputs the selector decides from.  Everything is derived from
/// persisted/observed state, never from fabricated claims.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct StrategyContext {
    /// The user prompt text (text blocks joined with spaces).
    pub prompt_text: String,
    /// Whether the session already holds unvalidated code changes.
    pub has_code_changes: bool,
    /// Whether a validation-capable tool is registered.
    pub validation_tools_available: bool,
    /// Whether delegation is allowed by policy.
    pub delegation_allowed: bool,
    /// Snapshot of the task graph (child task shape decides parallel work).
    pub task_graph: TaskGraph,
    /// Tool schemas available to the model.
    pub tool_definitions: Vec<ToolDefinition>,
}

/// Inputs a strategic turn needs beyond the ACP prompt itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StrategicInput {
    /// Whether the session already holds unvalidated code changes.
    pub has_code_changes: bool,
    /// Whether a validation-capable tool is registered.
    pub validation_tools_available: bool,
}

/// Pure, deterministic strategy selector.
#[derive(Debug, Clone, Copy, Default)]
pub struct StrategySelector;

impl StrategySelector {
    /// Selects a strategy by the first matching rule in fixed cascade order:
    /// parallel delegation, validate-then-review, research-then-edit,
    /// plan-then-execute, tool loop, then the conservative default.
    #[must_use]
    pub fn select(&self, ctx: &StrategyContext) -> StrategyDecision {
        if let Some(decision) = self.parallel_delegation(ctx) {
            return decision;
        }
        if let Some(decision) = self.validate_then_review(ctx) {
            return decision;
        }
        if let Some(decision) = self.research_then_edit(ctx) {
            return decision;
        }
        if let Some(decision) = self.plan_then_execute(ctx) {
            return decision;
        }
        if let Some(decision) = self.tool_loop(ctx) {
            return decision;
        }
        StrategyDecision::new(TurnStrategy::SimpleAnswer, StrategyReason::NoToolsRequested)
    }

    /// `ParallelDelegation` only when the task graph holds at least two
    /// independent pending child tasks (no child depends on another child)
    /// and delegation is policy-allowed.  Independence is the deterministic
    /// proxy for read-only or disjoint write scopes.
    fn parallel_delegation(&self, ctx: &StrategyContext) -> Option<StrategyDecision> {
        (ctx.delegation_allowed && has_independent_children(&ctx.task_graph)).then(|| {
            StrategyDecision::new(
                TurnStrategy::ParallelDelegation,
                StrategyReason::ParallelIndependentWork,
            )
        })
    }

    /// `ValidateThenReview` when recorded code changes exist and a
    /// validation-capable tool is available.
    fn validate_then_review(&self, ctx: &StrategyContext) -> Option<StrategyDecision> {
        (ctx.has_code_changes && ctx.validation_tools_available).then(|| {
            StrategyDecision::new(
                TurnStrategy::ValidateThenReview,
                StrategyReason::ChangesWithValidation,
            )
        })
    }

    /// `ResearchThenEdit` when the prompt signals both research and change.
    fn research_then_edit(&self, ctx: &StrategyContext) -> Option<StrategyDecision> {
        let text = ctx.prompt_text.to_ascii_lowercase();
        (contains_any(&text, &RESEARCH_SIGNALS) && contains_any(&text, &CHANGE_SIGNALS)).then(
            || {
                StrategyDecision::new(
                    TurnStrategy::ResearchThenEdit,
                    StrategyReason::UnknownCodebaseChange,
                )
            },
        )
    }

    /// `PlanThenExecute` when the prompt signals implementation across
    /// multiple files.
    fn plan_then_execute(&self, ctx: &StrategyContext) -> Option<StrategyDecision> {
        let text = ctx.prompt_text.to_ascii_lowercase();
        (contains_any(&text, &IMPLEMENTATION_SIGNALS) && contains_any(&text, &MULTI_FILE_SIGNALS))
            .then(|| {
                StrategyDecision::new(
                    TurnStrategy::PlanThenExecute,
                    StrategyReason::MultiFileImplementation,
                )
            })
    }

    /// `ToolLoop` when the prompt asks for file inspection or names a
    /// registered tool.
    fn tool_loop(&self, ctx: &StrategyContext) -> Option<StrategyDecision> {
        let text = ctx.prompt_text.to_ascii_lowercase();
        let mentions_tool = ctx
            .tool_definitions
            .iter()
            .any(|definition| text.contains(&definition.name.to_ascii_lowercase()));
        (contains_any(&text, &INSPECTION_SIGNALS) || mentions_tool).then(|| {
            StrategyDecision::new(TurnStrategy::ToolLoop, StrategyReason::FileInspectionRequested)
        })
    }
}

/// Deterministic keyword signals for the selector.  Heuristics are
/// conservative: each rule needs an explicit signal, and the cascade order is
/// fixed and documented.
const RESEARCH_SIGNALS: [&str; 10] = [
    "investigate",
    "figure out",
    "find out",
    "understand",
    "look into",
    "research",
    "explore",
    "diagnose",
    "root cause",
    "why is",
];
const CHANGE_SIGNALS: [&str; 7] = ["fix", "change", "update", "modify", "add", "implement", "edit"];
const IMPLEMENTATION_SIGNALS: [&str; 8] =
    ["implement", "add", "create", "build", "refactor", "rewrite", "develop", "fix"];
const MULTI_FILE_SIGNALS: [&str; 6] =
    ["files", "module", "crate", "workspace", "package", "codebase"];
const INSPECTION_SIGNALS: [&str; 12] = [
    "read ",
    "inspect",
    "check",
    "list ",
    "open",
    "search",
    "look at",
    "show",
    "find",
    "grep",
    "cat ",
    "read the file",
];

/// Whether `haystack` contains any of `needles` (substring match).
fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

/// Whether the task graph holds at least two independent pending child
/// tasks: none depends on another child.  The deterministic proxy for
/// parallelizable read-only or disjoint write scopes.
#[must_use]
pub fn has_independent_children(tasks: &TaskGraph) -> bool {
    let children: Vec<TaskNode> =
        tasks.list().into_iter().filter(|task| task.parent.is_some()).collect();
    if children.len() < 2 {
        return false;
    }
    let child_ids: HashSet<TaskId> = children.iter().map(|child| child.id.clone()).collect();
    children.iter().all(|child| {
        child.status == TaskStatus::Pending
            && child.dependencies.iter().all(|dependency| !child_ids.contains(dependency))
    })
}

/// Whether a tool name looks validation-capable (`valid`/`validate`,
/// `test`, or `check` in the name).
#[must_use]
pub fn is_validation_tool_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("valid") || lower.contains("test") || lower.contains("check")
}

/// Everything a strategy run needs; all handles are clonable so the
/// validate-then-review wrapper can run its review phase after the edit loop.
#[derive(Clone)]
pub(crate) struct StrategyRun {
    pub task: TaskNode,
    pub task_graph: TaskGraph,
    pub sink: UpdateSink,
    pub client: ClientBridge,
    pub cancel: watch::Receiver<bool>,
    pub execution_log: Arc<Mutex<Vec<ToolExecutionLogEntry>>>,
}

/// Outcome of one strategic turn: the ACP prompt result, the strategy
/// decision, the typed final response, and the reflection outcome.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TurnResult {
    /// The ACP prompt result.
    pub prompt_result: PromptResult,
    /// The strategy that ran.
    pub strategy: StrategyDecision,
    /// The structured final response.
    pub final_response: FinalResponse,
    /// The bounded reflection outcome (review calls, fix loops, findings).
    pub reflection: ReflectionOutcome,
}

/// Strategy execution wrappers.  Built by the runtime with the shared stores;
/// every wrapper goes through the same budget, policy, and cancellation gates
/// as the standard loop.
pub(crate) struct StrategyExecutor {
    config: OrchestratorConfig,
    model: Arc<dyn ModelAdapter>,
    tools: Arc<Mutex<ToolRegistry>>,
    budget: Arc<Mutex<BudgetTracker>>,
    policy: PolicyEngine,
    tasks: Arc<Mutex<TaskGraph>>,
    events: EventRecorder,
}

impl StrategyExecutor {
    pub(crate) fn new(
        config: OrchestratorConfig,
        model: Arc<dyn ModelAdapter>,
        tools: Arc<Mutex<ToolRegistry>>,
        budget: Arc<Mutex<BudgetTracker>>,
        policy: PolicyEngine,
        tasks: Arc<Mutex<TaskGraph>>,
        events: EventRecorder,
    ) -> Self {
        Self { config, model, tools, budget, policy, tasks, events }
    }

    /// Runs one strategic turn: builds the transcript, then dispatches to the
    /// strategy wrapper.  `validation` collects recorded outcomes for
    /// validate-then-review; `run` carries the task, graph, and plumbing.
    /// Loop strategies run the bounded reflection pass when enabled and
    /// return its outcome.
    pub(crate) async fn execute(
        &self,
        strategy: TurnStrategy,
        prompt: PromptContext,
        memory: Option<String>,
        run: StrategyRun,
        validation: &mut ValidationRecorder,
    ) -> Result<(PromptResult, ReflectionOutcome), OrchestratorError> {
        let mut transcript = Transcript::from_prompt(&prompt);
        if let Some(facts) = &memory {
            transcript.prepend_system(format!("Memory facts:\n{facts}"));
        }
        let session_id = prompt.session_id.to_string();
        match strategy {
            TurnStrategy::SimpleAnswer => self
                .run_simple_answer(transcript, session_id, run)
                .await
                .map(|result| (result, ReflectionOutcome::default())),
            TurnStrategy::ToolLoop | TurnStrategy::PlanThenExecute => {
                let (result, mut transcript) = self
                    .run_loop(
                        LoopMode::Standard,
                        false,
                        transcript,
                        session_id.clone(),
                        run.clone(),
                    )
                    .await?;
                let reflection = self
                    .run_reflection_phase(&session_id, &mut transcript, &run, validation)
                    .await?;
                Ok((result, reflection))
            }
            TurnStrategy::ResearchThenEdit => {
                let (result, mut transcript) = self
                    .run_loop(LoopMode::Standard, true, transcript, session_id.clone(), run.clone())
                    .await?;
                let reflection = self
                    .run_reflection_phase(&session_id, &mut transcript, &run, validation)
                    .await?;
                Ok((result, reflection))
            }
            TurnStrategy::ValidateThenReview => {
                self.run_validate_then_review(transcript, session_id, run, validation).await
            }
            TurnStrategy::ParallelDelegation => self
                .run_parallel_delegation(transcript, session_id, run)
                .await
                .map(|result| (result, ReflectionOutcome::default())),
        }
    }

    /// `SimpleAnswer`: one model call, no tool execution; tool intents fail
    /// closed.  Uses the engine's `SimpleAnswer` mode so budget, policy, and
    /// cancellation checks stay identical to the standard path.
    async fn run_simple_answer(
        &self,
        transcript: Transcript,
        session_id: String,
        run: StrategyRun,
    ) -> Result<PromptResult, OrchestratorError> {
        self.run_loop(LoopMode::SimpleAnswer, false, transcript, session_id, run)
            .await
            .map(|(result, _)| result)
    }

    /// Standard or read-first loop over the shared engine.
    async fn run_loop(
        &self,
        mode: LoopMode,
        read_first: bool,
        transcript: Transcript,
        session_id: String,
        run: StrategyRun,
    ) -> Result<(PromptResult, Transcript), OrchestratorError> {
        let options = LoopOptions {
            mode,
            read_first,
            execution_log: Some(run.execution_log.clone()),
            graph: Some(self.tasks.clone()),
            ..LoopOptions::default()
        };
        let engine = LoopEngine::new(
            self.config.clone(),
            self.model.clone(),
            self.tools.clone(),
            self.budget.clone(),
            self.policy.clone(),
            self.events.clone(),
            options,
        );
        engine
            .run_transcript(transcript, session_id, run.sink, run.client, run.cancel, run.task)
            .await
    }

    /// `ValidateThenReview`: run the edit loop, then a bounded review phase
    /// that may only execute validation-capable tools, recording every
    /// outcome into `validation`, then the reflection pass when enabled.
    async fn run_validate_then_review(
        &self,
        transcript: Transcript,
        session_id: String,
        run: StrategyRun,
        validation: &mut ValidationRecorder,
    ) -> Result<(PromptResult, ReflectionOutcome), OrchestratorError> {
        let (result, transcript) = self
            .run_loop(LoopMode::Standard, false, transcript, session_id.clone(), run.clone())
            .await?;
        let mut transcript = self.run_review_phase(transcript, run.clone(), validation).await?;
        let reflection =
            self.run_reflection_phase(&session_id, &mut transcript, &run, validation).await?;
        Ok((result, reflection))
    }

    /// `ParallelDelegation`: re-check the selector preconditions (independent
    /// children, delegation policy) before running the loop; delegate intents
    /// flow through the shared subagent manager.
    async fn run_parallel_delegation(
        &self,
        transcript: Transcript,
        session_id: String,
        run: StrategyRun,
    ) -> Result<PromptResult, OrchestratorError> {
        if !has_independent_children(&run.task_graph) {
            return Err(OrchestratorError::InvalidState(
                "ParallelDelegation requires at least two independent pending child tasks".into(),
            ));
        }
        if !self.policy.policy().allow_delegate {
            return Err(OrchestratorError::PolicyDenied(
                "ParallelDelegation requires delegate policy allowance".into(),
            ));
        }
        self.run_loop(LoopMode::Standard, false, transcript, session_id, run)
            .await
            .map(|(result, _)| result)
    }

    /// One model call after the edit loop; validation tool intents execute
    /// and record outcomes.  Cancellation and budget checks mirror the main
    /// loop; non-validation intents fail closed.  Returns the transcript so
    /// the reflection pass can continue from it.
    async fn run_review_phase(
        &self,
        mut transcript: Transcript,
        run: StrategyRun,
        validation: &mut ValidationRecorder,
    ) -> Result<Transcript, OrchestratorError> {
        if *run.cancel.borrow() {
            return Err(OrchestratorError::Cancellation);
        }
        let budget_snapshot = {
            let mut budget = self.budget.lock().expect("budget tracker poisoned");
            budget.try_reserve_iteration()?;
            budget.try_reserve_model_call()?;
            budget.check_output_allowance()?;
            let snapshot = budget.snapshot();
            budget.emit(&self.events);
            snapshot
        };
        let tools = self.tools.lock().expect("tool registry poisoned").definitions();
        let prepared = crate::prompt_injection::prepare_request(transcript.messages());
        for detection in &prepared.detections {
            self.events.record(OrchestratorEvent::SuspiciousContentDetected {
                trust: detection.trust,
                pattern: detection.pattern.clone(),
                excerpt: detection.excerpt.clone(),
            });
        }
        let request =
            ModelRequest::new(prepared.messages, tools, budget_snapshot, run.task.clone());
        self.events.record(OrchestratorEvent::ModelRequested {
            iteration: budget_snapshot.iterations_used,
        });
        let response = match self.model.complete(request, run.cancel.clone()).await {
            Ok(response) => response,
            Err(error) => {
                self.events.record(OrchestratorEvent::Error { error: error.to_string() });
                return Err(error.into());
            }
        };
        self.events.record(OrchestratorEvent::ModelResponded {
            iteration: budget_snapshot.iterations_used,
        });
        if *run.cancel.borrow() {
            return Err(OrchestratorError::Cancellation);
        }
        {
            let mut budget = self.budget.lock().expect("budget tracker poisoned");
            let output_bytes =
                response.text.len() + response.reasoning.as_deref().map_or(0, str::len);
            budget.record_model_usage(
                output_bytes,
                response.usage.input_tokens,
                response.usage.output_tokens,
            )?;
            budget.emit(&self.events);
        }
        let executor = ToolExecutor::new(
            self.config.clone(),
            self.tools.clone(),
            self.budget.clone(),
            self.policy.clone(),
            0,
            self.events.clone(),
        );
        for intent in response.tool_intents {
            if !is_validation_tool(&intent, &self.tools) {
                return Err(OrchestratorError::ModelFailure(format!(
                    "review phase may only execute validation tools, got {}",
                    intent.name
                )));
            }
            self.events.record(OrchestratorEvent::ToolStarted {
                tool_call_id: intent.tool_call_id.clone(),
                tool_name: intent.name.clone(),
            });
            let executed = executor
                .execute(
                    &intent,
                    &run.sink,
                    &run.client,
                    run.cancel.clone(),
                    &run.task,
                    transcript.messages(),
                )
                .await;
            let success = executed.as_ref().is_ok_and(|result| result.success);
            self.events.record(OrchestratorEvent::ToolFinished {
                tool_call_id: intent.tool_call_id.clone(),
                tool_name: intent.name.clone(),
                success,
            });
            let command = intent.name.clone();
            match executed {
                Ok(result) => {
                    validation.record(
                        command.clone(),
                        if result.success {
                            ValidationOutcome::Passed
                        } else {
                            ValidationOutcome::Failed
                        },
                        Some(result.summary_text()),
                        Some(run.task.id.clone()),
                    );
                    log_execution(
                        &run.execution_log,
                        &intent,
                        Some(result.success),
                        &result.summary_text(),
                    );
                    transcript.push_tool_result(intent.tool_call_id.clone(), result);
                }
                Err(error)
                    if error.is_cancellation()
                        || matches!(
                            error,
                            OrchestratorError::BudgetExceeded(_) | OrchestratorError::Timeout(_)
                        ) =>
                {
                    return Err(error);
                }
                Err(error) => {
                    validation.record(
                        command,
                        ValidationOutcome::Failed,
                        Some(error.to_string()),
                        Some(run.task.id.clone()),
                    );
                    log_execution(&run.execution_log, &intent, None, &error.to_string());
                }
            }
            if *run.cancel.borrow() {
                return Err(OrchestratorError::Cancellation);
            }
        }
        Ok(transcript)
    }

    /// Bounded reflection pass after a tool/edit loop: review model calls
    /// (evidence from the execution log, validation records, and task state)
    /// convert findings into task-graph items, and at most the configured
    /// number of fix loops runs.  Every review call costs a model-call budget
    /// slot and every fix loop a loop-iteration budget slot, so the pass can
    /// never exceed its configured limits.
    async fn run_reflection_phase(
        &self,
        session_id: &str,
        transcript: &mut Transcript,
        run: &StrategyRun,
        validation: &ValidationRecorder,
    ) -> Result<ReflectionOutcome, OrchestratorError> {
        let config = self.config.reflection;
        let mut outcome = ReflectionOutcome::default();
        if !config.enabled {
            return Ok(outcome);
        }
        let log = run.execution_log.lock().expect("execution log poisoned").clone();
        if changed_files_from_log(&log, &run.task.id).is_empty() && validation.records().is_empty()
        {
            // Nothing observed to review: stay silent instead of spending a
            // model call on empty evidence.
            return Ok(outcome);
        }
        let task_graph = self.tasks.lock().expect("task graph poisoned").clone();
        let context = build_review_context(&log, validation, &task_graph);
        loop {
            if outcome.review_calls >= config.max_review_iterations {
                break;
            }
            if *run.cancel.borrow() {
                return Err(OrchestratorError::Cancellation);
            }
            let snapshot = {
                let mut budget = self.budget.lock().expect("budget tracker poisoned");
                budget.try_reserve_iteration()?;
                budget.try_reserve_model_call()?;
                budget.check_output_allowance()?;
                let snapshot = budget.snapshot();
                budget.emit(&self.events);
                snapshot
            };
            let tools = self.tools.lock().expect("tool registry poisoned").definitions();
            let request =
                build_review_request(transcript, &context, tools, snapshot, run.task.clone());
            self.events
                .record(OrchestratorEvent::ModelRequested { iteration: snapshot.iterations_used });
            let response = match self.model.complete(request, run.cancel.clone()).await {
                Ok(response) => response,
                Err(error) => {
                    self.events.record(OrchestratorEvent::Error { error: error.to_string() });
                    return Err(error.into());
                }
            };
            self.events
                .record(OrchestratorEvent::ModelResponded { iteration: snapshot.iterations_used });
            if *run.cancel.borrow() {
                return Err(OrchestratorError::Cancellation);
            }
            {
                let mut budget = self.budget.lock().expect("budget tracker poisoned");
                let output_bytes =
                    response.text.len() + response.reasoning.as_deref().map_or(0, str::len);
                budget.record_model_usage(
                    output_bytes,
                    response.usage.input_tokens,
                    response.usage.output_tokens,
                )?;
                budget.emit(&self.events);
            }
            outcome.review_calls += 1;

            // The review response becomes assistant transcript content so a
            // subsequent fix loop sees the findings.
            transcript.push_assistant(&response);
            let findings = findings_from_response(&response);
            if findings.is_empty() {
                break;
            }
            let ids = {
                let mut tasks = self.tasks.lock().expect("task graph poisoned");
                create_finding_tasks(&mut tasks, &run.task.id, &findings)?
            };
            let mut new_findings: Vec<ReviewFinding> = findings
                .into_iter()
                .map(|detail| ReviewFinding { detail, task_id: None })
                .collect();
            for (finding, id) in new_findings.iter_mut().zip(ids.iter()) {
                finding.task_id = Some(id.clone());
            }
            outcome.findings.extend(new_findings);
            outcome.finding_task_ids.extend(ids);

            if outcome.fix_loops >= config.max_fix_iterations {
                break;
            }
            outcome.fix_loops += 1;
            let (result, updated) = self
                .run_loop(
                    LoopMode::Standard,
                    false,
                    transcript.clone(),
                    session_id.to_string(),
                    run.clone(),
                )
                .await?;
            let _ = result;
            *transcript = updated;
            let mut tasks = self.tasks.lock().expect("task graph poisoned");
            mark_finding_tasks(&mut tasks, &outcome.finding_task_ids, "addressed in fix loop")?;
        }
        Ok(outcome)
    }
}

/// Whether the intent targets a registered validation-capable tool.
fn is_validation_tool(intent: &ToolIntent, tools: &Mutex<ToolRegistry>) -> bool {
    tools
        .lock()
        .expect("tool registry poisoned")
        .get(&intent.name)
        .is_some_and(|tool| is_validation_tool_name(&tool.definition().name))
}

/// Records one executed intent into the shared execution log.
fn log_execution(
    log: &Mutex<Vec<ToolExecutionLogEntry>>,
    intent: &ToolIntent,
    success: Option<bool>,
    summary: &str,
) {
    log.lock().expect("execution log poisoned").push(ToolExecutionLogEntry {
        tool_call_id: intent.tool_call_id.clone(),
        tool_name: intent.name.clone(),
        side_effect_class: None,
        arguments: intent.arguments.clone(),
        success: success.unwrap_or(false),
        summary: summary.to_string(),
    });
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use ee_acp_agent_server::server::OutboundEvent;
    use ee_acp_agent_server::{ClientBridge, UpdateSink};
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

    type ExecutorStores = (
        StrategyExecutor,
        Arc<Mutex<ToolRegistry>>,
        Arc<Mutex<BudgetTracker>>,
        Arc<Mutex<TaskGraph>>,
    );

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
                run,
                &mut validation,
            ));
        let result = executed.map(|(prompt_result, _reflection)| prompt_result);
        let recorded_log = log.lock().expect("log poisoned").clone();
        (result, events, validation, recorded_log, model)
    }

    fn read_tool() -> Arc<FakeTool> {
        Arc::new(FakeTool::new(
            ToolDefinition::new("read_file", "reads a file")
                .side_effect_class(SideEffectClass::Read),
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
                .tool_intents(vec![ToolIntent::new(
                    "tc-1",
                    "read_file",
                    json!({ "path": "/tmp/x" }),
                )])
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
            let config = OrchestratorConfig { max_model_calls: 1, ..OrchestratorConfig::default() };
            let model = Arc::new(FakeModel::new(vec![
                ModelResponse::new().text("first"),
                ModelResponse::new().text("second").completed(),
            ]));
            let (result, _, _, _, model) = run(
                config,
                model,
                None,
                PolicyEngine::default(),
                strategy,
                false,
                TaskGraph::new(),
            );
            assert!(
                matches!(result, Err(OrchestratorError::BudgetExceeded(_))),
                "{strategy:?}: {result:?}"
            );
            assert!(model.call_count() <= 1, "{strategy:?} exceeded its model-call budget");
        }

        // SimpleAnswer fits its single call inside the same budget cap.
        let config = OrchestratorConfig { max_model_calls: 1, ..OrchestratorConfig::default() };
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

        // The review request cited observed evidence only; the review prompt
        // is the last user message (the injection guard may append a policy
        // reminder after it when the transcript carries untrusted tool
        // output).
        let review_request = &model.requests()[2];
        let review_message = review_request
            .transcript
            .iter()
            .rev()
            .find(|message| message.role == ModelRole::User)
            .expect("review prompt message");
        let last_text = review_message.text_content();
        assert!(last_text.contains("Review the completed work"));
        assert!(last_text.contains("- /tmp/out.rs"));

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
}
