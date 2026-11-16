//! Anthropic Messages dialect codec for OpenCode (`/messages`).
//!
//! Routes whose `OpenCodeDialect::AnthropicMessages` models are served by this
//! codec (Zen Claude/Qwen and Go MiniMax/Qwen families) get their own request,
//! buffered-response, and streaming shapes:
//!
//! - system content is hoisted to the top-level `system` field, because Messages
//!   has no system role inside `messages`;
//! - the remaining transcript becomes user/assistant content blocks, and tool
//!   observations become correlated `tool_result` blocks grouped into one user
//!   turn, keeping tool-call identity;
//! - tool definitions become `{ name, description, input_schema }`;
//! - buffered `text` / `thinking` / `tool_use` blocks plus `stop_reason` and
//!   reported usage decode into the normalized [`ModelResponse`], and the SSE
//!   lifecycle (`message_start`, `content_block_*`, `message_delta`,
//!   `message_stop`, `error`) decodes in arrival order.
//!
//! The normalized transcript carries assistant text only, so assistant tool-call
//! arguments cannot be reconstructed; the `tool_result` block still carries the
//! original call id, and the model sees the observation in order. A request
//! encoded here is never replayed through another dialect.

use std::collections::BTreeMap;

use ee_agent_acp_server::ProviderError;
use ee_agent_orchestrator::{
    ModelContent, ModelMessage, ModelResponse, ModelRole, ToolDefinition, estimate_prompt_tokens,
};
use ee_chat_completions::{
    AssistantTurn, DeltaSink, EndpointRequest, EndpointTransport, SseControl, StreamDelta,
    ToolCall, Usage, content_text, drive_sse, emit_finished_deltas, is_event_stream,
    response_from_turn, tool_call_id_of, usage_from_tokens, with_buffered_retry, with_stream_retry,
};
use serde_json::{Value, json};

use crate::dialect::{codec_error, parse_arguments};

/// Dialect every route served by this module speaks.
pub const DIALECT: crate::routes::OpenCodeDialect =
    crate::routes::OpenCodeDialect::AnthropicMessages;

/// Output bound sent when the route carries no catalog limit.
///
/// Messages requires `max_tokens`; EE sends the shared fallback rather than
/// letting the provider pick an unbounded default. A catalog limit or an
/// explicit override replaces it.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = ee_agent_orchestrator::OUTPUT_LIMIT_FALLBACK;

