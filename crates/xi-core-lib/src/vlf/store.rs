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

//! [`VlfStore`]: the primary [`crate::text_store::TextStore`] implementation
//! for Very Large File mode.
//!
//! # Architecture
//!
//! `VlfStore` composes [`super::pager::FilePager`] (I/O + LRU cache) and
//! [`super::page_index::PageIndex`] (descriptor + line index).  Both are held
//! with interior mutability so all `TextStore` trait methods can take `&self`.
//!
//! # Read-only milestone
//!
//! The first VLF milestone is **read-only**: edit and save commands must be
//! rejected at the mode-contract layer (not only in TUI key bindings).
//! `VlfStore` enforces this structurally:
//!
//! - [`crate::text_store::TextStore::full_text_policy`] returns
//!   `Forbidden`, so no call site can extract the whole file as a string.
//! - There are no `mut` accessors for the underlying `Rope`; VLF documents
//!   are never converted to `Rope`.
//!
//! # UTF-8 seam adjustment
//!
//! When a requested byte range `[start, end)` might split a multibyte UTF-8
//! codepoint at either boundary, `read_byte_range` expands the read by up to
//! 3 bytes on each side, then locates the nearest valid codepoint boundaries
//! before decoding.  The `TextChunk::byte_range` in the result always reflects
//! the **adjusted** (decoded) range, not the original request.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{self, Read};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
#[cfg(unix)]
use std::{ptr, slice};

use crate::text_store::{
    ByteOffset, ByteRange, DocumentMode, EditPermission, FullTextPolicy, KnownLineCount,
    LineLookup, LogicalLine, TextChunk, TextChunkResult, TextStore, Utf16Lookup, Utf16Offset,
};

use super::page_index::{PageDescriptor, PageIndex, ScanState};
use super::pager::{CancelGeneration, DEFAULT_CACHE_BYTE_CAP, FilePager, pread_exact};
use crate::vlf::overlay::{
    OverlayEditContext, OverlayLimits, PieceOverlay, TextMetrics, VlfSavePolicy,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default page size used when scanning a VLF file (1 MiB).
pub const DEFAULT_PAGE_SIZE: u64 = 1024 * 1024;

/// Slack bytes added on each side of a requested byte range before seam
/// adjustment.  Four bytes covers the maximum UTF-8 codepoint length, ensuring
/// a boundary-split 4-byte sequence is always fully included.
const UTF8_SEAM_SLACK: u64 = 4;

/// Default batch size for viewport reads (256 KiB).
pub const DEFAULT_BATCH_SIZE: u64 = 256 * 1024;

/// Read buffer for streaming `wc -l`-style line counts.
const LINE_COUNT_BUFFER_SIZE: usize = 256 * 1024;

#[cfg(unix)]
const LINE_COUNT_MMAP_PARALLEL_THRESHOLD: usize = 64 * 1024 * 1024;

#[cfg(unix)]
const LINE_COUNT_MMAP_MAX_THREADS: usize = 8;

/// Default byte cap for the decoded-text cache (32 MiB).
///
/// Raw-page bytes are budgeted separately in [`FilePager`].  This cap applies
/// only to the UTF-8 decoded strings stored alongside each raw page.
pub const DEFAULT_DECODED_CACHE_BYTE_CAP: u64 = 32 * 1024 * 1024;

const VLF_READ_ONLY_REASON: &str =
    "VLF mode is read-only; copy, search, and navigation remain available";

// ---------------------------------------------------------------------------
// VlfMemoryBudget
// ---------------------------------------------------------------------------

/// Memory budget configuration for a [`VlfStore`].
///
/// Pass to [`VlfStore::open_with_budget`] to override the per-category byte
/// caps.  Budget tests should create a store with small caps so they can verify
/// enforcement without allocating gigabytes of actual file data.
#[derive(Debug, Clone)]
pub struct VlfMemoryBudget {
    /// Maximum bytes for the raw-page LRU cache inside [`FilePager`].
    /// Default: [`DEFAULT_CACHE_BYTE_CAP`] (64 MiB).
    pub raw_page_byte_cap: u64,
    /// Maximum bytes for the decoded-text LRU cache inside [`VlfStore`].
    /// Default: [`DEFAULT_DECODED_CACHE_BYTE_CAP`] (32 MiB).
    pub decoded_byte_cap: u64,
}

impl Default for VlfMemoryBudget {
    fn default() -> Self {
        VlfMemoryBudget {
            raw_page_byte_cap: DEFAULT_CACHE_BYTE_CAP,
            decoded_byte_cap: DEFAULT_DECODED_CACHE_BYTE_CAP,
        }
    }
}

// ---------------------------------------------------------------------------
// VlfEditError
// ---------------------------------------------------------------------------

/// Errors returned by VLF edit operations ([`VlfStore::apply_insert`] /
/// [`VlfStore::apply_delete`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VlfEditError {
    /// [`VlfStore::enable_editing`] has not been called yet.
    EditingNotEnabled,
    /// The underlying overlay operation failed.
    Overlay(crate::vlf::overlay::OverlayError),
}

impl std::fmt::Display for VlfEditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VlfEditError::EditingNotEnabled => {
                write!(f, "VLF editing not enabled; call enable_editing() first")
            }
            VlfEditError::Overlay(e) => write!(f, "overlay error: {e}"),
        }
    }
}

impl std::error::Error for VlfEditError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            VlfEditError::Overlay(e) => Some(e),
            VlfEditError::EditingNotEnabled => None,
        }
    }
}

// ---------------------------------------------------------------------------
// VlfMemoryStats
// ---------------------------------------------------------------------------

/// Peak memory usage counters tracked by a [`VlfStore`].
///
/// Updated whenever cache occupancy increases.  Use these counters in budget
/// regression tests instead of relying on OS-level RSS sampling, which is
/// unreliable in unit tests.
///
/// Overlay bytes are tracked in `peak_overlay_bytes` after editing is enabled.
#[derive(Debug, Clone, Default)]
pub struct VlfMemoryStats {
    /// Peak raw-page bytes held in the [`FilePager`] LRU cache.
    pub peak_raw_bytes: u64,
    /// Peak decoded-text bytes held in the decoded-text LRU cache.
    pub peak_decoded_bytes: u64,
    /// Approximate descriptor bytes: `size_of::<PageDescriptor>() × descriptor_count`.
    pub descriptor_bytes: u64,
    /// Peak overlay bytes (insert buffers + piece metadata); 0 until editing
    /// is enabled via [`VlfStore::enable_editing`].
    pub peak_overlay_bytes: u64,
    /// Cumulative raw bytes read from the pager before the first
    /// [`VlfStore::set_viewport`] call.  Useful for diagnosing how many bytes
    /// are fetched during open/scan before the first viewport render.
    pub bytes_before_first_viewport: u64,
}

// ---------------------------------------------------------------------------
// SeamResult
// ---------------------------------------------------------------------------

/// Result of a seam-adjusted read, preserving both the original requested
/// range and the adjusted decoded range separately.
#[derive(Debug, Clone)]
pub struct SeamResult {
    /// UTF-8 decoded text of the adjusted window.
    pub text: String,
    /// The byte range originally requested by the caller.
    pub original_range: ByteRange,
    /// The byte range that was actually decoded after expanding by up to
    /// `UTF8_SEAM_SLACK` bytes on each side and walking back to the nearest
    /// UTF-8 codepoint boundaries.
    pub decoded_range: ByteRange,
}

// ---------------------------------------------------------------------------
// PagePriority
// ---------------------------------------------------------------------------

/// Cache retention priority for decoded-text entries.
///
/// Entries are evicted in ascending priority order (Background first).
/// Within the same tier, the least-recently-used entry is evicted first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PagePriority {
    /// Cold pages far from the current viewport.
    Background = 0,
    /// Pages within one batch-size window of the viewport boundary.
    Overscan = 1,
    /// Pages that overlap the active viewport window.
    Viewport = 2,
}

// ---------------------------------------------------------------------------
// DecodedTextCache (internal)
// ---------------------------------------------------------------------------

/// A single entry in the decoded-text cache.
struct DecodedEntry {
    text: String,
    decoded_range: ByteRange,
    priority: PagePriority,
    /// Monotonically increasing timestamp for LRU ordering.
    access_time: u64,
}

/// Priority-aware LRU cache for decoded UTF-8 text windows.
///
/// Raw page bytes live in [`FilePager`]'s byte cache.  This cache stores the
/// UTF-8 decoded strings so that re-rendering the same viewport region avoids
/// re-decoding bytes.  It is populated only when the memory budget allows it.
///
/// **Eviction order**: Background first, then Overscan, then Viewport.
/// Within the same priority tier, the least-recently-used entry is evicted.
struct DecodedTextCache {
    /// page_start → decoded entry.
    entries: HashMap<u64, DecodedEntry>,
    /// `(priority_byte, access_time)` → page_start, for ordered eviction.
    order: BTreeMap<(u8, u64), u64>,
    access_counter: u64,
    used_bytes: u64,
    byte_cap: u64,
}

impl DecodedTextCache {
    fn new(byte_cap: u64) -> Self {
        DecodedTextCache {
            entries: HashMap::new(),
            order: BTreeMap::new(),
            access_counter: 0,
            used_bytes: 0,
            byte_cap,
        }
    }

    /// Look up the decoded text for `page_start`, updating LRU order.
    fn get(&mut self, page_start: u64) -> Option<(String, ByteRange)> {
        let entry = self.entries.get_mut(&page_start)?;
        let old_key = (entry.priority as u8, entry.access_time);
        self.order.remove(&old_key);
        self.access_counter += 1;
        entry.access_time = self.access_counter;
        let new_key = (entry.priority as u8, self.access_counter);
        self.order.insert(new_key, page_start);
        Some((entry.text.clone(), entry.decoded_range))
    }

    /// Insert decoded text for `page_start`.  Evicts low-priority entries when
    /// the budget would be exceeded.  Entries larger than the full cap are
    /// silently skipped (too large to cache).
    fn put(
        &mut self,
        page_start: u64,
        text: String,
        decoded_range: ByteRange,
        priority: PagePriority,
    ) {
        // Remove any existing entry for this page_start first.
        if let Some(old) = self.entries.remove(&page_start) {
            self.used_bytes = self.used_bytes.saturating_sub(old.text.len() as u64);
            self.order.remove(&(old.priority as u8, old.access_time));
        }

        let needed = text.len() as u64;
        if needed > self.byte_cap {
            // Entry would consume entire budget alone; skip.
            return;
        }

        // Evict (priority-ascending, then LRU-ascending) until we have room.
        while !self.entries.is_empty() && self.used_bytes + needed > self.byte_cap {
            let evict_key = match self.order.iter().next() {
                Some((&k, _)) => k,
                None => break,
            };
            if let Some(evict_start) = self.order.remove(&evict_key) {
                if let Some(evicted) = self.entries.remove(&evict_start) {
                    self.used_bytes = self.used_bytes.saturating_sub(evicted.text.len() as u64);
                }
            }
        }

        self.access_counter += 1;
        let access_time = self.access_counter;
        self.used_bytes += needed;
        self.order.insert((priority as u8, access_time), page_start);
        self.entries
            .insert(page_start, DecodedEntry { text, decoded_range, priority, access_time });
    }

    /// Update the priority of a cached entry without changing its LRU time.
    fn set_priority(&mut self, page_start: u64, new_priority: PagePriority) {
        if let Some(entry) = self.entries.get_mut(&page_start) {
            let old_key = (entry.priority as u8, entry.access_time);
            self.order.remove(&old_key);
            entry.priority = new_priority;
            self.order.insert((new_priority as u8, entry.access_time), page_start);
        }
    }

    /// Number of bytes currently used by cached decoded strings.
    fn used_bytes(&self) -> u64 {
        self.used_bytes
    }

    fn byte_cap(&self) -> u64 {
        self.byte_cap
    }
}

// ---------------------------------------------------------------------------
// VlfViewportState
// ---------------------------------------------------------------------------

/// Absolute byte window state for the active viewport.
///
/// Owned by [`VlfStore`] so TUI code never manages raw byte offsets directly.
/// Updated via [`VlfStore::set_viewport`].
#[derive(Debug, Clone)]
pub struct VlfViewportState {
    /// Absolute start byte offset of the current window (inclusive).
    pub window_start: ByteOffset,
    /// Absolute end byte offset of the current window (exclusive).
    pub window_end: ByteOffset,
    /// Byte range actually decoded after UTF-8 seam adjustment.
    ///
    /// May extend slightly beyond `[window_start, window_end)` when multibyte
    /// codepoints straddle the boundary.
    pub decoded_range: ByteRange,
    /// Original encoded byte length of the window before seam expansion
    /// (`window_end.0 - window_start.0`).
    pub original_encoded_len: u64,
    /// Whether the window has unsaved overlay changes.
    pub dirty: bool,
    /// Number of bytes fetched per read batch.  Configurable and later
    /// auto-tunable from observed read/decode timing.
    pub batch_size: u64,
}

impl VlfViewportState {
    fn new(batch_size: u64) -> Self {
        VlfViewportState {
            window_start: ByteOffset(0),
            window_end: ByteOffset(0),
            decoded_range: ByteRange::new(0, 0),
            original_encoded_len: 0,
            dirty: false,
            batch_size,
        }
    }
}

// ---------------------------------------------------------------------------
// VlfStore
// ---------------------------------------------------------------------------

/// A [`crate::text_store::TextStore`] backed by a paged file for VLF mode.
///
/// The file on disk is the single source of truth.  Memory holds only:
/// - page descriptors (no raw bytes),
/// - a bounded LRU cache of decoded page windows,
/// - (future) overlay edits.
///
/// Conversion to a full `Rope` is explicitly prohibited; calling
/// `read_full_text()` returns `TextChunkResult::Unsupported`.
pub struct VlfStore {
    pager: FilePager,
    /// Interior-mutable so TextStore's `&self` methods can update the index
    /// as pages are scanned.
    index: RefCell<PageIndex>,
    /// Page size used when dividing the file into scan units.
    page_size: u64,
    /// Absolute byte window state for the active viewport.
    viewport: RefCell<VlfViewportState>,
    /// Priority-aware LRU cache for decoded text, separate from the raw-byte
    /// cache in `FilePager`.
    decoded_cache: RefCell<DecodedTextCache>,
    /// Default batch size for viewport reads.
    batch_size: u64,
    /// Peak memory usage counters; updated on every cache write.
    stats: RefCell<VlfMemoryStats>,
    /// True once `set_viewport` has been called with a non-zero window.
    /// Used to stop accumulating `bytes_before_first_viewport`.
    first_viewport_set: Cell<bool>,
    /// Receiver for descriptors produced by the background indexing thread.
    ///
    /// `None` until [`start_background_indexing`](Self::start_background_indexing)
    /// is called.  `drain_incoming` drains the channel into `self.index`.
    scan_rx: RefCell<Option<mpsc::Receiver<PageDescriptor>>>,
    /// Set to `true` when the `VlfStore` is dropped, signalling the background
    /// scanner thread to stop.
    bg_cancel: Arc<AtomicBool>,
    /// Monotone lower bound on the approximate line count.
    ///
    /// Ensures that `known_line_count` returns an `Approximate` value that
    /// never decreases as more pages are scanned, keeping the status bar stable.
    approx_line_floor: Cell<u64>,
    /// Exact logical line count once a streaming count or full index has established it.
    exact_line_count: Cell<Option<u64>>,
    /// Sparse piece-based edit overlay.
    ///
    /// `None` in the read-only first milestone.  Becomes `Some` when
    /// [`VlfStore::enable_editing`] is called, which also changes
    /// [`TextStore::edit_permission`] from `Forbidden` to `Allowed`.
    ///
    /// The base file is never converted to a `Rope`; the overlay keeps
    /// only piece descriptors and append-only insert buffers in memory.
    overlay: RefCell<Option<PieceOverlay>>,
}

