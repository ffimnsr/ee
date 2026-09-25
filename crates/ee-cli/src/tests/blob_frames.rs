//! Binary text carrier tests: frame transport, positional decode, and rejection.
//!
//! The in-process pair carries two frame kinds, and an `update` whose ops set the
//! `blob` flag is followed by exactly one binary frame holding the row lengths and
//! bytes. These tests pin the round trip, the carrier equivalence on the frontend
//! cache, and the fail-closed rules for a malformed frame.
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;

use xi_core_lib::text_blob::TextBlob;
use xi_rpc::{ReadTransport, WriteTransport};

use crate::backend::update::CoreLine;
use crate::backend::{
    BackendEvent, CachedLine, ChannelReader, ChannelWriter, CoreUpdate, CoreUpdateKind,
    CoreUpdateOp, Frame, LineSlot, PendingRequests, xi_reader_thread,
};
use crate::tests::helpers::*;

/// One row of an insert op: its per-line text, or `None` to take a frame row.
type Row = Option<&'static str>;

/// A rejection case: a name, the rows it carries, the op's blob flag, and the frame.
type BlobCase = (&'static str, Vec<Row>, bool, Vec<u8>);

/// The `update` notification JSON for one insert op carrying `lines`.
fn update_json(lines: serde_json::Value, blob: bool) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "update",
        "params": {
            "view_id": "view-1",
            "update": {
                "pristine": true,
                "annotations": [],
                "ops": [{ "op": "ins", "n": 1, "blob": blob, "lines": lines }],
            }
        }
    })
    .to_string()
}

/// Encodes a frame from rows, the way the backend does.
fn frame_of(rows: &[&str]) -> Vec<u8> {
    let mut blob = TextBlob::new();
    for row in rows {
        assert!(blob.push_bytes(row.as_bytes()), "fixture should fit");
    }
    blob.into_frame()
}

/// One insert op for `rows`; `blob` says whether the rows come from the frame.
fn insert_op(rows: Vec<Row>, blob: bool) -> CoreUpdateOp {
    CoreUpdateOp {
        op: CoreUpdateKind::Insert,
        n: rows.len(),
        lines: rows
            .into_iter()
            .map(|text| CoreLine { text: text.map(str::to_owned), ..Default::default() })
            .collect(),
        blob,
    }
}

fn insert_update(rows: Vec<Row>, blob: bool) -> CoreUpdate {
    CoreUpdate {
        ops: vec![insert_op(rows, blob)],
        pristine: true,
        annotations: Vec::new(),
        vlf_total_lines: None,
        scopes: Vec::new(),
        blob: None,
    }
}

fn cached_texts(buf: &crate::buffer::BufState) -> Vec<String> {
    buf.line_cache
        .iter()
        .map(|slot| match slot {
            LineSlot::Known(line) => line.text.clone(),
            LineSlot::Invalid => String::from("<invalid>"),
        })
        .collect()
}

#[test]
fn in_process_pair_round_trips_both_frame_kinds() {
    let (tx, rx) = mpsc::channel::<Frame>();
    let mut writer = ChannelWriter { tx };
    assert!(writer.supports_binary_frames(), "the in-process pair carries byte frames");

    writer.write_message(b"{\"jsonrpc\":\"2.0\"}").unwrap();
    writer.write_binary_message(&[0xE2, 0x82, 0xAC]).unwrap();

    assert_eq!(rx.recv().unwrap(), Frame::Text(String::from("{\"jsonrpc\":\"2.0\"}")));
    assert_eq!(rx.recv().unwrap(), Frame::Binary(vec![0xE2, 0x82, 0xAC]));
}

#[test]
fn channel_reader_serves_request_messages() {
    // The frontend -> core channel stays text-only; the reader inherits the
    // trait's unsupported default for byte frames.
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(4);
    tx.blocking_send(String::from("{\"id\":1}")).unwrap();
    drop(tx);

    let mut reader = ChannelReader { rx };
    let mut buf = String::new();
    assert_eq!(reader.read_message(&mut buf).unwrap(), 8);
    assert_eq!(buf, "{\"id\":1}");
    assert_eq!(reader.read_message(&mut String::new()).unwrap(), 0, "closed channel is EOF");
    assert!(
        reader.read_binary_message(&mut Vec::new()).is_err(),
        "no byte carrier frontend -> core"
    );
}

