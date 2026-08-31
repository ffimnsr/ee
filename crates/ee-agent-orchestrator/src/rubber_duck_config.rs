//! Resolved rubber-duck policy translated from frontend-owned configuration.

use std::collections::BTreeSet;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Default maximum critique calls retained per session.
pub const DEFAULT_RUBBER_DUCK_MAX_CALLS: usize = 2;
/// Hard maximum critique calls accepted from configuration.
pub const MAX_RUBBER_DUCK_MAX_CALLS: usize = 16;
/// Default context byte cap.
pub const DEFAULT_RUBBER_DUCK_CONTEXT_BYTES: usize = 64 * 1024;
/// Hard context byte cap.
pub const MAX_RUBBER_DUCK_CONTEXT_BYTES: usize = 64 * 1024;
/// Default output byte cap.
pub const DEFAULT_RUBBER_DUCK_OUTPUT_BYTES: usize = 32 * 1024;
/// Hard output byte cap.
pub const MAX_RUBBER_DUCK_OUTPUT_BYTES: usize = 32 * 1024;
/// Default per-call timeout.
pub const DEFAULT_RUBBER_DUCK_TIMEOUT: Duration = Duration::from_secs(90);
/// Hard per-call timeout.
pub const MAX_RUBBER_DUCK_TIMEOUT: Duration = Duration::from_secs(300);

/// User-selected critique mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RubberDuckMode {
    Off,
    #[default]
    Manual,
    Automatic,
}

/// One unambiguous critic backend route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RubberDuckBackend {
    InternalModel { model_id: String },
    ExternalAgent { agent_id: String },
}

impl RubberDuckBackend {
    /// Resolves mutually exclusive frontend fields.
    pub fn from_optional_ids(
        internal_model_id: Option<String>,
        external_agent_id: Option<String>,
    ) -> Result<Option<Self>, RubberDuckConfigError> {
        match (internal_model_id, external_agent_id) {
            (Some(_), Some(_)) => Err(RubberDuckConfigError::AmbiguousBackend),
            (Some(model_id), None) => {
                validate_id("critic model id", &model_id)?;
                Ok(Some(Self::InternalModel { model_id }))
            }
            (None, Some(agent_id)) => {
                validate_id("critic agent id", &agent_id)?;
                Ok(Some(Self::ExternalAgent { agent_id }))
            }
            (None, None) => Ok(None),
        }
    }
}

/// Bounded critic policy. Config loading remains frontend-owned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RubberDuckConfig {
    pub mode: RubberDuckMode,
    pub backend: Option<RubberDuckBackend>,
    pub max_calls: usize,
    pub max_context_bytes: usize,
    pub max_output_bytes: usize,
    pub timeout: Duration,
}

impl Default for RubberDuckConfig {
    fn default() -> Self {
        Self {
            mode: RubberDuckMode::Manual,
            backend: None,
            max_calls: DEFAULT_RUBBER_DUCK_MAX_CALLS,
            max_context_bytes: DEFAULT_RUBBER_DUCK_CONTEXT_BYTES,
            max_output_bytes: DEFAULT_RUBBER_DUCK_OUTPUT_BYTES,
            timeout: DEFAULT_RUBBER_DUCK_TIMEOUT,
        }
    }
}

impl RubberDuckConfig {
    /// Validates limits and selected id shape.
    pub fn validate(&self) -> Result<(), RubberDuckConfigError> {
        if !(1..=MAX_RUBBER_DUCK_MAX_CALLS).contains(&self.max_calls) {
            return Err(RubberDuckConfigError::InvalidLimit {
                field: "max_calls",
                max: MAX_RUBBER_DUCK_MAX_CALLS,
            });
        }
        validate_limit("max_context_bytes", self.max_context_bytes, MAX_RUBBER_DUCK_CONTEXT_BYTES)?;
        validate_limit("max_output_bytes", self.max_output_bytes, MAX_RUBBER_DUCK_OUTPUT_BYTES)?;
        if self.timeout.is_zero() || self.timeout > MAX_RUBBER_DUCK_TIMEOUT {
            return Err(RubberDuckConfigError::InvalidTimeout);
        }
        match &self.backend {
            Some(RubberDuckBackend::InternalModel { model_id }) => {
                validate_id("critic model id", model_id)?
            }
            Some(RubberDuckBackend::ExternalAgent { agent_id }) => {
                validate_id("critic agent id", agent_id)?
            }
            None => {}
        }
        Ok(())
    }

