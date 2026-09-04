//! Editor configuration loading for ee.
//!
//! Settings are resolved by merging layers in priority order (lowest first):
//!   1. built-in defaults
//!   2. `/etc/ee/config.toml`
//!   3. `$XDG_CONFIG_HOME/ee/config.toml` or `~/.config/ee/config.toml`
//!   4. fallback `~/.ee.toml` when XDG user config is missing
//!   5. every ancestor `.ee.toml` from outermost to innermost
//!   6. `.editorconfig` (walked up from the open file, per spec)
//!
//! Later layers override earlier ones for any key that is explicitly set.

use super::constants::{
    MAX_BRAVE_RESULTS, MAX_BRAVE_SNIPPETS, MAX_BRAVE_TOKENS, MAX_BRAVE_URLS, MAX_EXA_RESULTS,
    MAX_TAVILY_CHUNKS_PER_SOURCE, MAX_TAVILY_RESULTS,
};
use super::raw::{AgentWebContextToml, WebContextBackendToml};
#[cfg(any(feature = "agents", test))]
use super::raw::{
    BraveFreshnessToml, BraveLlmContextOptionsToml, BraveSafeSearchModeToml,
    BraveThresholdModeToml, ExaSearchModeToml, ExaSearchOptionsToml, TavilySearchDepthToml,
    TavilySearchOptionsToml, WebContextLimitsToml,
};

#[cfg(any(feature = "agents", test))]
use ee_agent_host::{
    AgentWebContextConfig,
    web_context::{
        BraveFreshness, BraveLlmContextOptions, BraveSafeSearchMode, BraveThresholdMode,
        ExaSearchMode, ExaSearchOptions, TavilySearchDepth, TavilySearchOptions, WebSearchProvider,
        WebSearchProviderOptions,
    },
};

pub(super) fn validate_agent_web_context_config(
    web_context: &AgentWebContextToml,
) -> Result<(), String> {
    if let Some(reference) = &web_context.provider_secret_reference {
        crate::secrets::SecretReference::parse(reference).map_err(|_| {
            String::from("invalid secret reference in agents.web_context.provider_secret_reference")
        })?;
    }
    if let Some(reference) = &web_context.browser_run_api_token_reference {
        crate::secrets::SecretReference::parse(reference).map_err(|_| {
            String::from(
                "invalid secret reference in agents.web_context.browser_run_api_token_reference",
            )
        })?;
    }
    if let Some(account_id) = &web_context.browser_run_account_id
        && !(account_id.len() == 32 && account_id.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err(String::from(
            "agents.web_context.browser_run_account_id must be a 32-character hexadecimal Cloudflare account id",
        ));
    }
    let max_attempts = web_context.browser_run_max_attempts.unwrap_or(3);
    let base_delay_ms = web_context.browser_run_base_delay_ms.unwrap_or(500);
    let max_delay_ms = web_context.browser_run_max_delay_ms.unwrap_or(10_000);
    if max_attempts == 0 || max_attempts > 5 {
        return Err(String::from(
            "agents.web_context.browser_run_max_attempts must be 1 through 5",
        ));
    }
    if base_delay_ms == 0
        || base_delay_ms > 30_000
        || max_delay_ms == 0
        || max_delay_ms > 30_000
        || base_delay_ms > max_delay_ms
    {
        return Err(String::from(
            "Browser Run retry delay values must be within 1 through 30000 ms and base must not exceed max",
        ));
    }

    match web_context.backend {
        Some(WebContextBackendToml::Searxng)
            if web_context
                .endpoint
                .as_deref()
                .is_none_or(|endpoint| endpoint.trim().is_empty()) =>
        {
            return Err(String::from(
                "agents.web_context.endpoint is required when backend is searxng",
            ));
        }
        Some(
            WebContextBackendToml::Exa
            | WebContextBackendToml::BraveLlmContext
            | WebContextBackendToml::Tavily,
        ) if web_context.endpoint.is_some() => {
            return Err(String::from(
                "agents.web_context.endpoint is only permitted when backend is searxng",
            ));
        }
        None if web_context.endpoint.is_some() => {
            return Err(String::from(
                "agents.web_context.endpoint is only permitted when backend is searxng",
            ));
        }
        _ => {}
    }

    validate_web_context_provider_options(
        "exa",
        web_context.exa.is_some(),
        web_context.backend,
        WebContextBackendToml::Exa,
    )?;
    validate_web_context_provider_options(
        "brave_llm_context",
        web_context.brave_llm_context.is_some(),
        web_context.backend,
        WebContextBackendToml::BraveLlmContext,
    )?;
    validate_web_context_provider_options(
        "tavily",
        web_context.tavily.is_some(),
        web_context.backend,
        WebContextBackendToml::Tavily,
    )?;
    if let Some(exa) = &web_context.exa {
        validate_web_context_provider_limit("exa.max_results", exa.max_results, MAX_EXA_RESULTS)?;
    }
    if let Some(tavily) = &web_context.tavily {
        validate_web_context_provider_limit(
            "tavily.max_results",
            tavily.max_results,
            MAX_TAVILY_RESULTS,
        )?;
        validate_web_context_provider_limit(
            "tavily.chunks_per_source",
            tavily.chunks_per_source,
            MAX_TAVILY_CHUNKS_PER_SOURCE,
        )?;
    }
    if let Some(brave) = &web_context.brave_llm_context {
        validate_web_context_provider_limit(
            "brave_llm_context.max_results",
            brave.max_results,
            MAX_BRAVE_RESULTS,
        )?;
        validate_web_context_provider_limit(
            "brave_llm_context.max_tokens",
            brave.max_tokens,
            MAX_BRAVE_TOKENS,
        )?;
        validate_web_context_provider_limit(
            "brave_llm_context.max_urls",
            brave.max_urls,
            MAX_BRAVE_URLS,
        )?;
        validate_web_context_provider_limit(
            "brave_llm_context.max_snippets",
            brave.max_snippets,
            MAX_BRAVE_SNIPPETS,
        )?;
    }

    if let Some(limits) = &web_context.limits {
        validate_web_context_raw_limit("request_timeout_ms", limits.request_timeout_ms)?;
        validate_web_context_raw_limit(
            "max_concurrent_requests",
            limits.max_concurrent_requests.map(|value| value as u64),
        )?;
    }
    Ok(())
}

