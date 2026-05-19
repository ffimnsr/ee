// Copyright 2020 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Functions for editing ropes.

use std::borrow::Cow;
use std::collections::BTreeSet;

use regex::Regex;
use unicode_width::UnicodeWidthChar;
use xi_rope::{Cursor, DeltaBuilder, Interval, LinesMetric, Rope, RopeDelta};

use crate::backspace::offset_for_delete_backwards;
use crate::config::BufferItems;
use crate::line_offset::{LineOffset, LogicalLines};
use crate::linewrap::Lines;
use crate::movement::{Movement, region_movement};
use crate::selection::{SelRegion, Selection};
use crate::word_boundaries::WordCursor;

#[derive(Debug, Copy, Clone)]
pub enum IndentDirection {
    In,
    Out,
}

/// Replaces the selection with the text `T`.
pub fn insert<T: Into<Rope>>(base: &Rope, regions: &[SelRegion], text: T) -> RopeDelta {
    let rope = text.into();
    let mut builder = DeltaBuilder::new(base.len());
    for region in regions {
        let iv = Interval::new(region.min(), region.max());
        builder.replace(iv, rope.clone());
    }

    builder.build()
}

/// Leaves the current selection untouched, but surrounds it with two insertions.
pub fn surround<BT, AT>(
    base: &Rope,
    regions: &[SelRegion],
    before_text: BT,
    after_text: AT,
) -> RopeDelta
where
    BT: Into<Rope>,
    AT: Into<Rope>,
{
    let mut builder = DeltaBuilder::new(base.len());
    let before_rope = before_text.into();
    let after_rope = after_text.into();
    for region in regions {
        let before_iv = Interval::new(region.min(), region.min());
        builder.replace(before_iv, before_rope.clone());
        let after_iv = Interval::new(region.max(), region.max());
        builder.replace(after_iv, after_rope.clone());
    }

    builder.build()
}

pub fn duplicate_line(base: &Rope, regions: &[SelRegion], config: &BufferItems) -> RopeDelta {
    let mut builder = DeltaBuilder::new(base.len());
    // get affected lines or regions
    let mut to_duplicate = BTreeSet::new();

    for region in regions {
        let (first_line, _) = LogicalLines.offset_to_line_col(base, region.min());
        let line_start = LogicalLines.offset_of_line(base, first_line);

        let mut cursor = match region.is_caret() {
            true => Cursor::new(base, line_start),
            false => {
                // duplicate all lines together that are part of the same selections
                let (last_line, _) = LogicalLines.offset_to_line_col(base, region.max());
                let line_end = LogicalLines.offset_of_line(base, last_line);
                Cursor::new(base, line_end)
            }
        };

        if let Some(line_end) = cursor.next::<LinesMetric>() {
            to_duplicate.insert((line_start, line_end));
        }
    }

    for (start, end) in to_duplicate {
        // insert duplicates
        let iv = Interval::new(start, start);
        builder.replace(iv, base.slice(start..end));

        // last line does not have new line character so it needs to be manually added
        if end == base.len() {
            builder.replace(iv, Rope::from(&config.line_ending))
        }
    }

    builder.build()
}

/// Used when the user presses the backspace key. If no delta is returned, then nothing changes.
pub fn delete_backward(base: &Rope, regions: &[SelRegion], config: &BufferItems) -> RopeDelta {
    // TODO: this function is workable but probably overall code complexity
    // could be improved by implementing a "backspace" movement instead.
    let mut deletions = Selection::new();
    for region in regions {
        let start = offset_for_delete_backwards(region, base, config);
        let iv = Interval::new(start, region.max());
        if !iv.is_empty() {
            deletions.add_region(SelRegion::new(iv.start(), iv.end()));
        }
    }

    delete_sel_regions(base, &deletions)
}

/// Common logic for a number of delete methods. For each region in the
/// selection, if the selection is a caret, delete the region between
/// the caret and the movement applied to the caret, otherwise delete
/// the region.
///
/// If `save` is set, the tuple will contain a rope with the deleted text.
///
/// # Arguments
///
/// * `height` - viewport height
pub(crate) fn delete_by_movement(
    base: &Rope,
    regions: &[SelRegion],
    lines: &Lines,
    movement: Movement,
    height: usize,
    save: bool,
) -> (RopeDelta, Option<Rope>) {
    // We compute deletions as a selection because the merge logic
    // is convenient. Another possibility would be to make the delta
    // builder able to handle overlapping deletions (with union semantics).
    let mut deletions = Selection::new();
    for &r in regions {
        if r.is_caret() {
            let new_region = region_movement(movement, r, lines, height, base, true);
            deletions.add_region(new_region);
        } else {
            deletions.add_region(r);
        }
    }

    let kill_ring = if save {
        let saved = extract_sel_regions(base, &deletions).unwrap_or_default();
        Some(Rope::from(saved))
    } else {
        None
    };

    (delete_sel_regions(base, &deletions), kill_ring)
}

/// Deletes the given regions.
pub(crate) fn delete_sel_regions(base: &Rope, sel_regions: &[SelRegion]) -> RopeDelta {
    let mut builder = DeltaBuilder::new(base.len());
    for region in sel_regions {
        let iv = Interval::new(region.min(), region.max());
        if !iv.is_empty() {
            builder.delete(iv);
        }
    }

    builder.build()
}

/// Extracts non-caret selection regions into a string,
/// joining multiple regions with newlines.
pub(crate) fn extract_sel_regions<'a>(
    base: &'a Rope,
    sel_regions: &[SelRegion],
) -> Option<Cow<'a, str>> {
    let mut saved = None;
    for region in sel_regions {
        if !region.is_caret() {
            let val = base.slice_to_cow(region);
            match saved {
                None => saved = Some(val),
                Some(ref mut s) => {
                    s.to_mut().push('\n');
                    s.to_mut().push_str(&val);
                }
            }
        }
    }
    saved
}

