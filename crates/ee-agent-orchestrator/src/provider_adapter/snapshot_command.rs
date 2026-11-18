//! `/turn-diff [turn]` per-turn workspace diff reporting.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use ee_agent_acp_server::{PromptResult, ProviderError, UpdateSink};
use ee_agent_protocol::{DIFF_COMMAND_NAME, SessionId, StopReason};

use super::{ProviderTelemetryTurn, finish_provider_telemetry};
use crate::observability::{TelemetryRecorder, TelemetryTurnOutcome};
use crate::runtime::OrchestratorRuntime;
use crate::workspace_snapshot::TurnDiff;

/// Maximum serialized diff output emitted in one command response.
const MAX_DIFF_OUTPUT_BYTES: usize = 256 * 1024;

/// Inputs required to report one turn's workspace diff.
pub(super) struct DiffCommand<'a> {
    pub(super) runtime: &'a Arc<OrchestratorRuntime>,
    pub(super) sink: &'a UpdateSink,
    pub(super) session_id: &'a SessionId,
    pub(super) instructions: Option<&'a str>,
    pub(super) workspace: &'a std::path::Path,
    pub(super) telemetry: &'a Arc<Mutex<TelemetryRecorder>>,
    pub(super) telemetry_turn: &'a str,
    pub(super) telemetry_started_at: Instant,
    pub(super) telemetry_event_start: usize,
}

/// Executes `/diff [turn]` and returns the turn result.
pub(super) fn handle_diff_command(ctx: DiffCommand<'_>) -> Result<PromptResult, ProviderError> {
    let session_id = ctx.session_id.to_string();
    let snapshots = ctx.runtime.workspace_snapshots();
    let turn = match ctx.instructions.map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) => text.parse::<u64>().map_err(|_| {
            ProviderError::InvalidRequest(format!(
                "usage: /turn-diff [turn]; invalid turn sequence: {text}"
            ))
        })?,
        None => snapshots.latest_turn(&session_id)?.ok_or_else(|| {
            ProviderError::InvalidRequest(format!(
                "session {session_id} has no captured turns to diff"
            ))
        })?,
    };
    let mut diff = snapshots.turn_diff(&session_id, turn, ctx.workspace)?.ok_or_else(|| {
        ProviderError::InvalidRequest(format!(
            "turn {turn} has no start/end snapshot pair; wait for the turn to settle"
        ))
    })?;
    bound_output(&mut diff);
    let summary = serde_json::json!({
        "command": DIFF_COMMAND_NAME,
        "session_id": session_id,
        "diff": diff,
    });
    let _ = ctx.sink.agent_message_chunk("turn-diff", summary.to_string());
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

/// Drops patches once the cumulative output reaches the byte cap so one noisy
/// turn cannot flood the transcript.
fn bound_output(diff: &mut TurnDiff) {
    let mut total = 0usize;
    for file in &mut diff.files {
        let Some(patch) = &file.patch else { continue };
        total += patch.len();
        if total > MAX_DIFF_OUTPUT_BYTES {
            file.patch = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelAdapter;
    use crate::test_support::FakeModel;
    use ee_agent_acp_server::server::OutboundEvent;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn turn_diff_command_emits_the_changed_files() {
        let workspace = tempfile::tempdir().expect("workspace");
        let root = workspace.path();
        std::fs::write(root.join("a.rs"), "before\n").expect("seed");
        let runtime = Arc::new(OrchestratorRuntime::new(
            crate::config::OrchestratorConfig::default(),
            Arc::new(FakeModel::new(Vec::new())) as Arc<dyn ModelAdapter>,
        ));
        let record = runtime
            .workspace_snapshots()
            .begin_turn("session-1", &[root.to_path_buf()])
            .expect("capture start");
        std::fs::write(root.join("a.rs"), "after\n").expect("edit");
        runtime
            .workspace_snapshots()
            .finish_turn("session-1", record.turn, &[root.to_path_buf()])
            .expect("capture end");

        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = UpdateSink::new_for_test(SessionId::new("session-1"), tx);
        let telemetry = Arc::new(Mutex::new(TelemetryRecorder::default()));
        let instructions = record.turn.to_string();
        let result = handle_diff_command(DiffCommand {
            runtime: &runtime,
            sink: &sink,
            session_id: &SessionId::new("session-1"),
            instructions: Some(&instructions),
            workspace: root,
            telemetry: &telemetry,
            telemetry_turn: "turn-1",
            telemetry_started_at: Instant::now(),
            telemetry_event_start: 0,
        })
        .expect("diff command succeeds");
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        let event = rx.recv().await.expect("diff update emitted");
        let OutboundEvent::Update { update, .. } = event else {
            panic!("expected a session update, got {event:?}");
        };
        let text = serde_json::to_string(&update).expect("update serializes");
        assert!(text.contains("a.rs"), "diff names the changed file: {text}");
    }
}
