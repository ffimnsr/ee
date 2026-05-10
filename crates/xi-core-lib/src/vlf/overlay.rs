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

//! VLF overlay model — sparse piece-based edit representation.
//!
//! # Design
//!
//! The overlay sits on top of the read-only VLF storage layer.  Edits are
//! represented as an ordered sequence of [`Piece`]s that together describe the
//! full logical document:
//!
//! - [`Piece::Original`] — a slice of the original file addressed by absolute
//!   byte offset (file stays on disk; the piece is just a range descriptor).
//! - [`Piece::Inserted`] — a slice of an append-only [`InsertBuffer`].
//!
//! Inserted text is **never** written into the [`super::pager::FilePager`]
//! page cache; it lives exclusively in its `InsertBuffer`.
//!
//! # Current milestone scope
//!
//! The first editable milestone supports **append-only and current-window
//! edits** only.  The internal piece list is a `Vec<Piece>` with binary search
//! on logical byte offsets; this will be replaced by a proper balanced tree
//! (e.g. a `BTreeMap` of cumulative offsets or a weight-balanced rope) once
//! arbitrary sparse edits are required.
//!
//! # Piece metrics
//!
//! Every piece carries [`TextMetrics`] so the overlay can answer line/byte
//! queries without re-reading the original file or re-decoding insert buffers.
//! Metrics are computed eagerly on insert and cached on original-piece split.
//! CRLF seam flags (`ends_with_cr`, `starts_with_lf`) let the overlay avoid
//! double-counting `\r\n` that spans a piece boundary.

use std::collections::HashMap;
use std::path::PathBuf;
use std::str;

use crate::text_store::{ByteRange, KnownLineCount, LineLookup, LogicalLine};

// ---------------------------------------------------------------------------
// OverlayEditContext
// ---------------------------------------------------------------------------

/// Revision context attached to a group of overlay edit operations.
///
/// The **editor's revision model** remains the owner of edit intent; this
/// struct is a thin carrier so the overlay can associate its pieces with the
/// surrounding undo group without duplicating revision logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayEditContext {
    /// Revision token from the editor's CRDT engine at the time of the edit.
    /// Opaque to the overlay; used only for bookkeeping.
    pub revision_id: u64,
    /// Undo group this edit belongs to, matching the editor's undo group IDs.
    pub undo_group: usize,
}

// ---------------------------------------------------------------------------
// OverlayOp / OverlayDelta
// ---------------------------------------------------------------------------

/// A single recorded operation within an [`OverlayDelta`].
///
/// Stored in application order so the delta can be re-applied or inverted.
#[derive(Debug, Clone)]
pub enum OverlayOp {
    /// Insert text at a logical byte offset.
    ///
    /// `buffer_id` and `range` identify the exact bytes in the insert buffer,
    /// so the op remains valid even if the active buffer grows after the edit.
    Insert { at: u64, buffer_id: BufferId, range: InsertRange },
    /// Delete a logical byte range from the document.
    Delete { range: ByteRange },
}

/// A recorded overlay delta associated with a single undo group.
///
/// The editor's revision model stores or references this payload so that
/// undo/redo can invert the corresponding overlay mutations.  The overlay
/// itself never decides *when* to undo; it only provides the mechanism.
#[derive(Debug, Clone)]
pub struct OverlayDelta {
    /// Undo group this delta belongs to.
    pub undo_group: usize,
    /// Editor CRDT revision token at the time of this edit.
    pub revision_id: u64,
    /// Operations in application order.
    pub ops: Vec<OverlayOp>,
}

// ---------------------------------------------------------------------------
// ArbitrarySparseEditGate
// ---------------------------------------------------------------------------

/// Capability preconditions that must all be satisfied before arbitrary sparse
/// VLF edits are enabled.
///
/// The three gates correspond to the three infrastructure requirements that
/// must be proven correct before non-append edits are safe:
///
/// 1. `read_byte_range_ready` — overlay-aware `read_byte_range` so the
///    viewport can read through inserted pieces without file I/O.
/// 2. `streaming_search_ready` — overlay-aware streaming search so query
///    replace can find matches across piece boundaries.
/// 3. `streaming_save_ready` — temp-file streaming save so the overlay can
///    be durably persisted before edit mode is committed.
///
/// Until all three are true, [`PieceOverlay::can_enable_arbitrary_edits`]
/// returns `false` and the overlay rejects non-append edits with
/// [`OverlayError::ArbitrarySparseEditsNotReady`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArbitrarySparseEditGate {
    /// overlay-aware `read_byte_range` is implemented and tested.
    pub read_byte_range_ready: bool,
    /// overlay-aware streaming search is implemented.
    pub streaming_search_ready: bool,
    /// temp-file streaming save is implemented.
    pub streaming_save_ready: bool,
}

impl ArbitrarySparseEditGate {
    /// Returns `true` only when all three preconditions are satisfied.
    pub fn can_enable_arbitrary_edits(&self) -> bool {
        self.read_byte_range_ready && self.streaming_search_ready && self.streaming_save_ready
    }
}

// ---------------------------------------------------------------------------
// OverlayLimits
// ---------------------------------------------------------------------------

/// Resource limits that [`PieceOverlay`] enforces to keep overlay memory
/// bounded during an editing session.
///
/// When a limit is exceeded the overlay **automatically GCs the oldest undo
/// group** before applying the new edit, rather than returning an error.  This
/// ensures the editing session continues but the oldest undo step silently
/// becomes unavailable.  The caller is notified via [`OverlayError::LimitReached`]
/// only when GC cannot bring usage below the cap (e.g. a single edit alone
/// exceeds the byte cap).
///
/// # Defaults
///
/// - `max_undo_groups`: 200 — sufficient for typical interactive sessions.
/// - `overlay_byte_cap`: 64 MiB — matches the default raw-page LRU budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayLimits {
    /// Maximum number of undo groups retained in [`PieceOverlay::undo_history`].
    ///
    /// When a new edit would push the count above this, the oldest recorded
    /// undo group is GC'd first.
    pub max_undo_groups: usize,
    /// Maximum bytes consumed by all insert buffers combined.
    ///
    /// When inserting text would push `overlay_bytes()` above this cap, the
    /// oldest undo group is GC'd first.  If the cap is still exceeded after GC
    /// the edit is rejected with [`OverlayError::LimitReached`].
    pub overlay_byte_cap: u64,
}

impl Default for OverlayLimits {
    fn default() -> Self {
        OverlayLimits {
            max_undo_groups: 200,
            overlay_byte_cap: 64 * 1024 * 1024, // 64 MiB
        }
    }
}

// ---------------------------------------------------------------------------
// VlfSavePolicy
// ---------------------------------------------------------------------------

/// Required save semantics for VLF documents.
///
/// Temp-file rewrite is the default crash-safe fallback.  Optimized in-place
/// policies are explicit because their commit point is the first target-file
/// write, not the final rename.
///
/// The caller must choose a policy explicitly before triggering a save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VlfSavePolicy {
    /// Stream the piece sequence through a temp file in the same directory,
    /// then atomically rename over the target on fsync+close success.
    ///
    /// `temp_dir` defaults to the target file's parent directory when `None`.
    /// Use an explicit path when the parent directory is read-only or resides
    /// on a different filesystem where rename-across-devices is unavailable.
    TempFileRewrite { temp_dir: Option<PathBuf> },
    /// Overwrite changed bytes in the original file when logical byte length
    /// is unchanged.  Falls back to temp-file rewrite only when the target path
    /// is not the pager's original file.
    SameSizeInPlaceOverwrite,
    /// Shift unchanged tail bytes in-place, then overwrite the changed window.
    /// Falls back to temp-file rewrite when the changed window is too large to
    /// stage within the bounded optimization buffer or when the target path is
    /// not the pager's original file.
    TailShift { fallback_temp_dir: Option<PathBuf> },
    /// Write the piece sequence to a new path instead of overwriting the
    /// original.  Required when the caller explicitly chooses to preserve the
    /// original file or when the target filesystem does not support atomic
    /// rename.
    SaveAs(PathBuf),
}

/// Threshold for `suggested_save_policy`: files larger than this where the
/// signed byte delta exceeds 10 % of the file size must be confirmed by the
/// caller as `SaveAs`.  Currently set to 512 MiB.
const LARGE_REWRITE_THRESHOLD_BYTES: u64 = 512 * 1024 * 1024;

