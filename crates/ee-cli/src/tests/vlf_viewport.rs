use std::sync::mpsc::{self, TryRecvError};
use std::time::Duration;

use serde_json::Value;

use crate::backend::{BackendEvent, CachedLine, LineSlot};
use crate::buffer::BufferManager;

#[test]
fn sustained_reverse_scroll_only_loads_latest_queued_vlf_viewport() {
    let (tx, rx) = mpsc::channel();
    let (backend_tx, backend_rx) = mpsc::channel();
    let mut manager = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    manager.is_vlf = true;
    manager.vlf_approx_line_count = 10_000;
    manager.vlf_line_count_exact = true;
    manager.vlf_cache_start_line = 9_760;
    manager.line_cache = (9_760..10_000)
        .map(|line| {
            LineSlot::Known(CachedLine {
                text: format!("tail {line}"),
                cursors: Vec::new(),
                syntax_spans: Vec::new(),
            })
        })
        .collect();

    let first_start = 9_500;
    manager.notify_scroll(first_start, first_start + 40).unwrap();
    for page in 1..=64 {
        let start = first_start - page * 40;
        manager.notify_scroll(start, start + 40).unwrap();
    }

    let first: Value = serde_json::from_str(
        &rx.recv_timeout(Duration::from_secs(1)).expect("first viewport request should be sent"),
    )
    .expect("viewport request should be json");
    assert_eq!(first["params"]["params"]["line_start"], first_start);
    assert_eq!(first["params"]["params"]["generation"], 1);
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));

    backend_tx
        .send(BackendEvent::VlfChunks {
            view_id: String::from("view-id-1"),
            generation: 1,
            line_start: first_start as u64,
            lines: (first_start..first_start + 240).map(|line| format!("stale {line}")).collect(),
            syntax_spans: Vec::new(),
            approximate_line_count: 10_000,
            line_count_exact: true,
            index_progress: 1.0,
        })
        .unwrap();
    manager.drain_events().unwrap();

    let final_start = first_start - 64 * 40;
    let latest: Value = serde_json::from_str(
        &rx.recv_timeout(Duration::from_secs(1)).expect("latest viewport request should be sent"),
    )
    .expect("viewport request should be json");
    let latest_generation = latest["params"]["params"]["generation"].as_u64().unwrap();
    assert_eq!(latest["params"]["params"]["line_start"], final_start);
    assert_eq!(latest["params"]["params"]["line_end"], final_start + 240);
    assert_eq!(latest_generation, 65);
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    assert!(manager.pending_line_request);

    backend_tx
        .send(BackendEvent::VlfChunks {
            view_id: String::from("view-id-1"),
            generation: latest_generation,
            line_start: final_start as u64,
            lines: (final_start..final_start + 240).map(|line| format!("latest {line}")).collect(),
            syntax_spans: Vec::new(),
            approximate_line_count: 10_000,
            line_count_exact: true,
            index_progress: 1.0,
        })
        .unwrap();
    manager.drain_events().unwrap();

    assert!(!manager.pending_line_request);
    assert_eq!(manager.vlf_cache_start_line, final_start);
    let missing = (final_start..final_start + 240)
        .filter(|&line| manager.get_line(line).is_none())
        .collect::<Vec<_>>();
    assert!(missing.is_empty(), "latest VLF viewport stayed unloaded: {missing:?}");
}

#[test]
fn empty_tail_response_retries_on_next_viewport_notification() {
    let (tx, rx) = mpsc::channel();
    let (backend_tx, backend_rx) = mpsc::channel();
    let mut manager = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    manager.is_vlf = true;
    manager.vlf_approx_line_count = 10_000;
    manager.vlf_line_count_exact = true;

    manager.request_vlf_tail_viewport(40).unwrap();
    let first = recv_viewport_request(&rx);
    assert_eq!(first["params"]["params"]["generation"], 1);

    backend_tx.send(vlf_response(1, 0, 0)).unwrap();
    manager.drain_events().unwrap();
    assert!(manager.pending_vlf_tail_jump);
    assert!(!manager.pending_line_request);

    manager.notify_scroll(0, 40).unwrap();
    let retry = recv_viewport_request(&rx);
    assert_eq!(retry["params"]["params"]["line_start"], u64::MAX);
    assert_eq!(retry["params"]["params"]["generation"], 2);

    backend_tx.send(vlf_response(2, 9_960, 40)).unwrap();
    manager.drain_events().unwrap();
    assert!(!manager.pending_vlf_tail_jump);
    assert!(!manager.pending_line_request);
    assert_eq!(manager.cursor_line, 9_999);
}

#[test]
fn repeated_tail_cancellation_keeps_obsolete_vlf_work_bounded() {
    let (tx, rx) = mpsc::channel();
    let (backend_tx, backend_rx) = mpsc::channel();
    let mut manager = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    manager.is_vlf = true;
    manager.vlf_approx_line_count = 10_000;
    manager.vlf_line_count_exact = true;

    manager.request_vlf_tail_viewport(40).unwrap();
    manager.cancel_vlf_tail_jump();
    manager.notify_scroll(9_000, 9_040).unwrap();
    manager.request_vlf_tail_viewport(40).unwrap();
    manager.cancel_vlf_tail_jump();
    manager.notify_scroll(8_000, 8_040).unwrap();

    let tail = recv_viewport_request(&rx);
    let first_replacement = recv_viewport_request(&rx);
    assert_eq!(tail["params"]["params"]["generation"], 1);
    assert_eq!(first_replacement["params"]["params"]["generation"], 3);
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));

    backend_tx.send(vlf_response(1, 9_960, 40)).unwrap();
    backend_tx.send(vlf_response(3, 9_000, 240)).unwrap();
    manager.drain_events().unwrap();

    let latest = recv_viewport_request(&rx);
    assert_eq!(latest["params"]["params"]["line_start"], 8_000);
    assert_eq!(latest["params"]["params"]["generation"], 6);
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

fn recv_viewport_request(rx: &mpsc::Receiver<String>) -> Value {
    serde_json::from_str(
        &rx.recv_timeout(Duration::from_secs(1)).expect("viewport request should be sent"),
    )
    .expect("viewport request should be json")
}

fn vlf_response(generation: u64, line_start: usize, line_count: usize) -> BackendEvent {
    BackendEvent::VlfChunks {
        view_id: String::from("view-id-1"),
        generation,
        line_start: line_start as u64,
        lines: (line_start..line_start + line_count).map(|line| format!("line {line}")).collect(),
        syntax_spans: Vec::new(),
        approximate_line_count: 10_000,
        line_count_exact: true,
        index_progress: 1.0,
    }
}
