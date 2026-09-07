//! Orchestrator runtime: owns the stores and runs turns.
//!
//! [`OrchestratorRuntime`] is constructed once per agent/session with an
//! injected [`ModelAdapter`]; [`OrchestratorRuntime::run_turn`] builds the
//! root task from the prompt and runs one bounded turn over the framework's
//! [`UpdateSink`] and [`ClientBridge`].  The stores (tasks, memory, budget,
//! tools) live inside the runtime, so provider code only interacts with the
//! ACP framework surface.

pub(crate) use std::sync::{Arc, Mutex, RwLock};

pub(crate) use sha2::{Digest, Sha256};

pub(crate) use ee_acp_agent_server::{ClientBridge, PromptContext, PromptResult, UpdateSink};
pub(crate) use ee_agent_protocol::{
    COMPACT_COMMAND_NAME, ContentBlock, RecoverableFault, SessionUpdate, UsageUpdate,
    parse_slash_command,
};
pub(crate) use tokio::sync::watch;

pub(crate) use crate::budget::BudgetTracker;
pub(crate) use crate::checkpoint::{
    CheckpointContextProvenance, OrchestratorCheckpoint, current_unix_millis,
};
pub(crate) use crate::checkpoint_store::{CheckpointHandle, CheckpointStore};
pub(crate) use crate::child_registry::{ChildCancelResult, ChildRegistry, ChildSnapshot};
pub(crate) use crate::command_intelligence::ValidationCommandFailure;
pub(crate) use crate::compaction::{
    CompactTurnReport, SESSION_SUMMARY_KEY, build_compaction_context, build_compaction_prompt,
};
pub(crate) use crate::config::OrchestratorConfig;
pub(crate) use crate::context_planner::{
    ContextInvalidation, ContextPlan, ContextPlanCache, ContextPlanner, ContextPlannerConfig,
};
pub(crate) use crate::decision_log::DecisionLog;
pub(crate) use crate::error::OrchestratorError;
pub(crate) use crate::events::{EventRecorder, OrchestratorEvent};
pub(crate) use crate::final_response::{
    FinalResponse, FinalResponseBuilder, ValidationOutcome, ValidationRecorder,
    changed_files_from_log,
};
pub(crate) use crate::loop_engine::{LoopEngine, LoopOptions, TurnSystemContext};
pub(crate) use crate::memory::{MemoryItem, MemoryStore};
pub(crate) use crate::memory_compaction::compact_memory;
pub(crate) use crate::metrics::OrchestratorMetrics;
pub(crate) use crate::model::{
    ModelAdapter, ModelMessage, ModelRequest, ModelRole, Transcript, prompt_result_with_usage,
};
pub(crate) use crate::model_registry::{DEFAULT_MODEL_ID, ModelRegistry};
pub(crate) use crate::model_router::ModelRouter;
pub(crate) use crate::plan_compiler::{PlanCompiler, PlanInput};
pub(crate) use crate::policy::PolicyEngine;
pub(crate) use crate::progress::ProgressTracker;
pub(crate) use crate::recovery::{RecoverableInterruption, TurnOutcome, session_timeout_expired};
pub(crate) use crate::repair::{
    RepairController, RepairDecision, RepairFailureSummary, RepairProgress, RepairReason,
    RepairStopReason,
};
pub(crate) use crate::repair_context::{
    REPAIR_CONTEXT_TOOLS, RepairContextObservation, RepairContextSnapshot, build_repair_context,
};
pub(crate) use crate::review_context::{ReviewContextMetadata, build_review_context_with_metadata};
pub(crate) use crate::rubber_duck::{
    FindingDecision, RubberDuckFindingLedger, RubberDuckOutcome, RubberDuckRequest,
    RubberDuckRunner, RubberDuckUnavailable,
};
pub(crate) use crate::rubber_duck_trigger::{
    RubberDuckTrigger, RubberDuckTriggerController, RubberDuckTriggerDecision,
    RubberDuckTriggerDisposition, RubberDuckTriggerFacts, RubberDuckTriggerKey,
    RubberDuckTriggerPolicy, RubberDuckTriggerReason, RubberDuckTriggerSkipReason,
};
pub(crate) use crate::sensitive_data::SensitiveDataGuard;
pub(crate) use crate::strategy::{
    StrategicInput, StrategyContext, StrategyExecutor, StrategyRun, StrategySelector, TurnResult,
    capability_aware_guidance,
};
pub(crate) use crate::subagents::{
    DelegateTool, SubagentManager, SubagentObservability, SubagentState,
};
pub(crate) use crate::tasks::{TaskGraph, TaskId, TaskNode, TaskStatus, truncate};
pub(crate) use crate::tools::{ServerTool, ToolExecutionLogEntry, ToolExecutor, ToolRegistry};
pub(crate) use crate::validation::{
    ValidationPlanner, ValidationPlanningContext, ValidationResult, ValidationRunner,
    WorkspaceValidationConfig, finalize_validation_tasks,
};
pub(crate) use crate::{CritiqueTarget, ReportEvidence};

