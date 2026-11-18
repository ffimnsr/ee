//! OpenAI Responses native compaction (`POST /responses/compact`).
//!
//! Provider-side compaction replaces the conversation prefix with a summary
//! plus an encrypted checkpoint. The checkpoint is opaque: it is replayed
//! byte-for-byte on later requests and never parsed, logged, or rendered.
//!
//! The compact endpoint accepts a smaller body than `/responses`: no `store`,
//! no `stream`, no output-token bound, and no tool choice. The request builder
//! therefore shares the item and tool encoders with
//! [`crate::responses`] while emitting only fields the endpoint documents.

use ee_agent_acp_server::ProviderError;
use ee_agent_orchestrator::{
    CompactionCheckpoint, CompactionOutcome, GenerationOptions, ModelContent, ModelMessage,
    ModelRole, SessionContext, ToolDefinition,
};
use serde_json::{Value, json};

use crate::wire::codec_error;
use crate::{
    CacheDirective, TrustedEndpoint, deep_merge, model_usage_of, openai_request_ttl,
    responses::{ResponsesCodec, guarded_transcript},
    responses_encode::{
        REASONING_ITEMS, compaction_items, function_call_items, input_item, tools_array,
    },
    responses_wire::{error_detail, prompt_cache_key},
    seal_with_provider, usage_from_tokens, with_buffered_retry,
};

/// Path appended to a Responses endpoint for native compaction.
pub const COMPACT_PATH_SUFFIX: &str = "/compact";

/// Sibling compaction endpoint for one Responses endpoint.
#[must_use]
pub fn compact_endpoint(endpoint: &str) -> String {
    format!("{}{COMPACT_PATH_SUFFIX}", endpoint.trim_end_matches('/'))
}

impl crate::EndpointTransport {
    /// Returns a transport that sends to `endpoint` with the same profile,
    /// credential, HTTP client, and phase budgets.
    ///
    /// Used for sibling operations documented under the same route (the
    /// Responses `/compact` endpoint) without rebuilding the profile.
    #[must_use]
    pub fn with_endpoint(&self, endpoint: TrustedEndpoint) -> Self {
        let mut profile = self.profile.clone();
        profile.endpoint = endpoint;
        self.with_profile(profile)
    }
}

impl ResponsesCodec {
    /// Runs provider-native compaction through the sibling `/compact` endpoint.
    ///
    /// The endpoint sits next to `/responses` under the same catalog root, so
    /// the profile's credential, headers, and budgets apply unchanged. The
    /// returned replacement transcript carries the encrypted checkpoint as an
    /// opaque content part.
    ///
    /// # Errors
    ///
    /// Returns a typed error for an unusable compact endpoint, transport or
    /// status failure, or a payload without exactly one checkpoint.
    pub async fn compact(
        &self,
        transcript: &[ModelMessage],
        tools: &[ToolDefinition],
        generation: Option<&GenerationOptions>,
        session: Option<&SessionContext>,
    ) -> Result<CompactionOutcome, ProviderError> {
        let session_transport = self.transport.with_session(session)?;
        let profile = session_transport.profile();
        let transcript = guarded_transcript(profile, transcript);
        let endpoint = TrustedEndpoint::parse(&compact_endpoint(profile.endpoint.as_str()))
            .map_err(|error| {
                codec_error(&profile.label, format!("compaction endpoint is not usable: {error}"))
            })?;
        let transport = session_transport.with_endpoint(endpoint);
        let profile = transport.profile();
        let body = compact_body(&CompactBody {
            model: &profile.model,
            provider: Some(profile.label.as_str()),
            system_prompt: &profile.system_prompt,
            transcript: transcript.as_ref(),
            tools,
            extensions: profile.extensions.as_ref(),
            generation,
            cache: &profile.cache,
            session,
        });
        let label = profile.label.as_str();
        with_buffered_retry(&profile.retry, || async {
            send_compact(&transport, &body, label, &profile.model).await
        })
        .await
    }
}

/// Sends one compaction request, mapping a missing endpoint to a capability
/// failure so `auto` mode can fall back to the local summary.
async fn send_compact(
    transport: &crate::EndpointTransport,
    body: &Value,
    provider: &str,
    model: &str,
) -> Result<CompactionOutcome, ProviderError> {
    let mut response = transport.send_json(body).await?;
    let status = response.status();
    if matches!(
        status,
        reqwest::StatusCode::NOT_FOUND
            | reqwest::StatusCode::METHOD_NOT_ALLOWED
            | reqwest::StatusCode::NOT_IMPLEMENTED
    ) {
        return Err(ProviderError::UnsupportedCompaction(format!(
            "{provider} {model} (responses)"
        )));
    }
    if !status.is_success() {
        let headers = response.headers().clone();
        let value = transport.read_json(&mut response, "compaction error response").await?;
        return Err(transport.status_error(status, &headers, &value));
    }
    let value = transport.read_json(&mut response, "compaction response").await?;
    decode_compaction(provider, model, &value)
}

/// Inputs for one `/responses/compact` body.
#[derive(Debug, Clone, Copy)]
pub struct CompactBody<'a> {
    /// Wire model id.
    pub model: &'a str,
    /// Route identity native state is sealed with; `None` replays by model only.
    pub provider: Option<&'a str>,
    /// Profile system prompt, sent as `instructions`.
    pub system_prompt: &'a str,
    /// Conversation to compact.
    pub transcript: &'a [ModelMessage],
    /// Tools the compacted conversation may reference.
    pub tools: &'a [ToolDefinition],
    /// Provider body fields merged last.
    pub extensions: Option<&'a Value>,
    /// Sampling knobs to write; `None` leaves every field absent.
    pub generation: Option<&'a GenerationOptions>,
    /// Prompt-cache policy, for the request-level lifetime only.
    pub cache: &'a CacheDirective,
    /// Session affinity; the cache key derives from it when present.
    pub session: Option<&'a SessionContext>,
}

