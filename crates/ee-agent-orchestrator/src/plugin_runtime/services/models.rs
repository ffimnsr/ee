//! `agent.models@1`: model adapter registration, metadata, and routing hints.
//!
//! The host keeps deterministic routing authority: plugins may register adapters
//! and propose routes, but selection still runs through [`ModelRouter`], whose
//! tier preference and stable id ordering decide the winner.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};

use ee_plugin_api::disposer::Disposer;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::ids::PluginId;
use ee_plugin_api::service::Service;
use serde::{Deserialize, Serialize};

use crate::error::OrchestratorError;
use crate::events::EventRecorder;
use crate::model::ModelAdapter;
use crate::model_registry::{ModelInfo, ModelRegistration, ModelRegistry};
use crate::model_router::{ModelRoute, ModelRouter, TaskKind};

/// Maximum adapters a single plugin may register.
pub const MAX_PLUGIN_MODELS: usize = 16;

/// Non-secret hints a plugin may attach to one adapter.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ModelMetadata {
    /// Display name hint; the registration identity remains authoritative.
    pub display_name: Option<String>,
    /// Bounded capability hints (`chat_completion`, `tools`, `streaming`, `vision`).
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone)]
struct OwnedModel {
    plugin: Option<PluginId>,
}

#[derive(Debug, Clone)]
struct RouteEntry {
    plugin: Option<PluginId>,
    route: ModelRoute,
}

/// Deterministic snapshot of the service state for provider construction.
#[derive(Clone)]
pub struct ModelServiceSnapshot {
    pub registry: ModelRegistry,
    pub routes: Vec<ModelRoute>,
}

/// Model registry service.
pub struct ModelService {
    registry: Arc<RwLock<ModelRegistry>>,
    owners: Arc<Mutex<BTreeMap<String, OwnedModel>>>,
    routes: Arc<RwLock<BTreeMap<String, RouteEntry>>>,
    metadata: Arc<RwLock<BTreeMap<String, ModelMetadata>>>,
    max_models_per_plugin: usize,
}

impl Service for ModelService {
    const ID: &'static str = "agent.models@1";
}

impl Default for ModelService {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelService {
    #[must_use]
    pub fn new() -> Self {
        Self {
            registry: Arc::new(RwLock::new(ModelRegistry::new())),
            owners: Arc::new(Mutex::new(BTreeMap::new())),
            routes: Arc::new(RwLock::new(BTreeMap::new())),
            metadata: Arc::new(RwLock::new(BTreeMap::new())),
            max_models_per_plugin: MAX_PLUGIN_MODELS,
        }
    }

    /// Imports a provider-owned registry (root and critic adapters) so plugin
    /// metadata and routes see the same identities the provider executes.
    pub fn adopt_registry(&self, registry: &ModelRegistry) -> Result<(), PluginError> {
        let mut target = self.registry.write().expect("model registry lock poisoned");
        let mut owners = self.owners.lock().expect("model ownership lock poisoned");
        for (id, adapter, registration) in registry.registrations() {
            if target.contains(&id) {
                continue;
            }
            target
                .register_model(id.clone(), adapter, registration)
                .map_err(|error| PluginError::Runtime { message: error.to_string() })?;
            owners.insert(id, OwnedModel { plugin: None });
        }
        Ok(())
    }

    /// Registers one adapter for `plugin`.
    pub fn register_adapter(
        &self,
        plugin: &PluginId,
        id: impl Into<String>,
        adapter: Arc<dyn ModelAdapter>,
        registration: ModelRegistration,
    ) -> Result<Disposer, PluginError> {
        self.register_adapter_with_metadata(
            plugin,
            id,
            adapter,
            registration,
            ModelMetadata::default(),
        )
    }

