//! `impl View` methods: select.
use super::*;

impl View {
    pub fn update_annotations(
        &mut self,
        plugin: PluginId,
        interval: Interval,
        annotations: Annotations,
    ) {
        self.annotations.update(plugin, interval, annotations)
    }

    pub fn update_diagnostics(
        &mut self,
        plugin: PluginId,
        diagnostics: Vec<crate::plugins::rpc::Diagnostic>,
    ) {
        self.diagnostics.insert(plugin, diagnostics);
    }

    pub fn get_diagnostics(&self) -> Vec<crate::plugins::rpc::Diagnostic> {
        self.diagnostics.values().flat_map(|diagnostics| diagnostics.iter().cloned()).collect()
    }

    /// Select entire buffer.
    ///
    /// Note: unlike movement based selection, this does not scroll.
    pub fn select_all(&mut self, text: &Rope) {
        let selection = SelRegion::new(0, text.len()).into();
        self.set_selection_raw(text, selection);
    }

    pub(super) fn merge_selections(&mut self, text: &Rope) {
        let Some(first) = self.selection.first().copied() else {
            return;
        };
        let Some(last) = self.selection.last().copied() else {
            return;
        };
        self.set_selection_raw(text, SelRegion::new(first.min(), last.max()).into());
    }

    pub(super) fn merge_consecutive_selections(&mut self, text: &Rope) {
        if self.selection.len() < 2 {
            return;
        }

        let mut merged = Selection::new();
        let mut current = self.selection[0];
        for &region in self.selection.iter().skip(1) {
            if current.max() == region.min() {
                current = SelRegion::new(current.min(), region.max());
            } else {
                merged.add_region(current);
                current = region;
            }
        }
        merged.add_region(current);
        self.set_selection_raw(text, merged);
    }

    /// Finds the unit of text containing the given offset.
    pub(super) fn unit(
        &self,
        text: &Rope,
        offset: usize,
        granularity: SelectionGranularity,
    ) -> Interval {
        match granularity {
            SelectionGranularity::Point => Interval::new(offset, offset),
            SelectionGranularity::Word => {
                let mut word_cursor = WordCursor::new(text, offset);
                let (start, end) = word_cursor.select_word();
                Interval::new(start, end)
            }
            SelectionGranularity::Line => {
                let (line, _) = self.offset_to_line_col(text, offset);
                let (start, end) = self.lines.logical_line_range(text, line);
                Interval::new(start, end)
            }
        }
    }

    /// Selects text with a certain granularity and supports multi_selection
    pub(super) fn select(
        &mut self,
        text: &Rope,
        offset: usize,
        granularity: SelectionGranularity,
        multi: bool,
    ) {
        // If multi-select is enabled, toggle existing regions
        if multi
            && granularity == SelectionGranularity::Point
            && self.deselect_at_offset(text, offset)
        {
            return;
        }

        let region = self.unit(text, offset, granularity).into();

        let base_sel = match multi {
            true => self.selection.clone(),
            false => Selection::new(),
        };
        let mut selection = base_sel.clone();
        selection.add_region(region);
        self.set_selection(text, selection);

        self.drag_state =
            Some(DragState { base_sel, min: region.start, max: region.end, granularity });
    }

    /// Extends an existing selection (eg. when the user performs SHIFT + click).
    pub fn extend_selection(
        &mut self,
        text: &Rope,
        offset: usize,
        granularity: SelectionGranularity,
    ) {
        if self.sel_regions().is_empty() {
            return;
        }

        let (base_sel, last) = {
            let mut base = Selection::new();
            // is_empty guard above ensures split_last is safe
            let Some((last, rest)) = self.sel_regions().split_last() else { return };
            for &region in rest {
                base.add_region(region);
            }
            (base, *last)
        };

        let mut sel = base_sel.clone();
        self.drag_state =
            Some(DragState { base_sel, min: last.start, max: last.start, granularity });

        let start = (last.start, last.start);
        let new_region = self.range_region(text, start, offset, granularity);
        sel.add_region(new_region);
        self.set_selection(text, sel);
    }

