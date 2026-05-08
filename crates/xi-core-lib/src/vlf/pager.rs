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

//! [`FilePager`]: file handle ownership, bounded positioned I/O, LRU page
//! cache, and cancellation-generation support.
//!
//! # Design
//!
//! `FilePager` owns the `File` handle and performs all I/O through
//! platform-native positioned reads (`pread` on Unix, `seek_read` on Windows)
//! so concurrent reads from multiple call sites can share the handle without
//! locking.
//!
//! The LRU byte cache evicts the least-recently-used pages once the
//! `cache_byte_cap` threshold is exceeded.  Cache entries are keyed by the
//! **start byte offset** of the read.
//!
//! `mmap` is intentionally excluded from the first milestone: `mmap` failure
//! modes (OOM-killer, SIGBUS on file truncation) differ significantly across
//! platforms.  It may be added later behind a feature flag.
//!
//! # Cancellation
//!
//! Each `FilePager` maintains an `AtomicU64` generation counter.  Callers
//! snapshot the counter via [`FilePager::current_generation`], pass the
//! snapshot to [`FilePager::read_at`], and receive
//! `Err(ErrorKind::Interrupted)` if [`FilePager::invalidate`] was called
//! (bumping the generation) before or during the read.  This lets stale
//! viewport reads be dropped without blocking the UI.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use crate::text_store::ByteRange;

// ---------------------------------------------------------------------------
// Public constants
// ---------------------------------------------------------------------------

/// Default maximum bytes allowed in a single `read_at` call (4 MiB).
pub const DEFAULT_MAX_READ_SIZE: u64 = 4 * 1024 * 1024;

/// Default page-cache byte capacity (64 MiB).
pub const DEFAULT_CACHE_BYTE_CAP: u64 = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------
// PageBytes
// ---------------------------------------------------------------------------

/// Raw bytes returned by a single [`FilePager::read_at`] call.
///
/// The inner `Arc<[u8]>` allows cheap cloning when the same page is needed by
/// multiple call sites without copying the underlying bytes.
#[derive(Clone, Debug)]
pub struct PageBytes(Arc<[u8]>);

impl PageBytes {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

// ---------------------------------------------------------------------------
// CancelGeneration
// ---------------------------------------------------------------------------

/// A snapshot of a cancellation generation counter.
///
/// Obtain via [`FilePager::current_generation`].  Pass to
/// [`FilePager::read_at`] so the pager can detect if [`FilePager::invalidate`]
/// was called between the snapshot and the read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CancelGeneration(pub u64);

// ---------------------------------------------------------------------------
// PagerMetrics
// ---------------------------------------------------------------------------

/// Snapshot of LRU cache statistics.
#[derive(Debug, Clone, Copy, Default)]
pub struct PagerMetrics {
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub cache_evictions: u64,
    pub cache_used_bytes: u64,
    pub cache_byte_cap: u64,
}

// ---------------------------------------------------------------------------
// PageCache (internal LRU)
// ---------------------------------------------------------------------------

/// LRU byte cache keyed by page-start byte offset.
///
/// Uses a `HashMap` for O(1) data access and a `BTreeMap<access_time,
/// page_start>` for O(log n) LRU eviction without external crates.
struct PageCache {
    /// page_start → (data, last_access_time)
    data: HashMap<u64, (PageBytes, u64)>,
    /// last_access_time → page_start (for LRU eviction)
    order: BTreeMap<u64, u64>,
    access_counter: u64,
    used_bytes: u64,
    byte_cap: u64,
    hits: u64,
    misses: u64,
    evictions: u64,
}

impl PageCache {
    fn new(byte_cap: u64) -> Self {
        PageCache {
            data: HashMap::new(),
            order: BTreeMap::new(),
            access_counter: 0,
            used_bytes: 0,
            byte_cap,
            hits: 0,
            misses: 0,
            evictions: 0,
        }
    }

    fn get(&mut self, page_start: u64) -> Option<PageBytes> {
        let (page, old_time) = self.data.get_mut(&page_start)?;
        // Update LRU order.
        self.order.remove(old_time);
        self.access_counter += 1;
        let new_time = self.access_counter;
        *old_time = new_time;
        self.order.insert(new_time, page_start);
        self.hits += 1;
        Some(page.clone())
    }

