//! UI: render_agents rendering.
use super::*;

pub(super) fn render_agents_pane(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
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
            ThreadUiState::Queued => "queued".into(),
            ThreadUiState::Running => "running".into(),
            ThreadUiState::AwaitingPermission => "awaiting permission".into(),
            ThreadUiState::AwaitingElicitation => "awaiting elicitation".into(),
            ThreadUiState::Cancelling => "cancelling".into(),
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
        let connection_active = app
            .agents
            .threads
            .iter()
            .filter(|candidate| {
                candidate.agent_id == thread.agent_id
                    && matches!(
                        candidate.state,
                        ThreadUiState::Running
                            | ThreadUiState::AwaitingPermission
                            | ThreadUiState::AwaitingElicitation
                            | ThreadUiState::Cancelling
                    )
            })
            .count();
        let connection_queued = app
            .agents
            .threads
            .iter()
            .filter(|candidate| {
                candidate.agent_id == thread.agent_id && candidate.state == ThreadUiState::Queued
            })
            .count();
        let connection = if connection_queued == 0 {
            format!("{connection_active}/{}", app.config.agents.max_concurrent_prompts)
        } else {
            format!(
                "{connection_active}/{} +{connection_queued} queued",
                app.config.agents.max_concurrent_prompts
            )
        };
        let footer_text = format!(
            "{} [{}] | agent:{} | conn:{} | mode:{} | session:{} / {} | thoughts:{} | unread:{} | last:{}",
            thread.nick,
            state_label,
            thread.agent_id,
            connection,
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
        let footer_style = if matches!(
            thread.state,
            ThreadUiState::Queued
                | ThreadUiState::Running
                | ThreadUiState::AwaitingPermission
                | ThreadUiState::AwaitingElicitation
                | ThreadUiState::Cancelling
        ) {
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
        if !app.agents.pending_sessions.is_empty() {
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