    /// Splits current selections into lines.
    pub(super) fn do_split_selection_into_lines(&mut self, text: &Rope) {
        let mut selection = Selection::new();

        for region in self.selection.iter() {
            if region.is_caret() {
                selection.add_region(SelRegion::caret(region.max()));
            } else {
                let mut cursor = Cursor::new(text, region.min());

                while cursor.pos() < region.max() {
                    let sel_start = cursor.pos();
                    let end_of_line = match cursor.next::<LinesMetric>() {
                        Some(end) if end >= region.max() => max(0, region.max() - 1),
                        Some(end) => max(0, end - 1),
                        None if cursor.pos() == text.len() => cursor.pos(),
                        _ => break,
                    };

                    selection.add_region(SelRegion::new(sel_start, end_of_line));
                }
            }
        }

        self.set_selection_raw(text, selection);
    }

    pub(super) fn do_select_regex(&mut self, text: &Rope, pattern: &str, case_sensitive: bool) {
        let Ok(regex) = RegexBuilder::new(pattern).case_insensitive(!case_sensitive).build() else {
            return;
        };

        let mut selection = Selection::new();
        for &region in self.selection.iter() {
            if region.is_caret() {
                continue;
            }

            let start = region.min();
            let end = region.max();
            let slice = text.slice_to_cow(start..end);
            for matched in regex.find_iter(&slice) {
                selection
                    .add_region(SelRegion::new(start + matched.start(), start + matched.end()));
            }
        }

        if !selection.is_empty() {
            self.set_selection_raw(text, selection);
        }
    }

    pub(super) fn trim_selections(&mut self, text: &Rope) {
        let mut selection = Selection::new();

        for &region in self.selection.iter() {
            if region.is_caret() {
                selection.add_region(region);
                continue;
            }

            let min = region.min();
            let max = region.max();
            let slice = text.slice_to_cow(min..max);
            let trimmed_start_len =
                slice.len() - slice.trim_start_matches(char::is_whitespace).len();
            let trimmed_end_len = slice.len() - slice.trim_end_matches(char::is_whitespace).len();
            let new_min = min + trimmed_start_len;
            let new_max = max.saturating_sub(trimmed_end_len);

            let trimmed = if new_min >= new_max {
                SelRegion::caret(new_min.min(max))
            } else if region.start <= region.end {
                SelRegion::new(new_min, new_max)
            } else {
                SelRegion::new(new_max, new_min)
            };
            selection.add_region(trimmed);
        }

        self.set_selection_raw(text, selection);
    }

    pub(super) fn flip_selections(&mut self, text: &Rope) {
        let mut selection = Selection::new();
        for &region in self.selection.iter() {
            selection.add_region(
                SelRegion::new(region.end, region.start).with_affinity(region.affinity),
            );
        }
        self.set_selection_raw(text, selection);
    }

    pub(super) fn ensure_selections_forward(&mut self, text: &Rope) {
        let mut selection = Selection::new();
        for &region in self.selection.iter() {
            selection.add_region(
                SelRegion::new(region.min(), region.max()).with_affinity(region.affinity),
            );
        }
        self.set_selection_raw(text, selection);
    }

    pub(super) fn keep_primary_selection(&mut self, text: &Rope) {
        let Some(region) = self.primary_sel_region() else {
            return;
        };
        self.set_selection_raw_with_primary(text, region.into(), 0);
    }

    pub(super) fn remove_primary_selection(&mut self, text: &Rope) {
        if self.selection.len() <= 1 {
            return;
        }

        let mut selection = Selection::new();
        for (index, &region) in self.selection.iter().enumerate() {
            if index != self.primary_selection_idx {
                selection.add_region(region);
            }
        }
        let primary_selection_idx =
            self.primary_selection_idx.min(selection.len().saturating_sub(1));
        self.set_selection_raw_with_primary(text, selection, primary_selection_idx);
    }

