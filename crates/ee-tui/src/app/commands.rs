use super::*;

impl App {
    pub(super) fn execute_command(&mut self) {
        let raw = self.command_buffer.trim().to_owned();

        // Push non-empty commands to history (deduplicate consecutive duplicates).
        if !raw.is_empty() && self.command_history.last().map(|s| s.as_str()) != Some(&raw) {
            self.command_history.push(raw.clone());
            const HISTORY_MAX: usize = 100;
            if self.command_history.len() > HISTORY_MAX {
                self.command_history.remove(0);
            }
        }
        self.history_idx = None;

        // Parse an optional line-address range from the front of the command.
        let cursor_line = self.backend.cursor_line;
        let line_count = self.backend.lines.len().max(1);
        let (range, rest) = parse_ex_range(&raw, cursor_line, line_count, &self.marks);
        let command = rest.trim_start();

        match crate::terminal::parse_command(command) {
            Ok(Some(shell_command)) => {
                self.run_terminal_command(shell_command);
                self.enter_normal_mode();
                return;
            }
            Err(message) => {
                self.backend.status_message = Some(message.to_owned());
                self.enter_normal_mode();
                return;
            }
            Ok(None) => {}
        }

        // Bare range (e.g. `:5`, `:.`, `:%`) with no following command → jump.
        if command.is_empty() {
            if let Some((start, _end)) = range {
                self.jump_to_line(start);
                self.enter_normal_mode();
                return;
            }
            self.enter_normal_mode();
            return;
        }

        let mut parts = command.split_whitespace();
        match parts.next().unwrap_or_default() {
            "q" | "quit" => {
                if !self.backend.pristine {
                    self.backend.status_message =
                        Some("unsaved changes (use :w to save or :q! to force)".to_owned());
                    self.enter_normal_mode();
                    return;
                }
                self.should_quit = true;
            }
            "q!" | "quit!" => self.should_quit = true,
            "w" | "write" => {
                if let Err(err) = self.backend.save() {
                    self.backend.status_message = Some(format!("save failed: {err}"));
                }
            }
            "wq" | "x" => {
                if let Err(err) = self.backend.save() {
                    self.backend.status_message = Some(format!("save failed: {err}"));
                } else {
                    self.should_quit = true;
                }
            }
            cmd if cmd == "s"
                || cmd == "substitute"
                || cmd.starts_with("s/")
                || cmd.starts_with("s!")
                || cmd.starts_with("s|")
                || cmd.starts_with("s,") =>
            {
                let body = if cmd == "s" || cmd == "substitute" {
                    let leftover = parts.collect::<Vec<_>>().join(" ");
                    if leftover.is_empty() {
                        self.backend.status_message =
                            Some("substitute: usage: s/pattern/replacement/[flags]".to_owned());
                        self.enter_normal_mode();
                        return;
                    }
                    leftover
                } else {
                    cmd[1..].to_owned()
                };
                let (start, end) = range.unwrap_or((cursor_line, cursor_line));
                match parse_substitute_cmd(&body) {
                    Some((pattern, replacement, flags)) => {
                        self.execute_substitute(start, end, &pattern, &replacement, &flags);
                    }
                    None => {
                        self.backend.status_message =
                            Some("substitute: usage: s/pattern/replacement/[flags]".to_owned());
                    }
                }
                self.enter_normal_mode();
                return;
            }
            "d" | "delete" => {
                let (start, end) = range.unwrap_or((cursor_line, cursor_line));
                self.delete_line_range(start, end);
            }
            "y" | "yank" => {
                let (start, end) = range.unwrap_or((cursor_line, cursor_line));
                self.yank_line_range(start, end);
            }
            "format" => {
                if let Err(err) = self.backend.format_document() {
                    self.backend.status_message = Some(format!("format failed: {err}"));
                }
            }
            "complete" => {
                if let Err(err) = self.backend.request_completion(None) {
                    self.backend.status_message = Some(format!("completion failed: {err}"));
                }
            }
            "definition" | "def" => {
                if let Err(err) = self.backend.request_definition() {
                    self.backend.status_message = Some(format!("definition failed: {err}"));
                }
            }
            "references" | "refs" => {
                if let Err(err) = self.backend.request_references() {
                    self.backend.status_message = Some(format!("references failed: {err}"));
                }
            }
            "symbols" | "outline" => {
                if let Err(err) = self.backend.request_document_symbols() {
                    self.backend.status_message = Some(format!("symbols failed: {err}"));
                }
            }
            "wsymbols" | "wsymbol" => {
                let query = parts.collect::<Vec<_>>().join(" ");
                if let Err(err) = self.backend.request_workspace_symbols(&query) {
                    self.backend.status_message = Some(format!("workspace symbols failed: {err}"));
                }
            }
            "codeaction" | "codeactions" => {
                let action_index = parts.next().and_then(|part| part.parse::<usize>().ok());
                if let Err(err) = self.backend.request_code_actions(action_index) {
                    self.backend.status_message = Some(format!("code action failed: {err}"));
                }
            }
            "rename" => {
                let new_name = parts.collect::<Vec<_>>().join(" ");
                if new_name.is_empty() {
                    self.backend.status_message =
                        Some(String::from("rename: usage: :rename new_name"));
                } else if let Err(err) = self.backend.request_rename(&new_name) {
                    self.backend.status_message = Some(format!("rename failed: {err}"));
                }
            }
            "diagnostics" => {
                self.open_diagnostics_location_list();
            }
            "hover" => {
                let position = Some((self.backend.cursor_line, self.backend.cursor_col));
                if let Err(err) = self.backend.request_hover(position) {
                    self.backend.status_message = Some(format!("hover failed: {err}"));
                }
            }
            "gblame" => {
                self.show_git_blame();
                self.enter_normal_mode();
                return;
            }
            "gdiff" => {
                self.open_git_diff_view(false);
                self.enter_normal_mode();
                return;
            }
            "ghunkdiff" => {
                self.open_git_diff_view(true);
                self.enter_normal_mode();
                return;
            }
            "reindent" => {
                let _ = self.backend.send_edit("reindent", json!([]));
            }
            "help" => {
                self.open_help_picker("Help", Self::help_items());
                return;
            }
            "commands" => {
                self.open_help_picker("Commands", Self::command_help_items());
                return;
            }
            "keymap" => {
                self.open_help_picker("Keymap", Self::keymap_help_items());
                return;
            }
            "selection_for_find" => {
                let _ =
                    self.backend.send_edit("selection_for_find", json!({ "case_sensitive": true }));
                let _ = self.backend.send_edit("highlight_find", json!({ "visible": true }));
            }
            "selection_for_replace" => {
                let _ = self.backend.send_edit("selection_for_replace", json!([]));
            }
            "transpose" => {
                let _ = self.backend.send_edit("transpose", json!([]));
            }
            "duplicate_line" => {
                let _ = self.backend.send_edit("duplicate_line", json!([]));
            }
            "increment" => {
                let _ = self.backend.send_edit("increase_number", json!([]));
            }
            "decrement" => {
                let _ = self.backend.send_edit("decrease_number", json!([]));
            }
            "multi_find" => {
                let terms = parts.collect::<Vec<_>>();
                if terms.is_empty() {
                    self.backend.status_message =
                        Some("multi_find: usage: :multi_find term [term ...]".to_owned());
                } else {
                    let queries = terms
                        .into_iter()
                        .enumerate()
                        .map(|(index, term)| {
                            json!({
                                "id": index,
                                "chars": term,
                                "case_sensitive": smart_case_sensitive(term),
                                "regex": false,
                                "whole_words": false,
                            })
                        })
                        .collect::<Vec<_>>();
                    let _ = self.backend.send_edit("multi_find", json!({ "queries": queries }));
                }
            }
            "selection_into_lines" | "split_selection" => {
                let _ = self.backend.send_edit("selection_into_lines", json!([]));
            }
            "split_selection_on_newline" => {
                let _ = self.backend.send_edit("selection_into_lines", json!([]));
            }
            "select_regex" => {
                let pattern = parts.collect::<Vec<_>>().join(" ");
                if pattern.is_empty() {
                    self.backend.status_message =
                        Some("select_regex: usage: :select_regex pattern".to_owned());
                } else {
                    let _ = self.backend.send_edit(
                        "select_regex",
                        json!({
                            "chars": pattern,
                            "case_sensitive": false,
                        }),
                    );
                }
            }
            "merge_selections" => {
                let _ = self.backend.send_edit("merge_selections", json!([]));
            }
            "merge_consecutive_selections" => {
                let _ = self.backend.send_edit("merge_consecutive_selections", json!([]));
            }
            "trim_selections" => {
                let _ = self.backend.send_edit("trim_selections", json!([]));
            }
            "collapse_selection" => {
                let _ = self.backend.send_edit("collapse_selections", json!([]));
            }
            "flip_selections" => {
                let _ = self.backend.send_edit("flip_selections", json!([]));
            }
            "ensure_selections_forward" => {
                let _ = self.backend.send_edit("ensure_selections_forward", json!([]));
            }
            "keep_primary_selection" => {
                let _ = self.backend.send_edit("keep_primary_selection", json!([]));
            }
            "remove_primary_selection" => {
                let _ = self.backend.send_edit("remove_primary_selection", json!([]));
            }
            "copy_selection_on_next_line" => {
                let _ = self.backend.send_edit("add_selection_below", json!([]));
            }
            "copy_selection_on_prev_line" => {
                let _ = self.backend.send_edit("add_selection_above", json!([]));
            }
            "rotate_selection_contents_backward" => {
                let _ = self.backend.send_edit("rotate_selection_contents_backward", json!([]));
            }
            "rotate_selection_contents_forward" => {
                let _ = self.backend.send_edit("rotate_selection_contents_forward", json!([]));
            }
            "select_all" => {
                let _ = self.backend.send_edit("select_all", json!([]));
            }
            "add_selection_above" => {
                let _ = self.backend.send_edit("add_selection_above", json!([]));
            }
            "add_selection_below" => {
                let _ = self.backend.send_edit("add_selection_below", json!([]));
            }
            "insert_tab" => {
                let _ = self.backend.send_edit("insert_tab", json!([]));
            }
            "e" | "edit" => {
                let path = parts.next().map(PathBuf::from);
                match self.backend.open_buffer(path) {
                    Ok(buf_id) => {
                        let _ = self.backend.switch_to_id(buf_id);
                        self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
                        self.viewport = Viewport::default();
                    }
                    Err(err) => {
                        self.backend.status_message = Some(format!("open failed: {err}"));
                    }
                }
            }
            "e!" | "edit!" => {
                let id = self.backend.active().id;
                match self.backend.reload_buffer(id) {
                    Ok(()) => {
                        self.viewport = Viewport::default();
                    }
                    Err(err) => {
                        self.backend.status_message = Some(format!("reload failed: {err}"));
                    }
                }
            }
            "recover" => {
                let recovery_path = self
                    .backend
                    .active()
                    .path
                    .as_ref()
                    .and_then(|p| crate::buffer::recovery_file_path(p));
                match recovery_path {
                    Some(rp) if rp.exists() => match self.backend.open_buffer(Some(rp)) {
                        Ok(buf_id) => {
                            let _ = self.backend.switch_to_id(buf_id);
                            self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
                            self.viewport = Viewport::default();
                        }
                        Err(err) => {
                            self.backend.status_message = Some(format!("recover failed: {err}"));
                        }
                    },
                    Some(_) => {
                        self.backend.status_message = Some("no recovery file found".to_owned());
                    }
                    None => {
                        self.backend.status_message =
                            Some("current buffer has no backing file".to_owned());
                    }
                }
            }
            "recoverdel" => {
                let recovery_path = self
                    .backend
                    .active()
                    .path
                    .as_ref()
                    .and_then(|p| crate::buffer::recovery_file_path(p));
                match recovery_path {
                    Some(rp) if rp.exists() => match std::fs::remove_file(&rp) {
                        Ok(()) => {
                            self.backend.status_message = Some(format!("deleted {}", rp.display()));
                        }
                        Err(err) => {
                            self.backend.status_message = Some(format!("recoverdel failed: {err}"));
                        }
                    },
                    _ => {
                        self.backend.status_message = Some("no recovery file found".to_owned());
                    }
                }
            }
            "copen" | "cope" => {
                self.quickfix_open = true;
                if self.quickfix.as_ref().is_some_and(|q| !q.is_empty()) {
                    self.quickfix_focused = true;
                }
            }
            "cclose" | "ccl" => {
                self.quickfix_open = false;
                self.quickfix_focused = false;
            }
            "cn" | "cnext" => self.qf_next(true),
            "cp" | "cprev" | "cprevious" => self.qf_prev(true),
            "cfirst" => {
                if let Some(qf) = self.quickfix.as_mut() {
                    let entry = qf.first_entry().cloned();
                    if let Some(e) = entry {
                        self.navigate_to_qf_entry(e);
                    }
                }
            }
            "clast" => {
                if let Some(qf) = self.quickfix.as_mut() {
                    let entry = qf.last_entry().cloned();
                    if let Some(e) = entry {
                        self.navigate_to_qf_entry(e);
                    }
                }
            }
            "cc" => {
                let n = parts.next().and_then(|s| s.parse::<usize>().ok()).unwrap_or(1);
                if let Some(qf) = self.quickfix.as_mut() {
                    let entry = qf.select_one_based(n).cloned();
                    if let Some(e) = entry {
                        self.navigate_to_qf_entry(e);
                    }
                }
            }
            "clist" | "cl" => {
                let msg = match &self.quickfix {
                    None => "no quickfix list".to_owned(),
                    Some(qf) if qf.is_empty() => "quickfix list is empty".to_owned(),
                    Some(qf) => qf
                        .entries
                        .iter()
                        .enumerate()
                        .map(|(i, e)| {
                            let marker = if i == qf.selected { ">" } else { " " };
                            format!("{marker}{}: {}", i + 1, e.display_label())
                        })
                        .collect::<Vec<_>>()
                        .join("  "),
                };
                self.backend.status_message = Some(msg);
            }
            "lopen" | "lop" => {
                self.location_list_open = true;
                if self.location_list.as_ref().is_some_and(|l| !l.is_empty()) {
                    self.location_list_focused = true;
                }
            }
            "lclose" | "lcl" => {
                self.location_list_open = false;
                self.location_list_focused = false;
            }
            "lnext" | "ln" => self.qf_next(false),
            "lprev" | "lp" | "lprevious" => self.qf_prev(false),
            "lfirst" => {
                if let Some(ll) = self.location_list.as_mut() {
                    let entry = ll.first_entry().cloned();
                    if let Some(e) = entry {
                        self.navigate_to_qf_entry(e);
                    }
                }
            }
            "llast" => {
                if let Some(ll) = self.location_list.as_mut() {
                    let entry = ll.last_entry().cloned();
                    if let Some(e) = entry {
                        self.navigate_to_qf_entry(e);
                    }
                }
            }
            "ll" => {
                let n = parts.next().and_then(|s| s.parse::<usize>().ok()).unwrap_or(1);
                if let Some(ll) = self.location_list.as_mut() {
                    let entry = ll.select_one_based(n).cloned();
                    if let Some(e) = entry {
                        self.navigate_to_qf_entry(e);
                    }
                }
            }
            "bn" | "bnext" => {
                let old = self.backend.active().id;
                self.backend.next_buffer();
                let new = self.backend.active().id;
                if old != new {
                    self.tabs.focused_windows_mut().set_focused_buffer(new);
                    self.viewport = Viewport::default();
                }
            }
            "bp" | "bprev" | "bprevious" => {
                let old = self.backend.active().id;
                self.backend.prev_buffer();
                let new = self.backend.active().id;
                if old != new {
                    self.tabs.focused_windows_mut().set_focused_buffer(new);
                    self.viewport = Viewport::default();
                }
            }
            "b#" => match self.backend.switch_alternate() {
                Ok(()) => {
                    let new = self.backend.active().id;
                    self.tabs.focused_windows_mut().set_focused_buffer(new);
                    self.viewport = Viewport::default();
                }
                Err(err) => {
                    self.backend.status_message = Some(format!("{err}"));
                }
            },
            "bd" | "bdelete" => {
                let id = self.backend.active().id;
                if let Err(err) = self.backend.close_buffer(id) {
                    self.backend.status_message = Some(format!("close failed: {err}"));
                } else {
                    let new = self.backend.active().id;
                    self.tabs.focused_windows_mut().set_focused_buffer(new);
                    self.viewport = Viewport::default();
                }
            }
            "ls" | "buffers" | "Buffers" => {
                let list = self.backend.list_buffers_str();
                self.backend.status_message = Some(list);
            }
            "sp" | "split" => {
                let path = parts.next().map(PathBuf::from);
                let buf_id = if let Some(p) = path {
                    match self.backend.open_buffer(Some(p)) {
                        Ok(id) => id,
                        Err(err) => {
                            self.backend.status_message = Some(format!("open failed: {err}"));
                            self.enter_normal_mode();
                            return;
                        }
                    }
                } else {
                    self.backend.active().id
                };
                let (_, new_vp) = self.tabs.focused_windows_mut().split(
                    SplitDir::Horizontal,
                    buf_id,
                    self.viewport,
                );
                self.viewport = new_vp;
                let _ = self.backend.switch_to_id(buf_id);
            }
            "vs" | "vsplit" => {
                let path = parts.next().map(PathBuf::from);
                let buf_id = if let Some(p) = path {
                    match self.backend.open_buffer(Some(p)) {
                        Ok(id) => id,
                        Err(err) => {
                            self.backend.status_message = Some(format!("open failed: {err}"));
                            self.enter_normal_mode();
                            return;
                        }
                    }
                } else {
                    self.backend.active().id
                };
                let (_, new_vp) = self.tabs.focused_windows_mut().split(
                    SplitDir::Vertical,
                    buf_id,
                    self.viewport,
                );
                self.viewport = new_vp;
                let _ = self.backend.switch_to_id(buf_id);
            }
            "tabnew" | "tabe" | "tabedit" => {
                let path = parts.next().map(PathBuf::from);
                let buf_id = match self.backend.open_buffer(path) {
                    Ok(id) => id,
                    Err(err) => {
                        self.backend.status_message = Some(format!("open failed: {err}"));
                        self.enter_normal_mode();
                        return;
                    }
                };
                let new_vp = self.tabs.new_tab(buf_id, self.viewport);
                self.viewport = new_vp;
                let _ = self.backend.switch_to_id(buf_id);
            }
            "tabc" | "tabclose" => {
                if let Some(new_vp) = self.tabs.close_tab(self.viewport) {
                    self.viewport = new_vp;
                    let new_buf = self.tabs.focused_windows().focused_window().buffer_id;
                    let _ = self.backend.switch_to_id(new_buf);
                } else {
                    self.should_quit = true;
                }
            }
            "tabn" | "tabnext" => {
                let new_vp = self.tabs.focus_next(self.viewport);
                self.viewport = new_vp;
                let new_buf = self.tabs.focused_windows().focused_window().buffer_id;
                let _ = self.backend.switch_to_id(new_buf);
            }
            "tabp" | "tabprev" | "tabprevious" => {
                let new_vp = self.tabs.focus_prev(self.viewport);
                self.viewport = new_vp;
                let new_buf = self.tabs.focused_windows().focused_window().buffer_id;
                let _ = self.backend.switch_to_id(new_buf);
            }
            "tabs" => {
                let info = (0..self.tabs.tab_count())
                    .map(|i| {
                        let marker = if i == self.tabs.focused_idx() { '>' } else { ' ' };
                        format!("{marker} Tab {}", i + 1)
                    })
                    .collect::<Vec<_>>()
                    .join("  ");
                self.backend.status_message = Some(info);
            }
            "files" | "Files" => {
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                self.picker = Some(PickerState::new_files(cwd));
                self.enter_normal_mode();
                return;
            }
            "bpick" => {
                let entries: Vec<_> = self
                    .backend
                    .all_bufs()
                    .iter()
                    .map(|b| (b.id, b.title(), b.path.clone()))
                    .collect();
                self.picker = Some(PickerState::new_buffers(entries));
                self.enter_normal_mode();
                return;
            }
            "grep" | "Grep" => {
                let query = parts.collect::<Vec<_>>().join(" ");
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                self.picker = Some(PickerState::new_grep(query, cwd));
                self.enter_normal_mode();
                return;
            }
            "set" => {
                let opt = parts.next().unwrap_or_default();
                self.apply_set_option(opt);
            }
            "noh" | "nohlsearch" => {
                self.search_pattern = None;
                let _ = self.backend.send_edit("highlight_find", json!({ "visible": false }));
                self.backend.status_message = Some("search highlight cleared".to_owned());
            }
            other if !other.is_empty() => {
                self.backend.status_message = Some(format!("unknown command: {other}"));
            }
            _ => {}
        }
        self.enter_normal_mode();
    }

