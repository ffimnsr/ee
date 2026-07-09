use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::mpsc::TryRecvError;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};
use xi_core_lib::plugins::rpc::ClientPluginInfo;

use crate::app::App;
use crate::backend::{
    BackendEvent, CachedLine, CoreAnnotation, CoreLine, CoreSyntaxSpan, CoreUpdate, CoreUpdateKind,
    CoreUpdateOp, LineSlot, invalid_line_ranges, parse_notification, startup_render_ready,
};
use crate::buffer::BufferManager;
use crate::git::{GitBufferCache, GitBufferStatus, GitSign};
use crate::tests::helpers::*;

#[test]
fn plugin_labels_render_expected_values() {
    let startup = xi_core_lib::plugin_manifest::PluginDescription {
        name: String::from("startup-plugin"),
        version: String::from("1.0.0"),
        requires: Vec::new(),
        scope: Default::default(),
        runtime: xi_core_lib::plugin_manifest::PluginRuntime::Native,
        capabilities: Vec::new(),
        launch: Default::default(),
        max_rss_bytes: None,
        max_cpu_seconds: None,
        rpc_timeout_ms: None,
        exec_path: std::path::PathBuf::from("bin/startup-plugin"),
        activations: Vec::new(),
        commands: Vec::new(),
        languages: Vec::new(),
    };
    let command = xi_core_lib::plugin_manifest::PluginDescription {
        name: String::from("command-plugin"),
        version: String::from("2.0.0"),
        requires: Vec::new(),
        scope: Default::default(),
        runtime: xi_core_lib::plugin_manifest::PluginRuntime::Wasm,
        capabilities: Vec::new(),
        launch: Default::default(),
        max_rss_bytes: None,
        max_cpu_seconds: None,
        rpc_timeout_ms: None,
        exec_path: std::path::PathBuf::from("bin/command-plugin"),
        activations: vec![xi_core_lib::plugin_manifest::PluginActivation::OnCommand],
        commands: Vec::new(),
        languages: Vec::new(),
    };

    assert_eq!(crate::plugin_activation_label(&startup), "startup");
    assert_eq!(crate::plugin_activation_label(&command), "command");
    assert_eq!(crate::plugin_runtime_label(&startup), "native");
    assert_eq!(crate::plugin_runtime_label(&command), "wasm");
}

#[test]
fn available_plugins_notification_updates_buffer_manager_snapshot() {
    let (tx, _rx) = mpsc::channel();
    let (backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    backend_tx
        .send(BackendEvent::AvailablePlugins {
            view_id: String::from("view-id-1"),
            plugins: vec![ClientPluginInfo { name: String::from("lsp"), running: true }],
        })
        .expect("send should succeed");

    client.drain_events().expect("drain should not fail");

    assert_eq!(client.available_plugins_for_current_view().len(), 1);
    assert_eq!(client.available_plugins_for_current_view()[0].name, "lsp");
    assert!(client.available_plugins_for_current_view()[0].running);
}

#[test]
fn parse_available_plugins_notification() {
    let params = json!({
        "view_id": "view-id-1",
        "plugins": [
            { "name": "lsp", "running": true },
            { "name": "fmt", "running": false }
        ]
    });

    let event =
        parse_notification("available_plugins", params).expect("available_plugins should parse");

    match event {
        BackendEvent::AvailablePlugins { view_id, plugins } => {
            assert_eq!(view_id, "view-id-1");
            assert_eq!(plugins.len(), 2);
            assert_eq!(plugins[0].name, "lsp");
            assert!(plugins[0].running);
            assert_eq!(plugins[1].name, "fmt");
            assert!(!plugins[1].running);
        }
        other => panic!("unexpected event: {:?}", other),
    }
}

#[test]
fn parse_notification_decodes_syntax_spans_in_update_lines() {
    let event = parse_notification(
        "update",
        json!({
            "view_id": "view-id-1",
            "update": {
                "pristine": true,
                "annotations": [],
                "ops": [{
                    "op": "ins",
                    "n": 1,
                    "lines": [{
                        "text": "let x = 1",
                        "cursor": [3],
                        "syntax_spans": [
                            { "start_byte": 0, "end_byte": 3, "scope": "keyword.control.rust" },
                            { "start_byte": 8, "end_byte": 9, "scope": "constant.numeric.decimal.rust" }
                        ]
                    }]
                }]
            }
        }),
    )
    .expect("update notification should parse");

    let BackendEvent::Update { update, .. } = event else { panic!("expected update event") };
    let spans = update.ops[0].lines[0].syntax_spans.as_ref().expect("missing syntax spans");
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0].scope, "keyword.control.rust");
}

