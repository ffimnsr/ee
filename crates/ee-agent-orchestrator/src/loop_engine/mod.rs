//! The bounded model → tool loop.
//!
//! Builds the initial transcript from the prompt and seeded memory facts,
//! calls the model adapter once per iteration (after reserving the
//! iteration/model-call/output-budget allowance and checking the wall-clock
//! deadline), streams reasoning and text through the update sink, hands tool
//! intents to the [`ToolExecutor`] (which owns the policy gate, budget
//! reservation, update lifecycle, and timeout), appends observations to the
//! transcript, and stops deterministically on the model's completion signal,
//! on two consecutive empty responses, on cancellation, on budget
//! exhaustion, or when the per-turn timeout elapses.  Subagent intents fail
//! closed until the subagent phase.  Every loop decision is recorded as an
//! [`OrchestratorEvent`] so tests can assert the exact decision sequence.
//!
//! Tool dispatch intentionally differs from the reference agent: `ee` runs
//! read-only tools in parallel up to [`crate::config::OrchestratorConfig::max_parallel_tools`]
//! and everything else sequentially, while the reference dispatches with
//! unbounded concurrency behind its permission gate. Transcript normalization
//! ([`crate::transcript_normalize`]) therefore reasons only about call/result
//! correlation, never about dispatch order.

use std::{
    collections::HashSet,
    path::Path,
    sync::{Arc, Mutex},
};

use ee_agent_acp_server::{ClientBridge, PromptContext, PromptResult, SteerInbox, UpdateSink};
use ee_agent_protocol::{SessionId, SessionInfoUpdate, SessionUpdate, StopReason, UsageUpdate};
use tokio::sync::watch;

use crate::budget::BudgetTracker;
use crate::checkpoint::{
    CheckpointCaptureOrigin, CompletedToolCall, IdGeneratorState, InFlightOperation,
    OrchestratorCheckpoint, ResumeState, SubagentTreeState, TranscriptSummary, current_unix_millis,
    tool_call_fingerprint,
};
use crate::checkpoint_store::CheckpointHandle;
use crate::config::OrchestratorConfig;
use crate::error::OrchestratorError;
use crate::events::{EventRecorder, OrchestratorEvent};
use crate::loop_control::{MAX_STEPS_PROMPT, validate_finish_reason};
use crate::memory::MemoryStore;
use crate::model::{
    ModelAdapter, ModelMessage, ModelRequest, ModelResponse, ModelRole, ModelUsage, Transcript,
    prompt_result_with_usage,
};
use crate::model_registry::ModelInfo;
use crate::parallel_tools::ParallelToolRunner;
use crate::prompt::context::TurnSystemContext;
use crate::prompt::repeat_guard::{RepeatGuard, RepeatGuardConfig};
use crate::session_context::SessionContext;
use crate::session_log::SessionLog;
use crate::stop_gate::{StopGate, StopGateConfig, StopGateOutcome, StopVeto};
use crate::streaming::run_streaming_response;
use crate::stuck::StuckDetector;
use crate::tasks::{TaskGraph, TaskNode};
use crate::tools::{
    SideEffectClass, ToolDefinition, ToolExecutionLogEntry, ToolExecutor, ToolIntent, ToolRegistry,
    ToolResult,
};

mod after_turn;
mod turn;
mod util;

#[cfg(test)]
mod tests;

/// Notice appended to the transcript when a reply is cut by the output bound.
pub(crate) const CONTINUE_AFTER_OUTPUT_LIMIT: &str = "Your last response hit the output token limit (stop reason: length). Do not apologize, \
     recap, or repeat yourself. Break the remaining work into smaller pieces.";

/// Maximum automatic continuations after a length-truncated reply in one turn.
#[cfg(test)]
pub(crate) const MAX_OUTPUT_CONTINUATIONS: usize = crate::config::DEFAULT_MAX_OUTPUT_CONTINUATIONS;

/// How the loop engine executes one turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoopMode {
    /// Standard bounded model → tool loop.
    Standard,
    /// Exactly one model call; tool intents fail closed; the turn always ends
    /// after the single call (strategy `SimpleAnswer`).
    SimpleAnswer,
}

