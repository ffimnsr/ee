//! Orchestrator runtime: owns the stores and runs turns.
//!
//! [`OrchestratorRuntime`] is constructed once per agent/session with an
//! injected [`ModelAdapter`]; [`OrchestratorRuntime::run_turn`] builds the
//! root task from the prompt and runs one bounded turn over the framework's
//! [`UpdateSink`] and [`ClientBridge`].  The stores (tasks, memory, budget,
//! tools) live inside the runtime, so provider code only interacts with the
//! ACP framework surface.

use std::sync::{Arc, Mutex};

use ee_acp_agent_server::{ClientBridge, PromptContext, PromptResult, UpdateSink};
use ee_agent_protocol::ContentBlock;
use tokio::sync::watch;

use crate::budget::BudgetTracker;
use crate::config::OrchestratorConfig;
use crate::error::OrchestratorError;
use crate::events::{EventRecorder, OrchestratorEvent};
use crate::final_response::{FinalResponseBuilder, ValidationRecorder, changed_files_from_log};
use crate::loop_engine::{LoopEngine, LoopOptions};
use crate::memory::MemoryStore;
use crate::model::ModelAdapter;
use crate::model_registry::{DEFAULT_MODEL_ID, ModelRegistry};
use crate::policy::PolicyEngine;
use crate::progress::ProgressTracker;
use crate::strategy::{
    StrategicInput, StrategyContext, StrategyExecutor, StrategyRun, StrategySelector, TurnResult,
};
use crate::subagents::{DelegateTool, SubagentManager};
use crate::tasks::{TaskGraph, truncate};
use crate::tools::{ServerTool, ToolExecutionLogEntry, ToolRegistry};

/// Cap on the root task title derived from the prompt.
const MAX_TASK_TITLE_CHARS: usize = 120;
/// Cap on the root task description derived from the prompt.
const MAX_TASK_DESCRIPTION_CHARS: usize = 4_000;
/// Title used when the prompt has no text blocks.
const UNTITLED_TASK: &str = "untitled task";

/// Server-side orchestrator runtime.
pub struct OrchestratorRuntime {
    config: OrchestratorConfig,
    models: Arc<ModelRegistry>,
    tools: Arc<Mutex<ToolRegistry>>,
    tasks: Arc<Mutex<TaskGraph>>,
    memory: Arc<Mutex<MemoryStore>>,
    budget: Arc<Mutex<BudgetTracker>>,
    policy: PolicyEngine,
}

impl OrchestratorRuntime {
    /// Creates a runtime with the given config and injected model adapter
    /// and empty stores.  Uses a conservative fail-closed policy: reads are
    /// allowed, writes, executes, and delegation are denied.
    #[must_use]
    pub fn new(config: OrchestratorConfig, model: Arc<dyn ModelAdapter>) -> Self {
        Self::with_policy(config, model, PolicyEngine::default())
    }

    /// Creates a runtime with the given config, injected model adapter, and
    /// policy.  Registers the built-in `delegate_task` tool backed by an
    /// internal subagent manager; delegation requires `allow_delegate` in the
    /// policy, and depth/parallelism limits still apply.
    #[must_use]
    pub fn with_policy(
        config: OrchestratorConfig,
        model: Arc<dyn ModelAdapter>,
        policy: PolicyEngine,
    ) -> Self {
        let memory = MemoryStore::new(config.memory_limit_bytes);
        Self::from_stores(
            config.clone(),
            ModelRegistry::single(model),
            policy,
            Arc::new(Mutex::new(TaskGraph::new())),
            Arc::new(Mutex::new(memory)),
            BudgetTracker::new(&config),
        )
    }

    /// Creates a runtime with a shared model registry, so delegation can
    /// select which adapter a subagent runs on.  Requires a registered
    /// [`DEFAULT_MODEL_ID`] entry (the parent/default adapter); fails closed
    /// otherwise.
    pub fn with_model_registry(
        config: OrchestratorConfig,
        models: ModelRegistry,
        policy: PolicyEngine,
    ) -> Result<Self, OrchestratorError> {
        models.default_adapter()?;
        let memory = MemoryStore::new(config.memory_limit_bytes);
        Ok(Self::from_stores(
            config.clone(),
            models,
            policy,
            Arc::new(Mutex::new(TaskGraph::new())),
            Arc::new(Mutex::new(memory)),
            BudgetTracker::new(&config),
        ))
    }

    /// Creates a runtime from previously persisted task and memory state
    /// (used by `session/load`-style restore flows).  Fresh stores are created
    /// for everything else, so restored state is isolated per session.
    #[must_use]
    pub fn with_state(
        config: OrchestratorConfig,
        model: Arc<dyn ModelAdapter>,
        policy: PolicyEngine,
        tasks: TaskGraph,
        memory: MemoryStore,
    ) -> Self {
        Self::from_stores(
            config.clone(),
            ModelRegistry::single(model),
            policy,
            Arc::new(Mutex::new(tasks)),
            Arc::new(Mutex::new(memory)),
            BudgetTracker::new(&config),
        )
    }

