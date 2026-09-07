//! Rope tests: write_to, edit snapshots, and rope builders.
use super::*;

#[test]
fn try_edit_reports_bounds_error() {
    let mut rope = Rope::from("hello");
    assert_eq!(
        rope.try_edit(4..8, "!"),
        Err(RopeError::IntervalOutOfBounds { start: 4, end: 8, len: 5 })
    );
    assert_eq!(String::from(&rope), "hello");
}

#[test]
fn write_to_streams_full_rope() {
    let text = format!("{}{}", "a".repeat(1200), "b".repeat(1200));
    let rope = Rope::from(text.as_str());
    let mut bytes = Vec::new();

    rope.write_to(&mut bytes).unwrap();

    assert_eq!(String::from_utf8(bytes).unwrap(), text);
}

#[test]
fn write_to_propagates_partial_write_errors() {
    let text = format!("{}{}", "a".repeat(1200), "b".repeat(1200));
    let rope = Rope::from(text.as_str());
    let mut writer = FailingWriter {
        written: Vec::new(),
        remaining: 1300,
        fail_kind: io::ErrorKind::BrokenPipe,
    };

    let err = rope.write_to(&mut writer).unwrap_err();

    assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(writer.written, text.as_bytes()[..1300]);
}

#[test]
fn clone_edit_preserves_snapshot_and_shares_unchanged_chunks() {
    let text = format!("{}{}", "left-side-line\n".repeat(200), "right-side-line\n".repeat(200));
    let mut rope = Rope::from(text.as_str());
    let snapshot = rope.clone();
    let snapshot_chunk_ptrs = snapshot.iter_chunks(..).map(str::as_ptr).collect::<Vec<_>>();

    assert!(rope.ptr_eq(&snapshot));

    rope.edit(0..4, "LEFT");

    assert_eq!(String::from(&snapshot), text);
    assert!(!rope.ptr_eq(&snapshot));
    assert!(
        rope.iter_chunks(..).skip(1).any(|chunk| snapshot_chunk_ptrs.contains(&chunk.as_ptr())),
        "unchanged suffix chunks should remain shared after copy-on-write edit"
    );
}

#[test]
fn rope_builder_preserves_content_metrics_and_leaf_boundaries() {
    let text = format!(
        "{}\n{}🙂{}",
        "a".repeat(MAX_LEAF - 24),
        "b".repeat(MAX_LEAF),
        "é".repeat(MIN_LEAF + 17)
    );
    let expected = Rope::from(text.as_str());
    let mut builder = RopeBuilder::new();
    let mut start = 0;

    while start < text.len() {
        let mut end = (start + 137).min(text.len());
        end = clamp_to_char_boundary(&text, end);
        builder.push_str(&text[start..end]);
        start = end;
    }

    let built = builder.finish();
    let boundaries = chunk_boundaries(&built);

    assert_eq!(String::from(&built), text);
    assert_eq!(built.measure::<LinesMetric>(), expected.measure::<LinesMetric>());
    assert_eq!(built.measure::<Utf16CodeUnitsMetric>(), expected.measure::<Utf16CodeUnitsMetric>());
    assert!(boundaries.len() >= 2, "expected multi-leaf rope");
}

#[test]
fn rope_builder_append_streams_existing_rope() {
    let suffix_text = format!("{}{}", "suffix-".repeat(120), "🙂tail");
    let suffix = Rope::from(suffix_text.as_str());
    let mut builder = RopeBuilder::new();

    builder.push_str("prefix-");
    builder.append(&suffix);

    let built = builder.finish();

    assert_eq!(String::from(&built), format!("prefix-{suffix_text}"));
    assert_eq!(built.measure::<LinesMetric>(), suffix.measure::<LinesMetric>());
}
