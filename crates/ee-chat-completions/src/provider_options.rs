//! Typed OpenAI / Responses provider options.
//!
//! The reference agent exposes a small set of provider-defined knobs next to the
//! portable generation options (`packages/ai/src/providers/openai-options.ts`,
//! `packages/ai/src/protocols/open-responses.ts:819-844`). EE parses them once at
//! startup, validates every value, and renders the documented subset per route
//! instead of accepting an opaque JSON blob: a typo fails at startup with the
//! variable name, and a value that may carry user data is never logged.
//!
//! `Debug` redacts `ProviderOptions::metadata` and
//! `ProviderOptions::safety_identifier`, so a diagnostic can print the whole
//! option set without disclosing a user identifier. There is deliberately no
//! `Display` implementation.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::{Map, Value, json};

use crate::merge::deep_merge;

/// Largest accepted option JSON document, in bytes.
pub const MAX_PROVIDER_OPTIONS_BYTES: usize = 16 * 1024;
/// Largest accepted metadata entry count.
pub const MAX_METADATA_ENTRIES: usize = 16;
/// Longest accepted metadata key, in bytes.
pub const MAX_METADATA_KEY_BYTES: usize = 64;
/// Longest accepted metadata value, in bytes.
pub const MAX_METADATA_VALUE_BYTES: usize = 512;
/// Longest accepted identifier value (`service_tier`, `safety_identifier`).
pub const MAX_IDENTIFIER_BYTES: usize = 256;

/// Verified `text.verbosity` values.
pub const TEXT_VERBOSITY_VALUES: &[&str] = &["low", "medium", "high"];
/// Verified `reasoning.summary` values.
pub const REASONING_SUMMARY_VALUES: &[&str] = &["auto", "concise", "detailed"];
/// Verified `truncation` values.
pub const TRUNCATION_VALUES: &[&str] = &["auto", "disabled"];

/// Provider-defined request options that are not sampling knobs.
///
/// Every field is optional: an unset option emits nothing, so a route that does
/// not document the field is never sent it.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ProviderOptions {
    /// `service_tier` (Responses and Chat Completions).
    service_tier: Option<String>,
    /// `text.verbosity` (Responses only).
    text_verbosity: Option<String>,
    /// `reasoning.summary` (Responses only).
    reasoning_summary: Option<String>,
    /// `safety_identifier`; redacted in `Debug`.
    safety_identifier: Option<String>,
    /// `metadata`; values are redacted in `Debug`.
    metadata: Option<BTreeMap<String, String>>,
    /// `max_tool_calls` (Responses only).
    max_tool_calls: Option<u64>,
    /// `truncation` (Responses only).
    truncation: Option<String>,
}

