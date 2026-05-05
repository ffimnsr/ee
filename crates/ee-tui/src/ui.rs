use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use xi_core_lib::plugin_rpc::DiagnosticSeverity;

use crate::app::{App, Mode, Viewport, smart_case_sensitive};
use crate::backend::{CoreAnnotation, LineSlot};
use crate::buffer::BufState;
use crate::config::{NumberStyle, StatuslineFormat};
use crate::picker::PickerKind;
use crate::quickfix::QfList;
use crate::text::{byte_col_to_display_col, display_col_to_byte};

#[derive(Clone, Copy)]
struct RootAreas {
    tab_bar_area: Option<Rect>,
    editor_area: Rect,
    qf_area: Option<Rect>,
    status_area: Rect,
    prompt_area: Rect,
}

fn split_root_areas(area: Rect, app: &App) -> RootAreas {
    let tab_count = app.tabs.tab_count();

    let qf_panel_visible = (app.quickfix_open && app.quickfix.is_some())
        || (app.location_list_open && app.location_list.is_some());
    const QF_HEIGHT: u16 = 8;

    let rows = if tab_count > 1 {
        if qf_panel_visible {
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Min(1),
                    Constraint::Length(QF_HEIGHT),
                    Constraint::Length(1),
                    Constraint::Length(1),
                ])
                .split(area)
        } else {
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Min(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                ])
                .split(area)
        }
    } else if qf_panel_visible {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(QF_HEIGHT),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)])
            .split(area)
    };

    let (tab_bar_area, editor_area, qf_area, status_area, prompt_area) =
        if tab_count > 1 && qf_panel_visible {
            (Some(rows[0]), rows[1], Some(rows[2]), rows[3], rows[4])
        } else if tab_count > 1 {
            (Some(rows[0]), rows[1], None, rows[2], rows[3])
        } else if qf_panel_visible {
            (None, rows[0], Some(rows[1]), rows[2], rows[3])
        } else {
            (None, rows[0], None, rows[1], rows[2])
        };

    RootAreas { tab_bar_area, editor_area, qf_area, status_area, prompt_area }
}

/// Return the visible editor row count for the current app state and terminal
/// size. Use this wherever xi-core must be told how many lines fit on screen.
pub(crate) fn compute_editor_height(terminal_size: ratatui::layout::Rect, app: &App) -> usize {
    split_root_areas(terminal_size, app).editor_area.height as usize
}

fn window_chunks(app: &App, win_rect: Rect, line_count: usize) -> [Rect; 2] {
    let gw = gutter_width(app, line_count);
    let editor = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(gw), Constraint::Min(1)])
        .split(win_rect);
    [editor[0], editor[1]]
}

fn rect_contains(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x && column < rect.right() && row >= rect.y && row < rect.bottom()
}

pub(crate) fn hit_test_buffer_cell(
    area: Rect,
    app: &App,
    column: u16,
    row: u16,
) -> Option<(usize, usize)> {
    let root = split_root_areas(area, app);
    let (win_id, buf_id, win_rect, _) = app
        .tabs
        .focused_windows()
        .layout_for_area(root.editor_area)
        .into_iter()
        .find(|(_, _, _, is_focused)| *is_focused)?;

    if !rect_contains(win_rect, column, row) {
        return None;
    }

    let buf = app.backend.all_bufs().iter().find(|b| b.id == buf_id)?;
    let vp = app.tabs.focused_windows().viewport_for_window(win_id, app.viewport);
    let [_, buffer_area] = window_chunks(app, win_rect, buf.lines.len());
    let line = vp.top_line + usize::from(row.saturating_sub(buffer_area.y));
    let display_col =
        if column < buffer_area.x { 0 } else { vp.left_col + usize::from(column - buffer_area.x) };
    Some((line, display_col))
}

pub(crate) fn ui(frame: &mut ratatui::Frame<'_>, app: &App) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(Style::default().bg(Color::Rgb(22, 24, 31))), area);
    let root = split_root_areas(area, app);

    // Tab bar (only when more than one tab is open).
    if let Some(tab_area) = root.tab_bar_area {
        render_tab_bar(frame, tab_area, app);
    }

    // Render each window in the focused tab.
    for (win_id, buf_id, win_rect, is_focused) in
        app.tabs.focused_windows().layout_for_area(root.editor_area)
    {
        let Some(buf) = app.backend.all_bufs().iter().find(|b| b.id == buf_id) else {
            continue;
        };
        let vp = app.tabs.focused_windows().viewport_for_window(win_id, app.viewport);
        let editor = window_chunks(app, win_rect, buf.lines.len());

        render_gutter(frame, editor[0], buf, vp, app);
        render_buffer(frame, editor[1], buf, vp, app);

        if is_focused {
            let cursor = cursor_position_for(buf, vp, app, editor[1], root.prompt_area);
            frame.set_cursor_position(cursor);
        }
    }

    render_status(frame, root.status_area, app);
    render_prompt(frame, root.prompt_area, app);

    // Quickfix / location-list panel (drawn before picker overlay).
    if let Some(qf_rect) = root.qf_area {
        if app.quickfix_open {
            if let Some(qf) = &app.quickfix {
                render_qf_panel(frame, qf_rect, qf, app.quickfix_focused, false);
            }
        } else if app.location_list_open {
            if let Some(ll) = &app.location_list {
                render_qf_panel(frame, qf_rect, ll, app.location_list_focused, true);
            }
        }
    }

    // Picker overlay (drawn last so it floats above everything).
    if app.hover_popup.is_some() {
        render_hover_popup(frame, area, app);
    }

    if app.picker.is_some() {
        render_picker(frame, area, app);
    }
}

