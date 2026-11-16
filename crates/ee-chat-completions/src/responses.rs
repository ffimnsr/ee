//! OpenAI Responses dialect codec (`/responses`).
//!
//! This codec is shared by every provider that speaks the OpenAI Responses
//! protocol (`ee-agent-opencode`'s Responses routes and OpenRouter's
//! `/responses` route). Routes served here get their own request,
//! buffered-response, and streaming shapes:
//!
//! - the normalized transcript becomes `input` items, with tool observations
//!   encoded as correlated `function_call_output` items so tool-call identity
//!   survives the round trip;
//! - tool definitions become flat Responses function tools;
//! - buffered output items (`message`, `reasoning`, `function_call`) and the
//!   documented SSE events (`response.output_text.delta`,
//!   `response.reasoning_summary_text.delta`,
//!   `response.function_call_arguments.delta`, `response.completed`, ...) decode
//!   into the same normalized [`ModelResponse`] the other dialects produce.
//!
//! A request encoded here is never replayed through another dialect: the
//! decoders accept only Responses payloads and fail closed on anything else.

use std::borrow::Cow;
use std::collections::BTreeMap;

use crate::{
    AssistantTurn, CacheDirective, DeltaSink, EndpointProfile, EndpointRequest, EndpointTransport,
    SseControl, StreamDelta, StreamFuture, ToolCall, Usage, content_text, drive_sse,
    emit_finished_deltas, is_event_stream, openai_request_ttl, response_from_turn, seal_response,
    tool_call_id_of, usage_from_tokens, with_buffered_retry, with_stream_retry,
};
use ee_agent_acp_server::ProviderError;
use ee_agent_orchestrator::{
    ModelContent, ModelMessage, ModelResponse, ModelRole, ToolCallControl, ToolChoice,
    ToolDefinition, estimate_prompt_tokens,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::wire::{codec_error, parse_arguments};

/// Applies the catalog modality gate and media-size budget before a request
/// body exists.
fn guarded_transcript<'a>(
    profile: &EndpointProfile,
    transcript: &'a [ModelMessage],
) -> Result<Cow<'a, [ModelMessage]>, ProviderError> {
    match ee_agent_orchestrator::guard_transcript(profile.modalities, &profile.model, transcript) {
        Ok(None) => Ok(Cow::Borrowed(transcript)),
        Ok(Some(bounded)) => Ok(Cow::Owned(bounded)),
        Err(detail) => Err(codec_error(&profile.label, detail)),
    }
}

/// Fills a catalog cost and seals the response for the producing model.
fn finalize(mut response: ModelResponse, profile: &EndpointProfile) -> ModelResponse {
    ee_agent_orchestrator::apply_catalog_cost(&mut response, profile.cost);
    seal_response(response, &profile.model)
}

/// Inputs for one Responses request body.
#[derive(Debug, Clone, Copy)]
pub struct ResponsesBody<'a> {
    /// Wire model id.
    pub model: &'a str,
    /// Profile system prompt, sent as `instructions`.
    pub system_prompt: &'a str,
    /// Normalized conversation.
    pub transcript: &'a [ModelMessage],
    /// Advertised tools.
    pub tools: &'a [ToolDefinition],
    /// Whether the response is streamed.
    pub stream: bool,
    /// Output-token bound.
    pub max_output_tokens: Option<u64>,
    /// Provider body fields merged last.
    pub extensions: Option<&'a Value>,
    /// Tool-call control.
    pub tool_control: &'a ToolCallControl,
    /// Prompt-cache policy, for the request-level lifetime only.
    pub cache: &'a CacheDirective,
}

/// Builds an OpenAI Responses request body.
///
/// The profile system prompt becomes `instructions`; transcript messages stay in
/// order as `input` items. `extensions` must be a JSON object when present and is
/// merged last.
#[must_use]
pub fn request_body(
    model: &str,
    system_prompt: &str,
    transcript: &[ModelMessage],
    tools: &[ToolDefinition],
    stream: bool,
    max_output_tokens: Option<u64>,
    extensions: Option<&Value>,
) -> Value {
    let auto = ToolCallControl::auto();
    let cache = CacheDirective::disabled();
    responses_body(&ResponsesBody {
        model,
        system_prompt,
        transcript,
        tools,
        stream,
        max_output_tokens,
        extensions,
        tool_control: &auto,
        cache: &cache,
    })
}

