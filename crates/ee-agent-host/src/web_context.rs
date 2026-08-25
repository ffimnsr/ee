//! Safe, bounded retrieval primitives for optional agent web context.
//!
//! This module deliberately has no default network transport. Callers provide a
//! transport which resolves DNS and reports its connected peer. That proof is
//! required to make DNS-rebinding checks testable and enforceable.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::Mutex,
    time::{Duration, Instant, SystemTime},
};

use base64::Engine;
use futures::{StreamExt, future::BoxFuture};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use url::{Host, Url};
use zeroize::Zeroizing;

/// Maximum redirects allowed by this service, regardless of configuration.
pub const MAX_REDIRECTS: usize = 3;
/// Maximum search results accepted from a configured service.
pub const MAX_SEARCH_RESULTS: usize = 50;
/// Maximum response size accepted by the service configuration.
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Maximum extracted text size accepted by the service configuration.
pub const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;

const DEFAULT_RESPONSE_BYTES: usize = 1024 * 1024;
const DEFAULT_TEXT_BYTES: usize = 256 * 1024;
const DEFAULT_SEARCH_RESULTS: usize = 10;
const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 2;
/// Default total Cloudflare Browser Run attempts, including the first request.
pub const DEFAULT_BROWSER_RUN_MAX_ATTEMPTS: u8 = 3;
/// Default exponential retry delay before the first retry.
pub const DEFAULT_BROWSER_RUN_BASE_DELAY_MS: u64 = 500;
/// Default upper bound for one Browser Run retry delay.
pub const DEFAULT_BROWSER_RUN_MAX_DELAY_MS: u64 = 10_000;
/// Hard ceiling for all Browser Run attempts, including the first request.
pub const MAX_BROWSER_RUN_ATTEMPTS: u8 = 5;
/// Hard ceiling for one Browser Run retry delay.
pub const MAX_BROWSER_RUN_DELAY_MS: u64 = 30_000;
/// Hard ceiling on one outbound web request timeout.
pub const MAX_REQUEST_TIMEOUT_MS: u64 = 120_000;
/// Hard ceiling on concurrent requests from one agent pane.
pub const MAX_CONCURRENT_REQUESTS: usize = 8;
const MAX_TITLE_BYTES: usize = 256;
const MAX_SNIPPET_BYTES: usize = 1024;
/// Maximum normalized search query size accepted before any cache or network work.
pub const MAX_SEARCH_QUERY_BYTES: usize = 4096;
/// Maximum Exa results accepted from trusted configuration.
pub const MAX_EXA_RESULTS: usize = 50;
/// Maximum Tavily results accepted from trusted configuration.
pub const MAX_TAVILY_RESULTS: usize = 50;
/// Maximum Tavily chunks per source accepted from trusted configuration.
pub const MAX_TAVILY_CHUNKS_PER_SOURCE: usize = 3;
/// Maximum Brave cited results accepted from trusted configuration.
pub const MAX_BRAVE_RESULTS: usize = 20;
/// Maximum Brave grounding-token budget accepted from trusted configuration.
pub const MAX_BRAVE_TOKENS: usize = 10_000;
/// Maximum Brave cited URLs accepted from trusted configuration.
pub const MAX_BRAVE_URLS: usize = 20;
/// Maximum Brave grounding snippets accepted from trusted configuration.
pub const MAX_BRAVE_SNIPPETS: usize = 20;
const WEB_CACHE_MAX_ENTRIES: usize = 32;
const WEB_CACHE_MAX_BYTES: usize = 1024 * 1024;
const WEB_CACHE_MAX_ENTRY_BYTES: usize = 256 * 1024;
const WEB_CACHE_TTL: Duration = Duration::from_secs(60);
const WEB_CACHE_STALE_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_CACHE_VALIDATOR_BYTES: usize = 1024;

/// Fixed vendor search endpoints. They are deliberately not configurable.
pub const EXA_SEARCH_ENDPOINT: &str = "https://api.exa.ai/search";
pub const BRAVE_LLM_CONTEXT_ENDPOINT: &str = "https://api.search.brave.com/res/v1/llm/context";
pub const TAVILY_SEARCH_ENDPOINT: &str = "https://api.tavily.com/search";

/// Trusted web-search provider selection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchProvider {
    #[default]
    Searxng,
    Exa,
    BraveLlmContext,
    Tavily,
}

impl WebSearchProvider {
    /// Stable provider id retained in bounded search provenance.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Searxng => "searxng",
            Self::Exa => "exa",
            Self::BraveLlmContext => "brave_llm_context",
            Self::Tavily => "tavily",
        }
    }

    /// Human-readable provider label safe for approval UI.
    pub const fn approval_label(self) -> &'static str {
        match self {
            Self::Searxng => "SearXNG",
            Self::Exa => "Exa",
            Self::BraveLlmContext => "Brave LLM Context",
            Self::Tavily => "Tavily",
        }
    }
}

/// Exa search mode. The adapter maps these stable semantic values to vendor JSON.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExaSearchMode {
    #[default]
    Auto,
    Neural,
    Fast,
}

/// Trusted Exa semantic options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExaSearchOptions {
    pub search_mode: ExaSearchMode,
    pub max_results: usize,
}

impl Default for ExaSearchOptions {
    fn default() -> Self {
        Self { search_mode: ExaSearchMode::Auto, max_results: DEFAULT_SEARCH_RESULTS }
    }
}

/// Tavily search depth. Richer tools remain out of scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TavilySearchDepth {
    Basic,
    #[default]
    Advanced,
}

/// Trusted Tavily semantic options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TavilySearchOptions {
    pub search_depth: TavilySearchDepth,
    pub max_results: usize,
    pub chunks_per_source: usize,
}

impl Default for TavilySearchOptions {
    fn default() -> Self {
        Self {
            search_depth: TavilySearchDepth::Advanced,
            max_results: DEFAULT_SEARCH_RESULTS,
            chunks_per_source: MAX_TAVILY_CHUNKS_PER_SOURCE,
        }
    }
}

/// Brave relevance threshold mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BraveThresholdMode {
    #[default]
    Balanced,
    Strict,
}

/// Brave freshness restriction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BraveFreshness {
    #[default]
    Any,
    Day,
    Week,
    Month,
}

/// Brave safe-search restriction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BraveSafeSearchMode {
    Off,
    #[default]
    Moderate,
    Strict,
}

/// Trusted Brave LLM Context semantic options. Local recall is intentionally absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BraveLlmContextOptions {
    pub max_results: usize,
    pub max_tokens: usize,
    pub max_urls: usize,
    pub max_snippets: usize,
    pub threshold_mode: BraveThresholdMode,
    pub freshness: BraveFreshness,
    pub safe_search: BraveSafeSearchMode,
}

impl Default for BraveLlmContextOptions {
    fn default() -> Self {
        Self {
            max_results: DEFAULT_SEARCH_RESULTS.min(MAX_BRAVE_RESULTS),
            max_tokens: 4_000,
            max_urls: DEFAULT_SEARCH_RESULTS.min(MAX_BRAVE_URLS),
            max_snippets: DEFAULT_SEARCH_RESULTS.min(MAX_BRAVE_SNIPPETS),
            threshold_mode: BraveThresholdMode::Balanced,
            freshness: BraveFreshness::Any,
            safe_search: BraveSafeSearchMode::Moderate,
        }
    }
}

/// Provider-specific trusted options. Agent requests never select this value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum WebSearchProviderOptions {
    #[default]
    Searxng,
    Exa(ExaSearchOptions),
    BraveLlmContext(BraveLlmContextOptions),
    Tavily(TavilySearchOptions),
}

impl WebSearchProviderOptions {
    fn validate_and_clamp(
        &mut self,
        provider: WebSearchProvider,
        limits: &WebContextLimits,
    ) -> Result<(), WebContextConfigError> {
        match (provider, self) {
            (WebSearchProvider::Searxng, Self::Searxng) => Ok(()),
            (WebSearchProvider::Exa, Self::Exa(options)) => {
                validate_provider_limit(options.max_results, MAX_EXA_RESULTS)?;
                options.max_results = options.max_results.min(limits.max_search_results);
                Ok(())
            }
            (WebSearchProvider::Tavily, Self::Tavily(options)) => {
                validate_provider_limit(options.max_results, MAX_TAVILY_RESULTS)?;
                validate_provider_limit(options.chunks_per_source, MAX_TAVILY_CHUNKS_PER_SOURCE)?;
                options.max_results = options.max_results.min(limits.max_search_results);
                Ok(())
            }
            (WebSearchProvider::BraveLlmContext, Self::BraveLlmContext(options)) => {
                validate_provider_limit(options.max_results, MAX_BRAVE_RESULTS)?;
                validate_provider_limit(options.max_tokens, MAX_BRAVE_TOKENS)?;
                validate_provider_limit(options.max_urls, MAX_BRAVE_URLS)?;
                validate_provider_limit(options.max_snippets, MAX_BRAVE_SNIPPETS)?;
                options.max_results = options.max_results.min(limits.max_search_results);
                options.max_urls = options.max_urls.min(options.max_results);
                options.max_snippets = options.max_snippets.min(options.max_results);
                options.max_tokens = options.max_tokens.min(limits.max_text_bytes / 4);
                if options.max_tokens == 0 {
                    return Err(WebContextConfigError::ProviderContentBudget);
                }
                Ok(())
            }
            _ => Err(WebContextConfigError::ProviderOptions),
        }
    }
}

/// Resolved, trusted configuration for optional agent web context.
///
/// Configuration resolution belongs to frontend code. This host-side type
/// contains semantic values only. Resolved provider credentials stay private and
/// are only attached to first-party configured search requests.
#[derive(Clone, Default)]
pub struct AgentWebContextConfig {
    /// Web retrieval is fail-closed unless explicitly enabled.
    pub enabled: bool,
    /// Selected trusted provider. Agent requests cannot alter it.
    pub provider: WebSearchProvider,
    /// Semantic options matching [`Self::provider`].
    pub provider_options: WebSearchProviderOptions,
    /// Configured SearXNG-compatible JSON endpoint. Vendor providers use fixed origins.
    pub search_endpoint: Option<String>,
    /// Exact host names that have already received user-global approval.
    pub preapproved_hosts: BTreeSet<String>,
    /// Resource limits enforced for every request.
    pub limits: WebContextLimits,
    /// Opaque user-config reference. Frontend resolves this only while lazily
    /// constructing the service, then removes it before host construction.
    pub provider_secret_reference: Option<String>,
    /// Cloudflare account identifier for Browser Run. It is only accepted from
    /// user-global configuration and cannot be selected by an agent.
    pub browser_run_account_id: Option<String>,
    /// Opaque user-global secret reference for Browser Run API authentication.
    pub browser_run_api_token_reference: Option<String>,
    /// Bounded retry policy used for transient Cloudflare Browser Run failures.
    pub browser_run_retry: BrowserRunRetryPolicy,
    search_authorization: Option<SearchAuthorization>,
    browser_run_api_token: Option<Zeroizing<String>>,
}

/// Opaque provider credential. It has neither equality nor a value-revealing formatter.
#[derive(Clone)]
struct SearchAuthorization(Zeroizing<String>);

impl SearchAuthorization {
    fn is_blank(&self) -> bool {
        self.0.trim().is_empty()
    }

    fn apply_to(&self, provider: WebSearchProvider, headers: &mut BTreeMap<String, String>) {
        let (name, value) = match provider {
            WebSearchProvider::BraveLlmContext => ("x-subscription-token", self.0.to_string()),
            WebSearchProvider::Searxng | WebSearchProvider::Exa | WebSearchProvider::Tavily => {
                ("authorization", format!("Bearer {}", self.0.as_str()))
            }
        };
        headers.insert(name.to_owned(), value);
    }
}

impl fmt::Debug for SearchAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SearchAuthorization(REDACTED)")
    }
}

impl PartialEq for AgentWebContextConfig {
    fn eq(&self, other: &Self) -> bool {
        self.enabled == other.enabled
            && self.provider == other.provider
            && self.provider_options == other.provider_options
            && self.search_endpoint == other.search_endpoint
            && self.preapproved_hosts == other.preapproved_hosts
            && self.limits == other.limits
            && self.provider_secret_reference == other.provider_secret_reference
            && self.browser_run_account_id == other.browser_run_account_id
            && self.browser_run_api_token_reference == other.browser_run_api_token_reference
            && self.browser_run_retry == other.browser_run_retry
    }
}

impl Eq for AgentWebContextConfig {}

impl fmt::Debug for AgentWebContextConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentWebContextConfig")
            .field("enabled", &self.enabled)
            .field("provider", &self.provider)
            .field("provider_options", &self.provider_options)
            .field(
                "search_endpoint",
                &self.search_endpoint.as_deref().map(redact_url_text_for_display),
            )
            .field("preapproved_hosts", &self.preapproved_hosts)
            .field("limits", &self.limits)
            .field(
                "provider_secret_reference",
                &self.provider_secret_reference.as_ref().map(|_| "CONFIGURED"),
            )
            .field("search_authorization", &self.search_authorization)
            .field("browser_run_account_id", &self.browser_run_account_id)
            .field(
                "browser_run_api_token_reference",
                &self.browser_run_api_token_reference.as_ref().map(|_| "CONFIGURED"),
            )
            .field(
                "browser_run_api_token",
                &self.browser_run_api_token.as_ref().map(|_| "CONFIGURED"),
            )
            .field("browser_run_retry", &self.browser_run_retry)
            .finish()
    }
}

impl AgentWebContextConfig {
    /// Adds an opaque credential for the selected provider's first-party request only.
    pub fn with_search_authorization(mut self, secret: Zeroizing<String>) -> Self {
        self.search_authorization = Some(SearchAuthorization(secret));
        self
    }

    /// Adds a Cloudflare Browser Run API token for fixed first-party API requests only.
    pub fn with_browser_run_api_token(mut self, secret: Zeroizing<String>) -> Self {
        self.browser_run_api_token = Some(secret);
        self
    }

    /// Redacted fingerprint for frontend service invalidation.
    pub fn semantic_fingerprint(&self) -> String {
        format!(
            "{:?}:{:?}:{:?}:{:?}:{:?}:{:?}:{:?}:{:?}:{:?}:{:?}",
            self.enabled,
            self.provider,
            self.provider_options,
            self.search_endpoint.as_deref().map(redact_url_text_for_display),
            self.provider_secret_reference
                .as_ref()
                .map(|reference| sha256_hex(reference.as_bytes())),
            self.browser_run_account_id,
            self.browser_run_api_token_reference
                .as_ref()
                .map(|reference| sha256_hex(reference.as_bytes())),
            self.browser_run_retry,
            self.preapproved_hosts,
            self.limits,
        )
    }
}

/// Bounded retry policy for transient Cloudflare Browser Run failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserRunRetryPolicy {
    /// Total attempts, including the first request.
    pub max_attempts: u8,
    /// Exponential delay before the first retry when Cloudflare sends no hint.
    pub base_delay_ms: u64,
    /// Upper bound for a server hint or exponential retry delay.
    pub max_delay_ms: u64,
}

impl Default for BrowserRunRetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_BROWSER_RUN_MAX_ATTEMPTS,
            base_delay_ms: DEFAULT_BROWSER_RUN_BASE_DELAY_MS,
            max_delay_ms: DEFAULT_BROWSER_RUN_MAX_DELAY_MS,
        }
    }
}

impl BrowserRunRetryPolicy {
    fn validate(self) -> Result<(), WebContextConfigError> {
        if self.max_attempts == 0 || self.max_attempts > MAX_BROWSER_RUN_ATTEMPTS {
            return Err(WebContextConfigError::BrowserRunRetryAttempts);
        }
        if self.base_delay_ms == 0
            || self.base_delay_ms > MAX_BROWSER_RUN_DELAY_MS
            || self.max_delay_ms == 0
            || self.max_delay_ms > MAX_BROWSER_RUN_DELAY_MS
            || self.base_delay_ms > self.max_delay_ms
        {
            return Err(WebContextConfigError::BrowserRunRetryDelay);
        }
        Ok(())
    }
}

/// Bounds applied to remote retrieval and normalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebContextLimits {
    /// Maximum decompressed response bytes accepted from a transport.
    pub max_response_bytes: usize,
    /// Maximum UTF-8 source text returned to a caller.
    pub max_text_bytes: usize,
    /// Maximum normalized search results returned to a caller.
    pub max_search_results: usize,
    /// Maximum redirects followed; cannot exceed [`MAX_REDIRECTS`].
    pub max_redirects: usize,
    /// Total wall-clock timeout for one HTTPS request.
    pub request_timeout_ms: u64,
    /// Maximum in-flight requests allowed through one service instance.
    pub max_concurrent_requests: usize,
}

impl Default for WebContextLimits {
    fn default() -> Self {
        Self {
            max_response_bytes: DEFAULT_RESPONSE_BYTES,
            max_text_bytes: DEFAULT_TEXT_BYTES,
            max_search_results: DEFAULT_SEARCH_RESULTS,
            max_redirects: MAX_REDIRECTS,
            request_timeout_ms: DEFAULT_REQUEST_TIMEOUT_MS,
            max_concurrent_requests: DEFAULT_MAX_CONCURRENT_REQUESTS,
        }
    }
}

impl WebContextLimits {
    fn validate(&self) -> Result<(), WebContextConfigError> {
        if self.max_response_bytes == 0 || self.max_response_bytes > MAX_RESPONSE_BYTES {
            return Err(WebContextConfigError::ResponseByteLimit);
        }
        if self.max_text_bytes == 0
            || self.max_text_bytes > self.max_response_bytes
            || self.max_text_bytes > MAX_TEXT_BYTES
        {
            return Err(WebContextConfigError::TextByteLimit);
        }
        if self.max_search_results == 0 || self.max_search_results > MAX_SEARCH_RESULTS {
            return Err(WebContextConfigError::SearchResultLimit);
        }
        if self.max_redirects > MAX_REDIRECTS {
            return Err(WebContextConfigError::RedirectLimit);
        }
        if self.request_timeout_ms == 0 || self.request_timeout_ms > MAX_REQUEST_TIMEOUT_MS {
            return Err(WebContextConfigError::RequestTimeoutLimit);
        }
        if self.max_concurrent_requests == 0
            || self.max_concurrent_requests > MAX_CONCURRENT_REQUESTS
        {
            return Err(WebContextConfigError::ConcurrencyLimit);
        }
        Ok(())
    }
}

