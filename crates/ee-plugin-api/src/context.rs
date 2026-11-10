//! Plugin context and the `Plugin` entry point.
//!
//! A context is the only capability a plugin receives. Registration methods
//! return disposers that are also tracked by the owning scope, so dropping a
//! scope tears down every listener, command, contribution, and effect it
//! created.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde_json::Value;

use crate::contribution::Contribution;
use crate::disposer::{Disposer, DisposerStack};
use crate::effect::{CommandHandler, CommandRequest, CommandSpec, Effect};
use crate::error::PluginError;
use crate::event::{Event, EventBus};
use crate::ids::{PluginId, ScopeId, ServiceId};
use crate::service::{Service, ServiceKey, ServiceRegistry};
use crate::wire::EventParams;

/// Registry boundary implemented by the editor and agent hosts.
pub trait CommandRegistry: Send + Sync + 'static {
    /// Register a command and return a disposer that unregisters it.
    fn register_command(
        &self,
        spec: CommandSpec,
        handler: Arc<dyn CommandHandler>,
    ) -> Result<Disposer, PluginError>;
}

/// Contribution boundary implemented by the editor and agent hosts.
pub trait ContributionSink: Send + Sync + 'static {
    /// Publish a contribution and return a disposer that removes it.
    fn contribute(&self, contribution: Contribution) -> Result<Disposer, PluginError>;
}

/// Service and effect boundary used by native plugins.
///
/// The SDK implements this over the v2 wire protocol. In-process hosts may
/// implement it directly; plugins never depend on a concrete host crate.
pub trait ServiceCaller: Send + Sync + 'static {
    /// Call one versioned service method.
    fn call_service(
        &self,
        service: ServiceId,
        method: &str,
        params: Value,
    ) -> Result<Value, PluginError>;

    /// Submit one effect batch to the host.
    fn submit_effects(&self, effects: Vec<Effect>) -> Result<(), PluginError>;
}

struct ContextInner {
    plugin_id: PluginId,
    scope: ScopeId,
    services: Arc<RwLock<ServiceRegistry>>,
    events: EventBus,
    commands: Option<Arc<dyn CommandRegistry>>,
    contributions: Option<Arc<dyn ContributionSink>>,
    caller: Option<Arc<dyn ServiceCaller>>,
    config: RwLock<Value>,
    disposers: Mutex<DisposerStack>,
    children: Mutex<Vec<PluginContext>>,
    disposed: AtomicBool,
}

impl ContextInner {
    /// Dispose children (reverse creation order) then this scope's disposers.
    /// Idempotent, and safe while clones of the scope are still alive.
    fn dispose(&self) {
        if self.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Ok(mut children) = self.children.lock() {
            while let Some(child) = children.pop() {
                child.dispose();
            }
        }
        if let Ok(mut disposers) = self.disposers.lock() {
            disposers.dispose_all();
        }
    }
}

impl Drop for ContextInner {
    fn drop(&mut self) {
        if *self.disposed.get_mut() {
            return;
        }
        if let Ok(children) = self.children.get_mut() {
            // Children dispose in reverse creation order, before parent cleanup.
            while let Some(child) = children.pop() {
                drop(child);
            }
        }
        if let Ok(disposers) = self.disposers.get_mut() {
            disposers.dispose_all();
        }
    }
}

/// Handle to a plugin scope.
#[derive(Clone)]
pub struct PluginContext {
    inner: Arc<ContextInner>,
}

impl PluginContext {
    /// Create a context with an empty service registry and no host sinks.
    ///
    /// This is the constructor used by SDK tests and builtin plugins. Hosts add
    /// their registries through the builder methods.
    pub fn new(plugin_id: impl Into<String>) -> Self {
        let plugin_id = PluginId::new(plugin_id).expect("plugin id must be a valid identifier");
        let scope = ScopeId::new("host").expect("static scope id");
        Self {
            inner: Arc::new(ContextInner {
                plugin_id,
                scope,
                services: Arc::new(RwLock::new(ServiceRegistry::new())),
                events: EventBus::new(),
                commands: None,
                contributions: None,
                caller: None,
                config: RwLock::new(Value::Null),
                disposers: Mutex::new(DisposerStack::new()),
                children: Mutex::new(Vec::new()),
                disposed: AtomicBool::new(false),
            }),
        }
    }

    /// Override the scope identifier.
    pub fn with_scope(mut self, scope: ScopeId) -> Self {
        Arc::get_mut(&mut self.inner).expect("context must be unique before configuration").scope =
            scope;
        self
    }

