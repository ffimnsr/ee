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

/// Renders the floating prompt editor over the agents pane. The draft is
/// edited live; Esc cancels (restoring the snapshot), Insert accepts.
/// Lines are hard-wrapped to the modal width so long lines (including
/// pasted content) stay inside the modal and the caret maps exactly onto the
/// rendered columns.
#[cfg(feature = "agents")]
pub(super) fn render_prompt_editor(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    thread: &crate::app::AgentThreadUi,
) {
    let modal = prompt_editor_rect(area);
    frame.render_widget(Clear, modal);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" prompt editor — Enter: newline · Insert: accept · Esc: cancel ")
        .border_style(Style::default().fg(theme::FG_KEY))
        .style(Style::default().fg(theme::FG_TEXT).bg(theme::BG_CHROME));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let draft = &thread.draft;
    let width = inner.width as usize;
    let visible = inner.height as usize;

    // Hard-wrap every logical line; ratatui truncates unwrapped lines at the
    // modal edge, which pushed the caret and typed text outside the modal.
    let wrapped: Vec<Vec<String>> =
        draft.split('\n').map(|line| crate::app::wrap_text_hard(line, width)).collect();
    let total_rows = wrapped.iter().map(|rows| rows.len()).sum::<usize>();

    // Caret position in wrapped rows/columns (byte offset is always a char
    // boundary: it comes from the draft caret).
    let byte = thread.draft_byte_at_cursor().min(draft.len());
    let (caret_line, caret_col) = crate::app::wrapped_caret_position(draft, byte, width);

    // Scroll only once the caret leaves the modal, so the view stays put
    // while the caret moves inside the wrapped window (paste lands the caret
    // at the end of the inserted text and the view follows it).
    let scroll = caret_line
        .saturating_sub(visible.saturating_sub(1))
        .min(total_rows.saturating_sub(visible));

    let mut lines = Vec::with_capacity(total_rows);
    for rows in wrapped {
        for row in rows {
            lines.push(Line::from(Span::styled(row, Style::default().fg(theme::FG_TEXT))));
        }
    }
    frame.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), inner);
    frame.set_cursor_position(ratatui::layout::Position {
        x: inner.x.saturating_add(caret_col as u16).min(inner.right().saturating_sub(1)),
        y: inner.y.saturating_add((caret_line - scroll) as u16),
    });
}

#[cfg(feature = "agents")]
pub(super) fn prompt_editor_rect(area: Rect) -> Rect {
    let width = (area.width.saturating_mul(4) / 5).clamp(40, 120);
    let height = (area.height.saturating_mul(3) / 5).clamp(8, 40);
    let x = area.x + area.width.saturating_sub(width).saturating_sub(2) / 2;
    let y = area.y + area.height.saturating_sub(height).saturating_sub(4) / 2;
    Rect { x, y, width, height }
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
    composer_width: usize,
) -> Vec<Span<'static>> {
    if let Some(permission) = app.agents.permission() {
        let count = permission.options.len();
        let selected = permission.selected.min(count.saturating_sub(1));
        let option = permission
            .options
            .get(selected)
            .map(|option| option.name.clone())
            .unwrap_or_else(|| String::from("(none)"));
        let mut spans = vec![
            Span::styled("> ", Style::default().fg(theme::FG_KEY)),
            Span::styled(
                format!("[{}/{}] ", selected + 1, count),
                Style::default().fg(theme::FG_WARNING),
            ),
            Span::styled(option, Style::default().fg(theme::FG_TEXT)),
        ];
        if let Some(marker) = permission.preview.composer_marker() {
            // The full validated summary (or heuristic note) renders in the
            // transcript line above.
            spans.push(Span::styled(format!(" {marker}"), Style::default().fg(theme::FG_KEY)));
        }
        spans.push(Span::styled(
            " (Enter confirm, ←/→ change, Esc back)",
            theme_style(theme::FG_DIM),
        ));
        return spans;
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
    let command_hint = agents_command_hint(draft);
    let snippet = agents_composer_draft(draft, composer_width, command_hint);
    let mut spans = vec![
        Span::styled("prompt> ", Style::default().fg(theme::FG_KEY).add_modifier(Modifier::BOLD)),
        Span::styled(
            snippet.clone().unwrap_or_else(|| draft.to_owned()),
            Style::default().fg(theme::FG_TEXT),
        ),
    ];
    if snippet.is_some() {
        spans.push(Span::styled(DRAFT_INSERT_HINT, theme_style(theme::FG_DIM)));
    }
    if command_hint {
        spans.push(Span::styled(DRAFT_HINT, theme_style(theme::FG_DIM)));
    }
    spans
}

