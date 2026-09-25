//! The `RenderSource` facade: the text-access surface the render pipeline
//! consumes, independent of whether the buffer is rope-backed or VLF.
//!
//! Stage A goal: one render pipeline (see `VLF_RENDER_UNIFICATION_PLAN.md`).
//! `View::render_if_dirty`/`send_update_for_plan`/`encode_line` operate on
//! `&dyn RenderSource`; wrap-aware view machinery (`Lines`, `WidthCache`),
//! find, and annotations remain rope-backed in Stage A and are gated through
//! [`RenderSource::as_rope`]. Non-rope sources always render without wrapping
//! and without rope-bound annotations (VLF ships those gates in Phase 2).

use std::borrow::Cow;

use xi_rope::Rope;

use super::LineLookup;

/// Logical line count for render planning.
///
/// Mirrors [`super::KnownLineCount`] but without the `Unknown` state: the
/// render pipeline always needs a number, so stores map their internal
/// "not yet known" state to an estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderLineCount {
    /// The exact number of logical lines in the document.
    Exact(u64),
    /// A lower-bound estimate; background indexing is still in progress.
    Approximate(u64),
}

/// Result of a bounded text read for a render window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadResult<'a> {
    /// The text for the requested range (borrowed when the source can serve a
    /// slice without copying, owned otherwise).
    Ready(Cow<'a, str>),
    /// The source cannot serve the range yet (VLF index/decode in flight);
    /// callers render the window without text and retry on the next repaint.
    Pending,
    /// The read was cancelled by a newer generation; treat like `Pending`.
    Cancelled,
}

/// Result of a bounded byte read for a render window.
///
/// Mirrors [`ReadResult`] for callers that want the row's bytes without an
/// intermediate `String` (the binary line-text carrier).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadBytesResult<'a> {
    /// The bytes for the requested range (borrowed when the source can serve a
    /// slice of its own storage, owned otherwise).
    Ready(Cow<'a, [u8]>),
    /// The source cannot serve the range yet (VLF index/decode in flight);
    /// callers render the window without text and retry on the next repaint.
    Pending,
    /// The read was cancelled by a newer generation; treat like `Pending`.
    Cancelled,
    /// The source has no byte carrier for the range; callers omit the row's text
    /// and let the frontend keep what it already shows.
    Unsupported,
}

/// Text-access surface for the render pipeline.
///
/// Implemented by `RopeTextStore` (exact rope ops, byte-identical with direct
/// `Rope` access) and — in Phase 2 — by `VlfStore` (windowed reads, lazy
/// index). All operations are `O(log)` or better and never materialize the
/// whole document.
pub trait RenderSource {
    /// Total length of the document in bytes.
    fn len_bytes(&self) -> usize;

    /// Logical line count; may be approximate while the index builds.
    fn total_lines(&self) -> RenderLineCount;

    /// Byte offset of the start of `line` (0-based). Exact or approximate;
    /// `Pending`/`OutOfRange` when the index cannot answer yet.
    fn line_to_byte(&self, line: u64) -> LineLookup;

    /// Logical line containing `byte` (0-based).
    fn byte_to_line(&self, byte: usize) -> Option<u64>;

    /// Read the half-open byte range `start..end`, clamped to the document.
    fn read_range(&self, start: usize, end: usize) -> ReadResult<'_>;

    /// Read the half-open byte range `start..end` as bytes, clamped and snapped
    /// exactly like [`RenderSource::read_range`].
    ///
    /// The default derives the bytes from `read_range`, which is correct for any
    /// source but copies through a `String`; stores with a lendable chunk carrier
    /// override it so a row can be appended to the update's text blob without an
    /// intermediate allocation.
    fn read_bytes(&self, start: usize, end: usize) -> ReadBytesResult<'_> {
        match self.read_range(start, end) {
            ReadResult::Ready(text) => ReadBytesResult::Ready(match text {
                Cow::Borrowed(text) => Cow::Borrowed(text.as_bytes()),
                Cow::Owned(text) => Cow::Owned(text.into_bytes()),
            }),
            ReadResult::Pending => ReadBytesResult::Pending,
            ReadResult::Cancelled => ReadBytesResult::Cancelled,
        }
    }

    /// Index completeness in `[0.0, 1.0]`; `1.0` means lookups are exact.
    fn index_progress(&self) -> f64;

    /// Direct the source's read-ahead window to the current visible byte
    /// range (drive the VLF pager + syntax semantic window from the unified
    /// render path; no-op for rope).
    fn set_viewport(&self, _start: usize, _end: usize) {}

    /// Rope handle for wrap-aware view machinery (`Lines`, edit deltas, find,
    /// annotations). Rope-backed sources return `Some`; VLF returns `None`
    /// (no wrapping in Stage A, no rope-bound annotations until Phase 2).
    fn as_rope(&self) -> Option<&Rope>;
}
