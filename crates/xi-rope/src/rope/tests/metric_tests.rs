//! Rope tests: line/offset lookups and bounds errors.
use super::*;

#[test]
fn line_of_offset_small() {
    let a = Rope::from("a\nb\nc");
    assert_eq!(0, a.line_of_offset(0));
    assert_eq!(0, a.line_of_offset(1));
    assert_eq!(1, a.line_of_offset(2));
    assert_eq!(1, a.line_of_offset(3));
    assert_eq!(2, a.line_of_offset(4));
    assert_eq!(2, a.line_of_offset(5));
    let b = a.slice(2..4);
    assert_eq!(0, b.line_of_offset(0));
    assert_eq!(0, b.line_of_offset(1));
    assert_eq!(1, b.line_of_offset(2));
}

#[test]
fn offset_of_line_small() {
    let a = Rope::from("a\nb\nc");
    assert_eq!(0, a.offset_of_line(0));
    assert_eq!(2, a.offset_of_line(1));
    assert_eq!(4, a.offset_of_line(2));
    assert_eq!(5, a.offset_of_line(3));
    let b = a.slice(2..4);
    assert_eq!(0, b.offset_of_line(0));
    assert_eq!(2, b.offset_of_line(1));
}
#[test]
#[should_panic]
fn line_of_offset_panic() {
    let rope = Rope::from("hi\ni'm\nfour\nlines");
    rope.line_of_offset(20);
}

#[test]
#[should_panic]
fn offset_of_line_panic() {
    let rope = Rope::from("hi\ni'm\nfour\nlines");
    rope.offset_of_line(5);
}

#[test]
fn try_line_of_offset_reports_bounds_error() {
    let rope = Rope::from("hi\ni'm\nfour\nlines");
    assert_eq!(
        rope.try_line_of_offset(20),
        Err(RopeError::OffsetOutOfBounds { offset: 20, len: rope.len() })
    );
}

#[test]
fn try_offset_of_line_reports_bounds_error() {
    let rope = Rope::from("hi\ni'm\nfour\nlines");
    assert_eq!(
        rope.try_offset_of_line(5),
        Err(RopeError::LineOutOfBounds { line: 5, max_line: 4 })
    );
}

#[test]
fn try_slice_reports_bounds_error() {
    let rope = Rope::from("hello");
    assert_eq!(
        rope.try_slice(0..10),
        Err(RopeError::IntervalOutOfBounds { start: 0, end: 10, len: 5 })
    );
}

#[test]
fn str_utf16_helpers_match_std() {
    for s in ["", "abc", "aé", "😀🐸", "Hello みんなさん 🐸\n"] {
        assert_eq!(count_utf16_code_units(s), s.encode_utf16().count(), "{s:?}");
        assert_eq!(utf16_cu_to_byte_idx(s, count_utf16_code_units(s)), Some(s.len()), "{s:?}");
    }
}

#[test]
fn byte_to_utf16_cu_idx_rounds_mid_char_down() {
    let s = "a😀x";
    // '😀' occupies bytes 1..=4 (4 bytes, 2 UTF-16 units).
    assert_eq!(byte_to_utf16_cu_idx(s, 0), 0);
    assert_eq!(byte_to_utf16_cu_idx(s, 1), 1);
    assert_eq!(byte_to_utf16_cu_idx(s, 2), 1); // mid-char rounds down
    assert_eq!(byte_to_utf16_cu_idx(s, 3), 1);
    assert_eq!(byte_to_utf16_cu_idx(s, 4), 1); // still inside the char
    assert_eq!(byte_to_utf16_cu_idx(s, 5), 3);
    assert_eq!(byte_to_utf16_cu_idx(s, 6), 4);
    assert_eq!(byte_to_utf16_cu_idx(s, 99), 4);
    assert_eq!(byte_to_utf16_cu_idx("", 0), 0);
}

#[test]
fn utf16_cu_to_byte_idx_accumulation_semantics() {
    let s = "a😀x"; // 1 + 2 + 1 = 4 UTF-16 units, 6 bytes
    assert_eq!(utf16_cu_to_byte_idx(s, 0), Some(0));
    assert_eq!(utf16_cu_to_byte_idx(s, 1), Some(1));
    // Target splits the surrogate pair: resolves to the end of the char (byte 5).
    assert_eq!(utf16_cu_to_byte_idx(s, 2), Some(5));
    assert_eq!(utf16_cu_to_byte_idx(s, 3), Some(5));
    assert_eq!(utf16_cu_to_byte_idx(s, 4), Some(6));
    assert_eq!(utf16_cu_to_byte_idx(s, 5), None);
    assert_eq!(utf16_cu_to_byte_idx("abc", 3), Some(3));
    assert_eq!(utf16_cu_to_byte_idx("abc", 4), None);
    assert_eq!(utf16_cu_to_byte_idx("", 0), Some(0));
    assert_eq!(utf16_cu_to_byte_idx("", 1), None);
}

#[test]
fn utf16_helpers_parity_with_chars_loop() {
    // Pin the helpers against the classic accumulation loop they replaced in
    // the LSP client conversions, across all targets and byte offsets.
    let corpus = ["", "abc", "aé😀x\n", "Hello みんなさん 🐸", "\r\n", "🌊🌊"];
    for s in corpus {
        let total = s.encode_utf16().count();
        for target in 0..=total + 2 {
            let mut seen = 0;
            let mut bytes = 0;
            for ch in s.chars() {
                if seen >= target {
                    break;
                }
                seen += ch.len_utf16();
                bytes += ch.len_utf8();
            }
            let expected = if seen >= target { Some(bytes) } else { None };
            assert_eq!(
                utf16_cu_to_byte_idx(s, target),
                expected,
                "utf16_cu_to_byte_idx({target}) in {s:?}"
            );
        }
        for b in 0..=s.len() {
            let mut clamped = b.min(s.len());
            while clamped > 0 && !s.is_char_boundary(clamped) {
                clamped -= 1;
            }
            assert_eq!(
                byte_to_utf16_cu_idx(s, b),
                s[..clamped].encode_utf16().count(),
                "byte_to_utf16_cu_idx({b}) in {s:?}"
            );
        }
    }
}
