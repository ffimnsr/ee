use std::sync::mpsc::{self, TryRecvError};
use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};

use crate::app::App;
use crate::backend::update::{CoreLine, VlfTotalLines};
use crate::backend::{
    BackendEvent, CachedLine, CoreUpdate, CoreUpdateKind, CoreUpdateOp, LineSlot,
    coalesce_backend_events, invalid_line_ranges_bounded, parse_notification,
};
use crate::buffer::BufferManager;
use crate::tests::helpers::*;
use crate::ui;

// ── Render benchmarks (large-file rendering must stay within one frame) ─────

#[test]
fn render_300k_line_fixture_under_one_frame_budget() {
    const LINES: usize = 300_000;
    const FRAME_BUDGET_MS: u128 = 50; // 3× 60 Hz frame; avoids CI flake

    let lines = fixture::many_line_fixture(LINES, 30);
    let elapsed = timed_render(lines);

    assert!(
        elapsed.as_millis() < FRAME_BUDGET_MS,
        "render of {LINES} lines took {}ms, expected < {FRAME_BUDGET_MS}ms \
         (possible full-buffer Vec<String> clone in render path)",
        elapsed.as_millis()
    );
}

#[test]
fn render_long_line_fixture_under_one_frame_budget() {
    const LINES: usize = 300_000;
    const LINE_LEN: usize = 200;
    const FRAME_BUDGET_MS: u128 = 50;

    let lines = fixture::long_line_fixture(LINES, LINE_LEN);
    let elapsed = timed_render(lines);

    assert!(
        elapsed.as_millis() < FRAME_BUDGET_MS,
        "render of {LINES} long-line fixture took {}ms, expected < {FRAME_BUDGET_MS}ms",
        elapsed.as_millis()
    );
}

#[test]
fn render_single_very_long_ascii_line_under_budget() {
    const LINE_LEN: usize = 1_000_000;
    const FRAME_BUDGET_MS: u128 = 50;

    let mut app = App::from_path(None).unwrap();
    app.backend.lines = vec!["3".repeat(LINE_LEN)];
    app.viewport.left_col = 100_000;

    let backend = TestBackend::new(120, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    let start = std::time::Instant::now();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_millis() < FRAME_BUDGET_MS,
        "render of {LINE_LEN}-byte line took {}ms, expected < {FRAME_BUDGET_MS}ms",
        elapsed.as_millis()
    );
}

#[test]
fn render_mixed_crlf_fixture_under_one_frame_budget() {
    const LINES: usize = 300_000;
    const FRAME_BUDGET_MS: u128 = 50;

    let lines = fixture::mixed_crlf_fixture(LINES);
    let elapsed = timed_render(lines);

    assert!(
        elapsed.as_millis() < FRAME_BUDGET_MS,
        "render of {LINES} mixed-CRLF fixture took {}ms, expected < {FRAME_BUDGET_MS}ms",
        elapsed.as_millis()
    );
}

/// Regression: rendering a 300 K LOC source-like fixture must stay within budget.
#[test]
fn render_source_fixture_under_one_frame_budget() {
    const LINES: usize = 300_000;
    const FRAME_BUDGET_MS: u128 = 50;

    let lines = fixture::source_fixture(LINES);
    let elapsed = timed_render(lines);

    assert!(
        elapsed.as_millis() < FRAME_BUDGET_MS,
        "render of {LINES}-line source fixture took {}ms, expected < {FRAME_BUDGET_MS}ms",
        elapsed.as_millis()
    );
}

// ── VLF update-window protocol (Stage A Phase 3) ───────────────────────────

fn vlf_update(ops: Vec<CoreUpdateOp>, count: u64, exact: bool) -> CoreUpdate {
    CoreUpdate {
        blob: None,
        ops,
        pristine: true,
        annotations: Vec::new(),
        vlf_total_lines: Some(VlfTotalLines { count, exact, index_progress: 1.0 }),
        scopes: Vec::new(),
    }
}

