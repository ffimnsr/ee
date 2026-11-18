//! Chat Completions wire format: response decoding, SSE, normalization.
//!
//! Everything here is a property of the OpenAI-compatible Chat Completions
//! dialect, not of a provider: buffered responses, Server-Sent Events framing,
//! streamed delta accumulation, usage extraction, and the mapping between the
//! wire shape and the normalized orchestrator transcript. Request assembly and
//! transcript conversion live in [`crate::request`] and are re-exported here.
//!
//! [`AssistantTurn`] and the normalization helpers
//! ([`response_from_turn`], [`content_text`], [`tool_call_id_of`]) are shared by
//! every dialect: Responses and Messages decoders build the same turn shape and
//! normalize it identically.

use std::collections::BTreeMap;

use ee_agent_acp_server::ProviderError;
use ee_agent_orchestrator::{ModelContent, ModelResponse, ModelUsage, ToolIntent};
use serde_json::{Value, json};

pub use crate::error_classification::{
    classify_http_error, classify_status_error, http_error_message, is_retryable, is_timeout,
    is_unreachable, parse_rate_limit, parse_retry_after, parse_should_retry, retry_after_of,
    retry_delay, timeout_phase, unreachable_delay,
};
use crate::reasoning_replay::{append_reasoning_detail, native_reasoning_of};
pub use crate::request::{
    ChatRequestParts, RequestBodyOptions, chat_request_parts, messages_from_transcript,
    messages_from_transcript_replaying, messages_from_transcript_with_cache, request_body,
    tool_choice_value, tools_from_definitions, tools_from_definitions_with,
};
use crate::transport::DeltaSink;

/// One decoded assistant turn.
///
/// Dialect decoders (Chat Completions, OpenAI Responses, Anthropic Messages) all
/// produce this shape, so normalization into a [`ModelResponse`] happens in one
/// place and cannot drift between dialects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantTurn {
    /// Plain-text content of the final answer (may be empty).
    pub content: String,
    /// Reasoning text, if the model emitted any.
    pub reasoning: String,
    /// Dialect-native assistant payload, kept for diagnostics and for
    /// dialect-aware transcript appending. Never re-encoded into another
    /// dialect.
    pub raw: Value,
    /// Reasoning-specific provider state to replay verbatim on the next turn
    /// (thinking blocks with signatures, `reasoning_details`, encrypted
    /// reasoning items). `None` when the dialect produced no replayable state.
    pub native_reasoning: Option<Value>,
    /// Tool calls the model requested.
    pub tool_calls: Vec<ToolCall>,
    /// Canonical stop signal: `stop` for a natural end, `tool_calls` when the
    /// model asked for tools, `length` when the output bound stopped it, plus
    /// any dialect-specific reason. Missing means a natural end.
    pub finish_reason: Option<String>,
    /// Token usage reported for this round trip, when present.
    pub usage: Option<Usage>,
    /// Provider response id usable for continuation, when the route stores state.
    pub response_id: Option<String>,
}

/// One tool call requested by the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// Model-assigned tool call id, echoed back on the tool result.
    pub id: String,
    /// Requested function name, as the model wrote it.
    pub name: String,
    /// Parsed tool arguments object.
    pub arguments: Value,
    /// Whether the provider executed this tool itself (hosted search, code
    /// interpreter, MCP). Provider-executed calls are never dispatched locally.
    pub provider_executed: bool,
    /// Provider-supplied result for a provider-executed call, verbatim.
    pub provider_result: Option<Value>,
    /// Bounded parse failure when `arguments` is `Null` because the model emitted
    /// malformed JSON. The loop feeds this text back so the model can retry.
    pub argument_error: Option<String>,
}

impl ToolCall {
    /// Creates a locally executed call with parsed arguments.
    #[must_use]
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
            provider_executed: false,
            provider_result: None,
            argument_error: None,
        }
    }

    /// Marks the call as executed by the provider.
    #[must_use]
    pub fn provider_executed(mut self) -> Self {
        self.provider_executed = true;
        self
    }

    /// Attaches the provider's result payload.
    #[must_use]
    pub fn with_provider_result(mut self, result: Value) -> Self {
        self.provider_result = Some(result);
        self
    }

    /// Attaches the bounded parse failure that explains `Null` arguments.
    #[must_use]
    pub fn with_argument_error(mut self, error: impl Into<String>) -> Self {
        self.argument_error = Some(error.into());
        self
    }
}

