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
    GenerationOptions, ModelContent, ModelMessage, ModelResponse, ModelRole, ToolCallControl,
    ToolChoice, ToolDefinition, estimate_prompt_tokens,
};
use ee_chat_completions::{
    AssistantTurn, CacheCandidate, CacheCandidateRole, CacheCandidates, CacheDirective, DeltaSink,
    EndpointRequest, EndpointTransport, SseControl, StreamDelta, ToolCall, Usage, cache_control,
    content_text, drive_sse_with_chunk_timeout, emit_finished_deltas, is_event_stream, plan,
    response_from_turn, tool_call_id_of, usage_from_tokens, warn_dropped, with_buffered_retry,
    with_stream_retry,
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

/// Inputs for one Anthropic Messages request body.
#[derive(Debug, Clone, Copy)]
pub struct MessagesBody<'a> {
    /// Wire model id.
    pub model: &'a str,
    /// Profile system prompt, hoisted into `system`.
    pub system_prompt: &'a str,
    /// Normalized conversation.
    pub transcript: &'a [ModelMessage],
    /// Advertised tools.
    pub tools: &'a [ToolDefinition],
    /// Whether the response is streamed.
    pub stream: bool,
    /// Output-token bound.
    pub max_output_tokens: u64,
    /// Provider body fields merged last.
    pub extensions: Option<&'a Value>,
    /// Tool-call control.
    pub tool_control: &'a ToolCallControl,
    /// Prompt-cache policy and route capabilities.
    pub cache: &'a CacheDirective,
    /// Prompt size measured by the provider on the previous call, when known.
    pub prompt_tokens: Option<u64>,
    /// Sampling knobs to write; `None` leaves every field absent.
    pub generation: Option<&'a GenerationOptions>,
}

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
    let auto = ToolCallControl::auto();
    let cache = CacheDirective::disabled();
    messages_body(&MessagesBody {
        model,
        system_prompt,
        transcript,
        tools,
        stream,
        max_output_tokens,
        extensions,
        tool_control: &auto,
        cache: &cache,
        prompt_tokens: None,
        generation: None,
    })
}

