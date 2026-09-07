//! `impl TextStore for VlfStore`: text-store contract surface.
use super::*;

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
