use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{App, Mode, Viewport};
use crate::buffer::BufState;
use crate::config::{NumberStyle, StatuslineFormat};
use crate::quickfix::QfList;
use crate::text::{byte_col_to_display_col, display_col_to_byte};
use crate::picker::PickerKind;

pub(crate) fn ui(frame: &mut ratatui::Frame<'_>, app: &App) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(Style::default().bg(Color::Rgb(22, 24, 31))), area);

    let tab_count = app.tabs.tab_count();

    // Determine whether a list panel (quickfix or location list) is visible.
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

    // Tab bar (only when more than one tab is open).
    if let Some(tab_area) = tab_bar_area {
        render_tab_bar(frame, tab_area, app);
    }

    // Render each window in the focused tab.
    for (win_id, buf_id, win_rect, is_focused) in
        app.tabs.focused_windows().layout_for_area(editor_area)
    {
        let Some(buf) = app.backend.all_bufs().iter().find(|b| b.id == buf_id) else {
            continue;
        };
        let vp = app.tabs.focused_windows().viewport_for_window(win_id, app.viewport);
        let gw = gutter_width(app, buf.lines.len());

        let editor = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(gw), Constraint::Min(1)])
            .split(win_rect);

        render_gutter(frame, editor[0], buf, vp, app);
        render_buffer(frame, editor[1], buf, vp, app);

        if is_focused {
            let cursor = cursor_position_for(buf, vp, app, editor[1], prompt_area);
            frame.set_cursor_position(cursor);
        }
    }

    render_status(frame, status_area, app);
    render_prompt(frame, prompt_area, app);

    // Quickfix / location-list panel (drawn before picker overlay).
    if let Some(qf_rect) = qf_area {
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
        Paragraph::new(Line::from(spans))
            .style(Style::default().bg(Color::Rgb(22, 24, 31))),
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

    let mut lines: Vec<Line> = Vec::with_capacity(height);
    let mut li = top;
    for _ in 0..height {
        if li >= line_count {
            lines.push(Line::from(" "));
            continue;
        }
        let is_cursor = li == cursor_line;
        let bg = if is_cursor && app.config.cursor_line { cursor_line_bg } else { Color::Rgb(30, 32, 39) };

        // Sign column: show fold markers when applicable.
        let sign_span = if sign_col {
            let marker = if app.folds.fold_at(buf.id, li).is_some() {
                "▸ "
            } else {
                "  "
            };
            Span::styled(marker, Style::default().fg(Color::Rgb(100, 130, 160)).bg(bg))
        } else {
            Span::raw("")
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
            lines.push(Line::from(vec![sign_span, num_span]));
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

    let extension = buf
        .path
        .as_ref()
        .and_then(|p| p.extension())
        .and_then(|e| e.to_str());

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
                    format!("{preview}  ··· (folded)", ),
                    Style::default()
                        .fg(Color::Rgb(100, 130, 160))
                        .bg(bg)
                        .add_modifier(Modifier::ITALIC),
                )]
            } else {
                let byte_start = display_col_to_byte(line, left);
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
                    vec![Span::styled(
                        line[byte_start..].to_owned(),
                        Style::default().bg(bg),
                    )]
                }
            };

            // Apply visible-whitespace substitution when enabled.
            if app.config.show_visible_whitespace && !is_fold_header {
                spans = apply_visible_whitespace(spans);
            }

            // Apply color column highlight.
            if let Some(cc) = app.config.color_column {
                if cc >= left {
                    let screen_col = cc - left;
                    spans = apply_color_column(spans, screen_col);
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
            vec![mode, file, modified, position]
        }
        StatuslineFormat::Default => {
            let buf_count = app.backend.buf_count();
            let buf_indicator = if buf_count > 1 {
                Span::styled(
                    format!("  [{}/{}]", app.backend.current_idx() + 1, buf_count),
                    Style::default()
                        .fg(Color::Rgb(166, 173, 200))
                        .bg(Color::Rgb(49, 54, 68)),
                )
            } else {
                Span::raw("")
            };
            // Show wrap/list/number indicators on the right.
            let flags = {
                let mut f = String::new();
                if app.config.wrap_lines { f.push_str(" wrap"); }
                if app.config.show_visible_whitespace { f.push_str(" list"); }
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
            vec![mode, file, modified, buf_indicator, flag_span, position]
        }
    };

    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .style(Style::default().bg(Color::Rgb(49, 54, 68))),
        area,
    );
}

fn render_prompt(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let prompt = match app.mode {
        Mode::Normal => Line::from(match app.backend.status_message.as_deref() {
            Some(message) => message.to_owned(),
            None => "normal | i insert | v visual | : command | q quit".to_owned(),
        }),
        Mode::Insert => Line::from("insert | esc normal"),
        Mode::Visual => Line::from("visual | hjkl/move selects | d/y/c operators | v/esc normal | : command"),
        Mode::VisualLine => Line::from("visual-line | j/k extends | d/y/c operators | V/esc normal"),
        Mode::VisualBlock => Line::from("visual-block | hjkl extends block | d/y I/A operators | Ctrl-V/esc normal"),
        Mode::CommandLine => {
            Line::from(vec![Span::raw(":"), Span::raw(app.command_buffer.as_str())])
        }
        Mode::Search => Line::from(vec![Span::raw("/"), Span::raw(app.command_buffer.as_str())]),
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

    let title = match picker.kind {
        PickerKind::Files => " Files ",
        PickerKind::Buffers => " Buffers ",
        PickerKind::LiveGrep => " Live Grep ",
    };

    let block = Block::default()
        .title(title)
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
    let scroll_off = if selected >= list_height {
        selected + 1 - list_height
    } else {
        0
    };

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
    frame.render_stateful_widget(
        List::new(list_items),
        chunks[1],
        &mut list_state,
    );
}
