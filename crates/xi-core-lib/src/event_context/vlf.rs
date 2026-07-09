use std::path::Path;

use crate::edit_types::SpecialEvent;
use crate::editor::EditType;
use crate::object::{self, SyntaxNavigationAction, SyntaxSelectionAction};
use crate::selection::{SelRegion, Selection};
use crate::text_store::{
    ByteOffset, ByteRange, LineLookup, LogicalLine, TextChunkResult, TextStore,
};
use crate::tree_sitter_support::{
    VisibleSyntaxLimits, VisibleSyntaxSpan, syntax_feature_availability, visible_syntax_spans,
};

use super::{EventContext, VlfViewportResponse};

// ── VLF free functions ──

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

pub(crate) fn vlf_tail_lines_for_range(
    store: &dyn TextStore,
    requested_line_start: u64,
    requested_count: usize,
    exact_line_count: u64,
) -> Option<(u64, Vec<String>, ByteRange)> {
    let tail_len = (256 * 1024).min(store.len_bytes());
    let tail_start = ByteOffset(store.len_bytes().saturating_sub(tail_len));
    let tail_end = ByteOffset(store.len_bytes());
    let (tail_text, tail_byte_range) =
        vlf_read_text_range(store, ByteRange { start: tail_start, end: tail_end })?;

    let mut tail_lines = tail_text.split('\n').map(str::to_owned).collect::<Vec<_>>();
    if tail_byte_range.start.0 > 0 && !tail_lines.is_empty() {
        tail_lines.remove(0);
    }

    let effective_line_count =
        if tail_byte_range.start.0 == 0 { tail_lines.len() as u64 } else { exact_line_count };
    let tail_line_start = effective_line_count.saturating_sub(tail_lines.len() as u64);
    let response_line_start = requested_line_start.max(tail_line_start);
    let requested_line_end =
        requested_line_start.saturating_add(requested_count as u64).min(effective_line_count);
    if response_line_start >= requested_line_end {
        return None;
    }

    let start_idx = usize::try_from(response_line_start.saturating_sub(tail_line_start)).ok()?;
    let end_idx = usize::try_from(requested_line_end.saturating_sub(tail_line_start)).ok()?;
    if start_idx >= tail_lines.len() || end_idx > tail_lines.len() || start_idx >= end_idx {
        return None;
    }

    Some((response_line_start, tail_lines[start_idx..end_idx].to_vec(), tail_byte_range))
}

pub(crate) fn vlf_tail_lines_for_count(
    store: &dyn TextStore,
    requested_count: usize,
    approximate_line_count: u64,
) -> Option<(u64, Vec<String>, Option<u64>, ByteRange)> {
    let tail_len = (256 * 1024).min(store.len_bytes());
    let tail_start = ByteOffset(store.len_bytes().saturating_sub(tail_len));
    let tail_end = ByteOffset(store.len_bytes());
    let (tail_text, tail_byte_range) =
        vlf_read_text_range(store, ByteRange { start: tail_start, end: tail_end })?;

    let mut tail_lines = tail_text.split('\n').map(str::to_owned).collect::<Vec<_>>();
    if tail_byte_range.start.0 > 0 && !tail_lines.is_empty() {
        tail_lines.remove(0);
    }

    if tail_lines.is_empty() {
        return None;
    }

    let exact_line_count = (tail_byte_range.start.0 == 0).then_some(tail_lines.len() as u64);
    let reported_line_count =
        exact_line_count.unwrap_or_else(|| approximate_line_count.max(tail_lines.len() as u64));
    let start_idx = tail_lines.len().saturating_sub(requested_count.max(1));
    let lines = tail_lines[start_idx..].to_vec();
    let response_line_start = reported_line_count.saturating_sub(lines.len() as u64);

    Some((response_line_start, lines, exact_line_count, tail_byte_range))
}

pub(crate) fn vlf_estimate_line_count_from_head(store: &dyn TextStore, minimum: u64) -> u64 {
    if store.len_bytes() == 0 {
        return 1;
    }

    let head_end = ByteOffset((256 * 1024).min(store.len_bytes()));
    let Some((head_text, head_range)) =
        vlf_read_text_range(store, ByteRange { start: ByteOffset(0), end: head_end })
    else {
        return minimum.max(1);
    };

    let bytes_read = head_range.end.0.saturating_sub(head_range.start.0);
    if bytes_read == 0 || head_text.is_empty() {
        return minimum.max(1);
    }

    let lines_read = head_text.as_bytes().iter().filter(|&&byte| byte == b'\n').count() as u64 + 1;
    let estimated =
        store.len_bytes().saturating_mul(lines_read).saturating_add(bytes_read.saturating_sub(1))
            / bytes_read.max(1);
    estimated.max(minimum).max(lines_read)
}

