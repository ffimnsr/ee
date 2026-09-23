//! `impl VlfStore` methods: overlay.
use super::*;

/// Monotone line→byte cursor for the overlay's CRLF-aware line walk.
///
/// The original walk restarted at byte 0 for every lookup, making renders and
/// goto costing O(bytes-to-target) per line lookup — a 200-line window at the
/// middle of a large file took seconds. Renders query lines in ascending
/// order, so the cursor resumes from the previously resolved line start
/// (always a clean CRLF boundary: counting consumes `\r` state before the
/// next line begins) and amortizes to O(window).
#[derive(Clone, Copy)]
pub(super) struct OverlayLineCursor {
    /// Resolved logical line (1-based, matching the walk's counter).
    pub line: u64,
    /// Absolute byte offset of that line's start.
    pub byte: u64,
}

/// Line-ending state observed by overlay walks / page scans. Governs the
/// unedited fast path of [`overlay_line_to_byte`]: base-index answers are
/// identical under `\n`-only and merged `\r\n` counting (line starts sit
/// after the `\n` in both), but a lone `\r` line ending shifts overlay line
/// numbers relative to the base index, so files with lone `\r` must always
/// take the walk.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CrlfState {
    /// No evidence either way yet.
    Unknown,
    /// Only `\r\n` pairs seen (safe for the fast path).
    CrlfOnly,
    /// At least one lone `\r` seen (must walk).
    LoneCr,
}

impl VlfStore {
    /// Observe bytes for `\r` line-ending evidence. Pairs `\r` with the
    /// following byte when both are in `bytes`; a trailing `\r` (or a `\r`
    /// split across windows) stays unresolved and is judged by the next
    /// window or the walk's pending-CR tail.
    pub(super) fn mark_crlf_state(&self, bytes: &[u8]) {
        if self.crlf_state.get() == CrlfState::LoneCr || bytes.is_empty() {
            return;
        }
        let mut i = 0usize;
        let mut state = self.crlf_state.get();
        while i < bytes.len() {
            if bytes[i] != b'\r' {
                i += 1;
                continue;
            }
            let Some(&next) = bytes.get(i + 1) else {
                break; // trailing `\r`: judge on the next window
            };
            if next == b'\n' {
                state = CrlfState::CrlfOnly;
                i += 2;
            } else {
                state = CrlfState::LoneCr;
                break;
            }
        }
        self.crlf_state.set(state);
    }

    pub(super) fn overlay_read_enabled(&self) -> bool {
        self.overlay
            .borrow()
            .as_ref()
            .is_some_and(|overlay| overlay.edit_gate().read_byte_range_ready)
    }

    pub(super) fn overlay_len_bytes(&self) -> Option<u64> {
        let overlay = self.overlay.borrow();
        overlay.as_ref().map(PieceOverlay::total_byte_len)
    }

    /// True when the overlay content differs from the base file (inserts,
    /// deletes, or replaces). The unedited overlay can answer line lookups
    /// straight from the base index.
    fn overlay_has_edits(&self) -> bool {
        self.overlay.borrow().as_ref().is_some_and(|overlay| {
            overlay.signed_byte_delta() != 0
                || overlay
                    .pieces()
                    .iter()
                    .any(|piece| matches!(piece, crate::vlf::overlay::Piece::Inserted { .. }))
        })
    }