impl ProviderOptions {
    /// No provider options configured.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Whether nothing is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.service_tier.is_none()
            && self.text_verbosity.is_none()
            && self.reasoning_summary.is_none()
            && self.safety_identifier.is_none()
            && self.metadata.as_ref().is_none_or(BTreeMap::is_empty)
            && self.max_tool_calls.is_none()
            && self.truncation.is_none()
    }

    /// Parses and validates the option document.
    ///
    /// Unknown keys are rejected rather than forwarded, so a typo cannot silently
    /// become a request field no provider documents.
    ///
    /// # Errors
    ///
    /// Returns a message naming `variable` and the offending key.
    pub fn parse_json(variable: &str, raw: &str) -> Result<Self, String> {
        if raw.len() > MAX_PROVIDER_OPTIONS_BYTES {
            return Err(format!("{variable} must be at most {MAX_PROVIDER_OPTIONS_BYTES} bytes"));
        }
        let value: Value = serde_json::from_str(raw)
            .map_err(|error| format!("{variable} is not valid JSON: {error}"))?;
        let Some(object) = value.as_object() else {
            return Err(format!("{variable} must be a JSON object"));
        };
        let mut options = Self::default();
        for (key, value) in object {
            match key.as_str() {
                "service_tier" => {
                    options.service_tier =
                        Some(identifier(variable, key, value, MAX_IDENTIFIER_BYTES)?)
                }
                "text_verbosity" => {
                    options.text_verbosity =
                        Some(one_of(variable, key, value, TEXT_VERBOSITY_VALUES)?)
                }
                "reasoning_summary" => {
                    options.reasoning_summary =
                        Some(one_of(variable, key, value, REASONING_SUMMARY_VALUES)?)
                }
                "safety_identifier" => {
                    options.safety_identifier = value
                        .as_str()
                        .map(str::to_string)
                        .filter(|value| !value.is_empty() && value.len() <= MAX_IDENTIFIER_BYTES);
                    if options.safety_identifier.is_none() {
                        return Err(format!(
                            "{variable}.{key} must be a non-empty string of at most \
                             {MAX_IDENTIFIER_BYTES} bytes"
                        ));
                    }
                }
                "metadata" => options.metadata = Some(metadata(variable, value)?),
                "max_tool_calls" => {
                    options.max_tool_calls = value.as_u64().filter(|value| *value > 0);
                    if options.max_tool_calls.is_none() {
                        return Err(format!("{variable}.{key} must be a positive integer"));
                    }
                }
                "truncation" => {
                    options.truncation = Some(one_of(variable, key, value, TRUNCATION_VALUES)?)
                }
                other => {
                    return Err(format!(
                        "{variable}.{other} is not a documented option: expected one of \
                         service_tier, text_verbosity, reasoning_summary, safety_identifier, \
                         metadata, max_tool_calls, truncation"
                    ));
                }
            }
        }
        Ok(options)
    }

    /// Renders the Chat Completions subset.
    ///
    /// Chat documents `service_tier`, `metadata`, and `safety_identifier`; the
    /// Responses-only fields are omitted so a strict endpoint never sees them.
    #[must_use]
    pub fn chat_fields(&self) -> Option<Value> {
        let mut object = Map::new();
        if let Some(tier) = &self.service_tier {
            object.insert(String::from("service_tier"), json!(tier));
        }
        if let Some(identifier) = &self.safety_identifier {
            object.insert(String::from("safety_identifier"), json!(identifier));
        }
        if let Some(metadata) = &self.metadata
            && !metadata.is_empty()
        {
            object.insert(String::from("metadata"), json!(metadata));
        }
        (!object.is_empty()).then_some(Value::Object(object))
    }

    /// Renders the Responses subset, including the nested `text` and `reasoning`
    /// fields.
    #[must_use]
    pub fn responses_fields(&self) -> Option<Value> {
        let mut object = Map::new();
        if let Some(tier) = &self.service_tier {
            object.insert(String::from("service_tier"), json!(tier));
        }
        if let Some(identifier) = &self.safety_identifier {
            object.insert(String::from("safety_identifier"), json!(identifier));
        }
        if let Some(metadata) = &self.metadata
            && !metadata.is_empty()
        {
            object.insert(String::from("metadata"), json!(metadata));
        }
        if let Some(calls) = self.max_tool_calls {
            object.insert(String::from("max_tool_calls"), json!(calls));
        }
        if let Some(truncation) = &self.truncation {
            object.insert(String::from("truncation"), json!(truncation));
        }
        if let Some(verbosity) = &self.text_verbosity {
            object.insert(String::from("text"), json!({ "verbosity": verbosity }));
        }
        if let Some(summary) = &self.reasoning_summary {
            object.insert(String::from("reasoning"), json!({ "summary": summary }));
        }
        (!object.is_empty()).then_some(Value::Object(object))
    }

    /// Merges the route subset into an extension object.
    ///
    /// The merge is deep, so a `reasoning.summary` lands next to an existing
    /// `reasoning.effort` instead of replacing it.
    pub fn merge_into(&self, extensions: &mut Value, responses: bool) {
        let fields = if responses { self.responses_fields() } else { self.chat_fields() };
        if let Some(fields) = fields {
            deep_merge(extensions, &fields);
        }
    }
}

impl fmt::Debug for ProviderOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderOptions")
            .field("service_tier", &self.service_tier)
            .field("text_verbosity", &self.text_verbosity)
            .field("reasoning_summary", &self.reasoning_summary)
            .field("safety_identifier", &self.safety_identifier.as_ref().map(|_| "[redacted]"))
            .field(
                "metadata",
                &self.metadata.as_ref().map(|metadata| {
                    metadata
                        .keys()
                        .map(|key| (key.as_str(), "[redacted]"))
                        .collect::<BTreeMap<_, _>>()
                }),
            )
            .field("max_tool_calls", &self.max_tool_calls)
            .field("truncation", &self.truncation)
            .finish()
    }
}

fn identifier(
    variable: &str,
    key: &str,
    value: &Value,
    max_bytes: usize,
) -> Result<String, String> {
    let Some(text) = value.as_str() else {
        return Err(format!("{variable}.{key} must be a string"));
    };
    let text = text.trim();
    if text.is_empty() || text.len() > max_bytes {
        return Err(format!("{variable}.{key} must be 1..={max_bytes} bytes"));
    }
    if !text
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.'))
    {
        return Err(format!(
            "{variable}.{key} must contain only letters, digits, '_', '-', or '.'"
        ));
    }
    Ok(text.to_string())
}

