//! Editor plugin host.
//!
//! Owns discovery/activation (`PluginManager`), the builtin service registry,
//! command/contribution registries, and the effect queue. Plugin callbacks never
//! run while the core holds a mutable borrow: effects are queued here and applied
//! by [`crate::tabs::CoreState`] after the event context is dropped.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use ee_plugin_api::contribution::{Contribution, LanguageContribution, LspServerContribution};
use ee_plugin_api::disposer::Disposer;
use ee_plugin_api::effect::Effect;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::event::Event;
use ee_plugin_api::host::{HostAdapter, HostKind};
use ee_plugin_api::ids::{ContributionId, PluginId, ServiceId};
use ee_plugin_api::service::ServiceRegistry;
use ee_plugin_api::wire::ServiceRequestParams;
use ee_plugin_runtime::builtin::BuiltinCatalog;
use ee_plugin_runtime::contributions::contribution_key;
use ee_plugin_runtime::manager::{ManagerStatus, PluginManager, PluginManagerConfig};
use serde::Serialize;
use serde_json::Value;

use crate::plugin_runtime::commands::EditorCommandRegistry;
use crate::plugin_runtime::contributions::EditorContributionRegistry;
use crate::plugin_runtime::core_api::{DocumentSnapshot, SharedEditorAccess};
use crate::plugin_runtime::services::{
    ConfigService, DecorationService, DocumentEditService, DocumentService, FsService, LspService,
    SelectionService, SyntaxService, TaskService, UiService, ViewsService, fs::FsScope,
    router::EditorServiceRouter,
};

/// One queued effect with its originating plugin.
#[derive(Debug, Clone)]
pub struct QueuedEffect {
    pub owner: Option<PluginId>,
    pub effect: Effect,
}

/// Host-owned notification sink for frontend status and contributions.
pub trait HostNotifier: Send + Sync + 'static {
    fn plugin_contributions(
        &self,
        revision: u64,
        upsert: Vec<Contribution>,
        remove: Vec<ee_plugin_api::ids::ContributionId>,
    );
    fn plugin_status(&self, status: &ManagerStatus);
    fn plugin_health(&self, plugin: &str, health: &str, reason: Option<&str>);
    fn alert(&self, message: &str);
}

/// Notifier event queued for the core thread.
#[derive(Debug, Clone)]
pub enum NotifierEvent {
    Contributions {
        revision: u64,
        upsert: Vec<Contribution>,
        remove: Vec<ee_plugin_api::ids::ContributionId>,
    },
    Status(ManagerStatus),
    Health {
        plugin: String,
        state: String,
        reason: Option<String>,
    },
    Alert(String),
}

/// Thread-safe notifier that queues events for the core thread to send.
#[derive(Default)]
pub struct QueuedNotifier {
    events: Mutex<Vec<NotifierEvent>>,
}

impl QueuedNotifier {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn drain(&self) -> Vec<NotifierEvent> {
        std::mem::take(&mut *self.events.lock().expect("notifier lock poisoned"))
    }
}

impl HostNotifier for QueuedNotifier {
    fn plugin_contributions(
        &self,
        revision: u64,
        upsert: Vec<Contribution>,
        remove: Vec<ee_plugin_api::ids::ContributionId>,
    ) {
        self.events.lock().expect("notifier lock poisoned").push(NotifierEvent::Contributions {
            revision,
            upsert,
            remove,
        });
    }

    fn plugin_status(&self, status: &ManagerStatus) {
        self.events
            .lock()
            .expect("notifier lock poisoned")
            .push(NotifierEvent::Status(status.clone()));
    }

    fn plugin_health(&self, plugin: &str, health: &str, reason: Option<&str>) {
        self.events.lock().expect("notifier lock poisoned").push(NotifierEvent::Health {
            plugin: plugin.to_owned(),
            state: health.to_owned(),
            reason: reason.map(str::to_owned),
        });
    }

    fn alert(&self, message: &str) {
        self.events
            .lock()
            .expect("notifier lock poisoned")
            .push(NotifierEvent::Alert(message.to_owned()));
    }
}

/// Notifier used by tests and hosts without a frontend.
#[derive(Default)]
pub struct NoopNotifier;

impl HostNotifier for NoopNotifier {
    fn plugin_contributions(
        &self,
        _revision: u64,
        _upsert: Vec<Contribution>,
        _remove: Vec<ee_plugin_api::ids::ContributionId>,
    ) {
    }

