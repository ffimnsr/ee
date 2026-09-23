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

//! [`PageDescriptor`] and [`PageIndex`] — the metadata layer for the VLF
//! engine.
//!
//! `PageDescriptor` records per-page metadata **without** holding raw page
//! bytes.  `PageIndex` stores descriptors in a `BTreeMap` keyed by page-start
//! byte offset for O(log n) byte-to-page and line-to-page lookups.
//!
//! Background scanning inserts descriptors as pages are processed; callers
//! receive [`crate::text_store::LineLookup::Pending`] for regions that have
//! not yet been scanned.

use std::collections::BTreeMap;

use crate::text_store::{ByteRange, LineLookup, LogicalLine};

// ---------------------------------------------------------------------------
// ScanState
// ---------------------------------------------------------------------------

/// Whether a page has been analysed by the background scanner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanState {
    /// Descriptor was inserted as a placeholder; metrics are not yet accurate.
    NotScanned,
    /// Page bytes have been read and all metrics are exact.
    Scanned,
}

// ---------------------------------------------------------------------------
// PageDescriptor
// ---------------------------------------------------------------------------

/// Metadata about one page in the VLF index.
///
/// Raw page bytes are **not** stored here; they live in the
/// [`super::pager::FilePager`] cache and are evicted under memory pressure.
/// Only the metadata needed for line-addressing and viewport stitching is
/// kept.
#[derive(Debug, Clone)]
pub struct PageDescriptor {
    /// Byte range in the file that this page covers.
    pub file_range: ByteRange,

    /// Byte range within `file_range` that has been decoded as valid UTF-8
    /// after seam adjustment.  May differ from `file_range` by up to 3 bytes
    /// on each side when multibyte codepoints cross page boundaries.
    pub decoded_range: ByteRange,

    /// Raw byte length of the page (equals `file_range.len()`).
    pub byte_len: u64,

    /// UTF-16 code-unit length of the decoded text.
    pub utf16_len: u64,

    /// Number of complete line endings (`\n`) within this page, accounting
    /// for CRLF seam flags.
    pub newline_count: u64,

    /// Bytes from the page start to the end of the first line ending (i.e.
    /// to and including the first `\n`).  Equals `byte_len` when the page
    /// contains no line endings.
    ///
    /// Used for fast viewport stitching: tells the renderer how many bytes of
    /// this page complete the previous page's partial last line.
    pub first_line_prefix_len: u64,

    /// Bytes from after the last line ending to the page end.  Zero when the
    /// page ends on a `\n`.  Equals `byte_len` when the page contains no
    /// line endings.
    ///
    /// Used for viewport stitching: the partial line at the end of this page
    /// continues into the next page.
    pub last_line_suffix_len: u64,

    /// `true` if the page's first byte is on a valid UTF-8 codepoint
    /// boundary (i.e. is not a continuation byte).
    pub starts_at_utf8_boundary: bool,

    /// `true` if the page ends on a complete UTF-8 codepoint (no leading
    /// byte at the end without its required continuation bytes).
    pub ends_at_utf8_boundary: bool,

    /// `true` when this page starts with the `\n` that is the second half of
    /// a `\r\n` pair split across the boundary from the previous page.
    ///
    /// When set, the leading `\n` must **not** be counted as a new line
    /// ending (the `\r` on the previous page already completed the pair).
    pub starts_with_lf_of_crlf: bool,

    /// `true` when this page ends with a lone `\r` whose matching `\n` is
    /// the first byte of the next page.
    ///
    /// When set, the trailing `\r` must **not** yet be counted as a line
    /// ending (the pair will be completed when the next page is scanned).
    pub ends_with_cr_before_lf: bool,

    /// Scan state of this page.
    pub scan_state: ScanState,
}

