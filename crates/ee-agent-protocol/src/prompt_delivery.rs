//! Prompt delivery semantics.
//!
//! ACP `session/prompt` has no delivery field.  ee extends the request with
//! an optional top-level `delivery` value (`steer` or `queue`); clients that
//! cannot add top-level fields may carry the same value under
//! `_meta.delivery`.  The default is `steer`, matching the reference inbox:
//! a prompt sent while a turn runs is injected into that turn instead of
//! being rejected.
//!
//! This module owns only parsing and the stable wire name; the inbox
//! semantics live in `ee-agent-acp-server`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Meta;

/// How a prompt should be delivered when a turn is already running.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptDelivery {
    /// Inject the prompt into the running turn at its next step boundary.
    #[default]
    Steer,
    /// Hold the prompt and promote it when the running turn settles.
    Queue,
}

impl PromptDelivery {
    /// Stable wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::Queue => "queue",
        }
    }

    /// Parses one wire value.
    ///
    /// # Errors
    ///
    /// Returns the offending value when it is not `steer` or `queue`.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "steer" => Ok(Self::Steer),
            "queue" => Ok(Self::Queue),
            other => Err(other.to_string()),
        }
    }

    /// Reads the optional delivery field from raw `session/prompt` params.
    ///
    /// The top-level `delivery` field wins; `_meta.delivery` is the fallback
    /// for clients that can only attach standard metadata.  Missing fields
    /// default to [`Self::Steer`].
    ///
    /// # Errors
    ///
    /// Returns a stable reason string for non-string or unknown values.
    pub fn from_prompt_params(params: &Value) -> Result<Self, String> {
        let raw = params
            .get("delivery")
            .or_else(|| params.get("_meta").and_then(|meta| meta.get("delivery")));
        let Some(raw) = raw else {
            return Ok(Self::default());
        };
        let Some(value) = raw.as_str() else {
            return Err(format!("delivery must be a string, got {raw}"));
        };
        Self::parse(value)
            .map_err(|value| format!("unknown delivery {value:?}; expected \"steer\" or \"queue\""))
    }

    /// Attaches this delivery to a typed prompt request's `_meta`.
    ///
    /// Used by `ee-agent-host`, whose SDK request types cannot carry a
    /// top-level extension field.
    pub fn attach_to_meta(self, meta: Option<Meta>) -> Meta {
        let mut meta = meta.unwrap_or_default();
        meta.insert("delivery".to_string(), Value::String(self.as_str().to_string()));
        meta
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn missing_delivery_defaults_to_steer() {
        assert_eq!(
            PromptDelivery::from_prompt_params(&json!({ "sessionId": "s", "prompt": [] })),
            Ok(PromptDelivery::Steer)
        );
    }

    #[test]
    fn top_level_delivery_wins_over_meta() {
        let params = json!({
            "delivery": "queue",
            "_meta": { "delivery": "steer" },
        });
        assert_eq!(PromptDelivery::from_prompt_params(&params), Ok(PromptDelivery::Queue));
    }

    #[test]
    fn meta_delivery_is_accepted_as_fallback() {
        let params = json!({ "_meta": { "delivery": "queue" } });
        assert_eq!(PromptDelivery::from_prompt_params(&params), Ok(PromptDelivery::Queue));
    }

    #[test]
    fn unknown_delivery_is_rejected_with_the_value_named() {
        let error = PromptDelivery::from_prompt_params(&json!({ "delivery": "later" }))
            .expect_err("unknown delivery rejected");
        assert!(error.contains("later"), "error names the value: {error}");
    }

    #[test]
    fn attach_to_meta_preserves_existing_entries() {
        let mut meta = Meta::new();
        meta.insert("trace".to_string(), json!("t-1"));
        let meta = PromptDelivery::Queue.attach_to_meta(Some(meta));
        assert_eq!(meta.get("trace"), Some(&json!("t-1")));
        assert_eq!(meta.get("delivery"), Some(&json!("queue")));
    }
}
