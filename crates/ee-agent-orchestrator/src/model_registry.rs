//! Process-owned model registry with explicit, non-secret identity metadata.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::OrchestratorError;
use crate::model::ModelAdapter;
use crate::model_router::ModelTier;

/// Id of the default parent adapter in a registry.
pub const DEFAULT_MODEL_ID: &str = "default";
/// Stable role name preferred for bounded critic work.
pub const RUBBER_DUCK_ROLE: &str = "rubber_duck";
const MAX_CAPABILITIES: usize = 16;
const MAX_ROLES: usize = 16;

/// Declared model family. Contrast decisions use only this typed identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ModelFamily {
    OpenAi,
    Anthropic,
    Google,
    DeepSeek,
    Meta,
    Mistral,
    Qwen,
    Xai,
    Other(String),
}

impl ModelFamily {
    /// Validates explicit `other` identity and returns normalized family.
    pub fn validate(self) -> Result<Self, OrchestratorError> {
        if let Self::Other(identity) = &self {
            validate_metadata_id("model family", identity)?;
        }
        Ok(self)
    }
}

impl FromStr for ModelFamily {
    type Err = OrchestratorError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        let family = match value.to_ascii_lowercase().as_str() {
            "openai" => Self::OpenAi,
            "anthropic" => Self::Anthropic,
            "google" | "gemini" => Self::Google,
            "deepseek" => Self::DeepSeek,
            "meta" | "llama" => Self::Meta,
            "mistral" => Self::Mistral,
            "qwen" => Self::Qwen,
            "xai" | "grok" => Self::Xai,
            _ => {
                let Some(identity) = value.strip_prefix("other:") else {
                    return Err(OrchestratorError::InvalidState(format!(
                        "unknown model family {value:?}; use other:<identity> for explicit custom families"
                    )));
                };
                Self::Other(identity.to_string())
            }
        };
        family.validate()
    }
}

/// Bounded capabilities used for deterministic routing decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ModelCapability {
    ChatCompletion,
    Tools,
    Streaming,
    Vision,
}

/// Stable provider/model identity. Never contains credentials or clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelIdentity {
    pub model_id: String,
    pub provider_id: String,
    pub family: ModelFamily,
    pub display_name: String,
    pub capabilities: BTreeSet<ModelCapability>,
}

impl ModelIdentity {
    pub fn new(
        model_id: impl Into<String>,
        provider_id: impl Into<String>,
        family: ModelFamily,
        display_name: impl Into<String>,
        capabilities: impl IntoIterator<Item = ModelCapability>,
    ) -> Result<Self, OrchestratorError> {
        let identity = Self {
            model_id: model_id.into(),
            provider_id: provider_id.into(),
            family: family.validate()?,
            display_name: display_name.into(),
            capabilities: capabilities.into_iter().collect(),
        };
        identity.validate()?;
        Ok(identity)
    }

    fn validate(&self) -> Result<(), OrchestratorError> {
        validate_metadata_id("model id", &self.model_id)?;
        validate_metadata_id("provider id", &self.provider_id)?;
        if self.display_name.trim().is_empty() || self.display_name.chars().count() > 128 {
            return Err(OrchestratorError::InvalidState(
                "model display name must contain 1 through 128 characters".into(),
            ));
        }
        if self.capabilities.len() > MAX_CAPABILITIES {
            return Err(OrchestratorError::InvalidState(format!(
                "model capabilities exceed limit {MAX_CAPABILITIES}"
            )));
        }
        Ok(())
    }
}

/// Public, non-secret registry entry metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelInfo {
    pub id: String,
    pub identity: ModelIdentity,
    /// Compatibility display hint; authoritative identity lives in `identity`.
    pub display_name: Option<String>,
    /// Compatibility hints; typed decisions use `identity.capabilities` only.
    pub capabilities: Vec<String>,
    pub roles: Vec<String>,
    pub tier: ModelTier,
    pub enabled: bool,
}

/// Registration metadata controlling deterministic routes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRegistration {
    pub identity: ModelIdentity,
    pub roles: Vec<String>,
    pub tier: ModelTier,
    pub enabled: bool,
}

impl ModelRegistration {
    #[must_use]
    pub fn new(identity: ModelIdentity) -> Self {
        Self { identity, roles: Vec::new(), tier: ModelTier::Cheap, enabled: true }
    }

