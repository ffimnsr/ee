//! VLF store tests: read.
use super::*;

#[test]
fn mode_is_vlf() {
    let (store, _f) = store_from(b"hello");
    assert_eq!(store.mode(), DocumentMode::Vlf);
}

#[test]
fn full_text_policy_is_forbidden() {
    let (store, _f) = store_from(b"hello");
    assert_eq!(store.full_text_policy(), FullTextPolicy::Forbidden);
}

#[test]
fn read_full_text_returns_unsupported() {
    let (store, _f) = store_from(b"hello world");
    assert_eq!(store.read_full_text(), TextChunkResult::Unsupported);
}

#[test]
fn count_lf_streaming_matches_wc_l_semantics() {
    let (store, _f) = store_from(b"alpha\nbeta\ngamma\n");
    assert_eq!(store.count_lf_streaming().unwrap(), 3);

    let (store, _f) = store_from(b"alpha\nbeta\ngamma");
    assert_eq!(store.count_lf_streaming().unwrap(), 2);
}

#[test]
fn len_bytes_matches_content() {
    let content = b"hello world\n";
    let (store, _f) = store_from(content);
    assert_eq!(store.len_bytes(), content.len() as u64);
}

#[test]
fn known_line_count_unknown_before_scan() {
    let (store, _f) = store_from(b"a\nb\nc");
    assert_eq!(store.known_line_count(), KnownLineCount::Unknown);
}

#[test]
fn known_line_count_exact_after_full_scan() {
    let content = b"a\nb\nc";
    let (store, _f) = store_from(content);
    store.scan_all().unwrap();
    match store.known_line_count() {
        KnownLineCount::Exact(n) => assert_eq!(n, 3),
        other => panic!("expected Exact, got {:?}", other),
    }
}

#[test]
fn known_line_count_trailing_newline() {
    let content = b"a\nb\n";
    let (store, _f) = store_from(content);
    store.scan_all().unwrap();
    match store.known_line_count() {
        KnownLineCount::Exact(n) => assert_eq!(n, 3),
        other => panic!("expected Exact, got {:?}", other),
    }
}

// ---- read_byte_range --------------------------------------------

#[test]
fn read_byte_range_ascii() {
    let content = b"hello world";
    let (store, _f) = store_from(content);
    match store.read_byte_range(ByteRange::new(6, 11)) {
        TextChunkResult::Ready(chunk) => assert_eq!(chunk.text, "world"),
        other => panic!("expected Ready, got {:?}", other),
    }
}

#[test]
fn read_byte_range_empty() {
    let (store, _f) = store_from(b"hello");
    match store.read_byte_range(ByteRange::new(2, 2)) {
        TextChunkResult::Ready(chunk) => assert!(chunk.text.is_empty()),
        other => panic!("expected Ready, got {:?}", other),
    }
}

#[test]
fn read_byte_range_out_of_bounds() {
    let (store, _f) = store_from(b"hi");
    assert_eq!(store.read_byte_range(ByteRange::new(0, 99)), TextChunkResult::Unsupported);
}

#[test]
fn read_byte_range_multibyte_seam_adjustment() {
    // "café": c(1) a(1) f(1) é(2 bytes U+00E9: 0xC3 0xA9)
    // Requesting [3, 4] splits the 2-byte 'é' codepoint.
    // The seam adjustment must include the full 'é'.
    let content = "café".as_bytes();
    let (store, _f) = store_from(content);
    match store.read_byte_range(ByteRange::new(3, 4)) {
        TextChunkResult::Ready(chunk) => {
            // The adjusted range should include 'é' fully.
            assert!(chunk.text.contains('é') || chunk.text == "é" || chunk.byte_range.len() >= 2);
        }
        other => panic!("expected Ready, got {:?}", other),
    }
}

// ---- line_to_byte / byte_to_line --------------------------------

#[test]
fn line_to_byte_line_zero_exact_without_scan() {
    // Line 0 always maps to byte 0 for non-empty files; no scanning needed.
    let (store, _f) = store_from(b"a\nb\nc");
    assert_eq!(store.line_to_byte(LogicalLine(0)), LineLookup::Exact(ByteOffset(0)));
}

#[test]
fn line_to_byte_non_zero_approximate_before_full_scan() {
    // Line 2 is not in the scanned prefix; expect Approximate (not Pending)
    // once at least one page is scanned so the interpolation has data.
    let content = b"hello\nworld\nfoo";
    let (store, _f) = store_from(content);
    store.scan_page_at(0).unwrap(); // scan first (and only) page
    match store.line_to_byte(LogicalLine(2)) {
        LineLookup::Exact(_) | LineLookup::Approximate(_) => {}
        other => panic!("expected Exact or Approximate before full scan, got {:?}", other),
    }
}

