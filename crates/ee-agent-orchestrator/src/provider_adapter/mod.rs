//! ACP provider adapter: wraps [`OrchestratorRuntime`] behind the framework's
//! [`AgentProvider`] trait.
//!
//! Agent binaries can serve ACP either framework-only (provider implements
//! [`AgentProvider`] directly) or orchestration-backed: build an
//! [`OrchestratorProvider`] with a [`ModelAdapter`] and the framework owns
//! JSON-RPC dispatch while the orchestrator owns the model–tool loop.  The
//! adapter is provider-neutral — no OpenRouter or other backend code lives
//! here.
//!
//! Session lifecycle:
//! - `session/new` creates a fresh [`OrchestratorRuntime`] per session, so
//!   task graph, memory, and budget state are isolated per session.  MCP
//!   server entries are validated into redacted descriptors and retained per
//!   session (Phase 12).
//! - `session/load` restores a previously persisted (serialized) task graph
//!   and memory store when the adapter still holds them.
//! - `session/prompt` bridges the session's MCP servers for the turn
//!   (connect, `tools/list`, registration, dispatch, disconnect — see
//!   [`crate::mcp`]), then delegates to
//!   [`OrchestratorRuntime::run_turn`]; the framework's cancellation watch is
//!   passed through unchanged, so `session/cancel` and `session/close` stop
//!   the active turn.
//! - `session/close` serializes the session's task/memory state (for a later
//!   `session/load`) and drops the runtime.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ee_acp_agent_server::{
    AgentProvider, ClientBridge, LoadSessionContext, NewSessionContext, PromptContext,
    PromptResult, ProviderError, ProviderFuture, SessionInit, SetModeContext, UpdateSink,
};
use ee_agent_protocol::{
    AgentCapabilities, COMPACT_COMMAND_NAME, ContentBlock, ContentChunk, DISCARD_COMMAND_NAME,
    Implementation, McpCapabilities, MessageId, RUBBER_DUCK_COMMAND_NAME, SessionCapabilities,
    SessionCloseCapabilities, SessionId, SessionListCapabilities, SessionMode, SessionModeId,
    SessionModeState, SessionResumeCapabilities, SessionUpdate, StopReason, TextContent,
    compact_available_command, discard_available_command, is_resume_command, parse_slash_command,
    resume_available_command, rubber_duck_available_command,
};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::checkpoint_store::CheckpointStore;
use crate::config::OrchestratorConfig;
use crate::final_response::FinalResponse;
use crate::mcp::{
    McpBackedTool, McpDiscoveryDiagnostic, McpServerDescriptor, McpSessionManager, McpToolPolicy,
};
#[cfg(test)]
use crate::memory::MemoryStore;
use crate::model::{ModelAdapter, ModelMessage, ModelRole};
use crate::observability::{
    TelemetryAttribution, TelemetryConfig, TelemetryRecorder, TelemetrySummary, TelemetryTransport,
    TelemetryTurnOutcome, TelemetryVersionLabels, ToolFailureReason, WaterfallFinish,
    WaterfallOutcome, WaterfallStage,
};
use crate::policy::{PolicyEngine, ToolPolicy};
use crate::runtime::{
    OrchestratorRuntime, StrategicRecoveryContext, StrategicRecoveryTurn, StrategicTurnOutcome,
};
use crate::session_store::SessionStateStore;
use crate::strategy::StrategicInput;
#[cfg(test)]
use crate::tasks::TaskGraph;
use crate::validation::WorkspaceValidationConfig;

/// Default implementation name advertised in `initialize` responses.
pub const DEFAULT_IMPLEMENTATION_NAME: &str = "ee-agent-orchestrator";
/// Title used when the implementation metadata is left at its default.
pub const DEFAULT_IMPLEMENTATION_TITLE: &str = "Agent Orchestrator";
/// Prefix of provider-generated session ids (`session-1`, `session-2`, ...).
pub const SESSION_ID_PREFIX: &str = "session";

