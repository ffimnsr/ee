use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;

use crate::app::{App, Mode};
use crate::backend::{BackendEvent, CachedLine, CoreSyntaxSpan, LineSlot};
use crate::buffer::BufferManager;
use crate::tests::helpers::*;

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
        }),
        LineSlot::Known(CachedLine {
            text: String::from("beta"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
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
        }),
        LineSlot::Known(CachedLine {
            text: String::from("beta"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
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
    assert_eq!(second["params"]["method"], "vlf_viewport");
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
        }),
        LineSlot::Known(CachedLine {
            text: String::from("beta"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
        LineSlot::Known(CachedLine {
            text: String::from("gamma"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
        LineSlot::Known(CachedLine {
            text: String::from("delta"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
        LineSlot::Known(CachedLine {
            text: String::from("epsilon"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
        LineSlot::Known(CachedLine {
            text: String::from("zeta"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
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
    assert_eq!(second["params"]["method"], "vlf_viewport");
    assert_eq!(second["params"]["params"]["line_start"], 40);
    assert_eq!(second["params"]["params"]["line_end"], 46);
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
        }),
        LineSlot::Known(CachedLine {
            text: String::from("beta"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
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
    assert_eq!(second["params"]["method"], "vlf_viewport");
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
        }),
        LineSlot::Known(CachedLine {
            text: String::from("ta"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
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
    assert_eq!(second["params"]["method"], "vlf_viewport");
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
        }),
        LineSlot::Known(CachedLine {
            text: String::from("ta"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
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
    assert_eq!(second["params"]["method"], "vlf_viewport");
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

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)));

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
fn vlf_goto_last_line_requests_tail_viewport_when_count_is_approximate() {
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
        .expect("tail viewport request should be json");
    assert_eq!(message["params"]["method"], "vlf_viewport");
    assert_eq!(message["params"]["params"]["line_start"], u64::MAX);
    assert_eq!(message["params"]["params"]["line_end"], 4095);
    assert!(app.backend.pending_vlf_tail_jump);
}

#[test]
fn vlf_navigation_away_from_pending_tail_jump_cancels_tail_request() {
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
            .expect("tail viewport request should be json");
    assert_eq!(tail_request["params"]["params"]["line_start"], u64::MAX);
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
    assert_eq!(top_request["params"]["method"], "vlf_viewport");
    assert_eq!(top_request["params"]["params"]["line_start"], 0);
    assert_eq!(top_request["params"]["params"]["line_end"], 240);
}

#[test]
fn vlf_pending_tail_jump_blocks_regular_viewport_scroll() {
    let (tx, rx) = mpsc::channel();
    let (backend_tx, backend_rx) = mpsc::channel();
    let mut mgr = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    mgr.is_vlf = true;
    mgr.vlf_approx_line_count = 10_000;

    mgr.request_vlf_tail_viewport(40).unwrap();
    let first: Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).unwrap())
        .expect("tail viewport request should be json");
    assert_eq!(first["params"]["params"]["line_start"], u64::MAX);

    mgr.notify_scroll(9_950, 9_990).unwrap();
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    assert!(mgr.pending_vlf_tail_jump);

    let tail_lines = (0..40).map(|idx| format!("tail {idx}")).collect::<Vec<_>>();
    backend_tx
        .send(BackendEvent::VlfChunks {
            view_id: String::from("view-id-1"),
            generation: 1,
            line_start: 9_960,
            lines: tail_lines,
            syntax_spans: Vec::new(),
            approximate_line_count: 10_000,
            line_count_exact: false,
            index_progress: 0.1,
        })
        .unwrap();
    mgr.drain_events().unwrap();

    assert!(!mgr.pending_vlf_tail_jump);
    assert_eq!(mgr.cursor_line, 9_999);
    mgr.notify_scroll(9_960, 10_000).unwrap();
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn vlf_completed_tail_jump_then_top_restores_cached_top_viewport() {
    let (tx, rx) = mpsc::channel();
    let (backend_tx, backend_rx) = mpsc::channel();
    let mut mgr = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    mgr.is_vlf = true;
    mgr.vlf_approx_line_count = 10_000;
    mgr.line_cache = (0..240)
        .map(|line| {
            LineSlot::Known(CachedLine {
                text: format!("top {line}"),
                cursors: Vec::new(),
                syntax_spans: Vec::new(),
            })
        })
        .collect();

    mgr.request_vlf_tail_viewport(40).unwrap();
    let first: Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).unwrap())
        .expect("tail viewport request should be json");
    assert_eq!(first["params"]["params"]["line_start"], u64::MAX);

    let tail_lines = (0..40).map(|idx| format!("tail {idx}")).collect::<Vec<_>>();
    backend_tx
        .send(BackendEvent::VlfChunks {
            view_id: String::from("view-id-1"),
            generation: 1,
            line_start: 9_960,
            lines: tail_lines,
            syntax_spans: Vec::new(),
            approximate_line_count: 10_000,
            line_count_exact: false,
            index_progress: 0.1,
        })
        .unwrap();
    mgr.drain_events().unwrap();

    assert_eq!(mgr.vlf_cache_start_line, 9_960);
    assert_eq!(mgr.get_line(9_960), Some("tail 0"));

    mgr.cursor_line = 0;
    mgr.notify_scroll(0, 40).unwrap();

    assert_eq!(mgr.vlf_cache_start_line, 0);
    assert_eq!(mgr.get_line(0), Some("top 0"));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn vlf_startup_pump_requests_initial_viewport_after_document_mode() {
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
        &rx.recv_timeout(Duration::from_secs(1)).expect("initial VLF viewport should be sent"),
    )
    .expect("viewport request should be json");
    assert_eq!(message["params"]["method"], "vlf_viewport");
    assert_eq!(message["params"]["params"]["line_start"], 0);
    assert_eq!(message["params"]["params"]["line_end"], 200);
    assert!(mgr.pending_line_request);

    fs::remove_file(path).unwrap();
}

#[test]
#[ignore = "manual real-fixture check; requires test_assets/vbig-100.txt"]
fn vlf_goto_vbig_100_matches_wc_last_line() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test_assets/vbig-100.txt");
    if !path.exists() {
        eprintln!("missing {}", path.display());
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
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test_assets/vbig-2gb.txt");
    if !path.exists() {
        eprintln!("missing {}", path.display());
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
#[ignore = "manual real-fixture check; requires test_assets/world92.txt"]
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
#[ignore = "manual real-fixture check; requires test_assets/world92.txt"]
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
#[ignore = "manual real-fixture check; requires test_assets/world92.txt"]
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
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE)));
    for _ in 0..120 {
        app.backend.pump().unwrap();
        if !app.backend.pending_vlf_tail_jump && app.backend.cursor_line > 65_000 {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    assert!(!app.backend.pending_vlf_tail_jump, "tail jump should complete");
    assert_eq!(app.backend.cursor_line, expected_logical_line_count.saturating_sub(1));
    let top = app.backend.cursor_line.saturating_sub(40);
    app.backend.notify_scroll(top, top + 40).unwrap();
    for _ in 0..120 {
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
#[ignore = "manual real-fixture check; requires test_assets/vbig-10gb.txt"]
fn vlf_goto_vbig_10gb_tail_returns_without_full_count() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test_assets/vbig-10gb.txt");
    if !path.exists() {
        eprintln!("missing {}", path.display());
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
