//! Durable append-only session log.
//!
//! The [`Transcript`](crate::model::Transcript) is a bounded request view: it
//! drops its oldest messages to stay inside the memory limit and checkpoints
//! intentionally retain no message content. This module adds the durable side
//! of that split.
//!
//! Every message that becomes visible to a model request is appended here
//! first; message bodies are content-addressed files under an owner-only state
//! directory, and each model request records the ordered message ids it used
//! together with the tool-schema and projection hashes. Windowed truncation is
//! therefore a view operation only and never deletes log records.
//!
//! Export paths redact secret-like values. Internal reconstruction keeps the
//! original content.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::budget::BudgetSnapshot;
use crate::error::OrchestratorError;
use crate::model::{ModelContent, ModelMessage, ModelRole};
use crate::private_fs::hex;
use crate::sensitive_data::SensitiveDataGuard;
use crate::session_log_sink::sanitize_component;
use crate::tools::ToolDefinition;
use crate::trust::TrustLevel;

pub use crate::session_log_sink::{FileSessionLogSink, InMemorySessionLogSink, SessionLogSink};

/// Current on-disk session-log format version.
pub const SESSION_LOG_SCHEMA_VERSION: u32 = 1;
/// File name of the append-only record stream inside one session directory.
pub const SESSION_LOG_RECORD_FILE: &str = "records.jsonl";
/// Default per-session serialized record cap.
pub const DEFAULT_MAX_SESSION_LOG_BYTES: usize = 32 * 1024 * 1024;
/// Default per-session record-count cap.
pub const DEFAULT_MAX_SESSION_LOG_RECORDS: usize = 100_000;
/// Maximum characters of redacted message text emitted by the export path.
pub const EXPORT_PREVIEW_MAX_CHARS: usize = 512;

/// Session-log configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionLogConfig {
    /// Whether the runtime records model-visible messages and requests.
    pub enabled: bool,
    /// Durable root directory. `None` keeps the log in memory for the life of
    /// the process (no crash reconstruction).
    pub dir: Option<PathBuf>,
    /// Serialized record + body bytes retained per session.
    pub max_session_bytes: usize,
    /// Records retained per session.
    pub max_records: usize,
}

impl Default for SessionLogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: None,
            max_session_bytes: DEFAULT_MAX_SESSION_LOG_BYTES,
            max_records: DEFAULT_MAX_SESSION_LOG_RECORDS,
        }
    }
}

impl SessionLogConfig {
    /// Enables durable, crash-reconstructable logs under `dir`.
    #[must_use]
    pub fn durable(dir: PathBuf) -> Self {
        Self { dir: Some(dir), ..Self::default() }
    }

    /// Rejects caps that would disable recording on the first append.
    pub fn validate(&self) -> Result<(), OrchestratorError> {
        if self.max_session_bytes == 0 || self.max_records == 0 {
            return Err(OrchestratorError::InvalidState(
                "session log caps must be positive when enabled".into(),
            ));
        }
        Ok(())
    }
}

/// One message appended to the durable log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionMessageRecord {
    /// Monotonic append sequence within the session.
    pub seq: u64,
    /// Stable message id used by model-request records.
    pub message_id: String,
    /// Session that owns the message.
    pub session_id: String,
    /// Normalized message role.
    pub role: ModelRole,
    /// Trust level the model saw.
    pub trust: TrustLevel,
    /// Content-addressed body hash (SHA-256 hex).
    pub content_hash: String,
    /// Serialized body size in bytes.
    pub content_bytes: usize,
    /// Turn number that appended the message.
    pub turn: u64,
    /// Loop iteration that appended the message.
    pub iteration: usize,
    /// Tool-call id when the message is a tool observation.
    pub tool_call_id: Option<String>,
}

