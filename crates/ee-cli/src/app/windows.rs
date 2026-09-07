//! `impl App` methods: windows domain.
use super::*;

impl App {
    pub(super) fn sync_backend_to_focused_window(&mut self) {
        let new_buf = self.tabs.focused_windows().focused_window().buffer_id;
        let _ = self.backend.switch_to_id(new_buf);
    }
    pub(super) fn rotate_view(&mut self) {
        let new_vp = self.tabs.focused_windows_mut().focus_next(self.viewport);
        self.viewport = new_vp;
        self.sync_backend_to_focused_window();
    }
    pub(super) fn rotate_view_reverse(&mut self) {
        let new_vp = self.tabs.focused_windows_mut().focus_prev(self.viewport);
        self.viewport = new_vp;
        self.sync_backend_to_focused_window();
    }
    pub(super) fn transpose_view(&mut self) {
        self.tabs.focused_windows_mut().transpose();
    }
    pub(super) fn jump_view(&mut self, direction: ViewDirection) {
        let new_vp = self.tabs.focused_windows_mut().focus_direction(direction, self.viewport);
        self.viewport = new_vp;
        self.sync_backend_to_focused_window();
    }
    pub(super) fn swap_view(&mut self, direction: ViewDirection) {
        if self.tabs.focused_windows_mut().swap_focused_with_direction(direction) {
            self.sync_backend_to_focused_window();
        }
    }
    pub(super) fn close_view(&mut self) {
        if let Some(new_vp) = self.tabs.focused_windows_mut().close_focused() {
            self.viewport = new_vp;
            self.sync_backend_to_focused_window();
        }
    }
    pub(super) fn close_other_views(&mut self) {
        let new_vp = self.tabs.focused_windows_mut().close_others(self.viewport);
        self.viewport = new_vp;
        self.sync_backend_to_focused_window();
    }
    /// Handle a key pressed after `Ctrl-W` in Normal mode.
    pub(super) fn handle_window_cmd(&mut self, c: char) {
        match c {
            // Horizontal split (same buffer).
            's' => {
                let buf_id = self.backend.active().id;
                let (_, new_vp) = self.tabs.focused_windows_mut().split(
                    SplitDir::Horizontal,
                    buf_id,
                    self.viewport,
                );
                self.viewport = new_vp;
            }
            // Vertical split (same buffer).
            'v' => {
                let buf_id = self.backend.active().id;
                let (_, new_vp) = self.tabs.focused_windows_mut().split(
                    SplitDir::Vertical,
                    buf_id,
                    self.viewport,
                );
                self.viewport = new_vp;
            }
            // Focus next window.
            'w' => self.rotate_view(),
            // Focus previous window.
            'W' | 'p' => self.rotate_view_reverse(),
            'h' => self.jump_view(ViewDirection::Left),
            'j' => self.jump_view(ViewDirection::Down),
            'k' => self.jump_view(ViewDirection::Up),
            'l' => self.jump_view(ViewDirection::Right),
            't' => self.transpose_view(),
            'H' => self.swap_view(ViewDirection::Left),
            'J' => self.swap_view(ViewDirection::Down),
            'K' => self.swap_view(ViewDirection::Up),
            'L' => self.swap_view(ViewDirection::Right),
            // Close focused window.
            'c' | 'q' => self.close_view(),
            'o' => self.close_other_views(),
            _ => {}
        }
    }
}
