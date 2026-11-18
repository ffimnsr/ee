//! `/revert` checkpoint restoration for the ACP provider surface.
//!
//! Unlike `/branch`, revert keeps the current session id: it restores the
//! checkpointed task, memory, and budget state in place and truncates the
//! model transcript to the logged prefix ending at that checkpoint. Workspace
//! files are never modified; a checkpoint carries conversation state only.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ee_agent_acp_server::{PromptResult, ProviderError, UpdateSink};
use ee_agent_protocol::{REVERT_COMMAND_NAME, SessionId, StopReason};

use super::branch_command::{messages_to_conversation, parse_checkpoint_sequence};
use super::{SessionRuntime, finish_provider_telemetry};
use crate::model::ModelMessage;
use crate::observability::{TelemetryRecorder, TelemetryTurnOutcome};
use crate::runtime::OrchestratorRuntime;

/// Inputs required to restore one session to a checkpoint in place.
pub(super) struct RevertCommand<'a> {
    pub(super) runtime: &'a Arc<OrchestratorRuntime>,
    pub(super) sessions: &'a Arc<Mutex<HashMap<String, SessionRuntime>>>,
    pub(super) sink: &'a UpdateSink,
    pub(super) session_id: &'a SessionId,
    pub(super) instructions: Option<&'a str>,
    pub(super) telemetry: &'a Arc<Mutex<TelemetryRecorder>>,
    pub(super) telemetry_turn: &'a str,
    pub(super) telemetry_started_at: Instant,
    pub(super) telemetry_event_start: usize,
}

/// Executes `/revert [checkpoint_seq]` and returns the turn result.
///
/// Missing checkpoints and malformed sequences fail closed before any session
/// state is restored.
pub(super) fn handle_revert_command(ctx: RevertCommand<'_>) -> Result<PromptResult, ProviderError> {
    let requested_seq = parse_checkpoint_sequence(ctx.instructions)?;
    let session_id = ctx.session_id.to_string();
    let checkpoint_store = ctx.runtime.checkpoint_store();
    let checkpoint_seq = match requested_seq {
        Some(seq) => seq,
        None => checkpoint_store.latest_seq(&session_id)?.ok_or_else(|| {
            ProviderError::InvalidRequest(format!(
                "session {session_id} has no checkpoints to revert to"
            ))
        })?,
    };
    let checkpoint = checkpoint_store
        .load_at(&session_id, checkpoint_seq)?
        .map(|(_, checkpoint)| checkpoint)
        .ok_or_else(|| {
            ProviderError::InvalidRequest(format!(
                "checkpoint {checkpoint_seq} for session {session_id} was not found"
            ))
        })?;
    ctx.runtime.restore_from_checkpoint(&checkpoint).map_err(ProviderError::from)?;
    // Truncate the model-visible transcript to the checkpoint prefix. Without
    // a session log the runtime state is still restored, but post-checkpoint
    // conversation cannot be reverted; report that instead of guessing.
    let replayed = ctx
        .runtime
        .replay_session_messages_until(&session_id, checkpoint_seq)
        .map_err(ProviderError::from)?;
    let conversation_reverted = replayed.is_some();
    if let Some(replayed) = replayed {
        let transcript: Vec<ModelMessage> =
            replayed.into_iter().map(|(_, message)| message).collect();
        if let Some(session) =
            ctx.sessions.lock().expect("adapter sessions poisoned").get_mut(&session_id)
        {
            *session.model_transcript.lock().expect("model transcript poisoned") =
                transcript.clone();
            *session.conversation.lock().expect("conversation poisoned") =
                messages_to_conversation(&transcript);
        }
    }
    let summary = serde_json::json!({
        "command": REVERT_COMMAND_NAME,
        "session_id": session_id,
        "checkpoint_seq": checkpoint_seq,
        "conversation_reverted": conversation_reverted,
        "conversation_unreplayed": !conversation_reverted,
        "workspace_files_changed": false,
    });
    let _ = ctx.sink.agent_message_chunk("revert", summary.to_string());
    finish_provider_telemetry(
        ctx.telemetry,
        crate::provider_adapter::ProviderTelemetryTurn {
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