/// One model request recorded before the model call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelRequestRecord {
    /// Monotonic append sequence within the session.
    pub seq: u64,
    /// Stable request id (`req-<n>`).
    pub request_id: String,
    /// Session that owns the request.
    pub session_id: String,
    /// Ordered message ids visible to the model.
    pub message_ids: Vec<String>,
    /// Hash of the advertised tool schemas.
    pub tool_schema_hash: String,
    /// Hash of the effective post-prune request projection.
    pub projection_hash: String,
    /// Budget state before the call.
    pub budget: BudgetSnapshot,
    /// Adapter registry id, when known.
    pub model_id: Option<String>,
    /// Turn number.
    pub turn: u64,
    /// Loop iteration.
    pub iteration: usize,
}

/// One truncation event applied to the bounded request view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TruncationRecord {
    /// Monotonic append sequence within the session.
    pub seq: u64,
    /// Session that recorded the truncation.
    pub session_id: String,
    /// Oldest messages dropped from the request view.
    pub removed_messages: usize,
    /// Bytes dropped with those messages.
    pub removed_bytes: usize,
    /// Bytes retained in the request view.
    pub retained_bytes: usize,
    /// Limit that triggered the truncation.
    pub limit_bytes: usize,
}

/// One append-only log record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SessionLogRecord {
    /// A turn started.
    TurnStarted {
        /// Append sequence.
        seq: u64,
        /// Session id.
        session_id: String,
        /// Root task id.
        task_id: String,
    },
    /// A turn stopped.
    TurnStopped {
        /// Append sequence.
        seq: u64,
        /// Session id.
        session_id: String,
        /// Explicit stop reason.
        stop_reason: String,
    },
    /// A model-visible message was appended.
    Message(SessionMessageRecord),
    /// A model request was recorded.
    ModelRequest(Box<ModelRequestRecord>),
    /// The bounded request view dropped oldest messages.
    Truncation(TruncationRecord),
    /// A recovery checkpoint was persisted.
    Checkpoint {
        /// Append sequence.
        seq: u64,
        /// Session id.
        session_id: String,
        /// Stable checkpoint id.
        checkpoint_id: String,
        /// Numeric checkpoint sequence parsed from the id.
        checkpoint_seq: u64,
    },
    /// A compaction funnel result was applied.
    Compaction {
        /// Append sequence.
        seq: u64,
        /// Session id.
        session_id: String,
        /// Read-only tool results pruned.
        pruned_tool_results: usize,
        /// Byte-identical duplicate messages elided.
        deduped_messages: usize,
        /// Oldest messages dropped from the compaction view.
        dropped_messages: usize,
    },
    /// A new session was branched from a checkpoint.
    Branch {
        /// Append sequence.
        seq: u64,
        /// New session id.
        session_id: String,
        /// Session the branch was taken from.
        source_session_id: String,
        /// Checkpoint sequence the branch starts at.
        checkpoint_seq: u64,
        /// Whether the conversation prefix was replayed into this session.
        conversation_replayed: bool,
    },
}

impl SessionLogRecord {
    /// Append sequence of the record.
    #[must_use]
    pub fn seq(&self) -> u64 {
        match self {
            Self::TurnStarted { seq, .. }
            | Self::TurnStopped { seq, .. }
            | Self::Checkpoint { seq, .. }
            | Self::Compaction { seq, .. }
            | Self::Branch { seq, .. } => *seq,
            Self::Message(record) => record.seq,
            Self::ModelRequest(record) => record.seq,
            Self::Truncation(record) => record.seq,
        }
    }

    /// Session id that owns the record.
    #[must_use]
    pub fn session_id(&self) -> &str {
        match self {
            Self::TurnStarted { session_id, .. }
            | Self::TurnStopped { session_id, .. }
            | Self::Checkpoint { session_id, .. }
            | Self::Compaction { session_id, .. }
            | Self::Branch { session_id, .. } => session_id,
            Self::Message(record) => &record.session_id,
            Self::ModelRequest(record) => &record.session_id,
            Self::Truncation(record) => &record.session_id,
        }
    }

