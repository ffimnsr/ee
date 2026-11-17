//! Opaque provider reasoning state: sealing, replay gating, and field shaping.
//!
//! Provider reasoning state (Anthropic thinking signatures, OpenAI encrypted
//! reasoning items, OpenRouter `reasoning_details`) may only be replayed to the
//! model that produced it. Every codec seals the decoded state with the model id
//! that produced it and re-emits it only when the next request targets the same
//! model.
//!
//! The envelope is a provider-neutral object: `_ee` holds the producer identity
//! and every other key is the codec's own payload. Encoders read only their own
//! dialect's keys, so a transcript that moves between dialects never replays a
//! foreign shape.

use serde_json::{Value, json};

use ee_agent_orchestrator::{ModelResponse, OpaqueProviderState};

/// Envelope key holding the producer identity. Never sent on the wire.
pub const NATIVE_META_KEY: &str = "_ee";

/// Envelope key holding the producer provider label inside [`NATIVE_META_KEY`].
pub const PROVIDER_META_KEY: &str = "provider";

/// Chat Completions assistant fields that may be replayed verbatim.
///
/// `reasoning_details` keeps every field it arrived with, including `index`;
/// the scalar spellings are echoed under the name the provider used.
pub const CHAT_REPLAY_FIELDS: [&str; 4] =
    ["reasoning", "reasoning_content", "reasoning_text", "reasoning_opaque"];

/// Seals decoded provider state with the model that produced it.
///
/// Returns `None` for a non-object payload or one that exceeds the opaque-state
/// bound, so oversized state is dropped rather than replayed truncated.
#[must_use]
pub fn seal(state: Value, model: &str) -> Option<OpaqueProviderState> {
    seal_with_provider(state, None, model)
}

/// Seals decoded provider state with the provider and model that produced it.
///
/// The provider label is a route identity (for example `OpenRouter`), not a
/// credential. Recording it lets a transcript replay reasoning to the same
/// route even when two providers expose the same model id.
#[must_use]
pub fn seal_with_provider(
    state: Value,
    provider: Option<&str>,
    model: &str,
) -> Option<OpaqueProviderState> {
    let Value::Object(mut object) = state else {
        return None;
    };
    let mut identity = json!({ "model": model });
    if let Some(provider) = provider.filter(|provider| !provider.is_empty()) {
        identity[PROVIDER_META_KEY] = Value::String(provider.to_string());
    }
    object.insert(NATIVE_META_KEY.to_string(), identity);
    OpaqueProviderState::new(Value::Object(object))
}

/// Seals a response's decoded provider state with the model that produced it.
#[must_use]
pub fn seal_response(mut response: ModelResponse, model: &str) -> ModelResponse {
    if let Some(state) = response.native.take() {
        response.native = seal(state.into_value(), model);
    }
    response
}

/// Seals a response's decoded provider state with the provider and model.
#[must_use]
pub fn seal_response_with_provider(
    mut response: ModelResponse,
    provider: Option<&str>,
    model: &str,
) -> ModelResponse {
    if let Some(state) = response.native.take() {
        response.native = seal_with_provider(state.into_value(), provider, model);
    }
    response
}

/// Provider state that `model` may replay.
///
/// Returns `None` when the state was produced by another model or carries no
/// producer identity: an unknown producer is never replayed, because replaying
/// foreign reasoning can be rejected by the provider or degrade the turn.
#[must_use]
pub fn replayable<'a>(
    native: &'a OpaqueProviderState,
    model: &str,
) -> Option<&'a serde_json::Map<String, Value>> {
    replayable_for(native, None, model)
}

/// Provider state that `provider` and `model` may replay.
///
/// A payload sealed with a provider label returns `None` unless the caller
/// names the same provider. Payloads sealed before provider identity existed
/// (no provider key) still replay by model so stored transcripts stay usable.
#[must_use]
pub fn replayable_for<'a>(
    native: &'a OpaqueProviderState,
    provider: Option<&str>,
    model: &str,
) -> Option<&'a serde_json::Map<String, Value>> {
    let object = native.as_value().as_object()?;
    let identity = object.get(NATIVE_META_KEY)?;
    let producer = identity.get("model")?.as_str()?;
    if producer != model {
        return None;
    }
    if let Some(sealed) = identity.get(PROVIDER_META_KEY).and_then(Value::as_str)
        && provider != Some(sealed)
    {
        return None;
    }
    Some(object)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn provider_scoped_state_replays_only_to_the_same_provider_and_model() {
        let state =
            seal_with_provider(json!({ "thinking_blocks": [] }), Some("OpenRouter"), "claude-x")
                .expect("fits");
        assert!(replayable_for(&state, Some("OpenRouter"), "claude-x").is_some());
        assert!(replayable_for(&state, Some("OpenCode"), "claude-x").is_none());
        assert!(replayable_for(&state, Some("OpenRouter"), "claude-y").is_none());
        assert!(
            replayable_for(&state, None, "claude-x").is_none(),
            "a provider-scoped payload is never replayed without a provider match"
        );
    }

    #[test]
    fn legacy_state_without_provider_still_replays_by_model() {
        let state = seal(json!({ "thinking_blocks": [] }), "claude-x").expect("fits");
        assert!(replayable(&state, "claude-x").is_some());
        assert!(replayable_for(&state, Some("OpenRouter"), "claude-x").is_some());
        assert!(replayable(&state, "claude-y").is_none());
    }
}