/// Builds a Messages body with tool-call control and prompt-cache placement.
///
/// Breakpoints are planned across the system, tool, and message planes under the
/// shared four-breakpoint budget, and Anthropic is the one dialect where a
/// `tool_result` block may carry one.
#[must_use]
pub fn messages_body(input: &MessagesBody<'_>) -> Value {
    let markers = input.cache.places_markers();
    let marker = cache_control(input.cache);
    let mut system_parts = Vec::new();
    if !input.system_prompt.is_empty() {
        system_parts.push(input.system_prompt.to_string());
    }
    let mut messages: Vec<Value> = Vec::new();
    let mut candidates: Vec<CacheCandidate> = Vec::new();
    let mut manual: Vec<usize> = Vec::new();
    let mut manual_system: Vec<usize> = Vec::new();
    let mut open_tool_results = false;
    for message in input.transcript {
        match message.role {
            ModelRole::System => {
                let text = content_text(&message.content);
                if !text.is_empty() {
                    system_parts.push(text);
                    if markers && message.is_cache_breakpoint() {
                        manual_system.push(system_parts.len() - 1);
                    }
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
                    candidates.push(CacheCandidate {
                        index: messages.len(),
                        role: CacheCandidateRole::ToolResult,
                    });
                    messages.push(json!({ "role": "user", "content": [block] }));
                    open_tool_results = true;
                }
                continue;
            }
            ModelRole::User | ModelRole::Subagent => {
                let text = content_text(&message.content);
                if !text.is_empty() {
                    if markers && message.is_cache_breakpoint() {
                        manual.push(messages.len());
                    }
                    candidates.push(CacheCandidate {
                        index: messages.len(),
                        role: CacheCandidateRole::User,
                    });
                    messages.push(json!({
                        "role": "user",
                        "content": [text_block(&text, false, &marker)],
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
                    let mut blocks =
                        replay_thinking_blocks(message, input.model).unwrap_or_default();
                    if !text.is_empty() {
                        blocks.push(text_block(&text, false, &marker));
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
                    if markers && message.is_cache_breakpoint() {
                        manual.push(messages.len());
                    }
                    candidates.push(CacheCandidate {
                        index: messages.len(),
                        role: CacheCandidateRole::Assistant,
                    });
                    messages.push(json!({ "role": "assistant", "content": blocks }));
                }
            }
        }
        open_tool_results = false;
    }

    let inputs = CacheCandidates {
        tools: input.tools.len(),
        system_parts: system_parts.len(),
        messages: &candidates,
        manual: &manual,
        manual_system: &manual_system,
        tool_results: true,
        prompt_tokens: input.prompt_tokens,
    };
    let cache_plan = plan(input.cache, &inputs);
    warn_dropped(&cache_plan, "Anthropic Messages", input.cache.support.max_breakpoints);
    // Message markers are applied before the body is built: `json!` copies the
    // value it interpolates, so a later mutation of the Vec would be lost.
    for &index in &cache_plan.messages {
        if let Some(message) = messages.get_mut(index) {
            mark_last_block(message, &marker);
        }
    }

    let mut body = json!({
        "model": input.model,
        "max_tokens": input.max_output_tokens,
        "messages": messages,
        "stream": input.stream,
    });
    if !system_parts.is_empty() {
        body["system"] = Value::Array(
            system_parts
                .iter()
                .enumerate()
                .map(|(index, text)| text_block(text, cache_plan.system.contains(&index), &marker))
                .collect(),
        );
    }
    if !input.tools.is_empty() {
        body["tools"] = Value::Array(
            input
                .tools
                .iter()
                .enumerate()
                .map(|(index, definition)| {
                    let mut tool = json!({
                        "name": definition.name,
                        "description": definition.description,
                        "input_schema": definition.input_schema,
                    });
                    // Anthropic caches the tool block up to the marked schema.
                    if cache_plan.tools == Some(index) {
                        tool["cache_control"] = marker.clone();
                    }
                    tool
                })
                .collect(),
        );
        body["tool_choice"] = messages_tool_choice(input.tool_control);
    }
    if let Some(generation) = input.generation {
        ee_chat_completions::apply_generation_messages(&mut body, generation);
    }
    if let Some(extensions) = input.extensions {
        // Deep merge so a partial provider override keeps sibling keys.
        ee_chat_completions::deep_merge(&mut body, extensions);
    }
    clamp_thinking_budget(&mut body, input.max_output_tokens);
    // Sampling knobs may have lowered the output bound; keep the thinking budget
    // below the final value.
    if let Some(max_tokens) = input.generation.and_then(|generation| generation.max_output_tokens)
        && max_tokens < input.max_output_tokens
    {
        clamp_thinking_budget(&mut body, max_tokens);
    }
    body
}

/// Keeps an enabled thinking budget strictly below the output bound.
///
/// Anthropic rejects a budget that is not below `max_tokens`, and a budget under
/// 1,024 tokens is below its documented minimum. The catalog-derived budget is
/// already bounded for the model's full output limit, so this only bites when a
/// large prompt squeezes the fitted `max_tokens`; the field is dropped instead of
/// sending a request the provider would reject.
fn clamp_thinking_budget(body: &mut Value, max_tokens: u64) {
    let Some(thinking) = body.get_mut("thinking") else {
        return;
    };
    if thinking.get("type").and_then(Value::as_str) != Some("enabled") {
        return;
    }
    let Some(budget) = thinking.get("budget_tokens").and_then(Value::as_u64) else {
        return;
    };
    let ceiling = max_tokens.saturating_sub(1);
    if budget <= ceiling {
        return;
    }
    if ceiling < 1_024 {
        body.as_object_mut().map(|object| object.remove("thinking"));
        return;
    }
    thinking["budget_tokens"] = Value::from(ceiling);
}

/// Lowered Anthropic tool choice, including the parallel-call switch.
fn messages_tool_choice(control: &ToolCallControl) -> Value {
    let mut choice = match &control.choice {
        ToolChoice::Auto => json!({ "type": "auto" }),
        ToolChoice::None => json!({ "type": "none" }),
        ToolChoice::Required => json!({ "type": "any" }),
        ToolChoice::Tool { name } => json!({ "type": "tool", "name": name }),
    };
    if let Some(parallel) = control.parallel
        && control.choice != ToolChoice::None
    {
        choice["disable_parallel_tool_use"] = Value::from(!parallel);
    }
    choice
}

/// Adds the marker to the last block that can carry one.
fn mark_last_block(message: &mut Value, marker: &Value) {
    let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    if let Some(block) = blocks.iter_mut().rev().find(|block| {
        matches!(
            block.get("type").and_then(Value::as_str),
            Some("text" | "tool_result" | "tool_use")
        )
    }) {
        block["cache_control"] = marker.clone();
    }
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
        response_id: None,
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
fn text_block(text: &str, cache: bool, marker: &Value) -> Value {
    let mut block = json!({ "type": "text", "text": text });
    if cache {
        block["cache_control"] = marker.clone();
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
        let control = ToolCallControl::auto();
        self.complete_controlled(transcript, tools, &control, prompt_tokens, None, None).await
    }

    /// Performs one buffered round trip with tool-call control.
    pub(crate) async fn complete_controlled(
        &self,
        transcript: &[ModelMessage],
        tools: &[ToolDefinition],
        tool_control: &ToolCallControl,
        prompt_tokens: Option<u64>,
        generation: Option<&GenerationOptions>,
        session: Option<&ee_agent_orchestrator::SessionContext>,
    ) -> Result<ModelResponse, ProviderError> {
        let transport = self.transport.with_session(session)?;
        let profile = transport.profile();
        let transcript = crate::dialect::guarded_transcript(profile, transcript);
        let prompt = prompt_tokens.unwrap_or_else(|| {
            estimate_prompt_tokens(transcript.as_ref(), tools, &profile.system_prompt)
        });
        let max_output_tokens = profile
            .fitted_output_limit_with(
                prompt,
                generation.and_then(|generation| generation.max_output_tokens),
            )
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
        let body = messages_body(&MessagesBody {
            model: &profile.model,
            system_prompt: &profile.system_prompt,
            transcript: transcript.as_ref(),
            tools,
            stream: false,
            max_output_tokens,
            extensions: profile.extensions.as_ref(),
            tool_control,
            cache: &profile.cache,
            prompt_tokens,
            generation,
        });
        let transport = &transport;
        let label = profile.label.as_str();
        with_buffered_retry(&profile.retry, || async {
            let mut response = EndpointRequest::new(transport, &body).send().await?;
            let value = transport.read_json(&mut response, "response").await?;
            response_from_buffered(label, &value)
        })
        .await
        .map(|response| crate::dialect::finalize(response, profile))
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
        let control = ToolCallControl::auto();
        self.complete_streaming_controlled(
            transcript,
            tools,
            &control,
            sink,
            prompt_tokens,
            ee_chat_completions::CallOptions::default(),
        )
        .await
    }

    /// Performs one streaming round trip with tool-call control.
    pub(crate) async fn complete_streaming_controlled(
        &self,
        transcript: &[ModelMessage],
        tools: &[ToolDefinition],
        tool_control: &ToolCallControl,
        sink: DeltaSink<'_>,
        prompt_tokens: Option<u64>,
        options: ee_chat_completions::CallOptions<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        let transport = self.transport.with_session(options.session)?;
        let profile = transport.profile();
        let transcript = crate::dialect::guarded_transcript(profile, transcript);
        let prompt = prompt_tokens.unwrap_or_else(|| {
            estimate_prompt_tokens(transcript.as_ref(), tools, &profile.system_prompt)
        });
        let max_output_tokens = profile
            .fitted_output_limit_with(
                prompt,
                options.generation.and_then(|generation| generation.max_output_tokens),
            )
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
        let body = messages_body(&MessagesBody {
            model: &profile.model,
            system_prompt: &profile.system_prompt,
            transcript: transcript.as_ref(),
            tools,
            stream: true,
            max_output_tokens,
            extensions: profile.extensions.as_ref(),
            tool_control,
            cache: &profile.cache,
            prompt_tokens,
            generation: options.generation,
        });
        let mut attempt = MessagesStreamAttempt { transport: &transport, body: &body };
        with_stream_retry(&transport.profile().retry, sink, &mut attempt)
            .await
            .map(|response| crate::dialect::finalize(response, profile))
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
            let raw = transport.profile().raw_stream_events;
            drive_sse_with_chunk_timeout(
                &label,
                &mut response,
                Some(transport.profile().timeouts.chunk),
                |event| {
                    if raw {
                        sink(StreamDelta::Raw { frame: event.to_string() })?;
                    }
                    decoder.apply(event, sink)
                },
            )
            .await?;
            decoder.finish().map(|response| ee_chat_completions::seal_response(response, &model))
        })
    }
}

#[cfg(test)]
mod tests;
