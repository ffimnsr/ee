pub(crate) use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
pub(crate) use ratatui::style::{Color, Modifier, Style};
pub(crate) use ratatui::text::{Line, Span};
pub(crate) use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap,
};
pub(crate) use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
pub(crate) use xi_core_lib::plugin_rpc::DiagnosticSeverity;

pub(crate) use crate::app::{App, Mode, SwiftMotionTarget, Viewport, smart_case_sensitive};
pub(crate) use crate::backend::{CoreAnnotation, LineSlot, VlfSearchRange};
pub(crate) use crate::buffer::BufState;
pub(crate) use crate::config::{NumberStyle, StatuslineFormat};
pub(crate) use crate::picker::PickerKind;
pub(crate) use crate::quickfix::QfList;
pub(crate) use crate::text::{byte_col_to_display_col, display_col_to_byte, wrap_text};
pub(crate) use crate::theme::ui as theme;

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
    let rendered_row = usize::from(row.saturating_sub(content_area.y));
    let line =
        app.folds.line_for_rendered_row(buf_id, vp.top_line, rendered_row, buf.line_count())?;
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

mod agents_pane;
mod annotations;
mod composer;
mod panels;
mod picker;
mod prompt;
mod render_agents;
mod spans;
mod toast;

use agents_pane::*;
use annotations::*;
use composer::*;
use panels::*;
use picker::*;
use prompt::*;
use render_agents::*;
use spans::*;
use toast::*;

pub(crate) use agents_pane::agents_transcript_scroll_max;
#[allow(unused_imports)]
pub(crate) use spans::compute_editor_width;

#[cfg(test)]
mod tests;
