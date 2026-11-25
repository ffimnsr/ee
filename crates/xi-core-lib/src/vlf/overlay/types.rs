use super::*;

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
    /// Maximum number of undo groups retained in `PieceOverlay::undo_history`.
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
pub(super) const LARGE_REWRITE_THRESHOLD_BYTES: u64 = 512 * 1024 * 1024;

// ---------------------------------------------------------------------------
// OverlaySaveSnapshot
// ---------------------------------------------------------------------------

/// Immutable snapshot of overlay state needed for background saves.
///
/// Keeps worker-thread ownership bounded to piece descriptors plus cloned
/// inserted buffers; original-file bytes are still read from disk.
#[derive(Debug, Clone)]
pub struct OverlaySaveSnapshot {
    pub(super) pieces: Vec<Piece>,
    pub(super) inserted_buffers: HashMap<BufferId, Vec<u8>>,
    pub(super) total_bytes: u64,
    pub(super) original_file_byte_len: u64,
}

impl OverlaySaveSnapshot {
    /// Ordered piece list for the logical document.
    pub fn pieces(&self) -> &[Piece] {
        &self.pieces
    }

    /// Total logical byte length represented by this snapshot.
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Original file byte length at the time the overlay snapshot was taken.
    pub fn original_file_byte_len(&self) -> u64 {
        self.original_file_byte_len
    }

    /// Retrieve bytes for an inserted piece from the cloned buffer snapshot.
    pub fn inserted_bytes_for_piece<'a>(&'a self, piece: &Piece) -> Option<&'a [u8]> {
        match piece {
            Piece::Inserted { buffer_id, range, .. } => {
                let bytes = self.inserted_buffers.get(buffer_id)?;
                let start = usize::try_from(range.start).ok()?;
                let end = usize::try_from(range.end).ok()?;
                bytes.get(start..end)
            }
            Piece::Original { .. } => None,
        }
    }
}

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