    /// Use a shared service registry.
    pub fn with_services(mut self, services: Arc<RwLock<ServiceRegistry>>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("context must be unique before configuration")
            .services = services;
        self
    }

    /// Use a shared event bus.
    pub fn with_events(mut self, events: EventBus) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("context must be unique before configuration")
            .events = events;
        self
    }

    /// Install the host command registry.
    pub fn with_command_registry(mut self, registry: Arc<dyn CommandRegistry>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("context must be unique before configuration")
            .commands = Some(registry);
        self
    }

    /// Install the host contribution sink.
    pub fn with_contribution_sink(mut self, sink: Arc<dyn ContributionSink>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("context must be unique before configuration")
            .contributions = Some(sink);
        self
    }

    /// Install the native service/effect caller.
    pub fn with_service_caller(mut self, caller: Arc<dyn ServiceCaller>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("context must be unique before configuration")
            .caller = Some(caller);
        self
    }

    /// Caller installed by the SDK runtime for external plugins.
    pub fn service_caller(&self) -> Option<Arc<dyn ServiceCaller>> {
        self.inner.caller.clone()
    }

    /// Call one service method through the host caller.
    pub fn call_service(
        &self,
        service: ServiceId,
        method: &str,
        params: Value,
    ) -> Result<Value, PluginError> {
        self.inner
            .caller
            .as_ref()
            .ok_or(PluginError::Unsupported { feature: "service calls" })?
            .call_service(service, method, params)
    }

    /// Submit effects through the host caller.
    pub fn submit_effects(&self, effects: Vec<Effect>) -> Result<(), PluginError> {
        if effects.is_empty() {
            return Ok(());
        }
        self.inner
            .caller
            .as_ref()
            .ok_or(PluginError::Unsupported { feature: "effect submission" })?
            .submit_effects(effects)
    }

    /// Set the initial configuration value.
    pub fn configured_with(self, config: Value) -> Self {
        *self.inner.config.write().expect("config lock poisoned") = config;
        self
    }

    /// Plugin package identifier.
    pub fn plugin_id(&self) -> &PluginId {
        &self.inner.plugin_id
    }

    /// Scope identifier.
    pub fn scope(&self) -> &ScopeId {
        &self.inner.scope
    }

    /// Current configuration value.
    pub fn config(&self) -> Value {
        self.inner.config.read().expect("config lock poisoned").clone()
    }

    /// Resolve a service by type.
    pub fn service<T: Service>(&self) -> Option<Arc<T>> {
        self.inner
            .services
            .read()
            .expect("service registry lock poisoned")
            .get(&ServiceKey::<T>::new())
    }

    /// Resolve a required service.
    pub fn require<T: Service>(&self) -> Result<Arc<T>, PluginError> {
        self.inner
            .services
            .read()
            .expect("service registry lock poisoned")
            .require(&ServiceKey::<T>::new())
            .map_err(PluginError::from)
    }

    /// Register a service under `T::ID` and remove it when the returned
    /// disposer runs or this scope is dropped.
    pub fn register_service<T: Service>(&self, service: Arc<T>) -> Result<Disposer, PluginError> {
        self.register_service_with_id(ServiceId::from_static(T::ID), service)
    }

    /// Register a service under an explicit identifier.
    ///
    /// Used by data-driven builtins whose runtime identity is not a Rust type
    /// constant. Lookups still check the concrete type id.
    pub fn register_service_with_id<T: 'static + Send + Sync>(
        &self,
        id: ServiceId,
        service: Arc<T>,
    ) -> Result<Disposer, PluginError> {
        let shared = Arc::clone(&self.inner.services);
        let erased: Arc<dyn std::any::Any + Send + Sync> = service.clone();
        shared
            .write()
            .expect("service registry lock poisoned")
            .register_with_id(id.clone(), service)?;

        let disposer = Disposer::new(move || {
            if let Ok(mut registry) = shared.write() {
                registry.remove_if_same(&id, &erased);
            }
        });
        self.track(disposer.clone());
        Ok(disposer)
    }

    /// Services a plugin declares as required.
    pub fn requires(&self) -> &[ServiceId] {
        &[]
    }

    /// Register an observe-only event listener.
    pub fn on<E: Event>(
        &self,
        listener: impl Fn(&E) + Send + Sync + 'static,
    ) -> Result<Disposer, PluginError> {
        let disposer = self.inner.events.on(listener)?;
        self.track(disposer.clone());
        Ok(disposer)
    }

    /// Dispose this scope and every child scope immediately.
    ///
    /// Idempotent. Hosts call this when a session or turn ends so registrations
    /// are revoked even while other clones of the scope are still alive.
    pub fn dispose(&self) {
        self.inner.dispose();
    }

    /// Register a cleanup tied to this scope.
    ///
    /// The installer runs immediately and must return the cleanup closure.
    pub fn effect<F, D>(&self, install: F) -> Result<(), PluginError>
    where
        F: FnOnce() -> Result<D, PluginError>,
        D: FnOnce() + Send + 'static,
    {
        let cleanup = install()?;
        self.track(Disposer::new(cleanup));
        Ok(())
    }

    /// Register a command through the host registry.
    pub fn command(
        &self,
        spec: CommandSpec,
        handler: Arc<dyn CommandHandler>,
    ) -> Result<Disposer, PluginError> {
        let registry = self
            .inner
            .commands
            .as_ref()
            .ok_or(PluginError::Unsupported { feature: "command registration" })?;
        let disposer = registry.register_command(spec, handler)?;
        self.track(disposer.clone());
        Ok(disposer)
    }

    /// Publish a declarative contribution.
    pub fn contribute(&self, contribution: Contribution) -> Result<Disposer, PluginError> {
        let sink = self
            .inner
            .contributions
            .as_ref()
            .ok_or(PluginError::Unsupported { feature: "contribution publishing" })?;
        let disposer = sink.contribute(contribution)?;
        self.track(disposer.clone());
        Ok(disposer)
    }

    /// Create a child scope that shares services, events, and host sinks.
    ///
    /// The child is disposed with the parent, in reverse creation order.
    pub fn child_scope(&self, scope: ScopeId) -> PluginContext {
        self.make_child(scope, self.config())
    }

    /// Create a child scope with an overridden configuration value.
    pub fn with_config(&self, config: Value) -> PluginContext {
        self.make_child(self.inner.scope.clone(), config)
    }

    fn make_child(&self, scope: ScopeId, config: Value) -> PluginContext {
        let child = PluginContext {
            inner: Arc::new(ContextInner {
                plugin_id: self.inner.plugin_id.clone(),
                scope,
                services: Arc::clone(&self.inner.services),
                events: self.inner.events.clone(),
                commands: self.inner.commands.clone(),
                contributions: self.inner.contributions.clone(),
                caller: self.inner.caller.clone(),
                config: RwLock::new(config),
                disposers: Mutex::new(DisposerStack::new()),
                children: Mutex::new(Vec::new()),
                disposed: AtomicBool::new(false),
            }),
        };
        self.inner.children.lock().expect("children lock poisoned").push(child.clone());
        if self.inner.disposed.load(Ordering::SeqCst) {
            child.dispose();
        }
        child
    }

    fn track(&self, disposer: Disposer) {
        if self.inner.disposed.load(Ordering::SeqCst) {
            disposer.dispose();
            return;
        }
        self.inner.disposers.lock().expect("disposer lock poisoned").push(disposer);
    }
}

