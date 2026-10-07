use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Deterministic display-width wrapping for terminal UI text.
///
/// Breaks at whitespace when possible and hard-breaks overlong words; output
/// never exceeds `width` display columns.
pub(crate) fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(4);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let paragraph = paragraph.strip_suffix('\r').unwrap_or(paragraph);
        let mut wrapped = wrap_text_paragraph(paragraph, width);
        if wrapped.is_empty() {
            lines.push(String::new());
        } else {
            lines.append(&mut wrapped);
        }
    }
    lines
}

/// Display-width hard wrapping that preserves every character, including
/// leading/trailing whitespace. Unlike [`wrap_text`] (which collapses
/// whitespace and breaks at word boundaries), the wrapped segments are
/// lossless, so rendered columns map exactly back onto the source text.
/// Used by the floating prompt editor for caret positioning.
pub(crate) fn wrap_text_hard(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_width + ch_width > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width += ch_width;
    }
    lines.push(current);
    lines
}

/// Maps a byte offset in `text` to its `(wrapped_line, wrapped_col)` when the
/// text is hard-wrapped at `width` display columns with [`wrap_text_hard`].
/// The wrapped line index is 0-based; the column is the display width of the
/// caret's wrapped row. Used by the floating prompt editor to keep the caret
/// on the modal while the draft wraps.
pub(crate) fn wrapped_caret_position(
    text: &str,
    byte_offset: usize,
    width: usize,
) -> (usize, usize) {
    let prefix = &text[..byte_offset.min(text.len())];
    let prefix_lines: Vec<&str> = prefix.split('\n').collect();
    let last_line = prefix_lines.len() - 1;
    let mut line = 0usize;
    let mut col = 0usize;
    for (index, part) in prefix_lines.iter().enumerate() {
        let rows = wrap_text_hard(part, width);
        line += if index == last_line { rows.len().saturating_sub(1) } else { rows.len() };
        if index == last_line {
            col = rows.last().map_or(0, |row| UnicodeWidthStr::width(row.as_str()));
        }
    }
    (line, col)
}

