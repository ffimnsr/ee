use std::path::Path;

use crate::edit_types::SpecialEvent;
use crate::editor::EditType;
use crate::object::{self, SyntaxNavigationAction, SyntaxSelectionAction};
use crate::selection::{SelRegion, Selection};
use crate::text_store::{
    ByteOffset, ByteRange, LineLookup, LogicalLine, TextChunkResult, TextStore,
};

use super::EventContext;

// ── VLF free functions ──

#[cfg(test)]
pub(crate) fn vlf_exact_line_byte(
    store: &dyn TextStore,
    line: u64,
) -> Result<ByteOffset, LineLookup> {
    match store.line_to_byte(LogicalLine(line)) {
        LineLookup::Exact(byte) => Ok(byte),
        lookup => Err(lookup),
    }
}

#[cfg(test)]
pub(crate) fn vlf_exact_logical_line_count(store: &crate::vlf::store::VlfStore) -> Option<u64> {
    store.exact_logical_line_count_streaming().ok()
}

pub(crate) fn vlf_read_text_range(
    store: &dyn TextStore,
    range: ByteRange,
) -> Option<(String, ByteRange)> {
    if let TextChunkResult::Ready(chunk) = store.read_byte_range(range) {
        return Some((chunk.text, chunk.byte_range));
    }

    let mut text = String::new();
    let mut decoded_start = None;
    let mut decoded_end = range.start;
    for result in store.iter_chunks(range) {
        let TextChunkResult::Ready(chunk) = result else {
            return None;
        };
        decoded_start.get_or_insert(chunk.byte_range.start);
        decoded_end = chunk.byte_range.end;
        text.push_str(&chunk.text);
    }

    Some((text, ByteRange { start: decoded_start.unwrap_or(range.start), end: decoded_end }))
}

pub(crate) fn vlf_read_exact_text_range(
    store: &dyn TextStore,
    range: ByteRange,
) -> Option<(String, ByteRange)> {
    let (text, decoded_range) = vlf_read_text_range(store, range)?;
    if decoded_range == range {
        return Some((text, range));
    }

    let start = usize::try_from(range.start.0.saturating_sub(decoded_range.start.0)).ok()?;
    let end = usize::try_from(range.end.0.saturating_sub(decoded_range.start.0)).ok()?;
    if end > text.len() || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
        return None;
    }

    Some((text[start..end].to_owned(), range))
}

// ── VLF feature name helpers ──

pub(crate) fn vlf_buffer_feature_name(cmd: &crate::edit_types::BufferEvent) -> &'static str {
    use crate::edit_types::BufferEvent;

    match cmd {
        BufferEvent::Delete { .. } | BufferEvent::Backspace => "delete",
        BufferEvent::Transpose => "transpose",
        BufferEvent::Undo => "undo",
        BufferEvent::Redo => "redo",
        BufferEvent::Uppercase | BufferEvent::Lowercase | BufferEvent::Capitalize => "transform",
        BufferEvent::Indent | BufferEvent::Outdent | BufferEvent::InsertTab => "indent",
        BufferEvent::InsertNewline | BufferEvent::Insert(_) => "insert",
        BufferEvent::Paste(_) | BufferEvent::PasteRegister { .. } | BufferEvent::Yank => "paste",
        BufferEvent::ReplaceNext | BufferEvent::ReplaceAll => "replace",
        BufferEvent::DuplicateLine => "duplicate-line",
        BufferEvent::IncreaseNumber | BufferEvent::DecreaseNumber => "number-change",
        BufferEvent::AlignSelections | BufferEvent::AlignIt { .. } => "align",
        BufferEvent::ExpandTabs { .. }
        | BufferEvent::ReflowLines { .. }
        | BufferEvent::SortLines { .. } => "linewise-transform",
        BufferEvent::RotateSelectionContentsBackward
        | BufferEvent::RotateSelectionContentsForward
        | BufferEvent::ReverseSelectionContents => "selection-rotation",
    }
}

