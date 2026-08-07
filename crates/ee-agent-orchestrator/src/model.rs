//! Normalized model messages, the transcript builder, and the
//! provider-supplied model adapter trait.
//!
//! The orchestrator never sees provider-specific JSON: adapters consume a
//! normalized [`ModelRequest`] (transcript, tool schemas, budget snapshot,
//! current task) and return a normalized [`ModelResponse`] (text, reasoning,
//! tool intents, subagent intents, completion signal).  All message and
//! response types are serializable and deterministic for tests and future
//! persistence.
//!
//! [`Transcript`] normalizes ACP prompt content into a deterministic
//! `ModelMessage` sequence and keeps the transcript inside the configured
//! memory budget by dropping the oldest messages first, recording truncation
//! metadata so later phases can surface it.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use ee_acp_agent_server::PromptContext;
use ee_agent_protocol::ContentBlock;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::budget::BudgetSnapshot;
use crate::subagents::SubagentIntent;
use crate::tasks::TaskNode;
use crate::tools::{ToolDefinition, ToolIntent, ToolResult};
use crate::trust::{TrustLevel, trust_for_role};

/// Cap on diagnostic metadata entries; keeps provider metadata bounded.
pub const MAX_METADATA_ENTRIES: usize = 16;

/// Boxed future returned by [`ModelAdapter`] methods.
///
/// The box keeps the trait object-safe without depending on `async-trait`.
pub type ModelFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Role of one normalized transcript message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ModelRole {
    /// System-level instructions.
    System,
    /// User (or delegated client) content.
    User,
    /// Assistant text and reasoning.
    Assistant,
    /// A tool observation appended after execution.
    Tool,
    /// A subagent summary (subagent phase).
    Subagent,
}

/// One content block inside a normalized [`ModelMessage`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ModelContent {
    /// Plain text.
    Text(String),
    /// A tool execution result, correlated by the stable tool-call id.
    ToolResult {
        /// The tool-call id used by the model and by update correlation.
        tool_call_id: String,
        /// The normalized execution outcome.
        result: ToolResult,
    },
    /// A reference to a workspace file.
    FileReference { path: String },
    /// A reference to a terminal session (later phases).
    TerminalReference { terminal_id: String },
}

/// One normalized transcript message.
///
/// Non-exhaustive so later phases can add fields without breaking adapters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelMessage {
    /// Who produced the message.
    pub role: ModelRole,
    /// The message content blocks.
    pub content: Vec<ModelContent>,
    /// Optional condensed reasoning summary kept separate from the text.
    pub reasoning_summary: Option<String>,
    /// Bounded diagnostic metadata; never carries required fields.  Provider
    /// metadata is flattened into at most [`MAX_METADATA_ENTRIES`] string
    /// entries so the transcript stays deterministic and bounded.
    pub metadata: BTreeMap<String, String>,
    /// Trust level of the content: tool output and subagent summaries are
    /// untrusted and may never override policy (see [`crate::prompt_injection`]).
    pub trust: TrustLevel,
}

impl ModelMessage {
    /// Creates an empty message with the given role; trust defaults from the
    /// role (system → system policy, tool → untrusted tool output, ...).
    #[must_use]
    pub fn new(role: ModelRole) -> Self {
        Self {
            role,
            content: Vec::new(),
            reasoning_summary: None,
            metadata: BTreeMap::new(),
            trust: trust_for_role(role),
        }
    }

    /// Creates a text-only message with the given role.
    #[must_use]
    pub fn text(role: ModelRole, text: impl Into<String>) -> Self {
        Self::new(role).with_content(vec![ModelContent::Text(text.into())])
    }

    /// Creates a tool-observation message from a tool result, keeping the
    /// stable tool-call id for correlation.
    #[must_use]
    pub fn tool_result(tool_call_id: impl Into<String>, result: ToolResult) -> Self {
        Self::new(ModelRole::Tool).with_content(vec![ModelContent::ToolResult {
            tool_call_id: tool_call_id.into(),
            result,
        }])
    }

    /// Replaces the message content.
    #[must_use]
    pub fn with_content(mut self, content: Vec<ModelContent>) -> Self {
        self.content = content;
        self
    }

    /// Sets the optional reasoning summary.
    #[must_use]
    pub fn with_reasoning_summary(mut self, summary: impl Into<String>) -> Self {
        self.reasoning_summary = Some(summary.into());
        self
    }

