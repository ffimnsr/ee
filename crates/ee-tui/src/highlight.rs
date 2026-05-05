//! Syntax highlighting for the `ee-tui` frontend.
//!
//! Backend semantic syntax spans are preferred when xi-core provides them.
//! Local `syntect` parsing remains as whole-line fallback for lines without
//! backend syntax data.

use ratatui::style::{Color, Style};
use ratatui::text::Span;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

use crate::backend::CoreSyntaxSpan;

/// A highlighted span: a foreground color and the text of that span.
pub(crate) type HlSpan = (Color, String);

/// Loaded once at startup; passed by reference into `render_buffer`.
pub(crate) struct Highlighter {
    syntax_set: SyntaxSet,
    theme: Theme,
}

impl std::fmt::Debug for Highlighter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Highlighter").finish_non_exhaustive()
    }
}

impl Highlighter {
    /// Build a `Highlighter` using bundled grammars and the "base16-ocean.dark" theme.
    pub(crate) fn new() -> Self {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let theme_set = ThemeSet::load_defaults();
        // "base16-ocean.dark" ships with syntect's default theme set and works
        // well on a dark terminal background.
        let theme = theme_set.themes.get("base16-ocean.dark").cloned().unwrap_or_else(|| {
            theme_set.themes.values().next().cloned().expect("syntect default themes are empty")
        });
        Self { syntax_set, theme }
    }

    /// Highlight the lines visible in the current viewport for syntect fallback.
    ///
    /// * `lines`     — all buffer lines (may be large; only visible range is returned)
    /// * `extension` — file extension used to select the grammar (e.g. `"rs"`, `"py"`)
    /// * `top`       — index of the first visible line
    /// * `count`     — number of visible lines (height of the editor area)
    ///
    /// Returns one `Vec<HlSpan>` per visible line. The spans for each line
    /// concatenate to the full line text. If a line has no highlights (e.g.
    /// plain text fallback) its span vec holds a single entry with the default
    /// foreground color. `ui.rs` only uses this output when backend
    /// `syntax_spans` are absent for the line.
    ///
    /// To build correct incremental state, the function processes every line
    /// from 0 up to `top + count`.  For most files (< ~10 k lines visible) this
    /// is fast enough at 60 fps; a checkpoint cache can be added in Phase 2.
    pub(crate) fn highlight_visible(
        &self,
        lines: &[String],
        extension: Option<&str>,
        top: usize,
        count: usize,
    ) -> Vec<Vec<HlSpan>> {
        if lines.is_empty() || count == 0 {
            return Vec::new();
        }

        let syntax = extension
            .and_then(|ext| self.syntax_set.find_syntax_by_extension(ext))
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());

        let mut hl = HighlightLines::new(syntax, &self.theme);
        let visible_end = (top + count).min(lines.len());
        let mut result = Vec::with_capacity(visible_end.saturating_sub(top));

        for (i, line) in lines.iter().enumerate().take(visible_end) {
            // syntect expects a trailing newline for accurate state transitions
            // in some grammars (e.g. multi-line string detection).
            let owned;
            let line_str: &str = if line.ends_with('\n') {
                line.as_str()
            } else {
                owned = format!("{line}\n");
                &owned
            };

            match hl.highlight_line(line_str, &self.syntax_set) {
                Ok(ranges) if i >= top => {
                    let spans: Vec<HlSpan> = ranges
                        .iter()
                        .filter(|(_, text)| !text.is_empty())
                        .map(|(style, text)| {
                            let c = style.foreground;
                            (Color::Rgb(c.r, c.g, c.b), (*text).trim_end_matches('\n').to_owned())
                        })
                        .collect();
                    result.push(spans);
                }
                Ok(_) => {
                    // Pre-top line: advance state only, do not collect.
                }
                Err(_) if i >= top => {
                    result.push(Vec::new());
                }
                Err(_) => {}
            }
        }

