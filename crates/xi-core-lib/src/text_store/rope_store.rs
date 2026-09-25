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

use std::borrow::Cow;

use xi_rope::rope::{byte_to_utf16_cu_idx, utf16_cu_to_byte_idx};
use xi_rope::{LinesMetric, Rope};

use crate::text_store::{
    ByteOffset, ByteRange, ChunkBytes, DocumentMode, FullTextPolicy, KnownLineCount, LineLookup,
    LogicalLine, ReadBytesResult, ReadResult, RenderLineCount, RenderSource, TextChunk,
    TextChunkResult, TextStore, Utf16Lookup, Utf16Offset,
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

    /// Clamps `start..end` into the rope and snaps both ends to codepoint
    /// boundaries the way the VLF seam decoder does (start back to the previous
    /// boundary, end forward past a trailing continuation): `slice_to_cow` panics
    /// on mid-codepoint ranges, and the renderer always passes line-interval
    /// boundaries anyway.
    fn clamped_range(&self, start: usize, end: usize) -> (usize, usize) {
        let len = self.rope.len();
        let start = start.min(len);
        let end = end.min(len).max(start);
        let start = if self.rope.is_codepoint_boundary(start) {
            start
        } else {
            self.rope.prev_codepoint_offset(start).unwrap_or(start)
        };
        let end = if self.rope.is_codepoint_boundary(end) {
            end
        } else {
            self.rope.at_or_next_codepoint_boundary(end).unwrap_or(end)
        };
        (start, end)
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
    byte_to_utf16_cu_idx(chunk, end)
}

fn byte_offset_for_utf16_in_chunk(chunk: &str, target_utf16: usize) -> Option<usize> {
    // Resolve like the legacy accumulation loop, then reject targets that
    // split a surrogate pair: `utf16_cu_to_byte_idx` lands on the containing
    // char's end, but this store requires an exact char-boundary match.
    let byte_idx = utf16_cu_to_byte_idx(chunk, target_utf16)?;
    if byte_to_utf16_cu_idx(chunk, byte_idx) == target_utf16 { Some(byte_idx) } else { None }
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

    fn read_chunk_bytes(&self, range: ByteRange) -> ChunkBytes<'_> {
        let Some((start, end)) = self.validate_range(range) else {
            return ChunkBytes::Unsupported;
        };
        if start == end {
            // Nothing to copy: report it as borrowed so the cost signal stays
            // "no allocation" for empty rows.
            return ChunkBytes::Ready { bytes: Cow::Borrowed(&[]), byte_range: range };
        }
        // A range inside one rope leaf is handed out as a borrow of that leaf.
        // Anything else falls back to the same bytes the owned carrier produces:
        // a range crossing leaves, or one whose ends are not char boundaries,
        // which the owned path rejects by panicking (`slice_to_cow` semantics) and
        // which must therefore not quietly succeed here.
        if let Some((chunk, byte_start, _, _)) = self.rope.chunk_at_offset(start) {
            let rel_start = start - byte_start;
            let rel_end = rel_start + (end - start);
            if rel_end <= chunk.len()
                && chunk.is_char_boundary(rel_start)
                && chunk.is_char_boundary(rel_end)
            {
                let borrowed = &chunk.as_bytes()[rel_start..rel_end];
                return ChunkBytes::Ready { bytes: Cow::Borrowed(borrowed), byte_range: range };
            }
        }
        ChunkBytes::Ready {
            bytes: Cow::Owned(self.collect_text(start, end).into_bytes()),
            byte_range: range,
        }
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
// RenderSource
// ---------------------------------------------------------------------------

/// Rope-backed `RenderSource`: every op delegates directly to the `Rope` so
/// render behaviour is byte-for-byte identical with direct `&Rope` access.
impl RenderSource for RopeTextStore {
    fn len_bytes(&self) -> usize {
        self.rope.len()
    }

    fn total_lines(&self) -> RenderLineCount {
        // LinesMetric counts newlines; total logical lines = newlines + 1.
        RenderLineCount::Exact((self.rope.measure::<LinesMetric>() + 1) as u64)
    }

    fn line_to_byte(&self, line: u64) -> LineLookup {
        // Clamp to the last line before delegating: `offset_of_line` panics on
        // out-of-range lines (debug builds); the renderer's pre-facade path
        // clamped inside `Lines::offset_of_visual_line`.
        let max_line = (self.rope.measure::<LinesMetric>() + 1) as u64;
        let line = line.min(max_line) as usize;
        LineLookup::Exact(ByteOffset(self.rope.offset_of_line(line) as u64))
    }

    fn byte_to_line(&self, byte: usize) -> Option<u64> {
        let byte = byte.min(self.rope.len());
        // `line_of_offset` panics on mid-codepoint offsets; snap down to the
        // previous boundary (same line, robust to arbitrary offsets).
        let byte = if self.rope.is_codepoint_boundary(byte) {
            byte
        } else {
            self.rope.prev_codepoint_offset(byte).unwrap_or(byte)
        };
        Some(self.rope.line_of_offset(byte) as u64)
    }

    fn read_range(&self, start: usize, end: usize) -> ReadResult<'_> {
        let (start, end) = self.clamped_range(start, end);
        ReadResult::Ready(self.rope.slice_to_cow(start..end))
    }

    fn read_bytes(&self, start: usize, end: usize) -> ReadBytesResult<'_> {
        let (start, end) = self.clamped_range(start, end);
        // Same range as `read_range`; only the carrier differs. A range inside one
        // leaf is lent straight out of the rope, otherwise the owned copy the text
        // carrier would have made is handed over as bytes.
        match TextStore::read_chunk_bytes(self, ByteRange::new(start as u64, end as u64)) {
            ChunkBytes::Ready { bytes, .. } => ReadBytesResult::Ready(bytes),
            ChunkBytes::Pending => ReadBytesResult::Pending,
            ChunkBytes::Cancelled => ReadBytesResult::Cancelled,
            ChunkBytes::Unsupported => ReadBytesResult::Unsupported,
        }
    }

    fn index_progress(&self) -> f64 {
        1.0
    }

    fn as_rope(&self) -> Option<&Rope> {
        Some(&self.rope)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use xi_rope::Rope;

    use crate::text_store::{
        ByteOffset, ByteRange, ChunkBytes, DocumentMode, FullTextPolicy, KnownLineCount,
        LineLookup, LogicalLine, TextChunkResult, TextStore, Utf16Lookup, Utf16Offset,
    };

    use super::{RopeTextStore, byte_offset_for_utf16_in_chunk, utf16_prefix_in_chunk};

    fn store(s: &str) -> RopeTextStore {
        RopeTextStore::new(Rope::from(s), 0)
    }

    // ---- utf16 chunk helpers -------------------------------------------------

    #[test]
    fn utf16_prefix_matches_encode_utf16() {
        let s = "aé😀x";
        for end in 0..=s.len() {
            let mut clamped = end.min(s.len());
            while clamped > 0 && !s.is_char_boundary(clamped) {
                clamped -= 1;
            }
            assert_eq!(utf16_prefix_in_chunk(s, end), s[..clamped].encode_utf16().count());
        }
    }

    #[test]
    fn utf16_byte_lookup_is_exact_boundary_only() {
        let s = "a😀x"; // 1 + 2 + 1 = 4 utf16 units
        assert_eq!(byte_offset_for_utf16_in_chunk(s, 0), Some(0));
        assert_eq!(byte_offset_for_utf16_in_chunk(s, 1), Some(1)); // after 'a'
        // Mid-surrogate targets must be rejected (strict Exact semantics).
        assert_eq!(byte_offset_for_utf16_in_chunk(s, 2), None);
        assert_eq!(byte_offset_for_utf16_in_chunk(s, 3), Some(5)); // after '😀'
        assert_eq!(byte_offset_for_utf16_in_chunk(s, 4), Some(6));
        assert_eq!(byte_offset_for_utf16_in_chunk(s, 5), None); // beyond
        assert_eq!(byte_offset_for_utf16_in_chunk("", 0), Some(0));
        assert_eq!(byte_offset_for_utf16_in_chunk("", 1), None);
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
        fn read_chunk_bytes(&self, _range: ByteRange) -> ChunkBytes<'_> {
            ChunkBytes::Pending
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

// ---------------------------------------------------------------------------
// RenderSource equivalence tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod chunk_bytes_tests {
    use std::borrow::Cow;

    use super::*;
    use crate::text_store::conformance;

    fn store(s: &str) -> RopeTextStore {
        RopeTextStore::new(Rope::from(s), 7)
    }

    #[test]
    fn single_leaf_range_borrows_rope_storage() {
        let text = "let value = 42;\nsecond line\n";
        let store = store(text);

        match store.read_chunk_bytes(ByteRange::new(4, 9)) {
            ChunkBytes::Ready { bytes, byte_range } => {
                assert_eq!(bytes.as_ref(), b"value");
                assert_eq!(byte_range, ByteRange::new(4, 9));
                let (leaf, leaf_start, _, _) =
                    store.rope().chunk_at_offset(4).expect("leaf at offset");
                let leaf_base = leaf.as_ptr() as usize;
                assert_eq!(
                    bytes.as_ptr() as usize - leaf_base,
                    4 - leaf_start,
                    "borrowed bytes must point into the rope leaf, not a copy"
                );
                assert!(bytes.as_ptr() as usize + bytes.len() <= leaf_base + leaf.len());
                assert!(matches!(bytes, Cow::Borrowed(_)));
            }
            other => panic!("expected ready chunk, got {other:?}"),
        }
    }

    #[test]
    fn empty_range_borrows_and_out_of_bounds_is_unsupported() {
        let store = store("abc");
        match store.read_chunk_bytes(ByteRange::new(1, 1)) {
            ChunkBytes::Ready { bytes, byte_range } => {
                assert!(bytes.is_empty());
                assert_eq!(byte_range, ByteRange::new(1, 1));
            }
            other => panic!("expected ready empty chunk, got {other:?}"),
        }
        assert_eq!(store.read_chunk_bytes(ByteRange::new(0, 4)), ChunkBytes::Unsupported);
        assert_eq!(store.read_chunk_bytes(ByteRange::new(3, 1)), ChunkBytes::Unsupported);
    }

    #[test]
    fn range_crossing_leaves_falls_back_to_owned_bytes() {
        // Long content to force multiple leaves, then a range that straddles the
        // first leaf boundary.
        let text = "0123456789abcdef".repeat(512);
        let store = store(&text);
        let first_leaf_len = store.rope().chunk_at_offset(0).expect("first leaf").0.len();
        assert!(
            first_leaf_len < text.len(),
            "fixture must span more than one leaf (leaf={first_leaf_len}, text={})",
            text.len()
        );

        let start = (first_leaf_len - 4) as u64;
        let end = (first_leaf_len + 4) as u64;
        let range = ByteRange::new(start, end);
        match (store.read_byte_range(range), store.read_chunk_bytes(range)) {
            (TextChunkResult::Ready(chunk), ChunkBytes::Ready { bytes, byte_range }) => {
                assert_eq!(bytes.as_ref(), chunk.text.as_bytes());
                assert_eq!(byte_range, ByteRange::new(start, end));
                assert!(matches!(bytes, Cow::Owned(_)), "crossing leaves copies today");
            }
            (owned, borrowed) => panic!("expected both ready: {owned:?} / {borrowed:?}"),
        }
    }

    #[test]
    fn row_sized_range_in_realistic_document_borrows() {
        // Text leaves are 511-1024 bytes (`xi_rope::rope`), so a whole window
        // always crosses leaves and takes the owned fallback, while a single
        // row usually fits inside one leaf and borrows. Phase 3 blob assembly
        // therefore reads row-by-row rather than window-by-window.
        let text: String = (0..200).map(|i| format!("line {i} with some content\n")).collect();
        let store = store(&text);
        let row_start = text.find("line 100 ").expect("row present") as u64;
        let row_end = row_start + "line 100 with some content\n".len() as u64;

        match store.read_chunk_bytes(ByteRange::new(row_start, row_end)) {
            ChunkBytes::Ready { bytes, .. } => {
                assert_eq!(bytes.as_ref(), &text.as_bytes()[row_start as usize..row_end as usize]);
                assert!(matches!(bytes, Cow::Borrowed(_)), "a row should fit inside one leaf");
            }
            other => panic!("expected ready chunk, got {other:?}"),
        }

        let window = ByteRange::new(0, text.len() as u64);
        match store.read_chunk_bytes(window) {
            ChunkBytes::Ready { bytes, .. } => {
                assert_eq!(bytes.as_ref(), text.as_bytes());
                assert!(
                    matches!(bytes, Cow::Owned(_)),
                    "a multi-leaf window cannot borrow from a single leaf"
                );
            }
            other => panic!("expected ready chunk, got {other:?}"),
        }
    }

    #[test]
    #[should_panic(expected = "char boundary")]
    fn borrowed_carrier_rejects_mid_codepoint_range_like_the_owned_one() {
        // 'é' spans bytes 1..3, so 2..3 splits it. The range fits inside one leaf
        // and would otherwise be borrowable, but the owned carrier panics here
        // (`slice_to_cow` semantics), so the borrowed carrier must fail the same
        // way rather than hand out invalid UTF-8.
        let store = store("aé😀x\n");
        store.read_chunk_bytes(ByteRange::new(2, 3));
    }

    #[test]
    #[should_panic(expected = "char boundary")]
    fn owned_carrier_rejects_the_same_mid_codepoint_range() {
        // Parity pin for the test above: whatever the borrowed carrier does with
        // this request, the owned carrier panics.
        let store = store("aé😀x\n");
        store.read_byte_range(ByteRange::new(2, 3));
    }

    #[test]
    fn rope_store_chunk_bytes_conformance() {
        let store = store("alpha café 😀\nbeta gamma\ndelta\n");
        conformance::assert_chunk_bytes_conformance(&store, "alpha café 😀\nbeta gamma\ndelta\n");
    }

    #[test]
    fn rope_store_chunk_bytes_conformance_across_leaves() {
        let text = "lorem ipsum dolor sit amet ".repeat(256);
        let store = RopeTextStore::new(Rope::from(text.clone()), 1);
        conformance::assert_chunk_bytes_conformance(&store, &text);
    }
}

#[cfg(test)]
mod render_source_tests {
    use std::borrow::Cow;

    use super::*;

    fn store(s: &str) -> RopeTextStore {
        RopeTextStore::new(Rope::from(s), 0)
    }

    #[test]
    fn read_bytes_matches_read_range_and_borrows_single_leaf_rows() {
        // The byte carrier must serve exactly what the text carrier serves, and a
        // row that fits inside one leaf must be lent rather than copied.
        let s = "alpha\ncafé\n😀 row\n";
        let rope = Rope::from(s);
        let src = store(s);

        for (start, end) in [(0, 6), (6, 11), (11, s.len()), (0, 0), (2, 4)] {
            let expected = match RenderSource::read_range(&src, start, end) {
                ReadResult::Ready(text) => text.into_owned(),
                other => panic!("text carrier should be ready for {start}..{end}: {other:?}"),
            };
            match RenderSource::read_bytes(&src, start, end) {
                ReadBytesResult::Ready(bytes) => {
                    assert_eq!(
                        bytes.as_ref(),
                        expected.as_bytes(),
                        "bytes differ for {start}..{end}"
                    );
                }
                other => panic!("byte carrier should be ready for {start}..{end}: {other:?}"),
            }
        }

        match RenderSource::read_bytes(&src, 0, 6) {
            ReadBytesResult::Ready(bytes) => {
                assert!(matches!(bytes, Cow::Borrowed(_)), "one-leaf row should be lent");
                assert_eq!(bytes.as_ref(), b"alpha\n");
            }
            other => panic!("expected ready bytes, got {other:?}"),
        }
        assert_eq!(rope.len(), s.len());
    }

    #[test]
    fn render_source_metrics_match_rope() {
        let s = "aé😀x\nsecond line\n";
        let rope = Rope::from(s);
        let src = store(s);

        assert_eq!(RenderSource::len_bytes(&src), rope.len());
        let expected_lines = rope.measure::<LinesMetric>() + 1;
        assert_eq!(RenderSource::total_lines(&src), RenderLineCount::Exact(expected_lines as u64));

        // In-range lookups delegate to `offset_of_line` exactly.
        for line in 0..=expected_lines {
            assert_eq!(
                RenderSource::line_to_byte(&src, line as u64),
                LineLookup::Exact(ByteOffset(rope.offset_of_line(line) as u64))
            );
        }
        // Out-of-range lines clamp to EOF, matching `offset_of_line` semantics
        // that the renderer relied on pre-facade.
        assert_eq!(
            RenderSource::line_to_byte(&src, 99),
            LineLookup::Exact(ByteOffset(rope.len() as u64))
        );

        for byte in [0, 1, 3, 7, 9, rope.len() / 2, rope.len()] {
            assert_eq!(
                RenderSource::byte_to_line(&src, byte),
                Some(rope.line_of_offset(byte) as u64),
                "byte {byte}"
            );
        }
        // Mid-codepoint offsets (e.g. byte 5 inside '😀' spanning 3..7) snap
        // down to the previous boundary: same line, no panic.
        assert_eq!(RenderSource::byte_to_line(&src, 5), Some(rope.line_of_offset(3) as u64));

        assert_eq!(RenderSource::index_progress(&src), 1.0);
        assert_eq!(src.as_rope().map(|r| r.len()), Some(rope.len()));
    }

    #[test]
    fn render_source_read_range_matches_slice_to_cow() {
        let s = "héllo wörld\n😀\n";
        let rope = Rope::from(s);
        let src = store(s);

        // Boundary-aligned or clamped ranges slice exactly like `slice_to_cow`.
        for (start, end) in
            [(0, 5), (1, 13), (0, s.len()), (s.len() - 1, s.len() + 10), (s.len() + 5, s.len() + 9)]
        {
            let clamped_start = start.min(s.len());
            let clamped_end = end.min(s.len()).max(clamped_start);
            match RenderSource::read_range(&src, start, end) {
                ReadResult::Ready(chunk) => {
                    assert_eq!(chunk, rope.slice_to_cow(clamped_start..clamped_end));
                }
                other => panic!("rope source must return Ready, got {other:?}"),
            }
        }
    }

    #[test]
    fn render_source_read_range_snaps_mid_codepoint_ranges() {
        // Mid-codepoint offsets snap like the VLF seam decoder: start back to
        // the previous boundary, end forward to the next boundary — never a
        // panic and never a partial char.
        let s = "aé😀x\n";
        let rope = Rope::from(s);
        let src = store(s);

        // 'é' spans bytes 1..3; start=2 snaps back to 1.
        match RenderSource::read_range(&src, 2, 3) {
            ReadResult::Ready(chunk) => {
                assert_eq!(chunk, rope.slice_to_cow(1..3), "start snaps to previous boundary");
            }
            other => panic!("rope source must return Ready, got {other:?}"),
        }
        // '😀' spans bytes 3..7; end=5 snaps forward to 7.
        match RenderSource::read_range(&src, 3, 5) {
            ReadResult::Ready(chunk) => {
                assert_eq!(chunk, rope.slice_to_cow(3..7), "end snaps to next boundary");
            }
            other => panic!("rope source must return Ready, got {other:?}"),
        }
    }

    #[test]
    fn render_source_empty_and_edge_shapes() {
        for s in ["", "no-trailing-newline", "a\n", "\n\n"] {
            let rope = Rope::from(s);
            let src = store(s);
            let expected_lines = rope.measure::<LinesMetric>() + 1;

            assert_eq!(RenderSource::line_to_byte(&src, 0), LineLookup::Exact(ByteOffset(0)));
            assert_eq!(
                RenderSource::total_lines(&src),
                RenderLineCount::Exact(expected_lines as u64)
            );
            // Last line start == EOF when the file ends without a newline;
            // trailing newline yields an empty final line at EOF.
            assert_eq!(
                RenderSource::line_to_byte(&src, expected_lines as u64),
                LineLookup::Exact(ByteOffset(rope.len() as u64))
            );
            assert_eq!(
                RenderSource::byte_to_line(&src, rope.len()),
                Some(expected_lines.saturating_sub(1) as u64)
            );
        }
    }
}
