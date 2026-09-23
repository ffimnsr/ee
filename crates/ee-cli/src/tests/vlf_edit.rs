use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;

use crate::app::{App, Mode};
use crate::backend::{
    BackendEvent, CachedLine, CoreLine, CoreSyntaxSpan, CoreUpdate, CoreUpdateKind, CoreUpdateOp,
    LineSlot,
};
use crate::buffer::BufferManager;
use crate::tests::helpers::*;
use serde_json::json;

#[test]
fn vlf_local_navigation_moves_cursor_without_core_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.line_cache = vec![
        LineSlot::Known(CachedLine {
            text: String::from("alpha"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
            logical_line: None,
        }),
        LineSlot::Known(CachedLine {
            text: String::from("beta"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
            logical_line: None,
        }),
    ];

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));

    assert_eq!(app.backend.cursor_line, 1);
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn vlf_insert_key_uses_overlay_edit_rpc_without_cursor_jump() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.vlf_cache_start_line = 40;
    app.backend.vlf_approx_line_count = 100;
    app.backend.vlf_line_count_exact = true;
    app.backend.cursor_line = 41;
    app.backend.cursor_col = 2;
    app.backend.line_cache = vec![
        LineSlot::Known(CachedLine {
            text: String::from("alpha"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
            logical_line: None,
        }),
        LineSlot::Known(CachedLine {
            text: String::from("beta"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
            logical_line: None,
        }),
    ];
    app.last_editor_height = 6;

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)));

    assert_eq!(app.mode, Mode::Insert);
    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (41, 3));
    assert_eq!(app.backend.status_message, None);
    assert_eq!(app.backend.get_line(41), Some("bexta"));

    let first: Value = serde_json::from_str(
        &rx.recv_timeout(Duration::from_secs(1)).expect("vlf edit rpc should be sent"),
    )
    .expect("message should be json");
    assert_eq!(first["method"], "edit");
    assert_eq!(first["params"]["method"], "vlf_replace_range");
    assert_eq!(first["params"]["params"]["start_line"], 41);
    assert_eq!(first["params"]["params"]["start_col"], 2);
    assert_eq!(first["params"]["params"]["end_line"], 41);
    assert_eq!(first["params"]["params"]["end_col"], 2);
    assert_eq!(first["params"]["params"]["text"], "x");

    let second: Value = serde_json::from_str(
        &rx.recv_timeout(Duration::from_secs(1)).expect("viewport refresh should follow vlf edit"),
    )
    .expect("message should be json");
    assert_eq!(second["params"]["method"], "scroll");
}

#[test]
fn vlf_insert_preserves_untouched_syntax_spans_before_viewport_reply() {
    let (tx, _rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.vlf_cache_start_line = 41;
    app.backend.vlf_approx_line_count = 100;
    app.backend.vlf_line_count_exact = true;
    app.backend.cursor_line = 41;
    app.backend.cursor_col = 2;
    app.backend.line_cache = vec![LineSlot::Known(CachedLine {
        text: String::from("beta"),
        cursors: Vec::new(),
        syntax_spans: vec![
            CoreSyntaxSpan { start_byte: 0, end_byte: 2, scope: String::from("prefix") },
            CoreSyntaxSpan { start_byte: 2, end_byte: 4, scope: String::from("suffix") },
        ],
        logical_line: None,
    })];
    app.last_editor_height = 6;

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)));

    let spans = match app.backend.line_slot(41) {
        Some(LineSlot::Known(line)) => line.syntax_spans.clone(),
        other => panic!("expected cached VLF line, got {other:?}"),
    };
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0].scope, "prefix");
    assert_eq!((spans[0].start_byte, spans[0].end_byte), (0, 2));
    assert_eq!(spans[1].scope, "suffix");
    assert_eq!((spans[1].start_byte, spans[1].end_byte), (3, 5));
}

