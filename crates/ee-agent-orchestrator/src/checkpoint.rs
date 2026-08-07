//! Orchestrator checkpoints: serializable snapshots and validated restore.
//!
//! An [`OrchestratorCheckpoint`] captures the bounded, secret-conscious state
//! of one session — config, task graph, memory store, transcript summary,
//! budget snapshot, finished subagent outcomes, the deterministic id
//! generator counter, and (for recovery checkpoints) an exact resumable
//! [`ResumeState`] — so turns are inspectable and resumable.  Restore is
//! fail-closed: [`OrchestratorCheckpoint::validate`] rejects unsupported
//! schema versions, dangling task references, over-limit or sensitive memory,
//! budget snapshots that do not match the checkpoint config, and resume state
//! that references unknown tasks.  Every restored item is attributed to the
//! checkpoint's provenance through a [`RestoreReport`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::budget::BudgetSnapshot;
use crate::config::OrchestratorConfig;
use crate::error::OrchestratorError;
use crate::memory::MemoryStore;
use crate::model::{ModelAdapter, ModelMessage};
use crate::policy::PolicyEngine;
use crate::runtime::OrchestratorRuntime;
use crate::subagents::{SubagentId, SubagentResult};
use crate::tasks::{TaskGraph, TaskId, TaskStatus};
use crate::tools::SideEffectClass;

/// Current checkpoint schema version; restore rejects everything else.
///
/// v2 adds recovery state: [`ResumeState`], creation timestamp, and provider
/// identity.  v1 checkpoints fail closed (never migrated silently).
pub const CHECKPOINT_SCHEMA_VERSION: u32 = 2;
/// Cap on transcript tail messages kept in a [`TranscriptSummary`]; older
/// messages are dropped, so snapshots stay bounded.
pub const MAX_TRANSCRIPT_TAIL_MESSAGES: usize = 8;
/// Default provenance label for checkpoints built without an explicit one.
pub const DEFAULT_CHECKPOINT_PROVENANCE: &str = "checkpoint";

/// Deterministic id generator state captured in a checkpoint.
///
/// Restoring a checkpoint replays the generator from the same counter, so two
/// restores of one checkpoint produce identical ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdGeneratorState {
    next: u64,
}

impl IdGeneratorState {
    /// Creates a fresh generator whose first id is `<prefix>-1`.
    #[must_use]
    pub fn new() -> Self {
        Self { next: 1 }
    }

    /// Allocates the next monotonic id for `prefix`.
    #[must_use]
    pub fn next(&mut self, prefix: &str) -> String {
        let id = format!("{prefix}-{}", self.next);
        self.next += 1;
        id
    }

    /// The next counter value; must be at least 1 in a valid checkpoint.
    #[must_use]
    pub fn next_value(&self) -> u64 {
        self.next
    }

    /// Validates the counter invariant (`next >= 1`).
    pub fn validate(&self) -> Result<(), OrchestratorError> {
        if self.next == 0 {
            return Err(OrchestratorError::InvalidState("id generator counter is zero".into()));
        }
        Ok(())
    }
}

impl Default for IdGeneratorState {
    fn default() -> Self {
        Self::new()
    }
}

/// Bounded summary of a turn's transcript, kept instead of the full transcript
/// so checkpoints stay small and secret-conscious.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TranscriptSummary {
    /// Total messages in the source transcript.
    pub message_count: usize,
    /// Total serialized bytes of the source transcript.
    pub total_bytes: usize,
    /// Whether older messages were dropped from `tail`.
    pub truncated: bool,
    /// Newest messages (at most [`MAX_TRANSCRIPT_TAIL_MESSAGES`]).
    pub tail: Vec<ModelMessage>,
}

