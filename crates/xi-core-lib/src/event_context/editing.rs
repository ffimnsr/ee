use std::ops::Range;

use regex::Regex;
use serde_json::json;
use xi_rope::{Interval, LinesMetric};
use xi_rpc::RemoteError;

use crate::edit_ops;
use crate::editor::EditType;
use crate::lang_features;
use crate::plugins::rpc::SelectionRange;
use crate::rpc::LineReplacement;
use crate::selection::{InsertDrift, SelRegion, Selection};
use crate::tree_sitter_support::syntax_feature_availability;

use super::{
    EventContext, apply_line_replacements, block_text, collect_join_operations,
    compute_line_replacements, display_col_to_byte, extend_line_above_selection,
    extend_line_below_selection, extend_to_line_bounds_selection, line_content_end, line_text,
    replace_interval_with_text, replace_line_range, select_chars_selection,
    select_line_above_selection, select_line_below_selection, selected_text_from_store,
    selection_matches_regex, shrink_to_line_bounds_selection,
};

impl<'a> EventContext<'a> {
    // ── Line / selection helpers ──

    pub(super) fn do_request_lines(&mut self, first: usize, last: usize) {
        let mut view = self.view.borrow_mut();
        let ed = self.editor.borrow();
        let file_path = self.info.map(|info| info.path.as_path());
        let capabilities = syntax_feature_availability(
            Some(self.language.as_ref()),
            file_path,
            ed.document_mode(),
        );
        let syntax_enabled = capabilities.syntax_spans && !ed.is_vlf();
        view.request_lines(
            ed.get_buffer(),
            self.client,
            first,
            last,
            ed.is_pristine(),
            self.language.as_ref(),
            syntax_enabled,
        )
    }

    fn selected_line_ranges(&mut self) -> Vec<(usize, usize)> {
        let ed = self.editor.borrow();
        let mut prev_range: Option<Range<usize>> = None;
        let mut line_ranges = Vec::new();
        for region in self.view.borrow().sel_regions().iter() {
            let start = ed.get_buffer().line_of_offset(region.min());
            let end = ed.get_buffer().line_of_offset(region.max()) + 1;
            let line_range = start..end;
            let prev = prev_range.take();
            match (prev, line_range) {
                (None, range) => prev_range = Some(range),
                (Some(ref prev), ref range) if range.start <= prev.end => {
                    let combined =
                        Range { start: prev.start.min(range.start), end: prev.end.max(range.end) };
                    prev_range = Some(combined);
                }
                (Some(prev), range) => {
                    line_ranges.push((prev.start, prev.end));
                    prev_range = Some(range);
                }
            }
        }

        if let Some(prev) = prev_range {
            line_ranges.push((prev.start, prev.end));
        }

        line_ranges
    }

    fn selected_offset_ranges(&self) -> Vec<(usize, usize)> {
        let mut ranges = self
            .view
            .borrow()
            .sel_regions()
            .iter()
            .map(|region| (region.min(), region.max()))
            .collect::<Vec<_>>();
        ranges.sort_unstable();
        ranges.dedup();
        ranges
    }

    // ── Toggle comment ──

    pub(super) fn do_toggle_comment(&mut self) {
        let line_ranges = self.selected_line_ranges();
        let selection_ranges = self.selected_offset_ranges();
        let lang_name = self.language.as_ref();
        let maybe_delta = {
            let ed = self.editor.borrow();
            let file_path = self.info.map(|info| info.path.as_path());
            let capabilities =
                syntax_feature_availability(Some(lang_name), file_path, ed.document_mode());
            let line_delta = capabilities
                .line_comments
                .then(|| lang_features::toggle_comment(ed.get_buffer(), &line_ranges, lang_name))
                .flatten();
            line_delta.or_else(|| {
                capabilities
                    .block_comments
                    .then(|| {
                        lang_features::toggle_block_comment(
                            ed.get_buffer(),
                            &selection_ranges,
                            lang_name,
                        )
                    })
                    .flatten()
            })
        };
        if let Some(delta) = maybe_delta {
            self.editor.borrow_mut().apply_direct_delta(EditType::Other, delta);
        }
    }

    pub(super) fn do_toggle_line_comment(&mut self) {
        let line_ranges = self.selected_line_ranges();
        let lang_name = self.language.as_ref();
        let maybe_delta = {
            let ed = self.editor.borrow();
            let file_path = self.info.map(|info| info.path.as_path());
            let capabilities =
                syntax_feature_availability(Some(lang_name), file_path, ed.document_mode());
            capabilities
                .line_comments
                .then(|| lang_features::toggle_comment(ed.get_buffer(), &line_ranges, lang_name))
                .flatten()
        };
        if let Some(delta) = maybe_delta {
            self.editor.borrow_mut().apply_direct_delta(EditType::Other, delta);
        }
    }

