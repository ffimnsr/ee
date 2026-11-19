//! Session export/import with redaction (I22).
//!
//! A transfer document is a bounded JSON snapshot of one session's durable
//! log: normalized messages plus aggregated usage. It never carries provider
//! native state (those blobs replay only through the Gap 25 producer rules)
//! and never carries workspace-memory references; redaction therefore covers
//! prompt text, tool inputs/outputs, tool-call metadata, and routing metadata.
//!
//! Redacted exports are the default. Import fails closed when the target
//! session id already has records; a caller may import into an empty target
//! session (the fresh-session + `/import-session` flow) instead.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::OrchestratorError;
use crate::model::{ModelContent, ModelMessage, ModelRole};
use crate::session_log::{SessionLog, SessionLogRecord, SessionLogStore, tool_call_id};
use crate::trust::TrustLevel;
use crate::usage::ModelUsage;

/// Current transfer-document format version.
pub const SESSION_TRANSFER_SCHEMA_VERSION: u32 = 1;
/// Default serialized export cap.
pub const DEFAULT_MAX_TRANSFER_BYTES: usize = 8 * 1024 * 1024;
/// Default exported message cap.
pub const DEFAULT_MAX_TRANSFER_MESSAGES: usize = 4_096;
/// Highest message count an import accepts.
pub const MAX_TRANSFER_MESSAGES: usize = 100_000;

/// Routing and display metadata for one exported session.
///
/// Redacted exports drop every field: none of them is required to resume the
/// imported transcript.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTransferInfo {
    /// Provider implementation name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Selected model registry id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// Selected ACP mode id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Selected user-defined primary agent name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Session title, when the client set one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// One exported model-visible message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionTransferMessage {
    /// Message role.
    pub role: ModelRole,
    /// Trust level the model saw.
    pub trust: TrustLevel,
    /// Tool-call id for tool observations; redacted exports clear it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Message blocks.
    pub content: Vec<ModelContent>,
}

/// Aggregated usage for one model (or `unknown`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTransferUsage {
    /// Input tokens consumed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<usize>,
    /// Output tokens produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<usize>,
    /// Prompt-cache read tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<usize>,
    /// Prompt-cache write tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<usize>,
    /// Hidden reasoning tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<usize>,
    /// Cost in integer micro-dollars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_micros: Option<u64>,
}

impl From<ModelUsage> for SessionTransferUsage {
    fn from(usage: ModelUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            cost_micros: usage.cost_micros,
        }
    }
}

impl SessionTransferUsage {
    /// Rebuilds the normalized usage value (context stays unknown).
    #[must_use]
    pub fn to_model_usage(self) -> ModelUsage {
        ModelUsage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cached_input_tokens: self.cached_input_tokens,
            cache_creation_input_tokens: self.cache_creation_input_tokens,
            reasoning_tokens: self.reasoning_tokens,
            context_tokens: None,
            cost_micros: self.cost_micros,
        }
    }

    fn accumulate(&mut self, other: Self) {
        *self = Self::from(self.to_model_usage().accumulate(&other.to_model_usage()));
    }
}

/// Portable session document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionTransferDocument {
    /// Format version; imports require [`SESSION_TRANSFER_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Source session id; imports reuse it unless a target is supplied.
    pub session_id: String,
    /// Export time (Unix seconds).
    pub exported_at_unix: u64,
    /// Whether message bodies and metadata were redacted.
    pub redacted: bool,
    /// Whether the bounded export dropped oldest messages.
    pub truncated: bool,
    /// Routing metadata (empty when redacted).
    pub info: SessionTransferInfo,
    /// Settled messages in append order.
    pub messages: Vec<SessionTransferMessage>,
    /// Aggregate usage across the exported session.
    pub usage: SessionTransferUsage,
    /// Per-model usage totals, keyed by model registry id (`unknown` when the
    /// adapter did not report one).
    #[serde(default)]
    pub usage_by_model: BTreeMap<String, SessionTransferUsage>,
}

/// Export knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferExportOptions {
    /// Strip message text, tool metadata, and routing metadata.
    pub redact: bool,
    /// Serialized document cap.
    pub max_bytes: usize,
    /// Message cap.
    pub max_messages: usize,
}

impl Default for TransferExportOptions {
    fn default() -> Self {
        Self {
            redact: true,
            max_bytes: DEFAULT_MAX_TRANSFER_BYTES,
            max_messages: DEFAULT_MAX_TRANSFER_MESSAGES,
        }
    }
}

