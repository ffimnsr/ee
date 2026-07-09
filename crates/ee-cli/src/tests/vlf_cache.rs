use std::sync::mpsc::{self, TryRecvError};
use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};

use crate::app::App;
use crate::backend::{
    BackendEvent, CachedLine, CoreLine, CoreUpdate, CoreUpdateKind, CoreUpdateOp, LineSlot,
    coalesce_backend_events, invalid_line_ranges_bounded, parse_notification,
};
use crate::buffer::{BufferManager, VlfChunkUpdate};
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

// ── VLF viewport protocol ──────────────────────────────────────────────────

#[test]
fn apply_vlf_chunks_populates_line_cache() {
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.vlf_generation = 7;
    buf.line_cache = vec![LineSlot::Invalid; 3];

    let lines = vec![String::from("alpha"), String::from("beta")];
    buf.apply_vlf_chunks(VlfChunkUpdate {
        generation: 7,
        line_start: 0,
        lines: &lines,
        syntax_spans: &[],
        approximate_line_count: 3,
        line_count_exact: true,
    });

    assert_eq!(
        buf.line_slot(0).cloned().unwrap(),
        LineSlot::Known(CachedLine {
            text: String::from("alpha"),
            cursors: vec![],
            syntax_spans: vec![]
        })
    );
    assert_eq!(
        buf.line_slot(1).cloned().unwrap(),
        LineSlot::Known(CachedLine {
            text: String::from("beta"),
            cursors: vec![],
            syntax_spans: vec![]
        })
    );
    assert_eq!(buf.line_count(), 3);
    assert!(buf.line_slot(2).is_none());
}

#[test]
fn apply_vlf_chunks_normalizes_crlf_line_endings() {
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.vlf_generation = 2;

    let lines = vec![String::from("alpha\r"), String::from("beta\r")];
    buf.apply_vlf_chunks(VlfChunkUpdate {
        generation: 2,
        line_start: 0,
        lines: &lines,
        syntax_spans: &[],
        approximate_line_count: 2,
        line_count_exact: true,
    });

    assert_eq!(buf.get_line(0), Some("alpha"));
    assert_eq!(buf.get_line(1), Some("beta"));
}

#[test]
fn apply_vlf_chunks_empty_response_preserves_loaded_cache() {
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.vlf_generation = 3;
    buf.vlf_cache_start_line = 40;
    buf.line_cache = vec![
        LineSlot::Known(CachedLine {
            text: String::from("line 40"),
            cursors: vec![],
            syntax_spans: vec![],
        }),
        LineSlot::Known(CachedLine {
            text: String::from("line 41"),
            cursors: vec![],
            syntax_spans: vec![],
        }),
    ];

    buf.apply_vlf_chunks(VlfChunkUpdate {
        generation: 3,
        line_start: 100,
        lines: &[],
        syntax_spans: &[],
        approximate_line_count: 1_000,
        line_count_exact: false,
    });

    assert_eq!(buf.vlf_cache_start_line, 40);
    assert_eq!(buf.get_line(40), Some("line 40"));
    assert_eq!(buf.get_line(41), Some("line 41"));
    assert_eq!(buf.vlf_approx_line_count, 1_000);
}

#[test]
fn apply_vlf_chunks_empty_response_keeps_tail_jump_pending() {
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.vlf_generation = 4;
    buf.pending_vlf_tail_jump = true;

    buf.apply_vlf_chunks(VlfChunkUpdate {
        generation: 4,
        line_start: u64::MAX - 1,
        lines: &[],
        syntax_spans: &[],
        approximate_line_count: 10_000,
        line_count_exact: false,
    });

    assert!(buf.pending_vlf_tail_jump);
    assert_eq!((buf.cursor_line, buf.cursor_col), (0, 0));
}

#[test]
fn apply_vlf_chunks_stale_generation_discarded() {
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.vlf_generation = 5;
    buf.line_cache = vec![LineSlot::Invalid; 2];

    let lines = vec![String::from("stale")];
    // Send with generation 3 (older than current 5) — must be ignored.
    buf.apply_vlf_chunks(VlfChunkUpdate {
        generation: 3,
        line_start: 0,
        lines: &lines,
        syntax_spans: &[],
        approximate_line_count: 2,
        line_count_exact: false,
    });

    assert_eq!(buf.line_cache[0], LineSlot::Invalid, "stale response must not update cache");
}

#[test]
fn apply_vlf_chunks_does_not_grow_cache_to_approximate_count() {
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.vlf_generation = 1;
    buf.line_cache = Vec::new(); // start empty

    let lines: Vec<String> = Vec::new();
    buf.apply_vlf_chunks(VlfChunkUpdate {
        generation: 1,
        line_start: 0,
        lines: &lines,
        syntax_spans: &[],
        approximate_line_count: 1000,
        line_count_exact: false,
    });

    assert_eq!(buf.line_cache.len(), 0, "cache must stay viewport-local");
    assert_eq!(buf.line_count(), 1000);
    assert_eq!(buf.vlf_approx_line_count, 1000);
}