/// Builds an OpenAI Responses `/compact` request body.
#[must_use]
pub fn compact_body(input: &CompactBody<'_>) -> Value {
    let mut items = Vec::new();
    for message in input.transcript {
        let checkpoints = compaction_items(message, input.provider);
        if !checkpoints.is_empty() {
            items.extend(checkpoints);
            continue;
        }
        if message.role == ModelRole::Assistant {
            items.extend(crate::responses_encode::replay_reasoning_items(
                message,
                input.provider,
                input.model,
            ));
            if let Some(item) = input_item(message, input.provider) {
                items.push(item);
            }
            items.extend(function_call_items(message));
        } else if let Some(item) = input_item(message, input.provider) {
            items.push(item);
        }
    }
    let mut body = json!({
        "model": input.model,
        "input": items,
        "include": ["reasoning.encrypted_content"],
    });
    if !input.system_prompt.is_empty() {
        body["instructions"] = Value::String(input.system_prompt.to_string());
        let key = input
            .session
            .map(SessionContext::cache_key)
            .filter(|key| !key.is_empty())
            .unwrap_or_else(|| prompt_cache_key(input.model, input.system_prompt));
        body["prompt_cache_key"] = Value::String(key);
    }
    if let Some(Value::Object(ttl)) = openai_request_ttl(input.cache)
        && let Some(object) = body.as_object_mut()
    {
        for (key, value) in ttl {
            object.insert(key, value);
        }
    }
    if !input.tools.is_empty() {
        body["tools"] = tools_array(input.tools);
    }
    if let Some(generation) = input.generation {
        crate::generation::apply_generation_responses(&mut body, generation);
    }
    if let Some(extensions) = input.extensions {
        deep_merge(&mut body, extensions);
    }
    body
}

/// Decodes a `/responses/compact` payload into a replacement transcript.
///
/// The provider must return exactly one `compaction` item; a response without a
/// checkpoint is a provider contract violation, because replaying the summary
/// alone would silently lose the compacted prefix.
///
/// # Errors
///
/// Returns a typed error for a provider error envelope, an unexpected payload,
/// a missing or oversized checkpoint, or an empty replacement.
pub fn decode_compaction(
    provider: &str,
    model: &str,
    value: &Value,
) -> Result<CompactionOutcome, ProviderError> {
    let label = provider;
    if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
        return Err(codec_error(label, error_detail(error)));
    }
    if value.get("object").and_then(Value::as_str) != Some("response.compaction") {
        return Err(codec_error(label, "payload is not an OpenAI Responses compaction response"));
    }
    let output = value
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| codec_error(label, "compaction response carried no output array"))?;
    let mut replacement = Vec::new();
    let mut checkpoint = None;
    let mut checkpoints = 0usize;
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("compaction") => {
                let encrypted = item
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .filter(|encrypted| !encrypted.is_empty())
                    .ok_or_else(|| {
                        codec_error(label, "compaction output is missing its encrypted content")
                    })?;
                let id = item.get("id").and_then(Value::as_str).map(str::to_string);
                let wrapped =
                    CompactionCheckpoint::new(provider, id, encrypted).ok_or_else(|| {
                        codec_error(label, "compaction checkpoint exceeds the supported size")
                    })?;
                checkpoints += 1;
                replacement.push(
                    ModelMessage::new(ModelRole::Assistant)
                        .with_content(vec![ModelContent::CompactionCheckpoint(wrapped.clone())]),
                );
                checkpoint = Some(wrapped);
            }
            Some("reasoning") => {
                let summary: String = item
                    .get("summary")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|part| part.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                let native = seal_with_provider(
                    json!({ REASONING_ITEMS: [item.clone()] }),
                    Some(provider),
                    model,
                );
                let mut message = ModelMessage::new(ModelRole::Assistant);
                if !summary.is_empty() {
                    message = message.with_reasoning_summary(summary);
                }
                if native.is_some() {
                    message = message.with_native(native);
                }
                replacement.push(message);
            }
            Some("message") => {
                let text = message_text(item);
                if text.trim().is_empty() {
                    continue;
                }
                let role = match item.get("role").and_then(Value::as_str) {
                    Some("assistant") => ModelRole::Assistant,
                    Some("system") => ModelRole::System,
                    _ => ModelRole::User,
                };
                replacement.push(ModelMessage::text(role, text));
            }
            _ => {}
        }
    }
    if checkpoints == 0 {
        return Err(codec_error(label, "compaction response did not contain a checkpoint"));
    }
    if checkpoints > 1 {
        return Err(codec_error(label, "compaction response contained more than one checkpoint"));
    }
    if replacement.is_empty() {
        return Err(codec_error(label, "compaction response replacement was empty"));
    }
    let usage =
        value.get("usage").and_then(usage_from_tokens).map(model_usage_of).unwrap_or_default();
    Ok(CompactionOutcome::new(replacement, checkpoint, usage))
}

/// Concatenated text of one `message` output item.
fn message_text(item: &Value) -> String {
    item.get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| {
                    matches!(
                        part.get("type").and_then(Value::as_str),
                        Some("input_text" | "output_text" | "text")
                    )
                    .then(|| part.get("text").and_then(Value::as_str))
                    .flatten()
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