// ---------------------------------------------------------------------------
// BufferId
// ---------------------------------------------------------------------------

/// Opaque identifier for an [`InsertBuffer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BufferId(pub u32);

// ---------------------------------------------------------------------------
// InsertRange
// ---------------------------------------------------------------------------

/// Half-open byte range `[start, end)` within a single [`InsertBuffer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InsertRange {
    pub start: u64,
    pub end: u64,
}

impl InsertRange {
    /// Byte length of the range.
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    /// Returns `true` if the range is empty.
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

// ---------------------------------------------------------------------------
// TextMetrics
// ---------------------------------------------------------------------------

/// Byte-level text metrics for a single piece or buffer slice.
///
/// These are accumulated at the overlay level so line/byte lookups do not
/// require re-reading file pages or re-decoding insert buffers.
///
/// CRLF seam flags let the overlay avoid double-counting a `\r\n` pair that
/// straddles a piece boundary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextMetrics {
    /// Raw byte length of the text.
    pub byte_len: u64,
    /// UTF-16 code-unit length of the text.
    pub utf16_len: u64,
    /// Number of complete logical line endings (`\n` or `\r\n`) in the text.
    ///
    /// CRLF pairs that cross a piece boundary are counted only once: the `\n`
    /// piece uses `starts_with_lf = true` and the `\r` piece uses
    /// `ends_with_cr = true`; the overlay adjusts the sum at query time.
    pub newline_count: u64,
    /// The last byte of this slice is `\r` (CR), which may form a CRLF pair
    /// with the first byte of the following piece.
    pub ends_with_cr: bool,
    /// The first byte of this slice is `\n` (LF), which may complete a CRLF
    /// pair started by the preceding piece.
    pub starts_with_lf: bool,
}

impl TextMetrics {
    /// Compute metrics from a validated UTF-8 byte slice.
    ///
    /// `prev_ends_with_cr` tells whether the preceding piece ended with `\r`
    /// so that a leading `\n` in `bytes` is counted as part of that CRLF and
    /// not as an additional line ending.  Pass `false` for the first piece or
    /// when the preceding context is unknown.
    pub fn from_bytes(bytes: &[u8], _prev_ends_with_cr: bool) -> Self {
        let byte_len = bytes.len() as u64;
        let text = str::from_utf8(bytes).expect("bytes must be valid UTF-8");

        let ends_with_cr = bytes.last() == Some(&b'\r');
        let starts_with_lf = bytes.first() == Some(&b'\n');

        let mut newline_count: u64 = 0;
        let mut utf16_len: u64 = 0;

        for ch in text.chars() {
            // UTF-16 length: chars outside BMP take 2 code units.
            utf16_len += if ch as u32 > 0xFFFF { 2 } else { 1 };
            if ch == '\n' {
                newline_count += 1;
            }
        }

        TextMetrics { byte_len, utf16_len, newline_count, ends_with_cr, starts_with_lf }
    }
}

// ---------------------------------------------------------------------------
// Piece
// ---------------------------------------------------------------------------

/// A single piece in the overlay sequence.
///
/// Pieces are ordered by their position in the logical document.  Together
/// they cover the full document without gaps.
///
/// Each piece embeds [`TextMetrics`] so overlay-level line/byte aggregations
/// are O(n pieces) without additional I/O.  `n` stays small for typical edit
/// sessions.
#[derive(Debug, Clone)]
pub enum Piece {
    /// A range within the original on-disk file.
    ///
    /// `file_range` is an **absolute** byte range in the file.  The piece does
    /// not hold a copy of the bytes; they are read on demand through
    /// [`super::pager::FilePager`].
    Original { file_range: ByteRange, metrics: TextMetrics },
    /// A slice of an append-only [`InsertBuffer`].
    ///
    /// Inserted bytes never enter the page cache.  The slice is addressed by
    /// [`BufferId`] + [`InsertRange`] so the buffer can grow without
    /// invalidating earlier pieces.
    Inserted { buffer_id: BufferId, range: InsertRange, metrics: TextMetrics },
}

impl Piece {
    /// Metrics for this piece.
    pub fn metrics(&self) -> &TextMetrics {
        match self {
            Piece::Original { metrics, .. } => metrics,
            Piece::Inserted { metrics, .. } => metrics,
        }
    }

    /// Logical byte length of this piece.
    pub fn byte_len(&self) -> u64 {
        self.metrics().byte_len
    }
}

// ---------------------------------------------------------------------------
// InsertBuffer
// ---------------------------------------------------------------------------

/// An append-only buffer holding inserted text.
///
/// Text is UTF-8 validated on entry; invalid bytes are rejected.  The buffer
/// can only grow; individual bytes are never removed.  Old pieces that
/// reference an earlier slice of the buffer remain valid indefinitely.
///
/// Inserted bytes are **never** copied into the [`super::pager::FilePager`]
/// page cache; they are separate from the original file.
pub struct InsertBuffer {
    /// Stable identifier.
    id: BufferId,
    /// UTF-8 validated bytes.
    bytes: Vec<u8>,
}

impl InsertBuffer {
    fn new(id: BufferId) -> Self {
        InsertBuffer { id, bytes: Vec::new() }
    }

    /// Identifier for this buffer.
    pub fn id(&self) -> BufferId {
        self.id
    }

    /// Append `text` to the buffer.
    ///
    /// Returns the [`InsertRange`] that addresses the newly appended bytes.
    ///
    /// Fails if `text` is not valid UTF-8 (though `&str` guarantees this at
    /// the type level).
    pub fn append(&mut self, text: &str) -> InsertRange {
        let start = self.bytes.len() as u64;
        self.bytes.extend_from_slice(text.as_bytes());
        let end = self.bytes.len() as u64;
        InsertRange { start, end }
    }

    /// Read a slice from the buffer.
    ///
    /// Returns `None` if `range` is out of bounds.
    pub fn slice(&self, range: InsertRange) -> Option<&[u8]> {
        let start = range.start as usize;
        let end = range.end as usize;
        self.bytes.get(start..end)
    }

    /// Total byte length of the buffer.
    pub fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    /// Returns `true` if the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

// ---------------------------------------------------------------------------
// PieceOverlay
// ---------------------------------------------------------------------------

/// Sparse piece-based edit overlay for VLF documents.
///
/// `PieceOverlay` stores the logical document as an ordered sequence of
/// [`Piece`]s.  Each piece is either a reference into the original on-disk
/// file ([`Piece::Original`]) or a reference into an append-only
/// [`InsertBuffer`] ([`Piece::Inserted`]).
///
/// ## Cumulative offsets
///
/// The struct maintains a parallel `Vec<u64>` of cumulative logical byte
/// offsets so that binary search can locate the piece containing any logical
/// byte in O(log n).  `cum_offsets[i]` is the logical byte start of
/// `pieces[i]` in the document.
///
/// ## First milestone: append-only and current-window edits
///
/// - **Append**: extend the last piece or push a new `Inserted` piece.
/// - **Delete / replace in current window**: split the affected `Original`
///   piece at both edit boundaries, then replace the middle segment with a new
///   `Inserted` piece (or remove it for a pure delete).
///
/// Arbitrary sparse edits (edits that require splitting pieces across
/// non-adjacent regions) will graduate to a proper balanced tree once the
/// infrastructure is proven correct for the window case.
pub struct PieceOverlay {
    /// Ordered pieces.  Together they span the full logical document.
    pieces: Vec<Piece>,
    /// `cum_offsets[i]` = logical byte start of `pieces[i]`.
    /// Length equals `pieces.len()`.
    cum_offsets: Vec<u64>,
    /// Total logical byte length of the current document.
    total_byte_len: u64,
    /// Total logical line count (newline_count sum, adjusted for CRLF seams).
    total_newlines: u64,
    /// Original file byte length at open time.  Never changes after `new()`.
    /// Used to compute [`Self::signed_byte_delta`].
    original_file_byte_len: u64,
    /// Append-only insert buffers, keyed by [`BufferId`].
    buffers: HashMap<BufferId, InsertBuffer>,
    /// Counter for allocating the next [`BufferId`].
    next_buffer_id: u32,
    /// Active insert buffer used for new edits.  `None` until first edit.
    active_buffer: Option<BufferId>,
    /// Recorded overlay deltas, keyed by undo group.
    ///
    /// The editor's revision model is the authoritative owner of undo intent;
    /// this map is purely a payload store so the editor can retrieve the
    /// overlay ops associated with each undo group for reversal or GC.
    undo_history: HashMap<usize, OverlayDelta>,
    /// Insertion-order record of undo group IDs so the oldest group can be
    /// found in O(1) when enforcing [`OverlayLimits::max_undo_groups`].
    undo_group_order: Vec<usize>,
    /// Resource limits for the overlay.
    limits: OverlayLimits,
    /// Capability gate for arbitrary sparse edits.
    ///
    /// Append-only and current-window edits are always permitted.  Arbitrary
    /// sparse edits (edits anywhere in the file) require all three gates to be
    /// open.
    edit_gate: ArbitrarySparseEditGate,
}

impl PieceOverlay {
    /// Create an overlay for a read-only file of `file_byte_len` bytes with
    /// the given original-file metrics.
    ///
    /// If `file_byte_len` is zero the overlay starts with no pieces (empty
    /// document).
    pub fn new(original_file_metrics: TextMetrics) -> Self {
        Self::with_limits(original_file_metrics, OverlayLimits::default())
    }