/// Builds an Anthropic Messages request body.
///
/// The profile system prompt and every transcript system message are joined into
/// the top-level `system` field; `extensions` must be a JSON object when present
/// and is merged last.
#[must_use]
pub fn request_body(
    model: &str,
    system_prompt: &str,
    transcript: &[ModelMessage],
    tools: &[ToolDefinition],
    stream: bool,
    max_output_tokens: u64,
    extensions: Option<&Value>,
) -> Value {
    let mut system_parts = Vec::new();
    if !system_prompt.is_empty() {
        system_parts.push(system_prompt.to_string());
    }
    let mut messages: Vec<Value> = Vec::new();
    let mut open_tool_results = false;
    // Anthropic accepts at most four `cache_control` breakpoints per request.
    // System and the final tool schema consume two; the last pinned
    // instruction message consumes the third. Message text is never rewritten.
    let last_breakpoint = transcript.iter().rposition(ModelMessage::is_cache_breakpoint);
    for (index, message) in transcript.iter().enumerate() {
        let pinned = last_breakpoint == Some(index);
        match message.role {
            ModelRole::System => {
                let text = content_text(&message.content);
                if !text.is_empty() {
                    system_parts.push(text);
                }
            }
            ModelRole::Tool => {
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": tool_call_id_of(&message.content),
                    "content": content_text(&message.content),
                });
                if open_tool_results
                    && let Some(content) = messages
                        .last_mut()
                        .and_then(|message| message.get_mut("content"))
                        .and_then(Value::as_array_mut)
                {
                    content.push(block);
                } else {
                    messages.push(json!({ "role": "user", "content": [block] }));
                    open_tool_results = true;
                }
                continue;
            }
            ModelRole::User | ModelRole::Subagent => {
                let text = content_text(&message.content);
                if !text.is_empty() {
                    messages.push(json!({
                        "role": "user",
                        "content": [text_block(&text, pinned)],
                    }));
                }
            }
            ModelRole::Assistant => {
                let text = content_text(&message.content);
                // Anthropic accepts reasoning blocks only when they are signed
                // and followed by output; an unsigned or thinking-only tail is
                // dropped rather than rejected by the provider.
                let has_calls = message
                    .content
                    .iter()
                    .any(|block| matches!(block, ModelContent::ToolCall { .. }));
                if !text.is_empty() || has_calls {
                    let mut blocks = replay_thinking_blocks(message, model).unwrap_or_default();
                    if !text.is_empty() {
                        blocks.push(text_block(&text, pinned));
                    }
                    for block in &message.content {
                        if let ModelContent::ToolCall { tool_call_id, name, arguments } = block {
                            blocks.push(json!({
                                "type": "tool_use",
                                "id": tool_call_id,
                                "name": name,
                                "input": arguments,
                            }));
                        }
                    }
                    messages.push(json!({ "role": "assistant", "content": blocks }));
                }
            }
        }
        open_tool_results = false;
    }
    let mut body = json!({
        "model": model,
        "max_tokens": max_output_tokens,
        "messages": messages,
        "stream": stream,
        "tool_choice": { "type": "auto" },
    });
    if !system_parts.is_empty() {
        // The final system block closes the first cacheable prefix plane.
        let blocks = system_parts
            .iter()
            .enumerate()
            .map(|(index, text)| text_block(text, index + 1 == system_parts.len()))
            .collect::<Vec<_>>();
        body["system"] = Value::Array(blocks);
    }
    if !tools.is_empty() {
        let last = tools.len() - 1;
        body["tools"] = Value::Array(
            tools
                .iter()
                .enumerate()
                .map(|(index, definition)| {
                    let mut tool = json!({
                        "name": definition.name,
                        "description": definition.description,
                        "input_schema": definition.input_schema,
                    });
                    // Anthropic caches the tool block up to the final schema.
                    if index == last {
                        tool["cache_control"] = json!({ "type": "ephemeral" });
                    }
                    tool
                })
                .collect(),
        );
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

/// Decodes a buffered Messages payload into a normalized response.
///
/// # Errors
///
/// Returns a typed error when the payload is not a Messages object, reports an
/// error, or carries a `tool_use` block without identity.
pub fn response_from_buffered(label: &str, value: &Value) -> Result<ModelResponse, ProviderError> {
    if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
        return Err(codec_error(label, error_detail(error)));
    }
    if !is_messages_payload(value) {
        return Err(codec_error(label, "payload is not an Anthropic Messages response"));
    }
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    for block in content_blocks(value) {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                text.push_str(block.get("text").and_then(Value::as_str).unwrap_or_default())
            }
            Some("thinking") => reasoning
                .push_str(block.get("thinking").and_then(Value::as_str).unwrap_or_default()),
            Some("tool_use") => tool_calls.push(tool_use_of(label, block)?),
            _ => {}
        }
    }
    Ok(response_from_turn(AssistantTurn {
        content: text,
        reasoning,
        raw: Value::Array(content_blocks(value).to_vec()),
        native_reasoning: thinking_blocks_of(content_blocks(value)),
        tool_calls,
        finish_reason: Some(finish_reason(value.get("stop_reason").and_then(Value::as_str))),
        usage: usage_from_tokens(value.get("usage").unwrap_or(&Value::Null)),
    }))
}

/// Key holding the replayable Anthropic thinking blocks.
const THINKING_BLOCKS: &str = "thinking_blocks";

