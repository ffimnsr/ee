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

//! Temporary compatibility layer for language-sensitive edit features.
//!
//! Runtime responsibilities that must survive full tree-sitter cutover:
//! toggle line comment, toggle block comment, reindent dispatch, and language
//! capability downgrade behavior. Comment toggles already read registry
//! metadata; reindent now routes through shared tree-sitter helpers.

use xi_rope::{DeltaBuilder, Interval, Rope, RopeDelta};

#[cfg(test)]
use crate::text_store::DocumentMode;
#[cfg(test)]
use crate::tree_sitter_support::syntax_feature_availability;
use crate::tree_sitter_support::{
    BlockCommentStyle, LineCommentStyle, indentation_levels_for_text, language_metadata_for_name,
};

/// Returns the line-comment token for `language_name` from shared language
/// metadata, or `None` if the language is unknown or has no single-line
/// comment form.
#[allow(dead_code)]
pub(crate) fn line_comment_token(language_name: &str) -> Option<&'static str> {
    match language_metadata_for_name(language_name)?.line_comment {
        LineCommentStyle::Unsupported => None,
        LineCommentStyle::Token(token) => Some(token),
    }
}

pub(crate) fn block_comment_tokens(language_name: &str) -> Option<(&'static str, &'static str)> {
    match language_metadata_for_name(language_name)?.block_comment {
        BlockCommentStyle::Unsupported => None,
        BlockCommentStyle::Tokens { open, close } => Some((open, close)),
    }
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

/// Returns `true` when built-in tree-sitter reindent supports `language_name`.
///
/// Unknown languages return `false`; callers should fall back to plugin
/// dispatch instead of starting an async whole-scan task.
#[cfg(test)]
fn language_supports_reindent(language_name: &str) -> bool {
    syntax_feature_availability(Some(language_name), None, DocumentMode::Normal).reindent
}

/// Re-indent selected line ranges using shared tree-sitter indentation levels.
///
/// The algorithm:
/// 1. Parse the buffer with the canonical tree-sitter language registry.
/// 2. Compute per-line indent levels from syntax-tree block structure plus
///    explicit dedent triggers for closers and Python clause lines.
/// 3. Rewrite the leading whitespace of every line inside `line_ranges` to
///    match the computed indent level, using `indent_str` as one level unit.
///
/// Returns `None` when:
///
/// - `language_name` has no safe built-in tree-sitter indentation strategy,
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

    let total_lines = text.measure::<xi_rope::LinesMetric>() + 1;
    // Highest line index we need to visit.
    let max_line = line_ranges.iter().map(|&(_, e)| e).max().unwrap_or(0).min(total_lines - 1);
    let text_snapshot: String = text.slice_to_cow(0..text.len()).into_owned();
    let indent_levels = indentation_levels_for_text(language_name, &text_snapshot, max_line)?;

    // Build the delta: replace leading whitespace of each targeted line.
    let target_lines = collect_lines(line_ranges);
    let mut builder = DeltaBuilder::new(text.len());
    let mut changed = false;

    for &ln in &target_lines {
        if ln >= total_lines {
            continue;
        }
        let expected_level = indent_levels.get(ln).copied().unwrap_or_default();
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
    fn test_line_comment_token_aliases_use_shared_registry() {
        assert_eq!(line_comment_token("tsx"), Some("//"));
        assert_eq!(line_comment_token("python3"), Some("#"));
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
    fn test_block_comment_tokens_html() {
        assert_eq!(block_comment_tokens("HTML"), Some(("<!--", "-->")));
    }

    #[test]
    fn test_language_supports_reindent_uses_registry_metadata() {
        assert!(!language_supports_reindent("Bash"));
        assert!(!language_supports_reindent("CSS"));
        assert!(!language_supports_reindent("JSON"));
        assert!(language_supports_reindent("TypeScript"));
        assert!(language_supports_reindent("tsx"));
        assert!(language_supports_reindent("Rust"));
        assert!(language_supports_reindent("Python"));
        assert!(!language_supports_reindent("HTML"));
        assert!(!language_supports_reindent("Ruby"));
        assert!(!language_supports_reindent("NonExistentLang"));
    }

    #[test]
    fn test_reindent_rust_uses_tree_sitter_levels() {
        let text = rope("fn main() {\nlet value = 1;\n}\n");
        let delta = reindent(&text, &[(0usize, 2usize)], "Rust", "    ").unwrap();
        let result = apply_delta(text, delta);
        assert_eq!(result, "fn main() {\n    let value = 1;\n}\n");
    }

    #[test]
    fn test_reindent_python_dedents_else_clause() {
        let text = rope("if ready:\nprint('yes')\nelse:\nprint('no')\n");
        let delta = reindent(&text, &[(0usize, 3usize)], "Python", "    ").unwrap();
        let result = apply_delta(text, delta);
        assert_eq!(result, "if ready:\n    print('yes')\nelse:\n    print('no')\n");
    }

    #[test]
    fn test_reindent_typescript_uses_tree_sitter_levels() {
        let text = rope("function demo() {\nconsole.log(1);\n}\n");
        let delta = reindent(&text, &[(0usize, 2usize)], "TypeScript", "    ").unwrap();
        let result = apply_delta(text, delta);
        assert_eq!(result, "function demo() {\n    console.log(1);\n}\n");
    }

    #[test]
    fn test_reindent_c_like_language_uses_tree_sitter_levels() {
        let text = rope("int main() {\nreturn 0;\n}\n");
        let delta = reindent(&text, &[(0usize, 2usize)], "C", "    ").unwrap();
        let result = apply_delta(text, delta);
        assert_eq!(result, "int main() {\n    return 0;\n}\n");
    }

    #[test]
    fn test_reindent_returns_none_for_explicitly_unsupported_language() {
        let text = rope("<div>\n<span>hi</span>\n</div>\n");
        assert!(reindent(&text, &[(0usize, 2usize)], "HTML", "    ").is_none());
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
    fn test_toggle_comment_preserves_blank_lines() {
        let text = rope("alpha\n\ncharlie\n");
        let line_ranges = vec![(0usize, 2usize)];
        let delta = toggle_comment(&text, &line_ranges, "Python").unwrap();
        let result = apply_delta(text, delta);
        assert_eq!(result, "# alpha\n\n# charlie\n");
    }

    #[test]
    fn test_toggle_comment_python_uses_metadata_token() {
        let text = rope("print('hi')\n");
        let line_ranges = vec![(0usize, 0usize)];
        let delta = toggle_comment(&text, &line_ranges, "Python").unwrap();
        let result = apply_delta(text, delta);
        assert_eq!(result, "# print('hi')\n");
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
    fn test_toggle_block_comment_html_current_line() {
        let text = rope("<span>hi</span>\n");
        let selections = vec![(0usize, 0usize)];
        let delta = toggle_block_comment(&text, &selections, "HTML").unwrap();
        let result = apply_delta(text, delta);
        assert_eq!(result, "<!-- <span>hi</span> -->\n");
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

    #[test]
    fn test_unknown_language_degrades_without_delta() {
        let text = rope("value\n");
        assert!(toggle_comment(&text, &[(0, 0)], "NonExistentLang").is_none());
        assert!(toggle_block_comment(&text, &[(0, 0)], "NonExistentLang").is_none());
        assert!(reindent(&text, &[(0, 0)], "NonExistentLang", "    ").is_none());
    }
}
