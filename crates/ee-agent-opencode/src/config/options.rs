//! Sampling and transport option parsing for the OpenCode agent.
//!
//! Every value is parsed once at startup and validated against the routed
//! catalog row, so an unsupported knob or a malformed number fails before a
//! billable request exists. Parsing is separated from `config.rs` so the
//! configuration module stays reviewable and below the 1,000-line threshold.

use std::time::Duration;

use ee_agent_orchestrator::{GenerationOptions, SamplingSupport};
use ee_chat_completions::{ProviderOptions, RequestEncoding, Timeouts};

use crate::config::ConfigError;

/// Sampling temperature.
pub const TEMPERATURE_ENV: &str = "OPENCODE_TEMPERATURE";
/// Nucleus sampling probability.
pub const TOP_P_ENV: &str = "OPENCODE_TOP_P";
/// Top-k sampling cut-off.
pub const TOP_K_ENV: &str = "OPENCODE_TOP_K";
/// Frequency penalty.
pub const FREQUENCY_PENALTY_ENV: &str = "OPENCODE_FREQUENCY_PENALTY";
/// Presence penalty.
pub const PRESENCE_PENALTY_ENV: &str = "OPENCODE_PRESENCE_PENALTY";
/// Deterministic sampling seed.
pub const SEED_ENV: &str = "OPENCODE_SEED";
/// Comma-separated stop sequences.
pub const STOP_ENV: &str = "OPENCODE_STOP";
/// Whole-exchange timeout; `OPENCODE_TIMEOUT_MS` is the historical alias.
pub const TIMEOUT_TOTAL_ENV: &str = "OPENCODE_TIMEOUT_TOTAL_MS";
/// Budget until response headers arrive.
pub const TIMEOUT_HEADERS_ENV: &str = "OPENCODE_TIMEOUT_HEADERS_MS";
/// Idle budget between streamed chunks.
pub const TIMEOUT_CHUNK_ENV: &str = "OPENCODE_TIMEOUT_CHUNK_MS";
/// Typed OpenAI/Responses provider options, as a JSON object.
pub const PROVIDER_OPTIONS_ENV: &str = "OPENCODE_PROVIDER_OPTIONS_JSON";
/// JSON request-body compression: `none`, `gzip`, or `zstd`.
pub const ENCODING_ENV: &str = "OPENCODE_REQUEST_ENCODING";
/// Extra endpoint query parameters, as a JSON object of string values.
pub const QUERY_ENV: &str = "OPENCODE_QUERY_JSON";
/// Largest accepted query parameter count.
pub const MAX_QUERY_PARAMS: usize = 16;

/// Reads the session-default sampling knobs.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidGenerationOption`] naming the offending
/// variable when a value is malformed or outside its documented range.
pub(crate) fn generation_from<F>(lookup: F) -> Result<GenerationOptions, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let options = GenerationOptions {
        max_output_tokens: None,
        temperature: parse_f64(TEMPERATURE_ENV, lookup(TEMPERATURE_ENV))?,
        top_p: parse_f64(TOP_P_ENV, lookup(TOP_P_ENV))?,
        top_k: parse_u64(TOP_K_ENV, lookup(TOP_K_ENV))?,
        frequency_penalty: parse_f64(FREQUENCY_PENALTY_ENV, lookup(FREQUENCY_PENALTY_ENV))?,
        presence_penalty: parse_f64(PRESENCE_PENALTY_ENV, lookup(PRESENCE_PENALTY_ENV))?,
        seed: parse_i64(SEED_ENV, lookup(SEED_ENV))?,
        stop: parse_stop(lookup(STOP_ENV))?,
    };
    options.validate().map_err(|detail| ConfigError::InvalidGenerationOption {
        variable: String::from("OPENCODE_*"),
        detail,
    })?;
    Ok(options)
}

/// Rejects a configured knob the routed model does not document.
///
/// # Errors
///
/// Returns [`ConfigError::UnsupportedSamplingKnob`] naming the model and knob.
pub(crate) fn validate_generation(
    model_id: &str,
    sampling: SamplingSupport,
    generation: &GenerationOptions,
) -> Result<(), ConfigError> {
    match generation.unsupported(sampling) {
        Some(knob) => {
            Err(ConfigError::UnsupportedSamplingKnob { model_id: model_id.to_string(), knob })
        }
        None => Ok(()),
    }
}