#[test]
fn apply_update_merges_copy_update_insert_and_invalidate() {
    let mut client = test_buf_state();
    client.line_cache = vec![
        LineSlot::Known(CachedLine {
            text: "alpha".into(),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
        LineSlot::Known(CachedLine {
            text: "beta".into(),
            cursors: vec![2],
            syntax_spans: vec![CoreSyntaxSpan {
                start_byte: 0,
                end_byte: 4,
                scope: "keyword.control.rust".into(),
            }],
        }),
        LineSlot::Known(CachedLine {
            text: "gamma".into(),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
    ];
    client.rebuild_lines();

    client
        .apply_update(CoreUpdate {
            pristine: false,
            annotations: vec![CoreAnnotation {
                annotation_type: String::from("selection"),
                ranges: vec![[1, 1, 1, 3]],
                payloads: None,
            }],
            ops: vec![
                CoreUpdateOp { op: CoreUpdateKind::Copy, n: 1, lines: Vec::new() },
                CoreUpdateOp {
                    op: CoreUpdateKind::Update,
                    n: 1,
                    lines: vec![CoreLine { text: None, cursor: vec![1], syntax_spans: None }],
                },
                CoreUpdateOp {
                    op: CoreUpdateKind::Insert,
                    n: 1,
                    lines: vec![CoreLine {
                        text: Some("delta".into()),
                        cursor: Vec::new(),
                        syntax_spans: Some(vec![CoreSyntaxSpan {
                            start_byte: 0,
                            end_byte: 5,
                            scope: "entity.name.function.rust".into(),
                        }]),
                    }],
                },
                CoreUpdateOp { op: CoreUpdateKind::Invalidate, n: 2, lines: Vec::new() },
            ],
        })
        .unwrap();

    assert_eq!(client.lines, vec!["alpha", "beta", "delta", "", ""]);
    assert_eq!((client.cursor_line, client.cursor_col), (1, 1));
    let LineSlot::Known(line) = &client.line_cache[1] else { panic!("expected cached line") };
    assert_eq!(line.syntax_spans.len(), 1);
    let LineSlot::Known(line) = &client.line_cache[2] else { panic!("expected cached line") };
    assert_eq!(line.syntax_spans.len(), 1);
    assert_eq!(invalid_line_ranges(&client.line_cache), vec![(3, 5)]);
    assert_eq!(client.annotations.len(), 1);
    assert!(!client.pristine);
}

#[test]
fn update_merge_normalizes_line_text() {
    let slot = LineSlot::Known(CachedLine {
        text: String::from("alpha"),
        cursors: Vec::new(),
        syntax_spans: Vec::new(),
    });

    let merged = slot
        .merge(CoreLine {
            text: Some(String::from("beta\n")),
            cursor: Vec::new(),
            syntax_spans: None,
        })
        .expect("update merge should succeed");

    let LineSlot::Known(line) = merged else { panic!("expected known line") };
    assert_eq!(line.text, "beta");
}

#[test]
fn pristine_external_reload_update_clears_changed_flag_for_trailing_blank_line_removal() {
    let path = unique_temp_path("ee-cli-external-reload-state");
    fs::write(&path, "alpha\n\n").unwrap();

    let (tx, _rx) = mpsc::channel();
    let (backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    let buf_id = client.active().id;
    client.set_buffer_path(buf_id, path.clone()).unwrap();
    client.line_cache = vec![
        LineSlot::Known(CachedLine {
            text: String::from("alpha"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
        LineSlot::Known(CachedLine {
            text: String::new(),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
        LineSlot::Known(CachedLine {
            text: String::new(),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
    ];
    client.rebuild_lines();

    let previous_mtime = client.mtime;
    thread::sleep(Duration::from_millis(25));
    fs::write(&path, "alpha\n").unwrap();
    client.check_external_changes();
    assert!(client.externally_modified);

    backend_tx
        .send(BackendEvent::Update {
            view_id: String::from("view-id-1"),
            update: CoreUpdate {
                pristine: true,
                annotations: Vec::new(),
                ops: vec![CoreUpdateOp { op: CoreUpdateKind::Copy, n: 2, lines: Vec::new() }],
            },
        })
        .unwrap();

    client.drain_events().unwrap();

    assert!(!client.externally_modified);
    assert_eq!(client.status_message.as_deref(), Some("reloaded"));
    assert_eq!(client.lines, vec![String::from("alpha"), String::new()]);
    assert_ne!(client.mtime, previous_mtime);

    let _ = fs::remove_file(&path);
}

#[test]
fn stale_view_updates_are_ignored() {
    let (tx, _rx) = mpsc::channel();
    let (backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("live-view"));
    client.line_cache = vec![LineSlot::Known(CachedLine {
        text: String::from("alpha"),
        cursors: Vec::new(),
        syntax_spans: Vec::new(),
    })];
    client.rebuild_lines();

    backend_tx
        .send(BackendEvent::Update {
            view_id: String::from("stale-view"),
            update: CoreUpdate {
                pristine: true,
                annotations: Vec::new(),
                ops: vec![CoreUpdateOp { op: CoreUpdateKind::Skip, n: 2, lines: Vec::new() }],
            },
        })
        .unwrap();

    client.drain_events().unwrap();

    assert_eq!(client.lines, vec![String::from("alpha")]);
    assert_eq!(client.line_cache.len(), 1);
}

#[test]
fn core_update_keeps_invalid_lines_lazy() {
    let (tx, rx) = mpsc::channel();
    let (backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    backend_tx
        .send(BackendEvent::Update {
            view_id: String::from("view-id-1"),
            update: CoreUpdate {
                pristine: true,
                annotations: Vec::new(),
                ops: vec![
                    CoreUpdateOp {
                        op: CoreUpdateKind::Insert,
                        n: 1,
                        lines: vec![CoreLine {
                            text: Some(String::from("visible")),
                            cursor: Vec::new(),
                            syntax_spans: None,
                        }],
                    },
                    CoreUpdateOp { op: CoreUpdateKind::Invalidate, n: 100_000, lines: Vec::new() },
                ],
            },
        })
        .unwrap();

    client.drain_events().unwrap();

    assert_eq!(client.lines.first().map(String::as_str), Some("visible"));
    assert_eq!(client.lines.len(), 100_001);
    assert_eq!(invalid_line_ranges(&client.line_cache), vec![(1, 100_001)]);
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn normal_invalid_line_requests_are_viewport_bounded() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    client.line_cache = vec![LineSlot::Invalid; 10_000];

    client.notify_scroll(1_000, 1_020).unwrap();
    let scroll: Value =
        serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).unwrap()).unwrap();
    assert_eq!(scroll["params"]["method"], "scroll");

    client.sync_pending_events().unwrap();

    let request: Value =
        serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).unwrap()).unwrap();
    assert_eq!(request["params"]["method"], "request_lines");
    assert_eq!(request["params"]["params"], json!([936, 1084]));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn source_control_skips_lazy_line_cache() {
    let (tx, _rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.line_cache = vec![
        LineSlot::Known(CachedLine {
            text: String::from("visible"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
        LineSlot::Invalid,
    ];
    app.backend.rebuild_lines();

    app.refresh_source_control();

    assert!(app.source_control.is_empty());
}

#[test]
fn source_control_skips_vlf_buffers_and_clears_stale_cache() {
    let mut app = App::from_path(None).unwrap();
    let buf_id = app.backend.active().id;
    app.backend.is_vlf = true;
    app.backend.line_cache = vec![LineSlot::Known(CachedLine {
        text: String::from("visible"),
        cursors: Vec::new(),
        syntax_spans: Vec::new(),
    })];
    app.source_control.insert(
        buf_id,
        GitBufferCache {
            fingerprint: 123,
            path: Some(PathBuf::from("/tmp/stale.rs")),
            last_refresh: Instant::now(),
            status: Some(GitBufferStatus {
                repo_root: PathBuf::from("/tmp/repo"),
                repo_name: String::from("repo"),
                repo_relative: String::from("src/lib.rs"),
                branch: String::from("main"),
                tracked: true,
                dirty: true,
                hunks: Vec::new(),
                line_signs: HashMap::from([(0, GitSign::Modified)]),
            }),
        },
    );

    app.refresh_source_control();

    assert!(app.source_control.is_empty());
}

#[test]
fn input_idle_gate_blocks_auto_source_control_during_key_bursts() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
    assert!(!app.input_idle_for(Duration::from_millis(250)));

    app.last_input_at = Instant::now() - Duration::from_millis(300);
    assert!(app.input_idle_for(Duration::from_millis(250)));
}

#[test]
fn open_file_bootstraps_visible_lines_lazily() {
    let path = unique_temp_path("ee-cli-open");
    let contents = (0..24).map(|i| format!("line-{i}")).collect::<Vec<_>>().join("\n");
    fs::write(&path, &contents).unwrap();

    let app = App::from_path(Some(path.clone())).unwrap();

    fs::remove_file(&path).unwrap();
    let expected = contents.split('\n').map(ToOwned::to_owned).collect::<Vec<_>>();
    assert_eq!(&app.backend.lines[..12], &expected[..12]);
    assert_eq!(app.backend.lines.len(), expected.len());
    assert_eq!(invalid_line_ranges(&app.backend.line_cache), vec![(12, 24)]);
}

#[test]
fn startup_render_ready_after_first_visible_line() {
    assert!(!startup_render_ready(&[]));
    assert!(!startup_render_ready(&[LineSlot::Invalid]));
    assert!(startup_render_ready(&[LineSlot::Known(CachedLine {
        text: String::from("line-0"),
        cursors: Vec::new(),
        syntax_spans: Vec::new(),
    })]));
    assert!(!startup_render_ready(&[
        LineSlot::Invalid,
        LineSlot::Known(CachedLine {
            text: String::from("line-1"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
    ]));
}

#[test]
fn open_many_line_20mb_fixture_meets_first_render_budget() {
    assert_open_to_first_render_budget("many-line", budget_many_line);
}

#[test]
fn open_long_line_20mb_fixture_meets_first_render_budget() {
    assert_open_to_first_render_budget("long-line", budget_long_line);
}

#[test]
#[ignore = "manual perf breakdown probe"]
fn open_many_line_20mb_fixture_reports_startup_breakdown() {
    report_open_to_first_render_breakdown("many-line", budget_many_line);
}

#[test]
#[ignore = "manual perf breakdown probe"]
fn open_long_line_20mb_fixture_reports_startup_breakdown() {
    report_open_to_first_render_breakdown("long-line", budget_long_line);
}

#[test]
fn parse_notification_handles_completions() {
    let event = parse_notification(
        "completions",
        json!({
            "view_id": "view-id-1",
            "items": [{
                "label": "println!",
                "detail": "macro",
                "insert_text": "println!($0)"
            }]
        }),
    )
    .expect("completion notification should parse");

    match event {
        BackendEvent::Completions { view_id, items } => {
            assert_eq!(view_id, "view-id-1");
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].label, "println!");
        }
        other => panic!("unexpected event: {:?}", other),
    }
}

#[test]
fn parse_notification_handles_diagnostics() {
    let event = parse_notification(
        "diagnostics",
        json!({
            "view_id": "view-id-1",
            "diagnostics": [{
                "range": { "start": 2, "end": 5 },
                "severity": "warning",
                "message": "watch this",
                "source": "lsp",
                "code": "W1"
            }]
        }),
    )
    .expect("diagnostics notification should parse");

    match event {
        BackendEvent::Diagnostics { view_id, diagnostics } => {
            assert_eq!(view_id, "view-id-1");
            assert_eq!(diagnostics.len(), 1);
            assert_eq!(diagnostics[0].message, "watch this");
        }
        other => panic!("unexpected event: {:?}", other),
    }
}

#[test]
fn parse_notification_handles_update_annotations() {
    let event = parse_notification(
        "update",
        json!({
            "view_id": "view-id-1",
            "update": {
                "ops": [],
                "pristine": true,
                "annotations": [{
                    "type": "selection",
                    "ranges": [[0, 1, 0, 4]],
                    "payloads": ["cursor"],
                    "n": 1
                }]
            }
        }),
    )
    .expect("update notification should parse");

    match event {
        BackendEvent::Update { view_id, update } => {
            assert_eq!(view_id, "view-id-1");
            assert_eq!(update.annotations.len(), 1);
            assert_eq!(update.annotations[0].annotation_type, "selection");
            assert_eq!(update.annotations[0].ranges, vec![[0, 1, 0, 4]]);
        }
        other => panic!("unexpected event: {:?}", other),
    }
}

#[test]
fn parse_notification_handles_code_actions() {
    let event = parse_notification(
        "code_actions",
        json!({
            "view_id": "view-id-1",
            "actions": [{ "title": "Extract variable" }]
        }),
    )
    .expect("code action notification should parse");

    match event {
        BackendEvent::CodeActions { view_id, actions } => {
            assert_eq!(view_id, "view-id-1");
            assert_eq!(actions[0].title, "Extract variable");
        }
        other => panic!("unexpected event: {:?}", other),
    }
}
