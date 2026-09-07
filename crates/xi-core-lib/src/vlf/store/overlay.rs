//! `impl VlfStore` methods: overlay.
use super::*;

impl VlfStore {
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

    pub(super) fn visit_overlay_range<F>(
        &self,
        range: ByteRange,
        token: CancelGeneration,
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
        let chunk_cap = self.pager.max_read_size().max(1);
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
        self.visit_overlay_range(range, token, |_, chunk| {
            bytes.extend_from_slice(chunk);
            true
        })?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        Ok(TextChunk { text, byte_range: range })
    }

    pub(super) fn overlay_line_to_byte(&self, line: u64) -> LineLookup {
        if line == 0 {
            return LineLookup::Exact(ByteOffset(0));
        }
        let Some(total_len) = self.overlay_len_bytes() else {
            return LineLookup::Pending;
        };
        let token = self.pager.current_generation();
        let mut lines_seen = 0u64;
        let mut pending_cr = false;
        let mut found = None;
        let _ = self.visit_overlay_range(
            ByteRange::new(0, total_len),
            token,
            |logical_start, chunk| {
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
            return LineLookup::Exact(found);
        }
        if pending_cr {
            lines_seen = lines_seen.saturating_add(1);
            if lines_seen == line {
                return LineLookup::Exact(ByteOffset(total_len));
            }
        }
        LineLookup::OutOfRange
    }

    pub(super) fn overlay_byte_to_line(&self, offset: u64) -> Option<LogicalLine> {
        let total_len = self.overlay_len_bytes()?;
        if offset > total_len {
            return None;
        }
        let token = self.pager.current_generation();
        let mut lines_seen = 0u64;
        let mut pending_cr = false;
        let mut reached_end = offset == 0;
        let _ = self.visit_overlay_range(
            ByteRange::new(0, total_len),
            token,
            |logical_start, chunk| {
                if logical_start >= offset {
                    reached_end = true;
                    return false;
                }
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
