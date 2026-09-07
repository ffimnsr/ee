//! `impl View` methods: selection.
use super::*;

impl View {
    pub(crate) fn do_edit(&mut self, text: &Rope, cmd: ViewEvent) {
        use self::ViewEvent::*;
        match cmd {
            Move(movement) => self.do_move(text, movement, false),
            ModifySelection(movement) => self.do_move(text, movement, true),
            SelectAll => self.select_all(text),
            MergeSelections => self.merge_selections(text),
            MergeConsecutiveSelections => self.merge_consecutive_selections(text),
            Scroll(range) => self.set_scroll(range.first, range.last),
            AddSelectionAbove => self.add_selection_by_movement(text, Movement::UpExactPosition),
            AddSelectionBelow => self.add_selection_by_movement(text, Movement::DownExactPosition),
            Gesture { line, col, ty } => self.do_gesture(text, line, col, ty),
            GotoLine { line } => self.goto_line(text, line),
            Find { chars, case_sensitive, regex, whole_words } => {
                let id = self.find.first().map(|q| q.id());
                let query_changes = FindQuery { id, chars, case_sensitive, regex, whole_words };
                self.set_find(text, [query_changes].to_vec())
            }
            MultiFind { queries } => self.set_find(text, queries),
            FindNext { wrap_around, allow_same, modify_selection } => {
                self.do_find_next(text, false, wrap_around, allow_same, &modify_selection)
            }
            FindPrevious { wrap_around, allow_same, modify_selection } => {
                self.do_find_next(text, true, wrap_around, allow_same, &modify_selection)
            }
            FindAll => self.do_find_all(text),
            Click(MouseAction { line, column, flags, click_count }) => {
                // Deprecated (kept for client compatibility):
                // should be removed in favor of do_gesture
                warn!("Usage of click is deprecated; use do_gesture");
                if (flags & FLAG_SELECT) != 0 {
                    self.do_gesture(
                        text,
                        line,
                        column,
                        GestureType::SelectExtend { granularity: SelectionGranularity::Point },
                    )
                } else if click_count == Some(2) {
                    self.do_gesture(text, line, column, GestureType::WordSelect)
                } else if click_count == Some(3) {
                    self.do_gesture(text, line, column, GestureType::LineSelect)
                } else {
                    self.do_gesture(text, line, column, GestureType::PointSelect)
                }
            }
            Drag(MouseAction { line, column, .. }) => {
                warn!("Usage of drag is deprecated; use gesture instead");
                self.do_gesture(text, line, column, GestureType::Drag)
            }
            CollapseSelections => self.collapse_selections(text),
            HighlightFind { visible } => {
                self.highlight_find = visible;
                self.find_changed = FindStatusChange::All;
                self.set_dirty(text);
            }
            SelectionForFind { case_sensitive } => self.do_selection_for_find(text, case_sensitive),
            Replace { chars, preserve_case } => self.do_set_replace(chars, preserve_case),
            SelectionForReplace => self.do_selection_for_replace(text),
            SelectRegex { chars, case_sensitive } => {
                self.do_select_regex(text, &chars, case_sensitive)
            }
            SelectionIntoLines => self.do_split_selection_into_lines(text),
            TrimSelections => self.trim_selections(text),
            FlipSelections => self.flip_selections(text),
            EnsureSelectionsForward => self.ensure_selections_forward(text),
            KeepPrimarySelection => self.keep_primary_selection(text),
            RemovePrimarySelection => self.remove_primary_selection(text),
            RotateSelectionsBackward => self.rotate_selections(text, false),
            RotateSelectionsForward => self.rotate_selections(text, true),
        }
    }

    pub(super) fn do_gesture(&mut self, text: &Rope, line: u64, col: u64, ty: GestureType) {
        let line = line as usize;
        let col = col as usize;
        let offset = self.line_col_to_offset(text, line, col);
        match ty {
            GestureType::Select { granularity, multi } => {
                self.select(text, offset, granularity, multi)
            }
            GestureType::SelectExtend { granularity } => {
                self.extend_selection(text, offset, granularity)
            }
            GestureType::Drag => self.do_drag(text, offset, Affinity::default()),

            _ => {
                warn!("Deprecated gesture type sent to do_gesture method");
            }
        }
    }

    pub(super) fn goto_line(&mut self, text: &Rope, line: u64) {
        let offset = self.line_col_to_offset(text, line as usize, 0);
        self.set_selection(text, SelRegion::caret(offset));
    }

    pub fn set_size(&mut self, size: Size) {
        self.size = size;
    }

    pub fn set_scroll(&mut self, first: i64, last: i64) {
        let first = max(first, 0) as usize;
        let last = max(last, 0) as usize;
        self.first_line = first;
        self.height = last.saturating_sub(first);
    }

    pub fn scroll_height(&self) -> usize {
        self.height
    }

