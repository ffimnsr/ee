//! The normalized transcript builder and its memory budget.

use std::collections::BTreeSet;
use std::sync::Arc;

use ee_agent_acp_server::PromptContext;
use ee_agent_protocol::ContentBlock;
use serde::{Deserialize, Serialize};

use crate::compaction_checkpoint::CompactionCheckpoint;
use crate::session_log::SessionLog;
use crate::tools::ToolResult;

use super::{
    ModelContent, ModelMessage, ModelResponse, ModelRole, PROVIDER_EXECUTED_METADATA_PREFIX,
    PROVIDER_EXECUTED_RESULT_KEY, TOOL_NAME_METADATA_KEY, TOOL_PATH_METADATA_KEY,
    TOOL_SIDE_EFFECT_METADATA_KEY,
};

/// How much context was dropped by the last memory-limit enforcement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TranscriptTruncation {
    /// How many oldest messages were removed.
    pub removed_messages: usize,
    /// Bytes removed with those messages.
    pub removed_bytes: usize,
    /// Bytes retained after enforcement.
    pub retained_bytes: usize,
    /// The limit that triggered the truncation.
    pub limit_bytes: usize,
}

/// A serializable, deterministic normalized conversation.
///
/// The builder converts ACP prompt content into user messages, appends
/// assistant responses (reasoning kept separate), tool observations with
/// stable tool-call ids, and subagent summaries, and enforces the memory
/// byte limit by dropping the oldest messages first.
///
/// When a [`SessionLog`] is attached, every pushed message is appended to the
/// durable log and its stable message id is retained alongside the bounded
/// request view. Truncation is therefore a view operation only.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Transcript {
    /// Normalized messages in chronological order.
    pub messages: Vec<ModelMessage>,
    /// What the last [`Transcript::enforce_memory_limit`] call removed,
    /// when it removed anything.
    pub truncation: Option<TranscriptTruncation>,
    /// Durable log the transcript appends to, when one is wired.
    #[serde(skip)]
    pub(crate) log: Option<Arc<SessionLog>>,
    /// Per-result bounds applied when a tool observation is admitted.
    #[serde(default)]
    pub(crate) tool_output: crate::tool_output::ToolOutputLimits,
    /// Parallel to `messages`; `None` when a message is not (yet) logged.
    #[serde(skip)]
    pub(crate) log_ids: Vec<Option<String>>,
}

impl PartialEq for Transcript {
    fn eq(&self, other: &Self) -> bool {
        self.messages == other.messages && self.truncation == other.truncation
    }
}