    /// Restores a runtime from a validated checkpoint (see
    /// [`OrchestratorCheckpoint::validate`](crate::checkpoint::OrchestratorCheckpoint::validate)).
    /// The task graph, memory store, and budget counters are rebuilt from the
    /// checkpoint; the wall-clock deadline restarts from the checkpoint's
    /// config.
    pub fn from_checkpoint(
        checkpoint: &crate::checkpoint::OrchestratorCheckpoint,
        model: Arc<dyn ModelAdapter>,
        policy: PolicyEngine,
    ) -> Result<Self, OrchestratorError> {
        checkpoint.validate()?;
        Self::from_validated_checkpoint(checkpoint, model, policy)
    }

    /// Shared restore path; `checkpoint` must already be validated.
    pub(crate) fn from_validated_checkpoint(
        checkpoint: &crate::checkpoint::OrchestratorCheckpoint,
        model: Arc<dyn ModelAdapter>,
        policy: PolicyEngine,
    ) -> Result<Self, OrchestratorError> {
        let config = checkpoint.config.clone();
        let mut budget = BudgetTracker::new(&config);
        budget.restore_used(&checkpoint.budget)?;
        Ok(Self::from_stores(
            config,
            ModelRegistry::single(model),
            policy,
            Arc::new(Mutex::new(checkpoint.tasks.clone())),
            Arc::new(Mutex::new(checkpoint.memory.clone())),
            budget,
        ))
    }

    /// Shared constructor: wires the stores, budget, and delegate tool.
    fn from_stores(
        config: OrchestratorConfig,
        models: ModelRegistry,
        policy: PolicyEngine,
        tasks: Arc<Mutex<TaskGraph>>,
        memory: Arc<Mutex<MemoryStore>>,
        budget: BudgetTracker,
    ) -> Self {
        let budget = Arc::new(Mutex::new(budget));
        let models = Arc::new(models);
        let tools = Arc::new(Mutex::new(ToolRegistry::new()));
        let manager = Arc::new(SubagentManager::new(
            config.clone(),
            models.clone(),
            tools.clone(),
            tasks.clone(),
            memory.clone(),
            budget.clone(),
        ));
        tools
            .lock()
            .expect("tool registry poisoned")
            .register(Arc::new(DelegateTool::new(manager)))
            .expect("registers delegate_task");
        Self { config, models, tools, tasks, memory, budget, policy }
    }

    /// The active configuration.
    #[must_use]
    pub fn config(&self) -> &OrchestratorConfig {
        &self.config
    }

    /// Registers a server-side tool for the loop engine.
    pub fn register_tool(&self, tool: Arc<dyn ServerTool>) -> Result<(), OrchestratorError> {
        self.tools.lock().expect("tool registry poisoned").register(tool)
    }

    /// Registers the built-in client-bridge tools (`read_file`, `write_file`,
    /// terminal lifecycle, `ask_user`) bound to `session_id`.  Called once per
    /// session by the ACP adapter; the registry rejects duplicate names.
    pub fn register_builtins(
        &self,
        session_id: &ee_agent_protocol::SessionId,
    ) -> Result<(), OrchestratorError> {
        self.tools.lock().expect("tool registry poisoned").register_builtins(session_id)
    }

    /// Snapshot of the current memory store state.
    #[must_use]
    pub fn memory(&self) -> MemoryStore {
        self.memory.lock().expect("memory store poisoned").clone()
    }

    /// Snapshot of the current task graph state.
    #[must_use]
    pub fn tasks(&self) -> TaskGraph {
        self.tasks.lock().expect("task graph poisoned").clone()
    }

    /// Snapshot of the current budget state, for checkpointing and tests.
    #[must_use]
    pub fn budget_snapshot(&self) -> crate::budget::BudgetSnapshot {
        self.budget.lock().expect("budget tracker poisoned").snapshot()
    }

    /// Runs one turn: builds the root task from the prompt, then runs the
    /// bounded model → tool loop, streaming updates through `sink` and
    /// making agent → client calls through `client`.
    ///
    /// `cancel` flips when the session is closed or the prompt is cancelled;
    /// the loop stops promptly instead of starting new work.
    pub async fn run_turn(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
    ) -> Result<PromptResult, OrchestratorError> {
        self.run_turn_recording(ctx, sink, client, cancel, EventRecorder::new()).await
    }