/// Token usage reported for one round trip.
///
/// `None` fields mean the endpoint did not report them — treated as unknown,
/// never counted as zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// `usage.prompt_tokens`.
    pub input_tokens: Option<u64>,
    /// `usage.completion_tokens`.
    pub output_tokens: Option<u64>,
    /// `usage.total_tokens`; may exceed `input + output` when cached tokens are
    /// billed separately.
    pub total_tokens: Option<u64>,
    /// Prompt tokens served from the provider cache.
    pub cached_input_tokens: Option<u64>,
    /// Prompt tokens written to the provider cache.
    pub cache_creation_input_tokens: Option<u64>,
    /// Hidden reasoning tokens; a subset of `output_tokens` when reported.
    pub reasoning_tokens: Option<u64>,
    /// Effective context size of the final iteration, when reported.
    pub context_tokens: Option<u64>,
    /// Provider-reported cost in integer micro-dollars, when reported.
    pub cost_micros: Option<u64>,
}

/// One displayable streaming delta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamDelta {
    /// Partial assistant text.
    Text(String),
    /// Partial model reasoning.
    Reasoning(String),
    /// A tool call started streaming its arguments.
    ToolInputStart {
        /// Provider-assigned call id.
        id: String,
        /// Requested function name.
        name: String,
    },
    /// One partial tool-argument chunk.
    ToolInputDelta {
        /// Provider-assigned call id.
        id: String,
        /// Raw JSON text appended by this chunk.
        text: String,
        /// Best-effort preview of the arguments so far; display-only.
        parsed: Option<Value>,
    },
    /// A tool call finished streaming its arguments.
    ToolInputEnd {
        /// Provider-assigned call id.
        id: String,
    },
    /// Token usage reported by a streamed chunk.
    Usage(Usage),
    /// One raw SSE frame, emitted only when the profile opts in.
    Raw {
        /// Verbatim event data.
        frame: String,
    },
}

/// Builds a codec failure whose text names the routed endpoint, never a
/// credential.
///
/// Shared by every dialect codec so a provider's error wording stays identical
/// across Chat Completions, Responses, and Messages.
#[must_use]
pub fn codec_error(label: &str, detail: impl std::fmt::Display) -> ProviderError {
    ProviderError::BackendFailure(format!("{label}: {detail}"))
}

/// Parses tool arguments from their wire form.
///
/// An empty value means "no arguments"; malformed JSON or a non-object keeps the
/// call identity with `Null` arguments, so the orchestrator rejects the call
/// before any tool side effect.
#[must_use]
pub fn parse_arguments(raw: &str) -> Value {
    parse_arguments_checked(raw).0
}

/// Parses tool arguments and reports why an invalid payload was rejected.
///
/// Returns `(Null, Some(message))` for malformed JSON or a non-object, and
/// `(json!({}), None)` for empty arguments. The message carries a bounded raw
/// excerpt so the model can correct itself without the loop logging the whole
/// payload.
#[must_use]
pub fn parse_arguments_checked(raw: &str) -> (Value, Option<String>) {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return (json!({}), None);
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(value) if value.is_object() => (value, None),
        Ok(_) => (Value::Null, Some(String::from("tool arguments must be a JSON object"))),
        Err(error) => {
            let excerpt: String = trimmed.chars().take(256).collect();
            (
                Value::Null,
                Some(format!("tool arguments were not valid JSON: {error}; raw: {excerpt}")),
            )
        }
    }
}

/// Converts reported wire usage into the orchestrator's usage record.
///
/// A field the provider omitted stays `None`; it is never counted as zero.
#[must_use]
pub fn model_usage_of(reported: Usage) -> ModelUsage {
    let mut usage = ModelUsage::new();
    usage.input_tokens = reported.input_tokens.and_then(|tokens| usize::try_from(tokens).ok());
    usage.output_tokens = reported.output_tokens.and_then(|tokens| usize::try_from(tokens).ok());
    usage.cached_input_tokens =
        reported.cached_input_tokens.and_then(|tokens| usize::try_from(tokens).ok());
    usage.cache_creation_input_tokens =
        reported.cache_creation_input_tokens.and_then(|tokens| usize::try_from(tokens).ok());
    usage.reasoning_tokens =
        reported.reasoning_tokens.and_then(|tokens| usize::try_from(tokens).ok());
    usage.context_tokens = reported.context_tokens.and_then(|tokens| usize::try_from(tokens).ok());
    usage.cost_micros = reported.cost_micros;
    usage
}

