//! VLF store tests: scan.
use super::*;

#[test]
fn byte_to_line_after_scan() {
    let content = b"hello\nworld\nfoo";
    let (store, _f) = store_from(content);
    store.scan_all().unwrap();
    assert_eq!(store.byte_to_line(ByteOffset(0)), Some(LogicalLine(0)));
    assert_eq!(store.byte_to_line(ByteOffset(6)), Some(LogicalLine(1)));
    assert_eq!(store.byte_to_line(ByteOffset(12)), Some(LogicalLine(2)));
}

#[test]
fn byte_to_line_out_of_range_returns_none() {
    let (store, _f) = store_from(b"hi");
    assert_eq!(store.byte_to_line(ByteOffset(99)), None);
}

// ---- iter_chunks ------------------------------------------------

#[test]
fn iter_chunks_covers_all_bytes() {
    let content = b"hello world";
    let (store, _f) = store_from(content);
    let chunks: String = store
        .iter_chunks(ByteRange::new(0, content.len() as u64))
        .map(|r| match r {
            TextChunkResult::Ready(c) => c.text,
            other => panic!("expected Ready, got {:?}", other),
        })
        .collect();
    assert_eq!(chunks.as_bytes(), content);
}

#[test]
fn iter_chunks_out_of_bounds_unsupported() {
    let (store, _f) = store_from(b"hi");
    let results: Vec<_> = store.iter_chunks(ByteRange::new(0, 99)).collect();
    assert_eq!(results, vec![TextChunkResult::Unsupported]);
}

// ---- scan_all + page index state --------------------------------

#[test]
fn scan_all_marks_progress_complete() {
    let content = b"line one\nline two\nline three\n";
    let (store, _f) = store_from(content);
    store.scan_all().unwrap();
    assert!(store.index().scan_progress().is_complete());
}

#[test]
fn scan_page_inserts_scanned_descriptor() {
    let content = b"abc\ndef\nghi\n";
    let (store, _f) = store_from(content);
    store.scan_page_at(0).unwrap();
    assert_eq!(store.index().len(), 1);
    let idx = store.index();
    let desc = idx.page_at_byte(0).unwrap();
    assert_eq!(desc.scan_state, ScanState::Scanned);
    assert_eq!(desc.newline_count, 3);
}

#[test]
fn no_rope_conversion_method_exists() {
    // Compile-time guard: VlfStore has no `to_rope` or similar method.
    // If someone adds one, this comment serves as documentation that it
    // violates the VLF core invariant.
    let content = b"data";
    let (store, _f) = store_from(content);
    // full_text_policy must be Forbidden; read_full_text must be Unsupported.
    assert_eq!(store.full_text_policy(), FullTextPolicy::Forbidden);
    assert_eq!(store.read_full_text(), TextChunkResult::Unsupported);
}
#[test]
fn analyse_bytes_counts_newlines() {
    let (nl, _, _, _) = analyse_bytes(b"a\nb\nc", false, false);
    assert_eq!(nl, 2);
}

#[test]
fn analyse_bytes_crlf_counted_once() {
    let (nl, _, _, _) = analyse_bytes(b"a\r\nb\r\n", false, false);
    assert_eq!(nl, 2);
}

#[test]
fn analyse_bytes_skips_leading_lf_when_seam() {
    // starts_with_lf_of_crlf=true means the leading \n must not count.
    let (nl, _, _, _) = analyse_bytes(b"\nb\n", true, false);
    assert_eq!(nl, 1);
}

#[test]
fn analyse_bytes_skips_trailing_cr_when_seam() {
    let (nl, _, _, _) = analyse_bytes(b"a\nb\r", false, true);
    assert_eq!(nl, 1); // only the \n counts; the \r is deferred
}

#[test]
fn analyse_bytes_prefix_suffix_lengths() {
    // "hello\nworld\nfoo"
    let b = b"hello\nworld\nfoo";
    let (_, _, prefix, suffix) = analyse_bytes(b, false, false);
    assert_eq!(prefix, 6); // "hello\n"
    assert_eq!(suffix, 3); // "foo"
}