/// Collects signed thinking blocks for replay.
///
/// Signatures are opaque proof of generation: the block is kept byte-identical
/// and replayed only to the model that produced it (see [`seal_response`]).
fn thinking_blocks_of(blocks: &[Value]) -> Option<Value> {
    let mut kept = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("thinking") => {
                let thinking = block.get("thinking").and_then(Value::as_str).unwrap_or_default();
                let signature = block.get("signature").and_then(Value::as_str);
                let mut item = json!({ "type": "thinking", "thinking": thinking });
                if let Some(signature) = signature {
                    item["signature"] = Value::String(signature.to_string());
                }
                kept.push(item);
            }
            Some("redacted_thinking") => {
                if let Some(data) = block.get("data").and_then(Value::as_str) {
                    kept.push(json!({ "type": "redacted_thinking", "data": data }));
                }
            }
            _ => {}
        }
    }
    (!kept.is_empty()).then(|| json!({ THINKING_BLOCKS: kept }))
}

/// Thinking blocks a transcript message may replay to `model`.
fn replay_thinking_blocks(message: &ModelMessage, model: &str) -> Option<Vec<Value>> {
    let state = message
        .native
        .as_ref()
        .and_then(|native| ee_chat_completions::replayable(native, model))?;
    let stored = state.get(THINKING_BLOCKS)?.as_array()?;
    let mut blocks = Vec::new();
    for block in stored {
        match block.get("type").and_then(Value::as_str) {
            Some("thinking") => {
                let signature = block.get("signature").and_then(Value::as_str).unwrap_or_default();
                let thinking = block.get("thinking").and_then(Value::as_str).unwrap_or_default();
                if !signature.is_empty() {
                    blocks.push(json!({
                        "type": "thinking",
                        "thinking": thinking,
                        "signature": signature,
                    }));
                }
            }
            Some("redacted_thinking") => {
                if let Some(data) = block.get("data").and_then(Value::as_str) {
                    blocks.push(json!({ "type": "redacted_thinking", "data": data }));
                }
            }
            _ => {}
        }
    }
    (!blocks.is_empty()).then_some(blocks)
}

/// Whether one message should carry an Anthropic cache breakpoint block.
fn text_block(text: &str, cache: bool) -> Value {
    let mut block = json!({ "type": "text", "text": text });
    if cache {
        block["cache_control"] = json!({ "type": "ephemeral" });
    }
    block
}

fn is_messages_payload(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("message")
        || value.get("content").is_some_and(Value::is_array)
        || value.get("stop_reason").is_some()
}

fn content_blocks(value: &Value) -> &[Value] {
    value.get("content").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

/// Decodes one `tool_use` block, failing closed on missing identity.
fn tool_use_of(label: &str, block: &Value) -> Result<ToolCall, ProviderError> {
    let id = block
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| codec_error(label, "tool_use block is missing its id"))?;
    let name = block
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| codec_error(label, "tool_use block is missing its name"))?;
    let arguments = match block.get("input") {
        Some(Value::String(raw)) => parse_arguments(raw),
        Some(object @ Value::Object(_)) => object.clone(),
        _ => Value::Null,
    };
    Ok(ToolCall { id: id.to_string(), name: name.to_string(), arguments })
}

/// Canonical stop signal for a Messages `stop_reason`.
fn finish_reason(stop_reason: Option<&str>) -> String {
    match stop_reason {
        None | Some("end_turn") | Some("stop_sequence") | Some("refusal") => String::from("stop"),
        Some("tool_use") => String::from("tool_calls"),
        Some("max_tokens") => String::from("length"),
        // `pause_turn` and future reasons stay non-completing: the orchestrator
        // must not treat an unknown stop as a finished turn.
        Some(other) => other.to_string(),
    }
}

/// Renders a nested error object as one diagnostic line.
fn error_detail(error: &Value) -> String {
    match error.get("message").and_then(Value::as_str) {
        Some(message) => message.to_string(),
        None => error.to_string(),
    }
}

/// Incremental decoder for Messages SSE events.
#[derive(Debug)]
pub struct StreamDecoder {
    label: String,
    text: String,
    reasoning: String,
    thinking: BTreeMap<u64, PendingThinking>,
    calls: BTreeMap<u64, PendingCall>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
}

