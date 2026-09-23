//! VLF store tests: overlay.
use super::*;

#[test]
fn edit_permission_is_forbidden_for_vlf() {
    let (store, _f) = store_from(b"hello");
    assert_eq!(store.edit_permission(), EditPermission::Forbidden { reason: VLF_READ_ONLY_REASON });
}

#[test]
fn copy_and_search_work_regardless_of_edit_permission() {
    // read_byte_range, iter_chunks, and line/byte lookups must succeed even
    // when edit_permission() returns Forbidden.
    let content = b"line one\nline two\nline three\n";
    let (store, _f) = store_from(content);
    store.scan_all().unwrap();

    assert_eq!(store.edit_permission(), EditPermission::Forbidden { reason: VLF_READ_ONLY_REASON });
    // Navigation/search still works.
    assert_eq!(store.line_to_byte(LogicalLine(0)), LineLookup::Exact(ByteOffset(0)));
    match store.read_byte_range(ByteRange::new(0, 8)) {
        TextChunkResult::Ready(c) => assert_eq!(c.text, "line one"),
        other => panic!("expected Ready, got {:?}", other),
    }
    let chunks: Vec<_> = store.iter_chunks(ByteRange::new(0, 8)).collect();
    assert!(!chunks.is_empty());
}

#[test]
fn overlay_reads_and_line_lookups_include_inserted_text() {
    let (store, _f) = store_from(b"alpha\nbeta\n");
    store.enable_editing();
    let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
    store.apply_insert(2, "XYZ", ctx).unwrap();

    assert_eq!(store.len_bytes(), 14);

    match store.read_byte_range(ByteRange::new(0, store.len_bytes())) {
        TextChunkResult::Ready(chunk) => assert_eq!(chunk.text, "alXYZpha\nbeta\n"),
        other => panic!("expected Ready, got {:?}", other),
    }

    assert_eq!(store.line_to_byte(LogicalLine(0)), LineLookup::Exact(ByteOffset(0)));
    assert_eq!(store.line_to_byte(LogicalLine(1)), LineLookup::Exact(ByteOffset(9)));
    assert_eq!(store.byte_to_line(ByteOffset(4)), Some(LogicalLine(0)));
    assert_eq!(store.byte_to_line(ByteOffset(11)), Some(LogicalLine(1)));

    let chunks: Vec<_> = store.iter_chunks(ByteRange::new(0, store.len_bytes())).collect();
    assert_eq!(chunks.len(), 1);
    match &chunks[0] {
        TextChunkResult::Ready(chunk) => assert_eq!(chunk.text, "alXYZpha\nbeta\n"),
        other => panic!("expected Ready, got {:?}", other),
    }
}

#[test]
fn overlay_line_cursor_stays_correct_ascending_with_crlf() {
    // Regression: overlay_line_to_byte used to walk from byte 0 on every
    // call (O(bytes-to-line) per lookup — multi-second mid-file renders).
    // The resumable cursor must keep exact answers, including CRLF merging.
    let content = (0..400).map(|i| format!("line {i:03}\r\n")).collect::<String>();
    let (store, _f) = store_from(content.as_bytes());
    store.enable_editing();
    let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
    store.apply_insert(3, "XX", ctx).unwrap();

    let overlay_text = match store.read_byte_range(ByteRange::new(0, store.len_bytes())) {
        TextChunkResult::Ready(chunk) => chunk.text,
        other => panic!("expected Ready, got {other:?}"),
    };
    let expected_starts: Vec<u64> = overlay_text
        .split_inclusive('\n')
        .scan(0u64, |acc, line| {
            let start = *acc;
            *acc += line.len() as u64;
            Some(start)
        })
        .collect();

    // Ascending lookups resume through the cursor; every answer must match
    // the byte offsets in the overlay text.
    for (line, expected) in expected_starts.iter().copied().enumerate() {
        assert_eq!(
            store.line_to_byte(LogicalLine(line as u64)),
            LineLookup::Exact(ByteOffset(expected)),
            "line {line}"
        );
    }
    // Out-of-range after the cursor advanced (line 400 is the trailing empty
    // line at EOF; 401 is past it).
    assert!(matches!(
        store.line_to_byte(LogicalLine(expected_starts.len() as u64 + 1)),
        LineLookup::OutOfRange
    ));
}

