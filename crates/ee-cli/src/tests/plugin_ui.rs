//! Frontend contribution registry, protocol, rendering, and interaction tests.

use std::sync::mpsc;
use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};

use crate::app::App;
use crate::buffer::BufferManager;
use crate::keymap::parse_action_spec;
use crate::plugin_ui::{
    ContributionBatch, PluginDiagnostic, PluginHealthEvent, PluginStatusSnapshot, PluginUiState,
    PluginUiUpdate, contributions::SUPPORTED_CONTRIBUTION_KINDS,
};
use crate::ui::ui;

fn batch(revision: u64, upsert: Value, remove: Value) -> ContributionBatch {
    ContributionBatch::parse(&json!({
        "host": "editor",
        "revision": revision,
        "upsert": upsert,
        "remove": remove,
    }))
    .expect("batch should parse")
}

fn demo_contributions() -> Value {
    json!([
        {
            "kind": "command",
            "id": "outline.toggle",
            "title": "Toggle Outline",
            "description": "Show or hide the document outline",
            "category": "outline",
            "args_schema": {"type": "object", "properties": {}}
        },
        {
            "kind": "ex_command",
            "name": "outline",
            "aliases": ["ol"],
            "command": "outline.toggle",
            "usage": "[mode]"
        },
        {
            "kind": "keybinding",
            "command": "outline.toggle",
            "key": "ctrl+shift+o",
            "when": "normal"
        },
        {
            "kind": "status_item",
            "key": "outline.status",
            "text": "outline: on",
            "alignment": "right",
            "priority": 20
        },
        {
            "kind": "panel",
            "id": "outline.panel",
            "title": "Outline",
            "placement": "right",
            "render": {"type": "list", "empty_text": "No symbols"}
        },
        {
            "kind": "overlay",
            "id": "outline.overlay",
            "anchor": "top_left",
            "render": {"type": "text"},
            "transient": true
        },
        {
            "kind": "picker",
            "id": "outline.picker",
            "title": "Outline Picker",
            "columns": [{"key": "name", "title": "Name"}],
            "filter_mode": "substring"
        },
        {
            "kind": "menu_item",
            "command": "outline.toggle",
            "label": "Outline",
            "group": "view",
            "order": 1
        }
    ])
}

#[test]
fn every_declared_frontend_kind_is_supported_and_registered() {
    assert_eq!(
        SUPPORTED_CONTRIBUTION_KINDS,
        &[
            "command",
            "ex_command",
            "keybinding",
            "status_item",
            "panel",
            "overlay",
            "picker",
            "menu_item"
        ]
    );
    let mut state = PluginUiState::new();
    state.apply_batch(&batch(1, demo_contributions(), json!([]))).unwrap();

    assert!(state.registry.has_command("outline.toggle"));
    assert_eq!(state.registry.resolve_ex_alias("ol").map(|id| id.as_str()), Some("outline.toggle"));
    assert_eq!(state.status_items().len(), 1);
    assert_eq!(state.registry.panels().count(), 1);
    assert_eq!(state.registry.overlays().count(), 1);
    assert_eq!(state.registry.pickers().count(), 1);
    assert_eq!(state.registry.menu_items().count(), 1);
}

#[test]
fn malformed_or_stale_batch_never_replaces_the_previous_registry() {
    let mut state = PluginUiState::new();
    state.apply_batch(&batch(4, demo_contributions(), json!([]))).unwrap();

    // Stale revision.
    let stale =
        batch(4, json!([{"kind": "command", "id": "late.run", "title": "Late"}]), json!([]));
    assert!(state.apply_batch(&stale).is_err());
    assert!(!state.registry.has_command("late.run"));

    // Malformed known kind rejects the whole batch.
    let malformed = ContributionBatch::parse(&json!({
        "revision": 5,
        "upsert": [{"kind": "status_item", "key": "bad.status"}]
    }));
    assert!(malformed.is_err());
    assert_eq!(state.registry.revision(), 4);
    assert!(state.registry.has_command("outline.toggle"));
}

