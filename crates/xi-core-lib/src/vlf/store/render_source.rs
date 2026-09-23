//! `impl RenderSource for VlfStore`: unified render-path surface for VLF.
//!
//! Delegates to the existing `TextStore` implementation (windowed seam reads,
//! lazy line index) so the render pipeline needs no VLF-specific logic:
//! lookups that the index cannot answer yet surface as `Approximate`/
//! `Pending`, `read_range` serves the requested byte range clipped out of the
//! seam-decoded window, and `as_rope` is `None` (no wrapping, no rope-bound
//! annotations in Stage A).

use std::borrow::Cow;

use xi_rope::Rope;

use super::*;
use crate::text_store::{
    ByteOffset, ByteRange, KnownLineCount, LineLookup, LogicalLine, ReadResult, RenderLineCount,
    RenderSource, TextChunkResult, TextStore,
};

impl RenderSource for VlfStore {
    fn len_bytes(&self) -> usize {
        TextStore::len_bytes(self) as usize
    }

    fn total_lines(&self) -> RenderLineCount {
        match TextStore::known_line_count(self) {
            KnownLineCount::Exact(n) => RenderLineCount::Exact(n),
            // The index is still building (or has not started): extrapolated
            // estimates are already monotone; `Unknown` renders an empty
            // window and the frontend retries on the next repaint.
            KnownLineCount::Approximate(n) => RenderLineCount::Approximate(n),
            KnownLineCount::Unknown => RenderLineCount::Approximate(0),
        }
    }

    fn line_to_byte(&self, line: u64) -> LineLookup {
        TextStore::line_to_byte(self, LogicalLine(line))
    }

    fn byte_to_line(&self, byte: usize) -> Option<u64> {
        // The TextStore lookup uses half-open page ranges, so `byte == len`
        // maps to None; the rope source reports the final line there. Clamp so
        // the facade matches rope semantics for the EOF boundary.
        let len = TextStore::len_bytes(self);
        if len == 0 {
            return None;
        }
        let byte = (byte as u64).min(len.saturating_sub(1)) as usize;
        TextStore::byte_to_line(self, crate::text_store::ByteOffset(byte as u64)).map(|l| l.0)
    }

    /// Read `start..end` clipped out of the seam-decoded window.
    ///
    /// The underlying decode covers the requested range plus UTF-8 seam slack;
    /// the returned string contains exactly the requested byte range (the
    /// renderer assumes `read_range(start, end).len() == end - start` for
    /// line intervals, matching the rope source).
    fn read_range(&self, start: usize, end: usize) -> ReadResult<'_> {
        let len = TextStore::len_bytes(self) as usize;
        let start = start.min(len);
        let end = end.min(len).max(start);
        match TextStore::read_byte_range(self, ByteRange::new(start as u64, end as u64)) {
            TextChunkResult::Ready(chunk) => {
                let d_start = chunk.byte_range.start.0 as usize;
                let d_end = chunk.byte_range.end.0 as usize;
                if start >= d_start && end <= d_end {
                    let text = chunk.text;
                    let rel_start = (start - d_start).min(text.len());
                    let rel_end = (end - d_start).min(text.len()).max(rel_start);
                    // Snap to codepoint boundaries like the seam decoder does
                    // (start back, end forward); render callers pass
                    // line-interval boundaries anyway.
                    let mut rel_start = rel_start;
                    while rel_start > 0 && !text.is_char_boundary(rel_start) {
                        rel_start -= 1;
                    }
                    let mut rel_end = rel_end;
                    while rel_end < text.len() && !text.is_char_boundary(rel_end) {
                        rel_end += 1;
                    }
                    ReadResult::Ready(Cow::Owned(text[rel_start..rel_end].to_owned()))
                } else {
                    // Decode did not cover the request (shouldn't happen:
                    // seam headroom exceeds any render window). Signal a
                    // retry rather than serving partial content.
                    ReadResult::Pending
                }
            }
            TextChunkResult::Pending => ReadResult::Pending,
            TextChunkResult::Cancelled => ReadResult::Cancelled,
            // Out-of-bounds / unsupported: clamped calls cannot hit this;
            // treat as retry.
            TextChunkResult::Unsupported => ReadResult::Pending,
        }
    }

    fn index_progress(&self) -> f64 {
        self.index().scan_progress().fraction()
    }

    fn set_viewport(&self, start: usize, end: usize) {
        // Drive the pager read-ahead + the syntax semantic window from the
        // unified render path (the legacy `vlf_viewport` handler that used to
        // do this is gone). Inherent `set_viewport` takes byte offsets.
        self.set_viewport(ByteOffset(start as u64), ByteOffset(end as u64));
    }

    fn as_rope(&self) -> Option<&Rope> {
        None
    }
}