#[test]
fn vlf_insert_forces_viewport_refresh_when_current_range_is_already_cached() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.vlf_cache_start_line = 40;
    app.backend.vlf_approx_line_count = 100;
    app.backend.vlf_line_count_exact = true;
    app.backend.cursor_line = 41;
    app.backend.cursor_col = 2;
    app.backend.last_scroll = Some((40, 46));
    app.viewport.top_line = 40;
    app.last_editor_height = 6;
    app.backend.line_cache = vec![
        LineSlot::Known(CachedLine {
            text: String::from("alpha"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
            logical_line: None,
        }),
        LineSlot::Known(CachedLine {
            text: String::from("beta"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
            logical_line: None,
        }),
        LineSlot::Known(CachedLine {
            text: String::from("gamma"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
            logical_line: None,
        }),
        LineSlot::Known(CachedLine {
            text: String::from("delta"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
            logical_line: None,
        }),
        LineSlot::Known(CachedLine {
            text: String::from("epsilon"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
            logical_line: None,
        }),
        LineSlot::Known(CachedLine {
            text: String::from("zeta"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
            logical_line: None,
        }),
    ];

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)));

    let first: Value = serde_json::from_str(
        &rx.recv_timeout(Duration::from_secs(1)).expect("vlf edit rpc should be sent"),
    )
    .expect("message should be json");
    assert_eq!(first["params"]["method"], "vlf_replace_range");

    let second: Value = serde_json::from_str(
        &rx.recv_timeout(Duration::from_secs(1))
            .expect("forced viewport refresh should be sent for cached range"),
    )
    .expect("message should be json");
    assert_eq!(second["params"]["method"], "scroll");
    assert_eq!(second["params"]["params"], json!([40, 46]));
}

#[test]
fn vlf_insert_newline_updates_local_cache_before_viewport_reply() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.vlf_cache_start_line = 40;
    app.backend.vlf_approx_line_count = 100;
    app.backend.vlf_line_count_exact = true;
    app.backend.cursor_line = 41;
    app.backend.cursor_col = 2;
    app.backend.line_cache = vec![
        LineSlot::Known(CachedLine {
            text: String::from("alpha"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
            logical_line: None,
        }),
        LineSlot::Known(CachedLine {
            text: String::from("beta"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
            logical_line: None,
        }),
    ];
    app.last_editor_height = 6;

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    assert_eq!(app.mode, Mode::Insert);
    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (42, 0));
    assert_eq!(app.backend.get_line(41), Some("be"));
    assert_eq!(app.backend.get_line(42), Some("ta"));
    assert_eq!(app.backend.line_count(), 101);

    let first: Value = serde_json::from_str(
        &rx.recv_timeout(Duration::from_secs(1)).expect("newline edit rpc should be sent"),
    )
    .expect("message should be json");
    assert_eq!(first["params"]["method"], "vlf_replace_range");
    assert_eq!(first["params"]["params"]["text"], "\n");

    let second: Value = serde_json::from_str(
        &rx.recv_timeout(Duration::from_secs(1)).expect("viewport refresh should follow"),
    )
    .expect("message should be json");
    assert_eq!(second["params"]["method"], "scroll");
}

#[test]
fn vlf_backspace_updates_local_cache_before_viewport_reply() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.vlf_cache_start_line = 41;
    app.backend.vlf_approx_line_count = 100;
    app.backend.vlf_line_count_exact = true;
    app.backend.cursor_line = 42;
    app.backend.cursor_col = 0;
    app.backend.line_cache = vec![
        LineSlot::Known(CachedLine {
            text: String::from("be"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
            logical_line: None,
        }),
        LineSlot::Known(CachedLine {
            text: String::from("ta"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
            logical_line: None,
        }),
    ];
    app.last_editor_height = 6;

    app.mode = Mode::Insert;
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)));

    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (41, 2));
    assert_eq!(app.backend.get_line(41), Some("beta"));
    assert_eq!(app.backend.get_line(42), None);
    assert_eq!(app.backend.line_count(), 99);

    let first: Value = serde_json::from_str(
        &rx.recv_timeout(Duration::from_secs(1)).expect("backspace edit rpc should be sent"),
    )
    .expect("message should be json");
    assert_eq!(first["params"]["method"], "vlf_replace_range");
    assert_eq!(first["params"]["params"]["start_line"], 41);
    assert_eq!(first["params"]["params"]["start_col"], 2);
    assert_eq!(first["params"]["params"]["end_line"], 42);
    assert_eq!(first["params"]["params"]["end_col"], 0);
    assert_eq!(first["params"]["params"]["text"], "");

    let second: Value = serde_json::from_str(
        &rx.recv_timeout(Duration::from_secs(1)).expect("viewport refresh should follow"),
    )
    .expect("message should be json");
    assert_eq!(second["params"]["method"], "scroll");
}