    /// Registers one adapter together with non-secret display/routing hints.
    pub fn register_adapter_with_metadata(
        &self,
        plugin: &PluginId,
        id: impl Into<String>,
        adapter: Arc<dyn ModelAdapter>,
        registration: ModelRegistration,
        metadata: ModelMetadata,
    ) -> Result<Disposer, PluginError> {
        let id = id.into();
        {
            let owners = self.owners.lock().expect("model ownership lock poisoned");
            let count =
                owners.values().filter(|owned| owned.plugin.as_ref() == Some(plugin)).count();
            if count >= self.max_models_per_plugin {
                return Err(PluginError::Resource {
                    message: format!(
                        "plugin `{plugin}` exceeded the model registration limit of {}",
                        self.max_models_per_plugin
                    ),
                });
            }
        }
        self.registry
            .write()
            .expect("model registry lock poisoned")
            .register_model(id.clone(), adapter, registration)
            .map_err(|error| PluginError::Runtime { message: error.to_string() })?;
        self.owners
            .lock()
            .expect("model ownership lock poisoned")
            .insert(id.clone(), OwnedModel { plugin: Some(plugin.clone()) });
        if metadata != ModelMetadata::default() {
            self.metadata
                .write()
                .expect("model metadata lock poisoned")
                .insert(id.clone(), metadata);
        }

        let registry = Arc::clone(&self.registry);
        let owners = Arc::clone(&self.owners);
        let metadata = Arc::clone(&self.metadata);
        let plugin = plugin.clone();
        Ok(Disposer::new(move || {
            let mut owners = owners.lock().expect("model ownership lock poisoned");
            if owners.get(&id).is_some_and(|owned| owned.plugin.as_ref() == Some(&plugin)) {
                owners.remove(&id);
                if let Ok(mut registry) = registry.write() {
                    registry.remove(&id);
                }
                if let Ok(mut metadata) = metadata.write() {
                    metadata.remove(&id);
                }
            }
        }))
    }

    /// Registers a deterministic routing hint.
    pub fn register_route(
        &self,
        plugin: &PluginId,
        route: ModelRoute,
    ) -> Result<Disposer, PluginError> {
        let id = route.route_id.clone();
        let mut routes = self.routes.write().expect("model route lock poisoned");
        if routes.contains_key(&id) {
            return Err(PluginError::Runtime {
                message: format!("duplicate model route id `{id}`"),
            });
        }
        routes.insert(id.clone(), RouteEntry { plugin: Some(plugin.clone()), route });
        drop(routes);

        let handle = Arc::clone(&self.routes);
        let plugin = plugin.clone();
        Ok(Disposer::new(move || {
            if let Ok(mut routes) = handle.write()
                && routes.get(&id).is_some_and(|entry| entry.plugin.as_ref() == Some(&plugin))
            {
                routes.remove(&id);
            }
        }))
    }

    /// Deterministic router over every registered route, sorted by route id.
    pub fn router(&self) -> Result<ModelRouter, OrchestratorError> {
        let mut routes: Vec<ModelRoute> = self
            .routes
            .read()
            .expect("model route lock poisoned")
            .values()
            .map(|entry| entry.route.clone())
            .collect();
        routes.sort_by(|left, right| left.route_id.cmp(&right.route_id));
        ModelRouter::new(routes)
    }

    /// Resolves the route for a task kind and optional subagent role.
    ///
    /// Fails closed when no route matches or when the selected route references
    /// an adapter that is not registered.
    pub fn resolve(
        &self,
        kind: TaskKind,
        role: Option<&str>,
    ) -> Result<ModelRoute, OrchestratorError> {
        let router = self.router()?;
        let recorder = EventRecorder::new();
        let route = router.select(kind, role, &recorder)?.clone();
        if !self.contains(&route.adapter_id) {
            return Err(OrchestratorError::InvalidState(format!(
                "model route {} references unknown adapter {}",
                route.route_id, route.adapter_id
            )));
        }
        Ok(route)
    }

    /// Snapshot for provider construction and tests.
    pub fn snapshot(&self) -> ModelServiceSnapshot {
        let registry = self.registry.read().expect("model registry lock poisoned").clone();
        let mut routes: Vec<ModelRoute> = self
            .routes
            .read()
            .expect("model route lock poisoned")
            .values()
            .map(|entry| entry.route.clone())
            .collect();
        routes.sort_by(|left, right| left.route_id.cmp(&right.route_id));
        ModelServiceSnapshot { registry, routes }
    }

    #[must_use]
    pub fn info(&self, id: &str) -> Option<ModelInfo> {
        self.registry.read().expect("model registry lock poisoned").info(id)
    }

    #[must_use]
    pub fn metadata(&self, id: &str) -> Option<ModelMetadata> {
        self.metadata.read().expect("model metadata lock poisoned").get(id).cloned()
    }

    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.registry.read().expect("model registry lock poisoned").contains(id)
    }

