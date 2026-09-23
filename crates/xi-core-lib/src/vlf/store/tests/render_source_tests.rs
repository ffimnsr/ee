//! VLF store tests: `RenderSource` surface (Stage A Phase 2).
use super::*;

use crate::text_store::{LogicalLine, ReadResult, RenderLineCount, RenderSource};

/// Fully scanned store: every lookup is exact.
#[test]
fn render_source_exact_lookups_match_text_store() {
    let content = b"alpha\nbeta gamma\ndelta\nepsilon";
    let (store, _f) = store_from(content);
    store.scan_all().unwrap();

    // len + line count mirror the TextStore contract.
    assert_eq!(RenderSource::len_bytes(&store), content.len());
    match RenderSource::total_lines(&store) {
        RenderLineCount::Exact(n) => assert_eq!(n, 4),
        other => panic!("fully scanned store must be Exact, got {other:?}"),
    }

    // line_to_byte delegates to the TextStore lookup.
    for line in 0..4u64 {
        match crate::text_store::TextStore::line_to_byte(&store, LogicalLine(line)) {
            LineLookup::Exact(offset) => {
                assert_eq!(RenderSource::line_to_byte(&store, line), LineLookup::Exact(offset));
            }
            other => panic!("expected Exact, got {other:?}"),
        }
    }

    // byte_to_line round-trips; EOF clamps to the final line like the rope
    // source (the TextStore lookup itself returns None at `byte == len`).
    for (line, offset) in [(0u64, 0usize), (1, 6), (2, 17), (3, 23)] {
        assert_eq!(RenderSource::byte_to_line(&store, offset), Some(line));
    }
    assert_eq!(RenderSource::byte_to_line(&store, content.len() - 1), Some(3));
    assert_eq!(RenderSource::byte_to_line(&store, content.len()), Some(3), "EOF clamps");
    assert_eq!(RenderSource::byte_to_line(&store, content.len() + 10), Some(3));

    assert_eq!(RenderSource::index_progress(&store), 1.0);
    assert!(RenderSource::as_rope(&store).is_none());
}

/// `read_range` serves exactly the requested line intervals, clipped out of
/// the seam-decoded window (multibyte content included).
#[test]
fn render_source_read_range_clips_seam_window() {
    let content = b"h\xC3\xA9llo\nw\xC3\xB6rld\n\xF0\x9F\x98\x80\n";
    let (store, _f) = store_from(content);
    store.scan_all().unwrap();
    let full = String::from_utf8_lossy(content).into_owned();

    // Line-interval boundaries: h(0) é(1-2) llo(3-5) \n(6) | w(7) ö(8-9) rld(10-12)
    // \n(13) | 😀(14-17) \n(18).
    for (start, end) in [(0, 7), (7, 14), (14, 19), (0, 19)] {
        match RenderSource::read_range(&store, start, end) {
            ReadResult::Ready(text) => {
                assert_eq!(text, full[start..end], "range [{start}..{end}) must be exact");
            }
            other => panic!("range [{start}..{end}) on scanned store: {other:?}"),
        }
    }

    // Mid-codepoint requests snap like the seam decoder: start back, end
    // forward — never a panic, never a partial char.
    match RenderSource::read_range(&store, 15, 17) {
        ReadResult::Ready(text) => assert_eq!(text, full[14..18], "snapped to full emoji"),
        other => panic!("scanned store must return Ready, got {other:?}"),
    }
}

/// Empty ranges decode nothing and clamps are honored.
#[test]
fn render_source_read_range_clamps_and_empty() {
    let content = b"abc\ndef\n";
    let (store, _f) = store_from(content);
    store.scan_all().unwrap();

    match RenderSource::read_range(&store, 0, 0) {
        ReadResult::Ready(text) => assert_eq!(text, ""),
        other => panic!("empty range must be Ready, got {other:?}"),
    }
    match RenderSource::read_range(&store, 20, 100) {
        ReadResult::Ready(text) => assert_eq!(text, ""),
        other => panic!("clamped past EOF must be Ready(empty), got {other:?}"),
    }

    // A wider request with the same start as a cached narrow decode must be
    // served fresh, not answered from the partial cache entry.
    match RenderSource::read_range(&store, 0, 3) {
        ReadResult::Ready(text) => assert_eq!(text, "abc"),
        other => panic!("narrow first read must be Ready, got {other:?}"),
    }
    match RenderSource::read_range(&store, 0, 8) {
        ReadResult::Ready(text) => assert_eq!(text, "abc\ndef\n"),
        other => panic!("wider same-start read must be Ready, got {other:?}"),
    }
    match RenderSource::read_range(&store, 0, 3) {
        ReadResult::Ready(text) => assert_eq!(text, "abc", "narrow re-read still exact"),
        other => panic!("narrow re-read must be Ready, got {other:?}"),
    }
}

/// Uns scanned / partially scanned index surfaces as approximate/pending like
/// the TextStore contract does.
#[test]
fn render_source_lines_incomplete_index() {
    // No scan yet: line count maps Unknown -> Approximate(0).
    let (store, _f) = store_from(b"a\nb\nc\n");
    match RenderSource::total_lines(&store) {
        RenderLineCount::Approximate(n) => assert_eq!(n, 0),
        other => panic!("unscanned store must be Approximate, got {other:?}"),
    }

    // Line lookups beyond the scanned region stay Pending/Approximate.
    match crate::text_store::TextStore::line_to_byte(&store, LogicalLine(100)) {
        LineLookup::Pending => {}
        other => panic!("expected Pending beyond scanned region, got {other:?}"),
    }
    assert!(RenderSource::index_progress(&store) < 1.0);
}

/// A partially scanned store shows an extrapolated approximate count.
#[test]
fn render_source_total_lines_approximate_partial_scan() {
    let content = (0..128).map(|i| format!("line {i}\n")).collect::<String>();
    let (store, _f) = store_from(content.as_bytes());
    // Scan only the first page (page_size 64 => first 64 bytes).
    store.scan_page_at(0).unwrap();

    match RenderSource::total_lines(&store) {
        RenderLineCount::Approximate(n) => {
            assert!(n > 0, "extrapolated estimate must be non-zero");
        }
        other => panic!("partial scan must be Approximate, got {other:?}"),
    }
}