/// Adapter configuration: the orchestrator knobs plus the ACP implementation
/// metadata advertised in `initialize` responses.
#[derive(Debug, Clone)]
pub struct OrchestratorProviderConfig {
    /// Orchestrator loop, tool, subagent, budget, and timeout knobs.
    pub orchestrator: OrchestratorConfig,
    /// ACP implementation metadata returned by [`AgentProvider::info`].
    pub implementation: Implementation,
    /// MCP tool bridging knobs (Phase 12): per-request timeouts and
    /// side-effect classification overrides for session-advertised MCP
    /// servers.
    pub mcp: McpToolPolicy,
    /// Optional root for durable normal-session snapshots. This is separate
    /// from recovery checkpoints, which represent interrupted turns only.
    pub session_state_dir: Option<std::path::PathBuf>,
    /// Maximum serialized bytes retained for one normal-session snapshot.
    pub max_session_state_bytes: usize,
    /// Trusted validation declarations supplied by server configuration. Repository
    /// instructions and ACP prompts cannot introduce executable commands here.
    pub validation_workspace: WorkspaceValidationConfig,
    /// Privacy-safe local telemetry retention. Disabled by default; this never
    /// enables network delivery or automatic persistence.
    pub telemetry: TelemetryConfig,
    /// Opaque version labels attached to locally retained telemetry records.
    pub telemetry_attribution: TelemetryAttribution,
}

impl Default for OrchestratorProviderConfig {
    fn default() -> Self {
        Self {
            orchestrator: OrchestratorConfig::default(),
            implementation: Implementation::new(
                DEFAULT_IMPLEMENTATION_NAME,
                env!("CARGO_PKG_VERSION"),
            )
            .title(DEFAULT_IMPLEMENTATION_TITLE),
            mcp: McpToolPolicy::default(),
            session_state_dir: None,
            max_session_state_bytes: crate::config::DEFAULT_MAX_CHECKPOINT_BYTES,
            validation_workspace: WorkspaceValidationConfig::default(),
            telemetry: TelemetryConfig::default(),
            telemetry_attribution: default_telemetry_attribution(),
        }
    }
}

/// One live session's orchestrator runtime and immutable session facts.
struct SessionRuntime {
    runtime: Arc<OrchestratorRuntime>,
    system_context: String,
    /// Workspace used to scope durable state and avoid cross-workspace loads.
    workspace: std::path::PathBuf,
    /// Current ACP mode, kept in provider state so its policy and prompt
    /// instructions remain aligned with framework session state.
    mode: SessionModeId,
    /// Validated, secret-redacted MCP server descriptors from `session/new`.
    mcp_servers: Vec<McpServerDescriptor>,
    /// Memory-bounded conversation log (user prompts + agent text chunks as
    /// the client saw them), persisted on close and replayed by
    /// `session/load` (ACP v1 conversation replay).
    conversation: Arc<Mutex<Vec<ConversationMessage>>>,
}

/// One recorded conversation message for `session/load` replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ConversationMessage {
    role: ConversationRole,
    text: String,
}

/// Who produced a recorded conversation message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::enum_variant_names)]
enum ConversationRole {
    User,
    Agent,
    /// Host-derived terminal completion report. Kept distinct from streamed
    /// model text so replay cannot turn it into unsupported model prose.
    FinalResponse,
}

const CONVERSATION_MAX_MESSAGES: usize = 256;

const ASK_MODE_ID: &str = "ask";
const WRITE_MODE_ID: &str = "write";
const PLAN_MODE_ID: &str = "plan";
const PLAN_PAYLOAD_MARKER: &str = "<!-- ee-plan";

fn default_telemetry_attribution() -> TelemetryAttribution {
    TelemetryAttribution::new(
        TelemetryVersionLabels {
            provider_version: "ee-acp".into(),
            model_version: "default".into(),
            prompt_version: "acp-v1".into(),
            manifest_version: "mcp-v1".into(),
            schema_version: "telemetry-v1".into(),
            policy_version: "policy-v1".into(),
            routing_version: "orchestrator-v1".into(),
        },
        TelemetryTransport::Acp,
    )
    .expect("built-in telemetry attribution is valid")
}