const MAX_TASK_TITLE_CHARS: usize = 120;
/// Cap on the root task description derived from the prompt.
const MAX_TASK_DESCRIPTION_CHARS: usize = 4_000;
/// Title used when the prompt has no text blocks.
const UNTITLED_TASK: &str = "untitled task";
const REPAIR_CONTEXT_TOOL_CALL_PREFIX: &str = "repair-context";
const MANUAL_RUBBER_DUCK_MAX_SYNTHESIS_BYTES: usize = 16 * 1024;

/// Server-observed validation results available to the repair controller.
/// They are not host-selected completion evidence.
#[derive(Debug, Clone, Default)]
struct PostWriteValidation {
    results: Vec<ValidationResult>,
}

/// Host-derived metadata used while preparing one turn. Only redacted provenance
/// fields are retained in checkpoints; normalized live-session history is transient.
#[derive(Debug, Clone, Default)]
struct RecoveryTurnMetadata {
    history: Vec<ModelMessage>,
    context_plan: Option<ContextPlan>,
    checkpoint_context: CheckpointContextProvenance,
    evidence_refs: Vec<crate::observability::RedactedEvidenceRef>,
}

/// Server-side orchestrator runtime.
/// Completed recovery turn plus its evidence-gated final response.
///
/// ACP still receives the contained [`PromptResult`]. The typed final response
/// travels through host-safe text/update surfaces instead of new ACP fields.
#[derive(Debug, Clone, PartialEq)]
pub struct StrategicRecoveryTurn {
    pub prompt_result: PromptResult,
    pub final_response: FinalResponse,
}

/// Result of explicit `/rubber-duck`: verified critic evidence followed by one
/// bounded root-owned synthesis. Raw critic transport output never appears here.
#[derive(Debug, Clone, PartialEq)]
pub struct ManualRubberDuckTurn {
    pub prompt_result: PromptResult,
    pub critic_outcome: RubberDuckOutcome,
    pub synthesis: Option<String>,
    pub timeline_summary: String,
}

/// Outcome of one deterministic automatic trigger boundary.
#[derive(Debug, Clone, PartialEq)]
pub enum AutomaticRubberDuckTurn {
    Ran { key: RubberDuckTriggerKey, reason: RubberDuckTriggerReason, outcome: RubberDuckOutcome },
    Skipped(RubberDuckTriggerSkipReason),
}

/// Inputs shared by a fresh strategic recovery turn and a resumed one.
/// Grouping this metadata keeps the production API narrow while retaining the
/// exact provider identity and system context used by checkpoint recovery.
#[derive(Debug, Clone)]
pub struct StrategicRecoveryContext {
    pub input: StrategicInput,
    pub system_context: String,
    pub provider: String,
}

impl StrategicRecoveryContext {
    #[must_use]
    pub fn new(
        input: StrategicInput,
        system_context: impl Into<String>,
        provider: impl Into<String>,
    ) -> Self {
        Self { input, system_context: system_context.into(), provider: provider.into() }
    }
}