        result
    }

    /// Produce `ratatui` [`Span`]s from pre-computed `HlSpan`s, applying
    /// a left-column byte offset for horizontal scrolling.
    ///
    /// `byte_start` is the byte index within the original line text at which
    /// the visible viewport begins (as returned by `display_col_to_byte`).
    /// Spans that fall entirely to the left of `byte_start` are discarded;
    /// spans that straddle `byte_start` are sliced at the boundary.
    pub(crate) fn spans_with_offset(
        hl_spans: &[HlSpan],
        byte_start: usize,
    ) -> Vec<ratatui::text::Span<'static>> {
        let mut out = Vec::new();
        let mut offset = 0usize;

        for (color, text) in hl_spans {
            let end = offset + text.len();
            if end <= byte_start {
                offset = end;
                continue;
            }
            let slice_start = byte_start.saturating_sub(offset);
            // Safety: `byte_start` is a char boundary in the full line, and
            // `offset` tracks exact byte positions of syntect span boundaries
            // (which are also char boundaries), so `slice_start` is valid.
            let visible = &text[slice_start..];
            if !visible.is_empty() {
                out.push(Span::styled(visible.to_owned(), Style::default().fg(*color)));
            }
            offset = end;
        }

        out
    }

    pub(crate) fn scope_spans_with_offset(
        line: &str,
        syntax_spans: &[CoreSyntaxSpan],
        byte_start: usize,
    ) -> Vec<Span<'static>> {
        let mut out = Vec::new();
        let mut cursor = 0usize;

        for span in syntax_spans {
            let start = span.start_byte.min(line.len());
            let end = span.end_byte.min(line.len());
            if end <= start {
                continue;
            }
            if cursor < start {
                Self::push_segment(&mut out, line, cursor, start, byte_start, Style::default());
            }
            Self::push_segment(
                &mut out,
                line,
                start,
                end,
                byte_start,
                Self::style_for_scope(&span.scope),
            );
            cursor = end;
        }

        if cursor < line.len() {
            Self::push_segment(&mut out, line, cursor, line.len(), byte_start, Style::default());
        }

        if out.is_empty() && byte_start <= line.len() {
            out.push(Span::styled(line[byte_start..].to_owned(), Style::default()));
        }

        out
    }

    fn push_segment(
        out: &mut Vec<Span<'static>>,
        line: &str,
        start: usize,
        end: usize,
        byte_start: usize,
        style: Style,
    ) {
        if end <= start || end <= byte_start {
            return;
        }
        let visible_start = start.max(byte_start);
        let Some(text) = line.get(visible_start..end) else {
            return;
        };
        if !text.is_empty() {
            out.push(Span::styled(text.to_owned(), style));
        }
    }

    fn style_for_scope(scope: &str) -> Style {
        for candidate in Self::scope_candidates(scope) {
            if let Some(style) = Self::lookup_scope_style(&candidate) {
                return style;
            }
        }
        Style::default()
    }

    fn scope_candidates(scope: &str) -> Vec<String> {
        let scope = scope.trim();
        if scope.is_empty() {
            return Vec::new();
        }

        let mut candidates = Vec::new();
        let stack: Vec<&str> = scope.split_whitespace().collect();

        for entry in stack.iter().rev() {
            let mut current = (*entry).to_owned();
            loop {
                candidates.push(current.clone());
                let Some((parent, _)) = current.rsplit_once('.') else {
                    break;
                };
                current = parent.to_owned();
            }
        }

        for len in (1..=stack.len()).rev() {
            candidates.push(stack[..len].join(" "));
        }

        candidates
    }

    fn lookup_scope_style(scope: &str) -> Option<Style> {
        let color = if scope.starts_with("comment") {
            Color::Rgb(101, 115, 126)
        } else if scope.starts_with("string") {
            Color::Rgb(195, 151, 66)
        } else if scope.starts_with("constant.numeric")
            || scope.starts_with("constant.language")
            || scope.starts_with("constant.character")
        {
            Color::Rgb(211, 120, 70)
        } else if scope.starts_with("keyword")
            || scope.starts_with("storage")
            || scope == "support.macro"
        {
            Color::Rgb(180, 142, 173)
        } else if scope.starts_with("entity.name.function")
            || scope.starts_with("support.function")
            || scope.starts_with("meta.function")
        {
            Color::Rgb(136, 192, 208)
        } else if scope.starts_with("entity.name.type")
            || scope.starts_with("support.type")
            || scope.starts_with("storage.type")
            || scope.starts_with("entity.name.namespace")
        {
            Color::Rgb(143, 188, 187)
        } else if scope.starts_with("variable") {
            Color::Rgb(216, 222, 233)
        } else if scope.starts_with("entity.name.tag") || scope.starts_with("keyword.operator") {
            Color::Rgb(129, 161, 193)
        } else if scope.starts_with("punctuation") {
            Color::Rgb(171, 178, 191)
        } else if scope.starts_with("invalid") {
            Color::Rgb(239, 83, 80)
        } else if scope.starts_with("markup.heading") {
            Color::Rgb(94, 129, 172)
        } else {
            return None;
        };
        Some(Style::default().fg(color))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_highlighter() -> Highlighter {
        Highlighter::new()
    }

    #[test]
    fn highlight_visible_rust_basic() {
        let hl = make_highlighter();
        let lines = vec!["fn main() {".to_owned(), "    let x = 42;".to_owned(), "}".to_owned()];
        let result = hl.highlight_visible(&lines, Some("rs"), 0, 3);
        assert_eq!(result.len(), 3);
        // Each visible line must produce at least one span.
        for (i, spans) in result.iter().enumerate() {
            assert!(!spans.is_empty(), "line {i} produced no spans");
        }
        // Concatenated span text should round-trip to the original line.
        for (i, (spans, original)) in result.iter().zip(lines.iter()).enumerate() {
            let joined: String = spans.iter().map(|(_, t)| t.as_str()).collect();
            assert_eq!(joined, original.as_str(), "line {i} text mismatch");
        }
    }

    #[test]
    fn highlight_visible_respects_top_offset() {
        let hl = make_highlighter();
        let lines: Vec<String> = (0..10).map(|i| format!("// line {i}")).collect();
        // Request only lines 3..6.
        let result = hl.highlight_visible(&lines, Some("rs"), 3, 3);
        assert_eq!(result.len(), 3);
        for (i, (spans, original)) in result.iter().zip(lines[3..6].iter()).enumerate() {
            let joined: String = spans.iter().map(|(_, t)| t.as_str()).collect();
            assert_eq!(joined, original.as_str(), "line {i} text mismatch");
        }
    }

    #[test]
    fn highlight_visible_empty_lines() {
        let hl = make_highlighter();
        let result = hl.highlight_visible(&[], Some("rs"), 0, 10);
        assert!(result.is_empty());
    }

    #[test]
    fn highlight_visible_count_zero() {
        let hl = make_highlighter();
        let lines = vec!["fn main() {}".to_owned()];
        let result = hl.highlight_visible(&lines, Some("rs"), 0, 0);
        assert!(result.is_empty());
    }

    #[test]
    fn highlight_visible_unknown_extension_fallback() {
        let hl = make_highlighter();
        let lines = vec!["hello world".to_owned()];
        // Unknown extension falls back to plain text; should still produce spans.
        let result = hl.highlight_visible(&lines, Some("zzz_unknown"), 0, 1);
        assert_eq!(result.len(), 1);
        let joined: String = result[0].iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(joined, "hello world");
    }

    #[test]
    fn spans_with_offset_trims_left() {
        let spans = vec![
            (Color::Rgb(255, 0, 0), "hello ".to_owned()),
            (Color::Rgb(0, 255, 0), "world".to_owned()),
        ];
        // byte_start = 6 should skip "hello " entirely
        let result = Highlighter::spans_with_offset(&spans, 6);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "world");
    }

    #[test]
    fn spans_with_offset_splits_span() {
        let spans = vec![(Color::Rgb(255, 0, 0), "hello world".to_owned())];
        // byte_start = 6 should yield "world"
        let result = Highlighter::spans_with_offset(&spans, 6);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "world");
    }

    #[test]
    fn spans_with_offset_zero_noop() {
        let spans = vec![(Color::Rgb(255, 0, 0), "hello".to_owned())];
        let result = Highlighter::spans_with_offset(&spans, 0);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "hello");
    }

    #[test]
    fn scope_spans_with_offset_uses_backend_ranges() {
        let line = "let answer = 42;";
        let spans = vec![
            CoreSyntaxSpan {
                start_byte: 0,
                end_byte: 3,
                scope: "keyword.control.rust".into(),
            },
            CoreSyntaxSpan {
                start_byte: 13,
                end_byte: 15,
                scope: "constant.numeric.decimal.rust".into(),
            },
        ];

        let result = Highlighter::scope_spans_with_offset(line, &spans, 0);
        let joined: String = result.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(joined, line);
        assert_eq!(result[0].content, "let");
        assert!(result.iter().any(|span| span.content == "42"));
    }

    #[test]
    fn scope_spans_with_offset_respects_scroll_offset() {
        let line = "let answer = 42;";
        let spans = vec![CoreSyntaxSpan {
            start_byte: 13,
            end_byte: 15,
            scope: "constant.numeric.decimal.rust".into(),
        }];

        let result = Highlighter::scope_spans_with_offset(line, &spans, 4);
        let joined: String = result.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(joined, "answer = 42;");
    }

    #[test]
    fn style_for_scope_falls_back_to_parent_segments() {
        let style = Highlighter::style_for_scope("meta.function variable.parameter.rust");
        assert_eq!(style.fg, Some(Color::Rgb(216, 222, 233)));
    }
}