#[test]
fn vlf_delete_char_forward_command_uses_overlay_edit_rpc() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.vlf_cache_start_line = 41;
    app.backend.vlf_approx_line_count = 100;
    app.backend.vlf_line_count_exact = true;
    app.backend.cursor_line = 41;
    app.backend.cursor_col = 2;
    app.backend.line_cache = vec![
        LineSlot::Known(CachedLine {
            text: String::from("be"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
            logical_line: None,
        }),
        LineSlot::Known(CachedLine {
            text: String::from("ta"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
            logical_line: None,
        }),
    ];
    app.last_editor_height = 6;

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)));
    for ch in "delete_char_forward".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    assert_eq!(app.backend.get_line(41), Some("beta"));
    assert_eq!(app.backend.get_line(42), None);

    let first: Value = serde_json::from_str(
        &rx.recv_timeout(Duration::from_secs(1)).expect("command edit rpc should be sent"),
    )
    .expect("message should be json");
    assert_eq!(first["params"]["method"], "vlf_replace_range");
    assert_eq!(first["params"]["params"]["start_line"], 41);
    assert_eq!(first["params"]["params"]["start_col"], 2);
    assert_eq!(first["params"]["params"]["end_line"], 42);
    assert_eq!(first["params"]["params"]["end_col"], 0);
    assert_eq!(first["params"]["params"]["text"], "");

    let second: Value = serde_json::from_str(
        &rx.recv_timeout(Duration::from_secs(1)).expect("viewport refresh should follow"),
    )
    .expect("message should be json");
    assert_eq!(second["params"]["method"], "scroll");
}

#[test]
fn vlf_zero_moves_to_line_start_without_core_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.cursor_line = 9;
    app.backend.cursor_col = 4;
    app.backend.line_cache = vec![LineSlot::Invalid; 20];
    app.backend.line_cache[9] = LineSlot::Known(CachedLine {
        text: String::from("alpha"),
        cursors: Vec::new(),
        syntax_spans: Vec::new(),
        logical_line: None,
    });

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE)));

    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (9, 0));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn vlf_goto_last_line_uses_sparse_line_count_without_core_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.line_cache = vec![LineSlot::Invalid; 500];
    app.backend.vlf_line_count_exact = true;

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE)));

    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (499, 0));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn vlf_goto_last_line_uses_reported_line_count_without_core_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.line_cache = vec![LineSlot::Invalid; 500];
    app.backend.vlf_approx_line_count = 10_000;
    app.backend.vlf_line_count_exact = true;

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE)));

    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (9_999, 0));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn vlf_goto_last_line_requests_tail_scroll_when_count_is_approximate() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.line_cache = vec![LineSlot::Invalid; 500];
    app.backend.vlf_approx_line_count = 10_000;
    app.backend.vlf_line_count_exact = false;
    app.last_editor_height = 40;

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE)));

    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (0, 0));
    let message: Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).unwrap())
        .expect("tail scroll request should be json");
    assert_eq!(message["params"]["method"], "scroll");
    assert_eq!(message["params"]["params"], json!([9_000_000_000i64 - 40, 9_000_000_000i64]));
    assert!(app.backend.pending_vlf_tail_jump);
}