    /// Rewrites the sequence and session id when replaying into a branch.
    fn resequence(self, seq: u64, session_id: &str) -> Self {
        match self {
            Self::Message(mut record) => {
                record.seq = seq;
                record.session_id = session_id.to_string();
                Self::Message(record)
            }
            Self::ModelRequest(mut record) => {
                record.seq = seq;
                record.session_id = session_id.to_string();
                Self::ModelRequest(record)
            }
            Self::Truncation(mut record) => {
                record.seq = seq;
                record.session_id = session_id.to_string();
                Self::Truncation(record)
            }
            other => other,
        }
    }
}

/// Outcome of branching a session from a checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BranchOutcome {
    /// New session id.
    pub session_id: String,
    /// Session the branch was taken from.
    pub source_session_id: String,
    /// Checkpoint sequence the branch starts at.
    pub checkpoint_seq: u64,
    /// Whether the conversation prefix was replayed from the log.
    pub conversation_replayed: bool,
    /// Messages replayed into the new session.
    pub replayed_messages: usize,
}

impl BranchOutcome {
    /// Branch that could not replay because no log existed; only task, memory,
    /// and budget state can be adopted.
    #[must_use]
    pub fn conversation_unreplayed(
        session_id: impl Into<String>,
        source_session_id: impl Into<String>,
        checkpoint_seq: u64,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            source_session_id: source_session_id.into(),
            checkpoint_seq,
            conversation_replayed: false,
            replayed_messages: 0,
        }
    }
}

#[derive(Debug)]
struct LogState {
    initialized: bool,
    next_seq: u64,
    next_message: u64,
    next_request: u64,
    turn: u64,
    iteration: usize,
    bytes: usize,
    records: usize,
    failure: Option<String>,
}

impl LogState {
    fn new() -> Self {
        Self {
            initialized: false,
            next_seq: 1,
            next_message: 1,
            next_request: 1,
            turn: 0,
            iteration: 0,
            bytes: 0,
            records: 0,
            failure: None,
        }
    }
}

/// Per-session durable log handle with monotonic ids and bounded retention.
#[derive(Debug)]
pub struct SessionLog {
    session_id: String,
    sink: Arc<dyn SessionLogSink>,
    config: SessionLogConfig,
    state: Mutex<LogState>,
}

impl SessionLog {
    /// Creates a log over `sink` for `session_id`.
    #[must_use]
    pub fn with_sink(
        session_id: impl Into<String>,
        sink: Arc<dyn SessionLogSink>,
        config: SessionLogConfig,
    ) -> Self {
        Self { session_id: session_id.into(), sink, config, state: Mutex::new(LogState::new()) }
    }

    /// Creates an in-memory log (tests and memory-only runtimes).
    #[must_use]
    pub fn in_memory(session_id: impl Into<String>) -> Self {
        Self::with_sink(
            session_id,
            Arc::new(InMemorySessionLogSink::new()),
            SessionLogConfig::default(),
        )
    }

    /// Session id this log belongs to.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Sink backing this log.
    #[must_use]
    pub fn sink(&self) -> Arc<dyn SessionLogSink> {
        self.sink.clone()
    }

    /// Starts a new turn, resetting the recorded iteration.
    pub fn begin_turn(&self) -> u64 {
        let mut state = self.state.lock().expect("session log state poisoned");
        state.turn += 1;
        state.iteration = 0;
        state.turn
    }

    /// Records the current loop iteration.
    pub fn set_iteration(&self, iteration: usize) {
        self.state.lock().expect("session log state poisoned").iteration = iteration;
    }

    /// Records a turn start.
    pub fn record_turn_started(&self, task_id: &str) -> Result<(), OrchestratorError> {
        let session_id = self.session_id.clone();
        let task_id = task_id.to_string();
        self.append_record(move |seq| SessionLogRecord::TurnStarted { seq, session_id, task_id })
    }

    /// Records a turn stop.
    pub fn record_turn_stopped(&self, stop_reason: &str) -> Result<(), OrchestratorError> {
        let session_id = self.session_id.clone();
        let stop_reason = stop_reason.to_string();
        self.append_record(move |seq| SessionLogRecord::TurnStopped {
            seq,
            session_id,
            stop_reason,
        })
    }