#[test]
fn blob_carrier_decodes_the_same_rows_as_the_text_carrier() {
    // Byte-for-byte equivalence on the cache, including the trailing line ending
    // the frontend strips, multibyte text, CRLF, and empty rows.
    let fixtures: [&str; 5] = ["alpha\nbeta\n", "café\n日本\n", "one\r\ntwo\r\n", "a\n\nb\n", "\n"];

    for fixture in fixtures {
        let rows: Vec<&str> = fixture.split_inclusive('\n').collect();

        let mut as_blob = test_buf_state();
        let mut update = insert_update(vec![None; rows.len()], true);
        update.blob = Some(Arc::from(frame_of(&rows).as_slice()));
        as_blob.apply_update(update).unwrap();

        let mut as_text = test_buf_state();
        as_text
            .apply_update(insert_update(rows.iter().copied().map(Some).collect(), false))
            .unwrap();

        assert_eq!(
            cached_texts(&as_blob),
            cached_texts(&as_text),
            "blob and text carriers must decode identically for {fixture:?}"
        );
    }
}

#[test]
fn blob_op_replaces_text_and_a_text_row_overrides_the_frame() {
    // Inside a blob op, a row without `text` takes the next frame row; a row with
    // `text` keeps its own text and consumes no frame bytes.
    let mut buf = test_buf_state();
    buf.apply_update(insert_update(vec![Some("alpha\n"), Some("beta\n")], false)).unwrap();

    let mut update = CoreUpdate {
        ops: vec![CoreUpdateOp {
            op: CoreUpdateKind::Update,
            n: 2,
            lines: vec![
                CoreLine::default(),
                CoreLine { text: Some(String::from("inline\n")), ..Default::default() },
            ],
            blob: true,
        }],
        pristine: true,
        annotations: Vec::new(),
        vlf_total_lines: None,
        scopes: Vec::new(),
        blob: None,
    };
    update.blob = Some(Arc::from(frame_of(&["gamma\n"]).as_slice()));
    buf.apply_update(update).unwrap();

    assert_eq!(cached_texts(&buf), vec![String::from("gamma"), String::from("inline")]);
}

#[test]
fn cursor_only_ops_keep_cached_text() {
    // A non-blob op that says nothing about text leaves the cached line alone.
    let mut buf = test_buf_state();
    buf.apply_update(insert_update(vec![Some("alpha\n"), Some("beta\n")], false)).unwrap();

    buf.apply_update(CoreUpdate {
        ops: vec![CoreUpdateOp {
            op: CoreUpdateKind::Update,
            n: 2,
            lines: vec![CoreLine { cursor: vec![1], ..Default::default() }, CoreLine::default()],
            blob: false,
        }],
        pristine: true,
        annotations: Vec::new(),
        vlf_total_lines: None,
        scopes: Vec::new(),
        blob: None,
    })
    .unwrap();

    assert_eq!(cached_texts(&buf), vec![String::from("alpha"), String::from("beta")]);
    let LineSlot::Known(first) = &buf.line_cache[0] else { panic!("expected cached line") };
    assert_eq!(first.cursors, vec![1], "cursor-only row keeps its text");
}

#[test]
fn malformed_blob_payloads_fail_closed() {
    let mut buf = test_buf_state();
    buf.apply_update(insert_update(vec![Some("keep\n")], false)).unwrap();
    let before = buf.line_cache.clone();

    let cases: Vec<BlobCase> = vec![
        ("op declares a frame but none arrived", vec![None], true, Vec::new()),
        ("frame has fewer rows than the op", vec![None, None], true, frame_of(&["one\n"])),
        ("frame has more rows than the op", vec![None], true, frame_of(&["one\n", "two\n"])),
        ("frame row is not UTF-8", vec![None], true, vec![1, 2, 0xE2, 0x82]),
        ("blob op row without text or a frame row", vec![None], true, frame_of(&[])),
        ("truncated frame", vec![None], true, vec![1, 5, b'a']),
        ("trailing bytes", vec![None], true, vec![1, 1, b'a', b'x']),
    ];

    for (name, rows, blob_flag, frame) in cases {
        let mut update = insert_update(rows, blob_flag);
        update.blob = if frame.is_empty() && name.contains("none arrived") {
            None
        } else {
            Some(Arc::from(frame.as_slice()))
        };
        let result = buf.apply_update(update);
        assert!(result.is_err(), "{name} must reject the payload");
        assert_eq!(buf.line_cache, before, "{name} must leave the cache intact");
    }
}