    fn put(&mut self, page_start: u64, bytes: PageBytes) {
        // Remove existing entry first to avoid double-counting bytes.
        if let Some((old, old_time)) = self.data.remove(&page_start) {
            self.used_bytes = self.used_bytes.saturating_sub(old.len() as u64);
            self.order.remove(&old_time);
        }
        let needed = bytes.len() as u64;
        // Evict LRU pages until there is room.
        while !self.data.is_empty() && self.used_bytes + needed > self.byte_cap {
            // BTreeMap::iter() yields keys in ascending order; the smallest
            // key is the least recently used.
            if let Some((&oldest_time, &oldest_start)) = self.order.iter().next() {
                self.order.remove(&oldest_time);
                if let Some((evicted, _)) = self.data.remove(&oldest_start) {
                    self.used_bytes = self.used_bytes.saturating_sub(evicted.len() as u64);
                    self.evictions += 1;
                }
            }
        }
        self.access_counter += 1;
        let time = self.access_counter;
        self.used_bytes += needed;
        self.data.insert(page_start, (bytes, time));
        self.order.insert(time, page_start);
        self.misses += 1; // counts a put as a cache miss (caller had to read)
    }

    fn metrics(&self) -> PagerMetrics {
        PagerMetrics {
            cache_hits: self.hits,
            // misses were recorded in put(); don't double-count from the get() path.
            cache_misses: self.misses,
            cache_evictions: self.evictions,
            cache_used_bytes: self.used_bytes,
            cache_byte_cap: self.byte_cap,
        }
    }
}

// ---------------------------------------------------------------------------
// FilePager
// ---------------------------------------------------------------------------

/// Owns a `File` handle for a VLF document and provides bounded positioned
/// reads with LRU caching and cancellation support.
///
/// All methods that perform I/O take `&self` (interior mutability via
/// [`RefCell`] for the cache) so they are compatible with
/// [`crate::text_store::TextStore`]'s `&self` requirement.
pub struct FilePager {
    file: File,
    canonical_path: PathBuf,
    /// File size recorded when the pager was opened.  The file is treated as
    /// immutable after opening (read-only milestone).
    file_size: u64,
    /// Modification time recorded on open; used for staleness detection.
    modified: Option<SystemTime>,
    /// Hard upper bound on bytes read in a single `read_at` call.
    max_read_size: u64,
    /// Monotonically increasing generation; bumped by `invalidate()`.
    cancel_gen: Arc<AtomicU64>,
    /// Interior-mutable LRU cache.
    cache: RefCell<PageCache>,
}

impl FilePager {
    /// Open `path` with the default cache capacity and max read size.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::open_with_config(path, DEFAULT_CACHE_BYTE_CAP, DEFAULT_MAX_READ_SIZE)
    }

    /// Open `path` with explicit `cache_byte_cap` and `max_read_size`.
    pub fn open_with_config(
        path: impl AsRef<Path>,
        cache_byte_cap: u64,
        max_read_size: u64,
    ) -> io::Result<Self> {
        let canonical = path.as_ref().canonicalize()?;
        let file = File::open(&canonical)?;
        let meta = file.metadata()?;
        Ok(FilePager {
            file_size: meta.len(),
            modified: meta.modified().ok(),
            file,
            canonical_path: canonical,
            max_read_size,
            cancel_gen: Arc::new(AtomicU64::new(0)),
            cache: RefCell::new(PageCache::new(cache_byte_cap)),
        })
    }

    /// Canonical path of the opened file.
    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    /// File size as recorded on open.
    pub fn file_size(&self) -> u64 {
        self.file_size
    }

    /// Modification time as recorded on open.
    pub fn modified(&self) -> Option<SystemTime> {
        self.modified
    }

    /// Snapshot the current cancellation generation.
    ///
    /// Pass the result to [`read_at`](Self::read_at).  If
    /// [`invalidate`](Self::invalidate) is called before the read completes,
    /// `read_at` returns `Err(ErrorKind::Interrupted)`.
    pub fn current_generation(&self) -> CancelGeneration {
        CancelGeneration(self.cancel_gen.load(Ordering::Acquire))
    }

    /// Bump the cancellation generation, invalidating all in-flight reads
    /// that hold an older `CancelGeneration`.
    ///
    /// Returns the new generation.
    pub fn invalidate(&self) -> CancelGeneration {
        let next = self.cancel_gen.fetch_add(1, Ordering::AcqRel) + 1;
        CancelGeneration(next)
    }

