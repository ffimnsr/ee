//! `/export-session` and `/import-session` transfer commands (I22).
//!
//! Export writes a bounded JSON document; redacted is the default and full
//! content requires `--include-content`. Import validates the document and
//! seeds the current session only when its log is empty, so an existing
//! session id can never be overwritten. The imported messages are restored
//! into both the replay conversation and the normalized model transcript, so
//! the next turn continues from the imported history.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ee_agent_acp_server::{PromptResult, ProviderError, UpdateSink};
use ee_agent_protocol::{
    EXPORT_SESSION_COMMAND_NAME, IMPORT_SESSION_COMMAND_NAME, SessionId, StopReason,
};

use super::conversation::{record_model_transcript, replay_conversation};
use super::{
    CONVERSATION_MAX_MESSAGES, ConversationMessage, ConversationRole, ProviderTelemetryTurn,
    finish_provider_telemetry,
};
use crate::model::{ModelMessage, ModelRole};
use crate::observability::{TelemetryRecorder, TelemetryTurnOutcome};
use crate::runtime::OrchestratorRuntime;
use crate::session_transfer::{
    DEFAULT_MAX_TRANSFER_BYTES, SessionTransferDocument, SessionTransferInfo,
    TransferExportOptions, export_session, import_session,
};

/// Maximum bytes an import accepts from disk.
const MAX_IMPORT_BYTES: u64 = 16 * 1024 * 1024;

/// Inputs for `/export-session`.
pub(super) struct ExportCommand<'a> {
    pub(super) runtime: &'a Arc<OrchestratorRuntime>,
    pub(super) sink: &'a UpdateSink,
    pub(super) session_id: &'a SessionId,
    pub(super) instructions: Option<&'a str>,
    pub(super) info: SessionTransferInfo,
    pub(super) telemetry: &'a Arc<Mutex<TelemetryRecorder>>,
    pub(super) telemetry_turn: &'a str,
    pub(super) telemetry_started_at: Instant,
    pub(super) telemetry_event_start: usize,
}

/// Inputs for `/import-session`.
pub(super) struct ImportCommand<'a> {
    pub(super) runtime: &'a Arc<OrchestratorRuntime>,
    pub(super) sink: &'a UpdateSink,
    pub(super) session_id: &'a SessionId,
    pub(super) instructions: Option<&'a str>,
    pub(super) conversation: &'a Arc<Mutex<Vec<ConversationMessage>>>,
    pub(super) model_transcript: &'a Arc<Mutex<Vec<ModelMessage>>>,
    pub(super) telemetry: &'a Arc<Mutex<TelemetryRecorder>>,
    pub(super) telemetry_turn: &'a str,
    pub(super) telemetry_started_at: Instant,
    pub(super) telemetry_event_start: usize,
}