fn validate_web_context_provider_options(
    name: &str,
    configured: bool,
    backend: Option<WebContextBackendToml>,
    expected_backend: WebContextBackendToml,
) -> Result<(), String> {
    if configured && backend != Some(expected_backend) {
        return Err(format!(
            "agents.web_context.{name} is only permitted when backend is {}",
            web_context_backend_name(expected_backend),
        ));
    }
    Ok(())
}

fn web_context_backend_name(backend: WebContextBackendToml) -> &'static str {
    match backend {
        WebContextBackendToml::Searxng => "searxng",
        WebContextBackendToml::Exa => "exa",
        WebContextBackendToml::BraveLlmContext => "brave_llm_context",
        WebContextBackendToml::Tavily => "tavily",
    }
}

fn validate_web_context_raw_limit(name: &str, value: Option<u64>) -> Result<(), String> {
    if value == Some(0) {
        return Err(format!("agents.web_context.limits.{name} must be greater than zero"));
    }
    Ok(())
}

fn validate_web_context_provider_limit(
    name: &str,
    value: Option<usize>,
    maximum: usize,
) -> Result<(), String> {
    if value.is_some_and(|value| value == 0 || value > maximum) {
        return Err(format!("agents.web_context.{name} must be within supported bounds"));
    }
    Ok(())
}

#[cfg(any(feature = "agents", test))]
pub(super) fn web_search_provider(backend: WebContextBackendToml) -> WebSearchProvider {
    match backend {
        WebContextBackendToml::Searxng => WebSearchProvider::Searxng,
        WebContextBackendToml::Exa => WebSearchProvider::Exa,
        WebContextBackendToml::BraveLlmContext => WebSearchProvider::BraveLlmContext,
        WebContextBackendToml::Tavily => WebSearchProvider::Tavily,
    }
}

#[cfg(any(feature = "agents", test))]
pub(super) fn web_search_provider_options(
    backend: WebContextBackendToml,
    patch: &AgentWebContextToml,
) -> WebSearchProviderOptions {
    match backend {
        WebContextBackendToml::Searxng => WebSearchProviderOptions::Searxng,
        WebContextBackendToml::Exa => {
            let mut options = ExaSearchOptions::default();
            if let Some(exa) = &patch.exa {
                if let Some(max_results) = exa.max_results {
                    options.max_results = max_results;
                }
                if let Some(search_mode) = exa.search_mode {
                    options.search_mode = search_mode.into();
                }
            }
            WebSearchProviderOptions::Exa(options)
        }
        WebContextBackendToml::BraveLlmContext => {
            let mut options = BraveLlmContextOptions::default();
            if let Some(brave) = &patch.brave_llm_context {
                if let Some(max_results) = brave.max_results {
                    options.max_results = max_results;
                }
                if let Some(max_tokens) = brave.max_tokens {
                    options.max_tokens = max_tokens;
                }
                if let Some(max_urls) = brave.max_urls {
                    options.max_urls = max_urls;
                }
                if let Some(max_snippets) = brave.max_snippets {
                    options.max_snippets = max_snippets;
                }
                if let Some(threshold_mode) = brave.threshold_mode {
                    options.threshold_mode = threshold_mode.into();
                }
                if let Some(freshness) = brave.freshness {
                    options.freshness = freshness.into();
                }
                if let Some(safe_search) = brave.safe_search {
                    options.safe_search = safe_search.into();
                }
            }
            WebSearchProviderOptions::BraveLlmContext(options)
        }
        WebContextBackendToml::Tavily => {
            let mut options = TavilySearchOptions::default();
            if let Some(tavily) = &patch.tavily {
                if let Some(max_results) = tavily.max_results {
                    options.max_results = max_results;
                }
                if let Some(chunks_per_source) = tavily.chunks_per_source {
                    options.chunks_per_source = chunks_per_source;
                }
                if let Some(search_depth) = tavily.search_depth {
                    options.search_depth = search_depth.into();
                }
            }
            WebSearchProviderOptions::Tavily(options)
        }
    }
}

