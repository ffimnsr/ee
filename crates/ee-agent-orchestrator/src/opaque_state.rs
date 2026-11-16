//! Opaque provider reasoning state.
//!
//! State such as Anthropic thinking signatures, `reasoning_details`, and
//! encrypted OpenAI reasoning is only meaningful to the provider and model that
//! produced it. It is wrapped here so a transcript, log, or error dump can never
//! print it and an oversized value is dropped rather than replayed truncated.

use serde::{Deserialize, Serialize};

/// Bounded opaque provider state that must be replayed verbatim to the same
/// provider and model (reasoning signatures, `reasoning_details`, encrypted
/// reasoning).
///
/// `Debug` never prints the payload, so a transcript or error dump cannot leak
/// provider reasoning text. Values above [`OpaqueProviderState::MAX_BYTES`] are
/// dropped rather than replayed in truncated form.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OpaqueProviderState(serde_json::Value);

impl OpaqueProviderState {
    /// Largest accepted serialized payload, in bytes.
    pub const MAX_BYTES: usize = 64 * 1024;

    /// Wraps a payload when it fits the bound.
    #[must_use]
    pub fn new(value: serde_json::Value) -> Option<Self> {
        let bytes = serde_json::to_vec(&value).ok()?.len();
        (bytes <= Self::MAX_BYTES).then_some(Self(value))
    }

    /// Borrows the payload.
    #[must_use]
    pub fn as_value(&self) -> &serde_json::Value {
        &self.0
    }

    /// Consumes the wrapper and returns the payload.
    #[must_use]
    pub fn into_value(self) -> serde_json::Value {
        self.0
    }
}

impl std::fmt::Debug for OpaqueProviderState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("OpaqueProviderState([opaque])")
    }
}