    /// Appends one model-visible message and returns its stable id.
    pub fn record_message(
        &self,
        message: &ModelMessage,
        tool_call_id: Option<&str>,
    ) -> Result<String, OrchestratorError> {
        let body = serde_json::to_vec(message).map_err(|error| {
            OrchestratorError::Serialization(format!(
                "session log message serialization failed: {error}"
            ))
        })?;
        let content_hash = hex(&Sha256::digest(&body));
        let content_bytes = body.len();
        let mut state = self.state.lock().expect("session log state poisoned");
        self.ensure_initialized(&mut state)?;
        if let Err(error) = self.check_capacity(&state, body.len()) {
            state.failure = Some(error.to_string());
            return Err(error);
        }
        let message_id = format!("msg-{}", state.next_message);
        let turn = state.turn;
        let iteration = state.iteration;
        let session_id = self.session_id.clone();
        let seq = state.next_seq;
        let record = SessionLogRecord::Message(SessionMessageRecord {
            seq,
            message_id: message_id.clone(),
            session_id,
            role: message.role,
            trust: message.trust,
            content_hash: content_hash.clone(),
            content_bytes,
            turn,
            iteration,
            tool_call_id: tool_call_id.map(str::to_string),
        });
        self.sink.put_body(&self.session_id, &content_hash, &body)?;
        state.next_message += 1;
        self.commit(&mut state, record).inspect_err(|error| {
            state.failure = Some(error.to_string());
        })?;
        Ok(message_id)
    }

    /// Records one model request over ordered message ids.
    pub fn record_model_request(
        &self,
        message_ids: Vec<String>,
        tools: &[ToolDefinition],
        projection_hash: String,
        budget: &BudgetSnapshot,
        model_id: Option<&str>,
    ) -> Result<String, OrchestratorError> {
        let tool_schema_hash = tool_schema_hash(tools);
        let mut state = self.state.lock().expect("session log state poisoned");
        self.ensure_initialized(&mut state)?;
        if let Err(error) = self.check_capacity(&state, 0) {
            state.failure = Some(error.to_string());
            return Err(error);
        }
        let request_id = format!("req-{}", state.next_request);
        let turn = state.turn;
        let iteration = state.iteration;
        let session_id = self.session_id.clone();
        let seq = state.next_seq;
        let record = SessionLogRecord::ModelRequest(Box::new(ModelRequestRecord {
            seq,
            request_id: request_id.clone(),
            session_id,
            message_ids,
            tool_schema_hash,
            projection_hash,
            budget: *budget,
            model_id: model_id.map(str::to_string),
            turn,
            iteration,
        }));
        state.next_request += 1;
        self.commit(&mut state, record).inspect_err(|error| {
            state.failure = Some(error.to_string());
        })?;
        Ok(request_id)
    }

    /// Records a whole request in one call (compaction model calls).
    pub fn record_request(
        &self,
        messages: &[ModelMessage],
        tools: &[ToolDefinition],
        projection_hash: String,
        budget: &BudgetSnapshot,
        model_id: Option<&str>,
    ) -> Result<String, OrchestratorError> {
        let mut ids = Vec::with_capacity(messages.len());
        for message in messages {
            ids.push(self.record_message(message, tool_call_id(message))?);
        }
        self.record_model_request(ids, tools, projection_hash, budget, model_id)
    }

    /// Records that the bounded request view dropped oldest messages.
    pub fn record_truncation(&self, truncation: &crate::model::TranscriptTruncation) {
        let session_id = self.session_id.clone();
        let truncation = TruncationRecord {
            seq: 0,
            session_id,
            removed_messages: truncation.removed_messages,
            removed_bytes: truncation.removed_bytes,
            retained_bytes: truncation.retained_bytes,
            limit_bytes: truncation.limit_bytes,
        };
        let _ = self.append_record(move |seq| {
            SessionLogRecord::Truncation(TruncationRecord { seq, ..truncation })
        });
    }

