use super::*;

use super::types::LARGE_REWRITE_THRESHOLD_BYTES;

impl PieceOverlay {
    /// Number of pieces in the overlay (for testing / diagnostics).
    pub fn piece_count(&self) -> usize {
        self.pieces.len()
    }

    /// Borrow the ordered piece list (for testing / diagnostics).
    pub fn pieces(&self) -> &[Piece] {
        &self.pieces
    }

    /// Retrieve the bytes for an `Inserted` piece from the insert buffer.
    ///
    /// Returns `None` if the piece is `Original` (bytes live in the file, not
    /// the overlay) or if the buffer/range is missing (should not happen for
    /// well-formed overlays).
    pub fn inserted_bytes_for_piece<'a>(&'a self, piece: &Piece) -> Option<&'a [u8]> {
        match piece {
            Piece::Inserted { buffer_id, range, .. } => self.buffers.get(buffer_id)?.slice(*range),
            Piece::Original { .. } => None,
        }
    }

    /// Clone bounded save inputs for background VLF save execution.
    pub fn save_snapshot(&self) -> OverlaySaveSnapshot {
        OverlaySaveSnapshot {
            pieces: self.pieces.clone(),
            inserted_buffers: self
                .buffers
                .iter()
                .map(|(buffer_id, buffer)| (*buffer_id, buffer.bytes.clone()))
                .collect(),
            total_bytes: self.total_byte_len,
            original_file_byte_len: self.original_file_byte_len,
        }
    }

    /// Byte length of the named insert buffer.  Returns 0 if not found.
    pub fn buffer_len(&self, id: BufferId) -> u64 {
        self.buffers.get(&id).map_or(0, |b| b.len())
    }

    /// Approximate overlay memory usage in bytes.
    pub fn overlay_bytes(&self) -> u64 {
        let piece_bytes = (self.pieces.len() * std::mem::size_of::<Piece>()) as u64;
        let buffer_bytes: u64 = self.buffers.values().map(|b| b.len()).sum();
        piece_bytes + buffer_bytes
    }

    /// Borrow the resource limits that govern this overlay.
    pub fn limits(&self) -> &OverlayLimits {
        &self.limits
    }

    /// Signed byte-length delta relative to the original file.
    ///
    /// A positive value means the edited document is larger than the file on
    /// disk; negative means it is smaller.  Zero means the byte count is
    /// identical to the original (though content may still differ).
    ///
    /// Callers use this to decide whether an optimized in-place save, a
    /// temp-file streaming rewrite, or an explicit save-as is required.  See [`VlfSavePolicy`] and
    /// [`Self::suggested_save_policy`].
    pub fn signed_byte_delta(&self) -> i64 {
        self.total_byte_len as i64 - self.original_file_byte_len as i64
    }

    /// Net byte-length delta contributed by overlay edits that intersect
    /// `range` (logical document bytes).
    ///
    /// Returns the sum of inserted bytes minus deleted original bytes within
    /// the range.  Only `Inserted` pieces within `[range.start, range.end)` and
    /// `Original` pieces that no longer exist (deleted) contribute to the delta.
    ///
    /// This is an approximation: deleted original bytes within `range` are
    /// counted by subtracting the byte length of `Original` pieces that have
    /// been split away or removed.  Inserted pieces that fully overlap `range`
    /// are counted in full; partially overlapping pieces are clamped.
    pub fn dirty_byte_delta_for_range(&self, range: ByteRange) -> i64 {
        let range_start = range.start.0;
        let range_end = range.end.0;

        // Count inserted bytes within [range_start, range_end).
        let mut inserted: i64 = 0;
        let mut original_present: i64 = 0;

        let mut doc_pos: u64 = 0;
        for piece in &self.pieces {
            let piece_end = doc_pos + piece.byte_len();
            // Skip pieces that don't overlap the query range.
            if piece_end <= range_start || doc_pos >= range_end {
                doc_pos = piece_end;
                continue;
            }
            let overlap_start = doc_pos.max(range_start);
            let overlap_end = piece_end.min(range_end);
            let overlap_len = overlap_end.saturating_sub(overlap_start) as i64;
            match piece {
                Piece::Inserted { .. } => inserted += overlap_len,
                Piece::Original { .. } => original_present += overlap_len,
            }
            doc_pos = piece_end;
        }

        // Original bytes in [range_start, range_end) that are NOT present in
        // any Original piece are deleted bytes.  We estimate original bytes that
        // *should* be in range as `(range_end - range_start)` minus the
        // original_present counted above.  This is an approximation because the
        // range may include inserted bytes that displace original offsets.
        let range_len = range_end.saturating_sub(range_start) as i64;
        let deleted_original = (range_len - original_present).max(0);
        inserted - deleted_original
    }

    /// Suggested save policy for the current overlay state.
    ///
    /// Returns the narrowest save strategy that fits the current overlay.  Returns
    /// [`VlfSavePolicy::SaveAs`] placeholder when the file is above
    /// `LARGE_REWRITE_THRESHOLD_BYTES` **and** the signed byte delta exceeds
    /// 10 % of the original file size, indicating the caller should confirm an
    /// explicit save destination.  Same-size edits prefer
    /// [`VlfSavePolicy::SameSizeInPlaceOverwrite`]; byte-length-changing edits
    /// prefer [`VlfSavePolicy::TailShift`] with temp-file fallback.
    ///
    /// The returned `SaveAs` path is a placeholder (`<save-as-required>`); the
    /// actual destination must be supplied by the caller after prompting the
    /// user.
    pub fn suggested_save_policy(&self) -> VlfSavePolicy {
        let delta = self.signed_byte_delta().unsigned_abs();
        let is_large = self.original_file_byte_len >= LARGE_REWRITE_THRESHOLD_BYTES;
        let delta_fraction = if self.original_file_byte_len > 0 {
            delta as f64 / self.original_file_byte_len as f64
        } else {
            0.0
        };

        if is_large && delta_fraction > 0.10 {
            // Large file with >10 % byte change: require explicit save-as.
            VlfSavePolicy::SaveAs(PathBuf::from("<save-as-required>"))
        } else if self.signed_byte_delta() == 0 {
            VlfSavePolicy::SameSizeInPlaceOverwrite
        } else {
            VlfSavePolicy::TailShift { fallback_temp_dir: None }
        }
    }

    // -----------------------------------------------------------------------
    // Limit enforcement
    // -----------------------------------------------------------------------

    /// Enforce [`OverlayLimits`] before applying an edit.
    ///
    /// GCs the oldest undo group until either the undo group count is within
    /// `max_undo_groups` or the overlay byte usage is within `overlay_byte_cap`.
    /// Returns `Err(LimitReached)` only when GC cannot bring byte usage below
    /// the cap (i.e. the *current* overlay without any undo history already
    /// exceeds the cap).
    pub(super) fn enforce_limits(&mut self, incoming_bytes: u64) -> Result<(), OverlayError> {
        // GC oldest undo groups until count is within the limit.
        while self.undo_history.len() >= self.limits.max_undo_groups {
            if let Some(oldest) = self.undo_group_order.first().copied() {
                self.undo_group_order.remove(0);
                self.gc_undo_group(oldest);
            } else {
                break;
            }
        }

        // GC oldest undo groups until byte usage would fit within the cap.
        while self.overlay_bytes() + incoming_bytes > self.limits.overlay_byte_cap {
            if let Some(oldest) = self.undo_group_order.first().copied() {
                self.undo_group_order.remove(0);
                self.gc_undo_group(oldest);
            } else {
                // No more undo groups to free; reject the edit.
                return Err(OverlayError::LimitReached {
                    kind: LimitKind::MemoryCap {
                        cap: self.limits.overlay_byte_cap,
                        used: self.overlay_bytes(),
                    },
                });
            }
        }

        Ok(())
    }

    /// Insert `text` at logical byte offset `at`, recording the operation in
    /// `ctx`'s undo group delta.
    ///
    /// The editor's revision model remains the owner of undo intent; this
    /// method is just a thin wrapper that tags the underlying insert with the
    /// provided context so the editor can later call [`Self::gc_undo_group`]
    /// when that undo group leaves history.
    pub fn insert_in_group(
        &mut self,
        at: u64,
        text: &str,
        ctx: OverlayEditContext,
    ) -> Result<(), OverlayError> {
        self.enforce_limits(text.len() as u64)?;
        self.insert(at, text)?;

        // Record the op in the delta for this undo group so the editor can
        // later retrieve or GC it.  The buffer_id and range are set to the
        // last inserted piece, which we can recover from the active buffer.
        let active_id = self.active_buffer.unwrap();
        let buf = self.buffers.get(&active_id).expect("active buffer must exist");
        let end = buf.len();
        let start = end - text.len() as u64;
        let op = OverlayOp::Insert { at, buffer_id: active_id, range: InsertRange { start, end } };
        let is_new_group = !self.undo_history.contains_key(&ctx.undo_group);
        let delta = self.undo_history.entry(ctx.undo_group).or_insert_with(|| OverlayDelta {
            undo_group: ctx.undo_group,
            revision_id: ctx.revision_id,
            ops: Vec::new(),
        });
        delta.ops.push(op);
        if is_new_group {
            self.undo_group_order.push(ctx.undo_group);
        }
        Ok(())
    }

    /// Delete bytes `[start, end)` from the logical document, recording the
    /// operation in `ctx`'s undo group delta.
    pub fn delete_in_group(
        &mut self,
        range: ByteRange,
        ctx: OverlayEditContext,
    ) -> Result<(), OverlayError> {
        // Deletes don't add buffer bytes; enforce only the undo group count.
        self.enforce_limits(0)?;
        self.delete(range)?;

        let op = OverlayOp::Delete { range };
        let is_new_group = !self.undo_history.contains_key(&ctx.undo_group);
        let delta = self.undo_history.entry(ctx.undo_group).or_insert_with(|| OverlayDelta {
            undo_group: ctx.undo_group,
            revision_id: ctx.revision_id,
            ops: Vec::new(),
        });
        delta.ops.push(op);
        if is_new_group {
            self.undo_group_order.push(ctx.undo_group);
        }
        Ok(())
    }

    /// Retrieve the [`OverlayDelta`] for `undo_group`, if any.
    ///
    /// The editor's revision model calls this when it needs the overlay
    /// payload to invert or inspect an undo group.
    pub fn delta_for_group(&self, undo_group: usize) -> Option<&OverlayDelta> {
        self.undo_history.get(&undo_group)
    }

    /// Release overlay resources associated with `undo_group`.
    ///
    /// Called when the editor's undo history permanently drops an undo group
    /// (i.e. the group has been GC'd from the CRDT engine and can never be
    /// undone or redone again).
    ///
    /// # GC rule
    ///
    /// Any [`InsertBuffer`] whose bytes are referenced *only* by pieces
    /// originating from `undo_group` — and whose pieces are no longer in the
    /// active piece list — can be freed.  Buffers shared with live pieces are
    /// left intact.
    pub fn gc_undo_group(&mut self, undo_group: usize) {
        let Some(delta) = self.undo_history.remove(&undo_group) else {
            return;
        };

        // Remove from insertion-order record.
        self.undo_group_order.retain(|&g| g != undo_group);

        // Collect buffer IDs referenced by the GC'd delta.
        let mut gc_buffer_ids: Vec<BufferId> = delta
            .ops
            .iter()
            .filter_map(|op| match op {
                OverlayOp::Insert { buffer_id, .. } => Some(*buffer_id),
                OverlayOp::Delete { .. } => None,
            })
            .collect();
        gc_buffer_ids.sort_unstable_by_key(|b| b.0);
        gc_buffer_ids.dedup_by_key(|b| b.0);

        // Find all buffer IDs still referenced by live pieces.
        let live_buffer_ids: std::collections::HashSet<u32> = self
            .pieces
            .iter()
            .filter_map(|p| match p {
                Piece::Inserted { buffer_id, .. } => Some(buffer_id.0),
                _ => None,
            })
            .collect();

        // Also keep buffers referenced by other undo groups in history.
        let history_buffer_ids: std::collections::HashSet<u32> = self
            .undo_history
            .values()
            .flat_map(|d| d.ops.iter())
            .filter_map(|op| match op {
                OverlayOp::Insert { buffer_id, .. } => Some(buffer_id.0),
                _ => None,
            })
            .collect();

        // Drop buffers that are no longer referenced anywhere.
        for id in gc_buffer_ids {
            if !live_buffer_ids.contains(&id.0) && !history_buffer_ids.contains(&id.0) {
                self.buffers.remove(&id);
                // Clear active_buffer pointer if we freed it.
                if self.active_buffer == Some(id) {
                    self.active_buffer = None;
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Arbitrary sparse edit gate
    // -----------------------------------------------------------------------

    /// Returns `true` when all preconditions for arbitrary sparse edits are met.
    pub fn can_enable_arbitrary_edits(&self) -> bool {
        self.edit_gate.can_enable_arbitrary_edits()
    }

    /// Mark that overlay-aware `read_byte_range` is ready.
    pub fn set_read_byte_range_ready(&mut self) {
        self.edit_gate.read_byte_range_ready = true;
    }

    /// Mark that overlay-aware streaming search is ready.
    pub fn set_streaming_search_ready(&mut self) {
        self.edit_gate.streaming_search_ready = true;
    }

    /// Mark that temp-file streaming save is ready.
    pub fn set_streaming_save_ready(&mut self) {
        self.edit_gate.streaming_save_ready = true;
    }

    /// Borrow the current edit gate state.
    pub fn edit_gate(&self) -> &ArbitrarySparseEditGate {
        &self.edit_gate
    }
}