    /// Sets diagnostic metadata entries, keeping at most
    /// [`MAX_METADATA_ENTRIES`] (earlier entries win).
    #[must_use]
    pub fn with_metadata(mut self, entries: impl IntoIterator<Item = (String, String)>) -> Self {
        for (key, value) in entries.into_iter().take(MAX_METADATA_ENTRIES) {
            self.metadata.insert(key, value);
        }
        self
    }

    /// Overrides the trust level of the message.
    #[must_use]
    pub fn with_trust(mut self, trust: TrustLevel) -> Self {
        self.trust = trust;
        self
    }

    /// Concatenated text content of this message (empty when the message has
    /// no text blocks).
    #[must_use]
    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ModelContent::Text(text) => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Normalized request handed to [`ModelAdapter::complete`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelRequest {
    /// The conversation so far (prompt, assistant turns, tool observations).
    pub transcript: Vec<ModelMessage>,
    /// The tool schemas available to the model this turn.
    pub tools: Vec<ToolDefinition>,
    /// A snapshot of the budget state before the call.
    pub budget: BudgetSnapshot,
    /// The current task state.
    pub task: TaskNode,
    /// The advertised model list the delegating model can select for
    /// subagents (ids plus display name/capability hints); empty when no
    /// registry is wired.
    #[serde(default)]
    pub available_models: Vec<crate::model_registry::ModelInfo>,
    /// The registry id of the adapter this call runs on (diagnostic-only; the
    /// child's selection is recorded here too).
    #[serde(default)]
    pub model_id: Option<String>,
}

impl ModelRequest {
    /// Creates a request from its parts; the advertised model list and the
    /// diagnostic model id are empty/unset until set with the builders.
    #[must_use]
    pub fn new(
        transcript: Vec<ModelMessage>,
        tools: Vec<ToolDefinition>,
        budget: BudgetSnapshot,
        task: TaskNode,
    ) -> Self {
        Self { transcript, tools, budget, task, available_models: Vec::new(), model_id: None }
    }

    /// Sets the advertised model list the delegating model can pick from.
    #[must_use]
    pub fn with_available_models(mut self, models: Vec<crate::model_registry::ModelInfo>) -> Self {
        self.available_models = models;
        self
    }

    /// Sets the diagnostic model id of the adapter this call runs on.
    #[must_use]
    pub fn with_model_id(mut self, model_id: Option<String>) -> Self {
        self.model_id = model_id;
        self
    }
}

/// Reported token usage for one model completion.
///
/// `None` fields mean the adapter did not report usage — treated as unknown
/// (not zero) by the budget tracker when a token cap is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelUsage {
    /// Input tokens consumed; `None` when the provider did not report them.
    pub input_tokens: Option<usize>,
    /// Output tokens produced; `None` when the provider did not report them.
    pub output_tokens: Option<usize>,
}

impl ModelUsage {
    /// Creates usage with no reported tokens.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the reported input-token count.
    #[must_use]
    pub fn with_input_tokens(mut self, tokens: usize) -> Self {
        self.input_tokens = Some(tokens);
        self
    }

    /// Sets the reported output-token count.
    #[must_use]
    pub fn with_output_tokens(mut self, tokens: usize) -> Self {
        self.output_tokens = Some(tokens);
        self
    }

    /// Maps to the ACP per-turn usage when both input and output tokens are
    /// known; unknown stays unknown (never counted as zero).
    #[must_use]
    pub(crate) fn acp_usage(&self) -> Option<ee_agent_protocol::Usage> {
        let input_tokens = self.input_tokens?;
        let output_tokens = self.output_tokens?;
        Some(ee_agent_protocol::Usage::new(
            input_tokens.saturating_add(output_tokens) as u64,
            input_tokens as u64,
            output_tokens as u64,
        ))
    }
}

/// Builds an ACP prompt result, attaching reported token usage when known.
pub(crate) fn prompt_result_with_usage(
    stop_reason: ee_agent_protocol::StopReason,
    usage: ModelUsage,
) -> ee_acp_agent_server::PromptResult {
    let mut result = ee_acp_agent_server::PromptResult::new(stop_reason);
    if let Some(usage) = usage.acp_usage() {
        result = result.usage(usage);
    }
    result
}

/// Normalized model response for one completion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelResponse {
    /// Assistant text (empty when the model only reasoned or called tools).
    pub text: String,
    /// Reasoning text, kept separate from the answer.
    pub reasoning: Option<String>,
    /// Tool intents the model wants executed, in order.
    pub tool_intents: Vec<ToolIntent>,
    /// Subagent delegation intents (unsupported until the subagent phase).
    pub subagent_intents: Vec<SubagentIntent>,
    /// Whether the model signals turn completion.
    pub completed: bool,
    /// Reported token usage; unknown fields stay `None`.
    pub usage: ModelUsage,
}

