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

use ee_chat_completions::{
    BearerToken, ChatCompletionsClient, EndpointProfile, MaxTokensField, RetryPolicy, TokenSource,
    TrustedEndpoint,
};
use reqwest::header::{HeaderName, HeaderValue};
use serde_json::{Value, json};

use crate::config::Config;

pub(crate) use ee_chat_completions::{
    StreamDelta as OpenRouterStreamDelta, ToolCall as OpenRouterToolCall, Usage as OpenRouterUsage,
};

/// Environment variable holding the OpenRouter API key.
pub(crate) const API_KEY_ENV: &str = "OPENROUTER_API_KEY";

/// Startup diagnostic used whenever no usable OpenRouter key is configured.
pub(crate) const MISSING_API_KEY: &str =
    "OPENROUTER_API_KEY is not set; export it before starting ee";

/// Largest accepted option JSON value, in bytes.
pub(crate) const MAX_OPTION_JSON_BYTES: usize = 64 * 1024;
/// Largest accepted fallback-model list length.
pub(crate) const MAX_FALLBACK_MODELS: usize = 16;
/// Longest accepted `OPENROUTER_USER` value, in bytes.
pub(crate) const MAX_USER_BYTES: usize = 256;
/// Reasoning effort levels OpenRouter documents.
pub(crate) const REASONING_EFFORTS: &[&str] =
    &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// OpenRouter request options parsed from the environment at startup.
///
/// Parsing happens once, before any request exists, so a malformed option or an
/// unsupported effort fails startup instead of failing a billable call.
#[derive(Debug, Clone, Default)]
pub(crate) struct OpenRouterOptions {
    provider: Option<Value>,
    plugins: Option<Value>,
    models: Option<Vec<String>>,
    user: Option<String>,
    passthrough: Option<Value>,
    include_usage: bool,
    reasoning: Option<Value>,
    reasoning_field: Option<String>,
    max_output_tokens: Option<u64>,
    max_tokens_field: Option<MaxTokensField>,
}

impl OpenRouterOptions {
    /// Parses every option from the process environment.
    pub(crate) fn from_env(reasoning_effort: Option<&str>) -> Result<Self, String> {
        Self::parse(
            OptionSources {
                provider: std::env::var("OPENROUTER_PROVIDER_PREFERENCES").ok(),
                plugins: std::env::var("OPENROUTER_PLUGINS").ok(),
                models: std::env::var("OPENROUTER_MODELS").ok(),
                user: std::env::var("OPENROUTER_USER").ok(),
                passthrough: std::env::var("OPENROUTER_OPTIONS_JSON").ok(),
                include_usage: std::env::var("OPENROUTER_INCLUDE_USAGE").ok(),
                reasoning_max_tokens: std::env::var("OPENROUTER_REASONING_MAX_TOKENS").ok(),
                reasoning_field: std::env::var("OPENROUTER_REASONING_FIELD").ok(),
                max_output_tokens: std::env::var("OPENROUTER_MAX_OUTPUT_TOKENS").ok(),
                max_tokens_field: std::env::var("OPENROUTER_MAX_TOKENS_FIELD").ok(),
            },
            reasoning_effort,
        )
    }