    /// Records a persisted recovery checkpoint.
    pub fn record_checkpoint(
        &self,
        checkpoint_id: &str,
        checkpoint_seq: u64,
    ) -> Result<(), OrchestratorError> {
        let session_id = self.session_id.clone();
        let checkpoint_id = checkpoint_id.to_string();
        self.append_record(move |seq| SessionLogRecord::Checkpoint {
            seq,
            session_id,
            checkpoint_id,
            checkpoint_seq,
        })
    }

    /// Records an applied compaction funnel result.
    pub fn record_compaction(
        &self,
        pruned_tool_results: usize,
        deduped_messages: usize,
        dropped_messages: usize,
    ) -> Result<(), OrchestratorError> {
        let session_id = self.session_id.clone();
        self.append_record(move |seq| SessionLogRecord::Compaction {
            seq,
            session_id,
            pruned_tool_results,
            deduped_messages,
            dropped_messages,
        })
    }

    /// Every record in append order.
    pub fn records(&self) -> Result<Vec<SessionLogRecord>, OrchestratorError> {
        {
            let state = self.state.lock().expect("session log state poisoned");
            if let Some(failure) = &state.failure {
                return Err(OrchestratorError::InvalidState(format!(
                    "session log is unavailable after a failed append: {failure}"
                )));
            }
        }
        self.sink.records(&self.session_id)
    }

    /// Replays every logged message in append order.
    pub fn replay_messages(&self) -> Result<Vec<(String, ModelMessage)>, OrchestratorError> {
        let mut replayed = Vec::new();
        for record in self.records()? {
            let SessionLogRecord::Message(record) = record else { continue };
            let body =
                self.sink.body(&self.session_id, &record.content_hash)?.ok_or_else(|| {
                    OrchestratorError::InvalidState(format!(
                        "session log body {} for {} is missing",
                        record.content_hash, record.message_id
                    ))
                })?;
            let message: ModelMessage = serde_json::from_slice(&body).map_err(|error| {
                OrchestratorError::Serialization(format!(
                    "session log body {} failed to deserialize: {error}",
                    record.content_hash
                ))
            })?;
            replayed.push((record.message_id, message));
        }
        Ok(replayed)
    }

    /// Replays the log into a bounded view under `limit_bytes`, oldest messages
    /// dropped first. The newest message always survives.
    pub fn replay_messages_bounded(
        &self,
        limit_bytes: usize,
    ) -> Result<Vec<(String, ModelMessage)>, OrchestratorError> {
        let mut replayed = self.replay_messages()?;
        while replayed.len() > 1 {
            let total: usize = replayed
                .iter()
                .map(|(_, message)| {
                    serde_json::to_vec(message).map_or(usize::MAX, |bytes| bytes.len())
                })
                .sum();
            if total <= limit_bytes {
                break;
            }
            replayed.remove(0);
        }
        Ok(replayed)
    }

    /// Exports every record as JSONL with redacted message previews.
    ///
    /// Record payloads carry hashes and counters only, so the raw content never
    /// leaves this path; previews additionally run through the secret guard.
    pub fn export_redacted_jsonl(&self) -> Result<String, OrchestratorError> {
        let guard = SensitiveDataGuard::new();
        let mut out = String::new();
        for record in self.records()? {
            let mut value = serde_json::to_value(&record).map_err(|error| {
                OrchestratorError::Serialization(format!(
                    "session log export serialization failed: {error}"
                ))
            })?;
            if let SessionLogRecord::Message(message) = &record
                && let Some(body) = self.sink.body(&self.session_id, &message.content_hash)?
                && let Ok(parsed) = serde_json::from_slice::<ModelMessage>(&body)
            {
                let preview: String = guard
                    .redact(&parsed.text_content())
                    .chars()
                    .take(EXPORT_PREVIEW_MAX_CHARS)
                    .collect();
                if let Some(object) = value.as_object_mut() {
                    object.insert("redacted_preview".into(), serde_json::Value::String(preview));
                }
            }
            let line = serde_json::to_string(&value).map_err(|error| {
                OrchestratorError::Serialization(format!(
                    "session log export serialization failed: {error}"
                ))
            })?;
            out.push_str(&line);
            out.push('\n');
        }
        Ok(out)
    }