/// Rejected host-service configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebContextConfigError {
    ResponseByteLimit,
    TextByteLimit,
    SearchResultLimit,
    RedirectLimit,
    RequestTimeoutLimit,
    ConcurrencyLimit,
    SearchEndpoint,
    ProviderEndpoint,
    ProviderOptions,
    ProviderAuthorization,
    ProviderContentBudget,
    PreapprovedHost,
    BrowserRunRetryAttempts,
    BrowserRunRetryDelay,
}

impl fmt::Display for WebContextConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::ResponseByteLimit => "response byte limit must be within supported bounds",
            Self::TextByteLimit => "text byte limit must be within response byte limit",
            Self::SearchResultLimit => "search result limit must be within supported bounds",
            Self::RedirectLimit => "redirect limit exceeds service maximum",
            Self::RequestTimeoutLimit => "request timeout must be within supported bounds",
            Self::ConcurrencyLimit => "concurrency limit must be within supported bounds",
            Self::SearchEndpoint => "search endpoint is not a strict HTTPS URL",
            Self::ProviderEndpoint => {
                "selected provider does not accept this endpoint configuration"
            }
            Self::ProviderOptions => "selected provider options are invalid",
            Self::ProviderAuthorization => "selected provider requires an authorization secret",
            Self::ProviderContentBudget => "provider content budget exceeds configured text limits",
            Self::PreapprovedHost => "preapproved host is invalid",
            Self::BrowserRunRetryAttempts => {
                "Browser Run retry attempts must be within supported bounds"
            }
            Self::BrowserRunRetryDelay => {
                "Browser Run retry delays must be within supported bounds"
            }
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for WebContextConfigError {}

/// Stable public failure codes for web-context calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebContextErrorCode {
    WebDisabled,
    WebSearchUnavailable,
    NetworkApprovalRequired,
    UrlRejected,
    DnsRejected,
    RedirectRejected,
    UnsupportedContentType,
    ResponseTooLarge,
    NetworkTimeout,
    NetworkFailure,
}

impl WebContextErrorCode {
    /// Stable wire-format error code.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WebDisabled => "web_disabled",
            Self::WebSearchUnavailable => "web_search_unavailable",
            Self::NetworkApprovalRequired => "network_approval_required",
            Self::UrlRejected => "url_rejected",
            Self::DnsRejected => "dns_rejected",
            Self::RedirectRejected => "redirect_rejected",
            Self::UnsupportedContentType => "unsupported_content_type",
            Self::ResponseTooLarge => "response_too_large",
            Self::NetworkTimeout => "network_timeout",
            Self::NetworkFailure => "network_failure",
        }
    }

    const fn message(self) -> &'static str {
        match self {
            Self::WebDisabled => "web context is disabled",
            Self::WebSearchUnavailable => "web search is unavailable",
            Self::NetworkApprovalRequired => "network host approval is required",
            Self::UrlRejected => "URL was rejected by web safety policy",
            Self::DnsRejected => "DNS resolution was rejected by web safety policy",
            Self::RedirectRejected => "redirect was rejected by web safety policy",
            Self::UnsupportedContentType => "response content type is not supported",
            Self::ResponseTooLarge => "response exceeds configured size limit",
            Self::NetworkTimeout => "network request timed out",
            Self::NetworkFailure => "network request failed",
        }
    }
}

impl fmt::Display for WebContextErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Redacted, typed web-context failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebContextError {
    pub code: WebContextErrorCode,
    pub message: String,
    /// Canonical host requiring approval. Never contains URL paths, queries,
    /// headers, credentials, or other request data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

impl WebContextError {
    pub fn new(code: WebContextErrorCode) -> Self {
        Self { code, message: code.message().to_owned(), host: None }
    }

    fn network_approval_required(host: String) -> Self {
        Self {
            code: WebContextErrorCode::NetworkApprovalRequired,
            message: WebContextErrorCode::NetworkApprovalRequired.message().to_owned(),
            host: Some(host),
        }
    }
}

impl fmt::Display for WebContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for WebContextError {}

/// Search request accepted by [`WebContextService`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchRequest {
    pub query: String,
}

/// Fetch request accepted by [`WebContextService`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebFetchRequest {
    pub url: String,
}

/// One normalized SearXNG search result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchResult {
    pub title: String,
    pub url: String,
    pub host: String,
    pub snippet: String,
    pub rank: usize,
}

/// Redacted provider metadata retained with every normalized search response.
/// It deliberately excludes request URLs, query text, credentials, headers, and
/// vendor response identifiers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchProvenance {
    pub provider: WebSearchProvider,
    pub adapter: String,
    /// UTC Unix epoch milliseconds when this search response was first retrieved.
    pub retrieved_at_unix_ms: u64,
}

impl WebSearchProvenance {
    fn for_provider(provider: WebSearchProvider) -> Self {
        Self {
            provider,
            adapter: PROVIDER_ADAPTER_VERSION.to_owned(),
            retrieved_at_unix_ms: current_unix_millis(),
        }
    }

    /// Stable bounded identity suitable for tool output and lifecycle display.
    pub fn identity(&self) -> String {
        format!("{}:{}", self.provider.id(), self.adapter)
    }
}

/// Bounded search response. Remote text remains untrusted external content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchResponse {
    pub results: Vec<WebSearchResult>,
    pub provenance: WebSearchProvenance,
    pub truncated: bool,
    /// Whether this response was returned from session-local cache.
    #[serde(default)]
    pub cached: bool,
}

/// Bounded fetch response. `text` is untrusted external content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebFetchResponse {
    pub requested_url: String,
    pub final_url: String,
    pub title: Option<String>,
    pub content_type: String,
    pub text: String,
    /// UTC Unix epoch milliseconds when this remote text was first retrieved.
    pub retrieved_at_unix_ms: u64,
    pub truncated: bool,
    pub redirects: usize,
    /// Whether this response was returned from session-local cache.
    #[serde(default)]
    pub cached: bool,
}

/// HTTP method allowed at the host-owned transport boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebTransportMethod {
    Get,
    Post,
}

/// Request given to a transport after URL, host-approval, and DNS checks.
///
/// Implementations disable automatic redirects and cookie storage, use only
/// host-owned headers and bodies, and stop reading at `max_response_bytes`.
#[derive(Clone)]
pub struct WebTransportRequest {
    pub method: WebTransportMethod,
    pub url: Url,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
    pub max_response_bytes: usize,
}

impl fmt::Debug for WebTransportRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebTransportRequest")
            .field("method", &self.method)
            .field("url", &redact_url_for_display(&self.url))
            .field("headers", &crate::redact::redact_headers(&self.headers))
            .field("body", &"REDACTED")
            .field("max_response_bytes", &self.max_response_bytes)
            .finish()
    }
}

/// Response returned by a transport.
///
/// `connected_peer` must be address of TCP peer actually connected, not a
/// later DNS lookup. `body_truncated` reports that reading stopped at limit.
#[derive(Debug, Clone)]
pub struct WebTransportResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
    pub body_truncated: bool,
    pub connected_peer: IpAddr,
}

/// Minimal asynchronous network boundary for an offline-testable web-context
/// service. Implementations must observe `cancellation` while resolving and
/// receiving a response body.
pub trait WebTransport: Send + Sync {
    /// Resolve host before each connection attempt.
    fn resolve<'a>(
        &'a self,
        host: &'a str,
        cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<IpAddr>, WebContextError>>;

    /// Perform one host-owned HTTPS request without following redirects.
    fn request<'a>(
        &'a self,
        request: &'a WebTransportRequest,
        cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<WebTransportResponse, WebContextError>>;
}

/// Production HTTPS-only transport with redirects, proxies, cookies, and caller
/// headers disabled. [`WebContextService`] validates DNS and connected peer.
pub struct ReqwestWebTransport {
    client: reqwest::Client,
}

impl ReqwestWebTransport {
    /// Builds bounded transport using Rustls through workspace `reqwest`.
    pub fn new(limits: &WebContextLimits) -> Result<Self, WebContextError> {
        limits.validate().map_err(|_| WebContextError::new(WebContextErrorCode::NetworkFailure))?;
        let request_timeout = Duration::from_millis(limits.request_timeout_ms);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(request_timeout.min(Duration::from_secs(10)))
            .timeout(request_timeout)
            .build()
            .map_err(|_| WebContextError::new(WebContextErrorCode::NetworkFailure))?;
        Ok(Self { client })
    }
}

impl WebTransport for ReqwestWebTransport {
    fn resolve<'a>(
        &'a self,
        host: &'a str,
        cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<IpAddr>, WebContextError>> {
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(cancellation_error()),
                addresses = tokio::net::lookup_host((host, 443)) => addresses
                    .map_err(|_| WebContextError::new(WebContextErrorCode::NetworkFailure))
                    .map(|addresses| addresses.map(|address| address.ip()).collect()),
            }
        })
    }

    fn request<'a>(
        &'a self,
        request: &'a WebTransportRequest,
        cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<WebTransportResponse, WebContextError>> {
        Box::pin(async move {
            let mut headers = reqwest::header::HeaderMap::new();
            for (name, value) in &request.headers {
                let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| WebContextError::new(WebContextErrorCode::NetworkFailure))?;
                let value = reqwest::header::HeaderValue::from_str(value)
                    .map_err(|_| WebContextError::new(WebContextErrorCode::NetworkFailure))?;
                headers.insert(name, value);
            }
            let response = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(cancellation_error()),
                response = self.client.request(
                    match request.method {
                        WebTransportMethod::Get => reqwest::Method::GET,
                        WebTransportMethod::Post => reqwest::Method::POST,
                    },
                    request.url.clone(),
                ).headers(headers).body(request.body.clone()).send() => response.map_err(reqwest_error)?,
            };
            let connected_peer = response
                .remote_addr()
                .ok_or_else(|| WebContextError::new(WebContextErrorCode::DnsRejected))?
                .ip();
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    value.to_str().ok().map(|value| (name.as_str().to_owned(), value.to_owned()))
                })
                .collect();
            let mut body = Vec::with_capacity(request.max_response_bytes.min(16 * 1024));
            let mut stream = response.bytes_stream();
            while let Some(chunk) = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(cancellation_error()),
                chunk = stream.next() => chunk,
            } {
                let chunk = chunk.map_err(reqwest_error)?;
                let remaining =
                    request.max_response_bytes.saturating_add(1).saturating_sub(body.len());
                body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                if body.len() > request.max_response_bytes {
                    body.truncate(request.max_response_bytes);
                    return Ok(WebTransportResponse {
                        status,
                        headers,
                        body,
                        body_truncated: true,
                        connected_peer,
                    });
                }
            }
            Ok(WebTransportResponse {
                status,
                headers,
                body,
                body_truncated: false,
                connected_peer,
            })
        })
    }
}

fn cancellation_error() -> WebContextError {
    WebContextError::new(WebContextErrorCode::NetworkFailure)
}

fn ensure_not_cancelled(cancellation: &CancellationToken) -> Result<(), WebContextError> {
    if cancellation.is_cancelled() { Err(cancellation_error()) } else { Ok(()) }
}

async fn run_with_timeout<R>(
    cancellation: &CancellationToken,
    timeout: Duration,
    operation: impl std::future::Future<Output = Result<R, WebContextError>>,
) -> Result<R, WebContextError> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(cancellation_error()),
        result = tokio::time::timeout(timeout, operation) => match result {
            Ok(result) => result,
            Err(_) => Err(WebContextError::new(WebContextErrorCode::NetworkTimeout)),
        },
    }
}

fn reqwest_error(error: reqwest::Error) -> WebContextError {
    let code = if error.is_timeout() {
        WebContextErrorCode::NetworkTimeout
    } else {
        WebContextErrorCode::NetworkFailure
    };
    WebContextError::new(code)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum WebCacheKey {
    Search { provider_identity: String, options_digest: String, query_digest: String },
    Fetch { final_url: String, representation: WebRepresentation },
}

/// Accepted remote representation. Keep this fixed and host-owned so cache keys
/// cannot be influenced by model-provided headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum WebRepresentation {
    TextV1,
}

#[derive(Debug, Clone)]
enum WebCacheValue {
    Search(WebSearchResponse),
    Fetch(WebFetchResponse),
}

/// Safe cache validators copied from a response only after strict size and
/// character validation. No arbitrary response headers are retained or replayed.
#[derive(Debug, Clone, Default)]
struct CacheValidators {
    etag: Option<String>,
    last_modified: Option<String>,
}

impl CacheValidators {
    fn is_empty(&self) -> bool {
        self.etag.is_none() && self.last_modified.is_none()
    }

    fn apply_to(&self, headers: &mut BTreeMap<String, String>) {
        if let Some(etag) = &self.etag {
            headers.insert("if-none-match".to_owned(), etag.clone());
        }
        if let Some(last_modified) = &self.last_modified {
            headers.insert("if-modified-since".to_owned(), last_modified.clone());
        }
    }
}

#[derive(Debug, Clone)]
struct WebCacheEntry {
    key: WebCacheKey,
    value: WebCacheValue,
    validators: CacheValidators,
    fresh_until: Instant,
    discard_after: Instant,
    bytes: usize,
}

#[derive(Debug, Clone)]
struct WebCacheLookup {
    canonical_key: WebCacheKey,
    value: WebCacheValue,
    validators: CacheValidators,
    stale: bool,
}

/// Bounded, session-local LRU cache. It retains only normalized public response
/// fields and safe validators, never raw response bytes, transport headers, or
/// credentials. Expired entries remain briefly only for conditional revalidation.
#[derive(Debug)]
struct WebContextCache {
    entries: VecDeque<WebCacheEntry>,
    aliases: BTreeMap<WebCacheKey, WebCacheKey>,
    bytes: usize,
    max_entries: usize,
    max_bytes: usize,
    max_entry_bytes: usize,
    ttl: Duration,
    stale_ttl: Duration,
}

impl WebContextCache {
    fn new(
        max_entries: usize,
        max_bytes: usize,
        max_entry_bytes: usize,
        ttl: Duration,
        stale_ttl: Duration,
    ) -> Self {
        Self {
            entries: VecDeque::new(),
            aliases: BTreeMap::new(),
            bytes: 0,
            max_entries,
            max_bytes,
            max_entry_bytes,
            ttl,
            stale_ttl,
        }
    }

    fn get(&mut self, key: &WebCacheKey) -> Option<WebCacheLookup> {
        self.remove_discarded();
        let canonical_key = self.aliases.get(key).cloned().unwrap_or_else(|| key.clone());
        let index = self.entries.iter().position(|entry| entry.key == canonical_key)?;
        let entry = self.entries.remove(index).expect("cache entry index exists");
        let stale = Instant::now() >= entry.fresh_until;
        let lookup = WebCacheLookup {
            canonical_key: entry.key.clone(),
            value: entry.value.clone(),
            validators: entry.validators.clone(),
            stale,
        };
        self.entries.push_back(entry);
        Some(lookup)
    }

    fn insert(
        &mut self,
        key: WebCacheKey,
        aliases: impl IntoIterator<Item = WebCacheKey>,
        value: WebCacheValue,
        validators: CacheValidators,
    ) {
        self.remove_discarded();
        let bytes = cache_entry_bytes(&key, &value, &validators);
        if self.max_entries == 0 || bytes > self.max_entry_bytes || bytes > self.max_bytes {
            return;
        }
        self.remove_key(&key);
        while self.entries.len() >= self.max_entries
            || self.bytes.saturating_add(bytes) > self.max_bytes
        {
            let entry = self.entries.pop_front().expect("cache has entry to evict");
            self.bytes -= entry.bytes;
            self.aliases.retain(|_, canonical| canonical != &entry.key);
        }
        let now = Instant::now();
        self.bytes += bytes;
        self.entries.push_back(WebCacheEntry {
            key: key.clone(),
            value,
            validators,
            fresh_until: now + self.ttl,
            discard_after: now + self.ttl + self.stale_ttl,
            bytes,
        });
        self.aliases.insert(key.clone(), key.clone());
        for alias in aliases {
            self.aliases.insert(alias, key.clone());
        }
    }