/// Behavior knobs for one loop run.
#[derive(Debug, Clone)]
pub(crate) struct LoopOptions {
    /// Subagent nesting depth of this run (root is 0).
    pub depth: usize,
    /// Execution mode.
    pub mode: LoopMode,
    /// Whether tool intents execute read-class tools before the rest.
    pub read_first: bool,
    /// Optional per-turn execution log for final-response assembly.
    pub execution_log: Option<Arc<Mutex<Vec<ToolExecutionLogEntry>>>>,
    /// Optional owning task store feeding the stuck detector's no-progress
    /// tracking.
    pub graph: Option<Arc<Mutex<TaskGraph>>>,
    /// Optional session todo plan; after each successful `todowrite` call the
    /// loop emits an ACP plan update from it.
    pub plan: Option<Arc<Mutex<crate::todo_plan::TodoPlan>>>,
    /// Optional pending session title; after each successful `session_rename`
    /// call the loop emits an ACP `session_info_update` carrying it.
    pub session_title: Option<Arc<Mutex<Option<String>>>>,
    /// Advertised model list handed to the delegating model so subagent
    /// selections can name a registry id; empty when no registry is wired.
    pub available_models: Vec<ModelInfo>,
    /// Registry id of the adapter this loop runs on (diagnostic and used as
    /// the delegation fallback).
    pub model_id: Option<String>,
    /// Exact model-visible tool names. `None` exposes normal registry surface;
    /// a critic supplies a filtered set mirrored by execution policy.
    pub visible_tool_names: Option<HashSet<String>>,
    /// When true the loop advertises read-class tools only. Plan mode sets it
    /// so the model never sees a mutation the mode policy would deny.
    pub read_only_tools: bool,
    /// Optional memory store for checkpoint captures; required (along with
    /// `checkpoint`) for recovery.
    pub memory: Option<Arc<Mutex<MemoryStore>>>,
    /// Optional durable checkpoint writer; `None` disables recovery captures.
    pub checkpoint: Option<CheckpointHandle>,
    /// Incoming resume state when this run continues an interrupted turn;
    /// completed tool results are reused instead of replayed.
    pub resume_state: Option<ResumeState>,
    /// Set when transcript truncation removed the retained runtime base.
    pub truncation_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// Append-only model-visible session log; `None` disables recording.
    pub session_log: Option<Arc<SessionLog>>,
    /// Evidence gate applied before a changed turn may stop.
    pub stop_gate: Option<StopGateConfig>,
    /// Optional plugin/host stop veto; can only deny an allowed stop.
    pub stop_veto: Option<StopVeto>,
    /// Receives the normalized transcript of this run after the loop ends, so
    /// the provider boundary can retain full multi-turn history.
    pub transcript_out: Option<Arc<Mutex<Vec<ModelMessage>>>>,
    /// Mid-turn nested workspace instruction discovery. `None` disables it;
    /// when set, tool access below `cwd` injects unseen nested `AGENTS.md` and
    /// `CLAUDE.md` files once per session.
    pub workspace_instructions:
        Option<Arc<Mutex<crate::prompt::workspace::WorkspaceInstructionTracker>>>,
    /// Background terminal jobs polled between model steps; `None` disables
    /// job tracking and exit notices.
    pub background_jobs: Option<crate::background_jobs::SharedBackgroundJobs>,
    /// Path remap for a git-worktree-isolated child. `None` edits the primary
    /// checkout directly.
    pub workspace_remap: Option<Arc<crate::workspace_isolation::WorkspacePathRemap>>,
    /// Runtime decision log for approval-classifier audit entries.
    pub decisions: Option<Arc<Mutex<crate::decision_log::DecisionLog>>>,
    /// Opt-in approval risk classifier; `None` leaves host approval unchanged.
    pub approval_classifier: Option<Arc<crate::approval_classifier::ApprovalClassifier>>,
    /// Optional loop stop predicate, evaluated after each step with the steps so
    /// far. Returning `true` ends the turn with the normal final response
    /// instead of relying on an iteration cap.
    pub until: Option<UntilPredicate>,
    /// Structured-output request; the hidden forced tool is added per step.
    pub response_format: Option<crate::model_options::ResponseFormat>,
    /// Provider/project/client affinity extras. The loop stamps the active
    /// session id; providers that know a parent or project id seed it here.
    pub session_context: Option<SessionContext>,
    /// Runtime metrics sink for per-model usage aggregation; `None` disables it.
    pub metrics: Option<Arc<Mutex<crate::metrics::OrchestratorMetrics>>>,
    /// Opaque checkpoint from a prior provider-native compaction. When set it
    /// is replayed as the first transcript item so the provider resumes from
    /// the conversation it already compacted.
    pub compaction_checkpoint: Option<crate::compaction_checkpoint::CompactionCheckpoint>,
}

