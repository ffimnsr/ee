use std::collections::HashMap;
use std::collections::HashSet;

use ratatui::layout::Rect;

use crate::app::Viewport;
use crate::buffer::BufferId;

pub(crate) type WindowId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SplitDir {
    /// Windows stacked top-to-bottom (`:sp` / `Ctrl-W s`).
    Horizontal,
    /// Windows side-by-side left-to-right (`:vs` / `Ctrl-W v`).
    Vertical,
}

/// One visible pane; owns a saved viewport used when the window is inactive.
#[derive(Debug, Clone)]
pub(crate) struct Window {
    pub(crate) id: WindowId,
    pub(crate) buffer_id: BufferId,
    /// Viewport saved when this window loses focus (active window's viewport
    /// lives in `App.viewport`).
    pub(crate) saved_viewport: Viewport,
}

/// Flat-list window layout: one or more windows in a single split direction.
///
/// The active window's viewport is kept in `App.viewport` (not here) for
/// backward compatibility with existing tests.  When focus changes the old
/// window's viewport is saved here and the new window's saved_viewport is
/// swapped into `App.viewport`.
#[derive(Debug)]
pub(crate) struct WindowLayout {
    windows: Vec<Window>,
    /// Index of the currently focused window in `windows`.
    focused: usize,
    /// Direction used when splitting.
    pub(crate) split_dir: SplitDir,
    next_id: WindowId,
}

impl WindowLayout {
    pub(crate) fn new(buffer_id: BufferId) -> Self {
        let win = Window { id: 1, buffer_id, saved_viewport: Viewport::default() };
        Self { windows: vec![win], focused: 0, split_dir: SplitDir::Horizontal, next_id: 2 }
    }

    pub(crate) fn from_parts(windows: Vec<Window>, focused: usize, split_dir: SplitDir) -> Self {
        let next_id = windows.iter().map(|window| window.id).max().unwrap_or(0).saturating_add(1);
        let focused = focused.min(windows.len().saturating_sub(1));
        Self { windows, focused, split_dir, next_id }
    }

    pub(crate) fn windows(&self) -> &[Window] {
        &self.windows
    }

    pub(crate) fn focused_idx(&self) -> usize {
        self.focused
    }

    pub(crate) fn focused_window(&self) -> &Window {
        &self.windows[self.focused]
    }

    /// Split the focused window, assigning `buffer_id` to the new pane.
    /// The new window becomes focused.  `dir` overrides the layout direction.
    ///
    /// Returns the id of the new window.
    pub(crate) fn split(
        &mut self,
        dir: SplitDir,
        buffer_id: BufferId,
        active_viewport: Viewport,
    ) -> (WindowId, Viewport) {
        // Save the current active viewport into the focused window.
        self.windows[self.focused].saved_viewport = active_viewport;
        self.split_dir = dir;

        let id = self.next_id;
        self.next_id += 1;
        let win = Window { id, buffer_id, saved_viewport: Viewport::default() };
        self.windows.insert(self.focused + 1, win);
        self.focused += 1;

        // New window starts with a blank viewport.
        (id, Viewport::default())
    }

    /// Close the focused window.  Fails (returns `None`) if it would leave
    /// zero windows.  Returns the viewport that should become `App.viewport`.
    pub(crate) fn close_focused(&mut self) -> Option<Viewport> {
        if self.windows.len() <= 1 {
            return None;
        }
        self.windows.remove(self.focused);
        if self.focused >= self.windows.len() {
            self.focused = self.windows.len() - 1;
        }
        let new_vp = self.windows[self.focused].saved_viewport;
        Some(new_vp)
    }

    /// Move focus to the next window (wrapping), returning the viewport to
    /// restore into `App.viewport`.
    pub(crate) fn focus_next(&mut self, active_viewport: Viewport) -> Viewport {
        if self.windows.len() <= 1 {
            return active_viewport;
        }
        self.windows[self.focused].saved_viewport = active_viewport;
        self.focused = (self.focused + 1) % self.windows.len();
        self.windows[self.focused].saved_viewport
    }