impl TransferExportOptions {
    /// Full-content export (explicit opt-in; never the default).
    #[must_use]
    pub fn with_content() -> Self {
        Self { redact: false, ..Self::default() }
    }
}

/// Import outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferImportOutcome {
    /// Session id the messages were written to.
    pub session_id: String,
    /// Imported message count.
    pub messages: usize,
    /// Aggregate imported usage.
    pub usage: SessionTransferUsage,
}

/// Exports one session log into a bounded transfer document.
///
/// # Errors
///
/// Fails when the log cannot be read, a message body is missing, or the
/// serialized document cannot be produced.
pub fn export_session(
    log: &SessionLog,
    info: SessionTransferInfo,
    options: TransferExportOptions,
) -> Result<SessionTransferDocument, OrchestratorError> {
    let mut messages = Vec::new();
    let mut usage = ModelUsage::new();
    let mut usage_by_model: BTreeMap<String, ModelUsage> = BTreeMap::new();
    for record in log.records()? {
        match record {
            SessionLogRecord::Message(message) => {
                let body =
                    log.sink().body(log.session_id(), &message.content_hash)?.ok_or_else(|| {
                        OrchestratorError::InvalidState(format!(
                            "session log body {} for {} is missing",
                            message.content_hash, message.message_id
                        ))
                    })?;
                let parsed: ModelMessage = serde_json::from_slice(&body).map_err(|error| {
                    OrchestratorError::Serialization(format!(
                        "session log body {} failed to deserialize: {error}",
                        message.content_hash
                    ))
                })?;
                // Prompt planes and host policy notices are rebuilt per turn;
                // exporting them would duplicate injected context after import.
                if parsed.role == ModelRole::System || parsed.metadata.contains_key("ee_prompt") {
                    continue;
                }
                messages.push(SessionTransferMessage {
                    role: parsed.role,
                    trust: parsed.trust,
                    tool_call_id: message.tool_call_id,
                    content: parsed.content,
                });
            }
            SessionLogRecord::Usage(record) => {
                usage = usage.accumulate(&record.usage);
                let key = record.model_id.unwrap_or_else(|| String::from("unknown"));
                let entry = usage_by_model.get(&key).copied().unwrap_or_default();
                usage_by_model.insert(key, entry.accumulate(&record.usage));
            }
            _ => {}
        }
    }

    let (mut document, mut truncated) =
        build_document(log.session_id(), info, options.redact, messages, usage, usage_by_model);
    while document.messages.len() > options.max_messages.max(1) {
        document.messages.remove(0);
        truncated = true;
    }
    let mut bytes = serialized_len(&document)?;
    while bytes > options.max_bytes.max(1) && document.messages.len() > 1 {
        let drop = (document.messages.len() / 4).max(1);
        document.messages.drain(..drop);
        truncated = true;
        bytes = serialized_len(&document)?;
    }
    if bytes > options.max_bytes.max(1) {
        return Err(OrchestratorError::InvalidState(format!(
            "session export for {} is {bytes} bytes after trimming, over the {} byte cap",
            document.session_id, options.max_bytes
        )));
    }
    document.truncated = truncated;
    Ok(document)
}

fn build_document(
    session_id: &str,
    info: SessionTransferInfo,
    redact: bool,
    messages: Vec<SessionTransferMessage>,
    usage: ModelUsage,
    usage_by_model: BTreeMap<String, ModelUsage>,
) -> (SessionTransferDocument, bool) {
    let messages =
        if redact { messages.into_iter().map(redact_message).collect() } else { messages };
    let usage_by_model = if redact {
        // Model ids are routing metadata: collapse the per-model rows into
        // one redacted row while keeping every token/cost total.
        let mut folded = SessionTransferUsage::default();
        for entry in usage_by_model.values().copied() {
            folded.accumulate(SessionTransferUsage::from(entry));
        }
        BTreeMap::from([(String::from("redacted"), folded)])
    } else {
        usage_by_model
            .into_iter()
            .map(|(model, usage)| (model, SessionTransferUsage::from(usage)))
            .collect()
    };
    let document = SessionTransferDocument {
        schema_version: SESSION_TRANSFER_SCHEMA_VERSION,
        session_id: session_id.to_string(),
        exported_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs()),
        redacted: redact,
        truncated: false,
        info: if redact { SessionTransferInfo::default() } else { info },
        messages,
        usage: SessionTransferUsage::from(usage),
        usage_by_model,
    };
    (document, false)
}