/// One streamed thinking block: its text, its signature, or its redacted payload.
#[derive(Debug, Default)]
struct PendingThinking {
    thinking: String,
    signature: Option<String>,
    redacted: Option<String>,
}

#[derive(Debug, Default)]
struct PendingCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl StreamDecoder {
    /// Builds a decoder whose errors name the routed endpoint.
    #[must_use]
    pub fn new(label: &str) -> Self {
        Self {
            label: label.to_string(),
            text: String::new(),
            reasoning: String::new(),
            thinking: BTreeMap::new(),
            calls: BTreeMap::new(),
            finish_reason: None,
            usage: None,
        }
    }

    /// Applies one SSE `data` payload, forwarding displayable deltas to `sink`.
    ///
    /// # Errors
    ///
    /// Returns a typed error for lifecycle events without an index, terminal
    /// errors, incomplete tool calls, or a closed sink.
    pub fn apply(&mut self, event: &str, sink: DeltaSink<'_>) -> Result<SseControl, ProviderError> {
        let value: Value = serde_json::from_str(event).map_err(|error| {
            codec_error(&self.label, format!("stream event was not JSON: {error}"))
        })?;
        match value.get("type").and_then(Value::as_str).unwrap_or_default() {
            "message_start" => {
                // Only the input side is known here; output usage arrives on
                // `message_delta`, so a placeholder is never recorded.
                let usage = value.pointer("/message/usage").unwrap_or(&Value::Null);
                let input_tokens = usage.get("input_tokens").and_then(Value::as_u64);
                if input_tokens.is_some() {
                    self.usage = Some(Usage {
                        input_tokens,
                        output_tokens: None,
                        total_tokens: None,
                        ..Default::default()
                    });
                }
            }
            "content_block_start" => {
                let index = block_index(&value, &self.label)?;
                let block = value.get("content_block").unwrap_or(&Value::Null);
                // Text and thinking are accumulated from their deltas; a redacted
                // block arrives whole and is never streamed in pieces.
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        let call = self.calls.entry(index).or_default();
                        call.id = block.get("id").and_then(Value::as_str).map(str::to_string);
                        call.name = block.get("name").and_then(Value::as_str).map(str::to_string);
                    }
                    Some("thinking") => {
                        let entry = self.thinking.entry(index).or_default();
                        if let Some(thinking) = block.get("thinking").and_then(Value::as_str)
                            && !thinking.is_empty()
                        {
                            entry.thinking.push_str(thinking);
                        }
                    }
                    Some("redacted_thinking") => {
                        self.thinking.entry(index).or_default().redacted = block
                            .get("data")
                            .and_then(Value::as_str)
                            .filter(|data| !data.is_empty())
                            .map(str::to_string);
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                block_index(&value, &self.label)?;
                let delta = value.get("delta").unwrap_or(&Value::Null);
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        let text = delta.get("text").and_then(Value::as_str).unwrap_or_default();
                        if !text.is_empty() {
                            self.text.push_str(text);
                            sink(StreamDelta::Text(text.to_string()))?;
                        }
                    }
                    Some("thinking_delta") => {
                        let text =
                            delta.get("thinking").and_then(Value::as_str).unwrap_or_default();
                        if !text.is_empty() {
                            let index = block_index(&value, &self.label)?;
                            self.thinking.entry(index).or_default().thinking.push_str(text);
                            self.reasoning.push_str(text);
                            sink(StreamDelta::Reasoning(text.to_string()))?;
                        }
                    }
                    Some("signature_delta") => {
                        let index = block_index(&value, &self.label)?;
                        if let Some(signature) = delta.get("signature").and_then(Value::as_str) {
                            self.thinking.entry(index).or_default().signature =
                                Some(signature.to_string());
                        }
                    }
                    Some("input_json_delta") => {
                        let index = block_index(&value, &self.label)?;
                        let partial =
                            delta.get("partial_json").and_then(Value::as_str).unwrap_or_default();
                        self.calls.entry(index).or_default().arguments.push_str(partial);
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = block_index(&value, &self.label)?;
                if let Some(call) = self.calls.get(&index) {
                    require_tool_identity(&self.label, call)?;
                }
            }
            "message_delta" => {
                if let Some(stop_reason) =
                    value.pointer("/delta/stop_reason").and_then(Value::as_str)
                {
                    self.finish_reason = Some(finish_reason(Some(stop_reason)));
                }
                if let Some(output_tokens) =
                    value.pointer("/usage/output_tokens").and_then(Value::as_u64)
                {
                    let usage = self.usage.get_or_insert(Usage::default());
                    usage.output_tokens = Some(output_tokens);
                }
            }
            "message_stop" => return Ok(SseControl::Stop),
            "error" => return Err(codec_error(&self.label, error_detail(&value))),
            _ => {}
        }
        Ok(SseControl::Continue)
    }