#[test]
fn vlf_navigation_away_from_pending_tail_jump_cancels_tail_jump() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.line_cache = vec![LineSlot::Invalid; 500];
    app.backend.vlf_approx_line_count = 10_000;
    app.backend.vlf_line_count_exact = false;
    app.last_editor_height = 40;

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE)));
    let tail_request: Value =
        serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).unwrap())
            .expect("tail scroll request should be json");
    assert_eq!(tail_request["params"]["method"], "scroll");
    assert!(app.backend.pending_vlf_tail_jump);

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE)));

    assert_eq!(app.backend.cursor_line, 0);
    assert!(!app.backend.pending_vlf_tail_jump);
    assert!(!app.backend.pending_line_request);

    app.backend.notify_scroll(0, 40).unwrap();
    let top_request: Value =
        serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).unwrap())
            .expect("top viewport request should be json");
    assert_eq!(top_request["params"]["method"], "scroll");
    assert_eq!(top_request["params"]["params"], json!([0, 40]));
}

#[test]
fn vlf_tail_jump_lands_cursor_when_tail_window_update_arrives() {
    let (tx, rx) = mpsc::channel();
    let (backend_tx, backend_rx) = mpsc::channel();
    let mut mgr = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    mgr.is_vlf = true;
    mgr.vlf_approx_line_count = 10_000;

    mgr.request_vlf_tail_viewport(40).unwrap();
    let first: Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).unwrap())
        .expect("tail scroll request should be json");
    assert_eq!(first["params"]["method"], "scroll");
    assert!(mgr.pending_vlf_tail_jump);

    // The tail window lands through the unified update channel.
    let ops = (0..40)
        .map(|idx| CoreUpdateOp {
            op: CoreUpdateKind::Insert,
            n: 1,
            lines: vec![CoreLine {
                text: Some(format!("tail {idx}\n")),
                cursor: Vec::new(),
                syntax_spans: Some(Vec::new()),
                logical_line: Some(9_960 + idx),
            }],
        })
        .collect::<Vec<_>>();
    backend_tx
        .send(BackendEvent::Update {
            view_id: String::from("view-id-1"),
            update: CoreUpdate {
                ops,
                pristine: true,
                annotations: Vec::new(),
                vlf_total_lines: Some(crate::backend::VlfTotalLines {
                    count: 10_000,
                    exact: false,
                    index_progress: 0.1,
                }),
            },
        })
        .unwrap();
    mgr.drain_events().unwrap();

    // Inexact landing follows the tail but keeps the jump pending until the
    // count is exact (the index can still move the true end).
    assert!(mgr.pending_vlf_tail_jump);
    assert_eq!(mgr.cursor_line, 9_999);
    assert_eq!(mgr.vlf_cache_start_line, 9_960);
    assert_eq!(mgr.get_line(9_960), Some("tail 0"));
}

#[test]
fn vlf_startup_pump_requests_initial_scroll_after_document_mode() {
    let path = unique_temp_path("ee-cli-vlf-startup");
    fs::write(&path, "alpha\nbeta\n").unwrap();

    let (tx, rx) = mpsc::channel();
    let (backend_tx, backend_rx) = mpsc::channel();
    let mut mgr = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    mgr.path = Some(path.clone());

    backend_tx
        .send(BackendEvent::DocumentMode { view_id: String::from("view-id-1"), is_vlf: true })
        .unwrap();

    mgr.pump_init().unwrap();

    let message: Value = serde_json::from_str(
        &rx.recv_timeout(Duration::from_secs(1)).expect("initial VLF scroll should be sent"),
    )
    .expect("scroll request should be json");
    assert_eq!(message["params"]["method"], "scroll");
    assert_eq!(message["params"]["params"], json!([0, 200]));
    assert_eq!(mgr.last_scroll, Some((0, 200)));

    fs::remove_file(path).unwrap();
}