#[test]
fn overlay_line_cursor_rewinds_correctly_after_edit() {
    // An edit invalidates the cursor; a rewind (descending) lookup must also
    // resolve exactly.
    let content = b"alpha\nbeta\ngamma\ndelta\n";
    let (store, _f) = store_from(content);
    store.enable_editing();
    let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };

    // Advance the cursor deep into the file (line starts: 0, 6, 11, 17).
    assert_eq!(store.line_to_byte(LogicalLine(3)), LineLookup::Exact(ByteOffset(17)));
    assert_eq!(store.line_to_byte(LogicalLine(2)), LineLookup::Exact(ByteOffset(11)));

    // Edit near the head; line starts shift +1, the stale cursor must not leak.
    store.apply_insert(1, "X", ctx).unwrap();
    assert_eq!(store.line_to_byte(LogicalLine(1)), LineLookup::Exact(ByteOffset(7)));
    assert_eq!(store.line_to_byte(LogicalLine(0)), LineLookup::Exact(ByteOffset(0)));
    assert_eq!(store.line_to_byte(LogicalLine(2)), LineLookup::Exact(ByteOffset(12)));
    assert_eq!(store.line_to_byte(LogicalLine(3)), LineLookup::Exact(ByteOffset(18)));
}

#[test]
fn unedited_overlay_uses_base_index_fast_path_exactly() {
    // Regression: the unedited fast path must answer identically to the
    // overlay walk for \n-only and merged \r\n files (line starts sit after
    // the `\n` in both). Editing enabled but nothing inserted: overlay is
    // active, base-index answers are exact.
    let content = (0..200).map(|i| format!("line {i:03}\r\n")).collect::<String>();
    let (store, _f) = store_from(content.as_bytes());
    store.enable_editing();

    for &start in &[0u64, 29, 87, 199] {
        let expected: u64 = (0..start).map(|i| format!("line {i:03}\r\n").len() as u64).sum();
        assert_eq!(
            store.line_to_byte(LogicalLine(start)),
            LineLookup::Exact(ByteOffset(expected)),
            "line {start}"
        );
    }
    // byte → line through the same fast path.
    let mid = content.find("line 100").unwrap() as u64;
    assert_eq!(store.byte_to_line(ByteOffset(mid)), Some(LogicalLine(100)));
}

#[test]
fn lone_cr_prefix_disables_base_fast_path() {
    // A lone `\r` in the file prefix must be sniffed before the fast path is
    // trusted: overlay line 1 starts after the `\r`, not after the later `\n`.
    let content = b"a\rb\nc\n";
    let (store, _f) = store_from(content);
    store.enable_editing();

    assert_eq!(store.line_to_byte(LogicalLine(1)), LineLookup::Exact(ByteOffset(2)));
    assert_eq!(store.line_to_byte(LogicalLine(2)), LineLookup::Exact(ByteOffset(4)));
    // byte → line must also walk (base would say line 0 for the `\n`).
    assert_eq!(store.byte_to_line(ByteOffset(3)), Some(LogicalLine(1)));
}

