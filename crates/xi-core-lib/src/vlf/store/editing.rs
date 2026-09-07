//! `impl VlfStore` methods: editing.
use super::*;

impl VlfStore {
    pub fn enable_editing(&self) {
        let mut ov = self.overlay.borrow_mut();
        if ov.is_some() {
            return;
        }
        let file_size = self.pager.file_size();
        // Use byte_len from file size; newline_count is approximate (we don't
        // scan the whole file here).  The overlay accumulates exact counts for
        // inserted pieces; Original piece newline counts stay approximate until
        // the page index fills in.
        let metrics = TextMetrics { byte_len: file_size, ..TextMetrics::default() };
        let mut overlay = PieceOverlay::with_limits(metrics, OverlayLimits::default());
        overlay.set_read_byte_range_ready();
        overlay.set_streaming_search_ready();
        overlay.set_streaming_save_ready();
        *ov = Some(overlay);
    }

    /// Enable editing mode with explicit resource limits.
    ///
    /// Use in tests or when the default [`OverlayLimits`] need to be tuned for
    /// a specific deployment.
    pub fn enable_editing_with_limits(&self, limits: OverlayLimits) {
        let mut ov = self.overlay.borrow_mut();
        if ov.is_some() {
            return;
        }
        let file_size = self.pager.file_size();
        let metrics = TextMetrics { byte_len: file_size, ..TextMetrics::default() };
        let mut overlay = PieceOverlay::with_limits(metrics, limits);
        overlay.set_read_byte_range_ready();
        overlay.set_streaming_search_ready();
        overlay.set_streaming_save_ready();
        *ov = Some(overlay);
    }

    /// Insert `text` at logical byte offset `at`, recording the edit in the
    /// overlay under `ctx`'s undo group.
    ///
    /// Returns `Err` when editing is not enabled (call [`Self::enable_editing`]
    /// first), when `at` is out of range, or when an overlay resource limit is
    /// reached.
    ///
    /// # Invariant
    ///
    /// The base file is never converted to a `Rope`.  Inserted bytes live
    /// exclusively in the overlay's append-only insert buffers.
    pub fn apply_insert(
        &self,
        at: u64,
        text: &str,
        ctx: OverlayEditContext,
    ) -> Result<(), VlfEditError> {
        let mut ov = self.overlay.borrow_mut();
        let overlay = ov.as_mut().ok_or(VlfEditError::EditingNotEnabled)?;
        overlay.insert_in_group(at, text, ctx).map_err(VlfEditError::Overlay)?;
        // Update peak overlay bytes tracking.
        let overlay_bytes = overlay.overlay_bytes();
        drop(ov);
        let mut stats = self.stats.borrow_mut();
        if overlay_bytes > stats.peak_overlay_bytes {
            stats.peak_overlay_bytes = overlay_bytes;
        }
        Ok(())
    }

    /// Delete bytes `[range.start, range.end)` from the logical document,
    /// recording the edit in the overlay under `ctx`'s undo group.
    ///
    /// Returns `Err` when editing is not enabled or when the range is out of
    /// bounds.
    pub fn apply_delete(
        &self,
        range: crate::text_store::ByteRange,
        ctx: OverlayEditContext,
    ) -> Result<(), VlfEditError> {
        let mut ov = self.overlay.borrow_mut();
        let overlay = ov.as_mut().ok_or(VlfEditError::EditingNotEnabled)?;
        overlay.delete_in_group(range, ctx).map_err(VlfEditError::Overlay)?;
        Ok(())
    }

    /// Return recorded overlay delta for `undo_group`, if present.
    #[allow(dead_code)]
    pub fn overlay_delta_for_undo_group(
        &self,
        undo_group: usize,
    ) -> Option<crate::vlf::overlay::OverlayDelta> {
        let ov = self.overlay.borrow();
        ov.as_ref()?.delta_for_group(undo_group).cloned()
    }

    /// Release overlay history and insert buffers owned only by `undo_group`.
    pub fn gc_undo_group(&self, undo_group: usize) {
        let mut ov = self.overlay.borrow_mut();
        if let Some(overlay) = ov.as_mut() {
            overlay.gc_undo_group(undo_group);
        }
    }

    /// Suggested save policy given the current overlay state.
    ///
    /// Returns the narrowest available strategy: same-size overwrite,
    /// tail-shift with temp fallback, temp rewrite, or save-as.  Returns
    /// `None` when editing is not enabled (no overlay changes to save).
    pub fn suggested_save_policy(&self) -> Option<VlfSavePolicy> {
        self.overlay.borrow().as_ref().map(|ov| ov.suggested_save_policy())
    }

    /// Returns `true` when an edit overlay is active for this VLF buffer.
    pub fn is_editing_enabled(&self) -> bool {
        self.overlay.borrow().is_some()
    }