    #[must_use]
    pub fn for_roles(mut self, roles: &[&str]) -> Self {
        self.roles = roles.iter().map(|role| (*role).to_string()).collect();
        self
    }

    #[must_use]
    pub fn tier(mut self, tier: ModelTier) -> Self {
        self.tier = tier;
        self
    }

    #[must_use]
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    fn validate(&self) -> Result<(), OrchestratorError> {
        self.identity.validate()?;
        if self.roles.len() > MAX_ROLES {
            return Err(OrchestratorError::InvalidState(format!(
                "model roles exceed limit {MAX_ROLES}"
            )));
        }
        for role in &self.roles {
            validate_metadata_id("model role", role)?;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct RegistryEntry {
    adapter: Arc<dyn ModelAdapter>,
    registration: ModelRegistration,
}

/// Why no trustworthy contrasting model can be selected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum ContrastUnavailable {
    UnknownActiveIdentity { active_id: String },
    NoAlternative,
    SameFamilyOnly { family: ModelFamily },
    MissingCapability { required: BTreeSet<ModelCapability> },
    DisabledRoute,
}

/// Selected contrasting adapter and auditable identity pair.
#[derive(Clone)]
pub struct ContrastingModel {
    pub active: ModelInfo,
    pub selected: ModelInfo,
    pub adapter: Arc<dyn ModelAdapter>,
}

impl fmt::Debug for ContrastingModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ContrastingModel")
            .field("active", &self.active)
            .field("selected", &self.selected)
            .finish_non_exhaustive()
    }
}

/// Registry of named process-owned adapters, deterministic in route-id order.
#[derive(Clone)]
pub struct ModelRegistry {
    entries: BTreeMap<String, RegistryEntry>,
}