    pub(super) fn scroll_to_cursor(&mut self, text: &Rope) {
        let end = match self.primary_sel_region() {
            Some(region) => region.end,
            None => return,
        };
        let line = self.line_of_offset(text, end);
        if line < self.first_line {
            self.first_line = line;
        } else if self.first_line.saturating_add(self.height) <= line {
            self.first_line = line.saturating_sub(self.height.saturating_sub(1));
        }
        // Primary selection drives cursor-oriented state and scroll targets.
        self.scroll_to = Some(end);
    }

    /// Removes any selection present at the given offset.
    /// Returns true if a selection was removed, false otherwise.
    pub fn deselect_at_offset(&mut self, text: &Rope, offset: usize) -> bool {
        if !self.selection.regions_in_range(offset, offset).is_empty() {
            let mut sel = self.selection.clone();
            sel.delete_range(offset, offset, true);
            if !sel.is_empty() {
                self.drag_state = None;
                self.set_selection_raw(text, sel);
                return true;
            }
        }
        false
    }

    /// Move the selection by the given movement. Return value is the offset of
    /// a point that should be scrolled into view.
    ///
    /// If `modify` is `true`, the selections are modified, otherwise the results
    /// of individual region movements become carets.
    pub fn do_move(&mut self, text: &Rope, movement: Movement, modify: bool) {
        self.drag_state = None;
        let new_sel =
            selection_movement(movement, &self.selection, self, self.scroll_height(), text, modify);
        self.set_selection(text, new_sel);
    }

    /// Set the selection to a new value.
    pub fn set_selection<S: Into<Selection>>(&mut self, text: &Rope, sel: S) {
        self.set_selection_raw(text, sel.into());
        self.scroll_to_cursor(text);
    }

    pub(crate) fn set_vlf_selection<S: Into<Selection>>(&mut self, sel: S) {
        self.selection = sel.into();
        self.clamp_primary_selection();
        self.scroll_to = self.primary_sel_region().map(|region| region.end);
    }

    /// Sets the selection to a new value, without invalidating.
    pub(super) fn set_selection_for_edit(&mut self, text: &Rope, sel: Selection) {
        self.selection = sel;
        self.clamp_primary_selection();
        self.scroll_to_cursor(text);
    }

    /// Sets the selection to a new value, invalidating the line cache as needed.
    /// This function does not perform any scrolling.
    pub(super) fn set_selection_raw(&mut self, text: &Rope, sel: Selection) {
        let primary_selection_idx = sel.len().saturating_sub(1);
        self.set_selection_raw_with_primary(text, sel, primary_selection_idx);
    }

    pub(super) fn set_selection_raw_with_primary(
        &mut self,
        text: &Rope,
        sel: Selection,
        primary_selection_idx: usize,
    ) {
        self.invalidate_selection(text);
        self.selection = sel;
        self.primary_selection_idx = primary_selection_idx;
        self.clamp_primary_selection();
        self.invalidate_selection(text);
    }

    pub(super) fn clamp_primary_selection(&mut self) {
        self.primary_selection_idx = if self.selection.is_empty() {
            0
        } else {
            self.primary_selection_idx.min(self.selection.len() - 1)
        };
    }

    pub(super) fn set_primary_selection_idx(&mut self, text: &Rope, primary_selection_idx: usize) {
        if self.selection.is_empty() {
            self.primary_selection_idx = 0;
            return;
        }

        let primary_selection_idx = primary_selection_idx.min(self.selection.len() - 1);
        if primary_selection_idx == self.primary_selection_idx {
            return;
        }

        self.invalidate_selection(text);
        self.primary_selection_idx = primary_selection_idx;
        self.invalidate_selection(text);
        self.scroll_to_cursor(text);
    }

    pub(crate) fn primary_sel_region(&self) -> Option<SelRegion> {
        self.selection
            .get(self.primary_selection_idx)
            .copied()
            .or_else(|| self.selection.last().copied())
    }

    /// Invalidate the current selection. Note that we could be even more
    /// fine-grained in the case of multiple cursors, but we also want this
    /// method to be fast even when the selection is large.
    pub(super) fn invalidate_selection(&mut self, text: &Rope) {
        let (first, last) = match (self.selection.first(), self.selection.last()) {
            (Some(f), Some(l)) => (f, l),
            _ => return,
        };
        let first_line = self.line_of_offset(text, first.min());
        let first_line = if first.is_upstream() && first.end == first.min() {
            first_line.saturating_sub(1)
        } else {
            first_line
        };
        let last_line = self.line_of_offset(text, last.max()) + 1;
        let all_caret = self.selection.iter().all(|region| region.is_caret());
        let invalid = if all_caret {
            line_cache_shadow::CURSOR_VALID
        } else {
            line_cache_shadow::CURSOR_VALID | line_cache_shadow::SYNTAX_VALID
        };
        self.lc_shadow.partial_invalidate(first_line, last_line, invalid);
    }

    pub(super) fn add_selection_by_movement(&mut self, text: &Rope, movement: Movement) {
        let mut sel = Selection::new();
        for &region in self.sel_regions() {
            sel.add_region(region);
            let new_region =
                region_movement(movement, region, self, self.scroll_height(), text, false);
            sel.add_region(new_region);
        }
        self.set_selection(text, sel);
    }
}
