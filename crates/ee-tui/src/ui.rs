use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::{App, Mode};
use crate::text::display_col_to_byte;

pub(crate) fn ui(frame: &mut ratatui::Frame<'_>, app: &App) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(Style::default().bg(Color::Rgb(22, 24, 31))), area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)])
        .split(area);

    let editor = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(6), Constraint::Min(1)])
        .split(rows[0]);

    render_gutter(frame, editor[0], app);
    render_buffer(frame, editor[1], app);
    render_status(frame, rows[1], app);
    render_prompt(frame, rows[2], app);

    let cursor = app.cursor_position(editor[1], rows[2]);
    frame.set_cursor_position(cursor);
}

fn render_gutter(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let height = area.height as usize;
    let line_count = app.backend.lines.len().max(1);
    let top = app.viewport.top_line;
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

fn render_buffer(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let height = area.height as usize;
    let top = app.viewport.top_line;
    let left = app.viewport.left_col;

    let text = if app.backend.lines.is_empty() && top == 0 {
        vec![Line::from(Span::styled(
            "empty buffer",
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        ))]
    } else {
        app.backend
            .lines
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
    let position = Span::styled(
        format!("  Ln {}, Col {} ", app.backend.cursor_line + 1, app.backend.cursor_col + 1),
        Style::default().fg(Color::Rgb(186, 194, 222)).bg(Color::Rgb(49, 54, 68)),
    );

    frame.render_widget(
        Paragraph::new(Line::from(vec![mode, file, modified, position]))
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
        Mode::Visual => Line::from("visual | move selects | v/esc normal | : command"),
        Mode::CommandLine => {
            Line::from(vec![Span::raw(":"), Span::raw(app.command_buffer.as_str())])
        }
        Mode::Search => Line::from(vec![Span::raw("/"), Span::raw(app.command_buffer.as_str())]),
    };

    frame.render_widget(
        Paragraph::new(prompt)
            .style(Style::default().fg(Color::Rgb(166, 173, 200)).bg(Color::Rgb(24, 25, 38))),
        area,
    );
}
