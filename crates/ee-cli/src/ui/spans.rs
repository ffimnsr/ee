//! UI: spans rendering.
use super::*;

pub(super) fn theme_style(color: Color) -> Style {
    Style::default().fg(color)
}

// ── Status bar ───────────────────────────────────────────────────────────────

/// Compute the gutter column width based on display settings and line count.
pub(super) fn gutter_width(app: &App, line_count: usize) -> u16 {
    let digits = line_count.max(1).to_string().len().max(3);
    let num_cols = digits + 1; // trailing space
    let sign_cols: usize = if app.config.sign_column { 2 } else { 0 };
    (num_cols + sign_cols) as u16
}

/// Return the visible editor column count (buffer text area width, excluding
/// the gutter) for the current app state and terminal size. Pass this to
/// `scroll_into_view` so the horizontal viewport is clamped correctly.
pub(crate) fn compute_editor_width(terminal_size: ratatui::layout::Rect, app: &App) -> usize {
    let area = split_root_areas(terminal_size, app).editor_area;
    let line_count = app.backend.line_count().max(1);
    let gw = gutter_width(app, line_count);
    area.width.saturating_sub(gw + BUFFER_LEFT_PADDING_COLS) as usize
}

// ── Visible-whitespace substitution ──────────────────────────────────────────

/// Substitute space `' '` → `'·'` and tab `'\t'` → `'→'` in rendered spans,
/// applying a dimmed style to the replaced characters.
pub(super) fn apply_visible_whitespace(spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let dim = Style::default().fg(theme::FG_SUBTLE);
    let mut out: Vec<Span<'static>> = Vec::new();
    for span in spans {
        let style = span.style;
        let mut current = String::new();
        let mut current_is_ws = false;
        for ch in span.content.chars() {
            let is_ws = ch == ' ' || ch == '\t';
            let disp = match ch {
                ' ' => '·',
                '\t' => '→',
                c => c,
            };
            if is_ws != current_is_ws && !current.is_empty() {
                let s = if current_is_ws { dim } else { style };
                out.push(Span::styled(current.clone(), s));
                current.clear();
            }
            current_is_ws = is_ws;
            current.push(disp);
        }
        if !current.is_empty() {
            let s = if current_is_ws { dim } else { style };
            out.push(Span::styled(current, s));
        }
    }
    out
}

pub(super) fn control_picture(ch: char) -> Option<char> {
    match ch {
        '\0'..='\u{1f}' if ch != '\t' => char::from_u32(0x2400 + ch as u32),
        '\u{7f}' => Some('\u{2421}'),
        _ => None,
    }
}

/// Replace raw control characters with visible glyphs before terminal paint.
///
/// This keeps files containing legacy CR-only line endings or pasted control
/// bytes from moving the terminal cursor and corrupting the frame.
pub(super) fn sanitize_control_chars(spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    spans
        .into_iter()
        .map(|span| {
            let mut text = String::with_capacity(span.content.len());
            let mut changed = false;
            for ch in span.content.chars() {
                if let Some(picture) = control_picture(ch) {
                    text.push(picture);
                    changed = true;
                } else {
                    text.push(ch);
                }
            }
            if changed { Span::styled(text, span.style) } else { span }
        })
        .collect()
}

// ── Color column injection ────────────────────────────────────────────────────

