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
    BearerToken, ChatCompletionsClient, EndpointProfile, RetryPolicy, TokenSource, TrustedEndpoint,
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
        .with_retry(RetryPolicy {
            max_attempts: config.retry_max_attempts,
            base_delay: config.retry_base_delay,
            max_delay: config.retry_max_delay,
        })
        .with_headers(headers);
    if let Some(effort) = config.reasoning_effort.as_deref().filter(|effort| !effort.is_empty()) {
        profile = profile.with_extensions(json!({ "reasoning": { "effort": effort } }));
    }
    Ok(profile)
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
            context_window: crate::config::DEFAULT_CONTEXT_WINDOW_TOKENS,
            max_iterations: ee_agent_orchestrator::config::DEFAULT_MAX_LOOP_ITERATIONS,
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

        let body =
            request_body(&profile.model, &messages, &tools, false, profile.extensions.as_ref());

        assert_eq!(body["model"], "test/model");
        assert_eq!(body["messages"][0]["content"], "system");
        assert_eq!(body["reasoning"]["effort"], "medium");
        assert_eq!(body["stream"], false);
        assert_eq!(body["tool_choice"], "auto");

        let mut without = test_config();
        without.reasoning_effort = Some(String::new());
        let profile = openrouter_profile(&without, &without.model).expect("profile builds");
        assert!(profile.extensions.is_none(), "an empty effort is not sent");
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
}
