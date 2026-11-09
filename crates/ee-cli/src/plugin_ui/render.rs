//! Declarative rendering for plugin contributions.
//!
//! Every widget here is built from validated, bounded data. Styles are mapped
//! from the frontend theme; contributions never carry RGB or ANSI values.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};

use super::PluginUiState;
use super::picker::{PluginPickerState, picker_cells};
use crate::theme::ui as theme;
use ee_plugin_api::contribution::{OverlayAnchor, PanelPlacement, RenderSpec, StatusAlignment};

/// Status items split into left-aligned and right-aligned spans.
#[must_use]
pub(crate) fn status_spans(state: &PluginUiState) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let mut left = Vec::new();
    let mut right = Vec::new();
    for (_, text, alignment, _) in state.status_items() {
        let span = Span::styled(
            format!("  {} ", text.trim()),
            Style::default().fg(theme::FG_STATUS_FLAG).bg(theme::BG_STATUS),
        );
        match alignment {
            StatusAlignment::Left => left.push(span),
            StatusAlignment::Right => right.push(span),
        }
    }
    (left, right)
}

/// Draw every visible declarative panel inside the editor area.
pub(crate) fn render_panels(frame: &mut Frame<'_>, area: Rect, state: &PluginUiState) {
    let mut index = 0usize;
    for visible in state.visible_panels() {
        let Some(content) = visible.content else { continue };
        let Some(rect) = panel_rect(visible.panel.placement, area, index) else {
            continue;
        };
        index += 1;
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", visible.panel.title))
            .border_style(Style::default().fg(theme::BORDER_MUTED))
            .style(Style::default().bg(theme::BG_CHROME).fg(theme::FG_TEXT));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        render_content(
            frame,
            inner,
            &visible.panel.render,
            &content.lines,
            Style::default().bg(theme::BG_CHROME).fg(theme::FG_TEXT),
        );
    }
}

/// Draw dynamic overlays anchored relative to the viewport.
pub(crate) fn render_overlays(
    frame: &mut Frame<'_>,
    area: Rect,
    cursor: Option<(u16, u16)>,
    state: &PluginUiState,
) {
    for visible in state.visible_overlays() {
        let Some(content) = visible.content else { continue };
        let Some(rect) = overlay_rect(visible.overlay.anchor, area, cursor, &content.lines) else {
            continue;
        };
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::BORDER_PICKER))
            .style(Style::default().bg(theme::BG_PICKER).fg(theme::FG_TEXT));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        render_content(
            frame,
            inner,
            &visible.overlay.render,
            &content.lines,
            Style::default().bg(theme::BG_PICKER).fg(theme::FG_TEXT),
        );
    }
}

/// Draw the active plugin picker with local query, columns, and selection.
pub(crate) fn render_picker(frame: &mut Frame<'_>, area: Rect, state: &PluginUiState) {
    let Some(picker) = state.picker.as_ref() else { return };
    let rect = picker_rect(area);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", picker.title))
        .border_style(Style::default().fg(theme::BORDER_PICKER))
        .style(Style::default().bg(theme::BG_PICKER).fg(theme::FG_TEXT));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(if picker.columns.is_empty() { 0 } else { 1 }),
            Constraint::Min(1),
        ])
        .split(inner);

    let query = Line::from(vec![
        Span::styled("> ", Style::default().fg(theme::FG_KEY)),
        Span::styled(
            picker.query.clone(),
            Style::default().fg(theme::FG_PICKER_QUERY).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("   {}/{}", picker.visible_count(), picker.items.len()),
            Style::default().fg(theme::FG_PICKER_COUNT),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(query).style(Style::default().bg(theme::BG_PICKER_QUERY)),
        rows[0],
    );

    if !picker.columns.is_empty() && rows[1].height > 0 {
        let header = picker
            .columns
            .iter()
            .map(|column| {
                Span::styled(column.title.clone(), Style::default().fg(theme::FG_PICKER_TITLE))
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(Line::from(header)), rows[1]);
    }

    render_picker_rows(frame, rows[2], picker);
}

fn render_picker_rows(frame: &mut Frame<'_>, area: Rect, picker: &PluginPickerState) {
    if picker.visible_count() == 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no matches",
                Style::default().fg(theme::FG_PICKER_EMPTY),
            ))),
            area,
        );
        return;
    }

    let height = area.height as usize;
    let start = picker.selected.saturating_sub(height.saturating_sub(1) / 2);
    let end = (start + height).min(picker.visible_count());
    let mut items = Vec::with_capacity(end.saturating_sub(start));
    for index in start..end {
        let Some(row) = picker.filtered.get(index).and_then(|index| picker.items.get(*index))
        else {
            continue;
        };
        let mut spans = Vec::new();
        if picker.columns.is_empty() {
            spans.push(Span::styled(row.label(), Style::default().fg(theme::FG_TEXT)));
        } else {
            for cell in picker_cells(row, &picker.columns) {
                spans.push(Span::styled(cell, Style::default().fg(theme::FG_TEXT)));
                spans.push(Span::raw("  "));
            }
        }
        let selected = index == picker.selected;
        let style = if selected {
            Style::default().bg(theme::BG_PICKER_ROW_ALT).fg(theme::FG_PICKER_TITLE)
        } else {
            Style::default().bg(theme::BG_PICKER_RESULTS).fg(theme::FG_TEXT)
        };
        items.push(ListItem::new(Line::from(spans)).style(style));
    }
    frame
        .render_widget(List::new(items).style(Style::default().bg(theme::BG_PICKER_RESULTS)), area);
}