#[test]
fn unsupported_kind_is_reported_and_skipped_atomically() {
    let mut state = PluginUiState::new();
    let batch = ContributionBatch::parse(&json!({
        "revision": 1,
        "upsert": [
            {"kind": "widget", "id": "outline.widget"},
            {"kind": "command", "id": "outline.toggle", "title": "Toggle"}
        ]
    }))
    .unwrap();
    assert_eq!(batch.diagnostics.len(), 1);
    state.apply_batch(&batch).unwrap();
    assert!(state.registry.has_command("outline.toggle"));
    assert!(matches!(
        state.diagnostics().first(),
        Some(PluginDiagnostic::UnsupportedKind { kind, .. }) if kind == "widget"
    ));
}

#[test]
fn duplicate_ids_and_invalid_text_are_rejected() {
    let duplicate = ContributionBatch::parse(&json!({
        "revision": 1,
        "upsert": [
            {"kind": "panel", "id": "demo.panel", "title": "One", "placement": "left", "render": {"type": "text"}},
            {"kind": "panel", "id": "demo.panel", "title": "Two", "placement": "right", "render": {"type": "text"}}
        ]
    }));
    assert!(duplicate.is_err());

    let ansi = ContributionBatch::parse(&json!({
        "revision": 1,
        "upsert": [{"kind": "status_item", "key": "demo.status", "text": "\u{1b}[31mred", "alignment": "left"}]
    }));
    assert!(ansi.is_err());
}

#[test]
fn protocol_senders_emit_command_and_ui_event_notifications() {
    let (record_tx, record_rx) = mpsc::channel::<String>();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let manager = BufferManager::test_new(record_tx, backend_rx, String::from("view-id-1"));

    manager.send_plugin_command("outline.toggle", json!({"mode": "tree"})).unwrap();
    let raw = record_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let command: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(command["method"], "command");
    assert_eq!(command["params"]["id"], "outline.toggle");
    assert_eq!(command["params"]["args"]["mode"], "tree");

    manager
        .send_plugin_ui_event(
            "outline.picker",
            crate::plugin_ui::PluginUiEventKind::Filter,
            json!("out"),
        )
        .unwrap();
    let raw = record_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let event: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(event["method"], "ui_event");
    assert_eq!(event["params"]["host"], "editor");
    assert_eq!(event["params"]["id"], "outline.picker");
    assert_eq!(event["params"]["kind"], "filter");
    assert_eq!(event["params"]["value"], "out");
}

#[test]
fn keymap_spec_round_trips_plugin_commands() {
    let parsed = parse_action_spec("plugin_command:outline.toggle:{\"mode\":\"tree\"}").unwrap();
    match &parsed {
        crate::keymap::Action::PluginCommandWithArgs(id, args) => {
            assert_eq!(id, "outline.toggle");
            assert_eq!(args, &json!({"mode": "tree"}));
        }
        other => panic!("unexpected action: {other:?}"),
    }
    assert_eq!(
        crate::keymap::format_action_spec(&parsed),
        "plugin_command:outline.toggle:{\"mode\":\"tree\"}"
    );
    assert_eq!(
        crate::keymap::format_action_spec(
            &parse_action_spec("plugin_command:outline.toggle").unwrap()
        ),
        "plugin_command:outline.toggle"
    );
}

#[test]
fn plugin_keybinding_is_merged_below_user_bindings() {
    let mut app = App::from_path(None).unwrap();
    app.backend.plugin_ui.apply_batch(&batch(1, demo_contributions(), json!([]))).unwrap();

    let binding = crate::keymap::BindingKey {
        mode: crate::app::Mode::Normal,
        key: crossterm::event::KeyCode::Char('o'),
        modifiers: crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::SHIFT,
        prefix: None,
    };
    assert_eq!(
        app.backend.plugin_ui.keybinding_action(&binding),
        Some(crate::keymap::Action::PluginCommand(String::from("outline.toggle")))
    );

    // An explicit user unbind suppresses the plugin default.
    app.config.keymap.operations.push(crate::keymap::KeymapOperation::Unbind(binding));
    assert!(crate::plugin_ui::keybindings::user_unbinds(&app.config.keymap, &binding));
}