#[test]
fn line_to_byte_approximate_before_scan_with_data() {
    // Before scanning, line 1 of a multi-page file returns Approximate (with
    // data from at least partial scan) or Pending when no scan data exists.
    let content = b"line1\nline2\nline3";
    let (store, _f) = store_from(content);
    // No pages scanned yet: line 1 is Pending (no interpolation data).
    match store.line_to_byte(LogicalLine(1)) {
        LineLookup::Pending | LineLookup::Approximate(_) => {}
        other => panic!("expected Pending or Approximate, got {:?}", other),
    }
    // Scan the single page; now line 1 is resolvable as Exact.
    store.scan_page_at(0).unwrap();
    assert_eq!(store.line_to_byte(LogicalLine(1)), LineLookup::Exact(ByteOffset(6)));
}

#[test]
fn line_to_byte_exact_after_scan() {
    let content = b"hello\nworld\nfoo";
    let (store, _f) = store_from(content);
    store.scan_all().unwrap();
    assert_eq!(store.line_to_byte(LogicalLine(0)), LineLookup::Exact(ByteOffset(0)));
    assert_eq!(store.line_to_byte(LogicalLine(1)), LineLookup::Exact(ByteOffset(6)));
    assert_eq!(store.line_to_byte(LogicalLine(2)), LineLookup::Exact(ByteOffset(12)));
}

#[test]
fn line_to_byte_out_of_range_after_full_scan() {
    let content = b"a\nb";
    let (store, _f) = store_from(content);
    store.scan_all().unwrap();
    assert_eq!(store.line_to_byte(LogicalLine(99)), LineLookup::OutOfRange);
}

#[test]
fn approximate_goto_line_with_partial_index() {
    // Build a file large enough to span multiple 64-byte pages.
    // Each line is "line_NNNNN\n" (10 bytes); 200 lines = 2000 bytes → ~31 pages.
    let mut content = Vec::new();
    for i in 0u32..200 {
        content.extend_from_slice(format!("line_{:05}\n", i).as_bytes());
    }
    let (store, _f) = store_from(&content);
    // Scan only the first page (bytes 0..64).
    store.scan_page_at(0).unwrap();

    // Line 0: always Exact.
    assert_eq!(store.line_to_byte(LogicalLine(0)), LineLookup::Exact(ByteOffset(0)));

    // Line 100 (mid-file): index can't resolve it exactly yet, but should
    // return Approximate rather than Pending once we have scan data.
    match store.line_to_byte(LogicalLine(100)) {
        LineLookup::Approximate(ByteOffset(off)) => {
            // Offset should be somewhere in the middle of the file, not 0
            // and not past the end.
            assert!(off > 0, "approximate offset should be > 0 for line 100");
            assert!(off <= content.len() as u64, "approximate offset must not exceed file size");
        }
        // Exact is acceptable if the index happened to cover it.
        LineLookup::Exact(_) => {}
        other => panic!("expected Approximate or Exact, got {:?}", other),
    }

    // After full scan, the result must be Exact.
    store.scan_all().unwrap();
    let expected_byte = 100 * 11; // "line_{:05}\n" = 11 bytes per line
    assert_eq!(store.line_to_byte(LogicalLine(100)), LineLookup::Exact(ByteOffset(expected_byte)));
}

#[test]
fn viewport_first_line_lookup_resolves_within_scanned_prefix() {
    // File spans multiple pages; scan only the first page which covers
    // the first few lines.  Those lines should resolve as Exact, while
    // lines beyond the scanned prefix return Approximate (not Pending).
    let mut content = Vec::new();
    for i in 0u32..200 {
        content.extend_from_slice(format!("line_{:05}\n", i).as_bytes());
    }
    let (store, _f) = store_from(&content);
    store.scan_page_at(0).unwrap();

    // Lines within the first 64-byte page are Exact.
    // Page size=64; each line is 11 bytes: lines 0..=4 fit (55 bytes), line 5 partly.
    assert_eq!(store.line_to_byte(LogicalLine(0)), LineLookup::Exact(ByteOffset(0)));
    assert_eq!(store.line_to_byte(LogicalLine(1)), LineLookup::Exact(ByteOffset(11)));

    // Lines well beyond the scanned prefix: Approximate (not Pending).
    match store.line_to_byte(LogicalLine(150)) {
        LineLookup::Approximate(_) | LineLookup::Exact(_) => {}
        LineLookup::Pending => panic!("expected Approximate not Pending for line 150"),
        other => panic!("unexpected result {:?}", other),
    }
}