impl Transcript {
    /// Creates an empty transcript.
    #[must_use]
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            truncation: None,
            log: None,
            log_ids: Vec::new(),
            tool_output: crate::tool_output::ToolOutputLimits::default(),
        }
    }

    /// Sets the per-result bounds applied when a tool observation is admitted.
    pub fn set_tool_output_limits(&mut self, limits: crate::tool_output::ToolOutputLimits) {
        self.tool_output = limits;
    }

    /// Converts ACP prompt content into one normalized user message.
    ///
    /// Text blocks become text content, resource links become file
    /// references, and image, audio, and embedded-resource blocks keep their
    /// payload so provider adapters can send them as native multimodal parts.
    /// Unknown or malformed blocks are recorded in bounded diagnostic metadata
    /// instead of being dropped silently.
    #[must_use]
    pub fn from_prompt(ctx: &PromptContext) -> Self {
        let mut content = Vec::new();
        let mut unsupported = Vec::new();
        for block in &ctx.prompt {
            match block {
                ContentBlock::Text(text) => content.push(ModelContent::Text(text.text.clone())),
                ContentBlock::ResourceLink(link) => {
                    content.push(ModelContent::FileReference { path: link.uri.clone() });
                }
                ContentBlock::Image(image) => content.push(ModelContent::Image {
                    data: image.data.clone(),
                    mime_type: image.mime_type.clone(),
                    uri: image.uri.clone(),
                }),
                ContentBlock::Audio(audio) => content.push(ModelContent::Audio {
                    data: audio.data.clone(),
                    mime_type: audio.mime_type.clone(),
                }),
                ContentBlock::Resource(resource) => match &resource.resource {
                    ee_agent_protocol::EmbeddedResourceResource::TextResourceContents(text) => {
                        content.push(ModelContent::EmbeddedResource {
                            uri: text.uri.clone(),
                            mime_type: text.mime_type.clone(),
                            text: Some(text.text.clone()),
                            blob: None,
                        });
                    }
                    ee_agent_protocol::EmbeddedResourceResource::BlobResourceContents(blob) => {
                        content.push(ModelContent::EmbeddedResource {
                            uri: blob.uri.clone(),
                            mime_type: blob.mime_type.clone(),
                            text: None,
                            blob: Some(blob.blob.clone()),
                        });
                    }
                    // The SDK enum is non-exhaustive; future resource kinds are
                    // recorded diagnostically instead of dropped silently.
                    _ => unsupported.push("embedded_resource"),
                },
                // The SDK enum is non-exhaustive; future block types are
                // recorded diagnostically instead of dropped silently.
                _ => unsupported.push("other"),
            }
        }
        let mut message = ModelMessage::new(ModelRole::User).with_content(content);
        if !unsupported.is_empty() {
            message = message.with_metadata([("unsupported_blocks".into(), unsupported.join(","))]);
        }
        let mut transcript = Self::new();
        transcript.messages = vec![message];
        transcript
    }

    /// Prepends a system message ahead of the prompt's user message, used to
    /// seed bounded memory facts before any model call.
    pub fn prepend_system(&mut self, text: impl Into<String>) {
        self.messages.insert(0, ModelMessage::text(ModelRole::System, text));
        let id = self.record_message_at(0, None);
        self.log_ids.insert(0, id);
    }

    /// Prepends an opaque provider compaction checkpoint unless an identical
    /// one is already present.
    ///
    /// The checkpoint stands in for the conversation prefix the provider
    /// already compacted, so it must be the first item the provider sees. It
    /// travels as its own content part and every codec that does not understand
    /// it ignores the message entirely.
    pub fn ensure_compaction_checkpoint(&mut self, checkpoint: &CompactionCheckpoint) {
        let present = self.messages.iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(
                    block,
                    ModelContent::CompactionCheckpoint(existing) if existing == checkpoint
                )
            })
        });
        if present {
            return;
        }
        let message = ModelMessage::new(ModelRole::System)
            .with_content(vec![ModelContent::CompactionCheckpoint(checkpoint.clone())]);
        self.messages.insert(0, message);
        let id = self.record_message_at(0, None);
        self.log_ids.insert(0, id);
    }

    /// Appends the assistant message for a model response: text stays in
    /// content, reasoning stays in the separate reasoning summary, and tool
    /// calls are kept so the next request can replay the turn. Empty responses
    /// append nothing.
    pub fn push_assistant(&mut self, response: &ModelResponse) {
        let mut content = Vec::new();
        if !response.text.is_empty() {
            content.push(ModelContent::Text(response.text.clone()));
        }
        content.extend(response.tool_intents.iter().map(|intent| ModelContent::ToolCall {
            tool_call_id: intent.tool_call_id.clone(),
            name: intent.name.clone(),
            arguments: intent.arguments.clone(),
        }));
        let has_reasoning = response.reasoning.as_deref().is_some_and(|text| !text.is_empty());
        if content.is_empty() && !has_reasoning && response.native.is_none() {
            return;
        }
        let mut message = ModelMessage::new(ModelRole::Assistant).with_content(content);
        let mut entries: Vec<(String, String)> = response
            .tool_intents
            .iter()
            .filter(|intent| intent.provider_executed)
            .map(|intent| {
                (
                    format!("{PROVIDER_EXECUTED_METADATA_PREFIX}{}", intent.tool_call_id),
                    String::from("true"),
                )
            })
            .collect();
        for intent in &response.tool_intents {
            if let Some(error) = &intent.argument_error {
                entries.push((
                    format!(
                        "{}{}",
                        crate::transcript_normalize::ARGUMENT_ERROR_METADATA_PREFIX,
                        intent.tool_call_id
                    ),
                    error.clone(),
                ));
            }
        }
        if Self::cut_turn(response) {
            entries.push((
                crate::transcript_normalize::TURN_ERROR_METADATA_KEY.to_string(),
                String::from("true"),
            ));
        }
        if !entries.is_empty() {
            message = message.with_metadata(entries);
        }
        if let Some(reasoning) = response.reasoning.as_deref() {
            message = message.with_reasoning_summary(reasoning);
        }
        // A turn that ended in a provider error must not replay reasoning state
        // whose generation was cut: the provider will reject a truncated or
        // unsigned tail. Readable reasoning text is kept.
        if response.native.is_some() && !Self::cut_turn(response) {
            message = message.with_native(response.native.clone());
        }
        self.messages.push(message);
        let id = self.record_message_at(self.messages.len() - 1, None);
        self.log_ids.push(id);
    }

    /// Whether a response ended without a usable finish marker.
    ///
    /// A transport error aborts the loop before the assistant message is pushed, so
    /// this only sees a response that explicitly reports an error/cut stop reason
    /// **or** reports no finish reason at all: a stream that stopped before its
    /// terminal event leaves the tail unsigned, and replaying that state makes the
    /// provider reject the next turn. Stored interrupted turns are repaired again
    /// by transcript normalization.
    fn cut_turn(response: &ModelResponse) -> bool {
        match response.finish_reason.as_deref() {
            None => true,
            Some("error" | "interrupted" | "cancelled") => true,
            Some(_) => false,
        }
    }

    /// Appends a tool observation carrying the stable tool-call id.
    pub fn push_tool_result(&mut self, tool_call_id: impl Into<String>, mut result: ToolResult) {
        crate::tool_output::apply_to_result(&mut result, self.tool_output, self.log.as_deref());
        let tool_call_id = tool_call_id.into();
        self.messages.push(ModelMessage::tool_result(tool_call_id.clone(), result));
        let id = self.record_message_at(self.messages.len() - 1, Some(&tool_call_id));
        self.log_ids.push(id);
    }

    /// Answers every still-unanswered call of the last assistant turn with a
    /// synthetic aborted observation.
    ///
    /// Called when tool execution is cancelled or a provider failure aborts the
    /// turn after the assistant message was already recorded, so a resumed or
    /// replayed transcript is valid without relying on request-time repair.
    /// Returns how many observations were added.
    pub(crate) fn push_aborted_results_for_unanswered(&mut self) -> usize {
        let Some(start) =
            self.messages.iter().rposition(|message| message.role == ModelRole::Assistant)
        else {
            return 0;
        };
        let answered: BTreeSet<String> = self.messages[start + 1..]
            .iter()
            .filter_map(|message| {
                message.content.iter().find_map(|block| match block {
                    ModelContent::ToolResult { tool_call_id, .. } => Some(tool_call_id.clone()),
                    _ => None,
                })
            })
            .collect();
        let calls: Vec<(String, String)> = self.messages[start]
            .content
            .iter()
            .filter_map(|block| match block {
                ModelContent::ToolCall { tool_call_id, name, .. } => {
                    Some((tool_call_id.clone(), name.clone()))
                }
                _ => None,
            })
            .collect();
        if calls.is_empty() {
            return 0;
        }
        let provider_executed: BTreeSet<String> = self.messages[start]
            .metadata
            .keys()
            .filter_map(|key| key.strip_prefix(PROVIDER_EXECUTED_METADATA_PREFIX))
            .map(str::to_string)
            .collect();
        let mut added = 0;
        for (tool_call_id, name) in calls {
            if answered.contains(&tool_call_id) || provider_executed.contains(&tool_call_id) {
                continue;
            }
            let result = ToolResult {
                success: false,
                text_output: crate::transcript_normalize::ABORTED_TOOL_RESULT_TEXT.to_string(),
                structured_output: None,
                error_kind: Some(crate::tools::ToolErrorKind::Cancelled),
            };
            self.push_message(ModelMessage::tool_result(tool_call_id, result).with_metadata([
                (
                    crate::transcript_normalize::SYNTHETIC_TOOL_RESULT_KEY.to_string(),
                    String::from("true"),
                ),
                (TOOL_NAME_METADATA_KEY.to_string(), name),
            ]));
            added += 1;
        }
        added
    }

    /// Appends a tool observation with classification metadata.
    ///
    /// Compaction may only prune results whose side-effect class is `read`, so
    /// write, execute, delegate, approval, validation, and evidence results
    /// are never rewritten.
    pub fn push_tool_result_classified(
        &mut self,
        tool_call_id: impl Into<String>,
        mut result: ToolResult,
        tool_name: &str,
        side_effect: &str,
        path: Option<&str>,
    ) {
        crate::tool_output::apply_to_result(&mut result, self.tool_output, self.log.as_deref());
        let mut entries = vec![
            (TOOL_NAME_METADATA_KEY.to_string(), tool_name.to_string()),
            (TOOL_SIDE_EFFECT_METADATA_KEY.to_string(), side_effect.to_string()),
        ];
        if let Some(path) = path {
            entries.push((TOOL_PATH_METADATA_KEY.to_string(), path.to_string()));
        }
        let tool_call_id = tool_call_id.into();
        self.messages
            .push(ModelMessage::tool_result(tool_call_id.clone(), result).with_metadata(entries));
        let id = self.record_message_at(self.messages.len() - 1, Some(&tool_call_id));
        self.log_ids.push(id);
    }

    /// Appends a provider-executed tool's result.
    ///
    /// The message is marked so a re-encoded request never synthesizes a local
    /// `function_call_output` for a call the provider already ran.
    pub fn push_provider_tool_result(
        &mut self,
        tool_call_id: impl Into<String>,
        mut result: ToolResult,
    ) {
        crate::tool_output::apply_to_result(&mut result, self.tool_output, self.log.as_deref());
        let tool_call_id = tool_call_id.into();
        self.messages.push(
            ModelMessage::tool_result(tool_call_id.clone(), result)
                .with_metadata([(PROVIDER_EXECUTED_RESULT_KEY.to_string(), String::from("true"))]),
        );
        let id = self.record_message_at(self.messages.len() - 1, Some(&tool_call_id));
        self.log_ids.push(id);
    }

    /// Appends an already-built message and records it in the durable log.
    pub(crate) fn push_message(&mut self, message: ModelMessage) {
        self.messages.push(message);
        let id = self.record_message_at(self.messages.len() - 1, None);
        self.log_ids.push(id);
    }

    /// Appends a subagent summary message (subagent phase).
    pub fn push_subagent_summary(&mut self, summary: impl Into<String>) {
        self.messages.push(ModelMessage::text(ModelRole::Subagent, summary));
        let id = self.record_message_at(self.messages.len() - 1, None);
        self.log_ids.push(id);
    }

    /// Request projection: tool-result middles pruned without mutating the
    /// durable transcript used by checkpoints and replay.
    #[must_use]
    pub(crate) fn request_messages(&self) -> Vec<ModelMessage> {
        let projected: Vec<ModelMessage> = self
            .messages
            .iter()
            .map(|message| message.with_pruned_tool_result().unwrap_or_else(|| message.clone()))
            .collect();
        crate::transcript_normalize::normalize_transcript(&projected)
    }

    /// Text of the newest assistant message, when it carries any — used as
    /// the bounded summary a subagent returns to its parent.
    #[must_use]
    pub fn last_assistant_text(&self) -> Option<String> {
        let message = self.messages.iter().rev().find(|m| m.role == ModelRole::Assistant)?;
        let texts: Vec<&str> = message
            .content
            .iter()
            .filter_map(|content| match content {
                ModelContent::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        (!texts.is_empty()).then(|| texts.join(" "))
    }

    /// Drops the oldest messages while the transcript exceeds `limit_bytes`,
    /// always keeping the newest message, and records what was removed.
    pub fn enforce_memory_limit(&mut self, limit_bytes: usize) {
        let mut total: usize = self.messages.iter().map(message_bytes).sum();
        let mut removed = 0usize;
        let mut removed_bytes = 0usize;
        while self.messages.len() > 1 && total > limit_bytes {
            let dropped = message_bytes(&self.messages[0]);
            total -= dropped;
            removed_bytes += dropped;
            self.messages.remove(0);
            if !self.log_ids.is_empty() {
                self.log_ids.remove(0);
            }
            removed += 1;
        }
        self.truncation = (removed > 0).then_some(TranscriptTruncation {
            removed_messages: removed,
            removed_bytes,
            retained_bytes: total,
            limit_bytes,
        });
        if let (Some(log), Some(truncation)) = (&self.log, &self.truncation) {
            log.record_truncation(truncation);
        }
    }

    /// Drops the oldest half of the removable messages, keeping the system
    /// prefix and the newest message.
    ///
    /// Used once when a provider reports a context overflow: the request can
    /// then be retried with a smaller prompt instead of ending the turn. Returns
    /// `false` when there is nothing left to drop.
    pub fn shrink_oldest(&mut self) -> bool {
        let first = self
            .messages
            .iter()
            .position(|message| message.role != ModelRole::System)
            .unwrap_or(self.messages.len());
        let removable = self.messages.len().saturating_sub(first + 1);
        if removable == 0 {
            return false;
        }
        let drop_count = removable.div_ceil(2);
        // Never leave the surviving tail starting with a tool observation whose
        // call was dropped: providers reject an orphan result.
        let mut drop_count = drop_count;
        while first + drop_count < self.messages.len()
            && self.messages[first + drop_count].role == ModelRole::Tool
        {
            drop_count += 1;
        }
        let removed_bytes: usize =
            self.messages[first..first + drop_count].iter().map(message_bytes).sum();
        self.messages.drain(first..first + drop_count);
        if self.log_ids.len() >= first + drop_count {
            self.log_ids.drain(first..first + drop_count);
        }
        let retained_bytes: usize = self.messages.iter().map(message_bytes).sum();
        self.truncation = Some(TranscriptTruncation {
            removed_messages: drop_count,
            removed_bytes,
            retained_bytes,
            limit_bytes: 0,
        });
        if let (Some(log), Some(truncation)) = (&self.log, &self.truncation) {
            log.record_truncation(truncation);
        }
        true
    }

    /// All normalized messages in chronological order.
    #[must_use]
    pub fn messages(&self) -> &[ModelMessage] {
        &self.messages
    }

    /// Consumes the transcript, returning its messages.
    #[must_use]
    pub fn into_messages(self) -> Vec<ModelMessage> {
        self.messages
    }

    /// Truncation recorded by the last [`Transcript::enforce_memory_limit`].
    #[must_use]
    pub fn truncation(&self) -> Option<&TranscriptTruncation> {
        self.truncation.as_ref()
    }

    /// Number of messages.
    #[must_use]
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Whether the transcript is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

/// Serialized byte size of one normalized message.  Normalized messages only
/// hold strings, maps, and plain JSON values, so serialization is infallible.
pub(crate) fn message_bytes(message: &ModelMessage) -> usize {
    serde_json::to_string(message).expect("normalized messages always serialize").len()
}