fn redact_message(message: SessionTransferMessage) -> SessionTransferMessage {
    let label = match message.role {
        ModelRole::User => "user",
        ModelRole::Assistant => "assistant",
        ModelRole::System => "system",
        ModelRole::Tool => "tool",
        ModelRole::Subagent => "subagent",
    };
    SessionTransferMessage {
        role: message.role,
        trust: message.trust,
        tool_call_id: None,
        content: vec![ModelContent::Text(format!("[redacted {label} message]"))],
    }
}

fn serialized_len(document: &SessionTransferDocument) -> Result<usize, OrchestratorError> {
    serde_json::to_vec(document).map(|bytes| bytes.len()).map_err(|error| {
        OrchestratorError::Serialization(format!("session transfer serialization failed: {error}"))
    })
}

/// Imports a transfer document into `store`.
///
/// `target_session_id` defaults to the document session id. The target must
/// have no records; an existing id fails closed with the id named.
///
/// # Errors
///
/// Fails on an unsupported schema version, empty/oversized documents, an
/// existing target id, or any log write failure.
pub fn import_session(
    store: &SessionLogStore,
    document: &SessionTransferDocument,
    target_session_id: Option<&str>,
) -> Result<TransferImportOutcome, OrchestratorError> {
    if document.schema_version != SESSION_TRANSFER_SCHEMA_VERSION {
        return Err(OrchestratorError::InvalidState(format!(
            "unsupported session transfer schema version {}; expected {}",
            document.schema_version, SESSION_TRANSFER_SCHEMA_VERSION
        )));
    }
    if document.session_id.trim().is_empty() {
        return Err(OrchestratorError::InvalidState(
            "session transfer document has an empty session id".into(),
        ));
    }
    if document.messages.len() > MAX_TRANSFER_MESSAGES {
        return Err(OrchestratorError::InvalidState(format!(
            "session transfer carries {} messages, over the {MAX_TRANSFER_MESSAGES} cap",
            document.messages.len()
        )));
    }
    let target = target_session_id.unwrap_or(&document.session_id);
    if !store.records(target)?.is_empty() || store.session_ids().iter().any(|id| id == target) {
        return Err(OrchestratorError::InvalidState(format!(
            "session {target} already exists; refusing to import over it"
        )));
    }

    let log = store.log_for(target);
    for message in &document.messages {
        let mut value = ModelMessage::new(message.role).with_trust(message.trust);
        value.content = message.content.clone();
        log.record_message(&value, message.tool_call_id.as_deref().or(tool_call_id(&value)))?;
    }
    let mut usage = ModelUsage::new();
    for (model, entry) in &document.usage_by_model {
        let value = entry.to_model_usage();
        usage = usage.accumulate(&value);
        log.record_usage(&value, Some(model))?;
    }
    if document.usage_by_model.is_empty() && document.usage != SessionTransferUsage::default() {
        usage = document.usage.to_model_usage();
        log.record_usage(&usage, None)?;
    }
    Ok(TransferImportOutcome {
        session_id: target.to_string(),
        messages: document.messages.len(),
        usage: SessionTransferUsage::from(usage),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelMessage;

    fn seeded_log(session: &str) -> std::sync::Arc<SessionLog> {
        let log = std::sync::Arc::new(SessionLog::in_memory(session));
        log.record_message(&ModelMessage::text(ModelRole::User, "secret prompt"), None)
            .expect("user");
        log.record_message(&ModelMessage::text(ModelRole::Assistant, "answer"), None)
            .expect("assistant");
        log.record_message(&ModelMessage::text(ModelRole::Tool, "read /work/secret.rs"), None)
            .expect("tool");
        log.record_message(
            &ModelMessage::text(ModelRole::System, "rebuilt prompt plane")
                .with_metadata([(String::from("ee_prompt"), String::from("base"))]),
            None,
        )
        .expect("prompt plane");
        log.record_usage(
            &ModelUsage::new().with_input_tokens(11).with_output_tokens(7).with_cost_micros(42),
            Some("model-a"),
        )
        .expect("usage");
        log
    }

    #[test]
    fn full_export_round_trips_messages_and_usage() {
        let log = seeded_log("s-1");
        let document = export_session(
            &log,
            SessionTransferInfo::default(),
            TransferExportOptions::with_content(),
        )
        .expect("exports");
        assert!(!document.redacted);
        assert_eq!(document.messages.len(), 3);
        assert!(matches!(
            &document.messages[0].content[0],
            ModelContent::Text(text) if text == "secret prompt"
        ));
        assert_eq!(document.usage.input_tokens, Some(11));
        assert_eq!(document.usage_by_model["model-a"].output_tokens, Some(7));
    }

    #[test]
    fn redacted_export_drops_text_metadata_and_routing() {
        let log = seeded_log("s-1");
        let document = export_session(
            &log,
            SessionTransferInfo {
                provider: Some("ee".into()),
                model_id: Some("model-a".into()),
                ..Default::default()
            },
            TransferExportOptions::default(),
        )
        .expect("exports");
        assert!(document.redacted);
        assert_eq!(document.info, SessionTransferInfo::default());
        let text = serde_json::to_string(&document).expect("json");
        assert!(!text.contains("secret prompt"));
        assert!(!text.contains("model-a"));
        assert!(!text.contains("/work/secret.rs"), "redacted export carries no absolute paths");
        assert!(document.messages.iter().all(|message| message.tool_call_id.is_none()));
        assert_eq!(document.usage.input_tokens, Some(11), "usage survives redaction");
        assert_eq!(document.usage_by_model.len(), 1);
        assert!(document.usage_by_model.contains_key("redacted"));
    }

    #[test]
    fn export_bounds_messages_and_marks_truncation() {
        let log = seeded_log("s-1");
        let document = export_session(
            &log,
            SessionTransferInfo::default(),
            TransferExportOptions { redact: false, max_bytes: 1024 * 1024, max_messages: 1 },
        )
        .expect("exports");
        assert_eq!(document.messages.len(), 1);
        assert!(document.truncated);
        assert!(matches!(
            &document.messages[0].content[0],
            ModelContent::Text(text) if text == "read /work/secret.rs"
        ));
    }

    #[test]
    fn import_refuses_to_overwrite_an_existing_session() {
        let store = SessionLogStore::from_config(&crate::session_log::SessionLogConfig::default())
            .expect("store");
        let document = SessionTransferDocument {
            schema_version: SESSION_TRANSFER_SCHEMA_VERSION,
            session_id: "taken".into(),
            exported_at_unix: 0,
            redacted: false,
            truncated: false,
            info: SessionTransferInfo::default(),
            messages: Vec::new(),
            usage: SessionTransferUsage::default(),
            usage_by_model: BTreeMap::new(),
        };
        // The store's sink is separate, so seed it through the store itself.
        store
            .log_for("taken")
            .record_message(&ModelMessage::text(ModelRole::User, "already here"), None)
            .expect("seed");
        let error = import_session(&store, &document, None).expect_err("conflict");
        assert!(error.to_string().contains("taken"), "{error}");
    }

    #[test]
    fn import_into_an_empty_target_restores_resumable_state() {
        let store = SessionLogStore::from_config(&crate::session_log::SessionLogConfig::default())
            .expect("store");
        let log = seeded_log("source");
        let document = export_session(
            &log,
            SessionTransferInfo::default(),
            TransferExportOptions::with_content(),
        )
        .expect("exports");
        let outcome = import_session(&store, &document, Some("fresh")).expect("imports");
        assert_eq!(outcome.session_id, "fresh");
        assert_eq!(outcome.messages, 3);
        assert_eq!(outcome.usage.output_tokens, Some(7));
        let replayed = store.log_for("fresh").replay_messages().expect("replay");
        assert_eq!(replayed.len(), 3);
        let records = store.records("fresh").expect("records");
        assert!(records.iter().any(|record| matches!(record, SessionLogRecord::Usage(_))));
    }

    #[test]
    fn import_rejects_unknown_schema_versions() {
        let store = SessionLogStore::from_config(&crate::session_log::SessionLogConfig::default())
            .expect("store");
        let document = SessionTransferDocument {
            schema_version: SESSION_TRANSFER_SCHEMA_VERSION + 1,
            session_id: "s".into(),
            exported_at_unix: 0,
            redacted: false,
            truncated: false,
            info: SessionTransferInfo::default(),
            messages: Vec::new(),
            usage: SessionTransferUsage::default(),
            usage_by_model: BTreeMap::new(),
        };
        assert!(import_session(&store, &document, None).is_err());
    }
}