pub(crate) fn vlf_special_feature_name(cmd: &SpecialEvent) -> Option<&'static str> {
    match cmd {
        SpecialEvent::DeleteLineRange { .. } => Some("delete-line"),
        SpecialEvent::DeleteBlock { .. } => Some("delete-block"),
        SpecialEvent::ReplayBlockInsert { .. } => Some("block-insert"),
        SpecialEvent::ReplaceLineRange { .. } => Some("replace-line-range"),
        SpecialEvent::ApplyLineReplacements { .. } => Some("replace"),
        SpecialEvent::AddNewlineAbove | SpecialEvent::AddNewlineBelow => Some("open-line"),
        SpecialEvent::JoinSelections { .. } => Some("join"),
        SpecialEvent::CommitUndoCheckpoint => Some("undo"),
        SpecialEvent::ToggleComment
        | SpecialEvent::ToggleLineComment
        | SpecialEvent::ToggleBlockComment => Some("comment"),
        SpecialEvent::Reindent => Some("reindent"),
        SpecialEvent::VlfReplaceRange { .. } => None,
        _ => None,
    }
}

// ── Store helper functions ──

pub(crate) fn selected_text_from_store(
    store: &dyn TextStore,
    regions: &[SelRegion],
    linewise: bool,
) -> String {
    let mut out = String::new();
    for region in regions {
        if region.is_caret() {
            continue;
        }

        if linewise {
            if let Some((start_line, end_line)) = selection_line_range_from_store(store, *region) {
                for line in start_line..=end_line {
                    if let Some(line_text) = line_text_from_store(store, line) {
                        out.push_str(&line_text);
                        out.push('\n');
                    }
                }
            }
        } else if let Some(text) =
            read_text_range_from_store(store, region.min() as u64, region.max() as u64)
        {
            out.push_str(&text);
        }
    }
    out
}

fn selection_line_range_from_store(store: &dyn TextStore, region: SelRegion) -> Option<(u64, u64)> {
    let start_line = store.byte_to_line(ByteOffset(region.min() as u64))?.0;
    let mut end_line = store.byte_to_line(ByteOffset(region.max() as u64))?.0;
    let end_line_start = exact_line_start(store, end_line)?;
    let end_col = (region.max() as u64).saturating_sub(end_line_start);
    if end_col == 0 && end_line > start_line {
        end_line = end_line.saturating_sub(1);
    }
    Some((start_line, end_line))
}

fn line_text_from_store(store: &dyn TextStore, line: u64) -> Option<String> {
    let start = exact_line_start(store, line)?;
    let end = match store.line_to_byte(LogicalLine(line.saturating_add(1))) {
        LineLookup::Exact(offset) => offset.0,
        LineLookup::OutOfRange => store.len_bytes(),
        LineLookup::Approximate(_) | LineLookup::Pending => return None,
    };
    let mut line_text = read_text_range_from_store(store, start, end)?;
    if line_text.ends_with('\n') {
        line_text.pop();
        if line_text.ends_with('\r') {
            line_text.pop();
        }
    }
    Some(line_text)
}

fn exact_line_start(store: &dyn TextStore, line: u64) -> Option<u64> {
    match store.line_to_byte(LogicalLine(line)) {
        LineLookup::Exact(offset) => Some(offset.0),
        _ => None,
    }
}

fn read_text_range_from_store(store: &dyn TextStore, start: u64, end: u64) -> Option<String> {
    match store.read_byte_range(ByteRange::new(start, end)) {
        TextChunkResult::Ready(chunk) => Some(chunk.text),
        TextChunkResult::Pending | TextChunkResult::Cancelled | TextChunkResult::Unsupported => {
            None
        }
    }
}

// ── VLF EventContext methods ──

impl<'a> EventContext<'a> {
    pub(crate) fn vlf_edit_dispatch_reason(&self, feature: &str, unsupported: bool) -> String {
        let editor = self.editor.borrow();
        let Some(store) = editor.vlf_store.as_ref() else {
            return format!("{feature} disabled in VLF");
        };
        match store.edit_permission() {
            crate::text_store::EditPermission::Allowed if unsupported => {
                format!("{feature} disabled in VLF: unsupported in sparse overlay mode")
            }
            crate::text_store::EditPermission::Allowed => {
                format!("{feature} disabled in VLF: sparse overlay edit path required")
            }
            crate::text_store::EditPermission::Forbidden { reason } => {
                format!("{feature} disabled in VLF: {reason}")
            }
        }
    }

