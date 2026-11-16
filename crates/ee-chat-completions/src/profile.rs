//! Provider-facing profile and credential handling.

use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderName, HeaderValue};
use serde_json::Value;

use ee_agent_orchestrator::{ModelLimits, OUTPUT_LIMIT_MAX, fit_output_limit};

use crate::endpoint::TrustedEndpoint;

/// Body field carrying the output-token bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MaxTokensField {
    /// `max_tokens`, the historical OpenAI-compatible field.
    #[default]
    MaxTokens,
    /// `max_completion_tokens`, required by newer OpenAI-family upstreams.
    MaxCompletionTokens,
}

impl MaxTokensField {
    /// Wire field name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MaxTokens => "max_tokens",
            Self::MaxCompletionTokens => "max_completion_tokens",
        }
    }

    /// Parses the configured field name.
    ///
    /// # Errors
    ///
    /// Returns guidance naming the accepted values.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim() {
            "max_tokens" => Ok(Self::MaxTokens),
            "max_completion_tokens" => Ok(Self::MaxCompletionTokens),
            other => Err(format!(
                "unsupported output-token field {other:?}: expected \"max_tokens\" or \"max_completion_tokens\""
            )),
        }
    }
}

/// Bounded retry policy for transient and rate-limited failures.
///
/// `max_attempts` counts *retries* after the first attempt, so `0` disables
/// retrying. Delays are capped by `max_delay` regardless of what the server
/// asks for in `Retry-After`, and `retry_after_max` bounds a server hint
/// separately so a hint never shortens a retry below the backoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Retries after the first attempt.
    pub max_attempts: u32,
    /// Initial backoff delay.
    pub base_delay: Duration,
    /// Upper bound for any single backoff delay.
    pub max_delay: Duration,
    /// Upper bound for a server-provided `Retry-After` hint (reference: 15 min).
    pub retry_after_max: Duration,
    /// Budget for retries that never reached the provider (reference: 1 hour).
    pub unreachable_window: Duration,
    /// Extra retries allowed for timed-out attempts (reference: 3).
    pub max_timeout_retries: u32,
}

impl RetryPolicy {
    /// Never retries.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            max_attempts: 0,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            retry_after_max: Duration::ZERO,
            unreachable_window: Duration::ZERO,
            max_timeout_retries: 0,
        }
    }

    /// Retry policy with the documented defaults for the server-hint cap,
    /// unreachable-network budget, and timeout allowance.
    #[must_use]
    pub const fn new(max_attempts: u32, base_delay: Duration, max_delay: Duration) -> Self {
        Self {
            max_attempts,
            base_delay,
            max_delay,
            retry_after_max: Duration::from_secs(15 * 60),
            unreachable_window: Duration::from_secs(60 * 60),
            max_timeout_retries: 3,
        }
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
    /// Emit Anthropic-style `cache_control` breakpoints on stable prompt
    /// planes. Opt-in because only Anthropic-compatible routes understand the
    /// marker; text is never rewritten.
    pub prompt_cache: bool,
    /// Catalog-declared context/output limits, when known.
    pub limits: Option<ModelLimits>,
    /// Explicit output-token override; still clamped to the catalog ceiling.
    pub max_output_tokens: Option<u64>,
    /// Request usage in the stream with `stream_options.include_usage`.
    pub stream_usage: bool,
    /// Body field carrying the output-token bound.
    pub max_tokens_field: MaxTokensField,
    /// Assistant field a route requires for replayed reasoning when the provider
    /// streamed only structured details (for example Kimi's `reasoning_content`).
    pub reasoning_field: Option<String>,
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
            prompt_cache: false,
            limits: None,
            max_output_tokens: None,
            stream_usage: false,
            max_tokens_field: MaxTokensField::MaxTokens,
            reasoning_field: None,
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

    /// Enables Anthropic-style prompt-cache breakpoints for this route.
    #[must_use]
    pub fn with_prompt_cache(mut self, enabled: bool) -> Self {
        self.prompt_cache = enabled;
        self
    }

    /// Sets the catalog-declared model limits.
    #[must_use]
    pub fn with_limits(mut self, limits: Option<ModelLimits>) -> Self {
        self.limits = limits;
        self
    }

    /// Sets an explicit output-token bound, clamped to the catalog ceiling when
    /// one is known.
    #[must_use]
    pub fn with_max_output_tokens(mut self, max_output_tokens: Option<u64>) -> Self {
        self.max_output_tokens = max_output_tokens;
        self
    }

    /// Requests usage in streamed responses.
    #[must_use]
    pub fn with_stream_usage(mut self, enabled: bool) -> Self {
        self.stream_usage = enabled;
        self
    }

    /// Selects the body field carrying the output-token bound.
    #[must_use]
    pub fn with_max_tokens_field(mut self, field: MaxTokensField) -> Self {
        self.max_tokens_field = field;
        self
    }

    /// Selects the assistant field a route requires for replayed reasoning.
    #[must_use]
    pub fn with_reasoning_field(mut self, field: Option<String>) -> Self {
        self.reasoning_field = field;
        self
    }

    /// Output-token bound fitted to the prompt and catalog window.
    ///
    /// An explicit override is clamped to the catalog output ceiling when one
    /// is known; the prompt estimate then leaves the documented margin. When no
    /// catalog limit exists, only an explicit override is returned, so callers
    /// omit the field rather than guess.
    #[must_use]
    pub fn fitted_output_limit(&self, prompt_tokens: u64) -> Option<u64> {
        let Some(limits) = self.limits else {
            return self.max_output_tokens;
        };
        let ceiling = limits.output.clamp(1, OUTPUT_LIMIT_MAX);
        let requested = self.max_output_tokens.map_or(ceiling, |value| value.min(ceiling));
        fit_output_limit(Some(ModelLimits::new(limits.context, requested)), Some(prompt_tokens))
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
            ..RetryPolicy::none()
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
