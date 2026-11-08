//! Plugin lifecycle: discovery, resolution, activation, reload, rollback, and
//! LIFO disposal.
//!
//! The manager is host-agnostic. It only knows [`HostAdapter`] plus the
//! in-process registries supplied through [`PluginManagerConfig`] builders.

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use ee_plugin_api::context::{CommandRegistry, ContributionSink, PluginContext};
use ee_plugin_api::disposer::Disposer;
use ee_plugin_api::error::{ErrorClass, PluginError};
use ee_plugin_api::event::EventBus;
use ee_plugin_api::host::{HostAdapter, HostKind};
use ee_plugin_api::ids::{ContributionId, ServiceId};
use ee_plugin_api::manifest::{HostTarget, ManifestV2, PluginKind};
use ee_plugin_api::service::ServiceRegistry;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::activation::{self, ActivationTrigger};
use crate::builtin::BuiltinCatalog;
use crate::config::{
    ConfigLayers, InterpolationContext, context_for_plugin, plugin_config_table, plugin_enabled,
    resolve_plugin_config,
};
use crate::contributions::{collect_manifest_contributions, contribution_key};
use crate::diagnostics::{Diagnostic, DiagnosticPhase};
use crate::discovery::{DiscoveryOptions, RootKind, discover};
use crate::health::{HealthPolicy, HealthReason, HealthSnapshot, HealthState, HealthTracker};
use crate::host_impl::{MemoryCommandRegistry, MemoryContributionSink};
use crate::native::{NativeClient, NativeSpec};
use crate::resolver::{self, Resolution};

/// Configuration for one manager instance.
#[derive(Debug, Clone)]
pub struct PluginManagerConfig {
    pub host_kind: HostKind,
    pub discovery: DiscoveryOptions,
    pub config_dir: PathBuf,
    pub workspace: PathBuf,
    pub env: BTreeMap<String, String>,
    pub env_allowlist: BTreeSet<String>,
    pub host_config: Value,
    pub workspace_config: Value,
    pub runtime_config: BTreeMap<String, Value>,
    pub policy: HealthPolicy,
}

impl PluginManagerConfig {
    pub fn new(
        host_kind: HostKind,
        config_dir: impl Into<PathBuf>,
        workspace: impl Into<PathBuf>,
    ) -> Self {
        Self {
            host_kind,
            discovery: DiscoveryOptions::default(),
            config_dir: config_dir.into(),
            workspace: workspace.into(),
            env: BTreeMap::new(),
            env_allowlist: BTreeSet::new(),
            host_config: Value::Object(serde_json::Map::new()),
            workspace_config: Value::Object(serde_json::Map::new()),
            runtime_config: BTreeMap::new(),
            policy: HealthPolicy::default(),
        }
    }

    pub fn with_discovery(mut self, discovery: DiscoveryOptions) -> Self {
        self.discovery = discovery;
        self
    }

    pub fn with_runtime_config(mut self, name: impl Into<String>, value: Value) -> Self {
        self.runtime_config.insert(name.into(), value);
        self
    }

    pub fn with_host_config(mut self, value: Value) -> Self {
        self.host_config = value;
        self
    }

    pub fn with_workspace_config(mut self, value: Value) -> Self {
        self.workspace_config = value;
        self
    }

    pub fn with_policy(mut self, policy: HealthPolicy) -> Self {
        self.policy = policy;
        self
    }
}

/// Status for one known plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginStatus {
    pub name: String,
    pub version: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_root: Option<RootKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_hash: Option<String>,
    pub active: bool,
    pub health: HealthSnapshot,
}

/// Status for the whole manager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerStatus {
    pub generation: u64,
    pub plugins: Vec<PluginStatus>,
}

#[derive(Debug, Clone)]
struct OriginInfo {
    root: RootKind,
    manifest_path: PathBuf,
    manifest_dir: PathBuf,
    origin_hash: String,
}

enum PluginInstance {
    Builtin { context: PluginContext },
    Native(Box<NativeClient>),
}

struct RunningPlugin {
    manifest: ManifestV2,
    origin: OriginInfo,
    instance: PluginInstance,
    provided: Vec<ServiceId>,
    contribution_disposers: Vec<Disposer>,
}