fn inserted(text: &str, ln: usize) -> CoreUpdateOp {
    CoreUpdateOp {
        blob: false,
        op: CoreUpdateKind::Insert,
        n: 1,
        lines: vec![CoreLine {
            text: Some(text.to_owned()),
            cursor: Vec::new(),
            spans: Some(Vec::new()),
            logical_line: Some(ln),
        }],
    }
}

fn inserted_row(text: &str, ln: Option<usize>) -> CoreUpdateOp {
    CoreUpdateOp {
        blob: false,
        op: CoreUpdateKind::Insert,
        n: 1,
        lines: vec![CoreLine {
            text: Some(text.to_owned()),
            cursor: Vec::new(),
            spans: Some(Vec::new()),
            logical_line: ln,
        }],
    }
}

#[test]
fn vlf_wrapped_window_slots_dense_rows_past_heads() {
    // Wrapped VLF (Phase 4): continuation rows omit `ln` — only logical-line
    // heads carry it. The window must slot dense display rows; a naive ln
    // fallback would collide continuations with the next head and drop rows.
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.line_cache = vec![LineSlot::Invalid; 3];

    buf.apply_update(vlf_update(
        vec![
            inserted_row("alpha ", Some(0)),
            inserted_row("alpha 2", None),
            inserted_row("beta\n", Some(1)),
            inserted_row("gamma x", Some(2)),
            inserted_row("gamma y", None),
        ],
        3,
        true,
    ))
    .unwrap();

    assert_eq!(buf.vlf_cache_start_line, 0);
    assert_eq!(buf.get_line(0), Some("alpha "));
    assert_eq!(buf.get_line(1), Some("alpha 2"));
    assert_eq!(buf.get_line(2), Some("beta"));
    assert_eq!(buf.get_line(3), Some("gamma x"));
    assert_eq!(buf.get_line(4), Some("gamma y"));
}

#[test]
fn vlf_wrapped_window_anchors_at_mid_file_head() {
    // Dense-row slotting when a wrapped window starts mid-file: anchored at
    // the first head's ln, rows fill sequentially after it.
    let mut buf = test_buf_state();
    buf.is_vlf = true;

    buf.apply_update(vlf_update(
        vec![
            inserted_row("line 10 ", Some(10)),
            inserted_row("line 10 cont", None),
            inserted_row("line 11\n", Some(11)),
        ],
        1000,
        false,
    ))
    .unwrap();

    assert_eq!(buf.vlf_cache_start_line, 10);
    assert_eq!(buf.get_line(10), Some("line 10 "));
    assert_eq!(buf.get_line(11), Some("line 10 cont"));
    assert_eq!(buf.get_line(12), Some("line 11"));
}

#[test]
fn rejected_vlf_update_keeps_the_window() {
    // A malformed insert must not empty the bounded window. The shared VLF
    // helper restores the previous cache on rejection, so the next payload —
    // which the core builds assuming the client still holds those rows — applies
    // cleanly instead of failing against an empty window forever.
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.apply_update(vlf_update(vec![inserted("alpha", 0), inserted("beta", 1)], 2, true)).unwrap();
    let before = buf.line_cache.clone();
    assert_eq!(buf.line_cache.len(), 2);

    let rejected = buf.apply_update(vlf_update(
        vec![CoreUpdateOp {
            blob: false,
            op: CoreUpdateKind::Insert,
            n: 1,
            lines: vec![CoreLine {
                text: Some(String::from("gamma")),
                cursor: Vec::new(),
                // Scope id 9 against an empty table: rejected on decode.
                spans: Some(vec![0, 3, 9]),
                logical_line: Some(2),
            }],
        }],
        3,
        true,
    ));
    assert!(rejected.is_err(), "unknown scope id must reject the payload");
    assert_eq!(buf.line_cache, before, "a rejected payload leaves the window intact");
    assert_eq!(buf.get_line(0), Some("alpha"), "window rows stay readable");

    buf.apply_update(vlf_update(
        vec![inserted("alpha", 0), inserted("beta", 1), inserted("gamma", 2)],
        3,
        true,
    ))
    .unwrap();
    assert_eq!(buf.line_cache.len(), 3);
}