    /// Parses explicit sources, so tests never mutate the environment.
    pub(crate) fn parse(
        sources: OptionSources,
        reasoning_effort: Option<&str>,
    ) -> Result<Self, String> {
        let OptionSources {
            provider,
            plugins,
            models,
            user,
            passthrough,
            include_usage,
            reasoning_max_tokens,
            reasoning_field,
            max_output_tokens,
            max_tokens_field,
        } = sources;
        let provider = provider
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|value| parse_object("OPENROUTER_PROVIDER_PREFERENCES", value))
            .transpose()?;
        let plugins = plugins
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(parse_plugins)
            .transpose()?;
        let models = models
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(parse_models)
            .transpose()?;
        let user = user.map(|value| value.trim().to_string()).filter(|value| !value.is_empty());
        if let Some(user) = user.as_deref()
            && user.len() > MAX_USER_BYTES
        {
            return Err(format!("OPENROUTER_USER must be at most {MAX_USER_BYTES} bytes"));
        }
        let passthrough = passthrough
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|value| parse_object("OPENROUTER_OPTIONS_JSON", value))
            .transpose()?;
        let include_usage =
            include_usage.as_deref().map(parse_boolish).transpose()?.unwrap_or(true);
        let max_output_tokens = max_output_tokens
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|value| parse_positive_u64("OPENROUTER_MAX_OUTPUT_TOKENS", value))
            .transpose()?
            .map(|value| value.min(ee_agent_orchestrator::OUTPUT_LIMIT_MAX));
        let max_tokens_field = max_tokens_field
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(MaxTokensField::parse)
            .transpose()?;
        let reasoning =
            reasoning_object(reasoning_effort, reasoning_max_tokens, max_output_tokens)?;
        let reasoning_field = reasoning_field
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(parse_reasoning_field)
            .transpose()?;
        Ok(Self {
            provider,
            plugins,
            models,
            user,
            passthrough,
            include_usage,
            reasoning,
            reasoning_field,
            max_output_tokens,
            max_tokens_field,
        })
    }

    /// Whether the streamed request asks for usage.
    pub(crate) const fn include_usage(&self) -> bool {
        self.include_usage
    }

    /// Explicit output-token bound, when configured.
    pub(crate) const fn max_output_tokens(&self) -> Option<u64> {
        self.max_output_tokens
    }

    /// Body field carrying the output-token bound, when configured explicitly.
    ///
    /// `None` means the field is detected from the model vendor and endpoint.
    pub(crate) const fn max_tokens_field(&self) -> Option<MaxTokensField> {
        self.max_tokens_field
    }

    /// Assistant field this route requires when replaying reasoning.
    pub(crate) fn reasoning_field(&self) -> Option<String> {
        self.reasoning_field.clone()
    }

    /// Builds the extension object merged into every request body.
    ///
    /// Passthrough options are applied first so typed fields win, and no empty
    /// object is ever emitted for a setting that was not configured.
    pub(crate) fn body_extensions(&self) -> Value {
        let mut object =
            self.passthrough.as_ref().and_then(Value::as_object).cloned().unwrap_or_default();
        if let Some(provider) = &self.provider {
            object.insert("provider".into(), provider.clone());
        }
        if let Some(plugins) = &self.plugins {
            object.insert("plugins".into(), plugins.clone());
        }
        if let Some(models) = &self.models {
            object.insert("models".into(), json!(models));
        }
        if let Some(user) = &self.user {
            object.insert("user".into(), json!(user));
        }
        if let Some(reasoning) = &self.reasoning {
            object.insert("reasoning".into(), reasoning.clone());
        }
        object.insert("usage".into(), json!({ "include": self.include_usage }));
        Value::Object(object)
    }
}

/// Raw option values read from the environment, before validation.
#[derive(Debug, Clone, Default)]
pub(crate) struct OptionSources {
    /// `OPENROUTER_PROVIDER_PREFERENCES`.
    pub provider: Option<String>,
    /// `OPENROUTER_PLUGINS`.
    pub plugins: Option<String>,
    /// `OPENROUTER_MODELS`.
    pub models: Option<String>,
    /// `OPENROUTER_USER`.
    pub user: Option<String>,
    /// `OPENROUTER_OPTIONS_JSON`.
    pub passthrough: Option<String>,
    /// `OPENROUTER_INCLUDE_USAGE`.
    pub include_usage: Option<String>,
    /// `OPENROUTER_REASONING_MAX_TOKENS`.
    pub reasoning_max_tokens: Option<String>,
    /// `OPENROUTER_REASONING_FIELD`.
    pub reasoning_field: Option<String>,
    /// `OPENROUTER_MAX_OUTPUT_TOKENS`.
    pub max_output_tokens: Option<String>,
    /// `OPENROUTER_MAX_TOKENS_FIELD`.
    pub max_tokens_field: Option<String>,
}

fn parse_object(name: &str, raw: &str) -> Result<Value, String> {
    if raw.len() > MAX_OPTION_JSON_BYTES {
        return Err(format!("{name} must be at most {MAX_OPTION_JSON_BYTES} bytes"));
    }
    let value: Value =
        serde_json::from_str(raw).map_err(|error| format!("{name} is not valid JSON: {error}"))?;
    if !value.is_object() {
        return Err(format!("{name} must be a JSON object"));
    }
    Ok(value)
}

fn parse_plugins(raw: &str) -> Result<Value, String> {
    if raw.len() > MAX_OPTION_JSON_BYTES {
        return Err(format!("OPENROUTER_PLUGINS must be at most {MAX_OPTION_JSON_BYTES} bytes"));
    }
    let value: Value = serde_json::from_str(raw)
        .map_err(|error| format!("OPENROUTER_PLUGINS is not valid JSON: {error}"))?;
    let Some(entries) = value.as_array() else {
        return Err(String::from("OPENROUTER_PLUGINS must be a JSON array"));
    };
    if entries.iter().any(|entry| !entry.is_object()) {
        return Err(String::from("OPENROUTER_PLUGINS entries must be JSON objects"));
    }
    Ok(value)
}

fn parse_models(raw: &str) -> Result<Vec<String>, String> {
    let models: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect();
    if models.is_empty() {
        return Err(String::from("OPENROUTER_MODELS must name at least one model"));
    }
    if models.len() > MAX_FALLBACK_MODELS {
        return Err(format!("OPENROUTER_MODELS accepts at most {MAX_FALLBACK_MODELS} entries"));
    }
    Ok(models)
}

