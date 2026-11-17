//! OpenRouter request options parsed from the environment.
//!
//! Parsing happens once, before any request exists, so a malformed option or an
//! unsupported effort fails startup instead of failing a billable call. The
//! provider profile merges the resulting object into the request body through
//! the shared `EndpointProfile.extensions` slot.

use ee_agent_orchestrator::GenerationOptions;
use ee_chat_completions::{
    CachePolicy, MaxTokensField, ProviderOptions, RequestEncoding, parse_policy, parse_ttl,
};
use serde_json::{Value, json};

/// Largest accepted option JSON value, in bytes.
pub(crate) const MAX_OPTION_JSON_BYTES: usize = 64 * 1024;
/// Largest accepted fallback-model list length.
pub(crate) const MAX_FALLBACK_MODELS: usize = 16;
/// Longest accepted `OPENROUTER_USER` value, in bytes.
pub(crate) const MAX_USER_BYTES: usize = 256;
/// Prompt-cache policy: `auto`, `none`, or a JSON object.
pub(crate) const CACHE_POLICY_ENV: &str = "OPENROUTER_CACHE_POLICY";
/// Prompt-cache lifetime, in the models.dev duration form.
pub(crate) const CACHE_TTL_ENV: &str = "OPENROUTER_CACHE_TTL";
/// Typed OpenAI/Responses provider options, as a JSON object.
pub(crate) const PROVIDER_OPTIONS_ENV: &str = "OPENROUTER_PROVIDER_OPTIONS_JSON";
/// JSON request-body compression: `none`, `gzip`, or `zstd`.
pub(crate) const ENCODING_ENV: &str = "OPENROUTER_REQUEST_ENCODING";
/// Extra endpoint query parameters, as a JSON object of string values.
pub(crate) const QUERY_ENV: &str = "OPENROUTER_QUERY_JSON";
/// Largest accepted query parameter count.
pub(crate) const MAX_QUERY_PARAMS: usize = 16;
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
    cache_policy: Option<CachePolicy>,
    cache_ttl: Option<String>,
    generation: GenerationOptions,
    provider_options: ProviderOptions,
    encoding: RequestEncoding,
    query: Vec<(String, String)>,
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
                cache_policy: std::env::var(CACHE_POLICY_ENV).ok(),
                cache_ttl: std::env::var(CACHE_TTL_ENV).ok(),
                temperature: std::env::var("OPENROUTER_TEMPERATURE").ok(),
                top_p: std::env::var("OPENROUTER_TOP_P").ok(),
                top_k: std::env::var("OPENROUTER_TOP_K").ok(),
                frequency_penalty: std::env::var("OPENROUTER_FREQUENCY_PENALTY").ok(),
                presence_penalty: std::env::var("OPENROUTER_PRESENCE_PENALTY").ok(),
                seed: std::env::var("OPENROUTER_SEED").ok(),
                stop: std::env::var("OPENROUTER_STOP").ok(),
                provider_options: std::env::var(PROVIDER_OPTIONS_ENV).ok(),
                encoding: std::env::var(ENCODING_ENV).ok(),
                query: std::env::var(QUERY_ENV).ok(),
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
            cache_policy,
            cache_ttl,
            temperature,
            top_p,
            top_k,
            frequency_penalty,
            presence_penalty,
            seed,
            stop,
            provider_options,
            encoding,
            query,
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
        let cache_policy = cache_policy
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|value| {
                parse_policy(value).map_err(|detail| format!("{CACHE_POLICY_ENV}: {detail}"))
            })
            .transpose()?;
        let cache_ttl = cache_ttl
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|value| parse_ttl(value).map_err(|detail| format!("{CACHE_TTL_ENV}: {detail}")))
            .transpose()?;
        let generation = parse_generation(GenerationSources {
            temperature,
            top_p,
            top_k,
            frequency_penalty,
            presence_penalty,
            seed,
            stop,
        })?;
        let provider_options = provider_options
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|value| ProviderOptions::parse_json(PROVIDER_OPTIONS_ENV, value))
            .transpose()?
            .unwrap_or_default();
        let encoding = encoding
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|value| {
                RequestEncoding::parse(value).map_err(|detail| format!("{ENCODING_ENV}: {detail}"))
            })
            .transpose()?
            .unwrap_or_default();
        let query = query
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(parse_query)
            .transpose()?
            .unwrap_or_default();
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
            cache_policy,
            cache_ttl,
            generation,
            provider_options,
            encoding,
            query,
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

    /// Configured prompt-cache policy override.
    pub(crate) fn cache_policy(&self) -> Option<CachePolicy> {
        self.cache_policy.clone()
    }

    /// Configured prompt-cache lifetime override.
    pub(crate) fn cache_ttl(&self) -> Option<String> {
        self.cache_ttl.clone()
    }

    /// Session-default sampling knobs.
    pub(crate) const fn generation(&self) -> &GenerationOptions {
        &self.generation
    }

    /// Configured request-body compression.
    pub(crate) const fn encoding(&self) -> RequestEncoding {
        self.encoding
    }

    /// Configured endpoint query parameters.
    pub(crate) fn query(&self) -> Vec<(String, String)> {
        self.query.clone()
    }

    /// Builds the extension object for one route.
    ///
    /// `/responses` does not document `usage` or `reasoning.max_tokens`, so the
    /// chat-only fields are dropped there instead of being sent blindly;
    /// `provider`, `plugins`, `models`, and `user` are accepted by both routes.
    pub(crate) fn body_extensions_for(&self, kind: crate::config::OpenRouterApiKind) -> Value {
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
            let reasoning = match kind {
                crate::config::OpenRouterApiKind::Chat => Some(reasoning.clone()),
                crate::config::OpenRouterApiKind::Responses => {
                    reasoning.as_object().and_then(|object| {
                        let filtered: serde_json::Map<String, Value> = object
                            .iter()
                            .filter(|(key, _)| matches!(key.as_str(), "effort" | "summary"))
                            .map(|(key, value)| (key.clone(), value.clone()))
                            .collect();
                        (!filtered.is_empty()).then_some(Value::Object(filtered))
                    })
                }
            };
            if let Some(reasoning) = reasoning {
                object.insert("reasoning".into(), reasoning);
            }
        }
        if kind == crate::config::OpenRouterApiKind::Chat {
            object.insert("usage".into(), json!({ "include": self.include_usage }));
        }
        // Typed provider options merge last, so an explicit `reasoning.summary`
        // lands next to the reasoning effort instead of replacing it.
        let mut extensions = Value::Object(object);
        self.provider_options
            .merge_into(&mut extensions, kind == crate::config::OpenRouterApiKind::Responses);
        extensions
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
    /// `OPENROUTER_CACHE_POLICY`.
    pub cache_policy: Option<String>,
    /// `OPENROUTER_CACHE_TTL`.
    pub cache_ttl: Option<String>,
    /// `OPENROUTER_TEMPERATURE`.
    pub temperature: Option<String>,
    /// `OPENROUTER_TOP_P`.
    pub top_p: Option<String>,
    /// `OPENROUTER_TOP_K`.
    pub top_k: Option<String>,
    /// `OPENROUTER_FREQUENCY_PENALTY`.
    pub frequency_penalty: Option<String>,
    /// `OPENROUTER_PRESENCE_PENALTY`.
    pub presence_penalty: Option<String>,
    /// `OPENROUTER_SEED`.
    pub seed: Option<String>,
    /// `OPENROUTER_STOP`.
    pub stop: Option<String>,
    /// `OPENROUTER_PROVIDER_OPTIONS_JSON`.
    pub provider_options: Option<String>,
    /// `OPENROUTER_REQUEST_ENCODING`.
    pub encoding: Option<String>,
    /// `OPENROUTER_QUERY_JSON`.
    pub query: Option<String>,
}

