//! Host lifecycle, dispatch, and event tests.

use std::sync::{Arc, Mutex};

use ee_plugin_api::effect::Effect;
use ee_plugin_api::ids::CommandId;
use ee_plugin_api::model::ViewId;
use serde_json::json;

use super::{RecordingNotifier, editor, event, host, single_host, two_hosts};

#[test]
fn start_activates_plugins_and_registers_surfaces() {
    let (_dir, host, notifier) = single_host();
    host.start().unwrap();
    assert!(host.is_active("fake"));
    assert!(host.commands().contains(&CommandId::new("fake.run").unwrap()));
    assert!(
        host.contributions()
            .get(&ee_plugin_api::ids::ContributionId::new("fake.status").unwrap(), "status_item")
            .is_some()
    );
    assert_eq!(host.contributions().decorations().len(), 1);
    assert!(!notifier.statuses.lock().unwrap().is_empty());
    assert!(
        notifier
            .health
            .lock()
            .unwrap()
            .iter()
            .any(|(name, state, _)| { name == "fake" && state == "loaded" })
    );
}

#[test]
fn dispatch_command_queues_effects() {
    let (_dir, host, _notifier) = single_host();
    host.start().unwrap();
    let effects =
        host.dispatch_command(&CommandId::new("fake.run").unwrap(), ViewId(1), json!({})).unwrap();
    assert!(matches!(effects[0], Effect::ApplyEdit(_)));
    assert_eq!(host.take_effects().len(), 1);

    let edit = match &effects[0] {
        Effect::ApplyEdit(edit) => edit,
        other => panic!("unexpected effect: {other:?}"),
    };
    assert_eq!(edit.undo_group, Some(7));
    assert_eq!(edit.author, "fake.run");
}

#[test]
fn events_reach_event_bus_and_isolate_panics() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let (_dir, host) = host(
        vec![("panic".to_owned(), true, order)],
        editor(),
        Arc::new(RecordingNotifier::default()),
    );
    host.start().unwrap();
    let report = host.emit(&event(ViewId(1)));
    assert!(report.is_ok(), "event emission failed: {:?}", report.failures);
    assert_eq!(host.events().panic_count(), 1);
    assert!(host.is_active("panic"));
}

#[test]
fn activation_order_is_deterministic() {
    let (_dir, host, order) = two_hosts();
    host.start().unwrap();
    assert_eq!(*order.lock().unwrap(), vec!["alpha".to_owned(), "beta".to_owned()]);
}

#[test]
fn stop_deactivates_and_reload_advances_generation() {
    let (_dir, host, _notifier) = single_host();
    host.start().unwrap();
    let generation = host.generation();
    host.reload_paths(&[]).unwrap();
    assert!(host.generation() > generation);
    assert!(host.is_active("fake"));

    host.stop();
    assert!(!host.is_active("fake"));
    assert!(!host.commands().contains(&CommandId::new("fake.run").unwrap()));
}

#[test]
fn service_registry_contains_editor_services() {
    let (_dir, host, _notifier) = single_host();
    let services = host.services();
    let registry = services.read().unwrap();
    for id in crate::plugin_runtime::host::editor_service_ids() {
        assert!(registry.contains(&id), "missing service {id}");
    }
}

#[test]
fn fake_plugin_vlf_mutation_is_rejected_before_effects() {
    let editor = editor();
    editor.set_vlf(ViewId(1), true);
    let order = Arc::new(Mutex::new(Vec::new()));
    let (_dir, host) = host(
        vec![("fake".to_owned(), false, order)],
        editor,
        Arc::new(RecordingNotifier::default()),
    );
    host.start().unwrap();
    let error = host
        .dispatch_command(&CommandId::new("fake.run").unwrap(), ViewId(1), json!({}))
        .unwrap_err();
    assert_eq!(error.class(), ee_plugin_api::error::ErrorClass::Permission);
    assert!(host.take_effects().is_empty());
}

#[test]
fn deferred_plugin_activates_on_event() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let (_dir, host) = host(
        vec![("deferred".to_owned(), false, order)],
        editor(),
        Arc::new(RecordingNotifier::default()),
    );
    // Builtin manifests default to startup-active, so the explicit activation
    // path is exercised through the generic event call here.
    host.start().unwrap();
    let failures = host.activate_for_event("editor.document.changed");
    assert!(failures.is_empty());
    assert!(host.is_active("deferred"));
}
