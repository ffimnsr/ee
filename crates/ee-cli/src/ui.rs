use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use xi_core_lib::plugin_rpc::DiagnosticSeverity;

use crate::app::{App, Mode, SwiftMotionTarget, Viewport, smart_case_sensitive};
use crate::backend::{CoreAnnotation, LineSlot, VlfSearchRange};
use crate::buffer::BufState;
use crate::config::{NumberStyle, StatuslineFormat};
use crate::picker::PickerKind;
use crate::quickfix::QfList;
use crate::text::{byte_col_to_display_col, display_col_to_byte, wrap_text};
use crate::theme::ui as theme;

#[derive(Clone, Copy)]
struct RootAreas {
    tab_bar_area: Option<Rect>,
    editor_area: Rect,
    agents_area: Option<Rect>,
    qf_area: Option<Rect>,
    key_hint_area: Option<Rect>,
    status_area: Rect,
    prompt_area: Rect,
}

/// Returns the rect the agents pane occupies inside `root_area`, or `None`
/// when the pane is closed.  The pane always steals from the editor area;
/// other root rows (status, prompt, qf) are untouched.
#[cfg(feature = "agents")]
fn agents_pane_rect(root_area: Rect, app: &App) -> Option<Rect> {
    use crate::app::AgentPaneLayout;
    match app.agents_layout() {
        AgentPaneLayout::Closed => None,
        AgentPaneLayout::Right => {
            let width = crate::app::AGENTS_PANE_RIGHT_WIDTH.min(root_area.width.saturating_sub(20));
            Some(Rect {
                x: root_area.right() - width,
                y: root_area.y,
                width,
                height: root_area.height,
            })
        }
        AgentPaneLayout::Bottom => {
            let height =
                crate::app::AGENTS_PANE_BOTTOM_HEIGHT.min(root_area.height.saturating_sub(6));
            Some(Rect {
                x: root_area.x,
                y: root_area.bottom() - height,
                width: root_area.width,
                height,
            })
        }
        AgentPaneLayout::Full => Some(root_area),
    }
}

/// The pane rect within a full terminal `area` (mouse hit-testing).
#[allow(dead_code)]
pub(crate) fn agents_pane_rect_for(area: Rect, app: &App) -> Option<Rect> {
    split_root_areas(area, app).agents_area
}

fn split_root_areas(area: Rect, app: &App) -> RootAreas {
    #[cfg(feature = "agents")]
    if app.agents_layout() == crate::app::AgentPaneLayout::Full {
        let zero = Rect { x: area.x, y: area.y, width: 0, height: 0 };
        return RootAreas {
            tab_bar_area: None,
            editor_area: zero,
            agents_area: Some(area),
            qf_area: None,
            key_hint_area: None,
            status_area: zero,
            prompt_area: zero,
        };
    }

    let tab_count = app.tabs.tab_count();
    let key_hint_visible = app.active_key_hint_entries().is_some();

    let qf_panel_visible = (app.quickfix_open && app.quickfix.is_some())
        || (app.location_list_open && app.location_list.is_some());
    const QF_HEIGHT: u16 = 8;
    const KEY_HINT_HEIGHT: u16 = 4;

    let rows = if tab_count > 1 {
        if qf_panel_visible {
            let mut constraints =
                vec![Constraint::Length(1), Constraint::Min(1), Constraint::Length(QF_HEIGHT)];
            if key_hint_visible {
                constraints.push(Constraint::Length(KEY_HINT_HEIGHT));
            }
            constraints.push(Constraint::Length(1));
            constraints.push(Constraint::Length(1));
            Layout::default().direction(Direction::Vertical).constraints(constraints).split(area)
        } else {
            let mut constraints = vec![Constraint::Length(1), Constraint::Min(1)];
            if key_hint_visible {
                constraints.push(Constraint::Length(KEY_HINT_HEIGHT));
            }
            constraints.push(Constraint::Length(1));
            constraints.push(Constraint::Length(1));
            Layout::default().direction(Direction::Vertical).constraints(constraints).split(area)
        }
    } else if qf_panel_visible {
        let mut constraints = vec![Constraint::Min(1), Constraint::Length(QF_HEIGHT)];
        if key_hint_visible {
            constraints.push(Constraint::Length(KEY_HINT_HEIGHT));
        }
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Length(1));
        Layout::default().direction(Direction::Vertical).constraints(constraints).split(area)
    } else {
        let mut constraints = vec![Constraint::Min(1)];
        if key_hint_visible {
            constraints.push(Constraint::Length(KEY_HINT_HEIGHT));
        }
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Length(1));
        Layout::default().direction(Direction::Vertical).constraints(constraints).split(area)
    };

    let mut index = 0;
    let tab_bar_area = if tab_count > 1 {
        let area = Some(rows[index]);
        index += 1;
        area
    } else {
        None
    };
    let editor_area = rows[index];
    index += 1;
    // The agents pane (feature `agents`) steals part of the editor area;
    // when closed the editor layout is untouched.
    #[cfg(feature = "agents")]
    let (editor_area, agents_area) = {
        use crate::app::AgentPaneLayout;
        if let Some(pane) = agents_pane_rect(editor_area, app) {
            match app.agents_layout() {
                AgentPaneLayout::Right => {
                    let columns = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([Constraint::Min(1), Constraint::Length(pane.width)])
                        .split(editor_area);
                    (columns[0], Some(pane))
                }
                AgentPaneLayout::Bottom => {
                    let rows = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Min(1), Constraint::Length(pane.height)])
                        .split(editor_area);
                    (rows[0], Some(pane))
                }
                AgentPaneLayout::Full => {
                    (Rect { x: editor_area.x, y: editor_area.y, width: 0, height: 0 }, Some(pane))
                }
                AgentPaneLayout::Closed => (editor_area, None),
            }
        } else {
            (editor_area, None)
        }
    };
    #[cfg(not(feature = "agents"))]
    let (editor_area, agents_area) = (editor_area, None);
    let qf_area = if qf_panel_visible {
        let area = Some(rows[index]);
        index += 1;
        area
    } else {
        None
    };
    let key_hint_area = if key_hint_visible {
        let area = Some(rows[index]);
        index += 1;
        area
    } else {
        None
    };
    let status_area = rows[index];
    index += 1;
    let prompt_area = rows[index];

    RootAreas {
        tab_bar_area,
        editor_area,
        agents_area,
        qf_area,
        key_hint_area,
        status_area,
        prompt_area,
    }
}

/// Return the visible editor row count for the current app state and terminal
/// size. Use this wherever xi-core must be told how many lines fit on screen.
pub(crate) fn compute_editor_height(terminal_size: ratatui::layout::Rect, app: &App) -> usize {
    split_root_areas(terminal_size, app).editor_area.height as usize
}

const BUFFER_LEFT_PADDING_COLS: u16 = 1;

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

fn buffer_content_area(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(BUFFER_LEFT_PADDING_COLS),
        y: area.y,
        width: area.width.saturating_sub(BUFFER_LEFT_PADDING_COLS),
        height: area.height,
    }
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
    let [_, buffer_area] = window_chunks(app, win_rect, buf.line_count());
    let content_area = buffer_content_area(buffer_area);
    let line = vp.top_line + usize::from(row.saturating_sub(content_area.y));
    let display_col = if column < buffer_area.x {
        0
    } else if column < content_area.x {
        vp.left_col
    } else {
        vp.left_col + usize::from(column - content_area.x)
    };
    Some((line, display_col))
}

