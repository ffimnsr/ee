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

/// Hardcoded whitespace markers, in the spirit of vim's `listchars`.
///
/// The characters are fixed for now; a customizable `listchars`-style config
/// value is deliberately out of scope.  The renderer and its tests share these
/// constants so a future config surface only has to feed them.
pub(super) const SPACE_GLYPH: char = '·';
pub(super) const TAB_GLYPH: char = '→';
pub(super) const LEAD_GLYPH: char = '╎';
pub(super) const TRAIL_GLYPH: char = '•';
pub(super) const NBSP_GLYPH: char = '␣';
pub(super) const EOL_GLYPH: char = '↵';
pub(super) const EXTENDS_GLYPH: char = '>';
pub(super) const PRECEDES_GLYPH: char = '<';

/// Dim style shared by every whitespace marker.
pub(super) fn whitespace_marker_style() -> Style {
    Style::default().fg(theme::FG_SUBTLE)
}

/// Per-line facts for the visible-whitespace pass, all in absolute display
/// columns (never viewport-relative) so horizontal scrolling cannot skew tab
/// stops or marker placement.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct WhitespaceLayout {
    /// Display column of the first cell in the rendered span list.
    pub(super) start_col: usize,
    /// Display column where the line's leading whitespace run ends (`0` = no
    /// leading whitespace).  Leading spaces render as `LEAD_GLYPH`.
    pub(super) lead_end_col: usize,
    /// Display column where the line's trailing whitespace run starts.
    /// Trailing spaces render as `TRAIL_GLYPH` unless the run is also leading
    /// (a whitespace-only line prefers the lead marker).
    pub(super) trail_start_col: Option<usize>,
    /// Append the end-of-line marker after the last rendered cell.
    pub(super) eol: bool,
}

/// Substitute the hardcoded whitespace markers in one rendered line.
///
/// Space → `SPACE_GLYPH`, leading space → `LEAD_GLYPH`, trailing space →
/// `TRAIL_GLYPH`, non-breaking space → `NBSP_GLYPH`; tab → `TAB_GLYPH` plus
/// `SPACE_GLYPH` padding up to the next `tab_width` stop.  Tabs are expanded
/// here rather than by [`expand_tabs_in_spans`] so the markers keep the source
/// text's display columns: cursor placement reads the raw line, not these
/// glyphs.  Markers render dimmed; everything else keeps its span style.
pub(super) fn apply_visible_whitespace(
    spans: Vec<Span<'static>>,
    tab_width: usize,
    layout: WhitespaceLayout,
) -> Vec<Span<'static>> {
    let tab_width = tab_width.max(1);
    let dim = whitespace_marker_style();
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_style = Style::default();
    let mut col = layout.start_col;

    for span in spans {
        for ch in span.content.chars() {
            match ch {
                '\t' => {
                    let width = tab_width - (col % tab_width);
                    flush_run(&mut out, &mut run, &mut run_style, dim);
                    run.push(TAB_GLYPH);
                    for _ in 1..width {
                        run.push(SPACE_GLYPH);
                    }
                    col += width;
                }
                ' ' => {
                    flush_run(&mut out, &mut run, &mut run_style, dim);
                    run.push(space_glyph(col, &layout));
                    col += 1;
                }
                '\u{a0}' => {
                    flush_run(&mut out, &mut run, &mut run_style, dim);
                    run.push(NBSP_GLYPH);
                    col += 1;
                }
                c => {
                    flush_run(&mut out, &mut run, &mut run_style, span.style);
                    run.push(c);
                    col += UnicodeWidthChar::width(c).unwrap_or(0);
                }
            }
        }
    }
    if !run.is_empty() {
        out.push(Span::styled(run, run_style));
    }
    if layout.eol {
        out.push(Span::styled(EOL_GLYPH.to_string(), dim));
    }
    out
}

/// Marker for a literal space at display column `col`: leading whitespace uses
/// `LEAD_GLYPH`, a trailing run uses `TRAIL_GLYPH`, everything else
/// `SPACE_GLYPH`.  Lead wins when a whitespace-only line is both.
fn space_glyph(col: usize, layout: &WhitespaceLayout) -> char {
    if col < layout.lead_end_col {
        LEAD_GLYPH
    } else if layout.trail_start_col.is_some_and(|start| col >= start) {
        TRAIL_GLYPH
    } else {
        SPACE_GLYPH
    }
}

