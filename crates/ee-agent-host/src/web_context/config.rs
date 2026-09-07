//! Web-context module: config.
use super::*;

/// Resolved, trusted configuration for optional agent web context.
///
/// Configuration resolution belongs to frontend code. This host-side type
/// contains semantic values only. Resolved provider credentials stay private and
/// are only attached to first-party configured search requests.
#[derive(Clone)]
pub struct AgentWebContextConfig {
    /// Web retrieval is enabled by default and can still be explicitly disabled.
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
    pub(super) search_authorization: Option<SearchAuthorization>,
    pub(super) browser_run_api_token: Option<Zeroizing<String>>,
}

/// Opaque provider credential. It has neither equality nor a value-revealing formatter.
#[derive(Clone)]
pub(super) struct SearchAuthorization(Zeroizing<String>);

impl SearchAuthorization {
    pub(super) fn is_blank(&self) -> bool {
        self.0.trim().is_empty()
    }

    pub(super) fn apply_to(
        &self,
        provider: WebSearchProvider,
        headers: &mut BTreeMap<String, String>,
    ) {
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
    pub(super) fn validate(self) -> Result<(), WebContextConfigError> {
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
    pub(super) fn validate(&self) -> Result<(), WebContextConfigError> {
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
