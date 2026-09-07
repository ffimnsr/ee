//! `impl VlfStore` methods: viewport.
use super::*;

impl VlfStore {
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
    pub(super) fn update_peak_stats(&self) {
        let raw = self.pager.metrics().cache_used_bytes;
        let decoded = self.decoded_cache.borrow().used_bytes();
        let desc_count = self.index.borrow().len() as u64;
        let descriptor_bytes =
            desc_count * std::mem::size_of::<super::super::page_index::PageDescriptor>() as u64;
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
    pub(super) fn record_pager_read(&self, byte_count: u64) {
        if !self.first_viewport_set.get() {
            self.stats.borrow_mut().bytes_before_first_viewport += byte_count;
        }
    }
}