/// Recovery-aware strategic completion outcome.
///
/// Interruption remains distinct from terminal completion so callers never
/// emit a final completion report for a checkpointed, resumable turn.
#[derive(Debug, Clone, PartialEq)]
pub enum StrategicTurnOutcome {
    Completed(Box<StrategicRecoveryTurn>),
    Interrupted(RecoverableInterruption),
}

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
    model_router: Arc<RwLock<Option<ModelRouter>>>,
    metrics: Arc<Mutex<OrchestratorMetrics>>,
    decisions: Arc<Mutex<DecisionLog>>,
    children: Arc<ChildRegistry>,
    /// Final response facts from latest terminal recovery turn. These facts
    /// are server-observed tool records only; host evidence remains separate.
    last_turn_changed_files: Mutex<Vec<crate::final_response::ChangedFile>>,
    last_turn_validation: Mutex<ValidationRecorder>,
    /// Trusted planning inputs set by strategic recovery before its loop starts.
    /// They select registered validation tools only; never completion evidence.
    validation_workspace: Mutex<WorkspaceValidationConfig>,
    validation_changed_symbols: Mutex<Vec<String>>,
    /// Revision-keyed fresh-context cache. It is invalidated conservatively on every
    /// host-observed mutation, diagnostics, VCS, and validation transition.
    context_cache: Mutex<ContextPlanCache>,
    /// Internal same-session contrasting-model critic and root-owned findings.
    rubber_duck: Arc<RubberDuckRunner>,
    /// Automatic-boundary claims. Claimed before dispatch; terminal failures
    /// remain claimed so equivalent evidence cannot form hidden retry loops.
    rubber_duck_triggers: Mutex<RubberDuckTriggerController>,
    /// Typed terminal repair stop from latest production turn. This never upgrades
    /// host completion evidence; it only blocks unsupported completion claims.
    last_repair_stop: Mutex<Option<RepairStopReason>>,
}

mod compact;
mod lifecycle;
mod resume;
mod rubber_duck;
mod strategic;
mod turn;

#[cfg(test)]
mod tests;
fn critique_finding_counts(report: &crate::CritiqueReport) -> (usize, usize, usize) {
    report.findings.iter().fold((0, 0, 0), |mut counts, finding| {
        match finding.severity {
            crate::CritiqueSeverity::Blocking => counts.0 += 1,
            crate::CritiqueSeverity::NonBlocking => counts.1 += 1,
            crate::CritiqueSeverity::Suggestion => counts.2 += 1,
        }
        counts
    })
}

fn contrast_unavailable_summary(reason: &crate::ContrastUnavailable) -> String {
    match reason {
        crate::ContrastUnavailable::UnknownActiveIdentity { active_id } => {
            format!("unknown active model identity {active_id}")
        }
        crate::ContrastUnavailable::NoAlternative => "no alternative model is registered".into(),
        crate::ContrastUnavailable::SameFamilyOnly { family } => {
            format!("only same-family alternatives are registered ({family:?})")
        }
        crate::ContrastUnavailable::MissingCapability { required } => {
            format!("alternative model lacks required capabilities {required:?}")
        }
        crate::ContrastUnavailable::DisabledRoute => "contrasting model route is disabled".into(),
    }
}