impl ModelResponse {
    /// Creates an empty, incomplete response.
    #[must_use]
    pub fn new() -> Self {
        Self {
            text: String::new(),
            reasoning: None,
            tool_intents: Vec::new(),
            subagent_intents: Vec::new(),
            completed: false,
            usage: ModelUsage::new(),
        }
    }

    /// Sets the reported token usage.
    #[must_use]
    pub fn with_usage(mut self, usage: ModelUsage) -> Self {
        self.usage = usage;
        self
    }

    /// Sets the assistant text.
    #[must_use]
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.text = text.into();
        self
    }

    /// Sets the reasoning text.
    #[must_use]
    pub fn reasoning(mut self, reasoning: impl Into<String>) -> Self {
        self.reasoning = Some(reasoning.into());
        self
    }

    /// Sets the tool intents.
    #[must_use]
    pub fn tool_intents(mut self, intents: Vec<ToolIntent>) -> Self {
        self.tool_intents = intents;
        self
    }

    /// Sets the subagent intents.
    #[must_use]
    pub fn subagent_intents(mut self, intents: Vec<SubagentIntent>) -> Self {
        self.subagent_intents = intents;
        self
    }

    /// Marks the response as completing the turn.
    #[must_use]
    pub fn completed(mut self) -> Self {
        self.completed = true;
        self
    }

    /// Whether the response carries nothing (no text, reasoning, or intents).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
            && self.reasoning.as_deref().is_none_or(str::is_empty)
            && self.tool_intents.is_empty()
            && self.subagent_intents.is_empty()
    }
}

impl Default for ModelResponse {
    fn default() -> Self {
        Self::new()
    }
}

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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Transcript {
    /// Normalized messages in chronological order.
    pub messages: Vec<ModelMessage>,
    /// What the last [`Transcript::enforce_memory_limit`] call removed,
    /// when it removed anything.
    pub truncation: Option<TranscriptTruncation>,
}

impl Transcript {
    /// Creates an empty transcript.
    #[must_use]
    pub fn new() -> Self {
        Self { messages: Vec::new(), truncation: None }
    }

    /// Converts ACP prompt content into one normalized user message.
    ///
    /// Text blocks become text content, resource links become file
    /// references, and unsupported blocks (image, audio, embedded resource)
    /// are recorded in bounded diagnostic metadata instead of being dropped
    /// silently.
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
                ContentBlock::Image(_) => unsupported.push("image"),
                ContentBlock::Audio(_) => unsupported.push("audio"),
                ContentBlock::Resource(_) => unsupported.push("embedded_resource"),
                // The SDK enum is non-exhaustive; future block types are
                // recorded diagnostically instead of dropped silently.
                _ => unsupported.push("other"),
            }
        }
        let mut message = ModelMessage::new(ModelRole::User).with_content(content);
        if !unsupported.is_empty() {
            message = message.with_metadata([("unsupported_blocks".into(), unsupported.join(","))]);
        }
        Self { messages: vec![message], truncation: None }
    }

    /// Prepends a system message ahead of the prompt's user message, used to
    /// seed bounded memory facts before any model call.
    pub fn prepend_system(&mut self, text: impl Into<String>) {
        self.messages.insert(0, ModelMessage::text(ModelRole::System, text));
    }

    /// Appends the assistant message for a model response: text stays in
    /// content, reasoning stays in the separate reasoning summary.  Empty
    /// responses append nothing.
    pub fn push_assistant(&mut self, response: &ModelResponse) {
        let has_text = !response.text.is_empty();
        let has_reasoning = response.reasoning.as_deref().is_some_and(|text| !text.is_empty());
        if !has_text && !has_reasoning {
            return;
        }
        let mut message = ModelMessage::new(ModelRole::Assistant);
        if has_text {
            message = message.with_content(vec![ModelContent::Text(response.text.clone())]);
        }
        if let Some(reasoning) = response.reasoning.as_deref() {
            message = message.with_reasoning_summary(reasoning);
        }
        self.messages.push(message);
    }

    /// Appends a tool observation carrying the stable tool-call id.
    pub fn push_tool_result(&mut self, tool_call_id: impl Into<String>, result: ToolResult) {
        self.messages.push(ModelMessage::tool_result(tool_call_id, result));
    }

    /// Appends a subagent summary message (subagent phase).
    pub fn push_subagent_summary(&mut self, summary: impl Into<String>) {
        self.messages.push(ModelMessage::text(ModelRole::Subagent, summary));
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
            removed += 1;
        }
        self.truncation = (removed > 0).then_some(TranscriptTruncation {
            removed_messages: removed,
            removed_bytes,
            retained_bytes: total,
            limit_bytes,
        });
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
fn message_bytes(message: &ModelMessage) -> usize {
    serde_json::to_string(message).expect("normalized messages always serialize").len()
}