#[cfg(any(feature = "agents", test))]
pub(super) fn agent_web_context_settings_to_toml(
    web_context: &AgentWebContextConfig,
) -> Option<AgentWebContextToml> {
    if web_context == &AgentWebContextConfig::default() {
        return None;
    }
    let backend = match web_context.provider {
        WebSearchProvider::Searxng => WebContextBackendToml::Searxng,
        WebSearchProvider::Exa => WebContextBackendToml::Exa,
        WebSearchProvider::BraveLlmContext => WebContextBackendToml::BraveLlmContext,
        WebSearchProvider::Tavily => WebContextBackendToml::Tavily,
    };
    let (exa, brave_llm_context, tavily) = match &web_context.provider_options {
        WebSearchProviderOptions::Searxng => (None, None, None),
        WebSearchProviderOptions::Exa(options) => (
            Some(ExaSearchOptionsToml {
                max_results: Some(options.max_results),
                search_mode: Some(match options.search_mode {
                    ExaSearchMode::Auto => ExaSearchModeToml::Auto,
                    ExaSearchMode::Neural => ExaSearchModeToml::Neural,
                    ExaSearchMode::Fast => ExaSearchModeToml::Fast,
                }),
            }),
            None,
            None,
        ),
        WebSearchProviderOptions::BraveLlmContext(options) => (
            None,
            Some(BraveLlmContextOptionsToml {
                max_results: Some(options.max_results),
                max_tokens: Some(options.max_tokens),
                max_urls: Some(options.max_urls),
                max_snippets: Some(options.max_snippets),
                threshold_mode: Some(match options.threshold_mode {
                    BraveThresholdMode::Balanced => BraveThresholdModeToml::Balanced,
                    BraveThresholdMode::Strict => BraveThresholdModeToml::Strict,
                }),
                freshness: Some(match options.freshness {
                    BraveFreshness::Any => BraveFreshnessToml::Any,
                    BraveFreshness::Day => BraveFreshnessToml::Day,
                    BraveFreshness::Week => BraveFreshnessToml::Week,
                    BraveFreshness::Month => BraveFreshnessToml::Month,
                }),
                safe_search: Some(match options.safe_search {
                    BraveSafeSearchMode::Off => BraveSafeSearchModeToml::Off,
                    BraveSafeSearchMode::Moderate => BraveSafeSearchModeToml::Moderate,
                    BraveSafeSearchMode::Strict => BraveSafeSearchModeToml::Strict,
                }),
            }),
            None,
        ),
        WebSearchProviderOptions::Tavily(options) => (
            None,
            None,
            Some(TavilySearchOptionsToml {
                max_results: Some(options.max_results),
                chunks_per_source: Some(options.chunks_per_source),
                search_depth: Some(match options.search_depth {
                    TavilySearchDepth::Basic => TavilySearchDepthToml::Basic,
                    TavilySearchDepth::Advanced => TavilySearchDepthToml::Advanced,
                }),
            }),
        ),
    };
    Some(AgentWebContextToml {
        enabled: Some(web_context.enabled),
        backend: Some(backend),
        endpoint: (backend == WebContextBackendToml::Searxng)
            .then(|| web_context.search_endpoint.clone())
            .flatten(),
        hosts: Some(web_context.preapproved_hosts.clone()),
        limits: Some(WebContextLimitsToml {
            max_response_bytes: Some(web_context.limits.max_response_bytes),
            max_text_bytes: Some(web_context.limits.max_text_bytes),
            max_search_results: Some(web_context.limits.max_search_results),
            max_redirects: Some(web_context.limits.max_redirects),
            request_timeout_ms: Some(web_context.limits.request_timeout_ms),
            max_concurrent_requests: Some(web_context.limits.max_concurrent_requests),
        }),
        provider_secret_reference: web_context.provider_secret_reference.clone(),
        browser_run_account_id: web_context.browser_run_account_id.clone(),
        browser_run_api_token_reference: web_context.browser_run_api_token_reference.clone(),
        browser_run_max_attempts: Some(web_context.browser_run_retry.max_attempts),
        browser_run_base_delay_ms: Some(web_context.browser_run_retry.base_delay_ms),
        browser_run_max_delay_ms: Some(web_context.browser_run_retry.max_delay_ms),
        exa,
        brave_llm_context,
        tavily,
    })
}
