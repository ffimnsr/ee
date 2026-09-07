//! UI: agents_pane rendering.
use super::*;

pub(super) fn transcript_lines(
    item: &crate::app::TranscriptItem,
    width: usize,
    show_tool_detail: bool,
) -> Vec<Line<'static>> {
    use crate::app::{MessageRenderKind, TranscriptItem};
    let width = width.max(8);
    let nick_col = crate::app::AGENTS_NICK_COL_WIDTH;
    let indent = "[HH:MM] ".chars().count() + nick_col + 1;
    let text_width = width.saturating_sub(indent).max(4);
    let mut lines = Vec::new();
    let dim = Style::default().fg(theme::FG_DIM);
    match item {
        TranscriptItem::Message { nick, text, kind, at, .. } => {
            let style = match kind {
                MessageRenderKind::User => Style::default().fg(theme::FG_KEY),
                MessageRenderKind::Assistant => Style::default().fg(theme::FG_TEXT),
                MessageRenderKind::Thought => Style::default()
                    .fg(theme::FG_INFO)
                    .add_modifier(Modifier::ITALIC | Modifier::BOLD),
            };
            let time = fmt_hhmm(*at);
            let nick_display = pad_or_trim(nick, nick_col);
            let wrapped = crate::app::wrap_text(text, text_width);
            for (index, segment) in
                wrapped.iter().filter(|segment| !segment.trim().is_empty()).enumerate()
            {
                if index == 0 {
                    lines.push(Line::from(vec![
                        Span::styled(format!("[{time}]"), dim),
                        Span::raw(" "),
                        Span::styled(nick_display.clone(), style),
                        Span::styled(" ", dim),
                        Span::styled(segment.clone(), style),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::raw(" ".repeat(indent)),
                        Span::styled(segment.clone(), style),
                    ]));
                }
            }
        }
        TranscriptItem::ToolCall { title, status, detail, at, .. } => {
            let time = fmt_hhmm(*at);
            lines.push(Line::from(vec![
                Span::styled(format!("[{time}]"), dim),
                Span::styled(" * ", Style::default().fg(theme::FG_WARNING)),
                Span::styled(format!("{title} [{status}]"), Style::default().fg(theme::FG_WARNING)),
            ]));
            if show_tool_detail {
                for segment in crate::app::wrap_text(detail, text_width) {
                    lines.push(Line::from(vec![
                        Span::raw(" ".repeat(indent)),
                        Span::styled(segment, dim),
                    ]));
                }
            }
        }

        TranscriptItem::Permission { title, options, at } => {
            let time = fmt_hhmm(*at);
            lines.push(Line::from(vec![
                Span::styled(format!("[{time}]"), dim),
                Span::styled(" permission: ", Style::default().fg(theme::FG_WARNING)),
                Span::styled(title.clone(), Style::default().fg(theme::FG_TEXT)),
            ]));
            for option in options {
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(indent)),
                    Span::styled("· ", Style::default().fg(theme::FG_WARNING)),
                    Span::styled(option.clone(), Style::default().fg(theme::FG_TEXT)),
                ]));
            }
        }
        TranscriptItem::Elicitation { agent, message, url, url_host, at } => {
            let time = fmt_hhmm(*at);
            lines.push(Line::from(vec![
                Span::styled(format!("[{time}]"), dim),
                Span::styled(" elicitation: ", Style::default().fg(theme::FG_KEY)),
                Span::styled(agent.clone(), Style::default().fg(theme::FG_WARNING)),
                Span::raw(" "),
                Span::styled(message.clone(), Style::default().fg(theme::FG_TEXT)),
            ]));
            if let Some(host) = url_host {
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(indent)),
                    Span::styled("host: ", dim),
                    Span::styled(host.clone(), Style::default().fg(theme::FG_WARNING)),
                ]));
            }
            if let Some(url) = url {
                for (index, segment) in
                    crate::app::wrap_text(url, text_width).into_iter().enumerate()
                {
                    let mut spans = vec![Span::raw(" ".repeat(indent))];
                    if index == 0 {
                        spans.push(Span::styled("url: ", dim));
                    }
                    spans.push(Span::styled(segment, Style::default().fg(theme::FG_INFO)));
                    lines.push(Line::from(spans));
                }
            }
        }
        TranscriptItem::System { text, .. } => {
            let notice_width = width.saturating_sub(4).max(4);
            for (index, segment) in
                crate::app::wrap_text(text, notice_width).into_iter().enumerate()
            {
                let prefix = if index == 0 { "-!- " } else { "    " };
                lines.push(Line::from(vec![Span::styled(prefix, dim), Span::styled(segment, dim)]));
            }
        }
        TranscriptItem::Stderr { text, .. } => {
            lines.push(Line::from(vec![
                Span::styled("-!- [stderr] ", Style::default().fg(theme::FG_WARNING)),
                Span::styled(text.clone(), dim),
            ]));
        }
    }
    lines
}