impl PageDescriptor {
    /// Create an unscanned placeholder descriptor for `file_range`.
    ///
    /// All metrics are set to conservative defaults; callers must replace the
    /// descriptor with a fully scanned one before relying on any metric.
    pub fn placeholder(file_range: ByteRange) -> Self {
        let byte_len = file_range.len();
        PageDescriptor {
            decoded_range: file_range,
            file_range,
            byte_len,
            utf16_len: 0,
            newline_count: 0,
            first_line_prefix_len: byte_len,
            last_line_suffix_len: 0,
            starts_at_utf8_boundary: false,
            ends_at_utf8_boundary: false,
            starts_with_lf_of_crlf: false,
            ends_with_cr_before_lf: false,
            scan_state: ScanState::NotScanned,
        }
    }
}

// ---------------------------------------------------------------------------
// ScanProgress
// ---------------------------------------------------------------------------

/// How much of the file has been scanned so far.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScanProgress {
    /// Bytes covered by `Scanned` descriptors.
    pub scanned_bytes: u64,
    /// Total file size.
    pub total_bytes: u64,
}

impl ScanProgress {
    /// `true` when all file bytes have been scanned.
    pub fn is_complete(&self) -> bool {
        self.scanned_bytes >= self.total_bytes
    }

    /// Scanning progress as a fraction in `[0.0, 1.0]`.
    pub fn fraction(&self) -> f64 {
        if self.total_bytes == 0 {
            1.0
        } else {
            (self.scanned_bytes as f64) / (self.total_bytes as f64)
        }
    }
}

// ---------------------------------------------------------------------------
// PageIndex
// ---------------------------------------------------------------------------

/// Sparse index of [`PageDescriptor`]s for a VLF document.
///
/// Descriptors are keyed by the **start byte offset** of their
/// `file_range` and stored in a `BTreeMap` for O(log n) range lookups.
///
/// Background scan tasks insert or replace descriptors as pages are
/// processed; the `cancel_gen` counter lets callers abandon stale scan work.
pub struct PageIndex {
    /// Keyed by `file_range.start.0` for O(log n) range lookups.
    pub(crate) descriptors: BTreeMap<u64, PageDescriptor>,
    /// Cumulative newline prefixes, one entry per descriptor in key order,
    /// covering the contiguous scanned run from the file start: entry `i` is
    /// `(page_start, newlines before that page)`. An unscanned gap ends the
    /// run. Lets `find_page_for_line` / `byte_to_line_internal` answer in
    /// O(log pages) instead of walking every descriptor.
    lines_prefix: Vec<(u64, u64)>,
    /// Total `newline_count` across all scanned descriptors; O(1) line-count
    /// queries (mirrors `progress.scanned_bytes`).
    scanned_newlines_total: u64,
    progress: ScanProgress,
    cancel_gen: u64,
}

impl PageIndex {
    /// Create an empty index for a file of `total_bytes` size.
    pub fn new(total_bytes: u64) -> Self {
        PageIndex {
            descriptors: BTreeMap::new(),
            lines_prefix: Vec::new(),
            scanned_newlines_total: 0,
            progress: ScanProgress { scanned_bytes: 0, total_bytes },
            cancel_gen: 0,
        }
    }

    /// Insert or replace a descriptor, updating `scanned_bytes` when the
    /// descriptor's `scan_state` is `Scanned`.
    pub fn insert(&mut self, desc: PageDescriptor) {
        let key = desc.file_range.start.0;
        let old = self.descriptors.get(&key).cloned();
        let old_was_scanned = old.as_ref().is_some_and(|d| d.scan_state == ScanState::Scanned);
        if desc.scan_state == ScanState::Scanned {
            // Avoid double-counting if we are replacing an already-scanned page.
            let already_scanned = old_was_scanned;
            if !already_scanned {
                self.progress.scanned_bytes =
                    self.progress.scanned_bytes.saturating_add(desc.byte_len);
            }
            let delta = if already_scanned {
                desc.newline_count.saturating_sub(old.as_ref().map_or(0, |d| d.newline_count))
            } else {
                desc.newline_count
            };
            self.scanned_newlines_total = self.scanned_newlines_total.saturating_add(delta);
        } else if old_was_scanned {
            // Defensive: a scanned page demoted to a placeholder drops its
            // newlines from the totals (never happens in production scans).
            self.scanned_newlines_total = self
                .scanned_newlines_total
                .saturating_sub(old.as_ref().map_or(0, |d| d.newline_count));
        }
        self.descriptors.insert(key, desc);
        // The prefix shifts on every scan-state/line-count change; a rebuild
        // is O(run) and runs at most once per scanned page (2048 pages at
        // 1 MiB per 2 GiB file), so incremental bookkeeping is not worth the
        // correctness risk of interleaved viewport-first inserts.
        if old_was_scanned != (self.descriptors[&key].scan_state == ScanState::Scanned)
            || self.descriptors[&key].scan_state == ScanState::Scanned
        {
            self.rebuild_lines_prefix();
        }
    }