    pub(super) fn do_toggle_block_comment(&mut self) {
        let selection_ranges = self.selected_offset_ranges();
        let lang_name = self.language.as_ref();
        let maybe_delta = {
            let ed = self.editor.borrow();
            let file_path = self.info.map(|info| info.path.as_path());
            let capabilities =
                syntax_feature_availability(Some(lang_name), file_path, ed.document_mode());
            capabilities
                .block_comments
                .then(|| {
                    lang_features::toggle_block_comment(
                        ed.get_buffer(),
                        &selection_ranges,
                        lang_name,
                    )
                })
                .flatten()
        };
        if let Some(delta) = maybe_delta {
            self.editor.borrow_mut().apply_direct_delta(EditType::Other, delta);
        }
    }

    // ── Reindent ──

    pub(super) fn begin_async_reindent(&mut self) {
        use crate::tabs::WHOLE_SCAN_IDLE_MASK;

        let line_ranges = self.selected_line_ranges();
        let lang_name = self.language.as_ref().to_string();
        let indent_str = if self.config.translate_tabs_to_spaces {
            " ".repeat(self.config.tab_size)
        } else {
            "\t".to_string()
        };

        let file_path = self.info.map(|info| info.path.as_path());
        let capabilities = syntax_feature_availability(
            Some(lang_name.as_str()),
            file_path,
            self.editor.borrow().document_mode(),
        );
        if !capabilities.reindent {
            self.dispatch_command_to_plugins("reindent", &json!(line_ranges));
            return;
        }

        let text = self.editor.borrow().get_buffer().clone();
        let _ = self.editor.borrow_mut().whole_scan_task.start_reindent(
            text,
            line_ranges,
            lang_name,
            indent_str,
        );

        self.client.alert("Reindenting in background…");
        let view_id_usize: usize = self.view_id.into();
        self.client.schedule_idle(WHOLE_SCAN_IDLE_MASK | view_id_usize);
    }

    pub(crate) fn apply_whole_scan_result(&mut self) {
        use crate::whole_scan::WholeScanResult;

        let maybe_result = self.editor.borrow_mut().whole_scan_task.poll();
        match maybe_result {
            Some(WholeScanResult::Reindent(Some(delta))) => {
                self.editor.borrow_mut().apply_direct_delta(EditType::Other, delta);
                self.after_edit("core");
                self.render_if_needed();
                self.client.alert("Reindent complete.");
            }
            Some(WholeScanResult::Reindent(None)) => {}
            None => {
                let view_id_usize: usize = self.view_id.into();
                self.client.schedule_idle(crate::tabs::WHOLE_SCAN_IDLE_MASK | view_id_usize);
            }
        }
    }

    // ── Delete operations ──

    pub(super) fn do_delete_line_range(&mut self, start_line: usize, end_line: usize) {
        let start_offset = {
            let editor = self.editor.borrow();
            let text = editor.get_buffer();
            let total_lines = text.measure::<LinesMetric>() + 1;
            let line = start_line.min(total_lines.saturating_sub(1));
            text.offset_of_line(line)
        };
        self.with_view(|view, text| view.set_selection(text, SelRegion::caret(start_offset)));
        let delta = {
            let editor = self.editor.borrow();
            edit_ops::delete_line_range(editor.get_buffer(), start_line, end_line)
        };
        if !delta.is_identity() {
            self.editor.borrow_mut().apply_direct_delta(EditType::Delete, delta);
        }
    }

    pub(super) fn do_delete_block(
        &mut self,
        start_line: usize,
        end_line: usize,
        left_col: usize,
        right_col: usize,
    ) {
        let delta = {
            let editor = self.editor.borrow();
            edit_ops::delete_block(editor.get_buffer(), start_line, end_line, left_col, right_col)
        };
        if !delta.is_identity() {
            self.editor.borrow_mut().apply_direct_delta(EditType::Delete, delta);
        }
    }

    pub(super) fn do_replay_block_insert(
        &mut self,
        start_line: usize,
        end_line: usize,
        column: usize,
        text: &str,
        append: bool,
    ) {
        let delta = {
            let editor = self.editor.borrow();
            edit_ops::replay_block_insert(
                editor.get_buffer(),
                start_line,
                end_line,
                column,
                text,
                append,
            )
        };
        if !delta.is_identity() {
            self.editor.borrow_mut().apply_direct_delta(EditType::InsertChars, delta);
        }
    }