pub(crate) fn delete_line_range(base: &Rope, start_line: usize, end_line: usize) -> RopeDelta {
    let total_lines = base.measure::<LinesMetric>() + 1;
    if total_lines == 0 {
        return identity_delta(base);
    }
    let start_line = start_line.min(total_lines.saturating_sub(1));
    let end_line = end_line.min(total_lines.saturating_sub(1));
    if start_line > end_line {
        return identity_delta(base);
    }

    let start = LogicalLines.offset_of_line(base, start_line);
    let end = if end_line + 1 < total_lines {
        LogicalLines.offset_of_line(base, end_line + 1)
    } else {
        base.len()
    };
    if start >= end {
        return identity_delta(base);
    }

    let mut builder = DeltaBuilder::new(base.len());
    builder.delete(Interval::new(start, end));
    builder.build()
}

pub(crate) fn delete_block(
    base: &Rope,
    start_line: usize,
    end_line: usize,
    left_col: usize,
    right_col: usize,
) -> RopeDelta {
    let total_lines = base.measure::<LinesMetric>() + 1;
    if total_lines == 0 {
        return identity_delta(base);
    }
    let start_line = start_line.min(total_lines.saturating_sub(1));
    let end_line = end_line.min(total_lines.saturating_sub(1));
    if start_line > end_line {
        return identity_delta(base);
    }

    let mut builder = DeltaBuilder::new(base.len());
    for line in start_line..=end_line {
        let (line_start, content) = logical_line_contents(base, line);
        let start = previous_char_boundary(&content, left_col.min(content.len()));
        let end = previous_char_boundary(&content, right_col.min(content.len()));
        if start < end {
            builder.delete(Interval::new(line_start + start, line_start + end));
        }
    }
    builder.build()
}

pub(crate) fn replay_block_insert(
    base: &Rope,
    start_line: usize,
    end_line: usize,
    column: usize,
    text: &str,
    append: bool,
) -> RopeDelta {
    if text.is_empty() {
        return identity_delta(base);
    }

    let total_lines = base.measure::<LinesMetric>() + 1;
    if total_lines == 0 {
        return identity_delta(base);
    }
    let start_line = start_line.min(total_lines.saturating_sub(1));
    let end_line = end_line.min(total_lines.saturating_sub(1));
    if start_line > end_line {
        return identity_delta(base);
    }

    let mut builder = DeltaBuilder::new(base.len());
    for line in start_line..=end_line {
        let (line_start, content) = logical_line_contents(base, line);
        let mut offset = previous_char_boundary(&content, column.min(content.len()));
        if append {
            offset = next_char_boundary(&content, offset);
        }
        let global = line_start + offset;
        builder.replace(Interval::new(global, global), Rope::from(text));
    }
    builder.build()
}

pub(crate) fn paste_register(
    base: &Rope,
    regions: &[SelRegion],
    text: &str,
    before: bool,
    line_ending: &str,
) -> RopeDelta {
    if text.is_empty() {
        return identity_delta(base);
    }

    let mut builder = DeltaBuilder::new(base.len());
    let linewise = text.ends_with('\n');
    let total_lines = base.measure::<LinesMetric>() + 1;

    for region in regions {
        let insert_offset = if linewise {
            let line = base.line_of_offset(region.max());
            if before {
                base.offset_of_line(line)
            } else if line + 1 < total_lines {
                base.offset_of_line(line + 1)
            } else {
                base.len()
            }
        } else if before {
            region.min()
        } else {
            paste_after_offset(base, region)
        };

        let insert_text = if linewise
            && !before
            && insert_offset == base.len()
            && !base.is_empty()
            && !base_ends_with_newline(base)
        {
            format!("{line_ending}{text}")
        } else {
            text.to_owned()
        };

        builder.replace(Interval::new(insert_offset, insert_offset), Rope::from(insert_text));
    }

    builder.build()
}

fn logical_line_contents(base: &Rope, line: usize) -> (usize, String) {
    let start = LogicalLines.offset_of_line(base, line);
    let end = LogicalLines.offset_of_line(base, line + 1).min(base.len());
    let mut contents = base.slice(start..end).to_string();
    while contents.ends_with('\n') || contents.ends_with('\r') {
        contents.pop();
    }
    (start, contents)
}

fn previous_char_boundary(line: &str, col: usize) -> usize {
    let mut col = col.min(line.len());
    while col > 0 && !line.is_char_boundary(col) {
        col -= 1;
    }
    col
}

fn next_char_boundary(line: &str, col: usize) -> usize {
    let col = previous_char_boundary(line, col.min(line.len()));
    if col >= line.len() {
        return col;
    }
    col + line[col..].chars().next().map(|ch| ch.len_utf8()).unwrap_or(0)
}

fn identity_delta(base: &Rope) -> RopeDelta {
    DeltaBuilder::new(base.len()).build()
}

fn paste_after_offset(base: &Rope, region: &SelRegion) -> usize {
    let offset = region.max();
    let line = base.line_of_offset(offset);
    let line_end = line_content_end(base, line);
    if offset >= line_end {
        line_end
    } else {
        base.next_codepoint_offset(offset).unwrap_or(line_end).min(line_end)
    }
}

fn line_content_end(base: &Rope, line: usize) -> usize {
    let (start, content) = logical_line_contents(base, line);
    start + content.len()
}

fn base_ends_with_newline(base: &Rope) -> bool {
    if base.is_empty() {
        return false;
    }
    let start = base.prev_codepoint_offset(base.len()).unwrap_or(0);
    base.slice_to_cow(start..base.len()).into_owned().ends_with('\n')
}