    fn refresh(&mut self, key: &WebCacheKey) {
        let canonical_key = self.aliases.get(key).cloned().unwrap_or_else(|| key.clone());
        if let Some(index) = self.entries.iter().position(|entry| entry.key == canonical_key) {
            let mut entry = self.entries.remove(index).expect("cache entry index exists");
            let now = Instant::now();
            entry.fresh_until = now + self.ttl;
            entry.discard_after = now + self.ttl + self.stale_ttl;
            self.entries.push_back(entry);
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.aliases.clear();
        self.bytes = 0;
    }

    fn remove_key(&mut self, key: &WebCacheKey) {
        if let Some(index) = self.entries.iter().position(|entry| entry.key == *key) {
            let entry = self.entries.remove(index).expect("cache entry index exists");
            self.bytes -= entry.bytes;
            self.aliases.retain(|_, canonical| canonical != &entry.key);
        }
    }

    fn remove_discarded(&mut self) {
        let now = Instant::now();
        let mut retained = VecDeque::with_capacity(self.entries.len());
        while let Some(entry) = self.entries.pop_front() {
            if now >= entry.discard_after {
                self.bytes -= entry.bytes;
                self.aliases.retain(|_, canonical| canonical != &entry.key);
            } else {
                retained.push_back(entry);
            }
        }
        self.entries = retained;
    }
}

fn cache_entry_bytes(
    key: &WebCacheKey,
    value: &WebCacheValue,
    validators: &CacheValidators,
) -> usize {
    let key_bytes = match key {
        WebCacheKey::Search { provider_identity, options_digest, query_digest } => {
            provider_identity.len() + options_digest.len() + query_digest.len()
        }
        WebCacheKey::Fetch { final_url, representation: _ } => final_url.len(),
    };
    let validator_bytes = validators.etag.as_ref().map_or(0, String::len)
        + validators.last_modified.as_ref().map_or(0, String::len);
    let value_bytes = match value {
        WebCacheValue::Search(response) => response.results.iter().fold(0, |total, result| {
            total
                + result.title.len()
                + result.url.len()
                + result.host.len()
                + result.snippet.len()
                + std::mem::size_of::<usize>()
        }),
        WebCacheValue::Fetch(response) => {
            response.requested_url.len()
                + response.final_url.len()
                + response.title.as_ref().map_or(0, String::len)
                + response.content_type.len()
                + response.text.len()
                + std::mem::size_of::<usize>()
        }
    };
    key_bytes.saturating_add(validator_bytes).saturating_add(value_bytes)
}

fn cache_validators(headers: &BTreeMap<String, String>) -> CacheValidators {
    CacheValidators {
        etag: cache_validator(header_value(headers, "etag")),
        last_modified: cache_validator(header_value(headers, "last-modified")),
    }
}

fn cache_validator(value: Option<&str>) -> Option<String> {
    value
        .filter(|value| {
            !value.is_empty()
                && value.len() <= MAX_CACHE_VALIDATOR_BYTES
                && value.bytes().all(|byte| byte.is_ascii_graphic() || byte == b' ')
        })
        .map(str::to_owned)
}

fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn normalize_search_query(query: &str) -> String {
    query.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_cloudflare_account_id(account_id: &str) -> bool {
    account_id.len() == 32 && account_id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn browser_run_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

fn browser_run_retry_delay(
    headers: &BTreeMap<String, String>,
    attempt: u8,
    policy: BrowserRunRetryPolicy,
) -> Duration {
    let exponential =
        policy.base_delay_ms.saturating_mul(1_u64 << attempt.saturating_sub(1).min(15));
    let delay_ms = header_value(headers, "retry-after")
        .and_then(|value| value.trim().parse::<u64>().ok())
        .and_then(|seconds| seconds.checked_mul(1_000))
        .unwrap_or(exponential)
        .min(policy.max_delay_ms);
    Duration::from_millis(delay_ms)
}

fn validate_search_query(query: &str) -> Result<(), WebContextError> {
    if query.is_empty() || query.len() > MAX_SEARCH_QUERY_BYTES {
        return Err(WebContextError::new(WebContextErrorCode::UrlRejected));
    }
    Ok(())
}

fn validate_provider_limit(value: usize, maximum: usize) -> Result<(), WebContextConfigError> {
    if value == 0 || value > maximum {
        return Err(WebContextConfigError::ProviderOptions);
    }
    Ok(())
}

fn provider_options_digest(options: &WebSearchProviderOptions) -> String {
    let identity = match options {
        WebSearchProviderOptions::Searxng => "searxng".to_owned(),
        WebSearchProviderOptions::Exa(options) => {
            format!("exa:{:?}:{}", options.search_mode, options.max_results)
        }
        WebSearchProviderOptions::Tavily(options) => format!(
            "tavily:{:?}:{}:{}",
            options.search_depth, options.max_results, options.chunks_per_source
        ),
        WebSearchProviderOptions::BraveLlmContext(options) => format!(
            "brave:{:?}:{}:{}:{}:{}:{:?}:{:?}",
            options.threshold_mode,
            options.max_results,
            options.max_tokens,
            options.max_urls,
            options.max_snippets,
            options.freshness,
            options.safe_search,
        ),
    };
    sha256_hex(identity.as_bytes())
}

const PROVIDER_ADAPTER_VERSION: &str = "v1";

/// Internal boundary between provider-specific JSON and shared web safety policy.
/// It is selected from trusted configuration when the service is constructed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderSearchAdapter {
    Searxng,
    Exa,
    BraveLlmContext,
    Tavily,
}

impl ProviderSearchAdapter {
    fn for_provider(provider: WebSearchProvider) -> Self {
        match provider {
            WebSearchProvider::Searxng => Self::Searxng,
            WebSearchProvider::Exa => Self::Exa,
            WebSearchProvider::BraveLlmContext => Self::BraveLlmContext,
            WebSearchProvider::Tavily => Self::Tavily,
        }
    }

    fn permits_redirects(self) -> bool {
        matches!(self, Self::Searxng)
    }

    fn permits_revalidation(self) -> bool {
        matches!(self, Self::Searxng)
    }

    fn cache_identity(self, endpoint: &Url) -> String {
        let provider = match self {
            Self::Searxng => WebSearchProvider::Searxng,
            Self::Exa => WebSearchProvider::Exa,
            Self::BraveLlmContext => WebSearchProvider::BraveLlmContext,
            Self::Tavily => WebSearchProvider::Tavily,
        };
        format!(
            "{}:{}:{}",
            provider.id(),
            redact_url_for_display(endpoint),
            PROVIDER_ADAPTER_VERSION
        )
    }

    fn build_request(
        self,
        config: &AgentWebContextConfig,
        endpoint: Url,
        query: &str,
    ) -> Result<WebTransportRequest, WebContextError> {
        match self {
            Self::Searxng => {
                let mut url = endpoint;
                url.query_pairs_mut().append_pair("format", "json").append_pair("q", query);
                Ok(WebTransportRequest {
                    method: WebTransportMethod::Get,
                    url,
                    headers: search_headers(config),
                    body: Vec::new(),
                    max_response_bytes: config.limits.max_response_bytes,
                })
            }
            Self::Exa => {
                let WebSearchProviderOptions::Exa(options) = &config.provider_options else {
                    return Err(WebContextError::new(WebContextErrorCode::WebSearchUnavailable));
                };
                let mode = match options.search_mode {
                    ExaSearchMode::Auto => "auto",
                    ExaSearchMode::Neural => "neural",
                    ExaSearchMode::Fast => "fast",
                };
                self.vendor_request(
                    config,
                    endpoint,
                    serde_json::json!({
                        "query": query,
                        "type": mode,
                        "numResults": options.max_results,
                        "contents": { "highlights": true },
                    }),
                )
            }
            Self::Tavily => {
                let WebSearchProviderOptions::Tavily(options) = &config.provider_options else {
                    return Err(WebContextError::new(WebContextErrorCode::WebSearchUnavailable));
                };
                let depth = match options.search_depth {
                    TavilySearchDepth::Basic => "basic",
                    TavilySearchDepth::Advanced => "advanced",
                };
                self.vendor_request(
                    config,
                    endpoint,
                    serde_json::json!({
                        "query": query,
                        "search_depth": depth,
                        "chunks_per_source": options.chunks_per_source,
                        "max_results": options.max_results,
                    }),
                )
            }
            Self::BraveLlmContext => {
                let WebSearchProviderOptions::BraveLlmContext(options) = &config.provider_options
                else {
                    return Err(WebContextError::new(WebContextErrorCode::WebSearchUnavailable));
                };
                let threshold = match options.threshold_mode {
                    BraveThresholdMode::Balanced => 0.5,
                    BraveThresholdMode::Strict => 0.8,
                };
                let freshness = match options.freshness {
                    BraveFreshness::Any => "all",
                    BraveFreshness::Day => "pd",
                    BraveFreshness::Week => "pw",
                    BraveFreshness::Month => "pm",
                };
                let safe_search = match options.safe_search {
                    BraveSafeSearchMode::Off => "off",
                    BraveSafeSearchMode::Moderate => "moderate",
                    BraveSafeSearchMode::Strict => "strict",
                };
                self.vendor_request(
                    config,
                    endpoint,
                    serde_json::json!({
                        "q": query,
                        "count": options.max_results,
                        "maximum_number_of_urls": options.max_urls,
                        "maximum_number_of_tokens": options.max_tokens,
                        "maximum_number_of_snippets": options.max_snippets,
                        "maximum_number_of_tokens_per_url": (options.max_tokens / options.max_urls).max(1),
                        "maximum_number_of_snippets_per_url": (options.max_snippets / options.max_urls).max(1),
                        "relevance_threshold": threshold,
                        "freshness": freshness,
                        "safesearch": safe_search,
                        "enable_local": false,
                    }),
                )
            }
        }
    }

    fn vendor_request(
        self,
        config: &AgentWebContextConfig,
        url: Url,
        body: serde_json::Value,
    ) -> Result<WebTransportRequest, WebContextError> {
        let mut headers = fixed_headers();
        headers.insert("accept".to_owned(), "application/json".to_owned());
        headers.insert("content-type".to_owned(), "application/json".to_owned());
        let authorization = config
            .search_authorization
            .as_ref()
            .ok_or_else(|| WebContextError::new(WebContextErrorCode::WebSearchUnavailable))?;
        authorization.apply_to(config.provider, &mut headers);
        Ok(WebTransportRequest {
            method: WebTransportMethod::Post,
            url,
            headers,
            body: serde_json::to_vec(&body)
                .map_err(|_| WebContextError::new(WebContextErrorCode::NetworkFailure))?,
            max_response_bytes: config.limits.max_response_bytes,
        })
    }

    fn result_limit(self, config: &AgentWebContextConfig) -> usize {
        match &config.provider_options {
            WebSearchProviderOptions::Searxng => config.limits.max_search_results,
            WebSearchProviderOptions::Exa(options) => options.max_results,
            WebSearchProviderOptions::BraveLlmContext(options) => options.max_results,
            WebSearchProviderOptions::Tavily(options) => options.max_results,
        }
    }

    fn parse_response(
        self,
        body: &[u8],
        max_body_bytes: usize,
        max_results: usize,
        max_brave_snippets: usize,
    ) -> Result<Vec<WebSearchResult>, WebContextError> {
        match self {
            Self::Searxng => parse_searxng_json(body, max_body_bytes, max_results),
            Self::Exa => parse_exa_json(body, max_body_bytes, max_results),
            Self::BraveLlmContext => {
                parse_brave_llm_context_json(body, max_body_bytes, max_results, max_brave_snippets)
            }
            Self::Tavily => parse_tavily_json(body, max_body_bytes, max_results),
        }
    }

    fn max_brave_snippets(self, config: &AgentWebContextConfig) -> usize {
        match &config.provider_options {
            WebSearchProviderOptions::BraveLlmContext(options) => options.max_snippets,
            _ => 0,
        }
    }
}

/// Safe remote retrieval service. It has no CLI, proxy, or UI wiring.
pub struct WebContextService<T> {
    config: AgentWebContextConfig,
    search_adapter: ProviderSearchAdapter,
    transport: T,
    cache: Mutex<WebContextCache>,
    active_requests: Mutex<usize>,
}

impl<T: WebTransport> WebContextService<T> {
    /// Builds a service after validating only trusted, frontend-resolved config.
    pub fn new(
        mut config: AgentWebContextConfig,
        transport: T,
    ) -> Result<Self, WebContextConfigError> {
        config.limits.validate()?;
        config.browser_run_retry.validate()?;
        config.provider_options.validate_and_clamp(config.provider, &config.limits)?;
        match config.provider {
            WebSearchProvider::Searxng => {
                if let Some(endpoint) = config.search_endpoint.as_deref() {
                    validate_search_endpoint_url(endpoint)
                        .map_err(|_| WebContextConfigError::SearchEndpoint)?;
                }
            }
            WebSearchProvider::Exa
            | WebSearchProvider::BraveLlmContext
            | WebSearchProvider::Tavily => {
                if config.search_endpoint.is_some() {
                    return Err(WebContextConfigError::ProviderEndpoint);
                }
                if config.enabled
                    && config
                        .search_authorization
                        .as_ref()
                        .is_none_or(SearchAuthorization::is_blank)
                {
                    return Err(WebContextConfigError::ProviderAuthorization);
                }
            }
        }
        config.preapproved_hosts = config
            .preapproved_hosts
            .iter()
            .map(|host| normalize_approved_host(host))
            .collect::<Result<_, _>>()?;
        let search_adapter = ProviderSearchAdapter::for_provider(config.provider);
        Ok(Self {
            config,
            search_adapter,
            transport,
            cache: Mutex::new(WebContextCache::new(
                WEB_CACHE_MAX_ENTRIES,
                WEB_CACHE_MAX_BYTES,
                WEB_CACHE_MAX_ENTRY_BYTES,
                WEB_CACHE_TTL,
                WEB_CACHE_STALE_TTL,
            )),
            active_requests: Mutex::new(0),
        })
    }

    /// Clears every session-local cached web response.
    pub fn clear_cache(&self) {
        self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clear();
    }

    /// Returns whether a safely canonicalized host is configured as preapproved.
    pub fn is_preapproved_host(&self, host: &str) -> bool {
        normalize_approved_host(host)
            .is_ok_and(|host| self.config.preapproved_hosts.contains(&host))
    }

    /// Returns canonical host for configured initial search request.
    pub fn search_initial_host(&self) -> Result<String, WebContextError> {
        self.search_endpoint_url().and_then(|url| canonical_url_host(&url))
    }

    /// Returns trusted, query-free provider label for network approval UI.
    pub const fn search_provider_approval_label(&self) -> &'static str {
        self.config.provider.approval_label()
    }

    /// Returns canonical host for initial fetch request.
    pub fn fetch_initial_host(&self, request: &WebFetchRequest) -> Result<String, WebContextError> {
        validate_https_url(&request.url).and_then(|url| canonical_url_host(&url))
    }

    /// Returns canonical host for an approved Browser Run target URL.
    pub fn browser_run_initial_host(
        &self,
        request: &ee_mcp::BrowserRunRequest,
    ) -> Result<String, WebContextError> {
        validate_https_url(&request.url).and_then(|url| canonical_url_host(&url))
    }

    /// Executes one configured Cloudflare Browser Run quick action.
    ///
    /// Browser Run receives only a public HTTPS target that passed local
    /// validation and a fixed action payload. Agent input cannot control the
    /// Cloudflare API origin, credentials, browser options, or request headers.
    pub async fn browser_run_with_approved_hosts_and_cancellation(
        &self,
        request: ee_mcp::BrowserRunRequest,
        approved_hosts: &BTreeSet<String>,
        cancellation: &CancellationToken,
    ) -> Result<ee_mcp::BrowserRunResult, WebContextError> {
        run_with_timeout(
            cancellation,
            Duration::from_millis(self.config.limits.request_timeout_ms),
            self.browser_run_with_approved_hosts_inner(request, approved_hosts, cancellation),
        )
        .await
    }

    async fn browser_run_with_approved_hosts_inner(
        &self,
        request: ee_mcp::BrowserRunRequest,
        approved_hosts: &BTreeSet<String>,
        cancellation: &CancellationToken,
    ) -> Result<ee_mcp::BrowserRunResult, WebContextError> {
        ensure_not_cancelled(cancellation)?;
        self.require_enabled()?;
        let target = validate_https_url(&request.url)?;
        let effective_hosts = self.effective_approved_hosts(approved_hosts)?;
        self.require_approved_host(&target, &effective_hosts)?;
        // Browser Run executes remotely, so validate target resolution before
        // disclosing it to Cloudflare. The fixed API request receives the
        // existing DNS/connected-peer validation in `request_checked` below.
        self.resolve_public_host(&target, cancellation).await?;

        let account_id = self
            .config
            .browser_run_account_id
            .as_deref()
            .filter(|account_id| is_cloudflare_account_id(account_id))
            .ok_or_else(|| WebContextError::new(WebContextErrorCode::WebDisabled))?;
        let api_token = self
            .config
            .browser_run_api_token
            .as_ref()
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| WebContextError::new(WebContextErrorCode::WebDisabled))?;
        let endpoint = Url::parse(&format!(
            "https://api.cloudflare.com/client/v4/accounts/{account_id}/browser-rendering/{}",
            request.action.as_str()
        ))
        .map_err(|_| WebContextError::new(WebContextErrorCode::NetworkFailure))?;
        let mut headers = fixed_headers();
        headers.insert("accept".to_owned(), "application/json, image/png".to_owned());
        headers.insert("content-type".to_owned(), "application/json".to_owned());
        headers.insert("authorization".to_owned(), format!("Bearer {}", api_token.as_str()));
        let mut body = serde_json::Map::new();
        body.insert("url".to_owned(), serde_json::Value::String(target.to_string()));
        match request.action {
            ee_mcp::BrowserRunAction::Scrape => {
                let selector = request
                    .selector
                    .filter(|selector| !selector.trim().is_empty())
                    .ok_or_else(|| WebContextError::new(WebContextErrorCode::UrlRejected))?;
                body.insert("elements".to_owned(), serde_json::json!([{ "selector": selector }]));
            }
            ee_mcp::BrowserRunAction::Json => {
                let prompt = request
                    .prompt
                    .filter(|prompt| !prompt.trim().is_empty())
                    .ok_or_else(|| WebContextError::new(WebContextErrorCode::UrlRejected))?;
                body.insert("prompt".to_owned(), serde_json::Value::String(prompt));
            }
            _ => {}
        }
        let body = serde_json::to_vec(&body)
            .map_err(|_| WebContextError::new(WebContextErrorCode::NetworkFailure))?;
        let fixed_hosts = BTreeSet::from([String::from("api.cloudflare.com")]);
        let transport_request = WebTransportRequest {
            method: WebTransportMethod::Post,
            url: endpoint,
            headers,
            body,
            max_response_bytes: self.config.limits.max_response_bytes,
        };
        let _permit = self.acquire_request()?;
        let mut attempt = 1;
        let response = loop {
            let (_url, response, _) = self
                .request_checked(transport_request.clone(), &fixed_hosts, cancellation, false, true)
                .await?;
            if browser_run_retryable_status(response.status)
                && attempt < self.config.browser_run_retry.max_attempts
            {
                let delay = browser_run_retry_delay(
                    &response.headers,
                    attempt,
                    self.config.browser_run_retry,
                );
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err(cancellation_error()),
                    () = tokio::time::sleep(delay) => {}
                }
                attempt += 1;
                continue;
            }
            if !(200..300).contains(&response.status) {
                return Err(WebContextError::new(WebContextErrorCode::NetworkFailure));
            }
            break response;
        };
        let content_type = header_value(&response.headers, "content-type")
            .unwrap_or("application/octet-stream")
            .split(';')
            .next()
            .unwrap_or("application/octet-stream")
            .to_owned();
        let result = if request.action == ee_mcp::BrowserRunAction::Screenshot
            && content_type.starts_with("image/")
        {
            serde_json::json!({
                "data": base64::engine::general_purpose::STANDARD.encode(&response.body),
                "encoding": "base64",
            })
        } else if let Ok(envelope) = serde_json::from_slice::<serde_json::Value>(&response.body) {
            if envelope.get("success") == Some(&serde_json::Value::Bool(false)) {
                return Err(WebContextError::new(WebContextErrorCode::NetworkFailure));
            }
            envelope.get("result").cloned().unwrap_or(envelope)
        } else {
            serde_json::Value::String(
                String::from_utf8(response.body).map_err(|_| {
                    WebContextError::new(WebContextErrorCode::UnsupportedContentType)
                })?,
            )
        };
        Ok(ee_mcp::BrowserRunResult {
            action: request.action,
            requested_url: redact_url_for_display(&target),
            content_type,
            result,
            truncated: false,
            trust: String::from("untrusted_external_content"),
        })
    }

    /// Returns typed configured search results from SearXNG-compatible JSON.
    pub async fn search(
        &self,
        request: WebSearchRequest,
    ) -> Result<WebSearchResponse, WebContextError> {
        self.search_with_cancellation(request, &CancellationToken::new()).await
    }

    /// Searches while observing caller cancellation.
    pub async fn search_with_cancellation(
        &self,
        request: WebSearchRequest,
        cancellation: &CancellationToken,
    ) -> Result<WebSearchResponse, WebContextError> {
        self.search_with_approved_hosts_and_cancellation(request, &BTreeSet::new(), cancellation)
            .await
    }

    /// Searches with frontend-supplied, request-scoped host approvals.
    ///
    /// Supplied approvals are canonicalized and unioned with configured
    /// `preapproved_hosts`; configuration is never mutated. Responses retrieved
    /// with extra approvals are not cached, so later calls cannot reuse them
    /// without rechecking every redirect host.
    pub async fn search_with_approved_hosts(
        &self,
        request: WebSearchRequest,
        approved_hosts: &BTreeSet<String>,
    ) -> Result<WebSearchResponse, WebContextError> {
        self.search_with_approved_hosts_and_cancellation(
            request,
            approved_hosts,
            &CancellationToken::new(),
        )
        .await
    }

    /// Searches with request-scoped host approvals while observing cancellation.
    pub async fn search_with_approved_hosts_and_cancellation(
        &self,
        request: WebSearchRequest,
        approved_hosts: &BTreeSet<String>,
        cancellation: &CancellationToken,
    ) -> Result<WebSearchResponse, WebContextError> {
        ensure_not_cancelled(cancellation)?;
        let effective_hosts = self.effective_approved_hosts(approved_hosts)?;
        let cache_allowed = effective_hosts == self.config.preapproved_hosts;
        run_with_timeout(
            cancellation,
            Duration::from_millis(self.config.limits.request_timeout_ms),
            self.search_with_hosts(request, &effective_hosts, cache_allowed, cancellation),
        )
        .await
    }

    async fn search_with_hosts(
        &self,
        request: WebSearchRequest,
        approved_hosts: &BTreeSet<String>,
        cache_allowed: bool,
        cancellation: &CancellationToken,
    ) -> Result<WebSearchResponse, WebContextError> {
        ensure_not_cancelled(cancellation)?;
        self.require_enabled()?;
        let query = normalize_search_query(&request.query);
        validate_search_query(&query)?;
        let adapter = self.search_adapter;
        let endpoint = self.search_endpoint_url()?;
        let cache_key = WebCacheKey::Search {
            provider_identity: adapter.cache_identity(&endpoint),
            options_digest: provider_options_digest(&self.config.provider_options),
            query_digest: sha256_hex(query.as_bytes()),
        };
        let cache_lookup = cache_allowed
            .then(|| {
                self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(&cache_key)
            })
            .flatten();
        if let Some(WebCacheLookup { value: WebCacheValue::Search(cached), stale: false, .. }) =
            &cache_lookup
        {
            ensure_not_cancelled(cancellation)?;
            let mut cached = cached.clone();
            cached.cached = true;
            return Ok(cached);
        }

        let mut transport_request = adapter.build_request(&self.config, endpoint, &query)?;
        if adapter.permits_revalidation()
            && let Some(lookup) = &cache_lookup
            && lookup.stale
            && !lookup.validators.is_empty()
        {
            lookup.validators.apply_to(&mut transport_request.headers);
        }

        let _permit = self.acquire_request()?;
        let (_final_url, response, redirects) = self
            .request_checked(
                transport_request,
                approved_hosts,
                cancellation,
                adapter.permits_redirects(),
                false,
            )
            .await?;
        ensure_not_cancelled(cancellation)?;
        if response.status == 304 {
            if !adapter.permits_revalidation() {
                return Err(WebContextError::new(WebContextErrorCode::NetworkFailure));
            }
            let Some(WebCacheLookup {
                canonical_key,
                value: WebCacheValue::Search(mut cached),
                stale: true,
                ..
            }) = cache_lookup
            else {
                return Err(WebContextError::new(WebContextErrorCode::NetworkFailure));
            };
            if redirects != 0 {
                return Err(WebContextError::new(WebContextErrorCode::RedirectRejected));
            }
            ensure_not_cancelled(cancellation)?;
            self.cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .refresh(&canonical_key);
            cached.cached = true;
            return Ok(cached);
        }
        if !is_json_mime(header_value(&response.headers, "content-type")) {
            return Err(WebContextError::new(WebContextErrorCode::UnsupportedContentType));
        }
        let mut results = adapter.parse_response(
            &response.body,
            self.config.limits.max_response_bytes,
            adapter.result_limit(&self.config),
            adapter.max_brave_snippets(&self.config),
        )?;
        let truncated = truncate_search_results(&mut results, self.config.limits.max_text_bytes);
        let validators = if adapter.permits_revalidation() {
            cache_validators(&response.headers)
        } else {
            CacheValidators::default()
        };
        let response = WebSearchResponse {
            results,
            provenance: WebSearchProvenance::for_provider(self.config.provider),
            truncated,
            cached: false,
        };
        if cache_allowed {
            ensure_not_cancelled(cancellation)?;
            self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(
                cache_key,
                std::iter::empty(),
                WebCacheValue::Search(response.clone()),
                validators,
            );
        }
        Ok(response)
    }

    /// Fetches one approved public HTTPS URL and returns bounded UTF-8 text.
    pub async fn fetch(
        &self,
        request: WebFetchRequest,
    ) -> Result<WebFetchResponse, WebContextError> {
        self.fetch_with_cancellation(request, &CancellationToken::new()).await
    }

    /// Fetches while observing caller cancellation.
    pub async fn fetch_with_cancellation(
        &self,
        request: WebFetchRequest,
        cancellation: &CancellationToken,
    ) -> Result<WebFetchResponse, WebContextError> {
        self.fetch_with_approved_hosts_and_cancellation(request, &BTreeSet::new(), cancellation)
            .await
    }

    /// Fetches with frontend-supplied, request-scoped host approvals.
    ///
    /// Supplied approvals are canonicalized and unioned with configured
    /// `preapproved_hosts`; configuration is never mutated. Responses retrieved
    /// with extra approvals are not cached, so later calls cannot reuse them
    /// without rechecking every redirect host.
    pub async fn fetch_with_approved_hosts(
        &self,
        request: WebFetchRequest,
        approved_hosts: &BTreeSet<String>,
    ) -> Result<WebFetchResponse, WebContextError> {
        self.fetch_with_approved_hosts_and_cancellation(
            request,
            approved_hosts,
            &CancellationToken::new(),
        )
        .await
    }

    /// Fetches with request-scoped host approvals while observing cancellation.
    pub async fn fetch_with_approved_hosts_and_cancellation(
        &self,
        request: WebFetchRequest,
        approved_hosts: &BTreeSet<String>,
        cancellation: &CancellationToken,
    ) -> Result<WebFetchResponse, WebContextError> {
        ensure_not_cancelled(cancellation)?;
        let effective_hosts = self.effective_approved_hosts(approved_hosts)?;
        let cache_allowed = effective_hosts == self.config.preapproved_hosts;
        run_with_timeout(
            cancellation,
            Duration::from_millis(self.config.limits.request_timeout_ms),
            self.fetch_with_hosts(request, &effective_hosts, cache_allowed, cancellation),
        )
        .await
    }

    async fn fetch_with_hosts(
        &self,
        request: WebFetchRequest,
        approved_hosts: &BTreeSet<String>,
        cache_allowed: bool,
        cancellation: &CancellationToken,
    ) -> Result<WebFetchResponse, WebContextError> {
        ensure_not_cancelled(cancellation)?;
        self.require_enabled()?;
        let requested_url = validate_https_url(&request.url)?;
        // Query components may contain signed URLs or bearer-like credentials.
        // They are valid request targets but never retained by the session cache.
        let cache_allowed = cache_allowed && requested_url.query().is_none();
        let requested_cache_key = WebCacheKey::Fetch {
            final_url: requested_url.to_string(),
            representation: WebRepresentation::TextV1,
        };
        let cache_lookup = cache_allowed
            .then(|| {
                self.cache
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(&requested_cache_key)
            })
            .flatten();
        if let Some(WebCacheLookup { value: WebCacheValue::Fetch(cached), stale: false, .. }) =
            &cache_lookup
        {
            ensure_not_cancelled(cancellation)?;
            let mut cached = cached.clone();
            cached.requested_url = requested_url.to_string();
            cached.cached = true;
            return Ok(cached);
        }

        let mut revalidation = None;
        let mut headers = fixed_headers();
        if let Some(lookup) = &cache_lookup
            && lookup.stale
            && !lookup.validators.is_empty()
            && let WebCacheValue::Fetch(cached) = &lookup.value
        {
            headers = fixed_headers();
            lookup.validators.apply_to(&mut headers);
            revalidation = Some((lookup.canonical_key.clone(), cached.final_url.clone()));
        }
        let request_url = match &revalidation {
            Some((_, final_url)) => validate_https_url(final_url)?,
            None => requested_url.clone(),
        };

        let _permit = self.acquire_request()?;
        let (final_url, response, redirects) = self
            .request_checked(
                WebTransportRequest {
                    method: WebTransportMethod::Get,
                    url: request_url,
                    headers,
                    body: Vec::new(),
                    max_response_bytes: self.config.limits.max_response_bytes,
                },
                approved_hosts,
                cancellation,
                true,
                false,
            )
            .await?;
        ensure_not_cancelled(cancellation)?;
        if response.status == 304 {
            let Some((canonical_key, expected_final_url)) = revalidation else {
                return Err(WebContextError::new(WebContextErrorCode::NetworkFailure));
            };
            if final_url.as_str() != expected_final_url || redirects != 0 {
                return Err(WebContextError::new(WebContextErrorCode::RedirectRejected));
            }
            let Some(WebCacheLookup {
                value: WebCacheValue::Fetch(mut cached), stale: true, ..
            }) = cache_lookup
            else {
                return Err(WebContextError::new(WebContextErrorCode::NetworkFailure));
            };
            ensure_not_cancelled(cancellation)?;
            self.cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .refresh(&canonical_key);
            cached.requested_url = requested_url.to_string();
            cached.cached = true;
            return Ok(cached);
        }
        let content_type = header_value(&response.headers, "content-type")
            .ok_or_else(|| WebContextError::new(WebContextErrorCode::UnsupportedContentType))?;
        if !is_text_mime(Some(content_type)) {
            return Err(WebContextError::new(WebContextErrorCode::UnsupportedContentType));
        }
        if response.body.len() > self.config.limits.max_response_bytes {
            return Err(WebContextError::new(WebContextErrorCode::ResponseTooLarge));
        }
        let source = std::str::from_utf8(&response.body)
            .map_err(|_| WebContextError::new(WebContextErrorCode::UnsupportedContentType))?;
        let (title, text) = if is_html_mime(content_type) {
            extract_html_text(source)
        } else {
            (None, source.to_owned())
        };
        // Keep the complete model-facing response within its configured text
        // budget: title and body combined, not body alone.
        let title = title
            .map(|title| {
                normalize_text(&title, MAX_TITLE_BYTES.min(self.config.limits.max_text_bytes))
            })
            .filter(|title| !title.is_empty());
        let remaining_text_bytes =
            self.config.limits.max_text_bytes.saturating_sub(title.as_ref().map_or(0, String::len));
        let (text, text_truncated) = truncate_utf8(&text, remaining_text_bytes);
        let validators = cache_validators(&response.headers);
        let final_url_has_query = final_url.query().is_some();
        let response = WebFetchResponse {
            requested_url: redact_url_for_display(&requested_url),
            final_url: redact_url_for_display(&final_url),
            title,
            content_type: content_type.to_owned(),
            text,
            retrieved_at_unix_ms: current_unix_millis(),
            truncated: response.body_truncated || text_truncated,
            redirects,
            cached: false,
        };
        if cache_allowed && !final_url_has_query {
            ensure_not_cancelled(cancellation)?;
            let final_cache_key = WebCacheKey::Fetch {
                final_url: response.final_url.clone(),
                representation: WebRepresentation::TextV1,
            };
            self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(
                final_cache_key,
                [requested_cache_key],
                WebCacheValue::Fetch(response.clone()),
                validators,
            );
        }
        Ok(response)
    }

    fn acquire_request(&self) -> Result<WebRequestPermit<'_>, WebContextError> {
        let mut active =
            self.active_requests.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if *active >= self.config.limits.max_concurrent_requests {
            return Err(WebContextError::new(WebContextErrorCode::NetworkFailure));
        }
        *active += 1;
        Ok(WebRequestPermit { active_requests: &self.active_requests })
    }

    fn require_enabled(&self) -> Result<(), WebContextError> {
        if self.config.enabled {
            Ok(())
        } else {
            Err(WebContextError::new(WebContextErrorCode::WebDisabled))
        }
    }

    async fn request_checked(
        &self,
        initial_request: WebTransportRequest,
        approved_hosts: &BTreeSet<String>,
        cancellation: &CancellationToken,
        allow_redirects: bool,
        allow_non_success_status: bool,
    ) -> Result<(Url, WebTransportResponse, usize), WebContextError> {
        let mut current = initial_request.url.clone();
        let mut redirects = 0;
        let mut next_request = Some(initial_request);
        loop {
            ensure_not_cancelled(cancellation)?;
            self.require_approved_host(&current, approved_hosts)?;
            let resolved = self.resolve_public_host(&current, cancellation).await?;
            let request = next_request.take().unwrap_or_else(|| WebTransportRequest {
                method: WebTransportMethod::Get,
                url: current.clone(),
                headers: fixed_headers(),
                body: Vec::new(),
                max_response_bytes: self.config.limits.max_response_bytes,
            });
            let response = self.transport.request(&request, cancellation).await?;
            ensure_not_cancelled(cancellation)?;
            if !is_public_ip(response.connected_peer)
                || !resolved.contains(&response.connected_peer)
            {
                return Err(WebContextError::new(WebContextErrorCode::DnsRejected));
            }
            // A transport sets this after consuming its decompressed-byte budget.
            // Never normalize or return a partial remote response as if complete.
            if response.body_truncated {
                return Err(WebContextError::new(WebContextErrorCode::ResponseTooLarge));
            }
            if is_redirect(response.status) {
                if !allow_redirects || redirects >= self.config.limits.max_redirects {
                    return Err(WebContextError::new(WebContextErrorCode::RedirectRejected));
                }
                let location = header_value(&response.headers, "location")
                    .ok_or_else(|| WebContextError::new(WebContextErrorCode::RedirectRejected))?;
                let redirect = current
                    .join(location)
                    .map_err(|_| WebContextError::new(WebContextErrorCode::RedirectRejected))?;
                current = validate_https_url(redirect.as_str())
                    .map_err(|_| WebContextError::new(WebContextErrorCode::RedirectRejected))?;
                next_request = Some(WebTransportRequest {
                    method: WebTransportMethod::Get,
                    url: current.clone(),
                    headers: fixed_headers(),
                    body: Vec::new(),
                    max_response_bytes: self.config.limits.max_response_bytes,
                });
                redirects += 1;
                continue;
            }
            if !allow_non_success_status
                && response.status != 304
                && !(200..300).contains(&response.status)
            {
                return Err(WebContextError::new(WebContextErrorCode::NetworkFailure));
            }
            if content_length_exceeds(&response.headers, self.config.limits.max_response_bytes) {
                return Err(WebContextError::new(WebContextErrorCode::ResponseTooLarge));
            }
            if response.body.len() > self.config.limits.max_response_bytes {
                return Err(WebContextError::new(WebContextErrorCode::ResponseTooLarge));
            }
            return Ok((current, response, redirects));
        }
    }

    /// Returns trusted provider endpoint for provenance and initial host approval only.
    /// Vendor endpoints are fixed constants; SearXNG credentials cannot appear in its URL.
    pub fn search_endpoint_url(&self) -> Result<Url, WebContextError> {
        match self.config.provider {
            WebSearchProvider::Searxng => {
                let endpoint = self.config.search_endpoint.as_deref().ok_or_else(|| {
                    WebContextError::new(WebContextErrorCode::WebSearchUnavailable)
                })?;
                validate_search_endpoint_url(endpoint)
            }
            WebSearchProvider::Exa => validate_search_endpoint_url(EXA_SEARCH_ENDPOINT),
            WebSearchProvider::BraveLlmContext => {
                validate_search_endpoint_url(BRAVE_LLM_CONTEXT_ENDPOINT)
            }
            WebSearchProvider::Tavily => validate_search_endpoint_url(TAVILY_SEARCH_ENDPOINT),
        }
    }

    fn effective_approved_hosts(
        &self,
        approved_hosts: &BTreeSet<String>,
    ) -> Result<BTreeSet<String>, WebContextError> {
        let mut effective_hosts = self.config.preapproved_hosts.clone();
        for host in approved_hosts {
            effective_hosts.insert(
                normalize_approved_host(host)
                    .map_err(|_| WebContextError::new(WebContextErrorCode::UrlRejected))?,
            );
        }
        Ok(effective_hosts)
    }

    fn require_approved_host(
        &self,
        url: &Url,
        approved_hosts: &BTreeSet<String>,
    ) -> Result<(), WebContextError> {
        let host = canonical_url_host(url)?;
        if approved_hosts.contains(&host) {
            Ok(())
        } else {
            Err(WebContextError::network_approval_required(host))
        }
    }

    async fn resolve_public_host(
        &self,
        url: &Url,
        cancellation: &CancellationToken,
    ) -> Result<Vec<IpAddr>, WebContextError> {
        ensure_not_cancelled(cancellation)?;
        let addresses = match url.host() {
            Some(Host::Ipv4(address)) => vec![IpAddr::V4(address)],
            Some(Host::Ipv6(address)) => vec![IpAddr::V6(address)],
            Some(Host::Domain(host)) => self.transport.resolve(host, cancellation).await?,
            None => return Err(WebContextError::new(WebContextErrorCode::UrlRejected)),
        };
        ensure_not_cancelled(cancellation)?;
        if addresses.is_empty() || addresses.iter().any(|address| !is_public_ip(*address)) {
            return Err(WebContextError::new(WebContextErrorCode::DnsRejected));
        }
        Ok(addresses)
    }
}

