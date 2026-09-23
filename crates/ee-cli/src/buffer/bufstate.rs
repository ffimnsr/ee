//! `impl BufState`: lines, updates, and VLF chunk application.
use super::*;

impl BufState {
    /// Logical line number of the visual row at `idx` (0-based); `None` on
    /// wrapped continuation rows and on lines the backend has not reported.
    pub(crate) fn row_logical_line(&self, idx: usize) -> Option<usize> {
        match self.line_slot(idx)? {
            LineSlot::Known(line) => line.logical_line,
            LineSlot::Invalid => None,
        }
    }

    pub(crate) fn title(&self) -> String {
        if let Some(name) = &self.display_name {
            return name.clone();
        }
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("[scratch]")
            .to_owned()
    }

    pub(crate) fn apply_update(&mut self, update: CoreUpdate) -> io::Result<ApplyUpdateStats> {
        if self.is_vlf {
            // Stage A Phase 3: VLF buffers consume the same `update` stream as
            // rope buffers, but the frontend keeps a bounded window (the core
            // shadow may cover the whole visited range). Insert segments
            // replace the window; copy/skip-only updates (scroll within the
            // window) leave it untouched.
            return self.apply_vlf_update_window(update);
        }
        let CoreUpdate { ops, pristine, annotations, vlf_total_lines: _ } = update;
        let previous = std::mem::take(&mut self.line_cache);
        let previous_lines = std::mem::take(&mut self.lines);
        let mut next_cache = Vec::new();
        let mut next_lines = Vec::new();
        let mut source_index = 0;

        self.pristine = pristine;
        self.annotations = annotations;

        for op in ops {
            match op.op {
                CoreUpdateKind::Insert => {
                    if op.lines.len() != op.n {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "insert op length mismatch: expected {}, got {}",
                                op.n,
                                op.lines.len()
                            ),
                        ));
                    }
                    for line in op.lines {
                        let slot = LineSlot::from(line);
                        next_lines.push(line_text_for_slot(&slot));
                        next_cache.push(slot);
                    }
                }
                CoreUpdateKind::Skip => {
                    source_index = checked_advance(source_index, op.n, previous.len(), "skip")?;
                }
                CoreUpdateKind::Invalidate => {
                    next_cache.extend(std::iter::repeat_n(LineSlot::Invalid, op.n));
                    next_lines.extend(std::iter::repeat_n(String::new(), op.n));
                }
                CoreUpdateKind::Copy => {
                    let end = checked_advance(source_index, op.n, previous.len(), "copy")?;
                    // Clear cursor data from copied slots: Copy op means content is
                    // unchanged from xi-core's perspective, but cursor positions may
                    // have moved. Only Insert/Update ops carry authoritative cursor data.
                    for (offset, slot) in previous[source_index..end].iter().enumerate() {
                        match slot.clone() {
                            LineSlot::Known(mut line) => {
                                line.cursors.clear();
                                next_cache.push(LineSlot::Known(line));
                            }
                            invalid => next_cache.push(invalid),
                        }
                        next_lines.push(
                            previous_lines
                                .get(source_index + offset)
                                .cloned()
                                .unwrap_or_else(|| line_text_for_slot(slot)),
                        );
                    }
                    source_index = end;
                }
                CoreUpdateKind::Update => {
                    if op.lines.len() != op.n {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "update op length mismatch: expected {}, got {}",
                                op.n,
                                op.lines.len()
                            ),
                        ));
                    }
                    let end = checked_advance(source_index, op.n, previous.len(), "update")?;
                    for (slot, line) in previous[source_index..end].iter().cloned().zip(op.lines) {
                        let slot = slot.merge(line)?;
                        next_lines.push(line_text_for_slot(&slot));
                        next_cache.push(slot);
                    }
                    source_index = end;
                }
            }
        }

        self.line_cache = next_cache;
        self.lines = next_lines;
        if matches!(
            self.line_cache.as_slice(),
            [LineSlot::Known(CachedLine { text, .. })] if text.is_empty()
        ) {
            self.lines.clear();
        }
        self.sync_cursor_from_cache();
        Ok(ApplyUpdateStats { rebuild_lines: Duration::ZERO })
    }

    /// VLF window application: take the inserted line segments from the core
    /// `update` stream and replace the bounded frontend window.
    ///
    /// The core's render plan always inserts the visible window (the shadow
    /// is re-seeded on any height drift), so one insert segment covers the
    /// new window and its `ln` fields position it exactly. Skip/copy-only
    /// updates (scrolling within the current window) keep the window as-is;
    /// only cursors/annotations refresh.
    fn apply_vlf_update_window(&mut self, update: CoreUpdate) -> io::Result<ApplyUpdateStats> {
        let CoreUpdate { ops, pristine, annotations, vlf_total_lines } = update;
        self.pristine = pristine;
        self.annotations = annotations;
        if let Some(total) = vlf_total_lines {
            self.vlf_approx_line_count = total.count;
            self.vlf_line_count_exact = total.exact;
            self.vlf_index_progress = total.index_progress;
        }

        // The previous window feeds copy/update ops (rows the core expects
        // the client to already hold); take it before the cache is replaced.
        let old_cache = std::mem::take(&mut self.line_cache);
        let Some((start, end, cache)) =
            vlf_window_from_ops(ops, &old_cache, self.vlf_cache_start_line)
        else {
            self.line_cache = old_cache;
            return Ok(ApplyUpdateStats { rebuild_lines: Duration::ZERO });
        };
        self.line_cache = cache;
        self.lines = self.line_cache.iter().map(line_text_for_slot).collect();
        self.vlf_cache_start_line = start;
        self.last_scroll = Some((start, end));
        if self.pending_vlf_tail_jump {
            self.cursor_line = end.saturating_sub(1);
            self.cursor_col = 0;
            // Settle only when the landing actually reached the exact tail: an
            // inexact tail moves as the index scans, and a delayed response to
            // an older scroll can land mid-file while carrying a fresh exact
            // count.
            let at_tail = self.vlf_line_count_exact
                && usize::try_from(self.vlf_approx_line_count).is_ok_and(|count| end >= count);
            self.pending_vlf_tail_jump = !at_tail;
        }
        self.sync_cursor_from_cache();
        Ok(ApplyUpdateStats { rebuild_lines: Duration::ZERO })
    }

    pub(crate) fn rebuild_lines(&mut self) {
        // VLF mode: skip full-buffer clone; `lines` stays empty.
        // Rendering reads `line_cache` directly for the visible viewport range.
        if self.is_vlf {
            return;
        }

        self.lines = self
            .line_cache
            .iter()
            .map(|slot| match slot {
                LineSlot::Known(line) => line.text.clone(),
                LineSlot::Invalid => String::new(),
            })
            .collect();

        if matches!(
            self.line_cache.as_slice(),
            [LineSlot::Known(CachedLine { text, .. })] if text.is_empty()
        ) {
            self.lines.clear();
        }
    }

    pub(super) fn sync_cursor_from_cache(&mut self) {
        // VLF keeps the cursor frontend-authoritative: navigation, tail jumps,
        // and edits move it locally, while the core's caret annotation stays at
        // the last core-side edit and would re-anchor the cursor whenever an
        // update window happens to include that row (most visibly the row-0
        // caret snapping the cursor back to line 1 while paging down at the
        // top of the file via copied rows).
        if self.is_vlf {
            self.clamp_cursor();
            return;
        }
        for (line_index, slot) in self.line_cache.iter().enumerate() {
            let LineSlot::Known(line) = slot else { continue };
            if let Some(&cursor_col) = line.cursors.first() {
                self.cursor_line = line_index;
                self.cursor_col = previous_char_boundary(&line.text, cursor_col);
                self.clamp_cursor();
                return;
            }
        }
        self.clamp_cursor();
    }

    pub(crate) fn clamp_cursor(&mut self) {
        if self.is_vlf {
            self.cursor_line = self.cursor_line.min(self.line_count().saturating_sub(1));
            if let Some(LineSlot::Known(line)) = self.line_slot(self.cursor_line) {
                self.cursor_col = previous_char_boundary(&line.text, self.cursor_col);
            }
            return;
        }

        if self.lines.is_empty() {
            self.cursor_line = 0;
            self.cursor_col = 0;
            return;
        }
        self.cursor_line = self.cursor_line.min(self.lines.len().saturating_sub(1));
        self.cursor_col = previous_char_boundary(&self.lines[self.cursor_line], self.cursor_col);
    }

    pub(crate) fn is_fully_cached(&self) -> bool {
        if self.is_vlf {
            return self.vlf_line_count_exact
                && self.vlf_cache_start_line == 0
                && self.line_cache.len() == self.line_count()
                && self.line_cache.iter().all(|slot| matches!(slot, LineSlot::Known(_)));
        }
        // An empty cache is a loading state, not a fully-cached one:
        // `all()` on an empty iterator is vacuously true, which made the
        // first deferred source-control refresh run while the buffer was
        // still loading and diff padded-empty slots against HEAD.
        !self.line_cache.is_empty()
            && self.line_cache.iter().all(|slot| matches!(slot, LineSlot::Known(_)))
    }

    /// Return the total line count regardless of mode.
    ///
    /// In VLF mode `lines` is empty; use `line_cache.len()` instead.
    pub(crate) fn line_count(&self) -> usize {
        if self.is_vlf {
            let reported = usize::try_from(self.vlf_approx_line_count).unwrap_or(usize::MAX);
            if self.vlf_line_count_exact && reported > 0 {
                reported
            } else {
                reported.max(self.vlf_cache_start_line.saturating_add(self.line_cache.len()))
            }
        } else {
            self.lines.len()
        }
    }

    /// Return the text of a line by logical index, or `None` if the slot is not loaded.
    ///
    /// In normal mode reads from `lines`.  In VLF mode reads from `line_cache`
    /// and returns `None` for `LineSlot::Invalid` (show a loading indicator).
    pub(crate) fn get_line(&self, idx: usize) -> Option<&str> {
        if self.is_vlf {
            match self.line_slot(idx)? {
                LineSlot::Known(line) => Some(&line.text),
                LineSlot::Invalid => None,
            }
        } else {
            self.lines.get(idx).map(|s| s.as_str())
        }
    }

    pub(crate) fn line_slot(&self, idx: usize) -> Option<&LineSlot> {
        if self.is_vlf {
            let local = idx.checked_sub(self.vlf_cache_start_line)?;
            self.line_cache.get(local)
        } else {
            self.line_cache.get(idx)
        }
    }

    pub(crate) fn line_len(&self, idx: usize) -> Option<usize> {
        self.get_line(idx).map(str::len)
    }

    pub(crate) fn line_range_owned(&self, start: usize, end: usize) -> Option<Vec<String>> {
        if start > end {
            return Some(Vec::new());
        }
        (start..=end).map(|idx| self.get_line(idx).map(str::to_owned)).collect()
    }

    pub(crate) fn line_start_offset(&self, line: usize) -> Option<usize> {
        let mut offset = 0usize;
        for idx in 0..line {
            offset = offset.checked_add(self.get_line(idx)?.len() + 1)?;
        }
        Some(offset)
    }

    pub(crate) fn whole_text(&self) -> Option<String> {
        if self.is_vlf {
            return None;
        }
        if self.line_count() == 0 {
            return Some(String::new());
        }
        let mut text = String::new();
        for idx in 0..self.line_count() {
            if idx > 0 {
                text.push('\n');
            }
            text.push_str(self.get_line(idx)?);
        }
        Some(text)
    }

    pub(crate) fn apply_local_vlf_replace_range(
        &mut self,
        start_line: usize,
        start_col: usize,
        end_line: usize,
        end_col: usize,
        text: &str,
    ) -> bool {
        if !self.is_vlf || start_line > end_line {
            return false;
        }

        let Some(start_local) = start_line.checked_sub(self.vlf_cache_start_line) else {
            return false;
        };
        let Some(end_local) = end_line.checked_sub(self.vlf_cache_start_line) else {
            return false;
        };
        if end_local >= self.line_cache.len() {
            return false;
        }

        let (LineSlot::Known(first_line), LineSlot::Known(last_line)) =
            (&self.line_cache[start_local], &self.line_cache[end_local])
        else {
            return false;
        };

        if start_col > first_line.text.len()
            || end_col > last_line.text.len()
            || !first_line.text.is_char_boundary(start_col)
            || !last_line.text.is_char_boundary(end_col)
        {
            return false;
        }

        let mut combined = first_line.text[..start_col].to_owned();
        combined.push_str(text);
        combined.push_str(&last_line.text[end_col..]);

        let replacement_lines: Vec<String> = combined.split('\n').map(str::to_owned).collect();
        let replacement_spans = build_optimistic_vlf_spans(
            first_line,
            last_line,
            start_col,
            end_col,
            &replacement_lines,
        );
        let replacement_count = replacement_lines.len();
        let replacement =
            replacement_lines.into_iter().zip(replacement_spans).map(|(line, syntax_spans)| {
                LineSlot::Known(CachedLine {
                    text: line,
                    cursors: Vec::new(),
                    syntax_spans,
                    logical_line: None,
                })
            });
        let replaced_count = end_local - start_local + 1;
        self.line_cache.splice(start_local..=end_local, replacement);

        match replacement_count.cmp(&replaced_count) {
            std::cmp::Ordering::Greater => {
                self.vlf_approx_line_count = self
                    .vlf_approx_line_count
                    .saturating_add((replacement_count - replaced_count) as u64);
            }
            std::cmp::Ordering::Less => {
                self.vlf_approx_line_count = self
                    .vlf_approx_line_count
                    .saturating_sub((replaced_count - replacement_count) as u64);
            }
            std::cmp::Ordering::Equal => {}
        }

        true
    }
}
