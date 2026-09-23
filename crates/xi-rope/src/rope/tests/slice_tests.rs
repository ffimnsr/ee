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

#[test]
fn byte_slice_accepts_char_aligned_ranges() {
    let text = "aé🐸\nみんなさん";
    let rope = Rope::from(text);
    let view = rope.byte_slice(1..8);
    let collected: String = view.iter_chunks().collect();
    assert_eq!(collected, &text[1..8]);
    assert_eq!(view.rope(), &rope);
}

#[test]
fn byte_slice_rejects_mid_char_boundaries() {
    let rope = Rope::from("aé🐸");
    // Byte 2 splits 'é' (bytes 1..=2); byte 5 splits '🐸' (bytes 3..=6).
    assert!(matches!(
        rope.try_byte_slice(2..4),
        Err(RopeError::IntervalNotCharBoundary { start: 2, end: 4 })
    ));
    assert!(matches!(
        rope.try_byte_slice(0..2),
        Err(RopeError::IntervalNotCharBoundary { start: 0, end: 2 })
    ));
    assert!(matches!(
        rope.try_byte_slice(5..7),
        Err(RopeError::IntervalNotCharBoundary { start: 5, end: 7 })
    ));
    assert!(rope.try_byte_slice(0..rope.len()).is_ok());
}

#[test]
#[should_panic]
fn byte_slice_panics_on_mid_char() {
    let rope = Rope::from("aé🐸");
    rope.byte_slice(2..4);
}

#[test]
fn byte_slice_preserves_other_errors() {
    let rope = Rope::from("aé🐸");
    let start = 3;
    let end = 1;
    assert!(matches!(rope.try_byte_slice(start..end), Err(RopeError::ReversedInterval { .. })));
    assert!(matches!(rope.try_byte_slice(0..9), Err(RopeError::IntervalOutOfBounds { .. })));
}

#[test]
fn slice_char_api_matches_model() {
    let text = "aé🐸\nみんなさん";
    let rope = Rope::from(text);
    let view = rope.byte_slice(1..8); // "é🐸\n": 2 + 4 + 1 = 7 bytes, 3 chars

    assert_eq!(view.len_chars(), 3);
    let collected: String = view.chars().collect();
    assert_eq!(collected, "é🐸\n");
    let bytes: Vec<u8> = view.bytes().collect();
    assert_eq!(bytes, text.as_bytes()[1..8]);
    assert_eq!(view.char_at(0), 'é');
    assert_eq!(view.get_char(1), Some('🐸'));
    assert_eq!(view.get_char(2), Some('\n'));
    assert_eq!(view.get_char(3), None);
}

#[test]
fn slice_char_conversions_are_view_relative() {
    let text = "aé🐸\nみんなさん";
    let rope = Rope::from(text);
    let view = rope.byte_slice(1..8);

    assert_eq!(view.char_to_byte(0), 0);
    assert_eq!(view.char_to_byte(2), 6); // é (2 bytes) + 🐸 (4 bytes)
    assert_eq!(view.char_to_byte(3), 7); // one-past-the-end: view byte length
    assert_eq!(view.byte_to_char(2), 1); // mid-'é' rounds down
    assert_eq!(view.byte_to_char(6), 2);
    assert_eq!(view.byte_to_char(7), 3); // view end == len_chars

    for c in 0..=view.len_chars() {
        let b = view.char_to_byte(c);
        assert_eq!(view.byte_to_char(b), c, "roundtrip char {c}");
    }
}

#[test]
fn slice_char_api_multileaf() {
    let text = "あ".repeat(700) + "xé🐸\n" + &"あ".repeat(700);
    let rope = Rope::from(text.as_str());
    // Both boundaries on char boundaries: 999 = 333 * 3, 2501 = 2108 + 393 * 3.
    let view = rope.byte_slice(999..2501);
    let model = &text[999..2501];

    assert_eq!(view.len_chars(), model.chars().count());
    let collected: String = view.chars().collect();
    assert_eq!(collected, model);
    let bytes: Vec<u8> = view.bytes().collect();
    assert_eq!(bytes, model.as_bytes());
    assert_eq!(view.byte_to_char(3), 1);
    assert_eq!(view.char_at(5), model.chars().nth(5).unwrap());
}