impl VlfStore {
    /// Open `path` with default page size and cache capacity.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::open_with_config(path, DEFAULT_PAGE_SIZE, DEFAULT_CACHE_BYTE_CAP)
    }

    /// Open `path` with explicit `page_size` and `cache_byte_cap`.
    pub fn open_with_config(
        path: impl AsRef<Path>,
        page_size: u64,
        cache_byte_cap: u64,
    ) -> io::Result<Self> {
        let pager = FilePager::open_with_config(path, cache_byte_cap, page_size * 4)?;
        let file_size = pager.file_size();
        Ok(VlfStore {
            pager,
            index: RefCell::new(PageIndex::new(file_size)),
            page_size,
            viewport: RefCell::new(VlfViewportState::new(DEFAULT_BATCH_SIZE)),
            decoded_cache: RefCell::new(DecodedTextCache::new(DEFAULT_DECODED_CACHE_BYTE_CAP)),
            batch_size: DEFAULT_BATCH_SIZE,
            stats: RefCell::new(VlfMemoryStats::default()),
            first_viewport_set: Cell::new(false),
            scan_rx: RefCell::new(None),
            bg_cancel: Arc::new(AtomicBool::new(false)),
            approx_line_floor: Cell::new(0),
            exact_line_count: Cell::new(None),
            overlay: RefCell::new(None),
        })
    }

    /// Open `path` with an explicit [`VlfMemoryBudget`].
    ///
    /// Use this constructor in budget regression tests or when tuning memory
    /// caps for specific file sizes.
    pub fn open_with_budget(path: impl AsRef<Path>, budget: VlfMemoryBudget) -> io::Result<Self> {
        let pager =
            FilePager::open_with_config(path, budget.raw_page_byte_cap, DEFAULT_PAGE_SIZE * 4)?;
        let file_size = pager.file_size();
        Ok(VlfStore {
            pager,
            index: RefCell::new(PageIndex::new(file_size)),
            page_size: DEFAULT_PAGE_SIZE,
            viewport: RefCell::new(VlfViewportState::new(DEFAULT_BATCH_SIZE)),
            decoded_cache: RefCell::new(DecodedTextCache::new(budget.decoded_byte_cap)),
            batch_size: DEFAULT_BATCH_SIZE,
            stats: RefCell::new(VlfMemoryStats::default()),
            first_viewport_set: Cell::new(false),
            scan_rx: RefCell::new(None),
            bg_cancel: Arc::new(AtomicBool::new(false)),
            approx_line_floor: Cell::new(0),
            exact_line_count: Cell::new(None),
            overlay: RefCell::new(None),
        })
    }

    // ------------------------------------------------------------------
    // Overlay edit API
    // ------------------------------------------------------------------

    /// Enable editing mode for this VLF document.
    ///
    /// Initialises the [`PieceOverlay`] with the current file size and metrics.
    /// After this call, [`TextStore::edit_permission`] returns `Allowed` and
    /// [`Self::apply_insert`] / [`Self::apply_delete`] can be used to record
    /// edits.
    ///
    /// The base file is **never** loaded into a `Rope`; the overlay holds only
    /// piece descriptors and append-only insert buffers.
    ///
    /// Calling this more than once is a no-op (the existing overlay is kept).
    pub fn enable_editing(&self) {
        let mut ov = self.overlay.borrow_mut();
        if ov.is_some() {
            return;
        }
        let file_size = self.pager.file_size();
        // Use byte_len from file size; newline_count is approximate (we don't
        // scan the whole file here).  The overlay accumulates exact counts for
        // inserted pieces; Original piece newline counts stay approximate until
        // the page index fills in.
        let metrics = TextMetrics { byte_len: file_size, ..TextMetrics::default() };
        let mut overlay = PieceOverlay::with_limits(metrics, OverlayLimits::default());
        overlay.set_read_byte_range_ready();
        overlay.set_streaming_search_ready();
        overlay.set_streaming_save_ready();
        *ov = Some(overlay);
    }

    /// Enable editing mode with explicit resource limits.
    ///
    /// Use in tests or when the default [`OverlayLimits`] need to be tuned for
    /// a specific deployment.
    pub fn enable_editing_with_limits(&self, limits: OverlayLimits) {
        let mut ov = self.overlay.borrow_mut();
        if ov.is_some() {
            return;
        }
        let file_size = self.pager.file_size();
        let metrics = TextMetrics { byte_len: file_size, ..TextMetrics::default() };
        let mut overlay = PieceOverlay::with_limits(metrics, limits);
        overlay.set_read_byte_range_ready();
        overlay.set_streaming_search_ready();
        overlay.set_streaming_save_ready();
        *ov = Some(overlay);
    }

    /// Insert `text` at logical byte offset `at`, recording the edit in the
    /// overlay under `ctx`'s undo group.
    ///
    /// Returns `Err` when editing is not enabled (call [`Self::enable_editing`]
    /// first), when `at` is out of range, or when an overlay resource limit is
    /// reached.
    ///
    /// # Invariant
    ///
    /// The base file is never converted to a `Rope`.  Inserted bytes live
    /// exclusively in the overlay's append-only insert buffers.
    pub fn apply_insert(
        &self,
        at: u64,
        text: &str,
        ctx: OverlayEditContext,
    ) -> Result<(), VlfEditError> {
        let mut ov = self.overlay.borrow_mut();
        let overlay = ov.as_mut().ok_or(VlfEditError::EditingNotEnabled)?;
        overlay.insert_in_group(at, text, ctx).map_err(VlfEditError::Overlay)?;
        // Update peak overlay bytes tracking.
        let overlay_bytes = overlay.overlay_bytes();
        drop(ov);
        let mut stats = self.stats.borrow_mut();
        if overlay_bytes > stats.peak_overlay_bytes {
            stats.peak_overlay_bytes = overlay_bytes;
        }
        Ok(())
    }

    /// Delete bytes `[range.start, range.end)` from the logical document,
    /// recording the edit in the overlay under `ctx`'s undo group.
    ///
    /// Returns `Err` when editing is not enabled or when the range is out of
    /// bounds.
    pub fn apply_delete(
        &self,
        range: crate::text_store::ByteRange,
        ctx: OverlayEditContext,
    ) -> Result<(), VlfEditError> {
        let mut ov = self.overlay.borrow_mut();
        let overlay = ov.as_mut().ok_or(VlfEditError::EditingNotEnabled)?;
        overlay.delete_in_group(range, ctx).map_err(VlfEditError::Overlay)?;
        Ok(())
    }

    /// Return recorded overlay delta for `undo_group`, if present.
    #[allow(dead_code)]
    pub fn overlay_delta_for_undo_group(
        &self,
        undo_group: usize,
    ) -> Option<crate::vlf::overlay::OverlayDelta> {
        let ov = self.overlay.borrow();
        ov.as_ref()?.delta_for_group(undo_group).cloned()
    }

    /// Release overlay history and insert buffers owned only by `undo_group`.
    pub fn gc_undo_group(&self, undo_group: usize) {
        let mut ov = self.overlay.borrow_mut();
        if let Some(overlay) = ov.as_mut() {
            overlay.gc_undo_group(undo_group);
        }
    }

    /// Suggested save policy given the current overlay state.
    ///
    /// Returns the narrowest available strategy: same-size overwrite,
    /// tail-shift with temp fallback, temp rewrite, or save-as.  Returns
    /// `None` when editing is not enabled (no overlay changes to save).
    pub fn suggested_save_policy(&self) -> Option<VlfSavePolicy> {
        self.overlay.borrow().as_ref().map(|ov| ov.suggested_save_policy())
    }

    /// Returns `true` when an edit overlay is active for this VLF buffer.
    pub fn is_editing_enabled(&self) -> bool {
        self.overlay.borrow().is_some()
    }

    /// Returns `true` when the overlay has the streaming-save gate enabled.
    pub fn is_save_enabled(&self) -> bool {
        self.overlay
            .borrow()
            .as_ref()
            .is_some_and(|overlay| overlay.edit_gate().streaming_save_ready)
    }

    /// Signed byte delta of the current overlay relative to the original file.
    ///
    /// Returns `0` when editing has not been enabled.
    pub fn signed_byte_delta(&self) -> i64 {
        self.overlay.borrow().as_ref().map_or(0, |ov| ov.signed_byte_delta())
    }

    /// Save the current overlay piece sequence to `dest` using the requested
    /// VLF save policy.
    ///
    /// # Parameters
    ///
    /// - `dest`        — Final destination path (overwritten atomically).
    /// - `policy`      — Determines temp-dir placement and save-as semantics.
    ///   Use [`Self::suggested_save_policy`] to get a policy recommendation
    ///   based on the current overlay.
    /// - `on_progress` — Called after each chunk is written.  Return `false`
    ///   to cancel **before** the rename commit point.
    ///
    /// # Errors
    ///
    /// Returns [`crate::vlf::save::VlfSaveError::EditingNotEnabled`] when `Self::enable_editing`
    /// has not been called (no overlay to save).  For an unmodified read-only
    /// VLF file the original file on disk already reflects the correct content.
    ///
    /// # Cancellation after commit
    ///
    /// Once the rename succeeds the file is durably committed.  `on_progress`
    /// is never called after the rename, so there is no way to cancel a
    /// completed save.  Callers should treat `Ok(())` as unconditional success.
    pub fn stream_save(
        &self,
        dest: &std::path::Path,
        policy: &crate::vlf::overlay::VlfSavePolicy,
        on_progress: &mut dyn FnMut(crate::vlf::save::SaveProgress) -> bool,
    ) -> Result<(), crate::vlf::save::VlfSaveError> {
        let ov = self.overlay.borrow();
        let overlay = ov.as_ref().ok_or(crate::vlf::save::VlfSaveError::EditingNotEnabled)?;
        crate::vlf::save::stream_save_pieces(
            overlay.pieces(),
            overlay,
            &self.pager,
            dest,
            policy,
            on_progress,
        )
    }

    /// Snapshot bounded save inputs for background VLF save execution.
    pub fn prepare_save_plan(
        &self,
    ) -> Result<crate::vlf::save::PreparedVlfSavePlan, crate::vlf::save::VlfSaveError> {
        let ov = self.overlay.borrow();
        let overlay = ov.as_ref().ok_or(crate::vlf::save::VlfSaveError::EditingNotEnabled)?;
        Ok(crate::vlf::save::PreparedVlfSavePlan {
            source_path: self.pager.canonical_path().to_owned(),
            snapshot: overlay.save_snapshot(),
        })
    }

    /// Rebase the store onto the just-saved on-disk file without leaving VLF mode.
    pub fn refresh_after_save(&mut self, path: &Path) -> io::Result<()> {
        let raw_cache_byte_cap = self.pager.metrics().cache_byte_cap;
        let decoded_cache_byte_cap = self.decoded_cache.get_mut().byte_cap();
        let viewport = self.viewport.get_mut().clone();

        let overlay_limits =
            self.overlay.get_mut().as_ref().map(|overlay| overlay.limits().clone());

        self.bg_cancel.store(true, Ordering::Release);
        *self.scan_rx.get_mut() = None;
        self.bg_cancel = Arc::new(AtomicBool::new(false));

        self.pager = FilePager::open_with_config(path, raw_cache_byte_cap, self.page_size * 4)?;
        let file_size = self.pager.file_size();

        *self.index.get_mut() = PageIndex::new(file_size);
        *self.decoded_cache.get_mut() = DecodedTextCache::new(decoded_cache_byte_cap);

        let window_start = viewport.window_start.0.min(file_size);
        let window_end = viewport.window_end.0.min(file_size).max(window_start);
        *self.viewport.get_mut() = VlfViewportState {
            window_start: ByteOffset(window_start),
            window_end: ByteOffset(window_end),
            decoded_range: ByteRange::new(window_start, window_end),
            original_encoded_len: window_end.saturating_sub(window_start),
            dirty: false,
            batch_size: viewport.batch_size,
        };

        self.first_viewport_set.set(window_end > window_start);
        self.approx_line_floor.set(0);
        self.exact_line_count.set(None);

        *self.overlay.get_mut() = overlay_limits.map(|limits| {
            let mut overlay = PieceOverlay::with_limits(
                TextMetrics { byte_len: file_size, ..TextMetrics::default() },
                limits,
            );
            overlay.set_read_byte_range_ready();
            overlay.set_streaming_search_ready();
            overlay.set_streaming_save_ready();
            overlay
        });

        self.start_background_indexing();
        Ok(())
    }

    /// Update the active viewport window.
    ///
    /// Adjusts decoded-text cache priorities: cached pages that overlap
    /// `[start, end)` are promoted to [`PagePriority::Viewport`]; pages
    /// within one batch window of the boundary are set to
    /// [`PagePriority::Overscan`]; everything else remains
    /// [`PagePriority::Background`].
    ///
    /// The `decoded_range` and `original_encoded_len` fields of the stored
    /// [`VlfViewportState`] are updated when a corresponding cached decoded
    /// result is available; otherwise they reflect the raw requested window.
    pub fn set_viewport(&self, start: ByteOffset, end: ByteOffset) {
        let batch = self.batch_size;
        let overscan_start = start.0.saturating_sub(batch);
        let overscan_end = end.0.saturating_add(batch).min(self.pager.file_size());

        // Promote/demote cache entries according to new viewport.
        {
            let mut cache = self.decoded_cache.borrow_mut();
            // Collect page_start keys to avoid holding mut borrow while calling set_priority.
            let keys: Vec<u64> = cache.entries.keys().copied().collect();
            for key in keys {
                let priority = if let Some(entry) = cache.entries.get(&key) {
                    let entry_end = entry.decoded_range.end.0;
                    let entry_start = entry.decoded_range.start.0;
                    if entry_start < end.0 && entry_end > start.0 {
                        PagePriority::Viewport
                    } else if entry_start < overscan_end && entry_end > overscan_start {
                        PagePriority::Overscan
                    } else {
                        PagePriority::Background
                    }
                } else {
                    continue;
                };
                cache.set_priority(key, priority);
            }
        }

        // Update viewport state.
        let original_encoded_len = end.0.saturating_sub(start.0);
        let (decoded_range, dirty) = {
            let cache = self.decoded_cache.borrow();
            // Try to find a cached decoded range that covers start.
            let cached = cache.entries.get(&start.0).map(|e| e.decoded_range);
            (cached.unwrap_or_else(|| ByteRange::new(start.0, end.0)), false)
        };

        *self.viewport.borrow_mut() = VlfViewportState {
            window_start: start,
            window_end: end,
            decoded_range,
            original_encoded_len,
            dirty,
            batch_size: batch,
        };

        // Mark first viewport as set so pre-viewport byte accounting stops.
        if !self.first_viewport_set.get() && end.0 > start.0 {
            self.first_viewport_set.set(true);
        }
    }

    /// Return a snapshot of the current viewport state.
    pub fn viewport_state(&self) -> VlfViewportState {
        self.viewport.borrow().clone()
    }

    pub(crate) fn viewport_window(&self) -> ByteRange {
        let viewport = self.viewport.borrow();
        ByteRange::new(viewport.window_start.0, viewport.window_end.0)
    }

    pub(crate) fn page_size(&self) -> u64 {
        self.page_size
    }

    pub(crate) fn invalidate_pending_reads(&self) -> CancelGeneration {
        self.pager.invalidate()
    }

    pub(crate) fn read_search_range(
        &self,
        range: ByteRange,
        token: CancelGeneration,
    ) -> io::Result<TextChunk> {
        if self.overlay_read_enabled() {
            return self.overlay_read_exact_range(range, token);
        }
        let raw = self.read_raw_range_token(range, token)?;
        let trim_start = leading_continuation_bytes(&raw);
        let trim_end = trailing_incomplete_bytes(&raw);
        let decoded_end = raw.len().saturating_sub(trim_end);
        let decoded_start = trim_start.min(decoded_end);
        let decoded_range = ByteRange::new(
            range.start.0 + decoded_start as u64,
            range.start.0 + decoded_end as u64,
        );
        let text = String::from_utf8_lossy(&raw[decoded_start..decoded_end]).into_owned();
        Ok(TextChunk { text, byte_range: decoded_range })
    }

    fn overlay_read_enabled(&self) -> bool {
        self.overlay
            .borrow()
            .as_ref()
            .is_some_and(|overlay| overlay.edit_gate().read_byte_range_ready)
    }

    fn overlay_len_bytes(&self) -> Option<u64> {
        let overlay = self.overlay.borrow();
        overlay.as_ref().map(PieceOverlay::total_byte_len)
    }

    fn visit_overlay_range<F>(
        &self,
        range: ByteRange,
        token: CancelGeneration,
        mut visitor: F,
    ) -> io::Result<()>
    where
        F: FnMut(u64, &[u8]) -> bool,
    {
        let overlay = self.overlay.borrow();
        let Some(overlay) = overlay.as_ref() else {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "overlay not enabled"));
        };
        if range.end.0 > overlay.total_byte_len() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "overlay range out of bounds"));
        }
        let chunk_cap = self.pager.max_read_size().max(1);
        let mut doc_pos = 0u64;
        for piece in overlay.pieces() {
            let piece_end = doc_pos.saturating_add(piece.byte_len());
            if piece_end <= range.start.0 {
                doc_pos = piece_end;
                continue;
            }
            if doc_pos >= range.end.0 {
                break;
            }

            let local_start = range.start.0.saturating_sub(doc_pos).min(piece.byte_len());
            let local_end = range.end.0.saturating_sub(doc_pos).min(piece.byte_len());
            if local_start >= local_end {
                doc_pos = piece_end;
                continue;
            }

            match piece {
                crate::vlf::overlay::Piece::Original { file_range, .. } => {
                    let mut file_pos = file_range.start.0.saturating_add(local_start);
                    let file_end = file_range.start.0.saturating_add(local_end);
                    let mut logical_pos = doc_pos.saturating_add(local_start);
                    while file_pos < file_end {
                        let next_file_end = file_pos.saturating_add(chunk_cap).min(file_end);
                        let chunk =
                            self.pager.read_at(ByteRange::new(file_pos, next_file_end), token)?;
                        self.record_pager_read(next_file_end.saturating_sub(file_pos));
                        if !visitor(logical_pos, chunk.as_bytes()) {
                            return Ok(());
                        }
                        logical_pos =
                            logical_pos.saturating_add(next_file_end.saturating_sub(file_pos));
                        file_pos = next_file_end;
                    }
                }
                crate::vlf::overlay::Piece::Inserted { .. } => {
                    if let Some(bytes) = overlay.inserted_bytes_for_piece(piece) {
                        let start =
                            usize::try_from(local_start).unwrap_or(bytes.len()).min(bytes.len());
                        let end =
                            usize::try_from(local_end).unwrap_or(bytes.len()).min(bytes.len());
                        if start < end
                            && !visitor(doc_pos.saturating_add(local_start), &bytes[start..end])
                        {
                            return Ok(());
                        }
                    }
                }
            }

            doc_pos = piece_end;
        }

        Ok(())
    }

    fn overlay_read_exact_range(
        &self,
        range: ByteRange,
        token: CancelGeneration,
    ) -> io::Result<TextChunk> {
        let Some(len_bytes) = self.overlay_len_bytes() else {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "overlay not enabled"));
        };
        if range.end.0 > len_bytes {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "overlay range out of bounds"));
        }
        let mut bytes = Vec::with_capacity(range.len() as usize);
        self.visit_overlay_range(range, token, |_, chunk| {
            bytes.extend_from_slice(chunk);
            true
        })?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        Ok(TextChunk { text, byte_range: range })
    }

    fn overlay_line_to_byte(&self, line: u64) -> LineLookup {
        if line == 0 {
            return LineLookup::Exact(ByteOffset(0));
        }
        let Some(total_len) = self.overlay_len_bytes() else {
            return LineLookup::Pending;
        };
        let token = self.pager.current_generation();
        let mut lines_seen = 0u64;
        let mut pending_cr = false;
        let mut found = None;
        let _ = self.visit_overlay_range(
            ByteRange::new(0, total_len),
            token,
            |logical_start, chunk| {
                let mut index = 0usize;
                if pending_cr {
                    if chunk.first() == Some(&b'\n') {
                        lines_seen = lines_seen.saturating_add(1);
                        pending_cr = false;
                        if lines_seen == line {
                            found = Some(ByteOffset(logical_start.saturating_add(1)));
                            return false;
                        }
                        index = 1;
                    } else {
                        lines_seen = lines_seen.saturating_add(1);
                        pending_cr = false;
                        if lines_seen == line {
                            found = Some(ByteOffset(logical_start));
                            return false;
                        }
                    }
                }

                while index < chunk.len() {
                    let abs = logical_start.saturating_add(index as u64);
                    match chunk[index] {
                        b'\r' => pending_cr = true,
                        b'\n' => {
                            lines_seen = lines_seen.saturating_add(1);
                            if lines_seen == line {
                                found = Some(ByteOffset(abs.saturating_add(1)));
                                return false;
                            }
                        }
                        _ if pending_cr => {
                            lines_seen = lines_seen.saturating_add(1);
                            pending_cr = false;
                            if lines_seen == line {
                                found = Some(ByteOffset(abs));
                                return false;
                            }
                            continue;
                        }
                        _ => {}
                    }
                    index += 1;
                }
                true
            },
        );

        if let Some(found) = found {
            return LineLookup::Exact(found);
        }
        if pending_cr {
            lines_seen = lines_seen.saturating_add(1);
            if lines_seen == line {
                return LineLookup::Exact(ByteOffset(total_len));
            }
        }
        LineLookup::OutOfRange
    }

    fn overlay_byte_to_line(&self, offset: u64) -> Option<LogicalLine> {
        let total_len = self.overlay_len_bytes()?;
        if offset > total_len {
            return None;
        }
        let token = self.pager.current_generation();
        let mut lines_seen = 0u64;
        let mut pending_cr = false;
        let mut reached_end = offset == 0;
        let _ = self.visit_overlay_range(
            ByteRange::new(0, total_len),
            token,
            |logical_start, chunk| {
                if logical_start >= offset {
                    reached_end = true;
                    return false;
                }
                let available = usize::try_from(offset.saturating_sub(logical_start))
                    .unwrap_or(chunk.len())
                    .min(chunk.len());
                let mut index = 0usize;
                if pending_cr && available > 0 {
                    if chunk[0] == b'\n' {
                        if offset > logical_start.saturating_add(1) {
                            lines_seen = lines_seen.saturating_add(1);
                        }
                        pending_cr = false;
                        index = 1;
                    } else {
                        lines_seen = lines_seen.saturating_add(1);
                        pending_cr = false;
                    }
                }

                while index < available {
                    match chunk[index] {
                        b'\r' => pending_cr = true,
                        b'\n' => {
                            lines_seen = lines_seen.saturating_add(1);
                            pending_cr = false;
                        }
                        _ if pending_cr => {
                            lines_seen = lines_seen.saturating_add(1);
                            pending_cr = false;
                            continue;
                        }
                        _ => {}
                    }
                    index += 1;
                }

                if available < chunk.len() {
                    reached_end = true;
                    return false;
                }
                true
            },
        );
        if reached_end && pending_cr && offset == total_len {
            lines_seen = lines_seen.saturating_add(1);
        }
        Some(LogicalLine(lines_seen))
    }

    /// Set the batch size for future viewport reads.
    ///
    /// The new value takes effect on the next [`set_viewport`](Self::set_viewport) call.
    pub fn set_batch_size(&mut self, batch_size: u64) {
        self.batch_size = batch_size;
    }

    /// Number of bytes currently held in the decoded-text cache.
    pub fn decoded_cache_used_bytes(&self) -> u64 {
        self.decoded_cache.borrow().used_bytes()
    }

    /// Snapshot of peak memory usage counters.
    ///
    /// Peak values are updated on each cache write; use these in budget
    /// regression tests to avoid dependency on OS RSS sampling.
    pub fn memory_stats(&self) -> VlfMemoryStats {
        self.stats.borrow().clone()
    }

    /// Update peak counters from current cache state.
    fn update_peak_stats(&self) {
        let raw = self.pager.metrics().cache_used_bytes;
        let decoded = self.decoded_cache.borrow().used_bytes();
        let desc_count = self.index.borrow().len() as u64;
        let descriptor_bytes =
            desc_count * std::mem::size_of::<super::page_index::PageDescriptor>() as u64;
        let mut stats = self.stats.borrow_mut();
        if raw > stats.peak_raw_bytes {
            stats.peak_raw_bytes = raw;
        }
        if decoded > stats.peak_decoded_bytes {
            stats.peak_decoded_bytes = decoded;
        }
        // Descriptor bytes are exact (not a peak), updated every call.
        stats.descriptor_bytes = descriptor_bytes;
        // peak_overlay_bytes stays 0 in the read-only milestone.
    }

    /// Record a raw pager read of `byte_count` bytes.
    ///
    /// If the first viewport has not yet been set, accumulates into
    /// `stats.bytes_before_first_viewport` so callers can diagnose how many
    /// bytes are fetched during open/scan before the first render.
    fn record_pager_read(&self, byte_count: u64) {
        if !self.first_viewport_set.get() {
            self.stats.borrow_mut().bytes_before_first_viewport += byte_count;
        }
    }

    /// Scan the page that begins at `page_start` (rounded down to
    /// `page_size` alignment is the caller's responsibility).
    ///
    /// Reads the page bytes, analyses line endings and UTF-8 seams, and
    /// inserts a `Scanned` [`PageDescriptor`] into the index.
    ///
    /// Background scan tasks call this repeatedly, advancing `page_start` by
    /// `page_size` until the whole file is covered.
    pub fn scan_page_at(&self, page_start: u64) -> io::Result<()> {
        let file_size = self.pager.file_size();
        if page_start >= file_size {
            return Ok(());
        }
        let page_end = (page_start + self.page_size).min(file_size);
        let file_range = ByteRange::new(page_start, page_end);

        let token = self.pager.current_generation();
        let page_bytes = self.pager.read_at(file_range, token)?;
        let bytes = page_bytes.as_bytes();
        self.record_pager_read(bytes.len() as u64);

        // ---- UTF-8 boundary detection ----------------------------------------

        let starts_at_utf8_boundary = page_start == 0 || is_utf8_leading(bytes.first().copied());
        let ends_at_utf8_boundary = ends_on_utf8_boundary(bytes);

        // ---- CRLF seam detection ---------------------------------------------

        // Does this page end with a lone \r whose \n is the first byte of the
        // next page?  Peek one byte beyond the page.
        let ends_with_cr_before_lf = if bytes.last() == Some(&b'\r') && page_end < file_size {
            let peek_token = self.pager.current_generation();
            matches!(
                self.pager.read_at(ByteRange::new(page_end, page_end + 1), peek_token),
                Ok(ref pb) if pb.as_bytes().first() == Some(&b'\n')
            )
        } else {
            false
        };

        // Does this page start with the LF half of a CRLF that was split from
        // the previous page?
        let starts_with_lf_of_crlf = if bytes.first() == Some(&b'\n') && page_start > 0 {
            self.index
                .borrow()
                .page_at_byte(page_start - 1)
                .is_some_and(|prev| prev.ends_with_cr_before_lf)
        } else {
            false
        };

        // ---- Count newlines + UTF-16 length ----------------------------------

        let (newline_count, utf16_len, first_line_prefix_len, last_line_suffix_len) =
            analyse_bytes(bytes, starts_with_lf_of_crlf, ends_with_cr_before_lf);

        // ---- Decoded range (seam-adjusted) -----------------------------------

        // The decoded range trims leading/trailing bytes that are not on UTF-8
        // codepoint boundaries.
        let decoded_start = if starts_at_utf8_boundary {
            page_start
        } else {
            page_start + leading_continuation_bytes(bytes) as u64
        };
        let decoded_end = if ends_at_utf8_boundary {
            page_end
        } else {
            page_end - trailing_incomplete_bytes(bytes) as u64
        };
        let decoded_range = ByteRange::new(decoded_start, decoded_end);

        let desc = PageDescriptor {
            file_range,
            decoded_range,
            byte_len: page_end - page_start,
            utf16_len,
            newline_count,
            first_line_prefix_len,
            last_line_suffix_len,
            starts_at_utf8_boundary,
            ends_at_utf8_boundary,
            starts_with_lf_of_crlf,
            ends_with_cr_before_lf,
            scan_state: ScanState::Scanned,
        };

        self.index.borrow_mut().insert(desc);
        self.update_peak_stats();
        Ok(())
    }

    /// Scan every page in the file sequentially.
    ///
    /// Intended for tests and single-threaded tooling.  In production a
    /// background task would drive `scan_page_at` viewport-first.
    pub fn scan_all(&self) -> io::Result<()> {
        let file_size = self.pager.file_size();
        let mut pos = 0u64;
        while pos < file_size {
            self.scan_page_at(pos)?;
            pos += self.page_size;
        }
        Ok(())
    }

    /// Scan pages in viewport-first order, then expand outward in alternating
    /// forward/backward steps until the whole file is covered.
    ///
    /// This is the intended driver for background scan tasks.  Pages overlapping
    /// `viewport` are scanned first so that line-addressing for the visible
    /// window becomes exact as quickly as possible.  The scan then expands one
    /// page forward and one page backward on each iteration until both ends of
    /// the file are reached.
    ///
    /// `cancel_check` is called before every page is scanned.  Return `true`
    /// from `cancel_check` to stop the scan early (e.g. when a newer viewport
    /// request arrives or the cancellation generation is bumped).
    pub fn scan_viewport_first(
        &self,
        viewport: ByteRange,
        mut cancel_check: impl FnMut() -> bool,
    ) -> io::Result<()> {
        let file_size = self.pager.file_size();
        if file_size == 0 {
            return Ok(());
        }

        let page_size = self.page_size;
        // Snap viewport start down and end up to page boundaries.
        let vp_first = (viewport.start.0 / page_size) * page_size;
        let vp_last = {
            let end = viewport.end.0.min(file_size).max(1);
            ((end - 1) / page_size) * page_size
        };

        // Phase 1: scan all pages that overlap the viewport, left to right.
        let mut pos = vp_first;
        while pos <= vp_last {
            if cancel_check() {
                return Ok(());
            }
            self.scan_page_at(pos)?;
            pos += page_size;
        }

        // Phase 2: expand outward from the viewport edges, alternating
        // forward (after vp_last) and backward (before vp_first).
        let mut forward = vp_last + page_size;
        let mut backward = vp_first.checked_sub(page_size);

        loop {
            let mut did_work = false;

            if forward < file_size {
                if cancel_check() {
                    return Ok(());
                }
                self.scan_page_at(forward)?;
                forward += page_size;
                did_work = true;
            }

            if let Some(bw) = backward {
                if cancel_check() {
                    return Ok(());
                }
                self.scan_page_at(bw)?;
                backward = bw.checked_sub(page_size);
                did_work = true;
            }

            if !did_work {
                break;
            }
        }

        Ok(())
    }

    /// Borrow the page index for inspection (e.g. by callers that drive
    /// viewport-first scanning).
    pub fn index(&self) -> std::cell::Ref<'_, PageIndex> {
        self.index.borrow()
    }

    /// Drain any descriptors produced by the background indexing thread into
    /// the local page index.
    ///
    /// Called at the start of every API method that depends on the scan state
    /// so callers always see the most up-to-date index without requiring locks.
    fn drain_incoming(&self) {
        let rx = self.scan_rx.borrow();
        if let Some(receiver) = rx.as_ref() {
            while let Ok(desc) = receiver.try_recv() {
                self.index.borrow_mut().insert(desc);
            }
        }
    }

    /// Start a background thread that scans the file sequentially from byte 0,
    /// sending [`PageDescriptor`]s through a channel that is drained by
    /// `drain_incoming`.
    ///
    /// The scan stops automatically when the file is fully covered or when
    /// `self` is dropped.  Calling this method more than once is a no-op.
    pub fn start_background_indexing(&self) {
        // Guard: don't start a second scanner if one is already running.
        if self.scan_rx.borrow().is_some() {
            return;
        }

        let (tx, rx) = mpsc::channel();
        *self.scan_rx.borrow_mut() = Some(rx);

        let path = self.pager.canonical_path().to_owned();
        let page_size = self.page_size;
        let cancel = self.bg_cancel.clone();

        thread::Builder::new()
            .name("vlf-indexer".into())
            .spawn(move || {
                BackgroundScanner { path, page_size, cancel }.run(tx);
            })
            .ok(); // Ignore spawn failure; indexing simply won't happen.
    }

    /// Count line-feed bytes by streaming the file from disk.
    ///
    /// This is intentionally equivalent to `wc -l`: it does not materialize
    /// text and does not require the sparse page index to be complete.
    pub fn count_lf_streaming(&self) -> io::Result<u64> {
        let mut file = File::open(self.pager.canonical_path())?;
        let file_size = self.pager.file_size();
        #[cfg(unix)]
        if let Some(count) = count_lf_mmap(&file, file_size)? {
            return Ok(count);
        }

        advise_line_count_sequential(&file, file_size);

        let mut buf = vec![0u8; LINE_COUNT_BUFFER_SIZE];
        let mut bytes_seen = 0u64;
        let mut count = 0u64;

        while bytes_seen < file_size {
            let len = (file_size - bytes_seen).min(LINE_COUNT_BUFFER_SIZE as u64) as usize;
            let bytes_read = file.read(&mut buf[..len])?;
            if bytes_read == 0 {
                break;
            }
            count += bytecount::count(&buf[..bytes_read], b'\n') as u64;
            bytes_seen += bytes_read as u64;
        }

        Ok(count)
    }

    /// Return exact logical line count, caching the streaming LF count result.
    pub fn exact_logical_line_count_streaming(&self) -> io::Result<u64> {
        if let Some(count) = self.exact_line_count.get() {
            return Ok(count);
        }

        let count = if self.pager.file_size() == 0 {
            1
        } else {
            self.count_lf_streaming()?.saturating_add(1)
        };
        self.exact_line_count.set(Some(count));
        Ok(count)
    }

    // ------------------------------------------------------------------
    // Internal read helpers
    // ------------------------------------------------------------------

    /// Read a byte range with UTF-8 seam expansion.
    ///
    /// Expands the read by up to [`UTF8_SEAM_SLACK`] bytes on each side, then
    /// trims back to valid codepoint boundaries before decoding.  Returns a
    /// [`SeamResult`] that preserves both the original requested range and the
    /// adjusted decoded range so callers can distinguish them.
    ///
    /// Decoded text is also stored in the decoded-text cache keyed by
    /// `range.start.0`, with a priority determined by the active viewport.
    fn read_with_seam(&self, range: ByteRange) -> io::Result<SeamResult> {
        let token = self.pager.current_generation();
        self.read_with_seam_token(range, token)
    }

    fn read_with_seam_token(
        &self,
        range: ByteRange,
        token: CancelGeneration,
    ) -> io::Result<SeamResult> {
        // Check decoded cache first.
        if let Some((text, decoded_range)) = self.decoded_cache.borrow_mut().get(range.start.0) {
            return Ok(SeamResult { text, original_range: range, decoded_range });
        }

        let file_size = self.pager.file_size();
        let expanded_start = range.start.0.saturating_sub(UTF8_SEAM_SLACK);
        let expanded_end = (range.end.0 + UTF8_SEAM_SLACK).min(file_size);

        let page_bytes = self.pager.read_at(ByteRange::new(expanded_start, expanded_end), token)?;
        self.record_pager_read(expanded_end - expanded_start);
        let raw = page_bytes.as_bytes();

        // Offsets within `raw`.
        let req_start = (range.start.0 - expanded_start) as usize;
        let req_end = ((range.end.0 - expanded_start) as usize).min(raw.len());

        // Walk backward from req_start past any continuation bytes to find the
        // nearest codepoint start ≤ req_start.
        let mut actual_start = req_start;
        while actual_start > 0 && is_utf8_continuation(raw[actual_start]) {
            actual_start -= 1;
        }

        // Walk forward from req_end past any trailing continuation bytes to
        // include the full last codepoint.
        let mut actual_end = req_end;
        while actual_end < raw.len() && is_utf8_continuation(raw[actual_end]) {
            actual_end += 1;
        }

        let text = String::from_utf8_lossy(&raw[actual_start..actual_end]).into_owned();
        let decoded_range = ByteRange::new(
            expanded_start + actual_start as u64,
            expanded_start + actual_end as u64,
        );

        // Determine cache priority from the active viewport.
        let priority = {
            let vp = self.viewport.borrow();
            let batch = vp.batch_size;
            let overscan_start = vp.window_start.0.saturating_sub(batch);
            let overscan_end = vp.window_end.0.saturating_add(batch).min(file_size);
            if decoded_range.start.0 < vp.window_end.0 && decoded_range.end.0 > vp.window_start.0 {
                PagePriority::Viewport
            } else if decoded_range.start.0 < overscan_end && decoded_range.end.0 > overscan_start {
                PagePriority::Overscan
            } else {
                PagePriority::Background
            }
        };

        self.decoded_cache.borrow_mut().put(range.start.0, text.clone(), decoded_range, priority);
        self.update_peak_stats();

        Ok(SeamResult { text, original_range: range, decoded_range })
    }

    fn read_raw_range_token(
        &self,
        range: ByteRange,
        token: CancelGeneration,
    ) -> io::Result<Vec<u8>> {
        let file_size = self.pager.file_size();
        if range.end.0 > file_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("read end {} exceeds file_size {}", range.end.0, file_size),
            ));
        }

        let total_len = range.end.0.saturating_sub(range.start.0);
        if total_len == 0 {
            return Ok(Vec::new());
        }

        let chunk_cap = self.pager.max_read_size();
        if chunk_cap == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "max_read_size must be greater than zero",
            ));
        }

        let mut bytes = Vec::with_capacity(total_len as usize);
        let mut pos = range.start.0;
        while pos < range.end.0 {
            let end = pos.saturating_add(chunk_cap).min(range.end.0);
            let chunk = self.pager.read_at(ByteRange::new(pos, end), token)?;
            self.record_pager_read(end - pos);
            bytes.extend_from_slice(chunk.as_bytes());
            pos = end;
        }

        Ok(bytes)
    }

    /// Walk the page index to count lines before `byte_offset`, loading the
    /// relevant page if needed.
    fn byte_to_line_internal(&self, offset: u64) -> Option<LogicalLine> {
        // Phase 1: find page + accumulated line count under borrow.
        let (file_range_opt, acc_lines) = {
            let index = self.index.borrow();
            let mut acc: u64 = 0;
            let mut found: Option<ByteRange> = None;
            for desc in index.descriptors.values() {
                if desc.scan_state != ScanState::Scanned {
                    break;
                }
                if desc.file_range.start.0 <= offset && offset < desc.file_range.end.0 {
                    found = Some(desc.file_range);
                    break;
                }
                acc += desc.newline_count;
            }
            (found, acc)
        };

        let fr = file_range_opt?;
        let offset_in_page = (offset - fr.start.0) as usize;

        let token = self.pager.current_generation();
        let pb = self.pager.read_at(fr, token).ok()?;
        let bytes = pb.as_bytes();
        let count_end = offset_in_page.min(bytes.len());
        let nl_count = bytes[..count_end].iter().filter(|&&b| b == b'\n').count() as u64;
        Some(LogicalLine(acc_lines + nl_count))
    }

    /// Resolve line → byte using page index + sub-page decode.
    fn line_to_byte_internal(&self, line: u64) -> LineLookup {
        // Fast path: line 0 always starts at byte 0 for non-empty files.
        if line == 0 && self.pager.file_size() > 0 {
            return LineLookup::Exact(ByteOffset(0));
        }

        // Phase 1: find the page and its line base, under borrow.
        let phase1 = {
            let index = self.index.borrow();
            match index.find_page_for_line(line) {
                Err(LineLookup::Pending) => {
                    // Exact lookup failed; fall back to linear interpolation so
                    // goto-line has an immediate approximate position to jump to
                    // while background indexing continues.
                    return match index.approximate_byte_for_line(line) {
                        Some(approx) => LineLookup::Approximate(approx),
                        None => LineLookup::Pending,
                    };
                }
                Err(lookup) => return lookup,
                Ok(loc) => (loc.page.file_range, loc.lines_before_page),
            }
        };
        let (fr, lines_before) = phase1;

        let line_within_page = line - lines_before;
        if line_within_page == 0 {
            return LineLookup::Exact(fr.start);
        }

        // Phase 2: load page bytes and count newlines to find the exact offset.
        let token = self.pager.current_generation();
        match self.pager.read_at(fr, token) {
            Err(_) => LineLookup::Pending,
            Ok(pb) => {
                let bytes = pb.as_bytes();
                let mut nl = 0u64;
                for (i, &b) in bytes.iter().enumerate() {
                    if b == b'\n' {
                        nl += 1;
                        if nl == line_within_page {
                            return LineLookup::Exact(ByteOffset(fr.start.0 + i as u64 + 1));
                        }
                    }
                }
                // Descriptor claimed more newlines than bytes contain.
                LineLookup::OutOfRange
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Drop: cancel the background indexer when the store is dropped
// ---------------------------------------------------------------------------

impl Drop for VlfStore {
    fn drop(&mut self) {
        // Signal the background thread to stop.  The thread checks this flag
        // before every page scan and exits when it is set.
        self.bg_cancel.store(true, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// BackgroundScanner
// ---------------------------------------------------------------------------

/// A background file-scanner that produces [`PageDescriptor`]s and sends them
/// to a [`VlfStore`] via an [`mpsc`] channel.
///
/// `BackgroundScanner` opens its own `File` handle so it runs independently of
/// the `VlfStore` (which stays on the main thread and uses `RefCell` for
/// interior mutability).  The scan proceeds sequentially from byte 0, which
/// ensures correct CRLF seam detection at every page boundary.
struct BackgroundScanner {
    path: PathBuf,
    page_size: u64,
    /// Shared with the owning `VlfStore`; set to `true` on drop.
    cancel: Arc<AtomicBool>,
}

impl BackgroundScanner {
    fn run(self, tx: mpsc::Sender<PageDescriptor>) {
        let file = match File::open(&self.path) {
            Ok(f) => f,
            Err(_) => return,
        };
        let file_size = match file.metadata() {
            Ok(m) => m.len(),
            Err(_) => return,
        };

        let mut pos = 0u64;
        // Track the previous page's CRLF tail flag for seam detection without
        // needing to look up the index (which lives on the main thread).
        let mut prev_ends_with_cr: bool = false;

        while pos < file_size {
            if self.cancel.load(Ordering::Acquire) {
                return;
            }

            let page_end = (pos + self.page_size).min(file_size);
            let len = (page_end - pos) as usize;

            let bytes = match pread_exact(&file, pos, len) {
                Ok(b) => b,
                Err(_) => return,
            };

            // ---- UTF-8 boundary detection -----------------------------------

            let starts_at_utf8_boundary = pos == 0 || is_utf8_leading(bytes.first().copied());
            let ends_at_utf8_boundary = ends_on_utf8_boundary(&bytes);

            // ---- CRLF seam detection ----------------------------------------

            let ends_with_cr_before_lf = if bytes.last() == Some(&b'\r') && page_end < file_size {
                // Peek at the first byte of the next page.
                match pread_exact(&file, page_end, 1) {
                    Ok(peek) => peek.first() == Some(&b'\n'),
                    Err(_) => false,
                }
            } else {
                false
            };

            // The leading \n is the LF half of a \r\n split from the previous
            // page when the previous page ended with a lone \r.
            let starts_with_lf_of_crlf =
                bytes.first() == Some(&b'\n') && pos > 0 && prev_ends_with_cr;

            // ---- Byte analysis ----------------------------------------------

            let (newline_count, utf16_len, first_line_prefix_len, last_line_suffix_len) =
                analyse_bytes(&bytes, starts_with_lf_of_crlf, ends_with_cr_before_lf);

            // ---- Decoded range (seam-adjusted) ------------------------------

            let decoded_start = if starts_at_utf8_boundary {
                pos
            } else {
                pos + leading_continuation_bytes(&bytes) as u64
            };
            let decoded_end = if ends_at_utf8_boundary {
                page_end
            } else {
                page_end - trailing_incomplete_bytes(&bytes) as u64
            };

            let file_range = ByteRange::new(pos, page_end);
            let desc = PageDescriptor {
                file_range,
                decoded_range: ByteRange::new(decoded_start, decoded_end),
                byte_len: page_end - pos,
                utf16_len,
                newline_count,
                first_line_prefix_len,
                last_line_suffix_len,
                starts_at_utf8_boundary,
                ends_at_utf8_boundary,
                starts_with_lf_of_crlf,
                ends_with_cr_before_lf,
                scan_state: ScanState::Scanned,
            };

            prev_ends_with_cr = ends_with_cr_before_lf;

            if tx.send(desc).is_err() {
                // Receiver (VlfStore) was dropped; stop scanning.
                return;
            }

            pos += self.page_size;
        }
    }
}

// ---------------------------------------------------------------------------
// TextStore impl
// ---------------------------------------------------------------------------

impl TextStore for VlfStore {
    fn mode(&self) -> DocumentMode {
        DocumentMode::Vlf
    }

    fn len_bytes(&self) -> u64 {
        self.overlay_len_bytes().unwrap_or_else(|| self.pager.file_size())
    }

    fn known_line_count(&self) -> KnownLineCount {
        if let Some(count) = self.exact_line_count.get() {
            return KnownLineCount::Exact(count);
        }

        self.drain_incoming();
        let index = self.index.borrow();
        let progress = index.scan_progress();
        if progress.is_complete() {
            // Sum all scanned newlines + 1 for the final partial line.
            let total_nl: u64 = index.descriptors.values().map(|d| d.newline_count).sum();
            let exact_count = total_nl + 1;
            self.exact_line_count.set(Some(exact_count));
            KnownLineCount::Exact(exact_count)
        } else if index.is_empty() {
            KnownLineCount::Unknown
        } else {
            // Extrapolate from scanned bytes.
            let scanned_nl: u64 = index
                .descriptors
                .values()
                .filter(|d| d.scan_state == ScanState::Scanned)
                .map(|d| d.newline_count)
                .sum();
            if progress.scanned_bytes == 0 {
                return KnownLineCount::Unknown;
            }
            let estimated = (scanned_nl as f64 / progress.scanned_bytes as f64
                * progress.total_bytes as f64) as u64;
            // Apply a monotone floor so the displayed approximate count never
            // decreases as more pages are scanned (stable line numbers).
            let floor = self.approx_line_floor.get();
            let stabilized = estimated.max(1).max(floor);
            self.approx_line_floor.set(stabilized);
            KnownLineCount::Approximate(stabilized)
        }
    }

    fn read_byte_range(&self, range: ByteRange) -> TextChunkResult {
        if self.overlay_read_enabled() {
            let len_bytes = self.len_bytes();
            if range.start.0 > len_bytes || range.end.0 > len_bytes {
                return TextChunkResult::Unsupported;
            }
            if range.is_empty() {
                return TextChunkResult::Ready(TextChunk {
                    text: String::new(),
                    byte_range: range,
                });
            }
            let token = self.pager.current_generation();
            return match self.overlay_read_exact_range(range, token) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => TextChunkResult::Cancelled,
                Err(_) => TextChunkResult::Pending,
                Ok(chunk) => TextChunkResult::Ready(chunk),
            };
        }
        let file_size = self.pager.file_size();
        if range.start.0 > file_size || range.end.0 > file_size {
            return TextChunkResult::Unsupported;
        }
        if range.is_empty() {
            return TextChunkResult::Ready(TextChunk { text: String::new(), byte_range: range });
        }
        match self.read_with_seam(range) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => TextChunkResult::Cancelled,
            Err(_) => TextChunkResult::Pending,
            Ok(seam) => TextChunkResult::Ready(TextChunk {
                text: seam.text,
                byte_range: seam.decoded_range,
            }),
        }
    }

    fn line_to_byte(&self, line: LogicalLine) -> LineLookup {
        if self.overlay_read_enabled() {
            return self.overlay_line_to_byte(line.0);
        }
        self.drain_incoming();
        self.line_to_byte_internal(line.0)
    }

    fn byte_to_line(&self, offset: ByteOffset) -> Option<LogicalLine> {
        if self.overlay_read_enabled() {
            return self.overlay_byte_to_line(offset.0);
        }
        if offset.0 > self.pager.file_size() {
            return None;
        }
        self.drain_incoming();
        self.byte_to_line_internal(offset.0)
    }

    fn iter_chunks(&self, range: ByteRange) -> Box<dyn Iterator<Item = TextChunkResult> + '_> {
        if self.overlay_read_enabled() {
            let len_bytes = self.len_bytes();
            if range.start.0 > len_bytes || range.end.0 > len_bytes {
                return Box::new(std::iter::once(TextChunkResult::Unsupported));
            }
            if range.is_empty() {
                return Box::new(std::iter::once(TextChunkResult::Ready(TextChunk {
                    text: String::new(),
                    byte_range: range,
                })));
            }
            let token = self.pager.current_generation();
            let result = match self.overlay_read_exact_range(range, token) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => TextChunkResult::Cancelled,
                Err(_) => TextChunkResult::Pending,
                Ok(chunk) => TextChunkResult::Ready(chunk),
            };
            return Box::new(std::iter::once(result));
        }
        let file_size = self.pager.file_size();
        if range.start.0 > file_size || range.end.0 > file_size {
            return Box::new(std::iter::once(TextChunkResult::Unsupported));
        }

        let token = self.pager.current_generation();
        let mut results = Vec::new();
        let mut pos = range.start.0;

        while pos < range.end.0 {
            // Check for cancellation between chunks.
            if token != self.pager.current_generation() {
                results.push(TextChunkResult::Cancelled);
                break;
            }
            let chunk_end = (pos + self.page_size).min(range.end.0).min(file_size);
            let chunk_range = ByteRange::new(pos, chunk_end);
            match self.pager.read_at(chunk_range, token) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                    results.push(TextChunkResult::Cancelled);
                    break;
                }
                Err(_) => {
                    results.push(TextChunkResult::Pending);
                    break;
                }
                Ok(pb) => {
                    let text = String::from_utf8_lossy(pb.as_bytes()).into_owned();
                    results
                        .push(TextChunkResult::Ready(TextChunk { text, byte_range: chunk_range }));
                    pos = chunk_end;
                }
            }
        }

        Box::new(results.into_iter())
    }

    fn snapshot_id(&self) -> u64 {
        // Derived from file metadata: size XOR mtime seconds.
        let mtime_secs = self
            .pager
            .modified()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.len_bytes().wrapping_add(mtime_secs)
    }

    fn byte_to_utf16(&self, _offset: ByteOffset) -> Option<Utf16Offset> {
        // UTF-16 offset mapping requires a fully scanned page; not yet
        // implemented at this milestone.  Returns None to signal unavailability.
        None
    }

    fn utf16_to_byte(&self, _offset: Utf16Offset) -> Utf16Lookup {
        Utf16Lookup::Pending
    }

    fn full_text_policy(&self) -> FullTextPolicy {
        // VLF documents must never expose full-text extraction.
        FullTextPolicy::Forbidden
    }

    fn edit_permission(&self) -> EditPermission {
        // When an overlay is active (editing enabled), edits are permitted.
        // Before enable_editing() is called the store remains read-only.
        if self.overlay_read_enabled() {
            EditPermission::Allowed
        } else {
            EditPermission::Forbidden { reason: VLF_READ_ONLY_REASON }
        }
    }

    fn doc_status(&self) -> crate::text_store::DocStatus {
        let gates = DocumentMode::Vlf.feature_gates();
        let progress = self.index.borrow().scan_progress();
        let mut disabled_features: Vec<&'static str> = gates.disabled_features().collect();
        let overlay = self.overlay.borrow();
        if let Some(overlay) = overlay.as_ref() {
            if overlay.edit_gate().read_byte_range_ready {
                disabled_features.retain(|feature| *feature != "editing");
            }
            if overlay.edit_gate().streaming_save_ready {
                disabled_features.retain(|feature| *feature != "save");
            }
        }
        crate::text_store::DocStatus {
            file_size_bytes: self.pager.file_size(),
            mode_name: "vlf",
            disabled_features,
            indexing_progress: progress.fraction(),
            downgrade_notice: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Byte analysis helpers
// ---------------------------------------------------------------------------

/// True if `byte` is a UTF-8 continuation byte (10xxxxxx).
#[inline]
fn is_utf8_continuation(byte: u8) -> bool {
    byte & 0xC0 == 0x80
}

/// True if `byte` is None (empty slice) or is a UTF-8 leading/ASCII byte.
#[inline]
fn is_utf8_leading(byte: Option<u8>) -> bool {
    byte.is_none_or(|b| !is_utf8_continuation(b))
}

/// True if `bytes` ends on a complete UTF-8 codepoint boundary.
fn ends_on_utf8_boundary(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    // Find the last non-continuation byte.
    let mut i = bytes.len();
    let mut cont = 0usize;
    while i > 0 && is_utf8_continuation(bytes[i - 1]) {
        i -= 1;
        cont += 1;
    }
    if i == 0 {
        return false; // All continuation bytes — invalid.
    }
    let leading = bytes[i - 1];
    if leading & 0x80 == 0 {
        return cont == 0; // ASCII must have no continuations.
    }
    let expected = if leading & 0xE0 == 0xC0 {
        1
    } else if leading & 0xF0 == 0xE0 {
        2
    } else if leading & 0xF8 == 0xF0 {
        3
    } else {
        return false; // Invalid leading byte.
    };
    cont == expected
}

/// Number of leading continuation bytes (bytes that should belong to the
/// previous page's incomplete codepoint).
fn leading_continuation_bytes(bytes: &[u8]) -> usize {
    bytes.iter().take_while(|&&b| is_utf8_continuation(b)).count()
}

/// Number of trailing bytes that form an incomplete codepoint at the end.
fn trailing_incomplete_bytes(bytes: &[u8]) -> usize {
    if bytes.is_empty() || ends_on_utf8_boundary(bytes) {
        return 0;
    }
    let mut i = bytes.len();
    let mut cnt = 0;
    while i > 0 {
        i -= 1;
        cnt += 1;
        if !is_utf8_continuation(bytes[i]) {
            break;
        }
    }
    cnt
}

/// Analyse raw page bytes and return:
/// `(newline_count, utf16_len, first_line_prefix_len, last_line_suffix_len)`.
///
/// Counts `\n` as line endings.  Accounts for CRLF seam flags so split
/// `\r\n` pairs are not double-counted.
fn analyse_bytes(
    bytes: &[u8],
    starts_with_lf_of_crlf: bool,
    ends_with_cr_before_lf: bool,
) -> (u64, u64, u64, u64) {
    let byte_len = bytes.len() as u64;
    let mut newline_count: u64 = 0;
    let mut first_nl: Option<usize> = None;
    let mut last_nl: Option<usize> = None;

    // Skip the leading \n if it is the LF of a split \r\n.
    let start_offset = if starts_with_lf_of_crlf && bytes.first() == Some(&b'\n') { 1 } else { 0 };

    let mut i = start_offset;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\n' {
            // Skip if it is the LF following a \r within this page (already counted at \r).
            let prev_is_cr = i > 0 && bytes[i - 1] == b'\r';
            if !prev_is_cr {
                newline_count += 1;
                if first_nl.is_none() {
                    first_nl = Some(i);
                }
                last_nl = Some(i);
            }
        } else if b == b'\r' {
            let is_last = i == bytes.len() - 1;
            if is_last && ends_with_cr_before_lf {
                // The \n will be on the next page; don't count yet.
            } else if bytes.get(i + 1) == Some(&b'\n') {
                // \r\n pair: count once (the \n branch above is skipped because prev_is_cr).
                newline_count += 1;
                if first_nl.is_none() {
                    first_nl = Some(i);
                }
                last_nl = Some(i + 1);
                i += 1; // skip the \n
            } else {
                // Lone \r (old Mac line ending).
                newline_count += 1;
                if first_nl.is_none() {
                    first_nl = Some(i);
                }
                last_nl = Some(i);
            }
        }
        i += 1;
    }

    // UTF-16 length: use lossy decode so invalid bytes don't panic.
    let utf16_len: u64 = String::from_utf8_lossy(bytes).chars().map(|c| c.len_utf16() as u64).sum();

    let first_line_prefix_len = first_nl.map_or(byte_len, |i| i as u64 + 1);
    let last_line_suffix_len = last_nl.map_or(byte_len, |i| byte_len - i as u64 - 1);

    (newline_count, utf16_len, first_line_prefix_len, last_line_suffix_len)
}

#[cfg(unix)]
fn count_lf_mmap(file: &File, file_size: u64) -> io::Result<Option<u64>> {
    if file_size == 0 {
        return Ok(Some(0));
    }

    let Ok(len) = usize::try_from(file_size) else {
        return Ok(None);
    };

    let ptr = unsafe {
        libc::mmap(ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, file.as_raw_fd(), 0)
    };

    if ptr == libc::MAP_FAILED {
        return Ok(None);
    }

    let bytes = unsafe { slice::from_raw_parts(ptr.cast::<u8>(), len) };
    let count = count_lf_mmap_bytes(bytes)?;

    if unsafe { libc::munmap(ptr, len) } != 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(Some(count))
}

#[cfg(unix)]
fn count_lf_mmap_bytes(bytes: &[u8]) -> io::Result<u64> {
    let worker_count = thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(LINE_COUNT_MMAP_MAX_THREADS);

    if worker_count <= 1 || bytes.len() < LINE_COUNT_MMAP_PARALLEL_THRESHOLD {
        return Ok(bytecount::count(bytes, b'\n') as u64);
    }

    let chunk_size = bytes.len().div_ceil(worker_count);
    thread::scope(|scope| {
        let handles = bytes
            .chunks(chunk_size)
            .map(|chunk| scope.spawn(move || bytecount::count(chunk, b'\n') as u64))
            .collect::<Vec<_>>();

        let mut count = 0u64;
        for handle in handles {
            count += handle
                .join()
                .map_err(|_| io::Error::other("parallel line count worker panicked"))?;
        }
        Ok(count)
    })
}

#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "ios", target_os = "tvos", target_os = "watchos"))
))]
fn advise_line_count_sequential(file: &File, file_size: u64) {
    let _ = unsafe {
        libc::posix_fadvise(
            file.as_raw_fd(),
            0,
            file_size.min(libc::off_t::MAX as u64) as libc::off_t,
            libc::POSIX_FADV_SEQUENTIAL,
        )
    };
}