/// Raw sampling values read from the environment, before validation.
#[derive(Debug, Clone, Default)]
pub(crate) struct GenerationSources {
    /// `OPENROUTER_TEMPERATURE`.
    pub temperature: Option<String>,
    /// `OPENROUTER_TOP_P`.
    pub top_p: Option<String>,
    /// `OPENROUTER_TOP_K`.
    pub top_k: Option<String>,
    /// `OPENROUTER_FREQUENCY_PENALTY`.
    pub frequency_penalty: Option<String>,
    /// `OPENROUTER_PRESENCE_PENALTY`.
    pub presence_penalty: Option<String>,
    /// `OPENROUTER_SEED`.
    pub seed: Option<String>,
    /// `OPENROUTER_STOP`.
    pub stop: Option<String>,
}

/// Parses and validates the session-default sampling knobs.
fn parse_generation(sources: GenerationSources) -> Result<GenerationOptions, String> {
    let stop = sources
        .stop
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            let stop: Vec<String> =
                value.split(',').map(|entry| entry.trim().to_string()).collect();
            if stop.iter().any(String::is_empty) {
                return Err(String::from("OPENROUTER_STOP rejects an empty stop sequence"));
            }
            Ok(stop)
        })
        .transpose()?;
    let generation = GenerationOptions {
        max_output_tokens: None,
        temperature: parsed("OPENROUTER_TEMPERATURE", sources.temperature.as_deref())?,
        top_p: parsed("OPENROUTER_TOP_P", sources.top_p.as_deref())?,
        top_k: parsed("OPENROUTER_TOP_K", sources.top_k.as_deref())?,
        frequency_penalty: parsed(
            "OPENROUTER_FREQUENCY_PENALTY",
            sources.frequency_penalty.as_deref(),
        )?,
        presence_penalty: parsed(
            "OPENROUTER_PRESENCE_PENALTY",
            sources.presence_penalty.as_deref(),
        )?,
        seed: parsed("OPENROUTER_SEED", sources.seed.as_deref())?,
        stop,
    };
    generation.validate()?;
    Ok(generation)
}