#[test]
fn overlay_fast_path_serves_mid_file_lookup_from_base_index() {
    // Unedited overlay + fully scanned base: mid-file lookups must be served
    // by the page index (one page read), not by the byte walk (whole file).
    let content = (0..40_000).map(|i| format!("line {i:05}\n")).collect::<String>();
    let mut f = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut f, content.as_bytes()).unwrap();
    std::io::Write::flush(&mut f).unwrap();
    let store = VlfStore::open_with_config(f.path(), 16 * 1024, 1024 * 1024).unwrap();
    store.scan_all().unwrap();
    store.enable_editing();

    let before = store.stats.borrow().bytes_before_first_viewport;
    assert_eq!(store.line_to_byte(LogicalLine(20_000)), LineLookup::Exact(ByteOffset(220_000)));
    let extra = store.stats.borrow().bytes_before_first_viewport - before;
    assert!(
        extra < content.len() as u64,
        "mid-file lookup read {extra} B; the walk would read the whole file"
    );
}

#[test]
fn overlay_out_of_range_answered_without_full_walk() {
    // Regression: lookups beyond EOF used to fall through to the
    // O(remaining-file) walk before reporting OutOfRange (the tail-jump
    // sentinel scrolled ~100 ms per request at 64 MiB). With a complete base
    // index the unedited overlay must answer directly.
    let content = (0..40_000).map(|i| format!("line {i:05}\n")).collect::<String>();
    let mut f = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut f, content.as_bytes()).unwrap();
    std::io::Write::flush(&mut f).unwrap();
    let store = VlfStore::open_with_config(f.path(), 16 * 1024, 1024 * 1024).unwrap();
    store.scan_all().unwrap();
    store.enable_editing();

    let before = store.stats.borrow().bytes_before_first_viewport;
    assert!(before > 0, "precondition: scan_all must have counted reads");
    assert!(matches!(store.line_to_byte(LogicalLine(1_000_000)), LineLookup::OutOfRange));
    let after = store.stats.borrow().bytes_before_first_viewport;
    assert_eq!(after, before, "OutOfRange must not walk the file");
}

#[test]
fn mid_file_line_iteration_is_window_bounded() {
    // The render hot path: per line, two lookups + one interval read at the
    // middle of a multi-page file. Must stay window-bounded (any page re-scan
    // or walk per lookup breaks this test's byte-read accounting, and regressed
    // renders to hundreds of milliseconds at 1 MiB pages).
    let content = (0..100_000).map(|i| format!("line {i:06}\n")).collect::<String>();
    let mut f = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut f, content.as_bytes()).unwrap();
    std::io::Write::flush(&mut f).unwrap();
    let store = VlfStore::open_with_config(f.path(), 1024 * 1024, 64 * 1024 * 1024).unwrap();
    store.scan_all().unwrap();
    store.enable_editing();

    let before = store.stats.borrow().bytes_before_first_viewport;
    let mid = 50_000u64;
    let mut last = None;
    for l in mid..mid + 200 {
        let a = store.line_to_byte(LogicalLine(l));
        let b = store.line_to_byte(LogicalLine(l + 1));
        let (a, b) = match (a, b) {
            (LineLookup::Exact(x), LineLookup::Exact(y)) => (x.0, y.0),
            other => panic!("non-exact at {l}: {other:?}"),
        };
        last = Some((a, b));
    }
    let extra = store.stats.borrow().bytes_before_first_viewport - before;
    // One initial page read (~1 MiB) plus the per-line resume reads (tiny);
    // a per-lookup page re-scan or full walk would blow well past this.
    assert!(
        extra < content.len() as u64,
        "mid-file window read {extra} B; lookups must stay page-resumed"
    );
    let (a, b) = last.unwrap();
    // Sanity: 12-byte lines; b - a must be the line length.
    assert_eq!(b - a, 12);
}

#[test]
fn overlay_search_reads_include_inserted_text() {
    let (store, _f) = store_from(b"alpha\nbeta\n");
    store.enable_editing();
    let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
    store.apply_insert(5, " plus", ctx).unwrap();

    let token = store.pager.current_generation();
    let chunk = store.read_search_range(ByteRange::new(0, store.len_bytes()), token).unwrap();
    assert_eq!(chunk.text, "alpha plus\nbeta\n");
}