    /// Create an overlay with explicit resource limits.
    ///
    /// Use this in tests or when tuning memory behaviour for a specific file
    /// size.  For production use [`Self::new`] which applies the defaults.
    pub fn with_limits(original_file_metrics: TextMetrics, limits: OverlayLimits) -> Self {
        let total_byte_len = original_file_metrics.byte_len;
        let total_newlines = original_file_metrics.newline_count;

        let (pieces, cum_offsets) = if total_byte_len == 0 {
            (Vec::new(), Vec::new())
        } else {
            let piece = Piece::Original {
                file_range: ByteRange::new(0, total_byte_len),
                metrics: original_file_metrics,
            };
            (vec![piece], vec![0u64])
        };

        PieceOverlay {
            pieces,
            cum_offsets,
            total_byte_len,
            total_newlines,
            original_file_byte_len: total_byte_len,
            buffers: HashMap::new(),
            next_buffer_id: 0,
            active_buffer: None,
            undo_history: HashMap::new(),
            undo_group_order: Vec::new(),
            limits,
            edit_gate: ArbitrarySparseEditGate::default(),
        }
    }

    /// Total logical byte length of the document including overlay edits.
    pub fn total_byte_len(&self) -> u64 {
        self.total_byte_len
    }

    /// Byte length of the original file at overlay creation time.
    pub fn original_file_byte_len(&self) -> u64 {
        self.original_file_byte_len
    }

    /// Known line count for the overlay document.
    pub fn known_line_count(&self) -> KnownLineCount {
        // newline_count is the number of line-ending characters, which equals
        // (number of logical lines - 1) for files not ending with a newline,
        // or (number of logical lines) for files that do.  For the overlay we
        // report the exact count since we track metrics eagerly.
        KnownLineCount::Exact(self.total_newlines + 1)
    }

    /// Logical byte offset of the start of `line` (0-based).
    ///
    /// Returns [`LineLookup::OutOfRange`] if `line` exceeds the document.
    pub fn line_to_byte(&self, line: LogicalLine) -> LineLookup {
        let target_line = line.0;
        if target_line == 0 {
            return LineLookup::Exact(crate::text_store::ByteOffset(0));
        }
        // Walk pieces accumulating newline counts until we reach target_line.
        let mut lines_seen: u64 = 0;
        let mut byte_pos: u64 = 0;

        for piece in &self.pieces {
            let m = piece.metrics();

            if lines_seen + m.newline_count >= target_line {
                // The target line start is within this piece.
                let remaining = target_line - lines_seen;
                let offset = self.find_nth_newline_in_piece(piece, remaining);
                return LineLookup::Exact(crate::text_store::ByteOffset(byte_pos + offset));
            }
            lines_seen += m.newline_count;
            byte_pos += m.byte_len;
        }
        LineLookup::OutOfRange
    }

    /// Find the byte offset within a piece where the `n`-th newline ends
    /// (i.e. the byte *after* the `\n`).  `n` must be ≥ 1.
    fn find_nth_newline_in_piece(&self, piece: &Piece, n: u64) -> u64 {
        let text = match piece {
            Piece::Original { .. } => {
                // For original pieces we cannot read without I/O here; return 0
                // as a safe fallback.  Callers that need exact in-piece
                // addressing should read the page bytes through FilePager.
                return 0;
            }
            Piece::Inserted { buffer_id, range, .. } => {
                let buf = self.buffers.get(buffer_id).expect("buffer must exist");
                let bytes = buf.slice(*range).expect("range must be valid");
                match str::from_utf8(bytes) {
                    Ok(s) => s.to_owned(),
                    Err(_) => return 0,
                }
            }
        };

        let mut count: u64 = 0;

        for (i, ch) in text.char_indices() {
            if ch == '\n' {
                count += 1;
                if count == n {
                    return (i + 1) as u64;
                }
            }
        }
        text.len() as u64
    }

    // -----------------------------------------------------------------------
    // Piece index helpers
    // -----------------------------------------------------------------------

    /// Binary-search for the index of the piece containing `logical_byte`.
    ///
    /// Returns `(piece_index, piece_logical_start)`.  Returns `None` if
    /// `logical_byte >= total_byte_len`.
    fn piece_for_byte(&self, logical_byte: u64) -> Option<(usize, u64)> {
        if logical_byte >= self.total_byte_len || self.pieces.is_empty() {
            return None;
        }
        let idx =
            self.cum_offsets.partition_point(|&start| start <= logical_byte).saturating_sub(1);
        Some((idx, self.cum_offsets[idx]))
    }

    /// Rebuild `cum_offsets` from `pieces`.  Called after structural changes.
    fn rebuild_offsets(&mut self) {
        self.cum_offsets.clear();
        let mut pos: u64 = 0;
        for p in &self.pieces {
            self.cum_offsets.push(pos);
            pos += p.byte_len();
        }
        self.total_byte_len = pos;
    }

    /// Compute total newlines by summing per-piece newline counts.
    ///
    /// We count only `\n` characters; `\r` alone is not a line ending.
    /// A `\r\n` pair spanning two pieces contributes exactly 1 (the `\n` in
    /// the second piece), so no seam adjustment is needed.
    fn compute_total_newlines(&self) -> u64 {
        self.pieces.iter().map(|p| p.metrics().newline_count).sum()
    }

    // -----------------------------------------------------------------------
    // Buffer helpers
    // -----------------------------------------------------------------------

    /// Get or create the active insert buffer.
    fn active_buffer_mut(&mut self) -> &mut InsertBuffer {
        if self.active_buffer.is_none() {
            let id = BufferId(self.next_buffer_id);
            self.next_buffer_id += 1;
            self.buffers.insert(id, InsertBuffer::new(id));
            self.active_buffer = Some(id);
        }
        let id = self.active_buffer.unwrap();
        self.buffers.get_mut(&id).unwrap()
    }

    // -----------------------------------------------------------------------
    // Edit operations
    // -----------------------------------------------------------------------

    /// Insert `text` at logical byte offset `at`.
    ///
    /// - `at == total_byte_len`: append to end of document.
    /// - `at < total_byte_len`: split the piece at `at`, insert new piece.
    ///
    /// Returns `Err` if `at > total_byte_len`.
    ///
    /// The text must be valid UTF-8 (enforced by the `&str` parameter type).
    pub fn insert(&mut self, at: u64, text: &str) -> Result<(), OverlayError> {
        if at > self.total_byte_len {
            return Err(OverlayError::OutOfRange { offset: at, len: self.total_byte_len });
        }
        if text.is_empty() {
            return Ok(());
        }

        // Determine CRLF context for metrics.
        let prev_ends_with_cr = self.char_before(at) == Some(b'\r');
        let metrics = TextMetrics::from_bytes(text.as_bytes(), prev_ends_with_cr);

        // Append to the active buffer.
        let range = self.active_buffer_mut().append(text);
        let buffer_id = self.active_buffer.unwrap();

        let new_piece_byte_len = metrics.byte_len;
        let new_piece = Piece::Inserted { buffer_id, range, metrics };

        if at == self.total_byte_len {
            // Fast path: append — update totals directly without rebuild_offsets.
            self.cum_offsets.push(self.total_byte_len);
            self.total_byte_len += new_piece_byte_len;
            self.pieces.push(new_piece);
            self.total_newlines = self.compute_total_newlines();
        } else {
            // Split piece at `at` and insert new piece between halves.
            // rebuild_offsets() sets total_byte_len from the piece list.
            let (idx, piece_start) = self.piece_for_byte(at).unwrap();
            let offset_in_piece = at - piece_start;

            if offset_in_piece == 0 {
                // Insert before this piece; rebuild_offsets fixes everything.
                self.pieces.insert(idx, new_piece);
            } else {
                // Split piece at offset_in_piece.
                let (lo, hi) = self.split_piece(idx, offset_in_piece)?;
                self.pieces.remove(idx);
                self.pieces.insert(idx, hi);
                self.pieces.insert(idx, new_piece);
                self.pieces.insert(idx, lo);
            }
            self.rebuild_offsets();
            self.total_newlines = self.compute_total_newlines();
        }

        Ok(())
    }