// ── Layout helpers ────────────────────────────────────────────────────────────

/// Compute the gutter column width based on display settings and line count.
fn gutter_width(app: &App, line_count: usize) -> u16 {
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
    let line_count = app.backend.lines.len().max(1);
    let gw = gutter_width(app, line_count);
    area.width.saturating_sub(gw) as usize
}

// ── Visible-whitespace substitution ──────────────────────────────────────────

/// Substitute space `' '` → `'·'` and tab `'\t'` → `'→'` in rendered spans,
/// applying a dimmed style to the replaced characters.
fn apply_visible_whitespace(spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let dim = Style::default().fg(Color::Rgb(70, 80, 100));
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

// ── Color column injection ────────────────────────────────────────────────────

/// Inject a distinct background color at display column `screen_col` within
/// the given span list.  Characters at that column keep their foreground but
/// get the color-column background.  When the line is shorter than `screen_col`,
/// a trailing colored space is appended.
fn apply_color_column(spans: Vec<Span<'static>>, screen_col: usize) -> Vec<Span<'static>> {
    let col_bg = Color::Rgb(55, 35, 35);
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

/// Overlay visual-mode selection highlight on a rendered span list.
///
/// `col_start`/`col_end` are display-column bounds (inclusive); pass `None`
/// for `col_end` to highlight the entire line (VisualLine / whole-line block
/// rows).  `left` is the horizontal scroll offset already applied.  The
/// visual background replaces each span's background in the selected range
/// while preserving foreground colours.
fn apply_visual_highlight(
    spans: Vec<Span<'static>>,
    col_start: Option<usize>,
    col_end: Option<usize>,
    left: usize,
) -> Vec<Span<'static>> {
    let vis_bg = Color::Rgb(68, 71, 90); // muted purple-grey selection
    let vis_fg = Color::Rgb(205, 214, 244);

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
/// `left` is the horizontal scroll offset (bytes already skipped), `bg` is
/// the background colour inherited from the cursor-line flag.
fn apply_search_highlights(
    spans: Vec<Span<'static>>,
    line: &str,
    pattern: &str,
    left: usize,
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

    // Collect all match byte ranges over the full line.
    let matches: Vec<(usize, usize)> = re.find_iter(line).map(|m| (m.start(), m.end())).collect();
    if matches.is_empty() {
        return spans;
    }

    let match_hl = Style::default()
        .fg(Color::Rgb(22, 24, 31))
        .bg(Color::Rgb(250, 179, 135)) // warm orange highlight
        .add_modifier(Modifier::BOLD);

    // Re-build spans, splitting on match boundaries (byte offsets relative to
    // the displayed slice starting at `left`).
    let mut out: Vec<Span<'static>> = Vec::new();
    // Accumulate raw bytes across all input spans so we can apply match ranges.
    // Build a flat (byte_offset, char_group, style) representation first.
    let mut flat: Vec<(String, Style)> = Vec::new();
    let _cursor = left; // current byte position in `line` (unused but documents intent)
    for span in &spans {
        let content = span.content.as_ref();
        let style = span.style;
        flat.push((content.to_owned(), style));
    }

    // Re-emit spans split by match ranges.
    let mut byte_pos = left; // position in `line` of the start of the current flat span
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AnnotationVisual {
    bg: Color,
    fg: Option<Color>,
    modifier: Modifier,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LineAnnotationSegment {
    start_display: usize,
    end_display: usize,
    visual: AnnotationVisual,
    priority: u8,
}

fn annotation_visual(kind: &str) -> AnnotationVisual {
    match kind {
        "selection" => AnnotationVisual {
            bg: Color::Rgb(68, 71, 90),
            fg: Some(Color::Rgb(205, 214, 244)),
            modifier: Modifier::empty(),
        },
        "find" => AnnotationVisual {
            bg: Color::Rgb(250, 179, 135),
            fg: Some(Color::Rgb(22, 24, 31)),
            modifier: Modifier::BOLD,
        },
        _ => AnnotationVisual {
            bg: Color::Rgb(43, 82, 74),
            fg: None,
            modifier: Modifier::UNDERLINED,
        },
    }
}

fn annotation_priority(kind: &str) -> u8 {
    match kind {
        "selection" => 0,
        "find" => 2,
        _ => 1,
    }
}

fn annotation_marker_color(kind: &str) -> Color {
    match kind {
        "find" => Color::Rgb(250, 179, 135),
        "selection" => Color::Rgb(137, 180, 250),
        _ => Color::Rgb(166, 227, 161),
    }
}

fn payload_marker_for_value(value: &serde_json::Value) -> Option<char> {
    match value {
        serde_json::Value::String(text) => text
            .chars()
            .find(|ch| !ch.is_whitespace())
            .map(|ch| if ch.is_alphanumeric() { ch.to_ascii_uppercase() } else { '•' }),
        serde_json::Value::Object(map) => ["label", "kind", "name", "message"]
            .iter()
            .find_map(|key| map.get(*key).and_then(payload_marker_for_value))
            .or(Some('•')),
        serde_json::Value::Array(values) => values.iter().find_map(payload_marker_for_value),
        serde_json::Value::Null => None,
        _ => Some('•'),
    }
}

fn annotation_marker_for_line(buf: &BufState, line_index: usize) -> Option<(char, Color)> {
    let mut best: Option<(u8, char, Color)> = None;

    for annotation in &buf.annotations {
        if matches!(annotation.annotation_type.as_str(), "selection" | "find") {
            continue;
        }
        let Some(payloads) = annotation.payloads.as_ref() else { continue };
        for (idx, range) in annotation.ranges.iter().enumerate() {
            let Some(payload) = payloads.get(idx) else { continue };
            let [start_line, _, end_line, _] = *range;
            if line_index < start_line || line_index > end_line {
                continue;
            }
            let Some(marker) = payload_marker_for_value(payload) else { continue };
            let priority = annotation_priority(&annotation.annotation_type);
            let color = annotation_marker_color(&annotation.annotation_type);
            if best.as_ref().is_none_or(|(best_priority, _, _)| priority >= *best_priority) {
                best = Some((priority, marker, color));
            }
        }
    }

    best.map(|(_, marker, color)| (marker, color))
}

fn annotation_style(base: Style, visual: AnnotationVisual) -> Style {
    let mut style = base.bg(visual.bg);
    if let Some(fg) = visual.fg {
        style = style.fg(fg);
    }
    if visual.modifier != Modifier::empty() {
        style = style.add_modifier(visual.modifier);
    }
    style
}

fn apply_annotation_overlay(
    spans: Vec<Span<'static>>,
    col_start: usize,
    col_end: usize,
    left: usize,
    visual: AnnotationVisual,
) -> Vec<Span<'static>> {
    let sel_start = col_start.saturating_sub(left);
    let mut sel_end = col_end.saturating_sub(left);
    if sel_end <= sel_start {
        sel_end = sel_start + 1;
    }

    let mut out: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;
    let mut painted = false;

    for sp in spans {
        let content = sp.content.into_owned();
        let style = sp.style;
        let span_cols = byte_col_to_display_col(&content, content.len());
        let sp_end = col + span_cols;

        if sp_end <= sel_start || col >= sel_end {
            out.push(Span::styled(content, style));
            col = sp_end;
            continue;
        }

        let local_start = sel_start.saturating_sub(col).min(span_cols);
        let local_end = sel_end.saturating_sub(col).min(span_cols);
        let start_byte = display_col_to_byte(&content, local_start);
        let end_byte = display_col_to_byte(&content, local_end);

        if start_byte > 0 {
            out.push(Span::styled(content[..start_byte].to_owned(), style));
        }

        if end_byte > start_byte {
            out.push(Span::styled(
                content[start_byte..end_byte].to_owned(),
                annotation_style(style, visual),
            ));
            painted = true;
        }

        if end_byte < content.len() {
            out.push(Span::styled(content[end_byte..].to_owned(), style));
        }

        col = sp_end;
    }

    if !painted && col <= sel_start {
        let pad = sel_start - col;
        if pad > 0 {
            out.push(Span::raw(" ".repeat(pad)));
        }
        out.push(Span::styled(" ", annotation_style(Style::default(), visual)));
    } else if painted && sel_end > col {
        out.push(Span::styled(" ", annotation_style(Style::default(), visual)));
    }

    out
}

fn collect_line_annotation_segments(
    line: &str,
    log_idx: usize,
    annotations: &[CoreAnnotation],
) -> Vec<LineAnnotationSegment> {
    let mut segments = Vec::new();

    for annotation in annotations {
        let visual = annotation_visual(&annotation.annotation_type);
        let priority = annotation_priority(&annotation.annotation_type);
        for range in &annotation.ranges {
            let [start_line, start_col, end_line, end_col] = *range;
            if log_idx < start_line || log_idx > end_line {
                continue;
            }

            let start_byte = if log_idx == start_line { start_col.min(line.len()) } else { 0 };
            let end_byte = if log_idx == end_line { end_col.min(line.len()) } else { line.len() };
            let start_display = byte_col_to_display_col(line, start_byte);
            let mut end_display = byte_col_to_display_col(line, end_byte);
            if end_display <= start_display {
                end_display = start_display + 1;
            }

            segments.push(LineAnnotationSegment { start_display, end_display, visual, priority });
        }
    }

    segments.sort_by_key(|segment| (segment.priority, segment.start_display, segment.end_display));

    let mut merged: Vec<LineAnnotationSegment> = Vec::new();
    for segment in segments {
        if let Some(last) = merged.last_mut() {
            if last.priority == segment.priority
                && last.visual == segment.visual
                && segment.start_display <= last.end_display
            {
                last.end_display = last.end_display.max(segment.end_display);
                continue;
            }
        }
        merged.push(segment);
    }

    merged
}

fn apply_core_annotations(
    mut spans: Vec<Span<'static>>,
    line: &str,
    log_idx: usize,
    annotations: &[CoreAnnotation],
    left: usize,
) -> Vec<Span<'static>> {
    for segment in collect_line_annotation_segments(line, log_idx, annotations) {
        spans = apply_annotation_overlay(
            spans,
            segment.start_display,
            segment.end_display,
            left,
            segment.visual,
        );
    }
    spans
}

fn render_tab_bar(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let focused_idx = app.tabs.focused_idx();
    let spans: Vec<Span> = app
        .tabs
        .iter()
        .flat_map(|(i, tab)| {
            // Derive a display name from the focused window's buffer title.
            let buf_id = tab.windows.focused_window().buffer_id;
            let label = app
                .backend
                .all_bufs()
                .iter()
                .find(|b| b.id == buf_id)
                .map(|b| b.title())
                .unwrap_or_else(|| format!("[{}]", i + 1));
            let style = if i == focused_idx {
                Style::default()
                    .fg(Color::Rgb(22, 24, 31))
                    .bg(Color::Rgb(137, 220, 235))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Rgb(186, 194, 222)).bg(Color::Rgb(30, 32, 39))
            };
            [Span::styled(format!(" {} ", label), style), Span::raw(" ")]
        })
        .collect();

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Rgb(22, 24, 31))),
        area,
    );
}

