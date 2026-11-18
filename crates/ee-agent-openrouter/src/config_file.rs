//! Bounded OpenRouter agent configuration file and its errors.
//!
//! Location: `$XDG_CONFIG_HOME/ee/agents/agent-openrouter.json`, falling back
//! to the platform config directory. The file is optional; every value in it is
//! bounded, interpolated through an allowlist, and validated before any request
//! body exists.
//!
//! # Shape
//!
//! ```json
//! {
//!   "default_model": "deepseek/deepseek-v4-flash-0731",
//!   "small_model": "deepseek/deepseek-v4-flash-0731",
//!   "base_url": "https://openrouter.ai/api/v1/chat/completions",
//!   "responses_url": "https://openrouter.ai/api/v1/responses",
//!   "compaction_mode": "local",
//!   "provider": {
//!     "headers": { "x-request-source": "ee" },
//!     "body": { "provider": { "sort": "throughput" } }
//!   },
//!   "models": {
//!     "anthropic/claude-sonnet-5": { "disabled": true, "variant": "high", "body": {} }
//!   }
//! }
//! ```
//!
//! Provider-wide overlays live under `provider`; a top-level `headers` or `body`
//! is rejected so one file cannot have two meanings for the same key.
//!
//! `${VAR}` resolves only against [`ALLOWED_ENV`]; an unresolved reference fails
//! startup with the variable name. Unknown keys are ignored so a new upstream
//! option does not require an ee release.
//!
//! Only static API keys are supported. OAuth login, a credential store, and
//! runtime provider-package installation are deliberately out of scope for this
//! layer; nothing here installs or executes code.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// File name under `<config>/ee/agents/`.
pub const FILE_NAME: &str = "agent-openrouter.json";

/// Environment variables a configuration value may reference.
pub const ALLOWED_ENV: [&str; 4] =
    ["OPENROUTER_API_KEY", "OPENROUTER_MODEL", "OPENROUTER_SITE_URL", "OPENROUTER_COMPACTION_MODE"];

/// Largest accepted header block, in entries.
const MAX_HEADERS: usize = 32;
/// Largest accepted header name or value, in bytes.
const MAX_HEADER_BYTES: usize = 4096;
/// Largest accepted model override block, in entries.
const MAX_MODEL_OVERRIDES: usize = 4096;

/// Configuration failure produced before any request exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// The configuration file is unreadable, oversized, malformed, or references
    /// a rejected value.
    ConfigFile {
        /// Bounded parser diagnostic naming the key or variable, never a value.
        detail: String,
    },
    /// The selected model is disabled in the configuration file.
    DisabledModel {
        /// Model id.
        model_id: String,
    },
    /// A `model#` selector carried an empty variant id.
    MissingVariant {
        /// Model id before the `#`.
        model_id: String,
    },
    /// An environment or configuration value is malformed.
    InvalidOption {
        /// Variable or option name.
        variable: String,
        /// Parser diagnostic.
        detail: String,
    },
    /// Provider-native compaction was forced on a route that does not advertise
    /// it.
    UnsupportedCompaction {
        /// Model id.
        model_id: String,
        /// Protocol route.
        route: &'static str,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConfigFile { detail } => {
                write!(formatter, "invalid agent configuration: {detail}")
            }
            Self::DisabledModel { model_id } => write!(
                formatter,
                "model {model_id:?} is disabled in the agent configuration: enable it or pick \
                 another model"
            ),
            Self::MissingVariant { model_id } => write!(
                formatter,
                "model {model_id:?} ends with an empty #variant selector: name a variant or drop \
                 the trailing #"
            ),
            Self::InvalidOption { variable, detail } => {
                write!(formatter, "invalid {variable}: {detail}")
            }
            Self::UnsupportedCompaction { model_id, route } => write!(
                formatter,
                "Provider compaction is not supported by OpenRouter {model_id} ({route}): use \
                 OPENROUTER_COMPACTION_MODE=local or auto"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Header and body overlay applied to one route.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overlay {
    /// Extra request headers, validated by the profile builder.
    pub headers: Vec<(String, String)>,
    /// Body fields deep-merged into every request.
    pub body: Option<Value>,
}

/// One per-model configuration block.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelEntry {
    /// The model may not be selected by flag, environment, or default.
    pub disabled: bool,
    /// Reasoning variant appended when the selector carries none.
    pub variant: Option<String>,
    /// Per-model header and body overlay.
    pub overlay: Overlay,
}