pub(crate) fn ui(frame: &mut ratatui::Frame<'_>, app: &App) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(Style::default().bg(theme::BG_APP)), area);
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
        let editor = window_chunks(app, win_rect, buf.line_count());
        let buffer_area = buffer_content_area(editor[1]);

        render_gutter(frame, editor[0], buf, vp, app);
        render_buffer(frame, editor[1], buf, vp, app);

        if is_focused && !cursor_hidden_by_agents(app) {
            let cursor = cursor_position_for(buf, vp, app, buffer_area, root.prompt_area);
            frame.set_cursor_position(cursor);
        }
    }

    render_status(frame, root.status_area, app);
    render_prompt(frame, root.prompt_area, app);

    // Agents pane (feature `agents`); full layout intentionally draws last so
    // it owns the whole terminal like a chat client.
    #[cfg(feature = "agents")]
    if let Some(rect) = root.agents_area {
        render_agents_pane(frame, rect, app);
    }

    if let Some(key_hint_area) = root.key_hint_area {
        render_key_hints(frame, key_hint_area, app);
    }

    // Quickfix / location-list panel (drawn before picker overlay).
    if let Some(qf_rect) = root.qf_area {
        if app.quickfix_open {
            if let Some(qf) = &app.quickfix {
                render_qf_panel(frame, qf_rect, qf, app.quickfix_focused, false);
            }
        } else if app.location_list_open
            && let Some(ll) = &app.location_list
        {
            render_qf_panel(frame, qf_rect, ll, app.location_list_focused, true);
        }
    }

    // Picker overlay (drawn last so it floats above everything).
    if app.hover_popup.is_some() {
        render_hover_popup(frame, area, app);
    }

    if app.picker.is_some() {
        render_picker(frame, area, app);
    }

    render_toast(frame, area, app);
}

// ── Agents pane (feature `agents`) ────────────────────────────────────────────

/// Whether the editor cursor must be hidden because the agents pane owns
/// focus (the pane places its own cursor in the composer).
fn cursor_hidden_by_agents(app: &App) -> bool {
    #[cfg(feature = "agents")]
    {
        app.agents_focused()
    }
    #[cfg(not(feature = "agents"))]
    {
        let _ = app;
        false
    }
}

/// Formats a wall-clock timestamp as `HH:MM` (UTC; display only).
#[cfg(feature = "agents")]
fn fmt_hhmm(at: std::time::SystemTime) -> String {
    let secs = at.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let day_secs = secs % 86_400;
    format!("{:02}:{:02}", day_secs / 3600, (day_secs / 60) % 60)
}

/// Renders one transcript item into wrapped lines for the given width.
#[cfg(feature = "agents")]
fn transcript_lines(
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
fn agents_composer_height(app: &App, area: Rect) -> u16 {
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
fn agents_pane_rows(app: &App, area: Rect) -> std::rc::Rc<[Rect]> {
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
fn agent_transcript_lines(
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

/// Renders agents pane: transcript scrollback, status footer, and composer line.
#[cfg(feature = "agents")]
fn render_agents_pane(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    use crate::app::ThreadUiState;

    let inner = area;
    frame.render_widget(Paragraph::new("").style(Style::default().bg(theme::BG_CHROME)), inner);

    // Disabled-state message (defensive; commands never open the pane while
    // agents are disabled).
    if !app.config.agents.enabled {
        let message = "agents mode disabled (set `agents.enabled = true` to enable)";
        let mut lines = Vec::new();
        for segment in crate::app::wrap_text(message, inner.width.saturating_sub(4) as usize) {
            lines.push(Line::from(Span::styled(segment, Style::default().fg(theme::FG_WARNING))));
        }
        let paragraph = Paragraph::new(lines).alignment(Alignment::Center);
        frame.render_widget(paragraph, inner);
        return;
    }

    let expanded_composer = session_deletion_confirmation_composer_lines(app)
        .or_else(|| additional_directory_confirmation_composer_lines(app))
        .or_else(|| terminal_stop_confirmation_composer_lines(app))
        .or_else(|| approval_mode_confirmation_composer_lines(app))
        .or_else(|| approval_composer_lines(app, inner.width as usize))
        .or_else(|| mode_selection_composer_lines(app, inner.width as usize));
    // Keep one transcript row and the footer visible. Expanded composer prompts
    // consume remaining space so every choice stays readable.
    let rows = agents_pane_rows(app, inner);
    let transcript_area = rows[0];
    let footer_area = rows[1];
    let composer_area = rows[2];

    // Transcript scrollback.
    let composer = if let Some(active_index) = app.agents.active_thread {
        let thread = &app.agents.threads[active_index];
        let lines =
            agent_transcript_lines(app, thread, transcript_area.width.saturating_sub(1) as usize);
        let visible_height = transcript_area.height as usize;
        let max_scroll = lines.len().saturating_sub(visible_height);
        let top = if thread.stick_to_bottom { max_scroll } else { thread.scroll.min(max_scroll) };
        let mut window: Vec<Line<'static>> =
            lines.into_iter().skip(top).take(visible_height).collect();
        if thread.stick_to_bottom && window.len() < visible_height {
            let mut bottom_aligned = vec![Line::default(); visible_height - window.len()];
            bottom_aligned.append(&mut window);
            window = bottom_aligned;
        }
        frame.render_widget(Paragraph::new(window).scroll((0, 0)), transcript_area);

        // Footer: nick, state, current mode, unread, stop reason (left), and
        // the session's ACP context-window usage (used/size tokens) right-aligned
        // — this is the row directly above the composer. The stop reason is omitted
        // because the transcript already logs `turn completed (stop: …)`.
        let state_label = match thread.state {
            ThreadUiState::Starting => "starting".into(),
            ThreadUiState::Ready => "ready".into(),
            ThreadUiState::Running => "running".into(),
            ThreadUiState::PausedRecoverable => thread.pending_recovery.as_ref().map_or_else(
                || std::borrow::Cow::Borrowed("paused (recoverable)"),
                |pending| {
                    std::borrow::Cow::Owned(format!("paused (recoverable: {})", pending.info.fault))
                },
            ),
            ThreadUiState::Closed => "closed".into(),
            ThreadUiState::Failed => "failed".into(),
        };
        let current_mode = thread
            .host
            .snapshot()
            .current_mode
            .map_or_else(|| String::from("unset"), |mode| mode.0.to_string());
        let footer_text = format!(
            "{} [{}] | mode:{} | session:{} / {} | thoughts:{} | unread:{} | last:{}",
            thread.nick,
            state_label,
            current_mode,
            active_index + 1,
            app.agents.threads.len(),
            if app.agents.show_thoughts { "on" } else { "off" },
            thread.unread,
            thread
                .last_turn_metrics
                .as_ref()
                .map(crate::app::turn_metrics_label)
                .unwrap_or_else(|| String::from("—")),
        );
        let footer_style = if thread.state == ThreadUiState::Running {
            Style::default().fg(theme::FG_WARNING).bg(theme::BG_AGENT_STATUS)
        } else {
            Style::default().fg(theme::FG_AGENT_STATUS).bg(theme::BG_AGENT_STATUS)
        };
        let usage_label = thread.usage.as_deref().unwrap_or_default();
        let usage_width = usage_label.width().min(footer_area.width as usize) as u16;
        let footer_line = Line::from(Span::styled(footer_text, footer_style));
        frame.render_widget(
            Block::default().style(Style::default().bg(theme::BG_AGENT_STATUS)),
            footer_area,
        );
        if usage_width == 0 {
            frame.render_widget(
                Paragraph::new(footer_line).style(Style::default().bg(theme::BG_AGENT_STATUS)),
                footer_area,
            );
        } else {
            // Split the row: the left footer truncates independently so the
            // context usage always stays visible at the rightmost edge, even
            // in narrow panes where the left text overflows.
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(0), Constraint::Length(usage_width)])
                .split(footer_area);
            frame.render_widget(Paragraph::new(footer_line), cols[0]);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    usage_label.to_string(),
                    Style::default().fg(theme::FG_AGENT_STATUS).bg(theme::BG_AGENT_STATUS),
                )))
                .style(Style::default().bg(theme::BG_AGENT_STATUS)),
                cols[1],
            );
        }
        agents_composer_line(app, thread)
    } else {
        let mut lines = vec![Line::from(vec![
            Span::styled("-!- ", theme_style(theme::FG_DIM)),
            Span::styled("no agent session", theme_style(theme::FG_WARNING)),
        ])];
        if app.agents.pending_session.is_some() {
            lines.push(Line::from(Span::styled(
                "-!- starting session… type now; draft carries into session",
                theme_style(theme::FG_DIM),
            )));
        } else if let Some(error) = &app.agents.error {
            lines.push(Line::from(vec![
                Span::styled("-!- ", theme_style(theme::FG_DIM)),
                Span::styled(error.clone(), theme_style(theme::FG_WARNING)),
            ]));
        } else {
            lines.push(Line::from(Span::styled(
                "-!- configure [agents.servers.<id>] in .ee.toml, then type /new_thread",
                theme_style(theme::FG_DIM),
            )));
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), transcript_area);
        let footer_style = Style::default().fg(theme::FG_AGENT_STATUS).bg(theme::BG_AGENT_STATUS);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "agents [no session] | mode:unset",
                footer_style,
            )))
            .style(footer_style),
            footer_area,
        );
        agents_pending_composer_line(app)
    };

    // Expanded composer prompts render one visible row per choice. Everything else
    // stays single-line to preserve editor space.
    if let Some((lines, selected_row)) = expanded_composer {
        frame.render_widget(Paragraph::new(lines), composer_area);
        if app.agents_focused() {
            frame.set_cursor_position(Position {
                x: composer_area.x,
                y: composer_area.y.saturating_add(
                    selected_row.min(composer_area.height.saturating_sub(1) as usize) as u16,
                ),
            });
        }
    } else {
        let cursor_col = composer
            .iter()
            .map(|span| span.width())
            .sum::<usize>()
            .min(composer_area.width.saturating_sub(1) as usize);
        frame.render_widget(Paragraph::new(Line::from(composer)), composer_area);
        if app.agents_focused() {
            frame.set_cursor_position(Position {
                x: composer_area.x.saturating_add(cursor_col as u16),
                y: composer_area.y,
            });
        }
    }

    if let Some(active_index) = app.agents.active_thread {
        let thread = &app.agents.threads[active_index];
        if thread.plan_modal_open && !thread.current_plan.is_empty() {
            render_plan_modal(frame, inner, &thread.current_plan);
        }
    }
}

