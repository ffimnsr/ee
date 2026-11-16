//! `/branch` checkpoint forking for the ACP provider surface.
//!
//! Extracted from `agent_impl` so the provider turn stays readable and the
//! branch flow can be tested and reviewed on its own. The command forks the
//! active session from a stored checkpoint, registers the branched session,
//! replays the log prefix when one exists, and emits a structured summary.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ee_agent_acp_server::{PromptResult, ProviderError, UpdateSink};
use ee_agent_protocol::{BRANCH_COMMAND_NAME, SessionId, SessionModeId, StopReason};

use super::conversation::session_snapshot;
use super::{
    ConversationMessage, ConversationRole, McpServerDescriptor, ProviderTelemetryTurn,
    SessionRuntime, SessionStateStore, finish_provider_telemetry, mode_policy,
};
use crate::model::{ModelMessage, ModelRole};
use crate::observability::{TelemetryRecorder, TelemetryTurnOutcome};
use crate::plugin_runtime::AgentPluginHost;
use crate::policy::PolicyEngine;
use crate::runtime::OrchestratorRuntime;

/// Inputs required to fork one session from a checkpoint.
pub(super) struct BranchCommand<'a> {
    pub(super) runtime: &'a Arc<OrchestratorRuntime>,
    pub(super) plugins: &'a Arc<AgentPluginHost>,
    pub(super) models: &'a Arc<crate::model_registry::ModelRegistry>,
    pub(super) sessions: &'a Arc<Mutex<HashMap<String, SessionRuntime>>>,
    pub(super) session_store: &'a SessionStateStore,
    pub(super) sink: &'a UpdateSink,
    pub(super) session_id: &'a SessionId,
    pub(super) instructions: Option<&'a str>,
    pub(super) base_policy: &'a PolicyEngine,
    pub(super) mode: &'a SessionModeId,
    pub(super) workspace: &'a std::path::Path,
    pub(super) workspace_roots: &'a [std::path::PathBuf],
    pub(super) provider_label: &'a str,
    pub(super) model_label: &'a str,
    pub(super) mcp_servers: &'a [McpServerDescriptor],
    pub(super) implementation_name: &'a str,
    pub(super) telemetry: &'a Arc<Mutex<TelemetryRecorder>>,
    pub(super) telemetry_turn: &'a str,
    pub(super) telemetry_started_at: Instant,
    pub(super) telemetry_event_start: usize,
}