    // ── Substitute / replace ──

    pub(crate) fn preview_substitute(
        &self,
        start_line: usize,
        end_line: usize,
        pattern: &str,
        replacement: &str,
        global: bool,
        case_sensitive: bool,
    ) -> Result<Vec<LineReplacement>, RemoteError> {
        if pattern.is_empty() {
            return Err(RemoteError::custom(400, "substitute: empty pattern", None));
        }

        let editor = self.editor.borrow();
        compute_line_replacements(
            editor.get_buffer(),
            start_line,
            end_line,
            pattern,
            replacement,
            global,
            case_sensitive,
        )
    }

    pub(super) fn do_apply_line_replacements(&mut self, replacements: &[LineReplacement]) {
        if replacements.is_empty() {
            return;
        }

        let delta = {
            let editor = self.editor.borrow();
            apply_line_replacements(editor.get_buffer(), replacements)
        };
        if !delta.is_identity() {
            self.editor.borrow_mut().apply_direct_delta(EditType::Other, delta);
        }
    }

    pub(super) fn do_replace_line_range(
        &mut self,
        start_line: usize,
        end_line: usize,
        lines: &[String],
    ) {
        let delta = {
            let editor = self.editor.borrow();
            replace_line_range(editor.get_buffer(), start_line, end_line, lines)
        };
        if !delta.is_identity() {
            self.editor.borrow_mut().apply_direct_delta(EditType::Other, delta);
        }
    }

    // ── Set / preview selections ──

    pub(super) fn do_set_selections(&mut self, selections: &[SelectionRange]) -> Option<Selection> {
        if selections.is_empty() {
            return None;
        }

        let mut selection = Selection::new();
        for range in selections {
            selection.add_region(SelRegion::new(range.start, range.end));
        }
        Some(selection)
    }

    pub(crate) fn preview_filter_selections(
        &mut self,
        pattern: &str,
        remove: bool,
    ) -> Result<Vec<SelectionRange>, RemoteError> {
        if pattern.is_empty() {
            return Err(RemoteError::custom(400, "filter_selections: empty pattern", None));
        }

        let regex = Regex::new(pattern)
            .map_err(|_| RemoteError::custom(400, "filter_selections: invalid regex", None))?;

        Ok(self.with_view(|view, text| {
            view.sel_regions()
                .iter()
                .copied()
                .filter(|region| selection_matches_regex(text, *region, &regex) != remove)
                .map(|region| SelectionRange { start: region.start, end: region.end })
                .collect()
        }))
    }

    pub(crate) fn preview_selected_text(&mut self, linewise: bool) -> String {
        let editor = self.editor.borrow();
        let view = self.view.borrow();

        if let Some(vlf_store) = editor.vlf_store.as_ref() {
            return selected_text_from_store(vlf_store.as_ref(), view.sel_regions(), linewise);
        }

        let store = editor.text_store_snapshot();
        selected_text_from_store(&store, view.sel_regions(), linewise)
    }

    pub(crate) fn preview_selections(&mut self) -> Vec<SelectionRange> {
        self.with_view(|view, _| {
            view.sel_regions()
                .iter()
                .map(|region| SelectionRange { start: region.start, end: region.end })
                .collect()
        })
    }

    pub(crate) fn preview_block_text(
        &mut self,
        start_line: usize,
        end_line: usize,
        left_col: usize,
        right_col: usize,
    ) -> String {
        let editor = self.editor.borrow();
        block_text(editor.get_buffer(), start_line, end_line, left_col, right_col)
    }

    pub(crate) fn preview_select_chars(&mut self, count: usize) -> Vec<SelectionRange> {
        self.with_view(|view, text| {
            select_chars_selection(text, view.sel_regions(), count.max(1))
                .iter()
                .map(|region| SelectionRange { start: region.start, end: region.end })
                .collect()
        })
    }

    // ── Goto column ──

    pub(super) fn do_goto_column(
        &mut self,
        display_col: usize,
        modify_selection: bool,
    ) -> Option<Selection> {
        self.with_view(|view, text| {
            let region = view.primary_sel_region()?;
            let line = text.line_of_offset(region.end);
            let line_text = line_text(text, line);
            let target_col = display_col_to_byte(&line_text, display_col);
            let target_offset = text.offset_of_line(line) + target_col;

            if modify_selection {
                let mut selection = Selection::new();
                if let Some((last, rest)) = view.sel_regions().split_last() {
                    for region in rest {
                        selection.add_region(*region);
                    }
                    selection.add_region(
                        SelRegion::new(last.start, target_offset)
                            .with_horiz(None)
                            .with_affinity(last.affinity),
                    );
                } else {
                    selection.add_region(SelRegion::caret(target_offset));
                }
                Some(selection)
            } else {
                Some(Selection::new_simple(SelRegion::caret(target_offset)))
            }
        })
    }

