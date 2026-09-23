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
        if let Some(marker_indent) = markdown_list_marker_indent(base, region, syntax_context) {
            return marker_indent;
        }
        apply_smart_indent_heuristic(base, region, config, &mut indent);
    }
    indent
}

/// Markdown list/quote continuation: pressing Enter at the end of a line whose
/// content starts with a blockquote marker, a bullet (`-`/`*`/`+`), or an
/// ordered-list marker (`1.`/`1)`) starts the new line with the same marker
/// prefix, so typing lists and quotes does not lose the marker.
///
/// - Empty items (`- `, `> ` with nothing after the marker) do not continue:
///   Enter ends the block instead.
/// - Marker-only prefixes are normalized to marker + single space (`>  x`
///   continues as `> `).
/// - Task-list checkboxes continue as an unchecked `[ ]` item in either state
///   (`- [x] done` continues `- [ ] `).
/// - Ordered markers increment: `1. one` continues as `2. `, `10) item` as
///   `11) `.
/// - Only end-of-line Enter continues; splitting mid-line keeps the plain
///   carried indent.
fn markdown_list_marker_indent(
    base: &Rope,
    region: &SelRegion,
    syntax_context: Option<&SyntaxIndentContext<'_>>,
) -> Option<String> {
    let context = syntax_context?;
    if !context.language_name.eq_ignore_ascii_case("markdown") {
        return None;
    }

    let anchor = region.min().min(base.len());
    let line = base.line_of_offset(anchor);
    let (line_start, content) = logical_line_contents(base, line);
    let anchor_in_line = anchor.saturating_sub(line_start).min(content.len());
    if anchor_in_line != content.len() {
        return None;
    }

    let mut rest = content.as_str();
    let mut prefix = String::new();
    // Keep leading indentation (list nesting, indented quotes).
    let indent_len = rest.len() - rest.trim_start_matches([' ', '\t']).len();
    prefix.push_str(&rest[..indent_len]);
    rest = &rest[indent_len..];

    // Blockquote markers may repeat (`> > nested quote`).
    let mut has_quote = false;
    while let Some(after) = rest.strip_prefix('>') {
        has_quote = true;
        prefix.push_str("> ");
        rest = after.trim_start_matches(' ');
    }

    // Bullet marker, optionally with a task-list checkbox.
    if rest.starts_with("- ") || rest.starts_with("* ") || rest.starts_with("+ ") {
        let marker = &rest[..1];
        let body = &rest[2..];
        if let Some(after) = body
            .strip_prefix("[ ] ")
            .or_else(|| body.strip_prefix("[x] "))
            .or_else(|| body.strip_prefix("[X] "))
        {
            if after.trim().is_empty() {
                return None;
            }
            prefix.push_str(marker);
            prefix.push_str(" [ ] ");
            return Some(prefix);
        }
        if body.trim().is_empty() {
            return None;
        }
        prefix.push_str(marker);
        prefix.push(' ');
        return Some(prefix);
    }

    // Ordered list marker: digits followed by `.` or `)` and a space; the new
    // item increments the number.
    let digits = rest.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digits > 0 {
        let separator =
            match rest[digits..].strip_prefix(". ").or_else(|| rest[digits..].strip_prefix(") ")) {
                Some(_) => rest.as_bytes()[digits] as char,
                None => {
                    if has_quote && !rest.trim().is_empty() {
                        return Some(prefix);
                    }
                    return None;
                }
            };
        let body = &rest[digits + 2..];
        if body.trim().is_empty() {
            return None;
        }
        // Saturating: absurdly long numbers keep their own width.
        let next = rest[..digits].parse::<u64>().unwrap_or(u64::MAX).saturating_add(1);
        prefix.push_str(&next.to_string());
        prefix.push(separator);
        prefix.push(' ');
        return Some(prefix);
    }

    // Quote-only line (`> text`): continue the quote without a list marker.
    if has_quote && !rest.trim().is_empty() {
        return Some(prefix);
    }
    None
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
        IndentOutcome::AlignTo(column) => *indent = indent_string_for_column(config, column),
    }
}

/// Build an indent string that reaches the given visual column, honoring the
/// config's tab/space choice (`@align`/`@anchor` absolute alignment).
fn indent_string_for_column(config: &BufferItems, column: usize) -> String {
    if config.translate_tabs_to_spaces {
        " ".repeat(column)
    } else {
        let tab_size = config.tab_size.max(1);
        "\t".repeat(column / tab_size) + &" ".repeat(column % tab_size)
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