/// Converts a decoded assistant turn into a normalized [`ModelResponse`]:
/// text, reasoning, tool intents, usage, and the completion signal derived from
/// `finish_reason`.
#[must_use]
pub fn response_from_turn(answer: AssistantTurn) -> ModelResponse {
    let completed =
        answer.tool_calls.is_empty() && answer.finish_reason.as_deref().unwrap_or("stop") == "stop";
    let intents = answer
        .tool_calls
        .into_iter()
        .map(|call| {
            let mut intent = ToolIntent::new(call.id, tool_name(&call.name), call.arguments)
                .with_provider_executed(call.provider_executed);
            if let Some(result) = call.provider_result {
                intent = intent.with_provider_result(result);
            }
            if let Some(error) = call.argument_error {
                intent = intent.with_argument_error(error);
            }
            intent
        })
        .collect();
    let reported = answer.usage.unwrap_or_default();
    let usage = model_usage_of(reported);
    let mut response =
        ModelResponse::new().text(answer.content).tool_intents(intents).with_usage(usage);
    response = response.with_finish_reason(answer.finish_reason);
    response = response.with_response_id(answer.response_id);
    response = response.with_native(
        answer.native_reasoning.and_then(ee_agent_orchestrator::OpaqueProviderState::new),
    );
    if !answer.reasoning.is_empty() {
        response = response.reasoning(answer.reasoning);
    }
    if completed {
        response = response.completed();
    }
    response
}

/// Decodes a Chat Completions `choices[0].message`; returns `None` when the
/// shape is missing.
#[must_use]
pub fn decode_message(value: &Value) -> Option<AssistantTurn> {
    let message = value.pointer("/choices/0/message")?;
    let content = message_content(message.get("content").unwrap_or(&Value::Null));
    let reasoning = reasoning_of(message);
    let tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| calls.iter().filter_map(tool_call_of).collect())
        .unwrap_or_default();
    let finish_reason =
        value.pointer("/choices/0/finish_reason").and_then(Value::as_str).map(str::to_string);
    let native_reasoning = native_reasoning_of(message);
    Some(AssistantTurn {
        content,
        reasoning,
        raw: message.clone(),
        native_reasoning,
        tool_calls,
        finish_reason,
        usage: usage_of(value),
        response_id: None,
    })
}

/// Emits a finished response's reasoning and text as single deltas.
///
/// A buffered fallback still reaches the caller in stream order, so clients see
/// the same event sequence either way.
///
/// # Errors
///
/// Returns the sink failure when the consumer is already gone.
pub fn emit_finished_deltas(
    response: &ModelResponse,
    sink: DeltaSink<'_>,
) -> Result<(), ProviderError> {
    if let Some(reasoning) = response.reasoning.as_deref().filter(|text| !text.is_empty()) {
        sink(StreamDelta::Reasoning(reasoning.to_string()))?;
    }
    if !response.text.is_empty() {
        sink(StreamDelta::Text(response.text.clone()))?;
    }
    Ok(())
}

mod usage;

pub use usage::usage_from_tokens;
pub(crate) use usage::usage_of;

/// Extracts reasoning text from first present of `reasoning`,
/// `reasoning_content`, or `thinking`.
fn reasoning_of(message: &Value) -> String {
    for pointer in ["/reasoning", "/reasoning_content", "/thinking"] {
        if let Some(text) = message.pointer(pointer).and_then(Value::as_str)
            && !text.is_empty()
        {
            return text.to_string();
        }
    }
    String::new()
}

/// Extracts plain text from a string or a parts array of text chunks.
fn message_content(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    if let Some(parts) = content.as_array() {
        return parts.iter().filter_map(|part| part.get("text").and_then(Value::as_str)).collect();
    }
    String::new()
}