    pub(crate) fn do_vlf_replace_range(
        &mut self,
        start_line: u64,
        start_col: u64,
        end_line: u64,
        end_col: u64,
        text: &str,
    ) -> Result<(), String> {
        let (range_start, range_end) = {
            let editor = self.editor.borrow();
            let Some(store) = editor.vlf_store.as_ref() else {
                return Err("vlf_replace_range: missing VLF store".to_string());
            };
            match store.edit_permission() {
                crate::text_store::EditPermission::Allowed => {}
                crate::text_store::EditPermission::Forbidden { reason } => {
                    return Err(format!("replace disabled in VLF: {reason}"));
                }
            }

            let start = self.vlf_position_to_byte(store.as_ref(), start_line, start_col)?;
            let end = self.vlf_position_to_byte(store.as_ref(), end_line, end_col)?;
            if start.0 <= end.0 { (start, end) } else { (end, start) }
        };

        {
            let mut editor = self.editor.borrow_mut();
            let edit_type = if !text.is_empty() && range_start != range_end {
                EditType::Other
            } else if !text.is_empty() {
                if text == "\n" { EditType::InsertNewline } else { EditType::InsertChars }
            } else {
                EditType::Delete
            };
            let ctx = editor
                .next_vlf_overlay_edit_context(edit_type)
                .ok_or_else(|| "vlf_replace_range: missing VLF edit context".to_string())?;
            let Some(store) = editor.vlf_store.as_ref() else {
                return Err("vlf_replace_range: missing VLF store".to_string());
            };

            if range_start.0 < range_end.0 {
                store
                    .apply_delete(ByteRange::new(range_start.0, range_end.0), ctx)
                    .map_err(|err| format!("vlf delete failed: {err}"))?;
            }
            if !text.is_empty() {
                store
                    .apply_insert(range_start.0, text, ctx)
                    .map_err(|err| format!("vlf insert failed: {err}"))?;
            }
            editor.commit_vlf_overlay_revision(ctx.revision_id);
        }

        let caret = ByteOffset(range_start.0.saturating_add(text.len() as u64));
        let (caret_line, caret_col) = {
            let editor = self.editor.borrow();
            let Some(store) = editor.vlf_store.as_ref() else {
                return Err("vlf_replace_range: missing VLF store".to_string());
            };
            self.vlf_byte_to_line_col(store.as_ref(), caret).unwrap_or((start_line, start_col))
        };

        self.view
            .borrow_mut()
            .set_vlf_selection(Selection::new_simple(SelRegion::caret(caret.0 as usize)));
        self.client.scroll_to(self.view_id, caret_line as usize, caret_col as usize);
        Ok(())
    }

    fn vlf_position_to_byte(
        &self,
        store: &dyn TextStore,
        line: u64,
        col: u64,
    ) -> Result<ByteOffset, String> {
        let line_start = match store.line_to_byte(LogicalLine(line)) {
            LineLookup::Exact(byte) => byte,
            LineLookup::Approximate(_) | LineLookup::Pending => {
                return Err(format!("vlf_replace_range: line {line} not resolved exactly yet"));
            }
            LineLookup::OutOfRange => {
                return Err(format!("vlf_replace_range: line {line} out of range"));
            }
        };
        let line_end = match store.line_to_byte(LogicalLine(line.saturating_add(1))) {
            LineLookup::Exact(byte) => byte,
            LineLookup::OutOfRange => ByteOffset(store.len_bytes()),
            LineLookup::Approximate(_) | LineLookup::Pending => {
                return Err(format!(
                    "vlf_replace_range: line {} end not resolved exactly yet",
                    line.saturating_add(1)
                ));
            }
        };
        let (line_text, _) =
            vlf_read_exact_text_range(store, ByteRange { start: line_start, end: line_end })
                .ok_or_else(|| format!("vlf_replace_range: failed to read line {line}"))?;
        let requested_col = usize::try_from(col)
            .map_err(|_| format!("vlf_replace_range: column {col} overflow"))?
            .min(line_text.len());
        let clamped_col = super::previous_char_boundary_in_text(&line_text, requested_col);
        Ok(ByteOffset(line_start.0.saturating_add(clamped_col as u64)))
    }

    fn vlf_byte_to_line_col(
        &self,
        store: &dyn TextStore,
        offset: ByteOffset,
    ) -> Option<(u64, u64)> {
        let line = store.byte_to_line(offset)?.0;
        let line_start = match store.line_to_byte(LogicalLine(line)) {
            LineLookup::Exact(byte) => byte,
            _ => return None,
        };
        Some((line, offset.0.saturating_sub(line_start.0)))
    }