struct WebRequestPermit<'a> {
    active_requests: &'a Mutex<usize>,
}

impl Drop for WebRequestPermit<'_> {
    fn drop(&mut self) {
        let mut active =
            self.active_requests.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *active = active.saturating_sub(1);
    }
}

/// Parses a strict HTTPS URL suitable for an outbound fetch target.
pub fn validate_https_url(input: &str) -> Result<Url, WebContextError> {
    let url =
        Url::parse(input).map_err(|_| WebContextError::new(WebContextErrorCode::UrlRejected))?;
    if url.scheme() != "https"
        || url.cannot_be_a_base()
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.port_or_known_default() != Some(443)
    {
        return Err(WebContextError::new(WebContextErrorCode::UrlRejected));
    }
    Ok(url)
}

/// Parses configured SearXNG endpoint. Provider credentials belong only in the
/// secret-backed authorization header, never in a URL query component.
fn validate_search_endpoint_url(input: &str) -> Result<Url, WebContextError> {
    let url = validate_https_url(input)?;
    if url.query().is_some() {
        return Err(WebContextError::new(WebContextErrorCode::UrlRejected));
    }
    Ok(url)
}

/// Produces a provenance-safe URL. Request queries can carry signed URLs or
/// bearer-like credentials, so source records retain only origin and path.
fn redact_url_for_display(url: &Url) -> String {
    let mut redacted = url.clone();
    let _ = redacted.set_username("");
    let _ = redacted.set_password(None);
    redacted.set_query(None);
    redacted.set_fragment(None);
    redacted.to_string()
}