    /// Returns `true` when the overlay has the streaming-save gate enabled.
    pub fn is_save_enabled(&self) -> bool {
        self.overlay
            .borrow()
            .as_ref()
            .is_some_and(|overlay| overlay.edit_gate().streaming_save_ready)
    }

    /// Signed byte delta of the current overlay relative to the original file.
    ///
    /// Returns `0` when editing has not been enabled.
    pub fn signed_byte_delta(&self) -> i64 {
        self.overlay.borrow().as_ref().map_or(0, |ov| ov.signed_byte_delta())
    }

    /// Save the current overlay piece sequence to `dest` using the requested
    /// VLF save policy.
    ///
    /// # Parameters
    ///
    /// - `dest`        — Final destination path (overwritten atomically).
    /// - `policy`      — Determines temp-dir placement and save-as semantics.
    ///   Use [`Self::suggested_save_policy`] to get a policy recommendation
    ///   based on the current overlay.
    /// - `on_progress` — Called after each chunk is written.  Return `false`
    ///   to cancel **before** the rename commit point.
    ///
    /// # Errors
    ///
    /// Returns [`crate::vlf::save::VlfSaveError::EditingNotEnabled`] when `Self::enable_editing`
    /// has not been called (no overlay to save).  For an unmodified read-only
    /// VLF file the original file on disk already reflects the correct content.
    ///
    /// # Cancellation after commit
    ///
    /// Once the rename succeeds the file is durably committed.  `on_progress`
    /// is never called after the rename, so there is no way to cancel a
    /// completed save.  Callers should treat `Ok(())` as unconditional success.
    pub fn stream_save(
        &self,
        dest: &std::path::Path,
        policy: &crate::vlf::overlay::VlfSavePolicy,
        on_progress: &mut dyn FnMut(crate::vlf::save::SaveProgress) -> bool,
    ) -> Result<(), crate::vlf::save::VlfSaveError> {
        let ov = self.overlay.borrow();
        let overlay = ov.as_ref().ok_or(crate::vlf::save::VlfSaveError::EditingNotEnabled)?;
        crate::vlf::save::stream_save_pieces(
            overlay.pieces(),
            overlay,
            &self.pager,
            dest,
            policy,
            on_progress,
        )
    }

    /// Snapshot bounded save inputs for background VLF save execution.
    pub fn prepare_save_plan(
        &self,
    ) -> Result<crate::vlf::save::PreparedVlfSavePlan, crate::vlf::save::VlfSaveError> {
        let ov = self.overlay.borrow();
        let overlay = ov.as_ref().ok_or(crate::vlf::save::VlfSaveError::EditingNotEnabled)?;
        Ok(crate::vlf::save::PreparedVlfSavePlan {
            source_path: self.pager.canonical_path().to_owned(),
            snapshot: overlay.save_snapshot(),
        })
    }

    /// Rebase the store onto the just-saved on-disk file without leaving VLF mode.
    pub fn refresh_after_save(&mut self, path: &Path) -> io::Result<()> {
        let raw_cache_byte_cap = self.pager.metrics().cache_byte_cap;
        let decoded_cache_byte_cap = self.decoded_cache.get_mut().byte_cap();
        let viewport = self.viewport.get_mut().clone();
        let restart_background_indexing = self.scan_rx.get_mut().is_some();

        let overlay_limits =
            self.overlay.get_mut().as_ref().map(|overlay| overlay.limits().clone());

        self.bg_cancel.store(true, Ordering::Release);
        *self.scan_rx.get_mut() = None;
        self.bg_cancel = Arc::new(AtomicBool::new(false));

        self.pager = FilePager::open_with_config(path, raw_cache_byte_cap, self.page_size * 4)?;
        let file_size = self.pager.file_size();

        *self.index.get_mut() = PageIndex::new(file_size);
        *self.decoded_cache.get_mut() = DecodedTextCache::new(decoded_cache_byte_cap);

        let window_start = viewport.window_start.0.min(file_size);
        let window_end = viewport.window_end.0.min(file_size).max(window_start);
        *self.viewport.get_mut() = VlfViewportState {
            window_start: ByteOffset(window_start),
            window_end: ByteOffset(window_end),
            decoded_range: ByteRange::new(window_start, window_end),
            original_encoded_len: window_end.saturating_sub(window_start),
            dirty: false,
            batch_size: viewport.batch_size,
        };

        self.first_viewport_set.set(window_end > window_start);
        self.approx_line_floor.set(0);
        self.exact_line_count.set(None);

        *self.overlay.get_mut() = overlay_limits.map(|limits| {
            let mut overlay = PieceOverlay::with_limits(
                TextMetrics { byte_len: file_size, ..TextMetrics::default() },
                limits,
            );
            overlay.set_read_byte_range_ready();
            overlay.set_streaming_search_ready();
            overlay.set_streaming_save_ready();
            overlay
        });

        if restart_background_indexing {
            self.start_background_indexing();
        }
        Ok(())
    }
}
