//! OpenRouter request options parsed from the environment.
//!
//! Parsing happens once, before any request exists, so a malformed option or an
//! unsupported effort fails startup instead of failing a billable call. The
//! provider profile merges the resulting object into the request body through
//! the shared `EndpointProfile.extensions` slot.

use ee_chat_completions::{CachePolicy, MaxTokensField, parse_policy, parse_ttl};
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
    /// `OPENROUTER_CACHE_POLICY`.
    pub cache_policy: Option<String>,
    /// `OPENROUTER_CACHE_TTL`.
    pub cache_ttl: Option<String>,
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