impl std::fmt::Debug for PluginContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginContext")
            .field("plugin_id", &self.inner.plugin_id)
            .field("scope", &self.inner.scope)
            .finish()
    }
}

/// A plugin entry point.
///
/// `apply` runs once during activation, outside the core lock. It may register
/// listeners, commands, services, and contributions. It must not hold a core
/// borrow.
pub trait Plugin: Send + Sync + 'static {
    /// Stable plugin name.
    fn name(&self) -> &str;

    /// Services the plugin requires before activation.
    fn requires(&self) -> &[ServiceId] {
        &[]
    }

    /// Install the plugin into a scope.
    fn apply(&mut self, ctx: &PluginContext) -> Result<(), PluginError>;

    /// Handle one host event.
    fn on_event(
        &mut self,
        _ctx: &PluginContext,
        _event: &EventParams,
    ) -> Result<Vec<Effect>, PluginError> {
        Ok(Vec::new())
    }

    /// Handle one command invocation.
    fn on_command(
        &mut self,
        _ctx: &PluginContext,
        request: &CommandRequest,
    ) -> Result<Vec<Effect>, PluginError> {
        Err(PluginError::Runtime { message: format!("command `{}` is not handled", request.id) })
    }

    /// Produce a fresh contribution snapshot.
    fn on_contribution_request(
        &mut self,
        _ctx: &PluginContext,
        _revision: u64,
    ) -> Result<(), PluginError> {
        Ok(())
    }

    /// Periodic work while the host is idle.
    fn on_tick(&mut self, _ctx: &PluginContext) -> Result<Vec<Effect>, PluginError> {
        Ok(Vec::new())
    }

    /// Interval at which [`Plugin::on_tick`] runs. `None` disables ticks.
    fn tick_interval(&self) -> Option<Duration> {
        None
    }

    /// Last chance to dispose resources before shutdown.
    fn on_shutdown(&mut self, _ctx: &PluginContext) {}
}