/// Builds a Responses body with tool-call control and the request-level cache
/// lifetime.
///
/// Responses ignores inline cache markers, so a configured lifetime lands in
/// `prompt_cache_retention` (24h) or `prompt_cache_options.ttl` instead.
#[must_use]
pub fn responses_body(input: &ResponsesBody<'_>) -> Value {
    let mut items = Vec::new();
    for message in input.transcript {
        // Reasoning items precede the assistant item they belong to: the
        // protocol replays them as their own input items, in generation order.
        if message.role == ModelRole::Assistant {
            items.extend(replay_reasoning_items(message, input.model));
            if let Some(item) = input_item(message) {
                items.push(item);
            }
            items.extend(function_call_items(message));
        } else if let Some(item) = input_item(message) {
            items.push(item);
        }
    }
    let mut body = json!({
        "model": input.model,
        "input": items,
        "stream": input.stream,
        // EE never asks the provider to keep the response for later retrieval.
        "store": false,
        // Encrypted reasoning travels back in the transcript, so a stateless
        // multi-step loop keeps its reasoning without server-side storage.
        "include": ["reasoning.encrypted_content"],
    });
    if let Some(max_output_tokens) = input.max_output_tokens {
        body["max_output_tokens"] = Value::from(max_output_tokens);
    }
    if !input.system_prompt.is_empty() {
        body["instructions"] = Value::String(input.system_prompt.to_string());
        // Deterministic, session-config-scoped cache key: stable across every
        // turn of the session because it hashes only the model id and the
        // stable system prompt. Routes that do not support the field ignore
        // it; extensions may still override it last.
        body["prompt_cache_key"] =
            Value::String(prompt_cache_key(input.model, input.system_prompt));
    }
    if let Some(Value::Object(ttl)) = openai_request_ttl(input.cache)
        && let Some(object) = body.as_object_mut()
    {
        for (key, value) in ttl {
            object.insert(key, value);
        }
    }
    if !input.tools.is_empty() {
        body["tools"] = Value::Array(
            input
                .tools
                .iter()
                .map(|definition| {
                    json!({
                        "type": "function",
                        "name": definition.name,
                        "description": definition.description,
                        "parameters": definition.input_schema,
                    })
                })
                .collect(),
        );
        body["tool_choice"] = responses_tool_choice(&input.tool_control.choice);
        if let Some(parallel) = input.tool_control.parallel {
            body["parallel_tool_calls"] = Value::from(parallel);
        }
    }
    if let Some(Value::Object(extra)) = input.extensions
        && let Some(object) = body.as_object_mut()
    {
        for (key, value) in extra {
            object.insert(key.clone(), value.clone());
        }
    }
    body
}

/// Lowers a tool choice into the Responses field value.
fn responses_tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Tool { name } => json!({ "type": "function", "name": name }),
    }
}

/// Deterministic prompt-cache key for one route/session configuration.
fn prompt_cache_key(model: &str, system_prompt: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(model.as_bytes());
    hasher.update([0]);
    hasher.update(system_prompt.as_bytes());
    let digest = hasher.finalize();
    digest.iter().take(16).map(|byte| format!("{byte:02x}")).collect()
}

/// Converts one transcript message into an input item.
///
/// Messages whose rendered text is empty are omitted: Responses rejects empty
/// text parts.
fn input_item(message: &ModelMessage) -> Option<Value> {
    let text = content_text(&message.content);
    match message.role {
        ModelRole::Tool => Some(json!({
            "type": "function_call_output",
            "call_id": tool_call_id_of(&message.content),
            "output": text,
        })),
        ModelRole::User | ModelRole::Subagent | ModelRole::System | ModelRole::Assistant => {
            let parts = response_parts(message.role, &message.content);
            if parts.is_empty() {
                return None;
            }
            let role = match message.role {
                ModelRole::System => "system",
                ModelRole::Assistant => "assistant",
                _ => "user",
            };
            Some(json!({
                "type": "message",
                "role": role,
                "content": parts,
            }))
        }
    }
}

