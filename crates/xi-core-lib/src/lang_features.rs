// Copyright 2024 The xi-editor Authors.
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

//! Language-sensitive edit features backed by `syntect`.
//!
//! Phase 2: reindent and toggle-comment are implemented in-core using syntect
//! for language detection rather than dispatching to plugin processes.

use std::sync::LazyLock;

use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};
use xi_rope::{DeltaBuilder, Interval, Rope, RopeDelta};

/// Process-global `SyntaxSet` loaded from bundled defaults.
/// Loaded lazily on first use; shared read-only across all callers.
static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

// ---------------------------------------------------------------------------
// Comment token lookup
// ---------------------------------------------------------------------------

/// Map a syntect scope root (e.g. `source.rust`) to its line-comment token.
///
/// Only scopes with a well-defined single-character line comment are covered.
/// Block-only languages (HTML, CSS) return `None`.
#[allow(dead_code)]
fn scope_to_line_comment(scope_root: &str) -> Option<&'static str> {
    // Match on the leading component so sub-scopes (e.g. `source.rust.embedded`)
    // are covered by the same rule.
    if scope_root.starts_with("source.rust") {
        return Some("//");
    }
    if scope_root.starts_with("source.c")
        || scope_root.starts_with("source.c++")
        || scope_root.starts_with("source.objc")
    {
        return Some("//");
    }
    if scope_root.starts_with("source.java")
        || scope_root.starts_with("source.groovy")
        || scope_root.starts_with("source.kotlin")
        || scope_root.starts_with("source.scala")
        || scope_root.starts_with("source.swift")
        || scope_root.starts_with("source.cs")
        || scope_root.starts_with("source.dart")
    {
        return Some("//");
    }
    if scope_root.starts_with("source.js")
        || scope_root.starts_with("source.ts")
        || scope_root.starts_with("source.jsx")
        || scope_root.starts_with("source.tsx")
    {
        return Some("//");
    }
    if scope_root.starts_with("source.go") {
        return Some("//");
    }
    if scope_root.starts_with("source.php") {
        return Some("//");
    }
    if scope_root.starts_with("source.python") {
        return Some("#");
    }
    if scope_root.starts_with("source.ruby") {
        return Some("#");
    }
    if scope_root.starts_with("source.sh")
        || scope_root.starts_with("source.shell")
        || scope_root.starts_with("source.bash")
        || scope_root.starts_with("source.fish")
        || scope_root.starts_with("source.zsh")
    {
        return Some("#");
    }
    if scope_root.starts_with("source.perl") {
        return Some("#");
    }
    if scope_root.starts_with("source.r") {
        return Some("#");
    }
    if scope_root.starts_with("source.yaml") {
        return Some("#");
    }
    if scope_root.starts_with("source.toml") {
        return Some("#");
    }
    if scope_root.starts_with("source.makefile") {
        return Some("#");
    }
    if scope_root.starts_with("source.elixir") {
        return Some("#");
    }
    if scope_root.starts_with("source.haskell")
        || scope_root.starts_with("source.elm")
        || scope_root.starts_with("source.lua")
        || scope_root.starts_with("source.sql")
    {
        return Some("--");
    }
    if scope_root.starts_with("source.lisp")
        || scope_root.starts_with("source.clojure")
        || scope_root.starts_with("source.racket")
    {
        return Some(";");
    }
    if scope_root.starts_with("source.erlang") {
        return Some("%");
    }
    if scope_root.starts_with("source.matlab") || scope_root.starts_with("source.octave") {
        return Some("%");
    }
    None
}

/// Returns the line-comment token for `language_name` using the bundled
/// syntect syntax definitions, or `None` if the language is unknown or has
/// no single-line comment form.
#[allow(dead_code)]
pub(crate) fn line_comment_token(language_name: &str) -> Option<&'static str> {
    let syntax = SYNTAX_SET.find_syntax_by_name(language_name)?;
    let scope_root = syntax.scope.to_string();
    scope_to_line_comment(&scope_root)
}