/// Parsed configuration file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileConfig {
    /// Model used when no selector is supplied.
    pub default_model: Option<String>,
    /// Rubber-duck model used when none is configured.
    pub small_model: Option<String>,
    /// Chat Completions endpoint override.
    pub base_url: Option<String>,
    /// Responses endpoint override.
    pub responses_url: Option<String>,
    /// `/compact` strategy spelling, validated by the config layer.
    pub compaction_mode: Option<String>,
    /// Provider-wide overlay.
    pub provider: Overlay,
    /// Per-model overrides.
    pub models: BTreeMap<String, ModelEntry>,
}

impl FileConfig {
    /// Resolves the effective overlay and variant for one model.
    #[must_use]
    pub fn resolve(&self, model_id: &str) -> ResolvedConfig {
        let entry = self.models.get(model_id);
        let mut headers = self.provider.headers.clone();
        let mut body = self.provider.body.clone();
        if let Some(entry) = entry {
            headers.extend(entry.overlay.headers.iter().cloned());
            if let Some(overlay) = entry.overlay.body.as_ref() {
                match body.as_mut() {
                    Some(existing) => ee_chat_completions::deep_merge(existing, overlay),
                    None => body = Some(overlay.clone()),
                }
            }
        }
        ResolvedConfig {
            headers,
            body,
            variant: entry.and_then(|entry| entry.variant.clone()),
            disabled: entry.is_some_and(|entry| entry.disabled),
        }
    }
}

/// Effective configuration for the selected model.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedConfig {
    /// Provider-wide plus per-model headers.
    pub headers: Vec<(String, String)>,
    /// Provider-wide plus per-model body fields.
    pub body: Option<Value>,
    /// Configured variant, when the selector carried none.
    pub variant: Option<String>,
    /// Whether the selected model is disabled.
    pub disabled: bool,
}

/// Returns the configuration file path, when a config directory exists.
#[must_use]
pub fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(dirs::config_dir)?;
    Some(base.join("ee").join("agents").join(FILE_NAME))
}

/// Loads the configuration file from [`path`], if it exists.
///
/// Test builds (`cfg(test)` or the `test-utils` feature) never read the
/// developer's real config tree. Use [`load_from`] for an explicit fixture.
///
/// # Errors
///
/// Returns a diagnostic for an unreadable, oversized, or malformed file, an
/// unresolved `${VAR}`, or a rejected field type.
pub fn load(lookup: &mut dyn FnMut(&str) -> Option<String>) -> Result<FileConfig, String> {
    if !config_files_enabled() {
        return Ok(FileConfig::default());
    }
    match path() {
        Some(path) => load_from(&path, lookup),
        None => Ok(FileConfig::default()),
    }
}

/// Whether this build reads the default configuration file.
#[must_use]
pub const fn config_files_enabled() -> bool {
    !cfg!(any(test, feature = "test-utils"))
}

/// Loads one explicit configuration file.
///
/// # Errors
///
/// Returns a diagnostic for an unreadable, oversized, or malformed file, an
/// unresolved `${VAR}`, or a rejected field type.
pub fn load_from(
    path: &Path,
    lookup: &mut dyn FnMut(&str) -> Option<String>,
) -> Result<FileConfig, String> {
    let Some(mut value) = ee_chat_completions::read_document(path)? else {
        return Ok(FileConfig::default());
    };
    ee_chat_completions::interpolate(&mut value, &ALLOWED_ENV, lookup)
        .map_err(|detail| format!("{}: {detail}", path.display()))?;
    parse(&value).map_err(|detail| format!("{}: {detail}", path.display()))
}