    pub(super) fn visit_overlay_range_slab<F>(
        &self,
        range: ByteRange,
        token: CancelGeneration,
        slab_cap: u64,
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
        let chunk_cap = slab_cap.min(self.pager.max_read_size()).max(1);
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

    pub(super) fn overlay_read_exact_range(
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
        // Bound the per-chunk slab to the requested range: line-interval
        // reads (the render hot path) must not pull pager-max slabs.
        self.visit_overlay_range_slab(range, token, range.len().max(1), |_, chunk| {
            bytes.extend_from_slice(chunk);
            true
        })?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        Ok(TextChunk { text, byte_range: range })
    }

    /// Scan the first 16 KiB of the file once, so a lone-`\r` line ending in
    /// the prefix never lets the unedited fast path serve wrong base-index
    /// answers (later regions are judged while forward walks pass them).
    fn sniff_prefix_crlf(&self) {
        let file_size = self.pager.file_size();
        if file_size == 0 {
            return;
        }
        let len = file_size.min(16 * 1024);
        if let Ok(pb) = self.pager.read_at(ByteRange::new(0, len), self.pager.current_generation())
        {
            self.mark_crlf_state(pb.as_bytes());
        }
    }

    pub(super) fn overlay_line_to_byte(&self, line: u64) -> LineLookup {
        if line == 0 {
            return LineLookup::Exact(ByteOffset(0));
        }
        // Unedited fast path: base index answers exactly (line starts are
        // identical under `\n`-only and merged `\r\n` counting; the overlay
        // only diverges once edits or lone-`\r` endings appear). This makes
        // mid-file jumps O(log pages) instead of O(bytes-to-line).
        if !self.overlay_has_edits() {
            // The background scanner feeds the page index through a channel;
            // overlay lookups must drain it or the fast path sees an empty
            // index and every lookup degrades to a full walk.
            self.drain_incoming();
            // A lone `\r` early in the file invalidates the fast path; sniff
            // the file prefix once before trusting base-index answers.
            if !self.crlf_prefix_sniffed.get() {
                self.sniff_prefix_crlf();
                self.crlf_prefix_sniffed.set(true);
            }
            if self.crlf_state.get() != CrlfState::LoneCr {
                match self.line_to_byte_internal(line) {
                    LineLookup::Exact(byte) => {
                        *self.line_cursor.borrow_mut() =
                            Some(OverlayLineCursor { line, byte: byte.0 });
                        return LineLookup::Exact(byte);
                    }
                    // Base answered OutOfRange only once the whole file is
                    // scanned; line numbering is identical in the unedited
                    // overlay, so reply without the O(remaining-file) walk.
                    LineLookup::OutOfRange => return LineLookup::OutOfRange,
                    // Pending/Approximate: fall through to the walk (index
                    // still building), matching the pre-optimization path.
                    _ => {}
                }
            }
        }
        let Some(total_len) = self.overlay_len_bytes() else {
            return LineLookup::Pending;
        };
        let token = self.pager.current_generation();
        let mut cursor = self.line_cursor.borrow_mut();
        // Exact hit: the cursor already resolved this line.
        if let Some(c) = *cursor {
            if c.line == line {
                return LineLookup::Exact(ByteOffset(c.byte));
            }
        }
        // Resume from the last resolved line start when the target is ahead
        // (the render pattern); otherwise restart at byte 0.
        let (start_byte, mut lines_seen) = match *cursor {
            Some(c) if c.line < line => (c.byte, c.line),
            _ => (0, 0),
        };
        let mut pending_cr = false;
        let mut found = None;
        // Line walks resolve within a few hundred bytes of the resume point;
        // a full pager-max slab (MBs) per lookup made windowed renders
        // O(max_read_size) per line. 16 KiB bounds the pager churn while
        // keeping giant-line walks linear in the line itself.
        let _ = self.visit_overlay_range_slab(
            ByteRange::new(start_byte, total_len),
            token,
            16 * 1024,
            |logical_start, chunk| {
                self.mark_crlf_state(chunk);
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
                            pending_cr = false;
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
            *cursor = Some(OverlayLineCursor { line, byte: found.0 });
            return LineLookup::Exact(found);
        }
        if pending_cr {
            // The document ends with a lone `\r`; line counting diverges from
            // the base index from here on.
            self.crlf_state.set(CrlfState::LoneCr);
            lines_seen = lines_seen.saturating_add(1);
            if lines_seen == line {
                let tail = ByteOffset(total_len);
                *cursor = Some(OverlayLineCursor { line, byte: tail.0 });
                return LineLookup::Exact(tail);
            }
        }
        LineLookup::OutOfRange
    }

    pub(super) fn overlay_byte_to_line(&self, offset: u64) -> Option<LogicalLine> {
        let total_len = self.overlay_len_bytes()?;
        if offset > total_len {
            return None;
        }
        // Unedited fast path (mirror of `overlay_line_to_byte`): base-index
        // counting is exact under `\n`-only and merged `\r\n`; lone-`\r`
        // files must walk.
        if !self.overlay_has_edits() {
            self.drain_incoming();
            if !self.crlf_prefix_sniffed.get() {
                self.sniff_prefix_crlf();
                self.crlf_prefix_sniffed.set(true);
            }
            if self.crlf_state.get() != CrlfState::LoneCr {
                if let Some(line) = self.byte_to_line_internal(offset) {
                    return Some(line);
                }
                // Index gaps fall through to the walk (partial counts).
            }
        }
        let token = self.pager.current_generation();
        let mut lines_seen = 0u64;
        let mut pending_cr = false;
        let mut reached_end = offset == 0;
        // Same 16 KiB walk slab as `overlay_line_to_byte`: byte-to-line for
        // scroll targets must not churn pager-max slabs before the offset.
        let _ = self.visit_overlay_range_slab(
            ByteRange::new(0, total_len),
            token,
            16 * 1024,
            |logical_start, chunk| {
                if logical_start >= offset {
                    reached_end = true;
                    return false;
                }
                self.mark_crlf_state(
                    &chunk[..chunk.len().min(
                        usize::try_from(offset.saturating_sub(logical_start))
                            .unwrap_or(chunk.len()),
                    )],
                );
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
}
