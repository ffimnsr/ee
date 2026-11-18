//! Usage decoding shared by the Chat Completions, Responses, and Messages
//! dialects.
//!
//! Split out of `wire.rs` to keep that module below the repository's
//! 1,000-line threshold. Every counter is decoded independently: a payload that
//! reports only cost, only cache counters, or only reasoning tokens still
//! produces a usage record, and a field the provider omitted stays `None`.

use serde_json::Value;

use super::Usage;

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
    let (cached_input_tokens, cache_creation_input_tokens) = cache_tokens_of(value);
    let reasoning_tokens = reasoning_tokens_of(value);
    let context_tokens = context_tokens_of(value);
    // Provider-reported cost is a first-class counter: a payload that carries
    // only `cost` must not be discarded as "no usage".
    let cost_micros = cost_micros_of(value);
    if usage.is_none()
        && output.is_none()
        && total.is_none()
        && cached_input_tokens.is_none()
        && cache_creation_input_tokens.is_none()
        && reasoning_tokens.is_none()
        && context_tokens.is_none()
        && cost_micros.is_none()
    {
        return None;
    }
    Some(Usage {
        input_tokens: usage,
        output_tokens: output,
        total_tokens: total,
        cached_input_tokens,
        cache_creation_input_tokens,
        reasoning_tokens,
        context_tokens,
        cost_micros,
    })
}

/// Reads a provider-reported dollar cost into integer micro-dollars.
///
/// OpenRouter (and compatible gateways) report `usage.cost` as USD with
/// fractional cents. A missing or negative value stays `None` so cost is never
/// zero-filled.
fn cost_micros_of(usage: &Value) -> Option<u64> {
    let cost = usage.get("cost")?.as_f64()?;
    if !cost.is_finite() || cost < 0.0 {
        return None;
    }
    Some((cost * 1_000_000.0).round() as u64)
}

/// Extracts `usage.prompt_tokens` / `completion_tokens` / `total_tokens`;
/// returns `None` when the response carries no usage object.
pub(crate) fn usage_of(value: &Value) -> Option<Usage> {
    let usage = value.get("usage")?;
    let input_tokens = usage.get("prompt_tokens").and_then(Value::as_u64);
    let output_tokens = usage.get("completion_tokens").and_then(Value::as_u64);
    let total_tokens = usage.get("total_tokens").and_then(Value::as_u64);
    let (cached_input_tokens, cache_creation_input_tokens) = cache_tokens_of(usage);
    let reasoning_tokens = reasoning_tokens_of(usage);
    let context_tokens = context_tokens_of(usage);
    let cost_micros = cost_micros_of(usage);
    if input_tokens.is_none()
        && output_tokens.is_none()
        && total_tokens.is_none()
        && cached_input_tokens.is_none()
        && cache_creation_input_tokens.is_none()
        && reasoning_tokens.is_none()
        && context_tokens.is_none()
        && cost_micros.is_none()
    {
        return None;
    }
    Some(Usage {
        input_tokens,
        output_tokens,
        total_tokens,
        cached_input_tokens,
        cache_creation_input_tokens,
        reasoning_tokens,
        context_tokens,
        cost_micros,
    })
}

/// Extracts prompt-cache counters from either the Anthropic names or the
/// OpenAI/OpenRouter `prompt_tokens_details.cached_tokens` shape.
fn cache_tokens_of(usage: &Value) -> (Option<u64>, Option<u64>) {
    let cached = usage
        .get("cache_read_input_tokens")
        .and_then(Value::as_u64)
        .or_else(|| usage.pointer("/prompt_tokens_details/cached_tokens").and_then(Value::as_u64))
        .or_else(|| usage.pointer("/input_tokens_details/cached_tokens").and_then(Value::as_u64));
    let creation = usage.get("cache_creation_input_tokens").and_then(Value::as_u64);
    (cached, creation)
}

/// Hidden reasoning tokens from either dialect's nested details or a top-level
/// counter.
fn reasoning_tokens_of(usage: &Value) -> Option<u64> {
    for pointer in [
        "/completion_tokens_details/reasoning_tokens",
        "/output_tokens_details/reasoning_tokens",
        "/output_tokens_details/thinking_tokens",
    ] {
        if let Some(tokens) = usage.pointer(pointer).and_then(Value::as_u64) {
            return Some(tokens);
        }
    }
    usage.get("reasoning_tokens").and_then(Value::as_u64)
}

/// Effective context size: an explicit field, or the Anthropic input + cache
/// breakdown when the payload reports one.
///
/// OpenAI-style payloads report cached tokens as a subset of `input_tokens`, so
/// adding them would double-count; only the Anthropic names imply a
/// non-overlapping breakdown.
fn context_tokens_of(usage: &Value) -> Option<u64> {
    if let Some(context) = usage.get("context_tokens").and_then(Value::as_u64) {
        return Some(context);
    }
    let anthropic_breakdown = usage.get("cache_read_input_tokens").is_some()
        || usage.get("cache_creation_input_tokens").is_some();
    if !anthropic_breakdown {
        return None;
    }
    let input = usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
    let cached = usage.get("cache_read_input_tokens").and_then(Value::as_u64).unwrap_or(0);
    let write = usage.get("cache_creation_input_tokens").and_then(Value::as_u64).unwrap_or(0);
    Some(input.saturating_add(cached).saturating_add(write))
}