pub fn insert_newline(base: &Rope, regions: &[SelRegion], config: &BufferItems) -> RopeDelta {
    insert(base, regions, &config.line_ending)
}

pub fn insert_tab(base: &Rope, regions: &[SelRegion], config: &BufferItems) -> RopeDelta {
    let mut builder = DeltaBuilder::new(base.len());
    let const_tab_text = get_tab_text(config, None);

    for region in regions {
        let line_range = LogicalLines.get_line_range(base, region);

        if line_range.len() > 1 {
            for line in line_range {
                let offset = LogicalLines.line_col_to_offset(base, line, 0);
                let iv = Interval::new(offset, offset);
                builder.replace(iv, Rope::from(const_tab_text));
            }
        } else {
            let (_, col) = LogicalLines.offset_to_line_col(base, region.start);
            let mut tab_size = config.tab_size;
            tab_size = tab_size - (col % tab_size);
            let tab_text = get_tab_text(config, Some(tab_size));

            let iv = Interval::new(region.min(), region.max());
            builder.replace(iv, Rope::from(tab_text));
        }
    }

    builder.build()
}

/// Indents or outdents lines based on selection and user's tab settings.
/// Uses a BTreeSet to holds the collection of lines to modify.
/// Preserves cursor position and current selection as much as possible.
/// Tries to have behavior consistent with other editors like Atom,
/// Sublime and VSCode, with non-caret selections not being modified.
pub fn modify_indent(
    base: &Rope,
    regions: &[SelRegion],
    config: &BufferItems,
    direction: IndentDirection,
) -> RopeDelta {
    let mut lines = BTreeSet::new();
    let tab_text = get_tab_text(config, None);
    for region in regions {
        let line_range = LogicalLines.get_line_range(base, region);
        for line in line_range {
            lines.insert(line);
        }
    }
    match direction {
        IndentDirection::In => indent(base, lines, tab_text),
        IndentDirection::Out => outdent(base, lines, tab_text),
    }
}

fn indent(base: &Rope, lines: BTreeSet<usize>, tab_text: &str) -> RopeDelta {
    let mut builder = DeltaBuilder::new(base.len());
    for line in lines {
        let offset = LogicalLines.line_col_to_offset(base, line, 0);
        let interval = Interval::new(offset, offset);
        builder.replace(interval, Rope::from(tab_text));
    }
    builder.build()
}

fn outdent(base: &Rope, lines: BTreeSet<usize>, tab_text: &str) -> RopeDelta {
    let mut builder = DeltaBuilder::new(base.len());
    for line in lines {
        let offset = LogicalLines.line_col_to_offset(base, line, 0);
        let tab_offset = LogicalLines.line_col_to_offset(base, line, tab_text.len());
        let interval = Interval::new(offset, tab_offset);
        let leading_slice = base.slice_to_cow(interval.start()..interval.end());
        if leading_slice == tab_text {
            builder.delete(interval);
        } else if let Some(first_char_col) = leading_slice.find(|c: char| !c.is_whitespace()) {
            let first_char_offset = LogicalLines.line_col_to_offset(base, line, first_char_col);
            let interval = Interval::new(offset, first_char_offset);
            builder.delete(interval);
        }
    }
    builder.build()
}

pub fn transpose(base: &Rope, regions: &[SelRegion]) -> RopeDelta {
    let mut builder = DeltaBuilder::new(base.len());
    let mut last = 0;
    let mut optional_previous_selection: Option<(Interval, Rope)> =
        last_selection_region(regions).map(|&region| sel_region_to_interval_and_rope(base, region));

    for &region in regions {
        if region.is_caret() {
            let mut middle = region.end;
            let mut start = base.prev_grapheme_offset(middle).unwrap_or(0);
            let mut end = base.next_grapheme_offset(middle).unwrap_or(middle);

            // Note: this matches Emac's behavior. It swaps last
            // two characters of line if at end of line.
            let end_line_offset =
                LogicalLines.offset_of_line(base, LogicalLines.line_of_offset(base, end));
            // include end != base.len() because if the editor is entirely empty, we dont' want to pull from empty space
            if (end == middle || end == end_line_offset) && end != base.len() {
                middle = start;
                start = base.prev_grapheme_offset(middle).unwrap_or(0);
                end = base.next_grapheme_offset(middle).unwrap_or(middle);
            }

            if start >= last {
                let interval = Interval::new(start, end);
                let before = base.slice_to_cow(start..middle);
                let after = base.slice_to_cow(middle..end);
                let swapped: String = [after, before].concat();
                builder.replace(interval, Rope::from(swapped));
                last = end;
            }
        } else if let Some(previous_selection) = optional_previous_selection.as_ref() {
            let current_interval = sel_region_to_interval_and_rope(base, region);
            if current_interval.0.start() >= last {
                builder.replace(current_interval.0, previous_selection.1.clone());
                last = current_interval.0.end();
                optional_previous_selection = Some(current_interval);
            }
        }
    }

    builder.build()
}

pub fn rotate_selection_contents(base: &Rope, regions: &[SelRegion], forward: bool) -> RopeDelta {
    if regions.len() < 2 {
        return identity_delta(base);
    }

    let intervals = regions
        .iter()
        .map(|&region| sel_region_to_interval_and_rope(base, region))
        .collect::<Vec<_>>();

    let mut builder = DeltaBuilder::new(base.len());
    let len = intervals.len();
    let mut last = 0;
    for (index, (interval, _)) in intervals.iter().enumerate() {
        if interval.start() < last {
            continue;
        }
        let source_index = if forward { (index + len - 1) % len } else { (index + 1) % len };
        builder.replace(*interval, intervals[source_index].1.clone());
        last = interval.end();
    }

    builder.build()
}