/// Executes `/export-session [path] [--include-content]`.
pub(super) fn handle_export_command(ctx: ExportCommand<'_>) -> Result<PromptResult, ProviderError> {
    let (path, include_content) = parse_export_args(ctx.instructions)?;
    let Some(log) = ctx.runtime.session_log(ctx.session_id.0.as_ref()) else {
        return Err(ProviderError::InvalidRequest(
            "session log is disabled; enable [orchestrator.session_log] to export sessions".into(),
        ));
    };
    let options = if include_content {
        TransferExportOptions::with_content()
    } else {
        TransferExportOptions::default()
    };
    let document = export_session(&log, ctx.info, options)
        .map_err(|error| ProviderError::BackendFailure(error.to_string()))?;
    let path = path.unwrap_or_else(|| default_export_path(ctx.runtime, ctx.session_id));
    write_document(&path, &document)?;
    let summary = serde_json::json!({
        "command": EXPORT_SESSION_COMMAND_NAME,
        "session_id": ctx.session_id.0.as_ref(),
        "path": path.display().to_string(),
        "messages": document.messages.len(),
        "redacted": document.redacted,
        "truncated": document.truncated,
        "bytes": std::fs::metadata(&path).map(|metadata| metadata.len()).unwrap_or(0),
    });
    let _ = ctx.sink.agent_message_chunk("export-session", summary.to_string());
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

/// Executes `/import-session <path>`.
pub(super) fn handle_import_command(ctx: ImportCommand<'_>) -> Result<PromptResult, ProviderError> {
    let path = ctx
        .instructions
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| ProviderError::InvalidRequest("usage: /import-session <path>".into()))?;
    let path = PathBuf::from(path);
    let document = read_document(&path)?;
    let Some(store) = ctx.runtime.session_log_store() else {
        return Err(ProviderError::InvalidRequest(
            "session log is disabled; enable [orchestrator.session_log] to import sessions".into(),
        ));
    };
    let session_id = ctx.session_id.to_string();
    let existing = store
        .records(&session_id)
        .map_err(|error| ProviderError::BackendFailure(error.to_string()))?;
    if !existing.is_empty() {
        return Err(ProviderError::InvalidRequest(format!(
            "session {session_id} already has {} log records; start a new session before importing",
            existing.len()
        )));
    }
    let outcome = import_session(&store, &document, Some(&session_id))
        .map_err(|error| ProviderError::InvalidRequest(error.to_string()))?;
    let messages = document_messages(&document);
    record_model_transcript(ctx.model_transcript, messages.clone());
    {
        let mut conversation = ctx.conversation.lock().expect("conversation poisoned");
        conversation.clear();
        for message in &messages {
            let text = message.text_content();
            if text.trim().is_empty() {
                continue;
            }
            let role = match message.role {
                ModelRole::User => ConversationRole::User,
                ModelRole::Assistant | ModelRole::Subagent => ConversationRole::Agent,
                _ => continue,
            };
            conversation.push(ConversationMessage { role, text });
        }
        if conversation.len() > CONVERSATION_MAX_MESSAGES {
            let overflow = conversation.len() - CONVERSATION_MAX_MESSAGES;
            conversation.drain(..overflow);
        }
    }
    replay_conversation(Some(ctx.sink), &ctx.conversation.lock().expect("conversation poisoned"))?;
    let summary = serde_json::json!({
        "command": IMPORT_SESSION_COMMAND_NAME,
        "session_id": outcome.session_id,
        "path": path.display().to_string(),
        "messages": outcome.messages,
        "redacted": document.redacted,
        "truncated": document.truncated,
    });
    let _ = ctx.sink.agent_message_chunk("import-session", summary.to_string());
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

fn parse_export_args(instructions: Option<&str>) -> Result<(Option<PathBuf>, bool), ProviderError> {
    let mut path = None;
    let mut include_content = false;
    for token in instructions.unwrap_or_default().split_whitespace() {
        match token {
            "--include-content" => include_content = true,
            other if other.starts_with('-') => {
                return Err(ProviderError::InvalidRequest(format!(
                    "usage: /export-session [path] [--include-content]; unknown flag {other}"
                )));
            }
            other => {
                if path.is_some() {
                    return Err(ProviderError::InvalidRequest(
                        "usage: /export-session [path] [--include-content]".into(),
                    ));
                }
                path = Some(PathBuf::from(other));
            }
        }
    }
    Ok((path, include_content))
}

fn default_export_path(runtime: &OrchestratorRuntime, session_id: &SessionId) -> PathBuf {
    let directory = runtime
        .config()
        .session_log
        .dir
        .clone()
        .map(|dir| dir.join("exports"))
        .unwrap_or_else(|| std::env::temp_dir().join("ee-session-exports"));
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let name = crate::session_log_sink::sanitize_component(session_id.0.as_ref());
    directory.join(format!("{name}-{stamp}.json"))
}

fn write_document(path: &Path, document: &SessionTransferDocument) -> Result<(), ProviderError> {
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|error| {
            ProviderError::BackendFailure(format!(
                "cannot create export directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    let bytes = serde_json::to_vec_pretty(document).map_err(|error| {
        ProviderError::BackendFailure(format!("cannot serialize session export: {error}"))
    })?;
    if bytes.len() > DEFAULT_MAX_TRANSFER_BYTES {
        return Err(ProviderError::BackendFailure(format!(
            "session export is {} bytes, over the {DEFAULT_MAX_TRANSFER_BYTES} byte cap",
            bytes.len()
        )));
    }
    std::fs::write(path, bytes).map_err(|error| {
        ProviderError::BackendFailure(format!(
            "cannot write session export {}: {error}",
            path.display()
        ))
    })
}

fn read_document(path: &Path) -> Result<SessionTransferDocument, ProviderError> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        ProviderError::InvalidRequest(format!("cannot read {}: {error}", path.display()))
    })?;
    if metadata.len() > MAX_IMPORT_BYTES {
        return Err(ProviderError::InvalidRequest(format!(
            "session import {} is {} bytes, over the {MAX_IMPORT_BYTES} byte cap",
            path.display(),
            metadata.len()
        )));
    }
    let bytes = std::fs::read(path).map_err(|error| {
        ProviderError::InvalidRequest(format!("cannot read {}: {error}", path.display()))
    })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        ProviderError::InvalidRequest(format!(
            "session import {} is not a valid transfer document: {error}",
            path.display()
        ))
    })
}