    /// Rebuild the cumulative prefix over the contiguous scanned run.
    fn rebuild_lines_prefix(&mut self) {
        self.lines_prefix.clear();
        let mut cum = 0u64;
        for d in self.descriptors.values() {
            if d.scan_state != ScanState::Scanned {
                break;
            }
            self.lines_prefix.push((d.file_range.start.0, cum));
            cum = cum.saturating_add(d.newline_count);
        }
    }

    /// Total newlines across all scanned descriptors (O(1)).
    pub fn scanned_newlines_total(&self) -> u64 {
        self.scanned_newlines_total
    }

    /// Newlines before `page_start` when that page is inside the contiguous
    /// scanned run (line numbering is exact there); `None` when the page is a
    /// placeholder or sits beyond an unscanned gap.
    pub fn cum_lines_before(&self, page_start: u64) -> Option<u64> {
        let idx = self.lines_prefix.partition_point(|&(start, _)| start < page_start);
        self.lines_prefix.get(idx).filter(|(start, _)| *start == page_start).map(|(_, cum)| *cum)
    }

    /// Return a reference to the descriptor for the page that contains
    /// `offset`, or `None` if no page covering `offset` has been inserted.
    ///
    /// Uses `BTreeMap::range` for O(log n) lookup: finds the greatest
    /// page-start ≤ `offset`, then checks that `offset < page.end`.
    pub fn page_at_byte(&self, offset: u64) -> Option<&PageDescriptor> {
        self.descriptors
            .range(..=offset)
            .next_back()
            .map(|(_, d)| d)
            .filter(|d| offset < d.file_range.end.0)
    }

    /// Map a logical line number to the byte offset of its first character,
    /// using accumulated line counts from scanned descriptors.
    ///
    /// Returns:
    /// - `Exact` — the page containing the line has been scanned; sub-page
    ///   resolution is left to the caller (see [`PageIndex::find_page_for_line`]).
    /// - `Pending` — a page that must be traversed to reach the target line
    ///   has not yet been scanned.
    /// - `OutOfRange` — the line is beyond the highest scanned line and the
    ///   full scan is complete.
    pub fn line_to_byte(&self, line: LogicalLine) -> LineLookup {
        match self.find_page_for_line(line.0) {
            Ok(loc) => LineLookup::Exact(loc.page.file_range.start),
            Err(lookup) => lookup,
        }
    }

