//! Syntax highlighting for the `ee-cli` frontend.
//!
//! Render-only layer. Backend syntax spans are authoritative; this module only
//! maps scope strings to styles and slices spans for visible text.

use ratatui::style::{Color, Style};
use ratatui::text::Span;
use std::path::Path;
use xi_core_lib::tree_sitter_support::language_name_for_path;

use crate::backend::CoreSyntaxSpan;

/// Loaded once at startup; used only for frontend syntax-span rendering.
pub(crate) struct Highlighter;

impl std::fmt::Debug for Highlighter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Highlighter").finish_non_exhaustive()
    }
}

impl Highlighter {
    pub(crate) const fn new() -> Self {
        Self
    }

    pub(crate) fn syntax_name_for_path(&self, path: Option<&Path>) -> Option<String> {
        path.and_then(language_name_for_path)
    }

    pub(crate) fn scope_spans_in_range(
        line: &str,
        syntax_spans: &[CoreSyntaxSpan],
        byte_start: usize,
        byte_end: usize,
    ) -> Vec<Span<'static>> {
        let mut out = Vec::new();
        let mut cursor = 0usize;
        let byte_end = byte_end.min(line.len());

        for span in syntax_spans {
            let start = span.start_byte.min(line.len());
            let end = span.end_byte.min(line.len());
            if end <= start {
                continue;
            }
            if cursor < start {
                Self::push_segment(
                    &mut out,
                    line,
                    cursor,
                    start,
                    byte_start,
                    byte_end,
                    Style::default(),
                );
            }
            Self::push_segment(
                &mut out,
                line,
                start,
                end,
                byte_start,
                byte_end,
                Self::style_for_scope(&span.scope),
            );
            cursor = end;
        }

        if cursor < line.len() {
            Self::push_segment(
                &mut out,
                line,
                cursor,
                line.len(),
                byte_start,
                byte_end,
                Style::default(),
            );
        }

        if out.is_empty() && byte_start < byte_end {
            out.push(Span::styled(line[byte_start..byte_end].to_owned(), Style::default()));
        }

        out
    }

    fn push_segment(
        out: &mut Vec<Span<'static>>,
        line: &str,
        start: usize,
        end: usize,
        byte_start: usize,
        byte_end: usize,
        style: Style,
    ) {
        if end <= start || end <= byte_start || start >= byte_end {
            return;
        }
        let visible_start = start.max(byte_start);
        let visible_end = end.min(byte_end);
        let Some(text) = line.get(visible_start..visible_end) else {
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
    use std::path::PathBuf;

    fn make_highlighter() -> Highlighter {
        Highlighter::new()
    }

    #[test]
    fn syntax_name_for_path_uses_core_registry() {
        let hl = make_highlighter();
        assert_eq!(hl.syntax_name_for_path(Some(&PathBuf::from("main.rs"))), Some("Rust".into()));
        assert_eq!(
            hl.syntax_name_for_path(Some(&PathBuf::from("component.tsx"))),
            Some("TypeScript".into())
        );
        assert_eq!(hl.syntax_name_for_path(Some(&PathBuf::from("notes.txt"))), None);
    }

    #[test]
    fn scope_spans_in_range_uses_backend_ranges() {
        let line = "let answer = 42;";
        let spans = vec![
            CoreSyntaxSpan { start_byte: 0, end_byte: 3, scope: "keyword.control.rust".into() },
            CoreSyntaxSpan {
                start_byte: 13,
                end_byte: 15,
                scope: "constant.numeric.decimal.rust".into(),
            },
        ];

        let result = Highlighter::scope_spans_in_range(line, &spans, 0, line.len());
        let joined: String = result.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(joined, line);
        assert_eq!(result[0].content, "let");
        assert!(result.iter().any(|span| span.content == "42"));
    }

    #[test]
    fn scope_spans_in_range_respects_scroll_offset() {
        let line = "let answer = 42;";
        let spans = vec![CoreSyntaxSpan {
            start_byte: 13,
            end_byte: 15,
            scope: "constant.numeric.decimal.rust".into(),
        }];

        let result = Highlighter::scope_spans_in_range(line, &spans, 4, line.len());
        let joined: String = result.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(joined, "answer = 42;");
    }

    #[test]
    fn scope_spans_in_range_trims_right() {
        let line = "let answer = 42;";
        let spans = vec![CoreSyntaxSpan {
            start_byte: 0,
            end_byte: 3,
            scope: "keyword.control.rust".into(),
        }];

        let result = Highlighter::scope_spans_in_range(line, &spans, 0, 3);
        let joined: String = result.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(joined, "let");
    }

    #[test]
    fn style_for_scope_falls_back_to_parent_segments() {
        let style = Highlighter::style_for_scope("meta.function variable.parameter.rust");
        assert_eq!(style.fg, Some(Color::Rgb(216, 222, 233)));
    }
}