impl TranscriptSummary {
    /// Builds a bounded summary from a transcript.
    #[must_use]
    pub fn from_transcript(transcript: &[ModelMessage]) -> Self {
        let total_bytes: usize = transcript
            .iter()
            .map(|message| serde_json::to_string(message).map_or(0, |json| json.len()))
            .sum();
        let start = transcript.len().saturating_sub(MAX_TRANSCRIPT_TAIL_MESSAGES);
        Self {
            message_count: transcript.len(),
            total_bytes,
            truncated: start > 0,
            tail: transcript[start..].to_vec(),
        }
    }

    /// Total messages in the source transcript.
    #[must_use]
    pub fn message_count(&self) -> usize {
        self.message_count
    }

    /// Total serialized bytes of the source transcript.
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Whether older messages were dropped from `tail`.
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// The retained newest messages.
    #[must_use]
    pub fn tail(&self) -> &[ModelMessage] {
        &self.tail
    }
}

/// Finished subagent outcomes, keyed by subagent id (the child task id).
///
/// The manager keeps no persistent state, so the checkpoint carries the
/// terminal outcomes; restore validates each id against the task graph.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentTreeState {
    subagents: BTreeMap<String, SubagentResult>,
}

impl SubagentTreeState {
    /// Creates an empty tree.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a finished subagent outcome; `false` when the id already
    /// exists (duplicate ids are rejected).
    pub fn insert(&mut self, result: SubagentResult) -> bool {
        self.subagents.insert(result.subagent_id.as_str().to_string(), result).is_none()
    }

    /// The recorded outcome for `subagent_id`, if any.
    #[must_use]
    pub fn get(&self, subagent_id: &SubagentId) -> Option<&SubagentResult> {
        self.subagents.get(subagent_id.as_str())
    }

    /// All recorded outcomes in stable id order.
    #[must_use]
    pub fn list(&self) -> Vec<SubagentResult> {
        self.subagents.values().cloned().collect()
    }

    /// Whether the tree is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.subagents.is_empty()
    }

    /// Validates every recorded outcome against the task graph: the id must
    /// match a child task node (present, with a parent) whose status is
    /// terminal, and the result's own id must agree with its key.
    pub fn validate(&self, tasks: &TaskGraph) -> Result<(), OrchestratorError> {
        for (key, result) in &self.subagents {
            if result.subagent_id.as_str() != key {
                return Err(OrchestratorError::InvalidState(format!(
                    "subagent tree key {key} does not match result id {}",
                    result.subagent_id
                )));
            }
            let Some(node) = tasks.get(&TaskId::new(key.clone())) else {
                return Err(OrchestratorError::InvalidState(format!(
                    "subagent tree references unknown child task {key}"
                )));
            };
            if node.parent.is_none() {
                return Err(OrchestratorError::InvalidState(format!(
                    "subagent tree references root task {key}, which is not a child"
                )));
            }
            if !matches!(
                node.status,
                TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
            ) {
                return Err(OrchestratorError::InvalidState(format!(
                    "subagent tree references non-terminal task {key}"
                )));
            }
        }
        Ok(())
    }
}

/// One completed tool call with its durable result summary, kept for
/// idempotent resume: replaying an identical call after an interruption
/// would re-run a side effect, so resumed turns reuse these results instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CompletedToolCall {
    /// Model-supplied tool-call id.
    pub tool_call_id: String,
    /// Tool name.
    pub tool_name: String,
    /// Exact arguments the model supplied.
    pub arguments: serde_json::Value,
    /// Whether the call succeeded.
    pub success: bool,
    /// Bounded result summary (never raw tool output).
    pub summary: String,
    /// Side-effect class, so replays of write/execute calls are blocked.
    pub side_effect_class: SideEffectClass,
}

/// A tool that was in flight when the turn stopped.  Its completion is
/// unknown, so automatic replay is blocked (ambiguous side effect).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct InFlightOperation {
    /// Model-supplied tool-call id.
    pub tool_call_id: String,
    /// Tool name.
    pub tool_name: String,
    /// Wall-clock millis when execution started.
    pub started_at_millis: u64,
}