fn wrap_text_paragraph(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for word in text.split_whitespace() {
        let word_width = UnicodeWidthStr::width(word);
        if current_width + 1 + word_width > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        if word_width > width {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            }
            let mut rest = word;
            while UnicodeWidthStr::width(rest) > width {
                let mut cut = width;
                while !rest.is_char_boundary(cut) {
                    cut -= 1;
                }
                lines.push(rest[..cut].to_string());
                rest = &rest[cut..];
            }
            if !rest.is_empty() {
                current = rest.to_string();
                current_width = UnicodeWidthStr::width(rest);
            }
            continue;
        }
        if !current.is_empty() {
            current.push(' ');
            current_width += 1;
        }
        current.push_str(word);
        current_width += word_width;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

#[cfg(test)]
fn is_long_word_char(ch: char) -> bool {
    !ch.is_whitespace()
}

#[cfg(test)]
fn is_motion_char(ch: char, long_word: bool) -> bool {
    if long_word { is_long_word_char(ch) } else { is_word_char(ch) }
}

#[cfg(test)]
fn char_at(line: &str, byte: usize) -> Option<char> {
    line.get(byte..)?.chars().next()
}

pub(crate) fn previous_char_boundary(line: &str, col: usize) -> usize {
    let mut col = col.min(line.len());
    while col > 0 && !line.is_char_boundary(col) {
        col -= 1;
    }
    col
}

/// Convert a byte column to its screen column, expanding tabs to the next
/// `tab_width` stop (the editor's display tab width, not `indent_size`).
pub(crate) fn byte_col_to_display_col(line: &str, byte_col: usize, tab_width: usize) -> usize {
    let tab_width = tab_width.max(1);
    let safe = previous_char_boundary(line, byte_col.min(line.len()));
    let prefix = &line[..safe];
    if prefix.is_ascii() && !prefix.as_bytes().contains(&b'\t') {
        return safe;
    }

    let mut col = 0usize;
    for ch in prefix.chars() {
        if ch == '\t' {
            col += tab_width - (col % tab_width);
        } else {
            col += UnicodeWidthChar::width(ch).unwrap_or(0);
        }
    }
    col
}

/// Convert a screen column back to a byte column: the first glyph whose start
/// column is at or after `display_col`, with tabs expanded to the next
/// `tab_width` stop.  A column past the line end returns the line length.
pub(crate) fn display_col_to_byte(line: &str, display_col: usize, tab_width: usize) -> usize {
    let tab_width = tab_width.max(1);
    let prefix_len = display_col.min(line.len());
    let prefix = &line.as_bytes()[..prefix_len];
    if prefix.is_ascii() && !prefix.contains(&b'\t') {
        return prefix_len;
    }

    let mut col = 0usize;
    for (byte_idx, ch) in line.char_indices() {
        if col >= display_col {
            return byte_idx;
        }
        if ch == '\t' {
            col += tab_width - (col % tab_width);
        } else {
            col += UnicodeWidthChar::width(ch).unwrap_or(0);
        }
    }
    line.len()
}

#[cfg(test)]
pub(crate) fn find_char_forward(line: &str, from_byte: usize, target: char) -> Option<usize> {
    let skip = line[from_byte..].chars().next().map(|c| c.len_utf8()).unwrap_or(0);
    let start = from_byte + skip;
    line[start..].char_indices().find(|(_, c)| *c == target).map(|(off, _)| start + off)
}

#[cfg(test)]
pub(crate) fn find_char_backward(line: &str, before_byte: usize, target: char) -> Option<usize> {
    line[..before_byte].char_indices().rfind(|(_, c)| *c == target).map(|(off, _)| off)
}

#[cfg(test)]
pub(crate) fn prev_char_start(line: &str, byte: usize) -> usize {
    let mut idx = byte.saturating_sub(1);
    while idx > 0 && !line.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

#[cfg(test)]
pub(crate) fn next_char_start(line: &str, byte: usize) -> usize {
    line[byte..].chars().next().map(|c| byte + c.len_utf8()).unwrap_or(byte)
}

#[cfg(test)]
pub(crate) fn next_word_start(line: &str, byte: usize, long_word: bool) -> Option<usize> {
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

#[cfg(test)]
pub(crate) fn prev_word_start(line: &str, byte: usize, long_word: bool) -> Option<usize> {
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

#[cfg(test)]
pub(crate) fn next_word_end(line: &str, byte: usize, long_word: bool) -> Option<usize> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_text_breaks_at_whitespace_and_hard_breaks_long_words() {
        let lines = wrap_text("alpha beta gamma delta", 10);
        assert_eq!(lines, vec!["alpha beta", "gamma", "delta"]);

        let long = wrap_text("supercalifragilistic", 8);
        assert!(long.iter().all(|line| UnicodeWidthStr::width(line.as_str()) <= 8));
        assert_eq!(long.join(""), "supercalifragilistic");
    }

    #[test]
    fn wrap_text_handles_narrow_widths() {
        let lines = wrap_text("ab cd", 4);
        assert_eq!(lines, vec!["ab", "cd"]);
        assert!(!wrap_text("x", 4).is_empty());
    }

    #[test]
    fn wrap_text_preserves_explicit_newlines() {
        let lines = wrap_text("alpha beta\ngamma\n\ndelta", 20);
        assert_eq!(lines, vec!["alpha beta", "gamma", "", "delta"]);
    }

    #[test]
    fn wrap_text_hard_preserves_characters_and_breaks_at_width() {
        assert_eq!(wrap_text_hard("", 10), vec![""]);
        assert_eq!(wrap_text_hard("ab cd", 10), vec!["ab cd"]);
        assert_eq!(wrap_text_hard("abcdef", 4), vec!["abcd", "ef"]);
        assert_eq!(wrap_text_hard("  foo", 10), vec!["  foo"], "leading spaces preserved");
        assert_eq!(wrap_text_hard("foo  ", 10), vec!["foo  "], "trailing spaces preserved");
        assert_eq!(wrap_text_hard("a b c", 3), vec!["a b", " c"], "whitespace survives the break");
        // Joining the segments never loses or reorders characters.
        assert_eq!(wrap_text_hard("ab cd ef gh", 5).join(""), "ab cd ef gh");
    }

    #[test]
    fn wrap_text_hard_is_unicode_width_aware() {
        // Wide chars count as two display columns.
        assert_eq!(wrap_text_hard("界界界", 4), vec!["界界", "界"]);
        // A single char wider than the width still gets its own line.
        assert_eq!(wrap_text_hard("界x", 1), vec!["界", "x"]);
        let lines = wrap_text_hard("界abc", 3);
        assert!(lines.iter().all(|line| UnicodeWidthStr::width(line.as_str()) <= 3));
    }

    #[test]
    fn wrapped_caret_position_maps_onto_wrapped_rows() {
        assert_eq!(wrapped_caret_position("", 0, 10), (0, 0));
        assert_eq!(wrapped_caret_position("abc", 3, 10), (0, 3));
        // Caret inside the first wrapped segment.
        assert_eq!(wrapped_caret_position("abcdef", 4, 4), (0, 4));
        // Caret on the second wrapped row of the first logical line.
        assert_eq!(wrapped_caret_position("abcdef", 5, 4), (1, 1));
        // Caret on the second logical line.
        assert_eq!(wrapped_caret_position("ab\ncd", 5, 10), (1, 2));
        // Wrapped first line, caret on the second logical line.
        assert_eq!(wrapped_caret_position("abcdef\ngh", 10, 4), (2, 2));
        // Caret offsets past the end clamp to the final position.
        assert_eq!(wrapped_caret_position("abcdef", 99, 4), (1, 2));
        // Wide chars count in the wrapped column (each 界 is 3 UTF-8 bytes).
        assert_eq!(wrapped_caret_position("界界", 6, 4), (0, 4));
        assert_eq!(wrapped_caret_position("界界界", 9, 4), (1, 2));
    }
}
