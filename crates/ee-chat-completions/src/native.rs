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
    let Value::Object(mut object) = state else {
        return None;
    };
    object.insert(NATIVE_META_KEY.to_string(), json!({ "model": model }));
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
    let object = native.as_value().as_object()?;
    let producer = object.get(NATIVE_META_KEY)?.get("model")?.as_str()?;
    (producer == model).then_some(object)
}
