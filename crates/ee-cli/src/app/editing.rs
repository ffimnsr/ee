//! `impl App` methods: editing domain.
use super::*;

impl App {
    pub(super) fn apply_case_transform(&mut self, method: &'static str) {
        let count = self.input_state.count() as usize;
        let had_visual = self.mode.is_visual();
        if !had_visual && !self.select_chars_from_cursor(count) {
            return;
        }
        let _ = self.backend.send_edit(method, json!([]));
        self.push_change();
        let _ = self.backend.send_edit("collapse_selections", json!([]));
        if had_visual {
            self.enter_normal_mode();
        }
    }
    pub(super) fn yank_selection(&mut self) {
        let count = self.input_state.count() as usize;
        let had_visual = self.mode.is_visual();
        if !had_visual && !self.select_chars_from_cursor(count) {
            return;
        }
        let reg = self.take_register();
        let text = self.selected_text_preview(false);
        self.registers.yank(&reg, text, false);
        let _ = self.backend.send_edit("collapse_selections", json!([]));
        if had_visual {
            self.enter_normal_mode();
        }
    }
    pub(super) fn yank_selection_to_register(&mut self, reg: RegisterName) {
        let count = self.input_state.count() as usize;
        let had_visual = self.mode.is_visual();
        let has_selection = self
            .backend
            .selections_preview()
            .map(|selections| selections.iter().any(|selection| selection.start != selection.end))
            .unwrap_or(false);
        if !had_visual && !has_selection && !self.select_chars_from_cursor(count) {
            return;
        }

        let text = self.selected_text_preview(false);
        if text.is_empty() {
            return;
        }

        self.registers.yank(&reg, text, false);
        let _ = self.backend.send_edit("collapse_selections", json!([]));
        if had_visual {
            self.enter_normal_mode();
        }
    }
    pub(super) fn apply_selection_edit(&mut self, method: &'static str, push_change: bool) {
        let had_visual = self.mode.is_visual();
        if !had_visual {
            let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
            let _ =
                self.backend.send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
        }
        let _ = self.backend.send_edit(method, json!([]));
        if push_change {
            self.push_change();
        }
        let _ = self.backend.send_edit("collapse_selections", json!([]));
        if had_visual {
            self.enter_normal_mode();
        }
    }
    pub(super) fn format_selections(&mut self) {
        if self.backend.format_document().is_ok() {
            self.push_change();
            if self.mode.is_visual() {
                self.enter_normal_mode();
            }
        }
    }
    pub(super) fn extend_line_below(&mut self) {
        let _ = self.backend.extend_line_below(self.input_state.count() as usize);
    }
    pub(super) fn extend_to_line_bounds(&mut self) {
        let _ = self.backend.extend_to_line_bounds();
    }
    pub(super) fn shrink_to_line_bounds(&mut self) {
        let _ = self.backend.shrink_to_line_bounds();
    }
    pub(super) fn filter_selections_from_search(&mut self, remove: bool) {
        let Some(pattern) = self.search_pattern.clone() else {
            self.backend.status_message = Some(format!(
                "{}: search pattern required",
                if remove { "remove_selections" } else { "keep_selections" }
            ));
            return;
        };
        self.filter_selections(&pattern, remove);
    }
    pub(super) fn filter_selections(&mut self, pattern: &str, remove: bool) {
        if regex::Regex::new(pattern).is_err() {
            self.backend.status_message = Some(format!(
                "{}: invalid regex",
                if remove { "remove_selections" } else { "keep_selections" }
            ));
            return;
        }

        let filtered = match self.backend.filter_selections_preview(pattern, remove) {
            Ok(filtered) => filtered,
            Err(err) => {
                self.backend.status_message = Some(format!(
                    "{}: {err}",
                    if remove { "remove_selections" } else { "keep_selections" }
                ));
                return;
            }
        };

        if filtered.is_empty() {
            self.backend.status_message = Some("no selections remaining".to_owned());
            return;
        }

        if self.backend.set_selections(&filtered).is_ok()
            && filtered.iter().any(|range| range.start != range.end)
        {
            self.mode = Mode::Visual;
        }
    }
    pub(super) fn join_selections(&mut self, select_space: bool) {
        if self.backend.join_selections(select_space).is_ok() {
            self.push_change();
        }
    }
    pub(super) fn dedup_selected_or_all_lines(&mut self) -> Result<String, String> {
        self.transform_selected_or_all_lines("dedup", |lines| {
            let mut seen = std::collections::HashSet::new();
            lines.retain(|line| seen.insert(line.clone()));
        })
    }
    pub(super) fn dedup_line_range(
        &mut self,
        start_line: usize,
        end_line: usize,
    ) -> Result<String, String> {
        self.transform_line_range("dedup", start_line, end_line, |lines| {
            let mut seen = std::collections::HashSet::new();
            lines.retain(|line| seen.insert(line.clone()));
        })
    }
    pub(super) fn delete_selection(&mut self, yank: bool, enter_insert: bool) {
        let count = self.input_state.count() as usize;
        let had_visual = self.mode.is_visual();
        if !had_visual && !self.select_chars_from_cursor(count) {
            return;
        }
        if yank {
            let reg = self.take_register();
            let text = self.selected_text_preview(false);
            self.registers.delete(&reg, text, false);
        }
        self.record_edit("delete_forward", json!([]));
        self.push_change();
        if had_visual {
            self.enter_normal_mode();
        }
        if enter_insert {
            self.mode = Mode::Insert;
        }
    }
    pub(super) fn replace_with_char(&mut self, ch: char) {
        let repeat = if self.mode.is_visual() {
            self.selected_text_preview(false)
                .chars()
                .filter(|current| *current != '\n')
                .count()
                .max(1)
        } else {
            self.input_state.count() as usize
        };
        if !self.mode.is_visual() && !self.select_chars_from_cursor(repeat) {
            return;
        }
        let _ = self.backend.send_edit("delete_forward", json!([]));
        let _ = self
            .backend
            .send_edit("insert", json!({ "chars": ch.to_string().repeat(repeat.max(1)) }));
        self.push_change();
        if self.mode.is_visual() {
            self.enter_normal_mode();
        }
    }
    pub(super) fn replace_with_yanked(&mut self) {
        let reg = self.take_register();
        let text = self.registers.get(&reg);
        if text.is_empty() {
            return;
        }
        let count = self.input_state.count() as usize;
        if !self.mode.is_visual() && !self.select_chars_from_cursor(count) {
            return;
        }
        let _ = self.backend.send_edit("delete_forward", json!([]));
        let _ = self.backend.send_edit("paste_register", json!({ "chars": text, "before": true }));
        self.push_change();
        if self.mode.is_visual() {
            self.enter_normal_mode();
        }
    }
    pub(super) fn replace_selections_with_register(&mut self, reg: RegisterName) {
        let text = self.registers.get(&reg);
        if text.is_empty() {
            return;
        }

        let count = self.input_state.count() as usize;
        let had_visual = self.mode.is_visual();
        let has_selection = self
            .backend
            .selections_preview()
            .map(|selections| selections.iter().any(|selection| selection.start != selection.end))
            .unwrap_or(false);
        if !had_visual && !has_selection && !self.select_chars_from_cursor(count) {
            return;
        }

        let _ = self.backend.send_edit("delete_forward", json!([]));
        let _ = self.backend.send_edit("paste_register", json!({ "chars": text, "before": true }));
        self.push_change();
        if had_visual {
            self.enter_normal_mode();
        }
    }
    pub(super) fn select_chars_from_cursor(&mut self, count: usize) -> bool {
        let selections = match self.backend.select_chars_preview(count.max(1)) {
            Ok(selections) => selections,
            Err(_) => return false,
        };
        if selections.is_empty() {
            return false;
        }
        self.backend.set_selections(&selections).is_ok()
    }
    pub(super) fn ensure_editable_selections(&mut self) -> Result<Vec<SelectionRange>, String> {
        self.backend
            .sync_pending_events()
            .map_err(|err| format!("selection preview failed: {err}"))?;
        let mut selections = self
            .backend
            .selections_preview()
            .map_err(|err| format!("selection preview failed: {err}"))?;
        if selections.is_empty()
            || selections.iter().all(|selection| selection.start == selection.end)
        {
            if !self.select_chars_from_cursor(1) {
                return Err(String::from("no selection available"));
            }
            self.backend
                .sync_pending_events()
                .map_err(|err| format!("selection preview failed: {err}"))?;
            selections = self
                .backend
                .selections_preview()
                .map_err(|err| format!("selection preview failed: {err}"))?;
        }
        if selections.is_empty() {
            Err(String::from("no selection available"))
        } else {
            Ok(selections)
        }
    }
    pub(super) fn primary_selection_preview(&mut self) -> Result<Option<SelectionRange>, String> {
        let selections = self
            .backend
            .selections_preview()
            .map_err(|err| format!("selection preview failed: {err}"))?;
        if selections.is_empty() {
            return Ok(None);
        }

        let cursor = self.active_cursor_offset();
        Ok(selections
            .into_iter()
            .find(|selection| {
                let start = selection.start.min(selection.end);
                let end = selection.start.max(selection.end);
                selection.end == cursor
                    || selection.start == cursor
                    || (start <= cursor && cursor <= end)
            })
            .or(Some(SelectionRange { start: cursor, end: cursor })))
    }
    pub(super) fn primary_selection_text(&mut self) -> Result<String, String> {
        let Some(selection) = self.primary_selection_preview()? else {
            return Ok(String::new());
        };
        let start = selection.start.min(selection.end);
        let end = selection.start.max(selection.end);
        if start == end {
            return Ok(String::new());
        }

        let buffer = self.current_buffer_text();
        buffer
            .get(start..end)
            .map(str::to_owned)
            .ok_or_else(|| String::from("primary selection range is invalid"))
    }
    pub(super) fn yank_main_selection_to_register(&mut self, reg: RegisterName) {
        let count = self.input_state.count() as usize;
        let had_visual = self.mode.is_visual();
        let initial = self.primary_selection_text().unwrap_or_default();
        if !had_visual && initial.is_empty() && !self.select_chars_from_cursor(count) {
            return;
        }

        let text = match self.primary_selection_text() {
            Ok(text) => text,
            Err(message) => {
                self.backend.status_message = Some(message);
                return;
            }
        };
        if text.is_empty() {
            return;
        }

        self.registers.yank(&reg, text, false);
        let _ = self.backend.send_edit("collapse_selections", json!([]));
        if had_visual {
            self.enter_normal_mode();
        }
    }
    pub(super) fn current_buffer_text(&self) -> String {
        self.backend.whole_text().unwrap_or_default()
    }
    pub(super) fn line_start_offset(&self, line: usize) -> usize {
        self.backend.line_start_offset(line).unwrap_or(0)
    }
    pub(super) fn replace_line_block(
        &mut self,
        start_line: usize,
        end_line: usize,
        lines: &[String],
    ) -> Result<(), String> {
        self.backend
            .replace_line_range(start_line, end_line, lines)
            .map_err(|err| format!("replace failed: {err}"))
    }
    pub(super) fn transform_selected_or_all_lines<F>(
        &mut self,
        label: &str,
        mut transform: F,
    ) -> Result<String, String>
    where
        F: FnMut(&mut Vec<String>),
    {
        let selected_text = self
            .backend
            .selected_text_preview(false)
            .map_err(|err| format!("{label}: selection preview failed: {err}"))?;
        let selected = self
            .backend
            .selected_text_preview(true)
            .map_err(|err| format!("{label}: selection preview failed: {err}"))?;

        let changed = if !selected_text.is_empty() {
            let original: Vec<String> = selected.lines().map(str::to_owned).collect();
            let mut updated = original.clone();
            transform(&mut updated);
            if original == updated {
                false
            } else {
                let _ = self.backend.send_edit("extend_to_line_bounds", json!([]));
                let expanded = self
                    .backend
                    .selected_text_preview(false)
                    .map_err(|err| format!("{label}: selection preview failed: {err}"))?;
                let mut replacement = updated.join("\n");
                if expanded.ends_with('\n') {
                    replacement.push('\n');
                }
                let _ = self.backend.send_edit("delete_forward", json!([]));
                if !replacement.is_empty() {
                    let _ = self.backend.send_edit("insert", json!({ "chars": replacement }));
                }
                true
            }
        } else {
            if self.backend.line_count() == 0 {
                return Ok(format!("{label}: no lines"));
            }

            self.backend
                .sync_pending_events_for_whole_document()
                .map_err(|err| format!("{label}: line sync failed: {err}"))?;

            // Whole-buffer policy-allowed: whole-document transform syncs full mirror first.
            let original = self.backend.lines.clone();
            let mut updated = original.clone();
            transform(&mut updated);
            if original == updated {
                false
            } else {
                self.replace_line_block(0, original.len().saturating_sub(1), &updated)?;
                true
            }
        };

        if changed {
            self.push_change();
            Ok(format!("{label}: applied"))
        } else {
            Ok(format!("{label}: no changes"))
        }
    }
    pub(super) fn transform_line_range<F>(
        &mut self,
        label: &str,
        start_line: usize,
        end_line: usize,
        mut transform: F,
    ) -> Result<String, String>
    where
        F: FnMut(&mut Vec<String>),
    {
        if self.backend.line_count() == 0 {
            return Ok(format!("{label}: no lines"));
        }

        let last_line = self.backend.line_count().saturating_sub(1);
        let start_line = start_line.min(last_line);
        let end_line = end_line.min(last_line).max(start_line);
        let original = self
            .backend
            .line_range_owned(start_line, end_line)
            .ok_or_else(|| format!("{label}: requested lines are not loaded"))?;
        let mut updated = original.clone();
        transform(&mut updated);

        if original == updated {
            return Ok(format!("{label}: no changes"));
        }

        self.replace_line_block(start_line, end_line, &updated)?;

        self.push_change();
        Ok(format!("{label}: applied"))
    }
    pub(super) fn replace_range_with_text(
        &mut self,
        range: SelectionRange,
        text: &str,
    ) -> Result<(), String> {
        let is_non_empty = range.start != range.end;
        self.backend
            .set_selections(std::slice::from_ref(&range))
            .map_err(|err| format!("replace failed: {err}"))?;
        self.backend.sync_pending_events().map_err(|err| format!("replace failed: {err}"))?;
        if is_non_empty {
            let _ = self.backend.send_edit("delete_forward", json!([]));
        }
        if !text.is_empty() {
            let _ = self.backend.send_edit("insert", json!({ "chars": text }));
        }
        Ok(())
    }
    pub(super) fn insert_at_offset(&mut self, offset: usize, text: &str) -> Result<(), String> {
        self.backend
            .set_selections(&[SelectionRange { start: offset, end: offset }])
            .map_err(|err| format!("insert failed: {err}"))?;
        self.backend.sync_pending_events().map_err(|err| format!("insert failed: {err}"))?;
        if !text.is_empty() {
            let _ = self.backend.send_edit("insert", json!({ "chars": text }));
        }
        Ok(())
    }
    /// Delete lines `start..=end` (0-based, inclusive).
    pub(super) fn delete_line_range(&mut self, start: usize, end: usize) {
        let _ = self.backend.delete_line_range(start, end);
        self.push_change();
    }
    pub(super) fn add_newline_below(&mut self) {
        if self.backend.add_newline_below().is_ok() {
            self.push_change();
        }
    }
    pub(super) fn add_newline_above(&mut self) {
        if self.backend.add_newline_above().is_ok() {
            self.push_change();
        }
    }
    /// Yank lines `start..=end` (0-based, inclusive) into the active register.
    pub(super) fn yank_line_range(&mut self, start: usize, end: usize) {
        let reg = self.take_register();
        self.yank_line_range_into_register(start, end, reg);
    }
    pub(super) fn yank_line_range_into_register(
        &mut self,
        start: usize,
        end: usize,
        reg: RegisterName,
    ) {
        let line_count = self.backend.line_count();
        if line_count == 0 {
            return;
        }
        let start = start.min(line_count.saturating_sub(1));
        let end = end.min(line_count.saturating_sub(1));
        let mut text = String::new();
        let Some(lines) = self.backend.line_range_owned(start, end) else {
            return;
        };
        for line in lines {
            text.push_str(&line);
            text.push('\n');
        }
        self.registers.yank(&reg, text, false);
        let count = end.saturating_sub(start) + 1;
        self.backend.status_message = Some(format!("{count} line(s) yanked"));
    }
}
