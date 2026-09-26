//! Lightweight markdown rendering for assistant chat messages.
//!
//! Supports the common chat surface: `# headings`, `> quotes`, `-`/`*`/`+`
//! bullets with `- [ ]`/`- [x]` checkboxes, numbered lists, `---` thematic
//! breaks, fenced code blocks (```` ``` ```` and `~~~`), inline `**bold**`,
//! `*italic*`, `` `code` ``, and `[label](url)` links. Anything unrecognized
//! passes through as plain text so no content ever disappears.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::ui::theme;

/// One rendered source row before width wrapping.
enum Row {
    /// Styled inline content (headings, lists, paragraphs).
    Styled(Vec<Span<'static>>),
    /// Fenced code content: wrapped char-wise so indentation survives.
    Code(String),
}

/// Renders an assistant message as wrapped, styled lines. `base` is the
/// default message style; markdown constructs layer modifiers on top.
#[must_use]
pub(crate) fn markdown_message_lines(text: &str, width: usize, base: Style) -> Vec<Line<'static>> {
    let width = width.max(4);
    let rows = parse_rows(text, base);
    let mut lines = Vec::new();
    for row in rows {
        match row {
            Row::Code(code) => lines.extend(wrap_code(&code, width, code_style(base))),
            Row::Styled(spans)
                if spans.is_empty() || spans.iter().all(|span| span.content.trim().is_empty()) => {}
            Row::Styled(spans) => lines.extend(wrap_styled(spans, width)),
        }
    }
    lines
}

fn parse_rows(text: &str, base: Style) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut fence: Option<&str> = None;
    for line in text.split('\n') {
        let trimmed = line.trim_start();
        if let Some(marker) = fence {
            if trimmed.starts_with(marker) {
                fence = None;
                rows.push(Row::Styled(vec![Span::styled(line.to_string(), dim(base))]));
            } else {
                rows.push(Row::Code(line.to_string()));
            }
            continue;
        }
        if let Some(marker) = ["```", "~~~"].iter().find(|marker| trimmed.starts_with(**marker)) {
            fence = Some(marker);
            rows.push(Row::Styled(vec![Span::styled(line.to_string(), dim(base))]));
            continue;
        }
        if trimmed.starts_with('#') {
            let heading = line.trim_start_matches('#').trim_start();
            if !heading.is_empty() {
                rows.push(Row::Styled(vec![Span::styled(
                    heading.to_string(),
                    base.add_modifier(Modifier::BOLD).fg(theme::FG_KEY),
                )]));
                continue;
            }
        }
        if matches!(trimmed, "---" | "***" | "___") {
            rows.push(Row::Styled(vec![Span::styled(trimmed.to_string(), dim(base))]));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('>') {
            let mut spans = vec![Span::styled("▍ ", dim(base))];
            spans.extend(inline_spans(rest.trim_start(), base));
            rows.push(Row::Styled(spans));
            continue;
        }
        if let Some((rest, checked)) = checkbox_rest(trimmed) {
            let mut spans = vec![Span::styled(
                if checked { "☑ " } else { "☐ " },
                if checked { base.fg(theme::FG_INFO) } else { dim(base) },
            )];
            spans.extend(inline_spans(rest.trim_start(), base));
            rows.push(Row::Styled(spans));
            continue;
        }
        if let Some(rest) = bullet_rest(trimmed) {
            let mut spans = vec![Span::styled("• ", base.fg(theme::FG_KEY))];
            spans.extend(inline_spans(rest.trim_start(), base));
            rows.push(Row::Styled(spans));
            continue;
        }
        if let Some((number, rest)) = numbered_rest(trimmed) {
            let mut spans = vec![Span::styled(format!("{number}. "), base.fg(theme::FG_KEY))];
            spans.extend(inline_spans(rest.trim_start(), base));
            rows.push(Row::Styled(spans));
            continue;
        }
        rows.push(Row::Styled(inline_spans(line, base)));
    }
    rows
}

/// `- [ ]` / `- [x]` checkbox content and state.
fn checkbox_rest(line: &str) -> Option<(&str, bool)> {
    let rest = line.strip_prefix("- [")?;
    let checked = rest.starts_with('x') || rest.starts_with('X');
    let marker = if checked { 'x' } else { ' ' };
    let after =
        rest.strip_prefix(marker).or_else(|| rest.strip_prefix(if checked { 'X' } else { ' ' }))?;
    let after = after.strip_prefix(']')?;
    Some((after, checked))
}

/// `-` / `*` / `+` bullet content.
fn bullet_rest(line: &str) -> Option<&str> {
    let rest = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("+ "))?;
    (!rest.trim().is_empty()).then_some(rest)
}

