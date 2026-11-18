//! Provider-facing profile and credential handling.

use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderName, HeaderValue};
use serde_json::Value;

use ee_agent_orchestrator::{
    GenerationOptions, Modalities, ModelCost, ModelLimits, OUTPUT_LIMIT_MAX, SamplingSupport,
    fit_output_limit,
};

use crate::cache_policy::{CacheDirective, CachePolicy};
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

/// Per-phase HTTP budgets for one endpoint.
///
/// The reference agent bounds each phase separately: a `headers` budget until
/// the response starts, an idle `chunk` budget between streamed events, and an
/// optional `total` budget for the whole exchange
/// (`packages/ai/src/schema/options.ts:44-56`). A single total timeout cannot
/// express a long reasoning stream: some providers stay silent for many minutes
/// while they think.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// Whole exchange, from send until the body is fully read; `None` is
    /// unbounded.
    pub total: Option<Duration>,
    /// Budget until response headers arrive.
    pub headers: Duration,
    /// Budget between streamed response chunks.
    pub chunk: Duration,
}

impl Timeouts {
    /// Reference defaults: 300 s to headers, 900 s between chunks, unbounded
    /// total.
    pub const DEFAULT_HEADERS: Duration = Duration::from_secs(300);
    /// Reference default idle budget between streamed chunks.
    pub const DEFAULT_CHUNK: Duration = Duration::from_secs(900);

    /// Reference defaults with no total budget.
    #[must_use]
    pub const fn new() -> Self {
        Self { total: None, headers: Self::DEFAULT_HEADERS, chunk: Self::DEFAULT_CHUNK }
    }

    /// Sets the whole-exchange budget; `None` keeps it unbounded.
    #[must_use]
    pub const fn with_total(mut self, total: Option<Duration>) -> Self {
        self.total = total;
        self
    }

    /// Sets the header budget.
    #[must_use]
    pub const fn with_headers(mut self, headers: Duration) -> Self {
        self.headers = headers;
        self
    }

    /// Sets the idle chunk budget.
    #[must_use]
    pub const fn with_chunk(mut self, chunk: Duration) -> Self {
        self.chunk = chunk;
        self
    }
}

impl Default for Timeouts {
    fn default() -> Self {
        Self::new()
    }
}

/// Compression applied to a JSON request body.
///
/// Off by default: only set it for an endpoint known to decode it. The value is
/// validated at startup so an unknown encoding never becomes a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RequestEncoding {
    /// Plain JSON.
    #[default]
    None,
    /// `Content-Encoding: gzip`.
    Gzip,
    /// `Content-Encoding: zstd`.
    Zstd,
}

impl RequestEncoding {
    /// Parses the configured encoding name.
    ///
    /// # Errors
    ///
    /// Returns guidance naming the accepted values.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" => Ok(Self::None),
            "gzip" => Ok(Self::Gzip),
            "zstd" => Ok(Self::Zstd),
            other => Err(format!(
                "unsupported request encoding {other:?}: expected \"none\", \"gzip\", or \"zstd\""
            )),
        }
    }

    /// `Content-Encoding` value, or `None` when the body is sent plain.
    #[must_use]
    pub const fn content_encoding(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Gzip => Some("gzip"),
            Self::Zstd => Some("zstd"),
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

/// Request-scoped knobs that travel together into a streaming codec call.
#[derive(Debug, Clone, Copy, Default)]
pub struct CallOptions<'a> {
    /// Sampling knobs to write; `None` keeps the profile default.
    pub generation: Option<&'a ee_agent_orchestrator::GenerationOptions>,
    /// Session affinity for headers and the prompt-cache key.
    pub session: Option<&'a ee_agent_orchestrator::SessionContext>,
    /// Provider-side continuation anchor for stateful Responses routes.
    pub continuation: Option<&'a ee_agent_orchestrator::Continuation>,
}