fn document_messages(document: &SessionTransferDocument) -> Vec<ModelMessage> {
    document
        .messages
        .iter()
        .map(|message| {
            let mut value = ModelMessage::new(message.role).with_trust(message.trust);
            value.content = message.content.clone();
            value
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OrchestratorConfig;
    use crate::model::ModelRole;
    use crate::session_log::SessionLogStore;
    use crate::test_support::FakeModel;
    use ee_agent_acp_server::server::OutboundEvent;
    use ee_agent_protocol::SessionId;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn runtime_with_log(dir: &Path) -> Arc<OrchestratorRuntime> {
        let mut config = OrchestratorConfig::default();
        config.session_log.dir = Some(dir.to_path_buf());
        Arc::new(OrchestratorRuntime::new(
            config,
            Arc::new(FakeModel::new(Vec::new())) as Arc<dyn crate::model::ModelAdapter>,
        ))
    }

    #[tokio::test]
    async fn export_then_import_round_trips_a_session() {
        let temp = tempfile::tempdir().expect("temp");
        let runtime = runtime_with_log(temp.path());
        let store = SessionLogStore::from_config(&runtime.config().session_log).expect("store");
        let log = store.log_for("session-1");
        log.record_message(&ModelMessage::text(ModelRole::User, "hello"), None).expect("user");
        log.record_message(&ModelMessage::text(ModelRole::Assistant, "world"), None)
            .expect("assistant");
        log.record_usage(
            &crate::usage::ModelUsage::new().with_input_tokens(10).with_output_tokens(4),
            Some("model-a"),
        )
        .expect("usage");

        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = UpdateSink::new_for_test(SessionId::new("session-1"), tx);
        let telemetry = Arc::new(Mutex::new(TelemetryRecorder::default()));
        let export_path = temp.path().join("session.json");
        let instructions = format!("{} --include-content", export_path.display());
        handle_export_command(ExportCommand {
            runtime: &runtime,
            sink: &sink,
            session_id: &SessionId::new("session-1"),
            instructions: Some(&instructions),
            info: SessionTransferInfo::default(),
            telemetry: &telemetry,
            telemetry_turn: "turn-1",
            telemetry_started_at: Instant::now(),
            telemetry_event_start: 0,
        })
        .expect("export");
        assert!(export_path.exists());
        let _ = rx.recv().await.expect("export update");

        let conversation = Arc::new(Mutex::new(Vec::new()));
        let transcript = Arc::new(Mutex::new(Vec::new()));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = UpdateSink::new_for_test(SessionId::new("fresh"), tx);
        handle_import_command(ImportCommand {
            runtime: &runtime,
            sink: &sink,
            session_id: &SessionId::new("fresh"),
            instructions: Some(&export_path.display().to_string()),
            conversation: &conversation,
            model_transcript: &transcript,
            telemetry: &telemetry,
            telemetry_turn: "turn-2",
            telemetry_started_at: Instant::now(),
            telemetry_event_start: 0,
        })
        .expect("import");
        assert_eq!(transcript.lock().expect("transcript").len(), 2);
        assert_eq!(conversation.lock().expect("conversation").len(), 2);
        let replayed = store.log_for("fresh").replay_messages().expect("replay");
        assert_eq!(replayed.len(), 2);
        let records = store.records("fresh").expect("records");
        assert!(
            records
                .iter()
                .any(|record| matches!(record, crate::session_log::SessionLogRecord::Usage(_))),
            "imported usage lands in the log"
        );
        let emitted: Vec<OutboundEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            emitted.iter().any(|event| matches!(event, OutboundEvent::Update { .. })),
            "import replays restored messages"
        );
    }

    #[test]
    fn export_defaults_to_redacted() {
        let (path, include) = parse_export_args(Some("out.json")).expect("args");
        assert_eq!(path, Some(PathBuf::from("out.json")));
        assert!(!include);
        let (_, include) = parse_export_args(Some("--include-content")).expect("args");
        assert!(include);
        assert!(parse_export_args(Some("--nope")).is_err());
    }

    #[tokio::test]
    async fn export_uses_the_live_in_memory_log() {
        let runtime = Arc::new(OrchestratorRuntime::new(
            OrchestratorConfig::default(),
            Arc::new(FakeModel::new(Vec::new())) as Arc<dyn crate::model::ModelAdapter>,
        ));
        let log = runtime.session_log("session-1").expect("live log");
        log.record_message(&ModelMessage::text(ModelRole::User, "memory only"), None)
            .expect("user");

        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("memory.json");
        let (tx, _rx) = mpsc::unbounded_channel();
        let sink = UpdateSink::new_for_test(SessionId::new("session-1"), tx);
        let telemetry = Arc::new(Mutex::new(TelemetryRecorder::default()));
        handle_export_command(ExportCommand {
            runtime: &runtime,
            sink: &sink,
            session_id: &SessionId::new("session-1"),
            instructions: Some(&path.display().to_string()),
            info: SessionTransferInfo::default(),
            telemetry: &telemetry,
            telemetry_turn: "turn-1",
            telemetry_started_at: Instant::now(),
            telemetry_event_start: 0,
        })
        .expect("memory export");
        let document: SessionTransferDocument =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        assert_eq!(document.messages.len(), 1, "live in-memory log fed the export");
    }
}