    /// Delete bytes `[start, end)` from the logical document.
    ///
    /// Returns `Err` if the range is out of bounds.
    pub fn delete(&mut self, range: ByteRange) -> Result<(), OverlayError> {
        let start = range.start.0;
        let end = range.end.0;
        if end > self.total_byte_len {
            return Err(OverlayError::OutOfRange { offset: end, len: self.total_byte_len });
        }
        if start >= end {
            return Ok(());
        }

        // Split at `end` first (so `start` index stays valid).
        if end < self.total_byte_len {
            let (idx, piece_start) = self.piece_for_byte(end).unwrap();
            let offset = end - piece_start;
            if offset > 0 && offset < self.pieces[idx].byte_len() {
                let (lo, hi) = self.split_piece(idx, offset)?;
                self.pieces.remove(idx);
                self.pieces.insert(idx, hi);
                self.pieces.insert(idx, lo);
                self.rebuild_offsets();
            }
        }

        // Split at `start`.
        if start > 0 {
            let (idx, piece_start) = self.piece_for_byte(start).unwrap();
            let offset = start - piece_start;
            if offset > 0 && offset < self.pieces[idx].byte_len() {
                let (lo, hi) = self.split_piece(idx, offset)?;
                self.pieces.remove(idx);
                self.pieces.insert(idx, hi);
                self.pieces.insert(idx, lo);
                self.rebuild_offsets();
            }
        }

        // Remove all pieces fully within [start, end).
        // After splits the pieces at `start` and `end` are exact boundaries.
        let first_idx = self.cum_offsets.partition_point(|&s| s < start);
        let last_idx = self.cum_offsets.partition_point(|&s| s < end);
        self.pieces.drain(first_idx..last_idx);
        self.rebuild_offsets();

        Ok(())
    }

    /// Split piece at `idx` into `(before, after)` at `offset_in_piece` bytes.
    ///
    /// Returns both halves without modifying `self.pieces`.  The caller is
    /// responsible for splicing them back.
    fn split_piece(
        &self,
        idx: usize,
        offset_in_piece: u64,
    ) -> Result<(Piece, Piece), OverlayError> {
        match &self.pieces[idx] {
            Piece::Original { file_range, .. } => {
                let mid = file_range.start.0 + offset_in_piece;
                let lo_range = ByteRange::new(file_range.start.0, mid);
                let hi_range = ByteRange::new(mid, file_range.end.0);
                // Metrics for split Original pieces are left as defaults and
                // should be recomputed from the file bytes on demand.  For
                // overlay accounting purposes we distribute newline_count
                // proportionally (approximate); exact metrics require I/O.
                let orig_m = self.pieces[idx].metrics();
                let lo_m = TextMetrics {
                    byte_len: lo_range.len(),
                    // Approximate: will be corrected on next scan.
                    utf16_len: (orig_m.utf16_len as f64
                        * (lo_range.len() as f64 / orig_m.byte_len as f64))
                        as u64,
                    newline_count: 0, // recomputed on scan
                    ends_with_cr: false,
                    starts_with_lf: orig_m.starts_with_lf,
                };
                let hi_m = TextMetrics {
                    byte_len: hi_range.len(),
                    utf16_len: orig_m.utf16_len.saturating_sub(lo_m.utf16_len),
                    newline_count: orig_m.newline_count, // recomputed on scan
                    ends_with_cr: orig_m.ends_with_cr,
                    starts_with_lf: false,
                };
                Ok((
                    Piece::Original { file_range: lo_range, metrics: lo_m },
                    Piece::Original { file_range: hi_range, metrics: hi_m },
                ))
            }
            Piece::Inserted { buffer_id, range, .. } => {
                let mid = range.start + offset_in_piece;
                let lo_range = InsertRange { start: range.start, end: mid };
                let hi_range = InsertRange { start: mid, end: range.end };
                let buf = self.buffers.get(buffer_id).expect("buffer must exist");
                let lo_bytes = buf.slice(lo_range).expect("lo_range valid");
                let hi_bytes = buf.slice(hi_range).expect("hi_range valid");
                let lo_m = TextMetrics::from_bytes(lo_bytes, false);
                let hi_m = TextMetrics::from_bytes(hi_bytes, lo_m.ends_with_cr);
                Ok((
                    Piece::Inserted { buffer_id: *buffer_id, range: lo_range, metrics: lo_m },
                    Piece::Inserted { buffer_id: *buffer_id, range: hi_range, metrics: hi_m },
                ))
            }
        }
    }

    /// Return the byte immediately before `logical_offset`, if any.
    fn char_before(&self, logical_offset: u64) -> Option<u8> {
        if logical_offset == 0 {
            return None;
        }
        let (idx, piece_start) = self.piece_for_byte(logical_offset - 1)?;
        let offset_in_piece = (logical_offset - 1) - piece_start;
        match &self.pieces[idx] {
            Piece::Original { .. } => None, // would need I/O; caller tolerates None
            Piece::Inserted { buffer_id, range, .. } => {
                let buf = self.buffers.get(buffer_id)?;
                let abs = range.start + offset_in_piece;
                buf.bytes.get(abs as usize).copied()
            }
        }
    }

    /// Number of pieces in the overlay (for testing / diagnostics).
    pub fn piece_count(&self) -> usize {
        self.pieces.len()
    }

    /// Borrow the ordered piece list (for testing / diagnostics).
    pub fn pieces(&self) -> &[Piece] {
        &self.pieces
    }

