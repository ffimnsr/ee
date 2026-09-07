//! Rope tests: slice_to_cow and slice views.
use super::*;

#[test]
fn slice_to_cow_small_string() {
    let short_text = "hi, i'm a small piece of text.";

    let rope = Rope::from(short_text);

    let cow = rope.slice_to_cow(..);

    assert!(short_text.len() <= 1024);
    assert_eq!(cow, Cow::Borrowed(short_text) as Cow<str>);
}

#[test]
fn slice_to_cow_long_string_long_slice() {
    // 32 char long string, repeat it 33 times so it is longer than 1024 bytes
    let long_text = "1234567812345678123456781234567812345678123456781234567812345678".repeat(33);

    let rope = Rope::from(&long_text);

    let cow = rope.slice_to_cow(..);

    assert!(long_text.len() > 1024);
    assert_eq!(cow, Cow::Owned(long_text) as Cow<str>);
}

#[test]
fn slice_to_cow_long_string_short_slice() {
    // 32 char long string, repeat it 33 times so it is longer than 1024 bytes
    let long_text = "1234567812345678123456781234567812345678123456781234567812345678".repeat(33);

    let rope = Rope::from(&long_text);

    let cow = rope.slice_to_cow(..500);

    assert!(long_text.len() > 1024);
    assert_eq!(cow, Cow::Borrowed(&long_text[..500]));
}

#[test]
fn slice_view_iterates_chunks_without_materializing() {
    let text = format!("{}{}{}", "a".repeat(MAX_LEAF), "\n", "b".repeat(MAX_LEAF));
    let rope = Rope::from(&text);
    let view = rope.slice_view(MAX_LEAF - 8..MAX_LEAF + 9);

    let collected: String = view.iter_chunks().collect();
    assert_eq!(collected, text[MAX_LEAF - 8..MAX_LEAF + 9]);
    assert_eq!(
        view.lines_raw().collect::<Vec<_>>(),
        vec![
            Cow::from(&text[MAX_LEAF - 8..MAX_LEAF + 1]),
            Cow::from(&text[MAX_LEAF + 1..MAX_LEAF + 9])
        ]
    );
}

#[test]
fn nested_slice_view_reuses_original_chunk_storage() {
    let text = format!("{}{}{}", "a".repeat(MAX_LEAF), "0123456789", "b".repeat(MAX_LEAF));
    let rope = Rope::from(&text);
    let nested = rope.slice_view(MAX_LEAF - 4..MAX_LEAF + 14).slice(3..15);

    let direct_chunks: Vec<_> = rope.iter_chunks(MAX_LEAF - 1..MAX_LEAF + 11).collect();
    let nested_chunks: Vec<_> = nested.iter_chunks().collect();

    assert_eq!(nested_chunks, direct_chunks);
    assert!(
        nested_chunks
            .iter()
            .zip(direct_chunks.iter())
            .all(|(nested, direct)| nested.as_ptr() == direct.as_ptr())
    );
}

#[test]
fn slice_view_cursor_stops_at_view_end() {
    let rope = Rope::from(format!("{}needle-after", "x".repeat(MAX_LEAF + 32)));
    let view = rope.slice_view(MAX_LEAF + 20..MAX_LEAF + 26);
    let mut cursor = view.cursor(0);

    assert_eq!(cursor.get_leaf().map(|(leaf, _)| leaf), Some("xxxxxx"));
    assert_eq!(cursor.next_leaf(), None);
    assert_eq!(cursor.pos(), view.len());
}