    /// Branches a new session from `checkpoint_seq`.
    ///
    /// Replays the exact log prefix when records exist. With no log at all the
    /// outcome reports `conversation_replayed = false`; the caller can still
    /// adopt task, memory, and budget state.
    pub fn branch_from(&self, checkpoint_seq: u64) -> Result<BranchOutcome, OrchestratorError> {
        let records = self.records()?;
        let new_session = branch_session_id(&self.session_id, checkpoint_seq);
        if records.is_empty() {
            let branch =
                SessionLog::with_sink(new_session.clone(), self.sink.clone(), self.config.clone());
            branch.record_branch(&self.session_id, checkpoint_seq, false)?;
            return Ok(BranchOutcome::conversation_unreplayed(
                new_session,
                self.session_id.clone(),
                checkpoint_seq,
            ));
        }
        let Some(checkpoint_position) = records.iter().position(|record| {
            matches!(record, SessionLogRecord::Checkpoint { checkpoint_seq: seq, .. } if *seq == checkpoint_seq)
        }) else {
            return Err(OrchestratorError::InvalidState(format!(
                "session {} has no log record for checkpoint sequence {checkpoint_seq}",
                self.session_id
            )));
        };
        let prefix = &records[..=checkpoint_position];
        let branch =
            SessionLog::with_sink(new_session.clone(), self.sink.clone(), self.config.clone());
        let mut replayed_messages = 0usize;
        for record in prefix {
            match record {
                SessionLogRecord::Message(message) => {
                    if let Some(body) = self.sink.body(&self.session_id, &message.content_hash)? {
                        self.sink.put_body(branch.session_id(), &message.content_hash, &body)?;
                    }
                    replayed_messages += 1;
                    branch
                        .append_record(|seq| record.clone().resequence(seq, branch.session_id()))?;
                }
                SessionLogRecord::ModelRequest(_) => {
                    branch
                        .append_record(|seq| record.clone().resequence(seq, branch.session_id()))?;
                }
                _ => {}
            }
        }
        branch.record_branch(&self.session_id, checkpoint_seq, true)?;
        Ok(BranchOutcome {
            session_id: new_session,
            source_session_id: self.session_id.clone(),
            checkpoint_seq,
            conversation_replayed: true,
            replayed_messages,
        })
    }

    fn record_branch(
        &self,
        source_session_id: &str,
        checkpoint_seq: u64,
        conversation_replayed: bool,
    ) -> Result<(), OrchestratorError> {
        let session_id = self.session_id.clone();
        let source_session_id = source_session_id.to_string();
        self.append_record(move |seq| SessionLogRecord::Branch {
            seq,
            session_id,
            source_session_id,
            checkpoint_seq,
            conversation_replayed,
        })
    }

    fn append_record(
        &self,
        make: impl FnOnce(u64) -> SessionLogRecord,
    ) -> Result<(), OrchestratorError> {
        let mut state = self.state.lock().expect("session log state poisoned");
        self.ensure_initialized(&mut state)?;
        if let Err(error) = self.check_capacity(&state, 0) {
            state.failure = Some(error.to_string());
            return Err(error);
        }
        let seq = state.next_seq;
        let record = make(seq);
        self.commit(&mut state, record).inspect_err(|error| {
            state.failure = Some(error.to_string());
        })
    }

    fn ensure_initialized(&self, state: &mut LogState) -> Result<(), OrchestratorError> {
        if state.initialized {
            return Ok(());
        }
        let existing = self.sink.records(&self.session_id)?;
        state.next_seq = existing.last().map_or(1, |record| record.seq() + 1);
        state.records = existing.len();
        state.bytes = existing.iter().map(record_bytes).sum();
        state.next_message =
            existing.iter().filter(|record| matches!(record, SessionLogRecord::Message(_))).count()
                as u64
                + 1;
        state.next_request = existing
            .iter()
            .filter(|record| matches!(record, SessionLogRecord::ModelRequest(_)))
            .count() as u64
            + 1;
        state.initialized = true;
        Ok(())
    }

