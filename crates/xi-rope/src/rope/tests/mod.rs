//! Rope tests: harness.
use super::*;

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

mod codepoint_tests;
mod eq_tests;
mod leaf_tests;
mod lines_tests;
mod metric_tests;
mod slice_tests;
mod stream_tests;

#[cfg(all(test, feature = "serde"))]
mod serde_tests;