    /// Move focus to the previous window (wrapping).
    pub(crate) fn focus_prev(&mut self, active_viewport: Viewport) -> Viewport {
        if self.windows.len() <= 1 {
            return active_viewport;
        }
        self.windows[self.focused].saved_viewport = active_viewport;
        self.focused = if self.focused == 0 { self.windows.len() - 1 } else { self.focused - 1 };
        self.windows[self.focused].saved_viewport
    }

    /// Return an iterator over `(window, rect)` pairs given the total area.
    /// Windows are distributed evenly.
    pub(crate) fn compute_rects(&self, area: Rect) -> Vec<(WindowId, Rect)> {
        let n = self.windows.len() as u16;
        if n == 0 {
            return Vec::new();
        }
        let mut result = Vec::with_capacity(self.windows.len());
        match self.split_dir {
            SplitDir::Horizontal => {
                let h = area.height / n;
                let extra = area.height % n;
                let mut y = area.y;
                for (i, win) in self.windows.iter().enumerate() {
                    let height = h + if i as u16 == n - 1 { extra } else { 0 };
                    result.push((win.id, Rect { x: area.x, y, width: area.width, height }));
                    y += height;
                }
            }
            SplitDir::Vertical => {
                let w = area.width / n;
                let extra = area.width % n;
                let mut x = area.x;
                for (i, win) in self.windows.iter().enumerate() {
                    let width = w + if i as u16 == n - 1 { extra } else { 0 };
                    result.push((win.id, Rect { x, y: area.y, width, height: area.height }));
                    x += width;
                }
            }
        }
        result
    }

    /// Returns a map of `WindowId -> (buffer_id, rect, is_focused)` for the UI.
    pub(crate) fn layout_for_area(&self, area: Rect) -> Vec<(WindowId, BufferId, Rect, bool)> {
        self.compute_rects(area)
            .into_iter()
            .map(|(id, rect)| {
                let win = self.windows.iter().find(|w| w.id == id).unwrap();
                let is_focused = self.windows[self.focused].id == id;
                (id, win.buffer_id, rect, is_focused)
            })
            .collect()
    }

    /// Update the buffer_id for a window (used when switching buffers in a
    /// given window without splitting).
    pub(crate) fn set_focused_buffer(&mut self, buffer_id: BufferId) {
        self.windows[self.focused].buffer_id = buffer_id;
    }

    pub(crate) fn retarget_invalid_buffers(
        &mut self,
        valid_buffers: &HashSet<BufferId>,
        fallback: BufferId,
    ) {
        for window in &mut self.windows {
            if !valid_buffers.contains(&window.buffer_id) {
                window.buffer_id = fallback;
                window.saved_viewport = Viewport::default();
            }
        }
    }

    /// Get the effective viewport for a window: the live `active_viewport` for
    /// the focused window, or the window's saved viewport for all others.
    pub(crate) fn viewport_for_window(
        &self,
        win_id: WindowId,
        active_viewport: crate::app::Viewport,
    ) -> crate::app::Viewport {
        let focused_id = self.windows[self.focused].id;
        if win_id == focused_id {
            active_viewport
        } else {
            self.windows
                .iter()
                .find(|w| w.id == win_id)
                .map(|w| w.saved_viewport)
                .unwrap_or_default()
        }
    }
}

// ---------------------------------------------------------------------------
// Tab page and tab manager
// ---------------------------------------------------------------------------

/// One tab page: owns a `WindowLayout` plus the saved viewport of its active
/// window.  When this tab is the active one `App.viewport` is the live value;
/// `saved_viewport` is only up-to-date while the tab is inactive.
#[derive(Debug)]
pub(crate) struct TabPage {
    pub(crate) windows: WindowLayout,
    /// Viewport preserved when this tab is not focused.
    pub(crate) saved_viewport: Viewport,
}

impl TabPage {
    fn new(buffer_id: BufferId) -> Self {
        Self { windows: WindowLayout::new(buffer_id), saved_viewport: Viewport::default() }
    }
}

/// Manages multiple tab pages.
#[derive(Debug)]
pub(crate) struct TabManager {
    tabs: Vec<TabPage>,
    /// Index of the currently active tab.
    focused: usize,
}

impl TabManager {
    pub(crate) fn new(buffer_id: BufferId) -> Self {
        Self { tabs: vec![TabPage::new(buffer_id)], focused: 0 }
    }