#[cfg(feature = "agents")]
fn render_plan_modal(frame: &mut ratatui::Frame<'_>, area: Rect, entries: &[(String, char)]) {
    let modal = plan_modal_rect(area, entries.len());
    frame.render_widget(Clear, modal);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" plan — Esc close ")
        .border_style(Style::default().fg(theme::FG_KEY))
        .style(Style::default().fg(theme::FG_TEXT).bg(theme::BG_CHROME));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let text_width = inner.width.saturating_sub(4).max(4) as usize;
    let mut lines = Vec::new();
    for (content, marker) in entries {
        let marker_style = match *marker {
            'x' => Style::default().fg(theme::FG_INFO),
            '>' => Style::default().fg(theme::FG_WARNING),
            '-' => Style::default().fg(theme::FG_KEY),
            _ => Style::default().fg(theme::FG_DIM),
        };
        let wrapped = crate::app::wrap_text(content, text_width);
        for (index, segment) in wrapped.into_iter().enumerate() {
            if index == 0 {
                lines.push(Line::from(vec![
                    Span::styled(format!("[{marker}] "), marker_style),
                    Span::styled(segment, Style::default().fg(theme::FG_TEXT)),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(segment, Style::default().fg(theme::FG_TEXT)),
                ]));
            }
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(feature = "agents")]
fn plan_modal_rect(area: Rect, entry_count: usize) -> Rect {
    let width = area.width.saturating_sub(4).clamp(24, 72);
    let height = (entry_count as u16 + 4).min(area.height.saturating_sub(2)).max(5);
    let x = area.x + area.width.saturating_sub(width).saturating_sub(2);
    let y = area.y.saturating_add(2);
    Rect { x, y, width, height }
}

/// Builds local-session deletion confirmation rows. Provider data is never deleted.
#[cfg(feature = "agents")]
fn session_deletion_confirmation_composer_lines(app: &App) -> Option<(Vec<Line<'static>>, usize)> {
    let confirmation = app.agents.session_deletion_confirmation.as_ref()?;
    Some((
        vec![
            Line::from(Span::styled(
                "delete local session transcript?",
                Style::default().fg(theme::FG_WARNING).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!("  name: {}", confirmation.session_name),
                theme_style(theme::FG_TEXT),
            )),
            Line::from(Span::styled(
                format!("  session: {}", confirmation.session_id),
                theme_style(theme::FG_TEXT),
            )),
            Line::from(Span::styled(
                "  removes local transcript and reconnect metadata only; provider session remains.",
                theme_style(theme::FG_TEXT),
            )),
            Line::from(Span::styled("  Enter delete · Esc cancel", theme_style(theme::FG_DIM))),
        ],
        0,
    ))
}

/// Builds additional-workspace-root confirmation rows.
#[cfg(feature = "agents")]
fn additional_directory_confirmation_composer_lines(
    app: &App,
) -> Option<(Vec<Line<'static>>, usize)> {
    let confirmation = app.agents.additional_directory_confirmation.as_ref()?;
    Some((
        vec![
            Line::from(Span::styled(
                "trust additional workspace root?",
                Style::default().fg(theme::FG_WARNING).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!("  {}", confirmation.path.display()),
                theme_style(theme::FG_TEXT),
            )),
            Line::from(Span::styled(
                "  adds local access only for this Agents TUI session; current provider session stays unchanged.",
                theme_style(theme::FG_TEXT),
            )),
            Line::from(Span::styled("  Enter trust · Esc cancel", theme_style(theme::FG_DIM))),
        ],
        0,
    ))
}

/// Builds multi-terminal stop confirmation rows.
#[cfg(feature = "agents")]
fn terminal_stop_confirmation_composer_lines(app: &App) -> Option<(Vec<Line<'static>>, usize)> {
    let confirmation = app.agents.terminal_stop_confirmation.as_ref()?;
    Some((
        vec![
            Line::from(Span::styled(
                format!("stop {} owned terminals?", confirmation.terminal_ids.len()),
                Style::default().fg(theme::FG_WARNING).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!("  {}", confirmation.terminal_ids.join(", ")),
                theme_style(theme::FG_TEXT),
            )),
            Line::from(Span::styled(
                "  only registered direct children are stopped; descendants may remain.",
                theme_style(theme::FG_TEXT),
            )),
            Line::from(Span::styled("  Enter stop · Esc cancel", theme_style(theme::FG_DIM))),
        ],
        0,
    ))
}

/// Builds bypass-mode confirmation rows. Returns selected option row for cursor placement.
#[cfg(feature = "agents")]
fn approval_mode_confirmation_composer_lines(app: &App) -> Option<(Vec<Line<'static>>, usize)> {
    app.agents.approval_mode_confirmation.as_ref()?;
    Some((
        vec![
            Line::from(Span::styled(
                "enable bypass tool approvals?",
                Style::default().fg(theme::FG_WARNING).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "  validated writes and terminal commands will run without approval dialogs for this session.",
                theme_style(theme::FG_TEXT),
            )),
            Line::from(Span::styled(
                "  workspace, secret, revision, and command validation remain enforced.",
                theme_style(theme::FG_TEXT),
            )),
            Line::from(Span::styled("  Enter enable · Esc cancel", theme_style(theme::FG_DIM))),
        ],
        0,
    ))
}

/// Builds expanded approval rows. Returns selected option row for cursor placement.
#[cfg(feature = "agents")]
fn approval_composer_lines(app: &App, width: usize) -> Option<(Vec<Line<'static>>, usize)> {
    let approval = app.agents.approvals.front()?;
    let count = approval.options.len();
    let selected = approval.selected.min(count.saturating_sub(1));
    let detail_width = width.saturating_sub(2).max(4);
    let mut lines = vec![Line::from(vec![
        Span::styled(
            "approval required ",
            Style::default().fg(theme::FG_WARNING).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("[{}/{}] {}", selected + 1, count, approval.title),
            Style::default().fg(theme::FG_TEXT),
        ),
    ])];

    for segment in crate::app::wrap_text(&approval.detail, detail_width) {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(segment, Style::default().fg(theme::FG_TEXT)),
        ]));
    }

    let first_option_row = lines.len();
    for (index, (label, _)) in approval.options.iter().enumerate() {
        let is_selected = index == selected;
        let style = if is_selected {
            Style::default().fg(theme::FG_KEY).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::FG_DIM)
        };
        lines.push(Line::from(vec![
            Span::styled(if is_selected { "> " } else { "  " }, style),
            Span::styled(label.clone(), style),
        ]));
    }
    lines.push(Line::from(Span::styled(
        "  ↑/↓ select · Enter confirm · Esc deny",
        theme_style(theme::FG_DIM),
    )));

    Some((lines, first_option_row + selected))
}