#[test]
fn apply_vlf_chunks_exact_count_replaces_stale_window() {
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.vlf_generation = 1;
    buf.line_cache = vec![LineSlot::Invalid; 1000];

    buf.apply_vlf_chunks(VlfChunkUpdate {
        generation: 1,
        line_start: 10,
        lines: &[String::from("tail")],
        syntax_spans: &[],
        approximate_line_count: 25,
        line_count_exact: true,
    });

    assert_eq!(buf.line_count(), 25);
    assert_eq!(buf.line_cache.len(), 1);
    assert_eq!(buf.vlf_cache_start_line, 10);
    assert_eq!(buf.get_line(10), Some("tail"));
    assert!(buf.vlf_line_count_exact);
}

#[test]
fn vlf_line_count_uses_exact_report_over_stale_cache() {
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.line_cache = vec![LineSlot::Invalid; 1000];
    buf.vlf_approx_line_count = 25;
    buf.vlf_line_count_exact = true;

    assert_eq!(buf.line_count(), 25);
}

#[test]
fn vlf_line_count_keeps_sparse_cache_when_exact_report_missing() {
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.line_cache = vec![LineSlot::Invalid; 500];
    buf.vlf_line_count_exact = true;

    assert_eq!(buf.line_count(), 500);
}

#[test]
fn apply_vlf_chunks_tail_jump_moves_cursor_to_returned_last_line() {
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.vlf_generation = 1;
    buf.pending_vlf_tail_jump = true;

    buf.apply_vlf_chunks(VlfChunkUpdate {
        generation: 1,
        line_start: 995,
        lines: &[String::from("line 998"), String::from("line 999")],
        syntax_spans: &[],
        approximate_line_count: 1000,
        line_count_exact: false,
    });

    assert_eq!((buf.cursor_line, buf.cursor_col), (996, 0));
    assert!(!buf.pending_vlf_tail_jump);
}

#[test]
fn vlf_document_mode_clears_stale_normal_cache_and_retries_viewport() {
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
    assert_eq!(mgr.active().line_cache.len(), 200);
    assert!(mgr.active().line_cache.iter().all(|slot| matches!(slot, LineSlot::Invalid)));

    mgr.notify_scroll(0, 4).unwrap();
    let first: Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).unwrap())
        .expect("vlf viewport notification should be json");
    assert_eq!(first["params"]["method"], "vlf_viewport");
    assert_eq!(first["params"]["params"]["line_start"], 0);
    assert_eq!(first["params"]["params"]["line_end"], 200);
    assert_eq!(first["params"]["params"]["generation"], 1);

    mgr.notify_scroll(0, 4).unwrap();
    assert!(matches!(
        rx.recv_timeout(Duration::from_millis(50)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));

    backend_tx
        .send(BackendEvent::VlfChunks {
            view_id: String::from("view-id-1"),
            generation: 1,
            line_start: 0,
            lines: Vec::new(),
            syntax_spans: Vec::new(),
            approximate_line_count: 1000,
            line_count_exact: false,
            index_progress: 0.1,
        })
        .unwrap();
    mgr.drain_events().unwrap();

    mgr.notify_scroll(0, 4).unwrap();
    let retry: Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).unwrap())
        .expect("vlf viewport retry after empty response should be json");
    assert_eq!(retry["params"]["method"], "vlf_viewport");
    assert_eq!(retry["params"]["params"]["line_start"], 0);
    assert_eq!(retry["params"]["params"]["line_end"], 204);
    assert_eq!(retry["params"]["params"]["generation"], 2);
}

