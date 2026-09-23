//! Rope tests: positional iterators (chars/bytes/lines/chunks + *_at).
use super::*;

#[test]
fn chars_iterator_matches_model() {
    let text = "aé🐸\nHello みんなさん\r\n";
    let rope = Rope::from(text);
    let collected: String = rope.chars().collect();
    assert_eq!(collected, text);
    assert_eq!(rope.chars().count(), text.chars().count());
}

#[test]
fn chars_iterator_multileaf() {
    let text = "あ".repeat(700) + "xé🐸\n" + &"あ".repeat(700);
    let rope = Rope::from(text.as_str());
    let collected: String = rope.chars().collect();
    assert_eq!(collected, text);
}

#[test]
fn chars_at_skip_model() {
    let text = "aé🐸\nhello みんな";
    let rope = Rope::from(text);
    let n = text.chars().count();
    for skip in [0, 1, 3, 5, n - 1, n] {
        let collected: String = rope.chars_at(skip).collect();
        let expected: String = text.chars().skip(skip).collect();
        assert_eq!(collected, expected, "chars_at({skip})");
    }
}

#[test]
fn chars_at_out_of_bounds() {
    let rope = Rope::from("hello");
    assert!(rope.chars_at(5).next().is_none());
    assert!(matches!(
        rope.try_chars_at(6),
        Err(RopeError::CharOffsetOutOfBounds { offset: 6, len: 5 })
    ));
    assert!(matches!(
        Rope::from("").try_chars_at(1),
        Err(RopeError::CharOffsetOutOfBounds { offset: 1, len: 0 })
    ));
}

#[test]
fn bytes_iterator_matches_model() {
    let text = "aé🐸\nHello みんなさん\r\n";
    let rope = Rope::from(text);
    let collected: Vec<u8> = rope.bytes().collect();
    assert_eq!(collected, text.as_bytes());
    assert_eq!(rope.bytes().count(), text.len());
}

#[test]
fn bytes_at_mid_char_start() {
    let text = "aé🐸";
    let rope = Rope::from(text);
    // 'é' occupies bytes 1..3; starting mid-char at byte 2 must yield the
    // continuation bytes rather than panicking on a utf8 slice.
    let collected: Vec<u8> = rope.bytes_at(2).collect();
    assert_eq!(collected, &text.as_bytes()[2..]);
    // One-past-the-end yields an empty iterator.
    assert!(rope.bytes_at(text.len()).next().is_none());
}

#[test]
fn bytes_at_exhaustive() {
    let text = "aé🐸\nみんな";
    let rope = Rope::from(text);
    for start in 0..=text.len() {
        let collected: Vec<u8> = rope.bytes_at(start).collect();
        assert_eq!(collected, &text.as_bytes()[start..], "bytes_at({start})");
    }
}

#[test]
fn bytes_at_out_of_bounds() {
    let rope = Rope::from("hello");
    assert!(matches!(
        rope.try_bytes_at(6),
        Err(RopeError::OffsetOutOfBounds { offset: 6, len: 5 })
    ));
}

#[test]
fn bytes_at_multileaf() {
    let text = "あ".repeat(700) + "xé🐸\n" + &"あ".repeat(700);
    let rope = Rope::from(text.as_str());
    for start in [0usize, 1, 100, 700, 2100, text.len()] {
        let collected: Vec<u8> = rope.bytes_at(start).collect();
        assert_eq!(collected, &text.as_bytes()[start..], "bytes_at({start})");
    }
}

#[test]
fn chars_at_exhaustive() {
    let text = "aé🐸\nみんな";
    let rope = Rope::from(text);
    let n = text.chars().count();
    for start in 0..=n {
        let collected: String = rope.chars_at(start).collect();
        let expected: String = text.chars().skip(start).collect();
        assert_eq!(collected, expected, "chars_at({start})");
    }
}

#[test]
fn lines_at_skip_model() {
    for text in ["Hello みんなさん\r\nsecond line\nthird\n", "a\n\nb\n\n", "\r\n"] {
        let rope = Rope::from(text);
        let expected: Vec<String> = text.lines().map(String::from).collect();
        for k in 0..=expected.len() {
            let got: Vec<String> = rope.lines_at(k).map(|l| l.into_owned()).collect();
            assert_eq!(got, expected[k..], "lines_at({k}) for {:?}", text);
        }
    }
}