/// Parses one numeric sampling value, ignoring an unset or empty variable.
fn parsed<T>(name: &str, raw: Option<&str>) -> Result<Option<T>, String>
where
    T: std::str::FromStr,
{
    let Some(raw) = raw.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    raw.trim().parse::<T>().map(Some).map_err(|_| format!("{name} has an invalid numeric value"))
}

/// Parses the extra endpoint query parameters.
fn parse_query(raw: &str) -> Result<Vec<(String, String)>, String> {
    if raw.len() > MAX_OPTION_JSON_BYTES {
        return Err(format!("{QUERY_ENV} must be at most {MAX_OPTION_JSON_BYTES} bytes"));
    }
    let value: Value = serde_json::from_str(raw)
        .map_err(|error| format!("{QUERY_ENV} is not valid JSON: {error}"))?;
    let Some(object) = value.as_object() else {
        return Err(format!("{QUERY_ENV} must be a JSON object of string values"));
    };
    if object.len() > MAX_QUERY_PARAMS {
        return Err(format!("{QUERY_ENV} accepts at most {MAX_QUERY_PARAMS} parameters"));
    }
    let mut query = Vec::new();
    for (key, value) in object {
        let Some(text) = value.as_str() else {
            return Err(format!("{QUERY_ENV}.{key} must be a string"));
        };
        if key.is_empty() || key.len() > 64 {
            return Err(format!("{QUERY_ENV} keys must be 1..=64 bytes"));
        }
        query.push((key.clone(), text.to_string()));
    }
    Ok(query)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OpenRouterApiKind;

    fn sources() -> OptionSources {
        OptionSources {
            temperature: Some(String::from("0.2")),
            top_p: Some(String::from("0.8")),
            top_k: Some(String::from("16")),
            frequency_penalty: Some(String::from("0.1")),
            presence_penalty: Some(String::from("0.2")),
            seed: Some(String::from("99")),
            stop: Some(String::from("END")),
            ..OptionSources::default()
        }
    }

    #[test]
    fn sampling_knobs_parse_into_the_generation_options() {
        let options = OpenRouterOptions::parse(sources(), None).expect("valid");
        let generation = options.generation();
        assert_eq!(generation.temperature, Some(0.2));
        assert_eq!(generation.top_p, Some(0.8));
        assert_eq!(generation.top_k, Some(16));
        assert_eq!(generation.seed, Some(99));
        assert_eq!(generation.stop.as_deref(), Some(&[String::from("END")][..]));
    }

    #[test]
    fn malformed_sampling_values_name_the_variable() {
        let mut raw = sources();
        raw.temperature = Some(String::from("warm"));
        let error = OpenRouterOptions::parse(raw, None).expect_err("rejected");
        assert!(error.contains("OPENROUTER_TEMPERATURE"), "{error}");

        let mut raw = sources();
        raw.temperature = Some(String::from("5.0"));
        let error = OpenRouterOptions::parse(raw, None).expect_err("out of range");
        assert!(error.contains("temperature"), "{error}");

        let mut raw = sources();
        raw.stop = Some(String::from("A,,B"));
        let error = OpenRouterOptions::parse(raw, None).expect_err("empty entry");
        assert!(error.contains("OPENROUTER_STOP"), "{error}");
    }

    #[test]
    fn an_empty_environment_produces_no_sampling_knobs() {
        let options = OpenRouterOptions::parse(OptionSources::default(), None).expect("valid");
        assert!(options.generation().is_empty());
        assert_eq!(options.encoding(), RequestEncoding::None);
        assert!(options.query().is_empty());
    }

    #[test]
    fn encoding_is_validated_at_startup() {
        let mut raw = sources();
        raw.encoding = Some(String::from("gzip"));
        assert_eq!(
            OpenRouterOptions::parse(raw, None).expect("valid").encoding(),
            RequestEncoding::Gzip
        );

        let mut raw = sources();
        raw.encoding = Some(String::from("br"));
        let error = OpenRouterOptions::parse(raw, None).expect_err("rejected");
        assert!(error.contains("OPENROUTER_REQUEST_ENCODING"), "{error}");
    }

    #[test]
    fn query_parameters_are_typed_and_bounded() {
        let mut raw = sources();
        raw.query = Some(String::from(r#"{"trace":"abc","debug":"1"}"#));
        let options = OpenRouterOptions::parse(raw, None).expect("valid");
        assert_eq!(
            options.query(),
            vec![
                (String::from("trace"), String::from("abc")),
                (String::from("debug"), String::from("1"))
            ]
        );

        let mut raw = sources();
        raw.query = Some(String::from(r#"{"trace":1}"#));
        assert!(OpenRouterOptions::parse(raw, None).is_err());
    }

    #[test]
    fn provider_options_merge_into_the_route_body() {
        let mut raw = sources();
        raw.provider_options =
            Some(String::from(r#"{"service_tier":"priority","reasoning_summary":"concise"}"#));
        let options = OpenRouterOptions::parse(raw, Some("high")).expect("valid");

        let chat = options.body_extensions_for(OpenRouterApiKind::Chat);
        assert_eq!(chat["service_tier"], "priority");
        assert_eq!(chat["reasoning"]["effort"], "high");
        assert!(chat["reasoning"].get("summary").is_none(), "summary is a Responses-only field");

        let responses = options.body_extensions_for(OpenRouterApiKind::Responses);
        assert_eq!(responses["reasoning"]["summary"], "concise");
        assert_eq!(responses["reasoning"]["effort"], "high", "deep merge keeps the sibling effort");
        assert!(
            responses.get("usage").is_none(),
            "the responses route receives no chat-only field"
        );
    }
}
