//! `impl VlfStore` methods: scan + background indexing.
use super::*;

impl VlfStore {
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
    pub(super) fn drain_incoming(&self) {
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
}

pub(super) struct BackgroundScanner {
    pub(super) path: PathBuf,
    pub(super) page_size: u64,
    /// Shared with the owning `VlfStore`; set to `true` on drop.
    pub(super) cancel: Arc<AtomicBool>,
}

impl BackgroundScanner {
    pub(super) fn run(self, tx: mpsc::Sender<PageDescriptor>) {
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
