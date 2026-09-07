//! UI: picker rendering.
use super::*;

pub(super) fn render_qf_panel(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    list: &QfList,
    focused: bool,
    is_location_list: bool,
) {
    let border_style = if focused {
        Style::default().fg(theme::FG_KEY)
    } else {
        Style::default().fg(theme::FG_EMPTY)
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
        .style(Style::default().bg(theme::BG_CHROME_ALT));

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
                Style::default().fg(theme::BG_APP).bg(theme::FG_KEY).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::FG_BUFFER)
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

pub(super) fn picker_kind_badge(kind: PickerKind) -> &'static str {
    match kind {
        PickerKind::Files => " FILES ",
        PickerKind::Buffers => " BUFFERS ",
        PickerKind::LiveGrep => " GREP ",
        PickerKind::Help => " HELP ",
        PickerKind::Completions => " COMPLETIONS ",
        PickerKind::CodeActions => " ACTIONS ",
        PickerKind::Symbols => " SYMBOLS ",
        PickerKind::Locations => " LOCATIONS ",
        #[cfg(feature = "agents")]
        PickerKind::AgentThreads => " AGENTS ",
        #[cfg(feature = "agents")]
        PickerKind::AgentServers => " NEW AGENT ",
    }
}

pub(super) fn truncate_picker_text(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }

    if text.width() <= max_width {
        return text.to_owned();
    }

    let ellipsis = if max_width >= 3 { "..." } else { "." };
    let ellipsis_width = ellipsis.width().min(max_width);
    let mut out = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if width + ch_width + ellipsis_width > max_width {
            break;
        }
        out.push(ch);
        width += ch_width;
    }
    out.push_str(&ellipsis[..ellipsis_width]);
    out
}

pub(super) fn picker_selection_summary(app: &App) -> String {
    let Some(picker) = &app.picker else { return String::new() };
    let Some(item) = picker.selected_item() else {
        return if picker.query.is_empty() {
            "No matches".to_owned()
        } else {
            format!("No matches for '{}'", picker.query)
        };
    };

    let mut parts = Vec::new();
    if let Some(detail) = item.detail.as_ref().filter(|detail| !detail.is_empty()) {
        parts.push(detail.clone());
    } else if let Some(path) = &item.path {
        parts.push(path.to_string_lossy().into_owned());
    }
    if let Some(line) = item.line {
        if let Some(col) = item.col {
            parts.push(format!("{}:{}", line + 1, col + 1));
        } else {
            parts.push(format!("{}", line + 1));
        }
    }
    if parts.is_empty() {
        parts.push(item.label.clone());
    }
    parts.join("  ")
}

