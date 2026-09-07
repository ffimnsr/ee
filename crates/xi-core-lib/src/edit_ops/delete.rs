//! Delete, movement-delete, and register-paste operations.
use super::*;

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

pub(crate) fn logical_line_contents(base: &Rope, line: usize) -> (usize, String) {
    let start = LogicalLines.offset_of_line(base, line);
    let end = LogicalLines.offset_of_line(base, line + 1).min(base.len());
    let mut contents = base.slice(start..end).to_string();
    while contents.ends_with('\n') || contents.ends_with('\r') {
        contents.pop();
    }
    (start, contents)
}

pub(crate) fn previous_char_boundary(line: &str, col: usize) -> usize {
    let mut col = col.min(line.len());
    while col > 0 && !line.is_char_boundary(col) {
        col -= 1;
    }
    col
}

pub(crate) fn next_char_boundary(line: &str, col: usize) -> usize {
    let col = previous_char_boundary(line, col.min(line.len()));
    if col >= line.len() {
        return col;
    }
    col + line[col..].chars().next().map(|ch| ch.len_utf8()).unwrap_or(0)
}

pub(crate) fn identity_delta(base: &Rope) -> RopeDelta {
    DeltaBuilder::new(base.len()).build()
}

pub(crate) fn paste_after_offset(base: &Rope, region: &SelRegion) -> usize {
    let offset = region.max();
    let line = base.line_of_offset(offset);
    let line_end = line_content_end(base, line);
    if offset >= line_end {
        line_end
    } else {
        base.next_codepoint_offset(offset).unwrap_or(line_end).min(line_end)
    }
}

pub(crate) fn line_content_end(base: &Rope, line: usize) -> usize {
    let (start, content) = logical_line_contents(base, line);
    start + content.len()
}

pub(crate) fn base_ends_with_newline(base: &Rope) -> bool {
    if base.is_empty() {
        return false;
    }
    let start = base.prev_codepoint_offset(base.len()).unwrap_or(0);
    base.slice_to_cow(start..base.len()).into_owned().ends_with('\n')
}