/// `tool_choice` values a route honours (`auto`, `none`, `required`, `tool`).
///
/// Catalog data, never a model-id heuristic. An empty list means the protocol
/// default applies, so an unadvertised route keeps today's bytes.
pub type ToolChoices = &'static [&'static str];

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
    /// Per-phase HTTP budgets applied by the HTTP client and the SSE reader.
    pub timeouts: Timeouts,
    /// Query parameters appended to the endpoint, percent-encoded exactly once.
    pub query: Vec<(String, String)>,
    /// Compression applied to the JSON request body.
    pub encoding: RequestEncoding,
    /// Retry policy for transient and rate-limited failures.
    pub retry: RetryPolicy,
    /// Extension headers sent with every request; never credential headers.
    pub headers: Vec<(HeaderName, HeaderValue)>,
    /// Extension body fields merged into every request, e.g. reasoning shaping.
    pub extensions: Option<Value>,
    /// Prompt-cache policy plus the route capabilities that gate it.
    pub cache: CacheDirective,
    /// Whether tool schemas accept the `strict` flag on this route.
    pub tools_strict: bool,
    /// Whether the catalog declares native structured-output support. A route
    /// without it rejects a `response_format` request before sending.
    pub structured_output: bool,
    /// Free-form tool input formats this route advertises (`text`, `lark`,
    /// `regex`); empty when it advertises none.
    pub freeform: Vec<String>,
    /// Catalog-declared context/output limits, when known.
    pub limits: Option<ModelLimits>,
    /// Catalog prices used when the provider reports tokens but no cost.
    pub cost: Option<ModelCost>,
    /// Catalog-declared input modalities, used to refuse undeclared media before
    /// a request exists.
    pub modalities: Modalities,
    /// Explicit output-token override; still clamped to the catalog ceiling.
    pub max_output_tokens: Option<u64>,
    /// Request usage in the stream with `stream_options.include_usage`.
    pub stream_usage: bool,
    /// Body field carrying the output-token bound.
    pub max_tokens_field: MaxTokensField,
    /// Assistant field a route requires for replayed reasoning when the provider
    /// streamed only structured details (for example Kimi's `reasoning_content`).
    pub reasoning_field: Option<String>,
    /// Sampling knobs the model documents.
    pub sampling: SamplingSupport,
    /// Session-default sampling knobs; a per-request value wins knob by knob.
    pub generation: GenerationOptions,
    /// Whether session affinity headers are emitted when a request carries a
    /// [`SessionContext`](ee_agent_orchestrator::SessionContext).
    pub session_headers: bool,
    /// Header naming dialect for session ids.
    pub session_header_style: SessionHeaderStyle,
    /// `User-Agent` sent with every request; binary name and version only.
    pub user_agent: String,
    /// Whether the route stores responses server-side, enabling
    /// `previous_response_id` continuation. Off by default: a route that does
    /// not advertise state must never receive a continuation anchor.
    pub store_responses: bool,
    /// Emit raw SSE frames as `StreamDelta::Raw`. Debug-only: frames may carry
    /// user data, so this stays off unless a caller opts in.
    pub raw_stream_events: bool,
    /// Whether the catalog advertises provider-native compaction on this route.
    /// Off by default: an unadvertised route must never receive a `/compact`
    /// request.
    pub compaction: bool,
    /// Opt-in header sent only when the request body carries an image part.
    /// OpenCode uses this to enable its image processing lane without changing
    /// text-only request bytes.
    pub image_opt_in_header: Option<HeaderName>,
    /// Capability flags that change request assembly: the `tool_choice` values
    /// the route accepts. Defaults (empty) send the protocol default.
    pub tool_choices: ToolChoices,
}

/// How provider-specific session header names are spelled.
///
/// `OpenCode` copies the reference agent's `x-opencode-*` names exactly because
/// OpenCode Zen keys caches and rate limits on them. `Ee` keeps the identical
/// semantics under `x-ee-*` names so we never impersonate OpenCode on a
/// provider that is not OpenCode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionHeaderStyle {
    /// `x-opencode-*` session headers.
    OpenCode,
    /// `x-ee-*` session headers (default).
    #[default]
    Ee,
}