/// Host-agnostic plugin manager.
pub struct PluginManager {
    adapter: Arc<dyn HostAdapter>,
    config: PluginManagerConfig,
    builtins: BuiltinCatalog,
    services: Arc<RwLock<ServiceRegistry>>,
    events: EventBus,
    commands: Arc<dyn CommandRegistry>,
    contributions: Arc<dyn ContributionSink>,
    manifests: BTreeMap<String, ManifestV2>,
    origins: BTreeMap<String, OriginInfo>,
    running: Vec<RunningPlugin>,
    deferred: BTreeSet<String>,
    activating: BTreeSet<String>,
    health: BTreeMap<String, HealthTracker>,
    blocked: BTreeMap<String, Diagnostic>,
    diagnostics: Vec<Diagnostic>,
    generation: u64,
}

impl PluginManager {
    pub fn new(adapter: Arc<dyn HostAdapter>, config: PluginManagerConfig) -> Self {
        Self {
            adapter,
            config,
            builtins: BuiltinCatalog::new(),
            services: Arc::new(RwLock::new(ServiceRegistry::new())),
            events: EventBus::new(),
            commands: Arc::new(MemoryCommandRegistry::new()),
            contributions: Arc::new(MemoryContributionSink::new()),
            manifests: BTreeMap::new(),
            origins: BTreeMap::new(),
            running: Vec::new(),
            deferred: BTreeSet::new(),
            activating: BTreeSet::new(),
            health: BTreeMap::new(),
            blocked: BTreeMap::new(),
            diagnostics: Vec::new(),
            generation: 0,
        }
    }

    pub fn with_builtins(mut self, builtins: BuiltinCatalog) -> Self {
        self.builtins = builtins;
        self
    }

    pub fn with_services(mut self, services: Arc<RwLock<ServiceRegistry>>) -> Self {
        self.services = services;
        self
    }

    pub fn with_events(mut self, events: EventBus) -> Self {
        self.events = events;
        self
    }

    pub fn with_command_registry(mut self, commands: Arc<dyn CommandRegistry>) -> Self {
        self.commands = commands;
        self
    }

    pub fn with_contribution_sink(mut self, contributions: Arc<dyn ContributionSink>) -> Self {
        self.contributions = contributions;
        self
    }

    pub fn services(&self) -> Arc<RwLock<ServiceRegistry>> {
        Arc::clone(&self.services)
    }

    pub fn events(&self) -> EventBus {
        self.events.clone()
    }

    pub fn command_registry(&self) -> Arc<dyn CommandRegistry> {
        Arc::clone(&self.commands)
    }

