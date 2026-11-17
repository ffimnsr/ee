//! Chat Completions request assembly: bodies, transcript conversion, tool schemas.
//!
//! Everything here turns a normalized orchestrator request into the OpenAI
//! Chat Completions wire shape. It is separate from [`crate::wire`] — which owns
//! decoding, SSE framing, and normalized-response mapping — so each side stays
//! reviewable on its own.
//!
//! Prompt-cache planning and tool-schema lowering happen in the same pass, so
//! the four-breakpoint budget is shared across the system, tool, and message
//! planes instead of being applied independently.

use ee_agent_orchestrator::{
    GenerationOptions, ModelMessage, ModelRole, ToolCallControl, ToolChoice, ToolDefinition,
    ToolFormat,
};
use serde_json::{Value, json};

use crate::cache_policy::{
    CacheCandidate, CacheCandidateRole, CacheCandidates, CacheDirective, cache_control, plan,
    warn_dropped,
};
use crate::generation::apply_generation_chat;
use crate::merge::deep_merge;
use crate::profile::MaxTokensField;
use crate::reasoning_replay::{replay_reasoning, replay_tool_calls};
use crate::wire::{content_has_media, content_parts, content_text, tool_call_id_of};

/// Optional fields for one Chat Completions request body.
#[derive(Debug, Clone, Copy)]
pub struct RequestBodyOptions<'a> {
    /// Whether the response is streamed.
    pub stream: bool,
    /// Whether a streamed response requests `stream_options.include_usage`.
    pub stream_usage: bool,
    /// Output-token bound, written to [`Self::max_tokens_field`].
    pub max_tokens: Option<u64>,
    /// Body field carrying [`Self::max_tokens`].
    pub max_tokens_field: MaxTokensField,
    /// Tool-call control; `None` keeps the provider default (`auto`).
    pub tool_control: Option<&'a ToolCallControl>,
    /// Sampling knobs to write; `None` leaves every field absent.
    pub generation: Option<&'a GenerationOptions>,
    /// Provider body fields merged last, e.g. reasoning shaping.
    pub extensions: Option<&'a Value>,
}

impl Default for RequestBodyOptions<'_> {
    fn default() -> Self {
        Self {
            stream: false,
            stream_usage: false,
            max_tokens: None,
            max_tokens_field: MaxTokensField::MaxTokens,
            tool_control: None,
            generation: None,
            extensions: None,
        }
    }
}

impl<'a> RequestBodyOptions<'a> {
    /// Options for a buffered request with no extra fields.
    #[must_use]
    pub fn buffered() -> Self {
        Self::default()
    }

    /// Options for a streamed request with no extra fields.
    #[must_use]
    pub fn streaming() -> Self {
        Self { stream: true, ..Self::default() }
    }
}

/// Builds a Chat Completions request body.
///
/// `options` controls streaming, usage reporting, the output bound, and
/// provider body fields. Extension fields are merged last so a provider can
/// shape the request without editing shared code.
#[must_use]
pub fn request_body(
    model: &str,
    messages: &[Value],
    tools: &[Value],
    options: RequestBodyOptions<'_>,
) -> Value {
    let RequestBodyOptions {
        stream,
        stream_usage,
        max_tokens,
        max_tokens_field,
        tool_control,
        generation,
        extensions,
    } = options;
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": stream,
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools.to_vec());
        // OpenAI rejects `tool_choice` and `parallel_tool_calls` when tools are
        // absent, so both fields ride with the tool list.
        let control = tool_control.cloned().unwrap_or_default();
        body["tool_choice"] = tool_choice_value(&control.choice);
        if let Some(parallel) = control.parallel {
            body["parallel_tool_calls"] = Value::from(parallel);
        }
    }
    if stream && stream_usage {
        body["stream_options"] = json!({ "include_usage": true });
    }
    if let Some(max_tokens) = max_tokens {
        body[max_tokens_field.as_str()] = Value::from(max_tokens);
    }
    if let Some(generation) = generation {
        apply_generation_chat(&mut body, generation);
    }
    if let Some(extensions) = extensions {
        // Deep merge so a partial provider override (for example
        // `reasoning.effort` alone) keeps the sibling keys EE already wrote.
        deep_merge(&mut body, extensions);
    }
    body
}

/// Lowers a tool choice into the Chat Completions field value.
#[must_use]
pub fn tool_choice_value(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Tool { name } => json!({ "type": "function", "function": { "name": name } }),
    }
}

/// Converts a normalized transcript into chat messages, prepending
/// `system_prompt`.
///
/// Tool observations carry their stable tool-call id; subagent summaries map to
/// user content.
#[must_use]
pub fn messages_from_transcript(system_prompt: &str, transcript: &[ModelMessage]) -> Vec<Value> {
    messages_from_transcript_with_cache(system_prompt, transcript, false)
}

