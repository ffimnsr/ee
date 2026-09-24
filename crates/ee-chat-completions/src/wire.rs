//! Chat Completions wire format: request bodies, response decoding, SSE.
//!
//! Everything here is a property of the OpenAI-compatible Chat Completions
//! dialect, not of a provider: buffered responses, Server-Sent Events framing,
//! streamed delta accumulation, usage extraction, and the mapping between the
//! wire shape and the normalized orchestrator transcript.
//!
//! [`AssistantTurn`] and the normalization helpers
//! ([`response_from_turn`], [`content_text`], [`tool_call_id_of`]) are shared by
//! every dialect: Responses and Messages decoders build the same turn shape and
//! normalize it identically.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ee_acp_agent_server::ProviderError;
use ee_agent_orchestrator::{
    ModelContent, ModelMessage, ModelResponse, ModelRole, ModelUsage, ToolDefinition, ToolIntent,
};
use reqwest::header::HeaderMap;
use serde_json::{Value, json};

use crate::profile::RetryPolicy;
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
    /// Tool calls the model requested.
    pub tool_calls: Vec<ToolCall>,
    /// Canonical stop signal: `stop` for a natural end, `tool_calls` when the
    /// model asked for tools, `length` when the output bound stopped it, plus
    /// any dialect-specific reason. Missing means a natural end.
    pub finish_reason: Option<String>,
    /// Token usage reported for this round trip, when present.
    pub usage: Option<Usage>,
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
}

/// One displayable streaming delta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamDelta {
    /// Partial assistant text.
    Text(String),
    /// Partial model reasoning.
    Reasoning(String),
}

/// Builds a Chat Completions request body.
///
/// `extensions` must be a JSON object when present; its fields are merged last
/// so a provider can shape the request without editing shared code.
#[must_use]
pub fn request_body(
    model: &str,
    messages: &[Value],
    tools: &[Value],
    stream: bool,
    extensions: Option<&Value>,
) -> Value {
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": stream,
        "tool_choice": "auto",
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools.to_vec());
    }
    if let Some(Value::Object(extra)) = extensions
        && let Some(object) = body.as_object_mut()
    {
        for (key, value) in extra {
            object.insert(key.clone(), value.clone());
        }
    }
    body
}

/// Converts a normalized transcript into chat messages, prepending
/// `system_prompt`.
///
/// Tool observations carry their stable tool-call id; subagent summaries map to
/// user content.
#[must_use]
pub fn messages_from_transcript(system_prompt: &str, transcript: &[ModelMessage]) -> Vec<Value> {
    let mut messages = vec![json!({ "role": "system", "content": system_prompt })];
    for message in transcript {
        let role = match message.role {
            ModelRole::System => "system",
            ModelRole::User | ModelRole::Subagent => "user",
            ModelRole::Assistant => "assistant",
            ModelRole::Tool => "tool",
        };
        let content = content_text(&message.content);
        let entry = if role == "tool" {
            json!({
                "role": "tool",
                "tool_call_id": tool_call_id_of(&message.content),
                "content": content,
            })
        } else {
            json!({ "role": role, "content": content })
        };
        messages.push(entry);
    }
    messages
}

/// Converts normalized tool definitions into a function tool schema.
#[must_use]
pub fn tools_from_definitions(definitions: &[ToolDefinition]) -> Vec<Value> {
    definitions
        .iter()
        .map(|definition| {
            json!({
                "type": "function",
                "function": {
                    "name": definition.name,
                    "description": definition.description,
                    "parameters": definition.input_schema,
                }
            })
        })
        .collect()
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
        .map(|call| ToolIntent::new(call.id, tool_name(&call.name), call.arguments))
        .collect();
    let reported = answer.usage.unwrap_or_default();
    let mut usage = ModelUsage::new();
    usage.input_tokens = reported.input_tokens.and_then(|tokens| usize::try_from(tokens).ok());
    usage.output_tokens = reported.output_tokens.and_then(|tokens| usize::try_from(tokens).ok());
    let mut response =
        ModelResponse::new().text(answer.content).tool_intents(intents).with_usage(usage);
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
    Some(AssistantTurn {
        content,
        reasoning,
        raw: message.clone(),
        tool_calls,
        finish_reason,
        usage: usage_of(value),
    })
}

/// Builds the failure detail for a non-success HTTP status.
#[must_use]
pub fn http_error_message(label: &str, status: u16, value: &Value) -> String {
    if let Some(message) = value.pointer("/error/message").and_then(Value::as_str) {
        format!("{label} HTTP {status}: {message}")
    } else {
        format!("{label} HTTP {status}: {value}")
    }
}

/// Classifies a non-success status after its body was read, reusing the
/// provider's own message when the body carries one.
#[must_use]
pub fn classify_status_error(
    label: &str,
    status: u16,
    headers: &HeaderMap,
    value: &Value,
) -> ProviderError {
    classify_http_error(
        status,
        parse_retry_after(headers),
        http_error_message(label, status, value),
    )
}