/// Builds expanded local mode-selection rows. Returns selected option row for cursor placement.
#[cfg(feature = "agents")]
fn mode_selection_composer_lines(app: &App, _width: usize) -> Option<(Vec<Line<'static>>, usize)> {
    let prompt = app.agents.mode_selection.as_ref()?;
    let count = prompt.options.len();
    if count == 0 {
        return Some((
            vec![
                Line::from(Span::styled(
                    "mode unavailable",
                    Style::default().fg(theme::FG_WARNING).add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    "  agent did not advertise selectable ACP modes",
                    theme_style(theme::FG_TEXT),
                )),
                Line::from(Span::styled("  Esc close", theme_style(theme::FG_DIM))),
            ],
            0,
        ));
    }
    let selected = prompt.selected.min(count.saturating_sub(1));
    let mut lines = vec![Line::from(vec![
        Span::styled(
            "select mode ",
            Style::default().fg(theme::FG_WARNING).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("[{}/{}]", selected + 1, count), Style::default().fg(theme::FG_TEXT)),
    ])];
    let first_option_row = lines.len();
    for (index, mode) in prompt.options.iter().enumerate() {
        let is_selected = index == selected;
        let style = if is_selected {
            Style::default().fg(theme::FG_KEY).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::FG_DIM)
        };
        let label = mode.clone();
        lines.push(Line::from(vec![
            Span::styled(if is_selected { "> " } else { "  " }, style),
            Span::styled(label, style),
        ]));
    }
    lines.push(Line::from(Span::styled(
        "  ↑/↓ select · Enter confirm · Esc cancel",
        theme_style(theme::FG_DIM),
    )));

    Some((lines, first_option_row + selected))
}

/// Builds the single-line composer for permission choice, elicitation widget, or
/// prompt draft. Expanded approvals and mode selection use dedicated renderers instead.
#[cfg(feature = "agents")]
fn agents_composer_line(app: &App, thread: &crate::app::AgentThreadUi) -> Vec<Span<'static>> {
    if let Some(permission) = &app.agents.permission {
        let count = permission.options.len();
        let selected = permission.selected.min(count.saturating_sub(1));
        let option = permission
            .options
            .get(selected)
            .map(|option| option.name.clone())
            .unwrap_or_else(|| String::from("(none)"));
        return vec![
            Span::styled("> ", Style::default().fg(theme::FG_KEY)),
            Span::styled(
                format!("[{}/{}] ", selected + 1, count),
                Style::default().fg(theme::FG_WARNING),
            ),
            Span::styled(option, Style::default().fg(theme::FG_TEXT)),
            Span::styled(" (Enter confirm, ←/→ change, Esc back)", theme_style(theme::FG_DIM)),
        ];
    }
    if let Some(elicitation) = &app.agents.elicitation {
        let mut spans = vec![Span::styled("> ", Style::default().fg(theme::FG_KEY))];
        if let Some(url) = &elicitation.url {
            let choice = match elicitation.selected_choice {
                1 => "decline",
                2 => "cancel",
                _ => "accept/open",
            };
            if let Some(host) = &elicitation.url_host {
                spans.push(Span::styled(
                    format!("host: {host} "),
                    Style::default().fg(theme::FG_WARNING),
                ));
            }
            spans.push(Span::styled(
                format!(
                    "url: {url} [{choice}] (←/→ change, Enter confirm, Ctrl-D decline, Esc cancel)"
                ),
                Style::default().fg(theme::FG_INFO),
            ));
            return spans;
        }
        let Some(field) = elicitation.fields.get(elicitation.selected_field) else {
            spans.push(Span::styled(
                "no fields (Enter accept, Ctrl-D decline, Esc cancel)",
                theme_style(theme::FG_DIM),
            ));
            return spans;
        };
        let marker = if field.unsupported.is_some() { "!" } else { ">" };
        let value = field.display_value();
        spans.push(Span::styled(
            format!("{} — {marker} {}: {}", elicitation.agent_label, field.label(), value),
            Style::default().fg(theme::FG_TEXT),
        ));
        spans.push(Span::styled(
            " (↑/↓ field, ←/→ value, Enter accept, Ctrl-D decline, Esc cancel)",
            theme_style(theme::FG_DIM),
        ));
        return spans;
    }
    // MCP browse picker (Phase 6): selection list + insert hint.
    if let Some(browse) = &app.agents.mcp.browse {
        let mut spans = vec![Span::styled("> ", Style::default().fg(theme::FG_KEY))];
        if browse.loading {
            spans.push(Span::styled(
                format!("mcp {} browse…", browse.kind.label()),
                theme_style(theme::FG_DIM),
            ));
            return spans;
        }
        if let Some(error) = &browse.error {
            spans.push(Span::styled(
                format!("mcp {} browse error: {error}", browse.kind.label()),
                theme_style(theme::FG_WARNING),
            ));
            return spans;
        }
        let Some(item) = browse.items.get(browse.selected) else {
            spans.push(Span::styled(
                format!("mcp {}: (empty) — Esc close", browse.kind.label()),
                theme_style(theme::FG_DIM),
            ));
            return spans;
        };
        let total = browse.items.len();
        let mut label = format!(
            "mcp {} [{}/{}] {}",
            browse.kind.label(),
            browse.selected + 1,
            total,
            item.label
        );
        if let Some(detail) = &item.detail {
            label.push_str(&format!(" — {detail}"));
        }
        spans.push(Span::styled(label, Style::default().fg(theme::FG_TEXT)));
        spans
            .push(Span::styled(" (↑/↓ move, Enter insert, Esc close)", theme_style(theme::FG_DIM)));
        return spans;
    }
    let draft = &thread.draft;
    let command_hint = draft
        .trim_start()
        .strip_prefix('/')
        .is_some_and(|prefix| !prefix.chars().any(char::is_whitespace));
    let mut spans = vec![
        Span::styled("prompt> ", Style::default().fg(theme::FG_KEY).add_modifier(Modifier::BOLD)),
        Span::styled(draft.clone(), Style::default().fg(theme::FG_TEXT)),
    ];
    if command_hint {
        spans.push(Span::styled("  Tab complete · /help", theme_style(theme::FG_DIM)));
    }
    spans
}