/// Reads the per-phase HTTP budgets.
///
/// `total` is the whole-exchange budget: an explicit `OPENCODE_TIMEOUT_TOTAL_MS`
/// or the historical `OPENCODE_TIMEOUT_MS` alias. `headers` and `chunk` are the
/// explicit CLI values when one was supplied.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidGenerationOption`] naming the variable when a
/// value is malformed.
pub(crate) fn timeouts_from<F>(
    lookup: F,
    total: Option<u64>,
    headers: Option<u64>,
    chunk: Option<u64>,
) -> Result<Timeouts, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let total = parse_u64(TIMEOUT_TOTAL_ENV, lookup(TIMEOUT_TOTAL_ENV))?.or(total);
    let defaults = Timeouts::new();
    let headers = headers
        .or(parse_u64(TIMEOUT_HEADERS_ENV, lookup(TIMEOUT_HEADERS_ENV))?)
        .map_or(defaults.headers, Duration::from_millis);
    let chunk = chunk
        .or(parse_u64(TIMEOUT_CHUNK_ENV, lookup(TIMEOUT_CHUNK_ENV))?)
        .map_or(defaults.chunk, Duration::from_millis);
    Ok(Timeouts { total: total.map(Duration::from_millis), headers, chunk })
}

fn parse_f64(name: &str, raw: Option<String>) -> Result<Option<f64>, ConfigError> {
    let Some(raw) = raw.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    raw.trim().parse::<f64>().map(Some).map_err(|_| ConfigError::InvalidGenerationOption {
        variable: name.to_string(),
        detail: String::from("must be a number"),
    })
}

fn parse_u64(name: &str, raw: Option<String>) -> Result<Option<u64>, ConfigError> {
    let Some(raw) = raw.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    raw.trim().parse::<u64>().map(Some).map_err(|_| ConfigError::InvalidGenerationOption {
        variable: name.to_string(),
        detail: String::from("must be a positive integer"),
    })
}

fn parse_i64(name: &str, raw: Option<String>) -> Result<Option<i64>, ConfigError> {
    let Some(raw) = raw.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    raw.trim().parse::<i64>().map(Some).map_err(|_| ConfigError::InvalidGenerationOption {
        variable: name.to_string(),
        detail: String::from("must be an integer"),
    })
}

fn parse_stop(raw: Option<String>) -> Result<Option<Vec<String>>, ConfigError> {
    let Some(raw) = raw.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    let stop: Vec<String> = raw.split(',').map(|entry| entry.trim().to_string()).collect();
    if stop.iter().any(String::is_empty) {
        return Err(ConfigError::InvalidGenerationOption {
            variable: STOP_ENV.to_string(),
            detail: String::from("rejects an empty stop sequence"),
        });
    }
    Ok(Some(stop))
}

/// Reads the typed OpenAI/Responses provider options.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidGenerationOption`] naming the variable and the
/// rejected key when the document is malformed or carries an undocumented field.
pub(crate) fn provider_options_from<F>(lookup: F) -> Result<ProviderOptions, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let Some(raw) = lookup(PROVIDER_OPTIONS_ENV).filter(|value| !value.trim().is_empty()) else {
        return Ok(ProviderOptions::none());
    };
    ProviderOptions::parse_json(PROVIDER_OPTIONS_ENV, &raw).map_err(|detail| {
        ConfigError::InvalidGenerationOption { variable: PROVIDER_OPTIONS_ENV.to_string(), detail }
    })
}

/// Reads the configured request-body compression.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidGenerationOption`] when the name is not one of
/// the documented encodings.
pub(crate) fn encoding_from<F>(lookup: F) -> Result<RequestEncoding, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let Some(raw) = lookup(ENCODING_ENV).filter(|value| !value.trim().is_empty()) else {
        return Ok(RequestEncoding::None);
    };
    RequestEncoding::parse(&raw).map_err(|detail| ConfigError::InvalidGenerationOption {
        variable: ENCODING_ENV.to_string(),
        detail,
    })
}

/// Reads the extra endpoint query parameters.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidGenerationOption`] when the document is not a
/// JSON object of string values, carries too many entries, or repeats a key.
pub(crate) fn query_from<F>(lookup: F) -> Result<Vec<(String, String)>, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let Some(raw) = lookup(QUERY_ENV).filter(|value| !value.trim().is_empty()) else {
        return Ok(Vec::new());
    };
    let value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|error| ConfigError::InvalidGenerationOption {
            variable: QUERY_ENV.to_string(),
            detail: format!("is not valid JSON: {error}"),
        })?;
    let Some(object) = value.as_object() else {
        return Err(ConfigError::InvalidGenerationOption {
            variable: QUERY_ENV.to_string(),
            detail: String::from("must be a JSON object of string values"),
        });
    };
    if object.len() > MAX_QUERY_PARAMS {
        return Err(ConfigError::InvalidGenerationOption {
            variable: QUERY_ENV.to_string(),
            detail: format!("accepts at most {MAX_QUERY_PARAMS} parameters"),
        });
    }
    let mut query = Vec::new();
    for (key, value) in object {
        let Some(text) = value.as_str() else {
            return Err(ConfigError::InvalidGenerationOption {
                variable: QUERY_ENV.to_string(),
                detail: format!("{key} must be a string"),
            });
        };
        if key.is_empty() || key.len() > 64 {
            return Err(ConfigError::InvalidGenerationOption {
                variable: QUERY_ENV.to_string(),
                detail: String::from("keys must be 1..=64 bytes"),
            });
        }
        query.push((key.clone(), text.to_string()));
    }
    Ok(query)
}
