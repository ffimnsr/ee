//! `impl View` methods: find.
use super::*;

impl View {
    pub(super) fn do_selection_for_find(&mut self, text: &Rope, case_sensitive: bool) {
        // set primary selection or word under current cursor as search query
        let search_query = match self.primary_sel_region() {
            Some(region) => {
                if !region.is_caret() {
                    text.slice_to_cow(region.min()..region.max())
                } else {
                    let (start, end) = {
                        let mut word_cursor = WordCursor::new(text, region.max());
                        word_cursor.select_word()
                    };
                    text.slice_to_cow(start..end)
                }
            }
            _ => return,
        };

        self.set_dirty(text);

        // set selection as search query for first find if no additional search queries are used
        // otherwise add new find with selection as search query
        if self.find.len() != 1 {
            self.add_find();
        }

        if let Some(find) = self.find.last_mut() {
            find.set_find(&search_query, case_sensitive, false, true);
        }
        self.find_progress = FindProgress::Started;
    }

    pub(super) fn add_find(&mut self) {
        let id = self.find_id_counter.next();
        self.find.push(Find::new(id));
    }

    pub(super) fn set_find(&mut self, text: &Rope, queries: Vec<FindQuery>) {
        // checks if at least query has been changed, otherwise we don't need to rerun find
        let mut find_changed = queries.len() != self.find.len();

        // remove deleted queries
        self.find.retain(|f| queries.iter().any(|q| q.id == Some(f.id())));

        for query in &queries {
            let pos = match query.id {
                Some(id) => {
                    // update existing query
                    match self.find.iter().position(|f| f.id() == id) {
                        Some(p) => p,
                        None => return,
                    }
                }
                None => {
                    // add new query
                    self.add_find();
                    self.find.len() - 1
                }
            };

            if self.find[pos].set_find(
                &query.chars.clone(),
                query.case_sensitive,
                query.regex,
                query.whole_words,
            ) {
                find_changed = true;
            }
        }

        if find_changed {
            self.set_dirty(text);
            self.find_progress = FindProgress::Started;
        }
    }

    pub fn do_find(&mut self, text: &Rope) {
        let search_range = match &self.find_progress.clone() {
            FindProgress::Started => {
                // start incremental find on visible region
                let start = self.offset_of_line(text, self.first_line);
                let end = min(text.len(), start + FIND_BATCH_SIZE);
                self.find_changed = FindStatusChange::Matches;
                self.find_progress = FindProgress::InProgress(Range { start, end });
                Some((start, end))
            }
            FindProgress::InProgress(searched_range) => {
                if searched_range.start == 0 && searched_range.end >= text.len() {
                    // the entire text has been searched
                    // end find by executing multi-line regex queries on entire text
                    // stop incremental find
                    self.find_progress = FindProgress::Ready;
                    self.find_changed = FindStatusChange::All;
                    Some((0, text.len()))
                } else {
                    self.find_changed = FindStatusChange::Matches;
                    // expand find to un-searched regions
                    let start_off = self.offset_of_line(text, self.first_line);

                    // If there is unsearched text before the visible region, we want to include it in this search operation
                    let search_preceding_range = start_off.saturating_sub(searched_range.start)
                        < searched_range.end.saturating_sub(start_off)
                        && searched_range.start > 0;

                    if search_preceding_range || searched_range.end >= text.len() {
                        let start = searched_range.start.saturating_sub(FIND_BATCH_SIZE);
                        self.find_progress =
                            FindProgress::InProgress(Range { start, end: searched_range.end });
                        Some((start, searched_range.start))
                    } else if searched_range.end < text.len() {
                        let end = min(text.len(), searched_range.end + FIND_BATCH_SIZE);
                        self.find_progress =
                            FindProgress::InProgress(Range { start: searched_range.start, end });
                        Some((searched_range.end, end))
                    } else {
                        self.find_changed = FindStatusChange::All;
                        None
                    }
                }
            }
            _ => {
                self.find_changed = FindStatusChange::None;
                None
            }
        };

        if let Some((search_range_start, search_range_end)) = search_range {
            for query in &mut self.find {
                if !query.is_multiline_regex() {
                    query.update_find(text, search_range_start, search_range_end, true);
                } else {
                    // only execute multi-line regex queries if we are searching the entire text (last step)
                    if search_range_start == 0 && search_range_end == text.len() {
                        query.update_find(text, search_range_start, search_range_end, true);
                    }
                }
            }
        }
    }

