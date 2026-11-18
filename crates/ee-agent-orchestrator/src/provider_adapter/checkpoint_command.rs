//! `/revert` checkpoint restoration for the ACP provider surface.
//!
//! Unlike `/branch`, revert keeps the current session id: it restores the
//! checkpointed task, memory, and budget state in place and truncates the
//! model transcript to the logged prefix ending at that checkpoint.  The
//! default mode changes conversation state only; `/revert N --files [paths...]`
//! restores selected workspace files from turn N's pre-turn snapshot without
//! touching the conversation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ee_agent_acp_server::{PromptResult, ProviderError, UpdateSink};
use ee_agent_protocol::{REVERT_COMMAND_NAME, SessionId, StopReason};

use super::branch_command::messages_to_conversation;
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
    /// Primary workspace root used when restoring files.
    pub(super) workspace: &'a std::path::Path,
    /// Additional workspace roots captured by the snapshot store.
    pub(super) workspace_roots: &'a [std::path::PathBuf],
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
    let options = parse_revert_options(ctx.instructions)?;
    if options.files {
        return revert_files(ctx, options);
    }
    let requested_seq = options.seq;
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

#[derive(Debug, Default)]
struct RevertOptions {
    seq: Option<u64>,
    files: bool,
    paths: Vec<String>,
}

/// Parses `/revert [seq] [--files [path...]]`.
fn parse_revert_options(instructions: Option<&str>) -> Result<RevertOptions, ProviderError> {
    let Some(text) = instructions.map(str::trim).filter(|text| !text.is_empty()) else {
        return Ok(RevertOptions::default());
    };
    let mut options = RevertOptions::default();
    for token in text.split_whitespace() {
        if token == "--files" {
            options.files = true;
        } else if !options.files
            && options.seq.is_none()
            && token.chars().all(|c| c.is_ascii_digit())
        {
            options.seq = Some(token.parse::<u64>().map_err(|_| {
                ProviderError::InvalidRequest(format!("invalid revert sequence: {token}"))
            })?);
        } else if options.files {
            options.paths.push(token.to_string());
        } else {
            return Err(ProviderError::InvalidRequest(
                "usage: /revert [seq] [--files [path...]]".into(),
            ));
        }
    }
    Ok(options)
}

/// Restores selected workspace files from a turn's pre-turn snapshot without
/// touching conversation or checkpoint state.
fn revert_files(
    ctx: RevertCommand<'_>,
    options: RevertOptions,
) -> Result<PromptResult, ProviderError> {
    let session_id = ctx.session_id.to_string();
    let snapshots = ctx.runtime.workspace_snapshots();
    let turn = match options.seq {
        Some(turn) => turn,
        None => snapshots.latest_turn(&session_id)?.ok_or_else(|| {
            ProviderError::InvalidRequest(format!(
                "session {session_id} has no captured turns to revert files from"
            ))
        })?,
    };
    let mut paths = options.paths;
    if paths.is_empty() {
        let diff = snapshots.turn_diff(&session_id, turn, ctx.workspace)?.ok_or_else(|| {
            ProviderError::InvalidRequest(format!("turn {turn} has no start/end snapshot pair yet"))
        })?;
        paths = diff.files.into_iter().map(|file| file.path).collect();
    }
    let mut roots = Vec::with_capacity(ctx.workspace_roots.len() + 1);
    roots.push(ctx.workspace.to_path_buf());
    roots.extend(ctx.workspace_roots.iter().cloned());
    let restored =
        ctx.runtime.workspace_snapshots().restore_turn_files(&session_id, turn, &paths, &roots)?;
    let summary = serde_json::json!({
        "command": REVERT_COMMAND_NAME,
        "session_id": session_id,
        "turn": turn,
        "files_mode": true,
        "files_restored": restored,
        "conversation_reverted": false,
        "workspace_files_changed": true,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelAdapter;
    use crate::runtime::OrchestratorRuntime;
    use crate::test_support::FakeModel;
    use tokio::sync::mpsc;

    fn write(path: &std::path::Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent");
        }
        std::fs::write(path, text).expect("write");
    }

    #[test]
    fn revert_options_accept_files_and_paths() {
        let options = parse_revert_options(Some("3 --files a.rs b.rs")).expect("parses");
        assert_eq!(options.seq, Some(3));
        assert!(options.files);
        assert_eq!(options.paths, vec!["a.rs", "b.rs"]);
        let options = parse_revert_options(Some("--files")).expect("parses all");
        assert!(options.files);
        assert!(options.paths.is_empty());
        assert!(parse_revert_options(Some("nope")).is_err());
    }

    #[tokio::test]
    async fn files_mode_restores_selected_paths_and_leaves_the_conversation_untouched() {
        let workspace = tempfile::tempdir().expect("workspace");
        let root = workspace.path();
        write(&root.join("a.rs"), "original-a\n");
        write(&root.join("b.rs"), "original-b\n");
        let runtime = Arc::new(OrchestratorRuntime::new(
            crate::config::OrchestratorConfig::default(),
            Arc::new(FakeModel::new(Vec::new())) as Arc<dyn ModelAdapter>,
        ));
        let record = runtime
            .workspace_snapshots()
            .begin_turn("session-1", &[root.to_path_buf()])
            .expect("capture start");
        write(&root.join("a.rs"), "changed-a\n");
        write(&root.join("b.rs"), "changed-b\n");
        runtime
            .workspace_snapshots()
            .finish_turn("session-1", record.turn, &[root.to_path_buf()])
            .expect("capture end");

        let (tx, _rx) = mpsc::unbounded_channel();
        let sink = UpdateSink::new_for_test(SessionId::new("session-1"), tx);
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        let telemetry = Arc::new(Mutex::new(TelemetryRecorder::default()));
        let instructions = format!("{} --files a.rs", record.turn);
        let result = handle_revert_command(RevertCommand {
            runtime: &runtime,
            sessions: &sessions,
            sink: &sink,
            session_id: &SessionId::new("session-1"),
            instructions: Some(&instructions),
            workspace: root,
            workspace_roots: &[],
            telemetry: &telemetry,
            telemetry_turn: "turn-1",
            telemetry_started_at: Instant::now(),
            telemetry_event_start: 0,
        })
        .expect("revert files succeeds");

        assert_eq!(result.stop_reason, StopReason::EndTurn);
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).expect("a"), "original-a\n");
        assert_eq!(std::fs::read_to_string(root.join("b.rs")).expect("b"), "changed-b\n");
        assert!(sessions.lock().expect("sessions").is_empty(), "conversation state untouched");
    }
}
