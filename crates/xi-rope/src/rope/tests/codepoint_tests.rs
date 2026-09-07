//! Rope tests: codepoint and grapheme offsets.
use super::*;

#[test]
fn prev_codepoint_offset_small() {
    let a = Rope::from("a\u{00A1}\u{4E00}\u{1F4A9}");
    assert_eq!(Some(6), a.prev_codepoint_offset(10));
    assert_eq!(Some(3), a.prev_codepoint_offset(6));
    assert_eq!(Some(1), a.prev_codepoint_offset(3));
    assert_eq!(Some(0), a.prev_codepoint_offset(1));
    assert_eq!(None, a.prev_codepoint_offset(0));
    let b = a.slice(1..10);
    assert_eq!(Some(5), b.prev_codepoint_offset(9));
    assert_eq!(Some(2), b.prev_codepoint_offset(5));
    assert_eq!(Some(0), b.prev_codepoint_offset(2));
    assert_eq!(None, b.prev_codepoint_offset(0));
}

#[test]
fn next_codepoint_offset_small() {
    let a = Rope::from("a\u{00A1}\u{4E00}\u{1F4A9}");
    assert_eq!(Some(10), a.next_codepoint_offset(6));
    assert_eq!(Some(6), a.next_codepoint_offset(3));
    assert_eq!(Some(3), a.next_codepoint_offset(1));
    assert_eq!(Some(1), a.next_codepoint_offset(0));
    assert_eq!(None, a.next_codepoint_offset(10));
    let b = a.slice(1..10);
    assert_eq!(Some(9), b.next_codepoint_offset(5));
    assert_eq!(Some(5), b.next_codepoint_offset(2));
    assert_eq!(Some(2), b.next_codepoint_offset(0));
    assert_eq!(None, b.next_codepoint_offset(9));
}

#[test]
fn peek_next_codepoint() {
    let inp = Rope::from("$¢€£💶");
    let mut cursor = Cursor::new(&inp, 0);
    assert_eq!(cursor.peek_next_codepoint(), Some('$'));
    assert_eq!(cursor.peek_next_codepoint(), Some('$'));
    assert_eq!(cursor.next_codepoint(), Some('$'));
    assert_eq!(cursor.peek_next_codepoint(), Some('¢'));
    assert_eq!(cursor.prev_codepoint(), Some('$'));
    assert_eq!(cursor.peek_next_codepoint(), Some('$'));
    assert_eq!(cursor.next_codepoint(), Some('$'));
    assert_eq!(cursor.next_codepoint(), Some('¢'));
    assert_eq!(cursor.peek_next_codepoint(), Some('€'));
    assert_eq!(cursor.next_codepoint(), Some('€'));
    assert_eq!(cursor.peek_next_codepoint(), Some('£'));
    assert_eq!(cursor.next_codepoint(), Some('£'));
    assert_eq!(cursor.peek_next_codepoint(), Some('💶'));
    assert_eq!(cursor.next_codepoint(), Some('💶'));
    assert_eq!(cursor.peek_next_codepoint(), None);
    assert_eq!(cursor.next_codepoint(), None);
    assert_eq!(cursor.peek_next_codepoint(), None);
}

#[test]
fn prev_grapheme_offset() {
    // A with ring, hangul, regional indicator "US"
    let a = Rope::from("A\u{030a}\u{110b}\u{1161}\u{1f1fa}\u{1f1f8}");
    assert_eq!(Some(9), a.prev_grapheme_offset(17));
    assert_eq!(Some(3), a.prev_grapheme_offset(9));
    assert_eq!(Some(0), a.prev_grapheme_offset(3));
    assert_eq!(None, a.prev_grapheme_offset(0));
}

#[test]
fn next_grapheme_offset() {
    // A with ring, hangul, regional indicator "US"
    let a = Rope::from("A\u{030a}\u{110b}\u{1161}\u{1f1fa}\u{1f1f8}");
    assert_eq!(Some(3), a.next_grapheme_offset(0));
    assert_eq!(Some(9), a.next_grapheme_offset(3));
    assert_eq!(Some(17), a.next_grapheme_offset(9));
    assert_eq!(None, a.next_grapheme_offset(17));
}

#[test]
fn next_grapheme_offset_with_ris_of_leaf_boundaries() {
    let s1 = "\u{1f1fa}\u{1f1f8}".repeat(100);
    let a = Rope::concat(
        Rope::from(s1.clone()),
        Rope::concat(Rope::from(s1.clone() + "\u{1f1fa}"), Rope::from(s1.clone())),
    );
    for i in 1..(s1.len() * 3) {
        assert_eq!(Some((i - 1) / 8 * 8), a.prev_grapheme_offset(i));
        assert_eq!(Some(i / 8 * 8 + 8), a.next_grapheme_offset(i));
    }
    for i in (s1.len() * 3 + 1)..(s1.len() * 3 + 4) {
        assert_eq!(Some(s1.len() * 3), a.prev_grapheme_offset(i));
        assert_eq!(Some(s1.len() * 3 + 4), a.next_grapheme_offset(i));
    }
    assert_eq!(None, a.prev_grapheme_offset(0));
    assert_eq!(Some(8), a.next_grapheme_offset(0));
    assert_eq!(Some(s1.len() * 3), a.prev_grapheme_offset(s1.len() * 3 + 4));
    assert_eq!(None, a.next_grapheme_offset(s1.len() * 3 + 4));
}