/// Returns an actionable message when `control` names a `tool_choice` the
/// route does not advertise.
///
/// Shared by every dialect so a Messages route validates the same rule as a
/// Chat or Responses route. `choices` empty means the catalog declares nothing
/// and the protocol default applies; no tools means the codecs omit the field.
#[must_use]
pub fn tool_choice_error(
    model: &str,
    choices: &[&str],
    control: &ee_agent_orchestrator::ToolCallControl,
    has_tools: bool,
) -> Option<String> {
    if choices.is_empty() || !has_tools {
        return None;
    }
    let requested = match &control.choice {
        ee_agent_orchestrator::ToolChoice::Auto => "auto",
        ee_agent_orchestrator::ToolChoice::None => "none",
        ee_agent_orchestrator::ToolChoice::Required => "required",
        ee_agent_orchestrator::ToolChoice::Tool { .. } => "tool",
    };
    if choices.contains(&requested) {
        return None;
    }
    Some(format!(
        "model `{model}` does not accept tool_choice `{requested}`; its catalog entry declares: {}",
        choices.join(", ")
    ))
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
            timeouts: Timeouts::new(),
            query: Vec::new(),
            encoding: RequestEncoding::None,
            retry: RetryPolicy::none(),
            headers: Vec::new(),
            extensions: None,
            cache: CacheDirective::disabled(),
            tools_strict: false,
            structured_output: false,
            freeform: Vec::new(),
            limits: None,
            cost: None,
            modalities: Modalities::UNKNOWN,
            max_output_tokens: None,
            stream_usage: false,
            max_tokens_field: MaxTokensField::MaxTokens,
            reasoning_field: None,
            sampling: SamplingSupport::ALL,
            generation: GenerationOptions::none(),
            session_headers: true,
            session_header_style: SessionHeaderStyle::default(),
            user_agent: format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
            store_responses: false,
            raw_stream_events: false,
            compaction: false,
            image_opt_in_header: None,
            tool_choices: &[],
        }
    }

    /// Declares the `tool_choice` values the route honours.
    #[must_use]
    pub fn with_tool_choices(mut self, choices: ToolChoices) -> Self {
        self.tool_choices = choices;
        self
    }

    /// Returns an actionable message when `control` names a `tool_choice` the
    /// catalog does not advertise for this route.
    ///
    /// `None` means the request may be sent: either the route declares nothing
    /// (protocol default) or it declares the requested value. With no tools the
    /// codecs omit the field entirely, so there is nothing to validate.
    #[must_use]
    pub fn tool_choice_error(
        &self,
        control: &ee_agent_orchestrator::ToolCallControl,
        has_tools: bool,
    ) -> Option<String> {
        tool_choice_error(&self.model, self.tool_choices, control, has_tools)
    }

    /// Sets the header sent only when the request carries an image part.
    #[must_use]
    pub fn with_image_opt_in_header(mut self, header: Option<HeaderName>) -> Self {
        self.image_opt_in_header = header;
        self
    }

    /// Declares whether the route stores responses for continuation.
    #[must_use]
    pub fn with_store_responses(mut self, store: bool) -> Self {
        self.store_responses = store;
        self
    }

    /// Enables raw stream-frame emission for debugging.
    #[must_use]
    pub fn with_raw_stream_events(mut self, enabled: bool) -> Self {
        self.raw_stream_events = enabled;
        self
    }

    /// Declares whether the route advertises provider-native compaction.
    #[must_use]
    pub fn with_compaction(mut self, enabled: bool) -> Self {
        self.compaction = enabled;
        self
    }

    /// Enables or disables session affinity headers.
    #[must_use]
    pub fn with_session_headers(mut self, enabled: bool) -> Self {
        self.session_headers = enabled;
        self
    }

    /// Sets the session header naming dialect.
    #[must_use]
    pub fn with_session_header_style(mut self, style: SessionHeaderStyle) -> Self {
        self.session_header_style = style;
        self
    }

    /// Sets the `User-Agent` value; callers pass binary name and version only.
    #[must_use]
    pub fn with_user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.user_agent = user_agent.into();
        self
    }

    /// Sets the system prompt prepended to normalized transcripts.
    #[must_use]
    pub fn with_system_prompt(mut self, system_prompt: impl Into<String>) -> Self {
        self.system_prompt = system_prompt.into();
        self
    }

    /// Sets the whole-exchange timeout.
    ///
    /// Kept as the short form of [`Self::with_timeouts`]: it only replaces the
    /// total budget, so the phase defaults stay in force.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeouts = self.timeouts.with_total(Some(timeout));
        self
    }

    /// Sets the per-phase HTTP budgets.
    #[must_use]
    pub fn with_timeouts(mut self, timeouts: Timeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    /// Sets query parameters appended to the endpoint.
    ///
    /// Duplicate keys keep the first value; the transport percent-encodes each
    /// pair exactly once, so a value cannot inject another parameter.
    #[must_use]
    pub fn with_query(mut self, query: Vec<(String, String)>) -> Self {
        let mut seen = std::collections::BTreeSet::new();
        self.query = query.into_iter().filter(|(key, _)| seen.insert(key.clone())).collect();
        self
    }

    /// Sets the request-body compression.
    #[must_use]
    pub fn with_encoding(mut self, encoding: RequestEncoding) -> Self {
        self.encoding = encoding;
        self
    }

    /// Sets the sampling knobs the model documents.
    #[must_use]
    pub fn with_sampling(mut self, sampling: SamplingSupport) -> Self {
        self.sampling = sampling;
        self
    }

    /// Sets the session-default sampling knobs.
    #[must_use]
    pub fn with_generation(mut self, generation: GenerationOptions) -> Self {
        self.generation = generation;
        self
    }

    /// Returns the first configured sampling knob the model does not document.
    #[must_use]
    pub fn unsupported_generation(&self) -> Option<&'static str> {
        self.generation.unsupported(self.sampling)
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

    /// Sets the prompt-cache policy and route capabilities.
    #[must_use]
    pub fn with_cache(mut self, cache: CacheDirective) -> Self {
        self.cache = cache;
        self
    }

    /// Enables or disables the reference automatic cache policy.
    ///
    /// `enabled` keeps the route's declared capabilities and only switches the
    /// placement policy, so a route without explicit caching still emits
    /// nothing.
    #[must_use]
    pub fn with_prompt_cache(mut self, enabled: bool) -> Self {
        self.cache.policy = if enabled { CachePolicy::Auto } else { CachePolicy::None };
        self
    }

    /// Sets whether tool schemas accept the `strict` flag.
    #[must_use]
    pub fn with_tools_strict(mut self, strict: bool) -> Self {
        self.tools_strict = strict;
        self
    }

    /// Declares whether the catalog advertises native structured output.
    #[must_use]
    pub fn with_structured_output(mut self, supported: bool) -> Self {
        self.structured_output = supported;
        self
    }

    /// Sets the free-form tool input formats the route advertises.
    #[must_use]
    pub fn with_freeform(mut self, freeform: Vec<String>) -> Self {
        self.freeform = freeform;
        self
    }

    /// Sets the catalog-declared model limits.
    #[must_use]
    pub fn with_limits(mut self, limits: Option<ModelLimits>) -> Self {
        self.limits = limits;
        self
    }

    /// Sets the catalog prices used as a cost fallback.
    #[must_use]
    pub fn with_cost(mut self, cost: Option<ModelCost>) -> Self {
        self.cost = cost;
        self
    }

    /// Sets the catalog-declared input modalities.
    #[must_use]
    pub fn with_modalities(mut self, modalities: Modalities) -> Self {
        self.modalities = modalities;
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

    /// Output-token bound for one call, where a per-request value wins over the
    /// profile default while still respecting the catalog ceiling.
    #[must_use]
    pub fn fitted_output_limit_with(
        &self,
        prompt_tokens: u64,
        requested: Option<u64>,
    ) -> Option<u64> {
        match (requested, self.max_output_tokens) {
            (Some(requested), _) => {
                let Some(limits) = self.limits else {
                    return Some(requested.clamp(1, OUTPUT_LIMIT_MAX));
                };
                let ceiling = limits.output.clamp(1, OUTPUT_LIMIT_MAX);
                fit_output_limit(
                    Some(ModelLimits::new(limits.context, requested.min(ceiling))),
                    Some(prompt_tokens),
                )
            }
            (None, _) => self.fitted_output_limit(prompt_tokens),
        }
    }
}

#[cfg(test)]
mod extra_tests;

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
        assert_eq!(profile.timeouts.total, Some(Duration::from_secs(5)));
        assert_eq!(profile.retry.max_attempts, 2);
        assert_eq!(profile.headers.len(), 0);
        assert_eq!(profile.extensions.expect("extensions")["reasoning"]["effort"], "low");
    }
}