/// Classifies a non-success status into a retry decision and a typed provider
/// error. Structural classification: never string parsing.
#[must_use]
pub fn classify_http_error(
    status: u16,
    retry_after: Option<Duration>,
    detail: String,
) -> ProviderError {
    match status {
        429 => ProviderError::RateLimited { retry_after, detail },
        // Transient server/network classes; safe to retry only before any
        // response bytes were produced (the streaming path guards that itself).
        408 | 409 | 425 | 500 | 502 | 503 | 504 | 521 | 522 | 524 => {
            ProviderError::Transient { retry_after, detail }
        }
        // 401/403 are permanent credential/policy problems: never retried,
        // never auto-resumed.
        401 | 403 => ProviderError::BackendFailure(detail),
        _ if status >= 500 => ProviderError::Transient { retry_after, detail },
        _ => ProviderError::InvalidRequest(detail),
    }
}

/// Whether a provider error may be retried (rate limits and transient failures
/// only; never side-effecting retries).
#[must_use]
pub fn is_retryable(error: &ProviderError) -> bool {
    matches!(error, ProviderError::RateLimited { .. } | ProviderError::Transient { .. })
}

/// Server-provided retry hint, when the error carries one.
#[must_use]
pub fn retry_after_of(error: &ProviderError) -> Option<Duration> {
    match error {
        ProviderError::RateLimited { retry_after, .. }
        | ProviderError::Transient { retry_after, .. } => *retry_after,
        _ => None,
    }
}

/// Parses a `Retry-After` header value (delta-seconds form; HTTP dates are rare
/// and rejected rather than mis-parsed).
#[must_use]
pub fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get("retry-after")?.to_str().ok()?.trim();
    value.parse::<u64>().ok().map(Duration::from_secs)
}

/// Retry delay for `attempt` (0-based): the server hint wins when present
/// (capped), otherwise exponential backoff with bounded jitter.
#[must_use]
pub fn retry_delay(policy: &RetryPolicy, attempt: u32, retry_after: Option<Duration>) -> Duration {
    if let Some(hint) = retry_after {
        return hint.min(policy.max_delay);
    }
    let base = policy.base_delay.as_millis() as u64;
    let backoff = base.saturating_mul(1 << attempt.min(10));
    let capped = backoff.min(policy.max_delay.as_millis() as u64);
    // Bounded jitter (±20%) so bursts do not retry in lockstep.
    let jitter = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos() as u64)
        % (capped / 5 + 1);
    Duration::from_millis(capped + jitter)
}

/// Extracts usage from an object carrying the neutral
/// `input_tokens` / `output_tokens` / `total_tokens` fields.
///
/// Used by the OpenAI Responses and Anthropic Messages decoders; unknown fields
/// stay `None` and are never counted as zero.
#[must_use]
pub fn usage_from_tokens(value: &Value) -> Option<Usage> {
    let usage = value.get("input_tokens").and_then(Value::as_u64);
    let output = value.get("output_tokens").and_then(Value::as_u64);
    let total = value.get("total_tokens").and_then(Value::as_u64);
    if usage.is_none() && output.is_none() && total.is_none() {
        return None;
    }
    Some(Usage { input_tokens: usage, output_tokens: output, total_tokens: total })
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

/// Extracts `usage.prompt_tokens` / `completion_tokens` / `total_tokens`;
/// returns `None` when the response carries no usage object.
fn usage_of(value: &Value) -> Option<Usage> {
    let usage = value.get("usage")?;
    let input_tokens = usage.get("prompt_tokens").and_then(Value::as_u64);
    let output_tokens = usage.get("completion_tokens").and_then(Value::as_u64);
    let total_tokens = usage.get("total_tokens").and_then(Value::as_u64);
    if input_tokens.is_none() && output_tokens.is_none() && total_tokens.is_none() {
        return None;
    }
    Some(Usage { input_tokens, output_tokens, total_tokens })
}

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
    let arguments = match function.get("arguments")? {
        Value::String(text) => serde_json::from_str(text).unwrap_or_else(|_| json!({})),
        value => value.clone(),
    };
    Some(ToolCall { id, name, arguments })
}

/// Renders one message's content blocks as text.
///
/// Shared by every dialect's transcript conversion; unordered or unknown content
/// kinds stay out of the text view.
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
            _ => {} // future content kinds stay out of the text view
        }
    }
    parts.join("\n")
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
}

impl StreamAccumulator {
    pub(crate) fn new(label: &str) -> Self {
        Self {
            label: label.to_string(),
            content: String::new(),
            reasoning: String::new(),
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
        if let Some(finish_reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(finish_reason.to_string());
        }
        if let Some(usage) = usage_of(value) {
            self.usage = Some(usage);
        }
        let Some(delta) = choice.get("delta") else {
            return Ok(Vec::new());
        };
        let mut output = Vec::new();
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
        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in tool_calls {
                self.apply_tool_call_delta(call)?;
            }
        }
        Ok(output)
    }

    fn apply_tool_call_delta(&mut self, value: &Value) -> Result<(), ProviderError> {
        let index = value.get("index").and_then(Value::as_u64).ok_or_else(|| {
            ProviderError::BackendFailure(format!("{} stream tool call missing index", self.label))
        })? as usize;
        let call = self.tool_calls.entry(index).or_default();
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
            }
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<AssistantTurn, ProviderError> {
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
            let arguments = serde_json::from_str::<Value>(&call.arguments).unwrap_or(Value::Null);
            tool_calls.push(ToolCall { id, name, arguments });
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
        Ok(AssistantTurn {
            content: self.content,
            reasoning: self.reasoning,
            raw,
            tool_calls,
            finish_reason: self.finish_reason,
            usage: self.usage,
        })
    }
}

#[cfg(test)]
mod tests;
