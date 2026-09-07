//! UI: toast rendering.
use super::*;

pub(super) const TOAST_MAX_WIDTH: u16 = 72;
pub(super) const TOAST_MIN_WIDTH: u16 = 24;
pub(super) const TOAST_MAX_CONTENT_HEIGHT: u16 = 6;
pub(super) const TOAST_MARGIN: u16 = 1;
pub(super) const TOAST_FRAME_WIDTH: u16 = 4;
pub(super) const TOAST_FRAME_HEIGHT: u16 = 2;

pub(super) fn toast_content_lines(message: &str, width: u16, max_height: u16) -> Vec<String> {
    let content_width = usize::from(width.saturating_sub(TOAST_FRAME_WIDTH)).max(4);
    let mut lines = wrap_text(message, content_width);
    lines.truncate(usize::from(max_height));
    lines
}

pub(super) fn toast_rect(area: Rect, message: &str) -> Option<Rect> {
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

pub(super) fn render_toast(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
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

pub(super) fn render_hover_popup(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
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

pub(super) fn diagnostic_marker_for_line(
    buf: &BufState,
    line_index: usize,
) -> Option<DiagnosticSeverity> {
    let (line_start, line_end) = line_byte_range(&buf.lines, line_index)?;
    buf.diagnostics
        .iter()
        .filter_map(|diagnostic| {
            let overlaps = diagnostic.range.start <= line_end && diagnostic.range.end >= line_start;
            overlaps.then_some(diagnostic.severity.clone())
        })
        .min_by_key(diagnostic_rank)
}

pub(super) fn line_byte_range(lines: &[String], target_line: usize) -> Option<(usize, usize)> {
    if target_line >= lines.len() {
        return None;
    }
    let start = lines.iter().take(target_line).fold(0usize, |acc, line| acc + line.len() + 1);
    let end = start + lines[target_line].len();
    Some((start, end))
}

pub(super) fn diagnostic_rank(severity: &DiagnosticSeverity) -> u8 {
    match severity {
        DiagnosticSeverity::Error => 0,
        DiagnosticSeverity::Warning => 1,
        DiagnosticSeverity::Information => 2,
        DiagnosticSeverity::Hint => 3,
    }
}