fn scope_to_block_comment(scope_root: &str) -> Option<(&'static str, &'static str)> {
    if scope_root.starts_with("source.rust")
        || scope_root.starts_with("source.c")
        || scope_root.starts_with("source.c++")
        || scope_root.starts_with("source.objc")
        || scope_root.starts_with("source.java")
        || scope_root.starts_with("source.groovy")
        || scope_root.starts_with("source.kotlin")
        || scope_root.starts_with("source.scala")
        || scope_root.starts_with("source.swift")
        || scope_root.starts_with("source.cs")
        || scope_root.starts_with("source.dart")
        || scope_root.starts_with("source.js")
        || scope_root.starts_with("source.ts")
        || scope_root.starts_with("source.jsx")
        || scope_root.starts_with("source.tsx")
        || scope_root.starts_with("source.go")
        || scope_root.starts_with("source.php")
        || scope_root.starts_with("source.css")
        || scope_root.starts_with("source.scss")
        || scope_root.starts_with("source.less")
        || scope_root.starts_with("source.sql")
    {
        return Some(("/*", "*/"));
    }
    if scope_root.starts_with("text.html")
        || scope_root.starts_with("text.xml")
        || scope_root.starts_with("text.sgml")
    {
        return Some(("<!--", "-->"));
    }
    None
}

pub(crate) fn block_comment_tokens(language_name: &str) -> Option<(&'static str, &'static str)> {
    let syntax = SYNTAX_SET.find_syntax_by_name(language_name)?;
    let scope_root = syntax.scope.to_string();
    scope_to_block_comment(&scope_root)
}

// ---------------------------------------------------------------------------
// Toggle-comment
// ---------------------------------------------------------------------------

/// Toggle line comments on the selected line ranges.
///
/// If **all** lines in the union of `line_ranges` are already commented with
/// `token`, the comments are removed; otherwise `token` is prepended (after
/// any leading whitespace) to every uncommented line.
///
/// Returns `None` when:
/// - `language_name` has no known line-comment token, or
/// - the text would not change (e.g. all lines are blank).
#[allow(dead_code)]
pub(crate) fn toggle_comment(
    text: &Rope,
    line_ranges: &[(usize, usize)],
    language_name: &str,
) -> Option<RopeDelta> {
    let token = line_comment_token(language_name)?;
    let token_sp = format!("{token} "); // preferred form when adding

    // Collect the distinct line indices covered by the selections.
    let lines = collect_lines(line_ranges);
    if lines.is_empty() {
        return None;
    }

    // Determine whether every non-blank line is already commented.
    let all_commented = lines.iter().all(|&ln| {
        let content = line_content(text, ln);
        let trimmed = content.trim_start();
        trimmed.is_empty() || trimmed.starts_with(token)
    });

    let mut builder = DeltaBuilder::new(text.len());
    let mut changed = false;

    for &ln in &lines {
        let content = line_content(text, ln);
        let trimmed = content.trim_start();

        if trimmed.is_empty() {
            // Skip blank lines.
            continue;
        }

        let line_start = text.offset_of_line(ln);

        if all_commented {
            // Remove the comment token (and an optional trailing space).
            let ws_len = content.len() - trimmed.len();
            let ws_end = line_start + ws_len;
            let after_token = trimmed
                .strip_prefix(&token_sp)
                .unwrap_or_else(|| trimmed.strip_prefix(token).unwrap_or(trimmed));
            let remove_end = ws_end + (trimmed.len() - after_token.len());
            if ws_end < remove_end {
                builder.delete(Interval::new(ws_end, remove_end));
                changed = true;
            }
        } else {
            // Add `token ` before the first non-whitespace character.
            let ws_len = content.len() - trimmed.len();
            let insert_offset = line_start + ws_len;
            // Skip lines that are already commented (partial selection).
            if trimmed.starts_with(token) {
                continue;
            }
            builder.replace(Interval::new(insert_offset, insert_offset), token_sp.clone().into());
            changed = true;
        }
    }

    if !changed {
        return None;
    }
    Some(builder.build())
}