/// Decodes one tool call; `arguments` may arrive as a JSON string or object.
fn tool_call_of(value: &Value) -> Option<ToolCall> {
    let id = value.get("id").and_then(Value::as_str)?.to_string();
    let function = value.get("function")?;
    let name = function.get("name").and_then(Value::as_str)?.to_string();
    let (arguments, argument_error) = match function.get("arguments")? {
        Value::String(text) => parse_arguments_checked(text),
        value => (value.clone(), None),
    };
    let mut call = ToolCall::new(id, name, arguments);
    if let Some(error) = argument_error {
        call = call.with_argument_error(error);
    }
    Some(call)
}

/// Renders one message's content blocks as text.
///
/// Shared by every dialect's transcript conversion; multimodal payloads become
/// stable placeholders so text-only summaries never carry raw base64.
#[must_use]
pub fn content_text(content: &[ModelContent]) -> String {
    let mut parts = Vec::new();
    for block in content {
        match block {
            ModelContent::Text(text) => parts.push(text.clone()),
            ModelContent::ToolResult { result, .. } => parts.push(result.summary_text()),
            ModelContent::FileReference { path } => parts.push(format!("[file:{path}]")),
            ModelContent::TerminalReference { terminal_id } => {
                parts.push(format!("[terminal:{terminal_id}]"))
            }
            ModelContent::Image { mime_type, uri, .. } => {
                parts.push(match uri {
                    Some(uri) => format!("[image:{mime_type}:{uri}]"),
                    None => format!("[image:{mime_type}]"),
                });
            }
            ModelContent::Audio { mime_type, .. } => parts.push(format!("[audio:{mime_type}]")),
            ModelContent::EmbeddedResource { uri, mime_type, text, blob } => match (text, blob) {
                (Some(text), _) => parts.push(format!("[resource:{uri}]\n{text}")),
                (None, Some(_)) => {
                    let mime = mime_type.as_deref().unwrap_or("application/octet-stream");
                    parts.push(format!("[resource:{uri}:{mime}]"));
                }
                (None, None) => parts.push(format!("[resource:{uri}]")),
            },
            _ => {} // future content kinds stay out of the text view
        }
    }
    parts.join("\n")
}

/// Whether any block needs the multimodal parts array instead of plain text.
#[must_use]
pub fn content_has_media(content: &[ModelContent]) -> bool {
    content.iter().any(|block| {
        matches!(
            block,
            ModelContent::Image { .. }
                | ModelContent::Audio { .. }
                | ModelContent::EmbeddedResource { blob: Some(_), text: None, .. }
        )
    })
}

/// Converts content blocks into OpenAI-compatible chat content parts.
#[must_use]
pub fn content_parts(content: &[ModelContent]) -> Vec<Value> {
    let mut parts = Vec::new();
    for block in content {
        match block {
            ModelContent::Text(text) => push_text_part(&mut parts, text),
            ModelContent::ToolResult { result, .. } => {
                push_text_part(&mut parts, &result.summary_text())
            }
            ModelContent::FileReference { path } => {
                push_text_part(&mut parts, &format!("[file:{path}]"))
            }
            ModelContent::TerminalReference { terminal_id } => {
                push_text_part(&mut parts, &format!("[terminal:{terminal_id}]"))
            }
            ModelContent::Image { data, mime_type, .. } => {
                parts.push(image_part(data, mime_type));
            }
            ModelContent::Audio { data, mime_type } => {
                parts.push(audio_part(data, mime_type));
            }
            ModelContent::EmbeddedResource { uri, mime_type, text, blob } => match (text, blob) {
                (Some(text), _) => push_text_part(&mut parts, &format!("[resource:{uri}]\n{text}")),
                (None, Some(blob)) => match mime_type.as_deref() {
                    Some(mime) if mime.starts_with("image/") => {
                        parts.push(image_part(blob, mime));
                    }
                    Some(mime) if mime.starts_with("audio/") => {
                        parts.push(audio_part(blob, mime));
                    }
                    mime => push_text_part(
                        &mut parts,
                        &format!("[resource:{uri}:{}]", mime.unwrap_or("application/octet-stream")),
                    ),
                },
                (None, None) => push_text_part(&mut parts, &format!("[resource:{uri}]")),
            },
            _ => {} // future content kinds stay out of the request
        }
    }
    parts
}

