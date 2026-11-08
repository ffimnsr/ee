//! Manager lifecycle integration tests using in-process builtins and the
//! fixture native binary.

use std::fs;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ee_plugin_api::context::{Plugin, PluginContext};
use ee_plugin_api::contribution::{StatusAlignment, StatusItemContribution};
use ee_plugin_api::error::{ErrorClass, PluginError};
use ee_plugin_api::event::Event;
use ee_plugin_api::host::{HostAdapter, HostKind};
use ee_plugin_api::ids::ServiceId;
use ee_plugin_api::manifest::{Activation, HostTarget, ManifestV2, Requires};
use ee_plugin_api::service::Service;
use ee_plugin_runtime::{
    BuiltinCatalog, BuiltinDescriptor, DiscoveryOptions, MemoryContributionSink, PluginManager,
    PluginManagerConfig, RootKind, builtin_manifest,
};
use serde_json::json;
use tempfile::TempDir;

const FIXTURE: &str = env!("CARGO_BIN_EXE_ee-plugin-runtime-echo");

struct TestAdapter;

impl HostAdapter for TestAdapter {
    fn host_kind(&self) -> HostKind {
        HostKind::Editor
    }

    fn submit_effects(
        &self,
        _plugin: Option<&ee_plugin_api::ids::PluginId>,
        _effects: Vec<ee_plugin_api::effect::Effect>,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}

#[derive(Debug)]
struct SharedService;

impl Service for SharedService {
    const ID: &'static str = "test.shared@1";
}

#[derive(Debug)]
struct DemoEvent;

impl Event for DemoEvent {
    const NAME: &'static str = "test.demo";
}

type Log = Arc<Mutex<Vec<String>>>;

struct TestPlugin {
    name: String,
    requires: Vec<ServiceId>,
    provides: bool,
    log: Log,
    fail: Arc<AtomicBool>,
}

impl Plugin for TestPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn requires(&self) -> &[ServiceId] {
        &self.requires
    }

    fn apply(&mut self, ctx: &PluginContext) -> Result<(), PluginError> {
        self.log.lock().unwrap().push(format!("apply {}", self.name));
        if self.fail.load(Ordering::SeqCst) {
            return Err(PluginError::Runtime { message: "injected failure".to_owned() });
        }
        if self.provides {
            let _ = ctx.register_service(Arc::new(SharedService))?;
        }
        if !self.requires.is_empty() {
            let _ = ctx.require::<SharedService>()?;
        }
        let name = self.name.clone();
        let log = Arc::clone(&self.log);
        ctx.effect(move || {
            Ok(move || {
                log.lock().unwrap().push(format!("stop {name}"));
            })
        })
    }
}

fn descriptor(
    name: &str,
    requires: &[&str],
    provides: &[&str],
    log: &Log,
    fail: Arc<AtomicBool>,
) -> BuiltinDescriptor {
    let mut manifest = builtin_manifest(name, HostTarget::Editor, provides);
    manifest.requires = Requires {
        services: requires.iter().map(|value| (*value).to_owned()).collect(),
        optional_services: Vec::new(),
    };
    let plugin_name = name.to_owned();
    let requires = manifest
        .requires
        .services
        .iter()
        .filter_map(|raw| raw.parse::<ServiceId>().ok())
        .collect::<Vec<_>>();
    let provides_any = !provides.is_empty();
    let log = Arc::clone(log);
    BuiltinDescriptor::new(
        manifest,
        Arc::new(move || {
            Box::new(TestPlugin {
                name: plugin_name.clone(),
                requires: requires.clone(),
                provides: provides_any,
                log: Arc::clone(&log),
                fail: Arc::clone(&fail),
            })
        }),
    )
}

fn catalog(entries: Vec<BuiltinDescriptor>) -> BuiltinCatalog {
    let mut catalog = BuiltinCatalog::new();
    for entry in entries {
        catalog.insert(entry).unwrap();
    }
    catalog
}

fn manager(
    temp: &TempDir,
    catalog: BuiltinCatalog,
    sink: Arc<MemoryContributionSink>,
) -> PluginManager {
    let config = PluginManagerConfig::new(
        HostKind::Editor,
        temp.path().join("config"),
        temp.path().join("workspace"),
    );
    PluginManager::new(Arc::new(TestAdapter), config)
        .with_builtins(catalog)
        .with_contribution_sink(sink)
}

fn add_status(manifest: &mut ManifestV2, key: &str) {
    manifest.contributes.ui.status_items.push(StatusItemContribution {
        key: key.to_owned(),
        text: "status".to_owned(),
        alignment: StatusAlignment::Left,
        priority: 0,
    });
}

