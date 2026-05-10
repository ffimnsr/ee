// Copyright 2026 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! `TextStore` implementation backed by `xi_rope::Rope`.
//!
//! `RopeTextStore` wraps the existing rope and exposes it through the
//! `TextStore` API. All conversions delegate directly to `Rope` methods so
//! normal-mode behaviour is byte-for-byte compatible with previous direct
//! `Rope` access.

use xi_rope::{LinesMetric, Rope};

use crate::text_store::{
    ByteOffset, ByteRange, DocumentMode, FullTextPolicy, KnownLineCount, LineLookup, LogicalLine,
    TextChunk, TextChunkResult, TextStore, Utf16Lookup, Utf16Offset,
};

// ---------------------------------------------------------------------------
// RopeTextStore
// ---------------------------------------------------------------------------

/// A `TextStore` backed by a `xi_rope::Rope`.
///
/// Intended as the normal-mode implementation of `TextStore`. Editing still
/// happens directly on the `Rope` via the `Editor`; this type covers read-only
/// and query paths only.
pub struct RopeTextStore {
    rope: Rope,
    /// Opaque revision token sourced from `Engine::get_head_rev_id().token()`.
    snapshot_id: u64,
    /// The document mode for this store. Defaults to `Normal`; callers that
    /// open a file in `ConstrainedNormal` mode can override via
    /// [`RopeTextStore::new_with_mode`].
    mode: DocumentMode,
}

impl RopeTextStore {
    /// Create a new `RopeTextStore` wrapping `rope` in `Normal` mode.
    ///
    /// `snapshot_id` should be the current engine revision token so that
    /// callers can detect when the content has changed.
    pub fn new(rope: Rope, snapshot_id: u64) -> Self {
        RopeTextStore { rope, snapshot_id, mode: DocumentMode::Normal }
    }

    /// Create a `RopeTextStore` with an explicit document mode.
    ///
    /// Use this when the open policy selects `ConstrainedNormal` for files
    /// near the normal-mode threshold that still fit in RAM.
    pub fn new_with_mode(rope: Rope, mode: DocumentMode) -> Self {
        RopeTextStore { rope, snapshot_id: 0, mode }
    }

    /// Create a `RopeTextStore` with an explicit document mode and revision.
    pub fn new_with_mode_and_snapshot(rope: Rope, mode: DocumentMode, snapshot_id: u64) -> Self {
        RopeTextStore { rope, snapshot_id, mode }
    }

    /// Borrow the underlying `Rope`.
    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    fn validate_range(&self, range: ByteRange) -> Option<(usize, usize)> {
        let start = range.start.0 as usize;
        let end = range.end.0 as usize;
        let len = self.rope.len();
        if start > len || end > len || start > end { None } else { Some((start, end)) }
    }

    fn collect_text(&self, start: usize, end: usize) -> String {
        let mut text = String::with_capacity(end.saturating_sub(start));
        let mut current = start;
        while current < end {
            let (chunk, byte_start, _, _) = self
                .rope
                .chunk_at_offset(current)
                .expect("validated range should resolve to a rope chunk");
            let rel_start = current - byte_start;
            let rel_end = chunk.len().min(end - byte_start);
            text.push_str(&chunk[rel_start..rel_end]);
            current = byte_start + rel_end;
        }
        text
    }
}

fn count_newlines_in_prefix(chunk: &str, end: usize) -> usize {
    chunk.as_bytes()[..end].iter().filter(|&&byte| byte == b'\n').count()
}

fn line_offset_in_chunk(chunk: &str, relative_line: usize) -> Option<usize> {
    if relative_line == 0 {
        return Some(0);
    }
    let mut remaining = relative_line;
    for (idx, byte) in chunk.as_bytes().iter().enumerate() {
        if *byte == b'\n' {
            remaining -= 1;
            if remaining == 0 {
                return Some(idx + 1);
            }
        }
    }
    None
}

fn utf16_prefix_in_chunk(chunk: &str, end: usize) -> usize {
    chunk[..end].encode_utf16().count()
}

