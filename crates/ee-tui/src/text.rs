use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(crate) fn previous_char_boundary(line: &str, col: usize) -> usize {
    let mut col = col.min(line.len());
    while col > 0 && !line.is_char_boundary(col) {
        col -= 1;
    }
    col
}

pub(crate) fn byte_col_to_display_col(line: &str, byte_col: usize) -> usize {
    let safe = previous_char_boundary(line, byte_col.min(line.len()));
    UnicodeWidthStr::width(&line[..safe])
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

pub(crate) fn find_char_forward(line: &str, from_byte: usize, target: char) -> Option<usize> {
    let skip = line[from_byte..].chars().next().map(|c| c.len_utf8()).unwrap_or(0);
    let start = from_byte + skip;
    line[start..].char_indices().find(|(_, c)| *c == target).map(|(off, _)| start + off)
}

pub(crate) fn find_char_backward(line: &str, before_byte: usize, target: char) -> Option<usize> {
    line[..before_byte].char_indices().rfind(|(_, c)| *c == target).map(|(off, _)| off)
}

pub(crate) fn prev_char_start(line: &str, byte: usize) -> usize {
    let mut idx = byte.saturating_sub(1);
    while idx > 0 && !line.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

pub(crate) fn next_char_start(line: &str, byte: usize) -> usize {
    line[byte..].chars().next().map(|c| byte + c.len_utf8()).unwrap_or(byte)
}