/// Builds Responses content parts; text keeps the dialect's part names and
/// image payloads become `input_image` items.
fn response_parts(role: ModelRole, content: &[ModelContent]) -> Vec<Value> {
    let text_part = if role == ModelRole::Assistant { "output_text" } else { "input_text" };
    let mut parts = Vec::new();
    for block in content {
        match block {
            ModelContent::Text(text) => push_response_text(&mut parts, text_part, text),
            ModelContent::ToolResult { result, .. } => {
                push_response_text(&mut parts, text_part, &result.summary_text())
            }
            ModelContent::FileReference { path } => {
                push_response_text(&mut parts, text_part, &format!("[file:{path}]"))
            }
            ModelContent::TerminalReference { terminal_id } => {
                push_response_text(&mut parts, text_part, &format!("[terminal:{terminal_id}]"))
            }
            ModelContent::Image { data, mime_type, .. } => {
                parts.push(input_image(data, mime_type));
            }
            ModelContent::Audio { mime_type, .. } => {
                push_response_text(&mut parts, text_part, &format!("[audio:{mime_type}]"))
            }
            ModelContent::EmbeddedResource { uri, mime_type, text, blob } => match (text, blob) {
                (Some(text), _) => {
                    let rendered = format!("[resource:{uri}]\n{text}");
                    push_response_text(&mut parts, text_part, &rendered)
                }
                (None, Some(blob)) => match mime_type.as_deref() {
                    Some(mime) if mime.starts_with("image/") => {
                        parts.push(input_image(blob, mime));
                    }
                    mime => push_response_text(
                        &mut parts,
                        text_part,
                        &format!("[resource:{uri}:{}]", mime.unwrap_or("application/octet-stream")),
                    ),
                },
                (None, None) => {
                    push_response_text(&mut parts, text_part, &format!("[resource:{uri}]"))
                }
            },
            _ => {} // future content kinds stay out of the request
        }
    }
    parts
}

fn push_response_text(parts: &mut Vec<Value>, part_type: &str, text: &str) {
    if !text.is_empty() {
        parts.push(json!({ "type": part_type, "text": text }));
    }
}

fn input_image(data: &str, mime_type: &str) -> Value {
    json!({
        "type": "input_image",
        "image_url": format!("data:{mime_type};base64,{data}"),
    })
}

/// Replays the assistant tool calls a turn's observations answer.
///
/// Responses keeps a function call as its own input item, so the assistant turn
/// is kept in the transcript and re-encoded here; a `function_call_output`
/// without its matching call is rejected.
fn function_call_items(message: &ModelMessage) -> Vec<Value> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ModelContent::ToolCall { tool_call_id, name, arguments } => Some(json!({
                "type": "function_call",
                "call_id": tool_call_id,
                "name": name,
                "arguments": arguments.to_string(),
            })),
            _ => None,
        })
        .collect()
}

/// Key holding the replayable Responses reasoning items.
const REASONING_ITEMS: &str = "reasoning_items";

/// Reasoning input items a transcript message may replay to `model`.
///
/// Only documented fields are replayed: `id` addresses the item, `summary`
/// carries its text, and `encrypted_content` is the stateless proof of
/// generation that lets the model resume a tool loop without server storage.
fn replay_reasoning_items(message: &ModelMessage, model: &str) -> Vec<Value> {
    let Some(state) = message.native.as_ref().and_then(|native| crate::replayable(native, model))
    else {
        return Vec::new();
    };
    let Some(items) = state.get(REASONING_ITEMS).and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut replayed = Vec::new();
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("reasoning") {
            continue;
        }
        let mut replay = json!({ "type": "reasoning", "summary": [] });
        if let Some(summary) = item.get("summary").filter(|summary| summary.is_array()) {
            replay["summary"] = summary.clone();
        }
        for key in ["id", "encrypted_content"] {
            if let Some(value) = item.get(key).filter(|value| !value.is_null()) {
                replay[key] = value.clone();
            }
        }
        let has_summary = replay["summary"].as_array().is_some_and(|summary| !summary.is_empty());
        if has_summary || replay.get("encrypted_content").is_some() {
            replayed.push(replay);
        }
    }
    replayed
}