/// Converts a normalized transcript into chat messages, optionally emitting
/// Anthropic/OpenRouter `cache_control` breakpoints on the stable prefix planes.
///
/// The text is never rewritten: a breakpoint turns the message content into a
/// single text part carrying `cache_control`, which providers that do not
/// support prompt caching ignore. Breakpoints are only placed on the system
/// prompt and on messages explicitly marked with `ee_cache_breakpoint`.
#[must_use]
pub fn messages_from_transcript_with_cache(
    system_prompt: &str,
    transcript: &[ModelMessage],
    cache_breakpoints: bool,
) -> Vec<Value> {
    messages_from_transcript_replaying(system_prompt, transcript, cache_breakpoints, "", None)
}

/// Converts a normalized transcript into chat messages, replaying provider
/// reasoning state the current model produced.
///
/// `model` is the model the request targets; native reasoning sealed for another
/// model is never replayed. `reasoning_field` names a scalar assistant field the
/// route requires even when the provider only streamed structured details (for
/// example Kimi's `reasoning_content`); it falls back to the message's reasoning
/// summary.
///
/// `cache_breakpoints` requests the legacy manual shape: only the system prompt
/// and messages explicitly marked with `ee_cache_breakpoint` are marked. The
/// automatic policy lives in [`chat_request_parts`].
#[must_use]
pub fn messages_from_transcript_replaying(
    system_prompt: &str,
    transcript: &[ModelMessage],
    cache_breakpoints: bool,
    model: &str,
    reasoning_field: Option<&str>,
) -> Vec<Value> {
    let cache =
        if cache_breakpoints { CacheDirective::manual_only() } else { CacheDirective::disabled() };
    chat_request_parts(&ChatRequestParts {
        system_prompt,
        transcript,
        definitions: &[],
        cache: &cache,
        model,
        provider: None,
        reasoning_field,
        tools_strict: false,
        freeform: &[],
        prompt_tokens: None,
    })
    .0
}

/// Inputs for one Chat Completions request assembly.
#[derive(Debug, Clone, Copy)]
pub struct ChatRequestParts<'a> {
    /// Profile system prompt, rendered as the first `system` message.
    pub system_prompt: &'a str,
    /// Normalized conversation.
    pub transcript: &'a [ModelMessage],
    /// Advertised tool definitions.
    pub definitions: &'a [ToolDefinition],
    /// Prompt-cache policy and route capabilities.
    pub cache: &'a CacheDirective,
    /// Target model id; native reasoning sealed for another model is not replayed.
    pub model: &'a str,
    /// Route identity native state is sealed with; `None` replays by model only.
    pub provider: Option<&'a str>,
    /// Assistant field this route requires for replayed reasoning.
    pub reasoning_field: Option<&'a str>,
    /// Whether the route accepts the function `strict` flag.
    pub tools_strict: bool,
    /// Free-form formats the route advertises.
    pub freeform: &'a [String],
    /// Prompt size measured by the provider on the previous call, when known;
    /// gates automatic placement below a route's cache minimum.
    pub prompt_tokens: Option<u64>,
}