impl ModelRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self { entries: BTreeMap::new() }
    }

    #[must_use]
    pub fn single(model: Arc<dyn ModelAdapter>) -> Self {
        let mut registry = Self::new();
        let identity = ModelIdentity::new(
            DEFAULT_MODEL_ID,
            "internal",
            ModelFamily::Other("unspecified".into()),
            "Default model",
            [ModelCapability::ChatCompletion],
        )
        .expect("built-in identity is valid");
        registry
            .register_model(DEFAULT_MODEL_ID, model, ModelRegistration::new(identity))
            .expect("default model id is unique in an empty registry");
        registry
    }

    /// Compatibility registration. Such entries remain deliberately unsuitable
    /// for family contrast because their family is `other:unspecified`.
    pub fn register(
        &mut self,
        id: impl Into<String>,
        adapter: Arc<dyn ModelAdapter>,
    ) -> Result<(), OrchestratorError> {
        let id = id.into();
        let identity = ModelIdentity::new(
            id.clone(),
            "internal",
            ModelFamily::Other("unspecified".into()),
            id.clone(),
            [ModelCapability::ChatCompletion],
        )?;
        self.register_model(id, adapter, ModelRegistration::new(identity))
    }

    /// Compatibility registration for existing advertised hints. Only known,
    /// bounded capability values are retained; free-form text never establishes
    /// trustworthy model contrast.
    pub fn register_with_hints(
        &mut self,
        id: impl Into<String>,
        adapter: Arc<dyn ModelAdapter>,
        display_name: Option<String>,
        capabilities: Vec<String>,
    ) -> Result<(), OrchestratorError> {
        let id = id.into();
        let capabilities = capabilities.into_iter().filter_map(|hint| match hint.as_str() {
            "chat_completion" => Some(ModelCapability::ChatCompletion),
            "tools" => Some(ModelCapability::Tools),
            "streaming" => Some(ModelCapability::Streaming),
            "vision" => Some(ModelCapability::Vision),
            _ => None,
        });
        let identity = ModelIdentity::new(
            id.clone(),
            "internal",
            ModelFamily::Other("unspecified".into()),
            display_name.unwrap_or_else(|| id.clone()),
            capabilities,
        )?;
        self.register_model(id, adapter, ModelRegistration::new(identity))
    }

    pub fn register_model(
        &mut self,
        id: impl Into<String>,
        adapter: Arc<dyn ModelAdapter>,
        registration: ModelRegistration,
    ) -> Result<(), OrchestratorError> {
        let id = id.into();
        validate_metadata_id("registry model id", &id)?;
        registration.validate()?;
        if self.entries.contains_key(&id) {
            return Err(OrchestratorError::InvalidState(format!("duplicate model id: {id}")));
        }
        if self
            .entries
            .values()
            .any(|entry| entry.registration.identity.model_id == registration.identity.model_id)
        {
            return Err(OrchestratorError::InvalidState(format!(
                "duplicate stable model id: {}",
                registration.identity.model_id
            )));
        }
        self.entries.insert(id, RegistryEntry { adapter, registration });
        Ok(())
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<dyn ModelAdapter>> {
        self.entries.get(id).map(|entry| entry.adapter.clone())
    }

    #[must_use]
    pub fn info(&self, id: &str) -> Option<ModelInfo> {
        self.entries.get(id).map(|entry| model_info(id, entry))
    }

    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }

    /// Resolves one explicitly configured critic id while enforcing same contrast rules.
    pub fn select_configured_contrasting(
        &self,
        active_id: &str,
        critic_id: &str,
        required: &BTreeSet<ModelCapability>,
    ) -> Result<ContrastingModel, ContrastUnavailable> {
        let active = self.info(active_id).ok_or_else(|| {
            ContrastUnavailable::UnknownActiveIdentity { active_id: active_id.to_string() }
        })?;
        let Some(entry) = self.entries.get(critic_id) else {
            return Err(ContrastUnavailable::NoAlternative);
        };
        if critic_id == active_id
            || entry.registration.identity.model_id == active.identity.model_id
        {
            return Err(ContrastUnavailable::NoAlternative);
        }
        if entry.registration.identity.family == active.identity.family {
            return Err(ContrastUnavailable::SameFamilyOnly {
                family: active.identity.family.clone(),
            });
        }
        let mut required = required.clone();
        required.insert(ModelCapability::ChatCompletion);
        if !required.is_subset(&entry.registration.identity.capabilities) {
            return Err(ContrastUnavailable::MissingCapability { required });
        }
        if !entry.registration.enabled {
            return Err(ContrastUnavailable::DisabledRoute);
        }
        Ok(ContrastingModel {
            active,
            selected: model_info(critic_id, entry),
            adapter: entry.adapter.clone(),
        })
    }

    pub fn default_adapter(&self) -> Result<Arc<dyn ModelAdapter>, OrchestratorError> {
        self.get(DEFAULT_MODEL_ID).ok_or_else(|| {
            OrchestratorError::InvalidState(format!(
                "model registry has no {DEFAULT_MODEL_ID} adapter"
            ))
        })
    }

    #[must_use]
    pub fn advertised(&self) -> Vec<ModelInfo> {
        self.entries.iter().map(|(id, entry)| model_info(id, entry)).collect()
    }

    /// Selects different-id, different-family model with all required
    /// capabilities. Role route wins, then strong tier, then route/model id.
    pub fn select_contrasting(
        &self,
        active_id: &str,
        required: &BTreeSet<ModelCapability>,
    ) -> Result<ContrastingModel, ContrastUnavailable> {
        let active = self.info(active_id).ok_or_else(|| {
            ContrastUnavailable::UnknownActiveIdentity { active_id: active_id.to_string() }
        })?;
        let alternatives: Vec<_> = self
            .entries
            .iter()
            .filter(|(id, entry)| {
                id.as_str() != active_id
                    && entry.registration.identity.model_id != active.identity.model_id
            })
            .collect();
        if alternatives.is_empty() {
            return Err(ContrastUnavailable::NoAlternative);
        }
        let different_family: Vec<_> = alternatives
            .iter()
            .copied()
            .filter(|(_, entry)| entry.registration.identity.family != active.identity.family)
            .collect();
        if different_family.is_empty() {
            return Err(ContrastUnavailable::SameFamilyOnly {
                family: active.identity.family.clone(),
            });
        }
        let mut required = required.clone();
        required.insert(ModelCapability::ChatCompletion);
        let capable: Vec<_> = different_family
            .iter()
            .copied()
            .filter(|(_, entry)| required.is_subset(&entry.registration.identity.capabilities))
            .collect();
        if capable.is_empty() {
            return Err(ContrastUnavailable::MissingCapability { required });
        }
        let mut enabled: Vec<_> =
            capable.iter().copied().filter(|(_, entry)| entry.registration.enabled).collect();
        if enabled.is_empty() {
            return Err(ContrastUnavailable::DisabledRoute);
        }
        enabled.sort_by(|(a_id, a), (b_id, b)| {
            let a_role = a.registration.roles.iter().any(|role| role == RUBBER_DUCK_ROLE);
            let b_role = b.registration.roles.iter().any(|role| role == RUBBER_DUCK_ROLE);
            let a_strong = a.registration.tier == ModelTier::Strong;
            let b_strong = b.registration.tier == ModelTier::Strong;
            b_role
                .cmp(&a_role)
                .then(b_strong.cmp(&a_strong))
                .then(a_id.cmp(b_id))
                .then(a.registration.identity.model_id.cmp(&b.registration.identity.model_id))
        });
        let (selected_id, entry) = enabled[0];
        Ok(ContrastingModel {
            active,
            selected: model_info(selected_id, entry),
            adapter: entry.adapter.clone(),
        })
    }

    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn model_info(id: &str, entry: &RegistryEntry) -> ModelInfo {
    let identity = entry.registration.identity.clone();
    ModelInfo {
        id: id.to_string(),
        display_name: Some(identity.display_name.clone()),
        capabilities: identity
            .capabilities
            .iter()
            .map(|capability| match capability {
                ModelCapability::ChatCompletion => "chat_completion",
                ModelCapability::Tools => "tools",
                ModelCapability::Streaming => "streaming",
                ModelCapability::Vision => "vision",
            })
            .map(str::to_string)
            .collect(),
        identity,
        roles: entry.registration.roles.clone(),
        tier: entry.registration.tier,
        enabled: entry.registration.enabled,
    }
}

