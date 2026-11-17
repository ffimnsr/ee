//! Provider-side response continuation anchors.
//!
//! A continuation names the provider response a request extends and the first
//! transcript message the provider has not seen. Only routes that advertise
//! server-side state use it; every other route sends the full transcript and
//! ignores the anchor.

use serde::{Deserialize, Serialize};

/// A provider-side continuation anchor and the transcript index to send from.
///
/// `input_from` is a message index into the projected request transcript: items
/// before it were already delivered by the response identified by
/// `previous_response_id`, so only the delta is sent. The Responses codec
/// ignores this when the profile does not advertise store support.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Continuation {
    /// Provider response id to continue from.
    pub previous_response_id: String,
    /// First transcript message not yet delivered to the provider.
    pub input_from: usize,
}

impl Continuation {
    /// Creates a continuation anchor.
    #[must_use]
    pub fn new(previous_response_id: impl Into<String>, input_from: usize) -> Self {
        Self { previous_response_id: previous_response_id.into(), input_from }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continuation_roundtrips_and_keeps_the_index() {
        let anchor = Continuation::new("resp_1", 4);
        let json = serde_json::to_string(&anchor).expect("serializes");
        let restored: Continuation = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, anchor);
        assert_eq!(restored.input_from, 4);
    }
}