#[cfg(test)]
mod tests {
    #![allow(unused_must_use)]

    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::contribution::StatusAlignment;
    use crate::effect::{CommandRequest, Effect, StatusItem};
    use crate::ids::CommandId;

    #[derive(Debug)]
    struct CounterService;
    impl Service for CounterService {
        const ID: &'static str = "test.counter@1";
    }

    #[derive(Debug)]
    struct DemoEvent;
    impl Event for DemoEvent {
        const NAME: &'static str = "test.demo";
    }

    struct NoopHandler;
    impl CommandHandler for NoopHandler {
        fn invoke(&self, _request: CommandRequest) -> Result<Vec<Effect>, PluginError> {
            Ok(vec![Effect::Noop])
        }
    }

    #[derive(Default)]
    struct RecordingRegistry {
        ids: StdMutex<Vec<CommandId>>,
    }

    impl CommandRegistry for RecordingRegistry {
        fn register_command(
            &self,
            spec: CommandSpec,
            _handler: Arc<dyn CommandHandler>,
        ) -> Result<Disposer, PluginError> {
            let id = spec.id.clone();
            self.ids.lock().unwrap().push(id.clone());
            Ok(Disposer::new(move || {
                let _ = id;
            }))
        }
    }

    struct RecordingSink {
        count: AtomicUsize,
    }

    impl ContributionSink for RecordingSink {
        fn contribute(&self, contribution: Contribution) -> Result<Disposer, PluginError> {
            contribution.validate()?;
            self.count.fetch_add(1, Ordering::SeqCst);
            Ok(Disposer::new(|| {}))
        }
    }

    #[test]
    fn service_registration_is_scoped() {
        let services = Arc::new(RwLock::new(ServiceRegistry::new()));
        let count = services.read().unwrap().ids().len();
        assert_eq!(count, 0);

        {
            let ctx = PluginContext::new("demo").with_services(Arc::clone(&services));
            let disposer = ctx.register_service(Arc::new(CounterService)).unwrap();
            assert!(ctx.service::<CounterService>().is_some());
            assert!(services.read().unwrap().contains(&ServiceId::from_static("test.counter@1")));
            disposer.dispose();
            assert!(!services.read().unwrap().contains(&ServiceId::from_static("test.counter@1")));
        }
    }

    #[test]
    fn stale_scope_does_not_remove_replacement_service() {
        let services = Arc::new(RwLock::new(ServiceRegistry::new()));
        let id = ServiceId::from_static("test.counter@1");

        let old = PluginContext::new("demo").with_services(Arc::clone(&services));
        let _ = old.register_service(Arc::new(CounterService)).unwrap();
        services.write().unwrap().remove(&id);

        let new = PluginContext::new("demo").with_services(Arc::clone(&services));
        let _ = new.register_service(Arc::new(CounterService)).unwrap();

        drop(old);
        assert!(new.service::<CounterService>().is_some());
        assert!(services.read().unwrap().contains(&id));
    }

    #[test]
    fn duplicate_service_registration_is_rejected() {
        let ctx = PluginContext::new("demo");
        ctx.register_service(Arc::new(CounterService)).unwrap();
        assert!(ctx.register_service(Arc::new(CounterService)).is_err());
    }

    #[test]
    fn child_scope_inherits_services_and_config() {
        let services = Arc::new(RwLock::new(ServiceRegistry::new()));
        services.write().unwrap().register(Arc::new(CounterService)).unwrap();

        let parent = PluginContext::new("demo")
            .with_services(Arc::clone(&services))
            .configured_with(serde_json::json!({"parent": true}));
        let child = parent.child_scope(ScopeId::new("buffer").unwrap());

        assert!(child.service::<CounterService>().is_some());
        assert_eq!(child.config(), serde_json::json!({"parent": true}));
        assert_eq!(child.scope().as_str(), "buffer");
    }