pub(crate) fn vlf_prefix_lines_for_pending_index(
    store: &dyn TextStore,
    requested_line_start: u64,
    requested_count: usize,
) -> Option<(Vec<String>, ByteRange)> {
    if store.len_bytes() == 0 {
        return (requested_line_start == 0).then(|| {
            (vec![String::new()], ByteRange { start: ByteOffset(0), end: ByteOffset(0) })
        });
    }

    let read_end =
        ByteOffset(super::VLF_PREFIX_PENDING_INDEX_FALLBACK_MAX_BYTES.min(store.len_bytes()));
    let (prefix_text, byte_range) =
        vlf_read_text_range(store, ByteRange { start: ByteOffset(0), end: read_end })?;
    let newline_count = prefix_text.as_bytes().iter().filter(|&&byte| byte == b'\n').count() as u64;
    let required_lines = requested_line_start.saturating_add(requested_count as u64);
    if byte_range.end.0 < store.len_bytes() && newline_count.saturating_add(1) < required_lines {
        return None;
    }

    let start = usize::try_from(requested_line_start).ok()?;
    let lines = prefix_text
        .split('\n')
        .skip(start)
        .take(requested_count.max(1))
        .map(str::to_owned)
        .collect::<Vec<_>>();

    (!lines.is_empty()).then_some((lines, byte_range))
}

pub(crate) fn vlf_lines_near_approximate_byte(
    store: &dyn TextStore,
    requested_line_start: u64,
    requested_count: usize,
    approximate_byte: ByteOffset,
) -> Option<(u64, Vec<String>)> {
    if store.len_bytes() == 0 {
        return Some((0, vec![String::new()]));
    }

    let range_start = ByteOffset(approximate_byte.0.saturating_sub(64 * 1024));
    let range_end =
        ByteOffset(approximate_byte.0.saturating_add(256 * 1024).min(store.len_bytes()));
    let (text, decoded_range) =
        vlf_read_text_range(store, ByteRange { start: range_start, end: range_end })?;
    if text.is_empty() {
        return None;
    }

    let approx_idx = usize::try_from(approximate_byte.0.saturating_sub(decoded_range.start.0))
        .ok()
        .map(|idx| super::previous_char_boundary_in_text(&text, idx))?;
    let line_start_idx =
        text[..approx_idx.min(text.len())].rfind('\n').map_or(0, |idx| idx.saturating_add(1));
    let lines = text[line_start_idx..]
        .split('\n')
        .take(requested_count.max(1))
        .map(str::to_owned)
        .collect::<Vec<_>>();

    (!lines.is_empty()).then_some((requested_line_start, lines))
}

