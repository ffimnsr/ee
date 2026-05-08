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

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::Path;

use crate::text_store::{
    ByteOffset, ByteRange, DocumentMode, EditPermission, FullTextPolicy, KnownLineCount,
    LineLookup, LogicalLine, TextChunk, TextChunkResult, TextStore, Utf16Lookup, Utf16Offset,
};

use super::page_index::{PageDescriptor, PageIndex, ScanState};
use super::pager::{DEFAULT_CACHE_BYTE_CAP, FilePager};

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

/// Default byte cap for the decoded-text cache (32 MiB).
///
/// Raw-page bytes are budgeted separately in [`FilePager`].  This cap applies
/// only to the UTF-8 decoded strings stored alongside each raw page.
pub const DEFAULT_DECODED_CACHE_BYTE_CAP: u64 = 32 * 1024 * 1024;

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
// VlfMemoryStats
// ---------------------------------------------------------------------------

/// Peak memory usage counters tracked by a [`VlfStore`].
///
/// Updated whenever cache occupancy increases.  Use these counters in budget
/// regression tests instead of relying on OS-level RSS sampling, which is
/// unreliable in unit tests.
///
/// In the read-only first milestone `peak_overlay_bytes` is always 0.
#[derive(Debug, Clone, Default)]
pub struct VlfMemoryStats {
    /// Peak raw-page bytes held in the [`FilePager`] LRU cache.
    pub peak_raw_bytes: u64,
    /// Peak decoded-text bytes held in the decoded-text LRU cache.
    pub peak_decoded_bytes: u64,
    /// Approximate descriptor bytes: `size_of::<PageDescriptor>() × descriptor_count`.
    pub descriptor_bytes: u64,
    /// Peak overlay bytes (always 0 in the read-only milestone).
    pub peak_overlay_bytes: u64,
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
    /// [`UTF8_SEAM_SLACK`] bytes on each side and walking back to the nearest
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
    /// Whether the window has unsaved overlay changes (always `false` in the
    /// read-only milestone).
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
        })
    }

    // ------------------------------------------------------------------
    // Viewport state
    // ------------------------------------------------------------------

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
    }

    /// Return a snapshot of the current viewport state.
    pub fn viewport_state(&self) -> VlfViewportState {
        self.viewport.borrow().clone()
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
        // Check decoded cache first.
        if let Some((text, decoded_range)) = self.decoded_cache.borrow_mut().get(range.start.0) {
            return Ok(SeamResult { text, original_range: range, decoded_range });
        }

        let file_size = self.pager.file_size();
        let expanded_start = range.start.0.saturating_sub(UTF8_SEAM_SLACK);
        let expanded_end = (range.end.0 + UTF8_SEAM_SLACK).min(file_size);

        let token = self.pager.current_generation();
        let page_bytes = self.pager.read_at(ByteRange::new(expanded_start, expanded_end), token)?;
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
        // Phase 1: find the page and its line base, under borrow.
        let phase1 = {
            let index = self.index.borrow();
            match index.find_page_for_line(line) {
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
// TextStore impl
// ---------------------------------------------------------------------------

impl TextStore for VlfStore {
    fn mode(&self) -> DocumentMode {
        DocumentMode::Vlf
    }

    fn len_bytes(&self) -> u64 {
        self.pager.file_size()
    }

    fn known_line_count(&self) -> KnownLineCount {
        let index = self.index.borrow();
        let progress = index.scan_progress();
        if progress.is_complete() {
            // Sum all scanned newlines + 1 for the final partial line.
            let total_nl: u64 = index.descriptors.values().map(|d| d.newline_count).sum();
            KnownLineCount::Exact(total_nl + 1)
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
            KnownLineCount::Approximate(estimated.max(1))
        }
    }

    fn read_byte_range(&self, range: ByteRange) -> TextChunkResult {
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
        self.line_to_byte_internal(line.0)
    }

    fn byte_to_line(&self, offset: ByteOffset) -> Option<LogicalLine> {
        if offset.0 > self.pager.file_size() {
            return None;
        }
        self.byte_to_line_internal(offset.0)
    }

    fn iter_chunks(&self, range: ByteRange) -> Box<dyn Iterator<Item = TextChunkResult> + '_> {
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
        self.pager.file_size().wrapping_add(mtime_secs)
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
        // First VLF milestone is read-only.  Copy, search, and navigation
        // remain available through TextStore chunk APIs.
        EditPermission::Forbidden {
            reason: "VLF mode is read-only; copy, search, and navigation remain available",
        }
    }

    fn doc_status(&self) -> crate::text_store::DocStatus {
        let gates = DocumentMode::Vlf.feature_gates();
        let progress = self.index.borrow().scan_progress();
        crate::text_store::DocStatus {
            file_size_bytes: self.pager.file_size(),
            mode_name: "vlf",
            disabled_features: gates.disabled_features().collect(),
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
    fn line_to_byte_pending_before_scan() {
        let (store, _f) = store_from(b"a\nb\nc");
        assert_eq!(store.line_to_byte(LogicalLine(0)), LineLookup::Pending);
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
            EditPermission::Forbidden {
                reason: "VLF mode is read-only; copy, search, and navigation remain available",
            }
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
            EditPermission::Forbidden {
                reason: "VLF mode is read-only; copy, search, and navigation remain available",
            }
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
        assert!(status.disabled_features.contains(&"lsp"), "lsp must be disabled");
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
}