#[test]
#[ignore = "manual real-fixture check; requires test_assets/vbig-100.txt"]
fn vlf_goto_vbig_100_matches_wc_last_line() {
    let _guard = large_fixture_test_lock();
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test_assets/vbig-100.txt");
    if !path.exists() {
        eprintln!("missing {}", path.display());
        return;
    }
    if skip_large_fixture_if_low_memory(&path) {
        return;
    }

    let wc = Command::new("wc").arg("-l").arg(&path).output().expect("wc -l should run");
    assert!(wc.status.success(), "wc -l failed: {wc:?}");
    let stdout = String::from_utf8(wc.stdout).expect("wc output should be utf8");
    let expected_last_line = stdout
        .split_whitespace()
        .next()
        .expect("wc output should include count")
        .parse::<usize>()
        .expect("wc count should parse");
    let expected_text = String::from_utf8(
        Command::new("tail").arg("-n").arg("1").arg(&path).output().unwrap().stdout,
    )
    .unwrap()
    .trim_end_matches('\n')
    .to_owned();

    let mut app = App::from_path(Some(path)).unwrap();
    for _ in 0..20 {
        app.backend.pump().unwrap();
        if app.backend.is_vlf {
            break;
        }
    }
    assert!(app.backend.is_vlf, "fixture should open in VLF");

    app.last_editor_height = 40;
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE)));

    for _ in 0..40 {
        app.backend.pump().unwrap();
        if !app.backend.pending_vlf_tail_jump && app.backend.cursor_line == expected_last_line {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(app.backend.cursor_line, expected_last_line);
    assert_eq!(app.backend.get_line(app.backend.cursor_line), Some(expected_text.as_str()));
}

#[test]
#[ignore = "manual real-fixture check; requires test_assets/vbig-2gb.txt"]
fn vlf_open_vbig_2gb_populates_initial_and_tail_scroll_cache() {
    let _guard = large_fixture_test_lock();
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test_assets/vbig-2gb.txt");
    if !path.exists() {
        eprintln!("missing {}", path.display());
        return;
    }
    if skip_large_fixture_if_low_memory(&path) {
        return;
    }

    let expected_text = String::from_utf8(
        Command::new("tail").arg("-n").arg("1").arg(&path).output().unwrap().stdout,
    )
    .unwrap()
    .trim_end_matches('\n')
    .to_owned();

    let mut app = App::from_path(Some(path)).unwrap();
    for _ in 0..80 {
        app.backend.pump().unwrap();
        if app.backend.is_vlf && app.backend.get_line(0).is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(app.backend.is_vlf, "fixture should open in VLF");
    assert!(app.backend.get_line(0).is_some(), "initial viewport should not stay Loading");

    app.last_editor_height = 40;
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE)));

    for _ in 0..120 {
        app.backend.pump().unwrap();
        if !app.backend.pending_vlf_tail_jump
            && app.backend.get_line(app.backend.cursor_line) == Some(expected_text.as_str())
        {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(app.backend.get_line(app.backend.cursor_line), Some(expected_text.as_str()));
    // The tail jump preloads the page above the tail (the cursor's viewport).
    let scroll_up_line = app.backend.cursor_line.saturating_sub(40);
    assert!(
        app.backend.get_line(scroll_up_line).is_some(),
        "tail preload should cover one page above the tail line {scroll_up_line}"
    );
}

#[test]
fn vlf_open_world92_populates_initial_viewport() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test_assets/world92.txt");
    if !path.exists() {
        eprintln!("missing {}", path.display());
        return;
    }

    let expected_text = String::from_utf8(
        Command::new("head").arg("-n").arg("1").arg(&path).output().unwrap().stdout,
    )
    .unwrap()
    .trim_end_matches(['\r', '\n'])
    .to_owned();

    let mut app = App::from_path(Some(path)).unwrap();
    for _ in 0..80 {
        app.backend.pump().unwrap();
        if app.backend.is_vlf && app.backend.get_line(0).is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    assert!(app.backend.is_vlf, "fixture should open in VLF");
    assert_eq!(app.backend.get_line(0), Some(expected_text.as_str()));
}

#[test]
fn vlf_world_fixture_open_populates_first_page_quickly() {
    // Regression: VLF open used to spend seconds before the first page
    // rendered. The core's "nothing changed" shortcut answers the startup
    // scroll with a whole-document copy sized by the *approximate* line count;
    // the window builder materialized those rows before the insert gate, so
    // every open paid O(approximate lines) per update cycle (~1-6 s on the
    // world fixtures). A generous wall-clock bound keeps the regression
    // caught without being flaky on slow machines.
    for name in ["world92.txt", "world03.txt", "world09.txt"] {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test_assets").join(name);
        if !path.exists() {
            eprintln!("missing {}", path.display());
            continue;
        }
        let started = std::time::Instant::now();
        let mut app = App::from_path(Some(path)).unwrap();
        for _ in 0..400 {
            app.backend.pump().unwrap();
            if app.backend.is_vlf && app.backend.get_line(0).is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let elapsed = started.elapsed();
        assert!(app.backend.is_vlf, "{name} should open in VLF");
        assert!(app.backend.get_line(0).is_some(), "{name} first page should populate");
        assert!(
            elapsed < Duration::from_secs(2),
            "{name} first page took {elapsed:?}; expected sub-second"
        );
    }
}

#[test]
fn vlf_world92_page_down_keeps_cursor_monotone() {
    // Regression: paging down near the top snapped the cursor back to line 1.
    // The update window keeps the row-0 caret annotation (copied rows), and
    // the cursor sync chased it; VLF navigation owns the cursor locally.
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test_assets/world92.txt");
    if !path.exists() {
        eprintln!("missing {}", path.display());
        return;
    }
    let mut app = App::from_path(Some(path)).unwrap();
    app.last_editor_height = 40;
    for _ in 0..80 {
        app.backend.pump().unwrap();
        if app.backend.is_vlf && app.backend.get_line(0).is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(app.backend.is_vlf, "fixture should open in VLF");

    let tick = |app: &mut App| {
        app.backend.pump().unwrap();
        app.backend.drain_events().unwrap();
        app.scroll_into_view(40, 100);
        let active = app.backend.active();
        let range = app.folds.line_range_for_rendered_rows(
            active.id,
            app.viewport.top_line,
            40,
            active.line_count(),
        );
        app.backend.notify_scroll(range.0, range.1).unwrap();
    };

    let mut previous = app.backend.cursor_line;
    for press in 1..=6 {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)));
        for _ in 0..20 {
            tick(&mut app);
            thread::sleep(Duration::from_millis(10));
        }
        let cursor = app.backend.cursor_line;
        assert!(
            cursor > previous || (press == 1 && cursor >= previous),
            "page-down {press}: cursor must not snap back (was {previous}, now {cursor})"
        );
        previous = cursor;
    }
    assert_eq!(previous, 120, "six half-page downs from the top land on line 120 (0-based)");
}

