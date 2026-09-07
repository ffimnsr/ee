//! UI: composer rendering.
use super::*;

pub(super) fn render_plan_modal(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    entries: &[(String, char)],
) {
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
pub(super) fn plan_modal_rect(area: Rect, entry_count: usize) -> Rect {
    let width = area.width.saturating_sub(4).clamp(24, 72);
    let height = (entry_count as u16 + 4).min(area.height.saturating_sub(2)).max(5);
    let x = area.x + area.width.saturating_sub(width).saturating_sub(2);
    let y = area.y.saturating_add(2);
    Rect { x, y, width, height }
}

/// Builds local-session deletion confirmation rows. Provider data is never deleted.
#[cfg(feature = "agents")]
pub(super) fn session_deletion_confirmation_composer_lines(
    app: &App,
) -> Option<(Vec<Line<'static>>, usize)> {
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
                format!("  agent: {}", confirmation.agent_id),
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
pub(super) fn additional_directory_confirmation_composer_lines(
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
pub(super) fn terminal_stop_confirmation_composer_lines(
    app: &App,
) -> Option<(Vec<Line<'static>>, usize)> {
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
pub(super) fn approval_mode_confirmation_composer_lines(
    app: &App,
) -> Option<(Vec<Line<'static>>, usize)> {
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
pub(super) fn approval_composer_lines(
    app: &App,
    width: usize,
) -> Option<(Vec<Line<'static>>, usize)> {
    let approval = app.agents.approvals.front()?;
    if let Some(preview) = approval.allow_confirmation_preview() {
        let detail_width = width.saturating_sub(2).max(4);
        let mut lines = vec![Line::from(Span::styled(
            "confirm bounded workspace allow",
            Style::default().fg(theme::FG_WARNING).add_modifier(Modifier::BOLD),
        ))];
        for (label, value) in preview.authority_fields() {
            for segment in crate::app::wrap_text(&format!("{label}: {value}"), detail_width) {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(segment, Style::default().fg(theme::FG_TEXT)),
                ]));
            }
        }
        lines.push(Line::from(Span::styled(
            "  Enter save and execute · Esc back",
            theme_style(theme::FG_DIM),
        )));
        return Some((lines, 0));
    }
    if let Some(preview) = approval.deny_confirmation_preview() {
        let detail_width = width.saturating_sub(2).max(4);
        let mut lines = vec![Line::from(Span::styled(
            "confirm workspace deny",
            Style::default().fg(theme::FG_WARNING).add_modifier(Modifier::BOLD),
        ))];
        for (label, value) in [
            ("effect".to_string(), "deny".to_string()),
            ("workspace".to_string(), preview.workspace.clone()),
            ("agent".to_string(), preview.agent.clone()),
        ]
        .into_iter()
        .chain(preview.matcher_fields.iter().cloned())
        .chain(std::iter::once(("expires".to_string(), preview.expires.clone())))
        {
            for segment in crate::app::wrap_text(&format!("{label}: {value}"), detail_width) {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(segment, Style::default().fg(theme::FG_TEXT)),
                ]));
            }
        }
        lines.push(Line::from(Span::styled(
            "  Matching future requests deny without approval UI",
            Style::default().fg(theme::FG_WARNING),
        )));
        lines.push(Line::from(Span::styled(
            "  Enter save and deny · Esc back",
            theme_style(theme::FG_DIM),
        )));
        return Some((lines, 0));
    }
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
    if let Some(mandatory) = approval.mandatory_confirmation() {
        let source = mandatory
            .template_id
            .as_deref()
            .map(|template| format!("template {template}"))
            .unwrap_or_else(|| format!("rule {}", mandatory.rule_id));
        lines.push(Line::from(Span::styled(
            format!("  mandatory confirmation: {source}"),
            Style::default().fg(theme::FG_WARNING),
        )));
        lines.push(Line::from(Span::styled(
            "  prior session, bounded, profile, and approval-mode allows cannot bypass this rule",
            theme_style(theme::FG_DIM),
        )));
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
pub(super) fn mode_selection_composer_lines(
    app: &App,
    _width: usize,
) -> Option<(Vec<Line<'static>>, usize)> {
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
pub(super) fn agents_composer_line(
    app: &App,
    thread: &crate::app::AgentThreadUi,
) -> Vec<Span<'static>> {
    if let Some(permission) = app.agents.permission() {
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
    if let Some(elicitation) = app.agents.elicitation() {
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
pub(super) fn agents_pending_composer_line(app: &App) -> Vec<Span<'static>> {
    vec![
        Span::styled("prompt> ", Style::default().fg(theme::FG_KEY).add_modifier(Modifier::BOLD)),
        Span::styled(app.agents.pending_draft.clone(), Style::default().fg(theme::FG_TEXT)),
    ]
}