#[test]
fn dependency_chain_activates_and_disposes_in_reverse() {
    let temp = TempDir::new().unwrap();
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let fail = Arc::new(AtomicBool::new(false));
    let catalog = catalog(vec![
        descriptor("a", &[], &["test.shared@1"], &log, Arc::clone(&fail)),
        descriptor("b", &["test.shared@1"], &[], &log, Arc::clone(&fail)),
    ]);
    let mut manager = manager(&temp, catalog, Arc::new(MemoryContributionSink::new()));

    manager.start().unwrap();
    assert!(manager.is_active("a"));
    assert!(manager.is_active("b"));
    assert_eq!(*log.lock().unwrap(), vec!["apply a", "apply b"]);

    manager.stop();
    assert_eq!(*log.lock().unwrap(), vec!["apply a", "apply b", "stop b", "stop a"]);
    assert!(!manager.is_active("a"));
}

#[test]
fn failed_plugin_does_not_abort_unrelated_plugins() {
    let temp = TempDir::new().unwrap();
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let fail_b = Arc::new(AtomicBool::new(true));
    let ok = Arc::new(AtomicBool::new(false));
    let catalog = catalog(vec![
        descriptor("a", &[], &[], &log, Arc::clone(&ok)),
        descriptor("b", &[], &[], &log, Arc::clone(&fail_b)),
        descriptor("c", &[], &[], &log, Arc::clone(&ok)),
    ]);
    let mut manager = manager(&temp, catalog, Arc::new(MemoryContributionSink::new()));

    manager.start().unwrap();
    assert!(manager.is_active("a"));
    assert!(!manager.is_active("b"));
    assert!(manager.is_active("c"));

    let status = manager.status();
    let b = status.plugins.iter().find(|plugin| plugin.name == "b").unwrap();
    assert!(!b.active);
    assert!(manager.diagnostics().iter().any(|diagnostic| {
        diagnostic.plugin.as_deref() == Some("b") && diagnostic.message.contains("injected failure")
    }));
}

#[test]
fn contribution_conflict_blocks_only_the_conflicting_plugin() {
    let temp = TempDir::new().unwrap();
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let fail = Arc::new(AtomicBool::new(false));
    let mut conflict_entries = Vec::new();
    for name in ["a", "b"] {
        let mut entry = descriptor(name, &[], &[], &log, Arc::clone(&fail));
        add_status(&mut entry.manifest, "shared.status");
        conflict_entries.push(entry);
    }
    let mut c = descriptor("c", &[], &[], &log, Arc::clone(&fail));
    add_status(&mut c.manifest, "c.status");
    conflict_entries.push(c);

    let sink = Arc::new(MemoryContributionSink::new());
    let mut manager = manager(&temp, catalog(conflict_entries), Arc::clone(&sink));

    manager.start().unwrap();
    assert!(manager.is_active("a"));
    assert!(!manager.is_active("b"));
    assert!(manager.is_active("c"));
    assert_eq!(sink.len(), 2);
    assert!(manager.diagnostics().iter().any(|diagnostic| {
        diagnostic.plugin.as_deref() == Some("b") && diagnostic.class == ErrorClass::Contribution
    }));
}

#[test]
fn reload_preserves_re_registered_services() {
    let temp = TempDir::new().unwrap();
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let fail = Arc::new(AtomicBool::new(false));
    let mut manager = manager(
        &temp,
        catalog(vec![descriptor("a", &[], &["test.shared@1"], &log, fail)]),
        Arc::new(MemoryContributionSink::new()),
    );
    manager.start().unwrap();
    let id: ServiceId = "test.shared@1".parse().unwrap();
    assert!(manager.services().read().unwrap().contains(&id));

    manager.reload("a").unwrap();
    assert!(manager.is_active("a"));
    assert!(manager.services().read().unwrap().contains(&id));
    manager.stop();
    assert!(!manager.services().read().unwrap().contains(&id));
}

#[test]
fn crash_restart_backoff_is_exposed() {
    let temp = TempDir::new().unwrap();
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let fail = Arc::new(AtomicBool::new(false));
    let mut manager = manager(
        &temp,
        catalog(vec![descriptor("a", &[], &[], &log, fail)]),
        Arc::new(MemoryContributionSink::new()),
    );
    manager.start().unwrap();
    assert_eq!(manager.record_crash("a", 0), Some(250));
    assert_eq!(manager.restart_delay_ms("a"), Some(250));
    assert_eq!(manager.record_crash("a", 0), Some(500));
    assert_eq!(manager.record_resource_breach("a"), Some(1_000));
    manager.stop();
}