fn manual_rubber_duck_outcome_summary(outcome: &RubberDuckOutcome) -> String {
    match outcome {
        RubberDuckOutcome::Completed(_) => "rubber duck completed".into(),
        RubberDuckOutcome::Unavailable(reason) => match reason {
            RubberDuckUnavailable::Disabled => "rubber duck disabled".into(),
            RubberDuckUnavailable::AutomaticDisabled => {
                "rubber duck skipped: automatic mode disabled".into()
            }
            RubberDuckUnavailable::ExternalBackendConfigured { agent_id } => {
                format!("rubber duck skipped: external backend `{agent_id}` requires host broker")
            }
            RubberDuckUnavailable::CallLimitReached { max_calls } => {
                format!("rubber duck skipped: session call limit {max_calls} reached")
            }
            RubberDuckUnavailable::CallAccountingCapacityReached { max_sessions } => {
                format!("rubber duck skipped: session accounting capacity {max_sessions} reached")
            }
            RubberDuckUnavailable::Contrast(reason) => {
                format!("rubber duck skipped: {}", contrast_unavailable_summary(reason))
            }
            RubberDuckUnavailable::BudgetDenied { reason }
            | RubberDuckUnavailable::InvalidRequest { reason } => {
                format!("rubber duck skipped: {reason}")
            }
        },
        RubberDuckOutcome::Quarantined { reason } => {
            format!("rubber duck quarantined: {reason}")
        }
        RubberDuckOutcome::Cancelled => "rubber duck cancelled".into(),
        RubberDuckOutcome::Failed { reason } => format!("rubber duck failed: {reason}"),
    }
}

async fn runtime_cancelled(mut cancel: watch::Receiver<bool>) {
    if *cancel.borrow() {
        return;
    }
    let _ = cancel.changed().await;
}

/// Builds checkpoint-safe metadata from host-provided strategic input. Context
/// excerpts and paths are deliberately excluded; only revision labels, source
/// classes, and host-redacted evidence identifiers may cross this boundary.
fn recovery_turn_metadata(input: &StrategicInput) -> RecoveryTurnMetadata {
    let checkpoint_context =
        input.context.as_ref().map_or_else(CheckpointContextProvenance::default, |context| {
            let mut source_labels = context
                .candidates
                .iter()
                .map(|candidate| candidate.source.label().to_string())
                .collect::<Vec<_>>();
            source_labels.sort();
            source_labels.dedup();
            source_labels.truncate(crate::checkpoint::MAX_CHECKPOINT_CONTEXT_SOURCES);
            CheckpointContextProvenance {
                workspace_revision: nonempty_label(&context.identity.workspace_revision),
                buffer_revision: nonempty_label(&context.identity.buffer_revision),
                diagnostics_revision: nonempty_label(&context.identity.diagnostics_revision),
                checkout_revision: nonempty_label(&context.identity.checkout_revision),
                source_labels,
            }
        });
    let mut evidence_ids = input
        .completion_evidence
        .as_ref()
        .into_iter()
        .flat_map(|evidence| {
            [
                evidence.changed_file_inventory.as_ref(),
                evidence.post_write_diagnostics.as_ref(),
                evidence.final_diff_review.as_ref(),
            ]
            .into_iter()
            .flatten()
            .map(|item| item.id.clone())
        })
        .collect::<Vec<_>>();
    evidence_ids.sort();
    evidence_ids.dedup();
    let evidence_refs = evidence_ids
        .into_iter()
        .take(crate::checkpoint::MAX_CHECKPOINT_EVIDENCE_REFS)
        .filter_map(|id| crate::observability::RedactedEvidenceRef::new(id).ok())
        .collect();
    RecoveryTurnMetadata {
        history: Vec::new(),
        context_plan: input
            .context
            .as_ref()
            .map(|context| ContextPlanner.plan(context, &ContextPlannerConfig::default())),
        checkpoint_context,
        evidence_refs,
    }
}

