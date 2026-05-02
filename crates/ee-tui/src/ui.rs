use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::app::{App, Mode, Viewport};
use crate::buffer::BufState;
use crate::text::{byte_col_to_display_col, display_col_to_byte};
use crate::picker::PickerKind;

pub(crate) fn ui(frame: &mut ratatui::Frame<'_>, app: &App) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(Style::default().bg(Color::Rgb(22, 24, 31))), area);

    let tab_count = app.tabs.tab_count();
    let rows = if tab_count > 1 {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
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

    let (tab_bar_area, editor_area, status_area, prompt_area) = if tab_count > 1 {
        (Some(rows[0]), rows[1], rows[2], rows[3])
    } else {
        (None, rows[0], rows[1], rows[2])
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

        let editor = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(6), Constraint::Min(1)])
            .split(win_rect);

        render_gutter(frame, editor[0], buf, vp);
        render_buffer(frame, editor[1], buf, vp);

        if is_focused {
            let cursor = cursor_position_for(buf, vp, app, editor[1], prompt_area);
            frame.set_cursor_position(cursor);
        }
    }

    render_status(frame, status_area, app);
    render_prompt(frame, prompt_area, app);

    // Picker overlay (drawn last so it floats above everything).
    if app.picker.is_some() {
        render_picker(frame, area, app);
    }
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

fn render_gutter(frame: &mut ratatui::Frame<'_>, area: Rect, buf: &BufState, vp: Viewport) {
    let height = area.height as usize;
    let line_count = buf.lines.len().max(1);
    let top = vp.top_line;
    let lines = (0..height)
        .map(|i| {
            let line_idx = top + i;
            let number = if line_idx < line_count { line_idx + 1 } else { 0 };
            if number == 0 {
                Line::from(" ")
            } else {
                Line::from(Span::styled(
                    format!("{number:>4} "),
                    Style::default().fg(Color::DarkGray),
                ))
            }
        })
        .collect::<Vec<_>>();

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(Color::Rgb(30, 32, 39))),
        area,
    );
}

fn render_buffer(frame: &mut ratatui::Frame<'_>, area: Rect, buf: &BufState, vp: Viewport) {
    let height = area.height as usize;
    let top = vp.top_line;
    let left = vp.left_col;

    let text = if buf.lines.is_empty() && top == 0 {
        vec![Line::from(Span::styled(
            "empty buffer",
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        ))]
    } else {
        buf.lines
            .iter()
            .skip(top)
            .take(height)
            .map(|line| {
                let byte_start = display_col_to_byte(line, left);
                Line::from(&line[byte_start..])
            })
            .collect::<Vec<_>>()
    };

    frame.render_widget(
        Paragraph::new(text)
            .block(Block::default().borders(Borders::NONE))
            .style(Style::default().fg(Color::Rgb(213, 216, 224)).bg(Color::Rgb(22, 24, 31))),
        area,
    );
}

fn render_status(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let mode = Span::styled(
        format!(" {} ", app.mode.label()),
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
    let buf_count = app.backend.buf_count();
    let buf_indicator = if buf_count > 1 {
        Span::styled(
            format!("  [{}/{}]", app.backend.current_idx() + 1, buf_count),
            Style::default().fg(Color::Rgb(166, 173, 200)).bg(Color::Rgb(49, 54, 68)),
        )
    } else {
        Span::raw("")
    };
    let position = Span::styled(
        format!("  Ln {}, Col {} ", app.backend.cursor_line + 1, app.backend.cursor_col + 1),
        Style::default().fg(Color::Rgb(186, 194, 222)).bg(Color::Rgb(49, 54, 68)),
    );

    frame.render_widget(
        Paragraph::new(Line::from(vec![mode, file, modified, buf_indicator, position]))
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