#[test]
fn reader_attaches_the_frame_and_alerts_on_malformed_frames() {
    // (a) A well-formed pair: the op sets `blob`, the binary frame that follows is
    // attached to the event.
    let (tx, rx) = mpsc::channel::<Frame>();
    let (backend_tx, backend_rx) = mpsc::channel::<BackendEvent>();
    let (core_tx, _core_rx) = tokio::sync::mpsc::channel::<String>(4);
    let pending: PendingRequests =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let shutdown = Arc::new(AtomicBool::new(false));

    let handle = std::thread::spawn({
        let shutdown = Arc::clone(&shutdown);
        move || xi_reader_thread(rx, core_tx, backend_tx, pending, Some(shutdown))
    });

    tx.send(Frame::Text(update_json(serde_json::json!([{ "cursor": [] }]), true))).unwrap();
    tx.send(Frame::Binary(frame_of(&["alpha\n"]))).unwrap();

    let BackendEvent::Update { update, .. } =
        backend_rx.recv_timeout(std::time::Duration::from_secs(5)).expect("update event")
    else {
        panic!("expected an update event");
    };
    assert_eq!(
        update.blob.as_deref(),
        Some(frame_of(&["alpha\n"]).as_slice()),
        "frame attached to the payload"
    );

    // (b) A payload that declares a frame but is followed by a text frame is
    // rejected with an operator-visible alert instead of a partial update.
    tx.send(Frame::Text(update_json(serde_json::json!([{ "cursor": [] }]), true))).unwrap();
    tx.send(Frame::Text(String::from("{\"jsonrpc\":\"2.0\"}"))).unwrap();

    let alert = backend_rx.recv_timeout(std::time::Duration::from_secs(5)).expect("alert event");
    match alert {
        BackendEvent::Alert(message) => {
            assert!(message.contains("malformed update text frame"), "unexpected alert: {message}");
        }
        other => panic!("expected an alert, got {other:?}"),
    }

    shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = handle.join();
}

#[test]
fn vlf_window_applies_blob_rows() {
    // The bounded VLF window uses the same decode path as the rope path.
    let mut buf = test_buf_state();
    buf.is_vlf = true;

    let mut update = CoreUpdate {
        ops: vec![CoreUpdateOp {
            op: CoreUpdateKind::Insert,
            n: 2,
            lines: vec![
                CoreLine { logical_line: Some(0), ..Default::default() },
                CoreLine { logical_line: Some(1), ..Default::default() },
            ],
            blob: true,
        }],
        pristine: true,
        annotations: Vec::new(),
        vlf_total_lines: Some(crate::backend::update::VlfTotalLines {
            count: 2,
            exact: true,
            index_progress: 1.0,
        }),
        scopes: Vec::new(),
        blob: None,
    };
    update.blob = Some(Arc::from(frame_of(&["alpha\n", "beta\n"]).as_slice()));
    buf.apply_update(update).unwrap();

    assert_eq!(buf.get_line(0), Some("alpha"));
    assert_eq!(buf.get_line(1), Some("beta"));
    assert_eq!(buf.line_cache.len(), 2);
}

#[test]
fn malformed_blob_leaves_the_active_buffer_intact() {
    // The app-level path (BufferManager) must survive a rejected payload too.
    let mut app = crate::app::App::from_path(None).unwrap();
    app.backend.apply_update(insert_update(vec![Some("keep\n")], false)).unwrap();
    let before: Vec<CachedLine> = app
        .backend
        .line_cache
        .iter()
        .filter_map(|slot| match slot {
            LineSlot::Known(line) => Some(line.clone()),
            LineSlot::Invalid => None,
        })
        .collect();

    let mut update = insert_update(vec![None, None], true);
    update.blob = Some(Arc::from(frame_of(&["one\n"]).as_slice()));
    assert!(app.backend.apply_update(update).is_err());

    let after: Vec<CachedLine> = app
        .backend
        .line_cache
        .iter()
        .filter_map(|slot| match slot {
            LineSlot::Known(line) => Some(line.clone()),
            LineSlot::Invalid => None,
        })
        .collect();
    assert_eq!(before, after, "a rejected payload leaves the buffer's cache intact");
}