#[test]
fn lines_at_empty_rope() {
    let rope = Rope::from("");
    assert!(rope.lines_at(0).next().is_none());
}

#[test]
fn try_lines_at_out_of_bounds() {
    let rope = Rope::from("a\nb");
    assert!(rope.lines_at(2).next().is_none());
    assert!(matches!(
        rope.try_lines_at(3),
        Err(RopeError::LineOutOfBounds { line: 3, max_line: 2 })
    ));
}

#[test]
fn char_indices_match_model() {
    let text = "aé🐸\nみんな";
    let rope = Rope::from(text);
    // Ropey-style: char indices (not std's byte indices).
    let collected: Vec<(usize, char)> = rope.char_indices().collect();
    let expected: Vec<(usize, char)> = text.chars().enumerate().collect();
    assert_eq!(collected, expected);
    // Absolute indices preserved when starting mid-rope.
    let skip = 2;
    let collected: Vec<(usize, char)> = rope.char_indices_at(skip).collect();
    let expected: Vec<(usize, char)> = text.chars().enumerate().skip(skip).collect();
    assert_eq!(collected, expected, "char_indices_at({skip})");
    assert!(rope.char_indices_at(text.chars().count()).next().is_none());
}

#[test]
fn byte_indices_match_model() {
    let text = "aé🐸\nみんな";
    let rope = Rope::from(text);
    let collected: Vec<(usize, u8)> = rope.byte_indices().collect();
    let expected: Vec<(usize, u8)> =
        text.as_bytes().iter().enumerate().map(|(i, &b)| (i, b)).collect();
    assert_eq!(collected, expected);
    let skip = 3;
    let collected: Vec<(usize, u8)> = rope.byte_indices_at(skip).collect();
    let expected: Vec<(usize, u8)> =
        text.as_bytes().iter().enumerate().skip(skip).map(|(i, &b)| (i, b)).collect();
    assert_eq!(collected, expected, "byte_indices_at({skip})");
    assert!(rope.byte_indices_at(text.len()).next().is_none());
}

#[test]
fn indices_out_of_bounds() {
    let rope = Rope::from("hello");
    assert!(matches!(
        rope.try_char_indices_at(6),
        Err(RopeError::CharOffsetOutOfBounds { offset: 6, len: 5 })
    ));
    assert!(matches!(
        rope.try_byte_indices_at(6),
        Err(RopeError::OffsetOutOfBounds { offset: 6, len: 5 })
    ));
    assert!(Rope::from("").char_indices().next().is_none());
    assert!(Rope::from("").byte_indices().next().is_none());
}

#[test]
fn chunks_matches_iter_chunks() {
    let rope = Rope::from("あ".repeat(700) + "xé🐸");
    let a: Vec<&str> = rope.chunks().collect();
    let b: Vec<&str> = rope.iter_chunks(..).collect();
    assert_eq!(a, b);
    assert!(a.iter().all(|c| !c.is_empty()));
}

#[test]
fn chunks_at_byte_reports_chunk_start() {
    let text = "あ".repeat(700) + "xé🐸" + &"あ".repeat(700);
    let rope = Rope::from(text.as_str());
    // Assert for every char boundary plus the one-past-the-end position.
    let mut positions: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
    positions.push(text.len());
    for byte_idx in positions {
        let (mut iter, byte_start, char_start, line_start) = rope.chunks_at_byte(byte_idx);
        let (chunk, b, l, _) = rope.chunk_at_offset(byte_idx).unwrap();
        assert_eq!((byte_start, char_start, line_start), (b, rope.count::<CharsMetric>(b), l));
        assert_eq!(iter.next(), Some(chunk), "first chunk at byte {byte_idx}");
    }
}

