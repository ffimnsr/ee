//! Model registry: explicit model ids → adapters, with an advertised list.
//!
//! [`ModelRegistry`] maps stable model ids to [`ModelAdapter`] instances so
//! delegation can choose which adapter a subagent runs on.  The id
//! [`DEFAULT_MODEL_ID`] always names the default (parent) adapter; role
//! selections resolve through the registry and unknown ids fail closed before
//! any child task node exists.  The advertised [`ModelInfo`] list (ids plus
//! optional display name and capability hints) is handed to the delegating
//! model so it can pick — never carrying provider secrets or credentials.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::OrchestratorError;
use crate::model::ModelAdapter;

/// Id of the default (parent) adapter in a registry.
pub const DEFAULT_MODEL_ID: &str = "default";

/// Advertised model information: the id plus optional display name and
/// capability hints.  Never carries provider secrets or credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelInfo {
    /// Stable id used in `SubagentRole::model` and `delegate_task` arguments.
    pub id: String,
    /// Optional human-readable display name.
    pub display_name: Option<String>,
    /// Optional capability hints (e.g. `tools`, `streaming`).
    pub capabilities: Vec<String>,
}

struct RegistryEntry {
    adapter: Arc<dyn ModelAdapter>,
    display_name: Option<String>,
    capabilities: Vec<String>,
}

/// Registry of named model adapters, deterministic in id order.
pub struct ModelRegistry {
    entries: BTreeMap<String, RegistryEntry>,
}

impl ModelRegistry {
    /// Creates an empty registry.  A runtime built from it fails closed until
    /// a `default` entry is registered.
    #[must_use]
    pub fn new() -> Self {
        Self { entries: BTreeMap::new() }
    }

    /// Single-adapter registry: `model` under [`DEFAULT_MODEL_ID`].  Keeps
    /// existing single-adapter construction working unchanged.
    #[must_use]
    pub fn single(model: Arc<dyn ModelAdapter>) -> Self {
        let mut registry = Self::new();
        registry
            .register(DEFAULT_MODEL_ID, model)
            .expect("default model id is unique in an empty registry");
        registry
    }

    /// Registers `adapter` under `id`; rejects empty and duplicate ids.
    pub fn register(
        &mut self,
        id: impl Into<String>,
        adapter: Arc<dyn ModelAdapter>,
    ) -> Result<(), OrchestratorError> {
        self.register_with_hints(id, adapter, None, Vec::new())
    }

    /// Registers an adapter with optional display name and capability hints
    /// for the advertised model list; rejects empty and duplicate ids.
    pub fn register_with_hints(
        &mut self,
        id: impl Into<String>,
        adapter: Arc<dyn ModelAdapter>,
        display_name: Option<String>,
        capabilities: Vec<String>,
    ) -> Result<(), OrchestratorError> {
        let id = id.into();
        if id.is_empty() {
            return Err(OrchestratorError::InvalidState("model id must not be empty".into()));
        }
        if self.entries.contains_key(&id) {
            return Err(OrchestratorError::InvalidState(format!("duplicate model id: {id}")));
        }
        self.entries.insert(id, RegistryEntry { adapter, display_name, capabilities });
        Ok(())
    }

    /// Looks up an adapter by id.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<dyn ModelAdapter>> {
        self.entries.get(id).map(|entry| entry.adapter.clone())
    }

    /// Whether `id` is registered.
    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }

    /// The default adapter; fails closed when no `default` entry exists.
    pub fn default_adapter(&self) -> Result<Arc<dyn ModelAdapter>, OrchestratorError> {
        self.get(DEFAULT_MODEL_ID).ok_or_else(|| {
            OrchestratorError::InvalidState(format!(
                "model registry has no {DEFAULT_MODEL_ID} adapter"
            ))
        })
    }

    /// Advertised model list, sorted by id.
    #[must_use]
    pub fn advertised(&self) -> Vec<ModelInfo> {
        self.entries
            .iter()
            .map(|(id, entry)| ModelInfo {
                id: id.clone(),
                display_name: entry.display_name.clone(),
                capabilities: entry.capabilities.clone(),
            })
            .collect()
    }

    /// Every registered id, sorted.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    /// Number of registered adapters.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ModelRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelRegistry").field("ids", &self.ids()).finish()
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

    #[test]
    fn single_registry_serves_the_default_id() {
        let registry = ModelRegistry::single(model());
        assert_eq!(registry.len(), 1);
        assert!(registry.contains(DEFAULT_MODEL_ID));
        assert!(registry.default_adapter().is_ok());
        assert_eq!(registry.ids(), vec![DEFAULT_MODEL_ID.to_string()]);
        assert_eq!(
            registry.advertised(),
            vec![ModelInfo {
                id: DEFAULT_MODEL_ID.to_string(),
                display_name: None,
                capabilities: Vec::new(),
            }]
        );
    }

    #[test]
    fn duplicate_model_ids_are_rejected() {
        let mut registry = ModelRegistry::single(model());
        let duplicate =
            registry.register(DEFAULT_MODEL_ID, model()).expect_err("duplicate rejected");
        assert!(
            matches!(duplicate, OrchestratorError::InvalidState(reason) if reason.contains("duplicate model id"))
        );
        let empty = registry.register("", model()).expect_err("empty id rejected");
        assert!(matches!(empty, OrchestratorError::InvalidState(_)));
    }

    #[test]
    fn unknown_model_id_lookup_fails() {
        let registry = ModelRegistry::single(model());
        assert!(!registry.contains("nope"));
        assert!(registry.get("nope").is_none());
        let error = registry.default_adapter();
        let missing = ModelRegistry::new();
        assert!(matches!(
            missing.default_adapter(),
            Err(OrchestratorError::InvalidState(reason)) if reason.contains("no default adapter")
        ));
        assert!(error.is_ok());
    }

    #[test]
    fn registered_models_are_advertised_with_hints_in_id_order() {
        let mut registry = ModelRegistry::single(model());
        registry
            .register_with_hints(
                "strong",
                model(),
                Some("Strong Model".to_string()),
                vec!["tools".to_string(), "streaming".to_string()],
            )
            .expect("registers strong");
        registry.register("fast", model()).expect("registers fast");
        assert_eq!(registry.ids(), vec!["default", "fast", "strong"]);
        let advertised = registry.advertised();
        assert_eq!(advertised[0].id, "default");
        assert_eq!(advertised[0].display_name, None);
        assert_eq!(advertised[1].id, "fast");
        assert_eq!(advertised[2].id, "strong");
        assert_eq!(advertised[2].display_name.as_deref(), Some("Strong Model"));
        assert_eq!(advertised[2].capabilities, vec!["tools", "streaming"]);
        assert!(registry.get("fast").is_some());
    }

    #[test]
    fn model_info_roundtrips_through_json() {
        let info = ModelInfo {
            id: "strong".into(),
            display_name: Some("Strong".into()),
            capabilities: vec!["tools".into()],
        };
        let json = serde_json::to_string(&info).expect("serializes");
        let restored: ModelInfo = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, info);
    }

    #[test]
    fn empty_registry_is_empty() {
        let registry = ModelRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.advertised().is_empty());
    }
}