#[test]
fn workspace_plugin_requires_trust() {
    let temp = TempDir::new().unwrap();
    let plugin_dir = temp.path().join("workspace/.ee/plugins/ws");
    fs::create_dir_all(&plugin_dir).unwrap();
    fs::write(
        plugin_dir.join("manifest.toml"),
        format!(
            r#"
manifest_version = 2
name = "ws.plugin"
version = "1.0.0"
api = "ee-plugin/2"
targets = ["editor"]
kind = "native"
entry = "{FIXTURE}"
activation = {{ on_startup = true }}
"#
        ),
    )
    .unwrap();

    let catalog = catalog(vec![]);
    let discovery = DiscoveryOptions::new()
        .with_root(RootKind::Workspace, temp.path().join("workspace/.ee/plugins"));

    let untrusted_config = PluginManagerConfig::new(
        HostKind::Editor,
        temp.path().join("config"),
        temp.path().join("workspace"),
    )
    .with_discovery(discovery.clone());
    let mut untrusted =
        PluginManager::new(Arc::new(TestAdapter), untrusted_config).with_builtins(catalog.clone());
    untrusted.start().unwrap();
    assert!(!untrusted.is_active("ws.plugin"));
    assert!(untrusted.diagnostics().iter().any(|diagnostic| {
        diagnostic.plugin.as_deref() == Some("ws.plugin")
            && diagnostic.class == ErrorClass::Permission
    }));

    let trusted_config = PluginManagerConfig::new(
        HostKind::Editor,
        temp.path().join("config"),
        temp.path().join("workspace"),
    )
    .with_discovery(discovery.trust_workspace(true));
    let mut trusted =
        PluginManager::new(Arc::new(TestAdapter), trusted_config).with_builtins(catalog);
    trusted.start().unwrap();
    assert!(trusted.is_active("ws.plugin"));
    let status = trusted.status();
    let plugin = status.plugins.iter().find(|plugin| plugin.name == "ws.plugin").unwrap();
    assert_eq!(plugin.kind, "native");
    trusted.stop();
}

#[test]
fn reload_failure_keeps_old_generation_live() {
    static FAIL: AtomicBool = AtomicBool::new(false);
    static EVENTS: AtomicUsize = AtomicUsize::new(0);

    struct Flaky {
        log: Log,
    }

    impl Plugin for Flaky {
        fn name(&self) -> &str {
            "flaky"
        }

        fn apply(&mut self, ctx: &PluginContext) -> Result<(), PluginError> {
            self.log.lock().unwrap().push("apply".to_owned());
            if FAIL.load(Ordering::SeqCst) {
                return Err(PluginError::Runtime { message: "reload failed".to_owned() });
            }
            let _ = ctx.on(|_: &DemoEvent| {
                EVENTS.fetch_add(1, Ordering::SeqCst);
            })?;
            Ok(())
        }
    }

    let temp = TempDir::new().unwrap();
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let log_for_factory = Arc::clone(&log);
    let mut catalog = BuiltinCatalog::new();
    catalog
        .insert(BuiltinDescriptor::new(
            builtin_manifest("flaky", HostTarget::Editor, &[]),
            Arc::new(move || Box::new(Flaky { log: Arc::clone(&log_for_factory) })),
        ))
        .unwrap();
    let mut manager = manager(&temp, catalog, Arc::new(MemoryContributionSink::new()));

    manager.start().unwrap();
    manager.events().emit(&DemoEvent).unwrap();
    assert_eq!(EVENTS.load(Ordering::SeqCst), 1);

    FAIL.store(true, Ordering::SeqCst);
    assert!(manager.reload("flaky").is_err());
    assert!(manager.is_active("flaky"));
    manager.events().emit(&DemoEvent).unwrap();
    assert_eq!(EVENTS.load(Ordering::SeqCst), 2);

    FAIL.store(false, Ordering::SeqCst);
    let generation = manager.generation();
    manager.reload("flaky").unwrap();
    assert_eq!(manager.generation(), generation + 1);
    manager.events().emit(&DemoEvent).unwrap();
    assert_eq!(EVENTS.load(Ordering::SeqCst), 3);
    assert_eq!(*log.lock().unwrap(), vec!["apply", "apply", "apply"]);
    manager.stop();
}

#[test]
fn disabled_plugin_is_blocked_and_deferred_activation_waits_for_language() {
    let temp = TempDir::new().unwrap();
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let fail = Arc::new(AtomicBool::new(false));

    let mut language_manifest = builtin_manifest("deferred", HostTarget::Editor, &[]);
    language_manifest.activation = Activation {
        on_startup: false,
        on_language: vec!["rust".to_owned()],
        ..Default::default()
    };
    let log_for_factory = Arc::clone(&log);
    let deferred = BuiltinDescriptor::new(
        language_manifest,
        Arc::new(move || {
            Box::new(TestPlugin {
                name: "deferred".to_owned(),
                requires: Vec::new(),
                provides: false,
                log: Arc::clone(&log_for_factory),
                fail: Arc::new(AtomicBool::new(false)),
            })
        }),
    );
    let disabled = descriptor("disabled", &[], &[], &log, fail);

    let mut config = PluginManagerConfig::new(
        HostKind::Editor,
        temp.path().join("config"),
        temp.path().join("workspace"),
    )
    .with_host_config(json!({"plugins": {"disabled": {"enabled": false}}}));
    config.discovery = DiscoveryOptions::new();
    let mut manager = PluginManager::new(Arc::new(TestAdapter), config)
        .with_builtins(catalog(vec![deferred, disabled]));

    manager.start().unwrap();
    assert!(!manager.is_active("deferred"));
    assert!(!manager.is_active("disabled"));

    let failures = manager.activate_for_language("rust");
    assert!(failures.is_empty());
    assert!(manager.is_active("deferred"));

    manager.activate_for_language("python");
    assert!(manager.is_active("deferred"));
}
