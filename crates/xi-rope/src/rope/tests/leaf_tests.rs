//! Rope tests: leaf splits, crlf seams, and chunk lookups.
use super::*;

#[test]
fn find_leaf_split_for_merge_prefers_newline_boundary() {
    let text = format!("{}\n{}", "a".repeat(MAX_LEAF - 8), "b".repeat(MIN_LEAF + 32));

    let splitpoint = find_leaf_split_for_merge(&text);

    assert!(text[..splitpoint].ends_with('\n'));
}

#[test]
fn find_leaf_split_for_merge_avoids_crlf_boundary() {
    let text = format!("{}\r\n{}", "a".repeat(MAX_LEAF - 1), "b".repeat(MIN_LEAF + 32));

    let splitpoint = find_leaf_split_for_merge(&text);

    assert!(!is_crlf_split_point(&text, splitpoint));
    assert!(splitpoint >= max(MIN_LEAF, text.len() - MAX_LEAF));
    assert!(splitpoint <= min(MAX_LEAF, text.len() - MIN_LEAF));
}

#[test]
fn utf16_code_units_metric() {
    let rope = Rope::from("hi\ni'm\nfour\nlines");
    let utf16_units = rope.measure::<Utf16CodeUnitsMetric>();
    assert_eq!(utf16_units, 17);

    // position after 'f' in four
    let utf8_offset = 9;
    let utf16_units = rope.count::<Utf16CodeUnitsMetric>(utf8_offset);
    assert_eq!(utf16_units, 9);

    let utf8_offset = rope.count_base_units::<Utf16CodeUnitsMetric>(utf16_units);
    assert_eq!(utf8_offset, 9);

    let rope_with_emoji = Rope::from("hi\ni'm\n😀 four\nlines");
    let utf16_units = rope_with_emoji.measure::<Utf16CodeUnitsMetric>();

    assert_eq!(utf16_units, 20);

    // position after 'f' in four
    let utf8_offset = 13;
    let utf16_units = rope_with_emoji.count::<Utf16CodeUnitsMetric>(utf8_offset);
    assert_eq!(utf16_units, 11);

    let utf8_offset = rope_with_emoji.count_base_units::<Utf16CodeUnitsMetric>(utf16_units);
    assert_eq!(utf8_offset, 13);

    //for next line
    let utf8_offset = 19;
    let utf16_units = rope_with_emoji.count::<Utf16CodeUnitsMetric>(utf8_offset);
    assert_eq!(utf16_units, 17);

    let utf8_offset = rope_with_emoji.count_base_units::<Utf16CodeUnitsMetric>(utf16_units);
    assert_eq!(utf8_offset, 19);
}

fn two_leaf_rope(left: &str, right: &str) -> Rope {
    Rope::from(left) + Rope::from(right)
}

fn assert_no_crlf_chunk_split(rope: &Rope) {
    let boundaries = chunk_boundaries(rope);
    for window in boundaries.windows(2) {
        assert!(
            !(window[0].1.ends_with('\r') && window[1].1.starts_with('\n')),
            "chunk boundary split CRLF at byte {}",
            window[1].0
        );
    }
}

fn assert_crlf_line_metrics(rope: &Rope, expected: &str, line_break: usize) {
    assert_eq!(String::from(rope), expected);
    assert_eq!(rope.lines(..).collect::<Vec<_>>(), expected.lines().collect::<Vec<_>>());
    assert_eq!(rope.measure::<LinesMetric>(), count_newlines(expected));
    assert_eq!(rope.offset_of_line(1), line_break + 2);
    assert_eq!(rope.line_of_offset(line_break), 0);
    assert_eq!(rope.line_of_offset(line_break + 1), 0);
    assert_eq!(rope.line_of_offset(line_break + 2), 1);
}

#[test]
fn chunk_at_offset_empty_rope() {
    let rope = Rope::from("");
    assert_eq!(rope.chunk_at_offset(0), Some(("", 0, 0, 0)));
    assert_eq!(rope.chunk_at_line(0), Some(("", 0, 0, 0)));
    assert_eq!(rope.chunk_at_utf16(0), Some(("", 0, 0, 0)));
    assert_eq!(rope.chunk_at_offset(1), None);
}

#[test]
fn chunk_at_offset_reports_leaf_boundary_metrics() {
    let left = "a".repeat(MAX_LEAF);
    let right = format!("{}{}", "b".repeat(MAX_LEAF), "\nccc");
    let rope = two_leaf_rope(&left, &right);
    let boundaries = chunk_boundaries(&rope);
    assert!(boundaries.len() >= 2, "expected multi-leaf rope");

    let (boundary, expected_chunk) = &boundaries[1];
    let located = rope.chunk_at_offset(*boundary).expect("chunk at boundary");
    assert_eq!(located.0, expected_chunk);
    assert_eq!(located.1, *boundary);
    assert_eq!(located.2, rope.line_of_offset(*boundary));
    assert_eq!(located.3, rope.count::<Utf16CodeUnitsMetric>(*boundary));
}

#[test]
fn chunk_at_line_handles_crlf_seam_across_leaves() {
    let left = format!("{}\r", "a".repeat(MIN_LEAF + 32));
    let right = format!("\n{}", "b".repeat(MIN_LEAF + 32));
    let rope = two_leaf_rope(&left, &right);
    let located = rope.chunk_at_offset(left.len()).expect("chunk at CRLF seam");
    assert_eq!(located.0, right);
    assert_eq!(located.1, left.len());
    assert_eq!(located.2, 0);
    assert_eq!(rope.chunk_at_line(1).expect("line 1 chunk").1, left.len());
    assert_eq!(rope.chunk_at_line(1).expect("line 1 chunk").2, 0);
}

#[test]
fn rope_builder_preserves_crlf_line_metrics_near_leaf_boundary() {
    let line_break = MAX_LEAF - 1;
    let text = format!("{}\r\n{}", "a".repeat(line_break), "b".repeat(MIN_LEAF + 32));
    let rope = Rope::from(text.as_str());

    assert_no_crlf_chunk_split(&rope);
    assert_crlf_line_metrics(&rope, &text, line_break);
}

#[test]
fn edit_preserves_crlf_line_metrics_at_edit_boundary() {
    let line_break = MAX_LEAF - 1;
    let suffix = "b".repeat(MIN_LEAF + 32);
    let mut rope = Rope::from(format!("{}\n{}", "a".repeat(line_break), suffix));
    rope.edit(line_break..line_break, "\r");
    let expected = format!("{}\r\n{}", "a".repeat(line_break), suffix);

    assert_no_crlf_chunk_split(&rope);
    assert_crlf_line_metrics(&rope, &expected, line_break);
}

#[test]
fn chunk_at_utf16_tracks_multibyte_leaf_boundary() {
    let left = "a".repeat(MIN_LEAF + 32);
    let right = format!("{}{}", "é".repeat(MIN_LEAF + 16), "🙂");
    let rope = two_leaf_rope(&left, &right);
    let byte_start = left.len();
    let utf16_start = rope.count::<Utf16CodeUnitsMetric>(byte_start);
    let expected = rope.chunk_at_offset(byte_start).expect("chunk at byte boundary");

    let located = rope.chunk_at_utf16(utf16_start).expect("chunk at utf16 boundary");
    assert_eq!(located.0, expected.0);
    assert_eq!(located.1, expected.1);
    assert_eq!(located.2, rope.line_of_offset(byte_start));
    assert_eq!(located.3, utf16_start);
}
