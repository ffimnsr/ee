//! `impl App` command methods: settings.
use super::*;

impl App {
    pub(super) fn apply_set_option(&mut self, opt: &str) {
        use crate::config::{NumberStyle, StatuslineFormat};

        if let Some((key, val)) = opt.split_once('=') {
            match key {
                "scrolloff" | "so" => {
                    if let Ok(n) = val.parse::<usize>() {
                        self.config.scroll_offset = n;
                    }
                }
                "colorcolumn" | "cc" => {
                    self.config.color_column = val.parse::<usize>().ok().filter(|&n| n > 0);
                }
                "statusline" | "stl" => match val {
                    "default" => self.config.statusline_format = StatuslineFormat::Default,
                    "minimal" => self.config.statusline_format = StatuslineFormat::Minimal,
                    _ => {}
                },
                "number" | "nu" | "nonu" | "nonumber" => {}
                _ => {
                    self.backend.status_message = Some(format!("unknown option: {key}"));
                    return;
                }
            }
        } else {
            match opt {
                "number" | "nu" => self.config.number_style = NumberStyle::Absolute,
                "nonumber" | "nonu" => self.config.number_style = NumberStyle::Absolute,
                "relativenumber" | "rnu" => {
                    self.config.number_style = NumberStyle::Relative;
                }
                "norelativenumber" | "nornu" => {
                    self.config.number_style = NumberStyle::Absolute;
                }
                "relativenumberabsolute" | "rnua" => {
                    self.config.number_style = NumberStyle::RelativeAbsolute;
                }
                "wrap" => self.config.wrap_lines = true,
                "nowrap" => self.config.wrap_lines = false,
                "cursorline" | "cul" => self.config.cursor_line = true,
                "nocursorline" | "nocul" => self.config.cursor_line = false,
                "list" => self.config.show_visible_whitespace = true,
                "nolist" => self.config.show_visible_whitespace = false,
                "signcolumn" | "smc" => self.config.sign_column = true,
                "nosigncolumn" | "nosmc" => self.config.sign_column = false,
                other => {
                    self.backend.status_message = Some(format!("unknown option: {other}"));
                    return;
                }
            }
        }
        self.backend.status_message = Some(format!("set: {opt}"));
    }
    pub(in crate::app) fn history_older(&mut self) {
        if self.command_history.is_empty() {
            return;
        }
        let new_idx = match self.history_idx {
            None => {
                self.history_draft = self.command_buffer.clone();
                self.command_history.len().saturating_sub(1)
            }
            Some(i) if i > 0 => i - 1,
            Some(i) => i,
        };
        self.history_idx = Some(new_idx);
        self.command_buffer = self.command_history[new_idx].clone();
    }
    pub(in crate::app) fn history_newer(&mut self) {
        let Some(idx) = self.history_idx else { return };
        if idx + 1 >= self.command_history.len() {
            self.history_idx = None;
            self.command_buffer = self.history_draft.clone();
        } else {
            let new_idx = idx + 1;
            self.history_idx = Some(new_idx);
            self.command_buffer = self.command_history[new_idx].clone();
        }
    }
    pub(in crate::app) fn complete_command(&mut self) {
        let prefix = self.command_buffer.clone();
        let candidates: Vec<&&str> =
            Self::ex_command_names().iter().filter(|c| c.starts_with(&*prefix)).collect();
        if let Some(&&first) = candidates.first() {
            self.command_buffer = first.to_owned();
        }
    }
    pub(super) fn open_help_picker(&mut self, title: &str, items: Vec<String>) {
        self.open_picker(PickerState::new_help(title, items));
        self.enter_normal_mode();
    }
    pub(super) fn log_picker_items(&self) -> Vec<crate::picker::PickerItem> {
        crate::logs::discover_log_paths()
            .into_iter()
            .filter(|candidate| candidate.path.is_file())
            .map(|candidate| crate::picker::PickerItem {
                label: candidate.label.to_owned(),
                detail: Some(candidate.path.display().to_string()),
                path: Some(candidate.path),
                buf_id: None,
                line: None,
                col: None,
                choice_index: None,
            })
            .collect()
    }
    pub(super) fn open_logs_picker(&mut self) {
        self.open_location_picker("Logs", "no logs found", self.log_picker_items());
    }
    pub(super) fn nearest_config_path(&self) -> Option<PathBuf> {
        let mut dir = self
            .backend
            .active()
            .path
            .as_deref()
            .and_then(|path| path.parent())
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));

        loop {
            let candidate = dir.join(".ee.toml");
            if candidate.is_file() {
                return Some(candidate);
            }
            if !dir.pop() {
                break;
            }
        }

        let xdg = crate::config::xi_core_config_dir().map(|dir| dir.join("config.toml"));
        if xdg.as_ref().is_some_and(|path| path.is_file()) {
            return xdg;
        }

        let legacy = dirs::home_dir().map(|home| home.join(".ee.toml"));
        if legacy.as_ref().is_some_and(|path| path.is_file()) {
            return legacy;
        }

        xdg.or(legacy)
    }
    pub(super) fn edit_nearest_config(&mut self) -> Result<(), String> {
        let path = self
            .nearest_config_path()
            .ok_or_else(|| String::from("edit_config failed: no config directory available"))?;
        self.open_path_in_current_view(path)
    }
    pub(in crate::app) fn current_buffer_language(&self) -> String {
        let buf = self.backend.active();
        self.syntax_overrides
            .get(&buf.id)
            .cloned()
            .or_else(|| {
                buf.path
                    .as_deref()
                    .and_then(xi_core_lib::tree_sitter_support::language_name_for_path)
            })
            .or_else(|| self.highlighter.syntax_name_for_path(buf.path.as_deref()))
            .unwrap_or_else(|| String::from("Plain Text"))
    }
    pub(super) fn cycle_buffer_command(&mut self, forward: bool) {
        let old = self.backend.active().id;
        if forward {
            self.backend.next_buffer();
        } else {
            self.backend.prev_buffer();
        }
        let new = self.backend.active().id;
        if old != new {
            self.tabs.focused_windows_mut().set_focused_buffer(new);
            self.viewport = Viewport::default();
        }
    }
    pub(in crate::app) fn goto_last_accessed_file(&mut self) {
        match self.backend.switch_last_accessed() {
            Ok(()) => {
                let new = self.backend.active().id;
                self.tabs.focused_windows_mut().set_focused_buffer(new);
                self.viewport = Viewport::default();
            }
            Err(err) => {
                self.backend.status_message = Some(err.to_string());
            }
        }
    }
    pub(in crate::app) fn goto_last_modified_file(&mut self) {
        match self.backend.switch_last_modified() {
            Ok(()) => {
                let new = self.backend.active().id;
                self.tabs.focused_windows_mut().set_focused_buffer(new);
                self.viewport = Viewport::default();
            }
            Err(err) => {
                self.backend.status_message = Some(err.to_string());
            }
        }
    }
    pub(in crate::app) fn goto_window_top(&mut self) {
        self.goto_window_line(WindowLineTarget::Top);
    }
    pub(in crate::app) fn goto_window_center(&mut self) {
        self.goto_window_line(WindowLineTarget::Center);
    }
    pub(in crate::app) fn goto_window_bottom(&mut self) {
        self.goto_window_line(WindowLineTarget::Bottom);
    }
    pub(super) fn goto_window_line(&mut self, target: WindowLineTarget) {
        let total_lines = self.backend.line_count().max(1);
        let visible_height = self.last_editor_height.max(1);
        let count =
            usize::try_from(self.input_state.count()).unwrap_or(usize::MAX).saturating_sub(1);
        let scrolloff = self.config.scroll_offset.min(visible_height.saturating_sub(1) / 2);
        let last_visible_line = visible_height.saturating_sub(1);
        let target_line = match target {
            WindowLineTarget::Top => self.viewport.top_line + scrolloff + count,
            WindowLineTarget::Center => self.viewport.top_line + (last_visible_line / 2),
            WindowLineTarget::Bottom => {
                self.viewport.top_line + last_visible_line.saturating_sub(scrolloff + count)
            }
        }
        .min(total_lines.saturating_sub(1));
        self.push_jump();
        self.move_cursor_to(target_line, 0);
    }
    pub(super) fn active_diagnostic_items(&self) -> Vec<(usize, QfEntry)> {
        let buf = self.backend.active();
        buf.diagnostics
            .iter()
            .map(|diagnostic| {
                // Whole-buffer policy-allowed: diagnostic offset→line/col requires full text mirror.
                let (line, col) = line_col_for_offset(&buf.lines, diagnostic.range.start);
                let severity = match diagnostic.severity {
                    xi_core_lib::plugin_rpc::DiagnosticSeverity::Error => "error",
                    xi_core_lib::plugin_rpc::DiagnosticSeverity::Warning => "warning",
                    xi_core_lib::plugin_rpc::DiagnosticSeverity::Information => "info",
                    xi_core_lib::plugin_rpc::DiagnosticSeverity::Hint => "hint",
                };
                (
                    diagnostic.range.start,
                    QfEntry {
                        path: buf.path.clone(),
                        line,
                        col,
                        message: format!("[{severity}] {}", diagnostic.message),
                    },
                )
            })
            .collect()
    }
    pub(in crate::app) fn populate_diagnostics_location_list(&mut self, selected: usize) -> bool {
        let items = self.active_diagnostic_items();
        if items.is_empty() {
            self.backend.status_message = Some(String::from("no diagnostics"));
            return false;
        }
        let entries = items.into_iter().map(|(_, entry)| entry).collect::<Vec<_>>();
        let mut list = QfList::new("Diagnostics", entries);
        let _ = list.select_one_based(selected + 1);
        self.location_list = Some(list);
        true
    }
    pub(in crate::app) fn active_cursor_offset(&self) -> usize {
        let buf = self.backend.active();
        let line = self.backend.cursor_line.min(buf.line_count().saturating_sub(1));
        // Bounded: reads only up to cursor line, not full buffer.
        let prefix = buf.line_start_offset(line).unwrap_or(0);
        let col = buf.get_line(line).map(|l| self.backend.cursor_col.min(l.len())).unwrap_or(0);
        prefix + col
    }
    pub(super) fn goto_adjacent_diagnostic(&mut self, forward: bool) {
        let items = self.active_diagnostic_items();
        if items.is_empty() {
            self.backend.status_message = Some(String::from("no diagnostics"));
            return;
        }

        let cursor_offset = self.active_cursor_offset();
        let target = if forward {
            items.iter().position(|(start, _)| *start > cursor_offset)
        } else {
            items.iter().rposition(|(start, _)| *start < cursor_offset)
        };

        let Some(selected) = target else {
            self.backend.status_message = Some(if forward {
                String::from("no next diagnostic")
            } else {
                String::from("no previous diagnostic")
            });
            return;
        };

        let entry = items[selected].1.clone();
        let _ = self.populate_diagnostics_location_list(selected);
        self.move_cursor_to(entry.line, entry.col);
    }
    pub(super) fn goto_edge_diagnostic(&mut self, first: bool) {
        let items = self.active_diagnostic_items();
        if items.is_empty() {
            self.backend.status_message = Some(String::from("no diagnostics"));
            return;
        }

        let selected = if first { 0 } else { items.len().saturating_sub(1) };
        let entry = items[selected].1.clone();
        let _ = self.populate_diagnostics_location_list(selected);
        self.move_cursor_to(entry.line, entry.col);
    }
}