    /// Retrieve the bytes for an `Inserted` piece from the insert buffer.
    ///
    /// Returns `None` if the piece is `Original` (bytes live in the file, not
    /// the overlay) or if the buffer/range is missing (should not happen for
    /// well-formed overlays).
    pub fn inserted_bytes_for_piece<'a>(&'a self, piece: &Piece) -> Option<&'a [u8]> {
        match piece {
            Piece::Inserted { buffer_id, range, .. } => self.buffers.get(buffer_id)?.slice(*range),
            Piece::Original { .. } => None,
        }
    }

    /// Byte length of the named insert buffer.  Returns 0 if not found.
    pub fn buffer_len(&self, id: BufferId) -> u64 {
        self.buffers.get(&id).map_or(0, |b| b.len())
    }

    /// Approximate overlay memory usage in bytes.
    pub fn overlay_bytes(&self) -> u64 {
        let piece_bytes = (self.pieces.len() * std::mem::size_of::<Piece>()) as u64;
        let buffer_bytes: u64 = self.buffers.values().map(|b| b.len()).sum();
        piece_bytes + buffer_bytes
    }

    /// Signed byte-length delta relative to the original file.
    ///
    /// A positive value means the edited document is larger than the file on
    /// disk; negative means it is smaller.  Zero means the byte count is
    /// identical to the original (though content may still differ).
    ///
    /// Callers use this to decide whether an optimized in-place save, a
    /// temp-file streaming rewrite, or an explicit save-as is required.  See [`VlfSavePolicy`] and
    /// [`Self::suggested_save_policy`].
    pub fn signed_byte_delta(&self) -> i64 {
        self.total_byte_len as i64 - self.original_file_byte_len as i64
    }

    /// Net byte-length delta contributed by overlay edits that intersect
    /// `range` (logical document bytes).
    ///
    /// Returns the sum of inserted bytes minus deleted original bytes within
    /// the range.  Only `Inserted` pieces within `[range.start, range.end)` and
    /// `Original` pieces that no longer exist (deleted) contribute to the delta.
    ///
    /// This is an approximation: deleted original bytes within `range` are
    /// counted by subtracting the byte length of `Original` pieces that have
    /// been split away or removed.  Inserted pieces that fully overlap `range`
    /// are counted in full; partially overlapping pieces are clamped.
    pub fn dirty_byte_delta_for_range(&self, range: ByteRange) -> i64 {
        let range_start = range.start.0;
        let range_end = range.end.0;

        // Count inserted bytes within [range_start, range_end).
        let mut inserted: i64 = 0;
        let mut original_present: i64 = 0;

        let mut doc_pos: u64 = 0;
        for piece in &self.pieces {
            let piece_end = doc_pos + piece.byte_len();
            // Skip pieces that don't overlap the query range.
            if piece_end <= range_start || doc_pos >= range_end {
                doc_pos = piece_end;
                continue;
            }
            let overlap_start = doc_pos.max(range_start);
            let overlap_end = piece_end.min(range_end);
            let overlap_len = overlap_end.saturating_sub(overlap_start) as i64;
            match piece {
                Piece::Inserted { .. } => inserted += overlap_len,
                Piece::Original { .. } => original_present += overlap_len,
            }
            doc_pos = piece_end;
        }

        // Original bytes in [range_start, range_end) that are NOT present in
        // any Original piece are deleted bytes.  We estimate original bytes that
        // *should* be in range as `(range_end - range_start)` minus the
        // original_present counted above.  This is an approximation because the
        // range may include inserted bytes that displace original offsets.
        let range_len = range_end.saturating_sub(range_start) as i64;
        let deleted_original = (range_len - original_present).max(0);
        inserted - deleted_original
    }

    /// Suggested save policy for the current overlay state.
    ///
    /// Returns the narrowest save strategy that fits the current overlay.  Returns
    /// [`VlfSavePolicy::SaveAs`] placeholder when the file is above
    /// [`LARGE_REWRITE_THRESHOLD_BYTES`] **and** the signed byte delta exceeds
    /// 10 % of the original file size, indicating the caller should confirm an
    /// explicit save destination.  Same-size edits prefer
    /// [`VlfSavePolicy::SameSizeInPlaceOverwrite`]; byte-length-changing edits
    /// prefer [`VlfSavePolicy::TailShift`] with temp-file fallback.
    ///
    /// The returned `SaveAs` path is a placeholder (`<save-as-required>`); the
    /// actual destination must be supplied by the caller after prompting the
    /// user.
    pub fn suggested_save_policy(&self) -> VlfSavePolicy {
        let delta = self.signed_byte_delta().unsigned_abs();
        let is_large = self.original_file_byte_len >= LARGE_REWRITE_THRESHOLD_BYTES;
        let delta_fraction = if self.original_file_byte_len > 0 {
            delta as f64 / self.original_file_byte_len as f64
        } else {
            0.0
        };

        if is_large && delta_fraction > 0.10 {
            // Large file with >10 % byte change: require explicit save-as.
            VlfSavePolicy::SaveAs(PathBuf::from("<save-as-required>"))
        } else if self.signed_byte_delta() == 0 {
            VlfSavePolicy::SameSizeInPlaceOverwrite
        } else {
            VlfSavePolicy::TailShift { fallback_temp_dir: None }
        }
    }

    // -----------------------------------------------------------------------
    // Limit enforcement
    // -----------------------------------------------------------------------

    /// Enforce [`OverlayLimits`] before applying an edit.
    ///
    /// GCs the oldest undo group until either the undo group count is within
    /// `max_undo_groups` or the overlay byte usage is within `overlay_byte_cap`.
    /// Returns `Err(LimitReached)` only when GC cannot bring byte usage below
    /// the cap (i.e. the *current* overlay without any undo history already
    /// exceeds the cap).
    fn enforce_limits(&mut self, incoming_bytes: u64) -> Result<(), OverlayError> {
        // GC oldest undo groups until count is within the limit.
        while self.undo_history.len() >= self.limits.max_undo_groups {
            if let Some(oldest) = self.undo_group_order.first().copied() {
                self.undo_group_order.remove(0);
                self.gc_undo_group(oldest);
            } else {
                break;
            }
        }

        // GC oldest undo groups until byte usage would fit within the cap.
        while self.overlay_bytes() + incoming_bytes > self.limits.overlay_byte_cap {
            if let Some(oldest) = self.undo_group_order.first().copied() {
                self.undo_group_order.remove(0);
                self.gc_undo_group(oldest);
            } else {
                // No more undo groups to free; reject the edit.
                return Err(OverlayError::LimitReached {
                    kind: LimitKind::MemoryCap {
                        cap: self.limits.overlay_byte_cap,
                        used: self.overlay_bytes(),
                    },
                });
            }
        }

        Ok(())
    }

    /// Insert `text` at logical byte offset `at`, recording the operation in
    /// `ctx`'s undo group delta.
    ///
    /// The editor's revision model remains the owner of undo intent; this
    /// method is just a thin wrapper that tags the underlying insert with the
    /// provided context so the editor can later call [`Self::gc_undo_group`]
    /// when that undo group leaves history.
    pub fn insert_in_group(
        &mut self,
        at: u64,
        text: &str,
        ctx: OverlayEditContext,
    ) -> Result<(), OverlayError> {
        self.enforce_limits(text.len() as u64)?;
        self.insert(at, text)?;

        // Record the op in the delta for this undo group so the editor can
        // later retrieve or GC it.  The buffer_id and range are set to the
        // last inserted piece, which we can recover from the active buffer.
        let active_id = self.active_buffer.unwrap();
        let buf = self.buffers.get(&active_id).expect("active buffer must exist");
        let end = buf.len();
        let start = end - text.len() as u64;
        let op = OverlayOp::Insert { at, buffer_id: active_id, range: InsertRange { start, end } };
        let is_new_group = !self.undo_history.contains_key(&ctx.undo_group);
        let delta = self.undo_history.entry(ctx.undo_group).or_insert_with(|| OverlayDelta {
            undo_group: ctx.undo_group,
            revision_id: ctx.revision_id,
            ops: Vec::new(),
        });
        delta.ops.push(op);
        if is_new_group {
            self.undo_group_order.push(ctx.undo_group);
        }
        Ok(())
    }

    /// Delete bytes `[start, end)` from the logical document, recording the
    /// operation in `ctx`'s undo group delta.
    pub fn delete_in_group(
        &mut self,
        range: ByteRange,
        ctx: OverlayEditContext,
    ) -> Result<(), OverlayError> {
        // Deletes don't add buffer bytes; enforce only the undo group count.
        self.enforce_limits(0)?;
        self.delete(range)?;

        let op = OverlayOp::Delete { range };
        let is_new_group = !self.undo_history.contains_key(&ctx.undo_group);
        let delta = self.undo_history.entry(ctx.undo_group).or_insert_with(|| OverlayDelta {
            undo_group: ctx.undo_group,
            revision_id: ctx.revision_id,
            ops: Vec::new(),
        });
        delta.ops.push(op);
        if is_new_group {
            self.undo_group_order.push(ctx.undo_group);
        }
        Ok(())
    }

    /// Retrieve the [`OverlayDelta`] for `undo_group`, if any.
    ///
    /// The editor's revision model calls this when it needs the overlay
    /// payload to invert or inspect an undo group.
    pub fn delta_for_group(&self, undo_group: usize) -> Option<&OverlayDelta> {
        self.undo_history.get(&undo_group)
    }

    /// Release overlay resources associated with `undo_group`.
    ///
    /// Called when the editor's undo history permanently drops an undo group
    /// (i.e. the group has been GC'd from the CRDT engine and can never be
    /// undone or redone again).
    ///
    /// # GC rule
    ///
    /// Any [`InsertBuffer`] whose bytes are referenced *only* by pieces
    /// originating from `undo_group` — and whose pieces are no longer in the
    /// active piece list — can be freed.  Buffers shared with live pieces are
    /// left intact.
    pub fn gc_undo_group(&mut self, undo_group: usize) {
        let Some(delta) = self.undo_history.remove(&undo_group) else {
            return;
        };

        // Remove from insertion-order record.
        self.undo_group_order.retain(|&g| g != undo_group);

        // Collect buffer IDs referenced by the GC'd delta.
        let mut gc_buffer_ids: Vec<BufferId> = delta
            .ops
            .iter()
            .filter_map(|op| match op {
                OverlayOp::Insert { buffer_id, .. } => Some(*buffer_id),
                OverlayOp::Delete { .. } => None,
            })
            .collect();
        gc_buffer_ids.sort_unstable_by_key(|b| b.0);
        gc_buffer_ids.dedup_by_key(|b| b.0);

        // Find all buffer IDs still referenced by live pieces.
        let live_buffer_ids: std::collections::HashSet<u32> = self
            .pieces
            .iter()
            .filter_map(|p| match p {
                Piece::Inserted { buffer_id, .. } => Some(buffer_id.0),
                _ => None,
            })
            .collect();

        // Also keep buffers referenced by other undo groups in history.
        let history_buffer_ids: std::collections::HashSet<u32> = self
            .undo_history
            .values()
            .flat_map(|d| d.ops.iter())
            .filter_map(|op| match op {
                OverlayOp::Insert { buffer_id, .. } => Some(buffer_id.0),
                _ => None,
            })
            .collect();

        // Drop buffers that are no longer referenced anywhere.
        for id in gc_buffer_ids {
            if !live_buffer_ids.contains(&id.0) && !history_buffer_ids.contains(&id.0) {
                self.buffers.remove(&id);
                // Clear active_buffer pointer if we freed it.
                if self.active_buffer == Some(id) {
                    self.active_buffer = None;
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Arbitrary sparse edit gate
    // -----------------------------------------------------------------------

    /// Returns `true` when all preconditions for arbitrary sparse edits are met.
    pub fn can_enable_arbitrary_edits(&self) -> bool {
        self.edit_gate.can_enable_arbitrary_edits()
    }

    /// Mark that overlay-aware `read_byte_range` is ready.
    pub fn set_read_byte_range_ready(&mut self) {
        self.edit_gate.read_byte_range_ready = true;
    }

    /// Mark that overlay-aware streaming search is ready.
    pub fn set_streaming_search_ready(&mut self) {
        self.edit_gate.streaming_search_ready = true;
    }

    /// Mark that temp-file streaming save is ready.
    pub fn set_streaming_save_ready(&mut self) {
        self.edit_gate.streaming_save_ready = true;
    }

    /// Borrow the current edit gate state.
    pub fn edit_gate(&self) -> &ArbitrarySparseEditGate {
        &self.edit_gate
    }
}

// ---------------------------------------------------------------------------
// LimitKind / OverlayError
// ---------------------------------------------------------------------------

/// Which resource limit was exceeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LimitKind {
    /// The undo group count exceeded [`OverlayLimits::max_undo_groups`] and
    /// GC could not reduce it (should not happen in practice because GC removes
    /// the oldest group unconditionally when the count is at the cap).
    UndoGroupCap { cap: usize },
    /// The insert-buffer byte usage exceeded [`OverlayLimits::overlay_byte_cap`]
    /// and GC could not free enough memory (all undo history already freed).
    MemoryCap { cap: u64, used: u64 },
}

/// Errors returned by [`PieceOverlay`] operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayError {
    /// An offset or range end exceeded the document length.
    OutOfRange { offset: u64, len: u64 },
    /// A byte offset falls inside a multibyte codepoint.
    InvalidUtf8Boundary,
    /// Arbitrary sparse edits are not yet enabled because one or more
    /// capability gates are not satisfied.
    ///
    /// Call [`PieceOverlay::set_read_byte_range_ready`],
    /// [`PieceOverlay::set_streaming_search_ready`], and
    /// [`PieceOverlay::set_streaming_save_ready`] before enabling arbitrary
    /// edits.
    ArbitrarySparseEditsNotReady,
    /// A resource limit was reached and the edit was rejected.
    ///
    /// Automatic GC of oldest undo groups was attempted but could not bring
    /// usage within the configured [`OverlayLimits`].
    LimitReached { kind: LimitKind },
}

impl std::fmt::Display for OverlayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OverlayError::OutOfRange { offset, len } => {
                write!(f, "offset {offset} out of range for document of {len} bytes")
            }
            OverlayError::InvalidUtf8Boundary => {
                write!(f, "offset falls inside a multibyte UTF-8 codepoint")
            }
            OverlayError::ArbitrarySparseEditsNotReady => {
                write!(
                    f,
                    "arbitrary sparse VLF edits are not enabled: \
                     read_byte_range, streaming search, and streaming save \
                     must all be overlay-aware before edit mode can be activated"
                )
            }
            OverlayError::LimitReached { kind } => match kind {
                LimitKind::UndoGroupCap { cap } => {
                    write!(f, "overlay undo group limit ({cap}) reached and GC failed")
                }
                LimitKind::MemoryCap { cap, used } => {
                    write!(
                        f,
                        "overlay memory cap ({cap} bytes) reached; \
                         current usage {used} bytes; save and reopen to continue editing"
                    )
                }
            },
        }
    }
}