/// Decodes a buffered Responses payload into a normalized response.
///
/// # Errors
///
/// Returns a typed error when the payload is not a Responses object, reports a
/// terminal failure, or carries an incomplete function call.
pub fn response_from_buffered(label: &str, value: &Value) -> Result<ModelResponse, ProviderError> {
    // An error envelope is valid on any dialect and is checked first, so a
    // provider failure never masquerades as a shape error.
    if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
        return Err(codec_error(label, error_detail(error)));
    }
    if !is_responses_payload(value) {
        return Err(codec_error(label, "payload is not an OpenAI Responses response"));
    }
    let status = value.get("status").and_then(Value::as_str);
    if status == Some("failed") {
        return Err(codec_error(label, "response failed"));
    }
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut reasoning_items = Vec::new();
    let mut tool_calls = Vec::new();
    for item in output_items(value) {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                text.push_str(&item_text(item));
                text.push_str(&refusal_text(item));
            }
            Some("reasoning") => {
                reasoning.push_str(&item_reasoning(item));
                reasoning_items.push(item.clone());
            }
            Some("function_call") => tool_calls.push(function_call_of(label, item)?),
            _ => {}
        }
    }
    let native_reasoning =
        (!reasoning_items.is_empty()).then(|| json!({ REASONING_ITEMS: reasoning_items }));
    Ok(response_from_turn(AssistantTurn {
        content: text,
        reasoning,
        raw: Value::Array(output_items(value).to_vec()),
        native_reasoning,
        tool_calls,
        finish_reason: Some(finish_reason(status, value)),
        usage: usage_from_tokens(value.get("usage").unwrap_or(&Value::Null)),
    }))
}

/// Decodes a buffered Responses payload that carries only usage/status, used by
/// streaming terminal events.
fn is_responses_payload(value: &Value) -> bool {
    let object = value.get("object").and_then(Value::as_str) == Some("response");
    object
        || value.get("status").is_some()
        || value.get("output").is_some()
        || value.get("incomplete_details").is_some()
}

fn output_items(value: &Value) -> &[Value] {
    value.get("output").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

/// Text of one `message` item.
fn item_text(item: &Value) -> String {
    match item.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("output_text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect(),
        _ => String::new(),
    }
}

/// Refusal text of one `message` item; a refusal is user-visible output.
fn refusal_text(item: &Value) -> String {
    item.get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("refusal"))
        .filter_map(|part| part.get("refusal").and_then(Value::as_str))
        .collect()
}

/// Reasoning text of one `reasoning` item: summary parts first, then raw parts.
fn item_reasoning(item: &Value) -> String {
    let mut parts = Vec::new();
    for field in ["summary", "content"] {
        for part in item.get(field).and_then(Value::as_array).into_iter().flatten() {
            if let Some(text) = part.get("text").and_then(Value::as_str)
                && !text.is_empty()
            {
                parts.push(text);
            }
        }
    }
    parts.join("")
}

/// Decodes one `function_call` item, failing closed on missing identity.
fn function_call_of(label: &str, item: &Value) -> Result<ToolCall, ProviderError> {
    let id = item
        .get("call_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| codec_error(label, "function call is missing its call id"))?;
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| codec_error(label, "function call is missing its name"))?;
    let arguments = match item.get("arguments") {
        Some(Value::String(raw)) => parse_arguments(raw),
        Some(object @ Value::Object(_)) => object.clone(),
        _ => Value::Null,
    };
    Ok(ToolCall { id: id.to_string(), name: name.to_string(), arguments })
}