/// Model-adapter failure, distinct from loop failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelError {
    /// The underlying model backend failed.
    Adapter(String),
    /// The adapter returned a response shape the orchestrator rejects.
    InvalidResponse(String),
    /// The adapter observed cancellation and stopped.
    Cancelled,
}

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Adapter(message) => write!(f, "model adapter failed: {message}"),
            Self::InvalidResponse(message) => write!(f, "invalid model response: {message}"),
            Self::Cancelled => f.write_str("model call cancelled"),
        }
    }
}

impl std::error::Error for ModelError {}

/// Provider-supplied model interface.
///
/// Implementations must be `Send + Sync + 'static` and cancellation-aware:
/// `cancel` flips when the turn is cancelled, and adapters should stop
/// promptly instead of returning late results.
pub trait ModelAdapter: Send + Sync + 'static {
    /// Completes one normalized request.
    fn complete(
        &self,
        request: ModelRequest,
        cancel: watch::Receiver<bool>,
    ) -> ModelFuture<Result<ModelResponse, ModelError>>;

    /// Completes one request while forwarding displayable deltas as they arrive.
    ///
    /// Adapters without native streaming keep the same client contract by
    /// emitting their completed reasoning and text as single chunks.
    fn complete_streaming(
        &self,
        request: ModelRequest,
        cancel: watch::Receiver<bool>,
        events: crate::streaming::StreamSink,
    ) -> ModelFuture<Result<ModelResponse, ModelError>> {
        let completion = self.complete(request, cancel);
        Box::pin(async move {
            let response = completion.await?;
            if let Some(reasoning) = response.reasoning.as_deref().filter(|text| !text.is_empty()) {
                events.reasoning(reasoning.to_string())?;
            }
            if !response.text.is_empty() {
                events.text(response.text.clone())?;
            }
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use ee_acp_agent_server::PromptContext;
    use ee_agent_protocol::{
        AudioContent, ContentBlock, ImageContent, ResourceLink, SessionId, TextContent,
    };
    use serde_json::json;
    use tokio::sync::watch;

    use super::*;
    use crate::budget::BudgetSnapshot;
    use crate::tasks::TaskId;
    use crate::test_support::FakeModel;
    use crate::tools::{ToolDefinition, ToolIntent};

    fn prompt(ctx_blocks: Vec<ContentBlock>) -> PromptContext {
        PromptContext::new(SessionId::new("s-1"), ctx_blocks)
    }

    fn sample_request() -> ModelRequest {
        ModelRequest {
            transcript: vec![ModelMessage::text(ModelRole::User, "hello")],
            tools: vec![ToolDefinition::new("read_file", "reads a file")],
            budget: BudgetSnapshot {
                iterations_used: 1,
                iterations_max: 16,
                model_calls_used: 1,
                model_calls_max: 16,
                tool_calls_used: 0,
                tool_calls_max: 32,
                subagents_used: 0,
                subagents_max: 8,
                output_bytes_used: 0,
                output_bytes_max: 1024 * 1024,
                input_tokens_used: None,
                input_tokens_max: None,
                output_tokens_used: None,
                output_tokens_max: None,
            },
            task: TaskNode::new(TaskId::new("task-1"), "hello", "hello"),
            available_models: Vec::new(),
            model_id: None,
        }
    }

    #[test]
    fn request_roundtrips_through_json() {
        let request = sample_request();
        let json = serde_json::to_string(&request).expect("serializes");
        let restored: ModelRequest = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, request);
    }

    #[test]
    fn response_roundtrips_through_json() {
        let response = ModelResponse::new()
            .reasoning("think")
            .text("answer")
            .tool_intents(vec![ToolIntent::new(
                "tc-1",
                "read_file",
                serde_json::json!({"path": "/a"}),
            )])
            .completed();
        let json = serde_json::to_string(&response).expect("serializes");
        let restored: ModelResponse = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, response);
        assert!(restored.completed);
        assert!(!restored.is_empty());
    }

    #[test]
    fn empty_response_detection_ignores_blank_reasoning() {
        assert!(ModelResponse::new().is_empty());
        assert!(ModelResponse::new().reasoning("").is_empty());
        assert!(!ModelResponse::new().text("x").is_empty());
    }

    #[test]
    fn message_builders_set_role_and_content() {
        let message = ModelMessage::text(ModelRole::System, "be terse");
        assert_eq!(message.role, ModelRole::System);
        assert_eq!(message.content, vec![ModelContent::Text("be terse".into())]);
        assert_eq!(message.reasoning_summary, None);
    }

    #[test]
    fn metadata_is_bounded_and_deterministic() {
        let entries: Vec<(String, String)> =
            (0..MAX_METADATA_ENTRIES + 4).map(|index| (format!("k{index}"), "v".into())).collect();
        let message = ModelMessage::new(ModelRole::System).with_metadata(entries.clone());
        assert_eq!(message.metadata.len(), MAX_METADATA_ENTRIES);
        for (key, _) in &entries[..MAX_METADATA_ENTRIES] {
            assert!(message.metadata.contains_key(key), "entry {key} kept");
        }
        assert!(!message.metadata.contains_key("k16"), "overflow entry dropped");
        let json = serde_json::to_value(&message.metadata).expect("serializes");
        assert!(json.is_object(), "metadata stays a flat object");
    }

    #[test]
    fn prompt_text_converts_to_normalized_user_message() {
        let transcript = Transcript::from_prompt(&prompt(vec![ContentBlock::Text(
            TextContent::new("hello world"),
        )]));
        assert_eq!(transcript.messages().len(), 1);
        let message = &transcript.messages()[0];
        assert_eq!(message.role, ModelRole::User);
        assert_eq!(message.content, vec![ModelContent::Text("hello world".into())]);
        assert_eq!(message.reasoning_summary, None);
        assert!(message.metadata.is_empty());
        assert!(transcript.truncation().is_none());
    }

    #[test]
    fn prompt_resource_link_converts_to_file_reference() {
        let transcript = Transcript::from_prompt(&prompt(vec![
            ContentBlock::Text(TextContent::new("read it")),
            ContentBlock::ResourceLink(ResourceLink::new("notes", "file:///work/notes.md")),
        ]));
        let content = &transcript.messages()[0].content;
        assert_eq!(content.len(), 2);
        assert_eq!(content[0], ModelContent::Text("read it".into()));
        assert_eq!(
            content[1],
            ModelContent::FileReference { path: "file:///work/notes.md".into() }
        );
    }

    #[test]
    fn unsupported_prompt_blocks_are_recorded_not_dropped() {
        let transcript = Transcript::from_prompt(&prompt(vec![
            ContentBlock::Image(ImageContent::new("data", "image/png")),
            ContentBlock::Audio(AudioContent::new("data", "audio/mp3")),
            ContentBlock::Text(TextContent::new("keep")),
        ]));
        let message = &transcript.messages()[0];
        assert_eq!(message.content, vec![ModelContent::Text("keep".into())]);
        assert_eq!(
            message.metadata.get("unsupported_blocks").map(String::as_str),
            Some("image,audio"),
            "unsupported blocks recorded in bounded metadata"
        );
    }

    #[test]
    fn reasoning_kept_separate_from_assistant_text() {
        let mut transcript = Transcript::new();
        transcript.push_assistant(&ModelResponse::new().reasoning("plan").text("answer"));
        assert_eq!(transcript.messages().len(), 1);
        let message = &transcript.messages()[0];
        assert_eq!(message.role, ModelRole::Assistant);
        assert_eq!(message.content, vec![ModelContent::Text("answer".into())]);
        assert_eq!(message.reasoning_summary.as_deref(), Some("plan"));
    }

    #[test]
    fn push_assistant_skips_empty_responses() {
        let mut transcript = Transcript::new();
        transcript.push_assistant(&ModelResponse::new());
        transcript.push_assistant(&ModelResponse::new().reasoning(""));
        assert!(transcript.is_empty());
    }

    #[test]
    fn tool_result_appends_stable_tool_call_id() {
        let mut transcript = Transcript::new();
        transcript.push_tool_result("tc-9", ToolResult::success("done"));
        assert_eq!(transcript.messages().len(), 1);
        let message = &transcript.messages()[0];
        assert_eq!(message.role, ModelRole::Tool);
        let ModelContent::ToolResult { tool_call_id, result } = &message.content[0] else {
            panic!("expected tool result content");
        };
        assert_eq!(tool_call_id, "tc-9");
        assert!(result.success);
    }

    #[test]
    fn subagent_summary_appends_subagent_message() {
        let mut transcript = Transcript::new();
        transcript.push_subagent_summary("subagent finished");
        let message = &transcript.messages()[0];
        assert_eq!(message.role, ModelRole::Subagent);
        assert_eq!(message.content, vec![ModelContent::Text("subagent finished".into())]);
    }

    #[test]
    fn transcript_truncation_preserves_newest_and_records_metadata() {
        let mut transcript = Transcript::new();
        for index in 0..6 {
            transcript.push_assistant(&ModelResponse::new().text(format!("message {index}")));
        }
        assert_eq!(transcript.messages().len(), 6);
        let newest_two =
            message_bytes(&transcript.messages()[4]) + message_bytes(&transcript.messages()[5]);

        transcript.enforce_memory_limit(newest_two);

        assert_eq!(transcript.messages().len(), 2);
        assert_eq!(
            transcript.messages()[0].content,
            vec![ModelContent::Text("message 4".into())],
            "oldest messages dropped first"
        );
        assert_eq!(transcript.messages()[1].content, vec![ModelContent::Text("message 5".into())]);
        let truncation = transcript.truncation().expect("records truncation");
        assert_eq!(truncation.removed_messages, 4);
        assert!(truncation.removed_bytes > 0);
        assert_eq!(truncation.retained_bytes, newest_two);
        assert_eq!(truncation.limit_bytes, newest_two);
    }

    #[test]
    fn enforce_memory_limit_keeps_single_oversized_message() {
        let mut transcript = Transcript::new();
        transcript.push_assistant(&ModelResponse::new().text("x".repeat(10_000)));
        transcript.enforce_memory_limit(1);
        assert_eq!(transcript.messages().len(), 1, "newest message is never dropped");
        assert!(transcript.truncation().is_none());
    }

    #[test]
    fn enforce_memory_limit_within_budget_clears_truncation() {
        let mut transcript = Transcript::new();
        transcript.push_assistant(&ModelResponse::new().text("hi"));
        transcript.enforce_memory_limit(1_000);
        assert!(transcript.truncation().is_none());
    }

    #[test]
    fn normalized_transcript_json_has_no_provider_specific_fields() {
        let mut transcript = Transcript::new();
        transcript.push_assistant(&ModelResponse::new().reasoning("r").text("a"));
        let json = serde_json::to_value(&transcript).expect("serializes");
        let message = &json["messages"][0];
        let mut keys: Vec<String> =
            message.as_object().expect("message object").keys().cloned().collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["content", "metadata", "reasoning_summary", "role", "trust"]);
        let serialized = serde_json::to_string(&transcript).expect("serializes");
        assert!(!serialized.contains("provider"), "no provider fields: {serialized}");
        let restored: Transcript = serde_json::from_str(&serialized).expect("parses");
        assert_eq!(restored, transcript);
    }

    #[tokio::test]
    async fn tool_intent_parsing_from_fake_model_response() {
        let model = FakeModel::new(vec![ModelResponse::new().tool_intents(vec![ToolIntent::new(
            "tc-1",
            "read_file",
            json!({"path": "/a"}),
        )])]);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let response = model.complete(sample_request(), cancel_rx).await.expect("fake succeeds");
        assert_eq!(response.tool_intents.len(), 1);
        let intent = &response.tool_intents[0];
        assert_eq!(intent.tool_call_id, "tc-1");
        assert_eq!(intent.name, "read_file");
        assert_eq!(intent.arguments, json!({"path": "/a"}));
    }

    #[tokio::test]
    async fn subagent_intent_parsing_from_fake_model_response() {
        let model = FakeModel::new(vec![
            ModelResponse::new()
                .subagent_intents(vec![SubagentIntent::new("investigate the failure")]),
        ]);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let response = model.complete(sample_request(), cancel_rx).await.expect("fake succeeds");
        assert_eq!(response.subagent_intents.len(), 1);
        assert_eq!(response.subagent_intents[0].task_description, "investigate the failure");
    }

    #[tokio::test]
    async fn empty_script_exhaustion_returns_empty_response() {
        let model = FakeModel::new(Vec::new());
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let response = model.complete(sample_request(), cancel_rx).await.expect("fake succeeds");
        assert!(response.is_empty());
        assert!(!response.completed);
    }
}
