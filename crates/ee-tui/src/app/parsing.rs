use super::*;

pub(crate) fn line_col_for_offset(lines: &[String], offset: usize) -> (usize, usize) {
    let mut remaining = offset;
    for (line_index, line) in lines.iter().enumerate() {
        let line_len = line.len();
        if remaining <= line_len {
            return (line_index, remaining);
        }
        remaining = remaining.saturating_sub(line_len + 1);
    }
    let line = lines.len().saturating_sub(1);
    let col = lines.get(line).map(|line| line.len()).unwrap_or(0);
    (line, col)
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_big_word_char(c: char) -> bool {
    !c.is_whitespace()
}

/// Inner / outer word text object.  `big_word` = WORD (non-whitespace) mode.
pub(crate) fn text_obj_word(
    line: &str,
    cursor: usize,
    inclusive: bool,
    big_word: bool,
) -> Option<(usize, usize)> {
    let pred: fn(char) -> bool = if big_word { is_big_word_char } else { is_word_char };

    let first = line[cursor..].chars().next()?;
    if !pred(first) {
        return None;
    }

    let mut start = cursor;
    for (i, c) in line[..cursor].char_indices().rev() {
        if pred(c) {
            start = i;
        } else {
            break;
        }
    }

    let mut end = cursor + first.len_utf8();
    for (off, c) in line[cursor + first.len_utf8()..].char_indices() {
        if pred(c) {
            end = cursor + first.len_utf8() + off + c.len_utf8();
        } else {
            break;
        }
    }

    if inclusive {
        for (off, c) in line[end..].char_indices() {
            if c == ' ' || c == '\t' {
                end += c.len_utf8();
                let _ = off;
            } else {
                break;
            }
        }
    }
    Some((start, end))
}

/// Inner / outer quote text object (`"`, `'`, `` ` ``).
pub(crate) fn text_obj_quote(
    line: &str,
    cursor: usize,
    quote: char,
    inclusive: bool,
) -> Option<(usize, usize)> {
    let positions: Vec<usize> =
        line.char_indices().filter(|(_, c)| *c == quote).map(|(i, _)| i).collect();

    let mut i = 0;
    while i + 1 < positions.len() {
        let open = positions[i];
        let close = positions[i + 1];
        if open <= cursor && cursor <= close {
            return if inclusive {
                Some((open, close + quote.len_utf8()))
            } else {
                let inner_start = open + quote.len_utf8();
                if inner_start <= close { Some((inner_start, close)) } else { None }
            };
        }
        i += 2;
    }
    None
}

/// Inner / outer bracket text object.  Finds innermost matching pair
/// that contains cursor on current line.
pub(crate) fn text_obj_bracket(
    line: &str,
    cursor: usize,
    open: char,
    close: char,
    inclusive: bool,
) -> Option<(usize, usize)> {
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let cur_idx = chars.partition_point(|(i, _)| *i < cursor);

    let mut depth = 0i32;
    let mut open_pos = None;
    for &(i, c) in chars[..cur_idx.min(chars.len())].iter().rev() {
        if c == close {
            depth += 1;
        } else if c == open {
            if depth == 0 {
                open_pos = Some(i);
                break;
            }
            depth -= 1;
        }
    }
    let open_pos = open_pos?;

    let start_idx = chars.partition_point(|(i, _)| *i <= open_pos);
    let mut depth = 1i32;
    let mut close_pos = None;
    for &(i, c) in &chars[start_idx..] {
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                close_pos = Some(i);
                break;
            }
        }
    }
    let close_pos = close_pos?;

    if inclusive {
        Some((open_pos, close_pos + close.len_utf8()))
    } else {
        let inner_start = open_pos + open.len_utf8();
        if inner_start <= close_pos { Some((inner_start, close_pos)) } else { None }
    }
}

/// Inner / outer tag text object (`<tag>…</tag>`). Same-line only.
pub(crate) fn text_obj_tag(line: &str, cursor: usize, inclusive: bool) -> Option<(usize, usize)> {
    let open_angle = line[..=cursor.min(line.len().saturating_sub(1))]
        .rfind('<')
        .filter(|&pos| line[pos..].contains('>'))?;
    let open_close_angle = open_angle + line[open_angle..].find('>')?;
    let tag_body = &line[open_angle + 1..open_close_angle];

    if tag_body.starts_with('/') || tag_body.ends_with('/') {
        return None;
    }
    let tag_name: &str = tag_body.split_whitespace().next()?;

    let content_start = open_close_angle + 1;
    let close_tag = format!("</{}>", tag_name);
    let close_start =
        line[content_start..].find(close_tag.as_str()).map(|off| content_start + off)?;

    if inclusive {
        Some((open_angle, close_start + close_tag.len()))
    } else {
        Some((content_start, close_start))
    }
}

/// Returns `true` if `query` should be treated as case-sensitive.
pub(crate) fn smart_case_sensitive(query: &str) -> bool {
    query.chars().any(|c| c.is_uppercase())
}