/// Builds the composer line shown while no session exists yet.
#[cfg(feature = "agents")]
fn agents_pending_composer_line(app: &App) -> Vec<Span<'static>> {
    vec![
        Span::styled("prompt> ", Style::default().fg(theme::FG_KEY).add_modifier(Modifier::BOLD)),
        Span::styled(app.agents.pending_draft.clone(), Style::default().fg(theme::FG_TEXT)),
    ]
}

/// Styled span helper used by the agents pane.
#[cfg(feature = "agents")]
fn theme_style(color: Color) -> Style {
    Style::default().fg(color)
}

// ── Status bar ───────────────────────────────────────────────────────────────

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
    let line_count = app.backend.line_count().max(1);
    let gw = gutter_width(app, line_count);
    area.width.saturating_sub(gw + BUFFER_LEFT_PADDING_COLS) as usize
}

// ── Visible-whitespace substitution ──────────────────────────────────────────

/// Substitute space `' '` → `'·'` and tab `'\t'` → `'→'` in rendered spans,
/// applying a dimmed style to the replaced characters.
fn apply_visible_whitespace(spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
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

fn control_picture(ch: char) -> Option<char> {
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
fn sanitize_control_chars(spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
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
fn apply_color_column(spans: Vec<Span<'static>>, screen_col: usize) -> Vec<Span<'static>> {
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

fn pad_spans_to_width(
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

fn expand_tabs_in_spans(spans: Vec<Span<'static>>, tab_width: usize) -> Vec<Span<'static>> {
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
fn apply_visual_highlight(
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
fn apply_search_highlights(
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
            bg: theme::BG_SELECTION,
            fg: Some(theme::FG_TEXT),
            modifier: Modifier::empty(),
        },
        "find" => AnnotationVisual {
            bg: theme::BG_FIND,
            fg: Some(theme::BG_APP),
            modifier: Modifier::BOLD,
        },
        _ => {
            AnnotationVisual { bg: theme::BG_ANNOTATION, fg: None, modifier: Modifier::UNDERLINED }
        }
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
        "find" => theme::FG_WARNING,
        "selection" => theme::FG_INFO,
        _ => theme::FG_MARKER_HINT,
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

    if buf.is_vlf && buf.vlf_search_ranges.iter().any(|range| range.line as usize == line_index) {
        best = Some((annotation_priority("find"), '•', annotation_marker_color("find")));
    }

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

fn replace_display_column(
    spans: Vec<Span<'static>>,
    display_col: usize,
    replacement: char,
    replacement_style: Style,
) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut col = 0usize;
    let mut replaced = false;

    for sp in spans {
        let content = sp.content.into_owned();
        let style = sp.style;
        let span_cols = UnicodeWidthStr::width(content.as_str());
        let span_end = col + span_cols;

        if replaced || display_col < col || display_col >= span_end {
            out.push(Span::styled(content, style));
            col = span_end;
            continue;
        }

        let local_start = display_col - col;
        let start_byte = display_col_to_byte(&content, local_start);
        let end_byte = display_col_to_byte(&content, local_start + 1);

        if start_byte > 0 {
            out.push(Span::styled(content[..start_byte].to_owned(), style));
        }
        out.push(Span::styled(replacement.to_string(), replacement_style));
        if end_byte < content.len() {
            out.push(Span::styled(content[end_byte..].to_owned(), style));
        }

        replaced = true;
        col = span_end;
    }

    out
}

fn apply_swift_motion_targets(
    mut spans: Vec<Span<'static>>,
    targets: &[SwiftMotionTarget],
    left: usize,
) -> Vec<Span<'static>> {
    let visual =
        AnnotationVisual { bg: theme::FG_KEY, fg: Some(theme::BG_APP), modifier: Modifier::BOLD };
    let label_style =
        Style::default().fg(theme::BG_APP).bg(theme::BG_SWIFT_LABEL).add_modifier(Modifier::BOLD);

    for target in targets {
        if target.end_display_col <= left || target.display_col < left {
            continue;
        }
        spans = apply_annotation_overlay(
            spans,
            target.display_col,
            target.end_display_col,
            left,
            visual,
        );
    }

    for target in targets {
        if target.display_col < left {
            continue;
        }
        spans = replace_display_column(spans, target.display_col - left, target.label, label_style);
    }

    spans
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
        if let Some(last) = merged.last_mut()
            && last.priority == segment.priority
            && last.visual == segment.visual
            && segment.start_display <= last.end_display
        {
            last.end_display = last.end_display.max(segment.end_display);
            continue;
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

fn apply_vlf_search_ranges(
    mut spans: Vec<Span<'static>>,
    log_idx: usize,
    ranges: &[VlfSearchRange],
    left: usize,
) -> Vec<Span<'static>> {
    let visual = annotation_visual("find");
    for range in ranges.iter().filter(|range| range.line as usize == log_idx) {
        spans = apply_annotation_overlay(spans, range.start_col, range.end_col, left, visual);
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
                Style::default().fg(theme::BG_APP).bg(theme::FG_KEY).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::FG_TAB_INACTIVE).bg(theme::BG_CHROME)
            };
            [Span::styled(format!(" {} ", label), style), Span::raw(" ")]
        })
        .collect();

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme::BG_APP)),
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
    let line_count = buf.line_count().max(1);
    let cursor_line = buf.cursor_line;
    let top = vp.top_line;
    let sign_col = app.config.sign_column;
    let num_digits = line_count.to_string().len().max(3);
    let cursor_line_bg = theme::BG_CURSOR_LINE;
    let vis_sel_bg = theme::BG_SELECTION;

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
    let tilde_style = Style::default().fg(theme::FG_TILDE).bg(theme::BG_CHROME);
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
            theme::BG_CHROME
        };

        // Sign column: show fold markers when applicable.
        let sign_spans = if sign_col {
            let (marker, fg) = if let Some(severity) = diagnostic_marker_for_line(buf, li) {
                match severity {
                    DiagnosticSeverity::Error => ('E', theme::FG_ERROR),
                    DiagnosticSeverity::Warning => ('W', theme::FG_WARNING),
                    DiagnosticSeverity::Information => ('I', theme::FG_KEY),
                    DiagnosticSeverity::Hint => ('H', theme::FG_MARKER_HINT),
                }
            } else if app.folds.fold_at(buf.id, li).is_some() {
                ('▸', theme::FG_FOLD)
            } else {
                (' ', theme::FG_FOLD)
            };
            let (annotation_marker, annotation_fg) = app
                .git_status(buf.id)
                .and_then(|status| status.sign_for_line(li))
                .map(|sign| (sign.marker(), git_sign_color(sign)))
                .or_else(|| annotation_marker_for_line(buf, li))
                .unwrap_or((' ', theme::FG_GUTTER_DIM));
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
        let num_fg = if is_cursor { theme::FG_TEXT } else { theme::FG_EMPTY };
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

    frame.render_widget(Paragraph::new(lines).style(Style::default().bg(theme::BG_CHROME)), area);
}

fn render_buffer(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    buf: &BufState,
    vp: Viewport,
    app: &App,
) {
    let content_area = buffer_content_area(area);
    let height = area.height as usize;
    let top = vp.top_line;
    let left = vp.left_col;
    let viewport_width = content_area.width as usize;
    let cursor_line = buf.cursor_line;
    let cursor_line_bg = theme::BG_CURSOR_LINE;
    let buf_bg = theme::BG_APP;

    frame.render_widget(Block::default().style(Style::default().bg(buf_bg)), area);

    // Pre-compute visual selection bounds for this buffer.
    // Returns (anchor_line, anchor_col, cursor_line, cursor_col) in natural order.
    let visual_sel = if app.mode.is_visual() {
        let (al, ac) = app.visual_anchor.unwrap_or((cursor_line, buf.cursor_col));
        let (cl, cc) = (cursor_line, buf.cursor_col);
        Some((al, ac, cl, cc))
    } else {
        None
    };

    if buf.line_count() == 0 && top == 0 {
        let label = if buf.pending_line_request
            || buf
                .path
                .as_ref()
                .and_then(|path| std::fs::metadata(path).ok())
                .is_some_and(|metadata| metadata.len() > 0)
        {
            "Loading..."
        } else {
            "empty buffer"
        };
        let text = vec![Line::from(Span::styled(
            label,
            Style::default().fg(theme::FG_EMPTY).add_modifier(Modifier::ITALIC),
        ))];
        frame.render_widget(
            Paragraph::new(text)
                .block(Block::default().borders(Borders::NONE))
                .style(Style::default().fg(theme::FG_BUFFER).bg(buf_bg)),
            content_area,
        );
        return;
    }

    // Collect visible logical lines (fold-aware).  Use line_count() so VLF
    // buffers iterate over `line_cache` size rather than the empty `lines` vec.
    let mut visible: Vec<usize> = Vec::with_capacity(height);
    let mut li = top;
    while visible.len() < height && li < buf.line_count() {
        visible.push(li);
        if let Some((_, end)) = app.folds.fold_at(buf.id, li) {
            li = end + 1;
        } else {
            li += 1;
        }
    }
    // Style for lines not yet loaded in VLF mode.
    let loading_style = Style::default().fg(theme::FG_LOADING).add_modifier(Modifier::ITALIC);

    let text: Vec<Line> = visible
        .iter()
        .map(|&log_idx| {
            let is_cursor = log_idx == cursor_line;
            let bg = if is_cursor && app.config.cursor_line { cursor_line_bg } else { buf_bg };

            // In VLF mode, a `None` return means the page is not yet loaded.
            // Render a non-blocking "Loading…" row instead of an empty placeholder.
            let line_opt = buf.get_line(log_idx);
            if buf.is_vlf && line_opt.is_none() {
                let loading_text = "  Loading…";
                let mut l = Line::from(Span::styled(loading_text, loading_style));
                if is_cursor && app.config.cursor_line {
                    l = l.style(Style::default().bg(bg));
                }
                return l;
            }

            let line = line_opt.unwrap_or("");
            let is_fold_header = app.folds.fold_at(buf.id, log_idx).is_some();
            let byte_start = display_col_to_byte(line, left);
            let byte_end =
                display_col_to_byte(line, left.saturating_add(viewport_width).saturating_add(1));

            let mut spans: Vec<Span<'static>> = if is_fold_header {
                // Show fold marker line (abbreviated first line + fold indicator).
                let preview: String = line.chars().take(40).collect();
                vec![Span::styled(
                    format!("{preview}  ··· (folded)",),
                    Style::default().fg(theme::FG_FOLD).bg(bg).add_modifier(Modifier::ITALIC),
                )]
            } else {
                let backend_syntax = match buf.line_slot(log_idx) {
                    Some(LineSlot::Known(cached_line)) if !cached_line.syntax_spans.is_empty() => {
                        Some(cached_line.syntax_spans.as_slice())
                    }
                    _ => None,
                };

                if let Some(syntax_spans) = backend_syntax {
                    crate::highlight::Highlighter::scope_spans_in_range(
                        line,
                        syntax_spans,
                        byte_start,
                        byte_end,
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
                    vec![Span::styled(
                        line[byte_start..byte_end].to_owned(),
                        Style::default().bg(bg),
                    )]
                }
            };

            // Apply visible-whitespace substitution when enabled.
            if app.config.show_visible_whitespace && !is_fold_header {
                spans = apply_visible_whitespace(spans);
            }

            if !is_fold_header {
                spans = apply_core_annotations(spans, line, log_idx, &buf.annotations, left);
                if buf.is_vlf {
                    spans = apply_vlf_search_ranges(spans, log_idx, &buf.vlf_search_ranges, left);
                }
            }

            spans = expand_tabs_in_spans(spans, app.config.tab_width);

            // Apply search match highlighting over the rendered spans.
            if let Some(ref pat) = app.search_pattern
                && !is_fold_header
                && !buf.is_vlf
            {
                spans = apply_search_highlights(spans, line, pat, byte_start, byte_end, bg);
            }

            spans = sanitize_control_chars(spans);

            // Apply color column highlight.
            if let Some(cc) = app.config.color_column
                && cc >= left
            {
                let screen_col = cc - left;
                spans = apply_color_column(spans, screen_col);
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

            if let Some(swift_motion) = app.swift_motion.as_ref() {
                let line_targets = swift_motion
                    .targets
                    .iter()
                    .filter(|target| target.line == log_idx)
                    .cloned()
                    .collect::<Vec<_>>();
                if !line_targets.is_empty() {
                    spans = apply_swift_motion_targets(spans, &line_targets, left);
                }
            }

            spans = pad_spans_to_width(spans, viewport_width, Style::default().bg(bg));

            let mut l = Line::from(spans);
            if is_cursor && app.config.cursor_line {
                l = l.style(Style::default().bg(bg));
            }
            l
        })
        .collect();

    let mut widget = Paragraph::new(text)
        .block(Block::default().borders(Borders::NONE))
        .style(Style::default().fg(theme::FG_BUFFER).bg(buf_bg));

    if app.config.wrap_lines {
        widget = widget.wrap(Wrap { trim: false });
    }

    frame.render_widget(widget, content_area);
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
        Style::default().fg(theme::BG_APP).bg(theme::FG_KEY),
    );
    let file = Span::styled(
        format!(" {}", app.backend.title()),
        Style::default().fg(theme::FG_STATUS_FILE).bg(theme::BG_STATUS),
    );
    let modified = if app.backend.pristine {
        Span::raw("")
    } else {
        Span::styled(" [+]", Style::default().fg(theme::FG_WARNING).bg(theme::BG_STATUS))
    };
    let vlf_gap = if app.backend.active().is_vlf {
        Span::styled(" ", Style::default().bg(theme::BG_STATUS))
    } else {
        Span::raw("")
    };
    let vlf = if app.backend.active().is_vlf {
        Span::styled(" VLF ", Style::default().fg(theme::BG_APP).bg(theme::BG_FIND))
    } else {
        Span::raw("")
    };
    let position_text =
        format!("  Ln {}, Col {} ", app.backend.cursor_line + 1, app.backend.cursor_col + 1);
    let position = Span::styled(
        position_text.as_str(),
        Style::default().fg(theme::FG_TAB_INACTIVE).bg(theme::BG_STATUS),
    );

    let mut spans = match app.config.statusline_format {
        StatuslineFormat::Minimal => {
            let mut spans = vec![mode, file, modified, vlf_gap, vlf];
            if let Some(git_span) = git_status_span(app) {
                spans.push(git_span);
            }
            spans
        }
        StatuslineFormat::Default => {
            let buf_count = app.backend.buf_count();
            let buf_indicator = if buf_count > 1 {
                Span::styled(
                    format!("  [{}/{}]", app.backend.current_idx() + 1, buf_count),
                    Style::default().fg(theme::FG_MUTED).bg(theme::BG_STATUS),
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
                    Style::default().fg(theme::FG_STATUS_FLAG).bg(theme::BG_STATUS),
                )
            };
            let mut spans = vec![mode, file, modified, vlf_gap, vlf];
            if let Some(git_span) = git_status_span(app) {
                spans.push(git_span);
            }
            spans.push(buf_indicator);
            spans.push(flag_span);
            spans
        }
    };
    let left_width =
        spans.iter().map(|span| UnicodeWidthStr::width(span.content.as_ref())).sum::<usize>();
    let position_width = UnicodeWidthStr::width(position_text.as_str());
    let status_width = area.width as usize;
    if left_width + position_width < status_width {
        spans.push(Span::styled(
            " ".repeat(status_width - left_width - position_width),
            Style::default().bg(theme::BG_STATUS),
        ));
    }
    spans.push(position);

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme::BG_STATUS)),
        area,
    );
}

