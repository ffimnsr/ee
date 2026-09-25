//! Binary text carrier tests: the blob payload against the per-line text payload.
//!
//! The carrier is a transport capability, not a document property, so the same
//! window rendered to a byte-frame peer and to a text-only peer must produce the
//! same row text. These tests pin that equivalence and the payload shape of each
//! carrier.
use super::*;
use crate::text_blob::decode_frame;

/// Row text as the frontend decodes it: rows of a `blob` op take the next row of
/// the frame in order, a row with `text` uses it, and a row with neither is
/// skipped (it keeps whatever the frontend already shows).
fn decoded_row_texts(notifications: &[(String, Value)], blob: Option<&[u8]>) -> Vec<String> {
    let frame_rows = match blob {
        Some(frame) => decode_frame(frame).expect("frame decodes"),
        None => Vec::new(),
    };
    let mut next_frame_row = 0usize;
    let mut rows = Vec::new();
    for (_method, params) in notifications.iter().filter(|(method, _)| method == "update") {
        let Some(ops) = params["update"]["ops"].as_array() else { continue };
        for op in ops {
            let Some(lines) = op["lines"].as_array() else { continue };
            let uses_blob = op["blob"].as_bool().unwrap_or(false);
            for line in lines {
                if let Some(text) = line.get("text").and_then(Value::as_str) {
                    rows.push(text.to_owned());
                    continue;
                }
                if uses_blob {
                    let row = frame_rows[next_frame_row];
                    next_frame_row += 1;
                    rows.push(String::from_utf8(row.to_vec()).expect("frame row is UTF-8"));
                }
            }
        }
    }
    assert_eq!(next_frame_row, frame_rows.len(), "every frame row should be consumed");
    rows
}

/// Renders `text` in a fresh view and returns the decoded row texts plus whether
/// the payload used the blob carrier.
fn render_rows(text: &Rope, binary_capable: bool) -> (Vec<String>, bool, Option<Vec<u8>>) {
    let mut view = View::new(1.into(), BufferId::new(2));
    view.debug_force_rewrap_cols(text, 0);
    let store = RopeTextStore::new(text.clone(), 0);
    let (client, peer) =
        if binary_capable { recording_client_with_binary() } else { recording_client() };

    view.request_lines(&store, &client, 0, 9, false, "rust", false);
    let notifications = peer.take_notifications();
    let blob = peer.take_binary_frames().into_iter().next();
    let used_blob = notifications.iter().any(|(_, params)| {
        params["update"]["ops"]
            .as_array()
            .is_some_and(|ops| ops.iter().any(|op| op["blob"].as_bool().unwrap_or(false)))
    });
    (decoded_row_texts(&notifications, blob.as_deref()), used_blob, blob)
}

#[test]
fn blob_and_text_carriers_decode_to_the_same_rows() {
    let fixtures: [&str; 7] = [
        "alpha\nbeta\ngamma\n",
        "café\nnaïve\n日本\n",
        "one\r\ntwo\r\nthree\r\n",
        "tab\there\nand\tthere\n",
        "no trailing newline",
        "a\n\n\nb\n",
        "\n",
    ];

    for fixture in fixtures {
        let text = Rope::from(fixture);
        let (blob_rows, used_blob, blob) = render_rows(&text, true);
        let (text_rows, _, _) = render_rows(&text, false);

        assert!(used_blob, "byte-frame peer should use the blob carrier for {fixture:?}");
        assert!(blob.is_some(), "a blob payload must send its frame for {fixture:?}");
        assert_eq!(blob_rows, text_rows, "carriers must decode identically for {fixture:?}");
        assert!(!blob_rows.is_empty(), "fixture {fixture:?} should render rows");
    }
}

#[test]
fn one_very_long_line_crosses_the_boundary_once() {
    // A megabyte-long single line: the row is still one blob slice, and the frame
    // carries the whole row once (no per-line string in the payload).
    let fixture = "x".repeat(1_000_000);
    let text = Rope::from(fixture.clone());
    let (rows, used_blob, blob) = render_rows(&text, true);

    assert!(used_blob, "long row should use the blob carrier");
    let blob = blob.expect("blob frame");
    // Row count varint (1) + length varint (3 for 1e6) + the row's bytes.
    assert_eq!(blob.len(), fixture.len() + 4, "the row's bytes plus its header");
    assert_eq!(rows, vec![fixture]);
}