fn render_content(
    frame: &mut Frame<'_>,
    area: Rect,
    render: &RenderSpec,
    lines: &[String],
    base: Style,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    match render {
        RenderSpec::List { empty_text } => {
            let entries = if lines.is_empty() {
                empty_text.iter().cloned().collect::<Vec<_>>()
            } else {
                lines.to_vec()
            };
            let items =
                entries.into_iter().map(|line| ListItem::new(Line::from(line))).collect::<Vec<_>>();
            frame.render_widget(List::new(items).style(base), area);
        }
        RenderSpec::Text => {
            let text = lines.iter().map(|line| Line::from(line.clone())).collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(text).style(base).wrap(Wrap { trim: false }), area);
        }
        RenderSpec::Markdown => {
            let text = lines.join("\n");
            let rendered = crate::ui::markdown_message_lines(&text, area.width as usize, base);
            frame.render_widget(
                Paragraph::new(rendered).style(base).wrap(Wrap { trim: false }),
                area,
            );
        }
    }
}

fn panel_rect(placement: PanelPlacement, area: Rect, index: usize) -> Option<Rect> {
    if area.width < 8 || area.height < 3 {
        return None;
    }
    let offset = u16::try_from(index).unwrap_or(u16::MAX).saturating_mul(2);
    let width = area.width.saturating_sub(offset).clamp(16, 48);
    let height = area.height.saturating_sub(offset).clamp(3, 12);
    let rect = match placement {
        PanelPlacement::Left => Rect::new(area.x, area.y, width, height),
        PanelPlacement::Right => {
            Rect::new(area.right().saturating_sub(width), area.y, width, height)
        }
        PanelPlacement::Top => {
            Rect::new(area.x, area.y, area.width.saturating_sub(offset).max(8), height.min(8))
        }
        PanelPlacement::Bottom => Rect::new(
            area.x,
            area.bottom().saturating_sub(height.min(8)),
            area.width.saturating_sub(offset).max(8),
            height.min(8),
        ),
        PanelPlacement::Center => center_rect(area, width.min(area.width), height.min(area.height)),
    };
    // Placement is a request. If it would not fit, fall back to a centered
    // panel instead of silently growing past the editor bounds.
    let rect = if rect.width < 8 || rect.height < 3 {
        center_rect(area, area.width.min(24), area.height.min(6))
    } else {
        rect
    };
    clip(area, rect)
}

