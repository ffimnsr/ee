//! Web-context module: types.
use super::cache::current_unix_millis;
use super::*;

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
    pub(super) fn for_provider(provider: WebSearchProvider) -> Self {
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