fn redact_url_text_for_display(input: &str) -> String {
    Url::parse(input)
        .map_or_else(|_| String::from("CONFIGURED"), |url| redact_url_for_display(&url))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;

    let digest = sha2::Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

fn canonical_url_host(url: &Url) -> Result<String, WebContextError> {
    url.host_str()
        .map(str::to_owned)
        .ok_or_else(|| WebContextError::new(WebContextErrorCode::UrlRejected))
}

/// Returns whether an IP address is globally routable enough for web retrieval.
/// Documentation, carrier-grade NAT, transition, and reserved ranges fail closed.
pub fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

/// Accepts only explicit text-like response MIME types.
fn is_html_mime(content_type: &str) -> bool {
    matches!(
        content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase().as_str(),
        "text/html" | "application/xhtml+xml"
    )
}

fn is_json_mime(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|content_type| {
        let mime = content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
        mime == "application/json" || (mime.starts_with("application/") && mime.ends_with("+json"))
    })
}

pub fn is_text_mime(content_type: Option<&str>) -> bool {
    let Some(content_type) = content_type else {
        return false;
    };
    let mime = content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    matches!(
        mime.as_str(),
        "text/plain"
            | "text/html"
            | "text/markdown"
            | "text/x-markdown"
            | "text/json"
            | "text/yaml"
            | "text/x-yaml"
            | "application/json"
            | "application/yaml"
            | "application/x-yaml"
            | "application/xml"
            | "text/xml"
    ) || (mime.starts_with("application/") && mime.ends_with("+json"))
        || (mime.starts_with("application/") && mime.ends_with("+xml"))
}

/// Extracts title and readable text from bounded HTML/XHTML with a small,
/// non-executing tokenizer. Remote text remains untrusted data.
///
/// This deliberately does not attempt browser-compatible rendering. It drops
/// subtrees with executable, interactive, inert, or hidden semantics instead
/// of preserving their text for an agent to interpret as document content.
fn extract_html_text(html: &str) -> (Option<String>, String) {
    let mut body = String::new();
    let mut title = String::new();
    let mut suppressed_depth = 0usize;
    let mut suppressed_tags: Vec<String> = Vec::new();
    let mut title_depth = 0usize;
    let mut cursor = 0usize;

    while cursor < html.len() {
        let Some(relative_start) = html[cursor..].find('<') else {
            append_html_text(
                &html[cursor..],
                suppressed_depth == 0,
                title_depth > 0,
                &mut title,
                &mut body,
            );
            break;
        };
        let start = cursor + relative_start;
        append_html_text(
            &html[cursor..start],
            suppressed_depth == 0,
            title_depth > 0,
            &mut title,
            &mut body,
        );
        let Some((end, token)) = next_html_tag(html, start) else {
            // A malformed trailing '<' is document text, never markup.
            append_html_text(
                &html[start..],
                suppressed_depth == 0,
                title_depth > 0,
                &mut title,
                &mut body,
            );
            break;
        };
        cursor = end;
        let Some(tag) = parse_html_tag(token) else {
            continue;
        };
        if tag.is_end {
            if tag.name.eq_ignore_ascii_case("title") && title_depth > 0 {
                title_depth -= 1;
            }
            if let Some(index) =
                suppressed_tags.iter().rposition(|open_tag| open_tag.eq_ignore_ascii_case(tag.name))
            {
                let removed = suppressed_tags.len() - index;
                suppressed_tags.truncate(index);
                suppressed_depth = suppressed_depth.saturating_sub(removed);
            }
            continue;
        }
        if tag.name.eq_ignore_ascii_case("title") && suppressed_depth == 0 && !tag.suppresses_text {
            title_depth += 1;
        }
        if tag.suppresses_text && !tag.self_closing {
            suppressed_depth += 1;
            suppressed_tags.push(tag.name.to_owned());
        }
        if is_html_block_tag(tag.name) {
            body.push(' ');
        }
    }

    let title = normalize_text(&decode_html_entities(&title), MAX_TITLE_BYTES);
    let body = normalize_text(&decode_html_entities(&body), MAX_TEXT_BYTES);
    (!title.is_empty()).then_some(title).map_or((None, body.clone()), |title| (Some(title), body))
}

#[derive(Debug)]
struct HtmlTag<'a> {
    name: &'a str,
    is_end: bool,
    self_closing: bool,
    suppresses_text: bool,
}

fn next_html_tag(html: &str, start: usize) -> Option<(usize, &str)> {
    let bytes = html.as_bytes();
    let mut index = start + 1;
    let mut quote = None;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(delimiter) = quote {
            if byte == delimiter {
                quote = None;
            }
        } else if matches!(byte, b'\'' | b'\"') {
            quote = Some(byte);
        } else if byte == b'>' {
            return Some((index + 1, &html[start + 1..index]));
        }
        index += 1;
    }
    None
}

fn parse_html_tag(token: &str) -> Option<HtmlTag<'_>> {
    let token = token.trim();
    if token.starts_with('!') || token.starts_with('?') {
        return None;
    }
    let (is_end, token) =
        token.strip_prefix('/').map_or((false, token), |token| (true, token.trim_start()));
    let name_end = token
        .find(|character: char| {
            !character.is_ascii_alphanumeric() && character != '-' && character != ':'
        })
        .unwrap_or(token.len());
    let name = token[..name_end].to_ascii_lowercase();
    if name.is_empty() {
        return None;
    }
    let attributes = &token[name_end..];
    let self_closing = attributes.trim_end().ends_with('/');
    let suppresses_text = is_suppressed_html_tag(&name) || html_attributes_hide_content(attributes);
    // Leak-free normalization: name is converted only to compare then borrowed
    // from the original token after validating ASCII tag syntax.
    let name = &token[..name_end];
    Some(HtmlTag { name, is_end, self_closing, suppresses_text })
}

fn is_suppressed_html_tag(name: &str) -> bool {
    [
        "script", "style", "form", "template", "noscript", "textarea", "select", "option",
        "button", "details", "dialog", "input", "output", "iframe", "object", "embed", "canvas",
        "svg",
    ]
    .into_iter()
    .any(|blocked| name.eq_ignore_ascii_case(blocked))
}

fn is_html_block_tag(name: &str) -> bool {
    [
        "p", "div", "section", "article", "main", "header", "footer", "li", "br", "tr", "h1", "h2",
        "h3", "h4", "h5", "h6",
    ]
    .into_iter()
    .any(|block| name.eq_ignore_ascii_case(block))
}

fn html_attributes_hide_content(attributes: &str) -> bool {
    let compact: String = attributes
        .chars()
        .filter(|character| !character.is_ascii_whitespace() && !matches!(character, '\'' | '\"'))
        .flat_map(char::to_lowercase)
        .collect();
    compact == "hidden"
        || compact == "inert"
        || compact.contains("hidden=")
        || compact.contains("inert=")
        || compact.contains("aria-hidden=true")
        || compact.contains("style=display:none")
        || compact.contains("style=visibility:hidden")
}

fn append_html_text(
    value: &str,
    visible: bool,
    title: bool,
    title_text: &mut String,
    body: &mut String,
) {
    if !visible {
        return;
    }
    if title {
        title_text.push_str(value);
    } else {
        body.push_str(value);
    }
}

fn decode_html_entities(value: &str) -> String {
    value
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

/// Parses and normalizes a bounded SearXNG-compatible JSON response.
pub fn parse_searxng_json(
    body: &[u8],
    max_body_bytes: usize,
    max_results: usize,
) -> Result<Vec<WebSearchResult>, WebContextError> {
    if body.len() > max_body_bytes {
        return Err(WebContextError::new(WebContextErrorCode::ResponseTooLarge));
    }
    let max_results = max_results.min(MAX_SEARCH_RESULTS);
    let response: SearxngResponse = serde_json::from_slice(body)
        .map_err(|_| WebContextError::new(WebContextErrorCode::NetworkFailure))?;
    let mut seen_urls = BTreeSet::new();
    let mut normalized = Vec::with_capacity(max_results);
    for result in response.results {
        if normalized.len() == max_results {
            break;
        }
        let Ok(url) = validate_https_url(&result.url) else {
            continue;
        };
        let canonical_url = redact_url_for_display(&url);
        if !seen_urls.insert(canonical_url.clone()) {
            continue;
        }
        let host = match url.host_str() {
            Some(host) => host.to_owned(),
            None => continue,
        };
        let title = normalize_text(&result.title, MAX_TITLE_BYTES);
        let snippet = normalize_text(&result.content, MAX_SNIPPET_BYTES);
        normalized.push(WebSearchResult {
            title: if title.is_empty() { host.clone() } else { title },
            url: canonical_url,
            host,
            snippet,
            rank: normalized.len() + 1,
        });
    }
    Ok(normalized)
}

#[derive(Debug, Deserialize)]
struct SearxngResponse {
    #[serde(default)]
    results: Vec<SearxngResult>,
}

#[derive(Debug, Deserialize)]
struct SearxngResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    content: String,
}

fn parse_exa_json(
    body: &[u8],
    max_body_bytes: usize,
    max_results: usize,
) -> Result<Vec<WebSearchResult>, WebContextError> {
    let response = provider_results(body, max_body_bytes)?;
    let results = response.get("results").and_then(serde_json::Value::as_array).expect("validated");
    if results.len() > max_results {
        return Err(provider_response_error());
    }
    let mut normalized = VendorResultNormalizer::new(max_results);
    for result in results {
        let title = required_json_string(result, "title")?;
        let url = required_json_string(result, "url")?;
        let highlights = result
            .get("highlights")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(provider_response_error)?;
        let highlight = highlights
            .first()
            .and_then(serde_json::Value::as_str)
            .ok_or_else(provider_response_error)?;
        normalized.push(title, url, highlight)?;
    }
    Ok(normalized.finish())
}

fn parse_tavily_json(
    body: &[u8],
    max_body_bytes: usize,
    max_results: usize,
) -> Result<Vec<WebSearchResult>, WebContextError> {
    let response = provider_results(body, max_body_bytes)?;
    let results = response.get("results").and_then(serde_json::Value::as_array).expect("validated");
    if results.len() > max_results {
        return Err(provider_response_error());
    }
    let mut normalized = VendorResultNormalizer::new(max_results);
    for result in results {
        let title = required_json_string(result, "title")?;
        let url = required_json_string(result, "url")?;
        let snippet = result
            .get("content")
            .and_then(serde_json::Value::as_str)
            .or_else(|| result.get("raw_content").and_then(serde_json::Value::as_str))
            .ok_or_else(provider_response_error)?;
        normalized.push(title, url, snippet)?;
    }
    Ok(normalized.finish())
}

fn parse_brave_llm_context_json(
    body: &[u8],
    max_body_bytes: usize,
    max_results: usize,
    max_snippets: usize,
) -> Result<Vec<WebSearchResult>, WebContextError> {
    if body.len() > max_body_bytes {
        return Err(WebContextError::new(WebContextErrorCode::ResponseTooLarge));
    }
    let response: serde_json::Value = serde_json::from_slice(body)
        .map_err(|_| WebContextError::new(WebContextErrorCode::NetworkFailure))?;
    let grounding = response
        .get("grounding")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(provider_response_error)?;
    let generic: &[serde_json::Value] = match grounding.get("generic") {
        Some(value) => value.as_array().ok_or_else(provider_response_error)?,
        None => &[],
    };

    let mut source_titles = BTreeMap::new();
    if let Some(sources) = response.get("sources") {
        let sources = sources.as_array().ok_or_else(provider_response_error)?;
        for source in sources {
            let Some(url) = source.get("url").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let Ok(url) = validate_https_url(url) else {
                continue;
            };
            if let Some(title) = source.get("title").and_then(serde_json::Value::as_str) {
                source_titles.insert(redact_url_for_display(&url), title.to_owned());
            }
        }
    }

    if generic.len() > max_results {
        return Err(provider_response_error());
    }

    let mut normalized = VendorResultNormalizer::new(max_results);
    let mut snippet_count = 0usize;
    for item in generic {
        let url = item
            .get("url")
            .or_else(|| item.get("source_url"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(provider_response_error)?;
        let snippets = item
            .get("snippets")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(provider_response_error)?;
        snippet_count = snippet_count.saturating_add(snippets.len());
        if snippet_count > max_snippets {
            return Err(provider_response_error());
        }
        let mut snippet = String::new();
        for source_snippet in snippets {
            let source_snippet = source_snippet
                .as_str()
                .or_else(|| source_snippet.get("text").and_then(serde_json::Value::as_str))
                .ok_or_else(provider_response_error)?;
            let normalized_snippet = normalize_text(source_snippet, MAX_SNIPPET_BYTES);
            if !snippet.is_empty() && !normalized_snippet.is_empty() {
                snippet.push(' ');
            }
            snippet.push_str(&normalized_snippet);
        }
        let canonical = validate_https_url(url).map_err(|_| provider_response_error())?;
        let canonical_url = redact_url_for_display(&canonical);
        let title = item
            .get("title")
            .and_then(serde_json::Value::as_str)
            .or_else(|| source_titles.get(&canonical_url).map(String::as_str))
            .unwrap_or_else(|| canonical.host_str().unwrap_or_default());
        normalized.push(title, url, &snippet)?;
    }
    Ok(normalized.finish())
}

fn provider_results(
    body: &[u8],
    max_body_bytes: usize,
) -> Result<serde_json::Value, WebContextError> {
    if body.len() > max_body_bytes {
        return Err(WebContextError::new(WebContextErrorCode::ResponseTooLarge));
    }
    let response: serde_json::Value = serde_json::from_slice(body)
        .map_err(|_| WebContextError::new(WebContextErrorCode::NetworkFailure))?;
    response
        .get("results")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(provider_response_error)?;
    Ok(response)
}

fn required_json_string<'a>(
    value: &'a serde_json::Value,
    field: &str,
) -> Result<&'a str, WebContextError> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(provider_response_error)
}

fn provider_response_error() -> WebContextError {
    WebContextError::new(WebContextErrorCode::NetworkFailure)
}

struct VendorResultNormalizer {
    max_results: usize,
    seen_urls: BTreeSet<String>,
    results: Vec<WebSearchResult>,
}

impl VendorResultNormalizer {
    fn new(max_results: usize) -> Self {
        Self {
            max_results: max_results.min(MAX_SEARCH_RESULTS),
            seen_urls: BTreeSet::new(),
            results: Vec::new(),
        }
    }

    fn push(&mut self, title: &str, url: &str, snippet: &str) -> Result<(), WebContextError> {
        if self.results.len() == self.max_results {
            return Ok(());
        }
        let url = validate_https_url(url).map_err(|_| provider_response_error())?;
        let canonical_url = redact_url_for_display(&url);
        if !self.seen_urls.insert(canonical_url.clone()) {
            return Ok(());
        }
        let host = canonical_url_host(&url).map_err(|_| provider_response_error())?;
        let title = normalize_text(title, MAX_TITLE_BYTES);
        if title.is_empty() {
            return Err(provider_response_error());
        }
        self.results.push(WebSearchResult {
            title,
            url: canonical_url,
            host,
            snippet: normalize_text(snippet, MAX_SNIPPET_BYTES),
            rank: self.results.len() + 1,
        });
        Ok(())
    }

    fn finish(self) -> Vec<WebSearchResult> {
        self.results
    }
}

fn normalize_approved_host(host: &str) -> Result<String, WebContextConfigError> {
    let value = host.trim();
    if value.is_empty() || value.contains('/') || value.contains('?') || value.contains('#') {
        return Err(WebContextConfigError::PreapprovedHost);
    }
    let url = Url::parse(&format!("https://{value}/"))
        .map_err(|_| WebContextConfigError::PreapprovedHost)?;
    if url.username() != "" || url.password().is_some() || url.port().is_some() {
        return Err(WebContextConfigError::PreapprovedHost);
    }
    let normalized = url.host_str().ok_or(WebContextConfigError::PreapprovedHost)?;
    if normalized != value.to_ascii_lowercase() {
        return Err(WebContextConfigError::PreapprovedHost);
    }
    Ok(normalized.to_owned())
}

fn search_headers(config: &AgentWebContextConfig) -> BTreeMap<String, String> {
    let mut headers = fixed_headers();
    if let Some(authorization) = &config.search_authorization {
        authorization.apply_to(config.provider, &mut headers);
    }
    headers
}

fn fixed_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("accept".to_owned(), "text/plain, text/html, application/json".to_owned()),
        ("user-agent".to_owned(), "ee-web-context/1".to_owned()),
    ])
}

fn header_value<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn content_length_exceeds(headers: &BTreeMap<String, String>, limit: usize) -> bool {
    header_value(headers, "content-length")
        .and_then(|value| value.trim().parse::<usize>().ok())
        .is_some_and(|size| size > limit)
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn truncate_utf8(value: &str, max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value.to_owned(), false);
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_owned(), true)
}

