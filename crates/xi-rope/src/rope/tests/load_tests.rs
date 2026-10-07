//! Rope tests: load-path construction (`Rope::from_owned`,
//! `RopeBuilder::push_owned`) and the ASCII fast path in
//! `RopeInfo::compute_info`.
use super::*;

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

fn assert_counters_match(text: &str, info: RopeInfo) {
    assert_eq!(info.lines, count_newlines(text), "lines mismatch for {text:?}");
    assert_eq!(info.chars, count_chars(text), "chars mismatch for {text:?}");
    assert_eq!(info.utf16_size, count_utf16_code_units(text), "utf16_size mismatch for {text:?}");
}

#[test]
fn compute_info_ascii_fast_path_matches_generic_counts() {
    for text in [
        "fn render_row(idx: usize) -> usize { idx * 42 + 7 }\n".repeat(8),
        "a".repeat(MAX_LEAF),
        "single-line-no-newline".to_owned(),
        "\n\n\n".to_owned(),
    ] {
        let info = <RopeInfo as NodeInfo>::compute_info(&text);
        assert_eq!(info.chars, text.len(), "ascii chars should equal byte length");
        assert_eq!(info.utf16_size, text.len(), "ascii utf16 should equal byte length");
        assert_counters_match(&text, info);
    }
}

#[test]
fn compute_info_mixed_utf8_keeps_generic_counts() {
    for text in [
        "αβγ🙂delta\r\nplain-line\n終わり\r\n".repeat(24),
        format!("ascii-prefix{}", "é".repeat(MAX_LEAF / 2)),
        "🙂".repeat(MIN_LEAF),
    ] {
        let info = <RopeInfo as NodeInfo>::compute_info(&text);
        assert_counters_match(&text, info);
    }
}

#[test]
fn from_owned_small_text_is_single_leaf_and_equivalent() {
    let text = "fn main() { println!(\"hello\"); }\n".repeat(16);
    assert!(text.len() <= MAX_LEAF);

    let owned = Rope::from_owned(text.clone());
    let borrowed = Rope::from(text.as_str());

    assert_matches_str(&owned, &text);
    assert_eq!(String::from(&owned), String::from(&borrowed));
    assert_eq!(owned.len_utf16_cu(), borrowed.len_utf16_cu());
    assert_eq!(owned.iter_chunks(..).count(), 1, "small owned string is one leaf");
}

#[test]
fn from_owned_large_text_matches_from_str() {
    let text = format!(
        "{}\n{}🙂{}",
        "a".repeat(MAX_LEAF - 24),
        "b".repeat(MAX_LEAF),
        "é".repeat(MIN_LEAF + 17)
    );
    let owned = Rope::from_owned(text.clone());
    let borrowed = Rope::from(text.as_str());

    assert_eq!(String::from(&owned), String::from(&borrowed));
    assert_matches_str(&owned, &text);
    assert!(owned.iter_chunks(..).count() >= 2, "large owned string spans leaves");
    assert!(
        owned.iter_chunks(..).all(|chunk| chunk.len() <= MAX_LEAF),
        "owned leaves respect MAX_LEAF"
    );
}

#[test]
fn push_owned_streams_multiple_pushes() {
    let mut builder = RopeBuilder::new();
    builder.push_owned("prefix-".to_owned());
    builder.push_owned("x".repeat(MAX_LEAF * 2));
    builder.push_owned("🙂tail".to_owned());
    let rope = builder.finish();

    let expected = format!("prefix-{}🙂tail", "x".repeat(MAX_LEAF * 2));
    assert_matches_str(&rope, &expected);
    assert_no_crlf_chunk_split(&rope);
}

#[test]
fn from_owned_preserves_crlf_near_leaf_boundary() {
    let line_break = MAX_LEAF - 1;
    let text = format!("{}\r\n{}", "a".repeat(line_break), "b".repeat(MIN_LEAF + 32));
    let rope = Rope::from_owned(text.clone());

    assert_no_crlf_chunk_split(&rope);
    assert_eq!(String::from(&rope), text);
    assert_eq!(rope.measure::<LinesMetric>(), 1);
    assert_eq!(rope.offset_of_line(1), line_break + 2);
}

#[test]
fn from_owned_empty_returns_empty_rope() {
    let rope = Rope::from_owned(String::new());
    assert_eq!(rope.len(), 0);
    assert_eq!(rope.measure::<LinesMetric>(), 0);
    assert_eq!(String::from(&rope), "");
}

#[test]
fn push_owned_empty_then_more_pushes_preserves_content() {
    let mut builder = RopeBuilder::new();
    builder.push_owned(String::new());
    builder.push_owned("tail".to_owned());
    let rope = builder.finish();
    assert_matches_str(&rope, "tail");
}

#[test]
fn from_owned_just_above_leaf_size_splits_into_two_leaves() {
    // No newlines: the bulk splitter falls back to a char-boundary clamp, so
    // a MAX_LEAF+1 string becomes 514 + 511 bytes instead of one oversized
    // leaf, and never exceeds MAX_LEAF per leaf.
    let text = "x".repeat(MAX_LEAF + 1);
    let rope = Rope::from_owned(text.clone());

    assert_matches_str(&rope, &text);
    assert_eq!(rope.iter_chunks(..).count(), 2);
    assert!(rope.iter_chunks(..).all(|chunk| chunk.len() <= MAX_LEAF));
}

#[test]
fn compute_info_single_ascii_byte_edge() {
    // ASCII control bytes (\r, \n, \0) must take the same fast path as
    // printable text without changing counters.
    for text in ["\r\n\0\t".to_owned(), "a\r\nb".to_owned()] {
        let info = <RopeInfo as NodeInfo>::compute_info(&text);
        assert_eq!(info.chars, text.len());
        assert_eq!(info.utf16_size, text.len());
        assert_counters_match(&text, info);
    }
}
