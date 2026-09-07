//! Transpose/rotate, line-wise alignment (align-it), sort, reflow, and tab expansion.
use super::delete::{identity_delta, logical_line_contents};
use super::*;

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
pub(crate) fn sel_region_to_interval_and_rope(base: &Rope, region: SelRegion) -> (Interval, Rope) {
    let as_interval = Interval::new(region.min(), region.max());
    let interval_rope = base.subseq(as_interval);
    (as_interval, interval_rope)
}

#[derive(Copy, Clone)]
pub(crate) enum AlignOccurrence {
    Index(isize),
    All,
}

#[derive(Copy, Clone)]
pub(crate) enum AlignItFieldAlign {
    Left,
    Right,
    Center,
}

#[derive(Copy, Clone)]
pub(crate) struct AlignItFormatSpec {
    pub(crate) align: AlignItFieldAlign,
    pub(crate) padding: usize,
}

pub(crate) fn parse_align_it_format(format: &str) -> Option<Vec<AlignItFormatSpec>> {
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

pub(crate) fn compile_align_it_pattern(pattern: &str, regex: bool) -> Option<Regex> {
    if pattern.is_empty() {
        return None;
    }
    let source = if regex { Cow::Borrowed(pattern) } else { Cow::Owned(regex::escape(pattern)) };
    Regex::new(source.as_ref()).ok()
}

pub(crate) fn selected_line_range(base: &Rope, regions: &[SelRegion]) -> Option<(usize, usize)> {
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

pub(crate) fn contiguous_matching_block(
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

pub(crate) fn transform_linewise<F>(
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

pub(crate) fn resolve_linewise_range(
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

pub(crate) fn line_matches(base: &Rope, line: usize, pattern: &Regex) -> bool {
    let (_, content) = logical_line_contents(base, line);
    pattern.find(&content).is_some_and(|found| found.start() != found.end())
}

pub(crate) fn hard_wrap_lines(lines: &[String], width: usize, tab_size: usize) -> Vec<String> {
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

pub(crate) fn flush_wrapped_paragraph(
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

pub(crate) fn expand_tabs(text: &str, tab_size: usize) -> String {
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

pub(crate) fn select_align_it_matches<'a>(
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

pub(crate) fn build_align_it_fields(content: &str, matches: &[regex::Match<'_>]) -> Vec<String> {
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

pub(crate) fn normalize_align_it_leading_text(text: &str) -> String {
    let indent_end = text
        .char_indices()
        .find_map(|(idx, ch)| (!ch.is_whitespace()).then_some(idx))
        .unwrap_or(text.len());
    let indent = &text[..indent_end];
    let body = text[indent_end..].trim_end_matches(char::is_whitespace);
    format!("{indent}{body}")
}

pub(crate) fn align_it_field(
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

pub(crate) fn display_col_for_offset(base: &Rope, offset: usize, tab_size: usize) -> usize {
    let line = base.line_of_offset(offset);
    let line_start = base.offset_of_line(line);
    display_col_for_str(base.slice_to_cow(line_start..offset).as_ref(), tab_size)
}

pub(crate) fn display_col_for_str(text: &str, tab_size: usize) -> usize {
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

pub(crate) fn last_selection_region(regions: &[SelRegion]) -> Option<&SelRegion> {
    regions.iter().rev().find(|&region| !region.is_caret()).map(|v| v as _)
}

pub(crate) fn get_tab_text(config: &BufferItems, tab_size: Option<usize>) -> &'static str {
    let tab_size = tab_size.unwrap_or(config.tab_size);
    let tab_text = if config.translate_tabs_to_spaces { n_spaces(tab_size) } else { "\t" };

    tab_text
}

pub(crate) fn n_spaces(n: usize) -> &'static str {
    let spaces = "                                ";
    assert!(n <= spaces.len());
    &spaces[..n]
}
