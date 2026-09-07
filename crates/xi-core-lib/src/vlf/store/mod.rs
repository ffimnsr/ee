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

pub(crate) use std::cell::{Cell, RefCell};
pub(crate) use std::collections::{BTreeMap, HashMap};
pub(crate) use std::fs::File;
pub(crate) use std::io::{self, Read};
#[cfg(unix)]
pub(crate) use std::os::fd::AsRawFd;
pub(crate) use std::path::{Path, PathBuf};
pub(crate) use std::sync::Arc;
pub(crate) use std::sync::atomic::{AtomicBool, Ordering};
pub(crate) use std::sync::mpsc;
pub(crate) use std::thread;
#[cfg(unix)]
pub(crate) use std::{ptr, slice};

pub(crate) use crate::text_store::{
    ByteOffset, ByteRange, DocumentMode, EditPermission, FullTextPolicy, KnownLineCount,
    LineLookup, LogicalLine, TextChunk, TextChunkResult, TextStore, Utf16Lookup, Utf16Offset,
};

pub(crate) use super::page_index::{PageDescriptor, PageIndex, ScanState};
pub(crate) use super::pager::{CancelGeneration, DEFAULT_CACHE_BYTE_CAP, FilePager, pread_exact};
pub(crate) use crate::vlf::overlay::{
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

impl Drop for VlfStore {
    fn drop(&mut self) {
        // Signal the background thread to stop.  The thread checks this flag
        // before every page scan and exits when it is set.
        self.bg_cancel.store(true, Ordering::Release);
    }
}

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

mod editing;
mod overlay;
mod read;
mod scan;
mod text_store;
mod viewport;

#[cfg(test)]
mod tests;
