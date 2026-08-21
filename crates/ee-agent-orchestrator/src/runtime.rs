//! Orchestrator runtime: owns the stores and runs turns.
//!
//! [`OrchestratorRuntime`] is constructed once per agent/session with an
//! injected [`ModelAdapter`]; [`OrchestratorRuntime::run_turn`] builds the
//! root task from the prompt and runs one bounded turn over the framework's
//! [`UpdateSink`] and [`ClientBridge`].  The stores (tasks, memory, budget,
//! tools) live inside the runtime, so provider code only interacts with the
//! ACP framework surface.

use std::sync::{Arc, Mutex, RwLock};

use ee_acp_agent_server::{ClientBridge, PromptContext, PromptResult, UpdateSink};
use ee_agent_protocol::{
    COMPACT_COMMAND_NAME, ContentBlock, RecoverableFault, SessionUpdate, UsageUpdate,
    parse_slash_command,
};
use tokio::sync::watch;

use crate::budget::BudgetTracker;
use crate::checkpoint::{OrchestratorCheckpoint, current_unix_millis};
use crate::checkpoint_store::{CheckpointHandle, CheckpointStore};
use crate::compaction::{
    CompactTurnReport, SESSION_SUMMARY_KEY, build_compaction_context, build_compaction_prompt,
};
use crate::config::OrchestratorConfig;
use crate::context_planner::{ContextPlan, ContextPlanner, ContextPlannerConfig};
use crate::error::OrchestratorError;
use crate::events::{EventRecorder, OrchestratorEvent};
use crate::final_response::{FinalResponseBuilder, ValidationRecorder, changed_files_from_log};
use crate::loop_engine::{LoopEngine, LoopOptions, TurnSystemContext};
use crate::memory::{MemoryItem, MemoryStore};
use crate::memory_compaction::compact_memory;
use crate::model::{
    ModelAdapter, ModelMessage, ModelRequest, ModelRole, Transcript, prompt_result_with_usage,
};
use crate::model_registry::{DEFAULT_MODEL_ID, ModelRegistry};
use crate::policy::PolicyEngine;
use crate::progress::ProgressTracker;
use crate::recovery::{RecoverableInterruption, TurnOutcome, session_timeout_expired};
use crate::sensitive_data::SensitiveDataGuard;
use crate::strategy::{
    StrategicInput, StrategyContext, StrategyExecutor, StrategyRun, StrategySelector, TurnResult,
};
use crate::subagents::{DelegateTool, SubagentManager};
use crate::tasks::{TaskGraph, TaskId, TaskNode, truncate};
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
    policy: Arc<RwLock<PolicyEngine>>,
    checkpoints: Arc<CheckpointStore>,
    events: EventRecorder,
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
        let checkpoints = Arc::new(CheckpointStore::new(&config.recovery));
        let events = EventRecorder::new();
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
        Self {
            config,
            models,
            tools,
            tasks,
            memory,
            budget,
            policy: Arc::new(RwLock::new(policy)),
            checkpoints,
            events,
        }
    }

    /// Replaces the active tool policy without resetting session state.
    pub fn set_policy(&self, policy: PolicyEngine) {
        *self.policy.write().expect("runtime policy poisoned") = policy;
    }

    /// Snapshot of the active tool policy.
    #[must_use]
    pub fn policy(&self) -> PolicyEngine {
        self.policy.read().expect("runtime policy poisoned").clone()
    }

    /// Snapshot of the runtime's recovery/loop events (diagnostics and
    /// traceability; the loop's own recorder is separate).
    #[must_use]
    pub fn event_snapshot(&self) -> Vec<OrchestratorEvent> {
        self.events.events()
    }

    /// The checkpoint store backing this runtime (same directory as every
    /// other runtime sharing the recovery config).
    #[must_use]
    pub(crate) fn checkpoint_store(&self) -> Arc<CheckpointStore> {
        self.checkpoints.clone()
    }

    /// Replaces the task/memory/budget stores from a validated checkpoint
    /// and restarts the wall-clock deadline slice.  Cumulative budget
    /// counters are retained; only the deadline is fresh.
    pub fn restore_from_checkpoint(
        &self,
        checkpoint: &OrchestratorCheckpoint,
    ) -> Result<(), OrchestratorError> {
        checkpoint.validate()?;
        *self.tasks.lock().expect("task graph poisoned") = checkpoint.tasks.clone();
        *self.memory.lock().expect("memory store poisoned") = checkpoint.memory.clone();
        let mut budget = BudgetTracker::new(&checkpoint.config);
        budget.restore_used(&checkpoint.budget)?;
        *self.budget.lock().expect("budget tracker poisoned") = budget;
        Ok(())
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

    /// Removes a previously registered tool (per-prompt MCP tools are
    /// deregistered at prompt end so they never leak across turns).
    pub fn remove_tool(&self, name: &str) {
        self.tools.lock().expect("tool registry poisoned").remove(name);
    }

    /// Names of the currently registered tools (tests and diagnostics).
    #[must_use]
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.lock().expect("tool registry poisoned").names()
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

    /// Runs one turn with immutable session context prepended to the model
    /// transcript as system facts.
    pub async fn run_turn_with_system_context(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        system_context: String,
    ) -> Result<PromptResult, OrchestratorError> {
        self.run_turn_recording_with_system_context(
            ctx,
            sink,
            client,
            cancel,
            EventRecorder::new(),
            Some(system_context),
        )
        .await
    }

    /// Runs one turn with recovery enabled: milestone checkpoints are
    /// persisted and deadline/timeout stops become
    /// [`TurnOutcome::Interrupted`] carrying a durable checkpoint instead of
    /// a fatal error.  Completed turns and cancellations clear the session's
    /// pending checkpoints.  `provider` stamps the checkpoint identity used
    /// by crash restore.
    pub async fn run_turn_recoverable(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        system_context: String,
        provider: &str,
    ) -> Result<TurnOutcome, OrchestratorError> {
        if !self.config.recovery.enabled {
            let result = self
                .run_turn_recording_with_system_context(
                    ctx,
                    sink,
                    client,
                    cancel,
                    EventRecorder::new(),
                    Some(system_context),
                )
                .await?;
            return Ok(TurnOutcome::Completed(result));
        }
        // Agent-advertised slash commands arrive as ordinary prompt text;
        // `/compact` takes the compaction path before any task or tool work.
        let prompt_text = prompt_text(&ctx);
        if let Some(command) = parse_slash_command(&prompt_text)
            && command.name == COMPACT_COMMAND_NAME
        {
            let result = self
                .run_compact_turn(ctx, sink, cancel, command.instructions, Some(system_context))
                .await?;
            return Ok(TurnOutcome::Completed(result));
        }
        let (title, description) = task_summary(&ctx);
        let (task, entries) = {
            let mut tasks = self.tasks.lock().expect("task graph poisoned");
            let task = tasks.create_root(&title, &description);
            let entries = tasks.plan_entries();
            (task, entries)
        };
        sink.plan_replace(entries).map_err(|error| {
            OrchestratorError::InvalidState(format!("plan emission failed: {error}"))
        })?;
        let session_id = ctx.session_id.to_string();
        let handle = CheckpointHandle::new(self.checkpoints.clone(), &session_id, provider);
        let outcome = self
            .run_loop_with_options(
                ctx.clone(),
                sink,
                client,
                cancel,
                task,
                Some(system_context),
                self.events.clone(),
                LoopOptions {
                    graph: Some(self.tasks.clone()),
                    available_models: self.models.advertised(),
                    model_id: Some(DEFAULT_MODEL_ID.to_string()),
                    memory: Some(self.memory.clone()),
                    checkpoint: Some(handle),
                    ..LoopOptions::default()
                },
            )
            .await;
        match outcome {
            Ok(result) => {
                self.checkpoints.delete_session(&session_id);
                Ok(TurnOutcome::Completed(result))
            }
            Err(OrchestratorError::Cancellation) => {
                self.checkpoints.delete_session(&session_id);
                Err(OrchestratorError::Cancellation)
            }
            Err(OrchestratorError::DeadlineExceeded(detail)) => Ok(TurnOutcome::Interrupted(
                self.interruption_for(&session_id, RecoverableFault::Deadline, detail, None)?,
            )),
            Err(OrchestratorError::Timeout(detail)) => Ok(TurnOutcome::Interrupted(
                self.interruption_for(&session_id, RecoverableFault::Deadline, detail, None)?,
            )),
            Err(error) => Err(error),
        }
    }

    /// Resumes an interrupted turn from its latest checkpoint: restores the
    /// stores (fresh deadline slice, cumulative counters retained), appends
    /// the new prompt to the checkpoint transcript tail, and runs the loop
    /// with the completed-tool idempotency guard.  `provider` must match the
    /// checkpoint's provider identity (crash-restore validation).
    pub async fn resume_turn(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        system_context: String,
        provider: &str,
    ) -> Result<TurnOutcome, OrchestratorError> {
        let session_id = ctx.session_id.to_string();
        let Some((checkpoint_id, checkpoint)) =
            self.checkpoints.load_latest(&session_id).map_err(|error| {
                OrchestratorError::Serialization(format!(
                    "failed to load pending checkpoint: {error}"
                ))
            })?
        else {
            return Err(OrchestratorError::InvalidState(format!(
                "no pending checkpoint for session {session_id}"
            )));
        };
        if checkpoint.provider != provider {
            return Err(OrchestratorError::PolicyDenied(format!(
                "checkpoint provider {:?} does not match {:?}; refusing restore",
                checkpoint.provider, provider
            )));
        }
        let resume = checkpoint.resume.clone().ok_or_else(|| {
            OrchestratorError::InvalidState("checkpoint has no resumable turn state".into())
        })?;
        if session_timeout_expired(
            &self.config,
            resume.first_started_at_millis,
            current_unix_millis(),
        ) {
            self.checkpoints.delete_session(&session_id);
            return Err(OrchestratorError::InvalidState(
                "session exceeded its cumulative timeout; checkpoint discarded".into(),
            ));
        }
        self.restore_from_checkpoint(&checkpoint)?;
        let mut resumed = resume.clone();
        resumed.resumed_count += 1;
        resumed.in_flight = None;
        self.events.record(OrchestratorEvent::TurnResumed {
            session_id: session_id.clone(),
            checkpoint_id: checkpoint_id.clone(),
            resumed_count: resumed.resumed_count,
        });
        let mut transcript = Transcript::new();
        transcript.messages = resume.transcript;
        // `/resume` continuation carries no prompt text: the original prompt
        // already lives in the checkpoint transcript, so nothing is appended.
        if !ctx.prompt.is_empty() {
            transcript.messages.extend(Transcript::from_prompt(&ctx).messages);
        }
        let memory = self.memory.lock().expect("memory store poisoned").compact_context();
        if let Some(facts) = memory {
            transcript.prepend_system(format!("Memory facts:\n{facts}"));
        }
        if !system_context.is_empty() {
            transcript.prepend_system(&system_context);
        }
        let handle = CheckpointHandle::new(self.checkpoints.clone(), &session_id, provider);
        let model = self.models.default_adapter()?;
        let policy = self.policy();
        let engine = LoopEngine::new(
            self.config.clone(),
            model,
            self.tools.clone(),
            self.budget.clone(),
            policy,
            self.events.clone(),
            LoopOptions {
                graph: Some(self.tasks.clone()),
                available_models: self.models.advertised(),
                model_id: Some(DEFAULT_MODEL_ID.to_string()),
                memory: Some(self.memory.clone()),
                checkpoint: Some(handle),
                resume_state: Some(resumed),
                ..LoopOptions::default()
            },
        );
        let task = {
            let tasks = self.tasks.lock().expect("task graph poisoned");
            tasks.get(&crate::tasks::TaskId::new(resume.active_task_id.clone())).cloned()
        }
        .ok_or_else(|| {
            OrchestratorError::InvalidState(format!(
                "resume references unknown active task {}",
                resume.active_task_id
            ))
        })?;
        let outcome = engine
            .run_transcript(&mut transcript, session_id.clone(), sink, client, cancel, task)
            .await;
        match outcome {
            Ok(result) => {
                self.checkpoints.delete_session(&session_id);
                Ok(TurnOutcome::Completed(result))
            }
            Err(OrchestratorError::Cancellation) => {
                self.checkpoints.delete_session(&session_id);
                Err(OrchestratorError::Cancellation)
            }
            Err(OrchestratorError::DeadlineExceeded(detail)) => {
                Ok(TurnOutcome::Interrupted(self.interruption_for(
                    &session_id,
                    RecoverableFault::Deadline,
                    detail,
                    Some(resume.resumed_count + 1),
                )?))
            }
            Err(OrchestratorError::Timeout(detail)) => {
                Ok(TurnOutcome::Interrupted(self.interruption_for(
                    &session_id,
                    RecoverableFault::Deadline,
                    detail,
                    Some(resume.resumed_count + 1),
                )?))
            }
            Err(error) => Err(error),
        }
    }

    /// Builds a [`RecoverableInterruption`] from the session's latest
    /// checkpoint, recording the interruption event.
    fn interruption_for(
        &self,
        session_id: &str,
        fault: RecoverableFault,
        detail: String,
        resumed_count_hint: Option<u32>,
    ) -> Result<RecoverableInterruption, OrchestratorError> {
        let latest = self.checkpoints.load_latest(session_id)?;
        let interruption = RecoverableInterruption::from_checkpoint(
            fault,
            detail,
            None,
            None,
            latest.as_ref().map(|(id, checkpoint)| (id.as_str(), checkpoint)),
        );
        let interruption = match resumed_count_hint {
            Some(count) => RecoverableInterruption { resumed_count: count, ..interruption },
            None => interruption,
        };
        self.events.record(OrchestratorEvent::TurnInterrupted {
            session_id: session_id.to_string(),
            fault: fault.as_str().to_string(),
            safe_resume: interruption.safe_resume,
            resumed_count: interruption.resumed_count,
        });
        Ok(interruption)
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
        self.run_turn_recording_with_system_context(ctx, sink, client, cancel, events, None).await
    }

    async fn run_turn_recording_with_system_context(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        events: EventRecorder,
        system_context: Option<String>,
    ) -> Result<PromptResult, OrchestratorError> {
        // Agent-advertised slash commands arrive as ordinary prompt text;
        // `/compact` takes the compaction path before any task or tool work.
        let prompt_text = prompt_text(&ctx);
        if let Some(command) = parse_slash_command(&prompt_text)
            && command.name == COMPACT_COMMAND_NAME
        {
            return self
                .run_compact_turn(ctx, sink, cancel, command.instructions, system_context)
                .await;
        }
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
        self.run_loop_with_options(
            ctx,
            sink,
            client,
            cancel,
            task,
            system_context,
            events,
            LoopOptions {
                graph: Some(self.tasks.clone()),
                available_models: self.models.advertised(),
                model_id: Some(DEFAULT_MODEL_ID.to_string()),
                ..LoopOptions::default()
            },
        )
        .await
    }

    /// Shared turn runner: builds the engine from `options` and runs one
    /// bounded turn over the prompt with the given system context.  The
    /// wall-clock deadline is re-anchored to this turn's slice (a runtime
    /// may idle between turns for longer than one timeout).
    #[allow(clippy::too_many_arguments)]
    async fn run_loop_with_options(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        task: TaskNode,
        system_context: Option<String>,
        events: EventRecorder,
        options: LoopOptions,
    ) -> Result<PromptResult, OrchestratorError> {
        self.budget
            .lock()
            .expect("budget tracker poisoned")
            .reset_deadline(self.config.turn_timeout);
        let memory = self.memory.lock().expect("memory store poisoned").compact_context();
        let model = self.models.default_adapter()?;
        let policy = self.policy();
        let engine = LoopEngine::new(
            self.config.clone(),
            model,
            self.tools.clone(),
            self.budget.clone(),
            policy,
            events,
            options,
        );
        engine
            .run_with_system_context(
                ctx,
                sink,
                client,
                cancel,
                task,
                TurnSystemContext { memory, session: system_context },
            )
            .await
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
        // Fresh slice: strategic turns re-anchor the deadline too.
        self.budget
            .lock()
            .expect("budget tracker poisoned")
            .reset_deadline(self.config.turn_timeout);
        let prompt_text = ctx
            .prompt
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        let context_plan: Option<ContextPlan> = input
            .context
            .as_ref()
            .map(|context| ContextPlanner.plan(context, &ContextPlannerConfig::default()));
        // Transaction evidence is host-observed and immutable within this
        // strategic turn. Keep it separate from model/tool prose, then use it
        // to prevent unsupported verified completion claims below.
        let write_transaction = input.write_transaction.clone();
        let strategy_ctx = StrategyContext {
            prompt_text,
            has_code_changes: input.has_code_changes,
            validation_tools_available: input.validation_tools_available,
            delegation_allowed: self.policy().policy().allow_delegate,
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
        let policy = self.policy();
        let executor = StrategyExecutor::new(
            self.config.clone(),
            model,
            self.tools.clone(),
            self.budget.clone(),
            policy,
            self.tasks.clone(),
            events.clone(),
        );
        let (prompt_result, reflection) = executor
            .execute(decision.strategy, ctx, memory, context_plan.as_ref(), run, &mut validation)
            .await?;
        let log = execution_log.lock().expect("execution log poisoned").clone();
        let changed_files = changed_files_from_log(&log, &task.id);
        let progress = ProgressTracker::from_execution_log(
            &log,
            &validation,
            (reflection.review_calls > 0).then_some(reflection.findings.len()),
        )
        .score(&self.tasks.lock().expect("task graph poisoned").clone());
        let mut final_response = FinalResponseBuilder {
            changed_files,
            validation: &validation,
            // Host-provided evidence is optional; absence deliberately keeps
            // the final response unverified rather than fabricating facts.
            completion_evidence: input.completion_evidence.as_ref(),
            unresolved_risks: Vec::new(),
            follow_up_suggestions: Vec::new(),
            task_graph: &self.tasks.lock().expect("task graph poisoned").clone(),
            memory: &self.memory.lock().expect("memory store poisoned").clone(),
            progress: Some(&progress),
        }
        .build();
        if let Some(transaction) = &write_transaction {
            transaction.constrain_completion(&mut final_response.completion);
            final_response.can_finish = final_response.completion.is_verified();
        }
        Ok(TurnResult {
            context_plan,
            prompt_result,
            strategy: decision,
            final_response,
            write_transaction,
            reflection,
        })
    }

    /// Runs one `/compact` turn: deterministic memory compaction first, then
    /// one tool-free model call over a provenance-rich bounded context, then
    /// the model-derived summary stored as session memory (additive only —
    /// protected keys are never deleted by LLM output).
    pub async fn run_compact_turn(
        &self,
        _ctx: PromptContext,
        sink: UpdateSink,
        cancel: watch::Receiver<bool>,
        instructions: Option<String>,
        system_context: Option<String>,
    ) -> Result<PromptResult, OrchestratorError> {
        if *cancel.borrow() {
            return Err(OrchestratorError::Cancellation);
        }
        // 1. Deterministic memory compaction (merges duplicates, decays
        //    low-value observations; protected keys survive by construction).
        let deterministic = {
            let mut memory = self.memory.lock().expect("memory store poisoned");
            compact_memory(&mut memory, &self.config.compaction.memory)
        };
        // 2. Provenance-rich, byte-bounded compaction context from the task
        //    graph, memory, validation facts, and budget state.
        let (tasks, memory, budget_snapshot) = {
            let tasks = self.tasks.lock().expect("task graph poisoned");
            let memory = self.memory.lock().expect("memory store poisoned");
            let budget = self.budget.lock().expect("budget tracker poisoned");
            (tasks.clone(), memory.clone(), budget.snapshot())
        };
        let context = build_compaction_context(
            &tasks,
            &memory,
            &budget_snapshot,
            self.config.compaction.max_input_bytes,
        );
        // 3. One model call, no tools, bounded by the per-turn timeout and
        //    observing cancellation before and after.
        let mut system = build_compaction_prompt(instructions.as_deref());
        if !context.is_empty() {
            system.push_str("\n\nSession context:\n");
            system.push_str(&context);
        }
        if let Some(session) = system_context {
            system.push_str("\n\n");
            system.push_str(&session);
        }
        let user_prompt =
            instructions.as_deref().filter(|text| !text.trim().is_empty()).map_or_else(
                || "Compress the session into a continuation summary.".to_string(),
                str::to_string,
            );
        let transcript = vec![
            ModelMessage::text(ModelRole::System, system),
            ModelMessage::text(ModelRole::User, user_prompt),
        ];
        let task = TaskNode::new(
            TaskId::new("compact-session"),
            "compact session",
            "LLM session compaction summary",
        );
        let model = self.models.default_adapter()?;
        // Fresh slice: the compaction model call gets the full per-turn
        // timeout, regardless of how long the session idled.
        self.budget
            .lock()
            .expect("budget tracker poisoned")
            .reset_deadline(self.config.turn_timeout);
        // The compaction model call consumes budget like any other call;
        // budget exhaustion or token caps fail closed.
        self.budget.lock().expect("budget tracker poisoned").try_reserve_model_call()?;
        let request = ModelRequest::new(transcript, Vec::new(), budget_snapshot, task);
        let response = match tokio::time::timeout(
            self.config.turn_timeout,
            model.complete(request, cancel.clone()),
        )
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => return Err(error.into()),
            Err(_) => {
                return Err(OrchestratorError::Timeout(
                    "compaction model call exceeded the turn timeout".into(),
                ));
            }
        };
        self.budget.lock().expect("budget tracker poisoned").record_model_usage(
            response.text.len(),
            response.usage.input_tokens,
            response.usage.output_tokens,
        )?;
        // Context-window usage after the compaction model call; unknown
        // usage emits nothing.
        if let Some(input_tokens) = response.usage.input_tokens {
            let _ = sink.raw_update(SessionUpdate::UsageUpdate(UsageUpdate::new(
                input_tokens as u64,
                self.config.context_window_tokens,
            )));
        }
        if *cancel.borrow() {
            return Err(OrchestratorError::Cancellation);
        }
        let summary = response.text.trim();
        if summary.is_empty() {
            return Err(OrchestratorError::InvalidState(
                "compaction summary was empty; memory unchanged".into(),
            ));
        }
        // 4. Store the redacted summary as model-derived session memory;
        //    insertion is additive and never touches protected keys.
        let guard = SensitiveDataGuard::new();
        let item = MemoryItem::new(SESSION_SUMMARY_KEY, guard.redact(summary));
        let summary_bytes = item.byte_size();
        {
            let mut memory = self.memory.lock().expect("memory store poisoned");
            memory.insert(item).map_err(|error| {
                OrchestratorError::InvalidState(format!(
                    "failed to store compaction summary: {error}"
                ))
            })?;
        }
        let retained_context_bytes = context.len();
        let report = CompactTurnReport {
            merged_duplicates: deterministic.merged_duplicates,
            decayed_observations: deterministic.decayed_observations,
            preserved_protected: deterministic.preserved_protected,
            summary_bytes,
            retained_context_bytes,
        };
        let _ = sink.agent_message_chunk("compact-report", report.to_status_text());
        Ok(prompt_result_with_usage(ee_agent_protocol::StopReason::EndTurn, response.usage))
    }
}