#[test]
fn conflicting_plugin_keybindings_are_rejected_deterministically() {
    let mut registry = crate::plugin_ui::ContributionRegistry::default();
    let batch = batch(
        1,
        json!([
            {"kind": "keybinding", "command": "demo.one", "key": "ctrl+k", "when": "normal"},
            {"kind": "keybinding", "command": "demo.two", "key": "ctrl+k", "when": "normal"}
        ]),
        json!([]),
    );
    assert!(matches!(
        registry.apply(&batch),
        Err(crate::plugin_ui::RegistryError::KeybindingConflict { .. })
    ));
    assert_eq!(registry.revision(), 0);
}

#[test]
fn plugin_ex_command_with_missing_required_args_reports_usage() {
    let mut app = App::from_path(None).unwrap();
    app.backend
        .plugin_ui
        .apply_batch(&batch(
            1,
            json!([
                {
                    "kind": "command",
                    "id": "outline.toggle",
                    "title": "Toggle Outline",
                    "args_schema": {"type": "object", "required": ["mode"]}
                },
                {
                    "kind": "ex_command",
                    "name": "outline",
                    "command": "outline.toggle",
                    "usage": "outline <mode>"
                }
            ]),
            json!([]),
        ))
        .unwrap();
    assert!(app.try_execute_plugin_ex_command("outline"));
    assert!(app.backend.status_message.as_deref().is_some_and(|message| message.contains("usage")));
}

#[test]
fn unknown_plugin_command_surfaces_a_status_error() {
    let mut app = App::from_path(None).unwrap();
    app.invoke_plugin_command("missing.command", json!({}));
    assert!(
        app.backend
            .status_message
            .as_deref()
            .is_some_and(|message| message.contains("unknown plugin command"))
    );
}

#[test]
fn command_palette_includes_contributed_rows_with_usage() {
    let mut app = App::from_path(None).unwrap();
    app.backend.plugin_ui.apply_batch(&batch(1, demo_contributions(), json!([]))).unwrap();
    let rows = app.plugin_command_help_items();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].contains("[outline]"));
    assert!(rows[0].contains(":outline"));
    assert!(rows[0].contains("[mode]"));
    assert!(rows[0].contains("Toggle Outline"));
}

