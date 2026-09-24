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

use ee_acp_agent_server::ProviderError;
use ee_agent_orchestrator::{ModelMessage, ModelResponse, ModelRole, ToolDefinition};
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

/// Output bound sent when the provider profile does not override it.
///
/// Messages requires `max_tokens`; EE sends a conservative bound instead of
/// letting the provider pick an unbounded default. A profile extension
/// (`{"max_tokens": N}`) overrides it.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 8192;

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
    extensions: Option<&Value>,
) -> Value {
    let mut system_parts = Vec::new();
    if !system_prompt.is_empty() {
        system_parts.push(system_prompt.to_string());
    }
    let mut messages: Vec<Value> = Vec::new();
    let mut open_tool_results = false;
    for message in transcript {
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
                        "content": [{ "type": "text", "text": text }],
                    }));
                }
            }
            ModelRole::Assistant => {
                let text = content_text(&message.content);
                if !text.is_empty() {
                    messages.push(json!({
                        "role": "assistant",
                        "content": [{ "type": "text", "text": text }],
                    }));
                }
            }
        }
        open_tool_results = false;
    }
    let mut body = json!({
        "model": model,
        "max_tokens": DEFAULT_MAX_OUTPUT_TOKENS,
        "messages": messages,
        "stream": stream,
        "tool_choice": { "type": "auto" },
    });
    if !system_parts.is_empty() {
        body["system"] = Value::String(system_parts.join("\n\n"));
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(
            tools
                .iter()
                .map(|definition| {
                    json!({
                        "name": definition.name,
                        "description": definition.description,
                        "input_schema": definition.input_schema,
                    })
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
        tool_calls,
        finish_reason: Some(finish_reason(value.get("stop_reason").and_then(Value::as_str))),
        usage: usage_from_tokens(value.get("usage").unwrap_or(&Value::Null)),
    }))
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
    calls: BTreeMap<u64, PendingCall>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
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
                    self.usage =
                        Some(Usage { input_tokens, output_tokens: None, total_tokens: None });
                }
            }
            "content_block_start" => {
                let index = block_index(&value, &self.label)?;
                let block = value.get("content_block").unwrap_or(&Value::Null);
                // Text and thinking are accumulated from their deltas.
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let call = self.calls.entry(index).or_default();
                    call.id = block.get("id").and_then(Value::as_str).map(str::to_string);
                    call.name = block.get("name").and_then(Value::as_str).map(str::to_string);
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
                            self.reasoning.push_str(text);
                            sink(StreamDelta::Reasoning(text.to_string()))?;
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
        Ok(response_from_turn(AssistantTurn {
            content: self.text,
            reasoning: self.reasoning,
            raw: Value::Null,
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
        let profile = self.transport.profile();
        let body = request_body(
            &profile.model,
            &profile.system_prompt,
            transcript,
            tools,
            false,
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
        let profile = self.transport.profile();
        let body = request_body(
            &profile.model,
            &profile.system_prompt,
            transcript,
            tools,
            true,
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
            let mut response = EndpointRequest::new(transport, self.body).send().await?;
            if !is_event_stream(response.headers()) {
                let value = transport.read_json(&mut response, "streaming response").await?;
                let decoded = response_from_buffered(&label, &value)?;
                emit_finished_deltas(&decoded, sink)?;
                return Ok(decoded);
            }
            let mut decoder = StreamDecoder::new(&label);
            drive_sse(&label, &mut response, |event| decoder.apply(event, sink)).await?;
            decoder.finish()
        })
    }
}

#[cfg(test)]
mod tests;