    pub(super) fn rotate_selections(&mut self, text: &Rope, forward: bool) {
        if self.selection.len() < 2 {
            return;
        }

        let len = self.selection.len();
        let next_idx = if forward {
            (self.primary_selection_idx + 1) % len
        } else {
            (self.primary_selection_idx + len - 1) % len
        };
        self.set_primary_selection_idx(text, next_idx);
    }

    /// Does a drag gesture, setting the selection from a combination of the drag
    /// state and new offset.
    pub(super) fn do_drag(&mut self, text: &Rope, offset: usize, affinity: Affinity) {
        let new_sel = self.drag_state.as_ref().map(|drag_state| {
            let mut sel = drag_state.base_sel.clone();
            let start = (drag_state.min, drag_state.max);
            let new_region = self.range_region(text, start, offset, drag_state.granularity);
            sel.add_region(new_region.with_horiz(None).with_affinity(affinity));
            sel
        });

        if let Some(sel) = new_sel {
            self.set_selection(text, sel);
        }
    }

    /// Creates a `SelRegion` for range select or drag operations.
    pub fn range_region(
        &self,
        text: &Rope,
        start: (usize, usize),
        offset: usize,
        granularity: SelectionGranularity,
    ) -> SelRegion {
        let (min_start, max_start) = start;
        let end = self.unit(text, offset, granularity);
        let (min_end, max_end) = (end.start, end.end);
        if offset >= min_start {
            SelRegion::new(min_start, max_end)
        } else {
            SelRegion::new(max_start, min_end)
        }
    }

    /// Returns the regions of the current selection.
    pub fn sel_regions(&self) -> &[SelRegion] {
        &self.selection
    }

    pub(crate) fn selection(&self) -> &Selection {
        &self.selection
    }

    pub(crate) fn syntax_selection_history_mut(&mut self) -> &mut Vec<Selection> {
        &mut self.object_selection_history
    }

    pub(crate) fn cached_semantic_window_text(
        &self,
        language_name: &str,
        file_path: Option<&Path>,
        base_offset: usize,
        end_offset: usize,
    ) -> Option<String> {
        self.semantic_parse_cache
            .contains_window(language_name, file_path, base_offset, end_offset)
            .then(|| self.semantic_parse_cache.source().to_owned())
    }

    pub(crate) fn apply_vlf_syntax_selection(
        &mut self,
        source: &str,
        base_offset: usize,
        language_name: &str,
        file_path: Option<&Path>,
        action: SyntaxSelectionAction,
    ) -> Result<(), object::SyntaxSelectionError> {
        self.semantic_parse_cache.update(source, base_offset, language_name, file_path)?;
        let current = self.selection.clone();
        let selection = object::apply_syntax_selection_in_cache(
            &self.semantic_parse_cache,
            &current,
            &mut self.object_selection_history,
            action,
        )?;
        self.set_vlf_selection(selection);
        Ok(())
    }

    pub(crate) fn apply_vlf_syntax_navigation(
        &mut self,
        source: &str,
        base_offset: usize,
        language_name: &str,
        file_path: Option<&Path>,
        action: SyntaxNavigationAction,
    ) -> Result<(), object::SyntaxSelectionError> {
        self.semantic_parse_cache.update(source, base_offset, language_name, file_path)?;
        let current = self.selection.clone();
        let selection =
            object::apply_syntax_navigation_in_cache(&self.semantic_parse_cache, &current, action)?;
        self.set_vlf_selection(selection);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn semantic_parse_cache_parse_count(&self) -> usize {
        self.semantic_parse_cache.parse_count()
    }

    /// Collapse all selections in this view into a single caret
    pub fn collapse_selections(&mut self, text: &Rope) {
        let mut sel = self.selection.clone();
        sel.collapse();
        self.set_selection(text, sel);
    }

    /// Determines whether the offset is in any selection (counting carets and
    /// selection edges).
    pub fn is_point_in_selection(&self, offset: usize) -> bool {
        !self.selection.regions_in_range(offset, offset).is_empty()
    }
}