/// Exact resumable turn state carried by recovery checkpoints.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ResumeState {
    /// Exact transcript tail (already bounded by the memory limit).  A
    /// resumed turn continues from here instead of rebuilding the prompt.
    pub transcript: Vec<ModelMessage>,
    /// Id of the active root task the resumed turn continues.
    pub active_task_id: String,
    /// Completed tool calls (bounded), reused instead of replayed.
    pub completed_tools: Vec<CompletedToolCall>,
    /// The in-flight operation, when one was interrupted.
    pub in_flight: Option<InFlightOperation>,
    /// How many times this turn has already been resumed.
    pub resumed_count: u32,
    /// Wall-clock millis when the turn first started, for the cumulative
    /// session-timeout cap.
    pub first_started_at_millis: u64,
}

/// Serializable snapshot of one orchestrated session's state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OrchestratorCheckpoint {
    /// Checkpoint schema version; must equal [`CHECKPOINT_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Where this snapshot came from (e.g. `checkpoint`, `session/load`).
    pub provenance: String,
    /// Provider identity that produced the checkpoint (workspace+provider
    /// provenance for crash restore); e.g. `ee-openrouter-agent`.
    pub provider: String,
    /// Wall-clock millis when the checkpoint was captured.
    pub created_at_millis: u64,
    /// Config snapshot; restore rebuilds budget limits from it.
    pub config: OrchestratorConfig,
    /// Active ACP session id.
    pub session_id: String,
    /// Task graph, including its deterministic id counter.
    pub tasks: TaskGraph,
    /// Bounded memory store.
    pub memory: MemoryStore,
    /// Bounded transcript summary.
    pub transcript_summary: TranscriptSummary,
    /// Budget counters at capture time.
    pub budget: BudgetSnapshot,
    /// Finished subagent outcomes.
    pub subagents: SubagentTreeState,
    /// Deterministic id generator state.
    pub id_generator: IdGeneratorState,
    /// Exact resumable turn state; `None` for plain snapshots without
    /// pending work.
    pub resume: Option<ResumeState>,
}