fn default_session_mode() -> SessionModeId {
    SessionModeId::new(ASK_MODE_ID)
}

/// Emits typed, evidence-derived completion on the existing ACP text-update
/// surface. No ACP request or response schema changes; a unique message id
/// lets the host pane retain this terminal state separately from model prose.
fn emit_final_response(
    sink: &UpdateSink,
    final_response: &FinalResponse,
    next_final_response: &AtomicU64,
) -> Result<(), ProviderError> {
    let id = next_final_response.fetch_add(1, Ordering::Relaxed);
    sink.agent_message_chunk(format!("ee-final-response-{id}"), final_response.to_string()).map_err(
        |error| ProviderError::BackendFailure(format!("failed to emit final response: {error}")),
    )
}

fn finish_provider_telemetry(
    recorder: &Arc<Mutex<TelemetryRecorder>>,
    turn_id: &str,
    started_at: Instant,
    events: &[crate::events::OrchestratorEvent],
    outcome: TelemetryTurnOutcome,
    terminal_state: Option<crate::completion::CompletionState>,
    evidence_ids: Vec<String>,
) {
    let mut recorder = recorder.lock().expect("telemetry recorder poisoned");
    let elapsed_ms = started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let mut operation_id = 1_u64;
    let mut model_calls = 0_u64;
    let mut tool_calls = 0_u64;
    let mut approval_count = 0_u64;
    let mut retry_count = 0_u64;
    let mut repair_count = 0_u64;
    let mut recovery_count = 0_u64;
    let mut validation_count = 0_u64;
    for event in events {
        match event {
            crate::events::OrchestratorEvent::ModelRequested { .. } => model_calls += 1,
            crate::events::OrchestratorEvent::ToolFinished { tool_name, success, .. } => {
                tool_calls += 1;
                let stage = if crate::strategy::is_validation_tool_name(tool_name) {
                    validation_count += 1;
                    WaterfallStage::Validation
                } else {
                    WaterfallStage::ToolExecution
                };
                let _ = recorder.record_started(
                    turn_id,
                    elapsed_ms,
                    stage,
                    operation_id,
                    Some(tool_name),
                );
                let _ = recorder.record_finished(
                    turn_id,
                    WaterfallFinish {
                        elapsed_ms,
                        stage,
                        operation_id,
                        outcome: if *success {
                            WaterfallOutcome::Succeeded
                        } else {
                            WaterfallOutcome::Failed
                        },
                        tool_failure: (!success).then_some(ToolFailureReason::InternalError),
                        tool_name: Some(tool_name.clone()),
                    },
                );
                operation_id += 1;
            }
            crate::events::OrchestratorEvent::ApprovalRequested { tool_name, .. } => {
                approval_count += 1;
                let _ = recorder.record_started(
                    turn_id,
                    elapsed_ms,
                    WaterfallStage::Approval,
                    operation_id,
                    Some(tool_name),
                );
                let _ = recorder.record_finished(
                    turn_id,
                    WaterfallFinish {
                        elapsed_ms,
                        stage: WaterfallStage::Approval,
                        operation_id,
                        outcome: WaterfallOutcome::Succeeded,
                        tool_failure: None,
                        tool_name: Some(tool_name.clone()),
                    },
                );
                operation_id += 1;
            }
            crate::events::OrchestratorEvent::RetryScheduled { tool_name, .. } => {
                retry_count += 1;
                let _ = recorder.record_started(
                    turn_id,
                    elapsed_ms,
                    WaterfallStage::Retry,
                    operation_id,
                    Some(tool_name),
                );
                let _ = recorder.record_finished(
                    turn_id,
                    WaterfallFinish {
                        elapsed_ms,
                        stage: WaterfallStage::Retry,
                        operation_id,
                        outcome: WaterfallOutcome::Succeeded,
                        tool_failure: None,
                        tool_name: Some(tool_name.clone()),
                    },
                );
                operation_id += 1;
            }
            crate::events::OrchestratorEvent::CheckpointSaved { .. }
            | crate::events::OrchestratorEvent::TurnInterrupted { .. }
            | crate::events::OrchestratorEvent::TurnResumed { .. }
            | crate::events::OrchestratorEvent::RepairStarted { .. }
            | crate::events::OrchestratorEvent::RepairStopped { .. } => {
                match event {
                    crate::events::OrchestratorEvent::RepairStarted { .. }
                    | crate::events::OrchestratorEvent::RepairStopped { .. } => repair_count += 1,
                    _ => recovery_count += 1,
                }
                let _ = recorder.record_started(
                    turn_id,
                    elapsed_ms,
                    WaterfallStage::Recovery,
                    operation_id,
                    None,
                );
                let _ = recorder.record_finished(
                    turn_id,
                    WaterfallFinish {
                        elapsed_ms,
                        stage: WaterfallStage::Recovery,
                        operation_id,
                        outcome: WaterfallOutcome::Succeeded,
                        tool_failure: None,
                        tool_name: None,
                    },
                );
                operation_id += 1;
            }
            _ => {}
        }
    }
    for _ in 0..model_calls {
        let _ = recorder.record_started(
            turn_id,
            elapsed_ms,
            WaterfallStage::ModelCall,
            operation_id,
            None,
        );
        let _ = recorder.record_finished(
            turn_id,
            WaterfallFinish {
                elapsed_ms,
                stage: WaterfallStage::ModelCall,
                operation_id,
                outcome: WaterfallOutcome::Succeeded,
                tool_failure: None,
                tool_name: None,
            },
        );
        operation_id += 1;
    }
    // Exact host approval outcomes remain host-owned and are never inferred.
    let evidence_artifacts = if outcome == TelemetryTurnOutcome::Failed {
        evidence_ids
            .into_iter()
            .filter_map(|id| crate::observability::RedactedEvidenceRef::new(id).ok())
            .collect()
    } else {
        Vec::new()
    };
    let _ = recorder.finish_turn_with_terminal_state(
        turn_id,
        outcome,
        terminal_state,
        TelemetrySummary {
            latency_ms: elapsed_ms,
            approval_count,
            retry_count,
            repair_count,
            recovery_count,
            validation_count,
            tool_calls,
            model_calls,
            estimated_cost_microusd: 0,
            ..TelemetrySummary::default()
        },
        None,
        evidence_artifacts,
    );
}

