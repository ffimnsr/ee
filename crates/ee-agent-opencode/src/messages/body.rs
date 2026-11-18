//! Anthropic Messages request-body encoding.
//!
//! System content is hoisted into the top-level `system` field, tool results are
//! correlated into one user turn per group, signed thinking blocks replay only to
//! the model that produced them, and cache breakpoints are planned across the
//! system, tool, and message planes.

use ee_agent_orchestrator::{
    GenerationOptions, ModelContent, ModelMessage, ModelRole, ToolCallControl, ToolChoice,
    ToolDefinition,
};
use ee_chat_completions::{
    CacheCandidate, CacheCandidateRole, CacheCandidates, CacheDirective, cache_control,
    content_text, plan, tool_call_id_of, warn_dropped,
};
use serde_json::{Value, json};

use super::THINKING_BLOCKS;

/// Inputs for one Anthropic Messages request body.
#[derive(Debug, Clone, Copy)]
pub struct MessagesBody<'a> {
    /// Wire model id.
    pub model: &'a str,
    /// Route identity native state is sealed with; `None` replays by model only.
    pub provider: Option<&'a str>,
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
        provider: None,
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
                        "content": user_content_blocks(&message.content, &marker),
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
                    let mut blocks = replay_thinking_blocks(message, input.provider, input.model)
                        .unwrap_or_default();
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

/// Thinking blocks a transcript message may replay to `model`.
pub(super) fn replay_thinking_blocks(
    message: &ModelMessage,
    provider: Option<&str>,
    model: &str,
) -> Option<Vec<Value>> {
    let state = message
        .native
        .as_ref()
        .and_then(|native| ee_chat_completions::replayable_for(native, provider, model))?;
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

/// Builds Anthropic user content blocks, routing inline media and PDFs to
/// their native block shapes and everything else to text.
fn user_content_blocks(content: &[ModelContent], marker: &Value) -> Vec<Value> {
    let mut blocks = Vec::new();
    for block in content {
        match block {
            ModelContent::Text(text) => blocks.push(text_block(text, false, marker)),
            ModelContent::Image { data, mime_type, .. } => {
                blocks.push(media_block("image", data, mime_type));
            }
            ModelContent::EmbeddedResource { uri, mime_type, text, blob } => match (text, blob) {
                (Some(text), _) => {
                    let mime = mime_type.as_deref().unwrap_or("text/plain");
                    blocks.push(text_block(
                        &format!("[file: {uri} ({mime})]\n{text}"),
                        false,
                        marker,
                    ));
                }
                (None, Some(blob)) => match mime_type.as_deref() {
                    Some(mime) if mime.starts_with("image/") => {
                        blocks.push(media_block("image", blob, mime));
                    }
                    Some("application/pdf") => blocks.push(document_block(blob)),
                    mime => blocks.push(text_block(
                        &format!("[resource:{uri}:{}]", mime.unwrap_or("application/octet-stream")),
                        false,
                        marker,
                    )),
                },
                (None, None) => {
                    blocks.push(text_block(&format!("[resource:{uri}]"), false, marker))
                }
            },
            other => {
                let text = content_text(std::slice::from_ref(other));
                if !text.is_empty() {
                    blocks.push(text_block(&text, false, marker));
                }
            }
        }
    }
    if blocks.is_empty() {
        blocks.push(text_block(&content_text(content), false, marker));
    }
    blocks
}

fn media_block(kind: &str, data: &str, mime_type: &str) -> Value {
    json!({
        "type": kind,
        "source": { "type": "base64", "media_type": mime_type, "data": data },
    })
}

fn document_block(data: &str) -> Value {
    json!({
        "type": "document",
        "source": { "type": "base64", "media_type": "application/pdf", "data": data },
    })
}