fn render_gutter(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    buf: &BufState,
    vp: Viewport,
    app: &App,
) {
    let height = area.height as usize;
    let line_count = buf.lines.len().max(1);
    let cursor_line = buf.cursor_line;
    let top = vp.top_line;
    let sign_col = app.config.sign_column;
    let num_digits = line_count.to_string().len().max(3);
    let cursor_line_bg = Color::Rgb(35, 38, 50);
    let vis_sel_bg = Color::Rgb(68, 71, 90);

    // Pre-compute visual selection line range for gutter highlight.
    let visual_line_range: Option<(usize, usize)> = if app.mode.is_visual() {
        let (al, _) = app.visual_anchor.unwrap_or((cursor_line, buf.cursor_col));
        let cl = cursor_line;
        let lo = al.min(cl);
        let hi = al.max(cl);
        Some((lo, hi))
    } else {
        None
    };

    let mut lines: Vec<Line> = Vec::with_capacity(height);
    let mut li = top;
    let tilde_style = Style::default().fg(Color::Rgb(65, 72, 95)).bg(Color::Rgb(30, 32, 39));
    for _ in 0..height {
        if li >= line_count {
            lines.push(Line::from(Span::styled("~", tilde_style)));
            continue;
        }
        let is_cursor = li == cursor_line;
        let in_visual = visual_line_range.is_some_and(|(lo, hi)| li >= lo && li <= hi);
        let bg = if in_visual {
            vis_sel_bg
        } else if is_cursor && app.config.cursor_line {
            cursor_line_bg
        } else {
            Color::Rgb(30, 32, 39)
        };

        // Sign column: show fold markers when applicable.
        let sign_spans = if sign_col {
            let (marker, fg) = if let Some(severity) = diagnostic_marker_for_line(buf, li) {
                match severity {
                    DiagnosticSeverity::Error => ('E', Color::Rgb(243, 139, 168)),
                    DiagnosticSeverity::Warning => ('W', Color::Rgb(250, 179, 135)),
                    DiagnosticSeverity::Information => ('I', Color::Rgb(137, 220, 235)),
                    DiagnosticSeverity::Hint => ('H', Color::Rgb(166, 227, 161)),
                }
            } else if app.folds.fold_at(buf.id, li).is_some() {
                ('▸', Color::Rgb(100, 130, 160))
            } else {
                (' ', Color::Rgb(100, 130, 160))
            };
            let (annotation_marker, annotation_fg) = app
                .git_status(buf.id)
                .and_then(|status| status.sign_for_line(li))
                .map(|sign| (sign.marker(), git_sign_color(sign)))
                .or_else(|| annotation_marker_for_line(buf, li))
                .unwrap_or((' ', Color::Rgb(90, 100, 125)));
            vec![
                Span::styled(marker.to_string(), Style::default().fg(fg).bg(bg)),
                Span::styled(
                    annotation_marker.to_string(),
                    Style::default().fg(annotation_fg).bg(bg),
                ),
            ]
        } else {
            Vec::new()
        };

        let num_text = match app.config.number_style {
            NumberStyle::Absolute => format!("{:>width$} ", li + 1, width = num_digits),
            NumberStyle::Relative => {
                let dist = li.abs_diff(cursor_line);
                if dist == 0 {
                    format!("{:>width$} ", li + 1, width = num_digits)
                } else {
                    format!("{:>width$} ", dist, width = num_digits)
                }
            }
            NumberStyle::RelativeAbsolute => {
                let dist = li.abs_diff(cursor_line);
                if is_cursor {
                    format!("{:>width$} ", li + 1, width = num_digits)
                } else {
                    format!("{:>width$} ", dist, width = num_digits)
                }
            }
        };
        let num_fg = if is_cursor { Color::Rgb(205, 214, 244) } else { Color::DarkGray };
        let num_span = Span::styled(num_text, Style::default().fg(num_fg).bg(bg));

        // Fold-aware: advance past fold body.
        if let Some((_, end)) = app.folds.fold_at(buf.id, li) {
            li = end + 1;
        } else {
            li += 1;
        }

        if sign_col {
            let mut row = sign_spans;
            row.push(num_span);
            lines.push(Line::from(row));
        } else {
            lines.push(Line::from(num_span));
        }
    }

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(Color::Rgb(30, 32, 39))),
        area,
    );
}