fn git_sign_color(sign: crate::git::GitSign) -> Color {
    match sign {
        crate::git::GitSign::Added => theme::FG_SUCCESS,
        crate::git::GitSign::Modified => theme::FG_WARNING,
        crate::git::GitSign::Deleted => theme::FG_ERROR,
    }
}

fn git_status_span(app: &App) -> Option<Span<'static>> {
    if app.backend.active().is_vlf {
        return Some(Span::styled(
            "  git:off(vlf)",
            Style::default().fg(theme::FG_WARNING).bg(theme::BG_STATUS),
        ));
    }

    let status = app.current_git_status()?;
    let dirty = if status.dirty { '*' } else { ' ' };
    Some(Span::styled(
        format!("  git:{}{}", status.branch, dirty),
        Style::default().fg(theme::FG_SUCCESS).bg(theme::BG_STATUS),
    ))
}

fn render_prompt(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
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

fn render_key_hints(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
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

fn key_hint_title(label: &str) -> Line<'static> {
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

fn pad_or_trim(text: &str, width: usize) -> String {
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

    let line = buf.get_line(buf.cursor_line).unwrap_or("");
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

fn picker_kind_badge(kind: PickerKind) -> &'static str {
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
    }
}

fn truncate_picker_text(text: &str, max_width: usize) -> String {
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

fn picker_selection_summary(app: &App) -> String {
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
fn render_picker(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
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

const TOAST_MAX_WIDTH: u16 = 72;
const TOAST_MIN_WIDTH: u16 = 24;
const TOAST_MAX_CONTENT_HEIGHT: u16 = 6;
const TOAST_MARGIN: u16 = 1;
const TOAST_FRAME_WIDTH: u16 = 4;
const TOAST_FRAME_HEIGHT: u16 = 2;

fn toast_content_lines(message: &str, width: u16, max_height: u16) -> Vec<String> {
    let content_width = usize::from(width.saturating_sub(TOAST_FRAME_WIDTH)).max(4);
    let mut lines = wrap_text(message, content_width);
    lines.truncate(usize::from(max_height));
    lines
}

fn toast_rect(area: Rect, message: &str) -> Option<Rect> {
    let available_width = area.width.saturating_sub(TOAST_MARGIN * 2);
    let available_height = area.height.saturating_sub(TOAST_MARGIN * 2);
    if available_width < TOAST_MIN_WIDTH || available_height <= TOAST_FRAME_HEIGHT {
        return None;
    }

    let natural_content_width = message.lines().map(UnicodeWidthStr::width).max().unwrap_or(1);
    let desired_width = u16::try_from(natural_content_width)
        .unwrap_or(u16::MAX)
        .saturating_add(TOAST_FRAME_WIDTH)
        .max(TOAST_MIN_WIDTH);
    let width = desired_width.min(TOAST_MAX_WIDTH).min(available_width);
    let max_content_height =
        TOAST_MAX_CONTENT_HEIGHT.min(available_height.saturating_sub(TOAST_FRAME_HEIGHT));
    let content_height =
        toast_content_lines(message, width, max_content_height).len().max(1) as u16;
    let height = content_height.saturating_add(TOAST_FRAME_HEIGHT);

    Some(Rect {
        x: area.right().saturating_sub(width + TOAST_MARGIN),
        y: area.y.saturating_add(TOAST_MARGIN),
        width,
        height,
    })
}

fn render_toast(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let Some(message) = app.toast_message() else { return };
    let Some(rect) = toast_rect(area, message) else { return };

    let content_height = rect.height.saturating_sub(TOAST_FRAME_HEIGHT);
    let content = toast_content_lines(message, rect.width, content_height).join("\n");

    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(content)
            .block(
                Block::default()
                    .title(" notification ")
                    .borders(Borders::ALL)
                    .padding(Padding::new(1, 1, 0, 0))
                    .border_style(Style::default().fg(theme::BORDER_MUTED))
                    .style(Style::default().bg(theme::BG_CHROME)),
            )
            .style(Style::default().fg(theme::FG_MUTED).bg(theme::BG_CHROME))
            .wrap(Wrap { trim: false }),
        rect,
    );
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
                    .border_style(Style::default().fg(theme::FG_KEY))
                    .style(Style::default().bg(theme::BG_APP)),
            )
            .style(Style::default().fg(theme::FG_BUFFER).bg(theme::BG_APP))
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
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[cfg(feature = "agents")]
    #[test]
    fn agents_system_notices_wrap_inside_transcript_width() {
        let item = crate::app::TranscriptItem::System {
            text: String::from(
                "commands: /compact — Summarize session history, /discard — Discard paused work",
            ),
            at: std::time::SystemTime::UNIX_EPOCH,
        };

        let lines = transcript_lines(&item, 24, false);
        assert!(lines.len() > 1, "long notice must wrap");
        assert!(lines.iter().all(|line| line.width() <= 24));
        assert_eq!(lines[0].spans[0].content, "-!- ");
        assert_eq!(lines[1].spans[0].content, "    ");
    }

    #[test]
    fn apply_annotation_overlay_styles_target_range() {
        let spans = vec![Span::styled("alpha", Style::default().fg(theme::BORDER_PICKER_RESULTS))];
        let visual = annotation_visual("find");

        let out = apply_annotation_overlay(spans, 1, 3, 0, visual);

        assert_eq!(out.len(), 3);
        assert_eq!(out[0].content, "a");
        assert_eq!(out[1].content, "lp");
        assert_eq!(out[2].content, "ha");
        assert_eq!(
            out[1].style,
            Style::default().fg(theme::BG_APP).bg(visual.bg).add_modifier(Modifier::BOLD)
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
            Style::default().bg(theme::BG_ANNOTATION).add_modifier(Modifier::UNDERLINED)
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
            save_complete: true,
            last_save_generation: 0,
            completed_save_generation: 0,
            last_save_result_generation: 0,
            last_save_succeeded: true,
            last_save_permission_denied: false,
            last_save_error_message: None,
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
            is_vlf: false,
            vlf_cache_start_line: 0,
            vlf_previous_viewport: None,
            vlf_generation: 0,
            vlf_approx_line_count: 0,
            vlf_line_count_exact: false,
            pending_vlf_tail_jump: false,
            vlf_search_ranges: Vec::new(),
        };

        assert_eq!(annotation_marker_for_line(&buf, 0), Some(('T', theme::FG_MARKER_HINT)));
    }

    #[test]
    fn status_messages_render_as_top_right_toasts_not_prompt_text() {
        let mut app = App::from_path(None).unwrap();
        app.backend.status_message = Some(String::from("saved /tmp/example.rs"));
        app.sync_status_toast();

        let width = 80;
        let height = 10;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| ui(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        let rect = toast_rect(Rect::new(0, 0, width, height), "saved /tmp/example.rs").unwrap();
        let row_text = |y| {
            (rect.x + 1..rect.right() - 1)
                .map(|x| buffer.cell((x, y)).unwrap().symbol())
                .collect::<String>()
        };
        let content_row = row_text(rect.y + 1);
        let prompt_row =
            (0..width).map(|x| buffer.cell((x, height - 1)).unwrap().symbol()).collect::<String>();

        assert!(content_row.contains("saved /tmp/example.rs"), "toast row: {content_row:?}");
        assert_eq!(buffer.cell((rect.x + 1, rect.y + 1)).unwrap().symbol(), " ");
        assert_eq!(buffer.cell((rect.right() - 2, rect.y + 1)).unwrap().symbol(), " ");
        assert!(prompt_row.trim().is_empty(), "prompt row: {prompt_row:?}");
        assert_eq!(buffer.cell((rect.x + 1, rect.y + 1)).unwrap().bg, theme::BG_CHROME);
    }

    #[test]
    fn static_mode_prompts_are_blank() {
        let mut app = App::from_path(None).unwrap();
        let modes = [
            Mode::Insert,
            Mode::Visual,
            Mode::VisualLine,
            Mode::VisualBlock,
            Mode::Picker,
            Mode::Quickfix,
            Mode::LocationList,
            Mode::OperatorPending,
            Mode::Agent,
        ];

        for mode in modes {
            app.mode = mode;
            let backend = TestBackend::new(80, 10);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| ui(frame, &app)).unwrap();
            let prompt_row = (0..80)
                .map(|x| terminal.backend().buffer().cell((x, 9)).unwrap().symbol())
                .collect::<String>();

            assert!(prompt_row.trim().is_empty(), "{mode:?} prompt: {prompt_row:?}");
        }
    }

    #[test]
    fn toast_rect_is_top_right_and_content_sized() {
        let area = Rect { x: 10, y: 4, width: 120, height: 40 };
        let rect = toast_rect(area, "saved /tmp/example.rs").expect("space for toast");

        assert_eq!(rect.x, 104);
        assert_eq!(rect.y, 5);
        assert_eq!(rect.width, 25);
        assert_eq!(rect.height, 3);
    }

    #[test]
    fn toast_hard_wraps_long_paths_and_accounts_for_horizontal_padding() {
        let area = Rect { x: 0, y: 0, width: 58, height: 20 };
        let message =
            "saved\n/home/pastel/Projects/ee/test_assets/sample-program/sample-program.rs";
        let rect = toast_rect(area, message).expect("space for toast");
        let lines = toast_content_lines(
            message,
            rect.width,
            rect.height.saturating_sub(TOAST_FRAME_HEIGHT),
        );

        assert_eq!(rect.x, 1);
        assert_eq!(rect.y, 1);
        assert_eq!(rect.width, 56);
        assert_eq!(rect.height, lines.len() as u16 + TOAST_FRAME_HEIGHT);
        assert_eq!(lines[0], "saved");
        assert_eq!(lines[1..].concat(), message.lines().nth(1).unwrap());
    }

    #[test]
    fn vlf_search_ranges_render_with_find_highlight() {
        let mut app = App::from_path(None).unwrap();
        app.backend.is_vlf = true;
        app.backend.line_cache = vec![LineSlot::Known(crate::backend::CachedLine {
            text: String::from("alpha needle omega"),
            cursors: vec![],
            syntax_spans: vec![],
        })];
        app.backend.vlf_search_ranges = vec![VlfSearchRange { line: 0, start_col: 6, end_col: 12 }];

        let width: u16 = 40;
        let height: u16 = 8;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| ui(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();

        let find_bg = theme::BG_FIND;
        let gutter_width: u16 = 5;
        let highlighted =
            (gutter_width + 6..gutter_width + 12).any(|x| buf.cell((x, 0)).unwrap().bg == find_bg);

        assert!(highlighted, "VLF search range should render using find highlight");
        assert_eq!(buf.cell((1, 0)).unwrap().symbol(), "•");
    }

    #[test]
    fn vlf_cursor_position_uses_line_cache_text() {
        let mut app = App::from_path(None).unwrap();
        app.backend.is_vlf = true;
        app.backend.cursor_line = 0;
        app.backend.cursor_col = 3;
        app.backend.line_cache = vec![LineSlot::Known(crate::backend::CachedLine {
            text: String::from("abcdef"),
            cursors: vec![],
            syntax_spans: vec![],
        })];

        let pos = cursor_position_for(
            app.backend.active(),
            app.viewport,
            &app,
            buffer_content_area(Rect { x: 5, y: 2, width: 20, height: 4 }),
            Rect { x: 0, y: 7, width: 20, height: 1 },
        );

        assert_eq!(pos, Position::new(9, 2));
    }

    #[cfg(feature = "agents")]
    #[test]
    fn plan_modal_floats_at_top_right() {
        let area = Rect { x: 10, y: 4, width: 120, height: 40 };

        let modal = plan_modal_rect(area, 2);

        assert_eq!(modal, Rect { x: 56, y: 6, width: 72, height: 6 });
    }

    #[test]
    fn rendered_spans_pad_to_viewport_width() {
        let spans = vec![Span::styled("short", Style::default().fg(Color::Green))];
        let padded = pad_spans_to_width(spans, 8, Style::default().bg(Color::Black));
        let joined = padded.iter().map(|span| span.content.as_ref()).collect::<String>();

        assert_eq!(joined, "short   ");
        assert_eq!(padded.last().unwrap().style.bg, Some(Color::Black));
    }

    #[test]
    fn rendered_spans_expand_tabs_to_spaces() {
        let spans = vec![Span::styled("ab\tcd", Style::default().fg(Color::Green))];
        let expanded = expand_tabs_in_spans(spans, 4);
        let joined = expanded.iter().map(|span| span.content.as_ref()).collect::<String>();

        assert_eq!(joined, "ab  cd");
        assert_eq!(expanded[0].style.fg, Some(Color::Green));
    }
}