/// Inject a distinct background color at display column `screen_col` within
/// the given span list.  Characters at that column keep their foreground but
/// get the color-column background.  When the line is shorter than `screen_col`,
/// a trailing colored space is appended.
pub(super) fn apply_color_column(
    spans: Vec<Span<'static>>,
    screen_col: usize,
) -> Vec<Span<'static>> {
    let col_bg = theme::BG_COLOR_COLUMN;
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;
    let mut injected = false;

    for span in spans {
        if injected {
            out.push(span);
            continue;
        }
        let style = span.style;
        let content: Vec<char> = span.content.chars().collect();
        let span_cols = content.len();
        if col + span_cols <= screen_col {
            // Color column is beyond this span.
            col += span_cols;
            out.push(span);
        } else {
            // Color column falls within this span.
            let offset = screen_col - col;
            let before: String = content[..offset].iter().collect();
            let at_ch: String = content[offset..offset + 1].iter().collect();
            let after: String = content[offset + 1..].iter().collect();
            if !before.is_empty() {
                out.push(Span::styled(before, style));
            }
            out.push(Span::styled(at_ch, style.bg(col_bg)));
            if !after.is_empty() {
                out.push(Span::styled(after, style));
            }
            col += span_cols;
            injected = true;
        }
    }

    // If the line was shorter than the color column, append a trailing marker.
    if !injected {
        let pad = screen_col.saturating_sub(col);
        if pad > 0 {
            out.push(Span::raw(" ".repeat(pad)));
        }
        out.push(Span::styled(" ", Style::default().bg(col_bg)));
    }
    out
}

pub(super) fn pad_spans_to_width(
    mut spans: Vec<Span<'static>>,
    width: usize,
    style: Style,
) -> Vec<Span<'static>> {
    let used =
        spans.iter().map(|span| UnicodeWidthStr::width(span.content.as_ref())).sum::<usize>();
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), style));
    }
    spans
}

pub(super) fn expand_tabs_in_spans(
    spans: Vec<Span<'static>>,
    tab_width: usize,
) -> Vec<Span<'static>> {
    let tab_width = tab_width.max(1);
    let mut out = Vec::with_capacity(spans.len());
    let mut col = 0usize;
    for span in spans {
        let style = span.style;
        let mut text = String::new();
        for ch in span.content.chars() {
            if ch == '\t' {
                let width = tab_width - (col % tab_width);
                text.push_str(&" ".repeat(width));
                col += width;
            } else {
                text.push(ch);
                col += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            }
        }
        if !text.is_empty() {
            out.push(Span::styled(text, style));
        }
    }
    out
}

/// Overlay visual-mode selection highlight on a rendered span list.
///
/// `col_start`/`col_end` are display-column bounds (inclusive); pass `None`
/// for `col_end` to highlight the entire line (VisualLine / whole-line block
/// rows).  `left` is the horizontal scroll offset already applied.  The
/// visual background replaces each span's background in the selected range
/// while preserving foreground colours.
pub(super) fn apply_visual_highlight(
    spans: Vec<Span<'static>>,
    col_start: Option<usize>,
    col_end: Option<usize>,
    left: usize,
) -> Vec<Span<'static>> {
    let vis_bg = theme::BG_SELECTION;
    let vis_fg = theme::FG_TEXT;

    // Whole-line highlight (VisualLine): just paint every span.
    if col_start.is_none() {
        let mut out = Vec::with_capacity(spans.len());
        for sp in spans {
            out.push(Span::styled(sp.content.into_owned(), sp.style.bg(vis_bg).fg(vis_fg)));
        }
        // Ensure at least one space so the highlight is visible on empty lines.
        if out.is_empty() {
            out.push(Span::styled(" ", Style::default().bg(vis_bg)));
        }
        return out;
    }

    let sel_start = col_start.unwrap().saturating_sub(left);
    // None col_end already handled above; here it means "to EOL" within block mode.
    let sel_end = col_end.map(|e| e.saturating_sub(left));

    let mut out: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize; // display column cursor

    for sp in spans {
        let style = sp.style;
        let chars: Vec<char> = sp.content.chars().collect();
        let sp_len = chars.len();

        // Fast path: entire span is outside the selection.
        let sp_end = col + sp_len;
        if sp_end <= sel_start || sel_end.is_some_and(|e| col > e) {
            col = sp_end;
            out.push(Span::styled(chars.iter().collect::<String>(), style));
            continue;
        }

        // The span overlaps the selection — split into up to three parts.
        let mut i = 0usize; // index into `chars`
        // Part before selection.
        let before = sel_start.saturating_sub(col);
        if before > 0 && i < chars.len() {
            let end = before.min(chars.len());
            out.push(Span::styled(chars[i..end].iter().collect::<String>(), style));
            i = end;
        }
        // Selected part.
        let sel_end_in_span = sel_end.map(|e| (e + 1).saturating_sub(col)).unwrap_or(sp_len);
        if i < chars.len() {
            let end = sel_end_in_span.min(chars.len());
            if i < end {
                out.push(Span::styled(
                    chars[i..end].iter().collect::<String>(),
                    style.bg(vis_bg).fg(vis_fg),
                ));
                i = end;
            }
        }
        // Part after selection.
        if i < chars.len() {
            out.push(Span::styled(chars[i..].iter().collect::<String>(), style));
        }

        col = sp_end;
    }

    // If the selection extends past the end of the line content, append a
    // trailing highlighted space so the block is always visible.
    let line_end = col;
    if sel_end.is_none() || sel_end.is_some_and(|e| e >= line_end) {
        if line_end <= sel_start {
            // Selection starts beyond line end — pad with spaces.
            let pad = sel_start - line_end;
            if pad > 0 {
                out.push(Span::raw(" ".repeat(pad)));
            }
        }
        out.push(Span::styled(" ", Style::default().bg(vis_bg)));
    }

    out
}

