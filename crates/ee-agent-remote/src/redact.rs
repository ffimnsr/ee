//! Remote-boundary redaction choke point.
//!
//! Every outbound notification, approval payload, tool result, history page,
//! and dispatch response passes through this module. It delegates to
//! [`ee_agent_host::redact`] — the same implementation the editor uses — so
//! there is exactly one redaction policy in the tree.
//!
//! Posture: false positives are acceptable, false negatives are not. A string
//! that merely looks like an assignment to a secret-like key is replaced even
//! when it is legitimate prose. Secrets are stripped before ring-buffer
//! insertion, so cursor replays are already clean.

use std::sync::Arc;

use ee_agent_host::redact::{is_secret_key, redact_secret_values, redact_sensitive_text};
use serde_json::Value;

/// Redactor carrying the known secret values captured at config resolution.
///
/// Configured env values are exact-match secrets that generic shape detection
/// cannot infer, so they are applied before the free-form pass. The set is
/// immutable after construction and shared by every surface of one host.
#[derive(Clone, Default)]
pub struct Redactor {
    secrets: Arc<[String]>,
}

impl std::fmt::Debug for Redactor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Redactor").field("secret_count", &self.secrets.len()).finish()
    }
}

impl Redactor {
    /// Builds a redactor from resolved secret values.
    #[must_use]
    pub fn new(secrets: Vec<String>) -> Self {
        let mut secrets = secrets;
        secrets.retain(|secret| !secret.is_empty());
        secrets.sort();
        secrets.dedup();
        Self { secrets: Arc::from(secrets) }
    }

    /// Redacts one free-form string: known values first, then credential
    /// shapes.
    #[must_use]
    pub fn text(&self, text: &str) -> String {
        redact_sensitive_text(&redact_secret_values(text, &self.secrets))
    }

    /// Recursively redacts a JSON value.
    ///
    /// - object values under secret-like keys become `"***"`;
    /// - every remaining string passes [`Self::text`];
    /// - arrays and nested objects recurse.
    #[must_use]
    pub fn value(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.text(text)),
            Value::Array(values) => {
                Value::Array(values.iter().map(|item| self.value(item)).collect())
            }
            Value::Object(entries) => Value::Object(
                entries
                    .iter()
                    .map(|(key, value)| {
                        let value = if is_secret_key(key) {
                            Value::String(String::from("***"))
                        } else {
                            self.value(value)
                        };
                        (key.clone(), value)
                    })
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

/// Redacts one free-form string without configured-value seeds.
#[must_use]
pub fn redact_text(text: &str) -> String {
    Redactor::default().text(text)
}

/// Recursively redacts a JSON value without configured-value seeds.
#[must_use]
pub fn redact_outbound(value: &Value) -> Value {
    Redactor::default().value(value)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Secret shapes seeded from `ee-chat-completions` cassette records
    /// (bearer headers, URL-embedded credentials, query secrets) plus the
    /// workspace `secret://` echo form.
    const SECRETS: [&str; 8] = [
        "sk-live-abcdef123456",
        "ghp_1234567890abcdefghij",
        "xoxb-1234567890-abcdef",
        "hunter2",
        "query-secret-value",
        "alice-password",
        "secret://prod/api-token",
        "eyJhbGciOiJIUzI1NiJ9.payload.signature",
    ];

    fn hostile_payload() -> Value {
        json!({
            "text": concat!(
                "Authorization: Bearer sk-live-abcdef123456\n",
                "token ghp_1234567890abcdefghij\n",
                "slack xoxb-1234567890-abcdef\n",
                "password=hunter2\n",
                "callback https://example.test/?access_token=query-secret-value\n",
                "fetch https://alice:alice-password@example.test/data\n",
                "reference secret://prod/api-token\n",
                "jwt eyJhbGciOiJIUzI1NiJ9.payload.signature\n"
            ),
            "rawInput": {
                "api_key": "sk-live-abcdef123456",
                "command": "curl -H 'Authorization: Bearer sk-live-abcdef123456'",
                "path": "/work/file.txt"
            },
            "headers": [
                { "authorization": "Bearer sk-live-abcdef123456" },
                { "x-trace": "safe" }
            ]
        })
    }

    #[test]
    fn no_known_secret_shape_survives_redaction() {
        let redacted = redact_outbound(&hostile_payload());
        let text = serde_json::to_string(&redacted).expect("serialize");
        for secret in SECRETS {
            assert!(!text.contains(secret), "leaked {secret}: {text}");
        }
        assert!(text.contains("/work/file.txt"), "safe content is preserved");
        assert!(text.contains("safe"));
    }

    #[test]
    fn configured_secret_values_never_survive() {
        let redactor = Redactor::new(vec![String::from("env-secret-value")]);
        let payload = json!({
            "text": "agent printed env-secret-value in its reply",
            "nested": ["env-secret-value"],
        });
        let text = serde_json::to_string(&redactor.value(&payload)).expect("serialize");
        assert!(!text.contains("env-secret-value"), "leaked configured secret: {text}");
        assert!(text.contains("agent printed"));
    }

    #[test]
    fn secret_like_keys_are_masked_whole() {
        let redacted = redact_outbound(&json!({ "token": "anything", "safe": "kept" }));
        assert_eq!(redacted["token"], "***");
        assert_eq!(redacted["safe"], "kept");
    }

    #[test]
    fn fixtures_cover_bearer_github_and_url_credentials() {
        for (input, needle) in [
            ("Authorization: Bearer abc.def", "abc.def"),
            ("use ghp_1234567890abcdefghij now", "ghp_1234567890abcdefghij"),
            ("https://user:pass@example.test/", "user:pass"),
        ] {
            let redacted = redact_text(input);
            assert!(!redacted.contains(needle), "leaked {needle} in {redacted}");
        }
    }
}