/// Executes `/branch [checkpoint_seq]` and returns the turn result.
///
/// Missing checkpoints and malformed sequences fail closed before the active
/// session state is touched.
pub(super) fn handle_branch_command(ctx: BranchCommand<'_>) -> Result<PromptResult, ProviderError> {
    let requested_seq = parse_checkpoint_sequence(ctx.instructions)?;
    let source_id = ctx.session_id.to_string();
    let checkpoint_store = ctx.runtime.checkpoint_store();
    let checkpoint_seq = match requested_seq {
        Some(seq) => seq,
        None => checkpoint_store.latest_seq(&source_id)?.ok_or_else(|| {
            ProviderError::InvalidRequest(format!(
                "session {source_id} has no checkpoints to branch from"
            ))
        })?,
    };
    let outcome = ctx
        .runtime
        .branch_from_checkpoint(&source_id, checkpoint_seq)
        .map_err(ProviderError::from)?;
    let checkpoint = checkpoint_store
        .load_at(&source_id, checkpoint_seq)?
        .map(|(_, checkpoint)| checkpoint)
        .ok_or_else(|| {
            ProviderError::InvalidRequest(format!(
                "checkpoint {checkpoint_seq} for session {source_id} was not found"
            ))
        })?;
    let branch_policy = ctx.plugins.gate_policy(mode_policy(ctx.base_policy, ctx.mode)?);
    let branch_runtime = Arc::new(
        OrchestratorRuntime::from_checkpoint_with_model_registry(
            &checkpoint,
            ctx.models.clone(),
            branch_policy,
        )?
        .with_plugin_observer(ctx.plugins.bridge().observer(outcome.session_id.clone())),
    );
    branch_runtime.register_builtins(&SessionId::new(outcome.session_id.clone())).map_err(
        |error| {
            ProviderError::BackendFailure(format!("failed to register built-in tools: {error}"))
        },
    )?;
    ctx.plugins.session_scope(&outcome.session_id).map_err(ProviderError::from)?;
    let replayed = if outcome.conversation_replayed {
        ctx.runtime.replay_session_messages(&outcome.session_id).map_err(ProviderError::from)?
    } else {
        Vec::new()
    };
    let branch_transcript: Vec<ModelMessage> =
        replayed.into_iter().map(|(_, message)| message).collect();
    let mut branch_conversation = messages_to_conversation(&branch_transcript);
    if !outcome.conversation_replayed {
        branch_conversation.push(ConversationMessage {
            role: ConversationRole::Agent,
            text: String::from(
                "conversation_unreplayed: no session log was available for this checkpoint; task, memory, and budget state were adopted",
            ),
        });
    }
    let entry = SessionRuntime {
        runtime: branch_runtime,
        workspace: ctx.workspace.to_path_buf(),
        workspace_roots: ctx.workspace_roots.to_vec(),
        provider_label: ctx.provider_label.to_string(),
        model_label: ctx.model_label.to_string(),
        mode: ctx.mode.clone(),
        mcp_servers: ctx.mcp_servers.to_vec(),
        conversation: Arc::new(Mutex::new(branch_conversation)),
        model_transcript: Arc::new(Mutex::new(branch_transcript)),
    };
    ctx.session_store
        .save(
            ctx.implementation_name,
            ctx.workspace,
            &outcome.session_id,
            &session_snapshot(&entry),
        )
        .map_err(ProviderError::from)?;
    ctx.sessions
        .lock()
        .expect("adapter sessions poisoned")
        .insert(outcome.session_id.clone(), entry);
    let summary = serde_json::json!({
        "command": BRANCH_COMMAND_NAME,
        "session_id": outcome.session_id,
        "source_session_id": outcome.source_session_id,
        "checkpoint_seq": outcome.checkpoint_seq,
        "conversation_replayed": outcome.conversation_replayed,
        "conversation_unreplayed": !outcome.conversation_replayed,
        "replayed_messages": outcome.replayed_messages,
    });
    let _ = ctx.sink.agent_message_chunk("branch", summary.to_string());
    finish_provider_telemetry(
        ctx.telemetry,
        ProviderTelemetryTurn {
            turn_id: ctx.telemetry_turn,
            started_at: ctx.telemetry_started_at,
            events: &ctx.runtime.event_snapshot()[ctx.telemetry_event_start..],
            outcome: TelemetryTurnOutcome::Succeeded,
            terminal_state: None,
            evidence_ids: Vec::new(),
            usage: None,
        },
    );
    Ok(PromptResult::new(StopReason::EndTurn))
}

/// Parses the optional `/branch <checkpoint_seq>` argument. Absence means "use
/// the newest checkpoint"; anything else must be a sequence number.
fn parse_checkpoint_sequence(instructions: Option<&str>) -> Result<Option<u64>, ProviderError> {
    let Some(text) = instructions.map(str::trim).filter(|text| !text.is_empty()) else {
        return Ok(None);
    };
    let seq = text.parse::<u64>().map_err(|_| {
        ProviderError::InvalidRequest(format!(
            "invalid checkpoint sequence {text:?}; expected a non-negative integer"
        ))
    })?;
    Ok(Some(seq))
}

/// Projects normalized model messages onto the ACP replay conversation. Tool,
/// system, and subagent-only records have no client-visible text counterpart.
fn messages_to_conversation(messages: &[ModelMessage]) -> Vec<ConversationMessage> {
    messages
        .iter()
        .filter_map(|message| {
            let role = match message.role {
                ModelRole::User => ConversationRole::User,
                ModelRole::Assistant => ConversationRole::Agent,
                _ => return None,
            };
            let text = message.text_content();
            (!text.is_empty()).then_some(ConversationMessage { role, text })
        })
        .collect()
}