/// Canonical stop signal for a buffered payload.
fn finish_reason(status: Option<&str>, value: &Value) -> String {
    match status {
        Some("completed") | None => String::from("stop"),
        Some("incomplete") => {
            let reason = value
                .pointer("/incomplete_details/reason")
                .and_then(Value::as_str)
                .unwrap_or("incomplete");
            match reason {
                "max_output_tokens" => String::from("length"),
                other => other.to_string(),
            }
        }
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

/// Incremental decoder for Responses SSE events.
#[derive(Debug)]
pub struct StreamDecoder {
    label: String,
    text: String,
    reasoning: String,
    reasoning_items: BTreeMap<u64, Value>,
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
            reasoning_items: BTreeMap::new(),
            calls: BTreeMap::new(),
            finish_reason: None,
            usage: None,
        }
    }

    /// Applies one SSE `data` payload, forwarding displayable deltas to `sink`.
    ///
    /// # Errors
    ///
    /// Returns a typed error for terminal failure events, malformed function
    /// call deltas, or a sink that is already closed.
    pub fn apply(&mut self, event: &str, sink: DeltaSink<'_>) -> Result<SseControl, ProviderError> {
        let value: Value = serde_json::from_str(event).map_err(|error| {
            codec_error(&self.label, format!("stream event was not JSON: {error}"))
        })?;
        let event_type = value.get("type").and_then(Value::as_str).unwrap_or_default();
        match event_type {
            "response.output_text.delta" => {
                let delta = value.get("delta").and_then(Value::as_str).unwrap_or_default();
                if !delta.is_empty() {
                    self.text.push_str(delta);
                    sink(StreamDelta::Text(delta.to_string()))?;
                }
            }
            "response.output_text.done" => {
                // A provider that skipped deltas still reports the final text.
                if self.text.is_empty()
                    && let Some(text) =
                        value.get("text").and_then(Value::as_str).filter(|text| !text.is_empty())
                {
                    self.text.push_str(text);
                    sink(StreamDelta::Text(text.to_string()))?;
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let delta = value.get("delta").and_then(Value::as_str).unwrap_or_default();
                if !delta.is_empty() {
                    self.reasoning.push_str(delta);
                    sink(StreamDelta::Reasoning(delta.to_string()))?;
                }
            }
            "response.output_item.added" => {
                self.apply_output_item(&value);
            }
            "response.output_item.done" => {
                self.apply_output_item(&value);
            }
            "response.function_call_arguments.delta" => {
                let index = output_index(&value, &self.label)?;
                let delta = value.get("delta").and_then(Value::as_str).unwrap_or_default();
                self.calls.entry(index).or_default().arguments.push_str(delta);
            }
            "response.function_call_arguments.done" => {
                let index = output_index(&value, &self.label)?;
                if let Some(arguments) = value.get("arguments").and_then(Value::as_str) {
                    self.calls.entry(index).or_default().arguments = arguments.to_string();
                }
            }
            "response.completed" | "response.incomplete" => {
                let response = value.get("response").unwrap_or(&value);
                self.finish_reason = Some(finish_reason(
                    response
                        .get("status")
                        .and_then(Value::as_str)
                        .or(Some(event_type.trim_start_matches("response."))),
                    response,
                ));
                if let Some(usage) =
                    usage_from_tokens(response.get("usage").unwrap_or(&Value::Null))
                {
                    self.usage = Some(usage);
                }
                return Ok(SseControl::Stop);
            }
            "response.failed" => {
                let response = value.get("response").unwrap_or(&value);
                let detail = response
                    .get("error")
                    .map(error_detail)
                    .unwrap_or_else(|| String::from("response failed"));
                return Err(codec_error(&self.label, detail));
            }
            "error" => return Err(codec_error(&self.label, error_detail(&value))),
            _ => {}
        }
        Ok(SseControl::Continue)
    }

    /// Records `call_id`/`name`/`arguments` from a function-call output item and
    /// keeps reasoning items so they can be replayed with the next request.
    fn apply_output_item(&mut self, value: &Value) {
        let Some(item) = value.get("item") else {
            return;
        };
        let Ok(index) = output_index(value, &self.label) else {
            return;
        };
        match item.get("type").and_then(Value::as_str) {
            Some("reasoning") => {
                // `done` carries the encrypted payload the `added` event omits, so
                // the later event replaces the earlier one for the same index.
                self.reasoning_items.insert(index, item.clone());
            }
            Some("function_call") => {
                let call = self.calls.entry(index).or_default();
                if let Some(id) =
                    item.get("call_id").and_then(Value::as_str).filter(|id| !id.is_empty())
                {
                    call.id = Some(id.to_string());
                }
                if let Some(name) =
                    item.get("name").and_then(Value::as_str).filter(|name| !name.is_empty())
                {
                    call.name = Some(name.to_string());
                }
                if let Some(arguments) =
                    item.get("arguments").and_then(Value::as_str).filter(|args| !args.is_empty())
                {
                    call.arguments = arguments.to_string();
                }
            }
            _ => {}
        }
    }

    /// Finalizes the streamed turn.
    ///
    /// # Errors
    ///
    /// Returns a typed error when a streamed function call is missing its
    /// identity, so no partial call can reach tool execution.
    pub fn finish(self) -> Result<ModelResponse, ProviderError> {
        let mut tool_calls = Vec::with_capacity(self.calls.len());
        for (_index, call) in self.calls {
            let id = call.id.ok_or_else(|| {
                codec_error(&self.label, "streamed function call is missing its call id")
            })?;
            let name = call.name.ok_or_else(|| {
                codec_error(&self.label, "streamed function call is missing its name")
            })?;
            tool_calls.push(ToolCall { id, name, arguments: parse_arguments(&call.arguments) });
        }
        let native_reasoning = (!self.reasoning_items.is_empty()).then(
            || json!({ REASONING_ITEMS: self.reasoning_items.into_values().collect::<Vec<_>>() }),
        );
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

fn output_index(value: &Value, label: &str) -> Result<u64, ProviderError> {
    value
        .get("output_index")
        .and_then(Value::as_u64)
        .ok_or_else(|| codec_error(label, "streamed function call event is missing output_index"))
}

/// One streaming Responses round trip over an authorized endpoint.
pub struct ResponsesCodec {
    transport: EndpointTransport,
}

impl ResponsesCodec {
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
        self.complete_controlled(transcript, tools, &control, prompt_tokens).await
    }

    /// Performs one buffered round trip with tool-call control.
    pub async fn complete_controlled(
        &self,
        transcript: &[ModelMessage],
        tools: &[ToolDefinition],
        tool_control: &ToolCallControl,
        prompt_tokens: Option<u64>,
    ) -> Result<ModelResponse, ProviderError> {
        let profile = self.transport.profile();
        let transcript = guarded_transcript(profile, transcript)?;
        let prompt = prompt_tokens.unwrap_or_else(|| {
            estimate_prompt_tokens(transcript.as_ref(), tools, &profile.system_prompt)
        });
        let max_output_tokens = profile.fitted_output_limit(prompt);
        let body = responses_body(&ResponsesBody {
            model: &profile.model,
            system_prompt: &profile.system_prompt,
            transcript: transcript.as_ref(),
            tools,
            stream: false,
            max_output_tokens,
            extensions: profile.extensions.as_ref(),
            tool_control,
            cache: &profile.cache,
        });
        let transport = &self.transport;
        let label = profile.label.as_str();
        with_buffered_retry(&profile.retry, || async {
            let mut response = EndpointRequest::new(transport, &body).send().await?;
            let value = transport.read_json(&mut response, "response").await?;
            response_from_buffered(label, &value)
        })
        .await
        .map(|response| finalize(response, profile))
    }

    /// Performs one streaming round trip under the profile retry policy.
    ///
    /// A retry happens only when no delta reached `sink` yet. Function-call
    /// arguments accumulate privately, so nothing a retry could duplicate ever
    /// reached the caller, and no tool intent exists until the stream finishes.
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
        self.complete_streaming_controlled(transcript, tools, &control, sink, prompt_tokens).await
    }

    /// Performs one streaming round trip with tool-call control.
    pub async fn complete_streaming_controlled(
        &self,
        transcript: &[ModelMessage],
        tools: &[ToolDefinition],
        tool_control: &ToolCallControl,
        sink: DeltaSink<'_>,
        prompt_tokens: Option<u64>,
    ) -> Result<ModelResponse, ProviderError> {
        let profile = self.transport.profile();
        let transcript = guarded_transcript(profile, transcript)?;
        let prompt = prompt_tokens.unwrap_or_else(|| {
            estimate_prompt_tokens(transcript.as_ref(), tools, &profile.system_prompt)
        });
        let max_output_tokens = profile.fitted_output_limit(prompt);
        let body = responses_body(&ResponsesBody {
            model: &profile.model,
            system_prompt: &profile.system_prompt,
            transcript: transcript.as_ref(),
            tools,
            stream: true,
            max_output_tokens,
            extensions: profile.extensions.as_ref(),
            tool_control,
            cache: &profile.cache,
        });
        let mut attempt = ResponsesStreamAttempt { transport: &self.transport, body: &body };
        with_stream_retry(&self.transport.profile().retry, sink, &mut attempt)
            .await
            .map(|response| finalize(response, profile))
    }
}

/// One real streaming attempt for [`ResponsesCodec`].
struct ResponsesStreamAttempt<'a> {
    transport: &'a EndpointTransport,
    body: &'a Value,
}

impl crate::StreamAttempt<ModelResponse> for ResponsesStreamAttempt<'_> {
    fn run<'a>(&'a mut self, sink: DeltaSink<'a>) -> StreamFuture<'a, ModelResponse> {
        Box::pin(async move {
            let transport = self.transport;
            let label = transport.profile().label.clone();
            let model = transport.profile().model.clone();
            let mut response = EndpointRequest::new(transport, self.body).send().await?;
            if !is_event_stream(response.headers()) {
                let value = transport.read_json(&mut response, "streaming response").await?;
                let decoded = response_from_buffered(&label, &value)?;
                emit_finished_deltas(&decoded, sink)?;
                return Ok(seal_response(decoded, &model));
            }
            let mut decoder = StreamDecoder::new(&label);
            drive_sse(&label, &mut response, |event| decoder.apply(event, sink)).await?;
            decoder.finish().map(|response| seal_response(response, &model))
        })
    }
}

#[cfg(test)]
mod tests;