/// Assembles the chat messages and tool schemas for one request.
///
/// Assembly and cache planning happen together so the four-breakpoint budget is
/// shared across the system, tool, and message planes instead of being applied
/// independently. Text is never rewritten: a breakpoint wraps the message
/// content in one text part carrying `cache_control`.
#[must_use]
pub fn chat_request_parts(parts: &ChatRequestParts<'_>) -> (Vec<Value>, Vec<Value>) {
    let markers = parts.cache.places_markers();
    let mut messages: Vec<Value> = Vec::new();
    let mut system_entries: Vec<usize> = Vec::new();
    let mut candidates: Vec<CacheCandidate> = Vec::new();
    let mut manual: Vec<usize> = Vec::new();
    let mut manual_system: Vec<usize> = Vec::new();
    let mut system_parts = 0usize;

    messages.push(json!({ "role": "system", "content": parts.system_prompt }));
    if !parts.system_prompt.is_empty() {
        system_entries.push(0);
        system_parts += 1;
    }
    for message in parts.transcript {
        let role = match message.role {
            ModelRole::System => "system",
            ModelRole::User | ModelRole::Subagent => "user",
            ModelRole::Assistant => "assistant",
            ModelRole::Tool => "tool",
        };
        let content = content_text(&message.content);
        let media = content_has_media(&message.content);
        let mut entry = if role == "tool" {
            json!({
                "role": "tool",
                "tool_call_id": tool_call_id_of(&message.content),
                "content": content,
            })
        } else if media {
            json!({ "role": role, "content": content_parts(&message.content) })
        } else {
            json!({ "role": role, "content": content })
        };
        if message.role == ModelRole::Assistant {
            replay_tool_calls(&mut entry, message, content.is_empty(), parts.provider, parts.model);
            replay_reasoning(
                &mut entry,
                message,
                parts.provider,
                parts.model,
                parts.reasoning_field,
            );
        }
        let index = messages.len();
        messages.push(entry);
        if content.is_empty() || media {
            continue;
        }
        match message.role {
            ModelRole::System => {
                system_entries.push(index);
                system_parts += 1;
                if markers && message.is_cache_breakpoint() {
                    manual_system.push(system_parts - 1);
                }
            }
            ModelRole::Tool => {
                candidates.push(CacheCandidate { index, role: CacheCandidateRole::ToolResult })
            }
            ModelRole::User | ModelRole::Subagent => {
                candidates.push(CacheCandidate { index, role: CacheCandidateRole::User });
                if markers && message.is_cache_breakpoint() {
                    manual.push(index);
                }
            }
            ModelRole::Assistant => {
                candidates.push(CacheCandidate { index, role: CacheCandidateRole::Assistant });
                if markers && message.is_cache_breakpoint() {
                    manual.push(index);
                }
            }
        }
    }

    let inputs = CacheCandidates {
        tools: parts.definitions.len(),
        system_parts,
        messages: &candidates,
        manual: &manual,
        manual_system: &manual_system,
        // A `tool` message carries a plain string on this wire; documented
        // breakpoints only exist on text parts, so the tail stays on the last
        // text-bearing message.
        tool_results: false,
        prompt_tokens: parts.prompt_tokens,
    };
    let cache_plan = plan(parts.cache, &inputs);
    warn_dropped(&cache_plan, "Chat Completions", parts.cache.support.max_breakpoints);
    let marker = cache_control(parts.cache);
    for &part in &cache_plan.system {
        if let Some(&entry) = system_entries.get(part)
            && let Some(message) = messages.get_mut(entry)
        {
            wrap_text_content(message, &marker);
        }
    }
    for &index in &cache_plan.messages {
        if let Some(message) = messages.get_mut(index) {
            wrap_text_content(message, &marker);
        }
    }

    let mut tools =
        tools_from_definitions_with(parts.definitions, parts.tools_strict, parts.freeform);
    if let Some(index) = cache_plan.tools
        && let Some(tool) = tools.get_mut(index)
    {
        tool["cache_control"] = marker;
    }
    (messages, tools)
}

/// Replaces a message's string content with one text part carrying a marker.
fn wrap_text_content(message: &mut Value, marker: &Value) {
    let Some(text) = message.get("content").and_then(Value::as_str) else {
        return;
    };
    if text.is_empty() {
        return;
    }
    message["content"] = json!([{ "type": "text", "text": text, "cache_control": marker }]);
}

/// Converts normalized tool definitions into a function tool schema.
#[must_use]
pub fn tools_from_definitions(definitions: &[ToolDefinition]) -> Vec<Value> {
    tools_from_definitions_with(definitions, false, &[])
}

/// Converts tool definitions, emitting `strict` and free-form tools only where
/// the route advertises support.
///
/// A definition whose format the route does not advertise stays a function
/// tool, so a grammar tool is never silently misdeclared.
#[must_use]
pub fn tools_from_definitions_with(
    definitions: &[ToolDefinition],
    tools_strict: bool,
    freeform: &[String],
) -> Vec<Value> {
    definitions
        .iter()
        .map(|definition| {
            if let Some(format) = &definition.format
                && freeform.iter().any(|supported| supported == format.as_str())
            {
                return json!({
                    "type": "custom",
                    "custom": {
                        "name": definition.name,
                        "description": definition.description,
                        "format": tool_format_value(format),
                    }
                });
            }
            let mut function = json!({
                "name": definition.name,
                "description": definition.description,
                "parameters": definition.input_schema,
            });
            if tools_strict {
                function["strict"] = Value::from(definition.strict);
            }
            json!({ "type": "function", "function": function })
        })
        .collect()
}

/// Lowers one free-form tool format into the provider shape.
fn tool_format_value(format: &ToolFormat) -> Value {
    match format {
        ToolFormat::Text => json!({ "type": "text" }),
        ToolFormat::Lark { definition } => json!({
            "type": "grammar",
            "grammar": { "syntax": "lark", "definition": definition },
        }),
        ToolFormat::Regex { definition } => json!({
            "type": "grammar",
            "grammar": { "syntax": "regex", "definition": definition },
        }),
        // The enum is non-exhaustive; a future format degrades to raw text
        // rather than sending an unknown provider shape.
        _ => json!({ "type": "text" }),
    }
}
