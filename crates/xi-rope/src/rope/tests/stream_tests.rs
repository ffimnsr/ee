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

struct OneByteReader {
    data: Vec<u8>,
    pos: usize,
}

impl io::Read for OneByteReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.data.len() {
            return Ok(0);
        }
        buf[0] = self.data[self.pos];
        self.pos += 1;
        Ok(1)
    }
}

struct FailingReader {
    kind: io::ErrorKind,
}

impl io::Read for FailingReader {
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::new(self.kind, "reader interrupted"))
    }
}

#[test]
fn from_reader_loads_utf8() {
    let text = "Hello みんなさん 🐸\nline2";
    let rope = Rope::from_reader(std::io::Cursor::new(text.as_bytes())).unwrap();
    assert_eq!(String::from(&rope), text);
    assert_matches_str(&rope, text);
    rope.assert_integrity();
}

#[test]
fn from_reader_empty() {
    let rope = Rope::from_reader(std::io::Cursor::new(Vec::<u8>::new())).unwrap();
    assert!(rope.is_empty());
    rope.assert_integrity();
}

#[test]
fn from_reader_rejects_invalid_utf8() {
    let err = Rope::from_reader(std::io::Cursor::new(vec![0xff, 0xfe, b'a'])).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn from_reader_handles_byte_at_a_time() {
    let text = "aé🐸\nこんにちは";
    let rope = Rope::from_reader(OneByteReader { data: text.as_bytes().to_vec(), pos: 0 }).unwrap();
    assert_eq!(String::from(&rope), text);
    rope.assert_integrity();
}

#[test]
fn from_reader_handles_char_straddling_buffer() {
    // Force a 4-byte char to straddle the 2*MAX_LEAF buffer boundary.
    let text = format!("{}🐸tail", "a".repeat(MAX_LEAF * 2 - 1));
    let rope = Rope::from_reader(std::io::Cursor::new(text.as_bytes())).unwrap();
    assert_eq!(String::from(&rope), text);
    rope.assert_integrity();
}

#[test]
fn from_reader_propagates_read_errors() {
    let err = Rope::from_reader(FailingReader { kind: io::ErrorKind::TimedOut }).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
}

#[test]
fn capacity_and_shrink_to_fit() {
    let mut builder = RopeBuilder::new();
    let mut model = String::new();
    // Small pushes force leaf growth, leaving over-allocation to reclaim.
    for i in 0..800 {
        let piece = format!("{i},");
        builder.push_str(&piece);
        model.push_str(&piece);
    }
    let rope = builder.finish();
    assert_matches_str(&rope, &model);
    let cap_before = rope.capacity();
    assert!(cap_before >= rope.len());

    let mut rope = rope;
    let snapshot = rope.clone();
    rope.shrink_to_fit();
    assert_matches_str(&rope, &model);
    assert!(!rope.is_instance(&snapshot));
    let cap_after = rope.capacity();
    assert!(cap_after <= cap_before);

    // Idempotent: a second shrink changes nothing.
    rope.shrink_to_fit();
    assert_eq!(rope.capacity(), cap_after);
    assert_matches_str(&rope, &model);
}

#[test]
fn capacity_empty_and_tiny() {
    assert_eq!(Rope::from("").capacity(), 0);
    let mut rope = Rope::from("abc");
    assert!(rope.capacity() >= 3);
    rope.shrink_to_fit();
    assert_matches_str(&rope, "abc");
}