#[test]
fn analyse_bytes_no_newlines() {
    let b = b"hello";
    let (nl, _, prefix, suffix) = analyse_bytes(b, false, false);
    assert_eq!(nl, 0);
    assert_eq!(prefix, 5);
    assert_eq!(suffix, 5);
}

// ---- ends_on_utf8_boundary helper -------------------------------

#[test]
fn utf8_boundary_ascii() {
    assert!(ends_on_utf8_boundary(b"hello"));
}

#[test]
fn utf8_boundary_multibyte_complete() {
    assert!(ends_on_utf8_boundary("café".as_bytes()));
}

#[test]
fn utf8_boundary_multibyte_incomplete() {
    // 'é' is 0xC3 0xA9; truncate to just 0xC3.
    assert!(!ends_on_utf8_boundary(&[0xC3]));
}

// ---- SeamResult preserves both ranges ---------------------------

#[test]
fn seam_result_preserves_original_range() {
    // 'é' = 0xC3 0xA9 at bytes [3,5) in "caféx".
    // Requesting [4,5) splits 'é'; seam must include full codepoint.
    let content = "caféx".as_bytes();
    let (store, _f) = store_from(content);
    let req = ByteRange::new(4, 5);
    let seam = store.read_with_seam(req).unwrap();
    // Original range is preserved exactly.
    assert_eq!(seam.original_range, req);
    // Decoded range covers the full 'é' (2 bytes starting at offset 3).
    assert!(seam.decoded_range.start.0 <= 3);
    assert!(seam.decoded_range.end.0 >= 5);
    assert!(seam.text.contains('é'));
}

#[test]
fn seam_result_unmodified_for_ascii_range() {
    let content = b"hello world";
    let (store, _f) = store_from(content);
    let req = ByteRange::new(6, 11);
    let seam = store.read_with_seam(req).unwrap();
    assert_eq!(seam.original_range, req);
    // ASCII — decoded range should equal requested range (no adjustment needed).
    assert_eq!(seam.decoded_range, req);
    assert_eq!(seam.text, "world");
}
#[test]
fn multibyte_at_page_boundary_read_includes_full_codepoint() {
    let (store, _f) = store_with_multibyte_at_boundary();
    // Request [3, 6) — exactly '€' bytes — should decode cleanly.
    match store.read_byte_range(ByteRange::new(3, 6)) {
        TextChunkResult::Ready(chunk) => assert!(chunk.text.contains('€')),
        other => panic!("expected Ready, got {:?}", other),
    }
}

#[test]
fn multibyte_split_at_page_boundary_seam_adjusts() {
    let (store, _f) = store_with_multibyte_at_boundary();
    // Request [2, 4): byte 2 = 'c', bytes 3-5 = '€'.
    // Seam adjustment must include the full '€' codepoint.
    let seam = store.read_with_seam(ByteRange::new(2, 4)).unwrap();
    assert!(seam.text.contains('€'), "decoded text must include '€': {:?}", seam.text);
    assert_ne!(seam.original_range, seam.decoded_range, "ranges should differ after adjustment");
}

#[test]
fn four_byte_codepoint_at_boundary_seam_adjusts() {
    // U+1F600 😀 = 0xF0 0x9F 0x98 0x80 (4 bytes)
    // page_size=4: page 0=[0,4)="abc\xF0", page 1=[4,8)="\x9F\x98\x80x"
    let content = b"abc\xF0\x9F\x98\x80x";
    let mut f = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut f, content).unwrap();
    std::io::Write::flush(&mut f).unwrap();
    let store = VlfStore::open_with_config(f.path(), 4, 1024 * 1024).unwrap();
    // Requesting [3,4) — first byte of 😀 — seam must expand to include all 4 bytes.
    let seam = store.read_with_seam(ByteRange::new(3, 4)).unwrap();
    assert!(seam.text.contains('😀'), "should include full 4-byte codepoint: {:?}", seam.text);
}