    /// Sorted registered adapter ids.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.registry
            .read()
            .expect("model registry lock poisoned")
            .registrations()
            .into_iter()
            .map(|(id, _, _)| id)
            .collect()
    }

    /// Performs one adapter lookup through the shared registry.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<dyn ModelAdapter>> {
        self.registry.read().expect("model registry lock poisoned").get(id)
    }
}

impl std::fmt::Debug for ModelService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ModelService").field("models", &self.ids()).finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    #![allow(unused_must_use)]
    use super::*;
    use crate::model_registry::{ModelCapability, ModelFamily, ModelIdentity};
    use crate::test_support::FakeModel;

    fn plugin() -> PluginId {
        PluginId::new("demo").expect("valid plugin id")
    }

    fn adapter(id: &str) -> Arc<dyn ModelAdapter> {
        let _ = id;
        Arc::new(FakeModel::new(Vec::new()))
    }

    fn registration(id: &str) -> ModelRegistration {
        ModelRegistration::new(
            ModelIdentity::new(
                id,
                "demo",
                ModelFamily::Other("demo".into()),
                id,
                [ModelCapability::ChatCompletion],
            )
            .expect("valid identity"),
        )
    }

    #[test]
    fn registers_and_removes_adapters() {
        let service = ModelService::new();
        let disposer = service
            .register_adapter(
                &plugin(),
                "demo-model",
                adapter("demo-model"),
                registration("demo-model"),
            )
            .expect("registers");
        assert!(service.contains("demo-model"));
        disposer.dispose();
        assert!(!service.contains("demo-model"));
    }

    #[test]
    fn duplicate_model_id_is_rejected() {
        let service = ModelService::new();
        service
            .register_adapter(
                &plugin(),
                "demo-model",
                adapter("demo-model"),
                registration("demo-model"),
            )
            .expect("registers");
        let error = service
            .register_adapter(
                &plugin(),
                "demo-model",
                adapter("demo-model"),
                registration("demo-model"),
            )
            .expect_err("duplicate rejected");
        assert!(matches!(error, PluginError::Runtime { .. }));
    }

    #[test]
    fn routes_resolve_deterministically() {
        let service = ModelService::new();
        service
            .register_adapter(
                &plugin(),
                "cheap-model",
                adapter("cheap-model"),
                registration("cheap-model").tier(crate::model_router::ModelTier::Cheap),
            )
            .expect("registers cheap");
        service
            .register_adapter(
                &plugin(),
                "strong-model",
                adapter("strong-model"),
                registration("strong-model").tier(crate::model_router::ModelTier::Strong),
            )
            .expect("registers strong");
        service
            .register_route(
                &plugin(),
                ModelRoute::new("strong", "strong-model", crate::model_router::ModelTier::Strong)
                    .for_kinds(&[TaskKind::Implementation]),
            )
            .expect("registers route");
        service
            .register_route(
                &plugin(),
                ModelRoute::new("cheap", "cheap-model", crate::model_router::ModelTier::Cheap),
            )
            .expect("registers route");
        let route = service.resolve(TaskKind::Implementation, None).expect("route");
        assert_eq!(route.adapter_id, "strong-model");
        let route = service.resolve(TaskKind::Simple, None).expect("route");
        assert_eq!(route.adapter_id, "cheap-model");
    }

    #[test]
    fn missing_adapter_fails_closed() {
        let service = ModelService::new();
        service
            .register_route(
                &plugin(),
                ModelRoute::new("orphan", "missing-model", crate::model_router::ModelTier::Cheap),
            )
            .expect("route registers");
        let error = service.resolve(TaskKind::Simple, None).expect_err("missing adapter fails");
        assert!(matches!(error, OrchestratorError::InvalidState(_)));
    }

    #[test]
    fn adopts_provider_registry_metadata() {
        let mut registry = ModelRegistry::new();
        registry
            .register_model("default", adapter("default"), registration("default"))
            .expect("registers");
        let service = ModelService::new();
        service.adopt_registry(&registry).expect("adopts");
        assert!(service.contains("default"));
        assert_eq!(service.ids(), vec!["default".to_owned()]);
    }
}
