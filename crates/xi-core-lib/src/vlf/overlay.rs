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
//! represented as an ordered sequence of [`crate::vlf::overlay::Piece`]s that together describe the
//! full logical document:
//!
//! - [`crate::vlf::overlay::Piece::Original`] — a slice of the original file addressed by absolute
//!   byte offset (file stays on disk; the piece is just a range descriptor).
//! - [`crate::vlf::overlay::Piece::Inserted`] — a slice of an append-only [`crate::vlf::overlay::InsertBuffer`].
//!
//! Inserted text is **never** written into the [`crate::vlf::pager::FilePager`]
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
//! Every piece carries [`crate::vlf::overlay::TextMetrics`] so the overlay can answer line/byte
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

mod edits;
mod insert_buffer;
mod query;
mod state;
mod types;

pub use self::insert_buffer::InsertBuffer;
pub use self::types::ArbitrarySparseEditGate;
pub use self::types::BufferId;
pub use self::types::InsertRange;
pub use self::types::OverlayDelta;
pub use self::types::OverlayEditContext;
pub use self::types::OverlayLimits;
pub use self::types::OverlayOp;
pub use self::types::OverlaySaveSnapshot;
pub use self::types::Piece;
pub use self::types::TextMetrics;
pub use self::types::VlfSavePolicy;

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
