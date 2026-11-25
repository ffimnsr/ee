use super::*;

impl PieceOverlay {
    // -----------------------------------------------------------------------
    // Edit operations
    // -----------------------------------------------------------------------

    /// Insert `text` at logical byte offset `at`.
    ///
    /// - `at == total_byte_len`: append to end of document.
    /// - `at < total_byte_len`: split the piece at `at`, insert new piece.
    ///
    /// Returns `Err` if `at > total_byte_len`.
    ///
    /// The text must be valid UTF-8 (enforced by the `&str` parameter type).
    pub fn insert(&mut self, at: u64, text: &str) -> Result<(), OverlayError> {
        if at > self.total_byte_len {
            return Err(OverlayError::OutOfRange { offset: at, len: self.total_byte_len });
        }
        if text.is_empty() {
            return Ok(());
        }

        // Determine CRLF context for metrics.
        let prev_ends_with_cr = self.char_before(at) == Some(b'\r');
        let metrics = TextMetrics::from_bytes(text.as_bytes(), prev_ends_with_cr);

        // Append to the active buffer.
        let range = self.active_buffer_mut().append(text);
        let buffer_id = self.active_buffer.unwrap();

        let new_piece_byte_len = metrics.byte_len;
        let new_piece = Piece::Inserted { buffer_id, range, metrics };

        if at == self.total_byte_len {
            // Fast path: append — update totals directly without rebuild_offsets.
            self.cum_offsets.push(self.total_byte_len);
            self.total_byte_len += new_piece_byte_len;
            self.pieces.push(new_piece);
            self.total_newlines = self.compute_total_newlines();
        } else {
            // Split piece at `at` and insert new piece between halves.
            // rebuild_offsets() sets total_byte_len from the piece list.
            let (idx, piece_start) = self.piece_for_byte(at).unwrap();
            let offset_in_piece = at - piece_start;

            if offset_in_piece == 0 {
                // Insert before this piece; rebuild_offsets fixes everything.
                self.pieces.insert(idx, new_piece);
            } else {
                // Split piece at offset_in_piece.
                let (lo, hi) = self.split_piece(idx, offset_in_piece)?;
                self.pieces.remove(idx);
                self.pieces.insert(idx, hi);
                self.pieces.insert(idx, new_piece);
                self.pieces.insert(idx, lo);
            }
            self.rebuild_offsets();
            self.total_newlines = self.compute_total_newlines();
        }

        Ok(())
    }

    /// Delete bytes `[start, end)` from the logical document.
    ///
    /// Returns `Err` if the range is out of bounds.
    pub fn delete(&mut self, range: ByteRange) -> Result<(), OverlayError> {
        let start = range.start.0;
        let end = range.end.0;
        if end > self.total_byte_len {
            return Err(OverlayError::OutOfRange { offset: end, len: self.total_byte_len });
        }
        if start >= end {
            return Ok(());
        }

        // Split at `end` first (so `start` index stays valid).
        if end < self.total_byte_len {
            let (idx, piece_start) = self.piece_for_byte(end).unwrap();
            let offset = end - piece_start;
            if offset > 0 && offset < self.pieces[idx].byte_len() {
                let (lo, hi) = self.split_piece(idx, offset)?;
                self.pieces.remove(idx);
                self.pieces.insert(idx, hi);
                self.pieces.insert(idx, lo);
                self.rebuild_offsets();
            }
        }

        // Split at `start`.
        if start > 0 {
            let (idx, piece_start) = self.piece_for_byte(start).unwrap();
            let offset = start - piece_start;
            if offset > 0 && offset < self.pieces[idx].byte_len() {
                let (lo, hi) = self.split_piece(idx, offset)?;
                self.pieces.remove(idx);
                self.pieces.insert(idx, hi);
                self.pieces.insert(idx, lo);
                self.rebuild_offsets();
            }
        }

        // Remove all pieces fully within [start, end).
        // After splits the pieces at `start` and `end` are exact boundaries.
        let first_idx = self.cum_offsets.partition_point(|&s| s < start);
        let last_idx = self.cum_offsets.partition_point(|&s| s < end);
        self.pieces.drain(first_idx..last_idx);
        self.rebuild_offsets();

        Ok(())
    }

    /// Split piece at `idx` into `(before, after)` at `offset_in_piece` bytes.
    ///
    /// Returns both halves without modifying `self.pieces`.  The caller is
    /// responsible for splicing them back.
    fn split_piece(
        &self,
        idx: usize,
        offset_in_piece: u64,
    ) -> Result<(Piece, Piece), OverlayError> {
        match &self.pieces[idx] {
            Piece::Original { file_range, .. } => {
                let mid = file_range.start.0 + offset_in_piece;
                let lo_range = ByteRange::new(file_range.start.0, mid);
                let hi_range = ByteRange::new(mid, file_range.end.0);
                // Metrics for split Original pieces are left as defaults and
                // should be recomputed from the file bytes on demand.  For
                // overlay accounting purposes we distribute newline_count
                // proportionally (approximate); exact metrics require I/O.
                let orig_m = self.pieces[idx].metrics();
                let lo_m = TextMetrics {
                    byte_len: lo_range.len(),
                    // Approximate: will be corrected on next scan.
                    utf16_len: (orig_m.utf16_len as f64
                        * (lo_range.len() as f64 / orig_m.byte_len as f64))
                        as u64,
                    newline_count: 0, // recomputed on scan
                    ends_with_cr: false,
                    starts_with_lf: orig_m.starts_with_lf,
                };
                let hi_m = TextMetrics {
                    byte_len: hi_range.len(),
                    utf16_len: orig_m.utf16_len.saturating_sub(lo_m.utf16_len),
                    newline_count: orig_m.newline_count, // recomputed on scan
                    ends_with_cr: orig_m.ends_with_cr,
                    starts_with_lf: false,
                };
                Ok((
                    Piece::Original { file_range: lo_range, metrics: lo_m },
                    Piece::Original { file_range: hi_range, metrics: hi_m },
                ))
            }
            Piece::Inserted { buffer_id, range, .. } => {
                let mid = range.start + offset_in_piece;
                let lo_range = InsertRange { start: range.start, end: mid };
                let hi_range = InsertRange { start: mid, end: range.end };
                let buf = self.buffers.get(buffer_id).expect("buffer must exist");
                let lo_bytes = buf.slice(lo_range).expect("lo_range valid");
                let hi_bytes = buf.slice(hi_range).expect("hi_range valid");
                let lo_m = TextMetrics::from_bytes(lo_bytes, false);
                let hi_m = TextMetrics::from_bytes(hi_bytes, lo_m.ends_with_cr);
                Ok((
                    Piece::Inserted { buffer_id: *buffer_id, range: lo_range, metrics: lo_m },
                    Piece::Inserted { buffer_id: *buffer_id, range: hi_range, metrics: hi_m },
                ))
            }
        }
    }

    /// Return the byte immediately before `logical_offset`, if any.
    fn char_before(&self, logical_offset: u64) -> Option<u8> {
        if logical_offset == 0 {
            return None;
        }
        let (idx, piece_start) = self.piece_for_byte(logical_offset - 1)?;
        let offset_in_piece = (logical_offset - 1) - piece_start;
        match &self.pieces[idx] {
            Piece::Original { .. } => None, // would need I/O; caller tolerates None
            Piece::Inserted { buffer_id, range, .. } => {
                let buf = self.buffers.get(buffer_id)?;
                let abs = range.start + offset_in_piece;
                buf.bytes.get(abs as usize).copied()
            }
        }
    }
}
