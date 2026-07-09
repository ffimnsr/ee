// ── Standalone helper functions for the event_context module ──

use regex::{Regex, RegexBuilder};
use unicode_width::UnicodeWidthChar;

use xi_rope::{DeltaBuilder, Interval, LinesMetric, Rope, RopeDelta};

use crate::selection::{SelRegion, Selection};
use xi_rpc::{RemoteError, ResultExt};

use crate::rpc::LineReplacement;

/// Compute line replacements for substitute command.
pub(crate) fn compute_line_replacements(
    text: &Rope,
    start_line: usize,
    end_line: usize,
    pattern: &str,
    replacement: &str,
    global: bool,
    case_sensitive: bool,
) -> Result<Vec<LineReplacement>, RemoteError> {
    let regex = RegexBuilder::new(&regex::escape(pattern))
        .case_insensitive(!case_sensitive)
        .build()
        .map_err_remote(400, |err| format!("substitute: bad pattern: {err}"))?;

    let total_lines = text.measure::<LinesMetric>() + 1;
    let start_line = start_line.min(total_lines.saturating_sub(1));
    let end_line = end_line.min(total_lines.saturating_sub(1));
    if start_line > end_line {
        return Ok(Vec::new());
    }

    let mut replacements = Vec::new();
    for line in start_line..=end_line {
        let current = line_text(text, line);
        let next = if global {
            regex.replace_all(&current, replacement).into_owned()
        } else {
            regex.replace(&current, replacement).into_owned()
        };
        if current != next {
            replacements.push(LineReplacement { line, text: next });
        }
    }
    Ok(replacements)
}

/// Apply line replacements as a delta.
pub(crate) fn apply_line_replacements(text: &Rope, replacements: &[LineReplacement]) -> RopeDelta {
    let mut sorted = replacements.to_vec();
    sorted.sort_by_key(|replacement| replacement.line);

    let mut builder = DeltaBuilder::new(text.len());
    for replacement in sorted {
        builder.replace(line_content_interval(text, replacement.line), replacement.text.into());
    }
    builder.build()
}

/// Replace a line range with new text.
pub(crate) fn replace_line_range(
    text: &Rope,
    start_line: usize,
    end_line: usize,
    lines: &[String],
) -> RopeDelta {
    let total_lines = text.measure::<LinesMetric>() + 1;
    let last_line = total_lines.saturating_sub(1);
    let start_line = start_line.min(last_line);
    let end_line = end_line.min(last_line).max(start_line);
    let start_offset = text.offset_of_line(start_line);
    let end_offset =
        if end_line + 1 < total_lines { text.offset_of_line(end_line + 1) } else { text.len() };

    let mut replacement = lines.join("\n");
    if end_line + 1 < total_lines && !lines.is_empty() {
        replacement.push('\n');
    }

    replace_interval_with_text(text, Interval::new(start_offset, end_offset), &replacement)
}

