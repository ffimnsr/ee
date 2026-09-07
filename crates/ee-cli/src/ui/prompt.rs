//! UI: prompt rendering.
use super::*;

pub(super) fn render_prompt(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    if let Some(swift_motion) = app.swift_motion.as_ref() {
        frame.render_widget(
            Paragraph::new(Line::from(swift_motion.prompt()))
                .style(Style::default().fg(theme::BG_APP).bg(theme::FG_KEY)),
            area,
        );
        return;
    }

    if let Some(label) = app.active_key_hint_label() {
        frame.render_widget(
            Paragraph::new(Line::from(format!("keys: {label}")))
                .style(Style::default().fg(theme::FG_MUTED).bg(theme::BG_CHROME)),
            area,
        );
        return;
    }

    if let Some(message) = app.pending_input_label() {
        frame.render_widget(
            Paragraph::new(Line::from(message))
                .style(Style::default().fg(theme::FG_MUTED).bg(theme::BG_CHROME)),
            area,
        );
        return;
    }

    let prompt = match app.mode {
        Mode::Normal => Line::default(),
        Mode::Insert
        | Mode::Visual
        | Mode::VisualLine
        | Mode::VisualBlock
        | Mode::Picker
        | Mode::Quickfix
        | Mode::LocationList
        | Mode::OperatorPending
        | Mode::Agent => Line::default(),
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
        Mode::PrivilegeConfirm => Line::from(match app.backend.status_message.as_deref() {
            Some(msg) => msg.to_owned(),
            None => "permission denied — retry with elevated save? [y]es [n]o".to_owned(),
        }),
    };

    frame.render_widget(
        Paragraph::new(prompt).style(Style::default().fg(theme::FG_MUTED).bg(theme::BG_CHROME)),
        area,
    );
}

pub(super) fn render_key_hints(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let Some(label) = app.active_key_hint_label() else { return };
    let Some(entries) = app.active_key_hint_entries() else { return };

    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(theme::BORDER_MUTED))
        .title(key_hint_title(&label))
        .style(Style::default().bg(theme::BG_CHROME));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let rows = (inner.height as usize).max(1);
    let min_cell_width = 24usize;
    let mut cols = (inner.width as usize / min_cell_width).max(1).min(entries.len().max(1));
    while cols > 1 && entries.len().div_ceil(cols) > rows {
        cols -= 1;
    }
    let visible_rows = entries.len().div_ceil(cols).max(1).min(rows);
    let cell_width = (inner.width as usize / cols).max(1);
    let key_width = entries
        .iter()
        .map(|entry| UnicodeWidthStr::width(entry.key.as_str()) + 2)
        .max()
        .unwrap_or(5)
        .min(cell_width.saturating_sub(4).max(5));
    let desc_width = cell_width.saturating_sub(key_width + 1);

    let mut lines: Vec<Line<'static>> = Vec::new();
    for row in 0..visible_rows {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut has_content = false;
        for col in 0..cols {
            let index = col * visible_rows + row;
            if index >= entries.len() {
                break;
            }
            let entry = &entries[index];
            let key_label = pad_or_trim(&entry.key, key_width.saturating_sub(2));
            let desc_text = entry.description.clone();
            let desc_label = pad_or_trim(&desc_text, desc_width);

            spans.push(Span::styled(
                format!(" {} ", key_label),
                Style::default().fg(theme::FG_KEY).add_modifier(Modifier::BOLD),
            ));
            if desc_width > 0 {
                spans.push(Span::styled(
                    desc_label,
                    Style::default().fg(theme::FG_TEXT).bg(theme::BG_CHROME),
                ));
            }
            if col + 1 < cols {
                spans.push(Span::raw(" "));
            }
            has_content = true;
        }
        if has_content {
            lines.push(Line::from(spans));
        }
    }

    if lines.is_empty() {
        lines.push(Line::from("no child bindings"));
    }

    lines.truncate(inner.height as usize);
    frame.render_widget(Paragraph::new(lines).style(Style::default().bg(theme::BG_CHROME)), inner);
}

pub(super) fn key_hint_title(label: &str) -> Line<'static> {
    let mut spans = vec![Span::styled(
        String::from(" keys "),
        Style::default().fg(theme::FG_MUTED).bg(theme::BG_CHROME).add_modifier(Modifier::DIM),
    )];

    let parts = label.split_whitespace().collect::<Vec<_>>();
    for (index, part) in parts.iter().enumerate() {
        let is_last = index + 1 == parts.len();
        spans.push(Span::styled(
            format!(" {} ", part),
            if is_last {
                Style::default().fg(theme::FG_TEXT).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(theme::FG_DIM)
                    .add_modifier(Modifier::DIM)
                    .add_modifier(Modifier::BOLD)
            },
        ));
        spans.push(Span::raw(" "));
    }

    Line::from(spans)
}

pub(super) fn pad_or_trim(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }

    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let ch_width = UnicodeWidthStr::width(ch.encode_utf8(&mut [0; 4]));
        if used + ch_width > width {
            break;
        }
        used += ch_width;
        out.push(ch);
    }
    if used < width {
        out.push_str(&" ".repeat(width - used));
    }
    out
}

pub(super) fn cursor_position_for(
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

    let line = buf.get_line(buf.cursor_line).unwrap_or("");
    let display_col = byte_col_to_display_col(line, buf.cursor_col);

    let screen_line =
        app.folds.rendered_row_for_line(buf.id, vp.top_line, buf.cursor_line).unwrap_or_default();
    let screen_col = display_col.saturating_sub(vp.left_col);

    let x = (editor_area.x + screen_col as u16).min(max_x);
    let y = (editor_area.y + screen_line as u16).min(max_y);
    Position::new(x, y)
}
