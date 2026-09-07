//! `impl App` methods: pickers domain.
use super::*;

impl App {
    /// Drain pending location results from the backend and populate the
    /// quickfix list.  Called each frame from the main loop.
    pub(crate) fn handle_pending_ui_actions(&mut self) {
        let active_view_id = self.backend.active().view_id.clone();
        for action in self.backend.drain_pending_ui_actions() {
            match action {
                PendingUiAction::Hover { view_id, content } if view_id == active_view_id => {
                    self.hover_popup = Some(HoverPopup { title: String::from("Hover"), content });
                }
                PendingUiAction::Completions { view_id, items } if view_id == active_view_id => {
                    self.open_completion_picker(&items);
                }
                PendingUiAction::CodeActions { view_id, actions } if view_id == active_view_id => {
                    self.open_code_action_picker(&actions);
                }
                _ => {}
            }
        }
    }
    pub(crate) fn handle_pending_locations(&mut self) {
        let locations = self.backend.drain_pending_locations();
        let active_view_id = self.backend.active().view_id.clone();
        for (view_id, title, targets) in locations {
            if view_id != active_view_id {
                continue;
            }
            if targets.is_empty() {
                continue;
            }
            // Single same-file result: jump directly and skip opening quickfix.
            if targets.len() == 1 {
                let t = &targets[0];
                let same_file = self
                    .backend
                    .active()
                    .path
                    .as_ref()
                    .is_some_and(|p| p.to_string_lossy() == t.path);
                if same_file {
                    let _ = self.backend.send_edit("goto_line", json!({ "line": t.line }));
                    continue;
                }
                // Different file with one result: navigate and skip panel.
                let path = PathBuf::from(&t.path);
                let line = t.line;
                match self.backend.open_buffer(Some(path)) {
                    Ok(buf_id) => {
                        let _ = self.backend.switch_to_id(buf_id);
                        self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
                        self.viewport = Viewport::default();
                        self.jump_to_line(line);
                    }
                    Err(err) => {
                        self.backend.status_message = Some(format!("{title}: {err}"));
                    }
                }
                continue;
            }
            // Multiple results: populate quickfix and open the panel.
            let entries: Vec<QfEntry> = targets
                .iter()
                .map(|t| QfEntry {
                    path: Some(PathBuf::from(&t.path)),
                    line: t.line,
                    col: t.column,
                    message: format!("line {}", t.line + 1),
                })
                .collect();
            self.quickfix = Some(QfList::new(title, entries));
            self.quickfix_open = true;
            self.quickfix_focused = true;
        }
    }
    pub(super) fn open_completion_picker(&mut self, items: &[CompletionSuggestion]) {
        self.hover_popup = None;
        if items.is_empty() {
            self.picker = None;
            self.backend.status_message = Some(String::from("no completions"));
            return;
        }
        self.open_picker(PickerState::new_completions(items));
    }
    pub(crate) fn handle_pending_symbols(&mut self) {
        let pending = self.backend.drain_pending_symbols();
        let active_view_id = self.backend.active().view_id.clone();
        for (view_id, title, symbols) in pending {
            if view_id != active_view_id {
                continue;
            }
            if symbols.is_empty() {
                self.backend.status_message = Some(format!("{title}: no symbols found"));
                continue;
            }
            self.open_picker(PickerState::new_symbols(title, symbols));
        }
    }
    pub(super) fn open_code_action_picker(&mut self, actions: &[CodeActionDescriptor]) {
        self.hover_popup = None;
        if actions.is_empty() {
            self.picker = None;
            self.backend.status_message = Some(String::from("no code actions"));
            return;
        }
        self.open_picker(PickerState::new_code_actions(actions));
    }
    pub(crate) fn open_picker(&mut self, picker: PickerState) {
        self.last_picker = Some(picker.clone());
        self.picker = Some(picker);
    }
    pub(super) fn open_diagnostics_location_list(&mut self) {
        if !self.populate_diagnostics_location_list(0) {
            return;
        }
        self.location_list_open = true;
        self.location_list_focused = true;
    }
    /// Write crash-recovery artifacts for all modified buffers with a backing
    /// file.  Called periodically from the main loop (every ~30 s).
    pub(crate) fn write_recovery_if_due(&mut self) {
        const INTERVAL_SECS: u64 = 30;
        let now = Instant::now();
        if now.duration_since(self.recovery_last_check).as_secs() < INTERVAL_SECS {
            return;
        }
        self.recovery_last_check = now;

        for buf in self.backend.all_bufs() {
            // Skip clean buffers and scratch buffers.
            // Whole-buffer policy-allowed: crash recovery serializes full buffer to disk.
            if buf.pristine || buf.path.is_none() || buf.lines.is_empty() {
                continue;
            }
            let path = buf.path.as_ref().unwrap();
            let Some(recovery_path) = crate::buffer::recovery_file_path(path) else {
                continue;
            };
            if let Some(parent) = recovery_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let content: String = buf.lines.iter().flat_map(|l| [l.as_str(), "\n"]).collect();
            let _ = std::fs::write(&recovery_path, content);
        }
    }
    /// Confirm the currently selected picker item and close the overlay.
    pub(super) fn handle_picker_confirm(&mut self) {
        let Some(picker) = self.picker.take() else { return };
        let Some(item) = picker.selected_item().cloned() else { return };

        match picker.kind {
            crate::picker::PickerKind::Files | crate::picker::PickerKind::LiveGrep => {
                let Some(path) = item.path else { return };
                match self.backend.open_buffer(Some(path)) {
                    Ok(buf_id) => {
                        let _ = self.backend.switch_to_id(buf_id);
                        self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
                        self.viewport = Viewport::default();
                        if let Some(line) = item.line {
                            self.push_jump();
                            self.move_cursor_to(line, item.col.unwrap_or(0));
                        }
                    }
                    Err(err) => {
                        self.backend.status_message = Some(format!("open failed: {err}"));
                    }
                }
            }
            crate::picker::PickerKind::Buffers => {
                let Some(buf_id) = item.buf_id else { return };
                if self.backend.switch_to_id(buf_id).is_ok() {
                    self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
                    self.viewport = Viewport::default();
                }
            }
            crate::picker::PickerKind::Completions => {
                let Some(index) = item.choice_index else { return };
                if let Err(err) = self.backend.request_completion(Some(index)) {
                    self.backend.status_message = Some(format!("completion failed: {err}"));
                }
            }
            crate::picker::PickerKind::CodeActions => {
                let Some(index) = item.choice_index else { return };
                if let Err(err) = self.backend.request_code_actions(Some(index)) {
                    self.backend.status_message = Some(format!("code action failed: {err}"));
                }
            }
            crate::picker::PickerKind::Help => {}
            #[cfg(feature = "agents")]
            crate::picker::PickerKind::AgentThreads => {
                let Some(index) = item.choice_index else { return };
                self.focus_thread(index);
            }
            #[cfg(feature = "agents")]
            crate::picker::PickerKind::AgentServers => {
                let Some(index) = item.choice_index else { return };
                let Some(agent_id) = self.config.agents.servers.keys().nth(index).cloned() else {
                    return;
                };
                self.start_selected_agent_session(agent_id);
            }
            crate::picker::PickerKind::Symbols => {
                let Some(path) = item.path else { return };
                match self.backend.open_buffer(Some(path)) {
                    Ok(buf_id) => {
                        let _ = self.backend.switch_to_id(buf_id);
                        self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
                        self.viewport = Viewport::default();
                        if let Some(line) = item.line {
                            self.push_jump();
                            self.move_cursor_to(line, item.col.unwrap_or(0));
                        }
                    }
                    Err(err) => {
                        self.backend.status_message = Some(format!("open failed: {err}"));
                    }
                }
            }
            crate::picker::PickerKind::Locations => {
                if let Some(buf_id) = item.buf_id {
                    if self.backend.switch_to_id(buf_id).is_ok() {
                        self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
                        self.viewport = Viewport::default();
                    }
                } else if let Some(path) = item.path {
                    match self.backend.open_buffer(Some(path)) {
                        Ok(buf_id) => {
                            let _ = self.backend.switch_to_id(buf_id);
                            self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
                            self.viewport = Viewport::default();
                        }
                        Err(err) => {
                            self.backend.status_message = Some(format!("open failed: {err}"));
                            return;
                        }
                    }
                }
                if let Some(line) = item.line {
                    self.push_jump();
                    self.move_cursor_to(line, item.col.unwrap_or(0));
                }
            }
        }
    }
    pub(crate) fn scroll_into_view(&mut self, editor_height: usize, editor_width: usize) {
        if editor_height == 0 {
            return;
        }
        self.last_editor_height = editor_height;
        self.last_editor_width = editor_width;
        let cursor_line = self.backend.cursor_line;
        let buffer_id = self.backend.active().id;
        let total_lines = self.backend.line_count().max(1);
        self.viewport.top_line = self.folds.viewport_top_for_cursor(
            buffer_id,
            self.viewport.top_line,
            cursor_line,
            editor_height,
            self.config.scroll_offset,
            total_lines,
        );
        let line = self.backend.get_line(cursor_line).unwrap_or("");
        let cursor_display_col = byte_col_to_display_col(line, self.backend.cursor_col);
        self.viewport.target_col = cursor_display_col;

        // Horizontal scroll: keep cursor within the visible column range.
        // In wrap mode all content is visible at left_col=0; reset any stale offset.
        if self.config.wrap_lines {
            self.viewport.left_col = 0;
        } else if editor_width > 0 {
            if cursor_display_col < self.viewport.left_col {
                self.viewport.left_col = cursor_display_col;
            } else if cursor_display_col >= self.viewport.left_col + editor_width {
                self.viewport.left_col = cursor_display_col + 1 - editor_width;
            }
        }
    }
}
