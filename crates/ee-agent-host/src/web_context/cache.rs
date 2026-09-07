//! Web-context module: cache.
use super::normalize::header_value;
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum WebCacheKey {
    Search { provider_identity: String, options_digest: String, query_digest: String },
    Fetch { final_url: String, representation: WebRepresentation },
}

/// Accepted remote representation. Keep this fixed and host-owned so cache keys
/// cannot be influenced by model-provided headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum WebRepresentation {
    TextV1,
}

#[derive(Debug, Clone)]
pub(super) enum WebCacheValue {
    Search(WebSearchResponse),
    Fetch(WebFetchResponse),
}

/// Safe cache validators copied from a response only after strict size and
/// character validation. No arbitrary response headers are retained or replayed.
#[derive(Debug, Clone, Default)]
pub(super) struct CacheValidators {
    pub(super) etag: Option<String>,
    pub(super) last_modified: Option<String>,
}

impl CacheValidators {
    pub(super) fn is_empty(&self) -> bool {
        self.etag.is_none() && self.last_modified.is_none()
    }

    pub(super) fn apply_to(&self, headers: &mut BTreeMap<String, String>) {
        if let Some(etag) = &self.etag {
            headers.insert("if-none-match".to_owned(), etag.clone());
        }
        if let Some(last_modified) = &self.last_modified {
            headers.insert("if-modified-since".to_owned(), last_modified.clone());
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct WebCacheEntry {
    pub(super) key: WebCacheKey,
    pub(super) value: WebCacheValue,
    pub(super) validators: CacheValidators,
    pub(super) fresh_until: Instant,
    pub(super) discard_after: Instant,
    pub(super) bytes: usize,
}

#[derive(Debug, Clone)]
pub(super) struct WebCacheLookup {
    pub(super) canonical_key: WebCacheKey,
    pub(super) value: WebCacheValue,
    pub(super) validators: CacheValidators,
    pub(super) stale: bool,
}

/// Bounded, session-local LRU cache. It retains only normalized public response
/// fields and safe validators, never raw response bytes, transport headers, or
/// credentials. Expired entries remain briefly only for conditional revalidation.
#[derive(Debug)]
pub(super) struct WebContextCache {
    pub(super) entries: VecDeque<WebCacheEntry>,
    pub(super) aliases: BTreeMap<WebCacheKey, WebCacheKey>,
    pub(super) bytes: usize,
    pub(super) max_entries: usize,
    pub(super) max_bytes: usize,
    pub(super) max_entry_bytes: usize,
    pub(super) ttl: Duration,
    pub(super) stale_ttl: Duration,
}

impl WebContextCache {
    pub(super) fn new(
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

    pub(super) fn get(&mut self, key: &WebCacheKey) -> Option<WebCacheLookup> {
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

    pub(super) fn insert(
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

    pub(super) fn refresh(&mut self, key: &WebCacheKey) {
        let canonical_key = self.aliases.get(key).cloned().unwrap_or_else(|| key.clone());
        if let Some(index) = self.entries.iter().position(|entry| entry.key == canonical_key) {
            let mut entry = self.entries.remove(index).expect("cache entry index exists");
            let now = Instant::now();
            entry.fresh_until = now + self.ttl;
            entry.discard_after = now + self.ttl + self.stale_ttl;
            self.entries.push_back(entry);
        }
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.aliases.clear();
        self.bytes = 0;
    }

    pub(super) fn remove_key(&mut self, key: &WebCacheKey) {
        if let Some(index) = self.entries.iter().position(|entry| entry.key == *key) {
            let entry = self.entries.remove(index).expect("cache entry index exists");
            self.bytes -= entry.bytes;
            self.aliases.retain(|_, canonical| canonical != &entry.key);
        }
    }

    pub(super) fn remove_discarded(&mut self) {
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

pub(super) fn cache_entry_bytes(
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

pub(super) fn cache_validators(headers: &BTreeMap<String, String>) -> CacheValidators {
    CacheValidators {
        etag: cache_validator(header_value(headers, "etag")),
        last_modified: cache_validator(header_value(headers, "last-modified")),
    }
}

pub(super) fn cache_validator(value: Option<&str>) -> Option<String> {
    value
        .filter(|value| {
            !value.is_empty()
                && value.len() <= MAX_CACHE_VALIDATOR_BYTES
                && value.bytes().all(|byte| byte.is_ascii_graphic() || byte == b' ')
        })
        .map(str::to_owned)
}

pub(super) fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub(super) fn normalize_search_query(query: &str) -> String {
    query.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) fn is_cloudflare_account_id(account_id: &str) -> bool {
    account_id.len() == 32 && account_id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(super) fn browser_run_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

pub(super) fn browser_run_retry_delay(
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

pub(super) fn validate_search_query(query: &str) -> Result<(), WebContextError> {
    if query.is_empty() || query.len() > MAX_SEARCH_QUERY_BYTES {
        return Err(WebContextError::new(WebContextErrorCode::UrlRejected));
    }
    Ok(())
}

pub(super) fn validate_provider_limit(
    value: usize,
    maximum: usize,
) -> Result<(), WebContextConfigError> {
    if value == 0 || value > maximum {
        return Err(WebContextConfigError::ProviderOptions);
    }
    Ok(())
}

pub(super) fn provider_options_digest(options: &WebSearchProviderOptions) -> String {
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