#[test]
fn chunks_at_char_reports_chunk_start() {
    let text = "あ".repeat(700) + "xé🐸" + &"あ".repeat(700);
    let rope = Rope::from(text.as_str());
    let n = text.chars().count();
    let mut positions: Vec<usize> = (0..=n).step_by(250).collect();
    positions.push(n);
    for char_idx in positions {
        let (mut iter, byte_start, char_start, line_start) = rope.chunks_at_char(char_idx);
        let byte_idx = rope.char_to_byte(char_idx);
        let (chunk, b, l, _) = rope.chunk_at_offset(byte_idx).unwrap();
        assert_eq!((byte_start, char_start, line_start), (b, rope.count::<CharsMetric>(b), l));
        assert_eq!(iter.next(), Some(chunk), "first chunk at char {char_idx}");
    }
}

#[test]
fn chunks_at_byte_mid_char() {
    let rope = Rope::from("aé🐸");
    // Byte 2 splits 'é' (bytes 1..3); the containing chunk is the whole leaf.
    let (iter, byte_start, char_start, line_start) = rope.chunks_at_byte(2);
    assert_eq!((byte_start, char_start, line_start), (0, 0, 0));
    assert_eq!(iter.collect::<Vec<_>>(), vec!["aé🐸"]);
}

#[test]
fn chunks_at_empty_rope() {
    let rope = Rope::from("");
    let (mut iter, byte_start, char_start, line_start) = rope.chunks_at_byte(0);
    assert_eq!((byte_start, char_start, line_start), (0, 0, 0));
    assert!(iter.next().is_none());
}

#[test]
fn chunks_at_out_of_bounds() {
    let rope = Rope::from("hi");
    assert!(matches!(
        rope.try_chunks_at_byte(3),
        Err(RopeError::OffsetOutOfBounds { offset: 3, len: 2 })
    ));
    assert!(matches!(
        rope.try_chunks_at_char(3),
        Err(RopeError::CharOffsetOutOfBounds { offset: 3, len: 2 })
    ));
}

#[test]
fn randomized_iterators_match_model() {
    let mut rng = Lcg(0x17e77e7);
    let mut rope = Rope::from("");
    let mut model = String::new();
    for _ in 0..300 {
        let n = rng.below(4) + 1;
        let text: String = std::iter::repeat_with(|| match rng.below(4) {
            0 => 'a',
            1 => 'é',
            2 => '🐸',
            _ => '\n',
        })
        .take(n as usize)
        .collect();
        let len = rope.len_chars();
        let pos = rng.below(len as u64 + 1) as usize;
        match rng.below(2) {
            0 => {
                rope.insert(pos, &text);
                insert_at_char(&mut model, pos, &text);
            }
            _ => {
                let end = (pos + rng.below(len as u64 - pos as u64 + 1) as usize).min(len);
                rope.remove(pos..end);
                replace_char_range(&mut model, pos, end, "");
            }
        }
        assert_matches_str(&rope, &model);

        let chars: String = rope.chars().collect();
        assert_eq!(chars, model);
        let bytes: Vec<u8> = rope.bytes().collect();
        assert_eq!(bytes, model.as_bytes());

        let skip_char = rng.below(rope.len_chars() as u64 + 1) as usize;
        let got: String = rope.chars_at(skip_char).collect();
        let expected: String = model.chars().skip(skip_char).collect();
        assert_eq!(got, expected, "chars_at({skip_char})");

        let skip_byte = rng.below(model.len() as u64 + 1) as usize;
        let got: Vec<u8> = rope.bytes_at(skip_byte).collect();
        assert_eq!(got, &model.as_bytes()[skip_byte..], "bytes_at({skip_byte})");

        let got: Vec<(usize, char)> = rope.char_indices().collect();
        let expected: Vec<(usize, char)> = model.chars().enumerate().collect();
        assert_eq!(got, expected);
        let got: Vec<(usize, u8)> = rope.byte_indices().collect();
        let expected: Vec<(usize, u8)> = model.bytes().enumerate().collect();
        assert_eq!(got, expected);

        let newlines = model.bytes().filter(|&b| b == b'\n').count();
        let skip_line = rng.below(newlines as u64 + 2) as usize;
        let got: Vec<String> = rope.lines_at(skip_line).map(|l| l.into_owned()).collect();
        let expected: Vec<String> = model.lines().skip(skip_line).map(String::from).collect();
        assert_eq!(got, expected, "lines_at({skip_line})");
    }
}
