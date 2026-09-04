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

use super::raw::RubberDuckToml;
#[cfg(any(feature = "agents", test))]
use std::collections::BTreeSet;

#[cfg(any(feature = "agents", test))]
use ee_agent_host::{
    ResolvedRubberDuckConfig, RubberDuckBackend, RubberDuckConfig, RubberDuckMode,
};

const DEFAULT_CRITIC_MAX_CALLS: usize = 2;
const MAX_CRITIC_MAX_CALLS: usize = 16;
const DEFAULT_CRITIC_CONTEXT_BYTES: usize = 64 * 1024;
const MAX_CRITIC_CONTEXT_BYTES: usize = 64 * 1024;
const DEFAULT_CRITIC_OUTPUT_BYTES: usize = 32 * 1024;
const MAX_CRITIC_OUTPUT_BYTES: usize = 32 * 1024;
const DEFAULT_CRITIC_TIMEOUT_MS: u64 = 90_000;
const MAX_CRITIC_TIMEOUT_MS: u64 = 300_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum RubberDuckModeSetting {
    Off,
    #[default]
    Manual,
    Automatic,
}

/// Fully merged frontend-owned rubber-duck settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RubberDuckSettings {
    pub mode: RubberDuckModeSetting,
    pub internal_model_id: Option<String>,
    pub external_agent_id: Option<String>,
    pub max_calls: usize,
    pub max_context_bytes: usize,
    pub max_output_bytes: usize,
    pub timeout_ms: u64,
}

impl Default for RubberDuckSettings {
    fn default() -> Self {
        Self {
            mode: RubberDuckModeSetting::Manual,
            internal_model_id: None,
            external_agent_id: None,
            max_calls: DEFAULT_CRITIC_MAX_CALLS,
            max_context_bytes: DEFAULT_CRITIC_CONTEXT_BYTES,
            max_output_bytes: DEFAULT_CRITIC_OUTPUT_BYTES,
            timeout_ms: DEFAULT_CRITIC_TIMEOUT_MS,
        }
    }
}

#[cfg(any(feature = "agents", test))]
impl RubberDuckSettings {
    /// Translates frontend config into validated backend policy. Unknown optional
    /// backend ids degrade critic only; ordinary agent operation remains usable.
    pub(crate) fn resolve_backend_policy(
        &self,
        model_ids: &BTreeSet<String>,
        agent_ids: &BTreeSet<String>,
    ) -> Result<ResolvedRubberDuckConfig, String> {
        let backend = RubberDuckBackend::from_optional_ids(
            self.internal_model_id.clone(),
            self.external_agent_id.clone(),
        )
        .map_err(|error| error.to_string())?;
        let config = RubberDuckConfig {
            mode: match self.mode {
                RubberDuckModeSetting::Off => RubberDuckMode::Off,
                RubberDuckModeSetting::Manual => RubberDuckMode::Manual,
                RubberDuckModeSetting::Automatic => RubberDuckMode::Automatic,
            },
            backend,
            max_calls: self.max_calls,
            max_context_bytes: self.max_context_bytes,
            max_output_bytes: self.max_output_bytes,
            timeout: std::time::Duration::from_millis(self.timeout_ms),
        };
        config.resolve(model_ids, agent_ids).map_err(|error| error.to_string())
    }
}

pub(super) fn merge_rubber_duck(
    existing: &RubberDuckSettings,
    patch: &RubberDuckToml,
) -> Result<RubberDuckSettings, String> {
    validate_rubber_duck_toml(patch)?;
    let mut resolved = existing.clone();
    if let Some(mode) = patch.mode.as_deref() {
        resolved.mode = match mode {
            "off" => RubberDuckModeSetting::Off,
            "manual" => RubberDuckModeSetting::Manual,
            "automatic" => RubberDuckModeSetting::Automatic,
            _ => return Err(String::from("mode must be off, manual, or automatic")),
        };
    }
    if let Some(model_id) = &patch.internal_model_id {
        resolved.internal_model_id = Some(model_id.clone());
    }
    if let Some(agent_id) = &patch.external_agent_id {
        resolved.external_agent_id = Some(agent_id.clone());
    }
    if resolved.internal_model_id.is_some() && resolved.external_agent_id.is_some() {
        return Err(String::from("internal_model_id and external_agent_id are mutually exclusive"));
    }
    if let Some(value) = patch.max_calls {
        resolved.max_calls = value;
    }
    if let Some(value) = patch.max_context_bytes {
        resolved.max_context_bytes = value;
    }
    if let Some(value) = patch.max_output_bytes {
        resolved.max_output_bytes = value;
    }
    if let Some(value) = patch.timeout_ms {
        resolved.timeout_ms = value;
    }
    Ok(resolved)
}

pub(super) fn validate_rubber_duck_toml(config: &RubberDuckToml) -> Result<(), String> {
    if config.internal_model_id.is_some() && config.external_agent_id.is_some() {
        return Err(String::from("internal_model_id and external_agent_id are mutually exclusive"));
    }
    for (field, value) in [
        ("internal_model_id", config.internal_model_id.as_deref()),
        ("external_agent_id", config.external_agent_id.as_deref()),
    ] {
        if value.is_some_and(|value| value.trim().is_empty()) {
            return Err(format!("{field} must not be empty"));
        }
    }
    if let Some(value) = config.max_calls
        && !(1..=MAX_CRITIC_MAX_CALLS).contains(&value)
    {
        return Err(format!("max_calls must be between 1 and {MAX_CRITIC_MAX_CALLS}"));
    }
    for (field, value, max) in [
        ("max_context_bytes", config.max_context_bytes, MAX_CRITIC_CONTEXT_BYTES),
        ("max_output_bytes", config.max_output_bytes, MAX_CRITIC_OUTPUT_BYTES),
    ] {
        if value.is_some_and(|value| value == 0 || value > max) {
            return Err(format!("{field} must be between 1 and {max}"));
        }
    }
    if config.timeout_ms.is_some_and(|value| value == 0 || value > MAX_CRITIC_TIMEOUT_MS) {
        return Err(format!("timeout_ms must be between 1 and {MAX_CRITIC_TIMEOUT_MS}"));
    }
    Ok(())
}