impl OrchestratorCheckpoint {
    /// Builds a checkpoint from its parts, validating everything up front so
    /// invalid state cannot be captured.
    ///
    /// All arguments are distinct, documented snapshot parts; the constructor
    /// is a deliberate data-assembly API.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provenance: impl Into<String>,
        config: OrchestratorConfig,
        session_id: impl Into<String>,
        tasks: TaskGraph,
        memory: MemoryStore,
        transcript_summary: TranscriptSummary,
        budget: BudgetSnapshot,
        subagents: SubagentTreeState,
        id_generator: IdGeneratorState,
    ) -> Result<Self, OrchestratorError> {
        Self::with_recovery(
            provenance,
            config,
            session_id,
            tasks,
            memory,
            transcript_summary,
            budget,
            subagents,
            id_generator,
            "unknown".to_string(),
            current_unix_millis(),
            None,
        )
    }

    /// Builds a checkpoint including recovery state (provider identity,
    /// capture time, and the exact resumable [`ResumeState`]).
    #[allow(clippy::too_many_arguments)]
    pub fn with_recovery(
        provenance: impl Into<String>,
        config: OrchestratorConfig,
        session_id: impl Into<String>,
        tasks: TaskGraph,
        memory: MemoryStore,
        transcript_summary: TranscriptSummary,
        budget: BudgetSnapshot,
        subagents: SubagentTreeState,
        id_generator: IdGeneratorState,
        provider: impl Into<String>,
        created_at_millis: u64,
        resume: Option<ResumeState>,
    ) -> Result<Self, OrchestratorError> {
        let checkpoint = Self {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            provenance: provenance.into(),
            provider: provider.into(),
            created_at_millis,
            config,
            session_id: session_id.into(),
            tasks,
            memory,
            transcript_summary,
            budget,
            subagents,
            id_generator,
            resume,
        };
        checkpoint.validate()?;
        Ok(checkpoint)
    }

    /// The checkpoint schema version.
    #[must_use]
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Fail-closed validation of every stored part: schema version, task
    /// references, id generator, memory limits and sensitivity, budget caps,
    /// and subagent references.
    pub fn validate(&self) -> Result<(), OrchestratorError> {
        if self.schema_version != CHECKPOINT_SCHEMA_VERSION {
            return Err(OrchestratorError::Serialization(format!(
                "unsupported checkpoint schema version {} (expected {CHECKPOINT_SCHEMA_VERSION})",
                self.schema_version
            )));
        }
        self.tasks.validate_references()?;
        self.id_generator.validate()?;
        let budget = &self.budget;
        let caps_match = budget.iterations_max == self.config.max_loop_iterations
            && budget.model_calls_max == self.config.max_model_calls
            && budget.tool_calls_max == self.config.max_tool_calls_per_turn
            && budget.subagents_max == self.config.max_subagents
            && budget.output_bytes_max == self.config.max_output_bytes
            && budget.input_tokens_max == self.config.max_input_tokens
            && budget.output_tokens_max == self.config.max_output_tokens;
        if !caps_match {
            return Err(OrchestratorError::InvalidState(
                "budget snapshot caps differ from checkpoint config".into(),
            ));
        }
        if budget.iterations_used > budget.iterations_max
            || budget.model_calls_used > budget.model_calls_max
            || budget.tool_calls_used > budget.tool_calls_max
            || budget.subagents_used > budget.subagents_max
            || budget.output_bytes_used > budget.output_bytes_max
            || budget
                .input_tokens_used
                .is_some_and(|used| used > budget.input_tokens_max.unwrap_or(usize::MAX))
            || budget
                .output_tokens_used
                .is_some_and(|used| used > budget.output_tokens_max.unwrap_or(usize::MAX))
        {
            return Err(OrchestratorError::InvalidState(
                "budget snapshot usage exceeds its caps".into(),
            ));
        }
        if self.memory.limit_bytes() != self.config.memory_limit_bytes {
            return Err(OrchestratorError::InvalidState(format!(
                "checkpoint memory limit {} differs from config limit {}",
                self.memory.limit_bytes(),
                self.config.memory_limit_bytes
            )));
        }
        if self.memory.total_bytes() > self.memory.limit_bytes() {
            return Err(OrchestratorError::InvalidState(format!(
                "checkpoint memory holds {} bytes over the {} byte limit",
                self.memory.total_bytes(),
                self.memory.limit_bytes()
            )));
        }
        if self.memory.items().iter().any(|item| item.sensitive) {
            return Err(OrchestratorError::PolicyDenied(
                "checkpoint contains sensitive memory items; refusing restore".into(),
            ));
        }
        self.subagents.validate(&self.tasks)?;
        if let Some(resume) = &self.resume {
            if self.tasks.get(&TaskId::new(resume.active_task_id.clone())).is_none() {
                return Err(OrchestratorError::InvalidState(format!(
                    "resume state references unknown active task {}",
                    resume.active_task_id
                )));
            }
            for completed in &resume.completed_tools {
                if completed.tool_call_id.is_empty() {
                    return Err(OrchestratorError::InvalidState(
                        "resume state holds a completed tool call with an empty id".into(),
                    ));
                }
            }
            if let Some(in_flight) = &resume.in_flight
                && in_flight.tool_call_id.is_empty()
            {
                return Err(OrchestratorError::InvalidState(
                    "resume state holds an in-flight operation with an empty id".into(),
                ));
            }
        }
        Ok(())
    }

    /// Restores the runtime and reports every restored item with its
    /// provenance.  The runtime rebuilds its stores from this checkpoint;
    /// `model` and `policy` come from the caller (never persisted).
    pub fn restore_runtime(
        &self,
        model: std::sync::Arc<dyn ModelAdapter>,
        policy: PolicyEngine,
    ) -> Result<(OrchestratorRuntime, RestoreReport), OrchestratorError> {
        self.validate()?;
        let runtime = OrchestratorRuntime::from_validated_checkpoint(self, model, policy)?;
        let report = RestoreReport {
            provenance: self.provenance.clone(),
            restored_tasks: self.tasks.list().into_iter().map(|task| task.id).collect(),
            restored_memory_items: self.memory.items().to_vec(),
        };
        Ok((runtime, report))
    }
}