    /// Selects the next find match.
    pub fn do_find_next(
        &mut self,
        text: &Rope,
        reverse: bool,
        wrap: bool,
        allow_same: bool,
        modify_selection: &SelectionModifier,
    ) {
        self.select_next_occurrence(text, reverse, false, allow_same, modify_selection);
        if self.scroll_to.is_none() && wrap {
            self.select_next_occurrence(text, reverse, true, allow_same, modify_selection);
        }
    }

    /// Selects all find matches.
    pub fn do_find_all(&mut self, text: &Rope) {
        let mut selection = Selection::new();
        for find in &self.find {
            for &occurrence in find.occurrences().iter() {
                selection.add_region(occurrence);
            }
        }

        if !selection.is_empty() {
            self.set_selection(text, selection);
        }
    }

    /// Select the next occurrence relative to the last cursor. `reverse` determines whether the
    /// next occurrence before (`true`) or after (`false`) the last cursor is selected. `wrapped`
    /// indicates a search for the next occurrence past the end of the file.
    pub fn select_next_occurrence(
        &mut self,
        text: &Rope,
        reverse: bool,
        wrapped: bool,
        _allow_same: bool,
        modify_selection: &SelectionModifier,
    ) {
        let (cur_start, cur_end) = match self.primary_sel_region() {
            Some(sel) => (sel.min(), sel.max()),
            _ => (0, 0),
        };

        // multiple queries; select closest occurrence
        let closest_occurrence = self
            .find
            .iter()
            .flat_map(|x| x.next_occurrence(text, reverse, wrapped, &self.selection))
            .min_by_key(|x| match reverse {
                true if x.end > cur_end => 2 * text.len() - x.end,
                true => cur_end - x.end,
                false if x.start < cur_start => x.start + text.len(),
                false => x.start - cur_start,
            });

        if let Some(occ) = closest_occurrence {
            match modify_selection {
                SelectionModifier::Set => self.set_selection(text, occ),
                SelectionModifier::Add => {
                    let mut selection = self.selection.clone();
                    selection.add_region(occ);
                    self.set_selection(text, selection);
                }
                SelectionModifier::AddRemovingCurrent => {
                    let mut selection = self.selection.clone();

                    if let Some(primary_selection) = self.primary_sel_region() {
                        if !primary_selection.is_caret() {
                            selection.delete_range(
                                primary_selection.min(),
                                primary_selection.max(),
                                false,
                            );
                        }
                    }

                    selection.add_region(occ);
                    self.set_selection(text, selection);
                }
                _ => {}
            }
        }
    }

    pub(super) fn do_set_replace(&mut self, chars: String, preserve_case: bool) {
        self.replace = Some(Replace { chars, preserve_case });
        self.replace_changed = true;
    }

    pub(super) fn do_selection_for_replace(&mut self, text: &Rope) {
        // set primary selection or word under current cursor as replacement string
        let replacement = match self.primary_sel_region() {
            Some(region) => {
                if !region.is_caret() {
                    text.slice_to_cow(region.min()..region.max())
                } else {
                    let (start, end) = {
                        let mut word_cursor = WordCursor::new(text, region.max());
                        word_cursor.select_word()
                    };
                    text.slice_to_cow(start..end)
                }
            }
            _ => return,
        };

        self.set_dirty(text);
        self.do_set_replace(replacement.into_owned(), false);
    }

    pub fn get_caret_offset(&self) -> Option<usize> {
        match self.selection.len() {
            1 if self.selection[0].is_caret() => {
                let offset = self.selection[0].start;
                Some(offset)
            }
            _ => None,
        }
    }
}
