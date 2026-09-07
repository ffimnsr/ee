//! Web-context module: providers.
use super::cache::validate_provider_limit;
use super::*;

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
    pub(super) fn validate_and_clamp(
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