/// Render the floating picker overlay centered in `area`.
pub(super) fn render_picker(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let Some(picker) = &app.picker else { return };

    let popup_w = ((area.width as f32 * 0.84) as u16).max(36).min(area.width);
    let popup_h = ((area.height as f32 * 0.76) as u16).max(12).min(area.height);
    let popup_x = area.x + (area.width.saturating_sub(popup_w)) / 2;
    let popup_y = area.y + (area.height.saturating_sub(popup_h)) / 2;
    let popup_rect = Rect::new(popup_x, popup_y, popup_w, popup_h);

    let shadow_x = popup_rect.x.saturating_add(1).min(area.right().saturating_sub(1));
    let shadow_y = popup_rect.y.saturating_add(1).min(area.bottom().saturating_sub(1));
    let shadow_rect = Rect::new(
        shadow_x,
        shadow_y,
        popup_rect.width.min(area.right().saturating_sub(shadow_x)),
        popup_rect.height.min(area.bottom().saturating_sub(shadow_y)),
    );

    if shadow_rect.width > 0 && shadow_rect.height > 0 {
        frame.render_widget(
            Block::default().style(Style::default().bg(theme::BG_OVERLAY_SHADOW)),
            shadow_rect,
        );
    }

    frame.render_widget(Clear, popup_rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::BORDER_PICKER))
        .style(Style::default().bg(theme::BG_PICKER));

    let inner = block.inner(popup_rect);
    frame.render_widget(block, popup_rect);

    if inner.height < 6 {
        return;
    }

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(inner);

    let selected_position = if picker.filtered.is_empty() {
        "0/0".to_owned()
    } else {
        format!("{}/{}", picker.selected + 1, picker.filtered.len())
    };
    let matches_label = format!(" {}  matches ", selected_position);
    let header = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(12),
            Constraint::Length(matches_label.chars().count() as u16),
        ])
        .split(sections[0]);
    let header_line = Line::from(vec![
        Span::styled(
            picker_kind_badge(picker.kind),
            Style::default()
                .fg(theme::FG_INVERTED)
                .bg(theme::BORDER_PICKER)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            picker.title.as_str(),
            Style::default().fg(theme::FG_PICKER_TITLE).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if picker.query.is_empty() { "" } else { "  filtered" },
            Style::default().fg(theme::FG_PICKER_SUBTLE),
        ),
    ]);
    frame.render_widget(Paragraph::new(header_line), header[0]);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {} ", selected_position),
                Style::default().fg(theme::FG_PICKER_COUNT),
            ),
            Span::styled(" matches ", Style::default().fg(theme::BORDER_PICKER)),
        ]))
        .alignment(Alignment::Right),
        header[1],
    );

    let search_prefix = match picker.kind {
        PickerKind::LiveGrep => "/ ",
        _ => "> ",
    };
    let search_block = Block::default()
        .title(" Query ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::BORDER_PICKER_QUERY))
        .style(Style::default().bg(theme::BG_PICKER_QUERY));
    let search_inner = search_block.inner(sections[1]);
    frame.render_widget(search_block, sections[1]);
    let search_line = Line::from(vec![
        Span::styled(
            search_prefix,
            Style::default().fg(theme::BORDER_PICKER).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if picker.query.is_empty() { "type to filter" } else { picker.query.as_str() },
            if picker.query.is_empty() {
                Style::default().fg(theme::FG_PICKER_PLACEHOLDER)
            } else {
                Style::default().fg(theme::FG_PICKER_QUERY)
            },
        ),
    ]);
    frame.render_widget(Paragraph::new(search_line), search_inner);

    let cursor_x = (search_inner.x + search_prefix.len() as u16 + picker.query.len() as u16)
        .min(search_inner.right().saturating_sub(1));
    frame.set_cursor_position(Position::new(cursor_x, search_inner.y));

    let list_title = format!(" Results {} ", picker.filtered.len());
    let list_block = Block::default()
        .title(list_title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::BORDER_PICKER_RESULTS))
        .style(Style::default().bg(theme::BG_PICKER_RESULTS));
    let list_inner = list_block.inner(sections[2]);
    frame.render_widget(list_block, sections[2]);

    if list_inner.height == 0 || list_inner.width == 0 {
        return;
    }

    let list_height = list_inner.height as usize;
    let selected = picker.selected;
    let scroll_off = if selected >= list_height { selected + 1 - list_height } else { 0 };

    if picker.filtered.is_empty() {
        frame.render_widget(
            Paragraph::new("No results")
                .style(Style::default().fg(theme::FG_PICKER_EMPTY).bg(theme::BG_PICKER_RESULTS))
                .alignment(Alignment::Center),
            list_inner,
        );
    } else {
        let row_width = list_inner.width.saturating_sub(7) as usize;
        let list_items: Vec<ListItem> = picker
            .visible_items_range(scroll_off, list_height)
            .into_iter()
            .enumerate()
            .map(|(i, label)| {
                let abs_idx = scroll_off + i;
                let is_sel = abs_idx == selected;
                let row_bg = if is_sel {
                    theme::BORDER_PICKER
                } else if abs_idx % 2 == 0 {
                    theme::BG_PICKER_RESULTS
                } else {
                    theme::BG_PICKER_ROW_ALT
                };
                let marker = if is_sel { ">" } else { " " };
                let index_style = if is_sel {
                    Style::default().fg(theme::FG_INVERTED).bg(row_bg)
                } else {
                    Style::default().fg(theme::FG_PICKER_INDEX).bg(row_bg)
                };
                let label_style = if is_sel {
                    Style::default().fg(theme::FG_INVERTED).bg(row_bg).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme::FG_BUFFER).bg(row_bg)
                };
                let row_label = truncate_picker_text(&label, row_width.max(1));
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {} ", marker), index_style),
                    Span::styled(format!("{:>3}", abs_idx + 1), index_style),
                    Span::styled("  ", Style::default().bg(row_bg)),
                    Span::styled(row_label, label_style),
                ]))
            })
            .collect();

        let mut list_state = ListState::default();
        list_state.select(Some(selected.saturating_sub(scroll_off)));
        frame.render_stateful_widget(List::new(list_items), list_inner, &mut list_state);
    }

    let footer = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(10), Constraint::Length(26)])
        .split(sections[3]);
    frame.render_widget(
        Paragraph::new(truncate_picker_text(
            &picker_selection_summary(app),
            footer[0].width as usize,
        ))
        .style(Style::default().fg(theme::FG_PICKER_FOOTER).bg(theme::BG_PICKER)),
        footer[0],
    );
    frame.render_widget(
        Paragraph::new("Enter open  Esc close")
            .style(Style::default().fg(theme::BORDER_PICKER).bg(theme::BG_PICKER))
            .alignment(Alignment::Right),
        footer[1],
    );
}
