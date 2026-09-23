//! Rope tests: harness.
use super::*;

/// Deterministic LCG for randomized tests (no external rng dependency).
pub(crate) struct Lcg(u64);

impl Lcg {
    pub(crate) fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0
    }

    pub(crate) fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Asserts that every rope metric agrees with a plain `str` model.
pub(crate) fn assert_matches_str(rope: &Rope, text: &str) {
    assert_eq!(String::from(rope), text);
    assert_eq!(rope.len(), text.len());
    assert_eq!(rope.len_chars(), text.chars().count());
    assert_eq!(rope.measure::<LinesMetric>(), text.bytes().filter(|&b| b == b'\n').count());
    assert_eq!(rope.len_utf16_cu(), text.encode_utf16().count());
}

/// Reference model: char index of the char containing `byte_idx` in `s`.
pub(crate) fn byte_to_char_idx_model(s: &str, byte_idx: usize) -> usize {
    if byte_idx >= s.len() {
        return s.chars().count();
    }
    for (idx, (start, ch)) in s.char_indices().enumerate() {
        if byte_idx < start + ch.len_utf8() {
            return idx;
        }
    }
    unreachable!()
}

pub(crate) fn char_to_byte_idx_model(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map_or(s.len(), |(b, _)| b)
}

pub(crate) fn insert_at_char(model: &mut String, char_idx: usize, text: &str) {
    let byte_idx = char_to_byte_idx_model(model, char_idx);
    model.insert_str(byte_idx, text);
}

pub(crate) fn replace_char_range(model: &mut String, start: usize, end: usize, text: &str) {
    let sb = char_to_byte_idx_model(model, start);
    let eb = char_to_byte_idx_model(model, end);
    model.replace_range(sb..eb, text);
}

struct FailingWriter {
    written: Vec<u8>,
    remaining: usize,
    fail_kind: io::ErrorKind,
}

impl io::Write for FailingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Err(io::Error::new(self.fail_kind, "writer interrupted"));
        }

        let written = self.remaining.min(buf.len());
        self.written.extend_from_slice(&buf[..written]);
        self.remaining -= written;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn replace_small() {
    let mut a = Rope::from("hello world");
    a.edit(1..9, "era");
    assert_eq!("herald", String::from(a));
}
fn chunk_boundaries(rope: &Rope) -> Vec<(usize, String)> {
    let mut offset = 0;
    let mut result = Vec::new();
    for chunk in rope.iter_chunks(..) {
        result.push((offset, chunk.to_owned()));
        offset += chunk.len();
    }
    result
}

mod chars_tests;
mod codepoint_tests;
mod eq_tests;
mod iter_tests;
mod leaf_tests;
mod lines_tests;
mod metric_tests;
mod slice_tests;
mod stream_tests;

#[cfg(all(test, feature = "serde"))]
mod serde_tests;