pub use crate::loop_hooks::{UntilFn, UntilPredicate};

impl Default for LoopOptions {
    fn default() -> Self {
        Self {
            depth: 0,
            mode: LoopMode::Standard,
            read_first: false,
            execution_log: None,
            graph: None,
            plan: None,
            session_title: None,
            available_models: Vec::new(),
            model_id: None,
            visible_tool_names: None,
            read_only_tools: false,
            memory: None,
            checkpoint: None,
            resume_state: None,
            truncation_flag: None,
            session_log: None,
            stop_gate: None,
            stop_veto: None,
            transcript_out: None,
            workspace_instructions: None,
            background_jobs: None,
            workspace_remap: None,
            decisions: None,
            approval_classifier: None,
            until: None,
            response_format: None,
            session_context: None,
            metrics: None,
            compaction_checkpoint: None,
        }
    }
}

/// Internal loop engine; the runtime owns the stores and builds one engine
/// per turn.  Visibility widens when the loop gains its public contract.
pub(crate) struct LoopEngine {
    config: OrchestratorConfig,
    model: Arc<dyn ModelAdapter>,
    tools: Arc<Mutex<ToolRegistry>>,
    budget: Arc<Mutex<BudgetTracker>>,
    executor: ToolExecutor,
    events: EventRecorder,
    options: LoopOptions,
    graph: Option<Arc<Mutex<TaskGraph>>>,
    memory: Option<Arc<Mutex<MemoryStore>>>,
    checkpoint: Option<CheckpointHandle>,
    resume_state: Option<ResumeState>,
}

/// Borrowed state for the final text-only call at the iteration cap.
struct MaxStepsCall<'a> {
    transcript: &'a mut Transcript,
    sink: &'a UpdateSink,
    cancel: &'a watch::Receiver<bool>,
    task: &'a TaskNode,
    session: &'a SessionContext,
    turn_usage: ModelUsage,
    session_id: &'a str,
    max_iterations: usize,
}

impl LoopEngine {
    /// Creates an engine sharing the runtime's stores, evaluating policy at
    /// the given subagent depth (via `options`), and recording every loop
    /// decision into `events`.  `options.graph` feeds the stuck detector's
    /// no-progress tracking; pass the owning task store when one exists.
    pub(crate) fn new(
        config: OrchestratorConfig,
        model: Arc<dyn ModelAdapter>,
        tools: Arc<Mutex<ToolRegistry>>,
        budget: Arc<Mutex<BudgetTracker>>,
        policy: crate::policy::PolicyEngine,
        events: EventRecorder,
        options: LoopOptions,
    ) -> Self {
        let executor = ToolExecutor::new(
            config.clone(),
            tools.clone(),
            budget.clone(),
            policy,
            options.depth,
            events.clone(),
        )
        .with_model_id(options.model_id.clone());
        let executor = executor.with_background_jobs(options.background_jobs.clone());
        let executor = executor.with_workspace_remap(options.workspace_remap.clone());
        let executor = executor
            .with_approval_classifier(options.approval_classifier.clone())
            .with_decisions(options.decisions.clone());
        let graph = options.graph.clone();
        let memory = options.memory.clone();
        let checkpoint = options.checkpoint.clone();
        let resume_state = options.resume_state.clone();
        Self {
            config,
            model,
            tools,
            budget,
            executor,
            events,
            options,
            graph,
            memory,
            checkpoint,
            resume_state,
        }
    }

    /// Runs one turn for `task`, bounded by `turn_timeout`, seeding the
    /// transcript with the given compact memory context (when any).
    #[cfg(test)]
    pub(crate) async fn run(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        task: TaskNode,
        memory: Option<String>,
    ) -> Result<PromptResult, OrchestratorError> {
        self.run_with_system_context(
            ctx,
            sink,
            client,
            cancel,
            task,
            TurnSystemContext { memory, ..TurnSystemContext::default() },
        )
        .await
    }