/// Fixed composer chrome before the draft text: the `prompt> ` marker
/// (keep in sync with the marker literals in this module).
pub(super) const DRAFT_PREFIX_WIDTH: usize = 8;

/// Tab-complete hint appended after the draft when it starts with a slash
/// command; its width is reserved before eliding the draft.
const DRAFT_HINT: &str = "  Tab complete · /help";

/// Hint appended after the elided draft telling the user that INSERT opens
/// the full prompt editor; its width is reserved while the draft elides so
/// the hint text always renders fully.
pub(super) const DRAFT_INSERT_HINT: &str = " [press INSERT key]";

/// True while the draft looks like a slash command (a single token after `/`).
pub(super) fn agents_command_hint(draft: &str) -> bool {
    draft
        .trim_start()
        .strip_prefix('/')
        .is_some_and(|prefix| !prefix.chars().any(char::is_whitespace))
}

/// Columns of the single-line composer available for the draft text after
/// reserving the prompt marker and the optional tab hint. Floors at a small
/// value so an extremely narrow pane never drops the draft entirely.
pub(super) fn agents_draft_limit(composer_width: usize, command_hint: bool) -> usize {
    let reserved = DRAFT_PREFIX_WIDTH + if command_hint { DRAFT_HINT.width() } else { 0 };
    composer_width.saturating_sub(reserved + 1).max(4)
}

/// Snippet form of a draft that does not fit the single-line composer: the
/// first line elided by display width so the trailing `…` stays visible.
/// `None` means the draft fits the composer line as-is — elision only kicks
/// in once the draft actually exceeds the column count handed in by
/// [`agents_draft_limit`]. Multiline drafts always show the line-count marker.
pub(super) fn agents_draft_snippet(draft: &str, max_width: usize) -> Option<String> {
    let first_line = draft.split('\n').next().unwrap_or_default();
    let multiline = draft.contains('\n');
    let suffix = if multiline {
        format!(" \u{2026} ({} lines)", draft.split('\n').count())
    } else {
        String::from(" \u{2026}")
    };
    let room = max_width.saturating_sub(suffix.width());
    if !multiline && first_line.width() <= room {
        return None;
    }
    let mut snippet = truncate_by_width(first_line, room).to_string();
    snippet.push_str(&suffix);
    Some(snippet)
}

/// Draft text for the single-line composer: the full draft when it fits the
/// composer width, or the elided snippet with the `press INSERT` hint budget
/// reserved. `None` means the draft fits and no hints are shown.
pub(super) fn agents_composer_draft(
    draft: &str,
    composer_width: usize,
    command_hint: bool,
) -> Option<String> {
    let budget = agents_draft_limit(composer_width, command_hint);
    // Full draft fits the composer: no elision and no hints.
    agents_draft_snippet(draft, budget)?;
    agents_draft_snippet(draft, budget.saturating_sub(DRAFT_INSERT_HINT.width()))
}

/// Longest prefix of `text` whose display width fits `width`; whole
/// characters only, so wide (CJK) glyphs are never split.
fn truncate_by_width(text: &str, width: usize) -> &str {
    let mut used = 0usize;
    for (index, ch) in text.char_indices() {
        let mut encoded = [0u8; 4];
        let ch_width = UnicodeWidthStr::width(ch.encode_utf8(&mut encoded));
        if used + ch_width > width {
            return &text[..index];
        }
        used += ch_width;
    }
    text
}

/// Builds the composer line shown while no session exists yet.
#[cfg(feature = "agents")]
pub(super) fn agents_pending_composer_line(app: &App) -> Vec<Span<'static>> {
    vec![
        Span::styled("prompt> ", Style::default().fg(theme::FG_KEY).add_modifier(Modifier::BOLD)),
        Span::styled(app.agents.pending_draft.clone(), Style::default().fg(theme::FG_TEXT)),
    ]
}
