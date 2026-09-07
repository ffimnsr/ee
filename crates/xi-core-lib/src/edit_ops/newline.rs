//! Newline insertion, smart-indent heuristics, tab handling, and indent changes.
use super::align::get_tab_text;
use super::delete::logical_line_contents;
use super::*;

pub fn insert_newline(base: &Rope, regions: &[SelRegion], config: &BufferItems) -> RopeDelta {
    insert_newline_with_context(base, regions, config, None)
}

pub(crate) fn insert_newline_with_context(
    base: &Rope,
    regions: &[SelRegion],
    config: &BufferItems,
    syntax_context: Option<&SyntaxIndentContext<'_>>,
) -> RopeDelta {
    if !config.auto_indent {
        return insert(base, regions, &config.line_ending);
    }

    let mut builder = DeltaBuilder::new(base.len());
    for region in regions {
        let mut insert_text =
            String::with_capacity(config.line_ending.len() + (config.tab_size.max(1) * 2));
        insert_text.push_str(&config.line_ending);
        insert_text.push_str(&newline_indent_for_region(base, region, config, syntax_context));

        let iv = Interval::new(region.min(), region.max());
        builder.replace(iv, Rope::from(insert_text));
    }

    builder.build()
}

pub(crate) fn newline_indent_for_region(
    base: &Rope,
    region: &SelRegion,
    config: &BufferItems,
    syntax_context: Option<&SyntaxIndentContext<'_>>,
) -> String {
    let mut indent = carried_indent_for_region(base, region);
    if let Some(context) = syntax_context {
        if let Some(outcome) = syntax_indent_outcome(base, region, context) {
            apply_indent_outcome(&mut indent, config, outcome);
            return indent;
        }
    }
    if config.smart_indent {
        apply_smart_indent_heuristic(base, region, config, &mut indent);
    }
    indent
}

pub(crate) fn carried_indent_for_region(base: &Rope, region: &SelRegion) -> String {
    let anchor = region.min().min(base.len());
    let line = base.line_of_offset(anchor);
    let (line_start, content) = logical_line_contents(base, line);
    let indent_end = content
        .char_indices()
        .find_map(|(idx, ch)| (!ch.is_whitespace()).then_some(idx))
        .unwrap_or(content.len());
    let anchor_in_line = anchor.saturating_sub(line_start).min(content.len());
    let carry_end = anchor_in_line.min(indent_end);

    if anchor_in_line < indent_end {
        content[..carry_end].to_owned()
    } else {
        content[..indent_end].to_owned()
    }
}

pub(crate) fn apply_indent_outcome(
    indent: &mut String,
    config: &BufferItems,
    outcome: IndentOutcome,
) {
    match outcome {
        IndentOutcome::Inherit => {}
        IndentOutcome::IndentOneLevel => indent.push_str(get_tab_text(config, None)),
        IndentOutcome::DedentOneLevel => trim_one_indent_level(indent, config),
    }
}

pub(crate) fn apply_smart_indent_heuristic(
    base: &Rope,
    region: &SelRegion,
    config: &BufferItems,
    indent: &mut String,
) {
    let anchor = region.min().min(base.len());
    let region_end = region.max().min(base.len());
    let line = base.line_of_offset(anchor);
    if base.line_of_offset(region_end) != line {
        return;
    }

    let (line_start, content) = logical_line_contents(base, line);
    let anchor_in_line = anchor.saturating_sub(line_start).min(content.len());
    let region_end_in_line = region_end.saturating_sub(line_start).min(content.len());
    let indent_end = content
        .char_indices()
        .find_map(|(idx, ch)| (!ch.is_whitespace()).then_some(idx))
        .unwrap_or(content.len());

    let before = &content[..anchor_in_line];
    let after = &content[region_end_in_line..];

    if trimmed_line_ends_with_opener(before) {
        indent.push_str(get_tab_text(config, None));
    }

    if anchor_in_line <= indent_end && trimmed_line_starts_with_closer(after) {
        trim_one_indent_level(indent, config);
    }
}

pub(crate) fn trimmed_line_ends_with_opener(before: &str) -> bool {
    before.trim_end().chars().last().is_some_and(|ch| matches!(ch, '{' | '[' | '('))
}

pub(crate) fn trimmed_line_starts_with_closer(after: &str) -> bool {
    after.trim_start().chars().next().is_some_and(|ch| matches!(ch, '}' | ']' | ')'))
}

pub(crate) fn trim_one_indent_level(indent: &mut String, config: &BufferItems) {
    let tab_text = get_tab_text(config, None);
    if indent.ends_with(tab_text) {
        indent.truncate(indent.len() - tab_text.len());
        return;
    }

    if indent.ends_with('\t') {
        indent.pop();
        return;
    }

    let trailing_spaces = indent.as_bytes().iter().rev().take_while(|&&byte| byte == b' ').count();
    if trailing_spaces == 0 {
        return;
    }

    let remove = trailing_spaces.min(config.tab_size.max(1));
    indent.truncate(indent.len() - remove);
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

pub(crate) fn indent(base: &Rope, lines: BTreeSet<usize>, tab_text: &str) -> RopeDelta {
    let mut builder = DeltaBuilder::new(base.len());
    for line in lines {
        let offset = LogicalLines.line_col_to_offset(base, line, 0);
        let interval = Interval::new(offset, offset);
        builder.replace(interval, Rope::from(tab_text));
    }
    builder.build()
}

pub(crate) fn outdent(base: &Rope, lines: BTreeSet<usize>, tab_text: &str) -> RopeDelta {
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