fn byte_offset_for_utf16_in_chunk(chunk: &str, target_utf16: usize) -> Option<usize> {
    if target_utf16 == 0 {
        return Some(0);
    }
    let mut utf16_seen = 0;
    let mut utf8_seen = 0;
    for ch in chunk.chars() {
        if utf16_seen >= target_utf16 {
            break;
        }
        utf16_seen += ch.len_utf16();
        utf8_seen += ch.len_utf8();
        if utf16_seen == target_utf16 {
            return Some(utf8_seen);
        }
        if utf16_seen > target_utf16 {
            return None;
        }
    }
    if utf16_seen == target_utf16 { Some(utf8_seen) } else { None }
}

struct RopeChunkIter<'a> {
    rope: &'a Rope,
    current: usize,
    end: usize,
}

impl<'a> Iterator for RopeChunkIter<'a> {
    type Item = TextChunkResult;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current >= self.end {
            return None;
        }
        let (chunk, byte_start, _, _) = self
            .rope
            .chunk_at_offset(self.current)
            .expect("validated range should resolve to a rope chunk");
        let rel_start = self.current - byte_start;
        let rel_end = chunk.len().min(self.end - byte_start);
        let range = ByteRange {
            start: ByteOffset(self.current as u64),
            end: ByteOffset((byte_start + rel_end) as u64),
        };
        let text = chunk[rel_start..rel_end].to_owned();
        self.current = byte_start + rel_end;
        Some(TextChunkResult::Ready(TextChunk { text, byte_range: range }))
    }
}

impl TextStore for RopeTextStore {
    fn mode(&self) -> DocumentMode {
        self.mode
    }

    fn len_bytes(&self) -> u64 {
        self.rope.len() as u64
    }

    fn known_line_count(&self) -> KnownLineCount {
        // LinesMetric counts newlines; total logical lines = newlines + 1.
        let newlines = self.rope.measure::<LinesMetric>();
        KnownLineCount::Exact((newlines + 1) as u64)
    }

    fn read_byte_range(&self, range: ByteRange) -> TextChunkResult {
        let Some((start, end)) = self.validate_range(range) else {
            return TextChunkResult::Unsupported;
        };
        let text = self.collect_text(start, end);
        TextChunkResult::Ready(TextChunk { text, byte_range: range })
    }

    fn line_to_byte(&self, line: LogicalLine) -> LineLookup {
        let line = line.0 as usize;
        let max_line = self.rope.measure::<LinesMetric>() + 1;
        if line > max_line {
            return LineLookup::OutOfRange;
        }
        if line == max_line {
            return LineLookup::Exact(ByteOffset(self.rope.len() as u64));
        }
        let Some((chunk, byte_start, line_start, _)) = self.rope.chunk_at_line(line) else {
            return LineLookup::OutOfRange;
        };
        let rel_line = line.saturating_sub(line_start);
        let Some(within_chunk) = line_offset_in_chunk(chunk, rel_line) else {
            return LineLookup::OutOfRange;
        };
        LineLookup::Exact(ByteOffset((byte_start + within_chunk) as u64))
    }

    fn byte_to_line(&self, offset: ByteOffset) -> Option<LogicalLine> {
        let off = offset.0 as usize;
        let (chunk, byte_start, line_start, _) = self.rope.chunk_at_offset(off)?;
        let within_chunk = off - byte_start;
        Some(LogicalLine((line_start + count_newlines_in_prefix(chunk, within_chunk)) as u64))
    }