#[test]
fn vlf_update_window_populates_line_cache() {
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.line_cache = vec![LineSlot::Invalid; 3];

    buf.apply_update(vlf_update(vec![inserted("alpha", 0), inserted("beta", 1)], 3, true)).unwrap();

    assert_eq!(buf.vlf_cache_start_line, 0);
    assert_eq!(
        buf.line_slot(0).cloned().unwrap(),
        LineSlot::from_core_line(
            CoreLine {
                text: Some(String::from("alpha")),
                cursor: vec![],
                spans: Some(Vec::new()),
                logical_line: Some(0),
            },
            &[],
            None,
        )
        .unwrap()
    );
    assert_eq!(buf.get_line(0), Some("alpha"));
    assert_eq!(buf.get_line(1), Some("beta"));
    assert_eq!(buf.vlf_approx_line_count, 3);
    assert!(buf.vlf_line_count_exact);
}

#[test]
fn vlf_update_window_positions_at_insert_ln() {
    // The window starts at the first inserted line's logical number, not 0:
    // scrolling down replaces the window with the new visible range.
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.line_cache = vec![LineSlot::Invalid; 2];

    buf.apply_update(vlf_update(
        vec![inserted("line 10", 10), inserted("line 11", 11)],
        1000,
        false,
    ))
    .unwrap();

    assert_eq!(buf.vlf_cache_start_line, 10);
    assert_eq!(buf.get_line(10), Some("line 10"));
    assert_eq!(buf.get_line(11), Some("line 11"));
    assert_eq!(buf.line_cache.len(), 2);
    assert_eq!(buf.line_count(), 1000, "cache stays window-local");
}

#[test]
fn vlf_update_normalizes_crlf_line_endings() {
    let mut buf = test_buf_state();
    buf.is_vlf = true;

    // Core line text carries the terminator (rope-parity wire shape); the
    // frontend strips `\r\n` like every other update line.
    buf.apply_update(vlf_update(vec![inserted("alpha\r\n", 0), inserted("beta\r\n", 1)], 2, true))
        .unwrap();

    assert_eq!(buf.get_line(0), Some("alpha"));
    assert_eq!(buf.get_line(1), Some("beta"));
}

#[test]
fn vlf_update_without_insert_keeps_window_but_refreshes_count() {
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.vlf_cache_start_line = 40;
    buf.line_cache = vec![LineSlot::Known(CachedLine {
        text: String::from("line 40"),
        cursors: vec![],
        syntax_spans: vec![],
        logical_line: Some(40),
    })];

    buf.apply_update(vlf_update(Vec::new(), 1_000, false)).unwrap();

    assert_eq!(buf.vlf_cache_start_line, 40);
    assert_eq!(buf.get_line(40), Some("line 40"));
    assert_eq!(buf.vlf_approx_line_count, 1_000);
}

#[test]
fn vlf_update_keeps_tail_jump_pending_until_window_lands() {
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.pending_vlf_tail_jump = true;

    // Copy-only update (no insert): tail not here yet; jump stays pending.
    buf.apply_update(vlf_update(Vec::new(), 10_000, false)).unwrap();
    assert!(buf.pending_vlf_tail_jump);

    // Inexact window lands: the cursor follows the returned tail while the
    // jump stays pending (the index can still move the true end).
    buf.apply_update(vlf_update(
        vec![inserted("line 998\n", 998), inserted("line 999\n", 999)],
        10_000,
        false,
    ))
    .unwrap();

    assert!(buf.pending_vlf_tail_jump, "inexact tail must not settle the jump");
    assert_eq!((buf.cursor_line, buf.cursor_col), (999, 0));

    // Exact count settles the jump.
    buf.apply_update(vlf_update(
        vec![inserted("line 9998\n", 9998), inserted("line 9999\n", 9999)],
        10_000,
        true,
    ))
    .unwrap();

    assert!(!buf.pending_vlf_tail_jump);
    assert_eq!((buf.cursor_line, buf.cursor_col), (9999, 0));
}