fn render_buffer(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    buf: &BufState,
    vp: Viewport,
    app: &App,
) {
    let height = area.height as usize;
    let top = vp.top_line;
    let left = vp.left_col;
    let cursor_line = buf.cursor_line;
    let cursor_line_bg = Color::Rgb(35, 38, 50);
    let buf_bg = Color::Rgb(22, 24, 31);

    // Pre-compute visual selection bounds for this buffer.
    // Returns (anchor_line, anchor_col, cursor_line, cursor_col) in natural order.
    let visual_sel = if app.mode.is_visual() {
        let (al, ac) = app.visual_anchor.unwrap_or((cursor_line, buf.cursor_col));
        let (cl, cc) = (cursor_line, buf.cursor_col);
        Some((al, ac, cl, cc))
    } else {
        None
    };

    if buf.lines.is_empty() && top == 0 {
        let text = vec![Line::from(Span::styled(
            "empty buffer",
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        ))];
        frame.render_widget(
            Paragraph::new(text)
                .block(Block::default().borders(Borders::NONE))
                .style(Style::default().fg(Color::Rgb(213, 216, 224)).bg(buf_bg)),
            area,
        );
        return;
    }

    let extension = buf.path.as_ref().and_then(|p| p.extension()).and_then(|e| e.to_str());

    // Collect visible logical lines (fold-aware).
    let mut visible: Vec<usize> = Vec::with_capacity(height);
    let mut li = top;
    while visible.len() < height && li < buf.lines.len() {
        visible.push(li);
        if let Some((_, end)) = app.folds.fold_at(buf.id, li) {
            li = end + 1;
        } else {
            li += 1;
        }
    }

    let hl_span = visible.last().copied().unwrap_or(top).saturating_sub(top) + 1;
    let hl_lines = app.highlighter.highlight_visible(&buf.lines, extension, top, hl_span);

    let text: Vec<Line> = visible
        .iter()
        .map(|&log_idx| {
            let is_cursor = log_idx == cursor_line;
            let is_fold_header = app.folds.fold_at(buf.id, log_idx).is_some();
            let line = buf.lines.get(log_idx).map(|s| s.as_str()).unwrap_or("");
            let bg = if is_cursor && app.config.cursor_line { cursor_line_bg } else { buf_bg };

            let mut spans: Vec<Span<'static>> = if is_fold_header {
                // Show fold marker line (abbreviated first line + fold indicator).
                let preview: String = line.chars().take(40).collect();
                vec![Span::styled(
                    format!("{preview}  ··· (folded)",),
                    Style::default()
                        .fg(Color::Rgb(100, 130, 160))
                        .bg(bg)
                        .add_modifier(Modifier::ITALIC),
                )]
            } else {
                let byte_start = display_col_to_byte(line, left);
                let backend_syntax = match buf.line_cache.get(log_idx) {
                    Some(LineSlot::Known(cached_line)) if !cached_line.syntax_spans.is_empty() => {
                        Some(cached_line.syntax_spans.as_slice())
                    }
                    _ => None,
                };

                if let Some(syntax_spans) = backend_syntax {
                    crate::highlight::Highlighter::scope_spans_with_offset(
                        line,
                        syntax_spans,
                        byte_start,
                    )
                    .into_iter()
                    .map(|s| {
                        let style = if is_cursor && app.config.cursor_line {
                            s.style.bg(bg)
                        } else {
                            s.style
                        };
                        Span::styled(s.content.into_owned(), style)
                    })
                    .collect()
                } else {
                    let raw = hl_lines.get(log_idx.saturating_sub(top));
                    if let Some(spans_ref) = raw {
                        if spans_ref.is_empty() {
                            vec![Span::styled(
                                line[byte_start..].to_owned(),
                                Style::default().bg(bg),
                            )]
                        } else {
                            crate::highlight::Highlighter::spans_with_offset(spans_ref, byte_start)
                                .into_iter()
                                .map(|s| {
                                    let style = if is_cursor && app.config.cursor_line {
                                        s.style.bg(bg)
                                    } else {
                                        s.style
                                    };
                                    Span::styled(s.content.into_owned(), style)
                                })
                                .collect()
                        }
                    } else {
                        vec![Span::styled(line[byte_start..].to_owned(), Style::default().bg(bg))]
                    }
                }
            };

            // Apply visible-whitespace substitution when enabled.
            if app.config.show_visible_whitespace && !is_fold_header {
                spans = apply_visible_whitespace(spans);
            }

            if !is_fold_header {
                spans = apply_core_annotations(spans, line, log_idx, &buf.annotations, left);
            }

            // Apply search match highlighting over the rendered spans.
            if let Some(ref pat) = app.search_pattern {
                if !is_fold_header {
                    let full_line = buf.lines.get(log_idx).map(|s| s.as_str()).unwrap_or("");
                    spans = apply_search_highlights(spans, full_line, pat, left, bg);
                }
            }

            // Apply color column highlight.
            if let Some(cc) = app.config.color_column {
                if cc >= left {
                    let screen_col = cc - left;
                    spans = apply_color_column(spans, screen_col);
                }
            }

            // Apply visual-mode selection highlight (drawn last so it wins).
            if let Some((al, ac, cl, cc)) = visual_sel {
                let (sel_top_line, sel_top_col) =
                    if al < cl || (al == cl && ac <= cc) { (al, ac) } else { (cl, cc) };
                let (sel_bot_line, sel_bot_col) =
                    if al < cl || (al == cl && ac <= cc) { (cl, cc) } else { (al, ac) };

                let in_sel = log_idx >= sel_top_line && log_idx <= sel_bot_line;
                if in_sel {
                    let (col_start, col_end) = match app.mode {
                        crate::app::Mode::VisualLine => (None, None),
                        crate::app::Mode::VisualBlock => {
                            let c1 = sel_top_col.min(sel_bot_col);
                            let c2 = sel_top_col.max(sel_bot_col);
                            (Some(c1), Some(c2))
                        }
                        _ => {
                            // Characterwise: first line starts at anchor col,
                            // last line ends at cursor col, middle lines full.
                            let cs =
                                if log_idx == sel_top_line { Some(sel_top_col) } else { Some(0) };
                            let ce = if log_idx == sel_bot_line { Some(sel_bot_col) } else { None };
                            (cs, ce)
                        }
                    };
                    spans = apply_visual_highlight(spans, col_start, col_end, left);
                }
            }

            let mut l = Line::from(spans);
            if is_cursor && app.config.cursor_line {
                l = l.style(Style::default().bg(bg));
            }
            l
        })
        .collect();

    let mut widget = Paragraph::new(text)
        .block(Block::default().borders(Borders::NONE))
        .style(Style::default().fg(Color::Rgb(213, 216, 224)).bg(buf_bg));

    if app.config.wrap_lines {
        widget = widget.wrap(Wrap { trim: false });
    }

    frame.render_widget(widget, area);
}