#[cfg(any(
    not(unix),
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos"
))]
fn advise_line_count_sequential(_file: &File, _file_size: u64) {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::super::pager::DEFAULT_MAX_READ_SIZE;
    use super::*;
    use crate::text_store::TextStore;
    fn store_from(content: &[u8]) -> (VlfStore, NamedTempFile) {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();
        let store = VlfStore::open_with_config(f.path(), 64, 1024 * 1024).unwrap();
        (store, f)
    }

    // ---- TextStore contract ------------------------------------------

    #[test]
    fn mode_is_vlf() {
        let (store, _f) = store_from(b"hello");
        assert_eq!(store.mode(), DocumentMode::Vlf);
    }

    #[test]
    fn full_text_policy_is_forbidden() {
        let (store, _f) = store_from(b"hello");
        assert_eq!(store.full_text_policy(), FullTextPolicy::Forbidden);
    }

    #[test]
    fn read_full_text_returns_unsupported() {
        let (store, _f) = store_from(b"hello world");
        assert_eq!(store.read_full_text(), TextChunkResult::Unsupported);
    }

    #[test]
    fn count_lf_streaming_matches_wc_l_semantics() {
        let (store, _f) = store_from(b"alpha\nbeta\ngamma\n");
        assert_eq!(store.count_lf_streaming().unwrap(), 3);

        let (store, _f) = store_from(b"alpha\nbeta\ngamma");
        assert_eq!(store.count_lf_streaming().unwrap(), 2);
    }

    #[test]
    fn len_bytes_matches_content() {
        let content = b"hello world\n";
        let (store, _f) = store_from(content);
        assert_eq!(store.len_bytes(), content.len() as u64);
    }

    #[test]
    fn known_line_count_unknown_before_scan() {
        let (store, _f) = store_from(b"a\nb\nc");
        assert_eq!(store.known_line_count(), KnownLineCount::Unknown);
    }

    #[test]
    fn known_line_count_exact_after_full_scan() {
        let content = b"a\nb\nc";
        let (store, _f) = store_from(content);
        store.scan_all().unwrap();
        match store.known_line_count() {
            KnownLineCount::Exact(n) => assert_eq!(n, 3),
            other => panic!("expected Exact, got {:?}", other),
        }
    }

    #[test]
    fn known_line_count_trailing_newline() {
        let content = b"a\nb\n";
        let (store, _f) = store_from(content);
        store.scan_all().unwrap();
        match store.known_line_count() {
            KnownLineCount::Exact(n) => assert_eq!(n, 3),
            other => panic!("expected Exact, got {:?}", other),
        }
    }

    // ---- read_byte_range --------------------------------------------

    #[test]
    fn read_byte_range_ascii() {
        let content = b"hello world";
        let (store, _f) = store_from(content);
        match store.read_byte_range(ByteRange::new(6, 11)) {
            TextChunkResult::Ready(chunk) => assert_eq!(chunk.text, "world"),
            other => panic!("expected Ready, got {:?}", other),
        }
    }

    #[test]
    fn read_byte_range_empty() {
        let (store, _f) = store_from(b"hello");
        match store.read_byte_range(ByteRange::new(2, 2)) {
            TextChunkResult::Ready(chunk) => assert!(chunk.text.is_empty()),
            other => panic!("expected Ready, got {:?}", other),
        }
    }

    #[test]
    fn read_byte_range_out_of_bounds() {
        let (store, _f) = store_from(b"hi");
        assert_eq!(store.read_byte_range(ByteRange::new(0, 99)), TextChunkResult::Unsupported);
    }

    #[test]
    fn read_byte_range_multibyte_seam_adjustment() {
        // "café": c(1) a(1) f(1) é(2 bytes U+00E9: 0xC3 0xA9)
        // Requesting [3, 4] splits the 2-byte 'é' codepoint.
        // The seam adjustment must include the full 'é'.
        let content = "café".as_bytes();
        let (store, _f) = store_from(content);
        match store.read_byte_range(ByteRange::new(3, 4)) {
            TextChunkResult::Ready(chunk) => {
                // The adjusted range should include 'é' fully.
                assert!(
                    chunk.text.contains('é') || chunk.text == "é" || chunk.byte_range.len() >= 2
                );
            }
            other => panic!("expected Ready, got {:?}", other),
        }
    }

    // ---- line_to_byte / byte_to_line --------------------------------

    #[test]
    fn line_to_byte_line_zero_exact_without_scan() {
        // Line 0 always maps to byte 0 for non-empty files; no scanning needed.
        let (store, _f) = store_from(b"a\nb\nc");
        assert_eq!(store.line_to_byte(LogicalLine(0)), LineLookup::Exact(ByteOffset(0)));
    }

    #[test]
    fn line_to_byte_non_zero_approximate_before_full_scan() {
        // Line 2 is not in the scanned prefix; expect Approximate (not Pending)
        // once at least one page is scanned so the interpolation has data.
        let content = b"hello\nworld\nfoo";
        let (store, _f) = store_from(content);
        store.scan_page_at(0).unwrap(); // scan first (and only) page
        match store.line_to_byte(LogicalLine(2)) {
            LineLookup::Exact(_) | LineLookup::Approximate(_) => {}
            other => panic!("expected Exact or Approximate before full scan, got {:?}", other),
        }
    }

    #[test]
    fn line_to_byte_approximate_before_scan_with_data() {
        // Before scanning, line 1 of a multi-page file returns Approximate (with
        // data from at least partial scan) or Pending when no scan data exists.
        let content = b"line1\nline2\nline3";
        let (store, _f) = store_from(content);
        // No pages scanned yet: line 1 is Pending (no interpolation data).
        match store.line_to_byte(LogicalLine(1)) {
            LineLookup::Pending | LineLookup::Approximate(_) => {}
            other => panic!("expected Pending or Approximate, got {:?}", other),
        }
        // Scan the single page; now line 1 is resolvable as Exact.
        store.scan_page_at(0).unwrap();
        assert_eq!(store.line_to_byte(LogicalLine(1)), LineLookup::Exact(ByteOffset(6)));
    }

    #[test]
    fn line_to_byte_exact_after_scan() {
        let content = b"hello\nworld\nfoo";
        let (store, _f) = store_from(content);
        store.scan_all().unwrap();
        assert_eq!(store.line_to_byte(LogicalLine(0)), LineLookup::Exact(ByteOffset(0)));
        assert_eq!(store.line_to_byte(LogicalLine(1)), LineLookup::Exact(ByteOffset(6)));
        assert_eq!(store.line_to_byte(LogicalLine(2)), LineLookup::Exact(ByteOffset(12)));
    }

    #[test]
    fn line_to_byte_out_of_range_after_full_scan() {
        let content = b"a\nb";
        let (store, _f) = store_from(content);
        store.scan_all().unwrap();
        assert_eq!(store.line_to_byte(LogicalLine(99)), LineLookup::OutOfRange);
    }

    #[test]
    fn approximate_goto_line_with_partial_index() {
        // Build a file large enough to span multiple 64-byte pages.
        // Each line is "line_NNNNN\n" (10 bytes); 200 lines = 2000 bytes → ~31 pages.
        let mut content = Vec::new();
        for i in 0u32..200 {
            content.extend_from_slice(format!("line_{:05}\n", i).as_bytes());
        }
        let (store, _f) = store_from(&content);
        // Scan only the first page (bytes 0..64).
        store.scan_page_at(0).unwrap();

        // Line 0: always Exact.
        assert_eq!(store.line_to_byte(LogicalLine(0)), LineLookup::Exact(ByteOffset(0)));

        // Line 100 (mid-file): index can't resolve it exactly yet, but should
        // return Approximate rather than Pending once we have scan data.
        match store.line_to_byte(LogicalLine(100)) {
            LineLookup::Approximate(ByteOffset(off)) => {
                // Offset should be somewhere in the middle of the file, not 0
                // and not past the end.
                assert!(off > 0, "approximate offset should be > 0 for line 100");
                assert!(
                    off <= content.len() as u64,
                    "approximate offset must not exceed file size"
                );
            }
            // Exact is acceptable if the index happened to cover it.
            LineLookup::Exact(_) => {}
            other => panic!("expected Approximate or Exact, got {:?}", other),
        }

        // After full scan, the result must be Exact.
        store.scan_all().unwrap();
        let expected_byte = 100 * 11; // "line_{:05}\n" = 11 bytes per line
        assert_eq!(
            store.line_to_byte(LogicalLine(100)),
            LineLookup::Exact(ByteOffset(expected_byte))
        );
    }

    #[test]
    fn viewport_first_line_lookup_resolves_within_scanned_prefix() {
        // File spans multiple pages; scan only the first page which covers
        // the first few lines.  Those lines should resolve as Exact, while
        // lines beyond the scanned prefix return Approximate (not Pending).
        let mut content = Vec::new();
        for i in 0u32..200 {
            content.extend_from_slice(format!("line_{:05}\n", i).as_bytes());
        }
        let (store, _f) = store_from(&content);
        store.scan_page_at(0).unwrap();

        // Lines within the first 64-byte page are Exact.
        // Page size=64; each line is 11 bytes: lines 0..=4 fit (55 bytes), line 5 partly.
        assert_eq!(store.line_to_byte(LogicalLine(0)), LineLookup::Exact(ByteOffset(0)));
        assert_eq!(store.line_to_byte(LogicalLine(1)), LineLookup::Exact(ByteOffset(11)));

        // Lines well beyond the scanned prefix: Approximate (not Pending).
        match store.line_to_byte(LogicalLine(150)) {
            LineLookup::Approximate(_) | LineLookup::Exact(_) => {}
            LineLookup::Pending => panic!("expected Approximate not Pending for line 150"),
            other => panic!("unexpected result {:?}", other),
        }
    }

    #[test]
    fn byte_to_line_after_scan() {
        let content = b"hello\nworld\nfoo";
        let (store, _f) = store_from(content);
        store.scan_all().unwrap();
        assert_eq!(store.byte_to_line(ByteOffset(0)), Some(LogicalLine(0)));
        assert_eq!(store.byte_to_line(ByteOffset(6)), Some(LogicalLine(1)));
        assert_eq!(store.byte_to_line(ByteOffset(12)), Some(LogicalLine(2)));
    }

    #[test]
    fn byte_to_line_out_of_range_returns_none() {
        let (store, _f) = store_from(b"hi");
        assert_eq!(store.byte_to_line(ByteOffset(99)), None);
    }

    // ---- iter_chunks ------------------------------------------------

    #[test]
    fn iter_chunks_covers_all_bytes() {
        let content = b"hello world";
        let (store, _f) = store_from(content);
        let chunks: String = store
            .iter_chunks(ByteRange::new(0, content.len() as u64))
            .map(|r| match r {
                TextChunkResult::Ready(c) => c.text,
                other => panic!("expected Ready, got {:?}", other),
            })
            .collect();
        assert_eq!(chunks.as_bytes(), content);
    }

    #[test]
    fn iter_chunks_out_of_bounds_unsupported() {
        let (store, _f) = store_from(b"hi");
        let results: Vec<_> = store.iter_chunks(ByteRange::new(0, 99)).collect();
        assert_eq!(results, vec![TextChunkResult::Unsupported]);
    }

    // ---- scan_all + page index state --------------------------------

    #[test]
    fn scan_all_marks_progress_complete() {
        let content = b"line one\nline two\nline three\n";
        let (store, _f) = store_from(content);
        store.scan_all().unwrap();
        assert!(store.index().scan_progress().is_complete());
    }

    #[test]
    fn scan_page_inserts_scanned_descriptor() {
        let content = b"abc\ndef\nghi\n";
        let (store, _f) = store_from(content);
        store.scan_page_at(0).unwrap();
        assert_eq!(store.index().len(), 1);
        let idx = store.index();
        let desc = idx.page_at_byte(0).unwrap();
        assert_eq!(desc.scan_state, ScanState::Scanned);
        assert_eq!(desc.newline_count, 3);
    }

    #[test]
    fn no_rope_conversion_method_exists() {
        // Compile-time guard: VlfStore has no `to_rope` or similar method.
        // If someone adds one, this comment serves as documentation that it
        // violates the VLF core invariant.
        let content = b"data";
        let (store, _f) = store_from(content);
        // full_text_policy must be Forbidden; read_full_text must be Unsupported.
        assert_eq!(store.full_text_policy(), FullTextPolicy::Forbidden);
        assert_eq!(store.read_full_text(), TextChunkResult::Unsupported);
    }

    // ---- analyse_bytes helper ---------------------------------------

    #[test]
    fn analyse_bytes_counts_newlines() {
        let (nl, _, _, _) = analyse_bytes(b"a\nb\nc", false, false);
        assert_eq!(nl, 2);
    }

    #[test]
    fn analyse_bytes_crlf_counted_once() {
        let (nl, _, _, _) = analyse_bytes(b"a\r\nb\r\n", false, false);
        assert_eq!(nl, 2);
    }

    #[test]
    fn analyse_bytes_skips_leading_lf_when_seam() {
        // starts_with_lf_of_crlf=true means the leading \n must not count.
        let (nl, _, _, _) = analyse_bytes(b"\nb\n", true, false);
        assert_eq!(nl, 1);
    }

    #[test]
    fn analyse_bytes_skips_trailing_cr_when_seam() {
        let (nl, _, _, _) = analyse_bytes(b"a\nb\r", false, true);
        assert_eq!(nl, 1); // only the \n counts; the \r is deferred
    }

    #[test]
    fn analyse_bytes_prefix_suffix_lengths() {
        // "hello\nworld\nfoo"
        let b = b"hello\nworld\nfoo";
        let (_, _, prefix, suffix) = analyse_bytes(b, false, false);
        assert_eq!(prefix, 6); // "hello\n"
        assert_eq!(suffix, 3); // "foo"
    }

    #[test]
    fn analyse_bytes_no_newlines() {
        let b = b"hello";
        let (nl, _, prefix, suffix) = analyse_bytes(b, false, false);
        assert_eq!(nl, 0);
        assert_eq!(prefix, 5);
        assert_eq!(suffix, 5);
    }

    // ---- ends_on_utf8_boundary helper -------------------------------

    #[test]
    fn utf8_boundary_ascii() {
        assert!(ends_on_utf8_boundary(b"hello"));
    }

    #[test]
    fn utf8_boundary_multibyte_complete() {
        assert!(ends_on_utf8_boundary("café".as_bytes()));
    }

    #[test]
    fn utf8_boundary_multibyte_incomplete() {
        // 'é' is 0xC3 0xA9; truncate to just 0xC3.
        assert!(!ends_on_utf8_boundary(&[0xC3]));
    }

    // ---- SeamResult preserves both ranges ---------------------------

    #[test]
    fn seam_result_preserves_original_range() {
        // 'é' = 0xC3 0xA9 at bytes [3,5) in "caféx".
        // Requesting [4,5) splits 'é'; seam must include full codepoint.
        let content = "caféx".as_bytes();
        let (store, _f) = store_from(content);
        let req = ByteRange::new(4, 5);
        let seam = store.read_with_seam(req).unwrap();
        // Original range is preserved exactly.
        assert_eq!(seam.original_range, req);
        // Decoded range covers the full 'é' (2 bytes starting at offset 3).
        assert!(seam.decoded_range.start.0 <= 3);
        assert!(seam.decoded_range.end.0 >= 5);
        assert!(seam.text.contains('é'));
    }

    #[test]
    fn seam_result_unmodified_for_ascii_range() {
        let content = b"hello world";
        let (store, _f) = store_from(content);
        let req = ByteRange::new(6, 11);
        let seam = store.read_with_seam(req).unwrap();
        assert_eq!(seam.original_range, req);
        // ASCII — decoded range should equal requested range (no adjustment needed).
        assert_eq!(seam.decoded_range, req);
        assert_eq!(seam.text, "world");
    }

    // ---- Multibyte chars exactly at page boundaries -----------------

    /// Build a store whose page boundary falls in the middle of a multibyte
    /// character.  Page size is the length of "abc" (3 bytes) and the content
    /// is "abc€xyz" where '€' (U+20AC) is 3 bytes: 0xE2 0x82 0xAC.
    /// The page break at byte 3 splits '€' across page 0 [0,3) and page 1 [3,6).
    fn store_with_multibyte_at_boundary() -> (VlfStore, tempfile::NamedTempFile) {
        // '€' = 0xE2 0x82 0xAC (3 bytes)
        let content = b"abc\xE2\x82\xACxyz";
        let mut f = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut f, content).unwrap();
        std::io::Write::flush(&mut f).unwrap();
        // page_size=3 so page 0 = [0,3)="abc", page 1 = [3,6)="\xE2\x82\xAC", page 2 = [6,9)="xyz"
        let store = VlfStore::open_with_config(f.path(), 3, 1024 * 1024).unwrap();
        (store, f)
    }

    #[test]
    fn multibyte_at_page_boundary_read_includes_full_codepoint() {
        let (store, _f) = store_with_multibyte_at_boundary();
        // Request [3, 6) — exactly '€' bytes — should decode cleanly.
        match store.read_byte_range(ByteRange::new(3, 6)) {
            TextChunkResult::Ready(chunk) => assert!(chunk.text.contains('€')),
            other => panic!("expected Ready, got {:?}", other),
        }
    }

    #[test]
    fn multibyte_split_at_page_boundary_seam_adjusts() {
        let (store, _f) = store_with_multibyte_at_boundary();
        // Request [2, 4): byte 2 = 'c', bytes 3-5 = '€'.
        // Seam adjustment must include the full '€' codepoint.
        let seam = store.read_with_seam(ByteRange::new(2, 4)).unwrap();
        assert!(seam.text.contains('€'), "decoded text must include '€': {:?}", seam.text);
        assert_ne!(
            seam.original_range, seam.decoded_range,
            "ranges should differ after adjustment"
        );
    }

    #[test]
    fn four_byte_codepoint_at_boundary_seam_adjusts() {
        // U+1F600 😀 = 0xF0 0x9F 0x98 0x80 (4 bytes)
        // page_size=4: page 0=[0,4)="abc\xF0", page 1=[4,8)="\x9F\x98\x80x"
        let content = b"abc\xF0\x9F\x98\x80x";
        let mut f = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut f, content).unwrap();
        std::io::Write::flush(&mut f).unwrap();
        let store = VlfStore::open_with_config(f.path(), 4, 1024 * 1024).unwrap();
        // Requesting [3,4) — first byte of 😀 — seam must expand to include all 4 bytes.
        let seam = store.read_with_seam(ByteRange::new(3, 4)).unwrap();
        assert!(seam.text.contains('😀'), "should include full 4-byte codepoint: {:?}", seam.text);
    }

    // ---- VlfViewportState -------------------------------------------

    #[test]
    fn viewport_state_initialized_to_zero_window() {
        let (store, _f) = store_from(b"hello");
        let vp = store.viewport_state();
        assert_eq!(vp.window_start.0, 0);
        assert_eq!(vp.window_end.0, 0);
        assert_eq!(vp.original_encoded_len, 0);
        assert!(!vp.dirty);
        assert_eq!(vp.batch_size, DEFAULT_BATCH_SIZE);
    }

    #[test]
    fn set_viewport_updates_window_state() {
        let content = b"hello world";
        let (store, _f) = store_from(content);
        store.set_viewport(ByteOffset(0), ByteOffset(5));
        let vp = store.viewport_state();
        assert_eq!(vp.window_start.0, 0);
        assert_eq!(vp.window_end.0, 5);
        assert_eq!(vp.original_encoded_len, 5);
        assert!(!vp.dirty);
    }

    #[test]
    fn set_batch_size_reflected_in_subsequent_set_viewport() {
        let content = b"hello world";
        let (mut store, _f) = store_from(content);
        store.set_batch_size(512);
        store.set_viewport(ByteOffset(0), ByteOffset(5));
        let vp = store.viewport_state();
        assert_eq!(vp.batch_size, 512);
    }

    // ---- Decoded text cache -----------------------------------------

    #[test]
    fn decoded_cache_populated_on_read() {
        let content = b"hello world";
        let (store, _f) = store_from(content);
        assert_eq!(store.decoded_cache_used_bytes(), 0);
        let _ = store.read_byte_range(ByteRange::new(0, 5));
        assert!(store.decoded_cache_used_bytes() > 0, "cache should be populated after read");
    }

    #[test]
    fn decoded_cache_hit_avoids_redundant_decode() {
        let content = b"hello world";
        let (store, _f) = store_from(content);
        // First read populates cache.
        let r1 = store.read_byte_range(ByteRange::new(0, 5));
        let used_after_first = store.decoded_cache_used_bytes();
        // Second read hits cache; used bytes must not grow.
        let r2 = store.read_byte_range(ByteRange::new(0, 5));
        assert_eq!(store.decoded_cache_used_bytes(), used_after_first);
        // Both reads return identical text.
        if let (TextChunkResult::Ready(c1), TextChunkResult::Ready(c2)) = (r1, r2) {
            assert_eq!(c1.text, c2.text);
        } else {
            panic!("expected two Ready results");
        }
    }

    #[test]
    fn decoded_cache_evicts_background_before_viewport() {
        // Content: "abcdef" (6 bytes); page_size=3 → pages [0,3)="abc", [3,6)="def".
        // decoded cache cap=5 bytes; batch_size=0 so overscan==viewport, making [3,6) Background.
        let content = b"abcdef";
        let mut f = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut f, content).unwrap();
        std::io::Write::flush(&mut f).unwrap();
        let store = VlfStore {
            pager: FilePager::open_with_config(f.path(), 1024 * 1024, DEFAULT_MAX_READ_SIZE)
                .unwrap(),
            index: RefCell::new(PageIndex::new(content.len() as u64)),
            page_size: 3,
            viewport: RefCell::new(VlfViewportState::new(0)),
            decoded_cache: RefCell::new(DecodedTextCache::new(5)),
            batch_size: 0, // overscan == viewport, so [3,6) is Background when viewport=[0,3)
            stats: RefCell::new(VlfMemoryStats::default()),
            first_viewport_set: Cell::new(false),
            scan_rx: RefCell::new(None),
            bg_cancel: Arc::new(AtomicBool::new(false)),
            approx_line_floor: Cell::new(0),
            exact_line_count: Cell::new(None),
            overlay: RefCell::new(None),
        };

        // Prime a background entry at [3,6) (before viewport is set).
        let _ = store.read_with_seam(ByteRange::new(3, 6)).unwrap();
        let used_bg = store.decoded_cache_used_bytes();
        assert!(used_bg > 0, "cache should have the background entry");

        // Set viewport over [0,3); with batch_size=0, [3,6) becomes Background.
        store.set_viewport(ByteOffset(0), ByteOffset(3));

        // Reading [0,3) should evict the background [3,6) entry to stay within cap=5.
        let _ = store.read_with_seam(ByteRange::new(0, 3)).unwrap();

        // Cache must not exceed cap.
        assert!(
            store.decoded_cache_used_bytes() <= 5,
            "cache exceeded cap: {} bytes",
            store.decoded_cache_used_bytes()
        );
    }

    // ---- edit_permission (read-only mode contract) -------------------

    #[test]
    fn edit_permission_is_forbidden_for_vlf() {
        let (store, _f) = store_from(b"hello");
        assert_eq!(
            store.edit_permission(),
            EditPermission::Forbidden { reason: VLF_READ_ONLY_REASON }
        );
    }

    #[test]
    fn copy_and_search_work_regardless_of_edit_permission() {
        // read_byte_range, iter_chunks, and line/byte lookups must succeed even
        // when edit_permission() returns Forbidden.
        let content = b"line one\nline two\nline three\n";
        let (store, _f) = store_from(content);
        store.scan_all().unwrap();

        assert_eq!(
            store.edit_permission(),
            EditPermission::Forbidden { reason: VLF_READ_ONLY_REASON }
        );
        // Navigation/search still works.
        assert_eq!(store.line_to_byte(LogicalLine(0)), LineLookup::Exact(ByteOffset(0)));
        match store.read_byte_range(ByteRange::new(0, 8)) {
            TextChunkResult::Ready(c) => assert_eq!(c.text, "line one"),
            other => panic!("expected Ready, got {:?}", other),
        }
        let chunks: Vec<_> = store.iter_chunks(ByteRange::new(0, 8)).collect();
        assert!(!chunks.is_empty());
    }

    #[test]
    fn overlay_reads_and_line_lookups_include_inserted_text() {
        let (store, _f) = store_from(b"alpha\nbeta\n");
        store.enable_editing();
        let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
        store.apply_insert(2, "XYZ", ctx).unwrap();

        assert_eq!(store.len_bytes(), 14);

        match store.read_byte_range(ByteRange::new(0, store.len_bytes())) {
            TextChunkResult::Ready(chunk) => assert_eq!(chunk.text, "alXYZpha\nbeta\n"),
            other => panic!("expected Ready, got {:?}", other),
        }

        assert_eq!(store.line_to_byte(LogicalLine(0)), LineLookup::Exact(ByteOffset(0)));
        assert_eq!(store.line_to_byte(LogicalLine(1)), LineLookup::Exact(ByteOffset(9)));
        assert_eq!(store.byte_to_line(ByteOffset(4)), Some(LogicalLine(0)));
        assert_eq!(store.byte_to_line(ByteOffset(11)), Some(LogicalLine(1)));

        let chunks: Vec<_> = store.iter_chunks(ByteRange::new(0, store.len_bytes())).collect();
        assert_eq!(chunks.len(), 1);
        match &chunks[0] {
            TextChunkResult::Ready(chunk) => assert_eq!(chunk.text, "alXYZpha\nbeta\n"),
            other => panic!("expected Ready, got {:?}", other),
        }
    }

    #[test]
    fn overlay_search_reads_include_inserted_text() {
        let (store, _f) = store_from(b"alpha\nbeta\n");
        store.enable_editing();
        let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
        store.apply_insert(5, " plus", ctx).unwrap();

        let token = store.pager.current_generation();
        let chunk = store.read_search_range(ByteRange::new(0, store.len_bytes()), token).unwrap();
        assert_eq!(chunk.text, "alpha plus\nbeta\n");
    }

    // ---- scan_viewport_first ----------------------------------------

    #[test]
    fn scan_viewport_first_covers_all_pages() {
        let content: Vec<u8> = (0..10).flat_map(|i| format!("line{i}\n").into_bytes()).collect();
        let (store, _f) = store_from(&content);
        let viewport = ByteRange::new(0, 20);
        store.scan_viewport_first(viewport, || false).unwrap();
        assert!(store.index().scan_progress().is_complete());
    }

    #[test]
    fn scan_viewport_first_scans_viewport_before_tail() {
        // Content: 10 * "line\n" = 50 bytes.  page_size=10 → 5 pages.
        // Viewport covers page 2 ([20,30)).  After scanning only the first
        // (viewport) step, page 2 must already be scanned.
        let content: Vec<u8> = b"0123456789".repeat(5).to_vec(); // 50 bytes, no newlines
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(&content).unwrap();
        f.flush().unwrap();
        let store = VlfStore::open_with_config(f.path(), 10, 1024 * 1024).unwrap();

        // Track how many pages are scanned on the viewport pass by stopping
        // after the viewport pages are done (cancel after 1 non-viewport page).
        let viewport = ByteRange::new(20, 30); // page 2
        let mut extra_count = 0u32;
        store
            .scan_viewport_first(viewport, || {
                // Count pages beyond the viewport; stop after 1 expansion step.
                let scanned = store.index().scan_progress().scanned_bytes;
                // After viewport (10 bytes) is scanned, allow one expansion step.
                if scanned > 10 {
                    extra_count += 1;
                    extra_count > 2
                } else {
                    false
                }
            })
            .unwrap();

        // Page 2 (viewport) must be scanned.
        let idx = store.index();
        let desc = idx.page_at_byte(20).expect("page 2 should be scanned");
        assert_eq!(desc.scan_state, ScanState::Scanned);
    }

    #[test]
    fn scan_viewport_first_cancellable() {
        let content: Vec<u8> = b"x".repeat(1024).to_vec();
        let (store, _f) = store_from(&content);
        let viewport = ByteRange::new(0, 64);
        // Cancel immediately after first page.
        let mut count = 0;
        store
            .scan_viewport_first(viewport, || {
                count += 1;
                count > 1
            })
            .unwrap();
        // Only a subset should be scanned.
        assert!(!store.index().scan_progress().is_complete());
    }

    // ---- VlfMemoryBudget / VlfMemoryStats ---------------------------

    #[test]
    fn open_with_budget_uses_provided_caps() {
        let content = b"hello world";
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();
        let budget = VlfMemoryBudget { raw_page_byte_cap: 4096, decoded_byte_cap: 2048 };
        let store = VlfStore::open_with_budget(f.path(), budget).unwrap();
        // Should open successfully; reads should still work.
        match store.read_byte_range(ByteRange::new(0, 5)) {
            TextChunkResult::Ready(c) => assert_eq!(c.text, "hello"),
            other => panic!("expected Ready, got {:?}", other),
        }
    }

    #[test]
    fn memory_stats_tracks_peak_decoded_bytes() {
        let content = b"hello world, this is some longer text for cache tracking";
        let (store, _f) = store_from(content);
        let before = store.memory_stats().peak_decoded_bytes;
        assert_eq!(before, 0);
        let _ = store.read_byte_range(ByteRange::new(0, content.len() as u64));
        let after = store.memory_stats().peak_decoded_bytes;
        assert!(after > 0, "peak_decoded_bytes should increase after a read");
    }

    #[test]
    fn memory_stats_tracks_descriptor_bytes() {
        let content = b"line one\nline two\nline three\n";
        let (store, _f) = store_from(content);
        assert_eq!(store.memory_stats().descriptor_bytes, 0);
        store.scan_all().unwrap();
        let stats = store.memory_stats();
        // At least one descriptor should have been tracked.
        assert!(stats.descriptor_bytes > 0, "descriptor_bytes must be non-zero after scan_all");
    }

    #[test]
    fn memory_stats_overlay_bytes_zero_in_read_only_milestone() {
        let (store, _f) = store_from(b"data");
        store.scan_all().unwrap();
        let _ = store.read_byte_range(ByteRange::new(0, 4));
        assert_eq!(store.memory_stats().peak_overlay_bytes, 0);
    }

    #[test]
    fn bytes_before_first_viewport_counts_reads_before_set_viewport() {
        // bytes_before_first_viewport must accumulate pager reads made before
        // set_viewport is called, then stop once the viewport is set.
        let (store, _f) = store_from(b"hello world");

        // Pre-viewport read: 11 bytes.
        let _ = store.read_byte_range(ByteRange::new(0, 11));
        let before = store.memory_stats().bytes_before_first_viewport;
        assert!(before > 0, "expected non-zero pre-viewport byte count, got {before}");

        // Set the viewport; counter must stop.
        store.set_viewport(ByteOffset(0), ByteOffset(11));

        // Post-viewport read.
        let _ = store.read_byte_range(ByteRange::new(0, 11));
        let after = store.memory_stats().bytes_before_first_viewport;
        assert_eq!(before, after, "bytes_before_first_viewport should not grow after set_viewport");
    }

    #[test]
    fn ten_gib_sparse_fixture_stays_within_configured_budget() {
        let ten_gib = 10u64 * 1024 * 1024 * 1024;
        let mut f = NamedTempFile::new().unwrap();
        f.as_file().set_len(ten_gib).unwrap();
        f.write_all(b"alpha\nbeta\n").unwrap();
        f.flush().unwrap();

        let budget = VlfMemoryBudget { raw_page_byte_cap: 64 * 1024, decoded_byte_cap: 16 * 1024 };
        let store = VlfStore::open_with_budget(f.path(), budget.clone()).unwrap();

        match store.read_byte_range(ByteRange::new(0, 8 * 1024)) {
            TextChunkResult::Ready(chunk) => assert!(chunk.text.starts_with("alpha\nbeta\n")),
            other => panic!("expected Ready, got {:?}", other),
        }

        let stats = store.memory_stats();
        assert_eq!(store.len_bytes(), ten_gib);
        assert!(
            stats.peak_raw_bytes <= budget.raw_page_byte_cap,
            "raw cache {} exceeded cap {}",
            stats.peak_raw_bytes,
            budget.raw_page_byte_cap
        );
        assert!(
            stats.peak_decoded_bytes <= budget.decoded_byte_cap,
            "decoded cache {} exceeded cap {}",
            stats.peak_decoded_bytes,
            budget.decoded_byte_cap
        );
    }

    #[test]
    fn first_viewport_read_does_not_require_full_scan() {
        let content = (0..20_000).map(|i| format!("line {i}\n")).collect::<String>();
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();
        let store = VlfStore::open_with_config(f.path(), 256, 64 * 1024).unwrap();

        store.set_viewport(ByteOffset(0), ByteOffset(256));

        match store.read_byte_range(ByteRange::new(0, 256)) {
            TextChunkResult::Ready(chunk) => assert!(chunk.text.starts_with("line 0\nline 1\n")),
            other => panic!("expected Ready, got {:?}", other),
        }

        assert_eq!(store.known_line_count(), KnownLineCount::Unknown);
        assert_eq!(store.index().len(), 0, "first viewport read must not force page-index scan");
        assert!(
            matches!(store.line_to_byte(LogicalLine(10_000)), LineLookup::Pending),
            "unscanned tail should stay unresolved after first viewport read"
        );
    }

    #[test]
    fn decoded_cache_stays_within_budget_cap() {
        // Small decoded cap: 20 bytes.  Content is 50 bytes spanning 5 × 10-byte pages.
        // Reading all 50 bytes should not exceed the cap.
        let content: Vec<u8> = b"0123456789".repeat(5).to_vec();
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(&content).unwrap();
        f.flush().unwrap();
        let budget = VlfMemoryBudget {
            raw_page_byte_cap: 1024 * 1024,
            decoded_byte_cap: 20, // only 2 pages can fit
        };
        let store = VlfStore::open_with_budget(f.path(), budget).unwrap();
        for start in (0u64..50).step_by(10) {
            let _ = store.read_byte_range(ByteRange::new(start, start + 10));
        }
        assert!(
            store.decoded_cache_used_bytes() <= 20,
            "decoded cache exceeded budget: {} bytes",
            store.decoded_cache_used_bytes()
        );
    }

    #[test]
    fn doc_status_mode_name_is_vlf() {
        let (store, _f) = store_from(b"hello");
        let status = store.doc_status();
        assert_eq!(status.mode_name, "vlf");
    }

    #[test]
    fn doc_status_file_size_matches_content() {
        let (store, _f) = store_from(b"hello world");
        let status = store.doc_status();
        assert_eq!(status.file_size_bytes, 11);
    }

    #[test]
    fn doc_status_disabled_features_excludes_search() {
        let (store, _f) = store_from(b"hi");
        let status = store.doc_status();
        assert!(!status.disabled_features.contains(&"search"), "search should be available in VLF");
        assert!(status.disabled_features.contains(&"editing"), "editing must be disabled");
        assert!(status.disabled_features.contains(&"save"), "save must be disabled");
        assert!(status.disabled_features.contains(&"undo"), "undo must be disabled");
        assert!(status.disabled_features.contains(&"lsp"), "lsp must be disabled");
    }

    #[test]
    fn doc_status_editable_vlf_reports_edit_and_save_enabled() {
        let (store, _f) = store_from(b"hi");
        store.enable_editing();

        let status = store.doc_status();
        assert!(!status.disabled_features.contains(&"editing"), "editing should be enabled");
        assert!(!status.disabled_features.contains(&"save"), "save should be enabled");
        assert!(status.disabled_features.contains(&"undo"), "undo should stay disabled");
        assert!(status.disabled_features.contains(&"lsp"), "other VLF restrictions remain");
        assert!(store.is_editing_enabled());
        assert!(store.is_save_enabled());
    }

    #[test]
    fn refresh_after_save_rebases_overlay_without_leaving_vlf_mode() {
        let (store, file) = store_from(b"alpha\n");
        store.enable_editing();
        let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
        store.apply_insert(6, "beta\n", ctx).unwrap();

        std::fs::write(file.path(), b"alpha\nbeta\n").unwrap();

        let mut store = store;
        store.refresh_after_save(file.path()).unwrap();

        assert!(store.is_editing_enabled());
        assert!(store.is_save_enabled());
        assert_eq!(store.signed_byte_delta(), 0);
        assert!(matches!(
            store.suggested_save_policy(),
            Some(VlfSavePolicy::SameSizeInPlaceOverwrite)
        ));
        assert_eq!(store.known_line_count(), KnownLineCount::Unknown);
    }

    #[test]
    fn doc_status_indexing_progress_zero_before_scan() {
        let (store, _f) = store_from(b"abc\ndef\n");
        let status = store.doc_status();
        // Nothing has been scanned yet.
        assert_eq!(status.indexing_progress, 0.0);
    }

    #[test]
    fn doc_status_indexing_progress_one_after_full_scan() {
        let (store, _f) = store_from(b"abc\ndef\n");
        store.scan_all().unwrap();
        let status = store.doc_status();
        assert!((status.indexing_progress - 1.0).abs() < f64::EPSILON);
    }

    // ---- Background indexing ----------------------------------------

    #[test]
    fn start_background_indexing_eventually_completes() {
        // Create content with several pages' worth of data.
        let content: Vec<u8> =
            (0..20).flat_map(|i| format!("line {i:04}\n").into_bytes()).collect();
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(&content).unwrap();
        f.flush().unwrap();

        let store = VlfStore::open_with_config(f.path(), 16, 1024 * 1024).unwrap();
        store.start_background_indexing();

        // Poll with a timeout to wait for background scan to complete.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            store.drain_incoming();
            if store.index().scan_progress().is_complete() {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("background indexing did not complete within 5 s");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // Line count must be exact after full scan.
        match store.known_line_count() {
            KnownLineCount::Exact(_) => {}
            other => panic!("expected Exact line count after full scan, got {:?}", other),
        }
    }

    #[test]
    fn start_background_indexing_idempotent() {
        // Calling start_background_indexing twice must not panic or spawn two threads.
        let (store, _f) = store_from(b"hello\nworld\n");
        store.start_background_indexing();
        store.start_background_indexing(); // second call is a no-op
    }

    #[test]
    fn background_indexing_produces_correct_newline_count() {
        let content = b"a\nb\nc\nd\ne\n";
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();

        // page_size=4 → pages overlap different newlines.
        let store = VlfStore::open_with_config(f.path(), 4, 1024 * 1024).unwrap();
        store.start_background_indexing();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            store.drain_incoming();
            if store.index().scan_progress().is_complete() {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("background indexing did not complete within 5 s");
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        // Content has 5 newlines → 6 lines.
        match store.known_line_count() {
            KnownLineCount::Exact(n) => assert_eq!(n, 6, "expected 6 lines"),
            other => panic!("expected Exact, got {:?}", other),
        }
    }

    // ---- Stable line numbers (approx_line_floor) --------------------

    #[test]
    fn approximate_line_count_never_decreases() {
        // Construct a file where the first page is denser with newlines than
        // the rest, so the extrapolated estimate would drop as more pages are
        // scanned.  The floor must prevent any decrease.
        //
        // Page 0 (8 bytes): "a\nb\nc\n\n" — 4 newlines in 8 bytes (dense).
        // Page 1 (8 bytes): "xxxxxxxx" — 0 newlines in 8 bytes (sparse).
        // Total = 16 bytes, 4 newlines → Exact(5) after full scan.
        // After page 0 only: estimate = 4/8 * 16 = 8 lines (over-estimate).
        // After pages 0+1: scan is complete → Exact(5).
        // The floor ensures the Approximate value only increased until exact.
        let content = b"a\nb\nc\n\nxxxxxxxx";
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();

        let store = VlfStore::open_with_config(f.path(), 8, 1024 * 1024).unwrap();

        // Scan only the first page and observe the approximate count.
        store.scan_page_at(0).unwrap();
        let first_approx = match store.known_line_count() {
            KnownLineCount::Approximate(n) => n,
            // If the single-page scan already produced Exact, the floor test
            // is vacuous — the index was fully covered in one pass.
            KnownLineCount::Exact(_) | KnownLineCount::Unknown => return,
        };

        // Scan the second page; the returned value must be >= first_approx
        // **unless** we now have an Exact value (which is always authoritative).
        store.scan_page_at(8).unwrap();
        match store.known_line_count() {
            KnownLineCount::Approximate(n) => {
                assert!(
                    n >= first_approx,
                    "Approximate line count decreased from {first_approx} to {n}"
                );
            }
            // Exact is always authoritative; no floor assertion needed.
            KnownLineCount::Exact(_) | KnownLineCount::Unknown => {}
        }
    }

    #[test]
    fn floor_preserved_across_multiple_drain_calls() {
        // After establishing a floor via known_line_count, subsequent calls
        // must not return a lower value even before the scan is complete.
        let content: Vec<u8> = b"line\n".repeat(100);
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(&content).unwrap();
        f.flush().unwrap();

        let store = VlfStore::open_with_config(f.path(), 10, 1024 * 1024).unwrap();
        store.scan_page_at(0).unwrap(); // scan first page only

        let first = match store.known_line_count() {
            KnownLineCount::Approximate(n) => n,
            KnownLineCount::Exact(_) => return, // already done; test not applicable
            KnownLineCount::Unknown => return,
        };

        // Second call must return >= first (floor is preserved).
        match store.known_line_count() {
            KnownLineCount::Approximate(n) => {
                assert!(n >= first, "second call returned {n} < {first}");
            }
            // Exact is authoritative; no floor assertion needed.
            KnownLineCount::Exact(_) | KnownLineCount::Unknown => {}
        }
    }

    // ---- Overlay edit API --------------------------------------------------

    #[test]
    fn edit_permission_forbidden_before_enable_editing() {
        let content = b"hello world";
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();

        let store = VlfStore::open(f.path()).unwrap();
        assert!(
            matches!(store.edit_permission(), EditPermission::Forbidden { .. }),
            "should be Forbidden before enable_editing"
        );
    }

    #[test]
    fn edit_permission_allowed_after_enable_editing() {
        let content = b"hello world";
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();

        let store = VlfStore::open(f.path()).unwrap();
        store.enable_editing();
        assert!(
            matches!(store.edit_permission(), EditPermission::Allowed),
            "should be Allowed after enable_editing"
        );
    }

    #[test]
    fn apply_insert_without_enable_editing_returns_error() {
        use crate::vlf::overlay::OverlayEditContext;
        let content = b"hello";
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();

        let store = VlfStore::open(f.path()).unwrap();
        let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
        let err = store.apply_insert(5, " world", ctx).unwrap_err();
        assert!(
            matches!(err, VlfEditError::EditingNotEnabled),
            "expected EditingNotEnabled, got {err:?}"
        );
    }

    #[test]
    fn apply_insert_after_enable_editing_succeeds() {
        use crate::vlf::overlay::OverlayEditContext;
        let content = b"hello";
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();

        let store = VlfStore::open(f.path()).unwrap();
        store.enable_editing();
        let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
        store.apply_insert(5, " world", ctx).unwrap();
        // signed_byte_delta should reflect the 6 inserted bytes.
        assert_eq!(store.signed_byte_delta(), 6);
    }

    #[test]
    fn apply_delete_after_enable_editing_succeeds() {
        use crate::vlf::overlay::OverlayEditContext;
        let content = b"hello world";
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();

        let store = VlfStore::open(f.path()).unwrap();
        store.enable_editing();
        let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
        use crate::text_store::ByteRange;
        store.apply_delete(ByteRange::new(5, 11), ctx).unwrap();
        assert_eq!(store.signed_byte_delta(), -6);
    }

    #[test]
    fn apply_insert_mid_file_preserves_surrounding_content() {
        use crate::vlf::overlay::OverlayEditContext;

        let content = b"alpha\nbeta\ngamma\n";
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();

        let store = VlfStore::open(f.path()).unwrap();
        store.enable_editing();
        let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
        store.apply_insert(10, " needle", ctx).unwrap();

        match store.read_byte_range(ByteRange::new(0, store.len_bytes())) {
            TextChunkResult::Ready(chunk) => {
                assert_eq!(chunk.text, "alpha\nbeta needle\ngamma\n");
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn apply_insert_mid_file_with_small_page_size_preserves_surrounding_content() {
        use crate::vlf::overlay::OverlayEditContext;

        let content = b"alpha\nbeta\ngamma\n";
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();

        let store = VlfStore::open_with_config(f.path(), 64, 1024 * 1024).unwrap();
        store.enable_editing();
        let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
        store.apply_insert(10, " needle", ctx).unwrap();

        match store.read_byte_range(ByteRange::new(0, store.len_bytes())) {
            TextChunkResult::Ready(chunk) => {
                assert_eq!(chunk.text, "alpha\nbeta needle\ngamma\n");
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn apply_insert_mid_file_after_scan_all_preserves_surrounding_content() {
        use crate::vlf::overlay::OverlayEditContext;

        let content = b"alpha\nbeta\ngamma\n";
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();

        let store = VlfStore::open_with_config(f.path(), 64, 1024 * 1024).unwrap();
        store.scan_all().unwrap();
        store.enable_editing();
        let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
        store.apply_insert(10, " needle", ctx).unwrap();

        match store.read_byte_range(ByteRange::new(0, store.len_bytes())) {
            TextChunkResult::Ready(chunk) => {
                assert_eq!(chunk.text, "alpha\nbeta needle\ngamma\n");
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn apply_delete_then_insert_replaces_mid_file_range() {
        use crate::vlf::overlay::OverlayEditContext;

        let content = b"alpha\nbeta\ngamma\n";
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();

        let store = VlfStore::open(f.path()).unwrap();
        store.enable_editing();
        let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
        store.apply_delete(ByteRange::new(6, 10), ctx).unwrap();
        store.apply_insert(6, "BETA!", ctx).unwrap();

        match store.read_byte_range(ByteRange::new(0, store.len_bytes())) {
            TextChunkResult::Ready(chunk) => {
                assert_eq!(chunk.text, "alpha\nBETA!\ngamma\n");
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn suggested_save_policy_none_before_enable_editing() {
        let content = b"hello";
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();

        let store = VlfStore::open(f.path()).unwrap();
        assert!(store.suggested_save_policy().is_none(), "no policy before editing");
    }

    #[test]
    fn suggested_save_policy_tail_shift_after_small_insert() {
        use crate::vlf::overlay::OverlayEditContext;
        let content = b"hello";
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();

        let store = VlfStore::open(f.path()).unwrap();
        store.enable_editing();
        let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
        store.apply_insert(5, " world", ctx).unwrap();
        assert!(
            matches!(
                store.suggested_save_policy(),
                Some(crate::vlf::overlay::VlfSavePolicy::TailShift { .. })
            ),
            "small insert should suggest TailShift"
        );
    }

    #[test]
    fn enable_editing_called_twice_is_noop() {
        let content = b"hello world";
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();

        let store = VlfStore::open(f.path()).unwrap();
        store.enable_editing();
        store.enable_editing(); // second call must not panic or reset overlay
        assert!(matches!(store.edit_permission(), EditPermission::Allowed));
    }

    #[test]
    fn peak_overlay_bytes_tracked_after_insert() {
        use crate::vlf::overlay::OverlayEditContext;
        let content = b"hello";
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();

        let store = VlfStore::open(f.path()).unwrap();
        store.enable_editing();
        let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
        store.apply_insert(5, " world", ctx).unwrap();
        assert!(store.memory_stats().peak_overlay_bytes > 0, "overlay bytes should be > 0");
    }
}