#[test]
fn vlf_update_gap_between_inserts_stays_invalid() {
    let mut buf = test_buf_state();
    buf.is_vlf = true;

    buf.apply_update(vlf_update(vec![inserted("head", 0), inserted("tail", 3)], 4, true)).unwrap();

    assert_eq!(buf.vlf_cache_start_line, 0);
    assert_eq!(buf.get_line(0), Some("head"));
    assert!(matches!(buf.line_slot(2), Some(LineSlot::Invalid)), "gap stays invalid");
    assert_eq!(buf.get_line(3), Some("tail"));
}

#[test]
fn vlf_copy_only_update_keeps_window_and_cursor() {
    // Bug regression: the core's "nothing changed" shortcut answers a scroll
    // with a whole-document copy. The bounded window must stay exactly as-is
    // (including the cursor, which VLF navigation owns locally) instead of
    // rebuilding slots from stale rows.
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.vlf_cache_start_line = 40;
    buf.cursor_line = 52;
    buf.line_cache = vec![LineSlot::Known(CachedLine {
        text: String::from("line 40"),
        cursors: vec![0], // would re-anchor the cursor if sync ran
        syntax_spans: vec![],
        logical_line: Some(40),
    })];

    buf.apply_update(vlf_update(
        vec![CoreUpdateOp { blob: false, op: CoreUpdateKind::Copy, n: 1, lines: Vec::new() }],
        1_000,
        false,
    ))
    .unwrap();

    assert_eq!((buf.cursor_line, buf.cursor_col), (52, 0), "copy-only reframe keeps cursor");
    assert_eq!(buf.vlf_cache_start_line, 40);
    assert_eq!(buf.get_line(40), Some("line 40"));
}

#[test]
fn vlf_mixed_insert_copy_window_keeps_copied_rows() {
    // Bug regression: a scroll near the tail re-renders only the changed
    // span; the core copies the overlap back from the client cache. The new
    // window must keep those copied rows at their absolute positions instead
    // of shrinking to the insert span, which stranded the viewport above the
    // tail as Loading rows (core shadow then claims them cached, so the
    // frontend stops re-requesting).
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.vlf_cache_start_line = 10;
    buf.line_cache = vec![
        LineSlot::Known(CachedLine {
            text: String::from("line 10"),
            cursors: vec![],
            syntax_spans: vec![],
            logical_line: Some(10),
        }),
        LineSlot::Known(CachedLine {
            text: String::from("line 11"),
            cursors: vec![],
            syntax_spans: vec![],
            logical_line: Some(11),
        }),
        LineSlot::Known(CachedLine {
            text: String::from("line 12"),
            cursors: vec![],
            syntax_spans: vec![],
            logical_line: Some(12),
        }),
    ];

    // Insert the newly rendered span, then copy the 3-row overlap back.
    buf.apply_update(vlf_update(
        vec![
            inserted("line 8", 8),
            inserted("line 9", 9),
            CoreUpdateOp { blob: false, op: CoreUpdateKind::Copy, n: 3, lines: Vec::new() },
        ],
        100,
        true,
    ))
    .unwrap();

    assert_eq!(buf.vlf_cache_start_line, 8);
    assert_eq!(buf.get_line(8), Some("line 8"));
    assert_eq!(buf.get_line(9), Some("line 9"));
    assert_eq!(buf.get_line(10), Some("line 10"), "copied overlap stays cached");
    assert_eq!(buf.get_line(11), Some("line 11"));
    assert_eq!(buf.get_line(12), Some("line 12"));
    assert_eq!(buf.line_cache.len(), 5, "window covers insert span plus copied overlap");
}