#[test]
fn status_panel_overlay_and_picker_render_from_fake_backend_data() {
    let mut app = App::from_path(None).unwrap();
    app.backend.plugin_ui.apply_batch(&batch(1, demo_contributions(), json!([]))).unwrap();
    app.backend
        .plugin_ui
        .apply_ui_update(
            &PluginUiUpdate::parse(&json!({
                "kind": "panel",
                "id": "outline.panel",
                "revision": 1,
                "visible": true,
                "data": {"items": ["alpha", "beta"]}
            }))
            .unwrap(),
        )
        .unwrap();
    app.backend
        .plugin_ui
        .apply_ui_update(
            &PluginUiUpdate::parse(&json!({
                "kind": "overlay",
                "id": "outline.overlay",
                "revision": 1,
                "visible": true,
                "data": {"text": "overlay text"}
            }))
            .unwrap(),
        )
        .unwrap();
    app.backend
        .plugin_ui
        .apply_ui_update(
            &PluginUiUpdate::parse(&json!({
                "kind": "picker",
                "id": "outline.picker",
                "revision": 1,
                "items": [{"name": "alpha"}, {"name": "beta"}]
            }))
            .unwrap(),
        )
        .unwrap();

    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let buffer = terminal.backend().buffer();
    let rows = (0..30)
        .map(|row| {
            (0..100).map(|col| buffer.cell((col, row)).unwrap().symbol()).collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(rows.contains("alpha"));
    assert!(rows.contains("beta"));
    assert!(rows.contains("overlay text"));
    assert!(rows.contains("Outline Picker"));
    assert!(rows.contains("outline: on"));
    assert!(rows.contains("Toggle Outline") || rows.contains("Outline"));
}

#[test]
fn unknown_dynamic_panel_is_diagnosed_and_not_rendered() {
    let mut state = PluginUiState::new();
    state.apply_batch(&batch(1, demo_contributions(), json!([]))).unwrap();
    let unknown = PluginUiUpdate::parse(&json!({
        "kind": "panel",
        "id": "missing.panel",
        "revision": 1,
        "visible": true,
        "data": {"text": "nope"}
    }))
    .unwrap();
    assert!(state.apply_ui_update(&unknown).is_err());
    assert!(state.panel_content.is_empty());
}

#[test]
fn status_and_health_snapshots_become_displayable_diagnostics() {
    let mut state = PluginUiState::new();
    let status = PluginStatusSnapshot::parse(&json!({
        "generation": 7,
        "plugins": [
            {"name": "lsp", "version": "1.0", "kind": "native", "active": true, "health": {"state": "loaded"}},
            {"name": "demo", "version": "2.0", "kind": "wasm", "active": false, "health": {"state": "blocked", "reason": "permission"}}
        ]
    }))
    .unwrap();
    state.apply_status(status);
    assert_eq!(state.status().generation, 7);
    assert!(state.diagnostics().iter().any(|diagnostic| diagnostic.message().contains("blocked")));

    state.apply_health(
        PluginHealthEvent::parse(&json!({
            "plugin": "demo",
            "state": "crashed",
            "reason": "resource_limit"
        }))
        .unwrap(),
    );
    assert!(state.diagnostics().iter().any(|diagnostic| diagnostic.message().contains("crashed")));

    state.apply_health(
        PluginHealthEvent::parse(&json!({
            "plugin": "lsp",
            "state": "degraded",
            "reason": "transport_timeout"
        }))
        .unwrap(),
    );
    assert!(state.diagnostics().iter().any(|diagnostic| diagnostic.message().contains("timeout")));

    state.apply_health(
        PluginHealthEvent::parse(&json!({
            "plugin": "demo",
            "state": "reloaded"
        }))
        .unwrap(),
    );
    assert!(state.diagnostics().iter().any(|diagnostic| diagnostic.message().contains("reloaded")));
}

#[test]
fn unsupported_renderer_kind_is_rejected_as_a_malformed_batch() {
    let batch = ContributionBatch::parse(&json!({
        "revision": 1,
        "upsert": [{
            "kind": "panel",
            "id": "demo.panel",
            "title": "Demo",
            "placement": "right",
            "render": {"type": "tree"}
        }]
    }));
    assert!(batch.is_err());
}

#[test]
fn plugin_picker_routes_filter_select_activate_and_close_events() {
    let (record_tx, record_rx) = mpsc::channel::<String>();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut manager = BufferManager::test_new(record_tx, backend_rx, String::from("view-id-1"));
    manager.plugin_ui.apply_batch(&batch(1, demo_contributions(), json!([]))).unwrap();
    manager
        .plugin_ui
        .apply_ui_update(
            &PluginUiUpdate::parse(&json!({
                "kind": "picker",
                "id": "outline.picker",
                "revision": 1,
                "items": [{"name": "alpha"}, {"name": "beta"}]
            }))
            .unwrap(),
        )
        .unwrap();

    let picker = manager.plugin_ui.picker.as_mut().unwrap();
    picker.move_down();
    let id = picker.id.clone();
    manager
        .send_plugin_ui_event(
            id.as_str(),
            crate::plugin_ui::PluginUiEventKind::Select,
            json!("beta"),
        )
        .unwrap();
    manager
        .send_plugin_ui_event(
            id.as_str(),
            crate::plugin_ui::PluginUiEventKind::Activate,
            json!("beta"),
        )
        .unwrap();
    manager
        .send_plugin_ui_event(id.as_str(), crate::plugin_ui::PluginUiEventKind::Close, Value::Null)
        .unwrap();

    let methods = (0..3)
        .map(|_| {
            let raw = record_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            serde_json::from_str::<Value>(&raw).unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(methods[0]["params"]["kind"], "select");
    assert_eq!(methods[1]["params"]["kind"], "activate");
    assert_eq!(methods[2]["params"]["kind"], "close");
}