pub fn reverse_selection_contents(base: &Rope, regions: &[SelRegion]) -> RopeDelta {
    transform_text(base, regions, |text| text.chars().rev().collect())
}

pub fn align_selections(base: &Rope, regions: &[SelRegion], tab_size: usize) -> RopeDelta {
    if regions.len() < 2 {
        return identity_delta(base);
    }

    let mut column_widths: Vec<usize> = Vec::new();
    let mut coordinates = Vec::with_capacity(regions.len());

    let mut previous_line = usize::MAX;
    let mut column_index = 0;
    let mut running_offset = 0;

    for &region in regions {
        let head_line = base.line_of_offset(region.end);
        let anchor_line = base.line_of_offset(region.start);
        if head_line != anchor_line {
            return identity_delta(base);
        }

        if head_line != previous_line {
            column_index = 0;
            running_offset = 0;
            previous_line = head_line;
        }

        let head_col = display_col_for_offset(base, region.end, tab_size);
        let width = head_col.saturating_sub(running_offset);
        match column_widths.get_mut(column_index) {
            Some(existing) => *existing = (*existing).max(width),
            None => column_widths.push(width),
        }

        coordinates.push((head_line, head_col, region.min()));
        running_offset += width;
        column_index += 1;
    }

    let column_positions: Vec<_> = column_widths
        .into_iter()
        .scan(0, |sum, width| {
            *sum += width;
            Some(*sum)
        })
        .collect();

    let mut builder = DeltaBuilder::new(base.len());
    previous_line = usize::MAX;
    column_index = 0;
    running_offset = 0;

    for (line, head_col, insert_pos) in coordinates {
        if line != previous_line {
            column_index = 0;
            running_offset = 0;
            previous_line = line;
        }

        let current_inserts =
            column_positions[column_index].saturating_sub(head_col + running_offset);
        if current_inserts > 0 {
            builder.replace(
                Interval::new(insert_pos, insert_pos),
                Rope::from(" ".repeat(current_inserts)),
            );
        }

        running_offset += current_inserts;
        column_index += 1;
    }

    builder.build()
}

pub fn align_it(
    base: &Rope,
    regions: &[SelRegion],
    tab_size: usize,
    pattern: &str,
    regex: bool,
    occurrence: i64,
    all: bool,
    format: &str,
    line_range: Option<(usize, usize)>,
) -> RopeDelta {
    let Some(pattern) = compile_align_it_pattern(pattern, regex) else {
        return identity_delta(base);
    };
    let Some(format_spec) = parse_align_it_format(format) else {
        return identity_delta(base);
    };
    let occurrence = if all {
        AlignOccurrence::All
    } else {
        let occurrence = occurrence.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as isize;
        if occurrence == 0 {
            return identity_delta(base);
        }
        AlignOccurrence::Index(occurrence)
    };

    let total_lines = base.measure::<LinesMetric>() + 1;
    if total_lines == 0 {
        return identity_delta(base);
    }

    let target_range = line_range
        .map(|(start, end)| {
            let last = total_lines.saturating_sub(1);
            (start.min(last), end.min(last))
        })
        .or_else(|| selected_line_range(base, regions))
        .or_else(|| contiguous_matching_block(base, regions, &pattern));

    let Some((start_line, end_line)) = target_range else {
        return identity_delta(base);
    };

    #[derive(Clone)]
    struct MatchLine {
        line_start: usize,
        content_len: usize,
        fields: Vec<String>,
    }

    let mut matched_lines = Vec::new();
    let mut column_widths: Vec<usize> = Vec::new();

    for line in start_line..=end_line {
        let (line_start, content) = logical_line_contents(base, line);
        let matches: Vec<_> =
            pattern.find_iter(&content).filter(|found| found.start() != found.end()).collect();
        let selected = select_align_it_matches(&matches, occurrence);
        if selected.is_empty() {
            continue;
        }

        let fields = build_align_it_fields(&content, &selected);
        if fields.is_empty() {
            continue;
        }

        for (index, field) in fields.iter().enumerate() {
            let width = display_col_for_str(field, tab_size);
            match column_widths.get_mut(index) {
                Some(existing) => *existing = (*existing).max(width),
                None => column_widths.push(width),
            }
        }

        matched_lines.push(MatchLine { line_start, content_len: content.len(), fields });
    }

    if matched_lines.is_empty() {
        return identity_delta(base);
    }

    let mut builder = DeltaBuilder::new(base.len());
    for line in matched_lines {
        let mut rebuilt = String::new();
        for (index, field) in line.fields.iter().enumerate() {
            let spec = format_spec[index % format_spec.len()];
            let width = column_widths.get(index).copied().unwrap_or_default();
            rebuilt.push_str(&align_it_field(
                field,
                width,
                spec.align,
                tab_size,
                index + 1 == line.fields.len(),
            ));
            if index + 1 < line.fields.len() {
                rebuilt.push_str(&" ".repeat(spec.padding));
            }
        }

        builder.replace(
            Interval::new(line.line_start, line.line_start + line.content_len),
            Rope::from(rebuilt),
        );
    }

    builder.build()
}

pub fn sort_lines(
    base: &Rope,
    regions: &[SelRegion],
    descending: bool,
    line_range: Option<(usize, usize)>,
) -> RopeDelta {
    transform_linewise(base, regions, line_range, |lines| {
        if descending {
            lines.sort_by(|left, right| right.cmp(left));
        } else {
            lines.sort();
        }
    })
}

pub fn reflow_lines(
    base: &Rope,
    regions: &[SelRegion],
    width: usize,
    tab_size: usize,
    line_range: Option<(usize, usize)>,
) -> RopeDelta {
    if width == 0 {
        return identity_delta(base);
    }
    transform_linewise(base, regions, line_range, |lines| {
        *lines = hard_wrap_lines(lines, width, tab_size);
    })
}