    /// Runs one prompt after prepending bounded system context and compact
    /// memory facts. System context is first so workspace/path rules survive as
    /// the highest-priority session facts.
    pub(crate) async fn run_with_system_context(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        task: TaskNode,
        context: TurnSystemContext,
    ) -> Result<PromptResult, OrchestratorError> {
        let mut transcript = Transcript::new();
        let steer = ctx.steer.clone();
        context.apply_to_transcript(&mut transcript, Transcript::from_prompt(&ctx).messages);
        self.run_transcript(
            &mut transcript,
            ctx.session_id.to_string(),
            sink,
            client,
            cancel,
            task,
            steer,
        )
        .await
    }

    /// Runs a turn over a prebuilt transcript, bounded by `turn_timeout`,
    /// returning the transcript so callers (e.g. subagents) can extract the
    /// final assistant summary.
    ///
    /// The transcript is borrowed, so it is available for checkpoint
    /// captures even when the turn fails (deadline, timeout, cancellation,
    /// or an ordinary error).  When recovery is wired, deadline and timeout
    /// stops persist a recovery checkpoint before the error is returned.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_transcript(
        &self,
        transcript: &mut Transcript,
        session_id: String,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        task: TaskNode,
        steer: Option<SteerInbox>,
    ) -> Result<PromptResult, OrchestratorError> {
        // Per-turn mutable recovery state: in-flight marker, completed tools,
        // and the resume counters.  Shared with the loop so the interruption
        // capture (which runs after the loop future is dropped on timeout)
        // sees the exact final state.
        let track = Arc::new(Mutex::new(ResumeState {
            transcript: Vec::new(),
            active_task_id: task.id.as_str().to_string(),
            completed_tools: self
                .resume_state
                .as_ref()
                .map_or_else(Vec::new, |resume| resume.completed_tools.clone()),
            in_flight: self.resume_state.as_ref().and_then(|resume| resume.in_flight.clone()),
            resumed_count: self.resume_state.as_ref().map_or(0, |resume| resume.resumed_count),
            first_started_at_millis: self
                .resume_state
                .as_ref()
                .map_or_else(current_unix_millis, |resume| resume.first_started_at_millis),
        }));
        // Attach the durable log before any message is appended so the
        // bounded request view and the append-only log stay in lockstep.
        if let Some(log) = &self.options.session_log {
            transcript.attach_session_log(log.clone());
        }
        transcript.set_tool_output_limits(self.config.tool_output);
        // A provider-native checkpoint replaces the conversation prefix the
        // provider already compacted, so it is replayed first on every step.
        if let Some(checkpoint) = self.options.compaction_checkpoint.as_ref() {
            transcript.ensure_compaction_checkpoint(checkpoint);
        }
        let turn = self.run_loop(
            transcript,
            track.clone(),
            session_id.clone(),
            sink,
            client,
            cancel,
            task.clone(),
            steer,
        );
        let outcome = if let Some(limit) = self.config.turn_timeout {
            match tokio::time::timeout(limit, turn).await {
                Ok(outcome) => outcome,
                Err(_elapsed) => {
                    self.events.record(OrchestratorEvent::Error {
                        error: format!("turn exceeded configured turn_timeout ({limit:?})"),
                    });
                    self.events
                        .record(OrchestratorEvent::TurnStopped { stop_reason: "timeout".into() });
                    Err(OrchestratorError::Timeout(format!(
                        "turn exceeded configured turn_timeout ({limit:?})"
                    )))
                }
            }
        } else {
            turn.await
        };
        // Deadline and timeout stops persist a recovery checkpoint (the
        // in-flight marker rides along when a tool was mid-execution).
        // Cancellation and ordinary errors do not capture: a cancelled turn
        // must never leave a stale pending checkpoint behind.
        if self.checkpoint.is_some()
            && matches!(
                outcome,
                Err(OrchestratorError::DeadlineExceeded(_)) | Err(OrchestratorError::Timeout(_))
            )
        {
            self.capture_checkpoint(transcript, &track, &task, true).await;
        }
        // A truncated transcript may have dropped the retained full runtime
        // snapshot; the next turn must resend it in full.
        if transcript.truncation().is_some()
            && let Some(flag) = &self.options.truncation_flag
        {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        // Hand the normalized transcript to the provider boundary even on a
        // failed turn, so new messages are not lost before the next capture.
        if let Some(out) = &self.options.transcript_out {
            *out.lock().expect("transcript output poisoned") = transcript.messages().to_vec();
        }
        outcome
    }
}