#[test]
fn vlf_window_trims_to_insert_span_with_overscan() {
    // Bug regression: copies re-emit rows the client holds, and unbounded
    // keeping grew the window toward the whole file (every later apply and
    // teardown became O(document)). The window must stay bounded around the
    // freshly rendered insert span.
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.vlf_cache_start_line = 9_000;
    buf.line_cache = (9_000..11_000)
        .map(|line| {
            LineSlot::Known(CachedLine {
                text: format!("old {line}"),
                cursors: Vec::new(),
                syntax_spans: Vec::new(),
                logical_line: Some(line),
            })
        })
        .collect();

    // Fresh render at 10_000..10_040 plus a copy re-assert over the overlap.
    let mut ops: Vec<CoreUpdateOp> = (10_000..10_040).map(|ln| inserted("new", ln)).collect();
    ops.push(CoreUpdateOp { blob: false, op: CoreUpdateKind::Copy, n: 2_000, lines: Vec::new() });
    buf.apply_update(vlf_update(ops, 20_000, true)).unwrap();

    assert_eq!(buf.vlf_cache_start_line, 10_000, "window anchors at the render span");
    assert_eq!(buf.line_cache.len(), 40 + 512, "copies keep only the overscan tail");
    assert_eq!(buf.get_line(10_000), Some("new"), "fresh rows stay cached");
    assert_eq!(buf.get_line(10_040), Some("old 10040"), "copied overlap keeps old content");
    assert_eq!(buf.get_line(10_551), Some("old 10551"), "overscan below stays cached");
    assert!(buf.get_line(10_552).is_none(), "copies beyond the overscan are dropped");
}

#[test]
fn vlf_document_mode_clears_stale_normal_cache_and_scrolls() {
    let (tx, rx) = mpsc::channel();
    let (backend_tx, backend_rx) = mpsc::channel();
    let mut mgr = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    mgr.lines = vec![String::from("stale normal line")];

    backend_tx
        .send(BackendEvent::DocumentMode { view_id: String::from("view-id-1"), is_vlf: true })
        .unwrap();
    mgr.drain_events().unwrap();
    assert!(mgr.active().is_vlf);
    assert!(mgr.active().lines.is_empty());

    mgr.notify_scroll(0, 4).unwrap();
    let first: Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).unwrap())
        .expect("scroll notification should be json");
    assert_eq!(first["params"]["method"], "scroll");
    assert_eq!(first["params"]["params"], json!([0, 4]));

    // Same range dedupes: no second scroll.
    mgr.notify_scroll(0, 4).unwrap();
    assert!(matches!(
        rx.recv_timeout(Duration::from_millis(50)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));

    // Copy-only update leaves the window untouched (no request line spam).
    backend_tx
        .send(BackendEvent::Update {
            view_id: String::from("view-id-1"),
            update: vlf_update(Vec::new(), 1000, false),
        })
        .unwrap();
    mgr.drain_events().unwrap();
    assert_eq!(mgr.active().vlf_approx_line_count, 1000);
}

#[test]
fn vlf_notify_scroll_sends_scroll_without_overscan() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut mgr = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    mgr.is_vlf = true;
    mgr.vlf_cache_start_line = 0;
    mgr.vlf_approx_line_count = 10_000;
    mgr.line_cache = (0..4)
        .map(|line| {
            LineSlot::Known(CachedLine {
                text: format!("line {line}"),
                cursors: Vec::new(),
                syntax_spans: Vec::new(),
                logical_line: None,
            })
        })
        .collect();

    mgr.notify_scroll(0, 4).unwrap();
    let scroll: Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).unwrap())
        .expect("scroll notification should be json");

    assert_eq!(scroll["params"]["method"], "scroll");
    assert_eq!(scroll["params"]["params"], json!([0, 4]));
}