    /// Same as [`OrchestratorRuntime::run_turn`] but records every loop
    /// decision into `events`; used by tests to assert stable decision
    /// sequences.
    pub(crate) async fn run_turn_recording(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        events: EventRecorder,
    ) -> Result<PromptResult, OrchestratorError> {
        let (title, description) = task_summary(&ctx);
        let (task, entries) = {
            let mut tasks = self.tasks.lock().expect("task graph poisoned");
            let task = tasks.create_root(&title, &description);
            let entries = tasks.plan_entries();
            (task, entries)
        };
        // Keep the client's plan view in sync with the task graph.
        sink.plan_replace(entries).map_err(|error| {
            OrchestratorError::InvalidState(format!("plan emission failed: {error}"))
        })?;
        let memory = self.memory.lock().expect("memory store poisoned").compact_context();
        let model = self.models.default_adapter()?;
        let engine = LoopEngine::new(
            self.config.clone(),
            model,
            self.tools.clone(),
            self.budget.clone(),
            self.policy.clone(),
            events,
            LoopOptions {
                graph: Some(self.tasks.clone()),
                available_models: self.models.advertised(),
                model_id: Some(DEFAULT_MODEL_ID.to_string()),
                ..LoopOptions::default()
            },
        );
        engine.run(ctx, sink, client, cancel, task, memory).await
    }

    /// Runs one strategic turn: selects a [`TurnStrategy`] from observed
    /// context, records the decision as an event, executes the strategy
    /// wrapper, and returns the ACP result together with the typed
    /// [`FinalResponse`].  Strategy execution never bypasses the configured
    /// budget, policy, or cancellation gates.
    pub async fn run_turn_strategic(
        &self,
        ctx: PromptContext,
        input: StrategicInput,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
    ) -> Result<TurnResult, OrchestratorError> {
        self.run_turn_strategic_recording(ctx, input, sink, client, cancel, EventRecorder::new())
            .await
    }

    /// Same as [`OrchestratorRuntime::run_turn_strategic`] but records every
    /// loop decision (including `StrategySelected`) into `events`.
    pub(crate) async fn run_turn_strategic_recording(
        &self,
        ctx: PromptContext,
        input: StrategicInput,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        events: EventRecorder,
    ) -> Result<TurnResult, OrchestratorError> {
        let (title, description) = task_summary(&ctx);
        let (task, entries, task_graph) = {
            let mut tasks = self.tasks.lock().expect("task graph poisoned");
            let task = tasks.create_root(&title, &description);
            let entries = tasks.plan_entries();
            let graph = tasks.clone();
            (task, entries, graph)
        };
        // Plan emission precedes any tool work: the client sees the task
        // graph before the first execution.
        sink.plan_replace(entries).map_err(|error| {
            OrchestratorError::InvalidState(format!("plan emission failed: {error}"))
        })?;
        let prompt_text = ctx
            .prompt
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        let strategy_ctx = StrategyContext {
            prompt_text,
            has_code_changes: input.has_code_changes,
            validation_tools_available: input.validation_tools_available,
            delegation_allowed: self.policy.policy().allow_delegate,
            task_graph: task_graph.clone(),
            tool_definitions: self.tools.lock().expect("tool registry poisoned").definitions(),
        };
        let decision = StrategySelector.select(&strategy_ctx);
        events.record(OrchestratorEvent::StrategySelected {
            strategy: decision.strategy,
            reason: decision.reason,
        });
        let memory = self.memory.lock().expect("memory store poisoned").compact_context();
        let execution_log: Arc<Mutex<Vec<ToolExecutionLogEntry>>> =
            Arc::new(Mutex::new(Vec::new()));
        let run = StrategyRun {
            task: task.clone(),
            task_graph,
            sink,
            client,
            cancel,
            execution_log: execution_log.clone(),
        };
        let mut validation = ValidationRecorder::new();
        let model = self.models.default_adapter()?;
        let executor = StrategyExecutor::new(
            self.config.clone(),
            model,
            self.tools.clone(),
            self.budget.clone(),
            self.policy.clone(),
            self.tasks.clone(),
            events.clone(),
        );
        let (prompt_result, reflection) =
            executor.execute(decision.strategy, ctx, memory, run, &mut validation).await?;
        let log = execution_log.lock().expect("execution log poisoned").clone();
        let changed_files = changed_files_from_log(&log, &task.id);
        let progress = ProgressTracker::from_execution_log(
            &log,
            &validation,
            (reflection.review_calls > 0).then_some(reflection.findings.len()),
        )
        .score(&self.tasks.lock().expect("task graph poisoned").clone());
        let final_response = FinalResponseBuilder {
            changed_files,
            validation: &validation,
            unresolved_risks: Vec::new(),
            follow_up_suggestions: Vec::new(),
            task_graph: &self.tasks.lock().expect("task graph poisoned").clone(),
            memory: &self.memory.lock().expect("memory store poisoned").clone(),
            progress: Some(&progress),
        }
        .build();
        Ok(TurnResult { prompt_result, strategy: decision, final_response, reflection })
    }
}