    /// Find the page that contains `line` and the number of lines that come
    /// before that page.
    ///
    /// Returns `Ok(PageLineLocation)` on success or `Err(LineLookup)` with
    /// `Pending` / `OutOfRange` when the lookup cannot be resolved.
    pub fn find_page_for_line(&self, line: u64) -> Result<PageLineLocation<'_>, LineLookup> {
        let n = self.lines_prefix.len();
        if n == 0 {
            // Nothing scanned yet; the walk's per-page checks are vacuous.
            return if self.progress.is_complete() {
                Err(LineLookup::OutOfRange)
            } else {
                Err(LineLookup::Pending)
            };
        }
        // `page_end(i)` is the next entry's cum (the scanned total for the
        // last entry) and is non-decreasing, so the first page covering
        // `line` is the first i with `line <= page_end(i)`.
        let page_end = |i: usize| {
            if i + 1 < n { self.lines_prefix[i + 1].1 } else { self.scanned_newlines_total }
        };
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = (lo + hi) / 2;
            if page_end(mid) < line {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo < n {
            let (page_start, lines_before_page) = self.lines_prefix[lo];
            return Ok(PageLineLocation {
                page: &self.descriptors[&page_start],
                lines_before_page,
            });
        }
        // Line is beyond the scanned run: an unscanned gap follows when
        // another descriptor exists, otherwise the scan completion decides.
        if self.lines_prefix.len() < self.descriptors.len() {
            return Err(LineLookup::Pending);
        }
        if self.progress.is_complete() {
            Err(LineLookup::OutOfRange)
        } else {
            Err(LineLookup::Pending)
        }
    }

    /// Current scan progress.
    pub fn scan_progress(&self) -> ScanProgress {
        self.progress
    }

    /// Estimate the byte offset for `line` using linear interpolation over
    /// the scanned portion of the file.
    ///
    /// Returns `Some(ByteOffset)` when there is enough scan data to produce a
    /// meaningful estimate, or `None` when no pages have been scanned yet.
    ///
    /// The estimate assumes uniform line density across the file.  It is only
    /// a lower-bound approximation; callers should treat it as a navigation
    /// hint and re-resolve once the index catches up.
    pub fn approximate_byte_for_line(&self, line: u64) -> Option<crate::text_store::ByteOffset> {
        if self.progress.scanned_bytes == 0 || self.progress.total_bytes == 0 {
            return None;
        }
        let scanned_nl = self.scanned_newlines_total;
        if scanned_nl == 0 {
            // No newlines yet; can't interpolate meaningfully beyond byte 0.
            return Some(crate::text_store::ByteOffset(0));
        }
        // Extrapolate total line count from scanned density.
        let estimated_total_nl = (scanned_nl as f64 / self.progress.scanned_bytes as f64
            * self.progress.total_bytes as f64)
            .max(scanned_nl as f64) as u64;
        // Clamp to line 0 = byte 0; interpolate for higher lines.
        if line == 0 {
            return Some(crate::text_store::ByteOffset(0));
        }
        let frac = (line as f64) / (estimated_total_nl.saturating_add(1) as f64);
        let approx = (frac * self.progress.total_bytes as f64) as u64;
        Some(crate::text_store::ByteOffset(approx.min(self.progress.total_bytes)))
    }

    /// Bump the cancellation generation and return the new value.
    ///
    /// Background scan tasks should check [`cancel_gen`](Self::cancel_gen)
    /// periodically and stop when it no longer matches their snapshot.
    pub fn bump_cancel(&mut self) -> u64 {
        self.cancel_gen += 1;
        self.cancel_gen
    }

    /// Current cancellation generation.
    pub fn cancel_gen(&self) -> u64 {
        self.cancel_gen
    }

    /// Number of descriptors stored in the index.
    pub fn len(&self) -> usize {
        self.descriptors.len()
    }

    /// `true` when no descriptors are stored.
    pub fn is_empty(&self) -> bool {
        self.descriptors.is_empty()
    }
}

// ---------------------------------------------------------------------------
// PageLineLocation
// ---------------------------------------------------------------------------