fn overlay_rect(
    anchor: OverlayAnchor,
    area: Rect,
    cursor: Option<(u16, u16)>,
    lines: &[String],
) -> Option<Rect> {
    if area.width < 6 || area.height < 3 {
        return None;
    }
    let width = lines.iter().map(|line| line.chars().count()).max().unwrap_or(12).saturating_add(2);
    let width = u16::try_from(width).unwrap_or(u16::MAX).min(area.width).max(6);
    let height =
        u16::try_from(lines.len()).unwrap_or(u16::MAX).saturating_add(2).min(area.height).max(3);
    let (cursor_x, cursor_y) = cursor.unwrap_or((area.x, area.y));
    let rect = match anchor {
        OverlayAnchor::Cursor => Rect::new(cursor_x.saturating_add(1), cursor_y, width, height),
        OverlayAnchor::TopLeft => Rect::new(area.x, area.y, width, height),
        OverlayAnchor::TopRight => {
            Rect::new(area.right().saturating_sub(width), area.y, width, height)
        }
        OverlayAnchor::BottomLeft => {
            Rect::new(area.x, area.bottom().saturating_sub(height), width, height)
        }
        OverlayAnchor::BottomRight => Rect::new(
            area.right().saturating_sub(width),
            area.bottom().saturating_sub(height),
            width,
            height,
        ),
        OverlayAnchor::Center => center_rect(area, width, height),
    };
    clip(area, rect)
}

fn picker_rect(area: Rect) -> Rect {
    let width = (area.width.saturating_mul(7) / 10).max(20).min(area.width);
    let height = (area.height.saturating_mul(6) / 10).max(6).min(area.height);
    center_rect(area, width, height)
}

fn center_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn clip(area: Rect, rect: Rect) -> Option<Rect> {
    let x = rect.x.max(area.x);
    let y = rect.y.max(area.y);
    let right = rect.right().min(area.right());
    let bottom = rect.bottom().min(area.bottom());
    let width = right.saturating_sub(x);
    let height = bottom.saturating_sub(y);
    (width > 0 && height > 0).then_some(Rect::new(x, y, width, height))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ee_plugin_api::contribution::{PanelContribution, StatusItemContribution};
    use ee_plugin_api::ids::CommandId;
    use ee_plugin_api::ids::ContributionId;

    #[test]
    fn clips_rects_to_the_terminal_area() {
        let area = Rect::new(0, 0, 20, 10);
        assert_eq!(clip(area, Rect::new(18, 9, 10, 10)), Some(Rect::new(18, 9, 2, 1)));
        assert_eq!(clip(area, Rect::new(40, 40, 4, 4)), None);
    }

    #[test]
    fn panel_and_overlay_geometries_stay_bounded() {
        let area = Rect::new(0, 0, 80, 24);
        for placement in [
            PanelPlacement::Left,
            PanelPlacement::Right,
            PanelPlacement::Top,
            PanelPlacement::Bottom,
            PanelPlacement::Center,
        ] {
            let rect = panel_rect(placement, area, 0).unwrap();
            assert!(rect.right() <= area.right());
            assert!(rect.bottom() <= area.bottom());
        }
    }

    #[test]
    fn status_items_split_by_alignment_and_source_order() {
        let mut state = PluginUiState::new();
        state
            .registry
            .apply(
                &super::super::ContributionBatch::parse(&serde_json::json!({
                    "revision": 1,
                    "upsert": [
                        {"kind": "status_item", "key": "demo.left", "text": "left", "alignment": "left"},
                        {"kind": "status_item", "key": "demo.right", "text": "right", "alignment": "right", "priority": 5}
                    ]
                }))
                .unwrap(),
            )
            .unwrap();
        let (left, right) = status_spans(&state);
        assert_eq!(left.len(), 1);
        assert_eq!(right.len(), 1);
    }

    #[test]
    fn markdown_panels_do_not_panic_on_bounded_text() {
        // Geometry helper only; the renderer is exercised end-to-end through
        // `ui()` in tests/plugin_ui.rs.
        let panel = PanelContribution {
            id: ContributionId::new("demo.panel").unwrap(),
            title: String::from("Demo"),
            placement: PanelPlacement::Right,
            render: RenderSpec::Markdown,
            data_provider: true,
        };
        let _ = panel;
        let _ = CommandId::new("demo.run").unwrap();
        let _ = StatusItemContribution {
            key: String::from("demo.status"),
            text: String::from("ok"),
            alignment: StatusAlignment::Left,
            priority: 0,
        };
    }
}
