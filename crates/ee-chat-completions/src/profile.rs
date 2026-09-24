//! Provider-facing profile and credential handling.

use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderName, HeaderValue};
use serde_json::Value;

use crate::endpoint::TrustedEndpoint;

/// Bounded retry policy for transient and rate-limited failures.
///
/// `max_attempts` counts *retries* after the first attempt, so `0` disables
/// retrying. Delays are capped by `max_delay` regardless of what the server
/// asks for in `Retry-After`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Retries after the first attempt.
    pub max_attempts: u32,
    /// Initial backoff delay.
    pub base_delay: Duration,
    /// Upper bound for any single delay.
    pub max_delay: Duration,
}

impl RetryPolicy {
    /// Never retries.
    #[must_use]
    pub const fn none() -> Self {
        Self { max_attempts: 0, base_delay: Duration::ZERO, max_delay: Duration::ZERO }
    }
}

/// Bearer token used only for the `Authorization` header.
///
/// The [`std::fmt::Debug`] output is redacted and there is no
/// [`std::fmt::Display`] implementation, so a token cannot reach a transcript,
/// checkpoint, diagnostic, or error message by accident. Call
/// [`BearerToken::expose`] where the header is actually written.
#[derive(Clone, PartialEq, Eq)]
pub struct BearerToken(String);

impl BearerToken {
    /// Wraps a resolved token.
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// Exposes the token for header construction.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for BearerToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("BearerToken([redacted])")
    }
}

/// Resolves the bearer token for each round trip.
///
/// The error message is provider-owned, so a missing credential can name its
/// own environment variable and the setup step that fixes it.
pub type TokenSource = Arc<dyn Fn() -> Result<BearerToken, String> + Send + Sync>;

/// Everything a provider must supply for its Chat Completions endpoint.
///
/// Provider attribution headers, provider-specific request shaping, environment
/// variable names, and error wording live here rather than in the shared
/// transport: the transport never learns a provider name it was not given.
#[derive(Debug, Clone)]
pub struct EndpointProfile {
    /// Provider display label used in error messages, e.g. `OpenRouter`.
    pub label: String,
    /// Environment variable holding the token, used only in error text.
    pub credential_var: String,
    /// Validated endpoint; the only origin this profile may contact.
    pub endpoint: TrustedEndpoint,
    /// Model id sent in the request body.
    pub model: String,
    /// System prompt prepended to a normalized transcript.
    pub system_prompt: String,
    /// Request timeout applied by the HTTP client.
    pub timeout: Duration,
    /// Retry policy for transient and rate-limited failures.
    pub retry: RetryPolicy,
    /// Extension headers sent with every request; never credential headers.
    pub headers: Vec<(HeaderName, HeaderValue)>,
    /// Extension body fields merged into every request, e.g. reasoning shaping.
    pub extensions: Option<Value>,
}

impl EndpointProfile {
    /// Builds a profile with no system prompt, no retries, no extension headers
    /// or body fields, and a 60-second timeout.
    #[must_use]
    pub fn new(
        label: impl Into<String>,
        credential_var: impl Into<String>,
        endpoint: TrustedEndpoint,
        model: impl Into<String>,
    ) -> Self {
        Self {
            label: label.into(),
            credential_var: credential_var.into(),
            endpoint,
            model: model.into(),
            system_prompt: String::new(),
            timeout: Duration::from_secs(60),
            retry: RetryPolicy::none(),
            headers: Vec::new(),
            extensions: None,
        }
    }

    /// Sets the system prompt prepended to normalized transcripts.
    #[must_use]
    pub fn with_system_prompt(mut self, system_prompt: impl Into<String>) -> Self {
        self.system_prompt = system_prompt.into();
        self
    }

    /// Sets the request timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets the retry policy.
    #[must_use]
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Sets provider extension headers.
    #[must_use]
    pub fn with_headers(mut self, headers: Vec<(HeaderName, HeaderValue)>) -> Self {
        self.headers = headers;
        self
    }

    /// Sets provider extension body fields; must be a JSON object.
    #[must_use]
    pub fn with_extensions(mut self, extensions: Value) -> Self {
        self.extensions = Some(extensions);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_token_debug_is_redacted_and_there_is_no_display() {
        let token = BearerToken::new("sk-secret-value");

        assert_eq!(token.expose(), "sk-secret-value");
        assert_eq!(format!("{token:?}"), "BearerToken([redacted])");
        assert!(!format!("{token:?}").contains("sk-secret-value"));
    }

    #[test]
    fn retry_policy_none_disables_retries() {
        assert_eq!(RetryPolicy::none().max_attempts, 0);
    }

    #[test]
    fn profile_builder_keeps_only_provider_supplied_values() {
        let profile = EndpointProfile::new(
            "Example",
            "EXAMPLE_API_KEY",
            TrustedEndpoint::parse("https://example.test/v1/chat/completions").expect("https"),
            "example-model",
        )
        .with_system_prompt("system")
        .with_timeout(Duration::from_secs(5))
        .with_retry(RetryPolicy {
            max_attempts: 2,
            base_delay: Duration::from_millis(10),
            max_delay: Duration::from_secs(1),
        })
        .with_extensions(serde_json::json!({ "reasoning": { "effort": "low" } }));

        assert_eq!(profile.label, "Example");
        assert_eq!(profile.credential_var, "EXAMPLE_API_KEY");
        assert_eq!(profile.model, "example-model");
        assert_eq!(profile.system_prompt, "system");
        assert_eq!(profile.timeout, Duration::from_secs(5));
        assert_eq!(profile.retry.max_attempts, 2);
        assert_eq!(profile.headers.len(), 0);
        assert_eq!(profile.extensions.expect("extensions")["reasoning"]["effort"], "low");
    }
}