#[test]
fn text_only_transport_keeps_the_per_line_carrier() {
    // A transport that reports no byte frames must still receive parseable
    // per-line updates: no blob slices, no binary frames.
    let text = Rope::from("alpha\nbeta\n");
    let (rows, used_blob, blob) = render_rows(&text, false);

    assert!(!used_blob, "text-only peer must not see blob slices");
    assert!(blob.is_none(), "text-only peer must not receive a binary frame");
    // Rows carry their line ending on both carriers; the frontend strips it when
    // it caches the line.
    assert_eq!(rows, vec![String::from("alpha\n"), String::from("beta\n"), String::new()]);
}

#[test]
fn blob_carrier_byte_cost_is_measured_both_ways() {
    // Payload cost of the same window on both carriers: JSON bytes for the text
    // carrier, JSON bytes plus the raw frame for the blob carrier. Measured in
    // bytes (not timings) so the trade-off is pinned rather than assumed.
    let measure = |text: &Rope| -> (usize, usize, usize) {
        let payload_bytes = |binary_capable: bool| -> (usize, usize) {
            let mut view = View::new(1.into(), BufferId::new(2));
            view.debug_force_rewrap_cols(text, 0);
            let store = RopeTextStore::new(text.clone(), 0);
            let (client, peer) =
                if binary_capable { recording_client_with_binary() } else { recording_client() };
            view.request_lines(&store, &client, 0, 199, false, "rust", false);
            let notifications = peer.take_notifications();
            let json = notifications
                .iter()
                .map(|(method, params)| method.len() + params.to_string().len())
                .sum();
            let frame = peer.take_binary_frames().iter().map(Vec::len).sum();
            (json, frame)
        };

        let (text_json, _) = payload_bytes(false);
        let (blob_json, blob_frame) = payload_bytes(true);
        (text_json, blob_json, blob_frame)
    };

    // Escape-light source text: the rows move into the frame and the JSON keeps only
    // the cursor/`ln` envelope plus one flag per op, so the total is well below the
    // per-line carrier (measured -19% on this fixture).
    let source = Rope::from(
        (0..200)
            .map(|index| format!("let value_{index} = compute({index});\n"))
            .collect::<String>(),
    );
    let (text_json, blob_json, blob_frame) = measure(&source);
    let blob_total = blob_json + blob_frame;
    assert!(
        blob_json * 4 < text_json,
        "the JSON should keep only the envelope: {blob_json} B vs {text_json} B"
    );
    assert!(
        blob_total < text_json,
        "the compact frame must win on total bytes: {blob_total} B vs {text_json} B"
    );

    // Escape-dense text wins by more: every tab, quote, and backslash costs two
    // JSON bytes and one frame byte.
    let dense =
        Rope::from((0..200).map(|_| "\t\"quoted\\ \"again\"\t\n".repeat(3)).collect::<String>());
    let (dense_text_json, dense_blob_json, dense_blob_frame) = measure(&dense);
    let dense_blob_total = dense_blob_json + dense_blob_frame;
    assert!(
        dense_blob_total * 4 < dense_text_json * 3,
        "escape-dense rows should win by at least a quarter: {dense_blob_total} B vs \
         {dense_text_json} B"
    );
    println!(
        "source rows: text {text_json} B vs blob {blob_json} B json + {blob_frame} B frame \
         = {blob_total} B ({}%); escape-dense rows: text {dense_text_json} B vs blob \
         {dense_blob_total} B ({}%)",
        blob_total * 100 / text_json,
        dense_blob_total * 100 / dense_text_json
    );
}

#[test]
fn empty_rows_carry_zero_length_slices_and_still_send_a_frame() {
    // Regression: an empty document renders a row with no bytes, i.e. a
    // zero-length slice. The frame must still be sent (empty), or the frontend
    // waits for bytes that never arrive.
    let text = Rope::from("");
    let (rows, used_blob, blob) = render_rows(&text, true);

    assert!(used_blob, "an empty row still references the blob");
    assert_eq!(blob.as_deref(), Some(&[1u8, 0][..]), "the frame is sent: one row, zero bytes");
    assert!(rows.iter().all(String::is_empty), "rows decode as empty: {rows:?}");

    // A window of blank lines keeps the same carrier contract: every row decodes
    // to what the per-line carrier would have sent.
    let blank = Rope::from("\n\n");
    let (blob_rows, used_blob, blob) = render_rows(&blank, true);
    let (text_rows, _, _) = render_rows(&blank, false);
    assert!(used_blob);
    assert_eq!(
        blob.as_deref(),
        Some(&[3u8, 1, 1, 0, 10, 10][..]),
        "two newline rows plus the trailing empty row"
    );
    assert_eq!(blob_rows, text_rows);
}
