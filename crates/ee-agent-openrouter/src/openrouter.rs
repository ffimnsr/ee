//! OpenRouter-specific Chat Completions configuration and simple-provider glue.
//!
//! The request body, wire decoding, SSE framing, retry boundary, and normalized
//! adapter mapping live in [`ee_chat_completions`], shared with every other
//! OpenAI-compatible provider.  This module keeps what belongs to OpenRouter
//! alone:
//!
//! - the `OPENROUTER_*` environment names and the missing-key wording,
//! - the `HTTP-Referer` / `X-Title` attribution headers,
//! - reasoning-effort request shaping,
//! - OpenRouter error wording, injected as the shared profile label,
//! - the simple provider's single `tool_read_file` schema.
//!
//! The shared wire types keep their historical `OpenRouter*` names here so
//! [`crate::provider`], [`crate::tools`], and [`crate::orchestrated`] read
//! naturally.

use std::sync::Arc;

use ee_agent_orchestrator::Modalities;
use ee_chat_completions::{
    BearerToken, CacheDirective, CacheFormat, CachePolicy, CacheSupport, ChatCompletionsClient,
    EndpointProfile, MaxTokensField, RetryPolicy, TokenSource, TrustedEndpoint, gateway_policy,
};
use reqwest::header::{HeaderName, HeaderValue};
use serde_json::{Value, json};

use crate::config::{Config, OpenRouterApiKind};

pub(crate) use ee_chat_completions::{
    StreamDelta as OpenRouterStreamDelta, ToolCall as OpenRouterToolCall, Usage as OpenRouterUsage,
};

/// Environment variable holding the OpenRouter API key.
pub(crate) const API_KEY_ENV: &str = "OPENROUTER_API_KEY";

/// Startup diagnostic used whenever no usable OpenRouter key is configured.
pub(crate) const MISSING_API_KEY: &str =
    "OPENROUTER_API_KEY is not set; export it before starting ee";

pub(crate) use crate::options::OpenRouterOptions;
#[cfg(test)]
pub(crate) use crate::options::{MAX_OPTION_JSON_BYTES, OptionSources};
use crate::route_plan::{
    clamp_reasoning_budget, merge_reasoning, resolved_variant, validate_generation, validate_route,
    variant_reasoning,
};

/// Builds the OpenRouter profile for one model id.
///
/// `model` is explicit because the rubber-duck critic runs the same provider
/// configuration against a different model id; nothing else differs.
///
/// # Errors
///
/// Returns a diagnostic when the configured endpoint or a header value is
/// unusable, so a broken configuration fails at startup instead of on the first
/// billable request.
pub(crate) fn openrouter_profile(config: &Config, model: &str) -> Result<EndpointProfile, String> {
    let options = OpenRouterOptions::from_env(config.reasoning_effort.as_deref())?;
    openrouter_profile_with_options(config, model, &options)
}

