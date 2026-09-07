//! Web-context module: vendors.
use super::normalize::fixed_headers;
use super::normalize::normalize_text;
use super::normalize::search_headers;
use super::*;

pub(super) const PROVIDER_ADAPTER_VERSION: &str = "v1";

/// Internal boundary between provider-specific JSON and shared web safety policy.
/// It is selected from trusted configuration when the service is constructed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProviderSearchAdapter {
    Searxng,
    Exa,
    BraveLlmContext,
    Tavily,
}

impl ProviderSearchAdapter {
    pub(super) fn for_provider(provider: WebSearchProvider) -> Self {
        match provider {
            WebSearchProvider::Searxng => Self::Searxng,
            WebSearchProvider::Exa => Self::Exa,
            WebSearchProvider::BraveLlmContext => Self::BraveLlmContext,
            WebSearchProvider::Tavily => Self::Tavily,
        }
    }

    pub(super) fn permits_redirects(self) -> bool {
        matches!(self, Self::Searxng)
    }

    pub(super) fn permits_revalidation(self) -> bool {
        matches!(self, Self::Searxng)
    }

    pub(super) fn cache_identity(self, endpoint: &Url) -> String {
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

    pub(super) fn build_request(
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

    pub(super) fn vendor_request(
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

    pub(super) fn result_limit(self, config: &AgentWebContextConfig) -> usize {
        match &config.provider_options {
            WebSearchProviderOptions::Searxng => config.limits.max_search_results,
            WebSearchProviderOptions::Exa(options) => options.max_results,
            WebSearchProviderOptions::BraveLlmContext(options) => options.max_results,
            WebSearchProviderOptions::Tavily(options) => options.max_results,
        }
    }

    pub(super) fn parse_response(
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

    pub(super) fn max_brave_snippets(self, config: &AgentWebContextConfig) -> usize {
        match &config.provider_options {
            WebSearchProviderOptions::BraveLlmContext(options) => options.max_snippets,
            _ => 0,
        }
    }
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
pub(super) struct SearxngResponse {
    #[serde(default)]
    pub(super) results: Vec<SearxngResult>,
}

#[derive(Debug, Deserialize)]
pub(super) struct SearxngResult {
    #[serde(default)]
    pub(super) title: String,
    #[serde(default)]
    pub(super) url: String,
    #[serde(default)]
    pub(super) content: String,
}

pub(super) fn parse_exa_json(
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

pub(super) fn parse_tavily_json(
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

pub(super) fn parse_brave_llm_context_json(
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

pub(super) fn provider_results(
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

pub(super) fn required_json_string<'a>(
    value: &'a serde_json::Value,
    field: &str,
) -> Result<&'a str, WebContextError> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(provider_response_error)
}

pub(super) fn provider_response_error() -> WebContextError {
    WebContextError::new(WebContextErrorCode::NetworkFailure)
}

pub(super) struct VendorResultNormalizer {
    pub(super) max_results: usize,
    pub(super) seen_urls: BTreeSet<String>,
    pub(super) results: Vec<WebSearchResult>,
}

impl VendorResultNormalizer {
    pub(super) fn new(max_results: usize) -> Self {
        Self {
            max_results: max_results.min(MAX_SEARCH_RESULTS),
            seen_urls: BTreeSet::new(),
            results: Vec::new(),
        }
    }

    pub(super) fn push(
        &mut self,
        title: &str,
        url: &str,
        snippet: &str,
    ) -> Result<(), WebContextError> {
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

    pub(super) fn finish(self) -> Vec<WebSearchResult> {
        self.results
    }
}
