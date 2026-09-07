//! `impl VlfStore` methods: read.
use super::*;

impl VlfStore {
    pub(super) fn read_with_seam(&self, range: ByteRange) -> io::Result<SeamResult> {
        let token = self.pager.current_generation();
        self.read_with_seam_token(range, token)
    }

    pub(super) fn read_with_seam_token(
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

    pub(super) fn read_raw_range_token(
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
    pub(super) fn byte_to_line_internal(&self, offset: u64) -> Option<LogicalLine> {
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
    pub(super) fn line_to_byte_internal(&self, line: u64) -> LineLookup {
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