/// Builds an OpenRouter profile from already parsed options.
pub(crate) fn openrouter_profile_with_options(
    config: &Config,
    model: &str,
    options: &OpenRouterOptions,
) -> Result<EndpointProfile, String> {
    let entry = config.catalog_entry();
    validate_route(config, entry)?;
    let endpoint = TrustedEndpoint::parse(match config.api_kind {
        OpenRouterApiKind::Chat => &config.api_url,
        OpenRouterApiKind::Responses => &config.responses_url,
    })
    .map_err(|error| match config.api_kind {
        OpenRouterApiKind::Chat => format!("invalid OPENROUTER_API_URL: {error}"),
        OpenRouterApiKind::Responses => format!("invalid OPENROUTER_RESPONSES_URL: {error}"),
    })?;
    let mut headers = vec![(
        HeaderName::from_static("x-title"),
        HeaderValue::from_str(&config.app_title)
            .map_err(|error| format!("invalid OpenRouter app title: {error}"))?,
    )];
    if let Some(site_url) = config.site_url.as_deref() {
        headers.push((
            HeaderName::from_static("http-referer"),
            HeaderValue::from_str(site_url)
                .map_err(|error| format!("invalid OpenRouter site URL header: {error}"))?,
        ));
    }
    let declared_field = entry
        .map(|entry| entry.max_tokens_field)
        .filter(|field| !field.is_empty())
        .and_then(|field| MaxTokensField::parse(field).ok());
    let max_tokens_field = options
        .max_tokens_field()
        .or(declared_field)
        .unwrap_or_else(|| detect_max_tokens_field(model, endpoint.as_str()));
    let reasoning_field = options
        .reasoning_field()
        .or_else(|| entry.and_then(|entry| entry.reasoning_field.map(String::from)));
    let mut extensions = options.body_extensions_for(config.api_kind);
    if let Some(variant) = resolved_variant(config, entry)? {
        merge_reasoning(&mut extensions, variant_reasoning(variant, options.max_output_tokens()));
    }
    if let Some(limits) = entry.and_then(|entry| entry.limits) {
        clamp_reasoning_budget(&mut extensions, limits.output);
    }
    let generation = options.generation().clone();
    validate_generation(
        config.api_kind,
        model,
        entry.map_or(ee_agent_orchestrator::SamplingSupport::ALL, |entry| entry.sampling),
        &generation,
    )?;
    let mut profile = EndpointProfile::new("OpenRouter", API_KEY_ENV, endpoint, model)
        .with_system_prompt(config.system_prompt.clone())
        .with_timeouts(config.timeouts)
        .with_session_headers(config.session_headers)
        .with_session_header_style(ee_chat_completions::SessionHeaderStyle::Ee)
        .with_user_agent(format!("ee-agent-openrouter/{}", env!("CARGO_PKG_VERSION")))
        .with_query(options.query())
        .with_encoding(options.encoding())
        .with_generation(generation)
        .with_retry(RetryPolicy::new(
            config.retry_max_attempts,
            config.retry_base_delay,
            config.retry_max_delay,
        ))
        .with_cache(cache_directive(model, options, config.api_kind))
        .with_headers(headers)
        .with_stream_usage(options.include_usage())
        .with_max_output_tokens(options.max_output_tokens())
        .with_max_tokens_field(max_tokens_field)
        .with_reasoning_field(reasoning_field)
        .with_limits(entry.and_then(|entry| entry.limits))
        .with_cost(entry.and_then(|entry| entry.cost))
        .with_modalities(entry.map_or(Modalities::UNKNOWN, |entry| entry.modalities))
        .with_sampling(
            entry.map_or(ee_agent_orchestrator::SamplingSupport::ALL, |entry| entry.sampling),
        )
        .with_tools_strict(entry.is_some_and(|entry| entry.tools_strict))
        .with_freeform(
            entry
                .map(|entry| entry.freeform.iter().map(|value| (*value).to_string()).collect())
                .unwrap_or_default(),
        );
    if extensions.as_object().is_some_and(|object| !object.is_empty()) {
        profile = profile.with_extensions(extensions);
    }
    Ok(profile)
}

/// Vendors whose OpenAI-compatible surface still requires `max_tokens`.
///
/// Anything else gets `max_completion_tokens`, matching the reference agent's
/// detection. Vendors are matched on the id prefix before `/`, and endpoints by
/// host substring.
const MAX_TOKENS_VENDORS: [&str; 9] = [
    "deepseek",
    "moonshotai",
    "togetherai",
    "zai",
    "z-ai",
    "zhipuai",
    "nvidia",
    "cerebras",
    "chutes",
];
const MAX_TOKENS_HOSTS: [&str; 10] = [
    "deepseek.com",
    "api.moonshot.ai",
    "api.together.",
    "api.z.ai",
    "open.bigmodel.cn",
    "integrate.api.nvidia.com",
    "cerebras.ai",
    "llm.chutes.ai",
    "gateway.ai.cloudflare.com",
    "api.cloudflare.com",
];

