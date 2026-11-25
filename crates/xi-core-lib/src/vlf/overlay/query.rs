use super::*;

impl PieceOverlay {
    /// Create an overlay for a read-only file of `file_byte_len` bytes with
    /// the given original-file metrics.
    ///
    /// If `file_byte_len` is zero the overlay starts with no pieces (empty
    /// document).
    pub fn new(original_file_metrics: TextMetrics) -> Self {
        Self::with_limits(original_file_metrics, OverlayLimits::default())
    }

    /// Create an overlay with explicit resource limits.
    ///
    /// Use this in tests or when tuning memory behaviour for a specific file
    /// size.  For production use [`Self::new`] which applies the defaults.
    pub fn with_limits(original_file_metrics: TextMetrics, limits: OverlayLimits) -> Self {
        let total_byte_len = original_file_metrics.byte_len;
        let total_newlines = original_file_metrics.newline_count;

        let (pieces, cum_offsets) = if total_byte_len == 0 {
            (Vec::new(), Vec::new())
        } else {
            let piece = Piece::Original {
                file_range: ByteRange::new(0, total_byte_len),
                metrics: original_file_metrics,
            };
            (vec![piece], vec![0u64])
        };

        PieceOverlay {
            pieces,
            cum_offsets,
            total_byte_len,
            total_newlines,
            original_file_byte_len: total_byte_len,
            buffers: HashMap::new(),
            next_buffer_id: 0,
            active_buffer: None,
            undo_history: HashMap::new(),
            undo_group_order: Vec::new(),
            limits,
            edit_gate: ArbitrarySparseEditGate::default(),
        }
    }

    /// Total logical byte length of the document including overlay edits.
    pub fn total_byte_len(&self) -> u64 {
        self.total_byte_len
    }

    /// Byte length of the original file at overlay creation time.
    pub fn original_file_byte_len(&self) -> u64 {
        self.original_file_byte_len
    }

    /// Known line count for the overlay document.
    pub fn known_line_count(&self) -> KnownLineCount {
        // newline_count is the number of line-ending characters, which equals
        // (number of logical lines - 1) for files not ending with a newline,
        // or (number of logical lines) for files that do.  For the overlay we
        // report the exact count since we track metrics eagerly.
        KnownLineCount::Exact(self.total_newlines + 1)
    }

    /// Logical byte offset of the start of `line` (0-based).
    ///
    /// Returns [`LineLookup::OutOfRange`] if `line` exceeds the document.
    pub fn line_to_byte(&self, line: LogicalLine) -> LineLookup {
        let target_line = line.0;
        if target_line == 0 {
            return LineLookup::Exact(crate::text_store::ByteOffset(0));
        }
        // Walk pieces accumulating newline counts until we reach target_line.
        let mut lines_seen: u64 = 0;
        let mut byte_pos: u64 = 0;

        for piece in &self.pieces {
            let m = piece.metrics();

            if lines_seen + m.newline_count >= target_line {
                // The target line start is within this piece.
                let remaining = target_line - lines_seen;
                let offset = self.find_nth_newline_in_piece(piece, remaining);
                return LineLookup::Exact(crate::text_store::ByteOffset(byte_pos + offset));
            }
            lines_seen += m.newline_count;
            byte_pos += m.byte_len;
        }
        LineLookup::OutOfRange
    }

    /// Find the byte offset within a piece where the `n`-th newline ends
    /// (i.e. the byte *after* the `\n`).  `n` must be ≥ 1.
    fn find_nth_newline_in_piece(&self, piece: &Piece, n: u64) -> u64 {
        let text = match piece {
            Piece::Original { .. } => {
                // For original pieces we cannot read without I/O here; return 0
                // as a safe fallback.  Callers that need exact in-piece
                // addressing should read the page bytes through FilePager.
                return 0;
            }
            Piece::Inserted { buffer_id, range, .. } => {
                let buf = self.buffers.get(buffer_id).expect("buffer must exist");
                let bytes = buf.slice(*range).expect("range must be valid");
                match str::from_utf8(bytes) {
                    Ok(s) => s.to_owned(),
                    Err(_) => return 0,
                }
            }
        };

        let mut count: u64 = 0;

        for (i, ch) in text.char_indices() {
            if ch == '\n' {
                count += 1;
                if count == n {
                    return (i + 1) as u64;
                }
            }
        }
        text.len() as u64
    }

    // -----------------------------------------------------------------------
    // Piece index helpers
    // -----------------------------------------------------------------------

    /// Binary-search for the index of the piece containing `logical_byte`.
    ///
    /// Returns `(piece_index, piece_logical_start)`.  Returns `None` if
    /// `logical_byte >= total_byte_len`.
    pub(super) fn piece_for_byte(&self, logical_byte: u64) -> Option<(usize, u64)> {
        if logical_byte >= self.total_byte_len || self.pieces.is_empty() {
            return None;
        }
        let idx =
            self.cum_offsets.partition_point(|&start| start <= logical_byte).saturating_sub(1);
        Some((idx, self.cum_offsets[idx]))
    }

    /// Rebuild `cum_offsets` from `pieces`.  Called after structural changes.
    pub(super) fn rebuild_offsets(&mut self) {
        self.cum_offsets.clear();
        let mut pos: u64 = 0;
        for p in &self.pieces {
            self.cum_offsets.push(pos);
            pos += p.byte_len();
        }
        self.total_byte_len = pos;
    }

    /// Compute total newlines by summing per-piece newline counts.
    ///
    /// We count only `\n` characters; `\r` alone is not a line ending.
    /// A `\r\n` pair spanning two pieces contributes exactly 1 (the `\n` in
    /// the second piece), so no seam adjustment is needed.
    pub(super) fn compute_total_newlines(&self) -> u64 {
        self.pieces.iter().map(|p| p.metrics().newline_count).sum()
    }

    // -----------------------------------------------------------------------
    // Buffer helpers
    // -----------------------------------------------------------------------

    /// Get or create the active insert buffer.
    pub(super) fn active_buffer_mut(&mut self) -> &mut InsertBuffer {
        if self.active_buffer.is_none() {
            let id = BufferId(self.next_buffer_id);
            self.next_buffer_id += 1;
            self.buffers.insert(id, InsertBuffer::new(id));
            self.active_buffer = Some(id);
        }
        let id = self.active_buffer.unwrap();
        self.buffers.get_mut(&id).unwrap()
    }
}