fn normalize_text(value: &str, max_bytes: usize) -> String {
    let (bounded, _) = truncate_utf8(value, max_bytes);
    bounded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Keeps one normalized search response within its configured aggregate text
/// budget. URLs and hosts remain intact for usable source records; later
/// snippets/results are truncated or omitted rather than retaining unbounded
/// provider content.
fn truncate_search_results(results: &mut Vec<WebSearchResult>, max_bytes: usize) -> bool {
    let mut bounded = Vec::with_capacity(results.len());
    let mut remaining = max_bytes;
    let mut truncated = false;

    for mut result in results.drain(..) {
        let metadata_bytes = result.title.len() + result.url.len() + result.host.len();
        if metadata_bytes > remaining {
            truncated = true;
            break;
        }
        remaining -= metadata_bytes;
        let (snippet, snippet_truncated) = truncate_utf8(&result.snippet, remaining);
        remaining -= snippet.len();
        result.snippet = snippet;
        bounded.push(result);
        truncated |= snippet_truncated;
    }

    *results = bounded;
    truncated
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    !matches!(
        (a, b, c),
        (0, _, _)
            | (10, _, _)
            | (100, 64..=127, _)
            | (127, _, _)
            | (169, 254, _)
            | (172, 16..=31, _)
            | (192, 0, _)
            | (192, 88, 99)
            | (192, 168, _)
            | (192, 175, 48)
            | (198, 18..=19, _)
            | (198, 51, 100)
            | (203, 0, 113)
            | (224..=255, _, _)
    )
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if address.is_unspecified() || address.is_loopback() || address.is_multicast() {
        return false;
    }
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }
    let segments = address.segments();
    if matches!(
        segments[0],
        0x0000..=0x00ff | 0x0100 | 0xfc00..=0xfdff | 0xfe80..=0xfebf | 0xfec0..=0xfeff
    ) {
        return false;
    }
    !matches!((segments[0], segments[1]), (0x2001, 0x0db8) | (0x2001, 0x0002) | (0x2001, 0x0000))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct FakeTransport {
        addresses: Vec<IpAddr>,
        responses: std::sync::Mutex<Vec<WebTransportResponse>>,
        requests: std::sync::Mutex<Vec<WebTransportRequest>>,
    }

    impl FakeTransport {
        fn new(addresses: Vec<IpAddr>, responses: Vec<WebTransportResponse>) -> Self {
            Self {
                addresses,
                responses: std::sync::Mutex::new(responses),
                requests: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl WebTransport for FakeTransport {
        fn resolve<'a>(
            &'a self,
            _host: &'a str,
            _cancellation: &'a CancellationToken,
        ) -> BoxFuture<'a, Result<Vec<IpAddr>, WebContextError>> {
            Box::pin(async move { Ok(self.addresses.clone()) })
        }

        fn request<'a>(
            &'a self,
            request: &'a WebTransportRequest,
            _cancellation: &'a CancellationToken,
        ) -> BoxFuture<'a, Result<WebTransportResponse, WebContextError>> {
            Box::pin(async move {
                self.requests.lock().unwrap().push(request.clone());
                Ok(self.responses.lock().unwrap().remove(0))
            })
        }
    }

    #[derive(Clone, Copy)]
    enum CancellationPhase {
        Dns,
        Request,
        Body,
    }

    struct BlockingTransport {
        phase: CancellationPhase,
        started: std::sync::Arc<tokio::sync::Notify>,
    }

    impl BlockingTransport {
        fn new(phase: CancellationPhase) -> Self {
            Self { phase, started: std::sync::Arc::new(tokio::sync::Notify::new()) }
        }
    }

    impl WebTransport for BlockingTransport {
        fn resolve<'a>(
            &'a self,
            _host: &'a str,
            cancellation: &'a CancellationToken,
        ) -> BoxFuture<'a, Result<Vec<IpAddr>, WebContextError>> {
            Box::pin(async move {
                if matches!(self.phase, CancellationPhase::Dns) {
                    self.started.notify_one();
                    cancellation.cancelled().await;
                    return Err(cancellation_error());
                }
                Ok(vec![public_ip()])
            })
        }

        fn request<'a>(
            &'a self,
            _request: &'a WebTransportRequest,
            cancellation: &'a CancellationToken,
        ) -> BoxFuture<'a, Result<WebTransportResponse, WebContextError>> {
            Box::pin(async move {
                if matches!(self.phase, CancellationPhase::Request | CancellationPhase::Body) {
                    self.started.notify_one();
                    cancellation.cancelled().await;
                    return Err(cancellation_error());
                }
                unreachable!("DNS phase never calls get after cancellation")
            })
        }
    }

    fn public_ip() -> IpAddr {
        "8.8.8.8".parse().unwrap()
    }

    fn config() -> AgentWebContextConfig {
        AgentWebContextConfig {
            enabled: true,
            provider: WebSearchProvider::Searxng,
            provider_options: WebSearchProviderOptions::Searxng,
            search_endpoint: Some("https://search.example/search".to_owned()),
            preapproved_hosts: BTreeSet::from([
                "search.example".to_owned(),
                "docs.example".to_owned(),
            ]),
            limits: WebContextLimits::default(),
            provider_secret_reference: None,
            browser_run_account_id: None,
            browser_run_api_token_reference: None,
            browser_run_retry: BrowserRunRetryPolicy::default(),
            search_authorization: None,
            browser_run_api_token: None,
        }
    }

    fn response(status: u16, headers: &[(&str, &str)], body: &[u8]) -> WebTransportResponse {
        WebTransportResponse {
            status,
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            body: body.to_vec(),
            body_truncated: false,
            connected_peer: public_ip(),
        }
    }

    async fn assert_cancellation_prevents_cache_insert(phase: CancellationPhase) {
        let transport = BlockingTransport::new(phase);
        let started = std::sync::Arc::clone(&transport.started);
        let service = std::sync::Arc::new(WebContextService::new(config(), transport).unwrap());
        let cancellation = CancellationToken::new();
        let task_service = std::sync::Arc::clone(&service);
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            task_service
                .fetch_with_cancellation(
                    WebFetchRequest { url: "https://docs.example/".to_owned() },
                    &task_cancellation,
                )
                .await
        });

        tokio::time::timeout(Duration::from_secs(1), started.notified())
            .await
            .expect("transport reaches cancellation phase");
        cancellation.cancel();
        assert_eq!(task.await.unwrap().unwrap_err().code, WebContextErrorCode::NetworkFailure);
        assert!(service.cache.lock().unwrap().entries.is_empty());
    }

    #[tokio::test]
    async fn cancellation_during_dns_prevents_cache_insert() {
        assert_cancellation_prevents_cache_insert(CancellationPhase::Dns).await;
    }

    #[tokio::test]
    async fn cancellation_during_request_prevents_cache_insert() {
        assert_cancellation_prevents_cache_insert(CancellationPhase::Request).await;
    }

    #[tokio::test]
    async fn cancellation_during_body_prevents_cache_insert() {
        assert_cancellation_prevents_cache_insert(CancellationPhase::Body).await;
    }

    #[tokio::test]
    async fn default_config_is_disabled() {
        assert!(!AgentWebContextConfig::default().enabled);
    }

    #[tokio::test]
    async fn preapproved_host_lookup_canonicalizes_and_rejects_invalid_hosts() {
        let service =
            WebContextService::new(config(), FakeTransport::new(Vec::new(), Vec::new())).unwrap();

        assert!(service.is_preapproved_host("DOCS.Example"));
        for invalid_host in ["", "https://docs.example/", "docs.example/path", "user@docs.example"]
        {
            assert!(!service.is_preapproved_host(invalid_host), "{invalid_host}");
        }
    }

    #[tokio::test]
    async fn error_codes_are_stable() {
        assert_eq!(WebContextErrorCode::WebDisabled.as_str(), "web_disabled");
        assert_eq!(
            serde_json::to_string(&WebContextErrorCode::NetworkApprovalRequired).unwrap(),
            "\"network_approval_required\""
        );
    }

    #[tokio::test]
    async fn strict_url_validation_rejects_unsafe_targets() {
        for url in [
            "http://example.com/",
            "file:///tmp/secret",
            "data:text/plain,hello",
            "javascript:alert(1)",
            "https://user@example.com/",
            "https://example.com/#fragment",
            "https://example.com:8443/",
            "https://",
        ] {
            assert_eq!(
                validate_https_url(url).unwrap_err().code,
                WebContextErrorCode::UrlRejected,
                "{url}"
            );
        }
        assert_eq!(validate_https_url("https://example.com:443/path").unwrap().scheme(), "https");
    }

    #[tokio::test]
    async fn public_ip_validation_fails_closed() {
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "100.64.0.1",
            "169.254.1.1",
            "192.168.1.1",
            "198.51.100.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "ff00::1",
            "2001:db8::1",
        ] {
            assert!(!is_public_ip(address.parse().unwrap()), "{address}");
        }
        assert!(is_public_ip("8.8.8.8".parse().unwrap()));
        assert!(is_public_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[tokio::test]
    async fn text_mime_validation_is_explicit() {
        assert!(is_text_mime(Some("application/problem+json; charset=utf-8")));
        assert!(is_text_mime(Some("text/markdown")));
        assert!(!is_text_mime(Some("application/octet-stream")));
        assert!(!is_text_mime(None));
    }

    #[tokio::test]
    async fn searxng_results_are_bounded_normalized_and_deduplicated() {
        let body = br#"{
            "results": [
                {"title":" First\n result ","url":"https://docs.example/a","content":" one\t two "},
                {"title":"duplicate","url":"https://docs.example/a","content":"ignored"},
                {"title":"unsafe","url":"http://127.0.0.1/","content":"ignored"},
                {"title":"second","url":"https://docs.example/b","content":"kept"}
            ]
        }"#;
        let results = parse_searxng_json(body, 4096, 2).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "First result");
        assert_eq!(results[0].snippet, "one two");
        assert_eq!(results[0].rank, 1);
        assert_eq!(results[1].url, "https://docs.example/b");
        assert_eq!(results[1].rank, 2);
    }

    #[tokio::test]
    async fn disabled_service_does_not_make_network_request() {
        let transport = FakeTransport::new(vec![public_ip()], Vec::new());
        let service = WebContextService::new(AgentWebContextConfig::default(), transport).unwrap();
        assert_eq!(
            service
                .fetch(WebFetchRequest { url: "https://docs.example/".to_owned() })
                .await
                .unwrap_err()
                .code,
            WebContextErrorCode::WebDisabled
        );
    }

    #[tokio::test]
    async fn fetch_revalidates_connected_peer_against_dns() {
        let mut network_response = response(200, &[("content-type", "text/plain")], b"ok");
        network_response.connected_peer = "1.1.1.1".parse().unwrap();
        let transport = FakeTransport::new(vec![public_ip()], vec![network_response]);
        let service = WebContextService::new(config(), transport).unwrap();
        assert_eq!(
            service
                .fetch(WebFetchRequest { url: "https://docs.example/".to_owned() })
                .await
                .unwrap_err()
                .code,
            WebContextErrorCode::DnsRejected
        );
    }

    #[tokio::test]
    async fn redirect_to_unapproved_host_requires_new_approval() {
        let transport = FakeTransport::new(
            vec![public_ip()],
            vec![response(302, &[("location", "https://unapproved.example/next")], b"")],
        );
        let service = WebContextService::new(config(), transport).unwrap();
        assert_eq!(
            service
                .fetch(WebFetchRequest { url: "https://docs.example/".to_owned() })
                .await
                .unwrap_err()
                .code,
            WebContextErrorCode::NetworkApprovalRequired
        );
    }

    #[tokio::test]
    async fn search_accepts_ephemeral_initial_host_without_mutating_config() {
        let mut config = config();
        config.preapproved_hosts.remove("search.example");
        let transport = FakeTransport::new(
            vec![public_ip()],
            vec![response(200, &[("content-type", "application/json")], br#"{"results":[]}"#)],
        );
        let service = WebContextService::new(config, transport).unwrap();
        let approved_hosts = BTreeSet::from(["SEARCH.EXAMPLE".to_owned()]);

        assert_eq!(service.search_initial_host().unwrap(), "search.example");
        service
            .search_with_approved_hosts(
                WebSearchRequest { query: "widget api".to_owned() },
                &approved_hosts,
            )
            .await
            .unwrap();

        assert!(!service.config.preapproved_hosts.contains("search.example"));
        assert_eq!(service.transport.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn ephemeral_redirect_requires_separate_host_approval() {
        let mut config = config();
        config.preapproved_hosts.remove("docs.example");
        let transport = FakeTransport::new(
            vec![public_ip()],
            vec![response(
                302,
                &[("location", "https://Unapproved.Example/next?token=provider-secret")],
                b"",
            )],
        );
        let service = WebContextService::new(config, transport).unwrap();
        let approved_hosts = BTreeSet::from(["docs.example".to_owned()]);
        let request = WebFetchRequest { url: "https://docs.example/".to_owned() };

        assert_eq!(service.fetch_initial_host(&request).unwrap(), "docs.example");
        let error = service.fetch_with_approved_hosts(request, &approved_hosts).await.unwrap_err();

        assert_eq!(error.code, WebContextErrorCode::NetworkApprovalRequired);
        assert_eq!(error.host.as_deref(), Some("unapproved.example"));
        assert!(!serde_json::to_string(&error).unwrap().contains("next?token=provider-secret"));
        assert_eq!(service.transport.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn html_fetch_strips_active_and_hidden_content() {
        let transport = FakeTransport::new(
            vec![public_ip()],
            vec![response(
                200,
                &[("content-type", "text/html; charset=utf-8")],
                br#"<html><head><title> Docs title </title><style>secret-css</style></head><body><script>ignore()</script><form>secret form</form><p>Useful docs</p><div hidden>private</div><div aria-hidden='true'>also private</div></body></html>"#,
            )],
        );
        let service = WebContextService::new(config(), transport).unwrap();
        let fetched = service
            .fetch(WebFetchRequest { url: "https://docs.example/".to_owned() })
            .await
            .unwrap();
        assert_eq!(fetched.title.as_deref(), Some("Docs title"));
        assert!(fetched.text.contains("Useful docs"));
        for omitted in ["ignore", "secret-css", "secret form", "private", "also private"] {
            assert!(!fetched.text.contains(omitted), "{omitted}");
        }
    }

    #[tokio::test]
    async fn html_tokenizer_drops_interactive_css_hidden_and_xhtml_content() {
        let (title, text) = extract_html_text(
            "<html><head><title>Visible title</title></head><body>visible <div style='display: none'>css hidden</div><textarea>typed secret</textarea><select><option>choice secret</option></select><button>button secret</button><details>details secret</details><div inert>inert secret</div></body></html>",
        );
        assert_eq!(title.as_deref(), Some("Visible title"));
        assert!(text.contains("visible"));
        for omitted in [
            "css hidden",
            "typed secret",
            "choice secret",
            "button secret",
            "details secret",
            "inert secret",
        ] {
            assert!(!text.contains(omitted), "{omitted}");
        }
        assert!(is_html_mime("application/xhtml+xml; charset=utf-8"));
    }

    #[tokio::test]
    async fn fetch_caps_title_and_body_within_configured_text_budget() {
        let mut limited = config();
        limited.limits.max_text_bytes = 12;
        let transport = FakeTransport::new(
            vec![public_ip()],
            vec![response(
                200,
                &[("content-type", "text/html")],
                b"<title>1234567890</title><p>abcdefghij</p>",
            )],
        );
        let service = WebContextService::new(limited, transport).unwrap();
        let fetched = service
            .fetch(WebFetchRequest { url: "https://docs.example/".to_owned() })
            .await
            .unwrap();
        assert!(fetched.title.as_ref().map_or(0, String::len) + fetched.text.len() <= 12);
        assert!(fetched.truncated);
    }

    #[tokio::test]
    async fn search_uses_fixed_headers_and_parses_json() {
        let transport = FakeTransport::new(
            vec![public_ip()],
            vec![response(
                200,
                &[("content-type", "application/json")],
                br#"{"results":[{"title":"Docs","url":"https://docs.example/","content":"Reference"}]}"#,
            )],
        );
        let service = WebContextService::new(config(), transport).unwrap();
        let results =
            service.search(WebSearchRequest { query: "widget api".to_owned() }).await.unwrap();
        assert!(!results.cached);
        assert_eq!(results.results[0].host, "docs.example");
        let request = service.transport.requests.lock().unwrap().remove(0);
        assert_eq!(
            request.headers.get("accept").unwrap(),
            "text/plain, text/html, application/json"
        );
        assert!(request.url.query().unwrap().contains("format=json"));
        assert!(request.url.query().unwrap().contains("q=widget+api"));
    }

    #[tokio::test]
    async fn search_authorization_is_redacted_and_only_sent_to_initial_search_request() {
        let config = config()
            .with_search_authorization(zeroize::Zeroizing::new(String::from("provider-secret")));
        assert!(!format!("{config:?}").contains("provider-secret"));
        let transport = FakeTransport::new(
            vec![public_ip()],
            vec![
                response(302, &[("location", "https://docs.example/results")], b""),
                response(200, &[("content-type", "application/json")], br#"{"results":[]}"#),
            ],
        );
        let service = WebContextService::new(config, transport).unwrap();

        service.search(WebSearchRequest { query: "widget api".to_owned() }).await.unwrap();

        let requests = service.transport.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].headers.get("authorization").map(String::as_str),
            Some("Bearer provider-secret")
        );
        assert!(!requests[1].headers.contains_key("authorization"));
        assert!(!format!("{requests:?}").contains("provider-secret"));
        assert!(!format!("{:?}", service.cache.lock().unwrap()).contains("provider-secret"));
    }

    #[tokio::test]
    async fn search_authorization_is_never_sent_to_fetch() {
        let transport = FakeTransport::new(
            vec![public_ip()],
            vec![response(200, &[("content-type", "text/plain")], b"ok")],
        );
        let service = WebContextService::new(
            config().with_search_authorization(zeroize::Zeroizing::new(String::from(
                "provider-secret",
            ))),
            transport,
        )
        .unwrap();

        service.fetch(WebFetchRequest { url: "https://docs.example/".to_owned() }).await.unwrap();

        let request = service.transport.requests.lock().unwrap().remove(0);
        assert!(!request.headers.contains_key("authorization"));
        assert!(!format!("{request:?}").contains("provider-secret"));
    }

    #[test]
    fn vendor_provider_config_uses_fixed_origin_and_requires_matching_options_and_secret() {
        let mut exa = config();
        exa.provider = WebSearchProvider::Exa;
        exa.provider_options = WebSearchProviderOptions::Exa(ExaSearchOptions::default());
        exa.search_endpoint = None;
        let missing_secret =
            WebContextService::new(exa.clone(), FakeTransport::new(vec![public_ip()], Vec::new()));
        assert!(matches!(missing_secret, Err(WebContextConfigError::ProviderAuthorization)));

        let blank_secret = WebContextService::new(
            exa.clone().with_search_authorization(Zeroizing::new(String::from(" \t "))),
            FakeTransport::new(vec![public_ip()], Vec::new()),
        );
        assert!(matches!(blank_secret, Err(WebContextConfigError::ProviderAuthorization)));

        let service = WebContextService::new(
            exa.with_search_authorization(Zeroizing::new(String::from("provider-secret"))),
            FakeTransport::new(vec![public_ip()], Vec::new()),
        )
        .unwrap();
        assert_eq!(service.search_initial_host().unwrap(), "api.exa.ai");
        assert_eq!(service.search_endpoint_url().unwrap().as_str(), EXA_SEARCH_ENDPOINT);

        let mut endpoint_mismatch = config();
        endpoint_mismatch.provider = WebSearchProvider::Tavily;
        endpoint_mismatch.provider_options =
            WebSearchProviderOptions::Tavily(TavilySearchOptions::default());
        assert!(matches!(
            WebContextService::new(
                endpoint_mismatch,
                FakeTransport::new(vec![public_ip()], Vec::new())
            ),
            Err(WebContextConfigError::ProviderEndpoint)
        ));

        let mut options_mismatch = config();
        options_mismatch.provider = WebSearchProvider::Tavily;
        options_mismatch.search_endpoint = None;
        options_mismatch = options_mismatch
            .with_search_authorization(Zeroizing::new(String::from("provider-secret")));
        assert!(matches!(
            WebContextService::new(
                options_mismatch,
                FakeTransport::new(vec![public_ip()], Vec::new())
            ),
            Err(WebContextConfigError::ProviderOptions)
        ));
    }

    #[tokio::test]
    async fn disabled_vendor_service_needs_no_secret_and_never_dispatches() {
        let mut disabled = config();
        disabled.enabled = false;
        disabled.provider = WebSearchProvider::Exa;
        disabled.provider_options = WebSearchProviderOptions::Exa(ExaSearchOptions::default());
        disabled.search_endpoint = None;
        disabled.preapproved_hosts = BTreeSet::from([String::from("api.exa.ai")]);
        let service =
            WebContextService::new(disabled, FakeTransport::new(vec![public_ip()], Vec::new()))
                .unwrap();

        assert_eq!(
            service
                .search(WebSearchRequest { query: String::from("must stay offline") })
                .await
                .unwrap_err()
                .code,
            WebContextErrorCode::WebDisabled
        );
        assert!(service.transport.requests.lock().unwrap().is_empty());
    }

    fn vendor_config(
        provider: WebSearchProvider,
        provider_options: WebSearchProviderOptions,
        host: &str,
    ) -> AgentWebContextConfig {
        let mut config = config();
        config.provider = provider;
        config.provider_options = provider_options;
        config.search_endpoint = None;
        config.preapproved_hosts = BTreeSet::from([host.to_owned(), "docs.example".to_owned()]);
        config.with_search_authorization(Zeroizing::new(String::from("provider-secret")))
    }

    fn vendor_profiles() -> [(WebSearchProvider, WebSearchProviderOptions, &'static str); 3] {
        [
            (
                WebSearchProvider::Exa,
                WebSearchProviderOptions::Exa(ExaSearchOptions::default()),
                "api.exa.ai",
            ),
            (
                WebSearchProvider::Tavily,
                WebSearchProviderOptions::Tavily(TavilySearchOptions::default()),
                "api.tavily.com",
            ),
            (
                WebSearchProvider::BraveLlmContext,
                WebSearchProviderOptions::BraveLlmContext(BraveLlmContextOptions::default()),
                "api.search.brave.com",
            ),
        ]
    }

    fn malformed_vendor_body(provider: WebSearchProvider) -> &'static [u8] {
        match provider {
            WebSearchProvider::Exa => br#"{"results":[{"title":"missing fields"}]}"#,
            WebSearchProvider::Tavily => br#"{"results":[{"title":"missing fields"}]}"#,
            WebSearchProvider::BraveLlmContext => {
                br#"{"grounding":{"generic":[{"url":"https://docs.example/"}]}}"#
            }
            WebSearchProvider::Searxng => unreachable!("vendor fixture only"),
        }
    }

    fn invalid_url_vendor_body(provider: WebSearchProvider) -> &'static [u8] {
        match provider {
            WebSearchProvider::Exa => {
                br#"{"results":[{"title":"bad","url":"http://docs.example/","highlights":["snippet"]}]}"#
            }
            WebSearchProvider::Tavily => {
                br#"{"results":[{"title":"bad","url":"http://docs.example/","content":"snippet"}]}"#
            }
            WebSearchProvider::BraveLlmContext => {
                br#"{"grounding":{"generic":[{"url":"http://docs.example/","snippets":["snippet"]}]}}"#
            }
            WebSearchProvider::Searxng => unreachable!("vendor fixture only"),
        }
    }

    #[tokio::test]
    async fn vendor_search_requests_are_posted_to_fixed_origins_and_parse_results() {
        let exa = WebContextService::new(
            vendor_config(
                WebSearchProvider::Exa,
                WebSearchProviderOptions::Exa(ExaSearchOptions::default()),
                "api.exa.ai",
            ),
            FakeTransport::new(
                vec![public_ip()],
                vec![response(
                    200,
                    &[("content-type", "application/json")],
                    br#"{"results":[{"title":" Exa docs ","url":"https://docs.example/exa?token=result-secret","highlights":[" first highlight ","ignored"]},{"title":"duplicate","url":"https://docs.example/exa","highlights":["ignored"]}]}"#,
                )],
            ),
        )
        .unwrap();
        let result = exa.search(WebSearchRequest { query: "rust api".to_owned() }).await.unwrap();
        assert_eq!(result.results.len(), 1);
        assert_eq!(result.results[0].title, "Exa docs");
        assert_eq!(result.results[0].url, "https://docs.example/exa");
        assert_eq!(result.results[0].snippet, "first highlight");
        assert_eq!(result.provenance.provider, WebSearchProvider::Exa);
        assert_eq!(result.provenance.adapter, PROVIDER_ADAPTER_VERSION);
        assert!(result.provenance.retrieved_at_unix_ms > 0);
        let request = exa.transport.requests.lock().unwrap().remove(0);
        assert_eq!(request.method, WebTransportMethod::Post);
        assert_eq!(request.url.as_str(), EXA_SEARCH_ENDPOINT);
        assert_eq!(
            request.headers.get("authorization").map(String::as_str),
            Some("Bearer provider-secret")
        );
        assert_eq!(
            request.headers.get("content-type").map(String::as_str),
            Some("application/json")
        );
        assert_eq!(request.headers.len(), 4);
        assert!(!request.headers.contains_key("cookie"));
        assert!(!request.headers.contains_key("proxy-authorization"));
        assert!(!request.headers.contains_key("x-subscription-token"));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&request.body).unwrap(),
            serde_json::json!({"query":"rust api","type":"auto","numResults":10,"contents":{"highlights":true}})
        );
        assert!(!format!("{request:?}").contains("provider-secret"));
        assert!(!format!("{request:?}").contains("rust api"));
    }

    #[tokio::test]
    async fn tavily_uses_bearer_post_and_ignores_answer() {
        let tavily = WebContextService::new(
            vendor_config(
                WebSearchProvider::Tavily,
                WebSearchProviderOptions::Tavily(TavilySearchOptions::default()),
                "api.tavily.com",
            ),
            FakeTransport::new(
                vec![public_ip()],
                vec![response(
                    200,
                    &[("content-type", "application/json")],
                    br#"{"answer":"do not use","results":[{"title":"Tavily docs","url":"https://docs.example/tavily","content":"source content","raw_content":"raw content"}]}"#,
                )],
            ),
        )
        .unwrap();
        let result =
            tavily.search(WebSearchRequest { query: "tavily api".to_owned() }).await.unwrap();
        assert_eq!(result.results[0].snippet, "source content");
        let request = tavily.transport.requests.lock().unwrap().remove(0);
        assert_eq!(request.method, WebTransportMethod::Post);
        assert_eq!(request.url.as_str(), TAVILY_SEARCH_ENDPOINT);
        assert_eq!(
            request.headers.get("authorization").map(String::as_str),
            Some("Bearer provider-secret")
        );
        assert_eq!(
            request.headers.get("content-type").map(String::as_str),
            Some("application/json")
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&request.body).unwrap(),
            serde_json::json!({"query":"tavily api","search_depth":"advanced","chunks_per_source":3,"max_results":10})
        );
        assert_eq!(request.headers.len(), 4);
        assert!(!request.headers.contains_key("cookie"));
        assert!(!request.headers.contains_key("proxy-authorization"));
        assert!(!request.headers.contains_key("x-subscription-token"));
    }

    #[tokio::test]
    async fn brave_uses_subscription_token_and_empty_grounding_succeeds() {
        let brave = WebContextService::new(
            vendor_config(
                WebSearchProvider::BraveLlmContext,
                WebSearchProviderOptions::BraveLlmContext(BraveLlmContextOptions::default()),
                "api.search.brave.com",
            ),
            FakeTransport::new(
                vec![public_ip()],
                vec![response(
                    200,
                    &[("content-type", "application/json")],
                    br#"{"grounding":{"generic":[{"url":"https://docs.example/brave","snippets":["first","second"]},{"url":"https://docs.example/brave","snippets":["duplicate"]}]},"sources":[{"url":"https://docs.example/brave","title":"Brave docs"}]}"#,
                )],
            ),
        )
        .unwrap();
        let result =
            brave.search(WebSearchRequest { query: "brave context".to_owned() }).await.unwrap();
        assert_eq!(result.results.len(), 1);
        assert_eq!(result.results[0].title, "Brave docs");
        assert_eq!(result.results[0].snippet, "first second");
        let request = brave.transport.requests.lock().unwrap().remove(0);
        assert_eq!(request.method, WebTransportMethod::Post);
        assert_eq!(request.url.as_str(), BRAVE_LLM_CONTEXT_ENDPOINT);
        assert_eq!(
            request.headers.get("x-subscription-token").map(String::as_str),
            Some("provider-secret")
        );
        assert!(!request.headers.contains_key("authorization"));
        assert_eq!(
            request.headers.get("content-type").map(String::as_str),
            Some("application/json")
        );
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["q"], "brave context");
        assert_eq!(body["enable_local"], false);
        assert_eq!(body["relevance_threshold"], 0.5);
        assert_eq!(body["freshness"], "all");
        assert_eq!(body["safesearch"], "moderate");
        assert_eq!(body["maximum_number_of_tokens_per_url"], 400);
        assert_eq!(body["maximum_number_of_snippets_per_url"], 1);
        assert_eq!(
            body.as_object().unwrap().keys().map(String::as_str).collect::<BTreeSet<_>>(),
            [
                "count",
                "enable_local",
                "freshness",
                "maximum_number_of_snippets",
                "maximum_number_of_snippets_per_url",
                "maximum_number_of_tokens",
                "maximum_number_of_tokens_per_url",
                "maximum_number_of_urls",
                "q",
                "relevance_threshold",
                "safesearch",
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(request.headers.len(), 4);
        assert!(!request.headers.contains_key("cookie"));
        assert!(!request.headers.contains_key("proxy-authorization"));
        assert!(!request.headers.contains_key("authorization"));

        let empty = WebContextService::new(
            vendor_config(
                WebSearchProvider::BraveLlmContext,
                WebSearchProviderOptions::BraveLlmContext(BraveLlmContextOptions::default()),
                "api.search.brave.com",
            ),
            FakeTransport::new(
                vec![public_ip()],
                vec![response(
                    200,
                    &[("content-type", "application/json")],
                    br#"{"grounding":{}}"#,
                )],
            ),
        )
        .unwrap();
        assert!(
            empty
                .search(WebSearchRequest { query: "empty".to_owned() })
                .await
                .unwrap()
                .results
                .is_empty()
        );
    }

    #[tokio::test]
    async fn vendor_redirect_is_rejected_without_second_credentialed_request() {
        let exa = WebContextService::new(
            vendor_config(
                WebSearchProvider::Exa,
                WebSearchProviderOptions::Exa(ExaSearchOptions::default()),
                "api.exa.ai",
            ),
            FakeTransport::new(
                vec![public_ip()],
                vec![
                    response(302, &[("location", "https://docs.example/redirect")], b""),
                    response(200, &[("content-type", "application/json")], br#"{"results":[]}"#),
                ],
            ),
        )
        .unwrap();
        assert_eq!(
            exa.search(WebSearchRequest { query: "no redirect".to_owned() })
                .await
                .unwrap_err()
                .code,
            WebContextErrorCode::RedirectRejected
        );
        let requests = exa.transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].headers.get("authorization").map(String::as_str),
            Some("Bearer provider-secret")
        );
    }

    #[tokio::test]
    async fn vendor_failure_is_explicit_and_never_falls_back() {
        let exa = WebContextService::new(
            vendor_config(
                WebSearchProvider::Exa,
                WebSearchProviderOptions::Exa(ExaSearchOptions::default()),
                "api.exa.ai",
            ),
            FakeTransport::new(
                vec![public_ip()],
                vec![response(429, &[("content-type", "application/json")], br#"{}"#)],
            ),
        )
        .unwrap();

        assert_eq!(
            exa.search(WebSearchRequest { query: "rate limited".to_owned() })
                .await
                .unwrap_err()
                .code,
            WebContextErrorCode::NetworkFailure
        );
        assert_eq!(exa.transport.requests.lock().unwrap().len(), 1);
        assert!(exa.cache.lock().unwrap().entries.is_empty());
    }

    #[tokio::test]
    async fn every_vendor_fails_closed_without_cache_or_fallback() {
        for (provider, options, host) in vendor_profiles() {
            let mut peer_mismatch = response(200, &[("content-type", "application/json")], b"{}");
            peer_mismatch.connected_peer = "1.1.1.1".parse().unwrap();
            let mut oversized = response(200, &[("content-type", "application/json")], b"{}");
            oversized.body_truncated = true;
            let scenarios = vec![
                (
                    "unauthorized",
                    response(401, &[("content-type", "application/json")], b"{}"),
                    WebContextErrorCode::NetworkFailure,
                ),
                (
                    "rate limited",
                    response(429, &[("content-type", "application/json")], b"{}"),
                    WebContextErrorCode::NetworkFailure,
                ),
                (
                    "server failure",
                    response(500, &[("content-type", "application/json")], b"{}"),
                    WebContextErrorCode::NetworkFailure,
                ),
                (
                    "redirect",
                    response(302, &[("location", "https://docs.example/redirect")], b""),
                    WebContextErrorCode::RedirectRejected,
                ),
                (
                    "wrong MIME",
                    response(200, &[("content-type", "text/plain")], b"{}"),
                    WebContextErrorCode::UnsupportedContentType,
                ),
                ("oversized body", oversized, WebContextErrorCode::ResponseTooLarge),
                (
                    "malformed JSON",
                    response(200, &[("content-type", "application/json")], b"{"),
                    WebContextErrorCode::NetworkFailure,
                ),
                (
                    "malformed result",
                    response(
                        200,
                        &[("content-type", "application/json")],
                        malformed_vendor_body(provider),
                    ),
                    WebContextErrorCode::NetworkFailure,
                ),
                (
                    "cross-scheme result",
                    response(
                        200,
                        &[("content-type", "application/json")],
                        invalid_url_vendor_body(provider),
                    ),
                    WebContextErrorCode::NetworkFailure,
                ),
                ("connected peer mismatch", peer_mismatch, WebContextErrorCode::DnsRejected),
            ];

            for (name, transport_response, expected) in scenarios {
                let service = WebContextService::new(
                    vendor_config(provider, options.clone(), host),
                    FakeTransport::new(vec![public_ip()], vec![transport_response]),
                )
                .unwrap();

                let error = service
                    .search(WebSearchRequest { query: format!("{name} query") })
                    .await
                    .unwrap_err();
                assert_eq!(error.code, expected, "{}: {name}", provider.id());
                assert_eq!(
                    service.transport.requests.lock().unwrap().len(),
                    1,
                    "{}: {name}",
                    provider.id()
                );
                assert!(
                    service.cache.lock().unwrap().entries.is_empty(),
                    "{}: {name}",
                    provider.id()
                );
            }

            let private_dns = WebContextService::new(
                vendor_config(provider, options, host),
                FakeTransport::new(vec!["127.0.0.1".parse().unwrap()], Vec::new()),
            )
            .unwrap();
            assert_eq!(
                private_dns
                    .search(WebSearchRequest { query: "private DNS".to_owned() })
                    .await
                    .unwrap_err()
                    .code,
                WebContextErrorCode::DnsRejected,
                "{} private DNS",
                provider.id()
            );
            assert!(private_dns.transport.requests.lock().unwrap().is_empty());
            assert!(private_dns.cache.lock().unwrap().entries.is_empty());
        }
    }

    #[tokio::test]
    async fn provider_decoders_reject_results_or_grounding_beyond_configured_bounds() {
        let exa = WebContextService::new(
            vendor_config(
                WebSearchProvider::Exa,
                WebSearchProviderOptions::Exa(ExaSearchOptions {
                    max_results: 1,
                    ..ExaSearchOptions::default()
                }),
                "api.exa.ai",
            ),
            FakeTransport::new(
                vec![public_ip()],
                vec![response(
                    200,
                    &[("content-type", "application/json")],
                    br#"{"results":[{"title":"one","url":"https://docs.example/one","highlights":["one"]},{"title":"two","url":"https://docs.example/two","highlights":["two"]}]}"#,
                )],
            ),
        )
        .unwrap();
        assert_eq!(
            exa.search(WebSearchRequest { query: String::from("bounded results") })
                .await
                .unwrap_err()
                .code,
            WebContextErrorCode::NetworkFailure
        );
        assert!(exa.cache.lock().unwrap().entries.is_empty());

        let brave = WebContextService::new(
            vendor_config(
                WebSearchProvider::BraveLlmContext,
                WebSearchProviderOptions::BraveLlmContext(BraveLlmContextOptions {
                    max_snippets: 1,
                    ..BraveLlmContextOptions::default()
                }),
                "api.search.brave.com",
            ),
            FakeTransport::new(
                vec![public_ip()],
                vec![response(
                    200,
                    &[("content-type", "application/json")],
                    br#"{"grounding":{"generic":[{"url":"https://docs.example/brave","snippets":["one","two"]}]}}"#,
                )],
            ),
        )
        .unwrap();
        assert_eq!(
            brave
                .search(WebSearchRequest { query: String::from("bounded grounding") })
                .await
                .unwrap_err()
                .code,
            WebContextErrorCode::NetworkFailure
        );
        assert!(brave.cache.lock().unwrap().entries.is_empty());
    }

    #[tokio::test]
    async fn vendor_search_aggregate_text_budget_truncates_untrusted_snippets() {
        let mut limited = vendor_config(
            WebSearchProvider::Exa,
            WebSearchProviderOptions::Exa(ExaSearchOptions::default()),
            "api.exa.ai",
        );
        limited.limits.max_text_bytes = 100;
        let upstream_id = "provider-request-id";
        let service = WebContextService::new(
            limited,
            FakeTransport::new(
                vec![public_ip()],
                vec![response(
                    200,
                    &[("content-type", "application/json"), ("x-request-id", upstream_id)],
                    br#"{"results":[{"title":"First source","url":"https://docs.example/one","highlights":["abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz"]},{"title":"Second source","url":"https://docs.example/two","highlights":["ignored after aggregate cap"]}],"output":"untrusted-provider-output"}"#,
                )],
            ),
        )
        .unwrap();

        let response = service
            .search(WebSearchRequest { query: "aggregate truncation".to_owned() })
            .await
            .unwrap();
        let output_bytes = response
            .results
            .iter()
            .map(|result| {
                result.title.len() + result.url.len() + result.host.len() + result.snippet.len()
            })
            .sum::<usize>();
        assert!(response.truncated);
        assert!(output_bytes <= 100);
        assert_eq!(response.results.len(), 1);
        assert!(response.results[0].snippet.len() < MAX_SNIPPET_BYTES);
        assert!(!format!("{response:?}").contains(upstream_id));
        assert!(!format!("{:?}", service.cache.lock().unwrap()).contains(upstream_id));
        assert!(
            !format!("{:?}", service.cache.lock().unwrap()).contains("untrusted-provider-output")
        );
    }

    #[tokio::test]
    async fn vendor_malformed_results_fail_without_cache_and_cache_keys_are_separate() {
        let exa = WebContextService::new(
            vendor_config(
                WebSearchProvider::Exa,
                WebSearchProviderOptions::Exa(ExaSearchOptions::default()),
                "api.exa.ai",
            ),
            FakeTransport::new(
                vec![public_ip()],
                vec![response(
                    200,
                    &[("content-type", "application/json")],
                    br#"{"results":[{"url":"https://docs.example/"}]}"#,
                )],
            ),
        )
        .unwrap();
        assert_eq!(
            exa.search(WebSearchRequest { query: "same query".to_owned() }).await.unwrap_err().code,
            WebContextErrorCode::NetworkFailure
        );
        assert!(exa.cache.lock().unwrap().entries.is_empty());
        let query_digest = sha256_hex(b"same query");
        assert_ne!(
            WebCacheKey::Search {
                provider_identity: "searxng:https://search.example/search:v1".to_owned(),
                options_digest: sha256_hex(b"Searxng"),
                query_digest: query_digest.clone()
            },
            WebCacheKey::Search {
                provider_identity: "exa:https://api.exa.ai/search:v1".to_owned(),
                options_digest: provider_options_digest(&WebSearchProviderOptions::Exa(
                    ExaSearchOptions::default()
                )),
                query_digest
            },
        );

        let provider_keys = [
            WebCacheKey::Search {
                provider_identity: "searxng:https://search.example/search:v1".to_owned(),
                options_digest: provider_options_digest(&WebSearchProviderOptions::Searxng),
                query_digest: sha256_hex(b"same query"),
            },
            WebCacheKey::Search {
                provider_identity: "exa:https://api.exa.ai/search:v1".to_owned(),
                options_digest: provider_options_digest(&WebSearchProviderOptions::Exa(
                    ExaSearchOptions::default(),
                )),
                query_digest: sha256_hex(b"same query"),
            },
            WebCacheKey::Search {
                provider_identity: "tavily:https://api.tavily.com/search:v1".to_owned(),
                options_digest: provider_options_digest(&WebSearchProviderOptions::Tavily(
                    TavilySearchOptions::default(),
                )),
                query_digest: sha256_hex(b"same query"),
            },
            WebCacheKey::Search {
                provider_identity:
                    "brave_llm_context:https://api.search.brave.com/res/v1/llm/context:v1"
                        .to_owned(),
                options_digest: provider_options_digest(
                    &WebSearchProviderOptions::BraveLlmContext(BraveLlmContextOptions::default()),
                ),
                query_digest: sha256_hex(b"same query"),
            },
        ];
        assert_eq!(provider_keys.into_iter().collect::<BTreeSet<_>>().len(), 4);

        let exa_auto =
            provider_options_digest(&WebSearchProviderOptions::Exa(ExaSearchOptions::default()));
        let exa_neural =
            provider_options_digest(&WebSearchProviderOptions::Exa(ExaSearchOptions {
                search_mode: ExaSearchMode::Neural,
                ..ExaSearchOptions::default()
            }));
        assert_ne!(exa_auto, exa_neural, "semantic provider options must partition cache keys");
    }

    #[tokio::test]
    async fn search_queries_are_bounded_before_transport() {
        let search =
            WebContextService::new(config(), FakeTransport::new(vec![public_ip()], Vec::new()))
                .unwrap();
        assert_eq!(
            search.search(WebSearchRequest { query: "  \t ".to_owned() }).await.unwrap_err().code,
            WebContextErrorCode::UrlRejected
        );
        assert_eq!(
            search
                .search(WebSearchRequest { query: "x".repeat(MAX_SEARCH_QUERY_BYTES + 1) })
                .await
                .unwrap_err()
                .code,
            WebContextErrorCode::UrlRejected
        );
        assert!(search.transport.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn search_cache_returns_normalized_query_hit() {
        let transport = FakeTransport::new(
            vec![public_ip()],
            vec![response(
                200,
                &[("content-type", "application/json")],
                br#"{"results":[{"title":"Docs","url":"https://docs.example/","content":"Reference"}]}"#,
            )],
        );
        let service = WebContextService::new(config(), transport).unwrap();

        let first =
            service.search(WebSearchRequest { query: "widget   api".to_owned() }).await.unwrap();
        let second =
            service.search(WebSearchRequest { query: "widget api".to_owned() }).await.unwrap();

        assert!(!first.cached);
        assert!(second.cached);
        assert_eq!(first.provenance.provider, WebSearchProvider::Searxng);
        assert_eq!(first.provenance.adapter, PROVIDER_ADAPTER_VERSION);
        assert_eq!(first.provenance, second.provenance);
        assert_eq!(service.transport.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn fetch_cache_preserves_original_retrieval_time() {
        let transport = FakeTransport::new(
            vec![public_ip()],
            vec![response(200, &[("content-type", "text/plain")], b"documentation")],
        );
        let service = WebContextService::new(config(), transport).unwrap();
        let request = WebFetchRequest { url: "https://docs.example/reference".to_owned() };

        let first = service.fetch(request.clone()).await.unwrap();
        let second = service.fetch(request).await.unwrap();

        assert!(!first.cached);
        assert!(second.cached);
        assert!(first.retrieved_at_unix_ms > 0);
        assert_eq!(first.retrieved_at_unix_ms, second.retrieved_at_unix_ms);
        assert_eq!(service.transport.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cache_marks_expired_entries_stale_and_evicts_least_recently_used_entries() {
        let mut cache =
            WebContextCache::new(1, 1024, 1024, Duration::from_secs(60), Duration::from_secs(60));
        let value = WebCacheValue::Search(WebSearchResponse {
            results: Vec::new(),
            provenance: WebSearchProvenance::for_provider(WebSearchProvider::Searxng),
            truncated: false,
            cached: false,
        });
        let first = WebCacheKey::Fetch {
            final_url: "https://docs.example/first".to_owned(),
            representation: WebRepresentation::TextV1,
        };
        let second = WebCacheKey::Fetch {
            final_url: "https://docs.example/second".to_owned(),
            representation: WebRepresentation::TextV1,
        };

        cache.insert(first.clone(), std::iter::empty(), value.clone(), CacheValidators::default());
        cache.entries.front_mut().unwrap().fresh_until =
            Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        assert!(cache.get(&first).is_some_and(|lookup| lookup.stale));

        cache.insert(second.clone(), std::iter::empty(), value, CacheValidators::default());
        assert!(cache.get(&first).is_none());
        assert!(cache.get(&second).is_some());
    }

    #[tokio::test]
    async fn stale_fetch_revalidates_with_safe_validators_and_reuses_cached_text_on_not_modified() {
        let transport = FakeTransport::new(
            vec![public_ip()],
            vec![
                response(
                    200,
                    &[("content-type", "text/plain"), ("etag", "\"source-v1\"")],
                    b"documentation",
                ),
                response(304, &[], b""),
            ],
        );
        let service = WebContextService::new(config(), transport).unwrap();
        let request = WebFetchRequest { url: "https://docs.example/reference".to_owned() };

        let first = service.fetch(request.clone()).await.unwrap();
        service.cache.lock().unwrap().entries.front_mut().unwrap().fresh_until =
            Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        let second = service.fetch(request).await.unwrap();

        assert!(!first.cached);
        assert!(second.cached);
        assert_eq!(second.text, "documentation");
        assert_eq!(first.retrieved_at_unix_ms, second.retrieved_at_unix_ms);
        let requests = service.transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].headers.get("if-none-match"), Some(&"\"source-v1\"".to_owned()));
        assert!(!requests[1].headers.contains_key("authorization"));
    }

    #[tokio::test]
    async fn missing_search_backend_and_unapproved_hosts_fail_before_transport() {
        let mut missing_backend = config();
        missing_backend.search_endpoint = None;
        let search = WebContextService::new(
            missing_backend,
            FakeTransport::new(vec![public_ip()], Vec::new()),
        )
        .unwrap();
        assert_eq!(
            search
                .search(WebSearchRequest { query: "rust docs".to_owned() })
                .await
                .unwrap_err()
                .code,
            WebContextErrorCode::WebSearchUnavailable
        );
        assert!(search.transport.requests.lock().unwrap().is_empty());

        let fetch =
            WebContextService::new(config(), FakeTransport::new(vec![public_ip()], Vec::new()))
                .unwrap();
        assert_eq!(
            fetch
                .fetch(WebFetchRequest { url: "https://unapproved.example/".to_owned() })
                .await
                .unwrap_err()
                .code,
            WebContextErrorCode::NetworkApprovalRequired
        );
        assert!(fetch.transport.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn private_dns_and_truncated_decompressed_bodies_fail_closed() {
        let private_dns = WebContextService::new(
            config(),
            FakeTransport::new(vec!["127.0.0.1".parse().unwrap()], Vec::new()),
        )
        .unwrap();
        assert_eq!(
            private_dns
                .fetch(WebFetchRequest { url: "https://docs.example/".to_owned() })
                .await
                .unwrap_err()
                .code,
            WebContextErrorCode::DnsRejected
        );
        assert!(private_dns.transport.requests.lock().unwrap().is_empty());

        let mut capped = response(200, &[("content-type", "text/plain")], b"partial");
        capped.body_truncated = true;
        let oversized =
            WebContextService::new(config(), FakeTransport::new(vec![public_ip()], vec![capped]))
                .unwrap();
        assert_eq!(
            oversized
                .fetch(WebFetchRequest { url: "https://docs.example/".to_owned() })
                .await
                .unwrap_err()
                .code,
            WebContextErrorCode::ResponseTooLarge
        );
        assert!(oversized.cache.lock().unwrap().entries.is_empty());
    }

    #[tokio::test]
    async fn malformed_search_json_and_binary_fetch_are_rejected_without_cache_entries() {
        let search = WebContextService::new(
            config(),
            FakeTransport::new(
                vec![public_ip()],
                vec![response(200, &[("content-type", "application/json")], b"{not-json")],
            ),
        )
        .unwrap();
        assert_eq!(
            search
                .search(WebSearchRequest { query: "rust docs".to_owned() })
                .await
                .unwrap_err()
                .code,
            WebContextErrorCode::NetworkFailure
        );
        assert!(search.cache.lock().unwrap().entries.is_empty());

        let fetch = WebContextService::new(
            config(),
            FakeTransport::new(
                vec![public_ip()],
                vec![response(200, &[("content-type", "application/octet-stream")], b"binary")],
            ),
        )
        .unwrap();
        assert_eq!(
            fetch
                .fetch(WebFetchRequest { url: "https://docs.example/download".to_owned() })
                .await
                .unwrap_err()
                .code,
            WebContextErrorCode::UnsupportedContentType
        );
        assert!(fetch.cache.lock().unwrap().entries.is_empty());
    }

    #[tokio::test]
    async fn redirects_stop_at_configured_cap_without_forwarding_search_credentials() {
        let mut limited = config()
            .with_search_authorization(zeroize::Zeroizing::new(String::from("provider-secret")));
        limited.limits.max_redirects = 1;
        let service = WebContextService::new(
            limited,
            FakeTransport::new(
                vec![public_ip()],
                vec![
                    response(302, &[("location", "https://search.example/next")], b""),
                    response(302, &[("location", "https://search.example/final")], b""),
                ],
            ),
        )
        .unwrap();

        assert_eq!(
            service
                .search(WebSearchRequest { query: "rust docs".to_owned() })
                .await
                .unwrap_err()
                .code,
            WebContextErrorCode::RedirectRejected
        );
        let requests = service.transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].headers.contains_key("authorization"));
        assert!(!requests[1].headers.contains_key("authorization"));
        assert!(!requests.iter().any(|request| request.headers.contains_key("cookie")));
    }

    #[tokio::test]
    async fn configured_search_endpoint_rejects_query_credentials_and_debug_redacts_them() {
        let mut invalid = config();
        invalid.search_endpoint =
            Some("https://search.example/search?api_key=provider-secret".to_owned());

        assert!(!format!("{invalid:?}").contains("provider-secret"));
        assert!(matches!(
            WebContextService::new(invalid, FakeTransport::new(vec![public_ip()], Vec::new())),
            Err(WebContextConfigError::SearchEndpoint)
        ));
    }

    #[tokio::test]
    async fn query_bearing_urls_are_requested_but_never_returned_or_cached() {
        let secret = "signed-url-secret";
        let service = WebContextService::new(
            config(),
            FakeTransport::new(
                vec![public_ip()],
                vec![response(200, &[("content-type", "text/plain")], b"documentation")],
            ),
        )
        .unwrap();
        let fetched = service
            .fetch(WebFetchRequest {
                url: format!("https://docs.example/reference?token={secret}"),
            })
            .await
            .unwrap();

        assert_eq!(fetched.requested_url, "https://docs.example/reference");
        assert_eq!(fetched.final_url, "https://docs.example/reference");
        assert!(!format!("{fetched:?}").contains(secret));
        assert!(!format!("{:?}", service.cache.lock().unwrap()).contains(secret));
        assert_eq!(
            service.transport.requests.lock().unwrap()[0].url.query(),
            Some("token=signed-url-secret")
        );
    }

    #[tokio::test]
    async fn search_cache_hashes_query_and_redacts_result_url_queries() {
        let secret = "search-query-secret";
        let service = WebContextService::new(
            config(),
            FakeTransport::new(
                vec![public_ip()],
                vec![response(
                    200,
                    &[("content-type", "application/json")],
                    br#"{"results":[{"title":"Docs","url":"https://docs.example/reference?token=result-url-secret","content":"Reference"}]}"#,
                )],
            ),
        )
        .unwrap();

        let first = service.search(WebSearchRequest { query: secret.to_owned() }).await.unwrap();
        let second = service.search(WebSearchRequest { query: secret.to_owned() }).await.unwrap();

        assert_eq!(first.results[0].url, "https://docs.example/reference");
        assert!(second.cached);
        let cache = format!("{:?}", service.cache.lock().unwrap());
        assert!(!cache.contains(secret));
        assert!(!cache.contains("result-url-secret"));
    }

    #[tokio::test]
    async fn browser_run_uses_fixed_cloudflare_endpoint_and_redacts_target_query() {
        let mut browser_config = config();
        browser_config.browser_run_account_id =
            Some(String::from("0123456789abcdef0123456789abcdef"));
        browser_config.browser_run_api_token_reference = Some(String::from("secret://browser-run"));
        browser_config =
            browser_config.with_browser_run_api_token(Zeroizing::new(String::from("token-value")));
        let service = WebContextService::new(
            browser_config,
            FakeTransport::new(
                vec![public_ip()],
                vec![response(
                    200,
                    &[("content-type", "application/json")],
                    br##"{"success":true,"result":"# Rendered"}"##,
                )],
            ),
        )
        .unwrap();
        let result = service
            .browser_run_with_approved_hosts_and_cancellation(
                ee_mcp::BrowserRunRequest {
                    action: ee_mcp::BrowserRunAction::Markdown,
                    url: String::from("https://docs.example/page?secret=query-value"),
                    selector: None,
                    prompt: None,
                },
                &BTreeSet::new(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(result.requested_url, "https://docs.example/page");
        assert_eq!(result.result, serde_json::Value::String(String::from("# Rendered")));
        let request = &service.transport.requests.lock().unwrap()[0];
        assert_eq!(request.url.host_str(), Some("api.cloudflare.com"));
        assert_eq!(
            request.url.path(),
            "/client/v4/accounts/0123456789abcdef0123456789abcdef/browser-rendering/markdown"
        );
        assert_eq!(request.headers["authorization"], "Bearer token-value");
        assert!(
            std::str::from_utf8(&request.body)
                .unwrap()
                .contains("docs.example/page?secret=query-value")
        );
        assert!(!format!("{:?}", service.config).contains("token-value"));
    }

    #[tokio::test]
    async fn browser_run_retries_transient_cloudflare_rate_limit_with_capped_retry_after() {
        let mut browser_config = config();
        browser_config.browser_run_account_id =
            Some(String::from("0123456789abcdef0123456789abcdef"));
        browser_config.browser_run_api_token_reference = Some(String::from("secret://browser-run"));
        browser_config.browser_run_retry =
            BrowserRunRetryPolicy { max_attempts: 2, base_delay_ms: 1, max_delay_ms: 1 };
        browser_config =
            browser_config.with_browser_run_api_token(Zeroizing::new(String::from("token-value")));
        let service = WebContextService::new(
            browser_config,
            FakeTransport::new(
                vec![public_ip()],
                vec![
                    response(429, &[("retry-after", "0")], br#"{"success":false}"#),
                    response(
                        200,
                        &[("content-type", "application/json")],
                        br##"{"success":true,"result":"retried"}"##,
                    ),
                ],
            ),
        )
        .unwrap();
        let result = service
            .browser_run_with_approved_hosts_and_cancellation(
                ee_mcp::BrowserRunRequest {
                    action: ee_mcp::BrowserRunAction::Markdown,
                    url: String::from("https://docs.example/page"),
                    selector: None,
                    prompt: None,
                },
                &BTreeSet::new(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(result.result, serde_json::Value::String(String::from("retried")));
        assert_eq!(service.transport.requests.lock().unwrap().len(), 2);
    }
}