fn render_status(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let mode_label = if app.quickfix_focused {
        "QFX"
    } else if app.location_list_focused {
        "LOC"
    } else {
        app.mode.label()
    };
    let mode = Span::styled(
        format!(" {} ", mode_label),
        Style::default().fg(Color::Rgb(22, 24, 31)).bg(Color::Rgb(137, 220, 235)),
    );
    let file = Span::styled(
        format!(" {}", app.backend.title()),
        Style::default().fg(Color::Rgb(238, 238, 238)).bg(Color::Rgb(49, 54, 68)),
    );
    let modified = if app.backend.pristine {
        Span::raw("")
    } else {
        Span::styled(
            " [+]",
            Style::default().fg(Color::Rgb(250, 179, 135)).bg(Color::Rgb(49, 54, 68)),
        )
    };
    let position = Span::styled(
        format!("  Ln {}, Col {} ", app.backend.cursor_line + 1, app.backend.cursor_col + 1),
        Style::default().fg(Color::Rgb(186, 194, 222)).bg(Color::Rgb(49, 54, 68)),
    );

    let spans = match app.config.statusline_format {
        StatuslineFormat::Minimal => {
            let mut spans = vec![mode, file, modified];
            if let Some(git_span) = git_status_span(app) {
                spans.push(git_span);
            }
            spans.push(position);
            spans
        }
        StatuslineFormat::Default => {
            let buf_count = app.backend.buf_count();
            let buf_indicator = if buf_count > 1 {
                Span::styled(
                    format!("  [{}/{}]", app.backend.current_idx() + 1, buf_count),
                    Style::default().fg(Color::Rgb(166, 173, 200)).bg(Color::Rgb(49, 54, 68)),
                )
            } else {
                Span::raw("")
            };
            // Show wrap/list/number indicators on the right.
            let flags = {
                let mut f = String::new();
                if app.config.wrap_lines {
                    f.push_str(" wrap");
                }
                if app.config.show_visible_whitespace {
                    f.push_str(" list");
                }
                f
            };
            let flag_span = if flags.is_empty() {
                Span::raw("")
            } else {
                Span::styled(
                    format!(" │{}", flags),
                    Style::default().fg(Color::Rgb(100, 120, 150)).bg(Color::Rgb(49, 54, 68)),
                )
            };
            let mut spans = vec![mode, file, modified];
            if let Some(git_span) = git_status_span(app) {
                spans.push(git_span);
            }
            spans.push(buf_indicator);
            spans.push(flag_span);
            spans.push(position);
            spans
        }
    };

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Rgb(49, 54, 68))),
        area,
    );
}