fn parse_boolish(value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(format!("expected a boolean (1/0, true/false, yes/no, on/off), got {value:?}")),
    }
}

/// Validates the assistant field a route requires for replayed reasoning.
///
/// Only the scalar field names the shared codec replays are accepted, so a typo
/// fails at startup instead of emitting an unknown field on every request.
fn parse_reasoning_field(value: &str) -> Result<String, String> {
    let value = value.trim();
    if ee_chat_completions::CHAT_REPLAY_FIELDS.contains(&value) {
        return Ok(value.to_string());
    }
    Err(format!(
        "unsupported OPENROUTER_REASONING_FIELD {value:?}: expected one of {}",
        ee_chat_completions::CHAT_REPLAY_FIELDS.join(", ")
    ))
}

fn parse_positive_u64(name: &str, value: &str) -> Result<u64, String> {
    let parsed: u64 =
        value.trim().parse().map_err(|_| format!("{name} must be a positive integer"))?;
    if parsed == 0 {
        return Err(format!("{name} must be at least 1"));
    }
    Ok(parsed)
}

fn reasoning_object(
    effort: Option<&str>,
    max_tokens: Option<String>,
    max_output_tokens: Option<u64>,
) -> Result<Option<Value>, String> {
    let effort = effort.map(str::trim).filter(|value| !value.is_empty());
    if let Some(effort) = effort
        && !REASONING_EFFORTS.contains(&effort)
    {
        return Err(format!(
            "unsupported reasoning effort {effort:?}: expected one of {}",
            REASONING_EFFORTS.join(", ")
        ));
    }
    let max_tokens = max_tokens
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|value| parse_positive_u64("OPENROUTER_REASONING_MAX_TOKENS", value))
        .transpose()?;
    if effort.is_none() && max_tokens.is_none() {
        return Ok(None);
    }
    let mut object = serde_json::Map::new();
    if let Some(effort) = effort {
        object.insert("effort".into(), json!(effort));
    }
    if let Some(max_tokens) = max_tokens {
        // The thinking budget must stay below the output bound; Anthropic
        // upstreams reject anything below 1,024 tokens.
        let fitted = match max_output_tokens {
            Some(output) => {
                let ceiling = output.saturating_sub(1).max(1);
                if ceiling < 1_024 {
                    return Err(String::from(
                        "OPENROUTER_MAX_OUTPUT_TOKENS must exceed the reasoning budget floor of 1024",
                    ));
                }
                max_tokens.clamp(1_024, ceiling)
            }
            None => max_tokens,
        };
        object.insert("max_tokens".into(), json!(fitted));
    }
    Ok(Some(Value::Object(object)))
}

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
    let endpoint = TrustedEndpoint::parse(&config.api_url)
        .map_err(|error| format!("invalid OPENROUTER_API_URL: {error}"))?;
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
    let mut profile = EndpointProfile::new("OpenRouter", API_KEY_ENV, endpoint, model)
        .with_system_prompt(config.system_prompt.clone())
        .with_timeout(config.timeout)
        .with_retry(RetryPolicy::new(
            config.retry_max_attempts,
            config.retry_base_delay,
            config.retry_max_delay,
        ))
        .with_prompt_cache(supports_anthropic_prompt_cache(model))
        .with_headers(headers)
        .with_stream_usage(options.include_usage())
        .with_max_output_tokens(options.max_output_tokens())
        .with_max_tokens_field(
            options
                .max_tokens_field()
                .unwrap_or_else(|| detect_max_tokens_field(model, &config.api_url)),
        )
        .with_reasoning_field(options.reasoning_field());
    let extensions = options.body_extensions();
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

/// Whether an OpenRouter model id routes to an Anthropic-family model, whose
/// API understands explicit `cache_control` breakpoints.
#[must_use]
pub(crate) fn supports_anthropic_prompt_cache(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.starts_with("anthropic/") || model.contains("claude")
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
            api_key: Some(String::from("sk-test")),
            site_url: None,
            app_title: String::from("ee-test"),
            timeout: Duration::from_secs(1),
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
            context_window: crate::config::DEFAULT_CONTEXT_WINDOW_TOKENS,
            max_iterations: None,
        }
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
        assert_eq!(profile.timeout, Duration::from_secs(1));
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
        assert_eq!(body["tool_choice"], "auto");

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
        assert!(profile.prompt_cache);

        config.model = String::from("openai/gpt-5");
        let profile = openrouter_profile(&config, &config.model).expect("profile builds");
        assert!(!profile.prompt_cache);
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
}
