//! Bounded OpenCode agent configuration file.
//!
//! Location: `$XDG_CONFIG_HOME/ee/agents/agent-opencode.json`, falling back to
//! the platform config directory. The file is optional and every value in it is
//! bounded, interpolated through an allowlist, and validated before any request
//! body exists.
//!
//! # Shape
//!
//! ```json
//! {
//!   "default_surface": "zen",
//!   "default_model": "gpt-5.5",
//!   "small_model": "gpt-5.5-mini",
//!   "disable_free_models": false,
//!   "org_id": "${OPENCODE_ORG_ID}",
//!   "headers": { "x-request-source": "ee" },
//!   "body": { "safety_identifier": "ee-agent" },
//!   "models": {
//!     "gpt-5.5": { "disabled": false, "variant": "high", "headers": {}, "body": {} }
//!   }
//! }
//! ```
//!
//! Unknown keys are ignored so an upstream option does not require an ee
//! release. `baseURL` is deliberately rejected: OpenCode endpoints are
//! catalog-derived HTTPS literals and no file may redirect them.
//!
//! `${VAR}` resolves only against [`ALLOWED_ENV`]; an unresolved reference fails
//! startup with the variable name.
//!
//! Only static API keys are supported. OAuth login, a credential store, and
//! runtime provider-package installation are deliberately out of scope for this
//! layer; nothing here installs or executes code.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::{API_KEY_ENV, COMPACTION_MODE_ENV, MODEL_ENV, SURFACE_ENV};

/// File name under `<config>/ee/agents/`.
pub const FILE_NAME: &str = "agent-opencode.json";

/// Environment variables a configuration value may reference.
pub const ALLOWED_ENV: [&str; 5] =
    [API_KEY_ENV, SURFACE_ENV, MODEL_ENV, COMPACTION_MODE_ENV, "OPENCODE_ORG_ID"];

/// Largest accepted header block, in entries.
const MAX_HEADERS: usize = 32;
/// Largest accepted header name or value, in bytes.
const MAX_HEADER_BYTES: usize = 4096;
/// Largest accepted model override block, in entries.
const MAX_MODEL_OVERRIDES: usize = 512;

/// Header and body overlay applied to one route.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overlay {
    /// Extra request headers, validated by the profile builder.
    pub headers: Vec<(String, String)>,
    /// Body fields deep-merged into every request.
    pub body: Option<Value>,
}

impl Overlay {
    /// Whether the overlay changes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.headers.is_empty() && self.body.is_none()
    }
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
    /// Surface used when no flag, environment value, or `.env` entry selects one.
    pub default_surface: Option<String>,
    /// Model used when no selector is supplied.
    pub default_model: Option<String>,
    /// Critic model used when no critic is configured.
    pub small_model: Option<String>,
    /// Hide free models from default selection, matching the reference switch.
    pub disable_free_models: bool,
    /// Workspace organization id for the usage probe; never inferred from a key.
    pub org_id: Option<String>,
    /// Provider-wide overlay.
    pub provider: Overlay,
    /// Per-model overrides, keyed by exact catalog id.
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
            org_id: self.org_id.clone(),
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
    /// Workspace organization id.
    pub org_id: Option<String>,
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
///
/// Test builds opt out so a developer's real agent config can never change a
/// test result.
#[must_use]
pub const fn config_files_enabled() -> bool {
    !cfg!(any(test, feature = "test-utils"))
}

/// Loads one explicit configuration file.
///
/// Returns the default configuration when the file is absent.
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
    reject_base_url(object, "")?;
    Ok(FileConfig {
        default_surface: optional_string(object, "default_surface")?,
        default_model: optional_string(object, "default_model")?,
        small_model: optional_string(object, "small_model")?,
        disable_free_models: optional_bool(object, "disable_free_models")?.unwrap_or(false),
        org_id: optional_string(object, "org_id")?,
        provider: overlay(object, "")?,
        models: models(object)?,
    })
}

/// Parses the per-model map.
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
        reject_base_url(block, &prefix)?;
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

/// Parses one header/body overlay.
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

/// Rejects an endpoint override: OpenCode origins are catalog-derived only.
fn reject_base_url(object: &serde_json::Map<String, Value>, prefix: &str) -> Result<(), String> {
    for key in ["baseURL", "base_url"] {
        if object.contains_key(key) {
            return Err(format!(
                "{prefix}{key} is not allowed: OpenCode endpoints come from the static catalog"
            ));
        }
    }
    Ok(())
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