    pub fn contribution_sink(&self) -> Arc<dyn ContributionSink> {
        Arc::clone(&self.contributions)
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    pub fn manifests(&self) -> &BTreeMap<String, ManifestV2> {
        &self.manifests
    }

    /// Discover, resolve, and activate startup plugins.
    ///
    /// Per-plugin failures become diagnostics; unrelated plugins keep working.
    pub fn start(&mut self) -> Result<(), PluginError> {
        if !self.running.is_empty() {
            return Ok(());
        }

        self.reset_loading_state();
        let mut discovery_options = self.config.discovery.clone();
        if let Some(paths) = self.config.env.get("EE_PLUGIN_PATH") {
            discovery_options.push_env_paths(paths);
        }
        let discovery = discover(&discovery_options);
        self.diagnostics.extend(discovery.diagnostics.clone());

        for (name, manifest) in self.builtins.manifests() {
            self.manifests.insert(name.clone(), manifest.clone());
            self.origins.insert(
                name.clone(),
                OriginInfo {
                    root: RootKind::Bundled,
                    manifest_path: PathBuf::from(format!("builtin:{name}")),
                    manifest_dir: PathBuf::new(),
                    origin_hash: format!("builtin:{name}"),
                },
            );
        }

        for discovered in discovery.plugins {
            let name = discovered.manifest.name.clone();
            if self.manifests.contains_key(&name) {
                let diagnostic = Diagnostic::new(
                    ErrorClass::Manifest,
                    "plugin.manifest",
                    DiagnosticPhase::Discovery,
                    format!(
                        "plugin `{name}` is already provided by a builtin; external `{}` is blocked",
                        discovered.manifest_path.display()
                    ),
                )
                .with_plugin(name.clone())
                .with_origin(discovered.manifest_path.clone());
                self.block_plugin(
                    name.clone(),
                    diagnostic,
                    HealthReason::Other("duplicate".into()),
                );
                continue;
            }

            self.manifests.insert(name.clone(), discovered.manifest.clone());
            self.origins.insert(
                name.clone(),
                OriginInfo {
                    root: discovered.root,
                    manifest_path: discovered.manifest_path.clone(),
                    manifest_dir: discovered.manifest_dir.clone(),
                    origin_hash: discovered.origin_hash.clone(),
                },
            );

            if let Some(diagnostic) = discovered.blocked {
                let reason = if diagnostic.class == ErrorClass::Permission {
                    HealthReason::Other("untrusted workspace".into())
                } else {
                    HealthReason::Other("duplicate".into())
                };
                self.block_plugin(name, diagnostic, reason);
            }
        }

        self.apply_enablement();
        self.apply_host_targets();

        let resolution = resolver::resolve_with_priority(
            &self.manifests,
            &self.builtins.manifests().map(|(name, _)| name.clone()).collect(),
        );
        for diagnostic in &resolution.blocked {
            if let Some(name) = &diagnostic.plugin {
                self.block_plugin(
                    name.clone(),
                    diagnostic.clone(),
                    HealthReason::DependencyMissing,
                );
            }
        }
        self.apply_contribution_conflicts(&resolution);

        // Defer plugins whose service providers are deferred.
        for name in &resolution.order {
            if self.blocked.contains_key(name) {
                continue;
            }
            if self
                .manifests
                .get(name)
                .is_some_and(|manifest| !activation::activates_at_startup(&manifest.activation))
            {
                self.deferred.insert(name.clone());
            }
        }
        let mut changed = true;
        while changed {
            changed = false;
            for name in &resolution.order {
                if self.blocked.contains_key(name) || self.deferred.contains(name) {
                    continue;
                }
                let Some(manifest) = self.manifests.get(name) else {
                    continue;
                };
                if manifest.requires.services.iter().any(|service| {
                    self.provider_for_service(service)
                        .is_some_and(|provider| self.deferred.contains(&provider))
                }) {
                    self.deferred.insert(name.clone());
                    changed = true;
                }
            }
        }

        for name in resolution.order {
            if self.blocked.contains_key(&name) || self.deferred.contains(&name) {
                continue;
            }
            if let Err(error) = self.activate(&name) {
                let diagnostic = Diagnostic::from_plugin_error(
                    &error,
                    DiagnosticPhase::Activation,
                    name.clone(),
                    self.origins.get(&name).map(|origin| origin.manifest_path.clone()),
                );
                self.block_plugin(name, diagnostic, HealthReason::ApplyFailed);
            }
        }

        self.generation = self.generation.saturating_add(1);
        self.dedupe_diagnostics();
        Ok(())
    }

    /// Activate a deferred plugin, activating its providers first.
    pub fn activate(&mut self, name: &str) -> Result<(), PluginError> {
        if self.running.iter().any(|plugin| plugin.manifest.name == name) {
            return Ok(());
        }
        if self.blocked.contains_key(name) {
            return Err(PluginError::Dependency { message: format!("plugin `{name}` is blocked") });
        }
        let manifest =
            self.manifests.get(name).cloned().ok_or_else(|| PluginError::Runtime {
                message: format!("unknown plugin `{name}`"),
            })?;
        if !self.activating.insert(name.to_owned()) {
            return Err(PluginError::Dependency {
                message: format!("activation cycle involving `{name}`"),
            });
        }

        let result = (|| {
            for service in &manifest.requires.services {
                if let Some(provider) = self.provider_for_service(service)
                    && provider != name
                    && !self.running.iter().any(|p| p.manifest.name == provider)
                {
                    self.activate(&provider)?;
                }
            }
            let plugin = self.build_plugin(&manifest)?;
            self.deferred.remove(name);
            self.running.push(plugin);
            self.health
                .entry(name.to_owned())
                .or_insert_with(|| HealthTracker::new(self.config.policy))
                .mark_loaded();
            Ok(())
        })();

        self.activating.remove(name);
        result
    }

    /// Activate every deferred plugin that matches a language trigger.
    pub fn activate_for_language(&mut self, language: &str) -> Vec<(String, PluginError)> {
        self.activate_matching(&ActivationTrigger::language(language))
    }

    /// Activate every deferred plugin that matches a command trigger.
    pub fn activate_for_command(&mut self, command: &str) -> Vec<(String, PluginError)> {
        self.activate_matching(&ActivationTrigger::command(command))
    }

    /// Activate every deferred plugin that matches an event trigger.
    pub fn activate_for_event(&mut self, event: &str) -> Vec<(String, PluginError)> {
        self.activate_matching(&ActivationTrigger::event(event))
    }

    fn activate_matching(&mut self, trigger: &ActivationTrigger<'_>) -> Vec<(String, PluginError)> {
        let candidates: Vec<String> = self
            .deferred
            .iter()
            .filter(|name| {
                self.manifests.get(*name).is_some_and(|manifest| {
                    activation::should_activate(&manifest.activation, trigger)
                })
            })
            .cloned()
            .collect();
        let mut failures = Vec::new();
        for name in candidates {
            if let Err(error) = self.activate(&name) {
                failures.push((name, error));
            }
        }
        failures
    }

    /// Stop every running plugin in reverse activation order.
    pub fn stop(&mut self) {
        let running = std::mem::take(&mut self.running);
        for mut plugin in running.into_iter().rev() {
            for disposer in plugin.contribution_disposers.drain(..).rev() {
                disposer.dispose();
            }
            let name = plugin.manifest.name.clone();
            let provided = plugin.provided.clone();
            drop(plugin);
            if let Ok(mut registry) = self.services.write() {
                for id in &provided {
                    registry.remove(id);
                }
            }
            if let Some(health) = self.health.get_mut(&name) {
                health.mark_disabled(HealthReason::Stopped);
            }
        }
        self.deferred.clear();
        self.activating.clear();
    }

    /// Reload one plugin. A failed candidate keeps the old instance live.
    pub fn reload(&mut self, name: &str) -> Result<(), PluginError> {
        let Some(index) = self.running.iter().position(|plugin| plugin.manifest.name == name)
        else {
            return self.activate(name);
        };

        let old_manifest = self.running[index].manifest.clone();
        let provided = self.running[index].provided.clone();

        // Temporarily release provided services so the candidate can register.
        let mut stash: Vec<(ServiceId, Arc<dyn Any + Send + Sync>)> = Vec::new();
        if let Ok(mut registry) = self.services.write() {
            for id in &provided {
                if let Some(service) = registry.remove(id) {
                    stash.push((id.clone(), service));
                }
            }
        }

        let old_disposers = std::mem::take(&mut self.running[index].contribution_disposers);
        for disposer in old_disposers.iter().rev() {
            disposer.dispose();
        }
        drop(old_disposers);

        match self.build_plugin(&old_manifest) {
            Ok(mut candidate) => {
                if old_manifest.kind == PluginKind::Builtin {
                    let missing = candidate
                        .provided
                        .iter()
                        .filter(|id| {
                            self.services
                                .read()
                                .map(|registry| !registry.contains(id))
                                .unwrap_or(true)
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    if !missing.is_empty() {
                        let error = PluginError::Dependency {
                            message: format!(
                                "plugin `{name}` did not register advertised services: {}",
                                missing
                                    .iter()
                                    .map(ToString::to_string)
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                        };
                        for disposer in candidate.contribution_disposers.drain(..).rev() {
                            disposer.dispose();
                        }
                        drop(candidate);
                        self.rollback_reload(name, index, &old_manifest, stash, &error)?;
                        return Err(error);
                    }
                }

                for disposer in candidate.contribution_disposers.drain(..) {
                    self.running[index].contribution_disposers.push(disposer);
                }
                let old = std::mem::replace(&mut self.running[index], candidate);
                drop(old);
                drop(stash);
                if let Some(health) = self.health.get_mut(name) {
                    health.mark_reloaded();
                }
                self.generation = self.generation.saturating_add(1);
                Ok(())
            }
            Err(error) => {
                self.rollback_reload(name, index, &old_manifest, stash, &error)?;
                Err(error)
            }
        }
    }

    fn rollback_reload(
        &mut self,
        name: &str,
        index: usize,
        old_manifest: &ManifestV2,
        stash: Vec<(ServiceId, Arc<dyn Any + Send + Sync>)>,
        error: &PluginError,
    ) -> Result<(), PluginError> {
        // Remove any services the failed candidate may have registered.
        if let Ok(mut registry) = self.services.write() {
            for raw in &old_manifest.provides.services {
                if let Ok(id) = raw.parse::<ServiceId>() {
                    registry.remove(&id);
                }
            }
            for (id, service) in stash {
                let _ = registry.register_erased(id, service);
            }
        }

        let mut restored = Vec::new();
        for contribution in collect_manifest_contributions(old_manifest) {
            if let Ok(disposer) = self.contributions.contribute(contribution) {
                restored.push(disposer);
            }
        }
        self.running[index].contribution_disposers = restored;

        let diagnostic = Diagnostic::from_plugin_error(
            error,
            DiagnosticPhase::Activation,
            name.to_owned(),
            self.origins.get(name).map(|origin| origin.manifest_path.clone()),
        );
        self.diagnostics.push(diagnostic);
        if let Some(health) = self.health.get_mut(name) {
            health.mark_degraded(HealthReason::ApplyFailed);
        }
        Ok(())
    }

    /// Record a crash for a known plugin and return the restart backoff.
    pub fn record_crash(&mut self, name: &str, uptime_ms: u64) -> Option<u64> {
        self.health.get_mut(name).map(|health| health.record_crash(uptime_ms))
    }

    /// Record a resource breach and return the restart backoff.
    pub fn record_resource_breach(&mut self, name: &str) -> Option<u64> {
        self.health.get_mut(name).map(HealthTracker::record_resource_breach)
    }

    /// Current restart backoff for a known plugin.
    pub fn restart_delay_ms(&self, name: &str) -> Option<u64> {
        self.health.get(name).map(HealthTracker::restart_delay_ms)
    }

    /// Status for every discovered plugin.
    pub fn status(&self) -> ManagerStatus {
        let mut plugins = Vec::with_capacity(self.manifests.len());
        for (name, manifest) in &self.manifests {
            let running = self.running.iter().find(|plugin| &plugin.manifest.name == name);
            let origin_info =
                running.map(|plugin| &plugin.origin).or_else(|| self.origins.get(name));
            let kind = if let Some(plugin) = running {
                match &plugin.instance {
                    PluginInstance::Builtin { context } => {
                        let _ = context.plugin_id();
                        "builtin".to_owned()
                    }
                    PluginInstance::Native(client) => {
                        let _ = client.plugin_id();
                        "native".to_owned()
                    }
                }
            } else {
                manifest.kind.as_str().to_owned()
            };
            let health =
                self.health.get(name).map(HealthTracker::snapshot).unwrap_or(HealthSnapshot {
                    state: HealthState::Loaded,
                    reason: None,
                    crashes: 0,
                    consecutive_failures: 0,
                    restart_delay_ms: 0,
                });
            plugins.push(PluginStatus {
                name: name.clone(),
                version: manifest.version.clone(),
                kind,
                origin: origin_info.map(|origin| origin.manifest_path.clone()),
                origin_root: origin_info.map(|origin| origin.root),
                origin_hash: origin_info.map(|origin| origin.origin_hash.clone()),
                active: running.is_some(),
                health,
            });
        }
        ManagerStatus { generation: self.generation, plugins }
    }

    /// Whether a plugin is currently active.
    pub fn is_active(&self, name: &str) -> bool {
        self.running.iter().any(|plugin| plugin.manifest.name == name)
    }

    fn reset_loading_state(&mut self) {
        self.manifests.clear();
        self.origins.clear();
        self.deferred.clear();
        self.activating.clear();
        self.health.clear();
        self.blocked.clear();
        self.diagnostics.clear();
    }

    fn apply_enablement(&mut self) {
        let names: Vec<String> = self.manifests.keys().cloned().collect();
        for name in names {
            let enabled = plugin_enabled(&self.config.workspace_config, &name)
                .or_else(|| plugin_enabled(&self.config.host_config, &name))
                .unwrap_or(true);
            if !enabled {
                let diagnostic = Diagnostic::new(
                    ErrorClass::Config,
                    "plugin.config",
                    DiagnosticPhase::Config,
                    format!("plugin `{name}` is disabled by configuration"),
                )
                .with_plugin(name.clone());
                self.block_plugin(
                    name,
                    diagnostic,
                    HealthReason::Other("disabled by configuration".into()),
                );
            }
        }
    }

    fn apply_host_targets(&mut self) {
        let host = match self.config.host_kind {
            HostKind::Editor => HostTarget::Editor,
            HostKind::Agent => HostTarget::Agent,
        };
        let names: Vec<String> = self.manifests.keys().cloned().collect();
        for name in names {
            let Some(manifest) = self.manifests.get(&name) else {
                continue;
            };
            if !manifest.targets.contains(&host) {
                let diagnostic = Diagnostic::new(
                    ErrorClass::Manifest,
                    "plugin.manifest",
                    DiagnosticPhase::Activation,
                    format!(
                        "plugin `{name}` does not target the {} host",
                        self.config.host_kind.as_str()
                    ),
                )
                .with_plugin(name.clone());
                self.block_plugin(
                    name,
                    diagnostic,
                    HealthReason::Other("host target mismatch".into()),
                );
            }
        }
    }

    fn apply_contribution_conflicts(&mut self, resolution: &Resolution) {
        let mut claimed: BTreeMap<(ContributionId, &'static str), String> = BTreeMap::new();
        for name in &resolution.order {
            if self.blocked.contains_key(name) {
                continue;
            }
            let Some(manifest) = self.manifests.get(name) else {
                continue;
            };
            let contributions = collect_manifest_contributions(manifest);
            if contributions.len() > manifest.limits.max_contributions {
                let diagnostic = Diagnostic::new(
                    ErrorClass::Resource,
                    "plugin.resource",
                    DiagnosticPhase::Activation,
                    format!(
                        "plugin `{name}` declares {} contributions, limit is {}",
                        contributions.len(),
                        manifest.limits.max_contributions
                    ),
                )
                .with_plugin(name.clone());
                self.block_plugin(name.clone(), diagnostic, HealthReason::ResourceLimit);
                continue;
            }

            let mut conflict = None;
            for contribution in &contributions {
                let key = contribution_key(contribution);
                if let Some(owner) = claimed.get(&key)
                    && owner != name
                {
                    conflict = Some(format!(
                        "contribution `{}` ({}) is already provided by `{owner}`",
                        key.0, key.1
                    ));
                    break;
                }
            }
            if let Some(message) = conflict {
                let diagnostic = Diagnostic::new(
                    ErrorClass::Contribution,
                    "plugin.contribution",
                    DiagnosticPhase::Activation,
                    message,
                )
                .with_plugin(name.clone());
                self.block_plugin(name.clone(), diagnostic, HealthReason::ContributionConflict);
                continue;
            }

            for contribution in &contributions {
                claimed.insert(contribution_key(contribution), name.clone());
            }
        }
    }

    fn build_plugin(&self, manifest: &ManifestV2) -> Result<RunningPlugin, PluginError> {
        let name = manifest.name.clone();
        let origin = self.origins.get(&name).cloned().ok_or_else(|| PluginError::Runtime {
            message: format!("missing origin for plugin `{name}`"),
        })?;
        let config = self.resolve_config(manifest)?;
        let provided = manifest
            .provides
            .services
            .iter()
            .filter_map(|raw| raw.parse::<ServiceId>().ok())
            .collect::<Vec<_>>();

        let instance = match manifest.kind {
            PluginKind::Builtin => {
                let descriptor = self.builtins.get(&name).ok_or_else(|| {
                    PluginError::Manifest(ee_plugin_api::manifest::ManifestError::InvalidEntry {
                        entry: manifest.entry.clone(),
                        kind: "builtin (not in catalog)",
                    })
                })?;
                let mut plugin = descriptor.create();
                let context = PluginContext::new(name.clone())
                    .with_services(Arc::clone(&self.services))
                    .with_events(self.events.clone())
                    .with_command_registry(Arc::clone(&self.commands))
                    .with_contribution_sink(Arc::clone(&self.contributions))
                    .configured_with(config.clone());
                plugin.apply(&context)?;
                PluginInstance::Builtin { context }
            }
            PluginKind::Native => {
                let services =
                    self.services.read().map(|registry| registry.ids()).unwrap_or_default();
                let spec = NativeSpec::from_manifest(
                    manifest,
                    &origin.manifest_dir,
                    self.config.host_kind,
                    services,
                    config,
                )?;
                let client = NativeClient::spawn(
                    spec,
                    Arc::clone(&self.adapter),
                    Arc::clone(&self.contributions),
                )?;
                PluginInstance::Native(Box::new(client))
            }
            PluginKind::Wasm => {
                return Err(PluginError::Unsupported {
                    feature: "wasm plugin transport (Phase 6)",
                });
            }
        };

        let mut contribution_disposers = Vec::new();
        for contribution in collect_manifest_contributions(manifest) {
            match self.contributions.contribute(contribution) {
                Ok(disposer) => contribution_disposers.push(disposer),
                Err(error) => {
                    for disposer in contribution_disposers.into_iter().rev() {
                        disposer.dispose();
                    }
                    return Err(error);
                }
            }
        }

        Ok(RunningPlugin {
            manifest: manifest.clone(),
            origin,
            instance,
            provided,
            contribution_disposers,
        })
    }

    fn resolve_config(&self, manifest: &ManifestV2) -> Result<Value, PluginError> {
        let name = &manifest.name;
        let origin = self.origins.get(name);
        let plugin_dir = origin.map(|origin| origin.manifest_dir.clone()).unwrap_or_default();
        let context = context_for_plugin(
            InterpolationContext::new(&self.config.config_dir, &self.config.workspace)
                .with_env(self.config.env.clone())
                .allow_env(self.config.env_allowlist.iter().cloned()),
            &plugin_dir,
        );
        let layers = ConfigLayers::new()
            .host(plugin_config_table(&self.config.host_config, name))
            .workspace(plugin_config_table(&self.config.workspace_config, name))
            .runtime(self.config.runtime_config.get(name));
        resolve_plugin_config(manifest, &layers, &context)
    }

    fn provider_for_service(&self, raw: &str) -> Option<String> {
        let service: ServiceId = raw.parse().ok()?;
        self.manifests.iter().find_map(|(name, manifest)| {
            manifest
                .provides
                .services
                .iter()
                .any(|provided| provided.parse::<ServiceId>().ok().as_ref() == Some(&service))
                .then(|| name.clone())
        })
    }

    fn block_plugin(&mut self, name: String, diagnostic: Diagnostic, reason: HealthReason) {
        self.blocked.insert(name.clone(), diagnostic.clone());
        self.health
            .entry(name)
            .or_insert_with(|| HealthTracker::new(self.config.policy))
            .mark_blocked(reason);
        self.diagnostics.push(diagnostic);
    }

    fn dedupe_diagnostics(&mut self) {
        self.diagnostics.sort_by(|left, right| {
            left.plugin
                .cmp(&right.plugin)
                .then(left.origin.cmp(&right.origin))
                .then(left.code.cmp(&right.code))
                .then(left.message.cmp(&right.message))
        });
        self.diagnostics.dedup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_builder_sets_runtime_layer() {
        let config =
            PluginManagerConfig::new(HostKind::Editor, "/tmp/ee-config", "/tmp/ee-workspace")
                .with_runtime_config("demo", serde_json::json!({"a": 1}));
        assert_eq!(config.runtime_config["demo"]["a"], 1);
    }
}