#[cfg(feature = "agents")]
pub(super) fn agents_composer_height(app: &App, area: Rect) -> u16 {
    session_deletion_confirmation_composer_lines(app)
        .or_else(|| additional_directory_confirmation_composer_lines(app))
        .or_else(|| terminal_stop_confirmation_composer_lines(app))
        .or_else(|| approval_mode_confirmation_composer_lines(app))
        .or_else(|| approval_composer_lines(app, area.width as usize))
        .or_else(|| mode_selection_composer_lines(app, area.width as usize))
        .map(|(lines, _)| lines.len().min(area.height.saturating_sub(2) as usize) as u16)
        .unwrap_or(1)
        .max(1)
}

#[cfg(feature = "agents")]
pub(super) fn agents_pane_rows(app: &App, area: Rect) -> std::rc::Rc<[Rect]> {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(agents_composer_height(app, area)),
        ])
        .split(area)
}

#[cfg(feature = "agents")]
pub(super) fn agent_transcript_lines(
    app: &App,
    thread: &crate::app::AgentThreadUi,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut rendered_response_groups = std::collections::BTreeSet::new();
    for item in &thread.transcript {
        if matches!(item, crate::app::TranscriptItem::Message { text, .. } if text.trim().is_empty())
        {
            continue;
        }
        if !app.agents.show_thoughts
            && matches!(
                item,
                crate::app::TranscriptItem::Message {
                    kind: crate::app::MessageRenderKind::Thought,
                    ..
                }
            )
        {
            continue;
        }
        let response_group = thread.response_group_for_item(item);
        let show_tool_detail = thread.transcript_raw
            || thread.transcript_detail
            || response_group.is_some_and(|group| thread.expanded_tool_details.contains(&group));
        if let Some(group) = response_group {
            if !thread.transcript_raw && rendered_response_groups.insert(group) {
                let (thoughts, tools) = thread.response_group_counts(group);
                let selected = thread.selected_response_group == Some(group);
                let collapsed = thread.collapsed_response_groups.contains(&group);
                let marker = if collapsed { "+" } else { "−" };
                let selected_marker = if selected { "> " } else { "  " };
                let mut header = format!(
                    "{selected_marker}[{marker}] response: {thoughts} reasoning, {tools} tools"
                );
                if let Some(summary) = thread.response_group_change_summary(group) {
                    header.push_str(&format!(" · {summary}"));
                }
                if let Some(metrics) = thread.turn_metrics.get(&group) {
                    header.push_str(&format!(" · {}", crate::app::turn_metrics_label(metrics)));
                } else if thread.active_response_group == Some(group)
                    && let Some(started) = thread.turn_started_at
                {
                    header.push_str(&format!(
                        " · {}",
                        crate::app::format_duration(started.elapsed())
                    ));
                }
                lines.push(Line::from(Span::styled(header, Style::default().fg(theme::FG_DIM))));
            }
            if !thread.transcript_raw && thread.collapsed_response_groups.contains(&group) {
                continue;
            }
        }
        lines.extend(transcript_lines(item, width, show_tool_detail));
    }
    lines
}

/// Maximum visual-row offset for active agent transcript within `area`.
#[cfg(feature = "agents")]
pub(crate) fn agents_transcript_scroll_max(app: &App, area: Rect) -> usize {
    let Some(active_index) = app.agents.active_thread else {
        return 0;
    };
    let transcript_area = agents_pane_rows(app, area)[0];
    let line_count = agent_transcript_lines(
        app,
        &app.agents.threads[active_index],
        transcript_area.width.saturating_sub(1) as usize,
    )
    .len();
    line_count.saturating_sub(transcript_area.height as usize)
}