/// Result returned by [`PageIndex::find_page_for_line`].
pub struct PageLineLocation<'a> {
    /// The scanned page that contains the requested line.
    pub page: &'a PageDescriptor,
    /// Number of logical lines that precede the start of this page.
    pub lines_before_page: u64,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text_store::ByteOffset;

    fn scanned_desc(start: u64, end: u64, newlines: u64) -> PageDescriptor {
        let fr = ByteRange::new(start, end);
        let byte_len = end - start;
        let first_nl = if newlines > 0 { byte_len / (newlines + 1) } else { byte_len };
        let last_nl = if newlines > 0 { byte_len - (byte_len / (newlines + 1)) } else { byte_len };
        PageDescriptor {
            file_range: fr,
            decoded_range: fr,
            byte_len,
            utf16_len: byte_len,
            newline_count: newlines,
            first_line_prefix_len: first_nl,
            last_line_suffix_len: last_nl,
            starts_at_utf8_boundary: true,
            ends_at_utf8_boundary: true,
            starts_with_lf_of_crlf: false,
            ends_with_cr_before_lf: false,
            scan_state: ScanState::Scanned,
        }
    }

    // ---- PageDescriptor -----------------------------------------------

    #[test]
    fn placeholder_has_not_scanned_state() {
        let d = PageDescriptor::placeholder(ByteRange::new(0, 100));
        assert_eq!(d.scan_state, ScanState::NotScanned);
        assert_eq!(d.byte_len, 100);
    }

    // ---- PageIndex::insert / len / is_empty ---------------------------

    #[test]
    fn empty_index() {
        let idx = PageIndex::new(1000);
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn insert_single_descriptor() {
        let mut idx = PageIndex::new(1000);
        idx.insert(scanned_desc(0, 100, 5));
        assert_eq!(idx.len(), 1);
        assert!(!idx.is_empty());
    }

    #[test]
    fn scan_progress_updated_on_scanned_insert() {
        let mut idx = PageIndex::new(1000);
        idx.insert(scanned_desc(0, 400, 10));
        assert_eq!(idx.scan_progress().scanned_bytes, 400);
    }

    #[test]
    fn scan_progress_not_double_counted_on_replace() {
        let mut idx = PageIndex::new(1000);
        idx.insert(scanned_desc(0, 400, 10));
        idx.insert(scanned_desc(0, 400, 12)); // replace same page
        assert_eq!(idx.scan_progress().scanned_bytes, 400, "must not double-count");
    }

    #[test]
    fn unscanned_placeholder_does_not_advance_progress() {
        let mut idx = PageIndex::new(1000);
        idx.insert(PageDescriptor::placeholder(ByteRange::new(0, 400)));
        assert_eq!(idx.scan_progress().scanned_bytes, 0);
    }

    // ---- page_at_byte -------------------------------------------------

    #[test]
    fn page_at_byte_within_first_page() {
        let mut idx = PageIndex::new(1000);
        idx.insert(scanned_desc(0, 500, 3));
        let d = idx.page_at_byte(250).unwrap();
        assert_eq!(d.file_range.start, ByteOffset(0));
    }

    #[test]
    fn page_at_byte_exact_start() {
        let mut idx = PageIndex::new(1000);
        idx.insert(scanned_desc(0, 500, 3));
        idx.insert(scanned_desc(500, 1000, 5));
        let d = idx.page_at_byte(500).unwrap();
        assert_eq!(d.file_range.start, ByteOffset(500));
    }

    #[test]
    fn page_at_byte_beyond_all_pages_returns_none() {
        let mut idx = PageIndex::new(1000);
        idx.insert(scanned_desc(0, 500, 3));
        assert!(idx.page_at_byte(600).is_none());
    }

    #[test]
    fn page_at_byte_exact_end_is_none() {
        // Range is half-open [start, end); offset == end is NOT in the page.
        let mut idx = PageIndex::new(500);
        idx.insert(scanned_desc(0, 500, 3));
        assert!(idx.page_at_byte(500).is_none());
    }

    // ---- line_to_byte -------------------------------------------------

    #[test]
    fn line_zero_maps_to_byte_zero() {
        let mut idx = PageIndex::new(100);
        // Page has 3 newlines: lines 0,1,2 are inside; line 0 starts at byte 0.
        idx.insert(scanned_desc(0, 100, 3));
        assert_eq!(idx.line_to_byte(LogicalLine(0)), LineLookup::Exact(ByteOffset(0)));
    }

    #[test]
    fn line_in_second_page_returns_page_start() {
        let mut idx = PageIndex::new(200);
        idx.insert(scanned_desc(0, 100, 3)); // covers lines 0-3
        idx.insert(scanned_desc(100, 200, 2)); // covers lines 4-6
        // Line 4 starts at the beginning of page 2 (byte 100).
        assert_eq!(idx.line_to_byte(LogicalLine(4)), LineLookup::Exact(ByteOffset(100)));
    }

    #[test]
    fn line_lookup_pending_when_unscanned_gap() {
        let mut idx = PageIndex::new(200);
        idx.insert(scanned_desc(0, 100, 3));
        idx.insert(PageDescriptor::placeholder(ByteRange::new(100, 200)));
        // Line 4 falls in the unscanned page → Pending.
        assert_eq!(idx.line_to_byte(LogicalLine(4)), LineLookup::Pending);
    }

    #[test]
    fn line_lookup_out_of_range_when_fully_scanned() {
        let mut idx = PageIndex::new(100);
        idx.insert(scanned_desc(0, 100, 3)); // lines 0-3
        // Line 10 is beyond; scan is complete (scanned_bytes == total_bytes).
        assert_eq!(idx.line_to_byte(LogicalLine(10)), LineLookup::OutOfRange);
    }

    #[test]
    fn line_lookup_pending_when_partially_scanned_and_line_beyond() {
        let mut idx = PageIndex::new(200);
        idx.insert(scanned_desc(0, 100, 3)); // only half the file scanned
        // Line 10 is beyond scanned; scan incomplete → Pending, not OutOfRange.
        assert_eq!(idx.line_to_byte(LogicalLine(10)), LineLookup::Pending);
    }

    // ---- cancel_gen ---------------------------------------------------

    #[test]
    fn bump_cancel_increments() {
        let mut idx = PageIndex::new(0);
        assert_eq!(idx.cancel_gen(), 0);
        assert_eq!(idx.bump_cancel(), 1);
        assert_eq!(idx.bump_cancel(), 2);
        assert_eq!(idx.cancel_gen(), 2);
    }

    // ---- scan_progress ------------------------------------------------

    #[test]
    fn scan_fraction_zero_for_empty_index() {
        let idx = PageIndex::new(1000);
        assert!((idx.scan_progress().fraction() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn scan_fraction_one_when_complete() {
        let mut idx = PageIndex::new(100);
        idx.insert(scanned_desc(0, 100, 0));
        assert!(idx.scan_progress().is_complete());
        assert!((idx.scan_progress().fraction() - 1.0).abs() < f64::EPSILON);
    }

    // ---- approximate_byte_for_line ------------------------------------

    #[test]
    fn approximate_byte_for_line_zero_returns_byte_zero() {
        let mut idx = PageIndex::new(1000);
        idx.insert(scanned_desc(0, 500, 10));
        assert_eq!(idx.approximate_byte_for_line(0), Some(ByteOffset(0)));
    }

    #[test]
    fn approximate_byte_for_line_none_when_no_scan_data() {
        let idx = PageIndex::new(1000);
        assert_eq!(idx.approximate_byte_for_line(5), None);
    }

    #[test]
    fn approximate_byte_for_line_midfile_estimate_in_range() {
        // 1000 bytes total; first 500 scanned with 10 newlines.
        // Estimated total newlines ≈ 20; line 10 ≈ 50% → ~500 bytes.
        let mut idx = PageIndex::new(1000);
        idx.insert(scanned_desc(0, 500, 10));
        let approx = idx.approximate_byte_for_line(10).unwrap();
        assert!(approx.0 <= 1000, "offset must not exceed file size");
        assert!(approx.0 > 0, "mid-file line offset must be > 0");
    }

    #[test]
    fn approximate_byte_for_line_clamped_to_file_size() {
        // Requesting a line far beyond estimated total should be clamped.
        let mut idx = PageIndex::new(100);
        idx.insert(scanned_desc(0, 100, 5));
        let approx = idx.approximate_byte_for_line(9999).unwrap();
        assert!(approx.0 <= 100);
    }

    // ---- cumulative prefix ---------------------------------------------

    #[test]
    fn prefix_matches_linear_walk_across_out_of_order_scans() {
        // Viewport-first order: pages 400/500 scan before page 100 arrives.
        let mut idx = PageIndex::new(2000);
        idx.insert(scanned_desc(400, 500, 4));
        idx.insert(scanned_desc(500, 600, 1));
        idx.insert(scanned_desc(100, 200, 3)); // backward pass
        // Answers must match the old linear walk: the run starts at the first
        // descriptor in key order.
        assert_eq!(idx.line_to_byte(LogicalLine(0)), LineLookup::Exact(ByteOffset(100)));
        assert_eq!(idx.line_to_byte(LogicalLine(3)), LineLookup::Exact(ByteOffset(100)));
        assert_eq!(idx.line_to_byte(LogicalLine(4)), LineLookup::Exact(ByteOffset(400)));
        assert_eq!(idx.line_to_byte(LogicalLine(7)), LineLookup::Exact(ByteOffset(400)));
        assert_eq!(idx.line_to_byte(LogicalLine(8)), LineLookup::Exact(ByteOffset(500)));
        // Beyond the scanned run, index still building → Pending.
        assert_eq!(idx.line_to_byte(LogicalLine(9)), LineLookup::Pending);
    }

    #[test]
    fn prefix_rescans_shift_cumulative_lines() {
        let mut idx = PageIndex::new(200);
        idx.insert(scanned_desc(0, 100, 3));
        idx.insert(scanned_desc(100, 200, 2));
        assert_eq!(idx.line_to_byte(LogicalLine(4)), LineLookup::Exact(ByteOffset(100)));
        // Re-scanning page 0 with a different line count shifts later cums.
        idx.insert(scanned_desc(0, 100, 5));
        assert_eq!(idx.line_to_byte(LogicalLine(4)), LineLookup::Exact(ByteOffset(0)));
        assert_eq!(idx.line_to_byte(LogicalLine(5)), LineLookup::Exact(ByteOffset(0)));
        assert_eq!(idx.line_to_byte(LogicalLine(6)), LineLookup::Exact(ByteOffset(100)));
        assert_eq!(idx.scanned_newlines_total(), 7);
    }

    #[test]
    fn zero_newline_pages_resolve_to_first_covering_page() {
        // Giant-line layout: pages 0..2 contain no newlines; page 3 has 4.
        let mut idx = PageIndex::new(400);
        idx.insert(scanned_desc(0, 100, 0));
        idx.insert(scanned_desc(100, 200, 0));
        idx.insert(scanned_desc(200, 300, 0));
        idx.insert(scanned_desc(300, 400, 4));
        assert_eq!(idx.line_to_byte(LogicalLine(0)), LineLookup::Exact(ByteOffset(0)));
        assert_eq!(idx.line_to_byte(LogicalLine(1)), LineLookup::Exact(ByteOffset(300)));
    }

    #[test]
    fn cum_lines_before_respects_scanned_run() {
        let mut idx = PageIndex::new(300);
        idx.insert(scanned_desc(0, 100, 3));
        idx.insert(PageDescriptor::placeholder(ByteRange::new(100, 200)));
        idx.insert(scanned_desc(200, 300, 5)); // scanned but beyond the gap
        assert_eq!(idx.cum_lines_before(0), Some(0));
        assert_eq!(idx.cum_lines_before(100), None); // placeholder
        assert_eq!(idx.cum_lines_before(200), None); // beyond the gap
    }

    #[test]
    fn scanned_newlines_total_tracks_placeholders_and_rescans() {
        let mut idx = PageIndex::new(300);
        assert_eq!(idx.scanned_newlines_total(), 0);
        idx.insert(scanned_desc(0, 100, 3));
        idx.insert(PageDescriptor::placeholder(ByteRange::new(100, 200)));
        assert_eq!(idx.scanned_newlines_total(), 3);
        idx.insert(scanned_desc(0, 100, 4)); // re-scan of page 0
        assert_eq!(idx.scanned_newlines_total(), 4);
    }
}