pub(crate) fn vlf_update_viewport_for_lines(
    vlf_store: &crate::vlf::store::VlfStore,
    line_start: u64,
    line_count: usize,
) {
    use crate::text_store::{ByteOffset, LineLookup, LogicalLine, TextStore};

    if line_count == 0 {
        return;
    }

    let store: &dyn TextStore = vlf_store;
    let start = match store.line_to_byte(LogicalLine(line_start)) {
        LineLookup::Exact(byte) => byte,
        _ => return,
    };
    let end = match store.line_to_byte(LogicalLine(line_start.saturating_add(line_count as u64))) {
        LineLookup::Exact(byte) => byte,
        LineLookup::OutOfRange => ByteOffset(store.len_bytes()),
        _ => return,
    };

    if end.0 > start.0 {
        vlf_store.set_viewport(start, end);
    }
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
        SpecialEvent::VlfReplaceRange { .. } | SpecialEvent::VlfViewport { .. } => None,
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

    pub(crate) fn do_vlf_viewport(&self, line_start: u64, line_end: u64, generation: u64) {
        use crate::text_store::{
            ByteOffset, ByteRange, KnownLineCount, LineLookup, LogicalLine, TextChunkResult,
            TextStore,
        };

        let resp: Option<VlfViewportResponse> = (|| {
            let editor = self.editor.borrow();
            let vlf_store = editor.vlf_store.as_ref()?;
            let store: &dyn TextStore = vlf_store.as_ref();

            let index_progress = vlf_store.index().scan_progress().fraction();

            if line_start == u64::MAX {
                let requested_count = (line_end.saturating_add(1)).max(1) as usize;
                let exact_line_count = (store.len_bytes()
                    <= super::VLF_TAIL_EXACT_LINE_COUNT_MAX_BYTES)
                    .then(|| vlf_store.exact_logical_line_count_streaming().ok())
                    .flatten();
                let mut approximate_line_count = exact_line_count.unwrap_or_else(|| {
                    vlf_estimate_line_count_from_head(store, line_end.saturating_add(1))
                });
                let mut line_count_exact = exact_line_count.is_some();
                if store.len_bytes() == 0 {
                    return Some(VlfViewportResponse {
                        line_start: 0,
                        lines: vec![String::new()],
                        syntax_spans: Vec::new(),
                        approximate_line_count: 1,
                        line_count_exact: true,
                        index_progress,
                    });
                }
                if let Some((response_line_start, lines, exact_count, tail_byte_range)) =
                    vlf_tail_lines_for_count(store, requested_count, approximate_line_count)
                {
                    if let Some(exact_count) = exact_count {
                        approximate_line_count = exact_count;
                        line_count_exact = true;
                    } else if !line_count_exact {
                        approximate_line_count = approximate_line_count
                            .max(response_line_start.saturating_add(lines.len() as u64));
                    }
                    vlf_store.set_viewport(tail_byte_range.start, tail_byte_range.end);
                    let syntax_spans = self.vlf_visible_syntax_spans(&lines);
                    return Some(VlfViewportResponse {
                        line_start: response_line_start,
                        lines,
                        syntax_spans,
                        approximate_line_count,
                        line_count_exact,
                        index_progress,
                    });
                }
                return Some(VlfViewportResponse {
                    line_start: approximate_line_count.saturating_sub(1),
                    lines: Vec::new(),
                    syntax_spans: Vec::new(),
                    approximate_line_count,
                    line_count_exact,
                    index_progress,
                });
            }

            let line_count_unknown = matches!(store.known_line_count(), KnownLineCount::Unknown);
            let (mut approximate_line_count, line_count_exact) = match store.known_line_count() {
                KnownLineCount::Exact(n) => (n, true),
                KnownLineCount::Approximate(n) => (n.max(line_end + 1), false),
                KnownLineCount::Unknown => (line_end.saturating_add(100), false),
            };

            let requested_count = (line_end - line_start + 1) as usize;

            if line_count_unknown && store.len_bytes() > 0 {
                let head_end = ByteOffset((256 * 1024).min(store.len_bytes()));
                if let TextChunkResult::Ready(chunk) =
                    store.read_byte_range(ByteRange { start: ByteOffset(0), end: head_end })
                {
                    let bytes_read =
                        chunk.byte_range.end.0.saturating_sub(chunk.byte_range.start.0);
                    let lines_read =
                        chunk.text.as_bytes().iter().filter(|&&b| b == b'\n').count() as u64 + 1;
                    if bytes_read > 0 && lines_read > 0 {
                        let estimated = store
                            .len_bytes()
                            .saturating_mul(lines_read)
                            .saturating_add(bytes_read.saturating_sub(1))
                            / bytes_read;
                        approximate_line_count = estimated.max(line_end + 1);
                    }
                }
            }

            if line_count_exact
                && store.len_bytes() > 0
                && line_end.saturating_add(1) >= approximate_line_count
                && let Some((response_line_start, lines, tail_byte_range)) =
                    vlf_tail_lines_for_range(
                        store,
                        line_start,
                        requested_count,
                        approximate_line_count,
                    )
            {
                vlf_store.set_viewport(tail_byte_range.start, tail_byte_range.end);
                let syntax_spans = self.vlf_visible_syntax_spans(&lines);
                return Some(VlfViewportResponse {
                    line_start: response_line_start,
                    lines,
                    syntax_spans,
                    approximate_line_count,
                    line_count_exact,
                    index_progress,
                });
            }

            if !line_count_exact
                && line_end.saturating_add(1) >= approximate_line_count
                && store.len_bytes() > 0
            {
                if let Some((response_line_start, lines, tail_byte_range)) =
                    vlf_tail_lines_for_range(
                        store,
                        line_start,
                        requested_count,
                        approximate_line_count,
                    )
                {
                    approximate_line_count = approximate_line_count
                        .max(response_line_start.saturating_add(lines.len() as u64));
                    vlf_store.set_viewport(tail_byte_range.start, tail_byte_range.end);
                    let syntax_spans = self.vlf_visible_syntax_spans(&lines);
                    return Some(VlfViewportResponse {
                        line_start: response_line_start,
                        lines,
                        syntax_spans,
                        approximate_line_count,
                        line_count_exact,
                        index_progress,
                    });
                }
            }

            // Resolve line_start → byte offset.
            let byte_start = match vlf_exact_line_byte(store, line_start) {
                Ok(b) => b,
                Err(LineLookup::Approximate(approximate_byte)) => {
                    if let Some((response_line_start, lines)) = vlf_lines_near_approximate_byte(
                        store,
                        line_start,
                        requested_count,
                        approximate_byte,
                    ) {
                        approximate_line_count = approximate_line_count
                            .max(response_line_start.saturating_add(lines.len() as u64));
                        vlf_update_viewport_for_lines(vlf_store, response_line_start, lines.len());
                        let syntax_spans = self.vlf_visible_syntax_spans(&lines);
                        return Some(VlfViewportResponse {
                            line_start: response_line_start,
                            lines,
                            syntax_spans,
                            approximate_line_count,
                            line_count_exact,
                            index_progress,
                        });
                    }
                    return Some(VlfViewportResponse {
                        line_start,
                        lines: Vec::new(),
                        syntax_spans: Vec::new(),
                        approximate_line_count,
                        line_count_exact,
                        index_progress,
                    });
                }
                // Index not ready; signal TUI to retry on next repaint.
                Err(_) => {
                    if let Some((lines, byte_range)) =
                        vlf_prefix_lines_for_pending_index(store, line_start, requested_count)
                    {
                        approximate_line_count = approximate_line_count
                            .max(line_start.saturating_add(lines.len() as u64));
                        vlf_store.set_viewport(byte_range.start, byte_range.end);
                        let syntax_spans = self.vlf_visible_syntax_spans(&lines);
                        return Some(VlfViewportResponse {
                            line_start,
                            lines,
                            syntax_spans,
                            approximate_line_count,
                            line_count_exact,
                            index_progress,
                        });
                    }
                    return Some(VlfViewportResponse {
                        line_start,
                        lines: Vec::new(),
                        syntax_spans: Vec::new(),
                        approximate_line_count,
                        line_count_exact,
                        index_progress,
                    });
                }
            };

            // Resolve the first byte past the last requested line.
            let byte_end = match store.line_to_byte(LogicalLine(line_end + 1)) {
                LineLookup::Exact(b) => b,
                LineLookup::OutOfRange => ByteOffset(store.len_bytes()),
                LineLookup::Approximate(_) | LineLookup::Pending => {
                    ByteOffset(byte_start.0.saturating_add(64 * 1024).min(store.len_bytes()))
                }
            };
            let byte_end = if byte_end.0 <= byte_start.0 {
                ByteOffset(byte_start.0.saturating_add(64 * 1024).min(store.len_bytes()))
            } else {
                byte_end
            };

            let range = ByteRange { start: byte_start, end: byte_end };
            if let Some(store) = editor.vlf_store.as_ref() {
                store.set_viewport(byte_start, byte_end);
            }
            let chunk = match store.read_byte_range(range) {
                TextChunkResult::Ready(c) => c,
                _ => {
                    return Some(VlfViewportResponse {
                        line_start,
                        lines: Vec::new(),
                        syntax_spans: Vec::new(),
                        approximate_line_count,
                        line_count_exact,
                        index_progress,
                    });
                }
            };
            let chunk_text = chunk.text;
            if line_count_unknown && !chunk_text.is_empty() {
                let bytes_read = chunk.byte_range.end.0.saturating_sub(chunk.byte_range.start.0);
                let lines_read =
                    chunk_text.as_bytes().iter().filter(|&&b| b == b'\n').count() as u64 + 1;
                if bytes_read > 0 && lines_read > 0 {
                    let estimated = store
                        .len_bytes()
                        .saturating_mul(lines_read)
                        .saturating_add(bytes_read.saturating_sub(1))
                        / bytes_read;
                    approximate_line_count = estimated.max(line_end + 1);
                }
            }

            let lines: Vec<String> =
                chunk_text.split('\n').take(requested_count).map(str::to_owned).collect();
            let syntax_spans = self.vlf_visible_syntax_spans(&lines);

            Some(VlfViewportResponse {
                line_start,
                lines,
                syntax_spans,
                approximate_line_count,
                line_count_exact,
                index_progress,
            })
        })();

        let Some(resp) = resp else {
            return;
        };

        self.client.vlf_chunks(
            self.view_id,
            generation,
            resp.line_start,
            &resp.lines,
            &resp.syntax_spans,
            resp.approximate_line_count,
            resp.line_count_exact,
            resp.index_progress,
        );
    }

    fn vlf_visible_syntax_spans(&self, lines: &[String]) -> Vec<Vec<VisibleSyntaxSpan>> {
        let mode = self.editor.borrow().document_mode();
        let file_path = self.info.map(|info| info.path.as_path());
        let capabilities =
            syntax_feature_availability(Some(self.language.as_ref()), file_path, mode);
        if lines.is_empty() || !capabilities.syntax_spans {
            return Vec::new();
        }
        let visible_text = lines.join("\n");
        visible_syntax_spans(self.language.as_ref(), &visible_text, VisibleSyntaxLimits::default())
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
