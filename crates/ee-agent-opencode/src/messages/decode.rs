//! Anthropic Messages buffered and streaming response decoding.
//!
//! Buffered `text` / `thinking` / `tool_use` blocks plus `stop_reason` and
//! reported usage decode into the normalized [`ModelResponse`], and the SSE
//! lifecycle (`message_start`, `content_block_*`, `message_delta`,
//! `message_stop`, `error`) decodes in arrival order.

use std::collections::BTreeMap;

use ee_agent_acp_server::ProviderError;
use ee_agent_orchestrator::ModelResponse;
use ee_chat_completions::{
    AssistantTurn, DeltaSink, SseControl, StreamDelta, ToolCall, Usage, response_from_turn,
    usage_from_tokens,
};
use serde_json::{Value, json};

use crate::dialect::{codec_error, parse_arguments};

use super::THINKING_BLOCKS;

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
        response_id: None,
    }))
}

/// Collects signed thinking blocks for replay.
///
/// Signatures are opaque proof of generation: the block is kept byte-identical
/// and replayed only to the model that produced it.
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
    Ok(ToolCall::new(id.to_string(), name.to_string(), arguments))
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
                let cached = usage.get("cache_read_input_tokens").and_then(Value::as_u64);
                let creation = usage.get("cache_creation_input_tokens").and_then(Value::as_u64);
                if input_tokens.is_some() || cached.is_some() || creation.is_some() {
                    let context_tokens = input_tokens.map(|input| {
                        input
                            .saturating_add(cached.unwrap_or(0))
                            .saturating_add(creation.unwrap_or(0))
                    });
                    self.usage = Some(Usage {
                        input_tokens,
                        output_tokens: None,
                        total_tokens: None,
                        cached_input_tokens: cached,
                        cache_creation_input_tokens: creation,
                        context_tokens,
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
                        if let (Some(id), Some(name)) = (call.id.clone(), call.name.clone()) {
                            sink(StreamDelta::ToolInputStart { id, name })?;
                        }
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
                        let call = self.calls.entry(index).or_default();
                        call.arguments.push_str(partial);
                        if !partial.is_empty()
                            && let Some(id) = call.id.clone()
                        {
                            sink(StreamDelta::ToolInputDelta {
                                id,
                                text: partial.to_string(),
                                parsed: ee_chat_completions::partial_json::parse_partial(
                                    &call.arguments,
                                ),
                            })?;
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = block_index(&value, &self.label)?;
                if let Some(call) = self.calls.get(&index) {
                    require_tool_identity(&self.label, call)?;
                    if let Some(id) = call.id.clone() {
                        sink(StreamDelta::ToolInputEnd { id })?;
                    }
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
                if let Some(reasoning) = value
                    .pointer("/usage/output_tokens_details/thinking_tokens")
                    .and_then(Value::as_u64)
                {
                    let usage = self.usage.get_or_insert(Usage::default());
                    usage.reasoning_tokens = Some(reasoning);
                }
                if let Some(usage) = self.usage
                    && usage.output_tokens.is_some()
                {
                    sink(StreamDelta::Usage(usage))?;
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
            tool_calls.push(ToolCall::new(id, name, parse_arguments(&call.arguments)));
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
            response_id: None,
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