/// Overlay search match highlighting on an already-rendered span list.
///
/// `line` is the full line byte string, `pattern` is the raw search query,
/// `byte_start..byte_end` is the rendered slice, and `bg` is the background
/// colour inherited from the cursor-line flag.
pub(super) fn apply_search_highlights(
    spans: Vec<Span<'static>>,
    line: &str,
    pattern: &str,
    byte_start: usize,
    byte_end: usize,
    bg: Color,
) -> Vec<Span<'static>> {
    // Build case-aware regex from the plain-text pattern.
    let case_insensitive = !smart_case_sensitive(pattern);
    let re_src = if case_insensitive {
        format!("(?i){}", regex::escape(pattern))
    } else {
        regex::escape(pattern)
    };
    let re = match regex::Regex::new(&re_src) {
        Ok(r) => r,
        Err(_) => return spans,
    };

    let Some(search_slice) = line.get(byte_start..byte_end.min(line.len())) else {
        return spans;
    };

    let matches: Vec<(usize, usize)> = re
        .find_iter(search_slice)
        .map(|m| (byte_start + m.start(), byte_start + m.end()))
        .collect();
    if matches.is_empty() {
        return spans;
    }

    let match_hl =
        Style::default().fg(theme::BG_APP).bg(theme::BG_FIND).add_modifier(Modifier::BOLD);

    // Re-build spans, splitting on match boundaries inside the rendered slice.
    let mut out: Vec<Span<'static>> = Vec::new();
    // Accumulate raw bytes across all input spans so we can apply match ranges.
    // Build a flat (byte_offset, char_group, style) representation first.
    let mut flat: Vec<(String, Style)> = Vec::new();
    for span in &spans {
        let content = span.content.as_ref();
        let style = span.style;
        flat.push((content.to_owned(), style));
    }

    // Re-emit spans split by match ranges.
    let mut byte_pos = byte_start; // position in `line` of the start of the current flat span
    for (content, base_style) in flat {
        let span_start = byte_pos;
        let span_end = byte_pos + content.len();
        byte_pos = span_end;

        // Find matches that overlap this span.
        let mut local_pos = 0usize; // position within `content` (bytes)
        for &(ms, me) in &matches {
            if me <= span_start || ms >= span_end {
                continue; // no overlap
            }
            let rel_start = ms.saturating_sub(span_start);
            let rel_end = me.min(span_end) - span_start;
            // Emit text before the match.
            if rel_start > local_pos {
                let s = content[local_pos..rel_start].to_owned();
                out.push(Span::styled(s, base_style.bg(bg)));
            }
            // Emit the match.
            let s = content[rel_start.min(content.len())..rel_end.min(content.len())].to_owned();
            if !s.is_empty() {
                out.push(Span::styled(s, match_hl));
            }
            local_pos = rel_end;
        }
        // Emit remainder.
        if local_pos < content.len() {
            out.push(Span::styled(content[local_pos..].to_owned(), base_style.bg(bg)));
        }
    }

    if out.is_empty() { spans } else { out }
}