    /// Finalizes the streamed turn.
    ///
    /// # Errors
    ///
    /// Returns a typed error when a streamed `tool_use` block is missing its
    /// identity, so no partial call can reach tool execution.
    pub fn finish(self) -> Result<ModelResponse, ProviderError> {
        let mut tool_calls = Vec::with_capacity(self.calls.len());
        for (_index, call) in self.calls {
            require_tool_identity(&self.label, &call)?;
            let id = call.id.unwrap_or_default();
            let name = call.name.unwrap_or_default();
            tool_calls.push(ToolCall { id, name, arguments: parse_arguments(&call.arguments) });
        }
        let mut thinking_blocks = Vec::new();
        for (_index, block) in self.thinking {
            if let Some(data) = block.redacted {
                thinking_blocks.push(json!({ "type": "redacted_thinking", "data": data }));
                continue;
            }
            let mut item = json!({ "type": "thinking", "thinking": block.thinking });
            if let Some(signature) = block.signature {
                item["signature"] = Value::String(signature);
            }
            thinking_blocks.push(item);
        }
        let native_reasoning =
            (!thinking_blocks.is_empty()).then(|| json!({ THINKING_BLOCKS: thinking_blocks }));
        Ok(response_from_turn(AssistantTurn {
            content: self.text,
            reasoning: self.reasoning,
            raw: Value::Null,
            native_reasoning,
            tool_calls,
            finish_reason: self.finish_reason,
            usage: self.usage,
        }))
    }
}

fn require_tool_identity(label: &str, call: &PendingCall) -> Result<(), ProviderError> {
    if call.id.as_deref().is_none_or(str::is_empty) {
        return Err(codec_error(label, "streamed tool_use block is missing its id"));
    }
    if call.name.as_deref().is_none_or(str::is_empty) {
        return Err(codec_error(label, "streamed tool_use block is missing its name"));
    }
    Ok(())
}

fn block_index(value: &Value, label: &str) -> Result<u64, ProviderError> {
    value
        .get("index")
        .and_then(Value::as_u64)
        .ok_or_else(|| codec_error(label, "stream event is missing its content block index"))
}

/// One streaming Messages round trip over an authorized endpoint.
pub struct MessagesCodec {
    transport: EndpointTransport,
}

impl MessagesCodec {
    /// Wraps one authorized transport.
    #[must_use]
    pub fn new(transport: EndpointTransport) -> Self {
        Self { transport }
    }

    /// Returns the transport this codec sends through.
    #[must_use]
    pub fn transport(&self) -> &EndpointTransport {
        &self.transport
    }

    /// Performs one buffered round trip under the profile retry policy.
    ///
    /// # Errors
    ///
    /// Returns a typed error for transport, status, or decode failures.
    pub async fn complete(
        &self,
        transcript: &[ModelMessage],
        tools: &[ToolDefinition],
    ) -> Result<ModelResponse, ProviderError> {
        self.complete_with_prompt(transcript, tools, None).await
    }