/// Parses an already interpolated configuration object.
///
/// # Errors
///
/// Returns a diagnostic naming the offending key path.
pub fn parse(value: &Value) -> Result<FileConfig, String> {
    let object = value.as_object().ok_or_else(|| String::from("expected a JSON object"))?;
    if object.contains_key("headers") || object.contains_key("body") {
        return Err(String::from(
            "headers/body must be nested under the \"provider\" object, not at the top level",
        ));
    }
    Ok(FileConfig {
        default_model: optional_string(object, "default_model")?,
        small_model: optional_string(object, "small_model")?,
        base_url: optional_string(object, "base_url")?,
        responses_url: optional_string(object, "responses_url")?,
        compaction_mode: optional_string(object, "compaction_mode")?,
        provider: provider_overlay(object)?,
        models: models(object)?,
    })
}

/// Parses the nested `provider` header/body overlay.
fn provider_overlay(object: &serde_json::Map<String, Value>) -> Result<Overlay, String> {
    let Some(value) = object.get("provider") else {
        return Ok(Overlay::default());
    };
    let block = value.as_object().ok_or_else(|| String::from("provider must be an object"))?;
    overlay(block, "provider.")
}

fn models(object: &serde_json::Map<String, Value>) -> Result<BTreeMap<String, ModelEntry>, String> {
    let Some(value) = object.get("models") else {
        return Ok(BTreeMap::new());
    };
    let map = value.as_object().ok_or_else(|| String::from("models must be an object"))?;
    if map.len() > MAX_MODEL_OVERRIDES {
        return Err(format!("models has more than {MAX_MODEL_OVERRIDES} entries"));
    }
    let mut entries = BTreeMap::new();
    for (model_id, entry) in map {
        let block =
            entry.as_object().ok_or_else(|| format!("models.{model_id} must be an object"))?;
        let prefix = format!("models.{model_id}");
        entries.insert(
            model_id.clone(),
            ModelEntry {
                disabled: optional_bool(block, "disabled")?.unwrap_or(false),
                variant: optional_string(block, "variant")?,
                overlay: overlay(block, &prefix)?,
            },
        );
    }
    Ok(entries)
}

fn overlay(object: &serde_json::Map<String, Value>, prefix: &str) -> Result<Overlay, String> {
    let mut headers = Vec::new();
    if let Some(value) = object.get("headers") {
        let map = value.as_object().ok_or_else(|| format!("{prefix}headers must be an object"))?;
        if map.len() > MAX_HEADERS {
            return Err(format!("{prefix}headers has more than {MAX_HEADERS} entries"));
        }
        for (name, value) in map {
            let value =
                value.as_str().ok_or_else(|| format!("{prefix}headers.{name} must be a string"))?;
            if name.is_empty() || value.len() > MAX_HEADER_BYTES || name.len() > MAX_HEADER_BYTES {
                return Err(format!("{prefix}headers.{name} is empty or too large"));
            }
            headers.push((name.clone(), value.to_string()));
        }
    }
    let body = match object.get("body") {
        Some(value) if value.is_object() => Some(value.clone()),
        Some(_) => return Err(format!("{prefix}body must be an object")),
        None => None,
    };
    Ok(Overlay { headers, body })
}

fn optional_string(
    object: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<String>, String> {
    match object.get(key) {
        Some(Value::String(text)) => {
            let text = text.trim();
            Ok((!text.is_empty()).then(|| text.to_string()))
        }
        Some(_) => Err(format!("{key} must be a string")),
        None => Ok(None),
    }
}

fn optional_bool(
    object: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<bool>, String> {
    match object.get(key) {
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(format!("{key} must be a boolean")),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests;