    fn plugin_status(&self, _status: &ManagerStatus) {}

    fn plugin_health(&self, _plugin: &str, _health: &str, _reason: Option<&str>) {}

    fn alert(&self, _message: &str) {}
}

/// Result of emitting one event.
#[derive(Debug, Default)]
pub struct EmissionReport {
    pub failures: Vec<(String, PluginError)>,
}

impl EmissionReport {
    pub fn is_ok(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Adapter handed to the runtime.
pub struct EditorHostAdapter {
    router: EditorServiceRouter,
    effects: Mutex<Vec<QueuedEffect>>,
}

impl EditorHostAdapter {
    pub fn new(router: EditorServiceRouter) -> Self {
        Self { router, effects: Mutex::new(Vec::new()) }
    }

    pub fn take_effects(&self) -> Vec<QueuedEffect> {
        std::mem::take(&mut *self.effects.lock().expect("effect lock poisoned"))
    }
}

impl HostAdapter for EditorHostAdapter {
    fn host_kind(&self) -> HostKind {
        HostKind::Editor
    }

    fn submit_effects(
        &self,
        plugin: Option<&PluginId>,
        effects: Vec<Effect>,
    ) -> Result<(), PluginError> {
        let mut queue = self.effects.lock().expect("effect lock poisoned");
        queue.extend(
            effects.into_iter().map(|effect| QueuedEffect { owner: plugin.cloned(), effect }),
        );
        Ok(())
    }

    fn handle_service_request(
        &self,
        plugin: &PluginId,
        request: &ServiceRequestParams,
    ) -> Result<Value, PluginError> {
        self.router.dispatch(plugin, &request.service, &request.method, request.params.clone())
    }
}

/// Editor plugin host.
pub struct EditorPluginHost {
    access: SharedEditorAccess,
    manager: Arc<Mutex<PluginManager>>,
    services: Arc<RwLock<ServiceRegistry>>,
    commands: Arc<EditorCommandRegistry>,
    contributions: Arc<EditorContributionRegistry>,
    ui: Arc<UiService>,
    tasks: Arc<TaskService>,
    decorations: Arc<DecorationService>,
    lsp: Arc<LspService>,
    fs: Arc<FsService>,
    config: Arc<ConfigService>,
    router: EditorServiceRouter,
    adapter: Arc<EditorHostAdapter>,
    notifier: Arc<dyn HostNotifier>,
    published_contributions: Mutex<BTreeSet<(ContributionId, &'static str)>>,
    started: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    reload_requested: Arc<AtomicBool>,
}

impl EditorPluginHost {
    /// Build a host with the standard editor service set.
    pub fn new(
        access: SharedEditorAccess,
        manager_config: PluginManagerConfig,
        builtins: BuiltinCatalog,
        fs_scope: FsScope,
        notifier: Arc<dyn HostNotifier>,
    ) -> Result<Self, PluginError> {
        let workspace = manager_config.workspace.clone();
        let host_config = manager_config.host_config.clone();

        let services = Arc::new(RwLock::new(ServiceRegistry::new()));
        let commands = Arc::new(EditorCommandRegistry::new(Arc::clone(&access)));
        let contributions = Arc::new(EditorContributionRegistry::new());
        let ui = Arc::new(UiService::new());
        let tasks = Arc::new(TaskService::new());
        let decorations = Arc::new(DecorationService::new(Arc::clone(&access)));
        let lsp = Arc::new(LspService::new());
        let fs = Arc::new(FsService::new(workspace, fs_scope)?);
        let config = Arc::new(ConfigService::new(host_config));

        let document = Arc::new(DocumentService::new(Arc::clone(&access)));
        let document_edit = Arc::new(DocumentEditService::new(Arc::clone(&access)));
        let views = Arc::new(ViewsService::new(Arc::clone(&access)));
        let selection = Arc::new(SelectionService::new(Arc::clone(&access)));
        let syntax = Arc::new(SyntaxService::new(Arc::clone(&access)));

        {
            let mut registry = services.write().expect("service lock poisoned");
            register_service(&mut registry, Arc::clone(&document))?;
            register_service(&mut registry, Arc::clone(&document_edit))?;
            register_service(&mut registry, Arc::clone(&views))?;
            register_service(&mut registry, Arc::clone(&selection))?;
            register_service(&mut registry, Arc::clone(&syntax))?;
            register_service(&mut registry, Arc::clone(&commands))?;
            register_service(&mut registry, Arc::clone(&lsp))?;
            register_service(&mut registry, Arc::clone(&decorations))?;
            register_service(&mut registry, Arc::clone(&ui))?;
            register_service(&mut registry, Arc::clone(&tasks))?;
            register_service(&mut registry, Arc::clone(&fs))?;
            register_service(&mut registry, Arc::clone(&config))?;
        }

        let router = EditorServiceRouter::new(
            document,
            document_edit,
            views,
            selection,
            Arc::clone(&commands),
            syntax,
            Arc::clone(&lsp),
            Arc::clone(&decorations),
            Arc::clone(&ui),
            Arc::clone(&tasks),
            Arc::clone(&fs),
            Arc::clone(&config),
        );
        let adapter = Arc::new(EditorHostAdapter::new(router.clone()));

        let manager = PluginManager::new(adapter.clone(), manager_config)
            .with_builtins(builtins)
            .with_services(Arc::clone(&services))
            .with_command_registry(commands.clone())
            .with_contribution_sink(contributions.clone());

        Ok(Self {
            access,
            manager: Arc::new(Mutex::new(manager)),
            services,
            commands,
            contributions,
            ui,
            tasks,
            decorations,
            lsp,
            fs,
            config,
            router,
            adapter,
            notifier,
            published_contributions: Mutex::new(BTreeSet::new()),
            started: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(AtomicBool::new(false)),
            reload_requested: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn access(&self) -> &SharedEditorAccess {
        &self.access
    }

    pub fn commands(&self) -> &Arc<EditorCommandRegistry> {
        &self.commands
    }

    pub fn contributions(&self) -> &Arc<EditorContributionRegistry> {
        &self.contributions
    }

    pub fn ui(&self) -> &Arc<UiService> {
        &self.ui
    }

    pub fn tasks(&self) -> &Arc<TaskService> {
        &self.tasks
    }

    pub fn decorations(&self) -> &Arc<DecorationService> {
        &self.decorations
    }

    pub fn lsp(&self) -> &Arc<LspService> {
        &self.lsp
    }

    pub fn fs(&self) -> &Arc<FsService> {
        &self.fs
    }

    pub fn config(&self) -> &Arc<ConfigService> {
        &self.config
    }

    pub fn services(&self) -> Arc<RwLock<ServiceRegistry>> {
        Arc::clone(&self.services)
    }

    pub fn events(&self) -> ee_plugin_api::event::EventBus {
        self.manager.lock().expect("manager lock poisoned").events()
    }

    pub fn router(&self) -> &EditorServiceRouter {
        &self.router
    }

    /// Discover and activate startup plugins.
    pub fn start(&self) -> Result<(), PluginError> {
        if self.started.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        if let Err(error) = self.start_inner() {
            self.started.store(false, Ordering::SeqCst);
            return Err(error);
        }
        Ok(())
    }

    fn start_inner(&self) -> Result<(), PluginError> {
        {
            let mut manager = self.manager.lock().expect("manager lock poisoned");
            manager.start()?;
        }
        self.sync_permissions();
        self.publish_status();
        self.publish_contributions();
        Ok(())
    }

    /// Whether startup activation has completed.
    pub fn is_started(&self) -> bool {
        self.started.load(Ordering::SeqCst)
    }

    /// Whether the host has been shut down and must not restart.
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Ask the core plugin cycle to re-discover plugin roots.
    pub fn request_reload(&self) {
        self.reload_requested.store(true, Ordering::SeqCst);
    }

    /// Consume a pending reload request.
    pub fn take_reload_request(&self) -> bool {
        self.reload_requested.swap(false, Ordering::SeqCst)
    }

    /// Stop all plugins in reverse activation order.
    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.started.store(false, Ordering::SeqCst);
        let events = self.manager.lock().expect("manager lock poisoned").events();
        let _ = events.emit(&crate::plugin_runtime::events::Shutdown);
        let mut manager = self.manager.lock().expect("manager lock poisoned");
        manager.stop();
        drop(manager);
        self.publish_status();
        self.publish_contributions();
    }

    /// Re-discover plugin roots. Existing instances are stopped first.
    pub fn reload_paths(&self, _paths: &[PathBuf]) -> Result<u64, PluginError> {
        {
            let mut manager = self.manager.lock().expect("manager lock poisoned");
            manager.stop();
            manager.start()?;
        }
        self.sync_permissions();
        self.publish_status();
        self.publish_contributions();
        Ok(self.manager.lock().expect("manager lock poisoned").generation())
    }

    /// Activate deferred plugins matching an event name.
    pub fn activate_for_event(&self, event: &str) -> Vec<(String, PluginError)> {
        self.activate_with(|manager| manager.activate_for_event(event))
    }

    /// Activate deferred plugins matching a language id.
    pub fn activate_for_language(&self, language: &str) -> Vec<(String, PluginError)> {
        self.activate_with(|manager| manager.activate_for_language(language))
    }

    fn activate_with(
        &self,
        activate: impl FnOnce(
            &mut ee_plugin_runtime::manager::PluginManager,
        ) -> Vec<(String, PluginError)>,
    ) -> Vec<(String, PluginError)> {
        let failures = {
            let mut manager = self.manager.lock().expect("manager lock poisoned");
            activate(&mut manager)
        };
        self.sync_permissions();
        if !failures.is_empty() {
            self.publish_status();
        }
        self.publish_contributions();
        failures
    }

    pub fn is_active(&self, name: &str) -> bool {
        self.manager.lock().expect("manager lock poisoned").is_active(name)
    }

    pub fn generation(&self) -> u64 {
        self.manager.lock().expect("manager lock poisoned").generation()
    }

    pub fn status(&self) -> ManagerStatus {
        self.manager.lock().expect("manager lock poisoned").status()
    }

    /// Alias used by status/diagnostics callers.
    pub fn status_snapshot(&self) -> ManagerStatus {
        self.status()
    }

    pub fn diagnostics(&self) -> Vec<ee_plugin_runtime::diagnostics::Diagnostic> {
        self.manager.lock().expect("manager lock poisoned").diagnostics().to_vec()
    }

    /// Emit a typed event to in-process and native plugins.
    pub fn emit<E: Event + Serialize>(&self, event: &E) -> EmissionReport {
        let mut report = EmissionReport::default();
        let activation_failures = self.activate_for_event(E::NAME);
        report.failures.extend(activation_failures);

        let (events, payload, revision) = {
            let manager = self.manager.lock().expect("manager lock poisoned");
            let payload = match serde_json::to_value(event) {
                Ok(payload) => payload,
                Err(error) => {
                    report.failures.push((
                        "host".to_owned(),
                        PluginError::Schema { message: error.to_string() },
                    ));
                    return report;
                }
            };
            (manager.events(), payload, manager.generation())
        };

        if let Err(error) = events.emit(event) {
            report.failures.push(("host".to_owned(), PluginError::from(error)));
        }

        {
            let mut manager = self.manager.lock().expect("manager lock poisoned");
            manager.emit_event(E::NAME, revision, payload);
        }
        report
    }

    /// Drain effects queued by native or in-process plugin work.
    pub fn take_effects(&self) -> Vec<QueuedEffect> {
        self.adapter.take_effects()
    }

    /// Poll native transports for side messages.
    pub fn pump(&self) -> Vec<(String, PluginError)> {
        let failures = {
            let mut manager = self.manager.lock().expect("manager lock poisoned");
            manager.pump()
        };
        if !failures.is_empty() {
            self.sync_permissions();
            self.publish_status();
        }
        failures
    }

    /// Dispatch a command through the registry.
    pub fn dispatch_command(
        &self,
        id: &ee_plugin_api::ids::CommandId,
        view_id: ee_plugin_api::model::ViewId,
        args: Value,
    ) -> Result<Vec<Effect>, PluginError> {
        for (plugin, error) in
            self.activate_with(|manager| manager.activate_for_command(id.as_str()))
        {
            tracing::warn!("failed to activate plugin `{plugin}` for command `{id}`: {error}");
        }
        let events = self.manager.lock().expect("manager lock poisoned").events();
        let before = crate::plugin_runtime::events::CommandBefore {
            id: id.clone(),
            view_id,
            args: args.clone(),
        };
        if let Err(error) = events.waterfall(&before) {
            return Err(PluginError::from(error));
        }
        let effects = if self.commands.contains(id) {
            let effects = self.commands.dispatch(id, view_id, args)?;
            self.adapter.submit_effects(None, effects.clone())?;
            effects
        } else {
            let mut manager = self.manager.lock().expect("manager lock poisoned");
            match manager.invoke_command(id, view_id, args)? {
                Some(_result) => Vec::new(),
                None => {
                    return Err(PluginError::Runtime {
                        message: format!("command `{id}` is not registered"),
                    });
                }
            }
        };
        let after = crate::plugin_runtime::events::CommandAfter {
            id: id.clone(),
            view_id,
            effects: effects.len(),
        };
        let _ = events.emit(&after);
        Ok(effects)
    }

    /// Language definitions contributed by active plugins.
    pub fn language_contributions(&self) -> Vec<LanguageContribution> {
        self.contributions
            .by_kind("language")
            .into_iter()
            .filter_map(|contribution| match contribution {
                Contribution::Language(language) => Some(language),
                _ => None,
            })
            .collect()
    }

    /// LSP server defaults contributed by active plugins.
    pub fn lsp_server_contributions(&self) -> Vec<LspServerContribution> {
        self.contributions
            .by_kind("lsp_server")
            .into_iter()
            .filter_map(|contribution| match contribution {
                Contribution::LspServer(server) => Some(server),
                _ => None,
            })
            .collect()
    }

    /// Rebuild the surface default view for a buffer, used by tests and hosts.
    pub fn document_snapshot(
        &self,
        buffer: ee_plugin_api::model::BufferId,
    ) -> Result<DocumentSnapshot, PluginError> {
        self.access.document_snapshot(buffer)
    }

    /// Register a provider for a contribution id.
    pub fn register_ui_provider(
        &self,
        id: ee_plugin_api::ids::ContributionId,
        provider: Arc<dyn crate::plugin_runtime::services::ui::UiProvider>,
    ) -> Disposer {
        self.ui.register_provider(id, provider)
    }

    /// Register an LSP provider.
    pub fn register_lsp_provider(
        &self,
        provider: Arc<dyn crate::plugin_runtime::services::lsp::LspProvider>,
    ) -> Disposer {
        self.lsp.register_provider(provider)
    }

    fn sync_permissions(&self) {
        let manager = self.manager.lock().expect("manager lock poisoned");
        for (name, manifest) in manager.manifests() {
            let Ok(plugin) = PluginId::new(name.clone()) else {
                continue;
            };
            let permissions = manifest
                .permissions
                .0
                .iter()
                .filter(|(_, granted)| **granted)
                .map(|(permission, _)| permission.clone())
                .collect::<BTreeSet<_>>();
            self.router.set_permissions(&plugin, permissions);
        }
    }

    /// Publish the contribution diff since the last publish.
    ///
    /// The frontend requires a strictly increasing revision, so unchanged
    /// registries are not re-sent. Removals travel as ids because the registry
    /// is the only authoritative source of current contributions.
    fn publish_contributions(&self) {
        let upsert = self.contributions.list();
        let current = upsert.iter().map(contribution_key).collect::<BTreeSet<_>>();
        let (removed, changed) = {
            let mut published =
                self.published_contributions.lock().expect("published lock poisoned");
            let removed =
                published.difference(&current).map(|(id, _)| id.clone()).collect::<BTreeSet<_>>();
            let changed = *published != current;
            *published = current;
            (removed.into_iter().collect::<Vec<_>>(), changed)
        };
        if !changed {
            return;
        }
        let revision = self.contributions.revision();
        self.notifier.plugin_contributions(revision, upsert, removed);
    }

    fn publish_status(&self) {
        let status = self.status();
        if status.plugins.is_empty() {
            return;
        }
        for plugin in &status.plugins {
            self.notifier.plugin_health(
                &plugin.name,
                plugin.health.state.as_str(),
                plugin.health.reason.as_ref().map(|reason| reason.as_str()),
            );
        }
        self.notifier.plugin_status(&status);
    }
}

fn register_service<T: ee_plugin_api::service::Service>(
    registry: &mut ServiceRegistry,
    service: Arc<T>,
) -> Result<(), PluginError> {
    registry.register(service).map_err(PluginError::from)
}

/// Service ids used by manifests and tests.
pub fn editor_service_ids() -> Vec<ServiceId> {
    [
        "editor.document@1",
        "editor.document.edit@1",
        "editor.views@1",
        "editor.selection@1",
        "editor.commands@1",
        "editor.syntax@1",
        "editor.lsp@1",
        "editor.decorations@1",
        "editor.ui@1",
        "editor.tasks@1",
        "editor.fs@1",
        "editor.config@1",
    ]
    .into_iter()
    .filter_map(|id| id.parse().ok())
    .collect()
}