fn git_sign_color(sign: crate::git::GitSign) -> Color {
    match sign {
        crate::git::GitSign::Added => Color::Rgb(166, 227, 161),
        crate::git::GitSign::Modified => Color::Rgb(250, 179, 135),
        crate::git::GitSign::Deleted => Color::Rgb(243, 139, 168),
    }
}

fn git_status_span(app: &App) -> Option<Span<'static>> {
    let status = app.current_git_status()?;
    let dirty = if status.dirty { '*' } else { ' ' };
    Some(Span::styled(
        format!("  git:{}{}", status.branch, dirty),
        Style::default().fg(Color::Rgb(166, 227, 161)).bg(Color::Rgb(49, 54, 68)),
    ))
}

fn render_prompt(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let prompt = match app.mode {
        Mode::Normal => Line::from(match app.backend.status_message.as_deref() {
            Some(message) => message.to_owned(),
            None => "normal | i insert | v visual | : command | :help discovery".to_owned(),
        }),
        Mode::Insert => Line::from("insert | esc normal"),
        Mode::Visual => {
            Line::from("visual | hjkl/move selects | d/y/c operators | v/esc normal | : command")
        }
        Mode::VisualLine => {
            Line::from("visual-line | j/k extends | d/y/c operators | V/esc normal")
        }
        Mode::VisualBlock => {
            Line::from("visual-block | hjkl extends block | d/y I/A operators | Ctrl-V/esc normal")
        }
        Mode::CommandLine => {
            Line::from(vec![Span::raw(":"), Span::raw(app.command_buffer.as_str())])
        }
        Mode::Search => {
            let prefix = if app.search_backward { "?" } else { "/" };
            Line::from(vec![Span::raw(prefix), Span::raw(app.command_buffer.as_str())])
        }
        Mode::SubstituteConfirm => Line::from(match app.backend.status_message.as_deref() {
            Some(msg) => msg.to_owned(),
            None => "substitute — replace? [y]es [n]o [a]ll [q]uit".to_owned(),
        }),
        Mode::OperatorPending => Line::from(
            match app.input_state.pending_operator {
                Some(crate::app::Operator::Delete) => "-- DELETE (motion / text-obj) --",
                Some(crate::app::Operator::Change) => "-- CHANGE (motion / text-obj) --",
                Some(crate::app::Operator::Yank) => "-- YANK (motion / text-obj) --",
                Some(crate::app::Operator::Indent) => "-- INDENT (motion) --",
                Some(crate::app::Operator::Outdent) => "-- OUTDENT (motion) --",
                Some(crate::app::Operator::Uppercase) => "-- UPPERCASE (motion) --",
                Some(crate::app::Operator::Lowercase) => "-- LOWERCASE (motion) --",
                Some(crate::app::Operator::CaseToggle) => "-- CASE TOGGLE (motion) --",
                None => "-- OPERATOR PENDING --",
            }
            .to_owned(),
        ),
    };

    frame.render_widget(
        Paragraph::new(prompt)
            .style(Style::default().fg(Color::Rgb(166, 173, 200)).bg(Color::Rgb(24, 25, 38))),
        area,
    );
}