pub fn expand_tabs_in_lines(
    base: &Rope,
    regions: &[SelRegion],
    tab_size: usize,
    line_range: Option<(usize, usize)>,
) -> RopeDelta {
    transform_linewise(base, regions, line_range, |lines| {
        for line in lines {
            if line.contains('\t') {
                *line = expand_tabs(line, tab_size);
            }
        }
    })
}

pub fn transform_text<F: Fn(&str) -> String>(
    base: &Rope,
    regions: &[SelRegion],
    transform_function: F,
) -> RopeDelta {
    let mut builder = DeltaBuilder::new(base.len());

    for region in regions {
        let selected_text = base.slice_to_cow(region);
        let interval = Interval::new(region.min(), region.max());
        builder.replace(interval, Rope::from(transform_function(&selected_text)));
    }

    builder.build()
}

/// Changes the number(s) under the cursor(s) with the `transform_function`.
/// If there is a number next to or on the beginning of the region, then
/// this number will be replaced with the result of `transform_function` and
/// the cursor will be placed at the end of the number.
/// Some Examples with a increment `transform_function`:
///
/// "|1234" -> "1235|"
/// "12|34" -> "1235|"
/// "-|12" -> "-11|"
/// "another number is 123|]" -> "another number is 124"
///
/// This function also works fine with multiple regions.
pub fn change_number<F: Fn(i128) -> Option<i128>>(
    base: &Rope,
    regions: &[SelRegion],
    transform_function: F,
) -> RopeDelta {
    let mut builder = DeltaBuilder::new(base.len());
    for region in regions {
        let mut cursor = WordCursor::new(base, region.end);
        let (mut start, end) = cursor.select_word();

        // if the word begins with '-', then it is a negative number
        if start > 0 && base.byte_at(start - 1) == (b'-') {
            start -= 1;
        }

        let word = base.slice_to_cow(start..end);
        if let Some(number) = word.parse::<i128>().ok().and_then(&transform_function) {
            let interval = Interval::new(start, end);
            builder.replace(interval, Rope::from(number.to_string()));
        }
    }

    builder.build()
}

// capitalization behaviour is similar to behaviour in XCode
pub fn capitalize_text(base: &Rope, regions: &[SelRegion]) -> (RopeDelta, Selection) {
    let mut builder = DeltaBuilder::new(base.len());
    let mut final_selection = Selection::new();

    for &region in regions {
        final_selection.add_region(SelRegion::new(region.max(), region.max()));
        let mut word_cursor = WordCursor::new(base, region.min());

        loop {
            // capitalize each word in the current selection
            let (start, end) = word_cursor.select_word();

            if start < end {
                let interval = Interval::new(start, end);
                let word = base.slice_to_cow(start..end);

                // first letter is uppercase, remaining letters are lowercase
                let (first_char, rest) = word.split_at(1);
                let capitalized_text = [first_char.to_uppercase(), rest.to_lowercase()].concat();
                builder.replace(interval, Rope::from(capitalized_text));
            }

            if word_cursor.next_boundary().is_none() || end > region.max() {
                break;
            }
        }
    }

    (builder.build(), final_selection)
}

fn sel_region_to_interval_and_rope(base: &Rope, region: SelRegion) -> (Interval, Rope) {
    let as_interval = Interval::new(region.min(), region.max());
    let interval_rope = base.subseq(as_interval);
    (as_interval, interval_rope)
}

#[derive(Copy, Clone)]
enum AlignOccurrence {
    Index(isize),
    All,
}

#[derive(Copy, Clone)]
enum AlignItFieldAlign {
    Left,
    Right,
    Center,
}

#[derive(Copy, Clone)]
struct AlignItFormatSpec {
    align: AlignItFieldAlign,
    padding: usize,
}

fn parse_align_it_format(format: &str) -> Option<Vec<AlignItFormatSpec>> {
    let format = format.trim();
    if format.is_empty() {
        return Some(vec![
            AlignItFormatSpec { align: AlignItFieldAlign::Left, padding: 1 },
            AlignItFormatSpec { align: AlignItFieldAlign::Right, padding: 1 },
            AlignItFormatSpec { align: AlignItFieldAlign::Left, padding: 0 },
        ]);
    }

    let mut specs = Vec::new();
    let mut index = 0;
    while index < format.len() {
        let align = match format.as_bytes()[index] {
            b'l' => AlignItFieldAlign::Left,
            b'r' => AlignItFieldAlign::Right,
            b'c' => AlignItFieldAlign::Center,
            _ => return None,
        };
        index += 1;
        let digit_start = index;
        while index < format.len() && format.as_bytes()[index].is_ascii_digit() {
            index += 1;
        }
        if digit_start == index {
            return None;
        }
        let padding = format[digit_start..index].parse().ok()?;
        specs.push(AlignItFormatSpec { align, padding });
    }

    (!specs.is_empty()).then_some(specs)
}

fn compile_align_it_pattern(pattern: &str, regex: bool) -> Option<Regex> {
    if pattern.is_empty() {
        return None;
    }
    let source = if regex { Cow::Borrowed(pattern) } else { Cow::Owned(regex::escape(pattern)) };
    Regex::new(source.as_ref()).ok()
}

fn selected_line_range(base: &Rope, regions: &[SelRegion]) -> Option<(usize, usize)> {
    if regions.is_empty() {
        return None;
    }
    let use_selection_range = regions.len() > 1 || regions.iter().any(|region| !region.is_caret());
    if !use_selection_range {
        return None;
    }

    let mut start_line = usize::MAX;
    let mut end_line = 0;
    for region in regions {
        let first_line = base.line_of_offset(region.min());
        let last_offset =
            if region.is_caret() { region.max() } else { region.max().saturating_sub(1) };
        let last_line = base.line_of_offset(last_offset);
        start_line = start_line.min(first_line);
        end_line = end_line.max(last_line);
    }

    (start_line != usize::MAX).then_some((start_line, end_line))
}

