//! Web-context module: service.
use super::cache::CacheValidators;
use super::cache::WebCacheKey;
use super::cache::WebCacheLookup;
use super::cache::WebCacheValue;
use super::cache::WebContextCache;
use super::cache::WebRepresentation;
use super::cache::browser_run_retry_delay;
use super::cache::browser_run_retryable_status;
use super::cache::cache_validators;
use super::cache::current_unix_millis;
use super::cache::is_cloudflare_account_id;
use super::cache::normalize_search_query;
use super::cache::provider_options_digest;
use super::cache::validate_search_query;
use super::html::extract_html_text;
use super::html::is_html_mime;
use super::html::is_json_mime;
use super::html::is_text_mime;
use super::normalize::content_length_exceeds;
use super::normalize::fixed_headers;
use super::normalize::header_value;
use super::normalize::is_redirect;
use super::normalize::normalize_approved_host;
use super::normalize::normalize_text;
use super::normalize::truncate_search_results;
use super::normalize::truncate_utf8;
use super::*;

/// Safe remote retrieval service. It has no CLI, proxy, or UI wiring.
pub struct WebContextService<T> {
    pub(super) config: AgentWebContextConfig,
    pub(super) search_adapter: ProviderSearchAdapter,
    pub(super) transport: T,
    pub(super) cache: Mutex<WebContextCache>,
    pub(super) active_requests: Mutex<usize>,
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

pub(super) struct WebRequestPermit<'a> {
    pub(super) active_requests: &'a Mutex<usize>,
}

impl Drop for WebRequestPermit<'_> {
    fn drop(&mut self) {
        let mut active =
            self.active_requests.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *active = active.saturating_sub(1);
    }
}