/// Derives the bounded root-task title and description from the prompt's
/// text blocks.
fn task_summary(ctx: &PromptContext) -> (String, String) {
    let texts: Vec<String> = ctx
        .prompt
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect();
    let description = texts.join(" ");
    let title = texts.first().cloned().unwrap_or_default();
    let title = if title.trim().is_empty() {
        UNTITLED_TASK.to_string()
    } else {
        truncate(title.trim(), MAX_TASK_TITLE_CHARS)
    };
    let description = if description.trim().is_empty() {
        title.clone()
    } else {
        truncate(description.trim(), MAX_TASK_DESCRIPTION_CHARS)
    };
    (title, description)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use ee_acp_agent_server::server::OutboundEvent;
    use ee_acp_agent_server::{ClientBridge, UpdateSink};
    use ee_agent_protocol::{ContentBlock, SessionId, SessionUpdate, StopReason, TextContent};
    use serde_json::json;
    use tokio::sync::{mpsc, watch};

    use super::*;
    use crate::events::OrchestratorEvent;
    use crate::model::{ModelResponse, ModelRole};
    use crate::policy::{PolicyEngine, ToolPolicy};
    use crate::strategy::StrategicInput;
    use crate::test_support::{
        FakeModel, FakeTool, delegate_then_answer_script, endless_tool_loop_script,
        simple_answer_script, tool_then_answer_script,
    };
    use crate::tools::{SideEffectClass, ToolDefinition, ToolIntent, ToolResult};

    fn prompt(text: &str) -> PromptContext {
        PromptContext::new(SessionId::new("s-1"), vec![ContentBlock::Text(TextContent::new(text))])
    }

    fn plumbing() -> (UpdateSink, ClientBridge, mpsc::UnboundedReceiver<OutboundEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            UpdateSink::new_for_test(SessionId::new("s-1"), tx.clone()),
            ClientBridge::new_for_test(Duration::from_secs(5), tx),
            rx,
        )
    }

    /// Drains the next `session/update` notification from the outbound
    /// channel, panicking on any other event.
    async fn next_update(rx: &mut mpsc::UnboundedReceiver<OutboundEvent>) -> SessionUpdate {
        match rx.recv().await.expect("outbound event queued") {
            OutboundEvent::Update { update, .. } => *update,
            other => panic!("expected update event, got {other:?}"),
        }
    }

    fn echo_tool() -> Arc<FakeTool> {
        Arc::new(FakeTool::new(
            ToolDefinition::new("echo", "echoes its arguments")
                .side_effect_class(SideEffectClass::Read),
            ToolResult::success("echoed"),
        ))
    }

    #[tokio::test]
    async fn runtime_runs_one_complete_turn_with_fake_model() {
        let model = FakeModel::new(vec![
            ModelResponse::new().reasoning("thinking hard").text("final answer").completed(),
        ]);
        let (sink, client, mut rx) = plumbing();
        let runtime =
            OrchestratorRuntime::new(OrchestratorConfig::default(), Arc::new(model.clone()));
        runtime.register_tool(echo_tool()).expect("registers echo");

        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let result = runtime
            .run_turn(prompt("hello world"), sink, client, cancel_rx)
            .await
            .expect("turn succeeds");
        assert_eq!(result.stop_reason, StopReason::EndTurn);

        // One model call with the prompt as the user message, the root task,
        // the registered tool schema, and the budget snapshot.
        let calls = model.requests();
        assert_eq!(calls.len(), 1);
        let request = &calls[0];
        assert_eq!(request.transcript.len(), 1);
        assert_eq!(request.transcript[0].role, ModelRole::User);
        assert_eq!(
            request.transcript[0].content,
            vec![crate::model::ModelContent::Text("hello world".into())]
        );
        assert_eq!(request.task.title, "hello world");
        assert!(request.task.id.as_str().starts_with("task-"));
        assert!(request.tools.iter().any(|tool| tool.name == "echo"));
        assert_eq!(request.budget.iterations_used, 1);
        assert_eq!(request.budget.iterations_max, 16);

        // Updates stream in order: plan, thought chunk, then message chunk.
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::Plan(_)));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::AgentThoughtChunk(_)));
        let update = next_update(&mut rx).await;
        assert!(matches!(update, SessionUpdate::AgentMessageChunk(_)));
        assert!(rx.try_recv().is_err(), "no further outbound events");
    }

    #[tokio::test]
    async fn runtime_executes_tool_intent_and_continues_loop() {
        let tool = echo_tool();
        let model = FakeModel::new(vec![
            ModelResponse::new().tool_intents(vec![ToolIntent::new(
                "tc-1",
                "echo",
                json!({ "text": "x" }),
            )]),
            ModelResponse::new().text("done").completed(),
        ]);
        let (sink, client, mut rx) = plumbing();
        let runtime =
            OrchestratorRuntime::new(OrchestratorConfig::default(), Arc::new(model.clone()));
        runtime.register_tool(tool.clone()).expect("registers echo");

        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let result = runtime
            .run_turn(prompt("hello world"), sink, client, cancel_rx)
            .await
            .expect("turn succeeds");
        assert_eq!(result.stop_reason, StopReason::EndTurn);

        // The tool ran exactly once with the model's arguments.
        assert_eq!(tool.call_count(), 1);
        assert_eq!(tool.call_arguments(), vec![json!({ "text": "x" })]);

        // The initial plan update precedes the tool lifecycle updates.
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::Plan(_)));
        // Tool updates precede the final message chunk.
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCall(_)));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCallUpdate(_)));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCallUpdate(_)));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::AgentMessageChunk(_)));

        // The second model call sees the tool observation in the transcript.
        let calls = model.requests();
        assert_eq!(calls.len(), 2);
        let transcript = &calls[1].transcript;
        let tool_message = transcript
            .iter()
            .find(|message| message.role == ModelRole::Tool)
            .expect("tool observation appended");
        assert_eq!(tool_message.content.len(), 1);
    }

    #[tokio::test]
    async fn runtime_denies_write_tool_by_default_policy() {
        let tool = Arc::new(FakeTool::new(
            ToolDefinition::new("write_file", "writes").side_effect_class(SideEffectClass::Write),
            ToolResult::success("written"),
        ));
        let model = FakeModel::new(vec![
            ModelResponse::new().tool_intents(vec![ToolIntent::new(
                "tc-1",
                "write_file",
                json!({}),
            )]),
            ModelResponse::new().text("done").completed(),
        ]);
        let (sink, client, mut rx) = plumbing();
        let runtime =
            OrchestratorRuntime::new(OrchestratorConfig::default(), Arc::new(model.clone()));
        runtime.register_tool(tool.clone()).expect("registers write_file");

        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let result = runtime
            .run_turn(prompt("hello world"), sink, client, cancel_rx)
            .await
            .expect("turn succeeds");
        assert_eq!(result.stop_reason, StopReason::EndTurn);

        // The tool never executed; the client saw a failed tool-call update.
        assert_eq!(tool.call_count(), 0);
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::Plan(_)));
        let update = next_update(&mut rx).await;
        let SessionUpdate::ToolCallUpdate(failed) = update else {
            panic!("expected tool call update, got {update:?}");
        };
        assert_eq!(failed.tool_call_id, ee_agent_protocol::ToolCallId::new("tc-1"));
        assert_eq!(failed.fields.status, Some(ee_agent_protocol::ToolCallStatus::Failed));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::AgentMessageChunk(_)));

        // The model saw the denial as a failed tool observation.
        let calls = model.requests();
        assert_eq!(calls.len(), 2);
        let transcript = &calls[1].transcript;
        let tool_message = transcript
            .iter()
            .find(|message| message.role == ModelRole::Tool)
            .expect("tool observation appended");
        let crate::model::ModelContent::ToolResult { result, .. } = &tool_message.content[0] else {
            panic!("expected tool result content");
        };
        assert!(!result.success);
        assert_eq!(result.error_kind, Some(crate::tools::ToolErrorKind::PermissionDenied));
    }

    #[tokio::test]
    async fn runtime_stops_when_loop_iteration_budget_exhausted() {
        let tool = echo_tool();
        let model = FakeModel::new(vec![
            ModelResponse::new().tool_intents(vec![ToolIntent::new("tc-1", "echo", json!({}))]),
            ModelResponse::new().tool_intents(vec![ToolIntent::new("tc-2", "echo", json!({}))]),
            ModelResponse::new().tool_intents(vec![ToolIntent::new("tc-3", "echo", json!({}))]),
        ]);
        let config = OrchestratorConfig { max_loop_iterations: 2, ..OrchestratorConfig::default() };
        let (sink, client, _rx) = plumbing();
        let runtime = OrchestratorRuntime::new(config, Arc::new(model.clone()));
        runtime.register_tool(tool).expect("registers echo");

        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let error = runtime
            .run_turn(prompt("hello world"), sink, client, cancel_rx)
            .await
            .expect_err("iteration budget must stop the loop");
        assert!(
            matches!(error, OrchestratorError::BudgetExceeded(ref reason) if reason.contains("max loop iterations"))
        );
        // Two iterations ran (both model calls happened); the third was denied.
        assert_eq!(model.requests().len(), 2);
    }

    #[tokio::test]
    async fn runtime_cancels_before_first_model_call() {
        let model = FakeModel::new(vec![ModelResponse::new().text("never").completed()]);
        let (sink, client, _rx) = plumbing();
        let runtime =
            OrchestratorRuntime::new(OrchestratorConfig::default(), Arc::new(model.clone()));

        let (_cancel_tx, cancel_rx) = watch::channel(true);
        let error = runtime
            .run_turn(prompt("hello world"), sink, client, cancel_rx)
            .await
            .expect_err("pre-cancelled turn stops");
        assert_eq!(error, OrchestratorError::Cancellation);
        assert_eq!(model.requests().len(), 0, "no model call may start after cancellation");
    }

    #[tokio::test]
    async fn runtime_stops_after_two_empty_model_responses() {
        let model = FakeModel::new(vec![ModelResponse::new(), ModelResponse::new()]);
        let (sink, client, mut rx) = plumbing();
        let runtime =
            OrchestratorRuntime::new(OrchestratorConfig::default(), Arc::new(model.clone()));

        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let result = runtime
            .run_turn(prompt("hello world"), sink, client, cancel_rx)
            .await
            .expect("empty responses stop deterministically");
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        assert_eq!(model.requests().len(), 2);
        // Only the initial plan update is emitted; empty responses stream nothing.
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::Plan(_)));
        assert!(rx.try_recv().is_err(), "empty responses emit no further updates");
    }

    // ── Deterministic fixture scripts ─────────────────────────────────────

    fn budget_event(
        iterations: usize,
        model_calls: usize,
        tools: usize,
        subagents: usize,
        output_bytes: usize,
    ) -> OrchestratorEvent {
        OrchestratorEvent::BudgetUpdated {
            iterations_used: iterations,
            model_calls_used: model_calls,
            tool_calls_used: tools,
            subagents_used: subagents,
            output_bytes_used: output_bytes,
        }
    }

    fn read_file_tool() -> Arc<FakeTool> {
        Arc::new(FakeTool::new(
            ToolDefinition::new("read_file", "reads a file")
                .side_effect_class(SideEffectClass::Read),
            ToolResult::success("file contents"),
        ))
    }

    #[tokio::test]
    async fn fixture_simple_answer_produces_stable_event_sequence() {
        let model = Arc::new(FakeModel::new(simple_answer_script()));
        let runtime = OrchestratorRuntime::new(OrchestratorConfig::default(), model.clone());
        let (sink, client, _rx) = plumbing();
        let events = EventRecorder::new();
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let result = runtime
            .run_turn_recording(prompt("hello world"), sink, client, cancel_rx, events.clone())
            .await
            .expect("turn succeeds");
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        assert_eq!(
            events.events(),
            vec![
                OrchestratorEvent::TurnStarted {
                    session_id: "s-1".into(),
                    task_id: "task-1".into(),
                },
                budget_event(1, 1, 0, 0, 0),
                OrchestratorEvent::ModelRequested { iteration: 1 },
                OrchestratorEvent::ModelResponded { iteration: 1 },
                budget_event(1, 1, 0, 0, 11), // "hello world"
                OrchestratorEvent::TurnStopped { stop_reason: "end_turn".into() },
            ]
        );
    }

    #[tokio::test]
    async fn fixture_tool_then_answer_produces_stable_event_sequence() {
        let model = Arc::new(FakeModel::new(tool_then_answer_script()));
        let runtime = OrchestratorRuntime::new(OrchestratorConfig::default(), model.clone());
        runtime.register_tool(read_file_tool()).expect("registers read_file");
        let (sink, client, _rx) = plumbing();
        let events = EventRecorder::new();
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let result = runtime
            .run_turn_recording(prompt("read a file"), sink, client, cancel_rx, events.clone())
            .await
            .expect("turn succeeds");
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        assert_eq!(
            events.events(),
            vec![
                OrchestratorEvent::TurnStarted {
                    session_id: "s-1".into(),
                    task_id: "task-1".into(),
                },
                budget_event(1, 1, 0, 0, 0),
                OrchestratorEvent::ModelRequested { iteration: 1 },
                OrchestratorEvent::ModelResponded { iteration: 1 },
                budget_event(1, 1, 0, 0, 0), // tool intent only
                OrchestratorEvent::ToolStarted {
                    tool_call_id: "tc-1".into(),
                    tool_name: "read_file".into(),
                },
                budget_event(1, 1, 1, 0, 0), // tool reservation
                OrchestratorEvent::ToolFinished {
                    tool_call_id: "tc-1".into(),
                    tool_name: "read_file".into(),
                    success: true,
                },
                budget_event(2, 2, 1, 0, 0),
                OrchestratorEvent::ModelRequested { iteration: 2 },
                OrchestratorEvent::ModelResponded { iteration: 2 },
                budget_event(2, 2, 1, 0, 7), // "read it"
                OrchestratorEvent::TurnStopped { stop_reason: "end_turn".into() },
            ]
        );
    }

    #[tokio::test]
    async fn fixture_delegate_then_answer_produces_stable_event_sequence() {
        // The child subagent consumes the fixture's answer; the parent gets
        // one more response after the delegation returns.
        let mut script = delegate_then_answer_script();
        script.push(ModelResponse::new().text("parent answer").completed());
        let model = Arc::new(FakeModel::new(script));
        let policy =
            PolicyEngine::new(ToolPolicy { allow_delegate: true, ..ToolPolicy::default() });
        let runtime =
            OrchestratorRuntime::with_policy(OrchestratorConfig::default(), model.clone(), policy);
        let (sink, client, _rx) = plumbing();
        let events = EventRecorder::new();
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let result = runtime
            .run_turn_recording(prompt("delegate work"), sink, client, cancel_rx, events.clone())
            .await
            .expect("turn succeeds");
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        assert_eq!(
            events.events(),
            vec![
                OrchestratorEvent::TurnStarted {
                    session_id: "s-1".into(),
                    task_id: "task-1".into(),
                },
                budget_event(1, 1, 0, 0, 0),
                OrchestratorEvent::ModelRequested { iteration: 1 },
                OrchestratorEvent::ModelResponded { iteration: 1 },
                budget_event(1, 1, 0, 0, 0),
                OrchestratorEvent::ToolStarted {
                    tool_call_id: "tc-1".into(),
                    tool_name: "delegate_task".into(),
                },
                budget_event(1, 1, 1, 0, 0), // tool reservation
                budget_event(1, 1, 1, 1, 0), // subagent reservation
                OrchestratorEvent::SubagentStarted {
                    subagent_id: "task-2".into(),
                    model_id: Some("default".into()),
                },
                // The child runs its own loop over the shared script.
                OrchestratorEvent::TurnStarted {
                    session_id: "subagent".into(),
                    task_id: "task-2".into(),
                },
                budget_event(1, 1, 0, 0, 0),
                OrchestratorEvent::ModelRequested { iteration: 1 },
                OrchestratorEvent::ModelResponded { iteration: 1 },
                budget_event(1, 1, 0, 0, 9), // "delegated"
                OrchestratorEvent::TurnStopped { stop_reason: "end_turn".into() },
                OrchestratorEvent::SubagentFinished { subagent_id: "task-2".into(), success: true },
                OrchestratorEvent::ToolFinished {
                    tool_call_id: "tc-1".into(),
                    tool_name: "delegate_task".into(),
                    success: true,
                },
                budget_event(2, 2, 1, 1, 0),
                OrchestratorEvent::ModelRequested { iteration: 2 },
                OrchestratorEvent::ModelResponded { iteration: 2 },
                budget_event(2, 2, 1, 1, 13), // "parent answer"
                OrchestratorEvent::TurnStopped { stop_reason: "end_turn".into() },
            ]
        );
    }

    #[tokio::test]
    async fn fixture_endless_tool_loop_stops_via_iteration_budget() {
        let model = Arc::new(FakeModel::new(endless_tool_loop_script(6)));
        let config = OrchestratorConfig { max_loop_iterations: 3, ..OrchestratorConfig::default() };
        let runtime = OrchestratorRuntime::new(config, model.clone());
        runtime.register_tool(read_file_tool()).expect("registers read_file");
        let (sink, client, _rx) = plumbing();
        let events = EventRecorder::new();
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let error = runtime
            .run_turn_recording(prompt("loop forever"), sink, client, cancel_rx, events.clone())
            .await
            .expect_err("iteration budget stops the loop");
        assert!(matches!(error, OrchestratorError::BudgetExceeded(_)));
        assert_eq!(model.call_count(), 3, "exactly the allowed iterations ran");
        let recorded = events.events();
        assert_eq!(
            recorded[0],
            OrchestratorEvent::TurnStarted { session_id: "s-1".into(), task_id: "task-1".into() }
        );
        assert_eq!(recorded[1], budget_event(1, 1, 0, 0, 0));
        // Every iteration reserves, calls the model, and executes the tool.
        assert_eq!(
            recorded
                .iter()
                .filter(|e| matches!(e, OrchestratorEvent::ModelRequested { .. }))
                .count(),
            3
        );
        assert_eq!(
            recorded.iter().filter(|e| matches!(e, OrchestratorEvent::ToolStarted { .. })).count(),
            3
        );
        assert_eq!(recorded[recorded.len() - 3], budget_event(3, 3, 3, 0, 0));
        assert_eq!(
            recorded[recorded.len() - 2],
            OrchestratorEvent::ToolFinished {
                tool_call_id: "tc-2".into(),
                tool_name: "read_file".into(),
                success: true,
            }
        );
        assert_eq!(
            recorded[recorded.len() - 1],
            OrchestratorEvent::TurnStopped { stop_reason: "budget_exceeded".into() }
        );
    }

    #[test]
    fn task_summary_derives_bounded_title_and_description() {
        let ctx = prompt("first line");
        let (title, description) = task_summary(&ctx);
        assert_eq!(title, "first line");
        assert_eq!(description, "first line");

        let long = "x".repeat(4_500);
        let ctx = prompt(&long);
        let (title, description) = task_summary(&ctx);
        assert_eq!(title.chars().count(), MAX_TASK_TITLE_CHARS + 1);
        assert!(title.ends_with('…'));
        assert_eq!(description.chars().count(), MAX_TASK_DESCRIPTION_CHARS + 1);
    }

    #[test]
    fn task_summary_falls_back_for_prompt_without_text() {
        let ctx = PromptContext::new(SessionId::new("s-1"), Vec::new());
        let (title, description) = task_summary(&ctx);
        assert_eq!(title, UNTITLED_TASK);
        assert_eq!(description, UNTITLED_TASK);
    }

    // ── Strategic turn path ────────────────────────────────────────────────

    #[tokio::test]
    async fn strategic_turn_selects_strategy_and_emits_decision_event() {
        let model =
            Arc::new(FakeModel::new(vec![ModelResponse::new().text("implemented").completed()]));
        let runtime = OrchestratorRuntime::new(OrchestratorConfig::default(), model);
        let (sink, client, _rx) = plumbing();
        let events = EventRecorder::new();
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let turn = runtime
            .run_turn_strategic_recording(
                prompt("implement login across multiple files"),
                StrategicInput::default(),
                sink,
                client,
                cancel_rx,
                events.clone(),
            )
            .await
            .expect("strategic turn succeeds");
        assert_eq!(turn.prompt_result.stop_reason, StopReason::EndTurn);
        assert_eq!(turn.strategy.strategy, crate::strategy::TurnStrategy::PlanThenExecute);
        assert_eq!(
            events.events()[0],
            OrchestratorEvent::StrategySelected {
                strategy: crate::strategy::TurnStrategy::PlanThenExecute,
                reason: crate::strategy::StrategyReason::MultiFileImplementation,
            }
        );
        assert!(turn.final_response.summary.contains("no files changed"));
        assert!(!turn.final_response.validation_passed());
    }

    #[tokio::test]
    async fn strategic_turn_plan_update_precedes_tool_updates() {
        let model = Arc::new(FakeModel::new(vec![
            ModelResponse::new().tool_intents(vec![ToolIntent::new(
                "tc-1",
                "read_file",
                json!({ "path": "/tmp/x" }),
            )]),
            ModelResponse::new().text("read it").completed(),
        ]));
        let runtime = OrchestratorRuntime::new(OrchestratorConfig::default(), model);
        runtime.register_tool(read_file_tool()).expect("registers read_file");
        let (sink, client, mut rx) = plumbing();
        let events = EventRecorder::new();
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let turn = runtime
            .run_turn_strategic_recording(
                prompt("read the file"),
                StrategicInput::default(),
                sink,
                client,
                cancel_rx,
                events,
            )
            .await
            .expect("strategic turn succeeds");
        assert_eq!(turn.strategy.strategy, crate::strategy::TurnStrategy::ToolLoop);
        // The plan update lands before any tool lifecycle update.
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::Plan(_)));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCall(_)));
        assert_eq!(turn.final_response.changed_file_count(), 0);
    }

    #[tokio::test]
    async fn strategic_turn_builds_final_response_from_observed_writes() {
        let model = Arc::new(FakeModel::new(vec![
            ModelResponse::new().tool_intents(vec![ToolIntent::new(
                "tc-1",
                "write_file",
                json!({ "path": "/tmp/out.rs", "content": "fn main() {}" }),
            )]),
            ModelResponse::new().text("done").completed(),
        ]));
        let policy = crate::policy::PolicyEngine::new(crate::policy::ToolPolicy {
            allow_write: true,
            ..crate::policy::ToolPolicy::default()
        });
        let runtime =
            OrchestratorRuntime::with_policy(OrchestratorConfig::default(), model, policy);
        runtime
            .register_tool(Arc::new(FakeTool::new(
                crate::tools::ToolDefinition::new("write_file", "writes a file")
                    .side_effect_class(crate::tools::SideEffectClass::Write),
                crate::tools::ToolResult::success("file written"),
            )))
            .expect("registers write_file");
        let (sink, client, _rx) = plumbing();
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let turn = runtime
            .run_turn_strategic(
                prompt("read the file"),
                StrategicInput::default(),
                sink,
                client,
                cancel_rx,
            )
            .await
            .expect("strategic turn succeeds");
        assert_eq!(turn.final_response.changed_file_count(), 1);
        assert_eq!(turn.final_response.changed_files[0].path, "/tmp/out.rs");
        assert!(turn.final_response.summary.contains("changed files: /tmp/out.rs"));
        assert!(turn.final_response.provenance.contains(&"change:/tmp/out.rs:task-1".to_string()));
    }

    #[tokio::test]
    async fn strategic_turn_respects_cancellation() {
        let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("x").completed()]));
        let runtime = OrchestratorRuntime::new(OrchestratorConfig::default(), model.clone());
        let (sink, client, _rx) = plumbing();
        let events = EventRecorder::new();
        let (_cancel_tx, cancel_rx) = watch::channel(true);

        let error = runtime
            .run_turn_strategic_recording(
                prompt("hello world"),
                StrategicInput::default(),
                sink,
                client,
                cancel_rx,
                events.clone(),
            )
            .await
            .expect_err("pre-cancelled strategic turn stops");
        assert_eq!(error, OrchestratorError::Cancellation);
        assert_eq!(model.call_count(), 0);
        // The decision was still recorded before execution was blocked.
        assert!(matches!(events.events()[0], OrchestratorEvent::StrategySelected { .. }));
    }
}