    fn apply_set_option(&mut self, opt: &str) {
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

    pub(super) fn history_older(&mut self) {
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

    pub(super) fn history_newer(&mut self) {
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

    pub(super) fn complete_command(&mut self) {
        const COMMANDS: &[&str] = &[
            "b#",
            "bd",
            "bdelete",
            "bn",
            "bnext",
            "bp",
            "bprev",
            "bprevious",
            "buffers",
            "Buffers",
            "cc",
            "ccl",
            "cclose",
            "cfirst",
            "cl",
            "clast",
            "clist",
            "cn",
            "cnext",
            "cope",
            "copen",
            "cp",
            "cprev",
            "cprevious",
            "codeaction",
            "codeactions",
            "complete",
            "d",
            "def",
            "definition",
            "delete",
            "diagnostics",
            "e",
            "e!",
            "edit",
            "edit!",
            "commands",
            "decrement",
            "duplicate_line",
            "files",
            "Files",
            "format",
            "grep",
            "Grep",
            "gblame",
            "gdiff",
            "ghunkdiff",
            "help",
            "hover",
            "increment",
            "insert_tab",
            "keymap",
            "lcl",
            "lclose",
            "lfirst",
            "llast",
            "ln",
            "lnext",
            "lop",
            "lopen",
            "lp",
            "lprev",
            "lprevious",
            "ls",
            "multi_find",
            "make",
            "q",
            "q!",
            "quit",
            "quit!",
            "recover",
            "recoverdel",
            "reindent",
            "rename",
            "references",
            "refs",
            "run",
            "selection_for_find",
            "selection_for_replace",
            "select_regex",
            "selection_into_lines",
            "split_selection",
            "split_selection_on_newline",
            "merge_selections",
            "merge_consecutive_selections",
            "trim_selections",
            "collapse_selection",
            "flip_selections",
            "ensure_selections_forward",
            "keep_primary_selection",
            "remove_primary_selection",
            "copy_selection_on_next_line",
            "copy_selection_on_prev_line",
            "rotate_selection_contents_backward",
            "rotate_selection_contents_forward",
            "select_all",
            "sp",
            "split",
            "tabc",
            "tabclose",
            "tabe",
            "tabedit",
            "tabn",
            "tabnext",
            "tabnew",
            "tabp",
            "tabprev",
            "tabprevious",
            "tabs",
            "term",
            "terminal",
            "test",
            "transpose",
            "add_selection_above",
            "add_selection_below",
            "vs",
            "vsplit",
            "w",
            "wq",
            "write",
            "x",
            "y",
            "yank",
        ];
        let prefix = self.command_buffer.clone();
        let candidates: Vec<&&str> = COMMANDS.iter().filter(|c| c.starts_with(&*prefix)).collect();
        if let Some(&&first) = candidates.first() {
            self.command_buffer = first.to_owned();
        }
    }

    fn open_help_picker(&mut self, title: &str, items: Vec<String>) {
        self.picker = Some(PickerState::new_help(title, items));
        self.enter_normal_mode();
    }

    fn help_items() -> Vec<String> {
        vec![
            "Discovery: :commands | :keymap".to_owned(),
            "Modes: i insert | v visual | V visual-line | Ctrl-V visual-block | : command".to_owned(),
            "Move: h j k l | w b e | gg G | % | * # | n N".to_owned(),
            "Edit: d c y operators | p/P register paste | u undo | Ctrl-R redo | . repeat".to_owned(),
            "IDE: :hover :complete :codeaction :definition :references :rename :diagnostics".to_owned(),
            "Backend ops: :transpose :duplicate_line :increment :decrement :reindent".to_owned(),
            "Selections: :select_regex :selection_into_lines :trim_selections :collapse_selection :select_all".to_owned(),
            "Search sets: :multi_find term [term ...]".to_owned(),
            "Shell: :term cmd | :!cmd | :make [args] | :test [args] | :run [args]".to_owned(),
            "Workspace: :files :bpick :grep :buffers :split :vsplit :tabnew".to_owned(),
        ]
    }

    fn command_help_items() -> Vec<String> {
        vec![
            ":help open searchable editor help".to_owned(),
            ":commands list ex commands and features".to_owned(),
            ":keymap list high-value normal-mode bindings".to_owned(),
            ":hover request LSP hover at cursor".to_owned(),
            ":term cmd run shell command and open transcript buffer".to_owned(),
            ":!cmd shorthand shell command runner".to_owned(),
            ":make [args] run cargo build in transcript buffer".to_owned(),
            ":test [args] run cargo test in transcript buffer".to_owned(),
            ":run [args] run cargo run in transcript buffer".to_owned(),
            ":gblame show git blame metadata for current line".to_owned(),
            ":gdiff open git diff for current buffer in scratch view".to_owned(),
            ":ghunkdiff open git diff for current hunk in scratch view".to_owned(),
            ":complete open completion picker from backend suggestions".to_owned(),
            ":codeaction open backend code-action picker".to_owned(),
            ":rename new_name request backend rename at cursor".to_owned(),
            ":diagnostics open location list for active-buffer diagnostics".to_owned(),
            ":reindent run core reindent on current selection or line".to_owned(),
            ":transpose backend transpose at cursor".to_owned(),
            ":duplicate_line backend duplicate current selection line(s)".to_owned(),
            ":increment / :decrement adjust number under cursor".to_owned(),
            ":selection_for_find / :selection_for_replace lift selection into find or replace"
                .to_owned(),
            ":selection_into_lines split selection into per-line cursors".to_owned(),
            ":select_regex pattern select regex matches inside current selections".to_owned(),
            ":split_selection_on_newline split selections on line boundaries".to_owned(),
            ":merge_selections / :merge_consecutive_selections combine active selections"
                .to_owned(),
            ":trim_selections / :collapse_selection normalize current selections".to_owned(),
            ":flip_selections / :ensure_selections_forward rewrite selection direction".to_owned(),
            ":keep_primary_selection / :remove_primary_selection keep or drop rightmost selection"
                .to_owned(),
            ":copy_selection_on_next_line / :copy_selection_on_prev_line clone selection to adjacent line"
                .to_owned(),
            ":rotate_selection_contents_backward / :rotate_selection_contents_forward cycle selected text"
                .to_owned(),
            ":select_all select entire buffer".to_owned(),
            ":add_selection_above / :add_selection_below grow multi-cursor set".to_owned(),
            ":multi_find term [term ...] run backend multi-find queries".to_owned(),
        ]
    }

    fn keymap_help_items() -> Vec<String> {
        vec![
            "K request hover".to_owned(),
            "gb show git blame for current line".to_owned(),
            "gD open git diff scratch view".to_owned(),
            "Ctrl-A increase number under cursor".to_owned(),
            "Ctrl-X decrease number under cursor".to_owned(),
            "Ctrl-Up add selection above".to_owned(),
            "Ctrl-Down add selection below".to_owned(),
            "gd duplicate current line or selection".to_owned(),
            "* / # selection-for-find forward/backward".to_owned(),
            "gt / gT next and previous tab".to_owned(),
            "]h / [h git hunk next and previous".to_owned(),
            "]q / [q quickfix next and previous".to_owned(),
            "]Q / [Q location list next and previous".to_owned(),
            "z a o c R M fold toggle/open/close/open-all/close-all".to_owned(),
            "Ctrl-O / Tab jump list older/newer".to_owned(),
            "g; / g, change list older/newer".to_owned(),
        ]
    }
}
