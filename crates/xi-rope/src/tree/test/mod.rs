//! Tree tests: harness.
use super::*;
use crate::rope::*;

fn build_triangle(n: u32) -> String {
    let mut s = String::new();
    let mut line = String::new();
    for _ in 0..n {
        s += &line;
        s += "\n";
        line += "a";
    }
    s
}

#[test]
fn eq_rope_with_pieces() {
    let n = 2_000;
    let s = build_triangle(n);
    let mut builder_default = TreeBuilder::new();
    let mut concat_rope = Rope::default();
    builder_default.push_str(&s);
    let mut i = 0;
    while i < s.len() {
        let j = (i + 1000).min(s.len());
        concat_rope = concat_rope + s[i..j].into();
        i = j;
    }
    let built_rope = builder_default.build();
    assert_eq!(built_rope, concat_rope);
}

mod cursor_tests;