    #[test]
    fn with_config_creates_config_override_child() {
        let parent = PluginContext::new("demo").configured_with(serde_json::json!({"a": 1}));
        let child = parent.with_config(serde_json::json!({"a": 2}));
        assert_eq!(parent.config(), serde_json::json!({"a": 1}));
        assert_eq!(child.config(), serde_json::json!({"a": 2}));
    }

    #[test]
    fn scope_drop_disposes_children_lifo() {
        let order = Arc::new(StdMutex::new(Vec::new()));
        {
            let parent = PluginContext::new("demo");
            let first = parent.child_scope(ScopeId::new("first").unwrap());
            let order_clone = Arc::clone(&order);
            first.effect(move || Ok(move || order_clone.lock().unwrap().push(1))).unwrap();

            let second = parent.child_scope(ScopeId::new("second").unwrap());
            let order_clone = Arc::clone(&order);
            second.effect(move || Ok(move || order_clone.lock().unwrap().push(2))).unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec![2, 1]);
    }

    #[test]
    fn listener_is_disposed_with_scope() {
        let bus = EventBus::new();
        let count = Arc::new(AtomicUsize::new(0));
        {
            let ctx = PluginContext::new("demo").with_events(bus.clone());
            let count_clone = Arc::clone(&count);
            ctx.on(move |_: &DemoEvent| {
                count_clone.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
            bus.emit(&DemoEvent).unwrap();
        }
        // Scope dropped, listener disposed, disposal is idempotent.
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(bus.listener_count::<DemoEvent>(), 0);
    }

    #[test]
    fn command_registration_requires_host_registry() {
        let ctx = PluginContext::new("demo");
        let spec = CommandSpec {
            id: CommandId::new("demo.run").unwrap(),
            title: "Run".to_owned(),
            description: String::new(),
            category: None,
            mutates_document: false,
            args_schema: None,
        };
        assert!(matches!(
            ctx.command(spec, Arc::new(NoopHandler)),
            Err(PluginError::Unsupported { .. })
        ));
    }

    #[test]
    fn command_and_contribution_are_delegated() {
        let registry = Arc::new(RecordingRegistry::default());
        let sink = Arc::new(RecordingSink { count: AtomicUsize::new(0) });
        let ctx = PluginContext::new("demo")
            .with_command_registry(registry.clone())
            .with_contribution_sink(sink.clone());

        let spec = CommandSpec {
            id: CommandId::new("demo.run").unwrap(),
            title: "Run".to_owned(),
            description: String::new(),
            category: None,
            mutates_document: false,
            args_schema: None,
        };
        ctx.command(spec, Arc::new(NoopHandler)).unwrap();
        assert_eq!(registry.ids.lock().unwrap().len(), 1);

        ctx.contribute(Contribution::StatusItem(crate::contribution::StatusItemContribution {
            key: "demo.status".to_owned(),
            text: "ok".to_owned(),
            alignment: StatusAlignment::Left,
            priority: 0,
        }))
        .unwrap();
        assert_eq!(sink.count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn effect_installer_runs_immediately() {
        let ran = Arc::new(AtomicUsize::new(0));
        let ran_clone = Arc::clone(&ran);
        let ctx = PluginContext::new("demo");
        ctx.effect(move || {
            ran_clone.fetch_add(1, Ordering::SeqCst);
            Ok(|| {})
        })
        .unwrap();
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn dispose_revokes_registrations_while_clones_are_alive() {
        let services = Arc::new(RwLock::new(ServiceRegistry::new()));
        let root = PluginContext::new("demo").with_services(Arc::clone(&services));
        let scope = root.child_scope(ScopeId::new("agent.session.1").unwrap());
        let clone = scope.clone();
        scope.register_service(Arc::new(CounterService)).unwrap();
        assert!(root.service::<CounterService>().is_some());

        clone.dispose();
        assert!(root.service::<CounterService>().is_none());

        // Idempotent, and a disposed scope rejects new registrations instead of
        // leaking them into the shared registry.
        clone.dispose();
        clone.register_service(Arc::new(CounterService)).unwrap();
        assert!(root.service::<CounterService>().is_none());
    }

    #[test]
    fn status_item_is_constructible() {
        let item = StatusItem {
            key: crate::effect::StatusItemKey::new("demo.status"),
            text: "ok".to_owned(),
            alignment: StatusAlignment::Right,
            priority: 0,
        };
        assert_eq!(item.text, "ok");
    }
}