fn one_of(variable: &str, key: &str, value: &Value, allowed: &[&str]) -> Result<String, String> {
    let Some(text) = value.as_str() else {
        return Err(format!("{variable}.{key} must be a string"));
    };
    if allowed.contains(&text) {
        Ok(text.to_string())
    } else {
        Err(format!("{variable}.{key} must be one of {}", allowed.join(", ")))
    }
}

fn metadata(variable: &str, value: &Value) -> Result<BTreeMap<String, String>, String> {
    let Some(object) = value.as_object() else {
        return Err(format!("{variable}.metadata must be a JSON object"));
    };
    if object.len() > MAX_METADATA_ENTRIES {
        return Err(format!("{variable}.metadata accepts at most {MAX_METADATA_ENTRIES} entries"));
    }
    let mut entries = BTreeMap::new();
    for (key, value) in object {
        if key.is_empty() || key.len() > MAX_METADATA_KEY_BYTES {
            return Err(format!(
                "{variable}.metadata keys must be 1..={MAX_METADATA_KEY_BYTES} bytes"
            ));
        }
        let Some(text) = value.as_str() else {
            return Err(format!("{variable}.metadata.{key} must be a string"));
        };
        if text.len() > MAX_METADATA_VALUE_BYTES {
            return Err(format!(
                "{variable}.metadata.{key} must be at most {MAX_METADATA_VALUE_BYTES} bytes"
            ));
        }
        entries.insert(key.clone(), text.to_string());
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_document_emits_nothing() {
        let options = ProviderOptions::parse_json("X", "{}").expect("parse");
        assert!(options.is_empty());
        assert!(options.chat_fields().is_none());
        assert!(options.responses_fields().is_none());
    }

    #[test]
    fn chat_omits_responses_only_fields() {
        let options = ProviderOptions::parse_json(
            "X",
            r#"{"service_tier":"priority","text_verbosity":"low","truncation":"auto","max_tool_calls":4}"#,
        )
        .expect("parse");
        let chat = options.chat_fields().expect("chat fields");
        assert_eq!(chat, json!({ "service_tier": "priority" }));
        let responses = options.responses_fields().expect("responses fields");
        assert_eq!(
            responses,
            json!({
                "service_tier": "priority",
                "max_tool_calls": 4,
                "truncation": "auto",
                "text": { "verbosity": "low" }
            })
        );
    }

    #[test]
    fn unknown_keys_are_rejected_with_the_variable_name() {
        let error =
            ProviderOptions::parse_json("OPENROUTER_PROVIDER_OPTIONS_JSON", r#"{"junk":1}"#)
                .expect_err("rejected");
        assert!(error.contains("OPENROUTER_PROVIDER_OPTIONS_JSON.junk"), "{error}");
    }

    #[test]
    fn values_are_validated() {
        for document in [
            r#"{"text_verbosity":"shout"}"#,
            r#"{"truncation":"sometimes"}"#,
            r#"{"reasoning_summary":"verbose"}"#,
            r#"{"max_tool_calls":0}"#,
            r#"{"safety_identifier":""}"#,
            r#"{"metadata":{"k":1}}"#,
            r#"{"service_tier":"high tier"}"#,
        ] {
            assert!(ProviderOptions::parse_json("X", document).is_err(), "{document}");
        }
    }

    #[test]
    fn metadata_bounds_are_enforced() {
        let too_many = json!({
            "metadata": (0..MAX_METADATA_ENTRIES + 1)
                .map(|index| (format!("k{index}"), json!("v")))
                .collect::<Map<String, Value>>()
        });
        assert!(ProviderOptions::parse_json("X", &too_many.to_string()).is_err());
    }

    #[test]
    fn debug_redacts_user_data() {
        let options = ProviderOptions::parse_json(
            "X",
            r#"{"safety_identifier":"user-123","metadata":{"account":"acct-9"}}"#,
        )
        .expect("parse");
        let rendered = format!("{options:?}");
        assert!(!rendered.contains("user-123"), "{rendered}");
        assert!(!rendered.contains("acct-9"), "{rendered}");
        assert!(rendered.contains("account"), "{rendered}");
    }

    #[test]
    fn merge_into_keeps_sibling_keys() {
        let options =
            ProviderOptions::parse_json("X", r#"{"reasoning_summary":"concise"}"#).expect("parse");
        let mut extensions = json!({ "reasoning": { "effort": "high" } });
        options.merge_into(&mut extensions, true);
        assert_eq!(extensions, json!({ "reasoning": { "effort": "high", "summary": "concise" } }));
    }
}