    fn iter_chunks(&self, range: ByteRange) -> Box<dyn Iterator<Item = TextChunkResult> + '_> {
        let Some((start, end)) = self.validate_range(range) else {
            return Box::new(std::iter::once(TextChunkResult::Unsupported));
        };
        Box::new(RopeChunkIter { rope: &self.rope, current: start, end })
    }

    fn snapshot_id(&self) -> u64 {
        self.snapshot_id
    }

    fn byte_to_utf16(&self, offset: ByteOffset) -> Option<Utf16Offset> {
        let off = offset.0 as usize;
        let (chunk, byte_start, _, utf16_start) = self.rope.chunk_at_offset(off)?;
        let within_chunk = off - byte_start;
        Some(Utf16Offset((utf16_start + utf16_prefix_in_chunk(chunk, within_chunk)) as u64))
    }

    fn utf16_to_byte(&self, offset: Utf16Offset) -> Utf16Lookup {
        let target = offset.0 as usize;
        let Some((chunk, byte_start, _, utf16_start)) = self.rope.chunk_at_utf16(target) else {
            return Utf16Lookup::OutOfRange;
        };
        let rel_utf16 = target.saturating_sub(utf16_start);
        let Some(within_chunk) = byte_offset_for_utf16_in_chunk(chunk, rel_utf16) else {
            return Utf16Lookup::OutOfRange;
        };
        Utf16Lookup::Exact(ByteOffset((byte_start + within_chunk) as u64))
    }

    fn full_text_policy(&self) -> FullTextPolicy {
        // Normal-mode rope store always permits full-text extraction.
        FullTextPolicy::Allowed
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use xi_rope::Rope;

    use crate::text_store::{
        ByteOffset, ByteRange, DocumentMode, FullTextPolicy, KnownLineCount, LineLookup,
        LogicalLine, TextChunkResult, TextStore, Utf16Lookup, Utf16Offset,
    };

    use super::RopeTextStore;

    fn store(s: &str) -> RopeTextStore {
        RopeTextStore::new(Rope::from(s), 0)
    }

    // ---- mode ---------------------------------------------------------------

    #[test]
    fn mode_is_normal() {
        assert_eq!(store("").mode(), DocumentMode::Normal);
    }

    // ---- len_bytes ----------------------------------------------------------

    #[test]
    fn len_bytes_empty() {
        assert_eq!(store("").len_bytes(), 0);
    }

    #[test]
    fn len_bytes_ascii() {
        assert_eq!(store("hello").len_bytes(), 5);
    }

    #[test]
    fn len_bytes_multibyte() {
        // "café" = 5 bytes (c a f é where é is 2 bytes)
        let s = "café";
        let rope = Rope::from(s);
        let st = RopeTextStore::new(rope, 1);
        assert_eq!(st.len_bytes(), s.len() as u64);
    }

    // ---- known_line_count ---------------------------------------------------

    #[test]
    fn line_count_single_line() {
        assert_eq!(store("hello").known_line_count(), KnownLineCount::Exact(1));
    }

    #[test]
    fn line_count_multiple_lines() {
        // "a\nb\nc" → 3 lines, 2 newlines
        assert_eq!(store("a\nb\nc").known_line_count(), KnownLineCount::Exact(3));
    }

    #[test]
    fn line_count_trailing_newline() {
        // "a\nb\n" → 3 lines (last is empty), 2 newlines
        assert_eq!(store("a\nb\n").known_line_count(), KnownLineCount::Exact(3));
    }

    #[test]
    fn line_count_empty() {
        assert_eq!(store("").known_line_count(), KnownLineCount::Exact(1));
    }

    #[test]
    fn line_count_matches_direct_rope() {
        // Verify RopeTextStore line count matches direct rope line_of_offset.
        let rope = Rope::from("line1\nline2\nline3");
        let st = RopeTextStore::new(rope.clone(), 0);
        let direct_last_line = rope.line_of_offset(rope.len());
        if let KnownLineCount::Exact(count) = st.known_line_count() {
            assert_eq!(count, (direct_last_line + 1) as u64);
        } else {
            panic!("expected Exact line count");
        }
    }

    // ---- read_byte_range ----------------------------------------------------

    #[test]
    fn read_byte_range_full() {
        let s = "hello world";
        let st = store(s);
        let result = st.read_byte_range(ByteRange::new(0, s.len() as u64));
        match result {
            TextChunkResult::Ready(chunk) => {
                assert_eq!(chunk.text, s);
                assert_eq!(chunk.byte_range.start, ByteOffset(0));
                assert_eq!(chunk.byte_range.end, ByteOffset(s.len() as u64));
            }
            other => panic!("expected Ready, got {:?}", other),
        }
    }

    #[test]
    fn read_byte_range_partial() {
        let st = store("hello world");
        let result = st.read_byte_range(ByteRange::new(6, 11));
        match result {
            TextChunkResult::Ready(chunk) => assert_eq!(chunk.text, "world"),
            other => panic!("expected Ready, got {:?}", other),
        }
    }

    #[test]
    fn read_byte_range_out_of_bounds_returns_unsupported() {
        let st = store("hi");
        assert_eq!(st.read_byte_range(ByteRange::new(0, 99)), TextChunkResult::Unsupported);
    }

    #[test]
    fn read_byte_range_matches_direct_rope() {
        let rope = Rope::from("abcdef");
        let st = RopeTextStore::new(rope.clone(), 0);
        let direct = rope.slice_to_cow(2..5).into_owned();
        match st.read_byte_range(ByteRange::new(2, 5)) {
            TextChunkResult::Ready(chunk) => assert_eq!(chunk.text, direct),
            other => panic!("expected Ready, got {:?}", other),
        }
    }

    // ---- line_to_byte -------------------------------------------------------

    #[test]
    fn line_to_byte_line_zero() {
        assert_eq!(store("a\nb\nc").line_to_byte(LogicalLine(0)), LineLookup::Exact(ByteOffset(0)));
    }

    #[test]
    fn line_to_byte_second_line() {
        // "a\nb\nc": line 1 starts at byte 2
        assert_eq!(store("a\nb\nc").line_to_byte(LogicalLine(1)), LineLookup::Exact(ByteOffset(2)));
    }

    #[test]
    fn line_to_byte_out_of_range() {
        // "hello" has 1 line (line 0); line 5 is out of range.
        assert_eq!(store("hello").line_to_byte(LogicalLine(5)), LineLookup::OutOfRange);
    }

    #[test]
    fn line_to_byte_matches_direct_rope() {
        let rope = Rope::from("one\ntwo\nthree");
        let st = RopeTextStore::new(rope.clone(), 0);
        for line in 0..3usize {
            let direct = rope.offset_of_line(line);
            match st.line_to_byte(LogicalLine(line as u64)) {
                LineLookup::Exact(off) => assert_eq!(off.0, direct as u64),
                other => panic!("line {}: expected Exact, got {:?}", line, other),
            }
        }
    }

    // ---- byte_to_line -------------------------------------------------------

    #[test]
    fn byte_to_line_start() {
        assert_eq!(store("a\nb\nc").byte_to_line(ByteOffset(0)), Some(LogicalLine(0)));
    }

    #[test]
    fn byte_to_line_second_line() {
        // "a\nb\nc": offset 2 is 'b' on line 1
        assert_eq!(store("a\nb\nc").byte_to_line(ByteOffset(2)), Some(LogicalLine(1)));
    }

    #[test]
    fn byte_to_line_out_of_range_returns_none() {
        assert_eq!(store("hi").byte_to_line(ByteOffset(99)), None);
    }

    #[test]
    fn byte_to_line_matches_direct_rope() {
        let rope = Rope::from("one\ntwo\nthree");
        let st = RopeTextStore::new(rope.clone(), 0);
        for off in [0usize, 1, 3, 4, 7, 8, 12] {
            let direct = rope.line_of_offset(off);
            match st.byte_to_line(ByteOffset(off as u64)) {
                Some(line) => assert_eq!(line.0, direct as u64),
                None => panic!("offset {}: expected Some, got None", off),
            }
        }
    }

    // ---- iter_chunks --------------------------------------------------------

    #[test]
    fn iter_chunks_covers_full_range() {
        let s = "hello world";
        let st = store(s);
        let chunks: Vec<_> = st.iter_chunks(ByteRange::new(0, s.len() as u64)).collect();
        let text: String = chunks
            .into_iter()
            .map(|c| match c {
                TextChunkResult::Ready(ch) => ch.text,
                other => panic!("expected Ready, got {:?}", other),
            })
            .collect();
        assert_eq!(text, s);
    }

    #[test]
    fn iter_chunks_preserves_absolute_ranges_across_leaf_boundaries() {
        let s = format!("{}{}", "a".repeat(1200), "b".repeat(1200));
        let st = store(&s);
        let mut offset = 0usize;
        let mut boundary = None;
        for chunk in st.rope().iter_chunks(..) {
            offset += chunk.len();
            if offset < s.len() {
                boundary = Some(offset as u64);
                break;
            }
        }
        let boundary = boundary.expect("expected multi-leaf rope");
        let chunks: Vec<_> =
            st.iter_chunks(ByteRange::new(boundary.saturating_sub(50), boundary + 50)).collect();
        assert!(chunks.len() >= 2);

        let ranges: Vec<_> = chunks
            .into_iter()
            .map(|chunk| match chunk {
                TextChunkResult::Ready(chunk) => (chunk.byte_range.start.0, chunk.byte_range.end.0),
                other => panic!("expected Ready, got {:?}", other),
            })
            .collect();

        assert_eq!(ranges.first().copied(), Some((boundary.saturating_sub(50), boundary)));
        assert_eq!(ranges.last().copied(), Some((boundary, boundary + 50)));
    }

    #[test]
    fn iter_chunks_out_of_bounds_returns_unsupported() {
        let st = store("hi");
        let result: Vec<_> = st.iter_chunks(ByteRange::new(0, 99)).collect();
        assert_eq!(result, vec![TextChunkResult::Unsupported]);
    }

    // ---- snapshot_id --------------------------------------------------------

    #[test]
    fn snapshot_id_round_trips() {
        let st = RopeTextStore::new(Rope::from("x"), 42);
        assert_eq!(st.snapshot_id(), 42);
    }

    // ---- regression: normal buffers never claim VLF mode -------------------

    #[test]
    fn normal_mode_is_not_vlf() {
        assert_ne!(store("anything").mode(), DocumentMode::Vlf);
    }

    // ---- byte_to_utf16 / utf16_to_byte --------------------------------------

    #[test]
    fn byte_to_utf16_ascii_identity() {
        // ASCII: UTF-8 offsets == UTF-16 offsets.
        let st = store("hello");
        assert_eq!(st.byte_to_utf16(ByteOffset(0)), Some(Utf16Offset(0)));
        assert_eq!(st.byte_to_utf16(ByteOffset(3)), Some(Utf16Offset(3)));
        assert_eq!(st.byte_to_utf16(ByteOffset(5)), Some(Utf16Offset(5)));
    }

    #[test]
    fn byte_to_utf16_multibyte() {
        // "café": c(1) a(1) f(1) é(2 bytes, 1 UTF-16 unit) → total 5 bytes, 4 UTF-16
        let st = store("café");
        // After 'c','a','f' (3 bytes) we have 3 UTF-16 units.
        assert_eq!(st.byte_to_utf16(ByteOffset(3)), Some(Utf16Offset(3)));
        // After 'é' (2 more bytes = offset 5) we have 4 UTF-16 units.
        assert_eq!(st.byte_to_utf16(ByteOffset(5)), Some(Utf16Offset(4)));
    }

    #[test]
    fn byte_to_utf16_out_of_range_returns_none() {
        assert_eq!(store("hi").byte_to_utf16(ByteOffset(99)), None);
    }

    #[test]
    fn utf16_to_byte_ascii_identity() {
        let st = store("hello");
        assert_eq!(st.utf16_to_byte(Utf16Offset(0)), Utf16Lookup::Exact(ByteOffset(0)));
        assert_eq!(st.utf16_to_byte(Utf16Offset(3)), Utf16Lookup::Exact(ByteOffset(3)));
        assert_eq!(st.utf16_to_byte(Utf16Offset(5)), Utf16Lookup::Exact(ByteOffset(5)));
    }

    #[test]
    fn utf16_to_byte_multibyte() {
        let st = store("café");
        // UTF-16 offset 4 should land at byte offset 5 (end of string).
        assert_eq!(st.utf16_to_byte(Utf16Offset(4)), Utf16Lookup::Exact(ByteOffset(5)));
    }

    #[test]
    fn utf16_to_byte_out_of_range() {
        assert_eq!(store("hi").utf16_to_byte(Utf16Offset(99)), Utf16Lookup::OutOfRange);
    }

    #[test]
    fn byte_utf16_roundtrip() {
        // Roundtrip: byte → utf16 → byte must be identity for ASCII.
        let st = store("hello world");
        for byte_off in 0u64..=11 {
            let utf16 = st.byte_to_utf16(ByteOffset(byte_off)).unwrap();
            assert_eq!(st.utf16_to_byte(utf16), Utf16Lookup::Exact(ByteOffset(byte_off)));
        }
    }

    #[test]
    fn byte_utf16_roundtrip_multibyte() {
        // Roundtrip on codepoint boundaries for multibyte chars.
        let s = "a\u{00e9}b"; // 'a', 'é' (2 bytes), 'b' → 4 bytes total
        let st = store(s);
        // byte offsets at codepoint boundaries: 0, 1, 3, 4
        for byte_off in [0u64, 1, 3, 4] {
            let utf16 = st.byte_to_utf16(ByteOffset(byte_off)).unwrap();
            assert_eq!(
                st.utf16_to_byte(utf16),
                Utf16Lookup::Exact(ByteOffset(byte_off)),
                "roundtrip failed at byte offset {}",
                byte_off
            );
        }
    }

    // ---- full_text_policy and read_full_text --------------------------------

    #[test]
    fn rope_store_policy_is_allowed() {
        assert_eq!(store("hello").full_text_policy(), FullTextPolicy::Allowed);
    }

    #[test]
    fn read_full_text_returns_content() {
        let s = "hello world";
        let st = store(s);
        match st.read_full_text() {
            TextChunkResult::Ready(chunk) => assert_eq!(chunk.text, s),
            other => panic!("expected Ready, got {:?}", other),
        }
    }

    // ---- VLF guardrail stub: full-text extraction is Unsupported ------------
    //
    // This stub exercises the `read_full_text` default method on a store that
    // returns `FullTextPolicy::Forbidden`. It verifies the guardrail contract
    // without needing a complete VlfStore implementation.

    struct StubVlfStore;

    impl TextStore for StubVlfStore {
        fn mode(&self) -> DocumentMode {
            DocumentMode::Vlf
        }
        fn len_bytes(&self) -> u64 {
            1024 * 1024 * 1024 // 1 GB
        }
        fn known_line_count(&self) -> KnownLineCount {
            KnownLineCount::Unknown
        }
        fn read_byte_range(&self, _range: ByteRange) -> TextChunkResult {
            TextChunkResult::Pending
        }
        fn line_to_byte(&self, _line: LogicalLine) -> LineLookup {
            LineLookup::Pending
        }
        fn byte_to_line(&self, _offset: ByteOffset) -> Option<LogicalLine> {
            None
        }
        fn iter_chunks(&self, _range: ByteRange) -> Box<dyn Iterator<Item = TextChunkResult> + '_> {
            Box::new(std::iter::once(TextChunkResult::Pending))
        }
        fn snapshot_id(&self) -> u64 {
            0
        }
        fn byte_to_utf16(&self, _offset: ByteOffset) -> Option<Utf16Offset> {
            None
        }
        fn utf16_to_byte(&self, _offset: Utf16Offset) -> Utf16Lookup {
            Utf16Lookup::Pending
        }
        fn full_text_policy(&self) -> FullTextPolicy {
            FullTextPolicy::Forbidden
        }
    }

    #[test]
    fn vlf_store_policy_is_forbidden() {
        assert_eq!(StubVlfStore.full_text_policy(), FullTextPolicy::Forbidden);
    }

    #[test]
    fn vlf_read_full_text_returns_unsupported() {
        // The default `read_full_text` must short-circuit to Unsupported when
        // policy is Forbidden; no chunk read is attempted.
        assert_eq!(StubVlfStore.read_full_text(), TextChunkResult::Unsupported);
    }

    #[test]
    fn vlf_mode_check_before_full_text() {
        // Callers that check full_text_policy before calling read_full_text
        // see Forbidden and must use chunk/range paths instead.
        let store: Box<dyn TextStore> = Box::new(StubVlfStore);
        assert_eq!(store.full_text_policy(), FullTextPolicy::Forbidden);
        // Confirm the chunk path is available (returns Pending, not Unsupported).
        let chunk_result = store.read_byte_range(ByteRange::new(0, 512));
        assert_eq!(chunk_result, TextChunkResult::Pending);
    }
}