    pub(crate) fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    pub(crate) fn focused_idx(&self) -> usize {
        self.focused
    }

    #[allow(dead_code)]
    pub(crate) fn focused_tab(&self) -> &TabPage {
        &self.tabs[self.focused]
    }

    #[allow(dead_code)]
    pub(crate) fn focused_tab_mut(&mut self) -> &mut TabPage {
        &mut self.tabs[self.focused]
    }

    pub(crate) fn focused_windows(&self) -> &WindowLayout {
        &self.tabs[self.focused].windows
    }

    pub(crate) fn focused_windows_mut(&mut self) -> &mut WindowLayout {
        &mut self.tabs[self.focused].windows
    }

    /// Open a new tab showing `buffer_id`.  Saves `active_viewport` into the
    /// current tab, then focuses the new one.  Returns the new tab's viewport
    /// (always blank).
    pub(crate) fn new_tab(&mut self, buffer_id: BufferId, active_viewport: Viewport) -> Viewport {
        self.tabs[self.focused].saved_viewport = active_viewport;
        let page = TabPage::new(buffer_id);
        self.focused += 1;
        self.tabs.insert(self.focused, page);
        Viewport::default()
    }

    /// Close the focused tab.  Returns `Some(viewport)` with the viewport to
    /// restore if at least one tab remains; `None` if this is the only tab
    /// (caller should quit or refuse).
    pub(crate) fn close_tab(&mut self, active_viewport: Viewport) -> Option<Viewport> {
        if self.tabs.len() <= 1 {
            return None;
        }
        // Discard the current tab's active viewport (we're removing it).
        let _ = active_viewport;
        self.tabs.remove(self.focused);
        if self.focused >= self.tabs.len() {
            self.focused = self.tabs.len() - 1;
        }
        Some(self.tabs[self.focused].saved_viewport)
    }

    /// Move to the next tab (wrapping).  Returns the viewport to restore.
    pub(crate) fn focus_next(&mut self, active_viewport: Viewport) -> Viewport {
        if self.tabs.len() <= 1 {
            return active_viewport;
        }
        self.tabs[self.focused].saved_viewport = active_viewport;
        self.focused = (self.focused + 1) % self.tabs.len();
        self.tabs[self.focused].saved_viewport
    }

    /// Move to the previous tab (wrapping).  Returns the viewport to restore.
    pub(crate) fn focus_prev(&mut self, active_viewport: Viewport) -> Viewport {
        if self.tabs.len() <= 1 {
            return active_viewport;
        }
        self.tabs[self.focused].saved_viewport = active_viewport;
        self.focused = if self.focused == 0 { self.tabs.len() - 1 } else { self.focused - 1 };
        self.tabs[self.focused].saved_viewport
    }

    /// Jump to tab at 0-based `idx`.  Returns viewport to restore.
    #[allow(dead_code)]
    pub(crate) fn focus_idx(&mut self, idx: usize, active_viewport: Viewport) -> Viewport {
        if idx >= self.tabs.len() || idx == self.focused {
            return active_viewport;
        }
        self.tabs[self.focused].saved_viewport = active_viewport;
        self.focused = idx;
        self.tabs[self.focused].saved_viewport
    }

    /// Iterator over all tabs for UI rendering: yields `(index, &TabPage)`.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (usize, &TabPage)> {
        self.tabs.iter().enumerate()
    }

    pub(crate) fn replace_tabs(&mut self, tabs: Vec<TabPage>, focused: usize) {
        if tabs.is_empty() {
            return;
        }
        self.tabs = tabs;
        self.focused = focused.min(self.tabs.len().saturating_sub(1));
    }

    pub(crate) fn retarget_invalid_buffers(
        &mut self,
        valid_buffers: &HashSet<BufferId>,
        fallback: BufferId,
    ) {
        for tab in &mut self.tabs {
            tab.windows.retarget_invalid_buffers(valid_buffers, fallback);
        }
    }
}

// Keep HashMap available via the unused helper below so the import doesn't
// get removed; in practice the module only uses Vec.
#[allow(dead_code)]
fn _use_map(_: HashMap<u32, u32>) {}
