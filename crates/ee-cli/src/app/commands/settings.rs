//! `impl App` command methods: settings.
use super::*;

impl App {
    /// Apply `:set` arguments (vim-style, runtime-only).  Each token is
    /// `name=value`, `name?` (query), a bare flag (`name` / `noname`), or
    /// `name value` with the next token consumed when it looks like a value
    /// (e.g. `:set wrap_lines true`).  Mutates the in-memory config only.
    pub(in crate::app) fn apply_set_args(&mut self, args: &[&str]) {
        if args.is_empty() {
            self.backend.status_message = Some(Self::changed_options_summary(self));
            return;
        }
        let mut index = 0;
        while index < args.len() {
            let token = args[index];
            if let Some((name, value)) = token.split_once('=') {
                self.set_runtime_option(name, Some(value));
                index += 1;
                continue;
            }
            // Value options always take the next token as their value.
            if is_value_option(token) {
                let value = args.get(index + 1).copied();
                self.set_runtime_option(token, value);
                index += if value.is_some() { 2 } else { 1 };
                continue;
            }
            // Bool options consume the next token only when it looks like a
            // value (`:set wrap_lines true`); otherwise it is a separate flag
            // (`:set nu list`) or a stray unknown (`:set wrap maybe` → E518).
            if let Some(value) = args.get(index + 1).copied().filter(|v| is_plausible_value(v)) {
                self.set_runtime_option(token, Some(value));
                index += 2;
                continue;
            }
            self.set_runtime_option(token, None);
            index += 1;
        }
    }
    fn set_runtime_option(&mut self, raw: &str, value: Option<&str>) {
        use crate::config::{IndentStyle, NumberStyle, StatuslineFormat};

        if let Some(name) = raw.strip_suffix('?') {
            // `no`-prefixed queries (`:set nowrap?`) read the plain option.
            let name = name.strip_prefix("no").unwrap_or(name);
            match self.runtime_option_value(name) {
                Some(current) => self.backend.status_message = Some(format!("{name}={current}")),
                None => self.backend.status_message = Some(format!("unknown option: {name}")),
            }
            return;
        }

        let (negate, name) = raw.strip_prefix("no").map_or((false, raw), |rest| (true, rest));
        let value = match (value, negate) {
            (Some(v), _) => v,
            (None, true) => "false",
            (None, false) => "true",
        };

        let result = match name {
            // Bool options (vim names + ee config names).
            "wrap" | "wrap_lines" => set_bool_value(&mut self.config.wrap_lines, value),
            "cursorline" | "cul" | "cursor_line" => {
                set_bool_value(&mut self.config.cursor_line, value)
            }
            "list" | "show_visible_whitespace" => {
                set_bool_value(&mut self.config.show_visible_whitespace, value)
            }
            "signcolumn" | "smc" | "sign_column" => {
                set_bool_value(&mut self.config.sign_column, value)
            }
            "autoindent" | "auto_indent" => set_bool_value(&mut self.config.auto_indent, value),
            "smartindent" | "smart_indent" => set_bool_value(&mut self.config.smart_indent, value),
            "trimtrailingwhitespace" | "trim_trailing_whitespace" => {
                set_bool_value(&mut self.config.trim_trailing_whitespace, value)
            }
            "insertfinalnewline" | "insert_final_newline" => {
                set_bool_value(&mut self.config.insert_final_newline, value)
            }
            "number" | "nu" => parse_bool_value(value).map(|on| {
                self.config.number_style =
                    if on { NumberStyle::Absolute } else { NumberStyle::None };
            }),
            "relativenumber" | "rnu" => parse_bool_value(value).map(|on| {
                self.config.number_style =
                    if on { NumberStyle::Relative } else { NumberStyle::None };
            }),
            "relativenumberabsolute" | "rnua" => parse_bool_value(value).map(|on| {
                self.config.number_style =
                    if on { NumberStyle::RelativeAbsolute } else { NumberStyle::None };
            }),
            // Number options.
            "scrolloff" | "so" => {
                value.parse::<usize>().map(|n| self.config.scroll_offset = n).map_err(|_| ())
            }
            "colorcolumn" | "cc" => value
                .parse::<usize>()
                .map(|n| self.config.color_column = (n > 0).then_some(n))
                .map_err(|_| ()),
            "tabwidth" | "tab_width" | "tabstop" | "ts" => {
                value.parse::<usize>().map(|n| self.config.tab_width = n).map_err(|_| ())
            }
            "shiftwidth" | "indent_size" | "sw" => {
                value.parse::<usize>().map(|n| self.config.indent_size = n).map_err(|_| ())
            }
            // String / enum options.
            "indentstyle" | "indent_style" => match value {
                "spaces" => {
                    self.config.indent_style = IndentStyle::Spaces;
                    Ok(())
                }
                "tabs" => {
                    self.config.indent_style = IndentStyle::Tabs;
                    Ok(())
                }
                _ => Err(()),
            },
            "endofline" | "end_of_line" | "eol" => match value {
                "lf" => {
                    self.config.end_of_line = crate::config::EndOfLine::Lf;
                    Ok(())
                }
                "crlf" => {
                    self.config.end_of_line = crate::config::EndOfLine::CrLf;
                    Ok(())
                }
                "cr" => {
                    self.config.end_of_line = crate::config::EndOfLine::Cr;
                    Ok(())
                }
                _ => Err(()),
            },
            "charset" => {
                self.config.charset = value.to_owned();
                Ok(())
            }
            "statusline" | "stl" => match value {
                "default" => {
                    self.config.statusline_format = StatuslineFormat::Default;
                    Ok(())
                }
                "minimal" => {
                    self.config.statusline_format = StatuslineFormat::Minimal;
                    Ok(())
                }
                _ => Err(()),
            },
            _ => {
                self.backend.status_message = Some(format!("unknown option: {name}"));
                return;
            }
        };

        match result {
            Ok(()) => {
                // Session override reaches the backend (word_wrap, tab_size,
                // line_ending, …) so core matches the runtime config; the
                // wrap width also needs the current viewport size.
                let _ = self.backend.push_runtime_editor_config(&self.config);
                self.sync_backend_viewport_size();
                self.backend.status_message = Some(format!("set: {name}={value}"));
            }
            Err(()) => {
                self.backend.status_message =
                    Some(format!("set: invalid value {value:?} for {name}"))
            }
        }
    }
    /// Current value of a settable runtime option, for `:set name?` queries
    /// and the `:set` summary.
    fn runtime_option_value(&self, name: &str) -> Option<String> {
        use crate::config::NumberStyle;
        let value = match name {
            "wrap" | "wrap_lines" => self.config.wrap_lines.to_string(),
            "cursorline" | "cul" | "cursor_line" => self.config.cursor_line.to_string(),
            "list" | "show_visible_whitespace" => self.config.show_visible_whitespace.to_string(),
            "signcolumn" | "smc" | "sign_column" => self.config.sign_column.to_string(),
            "autoindent" | "auto_indent" => self.config.auto_indent.to_string(),
            "smartindent" | "smart_indent" => self.config.smart_indent.to_string(),
            "trimtrailingwhitespace" | "trim_trailing_whitespace" => {
                self.config.trim_trailing_whitespace.to_string()
            }
            "insertfinalnewline" | "insert_final_newline" => {
                self.config.insert_final_newline.to_string()
            }
            "number" | "nu" => {
                matches!(self.config.number_style, NumberStyle::Absolute).to_string()
            }
            "relativenumber" | "rnu" => {
                matches!(self.config.number_style, NumberStyle::Relative).to_string()
            }
            "relativenumberabsolute" | "rnua" => {
                matches!(self.config.number_style, NumberStyle::RelativeAbsolute).to_string()
            }
            "scrolloff" | "so" => self.config.scroll_offset.to_string(),
            "colorcolumn" | "cc" => {
                self.config.color_column.map(|n| n.to_string()).unwrap_or_else(|| "0".to_owned())
            }
            "tabwidth" | "tab_width" | "tabstop" | "ts" => self.config.tab_width.to_string(),
            "shiftwidth" | "indent_size" | "sw" => self.config.indent_size.to_string(),
            "indentstyle" | "indent_style" => match self.config.indent_style {
                crate::config::IndentStyle::Spaces => "spaces",
                crate::config::IndentStyle::Tabs => "tabs",
            }
            .to_owned(),
            "endofline" | "end_of_line" | "eol" => match self.config.end_of_line {
                crate::config::EndOfLine::Lf => "lf",
                crate::config::EndOfLine::CrLf => "crlf",
                crate::config::EndOfLine::Cr => "cr",
            }
            .to_owned(),
            "charset" => self.config.charset.clone(),
            "statusline" | "stl" => match self.config.statusline_format {
                crate::config::StatuslineFormat::Default => "default",
                crate::config::StatuslineFormat::Minimal => "minimal",
            }
            .to_owned(),
            _ => return None,
        };
        Some(value)
    }
    /// `:set` with no args: vim lists options that differ from defaults; we
    /// do the same over the runtime-settable set.
    fn changed_options_summary(app: &App) -> String {
        const OPTIONS: &[&str] = &[
            "wrap",
            "cursorline",
            "list",
            "signcolumn",
            "autoindent",
            "smartindent",
            "trimtrailingwhitespace",
            "insertfinalnewline",
            "number",
            "relativenumber",
            "relativenumberabsolute",
            "scrolloff",
            "colorcolumn",
            "tabwidth",
            "shiftwidth",
            "indentstyle",
            "endofline",
            "charset",
            "statusline",
        ];
        let defaults = crate::config::EditorSettings::default();
        let changed: Vec<String> = OPTIONS
            .iter()
            .filter_map(|name| {
                let current = app.runtime_option_value(name)?;
                let default = App::default_option_value(&defaults, name)?;
                (current != default).then(|| format!("{name}={current}"))
            })
            .collect();
        if changed.is_empty() {
            String::from("set: all options at defaults")
        } else {
            format!("set: {}", changed.join(" "))
        }
    }
    fn default_option_value(
        defaults: &crate::config::EditorSettings,
        name: &str,
    ) -> Option<String> {
        use crate::config::IndentStyle;
        let value = match name {
            "wrap" => defaults.wrap_lines.to_string(),
            "cursorline" => defaults.cursor_line.to_string(),
            "list" => defaults.show_visible_whitespace.to_string(),
            "signcolumn" => defaults.sign_column.to_string(),
            "autoindent" => defaults.auto_indent.to_string(),
            "smartindent" => defaults.smart_indent.to_string(),
            "trimtrailingwhitespace" => defaults.trim_trailing_whitespace.to_string(),
            "insertfinalnewline" => defaults.insert_final_newline.to_string(),
            "number" => {
                matches!(defaults.number_style, crate::config::NumberStyle::Absolute).to_string()
            }
            "relativenumber" => {
                matches!(defaults.number_style, crate::config::NumberStyle::Relative).to_string()
            }
            "relativenumberabsolute" => {
                matches!(defaults.number_style, crate::config::NumberStyle::RelativeAbsolute)
                    .to_string()
            }
            "scrolloff" => defaults.scroll_offset.to_string(),
            "colorcolumn" => {
                defaults.color_column.map(|n| n.to_string()).unwrap_or_else(|| "0".to_owned())
            }
            "tabwidth" => defaults.tab_width.to_string(),
            "shiftwidth" => defaults.indent_size.to_string(),
            "indentstyle" => match defaults.indent_style {
                IndentStyle::Spaces => "spaces",
                IndentStyle::Tabs => "tabs",
            }
            .to_owned(),
            "endofline" => match defaults.end_of_line {
                crate::config::EndOfLine::Lf => "lf",
                crate::config::EndOfLine::CrLf => "crlf",
                crate::config::EndOfLine::Cr => "cr",
            }
            .to_owned(),
            "charset" => defaults.charset.clone(),
            "statusline" => match defaults.statusline_format {
                crate::config::StatuslineFormat::Default => "default",
                crate::config::StatuslineFormat::Minimal => "minimal",
            }
            .to_owned(),
            _ => return None,
        };
        Some(value)
    }
    pub(in crate::app) fn history_older(&mut self) {
        if self.command_history.is_empty() {
            return;
        }
        self.reset_command_completion();
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
        self.reset_command_completion();
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

        // Session in flight → rotate to the next candidate (vim: repeated Tab
        // cycles through the matches).  Any edit since the last Tab falls out
        // to a fresh match below.
        if let Some(state) = self.command_completion.take()
            && self.command_buffer == state.shown
        {
            let next = if state.at_lcp { 0 } else { (state.index + 1) % state.candidates.len() };
            self.command_buffer = state.candidates[next].clone();
            self.command_completion = Some(CommandCompletion {
                candidates: state.candidates,
                index: next,
                at_lcp: false,
                shown: self.command_buffer.clone(),
            });
            self.history_idx = None;
            return;
        }

        let candidates: Vec<String> = Self::ex_command_names()
            .iter()
            .filter(|name| name.starts_with(&prefix))
            .map(|name| name.to_string())
            .collect();
        if candidates.is_empty() {
            return;
        }
        // First Tab completes to the longest common prefix; further Tabs cycle.
        let common = longest_common_prefix(&candidates);
        let at_lcp = common.len() > prefix.len();
        let shown = if at_lcp { common } else { candidates[0].clone() };
        self.command_buffer = shown.clone();
        self.command_completion = Some(CommandCompletion { candidates, index: 0, at_lcp, shown });
        self.history_idx = None;
    }
    /// Any manual edit of the command buffer invalidates an active completion
    /// session; the next Tab must start fresh.
    pub(in crate::app) fn reset_command_completion(&mut self) {
        self.command_completion = None;
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

/// Longest common prefix of the given (non-empty) command names, byte-based
/// (command names are ASCII).
fn longest_common_prefix(candidates: &[String]) -> String {
    let Some(first) = candidates.first() else { return String::new() };
    let mut common = first.clone();
    for candidate in &candidates[1..] {
        while !candidate.starts_with(&common) {
            common.pop();
        }
        if common.is_empty() {
            break;
        }
    }
    common
}

/// `true`/`false`-style literal for `:set` values.
fn parse_bool_value(value: &str) -> Result<bool, ()> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" | "1" => Ok(true),
        "false" | "off" | "no" | "0" => Ok(false),
        _ => Err(()),
    }
}

/// Whether a bare token after an option name should be consumed as its value
/// (`:set wrap_lines true`).  Keeps bare flag lists working (`:set nu list`).
fn is_plausible_value(value: &str) -> bool {
    parse_bool_value(value).is_ok()
        || value.parse::<usize>().is_ok()
        || matches!(value, "spaces" | "tabs" | "lf" | "crlf" | "cr" | "default" | "minimal")
}

/// Value-taking options: the next token is always their value.
fn is_value_option(name: &str) -> bool {
    matches!(
        name,
        "scrolloff"
            | "so"
            | "colorcolumn"
            | "cc"
            | "tabwidth"
            | "tab_width"
            | "tabstop"
            | "ts"
            | "shiftwidth"
            | "indent_size"
            | "sw"
            | "indentstyle"
            | "indent_style"
            | "endofline"
            | "end_of_line"
            | "eol"
            | "charset"
            | "statusline"
            | "stl"
    )
}

/// Apply a parsed value to a bool option.
fn set_bool_value(slot: &mut bool, value: &str) -> Result<(), ()> {
    let parsed = parse_bool_value(value)?;
    *slot = parsed;
    Ok(())
}