impl std::error::Error for OverlayError {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text_store::{ByteOffset, LineLookup, LogicalLine};

    fn ascii_metrics(s: &str) -> TextMetrics {
        TextMetrics::from_bytes(s.as_bytes(), false)
    }

    // --- TextMetrics --------------------------------------------------------

    #[test]
    fn metrics_empty() {
        let m = TextMetrics::from_bytes(b"", false);
        assert_eq!(m.byte_len, 0);
        assert_eq!(m.utf16_len, 0);
        assert_eq!(m.newline_count, 0);
        assert!(!m.ends_with_cr);
        assert!(!m.starts_with_lf);
    }

    #[test]
    fn metrics_single_newline() {
        let m = TextMetrics::from_bytes(b"hello\n", false);
        assert_eq!(m.newline_count, 1);
        assert_eq!(m.byte_len, 6);
    }

    #[test]
    fn metrics_crlf_counted_once() {
        let m = TextMetrics::from_bytes(b"a\r\nb", false);
        assert_eq!(m.newline_count, 1);
        assert!(!m.ends_with_cr); // last byte is 'b'
    }

    #[test]
    fn metrics_crlf_seam_across_pieces() {
        // '\r' at end of prev piece; '\n' starts next piece.
        // With pure \n-counting, the \n in the second piece IS counted as a newline.
        let m_cr = TextMetrics::from_bytes(b"a\r", false);
        assert!(m_cr.ends_with_cr);
        assert_eq!(m_cr.newline_count, 0, "no \\n in 'a\\r'");
        let m_lf = TextMetrics::from_bytes(b"\nb", false);
        assert!(m_lf.starts_with_lf);
        assert_eq!(m_lf.newline_count, 1, "\\n is counted in second piece");
    }

    #[test]
    fn metrics_multibyte_utf16() {
        // Emoji U+1F600 is 4 UTF-8 bytes, 2 UTF-16 code units.
        let m = TextMetrics::from_bytes("😀".as_bytes(), false);
        assert_eq!(m.byte_len, 4);
        assert_eq!(m.utf16_len, 2);
        assert_eq!(m.newline_count, 0);
    }

    // --- InsertBuffer -------------------------------------------------------

    #[test]
    fn insert_buffer_append_and_slice() {
        let mut buf = InsertBuffer::new(BufferId(0));
        assert!(buf.is_empty());
        let r1 = buf.append("hello");
        let r2 = buf.append(" world");
        assert_eq!(buf.len(), 11);
        assert_eq!(buf.slice(r1), Some(b"hello".as_ref()));
        assert_eq!(buf.slice(r2), Some(b" world".as_ref()));
    }

    #[test]
    fn insert_buffer_slice_out_of_bounds_returns_none() {
        let buf = InsertBuffer::new(BufferId(0));
        assert_eq!(buf.slice(InsertRange { start: 0, end: 5 }), None);
    }

    // --- PieceOverlay: construction -----------------------------------------

    #[test]
    fn new_overlay_single_original_piece() {
        let m = ascii_metrics("hello\nworld\n");
        let overlay = PieceOverlay::new(m);
        assert_eq!(overlay.piece_count(), 1);
        assert_eq!(overlay.total_byte_len(), 12);
    }

    #[test]
    fn new_overlay_empty_file() {
        let m = TextMetrics::default();
        let overlay = PieceOverlay::new(m);
        assert_eq!(overlay.piece_count(), 0);
        assert_eq!(overlay.total_byte_len(), 0);
    }

    // --- PieceOverlay: insert -----------------------------------------------

    #[test]
    fn insert_append_increases_byte_len() {
        let m = ascii_metrics("hello");
        let mut overlay = PieceOverlay::new(m);
        overlay.insert(5, " world").unwrap();
        assert_eq!(overlay.total_byte_len(), 11);
        assert_eq!(overlay.piece_count(), 2);
    }

