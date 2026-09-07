//! Web-context module: consts.
use super::*;

/// Maximum redirects allowed by this service, regardless of configuration.
pub const MAX_REDIRECTS: usize = 3;
/// Maximum search results accepted from a configured service.
pub const MAX_SEARCH_RESULTS: usize = 50;
/// Maximum response size accepted by the service configuration.
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Maximum extracted text size accepted by the service configuration.
pub const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;

pub(super) const DEFAULT_RESPONSE_BYTES: usize = 1024 * 1024;
pub(super) const DEFAULT_TEXT_BYTES: usize = 256 * 1024;
pub(super) const DEFAULT_SEARCH_RESULTS: usize = 10;
pub(super) const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 30_000;
pub(super) const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 2;
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
pub(super) const MAX_TITLE_BYTES: usize = 256;
pub(super) const MAX_SNIPPET_BYTES: usize = 1024;
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
pub(super) const WEB_CACHE_MAX_ENTRIES: usize = 32;
pub(super) const WEB_CACHE_MAX_BYTES: usize = 1024 * 1024;
pub(super) const WEB_CACHE_MAX_ENTRY_BYTES: usize = 256 * 1024;
pub(super) const WEB_CACHE_TTL: Duration = Duration::from_secs(60);
pub(super) const WEB_CACHE_STALE_TTL: Duration = Duration::from_secs(5 * 60);
pub(super) const MAX_CACHE_VALIDATOR_BYTES: usize = 1024;

/// Fixed vendor search endpoints. They are deliberately not configurable.
pub const EXA_SEARCH_ENDPOINT: &str = "https://api.exa.ai/search";
pub const BRAVE_LLM_CONTEXT_ENDPOINT: &str = "https://api.search.brave.com/res/v1/llm/context";
pub const TAVILY_SEARCH_ENDPOINT: &str = "https://api.tavily.com/search";
