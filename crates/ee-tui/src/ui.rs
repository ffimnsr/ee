use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::{App, Mode, Viewport};
use crate::buffer::BufState;
use crate::text::{byte_col_to_display_col, display_col_to_byte};

pub(crate) fn ui(frame: &mut ratatui::Frame<'_>, app: &App) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(Style::default().bg(Color::Rgb(22, 24, 31))), area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)])
        .split(area);

    let editor_area = rows[0];

    // Render each window with its own buffer and viewport.
    for (win_id, buf_id, win_rect, is_focused) in app.windows.layout_for_area(editor_area) {
        let Some(buf) = app.backend.all_bufs().iter().find(|b| b.id == buf_id) else {
            continue;
        };
        let vp = app.windows.viewport_for_window(win_id, app.viewport);

        let editor = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(6), Constraint::Min(1)])
            .split(win_rect);

        render_gutter(frame, editor[0], buf, vp);
        render_buffer(frame, editor[1], buf, vp);

        if is_focused {
            let cursor = cursor_position_for(buf, vp, app, editor[1], rows[2]);
            frame.set_cursor_position(cursor);
        }
    }

    render_status(frame, rows[1], app);
    render_prompt(frame, rows[2], app);
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