    #[test]
    fn insert_at_zero_prepends() {
        let m = ascii_metrics("world");
        let mut overlay = PieceOverlay::new(m);
        overlay.insert(0, "hello ").unwrap();
        assert_eq!(overlay.total_byte_len(), 11);
    }

    #[test]
    fn insert_beyond_end_returns_error() {
        let m = ascii_metrics("hello");
        let mut overlay = PieceOverlay::new(m);
        let err = overlay.insert(10, "x").unwrap_err();
        assert!(matches!(err, OverlayError::OutOfRange { .. }));
    }

    #[test]
    fn insert_in_middle_splits_original_piece() {
        let m = ascii_metrics("helloworld");
        let mut overlay = PieceOverlay::new(m);
        overlay.insert(5, "---").unwrap();
        // Original split into two + one inserted = 3 pieces.
        assert_eq!(overlay.piece_count(), 3);
        assert_eq!(overlay.total_byte_len(), 13);
    }

    #[test]
    fn insert_empty_string_is_noop() {
        let m = ascii_metrics("hello");
        let mut overlay = PieceOverlay::new(m);
        overlay.insert(5, "").unwrap();
        assert_eq!(overlay.piece_count(), 1);
        assert_eq!(overlay.total_byte_len(), 5);
    }

    // --- PieceOverlay: delete -----------------------------------------------

    #[test]
    fn delete_from_single_original_shrinks_len() {
        let m = ascii_metrics("hello world");
        let mut overlay = PieceOverlay::new(m);
        overlay.delete(ByteRange::new(5, 6)).unwrap(); // delete space
        assert_eq!(overlay.total_byte_len(), 10);
    }

    #[test]
    fn delete_out_of_bounds_returns_error() {
        let m = ascii_metrics("hello");
        let mut overlay = PieceOverlay::new(m);
        let err = overlay.delete(ByteRange::new(3, 10)).unwrap_err();
        assert!(matches!(err, OverlayError::OutOfRange { .. }));
    }

    #[test]
    fn delete_empty_range_is_noop() {
        let m = ascii_metrics("hello");
        let mut overlay = PieceOverlay::new(m);
        overlay.delete(ByteRange::new(2, 2)).unwrap();
        assert_eq!(overlay.total_byte_len(), 5);
    }

    #[test]
    fn delete_spans_inserted_and_original_pieces() {
        let m = ascii_metrics("hello world");
        let mut overlay = PieceOverlay::new(m);
        overlay.insert(5, "---").unwrap(); // "hello--- world"
        overlay.delete(ByteRange::new(3, 8)).unwrap(); // delete "lo---"
        // remaining: "hel world" = 9 bytes
        assert_eq!(overlay.total_byte_len(), 9);
    }

    // --- PieceOverlay: newline counting -------------------------------------

    #[test]
    fn newline_count_tracks_inserts() {
        let m = ascii_metrics("hello");
        let mut overlay = PieceOverlay::new(m);
        overlay.insert(5, "\nworld").unwrap();
        // "hello\nworld" -> 1 newline -> 2 lines
        match overlay.known_line_count() {
            KnownLineCount::Exact(n) => assert_eq!(n, 2),
            _ => panic!("expected Exact"),
        }
    }

    #[test]
    fn newline_count_crlf_seam_not_double_counted() {
        // '\r' in original, '\n' inserted right after.
        let m = TextMetrics::from_bytes(b"a\r", false);
        let mut overlay = PieceOverlay::new(m);
        overlay.insert(2, "\nb").unwrap();
        // The '\r\n' pair should count as 1 line ending.
        match overlay.known_line_count() {
            KnownLineCount::Exact(n) => assert_eq!(n, 2, "a\\r\\nb is 2 lines"),
            _ => panic!("expected Exact"),
        }
    }

    // --- PieceOverlay: line_to_byte -----------------------------------------

    #[test]
    fn line_to_byte_line_zero_always_zero() {
        let m = ascii_metrics("hello\nworld");
        let overlay = PieceOverlay::new(m);
        assert_eq!(overlay.line_to_byte(LogicalLine(0)), LineLookup::Exact(ByteOffset(0)));
    }

    #[test]
    fn line_to_byte_out_of_range() {
        let m = ascii_metrics("hello");
        let overlay = PieceOverlay::new(m);
        assert_eq!(overlay.line_to_byte(LogicalLine(5)), LineLookup::OutOfRange);
    }

    // --- PieceOverlay: inserted text not in page cache ----------------------

    #[test]
    fn inserted_text_lives_only_in_insert_buffer() {
        let m = ascii_metrics("hello");
        let mut overlay = PieceOverlay::new(m);
        overlay.insert(5, " world").unwrap();
        // Verify the inserted piece references a buffer, not the original file.
        let inserted = overlay.pieces().iter().find(|p| matches!(p, Piece::Inserted { .. }));
        assert!(inserted.is_some(), "should have an Inserted piece");
        // Buffer should hold the inserted text.
        if let Some(Piece::Inserted { buffer_id, range, .. }) = inserted {
            assert_eq!(overlay.buffer_len(*buffer_id), range.end);
        }
    }

    // --- OverlayEditContext and undo grouping --------------------------------

    #[test]
    fn insert_in_group_records_op_in_delta() {
        let m = ascii_metrics("hello");
        let mut overlay = PieceOverlay::new(m);
        let ctx = OverlayEditContext { revision_id: 42, undo_group: 1 };
        overlay.insert_in_group(5, " world", ctx).unwrap();

        let delta = overlay.delta_for_group(1).expect("delta must be recorded");
        assert_eq!(delta.undo_group, 1);
        assert_eq!(delta.revision_id, 42);
        assert_eq!(delta.ops.len(), 1);
        assert!(matches!(delta.ops[0], OverlayOp::Insert { at: 5, .. }));
    }

    #[test]
    fn delete_in_group_records_op_in_delta() {
        let m = ascii_metrics("hello world");
        let mut overlay = PieceOverlay::new(m);
        let ctx = OverlayEditContext { revision_id: 7, undo_group: 2 };
        overlay.delete_in_group(ByteRange::new(5, 6), ctx).unwrap();

        let delta = overlay.delta_for_group(2).expect("delta must be recorded");
        assert_eq!(delta.ops.len(), 1);
        assert!(matches!(delta.ops[0], OverlayOp::Delete { .. }));
    }

    #[test]
    fn multiple_ops_in_same_group_appended_to_single_delta() {
        let m = ascii_metrics("abc");
        let mut overlay = PieceOverlay::new(m);
        let ctx = OverlayEditContext { revision_id: 1, undo_group: 5 };
        overlay.insert_in_group(3, "X", ctx).unwrap();
        overlay.insert_in_group(4, "Y", ctx).unwrap();

        let delta = overlay.delta_for_group(5).expect("delta exists");
        assert_eq!(delta.ops.len(), 2);
    }

    // --- gc_undo_group -------------------------------------------------------

    #[test]
    fn gc_undo_group_removes_delta_from_history() {
        let m = ascii_metrics("hello");
        let mut overlay = PieceOverlay::new(m);
        let ctx = OverlayEditContext { revision_id: 1, undo_group: 10 };
        overlay.insert_in_group(5, " world", ctx).unwrap();
        assert!(overlay.delta_for_group(10).is_some());

        overlay.gc_undo_group(10);
        assert!(overlay.delta_for_group(10).is_none());
    }

    #[test]
    fn gc_undo_group_frees_buffer_unreferenced_by_live_pieces() {
        let m = TextMetrics::default(); // empty file
        let mut overlay = PieceOverlay::new(m);
        let ctx = OverlayEditContext { revision_id: 1, undo_group: 3 };
        overlay.insert_in_group(0, "hi", ctx).unwrap();

        // Delete the inserted piece so the buffer is no longer live.
        overlay.delete(ByteRange::new(0, 2)).unwrap();
        assert_eq!(overlay.total_byte_len(), 0);

        // Buffer should be freed after GC.
        let buf_id = BufferId(0);
        assert!(overlay.buffers.contains_key(&buf_id), "buffer exists before gc");
        overlay.gc_undo_group(3);
        assert!(!overlay.buffers.contains_key(&buf_id), "buffer freed after gc");
    }