    /// Read raw bytes for `range` from the file (or cache).
    ///
    /// # Errors
    ///
    /// - `ErrorKind::Interrupted` — `token` is stale (generation bumped).
    /// - `ErrorKind::InvalidInput` — `range.len() > max_read_size` or
    ///   `range.end > file_size`.
    pub fn read_at(&self, range: ByteRange, token: CancelGeneration) -> io::Result<PageBytes> {
        // Stale-read guard.
        self.check_generation(token)?;

        let start = range.start.0;
        let end = range.end.0;
        let len = end.saturating_sub(start);

        if len == 0 {
            return Ok(PageBytes(Arc::from(&[] as &[u8])));
        }
        if len > self.max_read_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("read len {} exceeds max_read_size {}", len, self.max_read_size),
            ));
        }
        if end > self.file_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("read end {} exceeds file_size {}", end, self.file_size),
            ));
        }

        // Cache hit?
        if let Some(cached) = self.cache.borrow_mut().get(start) {
            return Ok(cached);
        }

        // Perform positioned I/O.
        let raw = pread_exact(&self.file, start, len as usize)?;

        // Post-I/O stale check before inserting into cache.
        self.check_generation(token)?;

        let page = PageBytes(Arc::from(raw.as_slice()));
        self.cache.borrow_mut().put(start, page.clone());
        Ok(page)
    }

    /// Acquire a shared advisory lock on the file.
    ///
    /// Advisory locks are best-effort and may be no-ops on some file systems.
    /// The lock is released by [`unlock_advisory`](Self::unlock_advisory) or
    /// when the `FilePager` is dropped (OS-level).
    #[allow(clippy::incompatible_msrv)] // fs2 crate method, not std 1.89
    pub fn lock_advisory_shared(&self) -> io::Result<()> {
        #[allow(unused_imports)]
        use fs2::FileExt;
        self.file.lock_shared()
    }

    /// Release the advisory lock acquired by
    /// [`lock_advisory_shared`](Self::lock_advisory_shared).
    #[allow(clippy::incompatible_msrv)] // fs2 crate method, not std 1.89
    pub fn unlock_advisory(&self) -> io::Result<()> {
        #[allow(unused_imports)]
        use fs2::FileExt;
        self.file.unlock()
    }

    /// Snapshot of LRU cache statistics.
    pub fn metrics(&self) -> PagerMetrics {
        self.cache.borrow().metrics()
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    fn check_generation(&self, token: CancelGeneration) -> io::Result<()> {
        if token.0 != self.cancel_gen.load(Ordering::Acquire) {
            Err(io::Error::new(io::ErrorKind::Interrupted, "read cancelled: stale generation"))
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Platform-specific positioned I/O
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn pread_exact(file: &File, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    use std::os::unix::fs::FileExt;
    let mut buf = vec![0u8; len];
    let n = file.read_at(&mut buf, offset)?;
    buf.truncate(n);
    Ok(buf)
}

#[cfg(windows)]
fn pread_exact(file: &File, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    use std::os::windows::fs::FileExt;
    let mut buf = vec![0u8; len];
    let n = file.seek_read(&mut buf, offset)?;
    buf.truncate(n);
    Ok(buf)
}

#[cfg(not(any(unix, windows)))]
fn pread_exact(_file: &File, _offset: u64, _len: usize) -> io::Result<Vec<u8>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "positioned I/O (pread) is not supported on this platform",
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;
    use crate::text_store::ByteOffset;

    fn make_pager(content: &[u8]) -> (FilePager, NamedTempFile) {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();
        let pager =
            FilePager::open_with_config(f.path(), 1024 * 1024, DEFAULT_MAX_READ_SIZE).unwrap();
        (pager, f)
    }

    #[test]
    fn open_records_file_size() {
        let content = b"hello world";
        let (pager, _f) = make_pager(content);
        assert_eq!(pager.file_size(), content.len() as u64);
    }

    #[test]
    fn read_full_content() {
        let content = b"hello world";
        let (pager, _f) = make_pager(content);
        let token = pager.current_generation();
        let range = ByteRange::new(0, content.len() as u64);
        let bytes = pager.read_at(range, token).unwrap();
        assert_eq!(bytes.as_bytes(), content);
    }

    #[test]
    fn read_partial_range() {
        let content = b"hello world";
        let (pager, _f) = make_pager(content);
        let token = pager.current_generation();
        let bytes = pager.read_at(ByteRange::new(6, 11), token).unwrap();
        assert_eq!(bytes.as_bytes(), b"world");
    }

    #[test]
    fn read_empty_range_returns_empty() {
        let content = b"hello";
        let (pager, _f) = make_pager(content);
        let token = pager.current_generation();
        let bytes = pager.read_at(ByteRange::new(2, 2), token).unwrap();
        assert!(bytes.is_empty());
    }

    #[test]
    fn read_beyond_file_returns_error() {
        let content = b"hi";
        let (pager, _f) = make_pager(content);
        let token = pager.current_generation();
        let err = pager.read_at(ByteRange::new(0, 99), token).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn stale_generation_returns_interrupted() {
        let content = b"hello";
        let (pager, _f) = make_pager(content);
        let token = pager.current_generation();
        pager.invalidate();
        let err = pager.read_at(ByteRange::new(0, 5), token).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn fresh_generation_after_invalidate_succeeds() {
        let content = b"hello";
        let (pager, _f) = make_pager(content);
        pager.invalidate();
        let token = pager.current_generation();
        let bytes = pager.read_at(ByteRange::new(0, 5), token).unwrap();
        assert_eq!(bytes.as_bytes(), b"hello");
    }

    #[test]
    fn cache_hit_recorded_in_metrics() {
        let content = b"hello world";
        let (pager, _f) = make_pager(content);
        let token = pager.current_generation();
        let range = ByteRange::new(0, content.len() as u64);
        // First read → miss (put into cache).
        pager.read_at(range, token).unwrap();
        // Second read → hit.
        pager.read_at(range, token).unwrap();
        let m = pager.metrics();
        assert!(m.cache_hits >= 1);
    }

    #[test]
    fn cache_eviction_under_byte_cap() {
        let content = vec![b'x'; 100];
        let (pager, _f) = make_pager(&content);
        // Cache cap of 60 bytes; two reads of 40 bytes each should cause eviction.
        let pager =
            FilePager::open_with_config(pager.canonical_path(), 60, DEFAULT_MAX_READ_SIZE).unwrap();
        let token = pager.current_generation();
        pager.read_at(ByteRange::new(0, 40), token).unwrap();
        pager.read_at(ByteRange::new(40, 80), token).unwrap();
        let m = pager.metrics();
        assert!(m.cache_evictions >= 1);
        assert!(m.cache_used_bytes <= 60);
    }

    #[test]
    fn max_read_size_enforced() {
        let content = vec![b'y'; 200];
        let (pager, _f) = make_pager(&content);
        let pager = FilePager::open_with_config(pager.canonical_path(), 1024, 50).unwrap();
        let token = pager.current_generation();
        let err = pager.read_at(ByteRange::new(0, 100), token).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn multibyte_content_read_intact() {
        let content = "café日本語".as_bytes().to_vec();
        let (pager, _f) = make_pager(&content);
        let token = pager.current_generation();
        let bytes = pager.read_at(ByteRange::new(0, content.len() as u64), token).unwrap();
        assert_eq!(bytes.as_bytes(), content.as_slice());
    }

    #[test]
    fn canonical_path_accessible() {
        let (pager, f) = make_pager(b"test");
        let canonical = f.path().canonicalize().unwrap();
        assert_eq!(pager.canonical_path(), canonical.as_path());
    }

    #[test]
    fn current_generation_starts_at_zero() {
        let (pager, _f) = make_pager(b"x");
        assert_eq!(pager.current_generation(), CancelGeneration(0));
    }

    #[test]
    fn invalidate_increments_generation() {
        let (pager, _f) = make_pager(b"x");
        let g1 = pager.invalidate();
        let g2 = pager.invalidate();
        assert_eq!(g1.0 + 1, g2.0);
    }

    #[test]
    fn byte_offset_zero_read() {
        let (pager, _f) = make_pager(b"abcdef");
        let token = pager.current_generation();
        let b =
            pager.read_at(ByteRange { start: ByteOffset(0), end: ByteOffset(1) }, token).unwrap();
        assert_eq!(b.as_bytes(), b"a");
    }
}