fn cursor_position_for(
    buf: &BufState,
    vp: Viewport,
    app: &App,
    editor_area: Rect,
    prompt_area: Rect,
) -> Position {
    if matches!(app.mode, Mode::CommandLine | Mode::Search) {
        let max_x = prompt_area.right().saturating_sub(1);
        let x = (prompt_area.x + 1 + app.command_buffer.len() as u16).min(max_x);
        return Position::new(x, prompt_area.y);
    }

    let max_x = editor_area.right().saturating_sub(1);
    let max_y = editor_area.bottom().saturating_sub(1);

    let line = buf.lines.get(buf.cursor_line).map(|s| s.as_str()).unwrap_or("");
    let display_col = byte_col_to_display_col(line, buf.cursor_col);

    let screen_line = buf.cursor_line.saturating_sub(vp.top_line);
    let screen_col = display_col.saturating_sub(vp.left_col);

    let x = (editor_area.x + screen_col as u16).min(max_x);
    let y = (editor_area.y + screen_line as u16).min(max_y);
    Position::new(x, y)
}

/// Render the quickfix or location-list panel.
///
/// `is_location_list` controls the title prefix only; both lists share
/// the same layout.
fn render_qf_panel(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    list: &QfList,
    focused: bool,
    is_location_list: bool,
) {
    let border_style = if focused {
        Style::default().fg(Color::Rgb(137, 220, 235))
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let kind = if is_location_list { "Location List" } else { "Quickfix" };
    let title = format!(
        " {} [{}/{}] ",
        if list.title.is_empty() { kind.to_owned() } else { list.title.clone() },
        if list.is_empty() { 0 } else { list.selected + 1 },
        list.len()
    );
    let block = Block::default()
        .title(title.as_str())
        .borders(Borders::ALL)
        .border_style(border_style)
        .style(Style::default().bg(Color::Rgb(24, 25, 38)));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || list.is_empty() {
        return;
    }

    let height = inner.height as usize;
    let selected = list.selected;
    let scroll_off = if selected >= height { selected + 1 - height } else { 0 };

    let items: Vec<ListItem> = list
        .entries
        .iter()
        .enumerate()
        .skip(scroll_off)
        .take(height)
        .map(|(i, entry)| {
            let is_sel = i == selected;
            let style = if is_sel {
                Style::default()
                    .fg(Color::Rgb(22, 24, 31))
                    .bg(Color::Rgb(137, 220, 235))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Rgb(213, 216, 224))
            };
            ListItem::new(Line::from(Span::styled(
                format!(" {:>3}  {}", i + 1, entry.display_label()),
                style,
            )))
        })
        .collect();

    let mut list_state = ListState::default();
    list_state.select(Some(selected.saturating_sub(scroll_off)));
    frame.render_stateful_widget(List::new(items), inner, &mut list_state);
}

/// Render the floating picker overlay centered in `area`.
fn render_picker(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let Some(picker) = &app.picker else { return };

    // Popup dimensions: 80 % of the available area, min 20×10.
    let popup_w = ((area.width as f32 * 0.8) as u16).max(20).min(area.width);
    let popup_h = ((area.height as f32 * 0.8) as u16).max(10).min(area.height);
    let popup_x = area.x + (area.width.saturating_sub(popup_w)) / 2;
    let popup_y = area.y + (area.height.saturating_sub(popup_h)) / 2;
    let popup_rect = Rect::new(popup_x, popup_y, popup_w, popup_h);

    // Clear the region behind the popup.
    frame.render_widget(Clear, popup_rect);

    let block = Block::default()
        .title(format!(" {} ", picker.title))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Rgb(137, 220, 235)))
        .style(Style::default().bg(Color::Rgb(22, 24, 31)));

    // Inner layout: search bar (1 line) + list (rest).
    let inner = block.inner(popup_rect);
    frame.render_widget(block, popup_rect);

    if inner.height < 2 {
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);

    // Search bar.
    let search_prefix = match picker.kind {
        PickerKind::LiveGrep => "/ ",
        _ => "> ",
    };
    let search_line = Line::from(vec![
        Span::styled(search_prefix, Style::default().fg(Color::Rgb(137, 220, 235))),
        Span::raw(picker.query.as_str()),
    ]);
    frame.render_widget(Paragraph::new(search_line), chunks[0]);

    // Position cursor in the search bar.
    let cursor_x = (chunks[0].x + search_prefix.len() as u16 + picker.query.len() as u16)
        .min(chunks[0].right().saturating_sub(1));
    frame.set_cursor_position(Position::new(cursor_x, chunks[0].y));

    // Item list.
    let list_height = chunks[1].height as usize;
    let selected = picker.selected;
    // Scroll offset so the selected item stays visible.
    let scroll_off = if selected >= list_height { selected + 1 - list_height } else { 0 };

    let visible = picker.visible_items_range(scroll_off, list_height);
    let list_items: Vec<ListItem> = visible
        .into_iter()
        .enumerate()
        .map(|(i, label)| {
            let abs_idx = scroll_off + i;
            let is_sel = abs_idx == selected;
            let style = if is_sel {
                Style::default()
                    .fg(Color::Rgb(22, 24, 31))
                    .bg(Color::Rgb(137, 220, 235))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Rgb(213, 216, 224))
            };
            ListItem::new(Line::from(Span::styled(label, style)))
        })
        .collect();

    let mut list_state = ListState::default();
    if !picker.filtered.is_empty() {
        list_state.select(Some(selected.saturating_sub(scroll_off)));
    }
    frame.render_stateful_widget(List::new(list_items), chunks[1], &mut list_state);
}