#[test]
fn vlf_notify_scroll_prefetches_beyond_ready_visible_rows() {
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
            })
        })
        .collect();

    mgr.notify_scroll(0, 4).unwrap();
    let scroll: Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).unwrap())
        .expect("vlf viewport notification should be json");

    assert_eq!(scroll["params"]["method"], "vlf_viewport");
    assert_eq!(scroll["params"]["params"]["line_start"], 0);
    assert_eq!(scroll["params"]["params"]["line_end"], 204);
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
        .send(BackendEvent::VlfChunks {
            view_id: String::from("view-id-1"),
            generation: 0,
            line_start: 0,
            lines: Vec::new(),
            syntax_spans: Vec::new(),
            approximate_line_count: 1000,
            line_count_exact: false,
            index_progress: 0.1,
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
fn vlf_ignores_normal_update_after_document_mode() {
    let (tx, _rx) = mpsc::channel();
    let (backend_tx, backend_rx) = mpsc::channel();
    let mut mgr = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    backend_tx
        .send(BackendEvent::DocumentMode { view_id: String::from("view-id-1"), is_vlf: true })
        .unwrap();
    backend_tx
        .send(BackendEvent::Update {
            view_id: String::from("view-id-1"),
            update: CoreUpdate {
                ops: vec![CoreUpdateOp {
                    op: CoreUpdateKind::Insert,
                    n: 1,
                    lines: vec![CoreLine {
                        text: Some(String::from("stale normal line")),
                        cursor: Vec::new(),
                        syntax_spans: Some(Vec::new()),
                    }],
                }],
                pristine: true,
                annotations: Vec::new(),
            },
        })
        .unwrap();

    mgr.drain_events().unwrap();
    assert!(mgr.active().is_vlf);
    assert!(
        mgr.active().line_cache.iter().all(|slot| matches!(slot, LineSlot::Invalid)),
        "VLF must ignore normal update payloads"
    );
}

// ── Backend event parsing ──────────────────────────────────────────────────

#[test]
fn vlf_chunks_backend_event_parsed() {
    let params = json!({
        "view_id": "view-1",
        "generation": 42,
        "line_start": 10,
        "lines": ["hello", "world"],
        "syntax_spans": [[{ "start_byte": 0, "end_byte": 5, "scope": "keyword.control" }], []],
        "approximate_line_count": 500,
        "line_count_exact": false,
        "index_progress": 0.42,
    });
    let event = parse_notification("vlf_chunks", params).expect("should parse vlf_chunks");
    match event {
        BackendEvent::VlfChunks {
            view_id,
            generation,
            line_start,
            lines,
            syntax_spans,
            approximate_line_count,
            line_count_exact,
            index_progress,
        } => {
            assert_eq!(view_id, "view-1");
            assert_eq!(generation, 42);
            assert_eq!(line_start, 10);
            assert_eq!(lines, vec!["hello", "world"]);
            assert_eq!(syntax_spans.len(), 2);
            assert_eq!(syntax_spans[0][0].scope, "keyword.control");
            assert_eq!(approximate_line_count, 500);
            assert!(!line_count_exact);
            assert!((index_progress - 0.42).abs() < 1e-9);
        }
        other => panic!("expected VlfChunks, got {:?}", other),
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
        BackendEvent::VlfChunks {
            view_id: String::from("view-1"),
            generation: 1,
            line_start: 0,
            lines: vec![String::from("old")],
            syntax_spans: Vec::new(),
            approximate_line_count: 10,
            line_count_exact: false,
            index_progress: 0.1,
        },
        BackendEvent::VlfSearchStatus {
            view_id: String::from("view-1"),
            query: String::from("needle"),
            scanned_bytes: 100,
            total_bytes: 100,
            complete: true,
            stored_match_count: 4,
            ranges: Vec::new(),
        },
        BackendEvent::VlfChunks {
            view_id: String::from("view-1"),
            generation: 2,
            line_start: 5,
            lines: vec![String::from("new")],
            syntax_spans: Vec::new(),
            approximate_line_count: 10,
            line_count_exact: false,
            index_progress: 0.2,
        },
    ];

    let coalesced = coalesce_backend_events(events);

    assert_eq!(coalesced.len(), 2);
    match &coalesced[0] {
        BackendEvent::VlfSearchStatus { complete, scanned_bytes, .. } => {
            assert!(*complete);
            assert_eq!(*scanned_bytes, 100);
        }
        other => panic!("expected latest search status, got {other:?}"),
    }
    match &coalesced[1] {
        BackendEvent::VlfChunks { generation, line_start, lines, .. } => {
            assert_eq!(*generation, 2);
            assert_eq!(*line_start, 5);
            assert_eq!(lines, &vec![String::from("new")]);
        }
        other => panic!("expected latest vlf chunks, got {other:?}"),
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
            })
        })
        .collect();
    state.rebuild_lines();

    state
        .apply_update(CoreUpdate {
            pristine: true,
            annotations: Vec::new(),
            ops: vec![
                // Copy entire existing cache — must not scan non-copy lines.
                CoreUpdateOp { op: CoreUpdateKind::Copy, n: large_line_count, lines: Vec::new() },
                // Append one new line.
                CoreUpdateOp {
                    op: CoreUpdateKind::Insert,
                    n: 1,
                    lines: vec![CoreLine {
                        text: Some(String::from("new-tail")),
                        cursor: Vec::new(),
                        syntax_spans: None,
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
            pristine: true,
            annotations: Vec::new(),
            ops: vec![CoreUpdateOp {
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
        });
    }
    for slot in &mut cache[viewport_end..] {
        *slot = LineSlot::Known(CachedLine {
            text: String::from("after"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
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