fn session_modes(current_mode: SessionModeId) -> SessionModeState {
    let mut modes = SessionModeState::new(
        ASK_MODE_ID,
        vec![
            SessionMode::new(ASK_MODE_ID, "Ask"),
            SessionMode::new(WRITE_MODE_ID, "Write"),
            SessionMode::new(PLAN_MODE_ID, "Plan"),
        ],
    );
    modes.current_mode_id = current_mode;
    modes
}

fn mode_policy(base: &PolicyEngine, mode: &SessionModeId) -> Result<PolicyEngine, ProviderError> {
    let mut policy: ToolPolicy = base.policy().clone();
    match mode.to_string().as_str() {
        ASK_MODE_ID => {
            policy.allow_read = true;
            policy.allow_write = false;
            policy.allow_execute = false;
            policy.allow_delegate = false;
            policy.allow_host_approved_side_effects = false;
        }
        PLAN_MODE_ID => {
            policy.allow_read = true;
            policy.allow_write = false;
            policy.allow_execute = false;
            policy.allow_delegate = false;
            policy.allow_host_approved_side_effects = false;
        }
        WRITE_MODE_ID => {}
        _ => {
            return Err(ProviderError::InvalidRequest(format!("unsupported session mode: {mode}")));
        }
    }
    Ok(PolicyEngine::new(policy))
}