fn render_hover_popup(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let Some(popup) = &app.hover_popup else { return };
    let popup_w = ((area.width as f32 * 0.65) as u16).max(24).min(area.width);
    let popup_h = ((area.height as f32 * 0.4) as u16).max(6).min(area.height);
    let popup_x = area.x + (area.width.saturating_sub(popup_w)) / 2;
    let popup_y = area.y + (area.height.saturating_sub(popup_h)) / 2;
    let popup_rect = Rect::new(popup_x, popup_y, popup_w, popup_h);

    frame.render_widget(Clear, popup_rect);
    frame.render_widget(
        Paragraph::new(popup.content.as_str())
            .block(
                Block::default()
                    .title(format!(" {} ", popup.title))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Rgb(137, 220, 235)))
                    .style(Style::default().bg(Color::Rgb(22, 24, 31))),
            )
            .style(Style::default().fg(Color::Rgb(213, 216, 224)).bg(Color::Rgb(22, 24, 31)))
            .wrap(Wrap { trim: false }),
        popup_rect,
    );
}

fn diagnostic_marker_for_line(buf: &BufState, line_index: usize) -> Option<DiagnosticSeverity> {
    let (line_start, line_end) = line_byte_range(&buf.lines, line_index)?;
    buf.diagnostics
        .iter()
        .filter_map(|diagnostic| {
            let overlaps = diagnostic.range.start <= line_end && diagnostic.range.end >= line_start;
            overlaps.then_some(diagnostic.severity.clone())
        })
        .min_by_key(diagnostic_rank)
}

fn line_byte_range(lines: &[String], target_line: usize) -> Option<(usize, usize)> {
    if target_line >= lines.len() {
        return None;
    }
    let start = lines.iter().take(target_line).fold(0usize, |acc, line| acc + line.len() + 1);
    let end = start + lines[target_line].len();
    Some((start, end))
}

fn diagnostic_rank(severity: &DiagnosticSeverity) -> u8 {
    match severity {
        DiagnosticSeverity::Error => 0,
        DiagnosticSeverity::Warning => 1,
        DiagnosticSeverity::Information => 2,
        DiagnosticSeverity::Hint => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_annotation_overlay_styles_target_range() {
        let spans = vec![Span::styled("alpha", Style::default().fg(Color::Rgb(1, 2, 3)))];
        let visual = annotation_visual("find");

        let out = apply_annotation_overlay(spans, 1, 3, 0, visual);

        assert_eq!(out.len(), 3);
        assert_eq!(out[0].content, "a");
        assert_eq!(out[1].content, "lp");
        assert_eq!(out[2].content, "ha");
        assert_eq!(
            out[1].style,
            Style::default().fg(Color::Rgb(22, 24, 31)).bg(visual.bg).add_modifier(Modifier::BOLD)
        );
    }

    #[test]
    fn apply_core_annotations_maps_byte_ranges_to_display_cols() {
        let spans = vec![Span::styled("abcdef", Style::default())];
        let annotations = vec![CoreAnnotation {
            annotation_type: String::from("other"),
            ranges: vec![[0, 2, 0, 5]],
            payloads: None,
        }];

        let out = apply_core_annotations(spans, "abcdef", 0, &annotations, 0);

        assert_eq!(out.len(), 3);
        assert_eq!(out[0].content, "ab");
        assert_eq!(out[1].content, "cde");
        assert_eq!(
            out[1].style,
            Style::default().bg(Color::Rgb(43, 82, 74)).add_modifier(Modifier::UNDERLINED)
        );
    }

    #[test]
    fn collect_line_annotation_segments_merges_same_priority_overlaps() {
        let annotations = vec![
            CoreAnnotation {
                annotation_type: String::from("lint"),
                ranges: vec![[0, 1, 0, 3]],
                payloads: None,
            },
            CoreAnnotation {
                annotation_type: String::from("lint"),
                ranges: vec![[0, 2, 0, 5]],
                payloads: None,
            },
        ];

        let segments = collect_line_annotation_segments("abcdef", 0, &annotations);

        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].start_display, 1);
        assert_eq!(segments[0].end_display, 5);
    }

    #[test]
    fn collect_line_annotation_segments_sorts_by_priority() {
        let annotations = vec![
            CoreAnnotation {
                annotation_type: String::from("find"),
                ranges: vec![[0, 1, 0, 2]],
                payloads: None,
            },
            CoreAnnotation {
                annotation_type: String::from("selection"),
                ranges: vec![[0, 1, 0, 2]],
                payloads: None,
            },
        ];

        let segments = collect_line_annotation_segments("abcdef", 0, &annotations);

        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].priority, annotation_priority("selection"));
        assert_eq!(segments[1].priority, annotation_priority("find"));
    }

    #[test]
    fn annotation_marker_for_line_prefers_payload_backed_plugin_annotations() {
        let buf = BufState {
            id: 1,
            path: None,
            display_name: None,
            view_id: String::new(),
            editor_config_synced: true,
            pending_line_request: false,
            line_cache: Vec::new(),
            lines: vec![String::from("alpha")],
            cursor_line: 0,
            cursor_col: 0,
            pristine: true,
            status_message: None,
            last_scroll: None,
            mtime: None,
            externally_modified: false,
            diagnostics: Vec::new(),
            annotations: vec![CoreAnnotation {
                annotation_type: String::from("lint"),
                ranges: vec![[0, 0, 0, 3]],
                payloads: Some(vec![serde_json::Value::String(String::from("todo"))]),
            }],
        };

        assert_eq!(annotation_marker_for_line(&buf, 0), Some(('T', Color::Rgb(166, 227, 161))));
    }
}