pub(crate) fn toggle_block_comment(
    text: &Rope,
    selections: &[(usize, usize)],
    language_name: &str,
) -> Option<RopeDelta> {
    let (open, close) = block_comment_tokens(language_name)?;
    let ranges = collect_comment_ranges(text, selections);
    if ranges.is_empty() {
        return None;
    }

    let all_commented = ranges.iter().all(|range| is_block_commented(text, range, open, close));
    let mut builder = DeltaBuilder::new(text.len());
    let mut changed = false;

    for range in ranges {
        let segment: String = text.slice_to_cow(range.start..range.end).into_owned();
        let leading_ws = segment.len() - segment.trim_start().len();
        let trailing_ws = segment.len() - segment.trim_end().len();
        let inner_start = range.start + leading_ws;
        let inner_end = range.end.saturating_sub(trailing_ws);
        if inner_start >= inner_end {
            continue;
        }

        if all_commented {
            let inner = &segment[leading_ws..segment.len() - trailing_ws];
            let after_open = inner.strip_prefix(open).unwrap_or(inner);
            let open_remove_end = inner_start + (inner.len() - after_open.len());
            let open_space_end = open_remove_end + usize::from(after_open.starts_with(' '));
            if inner_start < open_space_end {
                builder.delete(Interval::new(inner_start, open_space_end));
                changed = true;
            }

            let before_close = after_open
                .strip_suffix(close)
                .unwrap_or_else(|| after_open.trim_end_matches(' ').strip_suffix(close).unwrap());
            let close_keep_start = inner_end - (after_open.len() - before_close.len());
            let close_remove_start = close_keep_start
                .saturating_sub(usize::from(inner[..inner.len() - close.len()].ends_with(' ')));
            if close_remove_start < inner_end {
                builder.delete(Interval::new(close_remove_start, inner_end));
                changed = true;
            }
        } else {
            let prefix = if range.spaced { format!("{open} ") } else { open.to_owned() };
            let suffix = if range.spaced { format!(" {close}") } else { close.to_owned() };
            builder.replace(Interval::new(inner_start, inner_start), prefix.into());
            builder.replace(Interval::new(inner_end, inner_end), suffix.into());
            changed = true;
        }
    }

    if !changed {
        return None;
    }
    Some(builder.build())
}

// ---------------------------------------------------------------------------
// Reindent
// ---------------------------------------------------------------------------