fn contiguous_matching_block(
    base: &Rope,
    regions: &[SelRegion],
    pattern: &Regex,
) -> Option<(usize, usize)> {
    let region = regions.first()?;
    let line = base.line_of_offset(region.max());
    if !line_matches(base, line, pattern) {
        return None;
    }

    let total_lines = base.measure::<LinesMetric>() + 1;
    let mut start = line;
    while start > 0 && line_matches(base, start - 1, pattern) {
        start -= 1;
    }

    let mut end = line;
    while end + 1 < total_lines && line_matches(base, end + 1, pattern) {
        end += 1;
    }

    Some((start, end))
}

fn transform_linewise<F>(
    base: &Rope,
    regions: &[SelRegion],
    line_range: Option<(usize, usize)>,
    mut transform: F,
) -> RopeDelta
where
    F: FnMut(&mut Vec<String>),
{
    let total_lines = base.measure::<LinesMetric>() + 1;
    if total_lines == 0 {
        return identity_delta(base);
    }

    let Some((start_line, end_line)) =
        resolve_linewise_range(base, regions, line_range, total_lines)
    else {
        return identity_delta(base);
    };

    let original =
        (start_line..=end_line).map(|line| logical_line_contents(base, line).1).collect::<Vec<_>>();
    let mut updated = original.clone();
    transform(&mut updated);
    if original == updated {
        return identity_delta(base);
    }

    let start_offset = LogicalLines.offset_of_line(base, start_line);
    let end_offset = if end_line + 1 < total_lines {
        LogicalLines.offset_of_line(base, end_line + 1)
    } else {
        base.len()
    };
    let original_segment = base.slice_to_cow(start_offset..end_offset);
    let mut replacement = updated.join("\n");
    if original_segment.ends_with('\n') || original_segment.ends_with('\r') {
        replacement.push('\n');
    }

    let mut builder = DeltaBuilder::new(base.len());
    builder.replace(Interval::new(start_offset, end_offset), Rope::from(replacement));
    builder.build()
}

fn resolve_linewise_range(
    base: &Rope,
    regions: &[SelRegion],
    line_range: Option<(usize, usize)>,
    total_lines: usize,
) -> Option<(usize, usize)> {
    let last = total_lines.saturating_sub(1);
    line_range
        .map(|(start, end)| (start.min(last), end.min(last).max(start.min(last))))
        .or_else(|| selected_line_range(base, regions))
        .or(Some((0, last)))
}

fn line_matches(base: &Rope, line: usize, pattern: &Regex) -> bool {
    let (_, content) = logical_line_contents(base, line);
    pattern.find(&content).is_some_and(|found| found.start() != found.end())
}

fn hard_wrap_lines(lines: &[String], width: usize, tab_size: usize) -> Vec<String> {
    let width = width.max(1);
    let mut wrapped = Vec::with_capacity(lines.len());
    let mut paragraph_words: Vec<String> = Vec::new();
    let mut paragraph_indent = String::new();

    for line in lines {
        if line.trim().is_empty() {
            flush_wrapped_paragraph(
                &mut wrapped,
                &mut paragraph_words,
                &mut paragraph_indent,
                width,
                tab_size,
            );
            wrapped.push(String::new());
            continue;
        }

        if paragraph_words.is_empty() {
            paragraph_indent = line.chars().take_while(|ch| ch.is_whitespace()).collect();
        }
        paragraph_words.extend(line.split_whitespace().map(str::to_owned));
    }

    flush_wrapped_paragraph(
        &mut wrapped,
        &mut paragraph_words,
        &mut paragraph_indent,
        width,
        tab_size,
    );
    wrapped
}

fn flush_wrapped_paragraph(
    wrapped: &mut Vec<String>,
    words: &mut Vec<String>,
    indent: &mut String,
    width: usize,
    tab_size: usize,
) {
    if words.is_empty() {
        indent.clear();
        return;
    }

    let indent_text = expand_tabs(indent, tab_size);
    let indent_width = display_col_for_str(&indent_text, tab_size);
    let mut current = indent_text.clone();
    let mut current_width = indent_width;

    for word in words.drain(..) {
        let word_width = display_col_for_str(&word, tab_size);
        let separator_width = usize::from(current_width > indent_width);
        if current_width > indent_width && current_width + separator_width + word_width > width {
            wrapped.push(current);
            current = indent_text.clone();
            current.push_str(&word);
            current_width = indent_width + word_width;
            continue;
        }

        if current_width > indent_width {
            current.push(' ');
            current_width += 1;
        }
        current.push_str(&word);
        current_width += word_width;
    }

    wrapped.push(current);
    indent.clear();
}

fn expand_tabs(text: &str, tab_size: usize) -> String {
    let tab_size = tab_size.max(1);
    let mut expanded = String::with_capacity(text.len());
    let mut display_col = 0usize;
    for ch in text.chars() {
        if ch == '\t' {
            let width = tab_size - (display_col % tab_size);
            let width = if width == 0 { tab_size } else { width };
            expanded.extend(std::iter::repeat_n(' ', width));
            display_col += width;
        } else {
            expanded.push(ch);
            display_col += UnicodeWidthChar::width(ch).unwrap_or(0);
        }
    }
    expanded
}

fn select_align_it_matches<'a>(
    matches: &'a [regex::Match<'a>],
    occurrence: AlignOccurrence,
) -> Vec<regex::Match<'a>> {
    match occurrence {
        AlignOccurrence::All => matches.to_vec(),
        AlignOccurrence::Index(index) if index > 0 => {
            matches.get(index.saturating_sub(1) as usize).copied().into_iter().collect()
        }
        AlignOccurrence::Index(index) => {
            let offset = index.unsigned_abs();
            matches
                .len()
                .checked_sub(offset)
                .and_then(|selected| matches.get(selected))
                .copied()
                .into_iter()
                .collect()
        }
    }
}