#[test]
fn vlf_world92_populates_top_and_line_100_viewports() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test_assets/world92.txt");
    if !path.exists() {
        eprintln!("missing {}", path.display());
        return;
    }

    let mut app = App::from_path(Some(path)).unwrap();
    for _ in 0..80 {
        app.backend.pump().unwrap();
        if app.backend.is_vlf && app.backend.get_line(0).is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(app.backend.is_vlf, "fixture should open in VLF");

    app.backend.notify_scroll(0, 40).unwrap();
    for _ in 0..120 {
        app.backend.pump().unwrap();
        if (0..40).all(|line| app.backend.get_line(line).is_some()) {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let missing_top =
        (0..40).filter(|&line| app.backend.get_line(line).is_none()).collect::<Vec<_>>();
    assert!(missing_top.is_empty(), "missing top VLF lines: {missing_top:?}");

    app.backend.cursor_line = 100;
    app.backend.notify_scroll(100, 140).unwrap();
    for _ in 0..120 {
        app.backend.pump().unwrap();
        if (100..140).all(|line| app.backend.get_line(line).is_some()) {
            break;
        }
        app.backend.notify_scroll(100, 140).unwrap();
        thread::sleep(Duration::from_millis(10));
    }
    let missing_100 =
        (100..140).filter(|&line| app.backend.get_line(line).is_none()).collect::<Vec<_>>();
    assert!(missing_100.is_empty(), "missing line-100 VLF lines: {missing_100:?}");
}

#[test]
fn vlf_world92_tail_scroll_back_populates_full_viewport() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test_assets/world92.txt");
    if !path.exists() {
        eprintln!("missing {}", path.display());
        return;
    }

    let wc = Command::new("wc").arg("-l").arg(&path).output().expect("wc -l should run");
    assert!(wc.status.success(), "wc -l failed: {wc:?}");
    let stdout = String::from_utf8(wc.stdout).expect("wc output should be utf8");
    let expected_line_count = stdout
        .split_whitespace()
        .next()
        .expect("wc output should include count")
        .parse::<usize>()
        .expect("wc count should parse");
    let ends_with_newline = fs::read(&path).unwrap().last().is_some_and(|byte| *byte == b'\n');
    let expected_logical_line_count = expected_line_count + usize::from(ends_with_newline);

    let mut app = App::from_path(Some(path)).unwrap();
    for _ in 0..80 {
        app.backend.pump().unwrap();
        if app.backend.is_vlf && app.backend.get_line(0).is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(app.backend.is_vlf, "fixture should open in VLF");

    app.last_editor_height = 40;
    let tick = |app: &mut App| {
        app.backend.pump().unwrap();
        app.backend.drain_events().unwrap();
        app.scroll_into_view(40, 100);
        let active = app.backend.active();
        let range = app.folds.line_range_for_rendered_rows(
            active.id,
            app.viewport.top_line,
            40,
            active.line_count(),
        );
        app.backend.notify_scroll(range.0, range.1).unwrap();
    };
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE)));
    for _ in 0..600 {
        tick(&mut app);
        if !app.backend.pending_vlf_tail_jump
            && app.backend.cursor_line == expected_logical_line_count.saturating_sub(1)
        {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    assert!(!app.backend.pending_vlf_tail_jump, "tail jump should complete");
    assert_eq!(app.backend.cursor_line, expected_logical_line_count.saturating_sub(1));
    let top = app.backend.cursor_line.saturating_sub(40);
    app.backend.notify_scroll(top, top + 40).unwrap();
    for _ in 0..600 {
        app.backend.pump().unwrap();
        if (top..top + 40).all(|line| app.backend.get_line(line).is_some()) {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    let missing =
        (top..top + 40).filter(|&line| app.backend.get_line(line).is_none()).collect::<Vec<_>>();
    assert!(missing.is_empty(), "missing VLF lines after tail scroll-back: {missing:?}");
}

#[test]
fn vlf_world03_tail_gutter_and_page_up_render() {
    // Bug regressions on the real interactive loop (world03: CRLF, BOM,
    // trailing newline; 279,813 logical lines):
    //   1. after `G`, the tail page must render gutter numbers (the gutter
    //      lookup previously ignored `vlf_cache_start_line`, blanking every
    //      number once the window moved off row 0);
    //   2. one `PageUp` from the tail must not strand rows as Loading (the
    //      window previously shrank to the insert span, dropping the copied
    //      overlap the core re-emits over the client cache).
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test_assets/world03.txt");
    if !path.exists() {
        eprintln!("missing {}", path.display());
        return;
    }

    let mut app = App::from_path(Some(path)).unwrap();
    app.last_editor_height = 40;
    for _ in 0..80 {
        app.backend.pump().unwrap();
        if app.backend.is_vlf && app.backend.get_line(0).is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(app.backend.is_vlf, "fixture should open in VLF");

    let tick = |app: &mut App| {
        app.backend.pump().unwrap();
        app.backend.drain_events().unwrap();
        app.scroll_into_view(40, 100);
        let active = app.backend.active();
        let viewport_range = app.folds.line_range_for_rendered_rows(
            active.id,
            app.viewport.top_line,
            40,
            active.line_count(),
        );
        app.backend.notify_scroll(viewport_range.0, viewport_range.1).unwrap();
    };

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE)));
    for _ in 0..200 {
        tick(&mut app);
        if !app.backend.pending_vlf_tail_jump
            && app.backend.cursor_line > 200_000
            && app.backend.get_line(app.backend.cursor_line).is_some()
        {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(app.backend.cursor_line > 200_000, "goto-end should land on the tail");

    // Bug 1: the tail page renders gutter numbers, including the last row.
    let screen = render_editor_screen(&app, 100, 42);
    let rows: Vec<&str> = screen.lines().collect();
    assert!(rows.len() >= 2, "screen should have editor rows");
    assert!(
        rows.last().is_some_and(|row| row.contains("279813")),
        "last tail row should render its gutter number, got {:?}",
        rows.last()
    );

    // Bug 2: one page up from the tail keeps every viewport row cached.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)));
    for _ in 0..120 {
        tick(&mut app);
        let top = app.viewport.top_line;
        if (0..40).all(|k| app.backend.get_line(top + k).is_some()) {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let top = app.viewport.top_line;
    let missing: Vec<usize> =
        (0..40).filter(|&k| app.backend.get_line(top + k).is_none()).collect();
    assert!(
        missing.is_empty(),
        "missing VLF rows after tail page-up: {missing:?} \
         (top={top} start={} len={} last_scroll={:?} cursor={})",
        app.backend.vlf_cache_start_line,
        app.backend.line_cache.len(),
        app.backend.last_scroll,
        app.backend.cursor_line
    );
    assert!(top > 200_000, "page-up viewport should stay near the tail, got top={top}");
}

#[test]
#[ignore = "manual real-fixture check; requires test_assets/vbig-10gb.txt"]
fn vlf_goto_vbig_10gb_tail_returns_without_full_count() {
    let _guard = large_fixture_test_lock();
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test_assets/vbig-10gb.txt");
    if !path.exists() {
        eprintln!("missing {}", path.display());
        return;
    }
    if skip_large_fixture_if_low_memory(&path) {
        return;
    }

    let expected_text = String::from_utf8(
        Command::new("tail").arg("-n").arg("1").arg(&path).output().unwrap().stdout,
    )
    .unwrap()
    .trim_end_matches('\n')
    .to_owned();

    let mut app = App::from_path(Some(path)).unwrap();
    for _ in 0..80 {
        app.backend.pump().unwrap();
        if app.backend.is_vlf && app.backend.get_line(0).is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(app.backend.is_vlf, "fixture should open in VLF");
    assert!(app.backend.get_line(0).is_some(), "initial viewport should not stay Loading");

    app.last_editor_height = 40;
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE)));

    for _ in 0..120 {
        app.backend.pump().unwrap();
        if !app.backend.pending_vlf_tail_jump
            && app.backend.get_line(app.backend.cursor_line) == Some(expected_text.as_str())
        {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(app.backend.get_line(app.backend.cursor_line), Some(expected_text.as_str()));
    let scroll_up_line = app.backend.cursor_line.saturating_sub(80);
    assert!(
        app.backend.get_line(scroll_up_line).is_some(),
        "tail prefetch should cover nearby scroll-up line {scroll_up_line}"
    );
}

#[test]
fn vlf_visual_line_escape_restores_cursor_without_core_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.cursor_line = 7;
    app.backend.cursor_col = 3;
    app.backend.line_cache = vec![LineSlot::Invalid; 10];
    app.backend.line_cache[7] = LineSlot::Known(CachedLine {
        text: String::from("abcdef"),
        cursors: Vec::new(),
        syntax_spans: Vec::new(),
        logical_line: None,
    });

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('V'), KeyModifiers::NONE)));
    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (7, 0));

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));

    assert_eq!(app.mode, Mode::Normal);
    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (7, 3));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn vlf_visual_line_does_not_send_core_motion() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.cursor_line = 7;
    app.backend.cursor_col = 3;
    app.backend.line_cache = vec![LineSlot::Invalid; 10];

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('V'), KeyModifiers::NONE)));

    assert_eq!(app.mode, Mode::VisualLine);
    assert_eq!(app.visual_anchor, Some((7, 0)));
    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (7, 0));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}