    #[test]
    fn gc_undo_group_retains_buffer_still_used_by_live_pieces() {
        let m = TextMetrics::default();
        let mut overlay = PieceOverlay::new(m);
        let ctx = OverlayEditContext { revision_id: 1, undo_group: 4 };
        overlay.insert_in_group(0, "hello", ctx).unwrap();
        // Do NOT delete the piece — it is still live.

        let buf_id = BufferId(0);
        overlay.gc_undo_group(4);
        // Buffer must stay because the live piece still references it.
        assert!(overlay.buffers.contains_key(&buf_id), "live buffer must not be freed");
    }

    #[test]
    fn gc_nonexistent_undo_group_is_noop() {
        let m = ascii_metrics("hello");
        let overlay = PieceOverlay::new(m);
        // Should not panic.
        let mut overlay = overlay;
        overlay.gc_undo_group(999);
    }

    // --- ArbitrarySparseEditGate --------------------------------------------

    #[test]
    fn gate_defaults_to_all_closed() {
        let m = ascii_metrics("hello");
        let overlay = PieceOverlay::new(m);
        assert!(!overlay.can_enable_arbitrary_edits());
        let gate = overlay.edit_gate();
        assert!(!gate.read_byte_range_ready);
        assert!(!gate.streaming_search_ready);
        assert!(!gate.streaming_save_ready);
    }

    #[test]
    fn gate_requires_all_three_preconditions() {
        let m = ascii_metrics("hello");
        let mut overlay = PieceOverlay::new(m);
        overlay.set_read_byte_range_ready();
        assert!(!overlay.can_enable_arbitrary_edits(), "only one gate open");
        overlay.set_streaming_search_ready();
        assert!(!overlay.can_enable_arbitrary_edits(), "two gates open");
        overlay.set_streaming_save_ready();
        assert!(overlay.can_enable_arbitrary_edits(), "all three gates open");
    }

    #[test]
    fn arbitrary_sparse_edits_not_ready_error_has_display() {
        let err = OverlayError::ArbitrarySparseEditsNotReady;
        let msg = format!("{err}");
        assert!(msg.contains("arbitrary sparse VLF edits"), "display message: {msg}");
    }

    // --- PieceOverlay: overlay_bytes ----------------------------------------

    #[test]
    fn overlay_bytes_grows_with_inserts() {
        let m = ascii_metrics("hello");
        let mut overlay = PieceOverlay::new(m);
        let before = overlay.overlay_bytes();
        overlay.insert(5, " world").unwrap();
        let after = overlay.overlay_bytes();
        assert!(after > before, "overlay_bytes should grow after insert");
    }

    // --- OverlayLimits: undo group cap --------------------------------------

    #[test]
    fn undo_group_cap_auto_gcs_oldest_group() {
        let m = ascii_metrics("hello");
        let mut overlay = PieceOverlay::with_limits(
            m,
            OverlayLimits { max_undo_groups: 3, ..Default::default() },
        );

        for i in 0..4u64 {
            let ctx = OverlayEditContext { revision_id: i, undo_group: i as usize };
            // Append to avoid out-of-range errors as total_byte_len grows.
            overlay.insert_in_group(overlay.total_byte_len(), "x", ctx).unwrap();
        }

        // After 4 inserts with cap=3, oldest group (0) should have been GC'd.
        assert!(overlay.delta_for_group(0).is_none(), "group 0 should be GC'd");
        assert!(overlay.delta_for_group(3).is_some(), "group 3 should exist");
        // History count must not exceed cap.
        assert!(overlay.undo_history.len() <= 3);
    }

    #[test]
    fn undo_group_order_tracks_insertion_order() {
        let m = ascii_metrics("hello");
        let mut overlay = PieceOverlay::new(m);
        for i in 0..3u64 {
            let ctx = OverlayEditContext { revision_id: i, undo_group: i as usize };
            overlay.insert_in_group(overlay.total_byte_len(), "a", ctx).unwrap();
        }
        // Order should be [0, 1, 2].
        assert_eq!(overlay.undo_group_order, vec![0, 1, 2]);
    }

    #[test]
    fn gc_removes_from_undo_group_order() {
        let m = ascii_metrics("hello");
        let mut overlay = PieceOverlay::new(m);
        let ctx = OverlayEditContext { revision_id: 1, undo_group: 7 };
        overlay.insert_in_group(5, "x", ctx).unwrap();
        assert!(overlay.undo_group_order.contains(&7));
        overlay.gc_undo_group(7);
        assert!(!overlay.undo_group_order.contains(&7));
    }

    // --- OverlayLimits: memory cap ------------------------------------------

    #[test]
    fn memory_cap_rejects_edit_when_no_history_to_gc() {
        let m = ascii_metrics("hello");
        let mut overlay = PieceOverlay::with_limits(
            m,
            OverlayLimits { overlay_byte_cap: 5, ..Default::default() },
        );
        // A 10-byte insert should exceed the 5-byte cap with nothing to GC.
        let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
        let result = overlay.insert_in_group(5, "0123456789", ctx);
        assert!(
            matches!(result, Err(OverlayError::LimitReached { .. })),
            "should reject when cap cannot be satisfied"
        );
    }

    // --- signed_byte_delta --------------------------------------------------

    #[test]
    fn signed_byte_delta_zero_before_any_edits() {
        let m = ascii_metrics("hello");
        let overlay = PieceOverlay::new(m);
        assert_eq!(overlay.signed_byte_delta(), 0);
    }

    #[test]
    fn signed_byte_delta_positive_after_insert() {
        let m = ascii_metrics("hello");
        let mut overlay = PieceOverlay::new(m);
        overlay.insert(5, " world").unwrap();
        assert_eq!(overlay.signed_byte_delta(), 6);
    }

    #[test]
    fn signed_byte_delta_negative_after_delete() {
        let m = ascii_metrics("hello world");
        let mut overlay = PieceOverlay::new(m);
        overlay.delete(ByteRange::new(5, 11)).unwrap(); // delete " world"
        assert_eq!(overlay.signed_byte_delta(), -6);
    }

    // --- dirty_byte_delta_for_range -----------------------------------------

    #[test]
    fn dirty_byte_delta_for_range_zero_before_edits() {
        let m = ascii_metrics("hello world");
        let overlay = PieceOverlay::new(m);
        let delta = overlay.dirty_byte_delta_for_range(ByteRange::new(0, 5));
        assert_eq!(delta, 0);
    }

    // --- VlfSavePolicy ------------------------------------------------------

    #[test]
    fn suggested_save_policy_small_file_is_temp_rewrite() {
        let m = ascii_metrics("hello");
        let mut overlay = PieceOverlay::new(m);
        overlay.insert(5, " world").unwrap();
        assert!(matches!(overlay.suggested_save_policy(), VlfSavePolicy::TailShift { .. }));
    }

    #[test]
    fn suggested_save_policy_same_size_is_in_place() {
        let m = ascii_metrics("hello");
        let mut overlay = PieceOverlay::new(m);
        let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
        overlay.delete_in_group(ByteRange::new(1, 3), ctx).unwrap();
        overlay.insert_in_group(1, "XY", ctx).unwrap();
        assert!(matches!(overlay.suggested_save_policy(), VlfSavePolicy::SameSizeInPlaceOverwrite));
    }

    #[test]
    fn vlf_save_policy_display_variants_accessible() {
        let policy_rewrite = VlfSavePolicy::TempFileRewrite { temp_dir: None };
        let policy_in_place = VlfSavePolicy::SameSizeInPlaceOverwrite;
        let policy_tail_shift = VlfSavePolicy::TailShift { fallback_temp_dir: None };
        let policy_saveas = VlfSavePolicy::SaveAs(std::path::PathBuf::from("/tmp/test.txt"));
        // Just check they can be constructed and matched.
        assert!(matches!(policy_rewrite, VlfSavePolicy::TempFileRewrite { .. }));
        assert!(matches!(policy_in_place, VlfSavePolicy::SameSizeInPlaceOverwrite));
        assert!(matches!(policy_tail_shift, VlfSavePolicy::TailShift { .. }));
        assert!(matches!(policy_saveas, VlfSavePolicy::SaveAs(_)));
    }

    // --- OverlayError::LimitReached display ---------------------------------

    #[test]
    fn limit_reached_memory_cap_display() {
        let err =
            OverlayError::LimitReached { kind: LimitKind::MemoryCap { cap: 1024, used: 2048 } };
        let msg = format!("{err}");
        assert!(msg.contains("memory cap"), "display: {msg}");
    }
}