fn nonempty_label(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

/// Counts successful writes in a turn execution log.
fn successful_write_count(log: &[ToolExecutionLogEntry]) -> usize {
    log.iter()
        .filter(|entry| {
            entry.success && entry.side_effect_class == Some(crate::tools::SideEffectClass::Write)
        })
        .count()
}

fn repair_request(
    snapshot: &RepairContextSnapshot,
    validation: &PostWriteValidation,
    records: &ValidationRecorder,
) -> Option<(RepairReason, RepairFailureSummary, Vec<String>)> {
    if snapshot.error_diagnostic_count > 0 {
        return Some((
            RepairReason::Diagnostics,
            RepairFailureSummary::Diagnostics {
                diagnostic_count: snapshot.error_diagnostic_count,
                fingerprint: format!(
                    "diagnostics:{}:{}",
                    snapshot.revision, snapshot.error_diagnostic_count
                ),
            },
            Vec::new(),
        ));
    }
    if snapshot.has_conflicts {
        return Some((
            RepairReason::Diff,
            RepairFailureSummary::Diff {
                changed_file_count: snapshot.changed_file_count,
                fingerprint: format!(
                    "conflicted-diff:{}:{}",
                    snapshot.revision, snapshot.changed_file_count
                ),
            },
            Vec::new(),
        ));
    }
    let result =
        validation.results.iter().find(|result| result.status == ValidationOutcome::Failed)?;
    let evidence_ids = records
        .records()
        .iter()
        .filter(|record| record.command == result.command)
        .map(|record| record.evidence_id.clone())
        .take(1)
        .collect::<Vec<_>>();
    Some((
        RepairReason::SelectedValidationFailure,
        RepairFailureSummary::SelectedValidationFailure {
            evidence_id: evidence_ids.first().cloned().unwrap_or_default(),
            fingerprint: validation_result_fingerprint(result),
        },
        evidence_ids,
    ))
}

fn repair_validation_fingerprint(validation: &PostWriteValidation) -> String {
    validation
        .results
        .iter()
        .find(|result| result.status == ValidationOutcome::Failed)
        .map(validation_result_fingerprint)
        .unwrap_or_default()
}

fn validation_result_fingerprint(result: &ValidationResult) -> String {
    let failure = result.failure.map_or("unknown", ValidationCommandFailure::as_str);
    format!("validation:{}:{failure}", result.command_id)
}

fn repair_system_context(
    snapshot: &RepairContextSnapshot,
    attempt: &crate::repair::RepairAttempt,
) -> String {
    format!(
        "Repair controller request. Attempt {} of bounded automatic repair. Failure source: {}. Current host revision: {}. Use current untrusted context below; inspect before mutating. Do not claim verified completion.\n",
        attempt.attempt_number,
        repair_reason_label(attempt.reason),
        snapshot.revision,
    )
}

fn repair_reason_label(reason: RepairReason) -> &'static str {
    match reason {
        RepairReason::Diagnostics => "diagnostics",
        RepairReason::Diff => "diff",
        RepairReason::SelectedValidationFailure => "selected_validation_failure",
    }
}

fn repair_stop_label(reason: RepairStopReason) -> &'static str {
    match reason {
        RepairStopReason::RepeatedIdenticalToolCalls => "repeated_identical_tool_calls",
        RepairStopReason::UnchangedDiff => "unchanged_diff",
        RepairStopReason::RepeatedValidationFailure => "repeated_validation_failure",
        RepairStopReason::NoProgress => "no_progress",
        RepairStopReason::PolicyDenial => "policy_denial",
        RepairStopReason::StaleState => "stale_state",
        RepairStopReason::Cancellation => "cancellation",
        RepairStopReason::BudgetExhaustion => "budget_exhaustion",
        RepairStopReason::Timeout => "timeout",
        RepairStopReason::UnavailableEnvironment => "unavailable_environment",
        RepairStopReason::AttemptsExhausted => "attempts_exhausted",
    }
}

fn repair_safe_follow_up(reason: RepairStopReason) -> &'static str {
    match reason {
        RepairStopReason::PolicyDenial => "grant required approval or policy, then retry repair",
        RepairStopReason::StaleState => {
            "refresh editor buffers and workspace state, then retry repair"
        }
        RepairStopReason::Cancellation => "resume or start a new turn when ready",
        RepairStopReason::BudgetExhaustion => "start a new turn with a smaller repair scope",
        RepairStopReason::Timeout => "check environment responsiveness, then retry repair",
        RepairStopReason::UnavailableEnvironment => {
            "restore required editor or MCP context tools, then retry repair"
        }
        RepairStopReason::RepeatedIdenticalToolCalls
        | RepairStopReason::UnchangedDiff
        | RepairStopReason::RepeatedValidationFailure
        | RepairStopReason::NoProgress
        | RepairStopReason::AttemptsExhausted => {
            "review current failure evidence and provide a focused next repair instruction"
        }
    }
}

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
