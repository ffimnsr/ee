use unicode_width::UnicodeWidthChar;

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

pub(crate) fn byte_col_to_display_col(line: &str, byte_col: usize) -> usize {
    let safe = previous_char_boundary(line, byte_col.min(line.len()));
    let prefix = &line[..safe];
    if prefix.is_ascii() && !prefix.as_bytes().contains(&b'\t') {
        return safe;
    }

    let mut col = 0usize;
    for ch in prefix.chars() {
        if ch == '\t' {
            let tab_width = 4 - (col % 4);
            col += tab_width;
        } else {
            col += UnicodeWidthChar::width(ch).unwrap_or(0);
        }
    }
    col
}

pub(crate) fn display_col_to_byte(line: &str, display_col: usize) -> usize {
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
            col += 4 - (col % 4);
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