    pub(crate) fn do_vlf_syntax_selection(
        &mut self,
        language_name: &str,
        file_path: Option<&Path>,
        action: SyntaxSelectionAction,
    ) -> Result<(), object::SyntaxSelectionError> {
        let editor = self.editor.borrow();
        let store =
            editor.vlf_store.as_ref().ok_or(object::SyntaxSelectionError::SyntaxTreeUnavailable)?;
        let window_range = self
            .current_vlf_semantic_range(store)
            .ok_or(object::SyntaxSelectionError::OutsideParsedRange)?;
        let window_text = self
            .current_vlf_semantic_window_text(store, window_range, language_name, file_path)
            .ok_or(object::SyntaxSelectionError::OutsideParsedRange)?;
        drop(editor);
        let mut view = self.view.borrow_mut();
        view.apply_vlf_syntax_selection(
            &window_text,
            window_range.start.0 as usize,
            language_name,
            file_path,
            action,
        )?;
        Ok(())
    }

    pub(crate) fn do_vlf_syntax_navigation(
        &mut self,
        language_name: &str,
        file_path: Option<&Path>,
        action: SyntaxNavigationAction,
    ) -> Result<(), object::SyntaxSelectionError> {
        let editor = self.editor.borrow();
        let store =
            editor.vlf_store.as_ref().ok_or(object::SyntaxSelectionError::SyntaxTreeUnavailable)?;
        let window_range = self
            .current_vlf_semantic_range(store)
            .ok_or(object::SyntaxSelectionError::OutsideParsedRange)?;
        let window_text = self
            .current_vlf_semantic_window_text(store, window_range, language_name, file_path)
            .ok_or(object::SyntaxSelectionError::OutsideParsedRange)?;
        drop(editor);
        let mut view = self.view.borrow_mut();
        view.apply_vlf_syntax_navigation(
            &window_text,
            window_range.start.0 as usize,
            language_name,
            file_path,
            action,
        )?;
        Ok(())
    }

    pub(crate) fn current_vlf_semantic_range(
        &self,
        store: &crate::vlf::store::VlfStore,
    ) -> Option<ByteRange> {
        let viewport = store.viewport_state();
        let requested = ByteRange { start: viewport.window_start, end: viewport.window_end };
        (!requested.is_empty()).then_some(requested)
    }

    pub(crate) fn current_vlf_semantic_window_text(
        &self,
        store: &crate::vlf::store::VlfStore,
        window_range: ByteRange,
        language_name: &str,
        file_path: Option<&Path>,
    ) -> Option<String> {
        let start = usize::try_from(window_range.start.0).ok()?;
        let end = usize::try_from(window_range.end.0).ok()?;
        if let Some(cached) =
            self.view.borrow().cached_semantic_window_text(language_name, file_path, start, end)
        {
            return Some(cached);
        }
        vlf_read_exact_text_range(store, window_range).map(|(text, _)| text)
    }
}

// ── VLF find methods ──

impl<'a> EventContext<'a> {
    pub(crate) fn do_vlf_find(
        &mut self,
        chars: String,
        case_sensitive: bool,
        regex: bool,
        whole_words: bool,
    ) {
        let status = {
            let editor = self.editor.borrow();
            let Some(store) = editor.vlf_store.as_ref() else {
                return;
            };
            let mut view = self.view.borrow_mut();
            if chars.is_empty() {
                view.clear_vlf_find();
                return;
            }
            view.start_vlf_find(store, chars, case_sensitive, regex, whole_words);
            match view.scan_vlf_find(store) {
                Ok(status) => status,
                Err(err) => {
                    self.client.alert(format!("vlf search failed: {err}"));
                    None
                }
            }
        };

        if let Some(status) = status {
            self.client.vlf_search_status(
                self.view_id,
                &status.query,
                status.scanned_bytes,
                status.total_bytes,
                status.complete,
                status.stored_match_count,
                &status.ranges,
            );
        }

        if self.view.borrow().vlf_find_in_progress() {
            self.schedule_find();
        }
    }

    pub(crate) fn do_vlf_find_next(&mut self, reverse: bool, wrap: bool) {
        let matched = self.view.borrow_mut().advance_vlf_match(reverse, wrap);
        if let Some(matched) = matched {
            self.client.scroll_to(self.view_id, matched.line as usize, matched.start_col);
        } else if self.view.borrow().vlf_find_in_progress() {
            self.client.alert("search still scanning VLF buffer");
        }
    }
}