/// Parse `:s/pattern/replacement/flags` command body.
pub(crate) fn parse_substitute_cmd(body: &str) -> Option<(String, String, String)> {
    let delim = body.chars().next()?;
    if delim.is_alphanumeric() || delim == ' ' {
        return None;
    }
    let rest = &body[delim.len_utf8()..];
    let mut parts: Vec<String> = Vec::with_capacity(3);
    let mut current = String::new();
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&nc) = chars.peek() {
                if nc == delim {
                    chars.next();
                    current.push(nc);
                    continue;
                }
            }
            current.push(c);
        } else if c == delim {
            parts.push(std::mem::take(&mut current));
            if parts.len() == 3 {
                break;
            }
        } else {
            current.push(c);
        }
    }
    if parts.len() < 3 {
        parts.push(current);
    }

    let mut parts_iter = parts.into_iter();
    let pattern = parts_iter.next().unwrap_or_default();
    let replacement = parts_iter.next().unwrap_or_default();
    let flags = parts_iter.next().unwrap_or_default();
    Some((pattern, replacement, flags))
}

/// Parse vim-style address range from start of `input`.
pub(crate) fn parse_ex_range<'a>(
    input: &'a str,
    cursor_line: usize,
    line_count: usize,
    marks: &HashMap<char, (usize, usize)>,
) -> (Option<(usize, usize)>, &'a str) {
    let mut pos = 0;
    let bytes = input.as_bytes();

    if bytes.first() == Some(&b'%') {
        let end = line_count.saturating_sub(1);
        return (Some((0, end)), &input[1..]);
    }

    let Some(a1) = parse_addr(input, &mut pos, cursor_line, line_count, marks) else {
        return (None, input);
    };

    if bytes.get(pos) == Some(&b',') {
        pos += 1;
        if let Some(a2) = parse_addr(input, &mut pos, cursor_line, line_count, marks) {
            return (Some((a1, a2)), &input[pos..]);
        }
    }

    (Some((a1, a1)), &input[pos..])
}

fn parse_addr(
    input: &str,
    pos: &mut usize,
    cursor_line: usize,
    line_count: usize,
    marks: &HashMap<char, (usize, usize)>,
) -> Option<usize> {
    let bytes = input.as_bytes();
    let base: usize = match bytes.get(*pos)? {
        b'.' => {
            *pos += 1;
            cursor_line
        }
        b'$' => {
            *pos += 1;
            line_count.saturating_sub(1)
        }
        b'\'' => {
            *pos += 1;
            let ch = input[*pos..].chars().next()?;
            *pos += ch.len_utf8();
            marks.get(&ch).map(|&(l, _)| l)?
        }
        b if b.is_ascii_digit() => {
            let start = *pos;
            while *pos < input.len() && input.as_bytes()[*pos].is_ascii_digit() {
                *pos += 1;
            }
            let n: usize = input[start..*pos].parse().ok()?;
            n.saturating_sub(1).min(line_count.saturating_sub(1))
        }
        _ => return None,
    };

    let mut val = base;
    loop {
        match bytes.get(*pos) {
            Some(b'+') => {
                *pos += 1;
                let n = parse_number(input, pos).unwrap_or(1);
                val = val.saturating_add(n);
            }
            Some(b'-') => {
                *pos += 1;
                let n = parse_number(input, pos).unwrap_or(1);
                val = val.saturating_sub(n);
            }
            _ => break,
        }
    }

    Some(val.min(line_count.saturating_sub(1)))
}

fn parse_number(input: &str, pos: &mut usize) -> Option<usize> {
    let start = *pos;
    while *pos < input.len() && input.as_bytes()[*pos].is_ascii_digit() {
        *pos += 1;
    }
    if *pos == start {
        return None;
    }
    input[start..*pos].parse().ok()
}

#[cfg(test)]
mod range_tests {
    use super::parse_ex_range;
    use std::collections::HashMap;

    fn no_marks() -> HashMap<char, (usize, usize)> {
        HashMap::new()
    }

    #[test]
    fn bare_number_jumps_to_line() {
        let (range, rest) = parse_ex_range("5", 0, 10, &no_marks());
        assert_eq!(range, Some((4, 4)));
        assert_eq!(rest, "");
    }

    #[test]
    fn number_with_command() {
        let (range, rest) = parse_ex_range("3d", 0, 10, &no_marks());
        assert_eq!(range, Some((2, 2)));
        assert_eq!(rest, "d");
    }

    #[test]
    fn percent_is_whole_file() {
        let (range, rest) = parse_ex_range("%d", 0, 10, &no_marks());
        assert_eq!(range, Some((0, 9)));
        assert_eq!(rest, "d");
    }

    #[test]
    fn comma_range() {
        let (range, rest) = parse_ex_range("1,5d", 0, 10, &no_marks());
        assert_eq!(range, Some((0, 4)));
        assert_eq!(rest, "d");
    }

    #[test]
    fn dot_is_current_line() {
        let (range, rest) = parse_ex_range(".d", 3, 10, &no_marks());
        assert_eq!(range, Some((3, 3)));
        assert_eq!(rest, "d");
    }

    #[test]
    fn dollar_is_last_line() {
        let (range, rest) = parse_ex_range("$", 0, 10, &no_marks());
        assert_eq!(range, Some((9, 9)));
        assert_eq!(rest, "");
    }

    #[test]
    fn dot_comma_dollar() {
        let (range, rest) = parse_ex_range(".,$ d", 2, 10, &no_marks());
        assert_eq!(range, Some((2, 9)));
        assert_eq!(rest, " d");
    }

    #[test]
    fn offset_plus() {
        let (range, rest) = parse_ex_range(".+2d", 3, 10, &no_marks());
        assert_eq!(range, Some((5, 5)));
        assert_eq!(rest, "d");
    }

    #[test]
    fn no_range_returns_none() {
        let (range, rest) = parse_ex_range("w", 0, 10, &no_marks());
        assert_eq!(range, None);
        assert_eq!(rest, "w");
    }
}