fn validate_metadata_id(field: &str, value: &str) -> Result<(), OrchestratorError> {
    if value.is_empty()
        || value.len() > 128
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/' | b':')
        })
    {
        return Err(OrchestratorError::InvalidState(format!(
            "{field} must contain 1 through 128 ASCII id characters"
        )));
    }
    Ok(())
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ModelRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelRegistry").field("models", &self.advertised()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelResponse;
    use crate::test_support::FakeModel;

    fn model() -> Arc<dyn ModelAdapter> {
        Arc::new(FakeModel::new(vec![ModelResponse::new().text("ok").completed()]))
    }

    fn identity(id: &str, family: ModelFamily, capabilities: &[ModelCapability]) -> ModelIdentity {
        ModelIdentity::new(id, "test", family, id, capabilities.iter().copied()).expect("identity")
    }

    #[test]
    fn single_registry_serves_default_and_degrades_without_contrast() {
        let registry = ModelRegistry::single(model());
        assert!(registry.default_adapter().is_ok());
        assert_eq!(
            registry.select_contrasting(DEFAULT_MODEL_ID, &BTreeSet::new()).unwrap_err(),
            ContrastUnavailable::NoAlternative
        );
    }

    #[test]
    fn malformed_and_duplicate_metadata_are_rejected() {
        assert!(ModelFamily::from_str("other:").is_err());
        assert!(ModelIdentity::new("", "test", ModelFamily::OpenAi, "x", []).is_err());
        assert!(ModelIdentity::new("x", "bad provider", ModelFamily::OpenAi, "x", []).is_err());
        let mut registry = ModelRegistry::single(model());
        assert!(registry.register(DEFAULT_MODEL_ID, model()).is_err());
        let duplicate_identity =
            identity(DEFAULT_MODEL_ID, ModelFamily::OpenAi, &[ModelCapability::ChatCompletion]);
        assert!(
            registry
                .register_model("alias", model(), ModelRegistration::new(duplicate_identity))
                .is_err()
        );
    }

    #[test]
    fn contrast_reports_unknown_active_and_same_family_only() {
        let mut registry = ModelRegistry::new();
        registry
            .register_model(
                DEFAULT_MODEL_ID,
                model(),
                ModelRegistration::new(identity(
                    "root",
                    ModelFamily::Anthropic,
                    &[ModelCapability::ChatCompletion],
                )),
            )
            .unwrap();
        assert_eq!(
            registry.select_contrasting("missing", &BTreeSet::new()).unwrap_err(),
            ContrastUnavailable::UnknownActiveIdentity { active_id: "missing".into() }
        );
        registry
            .register_model(
                "same-family",
                model(),
                ModelRegistration::new(identity(
                    "other-anthropic",
                    ModelFamily::Anthropic,
                    &[ModelCapability::ChatCompletion],
                )),
            )
            .unwrap();
        assert_eq!(
            registry.select_contrasting(DEFAULT_MODEL_ID, &BTreeSet::new()).unwrap_err(),
            ContrastUnavailable::SameFamilyOnly { family: ModelFamily::Anthropic }
        );
    }

    #[test]
    fn contrast_excludes_same_family_and_prefers_role_then_tier_stably() {
        let mut registry = ModelRegistry::new();
        registry
            .register_model(
                DEFAULT_MODEL_ID,
                model(),
                ModelRegistration::new(identity(
                    "root",
                    ModelFamily::Anthropic,
                    &[ModelCapability::ChatCompletion, ModelCapability::Tools],
                )),
            )
            .unwrap();
        registry
            .register_model(
                "a-strong",
                model(),
                ModelRegistration::new(identity(
                    "critic-a",
                    ModelFamily::OpenAi,
                    &[ModelCapability::ChatCompletion, ModelCapability::Tools],
                ))
                .tier(ModelTier::Strong),
            )
            .unwrap();
        registry
            .register_model(
                "z-duck",
                model(),
                ModelRegistration::new(identity(
                    "critic-z",
                    ModelFamily::Google,
                    &[ModelCapability::ChatCompletion, ModelCapability::Tools],
                ))
                .for_roles(&[RUBBER_DUCK_ROLE]),
            )
            .unwrap();
        registry
            .register_model(
                "same-family",
                model(),
                ModelRegistration::new(identity(
                    "same",
                    ModelFamily::Anthropic,
                    &[ModelCapability::ChatCompletion, ModelCapability::Tools],
                ))
                .for_roles(&[RUBBER_DUCK_ROLE])
                .tier(ModelTier::Strong),
            )
            .unwrap();
        let required = BTreeSet::from([ModelCapability::ChatCompletion, ModelCapability::Tools]);
        let selected = registry.select_contrasting(DEFAULT_MODEL_ID, &required).unwrap();
        assert_eq!(selected.active.identity.model_id, "root");
        assert_eq!(selected.selected.id, "z-duck");
        assert_ne!(selected.active.identity.family, selected.selected.identity.family);
    }

    #[test]
    fn contrast_reports_specific_capability_and_disabled_failures() {
        let mut registry = ModelRegistry::new();
        registry
            .register_model(
                DEFAULT_MODEL_ID,
                model(),
                ModelRegistration::new(identity(
                    "root",
                    ModelFamily::OpenAi,
                    &[ModelCapability::ChatCompletion],
                )),
            )
            .unwrap();
        registry
            .register_model(
                "critic",
                model(),
                ModelRegistration::new(identity(
                    "critic",
                    ModelFamily::Anthropic,
                    &[ModelCapability::ChatCompletion],
                ))
                .enabled(false),
            )
            .unwrap();
        let tools = BTreeSet::from([ModelCapability::Tools]);
        assert!(matches!(
            registry.select_contrasting(DEFAULT_MODEL_ID, &tools),
            Err(ContrastUnavailable::MissingCapability { .. })
        ));
        assert_eq!(
            registry.select_contrasting(DEFAULT_MODEL_ID, &BTreeSet::new()).unwrap_err(),
            ContrastUnavailable::DisabledRoute
        );
    }

    #[test]
    fn model_info_roundtrips_without_adapter_or_secrets() {
        let info = ModelInfo {
            id: "critic".into(),
            identity: identity(
                "provider/model",
                ModelFamily::Other("custom".into()),
                &[ModelCapability::ChatCompletion],
            ),
            display_name: Some("provider/model".into()),
            capabilities: vec!["chat_completion".into()],
            roles: vec![RUBBER_DUCK_ROLE.into()],
            tier: ModelTier::Strong,
            enabled: true,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(!json.contains("api_key"));
        assert_eq!(serde_json::from_str::<ModelInfo>(&json).unwrap(), info);
    }
}