fn build_align_it_fields(content: &str, matches: &[regex::Match<'_>]) -> Vec<String> {
    if matches.is_empty() {
        return Vec::new();
    }

    let mut fields = Vec::with_capacity(matches.len() * 2 + 1);
    let mut cursor = 0;
    for (index, found) in matches.iter().enumerate() {
        let text = &content[cursor..found.start()];
        fields.push(if index == 0 {
            normalize_align_it_leading_text(text)
        } else {
            text.trim().to_owned()
        });
        fields.push(found.as_str().to_owned());
        cursor = found.end();
    }
    fields.push(content[cursor..].trim().to_owned());
    fields
}

fn normalize_align_it_leading_text(text: &str) -> String {
    let indent_end = text
        .char_indices()
        .find_map(|(idx, ch)| (!ch.is_whitespace()).then_some(idx))
        .unwrap_or(text.len());
    let indent = &text[..indent_end];
    let body = text[indent_end..].trim_end_matches(char::is_whitespace);
    format!("{indent}{body}")
}

fn align_it_field(
    value: &str,
    width: usize,
    align: AlignItFieldAlign,
    tab_size: usize,
    is_last: bool,
) -> String {
    let display_width = display_col_for_str(value, tab_size);
    let padding = width.saturating_sub(display_width);
    match align {
        AlignItFieldAlign::Left => {
            if is_last {
                value.to_owned()
            } else {
                format!("{value}{}", " ".repeat(padding))
            }
        }
        AlignItFieldAlign::Right => format!("{}{value}", " ".repeat(padding)),
        AlignItFieldAlign::Center => {
            let left = padding / 2;
            let right = if is_last { 0 } else { padding.saturating_sub(left) };
            format!("{}{}{}", " ".repeat(left), value, " ".repeat(right))
        }
    }
}

fn display_col_for_offset(base: &Rope, offset: usize, tab_size: usize) -> usize {
    let line = base.line_of_offset(offset);
    let line_start = base.offset_of_line(line);
    display_col_for_str(base.slice_to_cow(line_start..offset).as_ref(), tab_size)
}

fn display_col_for_str(text: &str, tab_size: usize) -> usize {
    let tab_size = tab_size.max(1);
    let mut display_col = 0;
    for ch in text.chars() {
        if ch == '\t' {
            let tab_width = tab_size - (display_col % tab_size);
            display_col += if tab_width == 0 { tab_size } else { tab_width };
        } else {
            display_col += UnicodeWidthChar::width(ch).unwrap_or(0);
        }
    }
    display_col
}

fn last_selection_region(regions: &[SelRegion]) -> Option<&SelRegion> {
    regions.iter().rev().find(|&region| !region.is_caret()).map(|v| v as _)
}

fn get_tab_text(config: &BufferItems, tab_size: Option<usize>) -> &'static str {
    let tab_size = tab_size.unwrap_or(config.tab_size);
    let tab_text = if config.translate_tabs_to_spaces { n_spaces(tab_size) } else { "\t" };

    tab_text
}

fn n_spaces(n: usize) -> &'static str {
    let spaces = "                                ";
    assert!(n <= spaces.len());
    &spaces[..n]
}

#[cfg(test)]
mod tests {
    use super::{
        align_it, align_selections, delete_backward, expand_tabs_in_lines, reflow_lines,
        reverse_selection_contents, rotate_selection_contents, sort_lines, transpose,
    };
    use crate::config::BufferItems;
    use crate::selection::SelRegion;
    use xi_rope::Rope;

    #[test]
    fn transpose_skips_overlapping_mixed_regions() {
        let text: Rope = "abcd".into();
        let regions = [SelRegion::new(1, 3), SelRegion::new(2, 2)];

        let delta = transpose(&text, &regions);

        assert_eq!(String::from(delta.apply(&text)), "abcd");
    }

    #[test]
    fn transpose_skips_eol_overlap_after_adjustment() {
        let text: Rope = "ab\n".into();
        let regions = [SelRegion::new(0, 0), SelRegion::new(2, 2)];

        let delta = transpose(&text, &regions);

        assert_eq!(String::from(delta.apply(&text)), "a\nb");
    }

    #[test]
    fn transpose_handles_multibyte_eol_grapheme() {
        let text: Rope = "1ё\n".into();
        let regions = [SelRegion::new(3, 3)];

        let delta = transpose(&text, &regions);

        assert_eq!(String::from(delta.apply(&text)), "1\nё");
    }

    #[test]
    fn delete_backward_merges_overlapping_regions() {
        let text: Rope = "abcd".into();
        let config = BufferItems {
            line_ending: "\n".to_owned(),
            tab_size: 4,
            translate_tabs_to_spaces: false,
            use_tab_stops: true,
            font_face: String::new(),
            font_size: 12.0,
            auto_indent: false,
            scroll_past_end: false,
            wrap_width: 0,
            word_wrap: false,
            autodetect_whitespace: false,
            surrounding_pairs: Vec::new(),
            save_with_newline: false,
        };
        let regions = [SelRegion::new(2, 2), SelRegion::new(3, 3)];

        let delta = delete_backward(&text, &regions, &config);

        assert_eq!(String::from(delta.apply(&text)), "ad");
    }

    #[test]
    fn rotate_selection_contents_forward_wraps_last_into_first() {
        let text: Rope = "aa bb cc".into();
        let regions = [SelRegion::new(0, 2), SelRegion::new(3, 5), SelRegion::new(6, 8)];

        let delta = rotate_selection_contents(&text, &regions, true);

        assert_eq!(String::from(delta.apply(&text)), "cc aa bb");
    }