    fn check_capacity(
        &self,
        state: &LogState,
        extra_body_bytes: usize,
    ) -> Result<(), OrchestratorError> {
        if state.records + 1 > self.config.max_records {
            return Err(OrchestratorError::InvalidState(format!(
                "session log for {} reached the {} record cap",
                self.session_id, self.config.max_records
            )));
        }
        if state.bytes.saturating_add(extra_body_bytes) > self.config.max_session_bytes {
            return Err(OrchestratorError::InvalidState(format!(
                "session log for {} reached the {} byte cap",
                self.session_id, self.config.max_session_bytes
            )));
        }
        Ok(())
    }

    fn commit(
        &self,
        state: &mut LogState,
        record: SessionLogRecord,
    ) -> Result<(), OrchestratorError> {
        let bytes = record_bytes(&record);
        self.sink.append(&self.session_id, &record)?;
        state.next_seq = record.seq() + 1;
        state.records += 1;
        state.bytes = state.bytes.saturating_add(bytes);
        Ok(())
    }
}

/// Per-runtime session-log store: one durable sink, one log per session.
#[derive(Debug)]
pub struct SessionLogStore {
    sink: Arc<dyn SessionLogSink>,
    config: SessionLogConfig,
    logs: Mutex<HashMap<String, Arc<SessionLog>>>,
}

impl SessionLogStore {
    /// Creates a store from configuration. Disabled configuration yields
    /// `None`, which keeps runtimes log-free.
    #[must_use]
    pub fn from_config(config: &SessionLogConfig) -> Option<Arc<Self>> {
        if !config.enabled || config.validate().is_err() {
            return None;
        }
        let sink: Arc<dyn SessionLogSink> = match &config.dir {
            Some(dir) => Arc::new(FileSessionLogSink::new(dir.clone(), config.max_session_bytes)),
            None => Arc::new(InMemorySessionLogSink::new()),
        };
        Some(Arc::new(Self { sink, config: config.clone(), logs: Mutex::new(HashMap::new()) }))
    }

    /// Stable log handle for one session.
    #[must_use]
    pub fn log_for(&self, session_id: &str) -> Arc<SessionLog> {
        let mut logs = self.logs.lock().expect("session log registry poisoned");
        logs.entry(session_id.to_string())
            .or_insert_with(|| {
                Arc::new(SessionLog::with_sink(session_id, self.sink.clone(), self.config.clone()))
            })
            .clone()
    }

    /// Records for one session in append order.
    pub fn records(&self, session_id: &str) -> Result<Vec<SessionLogRecord>, OrchestratorError> {
        self.sink.records(session_id)
    }

    /// Sessions with at least one record.
    #[must_use]
    pub fn session_ids(&self) -> Vec<String> {
        self.sink.session_ids()
    }
}

fn record_bytes(record: &SessionLogRecord) -> usize {
    serde_json::to_vec(record).map_or(0, |bytes| bytes.len())
}

/// Hash of the advertised tool schemas (empty schema set included).
#[must_use]
pub fn tool_schema_hash(tools: &[ToolDefinition]) -> String {
    let digest = sha256_json(tools);
    hex(&digest)
}

/// Hash of an effective request projection.
#[must_use]
pub fn projection_hash(messages: &[ModelMessage]) -> String {
    hex(&sha256_json(messages))
}

fn sha256_json<T: Serialize + ?Sized>(value: &T) -> [u8; 32] {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    Sha256::digest(&bytes).into()
}

/// Tool-call id carried by a message, when it is a tool observation.
#[must_use]
pub fn tool_call_id(message: &ModelMessage) -> Option<&str> {
    message.content.iter().find_map(|block| match block {
        ModelContent::ToolResult { tool_call_id, .. } => Some(tool_call_id.as_str()),
        _ => None,
    })
}

fn branch_session_id(session_id: &str, checkpoint_seq: u64) -> String {
    format!("{session_id}~branch{checkpoint_seq}")
}

/// Resolves the configured durable log root for a session log directory.
#[must_use]
pub fn session_log_root(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(sanitize_component(session_id))
}