/// Detects the output-token body field for one OpenRouter model and endpoint.
///
/// An explicit `OPENROUTER_MAX_TOKENS_FIELD` wins; otherwise the model vendor
/// prefix and endpoint host decide. Cloudflare vendors always use `max_tokens`.
#[must_use]
pub(crate) fn detect_max_tokens_field(model: &str, endpoint: &str) -> MaxTokensField {
    let vendor = model.split('/').next().unwrap_or(model).trim().to_ascii_lowercase();
    let host = endpoint.to_ascii_lowercase();
    let requires_max_tokens = MAX_TOKENS_VENDORS.iter().any(|name| vendor.starts_with(name))
        || vendor.starts_with("cloudflare")
        || MAX_TOKENS_HOSTS.iter().any(|name| host.contains(name));
    if requires_max_tokens {
        MaxTokensField::MaxTokens
    } else {
        MaxTokensField::MaxCompletionTokens
    }
}

/// Resolves the prompt-cache directive for one OpenRouter model and route.
///
/// OpenRouter forwards inline markers to upstreams that understand them, so the
/// chat route advertises explicit caching and the *policy* decides whether
/// anything is placed. The default policy is the reference gateway heuristic:
/// Anthropic and Qwen families only. `/responses` ignores inline markers, so it
/// advertises provider-managed caching and only carries a request-level TTL.
fn cache_directive(
    model: &str,
    options: &OpenRouterOptions,
    kind: crate::config::OpenRouterApiKind,
) -> CacheDirective {
    if kind == crate::config::OpenRouterApiKind::Responses {
        let policy = options.cache_policy().unwrap_or(CachePolicy::None);
        return CacheDirective {
            policy: policy.with_ttl(options.cache_ttl()),
            support: CacheSupport::IMPLICIT,
            format: CacheFormat::Anthropic,
        };
    }
    let format = if model.to_ascii_lowercase().contains("qwen") {
        CacheFormat::Alibaba
    } else {
        CacheFormat::Anthropic
    };
    let policy = options.cache_policy().unwrap_or_else(|| gateway_policy(model));
    let (format, support) = if format == CacheFormat::Alibaba {
        // Qwen ignores breakpoints on tool definitions and caches them with the
        // system prompt, so the tool plane is excluded outright.
        (
            CacheFormat::Alibaba,
            CacheSupport {
                targets: ee_chat_completions::CacheTargets::SYSTEM_AND_MESSAGES,
                ..CacheSupport::ANTHROPIC
            },
        )
    } else {
        (CacheFormat::Anthropic, CacheSupport::ANTHROPIC)
    };
    CacheDirective { policy: policy.with_ttl(options.cache_ttl()), support, format }
}

/// Resolves the OpenRouter bearer token for each round trip.
///
/// The key is read from [`Config`] (which never prints it) and never enters the
/// transcript, a checkpoint, or an error message.
pub(crate) fn openrouter_token_source(config: &Config) -> TokenSource {
    let config = config.clone();
    Arc::new(move || {
        config
            .api_key
            .clone()
            .filter(|key| !key.is_empty())
            .map(BearerToken::new)
            .ok_or_else(|| MISSING_API_KEY.to_string())
    })
}

/// Whether the configuration carries a usable OpenRouter key.
pub(crate) fn has_api_key(config: &Config) -> bool {
    config.api_key.as_deref().is_some_and(|key| !key.is_empty())
}

/// Builds the Chat Completions client for one OpenRouter model id.
pub(crate) fn openrouter_client(
    config: &Config,
    model: &str,
) -> Result<ChatCompletionsClient, String> {
    ChatCompletionsClient::new(openrouter_profile(config, model)?, openrouter_token_source(config))
}

