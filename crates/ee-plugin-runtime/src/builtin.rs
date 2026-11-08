//! Builtin plugin descriptors and the generated-catalog boundary.
//!
//! Builtins use the same `Plugin::apply` path as external plugins. The catalog
//! is data-driven so core never branches on plugin names.

use std::collections::BTreeMap;
use std::sync::Arc;

use ee_plugin_api::context::Plugin;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::manifest::{Activation, HostTarget, ManifestV2, PluginKind, Provides, Requires};

/// Factory invoked on every activation/reload generation.
pub type BuiltinFactory = Arc<dyn Fn() -> Box<dyn Plugin> + Send + Sync>;

/// One builtin plugin entry.
#[derive(Clone)]
pub struct BuiltinDescriptor {
    pub manifest: ManifestV2,
    pub factory: BuiltinFactory,
}

impl BuiltinDescriptor {
    pub fn new(manifest: ManifestV2, factory: BuiltinFactory) -> Self {
        Self { manifest, factory }
    }

    pub fn create(&self) -> Box<dyn Plugin> {
        (self.factory)()
    }
}

impl std::fmt::Debug for BuiltinDescriptor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BuiltinDescriptor")
            .field("manifest", &self.manifest)
            .finish_non_exhaustive()
    }
}

/// Catalog of builtin descriptors, keyed by manifest name.
#[derive(Default, Clone)]
pub struct BuiltinCatalog {
    entries: BTreeMap<String, BuiltinDescriptor>,
}

impl BuiltinCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, descriptor: BuiltinDescriptor) -> Result<(), PluginError> {
        let manifest = &descriptor.manifest;
        if manifest.kind != PluginKind::Builtin {
            return Err(PluginError::Manifest(
                ee_plugin_api::manifest::ManifestError::InvalidEntry {
                    entry: manifest.entry.clone(),
                    kind: manifest.kind.as_str(),
                },
            ));
        }
        let expected = format!("builtin:{}", manifest.name);
        if manifest.entry != expected {
            return Err(PluginError::Manifest(
                ee_plugin_api::manifest::ManifestError::InvalidEntry {
                    entry: manifest.entry.clone(),
                    kind: manifest.kind.as_str(),
                },
            ));
        }
        manifest.validate()?;
        if self.entries.contains_key(&manifest.name) {
            return Err(PluginError::Runtime {
                message: format!("builtin plugin `{}` is already registered", manifest.name),
            });
        }
        self.entries.insert(manifest.name.clone(), descriptor);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&BuiltinDescriptor> {
        self.entries.get(name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    pub fn manifests(&self) -> impl Iterator<Item = (&String, &ManifestV2)> {
        self.entries.iter().map(|(name, descriptor)| (name, &descriptor.manifest))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Build a valid builtin manifest for tests and generated catalogs.
pub fn builtin_manifest(name: &str, host: HostTarget, provides: &[&str]) -> ManifestV2 {
    ManifestV2 {
        manifest_version: 2,
        name: name.to_owned(),
        version: "0.0.0".to_owned(),
        api: ee_plugin_api::manifest::API_VERSION.to_owned(),
        targets: vec![host],
        kind: PluginKind::Builtin,
        entry: format!("builtin:{name}"),
        launch: Default::default(),
        activation: Activation { on_startup: true, ..Default::default() },
        requires: Requires::default(),
        provides: Provides { services: provides.iter().map(|value| (*value).to_owned()).collect() },
        permissions: Default::default(),
        limits: Default::default(),
        contributes: Default::default(),
        config: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noop() -> Box<dyn Plugin> {
        struct Noop;
        impl Plugin for Noop {
            fn name(&self) -> &str {
                "demo"
            }
            fn apply(
                &mut self,
                _ctx: &ee_plugin_api::context::PluginContext,
            ) -> Result<(), PluginError> {
                Ok(())
            }
        }
        Box::new(Noop)
    }

    #[test]
    fn registers_and_resolves_builtins() {
        let mut catalog = BuiltinCatalog::new();
        let manifest = builtin_manifest("demo", HostTarget::Editor, &["demo.service@1"]);
        catalog.insert(BuiltinDescriptor::new(manifest, Arc::new(noop))).unwrap();
        assert!(catalog.contains("demo"));
        assert_eq!(catalog.get("demo").unwrap().create().name(), "demo");
        assert_eq!(catalog.manifests().count(), 1);
    }

    #[test]
    fn rejects_duplicate_and_mismatched_descriptors() {
        let mut catalog = BuiltinCatalog::new();
        let manifest = builtin_manifest("demo", HostTarget::Editor, &[]);
        catalog.insert(BuiltinDescriptor::new(manifest.clone(), Arc::new(noop))).unwrap();
        assert!(catalog.insert(BuiltinDescriptor::new(manifest, Arc::new(noop))).is_err());

        let mut wrong = builtin_manifest("other", HostTarget::Editor, &[]);
        wrong.entry = "builtin:wrong".to_owned();
        assert!(catalog.insert(BuiltinDescriptor::new(wrong, Arc::new(noop))).is_err());
    }
}