    /// Performs one buffered round trip using the provider-reported prompt size
    /// when the caller has one.
    pub(crate) async fn complete_with_prompt(
        &self,
        transcript: &[ModelMessage],
        tools: &[ToolDefinition],
        prompt_tokens: Option<u64>,
    ) -> Result<ModelResponse, ProviderError> {
        let profile = self.transport.profile();
        let prompt = prompt_tokens
            .unwrap_or_else(|| estimate_prompt_tokens(transcript, tools, &profile.system_prompt));
        let max_output_tokens =
            profile.fitted_output_limit(prompt).unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
        let body = request_body(
            &profile.model,
            &profile.system_prompt,
            transcript,
            tools,
            false,
            max_output_tokens,
            profile.extensions.as_ref(),
        );
        let transport = &self.transport;
        let label = profile.label.as_str();
        with_buffered_retry(&profile.retry, || async {
            let mut response = EndpointRequest::new(transport, &body).send().await?;
            let value = transport.read_json(&mut response, "response").await?;
            response_from_buffered(label, &value)
        })
        .await
        .map(|response| ee_chat_completions::seal_response(response, &profile.model))
    }

    /// Performs one streaming round trip under the profile retry policy.
    ///
    /// A retry happens only when no delta reached `sink` yet. Tool input
    /// accumulates privately, so nothing a retry could duplicate ever reached
    /// the caller, and no tool intent exists until the stream finishes.
    ///
    /// # Errors
    ///
    /// Returns a typed error for transport, status, decode, or sink failures.
    pub async fn complete_streaming(
        &self,
        transcript: &[ModelMessage],
        tools: &[ToolDefinition],
        sink: DeltaSink<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        self.complete_streaming_with_prompt(transcript, tools, sink, None).await
    }

    /// Performs one streaming round trip using the provider-reported prompt size
    /// when the caller has one.
    pub(crate) async fn complete_streaming_with_prompt(
        &self,
        transcript: &[ModelMessage],
        tools: &[ToolDefinition],
        sink: DeltaSink<'_>,
        prompt_tokens: Option<u64>,
    ) -> Result<ModelResponse, ProviderError> {
        let profile = self.transport.profile();
        let prompt = prompt_tokens
            .unwrap_or_else(|| estimate_prompt_tokens(transcript, tools, &profile.system_prompt));
        let max_output_tokens =
            profile.fitted_output_limit(prompt).unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
        let body = request_body(
            &profile.model,
            &profile.system_prompt,
            transcript,
            tools,
            true,
            max_output_tokens,
            profile.extensions.as_ref(),
        );
        let mut attempt = MessagesStreamAttempt { transport: &self.transport, body: &body };
        with_stream_retry(&self.transport.profile().retry, sink, &mut attempt).await
    }
}

/// One real streaming attempt for [`MessagesCodec`].
struct MessagesStreamAttempt<'a> {
    transport: &'a EndpointTransport,
    body: &'a Value,
}

impl ee_chat_completions::StreamAttempt<ModelResponse> for MessagesStreamAttempt<'_> {
    fn run<'a>(
        &'a mut self,
        sink: ee_chat_completions::DeltaSink<'a>,
    ) -> ee_chat_completions::StreamFuture<'a, ModelResponse> {
        Box::pin(async move {
            let transport = self.transport;
            let label = transport.profile().label.clone();
            let model = transport.profile().model.clone();
            let mut response = EndpointRequest::new(transport, self.body).send().await?;
            if !is_event_stream(response.headers()) {
                let value = transport.read_json(&mut response, "streaming response").await?;
                let decoded = response_from_buffered(&label, &value)?;
                emit_finished_deltas(&decoded, sink)?;
                return Ok(ee_chat_completions::seal_response(decoded, &model));
            }
            let mut decoder = StreamDecoder::new(&label);
            drive_sse(&label, &mut response, |event| decoder.apply(event, sink)).await?;
            decoder.finish().map(|response| ee_chat_completions::seal_response(response, &model))
        })
    }
}

#[cfg(test)]
mod tests;