/// The tool schema advertised by the simple provider.
///
/// The orchestrated mode uses the orchestrator's registry tools instead; this
/// single read-only tool is the fallback mode's whole tool surface.
pub(crate) fn openrouter_tools() -> Vec<Value> {
    vec![json!({
        "type": "function",
        "function": {
            "name": "tool_read_file",
            "description": "Read a UTF-8 text file from the current ee workspace. Use this instead of printing tool syntax.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path, or path relative to the session cwd."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }
    })]
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ee_chat_completions::{messages_from_transcript, request_body, tools_from_definitions};

    use super::*;
    use crate::config::DEFAULT_API_URL;

    fn test_config() -> Config {
        Config {
            model: String::from("test/model"),
            model_family: None,
            rubber_duck_model: None,
            rubber_duck_model_family: None,
            rubber_duck: ee_agent_orchestrator::RubberDuckConfig::default(),
            api_url: String::from(DEFAULT_API_URL),
            api_kind: crate::config::OpenRouterApiKind::Chat,
            responses_url: String::from(crate::config::DEFAULT_RESPONSES_URL),
            api_key: Some(String::from("sk-test")),
            site_url: None,
            app_title: String::from("ee-test"),
            timeouts: ee_chat_completions::Timeouts::new().with_total(Some(Duration::from_secs(1))),
            system_prompt: String::from("system"),
            reasoning_effort: None,
            orchestrated: true,
            compact_min_messages: 4,
            compact_retained_tail: 2,
            compact_max_input_bytes: 65_536,
            auto_compact_threshold_percent: 80,
            retry_max_attempts: 2,
            retry_base_delay: Duration::from_millis(500),
            retry_max_delay: Duration::from_secs(30),
            checkpoint_dir: None,
            session_log_dir: None,
            session_log_durable: false,
            session_headers: true,
            context_window: crate::config::DEFAULT_CONTEXT_WINDOW_TOKENS,
            max_iterations: None,
        }
    }

    #[test]
    fn catalog_model_metadata_fills_the_profile() {
        let mut config = test_config();
        config.model = String::from("deepseek/deepseek-v4-flash");

        let profile = openrouter_profile(&config, config.base_model()).expect("profile builds");

        let limits = profile.limits.expect("catalog limits");
        assert!(limits.context >= 1_000_000, "{} context", limits.context);
        assert!(limits.output > 0);
        assert!(profile.cost.is_some(), "catalog prices are attached");
        assert!(profile.modalities.accepts("text"));
        assert_eq!(profile.model, "deepseek/deepseek-v4-flash");
    }

    #[test]
    fn variant_suffix_selects_an_effort_variant_without_reaching_the_wire_model() {
        let mut config = test_config();
        config.model = String::from("deepseek/deepseek-v4-flash#high");

        let profile = openrouter_profile(&config, config.base_model()).expect("profile builds");

        assert_eq!(profile.model, "deepseek/deepseek-v4-flash");
        assert_eq!(profile.extensions.expect("extensions")["reasoning"]["effort"], "high");
    }

    #[test]
    fn unknown_variant_fails_at_startup_with_the_valid_ids() {
        let mut config = test_config();
        config.model = String::from("deepseek/deepseek-v4-flash#bogus");

        let error = openrouter_profile(&config, config.base_model()).unwrap_err();

        assert!(error.contains("no reasoning variant \"bogus\""), "{error}");
        assert!(error.contains("high"), "{error}");
    }

    #[test]
    fn catalog_output_bound_clamps_a_configured_reasoning_budget() {
        let mut config = test_config();
        config.model = String::from("deepseek/deepseek-v4-flash");
        let options = OpenRouterOptions::parse(
            OptionSources {
                reasoning_max_tokens: Some(String::from("999999999")),
                ..OptionSources::default()
            },
            None,
        )
        .expect("options parse");

        let profile = openrouter_profile_with_options(&config, config.base_model(), &options)
            .expect("profile builds");

        let output = profile.limits.expect("limits").output;
        let budget = profile.extensions.expect("extensions")["reasoning"]["max_tokens"]
            .as_u64()
            .expect("budget");
        assert!(budget < output, "{budget} must stay below {output}");
    }

    #[test]
    fn a_model_without_reasoning_support_rejects_every_variant() {
        let mut config = test_config();
        config.model = String::from("amazon/nova-lite-v1#high");

        let error = openrouter_profile(&config, config.base_model()).unwrap_err();

        assert!(error.contains("no reasoning variant"), "{error}");
        assert!(error.contains("none"), "{error}");
    }

    #[test]
    fn non_chat_catalog_models_fail_before_any_request() {
        for (model, expected) in [
            ("tencent/hy-image-v3.5-preview", "image model"),
            ("cloudflare/clef", "decision model"),
        ] {
            let mut config = test_config();
            config.model = String::from(model);
            let error = openrouter_profile(&config, config.base_model()).unwrap_err();
            assert!(error.contains(expected), "{model}: {error}");
            assert!(error.contains("chat model"), "{model}: {error}");
        }
    }

    #[test]
    fn responses_route_uses_its_own_endpoint_and_drops_chat_only_fields() {
        let mut config = test_config();
        config.model = String::from("deepseek/deepseek-v4-flash");
        config.api_kind = crate::config::OpenRouterApiKind::Responses;
        let options =
            OpenRouterOptions::parse(OptionSources::default(), None).expect("empty options");

        let profile = openrouter_profile_with_options(&config, config.base_model(), &options)
            .expect("profile builds");

        assert_eq!(profile.endpoint.as_str(), crate::config::DEFAULT_RESPONSES_URL);
        assert!(
            profile.extensions.is_none(),
            "usage and chat-only reasoning fields are not sent to /responses"
        );
    }

    #[test]
    fn a_model_without_the_responses_route_is_refused_on_that_route() {
        let mut config = test_config();
        config.model = String::from("openrouter/auto");
        config.api_kind = crate::config::OpenRouterApiKind::Responses;

        let error = openrouter_profile(&config, config.base_model()).unwrap_err();

        assert!(error.contains("does not advertise the /responses route"), "{error}");
    }

    #[test]
    fn responses_adapter_builds_for_a_catalog_responses_model() {
        let mut config = test_config();
        config.model = String::from("deepseek/deepseek-v4-flash");
        config.api_kind = crate::config::OpenRouterApiKind::Responses;

        assert!(crate::orchestrated::OpenRouterModelAdapter::new(config).is_ok());
    }

    #[test]
    fn profile_carries_openrouter_endpoint_headers_and_retry_policy() {
        let mut config = test_config();
        config.site_url = Some(String::from("https://ee.example"));

        let profile = openrouter_profile(&config, "critic/model").expect("profile builds");

        assert_eq!(profile.label, "OpenRouter");
        assert_eq!(profile.credential_var, API_KEY_ENV);
        assert_eq!(profile.endpoint.as_str(), DEFAULT_API_URL);
        assert_eq!(profile.model, "critic/model");
        assert_eq!(profile.system_prompt, "system");
        assert_eq!(profile.timeouts.total, Some(Duration::from_secs(1)));
        assert_eq!(profile.retry.max_attempts, 2);
        assert_eq!(profile.retry.base_delay, Duration::from_millis(500));
        assert_eq!(profile.retry.max_delay, Duration::from_secs(30));
        let headers: Vec<(&str, &str)> = profile
            .headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.to_str().expect("header value is ASCII")))
            .collect();
        assert!(headers.contains(&("x-title", "ee-test")));
        assert!(headers.contains(&("http-referer", "https://ee.example")));
    }

    #[test]
    fn reasoning_effort_shapes_the_request_body_only_for_openrouter() {
        let mut config = test_config();
        config.reasoning_effort = Some(String::from("medium"));
        let profile = openrouter_profile(&config, &config.model).expect("profile builds");
        let messages = messages_from_transcript(&profile.system_prompt, &[]);
        let tools = tools_from_definitions(&[]);

        let body = request_body(
            &profile.model,
            &messages,
            &tools,
            ee_chat_completions::RequestBodyOptions {
                extensions: profile.extensions.as_ref(),
                ..ee_chat_completions::RequestBodyOptions::default()
            },
        );

        assert_eq!(body["model"], "test/model");
        assert_eq!(body["messages"][0]["content"], "system");
        assert_eq!(body["reasoning"]["effort"], "medium");
        assert_eq!(body["stream"], false);
        assert!(
            body.get("tool_choice").is_none(),
            "tool_choice is omitted when no tools are advertised"
        );

        let mut without = test_config();
        without.reasoning_effort = Some(String::new());
        let profile = openrouter_profile(&without, &without.model).expect("profile builds");
        let extensions = profile.extensions.expect("usage extension");
        assert!(extensions.get("reasoning").is_none(), "an empty effort is not sent");
        assert_eq!(extensions["usage"]["include"], true);
    }

    #[test]
    fn provider_preferences_are_verbatim_and_absent_when_unset() {
        let config = test_config();
        let preferences = json!({
            "order": ["anthropic"],
            "allow_fallbacks": false,
            "data_collection": "deny",
            "max_price": { "prompt": 3 }
        });
        let options = OpenRouterOptions::parse(
            OptionSources { provider: Some(preferences.to_string()), ..OptionSources::default() },
            None,
        )
        .expect("options parse");
        let profile = openrouter_profile_with_options(&config, &config.model, &options)
            .expect("profile builds");
        let extensions = profile.extensions.expect("extensions");
        assert_eq!(extensions["provider"], preferences);

        let options = OpenRouterOptions::parse(OptionSources::default(), None)
            .expect("default options parse");
        let profile = openrouter_profile_with_options(&config, &config.model, &options)
            .expect("profile builds");
        assert_eq!(profile.fitted_output_limit(1), None);
        assert!(profile.extensions.expect("usage")["provider"].is_null());
    }

    #[test]
    fn usage_include_defaults_true_and_can_be_disabled() {
        let config = test_config();
        let options =
            OpenRouterOptions::parse(OptionSources::default(), None).expect("defaults parse");
        assert!(options.include_usage());
        let profile = openrouter_profile_with_options(&config, &config.model, &options)
            .expect("profile builds");
        assert!(profile.stream_usage);
        assert_eq!(profile.extensions.expect("extensions")["usage"]["include"], true);

        let options = OpenRouterOptions::parse(
            OptionSources {
                include_usage: Some(String::from("false")),
                ..OptionSources::default()
            },
            None,
        )
        .expect("disabled usage parses");
        let profile = openrouter_profile_with_options(&config, &config.model, &options)
            .expect("profile builds");
        assert!(!profile.stream_usage);
        assert_eq!(profile.extensions.expect("extensions")["usage"]["include"], false);
    }

    #[test]
    fn reasoning_budget_stays_below_the_output_bound() {
        let config = test_config();
        let options = OpenRouterOptions::parse(
            OptionSources {
                reasoning_max_tokens: Some(String::from("8192")),
                max_output_tokens: Some(String::from("4096")),
                ..OptionSources::default()
            },
            Some("high"),
        )
        .expect("options parse");
        let profile = openrouter_profile_with_options(&config, &config.model, &options)
            .expect("profile builds");
        let fitted = profile.fitted_output_limit(1);
        let extensions = profile.extensions.as_ref().expect("extensions");
        assert_eq!(extensions["reasoning"]["effort"], "high");
        let budget = extensions["reasoning"]["max_tokens"].as_u64().expect("budget");
        assert!(budget >= 1_024, "Anthropic floor applies: {budget}");
        assert!(budget < 4_096, "budget must stay below max_tokens: {budget}");
        assert_eq!(fitted, Some(4_096));
    }

    #[test]
    fn invalid_or_oversized_options_fail_before_any_request() {
        let non_object = OpenRouterOptions::parse(
            OptionSources { provider: Some(String::from("[\"x\"]")), ..OptionSources::default() },
            None,
        )
        .expect_err("provider must be an object");
        assert!(non_object.contains("OPENROUTER_PROVIDER_PREFERENCES"), "{non_object}");

        let oversized = OpenRouterOptions::parse(
            OptionSources {
                passthrough: Some(format!("{{}}\"{}\"", "x".repeat(MAX_OPTION_JSON_BYTES))),
                ..OptionSources::default()
            },
            None,
        )
        .expect_err("oversized options must fail");
        assert!(oversized.contains("OPENROUTER_OPTIONS_JSON"), "{oversized}");

        let effort = OpenRouterOptions::parse(OptionSources::default(), Some("extreme"))
            .expect_err("unknown effort must fail");
        assert!(effort.contains("unsupported reasoning effort"), "{effort}");

        let bad_field = OpenRouterOptions::parse(
            OptionSources {
                max_tokens_field: Some(String::from("max_new_tokens")),
                ..OptionSources::default()
            },
            None,
        )
        .expect_err("unknown field must fail");
        assert!(bad_field.contains("max_completion_tokens"), "{bad_field}");
    }

    #[test]
    fn anthropic_routes_enable_prompt_cache_breakpoints() {
        let mut config = test_config();
        config.model = String::from("anthropic/claude-sonnet-4");
        let profile = openrouter_profile(&config, &config.model).expect("profile builds");
        assert_eq!(profile.cache.policy, CachePolicy::Auto);
        assert!(profile.cache.places_markers());

        config.model = String::from("openai/gpt-5");
        let profile = openrouter_profile(&config, &config.model).expect("profile builds");
        assert_eq!(profile.cache.policy, CachePolicy::None);
        assert!(!profile.cache.places_markers());
    }

    #[test]
    fn qwen_routes_keep_system_and_tail_breakpoints_only() {
        let mut config = test_config();
        config.model = String::from("qwen/qwen3-coder");
        let profile = openrouter_profile(&config, &config.model).expect("profile builds");

        assert_eq!(profile.cache.format, CacheFormat::Alibaba);
        assert_eq!(
            profile.cache.support.targets,
            ee_chat_completions::CacheTargets::SYSTEM_AND_MESSAGES
        );
        assert!(profile.cache.places_markers());
    }

    #[test]
    fn invalid_endpoint_and_header_values_fail_at_startup() {
        let mut config = test_config();
        config.api_url = String::from("http://gateway.example/v1/chat/completions");
        assert!(openrouter_profile(&config, "m").unwrap_err().contains("OPENROUTER_API_URL"));

        let mut config = test_config();
        config.app_title = String::from("bad\ntitle");
        assert!(openrouter_profile(&config, "m").unwrap_err().contains("app title"));
    }

    #[test]
    fn token_source_reports_the_missing_key_wording_without_exposing_a_key() {
        let mut config = test_config();
        config.api_key = None;
        let missing = openrouter_token_source(&config);
        assert_eq!(missing().unwrap_err(), MISSING_API_KEY);
        assert!(!has_api_key(&config));

        config.api_key = Some(String::from("sk-secret"));
        let present = openrouter_token_source(&config);
        let token = present().expect("token");
        assert_eq!(token.expose(), "sk-secret");
        assert!(!format!("{token:?}").contains("sk-secret"));
        assert!(has_api_key(&config));
    }

    #[test]
    fn simple_provider_tool_schema_stays_single_and_read_only() {
        let tools = openrouter_tools();

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["name"], "tool_read_file");
        assert_eq!(tools[0]["function"]["parameters"]["required"][0], "path");
    }

    #[test]
    fn output_token_field_is_detected_per_vendor() {
        let endpoint = "https://openrouter.ai/api/v1/chat/completions";
        for model in
            ["deepseek/deepseek-chat", "moonshotai/kimi-k2", "z-ai/glm-4.6", "nvidia/llama"]
        {
            assert_eq!(
                detect_max_tokens_field(model, endpoint),
                MaxTokensField::MaxTokens,
                "{model} still expects max_tokens"
            );
        }
        for model in ["anthropic/claude-sonnet-4", "openai/gpt-5", "google/gemini-3-pro"] {
            assert_eq!(
                detect_max_tokens_field(model, endpoint),
                MaxTokensField::MaxCompletionTokens,
                "{model} accepts the newer field"
            );
        }
    }

    #[test]
    fn explicit_output_token_field_overrides_detection() {
        let options = OpenRouterOptions::parse(
            OptionSources {
                max_tokens_field: Some(String::from("max_tokens")),
                ..OptionSources::default()
            },
            None,
        )
        .expect("parses");
        let mut config = test_config();
        let profile = openrouter_profile_with_options(&config, "openai/gpt-5", &options)
            .expect("profile builds");
        assert_eq!(profile.max_tokens_field, MaxTokensField::MaxTokens);

        config.api_url = String::from("https://openrouter.ai/api/v1/chat/completions");
        let detected = openrouter_profile(&config, "deepseek/deepseek-chat").expect("profile");
        assert_eq!(detected.max_tokens_field, MaxTokensField::MaxTokens);
        let detected = openrouter_profile(&config, "anthropic/claude-sonnet-4").expect("profile");
        assert_eq!(detected.max_tokens_field, MaxTokensField::MaxCompletionTokens);
    }

    #[test]
    fn reasoning_field_option_is_validated_and_applied() {
        let options = OpenRouterOptions::parse(
            OptionSources {
                reasoning_field: Some(String::from("reasoning_content")),
                ..OptionSources::default()
            },
            None,
        )
        .expect("parses");
        assert_eq!(options.reasoning_field().as_deref(), Some("reasoning_content"));
        let config = test_config();
        let profile = openrouter_profile_with_options(&config, "moonshotai/kimi-k2", &options)
            .expect("profile builds");
        assert_eq!(profile.reasoning_field.as_deref(), Some("reasoning_content"));

        let error = OpenRouterOptions::parse(
            OptionSources {
                reasoning_field: Some(String::from("private_thoughts")),
                ..OptionSources::default()
            },
            None,
        )
        .expect_err("unknown field must fail");
        assert!(error.contains("OPENROUTER_REASONING_FIELD"), "{error}");
    }

    #[test]
    fn cache_policy_and_ttl_are_parsed_at_startup() {
        let options = OpenRouterOptions::parse(
            OptionSources {
                cache_policy: Some(String::from("auto")),
                cache_ttl: Some(String::from("1h")),
                ..OptionSources::default()
            },
            None,
        )
        .expect("parses");
        let profile =
            openrouter_profile_with_options(&test_config(), "google/gemini-2.5-pro", &options)
                .expect("profile builds");
        assert_eq!(
            profile.cache.policy,
            CachePolicy::Explicit(
                ee_chat_completions::CachePolicyObject::auto().with_ttl(Some(String::from("1h")))
            )
        );
        assert!(profile.cache.places_markers());
        assert_eq!(profile.cache.policy.ttl(), Some("1h"));
    }
    #[test]
    fn invalid_cache_options_fail_before_any_request() {
        let error = OpenRouterOptions::parse(
            OptionSources { cache_policy: Some(String::from("{")), ..OptionSources::default() },
            None,
        )
        .expect_err("bad policy");
        assert!(error.contains("OPENROUTER_CACHE_POLICY"), "{error}");
        let error = OpenRouterOptions::parse(
            OptionSources { cache_ttl: Some(String::from("1w")), ..OptionSources::default() },
            None,
        )
        .expect_err("bad ttl");
        assert!(error.contains("OPENROUTER_CACHE_TTL"), "{error}");
    }
}
