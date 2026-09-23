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