    /// Resolves optional backend availability without breaking ordinary root operation.
    pub fn resolve(
        self,
        model_ids: &BTreeSet<String>,
        agent_ids: &BTreeSet<String>,
    ) -> Result<ResolvedRubberDuckConfig, RubberDuckConfigError> {
        self.validate()?;
        let unavailable = match &self.backend {
            Some(RubberDuckBackend::InternalModel { model_id })
                if !model_ids.contains(model_id) =>
            {
                Some(RubberDuckConfigUnavailable::UnknownModel { model_id: model_id.clone() })
            }
            Some(RubberDuckBackend::ExternalAgent { agent_id })
                if !agent_ids.contains(agent_id) =>
            {
                Some(RubberDuckConfigUnavailable::UnknownAgent { agent_id: agent_id.clone() })
            }
            _ => None,
        };
        Ok(ResolvedRubberDuckConfig { config: self, unavailable })
    }
}

/// Valid resolved policy plus optional critic-only degradation reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedRubberDuckConfig {
    pub config: RubberDuckConfig,
    pub unavailable: Option<RubberDuckConfigUnavailable>,
}

impl ResolvedRubberDuckConfig {
    #[must_use]
    pub fn critic_available(&self) -> bool {
        self.config.mode != RubberDuckMode::Off && self.unavailable.is_none()
    }
}

/// Optional backend unavailable after frontend resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum RubberDuckConfigUnavailable {
    UnknownModel { model_id: String },
    UnknownAgent { agent_id: String },
}

/// Invalid user configuration. Unlike unavailable optional backend, this rejects resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RubberDuckConfigError {
    AmbiguousBackend,
    InvalidId { field: &'static str },
    InvalidLimit { field: &'static str, max: usize },
    InvalidTimeout,
}

impl std::fmt::Display for RubberDuckConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AmbiguousBackend => write!(
                formatter,
                "rubber duck internal_model_id and external_agent_id are mutually exclusive"
            ),
            Self::InvalidId { field } => write!(formatter, "{field} is empty or malformed"),
            Self::InvalidLimit { field, max } => {
                write!(formatter, "rubber duck {field} must be between 1 and {max}")
            }
            Self::InvalidTimeout => write!(
                formatter,
                "rubber duck timeout must be between 1ms and {}s",
                MAX_RUBBER_DUCK_TIMEOUT.as_secs()
            ),
        }
    }
}

impl std::error::Error for RubberDuckConfigError {}

fn validate_limit(
    field: &'static str,
    value: usize,
    max: usize,
) -> Result<(), RubberDuckConfigError> {
    if value == 0 || value > max {
        return Err(RubberDuckConfigError::InvalidLimit { field, max });
    }
    Ok(())
}

fn validate_id(field: &'static str, value: &str) -> Result<(), RubberDuckConfigError> {
    if value.is_empty()
        || value.len() > 128
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/' | b':')
        })
    {
        return Err(RubberDuckConfigError::InvalidId { field });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_and_unavailable_reason_roundtrip() {
        let config = RubberDuckConfig {
            mode: RubberDuckMode::Automatic,
            backend: RubberDuckBackend::from_optional_ids(Some("critic/model".into()), None)
                .unwrap(),
            ..RubberDuckConfig::default()
        };
        let resolved = config.resolve(&BTreeSet::new(), &BTreeSet::new()).unwrap();
        assert!(matches!(
            resolved.unavailable,
            Some(RubberDuckConfigUnavailable::UnknownModel { .. })
        ));
        let json = serde_json::to_string(&resolved).unwrap();
        assert_eq!(serde_json::from_str::<ResolvedRubberDuckConfig>(&json).unwrap(), resolved);
    }

    #[test]
    fn ambiguous_backend_and_invalid_limits_fail_closed() {
        assert_eq!(
            RubberDuckBackend::from_optional_ids(Some("model".into()), Some("agent".into())),
            Err(RubberDuckConfigError::AmbiguousBackend)
        );
        let config = RubberDuckConfig { max_calls: 0, ..RubberDuckConfig::default() };
        assert!(matches!(config.validate(), Err(RubberDuckConfigError::InvalidLimit { .. })));
    }
}