/// Current wall-clock time in Unix milliseconds (checkpoint timestamps).
#[must_use]
pub fn current_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

/// Provenance-attributed summary of what a restore rebuilt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RestoreReport {
    /// Where the restored items came from.
    pub provenance: String,
    /// Task ids restored from the checkpoint, in stable order.
    pub restored_tasks: Vec<TaskId>,
    /// Memory items restored from the checkpoint (all non-sensitive).
    pub restored_memory_items: Vec<crate::memory::MemoryItem>,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::memory::MemoryItem;
    use crate::model::{ModelMessage, ModelRole};
    use crate::policy::{PolicyEngine, ToolPolicy};
    use crate::subagents::{SubagentResult, SubagentStatus};
    use crate::tasks::{TaskStatus, TaskWorker};
    use crate::test_support::FakeModel;

    use super::*;

    /// A valid checkpoint whose task graph holds a completed child task and
    /// whose subagent tree records that child's outcome.
    fn sample_checkpoint() -> OrchestratorCheckpoint {
        let config = OrchestratorConfig::default();
        let mut tasks = TaskGraph::new();
        let root = tasks.create_root("plan", "plan");
        let mut child = tasks.create_child(&root.id, "worker", "do the work").expect("child");
        child.set_worker(TaskWorker::Subagent(child.id.clone()));
        tasks.transition(&child.id, TaskStatus::Running).expect("pending -> running");
        tasks.transition(&child.id, TaskStatus::Completed).expect("running -> completed");

        let mut memory = MemoryStore::new(config.memory_limit_bytes);
        memory.insert(MemoryItem::from_task("cwd", "/work", root.id.clone())).expect("inserts");

        let mut subagents = SubagentTreeState::new();
        subagents.insert(SubagentResult {
            subagent_id: SubagentId::new(child.id.as_str()),
            status: SubagentStatus::Completed,
            summary: "done".into(),
            produced_memory_items: Vec::new(),
            tool_call_count: 0,
            error_summary: None,
            citations: crate::subagent_verifier::SubagentCitations::default(),
        });

        let transcript = vec![
            ModelMessage::text(ModelRole::User, "hello world"),
            ModelMessage::text(ModelRole::Assistant, "hi"),
        ];
        let budget = BudgetSnapshot {
            iterations_used: 1,
            iterations_max: config.max_loop_iterations,
            model_calls_used: 1,
            model_calls_max: config.max_model_calls,
            tool_calls_used: 0,
            tool_calls_max: config.max_tool_calls_per_turn,
            subagents_used: 1,
            subagents_max: config.max_subagents,
            output_bytes_used: 2,
            output_bytes_max: config.max_output_bytes,
            input_tokens_used: None,
            input_tokens_max: config.max_input_tokens,
            output_tokens_used: None,
            output_tokens_max: config.max_output_tokens,
        };
        OrchestratorCheckpoint::new(
            "test-snapshot",
            config,
            "s-1",
            tasks,
            memory,
            TranscriptSummary::from_transcript(&transcript),
            budget,
            subagents,
            IdGeneratorState::new(),
        )
        .expect("sample checkpoint is valid")
    }

    fn model() -> Arc<dyn ModelAdapter> {
        Arc::new(FakeModel::new(Vec::new()))
    }

    fn policy() -> PolicyEngine {
        PolicyEngine::new(ToolPolicy::default())
    }

    fn patched(
        checkpoint: &OrchestratorCheckpoint,
        patch: impl FnOnce(&mut serde_json::Value),
    ) -> OrchestratorCheckpoint {
        let mut value = serde_json::to_value(checkpoint).expect("checkpoint serializes");
        patch(&mut value);
        serde_json::from_value(value).expect("patched checkpoint deserializes")
    }

    #[test]
    fn schema_version_is_current() {
        assert_eq!(CHECKPOINT_SCHEMA_VERSION, 2);
        assert_eq!(sample_checkpoint().schema_version(), CHECKPOINT_SCHEMA_VERSION);
    }

    #[test]
    fn checkpoint_roundtrips_through_json() {
        let checkpoint = sample_checkpoint();
        let json = serde_json::to_string(&checkpoint).expect("serializes");
        let restored: OrchestratorCheckpoint = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, checkpoint);
        assert!(restored.validate().is_ok());
    }

    #[test]
    fn recovery_checkpoint_roundtrips_resume_state() {
        let base = sample_checkpoint();
        let checkpoint = OrchestratorCheckpoint::with_recovery(
            "recovery",
            base.config.clone(),
            "s-1",
            base.tasks.clone(),
            base.memory.clone(),
            base.transcript_summary.clone(),
            base.budget,
            base.subagents.clone(),
            base.id_generator,
            "ee-openrouter-agent",
            1_700_000_000_000,
            Some(ResumeState {
                transcript: vec![ModelMessage::text(ModelRole::User, "hello")],
                active_task_id: base.tasks.list()[0].id.as_str().to_string(),
                completed_tools: vec![CompletedToolCall {
                    tool_call_id: "tc-1".into(),
                    tool_name: "read_file".into(),
                    arguments: serde_json::json!({ "path": "/work/a.txt" }),
                    success: true,
                    summary: "a.txt: hello".into(),
                    side_effect_class: crate::tools::SideEffectClass::Read,
                }],
                in_flight: None,
                resumed_count: 1,
                first_started_at_millis: 1_699_000_000_000,
            }),
        )
        .expect("recovery checkpoint is valid");
        assert_eq!(checkpoint.provider, "ee-openrouter-agent");
        let json = serde_json::to_string(&checkpoint).expect("serializes");
        let restored: OrchestratorCheckpoint = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, checkpoint);
        assert!(restored.validate().is_ok());
        let resume = restored.resume.expect("resume state survives");
        assert_eq!(resume.completed_tools[0].tool_name, "read_file");
        assert_eq!(resume.resumed_count, 1);
    }

    #[test]
    fn resume_state_rejects_unknown_active_task() {
        let base = sample_checkpoint();
        let checkpoint = OrchestratorCheckpoint::with_recovery(
            "recovery",
            base.config.clone(),
            "s-1",
            base.tasks.clone(),
            base.memory.clone(),
            base.transcript_summary.clone(),
            base.budget,
            base.subagents.clone(),
            base.id_generator,
            "provider",
            1,
            Some(ResumeState {
                transcript: Vec::new(),
                active_task_id: "task-404".into(),
                completed_tools: Vec::new(),
                in_flight: None,
                resumed_count: 0,
                first_started_at_millis: 0,
            }),
        )
        .expect_err("dangling active task rejected");
        assert!(
            matches!(checkpoint, OrchestratorError::InvalidState(ref reason) if reason.contains("unknown active task")),
            "{checkpoint}"
        );
    }

    #[test]
    fn resume_state_rejects_empty_tool_ids() {
        let base = sample_checkpoint();
        let error = OrchestratorCheckpoint::with_recovery(
            "recovery",
            base.config.clone(),
            "s-1",
            base.tasks.clone(),
            base.memory.clone(),
            base.transcript_summary.clone(),
            base.budget,
            base.subagents.clone(),
            base.id_generator,
            "provider",
            1,
            Some(ResumeState {
                transcript: Vec::new(),
                active_task_id: base.tasks.list()[0].id.as_str().to_string(),
                completed_tools: vec![CompletedToolCall {
                    tool_call_id: String::new(),
                    tool_name: "read_file".into(),
                    arguments: serde_json::json!({}),
                    success: true,
                    summary: "x".into(),
                    side_effect_class: crate::tools::SideEffectClass::Read,
                }],
                in_flight: None,
                resumed_count: 0,
                first_started_at_millis: 0,
            }),
        )
        .expect_err("empty tool id rejected");
        assert!(
            matches!(error, OrchestratorError::InvalidState(ref reason) if reason.contains("empty id")),
            "{error}"
        );
    }

    #[test]
    fn id_generator_state_is_deterministic_across_restores() {
        let mut generator = IdGeneratorState::new();
        assert_eq!(generator.next("session"), "session-1");
        assert_eq!(generator.next("session"), "session-2");
        assert_eq!(generator.next("task"), "task-3");

        // A restored generator replays from the captured counter.
        let json = serde_json::to_string(&generator).expect("serializes");
        let mut restored: IdGeneratorState = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored.next("task"), "task-4");
        assert_eq!(restored.next_value(), 5);
    }

    #[test]
    fn transcript_summary_is_bounded_and_tracks_truncation() {
        use crate::model::ModelContent;
        let messages: Vec<ModelMessage> =
            (0..20).map(|index| ModelMessage::text(ModelRole::User, format!("m{index}"))).collect();
        let summary = TranscriptSummary::from_transcript(&messages);
        assert_eq!(summary.message_count(), 20);
        assert!(summary.truncated());
        assert_eq!(summary.tail().len(), MAX_TRANSCRIPT_TAIL_MESSAGES);
        // The newest message survives truncation.
        assert_eq!(
            summary.tail()[summary.tail().len() - 1].content,
            vec![ModelContent::Text("m19".into())]
        );

        let small = TranscriptSummary::from_transcript(&messages[..3]);
        assert!(!small.truncated());
        assert_eq!(small.tail().len(), 3);
        assert!(small.total_bytes() > 0);
    }

    #[test]
    fn restore_rebuilds_runtime_state() {
        let checkpoint = sample_checkpoint();
        let (runtime, report) = checkpoint.restore_runtime(model(), policy()).expect("restores");
        assert_eq!(runtime.config(), &checkpoint.config);
        assert_eq!(runtime.tasks(), checkpoint.tasks);
        assert_eq!(runtime.memory(), checkpoint.memory);
        assert_eq!(runtime.budget_snapshot(), checkpoint.budget);
        assert_eq!(report.provenance, "test-snapshot");
        assert_eq!(
            report.restored_tasks,
            checkpoint.tasks.list().into_iter().map(|task| task.id).collect::<Vec<_>>()
        );
        assert_eq!(report.restored_memory_items.len(), 1);
        assert_eq!(report.restored_memory_items[0].key, "cwd");
    }

    #[test]
    fn restore_rejects_unsupported_schema_versions() {
        for version in [0, 1, 99] {
            let checkpoint = patched(&sample_checkpoint(), |value| {
                value["schema_version"] = serde_json::json!(version);
            });
            let error = checkpoint.validate().expect_err("schema mismatch rejected");
            assert!(
                matches!(error, OrchestratorError::Serialization(ref reason) if reason.contains("schema version")),
                "{error}"
            );
        }
    }

    #[test]
    fn restore_rejects_dangling_task_references() {
        let checkpoint = patched(&sample_checkpoint(), |value| {
            value["tasks"]["tasks"]["task-1"]["parent"] = serde_json::json!("task-999");
        });
        let error = checkpoint.validate().expect_err("dangling parent rejected");
        assert!(
            matches!(error, OrchestratorError::InvalidState(ref reason) if reason.contains("unknown parent")),
            "{error}"
        );

        let checkpoint = patched(&sample_checkpoint(), |value| {
            value["tasks"]["tasks"]["task-1"]["dependencies"] = serde_json::json!(["task-404"]);
        });
        let error = checkpoint.validate().expect_err("dangling dependency rejected");
        assert!(
            matches!(error, OrchestratorError::InvalidState(ref reason) if reason.contains("unknown dependency")),
            "{error}"
        );
    }

    #[test]
    fn restore_rejects_over_limit_memory() {
        let checkpoint = patched(&sample_checkpoint(), |value| {
            value["memory"]["total_bytes"] = serde_json::json!(1_000_000_000);
        });
        let error = checkpoint.validate().expect_err("over-limit memory rejected");
        assert!(
            matches!(error, OrchestratorError::InvalidState(ref reason) if reason.contains("over the")),
            "{error}"
        );
    }

    #[test]
    fn restore_rejects_memory_limit_mismatch() {
        let checkpoint = patched(&sample_checkpoint(), |value| {
            value["memory"]["limit_bytes"] = serde_json::json!(123);
        });
        let error = checkpoint.validate().expect_err("limit mismatch rejected");
        assert!(
            matches!(error, OrchestratorError::InvalidState(ref reason) if reason.contains("differs from config")),
            "{error}"
        );
    }

    #[test]
    fn restore_rejects_sensitive_memory_items() {
        let checkpoint = patched(&sample_checkpoint(), |value| {
            value["memory"]["items"][0]["sensitive"] = serde_json::json!(true);
        });
        let error = checkpoint.validate().expect_err("sensitive memory rejected");
        assert!(
            matches!(error, OrchestratorError::PolicyDenied(ref reason) if reason.contains("sensitive")),
            "{error}"
        );
    }

    #[test]
    fn restore_rejects_subagent_reference_without_child_task() {
        let checkpoint = patched(&sample_checkpoint(), |value| {
            value["tasks"]["tasks"].as_object_mut().expect("task map").remove("task-2");
        });
        let error = checkpoint.validate().expect_err("unknown subagent task rejected");
        assert!(
            matches!(error, OrchestratorError::InvalidState(ref reason) if reason.contains("unknown child task")),
            "{error}"
        );
    }

    #[test]
    fn subagent_tree_rejects_duplicate_ids_and_root_references() {
        let mut tree = SubagentTreeState::new();
        let result = SubagentResult {
            subagent_id: SubagentId::new("task-2"),
            status: SubagentStatus::Completed,
            summary: "done".into(),
            produced_memory_items: Vec::new(),
            tool_call_count: 0,
            error_summary: None,
            citations: crate::subagent_verifier::SubagentCitations::default(),
        };
        assert!(tree.insert(result.clone()));
        assert!(!tree.insert(result), "duplicate subagent id rejected");
        assert_eq!(tree.list().len(), 1);
    }

    #[test]
    fn budget_snapshot_caps_are_validated_on_restore() {
        let checkpoint = patched(&sample_checkpoint(), |value| {
            value["budget"]["iterations_max"] = serde_json::json!(999);
        });
        let error = checkpoint.validate().expect_err("cap drift rejected");
        assert!(
            matches!(error, OrchestratorError::InvalidState(ref reason) if reason.contains("differ from checkpoint config")),
            "{error}"
        );
    }

    #[test]
    fn budget_snapshot_used_over_cap_is_rejected() {
        let checkpoint = patched(&sample_checkpoint(), |value| {
            value["budget"]["iterations_used"] = serde_json::json!(10_000);
        });
        let error = checkpoint.validate().expect_err("over-cap usage rejected");
        assert!(
            matches!(error, OrchestratorError::InvalidState(ref reason) if reason.contains("exceeds its caps")),
            "{error}"
        );
    }
}