/// Flush the pending run into `out` when the next cell needs a different
/// style, then adopt that style for the cells that follow.
fn flush_run(out: &mut Vec<Span<'static>>, run: &mut String, run_style: &mut Style, next: Style) {
    if !run.is_empty() && *run_style != next {
        out.push(Span::styled(std::mem::take(run), *run_style));
    }
    *run_style = next;
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
/// the given span list.  The glyph covering that column (which may be a wide
/// character) keeps its foreground but gets the color-column background.  When
/// the line is shorter than `screen_col`, a trailing colored space is appended.
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
        let mut text = String::new();
        for ch in span.content.chars() {
            let width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if !injected && col <= screen_col && screen_col < col + width {
                // The color column falls inside this glyph region: paint the
                // glyph's background (a wide glyph is painted whole).
                if !text.is_empty() {
                    out.push(Span::styled(std::mem::take(&mut text), style));
                }
                out.push(Span::styled(ch.to_string(), style.bg(col_bg)));
                injected = true;
            } else {
                text.push(ch);
            }
            col += width;
        }
        if !text.is_empty() {
            out.push(Span::styled(text, style));
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

/// Expand tabs to spaces at `tab_width` stops, starting from absolute display
/// column `start_col` so tab stops stay correct when the line is scrolled
/// horizontally (`spans` start at the display column of their first char).
pub(super) fn expand_tabs_in_spans(
    spans: Vec<Span<'static>>,
    tab_width: usize,
    start_col: usize,
) -> Vec<Span<'static>> {
    let tab_width = tab_width.max(1);
    let mut out = Vec::with_capacity(spans.len());
    let mut col = start_col;
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

// ── Clipped-edge markers ─────────────────────────────────────────────────────

/// Draw the vim `extends`/`precedes` markers in the viewport edge cells when a
/// line continues off-screen, clipping the line first so the markers always
/// fit inside `viewport_width`.
pub(super) fn apply_edge_markers(
    spans: Vec<Span<'static>>,
    viewport_width: usize,
    precedes: bool,
    extends: bool,
) -> Vec<Span<'static>> {
    let budget =
        viewport_width.saturating_sub(usize::from(precedes)).saturating_sub(usize::from(extends));
    let mut out = clip_spans_to_width(spans, budget);
    if extends {
        out.push(Span::styled(EXTENDS_GLYPH.to_string(), whitespace_marker_style()));
    }
    if precedes {
        out.insert(0, Span::styled(PRECEDES_GLYPH.to_string(), whitespace_marker_style()));
    }
    out
}

/// Truncate a span list to at most `max` display cells.  A glyph that would
/// straddle the limit is dropped whole, so wide characters are never split.
pub(super) fn clip_spans_to_width(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    let mut out = Vec::with_capacity(spans.len());
    let mut used = 0usize;
    for span in spans {
        if used >= max {
            break;
        }
        let style = span.style;
        let mut text = String::new();
        for ch in span.content.chars() {
            let width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + width > max {
                break;
            }
            used += width;
            text.push(ch);
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
    tab_width: usize,
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
        let content = sp.content.as_ref();
        // Wide glyphs occupy two columns, so columns and byte offsets only
        // coincide for ASCII — walk display columns, not char counts.
        let span_cols = UnicodeWidthStr::width(content);
        let sp_end = col + span_cols;

        // Fast path: entire span is outside the selection.
        if sp_end <= sel_start || sel_end.is_some_and(|e| col > e) {
            col = sp_end;
            out.push(Span::styled(content.to_owned(), style));
            continue;
        }

        // The span overlaps the selection — split into up to three parts.
        let local_start = sel_start.saturating_sub(col).min(span_cols);
        let local_end =
            sel_end.map(|e| (e + 1).saturating_sub(col).min(span_cols)).unwrap_or(span_cols);
        let start_byte = display_col_to_byte(content, local_start, tab_width);
        let end_byte = display_col_to_byte(content, local_end, tab_width);

        if start_byte > 0 {
            out.push(Span::styled(content[..start_byte].to_owned(), style));
        }
        if end_byte > start_byte {
            out.push(Span::styled(
                content[start_byte..end_byte].to_owned(),
                style.bg(vis_bg).fg(vis_fg),
            ));
        }
        if end_byte < content.len() {
            out.push(Span::styled(content[end_byte..].to_owned(), style));
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
/// Matches are located in `line` bytes and painted through the display-column
/// annotation overlay, so the highlight stays aligned when the rendered spans
/// are no longer byte-identical to `line` (tabs expanded, wide glyphs, visible
/// whitespace markers).  Only matches overlapping `byte_start..byte_end` are
/// painted; the caller passes the slice it rendered.
pub(super) fn apply_search_highlights(
    spans: Vec<Span<'static>>,
    line: &str,
    pattern: &str,
    byte_start: usize,
    byte_end: usize,
    left: usize,
    tab_width: usize,
) -> Vec<Span<'static>> {
    // Build case-aware regex from the plain-text pattern.
    let case_insensitive = !smart_case_sensitive(pattern);
    let re_src = if case_insensitive {
        format!("(?i){}", regex::escape(pattern))
    } else {
        regex::escape(pattern)
    };
    let Ok(re) = regex::Regex::new(&re_src) else {
        return spans;
    };

    let visual = annotation_visual("find");
    let mut out = spans;
    for found in re.find_iter(line) {
        if found.end() <= byte_start || found.start() >= byte_end {
            continue;
        }
        let start = byte_col_to_display_col(line, found.start(), tab_width);
        let end = byte_col_to_display_col(line, found.end(), tab_width);
        if end <= start {
            continue;
        }
        out = apply_annotation_overlay(out, start, end, left, visual, tab_width);
    }
    out
}