/// Concatenates the text content blocks of a prompt.
fn prompt_text(ctx: &PromptContext) -> String {
    ctx.prompt
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
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
    use std::collections::VecDeque;
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
    async fn strategic_turn_uses_editor_context_before_terminal_evidence() {
        let model =
            Arc::new(FakeModel::new(vec![ModelResponse::new().text("implemented").completed()]));
        let runtime = OrchestratorRuntime::new(OrchestratorConfig::default(), model.clone());
        let (sink, client, _rx) = plumbing();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let fresh = crate::context_planner::ContextFreshness::fresh("rev-1");
        let context = crate::context_planner::ContextPlanningInput {
            identity: crate::context_planner::ContextPlanIdentity {
                session_id: "s-1".to_string(),
                policy_revision: "policy-1".to_string(),
                workspace_revision: "workspace-1".to_string(),
                buffer_revision: "buffer-1".to_string(),
                diagnostics_revision: "diagnostics-1".to_string(),
                graph_revision: "graph-1".to_string(),
                checkout_revision: "checkout-1".to_string(),
            },
            candidates: vec![
                crate::context_planner::ContextCandidate::new(
                    "terminal:1",
                    crate::context_planner::ContextSource::TerminalOutput,
                    crate::context_planner::ContextTrustClass::TerminalOutput,
                    fresh.clone(),
                    "terminal probe result",
                ),
                crate::context_planner::ContextCandidate::new(
                    "diagnostic:1",
                    crate::context_planner::ContextSource::Diagnostics,
                    crate::context_planner::ContextTrustClass::RepositoryContent,
                    fresh.clone(),
                    "type mismatch",
                ),
                crate::context_planner::ContextCandidate::new(
                    "buffer:src/lib.rs",
                    crate::context_planner::ContextSource::DirtyBuffer,
                    crate::context_planner::ContextTrustClass::RepositoryContent,
                    fresh,
                    "unsaved edit",
                ),
            ],
        };
        let turn = runtime
            .run_turn_strategic(
                prompt("fix the active file"),
                StrategicInput { context: Some(context), ..StrategicInput::default() },
                sink,
                client,
                cancel_rx,
            )
            .await
            .expect("strategic turn succeeds");

        let plan = turn.context_plan.expect("host context planned");
        assert_eq!(
            plan.items.iter().map(|item| item.source).collect::<Vec<_>>(),
            vec![
                crate::context_planner::ContextSource::DirtyBuffer,
                crate::context_planner::ContextSource::Diagnostics,
                crate::context_planner::ContextSource::TerminalOutput,
            ]
        );
        let requests = model.requests();
        assert_eq!(requests.len(), 1, "planned context does not trigger terminal probing");
        let context_messages = requests[0]
            .transcript
            .iter()
            .filter(|message| message.metadata.contains_key("context_source"))
            .collect::<Vec<_>>();
        assert_eq!(context_messages.len(), 3);
        assert_eq!(context_messages[0].metadata["context_source"], "dirty_buffer");
        assert_eq!(context_messages[1].metadata["context_source"], "diagnostics");
        assert_eq!(context_messages[2].metadata["context_source"], "terminal_output");
        assert!(context_messages.iter().all(|message| message.trust.is_untrusted()));
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

    // ── /compact LLM compaction turn ───────────────────────────────────────

    /// A runtime seeded with protected memory facts and a duplicated
    /// observation, so the deterministic pass has work to report.
    fn compact_runtime(config: OrchestratorConfig, model: Arc<FakeModel>) -> OrchestratorRuntime {
        let mut memory = MemoryStore::new(4_096);
        memory.insert(MemoryItem::new("decision:api", "use v2")).expect("inserts");
        memory
            .insert(MemoryItem::from_task("obs:file", "old read", TaskId::new("task-1")))
            .expect("inserts");
        memory
            .insert(MemoryItem::from_task("obs:file", "new read", TaskId::new("task-1")))
            .expect("inserts");
        memory.insert(MemoryItem::new("constraint:offline", "no network")).expect("inserts");
        memory.insert(MemoryItem::new("validation:tests", "all pass")).expect("inserts");
        let runtime = OrchestratorRuntime::with_state(
            config,
            model,
            crate::policy::PolicyEngine::default(),
            TaskGraph::new(),
            memory,
        );
        runtime.register_tool(echo_tool()).expect("registers echo");
        runtime
    }

    #[tokio::test]
    async fn compact_turn_preserves_protected_memory_and_invokes_no_tools() {
        let model =
            Arc::new(FakeModel::new(vec![ModelResponse::new().text("SUMMARY TEXT").completed()]));
        let runtime = compact_runtime(OrchestratorConfig::default(), model.clone());
        let (sink, client, mut rx) = plumbing();
        let events = EventRecorder::new();
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let result = runtime
            .run_turn_recording(prompt("/compact"), sink, client, cancel_rx, events.clone())
            .await
            .expect("compaction turn succeeds");
        assert_eq!(result.stop_reason, StopReason::EndTurn);

        // Exactly one model call, no tools, compaction prompt in system.
        let calls = model.requests();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].tools.is_empty(), "no tools may be exposed during compaction");
        let crate::model::ModelContent::Text(system) = &calls[0].transcript[0].content[0] else {
            panic!("expected system text");
        };
        assert!(system.contains("Write a compact continuation summary"), "{system}");

        // The summary is stored as model-derived session memory; protected
        // keys survive; the deterministic pass merged the duplicates.
        let memory = runtime.memory();
        assert_eq!(memory.query("summary:session").expect("stored").value, "SUMMARY TEXT");
        assert_eq!(memory.query("decision:api").expect("kept").value, "use v2");
        assert_eq!(memory.query("constraint:offline").expect("kept").value, "no network");
        assert_eq!(memory.query("validation:tests").expect("kept").value, "all pass");
        assert_eq!(memory.query("obs:file").expect("kept").value, "new read", "merged");
        assert_eq!(memory.query_prefix("obs:").len(), 1, "duplicates merged");

        // No tools ran, no plan was emitted, and the report message carries
        // the deterministic counts.
        let recorded = events.events();
        assert!(
            !recorded.iter().any(|event| matches!(event, OrchestratorEvent::ToolStarted { .. })),
            "no tool lifecycle events: {recorded:?}"
        );
        let report = next_update(&mut rx).await;
        let SessionUpdate::AgentMessageChunk(chunk) = report else {
            panic!("expected the compaction report message");
        };
        let ContentBlock::Text(text) = chunk.content else {
            panic!("expected text content");
        };
        assert!(text.text.contains("Session compacted:"), "{}", text.text);
        assert!(text.text.contains("merged 1 duplicate facts"), "{}", text.text);
        assert!(text.text.contains("preserved 3 protected items"), "{}", text.text);
        assert!(text.text.contains("stored 27 summary bytes"), "{}", text.text);
    }

    #[tokio::test]
    async fn compact_turn_context_is_byte_bounded() {
        let model =
            Arc::new(FakeModel::new(vec![ModelResponse::new().text("SUMMARY").completed()]));
        let mut memory = MemoryStore::new(4_096);
        for index in 0..50 {
            memory
                .insert(MemoryItem::new(format!("obs:{index}"), "v".repeat(20)))
                .expect("inserts");
        }
        let config = OrchestratorConfig {
            compaction: crate::compaction::CompactionConfig {
                max_input_bytes: 300,
                ..crate::compaction::CompactionConfig::default()
            },
            ..OrchestratorConfig::default()
        };
        let runtime = OrchestratorRuntime::with_state(
            config,
            model.clone(),
            crate::policy::PolicyEngine::default(),
            TaskGraph::new(),
            memory,
        );
        let (sink, client, _rx) = plumbing();
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        runtime
            .run_turn(prompt("/compact"), sink, client, cancel_rx)
            .await
            .expect("compaction turn succeeds");

        let calls = model.requests();
        assert_eq!(calls.len(), 1);
        let crate::model::ModelContent::Text(system) = &calls[0].transcript[0].content[0] else {
            panic!("expected system text");
        };
        // The compaction context itself is bounded to 300 bytes; the prompt
        // text is separate, so the system message stays well under 1 KiB.
        assert!(system.len() < 1_024, "system message bounded: {} bytes", system.len());
        assert!(system.contains("obs:49"), "newest memory retained: {system}");
        assert!(!system.contains("obs:0"), "oldest memory dropped for the bound: {system}");
    }

    #[tokio::test]
    async fn compact_turn_rejects_empty_summary_without_memory_changes() {
        let model = Arc::new(FakeModel::new(vec![ModelResponse::new().completed()]));
        let runtime = compact_runtime(OrchestratorConfig::default(), model.clone());
        let (sink, client, _rx) = plumbing();
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let error = runtime
            .run_turn(prompt("/compact"), sink, client, cancel_rx)
            .await
            .expect_err("empty summaries reject");
        assert!(
            matches!(&error, OrchestratorError::InvalidState(reason)
                if reason.contains("compaction summary was empty")),
            "{error}"
        );
        let memory = runtime.memory();
        assert!(memory.query("summary:session").is_none(), "no summary stored");
        assert!(memory.query("decision:api").is_some(), "protected keys untouched");
        assert_eq!(model.call_count(), 1);
    }

    #[tokio::test]
    async fn compact_turn_respects_cancellation_before_model_call() {
        let model =
            Arc::new(FakeModel::new(vec![ModelResponse::new().text("SUMMARY").completed()]));
        let runtime = compact_runtime(OrchestratorConfig::default(), model.clone());
        let (sink, client, _rx) = plumbing();
        let (_cancel_tx, cancel_rx) = watch::channel(true);

        let error = runtime
            .run_turn(prompt("/compact"), sink, client, cancel_rx)
            .await
            .expect_err("pre-cancelled compaction stops");
        assert_eq!(error, OrchestratorError::Cancellation);
        assert_eq!(model.call_count(), 0, "no model call after cancellation");
        assert!(runtime.memory().query("summary:session").is_none());
    }

    #[tokio::test]
    async fn compact_prefix_collision_runs_the_normal_loop() {
        let model =
            Arc::new(FakeModel::new(vec![ModelResponse::new().text("normal answer").completed()]));
        let runtime = compact_runtime(OrchestratorConfig::default(), model.clone());
        let (sink, client, _rx) = plumbing();
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let result = runtime
            .run_turn(prompt("/compactness"), sink, client, cancel_rx)
            .await
            .expect("prefix collisions are ordinary prompts");
        assert_eq!(result.stop_reason, StopReason::EndTurn);

        let calls = model.requests();
        assert_eq!(calls.len(), 1);
        // The loop prepends a system message with memory facts; the user
        // message carries the original prompt verbatim.
        assert_eq!(calls[0].transcript[0].role, ModelRole::System);
        assert_eq!(calls[0].transcript[1].role, ModelRole::User);
        assert_eq!(
            calls[0].transcript[1].content,
            vec![crate::model::ModelContent::Text("/compactness".into())]
        );
        assert!(!calls[0].tools.is_empty(), "normal loop advertises tools");
        assert_eq!(runtime.tasks().len(), 1, "root task created by the normal loop");
        assert!(runtime.memory().query("summary:session").is_none());
    }

    // ── Recovery: resumable turn interruptions ───────────────────────────

    /// Model that parks on the virtual clock before answering, with per-call
    /// delays: a long first delay lets the outer turn timeout fire
    /// deterministically, fast later delays let resumed slices complete.
    /// Cancellation is observed after the park.
    #[derive(Clone)]
    struct DelayedModel {
        delays: Arc<Mutex<VecDeque<Duration>>>,
        default_delay: Duration,
        inner: FakeModel,
    }

    impl DelayedModel {
        fn new(delays: Vec<Duration>, default_delay: Duration, inner: FakeModel) -> Self {
            Self { delays: Arc::new(Mutex::new(delays.into())), default_delay, inner }
        }
    }

    impl ModelAdapter for DelayedModel {
        fn complete(
            &self,
            request: crate::model::ModelRequest,
            cancel: watch::Receiver<bool>,
        ) -> crate::model::ModelFuture<Result<crate::model::ModelResponse, crate::model::ModelError>>
        {
            let delay = self
                .delays
                .lock()
                .expect("delays poisoned")
                .pop_front()
                .unwrap_or(self.default_delay);
            let inner = self.inner.clone();
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                if *cancel.borrow() {
                    return Err(crate::model::ModelError::Cancelled);
                }
                inner.complete(request, cancel).await
            })
        }
    }

    fn recovery_config(turn_timeout: Duration) -> OrchestratorConfig {
        OrchestratorConfig {
            turn_timeout,
            recovery: crate::config::RecoveryConfig {
                enabled: true,
                ..crate::config::RecoveryConfig::default()
            },
            // Scripted text-only responses make no task-graph progress;
            // disable the no-progress rule for these tests.
            stuck: crate::stuck::StuckConfig {
                max_no_progress_iterations: 100,
                ..crate::stuck::StuckConfig::default()
            },
            ..OrchestratorConfig::default()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_stop_becomes_interrupted_outcome_with_durable_checkpoint() {
        // The first model call parks past the slice; the outer turn timeout
        // fires deterministically under the paused clock.
        let model = Arc::new(DelayedModel::new(
            vec![Duration::from_millis(5_000)],
            Duration::ZERO,
            FakeModel::new(vec![
                ModelResponse::new().text("one"),
                ModelResponse::new().text("two"),
            ]),
        ));
        let runtime =
            OrchestratorRuntime::new(recovery_config(Duration::from_millis(40)), model.clone());
        let (sink, client, _rx) = plumbing();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let outcome = tokio::join!(
            runtime.run_turn_recoverable(
                prompt("hello"),
                sink,
                client,
                cancel_rx,
                String::new(),
                "test-provider",
            ),
            async {
                tokio::time::sleep(Duration::from_millis(5)).await;
                tokio::time::advance(Duration::from_millis(500)).await;
            },
        )
        .0;
        let interruption = match outcome.expect("recoverable run returns an outcome") {
            TurnOutcome::Interrupted(interruption) => interruption,
            other => panic!("expected interruption, got {other:?}"),
        };
        assert_eq!(interruption.fault, ee_agent_protocol::RecoverableFault::Deadline);
        assert!(interruption.safe_resume, "no in-flight tool; resuming is safe");
        assert_eq!(interruption.resumed_count, 0);
        assert!(interruption.checkpoint_id.is_some(), "checkpoint persisted");

        let store = runtime.checkpoint_store();
        let (id, checkpoint) = store.load_latest("s-1").expect("loads").expect("pending");
        assert_eq!(id, interruption.checkpoint_id.unwrap());
        assert_eq!(checkpoint.provider, "test-provider");
        let resume = checkpoint.resume.expect("resume state captured");
        assert!(
            resume.transcript.iter().any(|message| message.role == ModelRole::User),
            "transcript tail carries the user turn"
        );
        assert!(runtime.event_snapshot().iter().any(|event| {
            matches!(
                event,
                OrchestratorEvent::TurnInterrupted { fault, .. } if fault == "deadline"
            )
        }));
    }

    #[tokio::test(start_paused = true)]
    async fn resumed_turn_completes_without_new_root_and_retains_counters() {
        let model = Arc::new(DelayedModel::new(
            vec![Duration::from_millis(5_000)],
            Duration::ZERO,
            FakeModel::new(vec![
                ModelResponse::new().text("one"),
                ModelResponse::new().text("two"),
                ModelResponse::new().text("three"),
                ModelResponse::new().text("four"),
                ModelResponse::new().text("five"),
                ModelResponse::new().text("six").completed(),
            ]),
        ));
        let runtime =
            OrchestratorRuntime::new(recovery_config(Duration::from_millis(40)), model.clone());
        let (sink, client, _rx) = plumbing();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let outcome = tokio::join!(
            runtime.run_turn_recoverable(
                prompt("hello"),
                sink,
                client,
                cancel_rx,
                String::new(),
                "test-provider",
            ),
            async {
                tokio::time::sleep(Duration::from_millis(5)).await;
                tokio::time::advance(Duration::from_millis(500)).await;
            },
        )
        .0;
        assert!(
            matches!(outcome, Ok(TurnOutcome::Interrupted(_))),
            "first slice interrupts, got {outcome:?}"
        );
        assert_eq!(runtime.tasks().len(), 1, "one root task so far");
        assert_eq!(model.inner.call_count(), 0, "hung slice consumes no scripted responses");

        // Resume: same session, same prompt; the checkpoint is consumed.
        // The clock only advances enough to fire the scripted 1 ms model
        // sleeps; a 500 ms jump would land past the fresh slice deadline.
        let (sink, client, _rx) = plumbing();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let outcome = tokio::join!(
            runtime.resume_turn(
                prompt("hello"),
                sink,
                client,
                cancel_rx,
                String::new(),
                "test-provider",
            ),
            async {
                tokio::time::sleep(Duration::from_millis(1)).await;
                tokio::time::advance(Duration::from_millis(10)).await;
            },
        )
        .0;
        match outcome.expect("resume returns an outcome") {
            TurnOutcome::Completed(result) => {
                assert_eq!(result.stop_reason, StopReason::EndTurn);
            }
            other => panic!("resumed turn should complete, got {other:?}"),
        }
        // One root task survives; no second root was created by the resume.
        assert_eq!(runtime.tasks().len(), 1, "resume reuses the existing root task");
        // Cumulative counters retained: slice 1 reserved one model call
        // before its hang; the resume consumed the six scripted responses.
        assert_eq!(runtime.budget_snapshot().model_calls_used, 7);
        assert_eq!(model.inner.call_count(), 6);
        // Completed turns clear pending checkpoints.
        assert!(runtime.checkpoint_store().load_latest("s-1").expect("loads").is_none());
        assert!(runtime.event_snapshot().iter().any(|event| {
            matches!(event, OrchestratorEvent::TurnResumed { checkpoint_id, .. } if !checkpoint_id.is_empty())
        }));
    }

    #[tokio::test(start_paused = true)]
    async fn resumed_turn_never_replays_identical_write_calls() {
        let write = || ToolIntent::new("tc-1", "write_file", json!({ "path": "/tmp/x" }));
        let tool = Arc::new(FakeTool::new(
            ToolDefinition::new("write_file", "writes a file")
                .side_effect_class(SideEffectClass::Write),
            ToolResult::success("written"),
        ));
        // Slice 1: one instant write, then a hang that trips the outer
        // timeout.  Slice 2 (resume): the model asks for the identical write
        // again, then completes.
        let model = Arc::new(DelayedModel::new(
            vec![Duration::ZERO, Duration::from_millis(5_000)],
            Duration::ZERO,
            FakeModel::new(vec![
                ModelResponse::new().text("write now").tool_intents(vec![write()]),
                ModelResponse::new().text("one"),
                ModelResponse::new().text("write again").tool_intents(vec![write()]),
                ModelResponse::new().text("done").completed(),
            ]),
        ));
        let runtime = OrchestratorRuntime::with_policy(
            recovery_config(Duration::from_millis(40)),
            model,
            PolicyEngine::new(ToolPolicy { allow_write: true, ..ToolPolicy::default() }),
        );
        runtime.register_tool(tool.clone()).expect("registers write_file");
        let (sink, client, _rx) = plumbing();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let outcome = tokio::join!(
            runtime.run_turn_recoverable(
                prompt("hello"),
                sink,
                client,
                cancel_rx,
                String::new(),
                "test-provider",
            ),
            async {
                tokio::time::sleep(Duration::from_millis(5)).await;
                tokio::time::advance(Duration::from_millis(500)).await;
            },
        )
        .0;
        assert!(
            matches!(outcome, Ok(TurnOutcome::Interrupted(_))),
            "first slice interrupts, got {outcome:?}"
        );
        assert_eq!(tool.call_count(), 1, "the first slice executed the write once");

        let (sink, client, _rx) = plumbing();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let outcome = tokio::join!(
            runtime.resume_turn(
                prompt("hello"),
                sink,
                client,
                cancel_rx,
                String::new(),
                "test-provider",
            ),
            async {
                tokio::time::sleep(Duration::from_millis(1)).await;
                tokio::time::advance(Duration::from_millis(50)).await;
            },
        )
        .0;
        assert!(
            matches!(outcome, Ok(TurnOutcome::Completed(_))),
            "resumed turn completes, got {outcome:?}"
        );
        assert_eq!(
            tool.call_count(),
            1,
            "identical write call reused from the checkpoint, never replayed"
        );
        assert!(runtime.event_snapshot().iter().any(|event| {
            matches!(
                event,
                OrchestratorEvent::ToolResultReused { tool_name, .. } if tool_name == "write_file"
            )
        }));
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_clears_pending_checkpoints() {
        // The model parks briefly so the bumper can flip the cancel watch
        // before the slice's outer timeout fires.
        let model = Arc::new(DelayedModel::new(
            vec![Duration::from_millis(10)],
            Duration::from_millis(10),
            FakeModel::new(vec![ModelResponse::new().text("one")]),
        ));
        // A long slice: cancellation (not the outer timeout) must end the
        // turn deterministically.
        let runtime = OrchestratorRuntime::new(recovery_config(Duration::from_secs(10)), model);
        let (sink, client, _rx) = plumbing();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let outcome = tokio::join!(
            runtime.run_turn_recoverable(
                prompt("hello"),
                sink,
                client,
                cancel_rx,
                String::new(),
                "test-provider",
            ),
            async {
                tokio::time::sleep(Duration::from_millis(1)).await;
                cancel_tx.send(true).expect("cancels");
                tokio::time::advance(Duration::from_millis(20)).await;
            },
        )
        .0;
        assert!(matches!(outcome, Err(OrchestratorError::Cancellation)), "got {outcome:?}");
        assert!(
            runtime.checkpoint_store().load_latest("s-1").expect("loads").is_none(),
            "a cancelled turn never leaves a stale pending checkpoint"
        );
    }

    #[tokio::test]
    async fn completed_turn_clears_pending_checkpoints() {
        let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("done").completed()]));
        let runtime = OrchestratorRuntime::new(recovery_config(Duration::from_secs(30)), model);
        let (sink, client, _rx) = plumbing();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let outcome = runtime
            .run_turn_recoverable(
                prompt("hello"),
                sink,
                client,
                cancel_rx,
                String::new(),
                "test-provider",
            )
            .await
            .expect("outcome");
        assert!(matches!(outcome, TurnOutcome::Completed(_)));
        assert!(
            runtime.checkpoint_store().load_latest("s-1").expect("loads").is_none(),
            "completed turns clear milestone checkpoints"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_is_reanchored_per_turn_not_per_session() {
        // A runtime may idle between turns for longer than one timeout; the
        // next turn must get a fresh slice instead of failing instantly on
        // the session-creation deadline.
        let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("done").completed()]));
        let runtime = OrchestratorRuntime::new(recovery_config(Duration::from_secs(30)), model);
        tokio::time::advance(Duration::from_secs(120)).await;
        let (sink, client, _rx) = plumbing();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let outcome = runtime
            .run_turn_recoverable(
                prompt("hello"),
                sink,
                client,
                cancel_rx,
                String::new(),
                "test-provider",
            )
            .await
            .expect("outcome");
        assert!(
            matches!(outcome, TurnOutcome::Completed(_)),
            "idle time must not consume the next turn's slice: {outcome:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn resume_rejects_provider_mismatch_and_expired_sessions() {
        let model = Arc::new(DelayedModel::new(
            vec![Duration::from_millis(5_000)],
            Duration::ZERO,
            FakeModel::new(vec![ModelResponse::new().text("one")]),
        ));
        let runtime = OrchestratorRuntime::new(recovery_config(Duration::from_millis(40)), model);
        let (sink, client, _rx) = plumbing();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let outcome = tokio::join!(
            runtime.run_turn_recoverable(
                prompt("hello"),
                sink,
                client,
                cancel_rx,
                String::new(),
                "test-provider",
            ),
            async {
                tokio::time::sleep(Duration::from_millis(5)).await;
                tokio::time::advance(Duration::from_millis(500)).await;
            },
        )
        .0;
        assert!(matches!(outcome, Ok(TurnOutcome::Interrupted(_))));

        // A different provider identity must never restore the checkpoint.
        let (sink, client, _rx) = plumbing();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let error = runtime
            .resume_turn(prompt("hello"), sink, client, cancel_rx, String::new(), "other-provider")
            .await
            .expect_err("provider mismatch rejects restore");
        assert!(
            matches!(error, OrchestratorError::PolicyDenied(ref reason) if reason.contains("provider")),
            "{error}"
        );

        // A checkpoint beyond the cumulative session cap is discarded.
        let runtime = OrchestratorRuntime::new(
            OrchestratorConfig {
                turn_timeout: Duration::from_millis(40),
                recovery: crate::config::RecoveryConfig {
                    enabled: true,
                    session_timeout: Some(Duration::from_secs(1)),
                    ..crate::config::RecoveryConfig::default()
                },
                ..OrchestratorConfig::default()
            },
            Arc::new(DelayedModel::new(
                vec![Duration::from_millis(5_000)],
                Duration::ZERO,
                FakeModel::new(vec![ModelResponse::new().text("one")]),
            )),
        );
        let (sink, client, _rx) = plumbing();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let outcome = tokio::join!(
            runtime.run_turn_recoverable(
                prompt("hello"),
                sink,
                client,
                cancel_rx,
                String::new(),
                "test-provider",
            ),
            async {
                tokio::time::sleep(Duration::from_millis(5)).await;
                tokio::time::advance(Duration::from_millis(500)).await;
            },
        )
        .0;
        assert!(matches!(outcome, Ok(TurnOutcome::Interrupted(_))));
        // Age the checkpoint past the one-second session cap.
        {
            let store = runtime.checkpoint_store();
            let (_, mut checkpoint) = store.load_latest("s-1").expect("loads").expect("pending");
            checkpoint.created_at_millis = crate::checkpoint::current_unix_millis();
            if let Some(resume) = checkpoint.resume.as_mut() {
                resume.first_started_at_millis =
                    crate::checkpoint::current_unix_millis().saturating_sub(2_000);
            }
            store.save("s-1", &checkpoint).expect("rewrites aged checkpoint");
        }
        let (sink, client, _rx) = plumbing();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let error = runtime
            .resume_turn(prompt("hello"), sink, client, cancel_rx, String::new(), "test-provider")
            .await
            .expect_err("session cap rejects resume");
        assert!(error.to_string().contains("cumulative timeout"), "{error}");
        assert!(
            runtime.checkpoint_store().load_latest("s-1").expect("loads").is_none(),
            "expired checkpoint discarded"
        );
    }
}