/// Re-indent the selected line ranges using syntect for bracket-aware parsing.
///
/// The algorithm:
/// 1. Parse the entire buffer from the start with syntect's `ParseState` to
///    build per-line scope context.
/// 2. At each line boundary, count opening (`{`, `[`, `(`) and closing
///    (`}`, `]`, `)`) tokens that appear **outside** string and comment scopes.
/// 3. Rewrite the leading whitespace of every line inside `line_ranges` to
///    match the computed indent level, using `indent_str` as one level unit.
///
/// Returns `None` when:
/// - `language_name` is not found in the bundled syntect syntax set,
/// - the buffer is empty, or
/// - no lines would actually change.
pub(crate) fn reindent(
    text: &Rope,
    line_ranges: &[(usize, usize)],
    language_name: &str,
    indent_str: &str,
) -> Option<RopeDelta> {
    if text.is_empty() || line_ranges.is_empty() || indent_str.is_empty() {
        return None;
    }

    let syntax = SYNTAX_SET.find_syntax_by_name(language_name)?;

    let total_lines = text.measure::<xi_rope::LinesMetric>() + 1;
    // Highest line index we need to visit.
    let max_line = line_ranges.iter().map(|&(_, e)| e).max().unwrap_or(0).min(total_lines - 1);

    // Build per-line indent-delta using syntect parsing.
    // `indent_level[i]` = the expected indent level at the *start* of line i.
    let mut indent_level: Vec<i64> = vec![0i64; max_line + 2];

    let mut parse_state = ParseState::new(syntax);
    let mut scope_stack = ScopeStack::new();
    let mut current_level: i64 = 0;

    for (ln, level_slot) in indent_level.iter_mut().enumerate().take(max_line + 1) {
        let line_str = line_content(text, ln);
        let trimmed = line_str.trim_start();
        let first_nonws = trimmed.chars().next();

        // Lines starting with a closer belong at one level above the opener.
        let target_level = match first_nonws {
            Some('}' | ']' | ')') => (current_level - 1).max(0),
            _ => current_level.max(0),
        };
        *level_slot = target_level;

        // Parse the line to advance the scope state and count brackets.
        let owned;
        let line_for_parse: &str = if line_str.ends_with('\n') {
            &line_str
        } else {
            owned = format!("{line_str}\n");
            &owned
        };

        let ops = match parse_state.parse_line(line_for_parse, &SYNTAX_SET) {
            Ok(ops) => ops,
            Err(_) => continue,
        };

        let line_bytes = line_for_parse.as_bytes();
        let limit = line_for_parse.len().saturating_sub(1);
        let mut op_idx = 0;

        // Walk every byte of the line (excluding the trailing newline).
        for (byte_off, &ch_byte) in line_bytes.iter().enumerate().take(limit) {
            // Apply all scope ops that fire at or before this byte position.
            while op_idx < ops.len() && ops[op_idx].0 <= byte_off {
                let _ = scope_stack.apply(&ops[op_idx].1);
                op_idx += 1;
            }
            let ch = ch_byte as char;
            if !in_string_or_comment(&scope_stack) {
                match ch {
                    '{' | '[' | '(' => current_level += 1,
                    '}' | ']' | ')' => current_level -= 1,
                    _ => {}
                }
            }
        }
        // Flush any remaining ops at end of line.
        while op_idx < ops.len() {
            let _ = scope_stack.apply(&ops[op_idx].1);
            op_idx += 1;
        }
    }

    // Build the delta: replace leading whitespace of each targeted line.
    let target_lines = collect_lines(line_ranges);
    let mut builder = DeltaBuilder::new(text.len());
    let mut changed = false;

    for &ln in &target_lines {
        if ln >= total_lines {
            continue;
        }
        let expected_level = indent_level[ln].max(0) as usize;
        let expected_ws = indent_str.repeat(expected_level);

        let content = line_content(text, ln);
        let ws_len = content.len() - content.trim_start().len();
        let existing_ws = &content[..ws_len];

        if existing_ws == expected_ws {
            continue;
        }

        let line_start = text.offset_of_line(ln);
        let ws_end = line_start + ws_len;
        builder.replace(Interval::new(line_start, ws_end), expected_ws.into());
        changed = true;
    }

    if !changed {
        return None;
    }
    Some(builder.build())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Collect sorted, deduplicated line indices from a set of `(start, end)` ranges
/// (both inclusive) as returned by `EventContext::selected_line_ranges`.
fn collect_lines(line_ranges: &[(usize, usize)]) -> Vec<usize> {
    let mut lines: Vec<usize> = line_ranges.iter().flat_map(|&(start, end)| start..=end).collect();
    lines.sort_unstable();
    lines.dedup();
    lines
}

#[derive(Clone, Copy)]
struct CommentRange {
    start: usize,
    end: usize,
    spaced: bool,
}

fn collect_comment_ranges(text: &Rope, selections: &[(usize, usize)]) -> Vec<CommentRange> {
    let mut ranges = Vec::new();

    for &(start, end) in selections {
        if start == end {
            let line = text.line_of_offset(start.min(text.len()));
            let line_start = text.offset_of_line(line);
            let total = text.measure::<xi_rope::LinesMetric>() + 1;
            let line_end =
                if line + 1 < total { text.offset_of_line(line + 1) } else { text.len() };
            let raw: String = text.slice_to_cow(line_start..line_end).into_owned();
            let content = raw.trim_end_matches('\n').trim_end_matches('\r');
            let leading_ws = content.len() - content.trim_start().len();
            let trailing_ws = content.len() - content.trim_end().len();
            let inner_start = line_start + leading_ws;
            let inner_end = line_start + content.len().saturating_sub(trailing_ws);
            if inner_start < inner_end {
                ranges.push(CommentRange { start: inner_start, end: inner_end, spaced: true });
            }
            continue;
        }

        let start = start.min(text.len());
        let end = end.min(text.len());
        if start < end {
            ranges.push(CommentRange { start, end, spaced: false });
        }
    }

    ranges.sort_unstable_by_key(|range| (range.start, range.end));
    ranges.dedup_by(|right, left| right.start == left.start && right.end == left.end);
    ranges
}

fn is_block_commented(text: &Rope, range: &CommentRange, open: &str, close: &str) -> bool {
    let segment: String = text.slice_to_cow(range.start..range.end).into_owned();
    let trimmed = segment.trim();
    trimmed.starts_with(open) && trimmed.ends_with(close)
}

/// Return the text of line `ln` as a `String`, **without** a trailing newline.
///
/// Returns an empty string if `ln` is beyond the end of the buffer.
fn line_content(text: &Rope, ln: usize) -> String {
    let total = text.measure::<xi_rope::LinesMetric>() + 1;
    if ln >= total {
        return String::new();
    }
    let start = text.offset_of_line(ln);
    let end = if ln + 1 < total { text.offset_of_line(ln + 1) } else { text.len() };
    let raw: String = text.slice_to_cow(start..end).into_owned();
    // strip trailing newline characters
    raw.trim_end_matches('\n').trim_end_matches('\r').to_owned()
}

/// Returns `true` if the current scope stack indicates we are inside a string
/// literal or a comment.
fn in_string_or_comment(stack: &ScopeStack) -> bool {
    stack.as_slice().iter().any(|scope| {
        let s = scope.to_string();
        s.starts_with("string") || s.starts_with("comment")
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn rope(s: &str) -> Rope {
        Rope::from(s)
    }

    /// Apply a `RopeDelta` to a `Rope` and return the result as a `String`.
    fn apply_delta(text: Rope, delta: RopeDelta) -> String {
        String::from(delta.apply(&text))
    }

    #[test]
    fn test_line_comment_token_rust() {
        assert_eq!(line_comment_token("Rust"), Some("//"));
    }

    #[test]
    fn test_line_comment_token_python() {
        assert_eq!(line_comment_token("Python"), Some("#"));
    }

    #[test]
    fn test_line_comment_token_unknown() {
        assert_eq!(line_comment_token("NonExistentLang"), None);
    }

    #[test]
    fn test_block_comment_tokens_css() {
        assert_eq!(block_comment_tokens("CSS"), Some(("/*", "*/")));
    }

    #[test]
    fn test_toggle_comment_add() {
        let text = rope("fn main() {\n    let x = 1;\n}\n");
        let line_ranges = vec![(0usize, 0usize)];
        let delta = toggle_comment(&text, &line_ranges, "Rust").unwrap();
        let result = apply_delta(text, delta);
        assert!(result.starts_with("// fn main()"), "got: {result:?}");
    }

    #[test]
    fn test_toggle_comment_remove() {
        let text = rope("// fn main() {\n    let x = 1;\n}\n");
        let line_ranges = vec![(0usize, 0usize)];
        let delta = toggle_comment(&text, &line_ranges, "Rust").unwrap();
        let result = apply_delta(text, delta);
        assert!(result.starts_with("fn main()"), "got: {result:?}");
    }

    #[test]
    fn test_toggle_comment_indented() {
        // Indented line: comment token should go after the whitespace.
        let text = rope("    let x = 1;\n");
        let line_ranges = vec![(0usize, 0usize)];
        let delta = toggle_comment(&text, &line_ranges, "Rust").unwrap();
        let result = apply_delta(text, delta);
        assert_eq!(result, "    // let x = 1;\n");
    }

    #[test]
    fn test_toggle_comment_multiline_all_commented_removes() {
        let text = rope("// a\n// b\n// c\n");
        let line_ranges = vec![(0usize, 2usize)];
        let delta = toggle_comment(&text, &line_ranges, "Rust").unwrap();
        let result = apply_delta(text, delta);
        assert_eq!(result, "a\nb\nc\n");
    }

    #[test]
    fn test_toggle_comment_multiline_mixed_adds() {
        // Mixed: not all commented → add to all.
        let text = rope("// a\nb\n");
        let line_ranges = vec![(0usize, 1usize)];
        let delta = toggle_comment(&text, &line_ranges, "Rust").unwrap();
        let result = apply_delta(text, delta);
        // Line 0 is already commented, line 1 should get a comment added.
        assert!(result.contains("// b"), "got: {result:?}");
    }

    #[test]
    fn test_toggle_block_comment_add_current_line() {
        let text = rope("    color: red;\n");
        let selections = vec![(0usize, 0usize)];
        let delta = toggle_block_comment(&text, &selections, "CSS").unwrap();
        let result = apply_delta(text, delta);
        assert_eq!(result, "    /* color: red; */\n");
    }

    #[test]
    fn test_toggle_block_comment_remove_current_line() {
        let text = rope("    /* color: red; */\n");
        let selections = vec![(0usize, 0usize)];
        let delta = toggle_block_comment(&text, &selections, "CSS").unwrap();
        let result = apply_delta(text, delta);
        assert_eq!(result, "    color: red;\n");
    }

    #[test]
    fn test_toggle_block_comment_selection_without_spaces() {
        let text = rope("abc def\n");
        let selections = vec![(0usize, 3usize)];
        let delta = toggle_block_comment(&text, &selections, "CSS").unwrap();
        let result = apply_delta(text, delta);
        assert_eq!(result, "/*abc*/ def\n");
    }

    #[test]
    fn test_collect_lines_dedup() {
        let ranges = vec![(0usize, 2usize), (1usize, 3usize)];
        let lines = collect_lines(&ranges);
        assert_eq!(lines, vec![0, 1, 2, 3]);
    }

    #[test]
    fn test_line_content_basic() {
        let text = rope("hello\nworld\n");
        assert_eq!(line_content(&text, 0), "hello");
        assert_eq!(line_content(&text, 1), "world");
    }
}
