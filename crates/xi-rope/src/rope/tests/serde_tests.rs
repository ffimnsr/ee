//! Rope tests: serde round trips.
use super::*;
use crate::Rope;
use serde_test::{Token, assert_tokens};

#[test]
fn serialize_and_deserialize() {
    const TEST_LINE: &str = "test line\n";

    // repeat test line enough times to exceed maximum leaf size
    let n_seg = MAX_LEAF / TEST_LINE.len() + 1;
    let test_str = TEST_LINE.repeat(n_seg);

    let rope = Rope::from(test_str.as_str());
    let json = serde_json::to_string(&rope).expect("error serializing");
    let deserialized_rope =
        serde_json::from_str::<Rope>(json.as_str()).expect("error deserializing");
    assert_eq!(rope, deserialized_rope);
}

#[test]
fn test_ser_de() {
    let rope = Rope::from("a\u{00A1}\u{4E00}\u{1F4A9}");
    assert_tokens(&rope, &[Token::Str("a\u{00A1}\u{4E00}\u{1F4A9}")]);
    assert_tokens(&rope, &[Token::String("a\u{00A1}\u{4E00}\u{1F4A9}")]);
    assert_tokens(&rope, &[Token::BorrowedStr("a\u{00A1}\u{4E00}\u{1F4A9}")]);
}