    #[test]
    fn rotate_selection_contents_backward_wraps_first_into_last() {
        let text: Rope = "aa bb cc".into();
        let regions = [SelRegion::new(0, 2), SelRegion::new(3, 5), SelRegion::new(6, 8)];

        let delta = rotate_selection_contents(&text, &regions, false);

        assert_eq!(String::from(delta.apply(&text)), "bb cc aa");
    }

    #[test]
    fn reverse_selection_contents_reverses_each_selection() {
        let text: Rope = "ab cde z".into();
        let regions = [SelRegion::new(0, 2), SelRegion::new(3, 6)];

        let delta = reverse_selection_contents(&text, &regions);

        assert_eq!(String::from(delta.apply(&text)), "ba edc z");
    }

    #[test]
    fn reverse_selection_contents_preserves_utf8_codepoints() {
        let text: Rope = "aéß".into();
        let regions = [SelRegion::new(0, text.len())];

        let delta = reverse_selection_contents(&text, &regions);

        assert_eq!(String::from(delta.apply(&text)), "ßéa");
    }

    #[test]
    fn align_selections_pads_columns_across_lines() {
        let text: Rope = "a  b\nab".into();
        let regions = [
            SelRegion::new(0, 1),
            SelRegion::new(3, 4),
            SelRegion::new(5, 6),
            SelRegion::new(6, 7),
        ];

        let delta = align_selections(&text, &regions, 4);

        assert_eq!(String::from(delta.apply(&text)), "a  b\na  b");
    }

    #[test]
    fn align_it_expands_contiguous_matching_block_from_caret() {
        let text: Rope = "a=1\nbbb=22\nskip\nz=3".into();
        let regions = [SelRegion::new(0, 0)];

        let delta = align_it(&text, &regions, 4, "=", false, 1, false, "", None);

        assert_eq!(String::from(delta.apply(&text)), "a   = 1\nbbb = 22\nskip\nz=3");
    }

    #[test]
    fn align_it_uses_selected_lines_and_skips_unmatched_lines() {
        let text: Rope = "a=1\nskip\nbb=2".into();
        let regions = [SelRegion::new(0, text.len())];

        let delta = align_it(&text, &regions, 4, "=", false, 1, false, "", None);

        assert_eq!(String::from(delta.apply(&text)), "a  = 1\nskip\nbb = 2");
    }

    #[test]
    fn align_it_supports_regex_and_explicit_line_ranges() {
        let text: Rope = "apple=1\nbanana += 22\npear ||= 3\nend".into();
        let regions = [SelRegion::new(0, 0)];

        let delta = align_it(&text, &regions, 4, r"\|\|=|\+=|=", true, 1, false, "", Some((0, 2)));

        assert_eq!(
            String::from(delta.apply(&text)),
            "apple    = 1\nbanana  += 22\npear   ||= 3\nend"
        );
    }

    #[test]
    fn align_it_supports_nth_match_selection() {
        let text: Rope = "a = 1 => foo\nlong_name = 22 => bar".into();
        let regions = [SelRegion::new(0, 0)];

        let delta = align_it(&text, &regions, 4, r"=>|=", true, 2, false, "", None);

        assert_eq!(
            String::from(delta.apply(&text)),
            "a = 1          => foo\nlong_name = 22 => bar"
        );
    }

    #[test]
    fn align_it_supports_all_matches_with_tabular_format() {
        let text: Rope = "abc,def,ghi\na,b\na,b,c".into();
        let regions = [SelRegion::new(0, text.len())];

        let delta = align_it(&text, &regions, 4, ",", false, 1, true, "r1c1l0", None);

        assert_eq!(String::from(delta.apply(&text)), "abc , def, ghi\n  a , b\n  a , b  ,  c");
    }

    #[test]
    fn align_it_supports_custom_spacing_format() {
        let text: Rope = "a=1\nbbb=22".into();
        let regions = [SelRegion::new(0, text.len())];

        let delta = align_it(&text, &regions, 4, "=", false, 1, false, "l0r0l0", None);

        assert_eq!(String::from(delta.apply(&text)), "a  =1\nbbb=22");
    }

    #[test]
    fn sort_lines_uses_whole_buffer_when_only_caret_present() {
        let text: Rope = "z\nc\na\nb".into();
        let regions = [SelRegion::new(0, 0)];

        let delta = sort_lines(&text, &regions, false, None);

        assert_eq!(String::from(delta.apply(&text)), "a\nb\nc\nz");
    }

    #[test]
    fn sort_lines_supports_explicit_reverse_range() {
        let text: Rope = "keep\naaa\nccc\nbbb\nstay".into();
        let regions = [SelRegion::new(0, 0)];

        let delta = sort_lines(&text, &regions, true, Some((1, 3)));

        assert_eq!(String::from(delta.apply(&text)), "keep\nccc\nbbb\naaa\nstay");
    }

    #[test]
    fn reflow_lines_wraps_selected_or_explicit_lines() {
        let text: Rope = "alpha beta\ngamma delta\n\nkeep".into();
        let regions = [SelRegion::new(0, 0)];

        let delta = reflow_lines(&text, &regions, 10, 4, Some((0, 1)));

        assert_eq!(String::from(delta.apply(&text)), "alpha beta\ngamma\ndelta\n\nkeep");
    }

    #[test]
    fn expand_tabs_in_lines_rewrites_selected_lines() {
        let text: Rope = "\talpha\nb\tcd".into();
        let regions = [SelRegion::new(0, text.len())];

        let delta = expand_tabs_in_lines(&text, &regions, 4, None);

        assert_eq!(String::from(delta.apply(&text)), "    alpha\nb   cd");
    }
}