#[test]
fn slice_view_debug_asserts_char_boundaries() {
    let rope = Rope::from("aé🐸");
    // Legacy unvalidated entry point: debug builds panic on mid-char views.
    let result = std::panic::catch_unwind(|| {
        let _view = rope.slice_view(2..4);
    });
    #[cfg(debug_assertions)]
    assert!(result.is_err());
    #[cfg(not(debug_assertions))]
    assert!(result.is_ok());
}

#[test]
fn byte_slice_empty_view() {
    let rope = Rope::from("aé🐸");
    let view = rope.byte_slice(3..3);
    assert!(view.is_empty());
    assert_eq!(view.len_chars(), 0);
    assert_eq!(view.get_char(0), None);
    assert_eq!(view.char_to_byte(0), 0);
    assert_eq!(view.chars().next(), None);
    assert_eq!(view.bytes().next(), None);
}

#[test]
fn byte_slice_accepts_interval_bound_forms() {
    let text = "aé🐸\nみんなさん";
    let rope = Rope::from(text);
    // RangeFull, RangeFrom, RangeTo, RangeInclusive.
    assert_eq!(rope.byte_slice(..).len_chars(), text.chars().count());
    assert_eq!(rope.byte_slice(8..).len_chars(), "みんなさん".chars().count());
    assert_eq!(rope.byte_slice(..8).len_chars(), 4); // "aé🐸\n"
    assert_eq!(rope.byte_slice(1..=7).len_chars(), 3); // "é🐸\n"
}

#[test]
fn byte_slice_view_ending_at_rope_end() {
    let text = "aé🐸\nみんなさん";
    let rope = Rope::from(text);
    let view = rope.byte_slice(8..rope.len());
    assert_eq!(view.len(), text.len() - 8);
    assert_eq!(view.len_chars(), "みんなさん".chars().count());
    let collected: String = view.chars().collect();
    assert_eq!(collected, "みんなさん");
    assert_eq!(view.char_to_byte(view.len_chars()), view.len());
    assert_eq!(view.byte_to_char(view.len()), view.len_chars());
}

#[test]
fn nested_aligned_slice_char_api() {
    let text = "aé🐸\nみんなさん";
    let rope = Rope::from(text);
    // byte_slice(1..8) = "é🐸\n"; nested slice(2..6) (bytes) = "🐸".
    let nested = rope.byte_slice(1..8).slice(2..6);
    assert_eq!(nested.len_chars(), 1);
    assert_eq!(nested.char_at(0), '🐸');
    let collected: String = nested.chars().collect();
    assert_eq!(collected, "🐸");
    assert_eq!(nested.byte_to_char(0), 0);
    assert_eq!(nested.byte_to_char(4), 1);
}

#[test]
fn slice_indices_are_view_relative() {
    let text = "aé🐸\nみんなさん";
    let rope = Rope::from(text);
    let view = rope.byte_slice(1..8); // "é🐸\n"
    let collected: Vec<(usize, char)> = view.char_indices().collect();
    assert_eq!(collected, vec![(0, 'é'), (1, '🐸'), (2, '\n')]);
    let collected: Vec<(usize, u8)> = view.byte_indices().collect();
    let expected: Vec<(usize, u8)> =
        text.as_bytes()[1..8].iter().enumerate().map(|(i, &b)| (i, b)).collect();
    assert_eq!(collected, expected);
}

#[test]
fn from_rope_slice_shares_and_copies() {
    let rope = Rope::from("aé🐸\nhello");
    // Full view: storage shared with the source.
    let full: Rope = rope.slice_view(..).into();
    assert!(full.is_instance(&rope));
    assert_eq!(String::from(&full), "aé🐸\nhello");
    // Partial view: content extracted, storage diverged but copy-on-write.
    let part: Rope = rope.byte_slice(1..8).into();
    assert_eq!(String::from(&part), "é🐸\n");
    assert!(!part.is_instance(&rope));
    let mut diverged = part.clone();
    diverged.insert(0, "X");
    assert_eq!(String::from(&part), "é🐸\n");
    assert_eq!(String::from(&diverged), "Xé🐸\n");
}