pub(crate) fn previous_char_boundary_in_text(text: &str, col: usize) -> usize {
    let mut idx = col.min(text.len());
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Extract block text (rectangular selection) from a rope buffer.
pub(crate) fn block_text(
    text: &Rope,
    start_line: usize,
    end_line: usize,
    left_col: usize,
    right_col: usize,
) -> String {
    let total_lines = text.measure::<LinesMetric>() + 1;
    if total_lines == 0 {
        return String::new();
    }

    let top = start_line.min(end_line);
    let bottom = start_line.max(end_line).min(total_lines.saturating_sub(1));
    let left = left_col.min(right_col);
    let right = left_col.max(right_col);

    let mut out = String::new();
    for line in top..=bottom {
        let line = line_text(text, line);
        let start = left.min(line.len());
        let end = right.min(line.len());
        out.push_str(&line[start..end]);
        out.push('\n');
    }
    out
}

#[derive(Debug)]
pub(crate) struct JoinOperation {
    pub(crate) start_offset: usize,
    pub(crate) end_offset: usize,
    pub(crate) joined: String,
    pub(crate) space_offsets: Vec<usize>,
}

pub(crate) fn collect_join_operations(text: &Rope, regions: &[SelRegion]) -> Vec<JoinOperation> {
    let mut operations = Vec::new();
    let total_lines = text.measure::<LinesMetric>() + 1;
    if total_lines == 0 {
        return operations;
    }

    let source_regions =
        if regions.is_empty() { vec![SelRegion::caret(0)] } else { regions.to_vec() };
    for region in source_regions {
        let (start_line, _) = logical_line_col(text, region.min());
        let (mut end_line, end_col) = logical_line_col(text, region.max());
        if end_col == 0 && end_line > start_line {
            end_line = end_line.saturating_sub(1);
        }
        if start_line == end_line {
            if end_line + 1 >= total_lines {
                continue;
            }
            end_line += 1;
        }

        let start_offset = text.offset_of_line(start_line);
        let end_offset =
            if end_line + 1 < total_lines { text.offset_of_line(end_line + 1) } else { text.len() };

        let mut joined = line_text(text, start_line);
        let mut space_offsets = Vec::new();
        for line in start_line + 1..=end_line {
            let trimmed = line_text(text, line).trim_start_matches([' ', '\t']).to_owned();
            if trimmed.is_empty() {
                continue;
            }
            space_offsets.push(start_offset + joined.len());
            joined.push(' ');
            joined.push_str(&trimmed);
        }

        operations.push(JoinOperation { start_offset, end_offset, joined, space_offsets });
    }

    operations.sort_by_key(|operation| std::cmp::Reverse(operation.start_offset));
    operations
}

// ── Selection helper functions ──

pub(crate) fn extend_line_below_selection(
    text: &Rope,
    regions: &[SelRegion],
    count: usize,
) -> Selection {
    let mut selection = Selection::new();
    let total_lines = text.measure::<LinesMetric>() + 1;
    if total_lines == 0 {
        return selection;
    }

    let source_regions =
        if regions.is_empty() { vec![SelRegion::caret(0)] } else { regions.to_vec() };
    let last_line = total_lines.saturating_sub(1);

    for region in source_regions {
        let (start_line, end_line) = selection_line_range(text, region);
        let start_offset = text.offset_of_line(start_line);
        let target_offset = if selection_is_linewise(text, region, start_line, end_line) {
            line_end_offset_inclusive(text, end_line.saturating_add(count).min(last_line))
        } else {
            let target_line = end_line.saturating_add(count);
            if target_line >= total_lines {
                line_end_offset_inclusive(text, last_line)
            } else {
                text.offset_of_line(target_line)
            }
        };
        selection.add_region(SelRegion::new(start_offset, target_offset));
    }

    selection
}

pub(crate) fn extend_line_above_selection(text: &Rope, regions: &[SelRegion]) -> Selection {
    let mut selection = Selection::new();
    let source_regions =
        if regions.is_empty() { vec![SelRegion::caret(0)] } else { regions.to_vec() };

    for region in source_regions {
        let (start_line, end_line) = selection_line_range(text, region);
        let start_offset = if selection_is_linewise(text, region, start_line, end_line) {
            text.offset_of_line(start_line.saturating_sub(1))
        } else {
            text.offset_of_line(start_line)
        };
        selection
            .add_region(SelRegion::new(start_offset, line_end_offset_inclusive(text, end_line)));
    }

    selection
}

pub(crate) fn select_line_above_selection(text: &Rope, regions: &[SelRegion]) -> Selection {
    select_line_selection(text, regions, false)
}

pub(crate) fn select_line_below_selection(text: &Rope, regions: &[SelRegion]) -> Selection {
    select_line_selection(text, regions, true)
}

fn select_line_selection(text: &Rope, regions: &[SelRegion], below: bool) -> Selection {
    let mut selection = Selection::new();
    let total_lines = text.measure::<LinesMetric>() + 1;
    if total_lines == 0 {
        return selection;
    }

    let source_regions =
        if regions.is_empty() { vec![SelRegion::caret(0)] } else { regions.to_vec() };
    let last_line = total_lines.saturating_sub(1);

    for region in source_regions {
        let (start_line, end_line) = selection_line_range(text, region);
        if !selection_is_linewise(text, region, start_line, end_line) {
            selection.add_region(SelRegion::new(
                text.offset_of_line(start_line),
                line_end_offset_inclusive(text, end_line),
            ));
            continue;
        }

        let is_forward = region.start <= region.end;
        let anchor_line = if is_forward { start_line } else { end_line };
        let active_line = if is_forward { end_line } else { start_line };
        let next_active = if below {
            active_line.saturating_add(1).min(last_line)
        } else {
            active_line.saturating_sub(1)
        };
        selection.add_region(linewise_region_for_anchor(text, anchor_line, next_active));
    }

    selection
}

fn linewise_region_for_anchor(text: &Rope, anchor_line: usize, active_line: usize) -> SelRegion {
    if active_line >= anchor_line {
        SelRegion::new(
            text.offset_of_line(anchor_line),
            line_end_offset_inclusive(text, active_line),
        )
    } else {
        SelRegion::new(
            line_end_offset_inclusive(text, anchor_line),
            text.offset_of_line(active_line),
        )
    }
}

pub(crate) fn extend_to_line_bounds_selection(text: &Rope, regions: &[SelRegion]) -> Selection {
    let mut selection = Selection::new();
    let source_regions =
        if regions.is_empty() { vec![SelRegion::caret(0)] } else { regions.to_vec() };

    for region in source_regions {
        let (start_line, end_line) = selection_line_range(text, region);
        selection.add_region(SelRegion::new(
            text.offset_of_line(start_line),
            line_end_offset_inclusive(text, end_line),
        ));
    }

    selection
}

pub(crate) fn shrink_to_line_bounds_selection(text: &Rope, regions: &[SelRegion]) -> Selection {
    let mut selection = Selection::new();
    let total_lines = text.measure::<LinesMetric>() + 1;
    if total_lines == 0 {
        return selection;
    }

    for &region in regions {
        let (start_line, end_line) = selection_line_range(text, region);
        if start_line == end_line {
            selection.add_region(region);
            continue;
        }

        let from = region.min();
        let to = region.max();
        let mut start = text.offset_of_line(start_line);
        let mut end = line_end_offset_inclusive(text, end_line);

        if start != from {
            start = text.offset_of_line((start_line + 1).min(total_lines));
        }
        if end != to {
            end = text.offset_of_line(end_line);
        }

        selection.add_region(SelRegion::new(start, end));
    }

    selection
}

// ── Word motion helpers ──

pub(crate) fn move_word_start_selection(
    text: &Rope,
    regions: &[SelRegion],
    forward: bool,
    long_word: bool,
    modify_selection: bool,
) -> Selection {
    let mut selection = Selection::new();
    let source_regions =
        if regions.is_empty() { vec![SelRegion::caret(0)] } else { regions.to_vec() };

    for region in source_regions {
        let active = region.end;
        let line = text.line_of_offset(active);
        let line_start = text.offset_of_line(line);
        let line_text = line_text(text, line);
        let cursor_byte = active.saturating_sub(line_start).min(line_text.len());
        let target = if forward {
            next_word_start(&line_text, cursor_byte, long_word)
        } else {
            prev_word_start(&line_text, cursor_byte, long_word)
        };
        if let Some(col) = target {
            selection.add_region(selection_region(region, line_start + col, modify_selection));
        }
    }

    selection
}

pub(crate) fn move_word_end_selection(
    text: &Rope,
    regions: &[SelRegion],
    long_word: bool,
    modify_selection: bool,
) -> Selection {
    let mut selection = Selection::new();
    let source_regions =
        if regions.is_empty() { vec![SelRegion::caret(0)] } else { regions.to_vec() };

    for region in source_regions {
        let active = region.end;
        let line = text.line_of_offset(active);
        let line_start = text.offset_of_line(line);
        let line_text = line_text(text, line);
        let cursor_byte = active.saturating_sub(line_start).min(line_text.len());
        if let Some(col) = next_word_end(&line_text, cursor_byte, long_word) {
            selection.add_region(selection_region(region, line_start + col, modify_selection));
        }
    }

    selection
}

pub(crate) fn find_char_selection(
    text: &Rope,
    regions: &[SelRegion],
    target: char,
    forward: bool,
    inclusive: bool,
    modify_selection: bool,
) -> Selection {
    let mut selection = Selection::new();
    let source_regions =
        if regions.is_empty() { vec![SelRegion::caret(0)] } else { regions.to_vec() };

    for region in source_regions {
        let active = region.end;
        let line = text.line_of_offset(active);
        let line_start = text.offset_of_line(line);
        let line_text = line_text(text, line);
        let cursor_byte = active.saturating_sub(line_start).min(line_text.len());
        let col = if forward {
            find_char_forward(&line_text, cursor_byte, target).and_then(|pos| {
                if inclusive {
                    Some(pos)
                } else if pos > 0 {
                    Some(prev_char_start(&line_text, pos))
                } else {
                    None
                }
            })
        } else {
            find_char_backward(&line_text, cursor_byte, target)
                .map(|pos| if inclusive { pos } else { next_char_start(&line_text, pos) })
        };

        if let Some(col) = col {
            selection.add_region(selection_region(region, line_start + col, modify_selection));
        }
    }

    selection
}

pub(crate) fn move_to_matching_bracket_selection(
    text: &Rope,
    regions: &[SelRegion],
    modify_selection: bool,
) -> Selection {
    let mut selection = Selection::new();
    let source_regions =
        if regions.is_empty() { vec![SelRegion::caret(0)] } else { regions.to_vec() };

    for region in source_regions {
        if let Some(offset) = matching_bracket_offset(text, region.end) {
            selection.add_region(selection_region(region, offset, modify_selection));
        }
    }

    selection
}

pub(crate) fn select_chars_selection(
    text: &Rope,
    regions: &[SelRegion],
    count: usize,
) -> Selection {
    let mut selection = Selection::new();
    let source_regions =
        if regions.is_empty() { vec![SelRegion::caret(0)] } else { regions.to_vec() };

    for region in source_regions {
        let active = region.end;
        let line = text.line_of_offset(active);
        let line_start = text.offset_of_line(line);
        let line_text = line_text(text, line);
        let cursor_byte = active.saturating_sub(line_start).min(line_text.len());
        if cursor_byte >= line_text.len() {
            continue;
        }

        let mut end = cursor_byte;
        for _ in 0..count {
            let next = next_char_start(&line_text, end);
            if next == end {
                break;
            }
            end = next;
        }
        if end == cursor_byte {
            continue;
        }

        selection.add_region(SelRegion::new(line_start + cursor_byte, line_start + end));
    }

    selection
}

fn selection_region(region: SelRegion, target_offset: usize, modify_selection: bool) -> SelRegion {
    if modify_selection {
        SelRegion::new(region.start, target_offset).with_horiz(None).with_affinity(region.affinity)
    } else {
        SelRegion::caret(target_offset)
    }
}

fn matching_bracket_offset(text: &Rope, offset: usize) -> Option<usize> {
    let line = text.line_of_offset(offset);
    let line_start = text.offset_of_line(line);
    let current_line = line_text(text, line);
    let cursor_byte = offset.saturating_sub(line_start).min(current_line.len());
    let ch = current_line.get(cursor_byte..)?.chars().next()?;

    let (open, close, forward) = match ch {
        '(' => ('(', ')', true),
        ')' => ('(', ')', false),
        '[' => ('[', ']', true),
        ']' => ('[', ']', false),
        '{' => ('{', '}', true),
        '}' => ('{', '}', false),
        _ => return None,
    };

    let total_lines = text.measure::<LinesMetric>() + 1;
    if forward {
        let mut depth = 0_i32;
        for line_idx in line..total_lines {
            let current = line_text(text, line_idx);
            let base = text.offset_of_line(line_idx);
            let start = if line_idx == line { cursor_byte } else { 0 };
            for (off, current_ch) in current[start..].char_indices() {
                if current_ch == open {
                    depth += 1;
                } else if current_ch == close {
                    depth -= 1;
                    if depth == 0 {
                        return Some(base + start + off);
                    }
                }
            }
        }
    } else {
        let mut depth = 0_i32;
        for line_idx in (0..=line).rev() {
            let current = line_text(text, line_idx);
            let scan_end = if line_idx == line {
                (cursor_byte + ch.len_utf8()).min(current.len())
            } else {
                current.len()
            };
            for (off, current_ch) in current[..scan_end].char_indices().rev() {
                if current_ch == close {
                    depth += 1;
                } else if current_ch == open {
                    depth -= 1;
                    if depth == 0 {
                        return Some(text.offset_of_line(line_idx) + off);
                    }
                }
            }
        }
    }

    None
}

// ── Character classification helpers ──

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn is_long_word_char(ch: char) -> bool {
    !ch.is_whitespace()
}

fn is_motion_char(ch: char, long_word: bool) -> bool {
    if long_word { is_long_word_char(ch) } else { is_word_char(ch) }
}

fn char_at(line: &str, byte: usize) -> Option<char> {
    line.get(byte..)?.chars().next()
}

fn previous_char_boundary(line: &str, col: usize) -> usize {
    let mut col = col.min(line.len());
    while col > 0 && !line.is_char_boundary(col) {
        col -= 1;
    }
    col
}

fn find_char_forward(line: &str, from_byte: usize, target: char) -> Option<usize> {
    let skip = line[from_byte..].chars().next().map(|c| c.len_utf8()).unwrap_or(0);
    let start = from_byte + skip;
    line[start..].char_indices().find(|(_, c)| *c == target).map(|(off, _)| start + off)
}

fn find_char_backward(line: &str, before_byte: usize, target: char) -> Option<usize> {
    line[..before_byte].char_indices().rfind(|(_, c)| *c == target).map(|(off, _)| off)
}

fn prev_char_start(line: &str, byte: usize) -> usize {
    let mut idx = byte.saturating_sub(1);
    while idx > 0 && !line.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn next_char_start(line: &str, byte: usize) -> usize {
    line[byte..].chars().next().map(|c| byte + c.len_utf8()).unwrap_or(byte)
}

fn next_word_start(line: &str, byte: usize, long_word: bool) -> Option<usize> {
    let mut idx = previous_char_boundary(line, byte.min(line.len()));
    let mut chars = line.get(idx..)?.chars();
    let current = chars.next()?;

    if is_motion_char(current, long_word) {
        idx = next_char_start(line, idx);
        while let Some(ch) = char_at(line, idx) {
            if !is_motion_char(ch, long_word) {
                break;
            }
            idx = next_char_start(line, idx);
        }
    }

    while let Some(ch) = char_at(line, idx) {
        if is_motion_char(ch, long_word) {
            return Some(idx);
        }
        idx = next_char_start(line, idx);
    }

    None
}

fn prev_word_start(line: &str, byte: usize, long_word: bool) -> Option<usize> {
    if line.is_empty() || byte == 0 {
        return None;
    }

    let mut idx = prev_char_start(line, byte.min(line.len()));
    while let Some(ch) = char_at(line, idx) {
        if is_motion_char(ch, long_word) {
            break;
        }
        if idx == 0 {
            return None;
        }
        idx = prev_char_start(line, idx);
    }

    while idx > 0 {
        let prev = prev_char_start(line, idx);
        let Some(ch) = char_at(line, prev) else {
            break;
        };
        if !is_motion_char(ch, long_word) {
            break;
        }
        idx = prev;
    }

    Some(idx)
}

fn next_word_end(line: &str, byte: usize, long_word: bool) -> Option<usize> {
    let mut idx = previous_char_boundary(line, byte.min(line.len()));

    while let Some(ch) = char_at(line, idx) {
        if is_motion_char(ch, long_word) {
            break;
        }
        idx = next_char_start(line, idx);
    }

    let mut end = idx;
    let mut found = false;
    while let Some(ch) = char_at(line, idx) {
        if !is_motion_char(ch, long_word) {
            break;
        }
        found = true;
        end = idx;
        idx = next_char_start(line, idx);
    }

    found.then_some(end)
}

// ── Selection range helpers ──

fn selection_line_range(text: &Rope, region: SelRegion) -> (usize, usize) {
    let (start_line, _) = logical_line_col(text, region.min());
    let (mut end_line, end_col) = logical_line_col(text, region.max());
    if end_col == 0 && end_line > start_line {
        end_line = end_line.saturating_sub(1);
    }
    (start_line, end_line)
}

fn selection_is_linewise(
    text: &Rope,
    region: SelRegion,
    start_line: usize,
    end_line: usize,
) -> bool {
    region.min() == text.offset_of_line(start_line)
        && region.max() == line_end_offset_inclusive(text, end_line)
}

// ── Text/interval helpers ──

pub(crate) fn replace_interval_with_text(
    text: &Rope,
    interval: Interval,
    replacement: &str,
) -> RopeDelta {
    let mut builder = DeltaBuilder::new(text.len());
    builder.replace(interval, Rope::from(replacement));
    builder.build()
}

fn line_end_offset_inclusive(text: &Rope, line: usize) -> usize {
    let total_lines = text.measure::<LinesMetric>() + 1;
    let line = line.min(total_lines.saturating_sub(1));
    if line + 1 < total_lines { text.offset_of_line(line + 1) } else { text.len() }
}

pub(crate) fn display_col_to_byte(line: &str, display_col: usize) -> usize {
    let mut col = 0usize;
    for (byte_idx, ch) in line.char_indices() {
        if col >= display_col {
            return byte_idx;
        }
        col += UnicodeWidthChar::width(ch).unwrap_or(0);
    }
    line.len()
}

fn logical_line_col(text: &Rope, offset: usize) -> (usize, usize) {
    let line = text.line_of_offset(offset);
    (line, offset.saturating_sub(text.offset_of_line(line)))
}

pub(crate) fn selection_matches_regex(text: &Rope, region: SelRegion, regex: &Regex) -> bool {
    if region.is_caret() {
        return regex.is_match("");
    }

    regex.is_match(text.slice_to_cow(region.min()..region.max()).as_ref())
}

pub(crate) fn line_content_end(text: &Rope, line: usize) -> usize {
    line_content_interval(text, line).end()
}

pub(crate) fn line_text(text: &Rope, line: usize) -> String {
    let interval = line_with_ending_interval(text, line);
    let mut line_text = text.slice_to_cow(interval).into_owned();
    if line_text.ends_with('\n') {
        line_text.pop();
        if line_text.ends_with('\r') {
            line_text.pop();
        }
    }
    line_text
}

fn line_content_interval(text: &Rope, line: usize) -> Interval {
    let interval = line_with_ending_interval(text, line);
    let start = interval.start();
    let mut end = interval.end();
    let line_text = text.slice_to_cow(interval).into_owned();
    if line_text.ends_with("\r\n") {
        end = end.saturating_sub(2);
    } else if line_text.ends_with('\n') {
        end = end.saturating_sub(1);
    }
    Interval::new(start, end)
}

fn line_with_ending_interval(text: &Rope, line: usize) -> Interval {
    let total_lines = text.measure::<LinesMetric>() + 1;
    let line = line.min(total_lines.saturating_sub(1));
    let start = text.offset_of_line(line);
    let end = if line + 1 < total_lines { text.offset_of_line(line + 1) } else { text.len() };
    Interval::new(start, end)
}