fn mode_system_context(system_context: String, mode: &SessionModeId) -> String {
    let instruction = match mode.to_string().as_str() {
        ASK_MODE_ID => {
            "Agent mode: ask. Answer directly; use read-only tools when needed. Do not modify files, run commands, or delegate."
        }
        PLAN_MODE_ID => {
            r#"Agent mode: plan. Investigate with read-only tools when needed, then return a concrete implementation plan, not a general explanation or a promise to investigate later.

Format final response exactly with these sections:
## Plan
1. `<file or symbol>` — exact change, reason, dependency/order, and observable success criterion.
2. Continue one numbered item per independently actionable step.
## Validation
- Name exact tests, checks, or manual verification for the completed implementation; say why validation cannot run when none applies.
## Open questions
- List only blockers that require a user decision; otherwise write `None`.

Every plan step must name affected files or symbols when known, describe an executable change, and state how completion is verified. Do not claim implementation is complete. Do not modify files, run commands, or delegate.

After `## Open questions`, include exactly one machine-readable payload. Keep it synchronized with `## Plan` and use this exact shape:
<!-- ee-plan
[
  {
    "title": "short task title",
    "action": "exact implementation action",
    "scope": "affected file or symbol",
    "expected_result": "observable completed state",
    "verification": "specific test, check, or manual verification",
    "depends_on": ["prior task title or #index"]
  }
]
-->
The payload must contain at least one task. It is hidden from rendered Markdown and becomes the ACP task plan."#
        }
        WRITE_MODE_ID => {
            "Agent mode: write. Implement task with allowed tools. Existing host approval gates remain required for writes and execution."
        }
        _ => "Agent mode: unknown. Do not invoke tools or make changes.",
    };
    format!("{system_context}\n\n{instruction}")
}

fn parse_plan_items(response: &str) -> Result<Vec<crate::plan_compiler::PlanInput>, ProviderError> {
    let (_, payload) = response.split_once(PLAN_PAYLOAD_MARKER).ok_or_else(|| {
        ProviderError::BackendFailure(
            "plan mode response omitted required <!-- ee-plan ... --> payload".to_string(),
        )
    })?;
    let (payload, _) = payload.split_once("-->").ok_or_else(|| {
        ProviderError::BackendFailure(
            "plan mode payload is missing its closing --> marker".to_string(),
        )
    })?;
    let items: Vec<crate::plan_compiler::PlanInput> = serde_json::from_str(payload.trim())
        .map_err(|error| {
            ProviderError::BackendFailure(format!(
                "plan mode payload is not valid plan JSON: {error}"
            ))
        })?;
    if items.is_empty() {
        return Err(ProviderError::BackendFailure(
            "plan mode payload must contain at least one task".to_string(),
        ));
    }
    Ok(items)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedSession {
    #[serde(default = "default_session_mode")]
    mode: SessionModeId,
}

/// ACP provider adapter around [`OrchestratorRuntime`].
///
/// Generic over the injected [`ModelAdapter`].  Instances are cheap to clone
/// (all state is shared behind `Arc`), so providers can keep a probe handle
/// alongside the one handed to the framework server.
pub struct OrchestratorProvider {
    config: OrchestratorProviderConfig,
    models: Arc<crate::model_registry::ModelRegistry>,
    policy: PolicyEngine,
    sessions: Arc<Mutex<HashMap<String, SessionRuntime>>>,
    persisted: Arc<Mutex<HashMap<String, PersistedSession>>>,
    session_store: Arc<SessionStateStore>,
    next_session: Arc<AtomicU64>,
    /// Unique final-response message ids across sessions and turns.
    next_final_response: Arc<AtomicU64>,
    /// User-controlled, local-only per-turn telemetry. Never session-persisted.
    telemetry: Arc<Mutex<TelemetryRecorder>>,
    /// Opaque telemetry IDs independent of ACP session/task identifiers.
    next_telemetry_turn: Arc<AtomicU64>,
}

impl Clone for OrchestratorProvider {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            models: self.models.clone(),
            policy: self.policy.clone(),
            sessions: self.sessions.clone(),
            persisted: self.persisted.clone(),
            session_store: self.session_store.clone(),
            next_session: self.next_session.clone(),
            next_final_response: self.next_final_response.clone(),
            telemetry: self.telemetry.clone(),
            next_telemetry_turn: self.next_telemetry_turn.clone(),
        }
    }
}

mod agent_impl;
mod builder;
mod conversation;

#[cfg(test)]
mod tests;