#[test]
fn vlf_invalid_cache_does_not_request_normal_lines() {
    let (tx, rx) = mpsc::channel();
    let (backend_tx, backend_rx) = mpsc::channel();
    let mut mgr = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    backend_tx
        .send(BackendEvent::DocumentMode { view_id: String::from("view-id-1"), is_vlf: true })
        .unwrap();
    backend_tx
        .send(BackendEvent::Update {
            view_id: String::from("view-id-1"),
            update: vlf_update(vec![inserted("alpha\n", 0)], 1000, false),
        })
        .unwrap();
    mgr.drain_events().unwrap();

    mgr.pump().unwrap();
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn vlf_git_diff_command_reports_clear_status() {
    let mut app = App::from_path(None).unwrap();
    app.backend.is_vlf = true;

    run_ex(&mut app, "gdiff");

    assert_eq!(
        app.backend.status_message.as_deref(),
        Some("git diff disabled in VLF: requires whole-buffer diff/blame scans")
    );
}

#[test]
fn vlf_applies_insert_updates_after_document_mode() {
    let (tx, _rx) = mpsc::channel();
    let (backend_tx, backend_rx) = mpsc::channel();
    let mut mgr = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    backend_tx
        .send(BackendEvent::DocumentMode { view_id: String::from("view-id-1"), is_vlf: true })
        .unwrap();
    backend_tx
        .send(BackendEvent::Update {
            view_id: String::from("view-id-1"),
            update: vlf_update(vec![inserted("frame line\n", 0)], 50, true),
        })
        .unwrap();

    mgr.drain_events().unwrap();
    assert!(mgr.active().is_vlf);
    assert_eq!(
        mgr.active().line_slot(0).cloned().unwrap(),
        LineSlot::Known(CachedLine {
            text: String::from("frame line"),
            cursors: vec![],
            syntax_spans: vec![],
            logical_line: Some(0),
        })
    );
}

// ── Backend event parsing ──────────────────────────────────────────────────

#[test]
fn update_with_vlf_total_lines_backend_event_parsed() {
    let params = json!({
        "view_id": "view-1",
        "update": {
            "ops": [],
            "pristine": true,
            "vlf_total_lines": { "count": 500, "exact": false, "index_progress": 0.42 },
        },
    });
    let event = parse_notification("update", params).expect("should parse update");
    match event {
        BackendEvent::Update { view_id, update } => {
            assert_eq!(view_id, "view-1");
            let total = update.vlf_total_lines.expect("vlf_total_lines present");
            assert_eq!(total.count, 500);
            assert!(!total.exact);
        }
        other => panic!("expected Update, got {:?}", other),
    }
}

#[test]
fn coalesce_backend_events_keeps_latest_noisy_view_events() {
    let events = vec![
        BackendEvent::VlfSearchStatus {
            view_id: String::from("view-1"),
            query: String::from("needle"),
            scanned_bytes: 10,
            total_bytes: 100,
            complete: false,
            stored_match_count: 1,
            ranges: Vec::new(),
        },
        BackendEvent::DocumentMode { view_id: String::from("view-1"), is_vlf: true },
        BackendEvent::VlfSearchStatus {
            view_id: String::from("view-1"),
            query: String::from("needle"),
            scanned_bytes: 100,
            total_bytes: 100,
            complete: true,
            stored_match_count: 4,
            ranges: Vec::new(),
        },
    ];

    let coalesced = coalesce_backend_events(events);

    assert_eq!(coalesced.len(), 2);
    assert!(matches!(&coalesced[0], BackendEvent::DocumentMode { is_vlf: true, .. }));
    match &coalesced[1] {
        BackendEvent::VlfSearchStatus { complete, scanned_bytes, .. } => {
            assert!(*complete);
            assert_eq!(*scanned_bytes, 100);
        }
        other => panic!("expected latest search status, got {other:?}"),
    }
}

#[test]
fn vlf_search_status_backend_event_parsed() {
    let params = json!({
        "view_id": "view-1",
        "query": "needle",
        "scanned_bytes": 1024,
        "total_bytes": 4096,
        "complete": false,
        "stored_match_count": 2,
        "ranges": [
            { "line": 3, "start_col": 2, "end_col": 8 },
            { "line": 7, "start_col": 0, "end_col": 6 }
        ]
    });
    let event =
        parse_notification("vlf_search_status", params).expect("should parse vlf search status");
    match event {
        BackendEvent::VlfSearchStatus {
            view_id,
            query,
            scanned_bytes,
            total_bytes,
            complete,
            stored_match_count,
            ranges,
        } => {
            assert_eq!(view_id, "view-1");
            assert_eq!(query, "needle");
            assert_eq!(scanned_bytes, 1024);
            assert_eq!(total_bytes, 4096);
            assert!(!complete);
            assert_eq!(stored_match_count, 2);
            assert_eq!(ranges.len(), 2);
            assert_eq!(ranges[0].line, 3);
            assert_eq!(ranges[0].start_col, 2);
            assert_eq!(ranges[0].end_col, 8);
        }
        other => panic!("expected VlfSearchStatus, got {:?}", other),
    }
}

// ── Regression counters/tests: no full line-cache clone on hot paths ──────────

#[test]
fn source_control_skips_constrained_sized_buffers() {
    // Buffers with more than CONSTRAINED_GIT_REFRESH_MAX_LINES (50_000) lines
    // must be skipped by the periodic background refresh to avoid an expensive
    // whole-buffer clone + diff on the UI thread.
    let (tx, _rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    let buf_id = app.backend.active().id;

    // Build a fully-cached line cache above the constrained threshold.
    let line_count = 50_001;
    app.backend.line_cache = (0..line_count)
        .map(|i| {
            LineSlot::Known(CachedLine {
                text: format!("line {i}"),
                cursors: Vec::new(),
                syntax_spans: Vec::new(),
                logical_line: None,
            })
        })
        .collect();
    app.backend.rebuild_lines();
    assert_eq!(app.backend.lines.len(), line_count);
    assert!(app.backend.is_fully_cached());

    // No source-control entry yet — periodic refresh should still skip it.
    app.refresh_source_control();

    assert!(
        !app.source_control.contains_key(&buf_id),
        "background refresh must not clone or diff a constrained-sized buffer"
    );
}

#[test]
fn source_control_skips_loading_buffer_with_empty_cache() {
    // Regression: `all()` on an empty line cache is vacuously true, so the
    // first deferred refresh could run while a buffer was still loading and
    // diff padded-empty lines against the HEAD blob — painting phantom git
    // signs (`-`) on a clean file.  An empty cache must not count as fully
    // cached.
    let (tx, _rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    let buf_id = app.backend.active().id;
    assert!(app.backend.line_cache.is_empty());

    assert!(
        !app.backend.is_fully_cached(),
        "an empty (loading) cache must not be treated as fully cached"
    );
    app.refresh_source_control();
    assert!(
        !app.source_control.contains_key(&buf_id),
        "background refresh must skip a buffer that is still loading"
    );
}

#[test]
fn apply_update_large_cache_insert_does_not_clone_non_copy_range() {
    // Prove that a Copy op over a large prefix followed by an Insert only
    // allocates what is actually needed: the copy range and the new line.
    // The whole line_cache length must match the op total exactly.
    let large_line_count = 60_000usize;
    let mut state = test_buf_state();
    state.line_cache = (0..large_line_count)
        .map(|i| {
            LineSlot::Known(CachedLine {
                text: format!("existing {i}"),
                cursors: Vec::new(),
                syntax_spans: Vec::new(),
                logical_line: None,
            })
        })
        .collect();
    state.rebuild_lines();

    state
        .apply_update(CoreUpdate {
            blob: None,
            pristine: true,
            vlf_total_lines: None,
            annotations: Vec::new(),
            scopes: Vec::new(),
            ops: vec![
                // Copy entire existing cache — must not scan non-copy lines.
                CoreUpdateOp {
                    blob: false,
                    op: CoreUpdateKind::Copy,
                    n: large_line_count,
                    lines: Vec::new(),
                },
                // Append one new line.
                CoreUpdateOp {
                    blob: false,
                    op: CoreUpdateKind::Insert,
                    n: 1,
                    lines: vec![CoreLine {
                        text: Some(String::from("new-tail")),
                        cursor: Vec::new(),
                        spans: None,
                        logical_line: None,
                    }],
                },
            ],
        })
        .unwrap();

    assert_eq!(state.line_cache.len(), large_line_count + 1);
    // Existing lines must be preserved through the copy.
    match &state.line_cache[0] {
        LineSlot::Known(l) => assert_eq!(l.text, "existing 0"),
        other => panic!("expected known slot at 0, got {other:?}"),
    }
    // New line must appear at the tail.
    match &state.line_cache[large_line_count] {
        LineSlot::Known(l) => assert_eq!(l.text, "new-tail"),
        other => panic!("expected known slot at tail, got {other:?}"),
    }
    assert_eq!(state.lines.len(), large_line_count + 1);
    assert_eq!(state.lines[large_line_count], "new-tail");
}

#[test]
fn invalidate_op_large_count_does_not_allocate_text() {
    // Ensure that an Invalidate op for a huge line range produces Invalid
    // slots with no text allocation — the `lines` mirror gets empty strings,
    // but the slot type itself must be Invalid (no text cloned from previous).
    let mut state = test_buf_state();

    state
        .apply_update(CoreUpdate {
            blob: None,
            pristine: true,
            vlf_total_lines: None,
            annotations: Vec::new(),
            scopes: Vec::new(),
            ops: vec![CoreUpdateOp {
                blob: false,
                op: CoreUpdateKind::Invalidate,
                n: 100_000,
                lines: Vec::new(),
            }],
        })
        .unwrap();

    assert_eq!(state.line_cache.len(), 100_000);
    assert!(
        state.line_cache.iter().all(|s| matches!(s, LineSlot::Invalid)),
        "all slots from Invalidate op must be Invalid"
    );
    // The lines mirror has empty strings for invalid slots — no content.
    assert!(state.lines.iter().all(|s| s.is_empty()));
}

#[test]
fn bounded_invalid_range_scan_stops_at_window_boundary() {
    // invalid_line_ranges_bounded must not iterate outside [start, end).
    // This is the primitive that keeps scroll from scanning the full cache.
    let cache_size = 10_000usize;
    let viewport_start = 4_000usize;
    let viewport_end = 4_050usize;

    let mut cache = vec![LineSlot::Invalid; cache_size];
    // Mark all lines outside the viewport as Known — they must never appear
    // in the returned ranges.
    for slot in &mut cache[..viewport_start] {
        *slot = LineSlot::Known(CachedLine {
            text: String::from("before"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
            logical_line: None,
        });
    }
    for slot in &mut cache[viewport_end..] {
        *slot = LineSlot::Known(CachedLine {
            text: String::from("after"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
            logical_line: None,
        });
    }

    let ranges = invalid_line_ranges_bounded(&cache, viewport_start, viewport_end);

    // The entire viewport window is invalid, so exactly one range covers it.
    assert_eq!(ranges, vec![(viewport_start, viewport_end)]);
    // No range must extend outside the requested window.
    for (start, end) in &ranges {
        assert!(*start >= viewport_start, "range started before viewport");
        assert!(*end <= viewport_end, "range extended past viewport");
    }
}

#[test]
fn normal_render_large_line_cache_only_displays_viewport_rows() {
    // Prove that rendering a buffer with a large line cache only shows lines
    // that fit in the terminal height — the render path must not expand all
    // Invalid slots or panic on a huge cache.
    let total_lines = 10_000usize;
    let width: u16 = 80;
    let height: u16 = 10;

    let (tx, _rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    // First line is Known so the render path sees at least one valid row.
    let mut cache: Vec<LineSlot> = vec![LineSlot::Known(CachedLine {
        text: String::from("first line"),
        cursors: vec![0],
        syntax_spans: Vec::new(),
        logical_line: None,
    })];
    cache.extend(std::iter::repeat_n(LineSlot::Invalid, total_lines - 1));
    app.backend.line_cache = cache;
    app.backend.rebuild_lines();

    // Rendering must complete without panic even though most slots are Invalid.
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| crate::ui::ui(frame, &app)).unwrap();

    let buf = terminal.backend().buffer();
    // The first row must contain the known line text.
    let first_row: String = (0..width).map(|x| buf.cell((x, 0)).unwrap().symbol()).collect();
    assert!(
        first_row.contains("first line"),
        "first row should render the known line, got: {first_row:?}"
    );
}