/// `1.` / `1)` numbered-list item content.
fn numbered_rest(line: &str) -> Option<(usize, &str)> {
    let digits: String = line.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let rest = line[digits.len()..]
        .strip_prefix(". ")
        .or_else(|| line[digits.len()..].strip_prefix(") "))?;
    let number = digits.parse::<usize>().ok()?;
    (!rest.trim().is_empty()).then_some((number, rest))
}

/// Parses inline `**bold**`, `*italic*`, `` `code` ``, and `[label](url)`.
/// Returns `(styled, chars_eaten)` relative to `rest`; unmatched markers stay
/// literal.
fn inline_spans(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let (styled, eaten) = if let Some(after) = rest.strip_prefix("**") {
            match after.find("**") {
                Some(end) => {
                    (Some((&after[..end], base.add_modifier(Modifier::BOLD))), 2 + end + 2)
                }
                None => (None, 0),
            }
        } else if let Some(after) = rest.strip_prefix('*') {
            match after.find('*') {
                Some(end) => {
                    (Some((&after[..end], base.add_modifier(Modifier::ITALIC))), 1 + end + 1)
                }
                None => (None, 0),
            }
        } else if let Some(after) = rest.strip_prefix('`') {
            match after.find('`') {
                Some(end) => (Some((&after[..end], code_style(base))), 1 + end + 1),
                None => (None, 0),
            }
        } else if let Some(after) = rest.strip_prefix('[') {
            match after.find(']') {
                Some(close) => {
                    let label = &after[..close];
                    let linked = after[close + 1..]
                        .strip_prefix('(')
                        .and_then(|url_rest| url_rest.find(')').map(|end| (url_rest, end)));
                    match linked {
                        Some((_, end)) => (
                            Some((label, base.add_modifier(Modifier::UNDERLINED))),
                            1 + close + 1 + 1 + end + 1,
                        ),
                        None => (None, 0),
                    }
                }
                None => (None, 0),
            }
        } else {
            (None, 0)
        };
        if let Some((content, style)) = styled {
            if !content.is_empty() {
                spans.push(Span::styled(content.to_string(), style));
            }
            let eaten = eaten.min(rest.len());
            rest = &rest[eaten..];
            continue;
        }
        let next = rest.chars().next().expect("non-empty rest");
        let next_len = next.len_utf8();
        if let Some(Span { content, style: existing, .. }) = spans.last_mut()
            && *existing == base
        {
            content.to_mut().push(next);
        } else {
            spans.push(Span::styled(next.to_string(), base));
        }
        rest = &rest[next_len..];
    }
    spans
}

// ── Wrapping ────────────────────────────────────────────────────────────────

/// Greedy word wrap that preserves per-token styles. Overlong words (URLs,
/// long identifiers) hard-split at the width boundary.
fn wrap_styled(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    let total: usize = spans.iter().map(Span::width).sum();
    if total <= width {
        return vec![Line::from(spans)];
    }
    let tokens = word_tokens(spans);
    let mut lines = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut current_width = 0usize;
    for token in tokens {
        if token.content == " " {
            if current_width == 0 {
                continue;
            }
            if current_width + 1 > width {
                continue;
            }
            current.push(token);
            current_width += 1;
            continue;
        }
        let token_width = token.width();
        if current_width > 0 && current_width + 1 + token_width > width {
            lines.push(Line::from(std::mem::take(&mut current)));
            current_width = 0;
        }
        if token_width > width {
            let mut rest = token;
            while rest.width() > width {
                let take: String = rest.content.chars().take(width).collect();
                let remaining = rest.content[take.len()..].to_string();
                lines.push(Line::from(vec![Span::styled(take, rest.style)]));
                rest = Span::styled(remaining, rest.style);
            }
            if rest.width() > 0 {
                let rest_width = rest.width();
                current.push(rest);
                current_width = rest_width;
            }
            continue;
        }
        if current_width > 0 {
            current.push(Span::raw(" "));
            current_width += 1;
        }
        current.push(token);
        current_width += token_width;
    }
    if !current.is_empty() {
        lines.push(Line::from(current));
    }
    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}

/// Splits spans into word and single-space tokens, each carrying the source
/// span style so styles survive wrapping.
fn word_tokens(spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let mut tokens = Vec::new();
    for span in spans {
        let mut word = String::new();
        for ch in span.content.chars() {
            if ch == ' ' {
                if !word.is_empty() {
                    tokens.push(Span::styled(std::mem::take(&mut word), span.style));
                }
                tokens.push(Span::styled(" ".to_string(), span.style));
            } else {
                word.push(ch);
            }
        }
        if !word.is_empty() {
            tokens.push(Span::styled(word, span.style));
        }
    }
    tokens
}