    // ── Newline operations ──

    pub(super) fn do_add_newline_below(&mut self) -> Option<Selection> {
        let (insert_offset, caret_offset) = self.with_view(|view, text| {
            let region = view.primary_sel_region()?;
            let line = text.line_of_offset(region.end);
            let insert_offset = line_content_end(text, line);
            Some((insert_offset, insert_offset + self.config.line_ending.len()))
        })?;

        let delta = replace_interval_with_text(
            &self.editor.borrow().get_buffer().clone(),
            Interval::new(insert_offset, insert_offset),
            &self.config.line_ending,
        );
        self.editor.borrow_mut().apply_direct_delta(EditType::InsertNewline, delta);
        Some(Selection::new_simple(SelRegion::caret(caret_offset)))
    }

    pub(super) fn do_add_newline_above(&mut self) -> Option<Selection> {
        let insert_offset = self.with_view(|view, text| {
            let region = view.primary_sel_region()?;
            let line = text.line_of_offset(region.end);
            Some(text.offset_of_line(line))
        })?;

        let delta = replace_interval_with_text(
            &self.editor.borrow().get_buffer().clone(),
            Interval::new(insert_offset, insert_offset),
            &self.config.line_ending,
        );
        self.editor.borrow_mut().apply_direct_delta(EditType::InsertNewline, delta);
        Some(Selection::new_simple(SelRegion::caret(insert_offset)))
    }

    // ── Join selections ──

    pub(super) fn do_join_selections(&mut self, select_space: bool) -> Option<Selection> {
        let operations =
            self.with_view(|view, text| collect_join_operations(text, view.sel_regions()));
        if operations.is_empty() {
            return None;
        }

        let mut final_selection = Selection::new();
        for operation in operations {
            let delta = replace_interval_with_text(
                &self.editor.borrow().get_buffer().clone(),
                Interval::new(operation.start_offset, operation.end_offset),
                &operation.joined,
            );
            final_selection = final_selection.apply_delta(&delta, false, InsertDrift::Default);
            self.editor.borrow_mut().apply_direct_delta(EditType::Other, delta);

            if select_space {
                for offset in operation.space_offsets {
                    final_selection.add_region(SelRegion::new(offset, offset + 1));
                }
            } else {
                final_selection
                    .add_region(SelRegion::caret(operation.start_offset + operation.joined.len()));
            }
        }

        if !select_space || !final_selection.is_empty() { Some(final_selection) } else { None }
    }

    // ── Extend / select lines ──

    pub(super) fn do_extend_line_below(&mut self, count: usize) -> Option<Selection> {
        self.with_view(|view, text| {
            let selection = extend_line_below_selection(text, view.sel_regions(), count.max(1));
            (!selection.is_empty()).then_some(selection)
        })
    }

    pub(super) fn do_extend_line_above(&mut self) -> Option<Selection> {
        self.with_view(|view, text| {
            let selection = extend_line_above_selection(text, view.sel_regions());
            (!selection.is_empty()).then_some(selection)
        })
    }

    pub(super) fn do_select_line_above(&mut self) -> Option<Selection> {
        self.with_view(|view, text| {
            let selection = select_line_above_selection(text, view.sel_regions());
            (!selection.is_empty()).then_some(selection)
        })
    }

    pub(super) fn do_select_line_below(&mut self) -> Option<Selection> {
        self.with_view(|view, text| {
            let selection = select_line_below_selection(text, view.sel_regions());
            (!selection.is_empty()).then_some(selection)
        })
    }

    pub(super) fn do_extend_to_line_bounds(&mut self) -> Option<Selection> {
        self.with_view(|view, text| {
            let selection = extend_to_line_bounds_selection(text, view.sel_regions());
            (!selection.is_empty()).then_some(selection)
        })
    }

    pub(super) fn do_shrink_to_line_bounds(&mut self) -> Option<Selection> {
        self.with_view(|view, text| {
            let selection = shrink_to_line_bounds_selection(text, view.sel_regions());
            (!selection.is_empty()).then_some(selection)
        })
    }
}