fn push_text_part(parts: &mut Vec<Value>, text: &str) {
    if !text.is_empty() {
        parts.push(json!({ "type": "text", "text": text }));
    }
}

fn image_part(data: &str, mime_type: &str) -> Value {
    json!({
        "type": "image_url",
        "image_url": { "url": format!("data:{mime_type};base64,{data}") },
    })
}

fn audio_part(data: &str, mime_type: &str) -> Value {
    json!({
        "type": "input_audio",
        "input_audio": { "data": data, "format": audio_format(mime_type) },
    })
}

fn audio_format(mime_type: &str) -> &str {
    match mime_type.rsplit('/').next().unwrap_or(mime_type) {
        "mpeg" => "mp3",
        "x-wav" | "wave" => "wav",
        other => other,
    }
}

/// Stable tool-call id of a tool observation message.
///
/// Shared by every dialect so tool results stay correlated with the call that
/// requested them.
#[must_use]
pub fn tool_call_id_of(content: &[ModelContent]) -> String {
    content
        .iter()
        .find_map(|block| match block {
            ModelContent::ToolResult { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Maps a model tool-call name onto the tool registry's name. The historical
/// `tool_read_file` alias maps to the built-in `read_file` tool; this is an ee
/// naming compatibility shim, not a provider behavior.
fn tool_name(name: &str) -> String {
    match name {
        "tool_read_file" => "read_file".to_string(),
        other => other.to_string(),
    }
}

/// Incremental SSE framing state. Events are decoded only once their blank line
/// terminator has arrived, so split UTF-8 sequences remain intact.
#[derive(Debug)]
pub(crate) struct SseDecoder {
    label: String,
    pending: Vec<u8>,
}

impl SseDecoder {
    pub(crate) fn new(label: &str) -> Self {
        Self { label: label.to_string(), pending: Vec::new() }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, ProviderError> {
        self.pending.extend_from_slice(bytes);
        self.drain_complete()
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<String>, ProviderError> {
        if self.pending.is_empty() {
            return Ok(Vec::new());
        }
        let event = std::mem::take(&mut self.pending);
        parse_sse_event(&event, &self.label).map(|event| event.into_iter().collect())
    }

    fn drain_complete(&mut self) -> Result<Vec<String>, ProviderError> {
        let mut events = Vec::new();
        while let Some((event_end, delimiter_len)) = sse_event_boundary(&self.pending) {
            let event: Vec<u8> = self.pending.drain(..event_end).collect();
            self.pending.drain(..delimiter_len);
            if let Some(data) = parse_sse_event(&event, &self.label)? {
                events.push(data);
            }
        }
        Ok(events)
    }
}

fn sse_event_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    for index in 0..bytes.len() {
        if bytes[index..].starts_with(b"\r\n\r\n") {
            return Some((index, 4));
        }
        if bytes[index..].starts_with(b"\n\n") {
            return Some((index, 2));
        }
    }
    None
}

fn parse_sse_event(bytes: &[u8], label: &str) -> Result<Option<String>, ProviderError> {
    let event = std::str::from_utf8(bytes).map_err(|error| {
        ProviderError::BackendFailure(format!("{label} SSE event was not UTF-8: {error}"))
    })?;
    let data: Vec<&str> = event
        .lines()
        .filter_map(|line| {
            line.strip_prefix("data:").map(|data| data.strip_prefix(' ').unwrap_or(data))
        })
        .collect();
    Ok((!data.is_empty()).then(|| data.join("\n")))
}

/// Accumulates streamed deltas into one assistant message.
#[derive(Debug)]
pub(crate) struct StreamAccumulator {
    label: String,
    content: String,
    reasoning: String,
    /// Structured reasoning entries, merged as they stream so the replayed
    /// array keeps the provider's own entry boundaries and indexes.
    native_details: Vec<Value>,
    /// Scalar reasoning field the provider streamed, with its accumulated text.
    native_scalar: Option<(String, String)>,
    /// Opaque per-turn reasoning token the provider echoed.
    native_opaque: Option<String>,
    tool_calls: BTreeMap<usize, PendingToolCall>,
    finish_reason: Option<String>,
    /// Latest reported usage; chunks carry cumulative totals, so later
    /// occurrences replace earlier ones.
    usage: Option<Usage>,
}

#[derive(Debug, Default)]
struct PendingToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
    /// Provider-native tool-call metadata, for example Gemini `extra_content`.
    extra_content: Option<Value>,
    started: bool,
    ended: bool,
}

impl StreamAccumulator {
    pub(crate) fn new(label: &str) -> Self {
        Self {
            label: label.to_string(),
            content: String::new(),
            reasoning: String::new(),
            native_details: Vec::new(),
            native_scalar: None,
            native_opaque: None,
            tool_calls: BTreeMap::new(),
            finish_reason: None,
            usage: None,
        }
    }

    pub(crate) fn apply(&mut self, value: &Value) -> Result<Vec<StreamDelta>, ProviderError> {
        if let Some(error) = value.pointer("/error/message").and_then(Value::as_str) {
            return Err(ProviderError::BackendFailure(format!(
                "{} stream error: {error}",
                self.label
            )));
        }
        let Some(choice) = value.pointer("/choices/0") else {
            return Ok(Vec::new());
        };
        let mut output = Vec::new();
        if let Some(finish_reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(finish_reason.to_string());
        }
        if let Some(usage) = usage_of(value) {
            self.usage = Some(usage);
            output.push(StreamDelta::Usage(usage));
        }
        let Some(delta) = choice.get("delta") else {
            return Ok(output);
        };
        if let Some(content) =
            delta.get("content").and_then(Value::as_str).filter(|text| !text.is_empty())
        {
            self.content.push_str(content);
            output.push(StreamDelta::Text(content.to_string()));
        }
        let reasoning = reasoning_of(delta);
        if !reasoning.is_empty() {
            self.reasoning.push_str(&reasoning);
            output.push(StreamDelta::Reasoning(reasoning));
        }
        self.absorb_native_reasoning(delta);
        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in tool_calls {
                output.extend(self.apply_tool_call_delta(call)?);
            }
        }
        // Chat Completions has no per-call stop event: the terminal
        // `finish_reason` closes every pending call.
        if self.finish_reason.is_some() {
            let ids: Vec<String> = self
                .tool_calls
                .values_mut()
                .filter(|call| !call.ended)
                .filter_map(|call| {
                    call.ended = true;
                    call.id.clone()
                })
                .collect();
            output.extend(ids.into_iter().map(|id| StreamDelta::ToolInputEnd { id }));
        }
        Ok(output)
    }

    /// Records dialect-native reasoning state so the next turn can replay it.
    ///
    /// The displayed reasoning text and the replay payload are separate: the
    /// first is flattened for the client, the second keeps the provider's own
    /// structure (including `index`) so the provider accepts it back.
    fn absorb_native_reasoning(&mut self, delta: &Value) {
        if let Some(details) = delta.get("reasoning_details").and_then(Value::as_array) {
            for detail in details {
                append_reasoning_detail(&mut self.native_details, detail.clone());
            }
        }
        if self.native_scalar.is_none() {
            for key in ["reasoning_content", "reasoning", "reasoning_text"] {
                if delta.get(key).and_then(Value::as_str).is_some_and(|text| !text.is_empty()) {
                    self.native_scalar = Some((key.to_string(), String::new()));
                    break;
                }
            }
        }
        if let Some((key, text)) = self.native_scalar.as_mut()
            && let Some(chunk) = delta.get(key.as_str()).and_then(Value::as_str)
        {
            text.push_str(chunk);
        }
        if let Some(opaque) =
            delta.get("reasoning_opaque").and_then(Value::as_str).filter(|value| !value.is_empty())
        {
            self.native_opaque = Some(opaque.to_string());
        }
    }

    /// Replay payload for this turn, when the stream produced any.
    fn native_reasoning(&self) -> Option<Value> {
        let mut object = serde_json::Map::new();
        if !self.native_details.is_empty() {
            object.insert("reasoning_details".into(), Value::Array(self.native_details.clone()));
        }
        if let Some((key, text)) = self.native_scalar.as_ref()
            && !text.is_empty()
        {
            object.insert(key.clone(), Value::String(text.clone()));
        }
        if let Some(opaque) = self.native_opaque.as_ref() {
            object.insert("reasoning_opaque".into(), Value::String(opaque.clone()));
        }
        let mut per_call = serde_json::Map::new();
        for call in self.tool_calls.values() {
            let Some(id) = call.id.as_deref().filter(|id| !id.is_empty()) else {
                continue;
            };
            let Some(extra) = call.extra_content.as_ref() else {
                continue;
            };
            per_call.insert(id.to_string(), json!({ "extra_content": extra }));
        }
        if !per_call.is_empty() {
            object.insert("tool_calls".into(), Value::Object(per_call));
        }
        (!object.is_empty()).then_some(Value::Object(object))
    }

    fn apply_tool_call_delta(&mut self, value: &Value) -> Result<Vec<StreamDelta>, ProviderError> {
        let index = value.get("index").and_then(Value::as_u64).ok_or_else(|| {
            ProviderError::BackendFailure(format!("{} stream tool call missing index", self.label))
        })? as usize;
        let mut output = Vec::new();
        let call = self.tool_calls.entry(index).or_default();
        if let Some(extra) = value.get("extra_content").filter(|value| !value.is_null()) {
            call.extra_content = Some(extra.clone());
        }
        if let Some(id) = value.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()) {
            call.id = Some(id.to_string());
        }
        if let Some(function) = value.get("function") {
            if let Some(name) =
                function.get("name").and_then(Value::as_str).filter(|name| !name.is_empty())
            {
                call.name = Some(name.to_string());
            }
            if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                call.arguments.push_str(arguments);
                // Identity may arrive on the first delta for this index, so the
                // start event is emitted as soon as both halves are known.
                if !call.started
                    && let (Some(id), Some(name)) = (call.id.clone(), call.name.clone())
                {
                    call.started = true;
                    output.push(StreamDelta::ToolInputStart { id, name });
                }
                if !arguments.is_empty()
                    && let Some(id) = call.id.clone()
                {
                    output.push(StreamDelta::ToolInputDelta {
                        id,
                        text: arguments.to_string(),
                        parsed: crate::partial_json::parse_partial(&call.arguments),
                    });
                }
            }
        }
        Ok(output)
    }

    pub(crate) fn finish(self) -> Result<AssistantTurn, ProviderError> {
        let native_reasoning = self.native_reasoning();
        let mut tool_calls = Vec::with_capacity(self.tool_calls.len());
        for (_index, call) in self.tool_calls {
            let id = call.id.ok_or_else(|| {
                ProviderError::BackendFailure(format!("{} stream tool call missing id", self.label))
            })?;
            let name = call.name.ok_or_else(|| {
                ProviderError::BackendFailure(format!(
                    "{} stream tool call missing function name",
                    self.label
                ))
            })?;
            // Keep the call identity so the tool loop can feed an invalid-input
            // observation back to the model. `Null` is never a valid tool
            // argument object, so validation fails before any tool side effect.
            let (arguments, argument_error) = parse_arguments_checked(&call.arguments);
            let mut tool_call = ToolCall::new(id, name, arguments);
            if let Some(error) = argument_error {
                tool_call = tool_call.with_argument_error(error);
            }
            tool_calls.push(tool_call);
        }
        let raw_tool_calls: Vec<Value> = tool_calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.id,
                    "type": "function",
                    "function": {
                        "name": call.name,
                        "arguments": call.arguments.to_string(),
                    },
                })
            })
            .collect();
        let mut raw = json!({
            "role": "assistant",
            "content": (!self.content.is_empty()).then_some(self.content.clone()),
        });
        if !raw_tool_calls.is_empty() {
            raw["tool_calls"] = Value::Array(raw_tool_calls);
        }
        if !self.reasoning.is_empty() {
            raw["reasoning"] = Value::String(self.reasoning.clone());
        }
        if let Some(Value::Object(fields)) = native_reasoning.as_ref()
            && let Some(object) = raw.as_object_mut()
        {
            for (key, value) in fields {
                object.insert(key.clone(), value.clone());
            }
        }
        Ok(AssistantTurn {
            content: self.content,
            reasoning: self.reasoning,
            raw,
            native_reasoning,
            tool_calls,
            finish_reason: self.finish_reason,
            usage: self.usage,
            response_id: None,
        })
    }
}

#[cfg(test)]
mod tests;