/// Char-wise wrap for fenced code, preserving every space. Trailing newlines
/// render as a final blank line exactly like a plain message would.
fn wrap_code(code: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for line in code.split('\n') {
        let mut rest = line;
        while rest.chars().count() > width {
            let take: String = rest.chars().take(width).collect();
            let remaining = &rest[take.len()..];
            lines.push(Line::from(Span::styled(take, style)));
            rest = remaining;
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(rest);
    }
    lines.push(Line::from(Span::styled(current, style)));
    lines
}

fn code_style(base: Style) -> Style {
    base.fg(theme::FG_INFO)
}

fn dim(base: Style) -> Style {
    base.fg(theme::FG_DIM)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str) -> Vec<String> {
        markdown_message_lines(text, 80, Style::default())
            .into_iter()
            .map(|line| line.to_string())
            .collect()
    }

    #[test]
    fn plain_text_passes_through_unchanged() {
        let text = "Hello, this is a simple assistant reply without markdown.";
        assert_eq!(plain(text), vec![text.to_string()]);
    }

    #[test]
    fn blank_lines_are_trimmed() {
        let text = "first\n\nsecond";
        assert_eq!(plain(text), vec!["first".to_string(), "second".to_string()]);
    }

    #[test]
    fn plain_text_wraps_at_width() {
        let long = "word ".repeat(30);
        let lines = plain(&long);
        assert!(lines.len() > 1);
        for line in lines {
            assert!(line.chars().count() <= 80, "{line:?}");
        }
    }

    #[test]
    fn inline_markdown_is_styled_and_stripped() {
        let lines = markdown_message_lines("**bold** *italic* `code` plain", 80, Style::default());
        let spans = &lines[0].spans;
        assert!(
            spans
                .iter()
                .any(|s| { s.content == "bold" && s.style.add_modifier.contains(Modifier::BOLD) })
        );
        assert!(
            spans.iter().any(|s| {
                s.content == "italic" && s.style.add_modifier.contains(Modifier::ITALIC)
            })
        );
        assert!(spans.iter().any(|s| s.content == "code" && s.style.fg == Some(theme::FG_INFO)));
        assert_eq!(lines[0].to_string(), "bold italic code plain");
    }

    #[test]
    fn headings_lists_quotes_and_rules() {
        let text = "# Title\n> quote\n- one\n- [x] done\n1. step\n---";
        let lines = plain(text);
        assert_eq!(lines[0], "Title");
        assert_eq!(lines[1], "▍ quote");
        assert_eq!(lines[2], "• one");
        assert_eq!(lines[3], "☑ done");
        assert_eq!(lines[4], "1. step");
    }

    #[test]
    fn heading_style_is_bold_and_key_colored() {
        let lines = markdown_message_lines("# Title", 80, Style::default());
        assert_eq!(lines[0].to_string(), "Title");
        let span = &lines[0].spans[0];
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(span.style.fg, Some(theme::FG_KEY));
    }

    #[test]
    fn code_fences_preserve_content_and_indentation() {
        let text = "```\n  fn main() {\n    ok();\n  }\n```";
        let lines = markdown_message_lines(text, 80, Style::default());
        let bodies: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
        assert!(bodies.iter().any(|line| line == "  fn main() {"), "{bodies:?}");
        assert!(bodies.iter().any(|line| line == "    ok();"), "{bodies:?}");
        assert_eq!(bodies.first().map(String::as_str), Some("```"));
    }

    #[test]
    fn links_keep_label_and_underline() {
        let lines = markdown_message_lines("[docs](https://example.com)", 80, Style::default());
        assert_eq!(lines[0].to_string(), "docs");
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|s| s.content == "docs"
                    && s.style.add_modifier.contains(Modifier::UNDERLINED))
        );
    }

    #[test]
    fn unmatched_markers_stay_literal() {
        let text = "**unclosed and `stray";
        assert_eq!(plain(text), vec![text.to_string()]);
    }

    #[test]
    fn bold_survives_a_wrap_boundary() {
        // A bold phrase longer than the line width keeps its style across the
        // wrapped continuation line.
        let text = format!("**{}**", "word ".repeat(30).trim_end());
        let lines = markdown_message_lines(&text, 40, Style::default());
        assert!(lines.len() > 1);
        for line in &lines {
            let rendered = line.to_string();
            let words: Vec<&str> = rendered.split_whitespace().filter(|w| !w.is_empty()).collect();
            for word in words {
                let styled = line.spans.iter().any(|s| {
                    s.content.contains(word) && s.style.add_modifier.contains(Modifier::BOLD)
                });
                assert!(styled, "word {word:?} lost its bold style");
            }
        }
    }

    #[test]
    fn long_word_hard_splits_without_exceeding_width() {
        let text = "x".repeat(100);
        let lines = plain(&text);
        assert!(lines.len() >= 2);
        for line in &lines {
            assert!(line.chars().count() <= 80, "{line:?}");
        }
        assert_eq!(lines.concat(), text);
    }
}
