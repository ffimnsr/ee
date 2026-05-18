use super::*;
use std::path::{Component, Path};

use crate::buffer::BufferId;
use crate::registers::RegisterName;

#[derive(Clone, Copy)]
enum WindowLineTarget {
    Top,
    Center,
    Bottom,
}

impl App {
    const LSP_PLUGIN_NAME: &'static str = "xi-lsp-plugin";

    fn current_workspace_root(&self) -> PathBuf {
        self.backend
            .active()
            .path
            .as_deref()
            .and_then(|path| crate::config::find_git_root(path.parent().unwrap_or(path)))
            .or_else(|| {
                std::env::current_dir().ok().and_then(|cwd| crate::config::find_git_root(&cwd))
            })
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    fn current_picker_root(&self) -> Option<PathBuf> {
        self.backend.active().path.as_ref().and_then(|path| path.parent().map(Path::to_path_buf))
    }

    fn open_file_picker_at(&mut self, cwd: PathBuf, title: &str) {
        let mut picker = PickerState::new_files(cwd);
        picker.title = title.to_owned();
        self.open_picker(picker);
    }

    pub(crate) fn open_file_picker_for_buffer_directory(&mut self) {
        let cwd = self
            .current_picker_root()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        self.open_file_picker_at(cwd, "Files");
    }

    pub(crate) fn open_file_picker_in_current_directory(&mut self) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        self.open_file_picker_at(cwd, "Files (cwd)");
    }

    pub(crate) fn open_file_explorer(&mut self) {
        self.open_file_picker_at(self.current_workspace_root(), "Explorer");
    }

    pub(crate) fn open_file_explorer_for_buffer_directory(&mut self) {
        let cwd = self
            .current_picker_root()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        self.open_file_picker_at(cwd, "Explorer (buffer dir)");
    }

    pub(crate) fn open_file_explorer_in_current_directory(&mut self) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        self.open_file_picker_at(cwd, "Explorer (cwd)");
    }

    pub(crate) fn open_buffer_picker(&mut self) {
        let entries: Vec<_> = self
            .backend
            .all_bufs()
            .iter()
            .map(|buffer| (buffer.id, buffer.title(), buffer.path.clone()))
            .collect();
        self.open_picker(PickerState::new_buffers(entries));
    }

    fn open_location_picker(
        &mut self,
        title: &str,
        empty_message: &str,
        items: Vec<crate::picker::PickerItem>,
    ) {
        if items.is_empty() {
            self.backend.status_message = Some(empty_message.to_owned());
            return;
        }
        self.open_picker(PickerState::new_locations(title, items));
    }

    pub(crate) fn open_jump_list_picker(&mut self) {
        let items = self
            .jump_list
            .iter()
            .enumerate()
            .map(|(index, (line, col))| crate::picker::PickerItem {
                label: format!(
                    "{}:{}:{} {}",
                    index + 1,
                    line + 1,
                    col + 1,
                    self.backend
                        .active()
                        .get_line(*line)
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                        .unwrap_or("<blank>")
                ),
                detail: None,
                path: None,
                buf_id: None,
                line: Some(*line),
                col: Some(*col),
                choice_index: None,
            })
            .collect();
        self.open_location_picker("Jumplist", "no jumplist entries", items);
    }

    pub(crate) fn open_changed_file_picker(&mut self) {
        let repo_root = self
            .backend
            .active()
            .path
            .as_deref()
            .and_then(|path| crate::config::find_git_root(path.parent().unwrap_or(path)))
            .or_else(|| {
                std::env::current_dir().ok().and_then(|cwd| crate::config::find_git_root(&cwd))
            });
        let Some(repo_root) = repo_root else {
            self.backend.status_message = Some(String::from("changed files: not inside git repo"));
            return;
        };
        match crate::git::changed_files(&repo_root) {
            Ok(files) => {
                let items = files
                    .into_iter()
                    .map(|path| {
                        let label = path
                            .strip_prefix(&repo_root)
                            .unwrap_or(&path)
                            .to_string_lossy()
                            .into_owned();
                        crate::picker::PickerItem {
                            label,
                            detail: None,
                            path: Some(path),
                            buf_id: None,
                            line: None,
                            col: None,
                            choice_index: None,
                        }
                    })
                    .collect();
                self.open_location_picker("Changed Files", "no changed files", items);
            }
            Err(err) => {
                self.backend.status_message = Some(format!("changed files failed: {err}"));
            }
        }
    }

    pub(crate) fn open_diagnostics_picker(&mut self) {
        let active_id = self.backend.active().id;
        let items = self
            .active_diagnostic_items()
            .into_iter()
            .map(|(_, entry)| crate::picker::PickerItem {
                label: entry.display_label(),
                detail: entry.path.as_ref().map(|path| path.to_string_lossy().into_owned()),
                path: entry.path,
                buf_id: Some(active_id),
                line: Some(entry.line),
                col: Some(entry.col),
                choice_index: None,
            })
            .collect();
        self.open_location_picker("Diagnostics", "no diagnostics", items);
    }

    pub(crate) fn open_workspace_diagnostics_picker(&mut self) {
        let items = self
            .backend
            .all_bufs()
            .iter()
            .flat_map(|buffer| {
                let prefix = buffer
                    .path
                    .as_ref()
                    .and_then(|path| path.file_name())
                    .and_then(|name| name.to_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| buffer.title());
                buffer.diagnostics.iter().map(move |diagnostic| {
                    // Whole-buffer policy-allowed: diagnostic offset→line/col requires full text mirror.
                    let (line, col) = line_col_for_offset(&buffer.lines, diagnostic.range.start);
                    crate::picker::PickerItem {
                        label: format!("{prefix}:{}: {}", line + 1, diagnostic.message),
                        detail: buffer
                            .path
                            .as_ref()
                            .map(|path| path.to_string_lossy().into_owned()),
                        path: buffer.path.clone(),
                        buf_id: Some(buffer.id),
                        line: Some(line),
                        col: Some(col),
                        choice_index: None,
                    }
                })
            })
            .collect();
        self.open_location_picker("Workspace Diagnostics", "no workspace diagnostics", items);
    }

    pub(super) fn open_global_search(&mut self) {
        let cwd = self.current_workspace_root();
        let mut picker = PickerState::new_grep(String::new(), cwd);
        picker.title = String::from("Global Search");
        self.open_picker(picker);
        self.enter_normal_mode();
    }

    pub(super) fn open_command_palette(&mut self) {
        self.open_help_picker("Command Palette", Self::command_help_items());
    }

    pub(crate) fn reopen_last_picker(&mut self) {
        let Some(picker) = self.last_picker.clone() else {
            self.backend.status_message = Some(String::from("no previous picker"));
            return;
        };
        self.picker = Some(picker);
    }

    pub(super) fn execute_command(&mut self) {
        let _ = self.backend.sync_pending_events();
        self.handle_pending_ui_actions();
        self.handle_pending_locations();
        self.handle_pending_symbols();

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
        let line_count = self.backend.line_count().max(1);
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
        let head = parts.next().unwrap_or_default();
        let tail = command[head.len()..].trim_start();
        match head {
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
            "qa" | "quit_all" => {
                if self.backend.all_bufs().iter().any(|buf| !buf.pristine) {
                    self.backend.status_message =
                        Some("unsaved changes (use :wa to save or :qa! to force)".to_owned());
                    self.enter_normal_mode();
                    return;
                }
                self.should_quit = true;
            }
            "qa!" | "quit_all!" => self.should_quit = true,
            "w" | "write" | "w!" | "write!" => {
                if let Err(message) = self.save_current_buffer() {
                    self.backend.status_message = Some(message);
                }
            }
            "u" | "update" => {
                if !self.backend.active().pristine
                    && let Err(message) = self.save_current_buffer()
                {
                    self.backend.status_message = Some(message);
                }
            }
            "wq" | "x" | "wq!" | "x!" | "write_quit" | "write_quit!" => {
                if let Err(message) = self.save_current_buffer() {
                    self.backend.status_message = Some(message);
                } else {
                    self.should_quit = true;
                }
            }
            "wa" | "wa!" | "write_all" | "write_all!" => {
                if let Err(message) = self.save_all_dirty_buffers() {
                    self.backend.status_message = Some(message);
                }
            }
            "wqa" | "xa" | "wqa!" | "xa!" | "write_quit_all" | "write_quit_all!" => {
                if let Err(message) = self.save_all_dirty_buffers() {
                    self.backend.status_message = Some(message);
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
            "paste_clipboard_after" => {
                self.paste_from_register(RegisterName::Clipboard, false);
            }
            "paste_clipboard_before" => {
                self.paste_from_register(RegisterName::Clipboard, true);
            }
            "yank_to_clipboard" => {
                if let Some((start, end)) = range {
                    self.yank_line_range_into_register(start, end, RegisterName::Clipboard);
                } else {
                    self.yank_selection_to_register(RegisterName::Clipboard);
                }
            }
            "yank_main_selection_to_clipboard" => {
                self.yank_main_selection_to_register(RegisterName::Clipboard);
            }
            "replace_selections_with_clipboard" => {
                self.replace_selections_with_register(RegisterName::Clipboard);
            }
            "paste_primary_clipboard_after" => {
                self.paste_from_register(RegisterName::PrimaryClipboard, false);
            }
            "paste_primary_clipboard_before" => {
                self.paste_from_register(RegisterName::PrimaryClipboard, true);
            }
            "yank_to_primary_clipboard" => {
                if let Some((start, end)) = range {
                    self.yank_line_range_into_register(start, end, RegisterName::PrimaryClipboard);
                } else {
                    self.yank_selection_to_register(RegisterName::PrimaryClipboard);
                }
            }
            "yank_main_selection_to_primary_clipboard" => {
                self.yank_main_selection_to_register(RegisterName::PrimaryClipboard);
            }
            "replace_selections_with_primary_clipboard" => {
                self.replace_selections_with_register(RegisterName::PrimaryClipboard);
            }
            "format" => {
                if let Err(err) = self.backend.format_document() {
                    self.backend.status_message = Some(format!("format failed: {err}"));
                }
            }
            "complete" | "completion" => {
                if let Err(err) = self.backend.request_completion(None) {
                    self.backend.status_message = Some(format!("completion failed: {err}"));
                }
            }
            "definition" | "def" => {
                if let Err(err) = self.backend.request_definition() {
                    self.backend.status_message = Some(format!("definition failed: {err}"));
                }
            }
            "goto_declaration" => {
                if let Err(err) = self.backend.request_declaration() {
                    self.backend.status_message = Some(format!("declaration failed: {err}"));
                }
            }
            "goto_definition" => {
                if let Err(err) = self.backend.request_definition() {
                    self.backend.status_message = Some(format!("definition failed: {err}"));
                }
            }
            "goto_type_definition" => {
                if let Err(err) = self.backend.request_type_definition() {
                    self.backend.status_message = Some(format!("type definition failed: {err}"));
                }
            }
            "references" | "refs" => {
                if let Err(err) = self.backend.request_references() {
                    self.backend.status_message = Some(format!("references failed: {err}"));
                }
            }
            "goto_reference" | "select_references_to_symbol_under_cursor" => {
                if let Err(err) = self.backend.request_references() {
                    self.backend.status_message = Some(format!("references failed: {err}"));
                }
            }
            "goto_implementation" => {
                if let Err(err) = self.backend.request_implementation() {
                    self.backend.status_message = Some(format!("implementation failed: {err}"));
                }
            }
            "symbols" | "outline" => {
                if let Err(err) = self.backend.request_document_symbols() {
                    self.backend.status_message = Some(format!("symbols failed: {err}"));
                }
            }
            "symbol_picker" => {
                if let Err(err) = self.backend.request_document_symbols() {
                    self.backend.status_message = Some(format!("symbols failed: {err}"));
                }
            }
            "wsymbols" | "wsymbol" => {
                let query = tail.to_owned();
                if let Err(err) = self.backend.request_workspace_symbols(&query) {
                    self.backend.status_message = Some(format!("workspace symbols failed: {err}"));
                }
            }
            "workspace_symbol_picker" => {
                if let Err(err) = self.backend.request_workspace_symbols("") {
                    self.backend.status_message = Some(format!("workspace symbols failed: {err}"));
                }
            }
            "codeaction" | "codeactions" => {
                let action_index = parts.next().and_then(|part| part.parse::<usize>().ok());
                if let Err(err) = self.backend.request_code_actions(action_index) {
                    self.backend.status_message = Some(format!("code action failed: {err}"));
                }
            }
            "code_action" => {
                if let Err(err) = self.backend.request_code_actions(None) {
                    self.backend.status_message = Some(format!("code action failed: {err}"));
                }
            }
            "goto_column" => {
                let Some(column) = parts.next().and_then(|part| part.parse::<usize>().ok()) else {
                    self.backend.status_message =
                        Some(String::from("goto_column: usage: :goto_column <column>"));
                    self.enter_normal_mode();
                    return;
                };
                self.goto_column(column.saturating_sub(1));
            }
            "goto_first_nonwhitespace" => {
                self.goto_first_nonwhitespace();
            }
            "goto_last_modification" => {
                self.change_list_older();
            }
            "goto_word" => {
                self.move_word_start(true, false);
            }
            "swift_motion" | "swift" => {
                self.start_swift_motion();
                return;
            }
            "goto_window_top" => {
                self.goto_window_top();
            }
            "goto_window_center" => {
                self.goto_window_center();
            }
            "goto_window_bottom" => {
                self.goto_window_bottom();
            }
            "goto_last_accessed_file" => {
                self.goto_last_accessed_file();
            }
            "goto_last_modified_file" => {
                self.goto_last_modified_file();
            }
            "goto_next_diag" => {
                self.goto_adjacent_diagnostic(true);
            }
            "goto_prev_diag" => {
                self.goto_adjacent_diagnostic(false);
            }
            "goto_first_diag" => {
                self.goto_edge_diagnostic(true);
            }
            "goto_last_diag" => {
                self.goto_edge_diagnostic(false);
            }
            "goto_next_function"
            | "goto_prev_function"
            | "goto_next_class"
            | "goto_prev_class"
            | "goto_next_parameter"
            | "goto_prev_parameter"
            | "goto_next_comment"
            | "goto_prev_comment"
            | "goto_next_test"
            | "goto_prev_test"
            | "goto_next_paragraph"
            | "goto_prev_paragraph" => {
                let _ = self.backend.send_edit(head, json!([]));
            }
            "goto_next_change" => {
                self.jump_to_git_hunk(true);
            }
            "goto_prev_change" => {
                self.jump_to_git_hunk(false);
            }
            "goto_first_change" => {
                self.jump_to_git_hunk_edge(true);
            }
            "goto_last_change" => {
                self.jump_to_git_hunk_edge(false);
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
            "diagnostics_picker" => {
                self.open_diagnostics_picker();
                self.enter_normal_mode();
                return;
            }
            "workspace_diagnostics_picker" => {
                self.open_workspace_diagnostics_picker();
                self.enter_normal_mode();
                return;
            }
            "hover" => {
                let position = Some((self.backend.cursor_line, self.backend.cursor_col));
                if let Err(err) = self.backend.request_hover(position) {
                    self.backend.status_message = Some(format!("hover failed: {err}"));
                }
            }
            "insert_register" => {
                self.backend.status_message = Some(match self.insert_register_command(tail) {
                    Ok(message) => message,
                    Err(message) => message,
                });
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
            "toggle_comments" => {
                let _ = self.backend.send_edit("toggle_comment", json!([]));
            }
            "toggle_line_comments" => {
                let _ = self.backend.send_edit("toggle_line_comment", json!([]));
            }
            "toggle_block_comments" => {
                let _ = self.backend.send_edit("toggle_block_comment", json!([]));
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
            "sort" => {
                let result = match range {
                    Some((start, end)) => self.sort_line_range(start, end),
                    None => self.sort_selected_or_all_lines(),
                };
                match result {
                    Ok(message) | Err(message) => self.backend.status_message = Some(message),
                }
            }
            "uniq" | "dedup" => {
                let result = match range {
                    Some((start, end)) => self.dedup_line_range(start, end),
                    None => self.dedup_selected_or_all_lines(),
                };
                match result {
                    Ok(message) | Err(message) => self.backend.status_message = Some(message),
                }
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
            "align_selections" => {
                let _ = self.backend.send_edit("align_selections", json!([]));
            }
            "align_it" => {
                let spec = tail.trim();
                match parse_align_it_spec(spec) {
                    Ok(spec) => {
                        let params = json!({
                            "pattern": spec.pattern,
                            "regex": spec.regex,
                            "occurrence": spec.occurrence,
                            "all": spec.all,
                            "format": spec.format,
                            "range": range.map(|(start, end)| [start as i64, end as i64]),
                        });
                        let _ = self.backend.send_edit("align_it", params);
                    }
                    Err(message) => {
                        self.backend.status_message = Some(message);
                    }
                }
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
            "rotate_selections_backward" => {
                let _ = self.backend.send_edit("rotate_selections_backward", json!([]));
            }
            "rotate_selections_forward" => {
                let _ = self.backend.send_edit("rotate_selections_forward", json!([]));
            }
            "move_line_down" => {
                if let Err(message) = self.move_current_line_adjacent(true) {
                    self.backend.status_message = Some(message);
                }
            }
            "move_line_up" => {
                if let Err(message) = self.move_current_line_adjacent(false) {
                    self.backend.status_message = Some(message);
                }
            }
            "create_directory" => {
                if tail.is_empty() {
                    self.backend.status_message =
                        Some(String::from("create_directory: usage: :create_directory <path>"));
                } else {
                    self.backend.status_message = Some(
                        self.create_directory_in_workspace(tail).unwrap_or_else(|message| message),
                    );
                }
            }
            "match_brackets" => {
                let _ = self.backend.move_to_matching_bracket(false);
            }
            "surround_add" => {
                let Some(pair) = parts.next() else {
                    self.backend.status_message =
                        Some("surround_add: usage: :surround_add <pair> [textobject]".to_owned());
                    self.enter_normal_mode();
                    return;
                };
                let textobject = parts.next().and_then(|arg| arg.chars().next());
                if let Err(message) = self.surround_add(pair, textobject) {
                    self.backend.status_message = Some(message);
                }
            }
            "surround_replace" => {
                let Some(pair) = parts.next() else {
                    self.backend.status_message =
                        Some("surround_replace: usage: :surround_replace <pair>".to_owned());
                    self.enter_normal_mode();
                    return;
                };
                if let Err(message) = self.surround_replace(pair) {
                    self.backend.status_message = Some(message);
                }
            }
            "surround_delete" => {
                if let Err(message) = self.surround_delete() {
                    self.backend.status_message = Some(message);
                }
            }
            "select_textobject_around" => {
                let Some(spec) = parts.next().and_then(|arg| arg.chars().next()) else {
                    self.backend.status_message = Some(
                        "select_textobject_around: usage: :select_textobject_around <specifier>"
                            .to_owned(),
                    );
                    self.enter_normal_mode();
                    return;
                };
                if let Err(message) = self.select_text_object(true, spec) {
                    self.backend.status_message = Some(message);
                }
            }
            "select_textobject_inner" => {
                let Some(spec) = parts.next().and_then(|arg| arg.chars().next()) else {
                    self.backend.status_message = Some(
                        "select_textobject_inner: usage: :select_textobject_inner <specifier>"
                            .to_owned(),
                    );
                    self.enter_normal_mode();
                    return;
                };
                if let Err(message) = self.select_text_object(false, spec) {
                    self.backend.status_message = Some(message);
                }
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
            "reverse_selection_contents" => {
                let _ = self.backend.send_edit("reverse_selection_contents", json!([]));
            }
            "select_all" => {
                let _ = self.backend.send_edit("select_all", json!([]));
            }
            "delete_word_backward" => {
                let _ = self.backend.send_edit("delete_word_backward", json!([]));
            }
            "delete_word_forward" => {
                let _ = self.backend.send_edit("delete_word_forward", json!([]));
            }
            "kill_to_line_start" => {
                let _ = self.backend.send_edit("delete_to_beginning_of_line", json!([]));
            }
            "kill_to_line_end" => {
                let _ = self.backend.send_edit("delete_to_end_of_paragraph", json!([]));
            }
            "kill_line" => {
                self.delete_line_range(cursor_line, cursor_line);
            }
            "delete_char_backward" => {
                let _ = self.backend.send_edit("delete_backward", json!([]));
            }
            "delete_char_forward" => {
                if self.try_vlf_delete_forward(1) {
                    return;
                }
                let _ = self.backend.send_edit("delete_forward", json!([]));
            }
            "insert_newline" => {
                let _ = self.backend.send_edit("insert_newline", json!([]));
            }
            "add_newline_below" => {
                self.add_newline_below();
            }
            "add_newline_above" => {
                self.add_newline_above();
            }
            "extend_char_left" => {
                let _ = self.backend.send_edit("move_left_and_modify_selection", json!([]));
            }
            "extend_char_right" => {
                let _ = self.backend.send_edit("move_right_and_modify_selection", json!([]));
            }
            "extend_line_up" | "extend_visual_line_up" => {
                let _ = self.backend.send_edit("move_up_and_modify_selection", json!([]));
            }
            "extend_line_down" | "extend_visual_line_down" => {
                let _ = self.backend.send_edit("move_down_and_modify_selection", json!([]));
            }
            "extend_line_above" => {
                let _ = self.backend.send_edit("extend_line_above", json!([]));
            }
            "extend_line_below" => {
                self.extend_line_below();
            }
            "select_line_above" => {
                let _ = self.backend.send_edit("select_line_above", json!([]));
            }
            "select_line_below" => {
                let _ = self.backend.send_edit("select_line_below", json!([]));
            }
            "extend_to_line_bounds" => {
                self.extend_to_line_bounds();
            }
            "shrink_to_line_bounds" => {
                self.shrink_to_line_bounds();
            }
            "goto_file_end" => {
                let _ = self.backend.send_edit("move_to_end_of_document", json!([]));
            }
            "extend_to_file_start" => {
                let _ = self
                    .backend
                    .send_edit("move_to_beginning_of_document_and_modify_selection", json!([]));
            }
            "extend_to_file_end" => {
                let _ = self
                    .backend
                    .send_edit("move_to_end_of_document_and_modify_selection", json!([]));
            }
            "join_selections" => {
                self.join_selections(false);
            }
            "join_selections_space" => {
                self.join_selections(true);
            }
            "keep_selections" => {
                let pattern = parts.collect::<Vec<_>>().join(" ");
                if pattern.is_empty() {
                    self.filter_selections_from_search(false);
                } else {
                    self.filter_selections(&pattern, false);
                }
            }
            "remove_selections" => {
                let pattern = parts.collect::<Vec<_>>().join(" ");
                if pattern.is_empty() {
                    self.filter_selections_from_search(true);
                } else {
                    self.filter_selections(&pattern, true);
                }
            }
            "expand_selection" => {
                let _ = self.backend.send_edit("expand_selection", json!([]));
            }
            "shrink_selection" => {
                let _ = self.backend.send_edit("shrink_selection", json!([]));
            }
            "select_prev_sibling" => {
                let _ = self.backend.send_edit("select_prev_sibling", json!([]));
            }
            "select_next_sibling" => {
                let _ = self.backend.send_edit("select_next_sibling", json!([]));
            }
            "select_all_siblings" => {
                let _ = self.backend.send_edit("select_all_siblings", json!([]));
            }
            "select_all_children" => {
                let _ = self.backend.send_edit("select_all_children", json!([]));
            }
            "move_parent_node_start" => {
                let _ = self.backend.send_edit("move_parent_node_start", json!([]));
            }
            "move_parent_node_end" => {
                let _ = self.backend.send_edit("move_parent_node_end", json!([]));
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
            "e" | "edit" | "o" | "open" => {
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
            "e!" | "edit!" | "rl" | "reload" => {
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
            "rla" | "reload_all" => {
                if let Err(message) = self.reload_all_buffers() {
                    self.backend.status_message = Some(message);
                } else {
                    self.viewport = Viewport::default();
                }
            }
            "new" | "n" => {
                if let Err(message) = self.open_scratch_buffer() {
                    self.backend.status_message = Some(message);
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
            "set_language" | "lang" => {
                self.backend.status_message = Some(if tail.is_empty() {
                    format!("language: {}", self.current_buffer_language())
                } else {
                    match self.set_current_buffer_language(tail) {
                        Ok(language) => format!("language: {language}"),
                        Err(message) => message,
                    }
                });
            }
            "reload_config" | "config_reload" => {
                self.backend.status_message = Some(match self.reload_runtime_config() {
                    Ok(message) => message,
                    Err(message) => message,
                });
            }
            "lsp_restart" => {
                self.backend.status_message =
                    Some(match self.backend.restart_plugin(Self::LSP_PLUGIN_NAME) {
                        Ok(()) => String::from("lsp restart requested"),
                        Err(err) => format!("lsp restart failed: {err}"),
                    });
            }
            "lsp_stop" => {
                self.backend.status_message =
                    Some(match self.backend.stop_plugin(Self::LSP_PLUGIN_NAME) {
                        Ok(()) => String::from("lsp stop requested"),
                        Err(err) => format!("lsp stop failed: {err}"),
                    });
            }
            "change_current_directory" | "cd" => {
                self.backend.status_message = Some(if tail.is_empty() {
                    String::from("cd: usage: :cd path")
                } else {
                    match std::env::set_current_dir(PathBuf::from(tail)) {
                        Ok(()) => match std::env::current_dir() {
                            Ok(path) => format!("cwd: {}", path.display()),
                            Err(err) => format!("cd failed: {err}"),
                        },
                        Err(err) => format!("cd failed: {err}"),
                    }
                });
            }
            "show_directory" | "pwd" => {
                self.backend.status_message = Some(match std::env::current_dir() {
                    Ok(path) => format!("cwd: {}", path.display()),
                    Err(err) => format!("pwd failed: {err}"),
                });
            }
            "pipe" | "|" | "shell_pipe" => {
                self.backend.status_message = Some(if tail.is_empty() {
                    format!("{head}: usage: :{head} shell-command")
                } else {
                    match self.run_shell_command_on_selections(tail, ShellSelectionMode::Replace) {
                        Ok(message) => message,
                        Err(message) => message,
                    }
                });
            }
            "pipe_to" | "shell_pipe_to" => {
                self.backend.status_message = Some(if tail.is_empty() {
                    format!("{head}: usage: :{head} shell-command")
                } else {
                    match self
                        .run_shell_command_on_selections(tail, ShellSelectionMode::IgnoreOutput)
                    {
                        Ok(message) => message,
                        Err(message) => message,
                    }
                });
            }
            "shell_insert_output" => {
                self.backend.status_message = Some(if tail.is_empty() {
                    String::from("shell_insert_output: usage: :shell_insert_output shell-command")
                } else {
                    match self
                        .run_shell_command_on_selections(tail, ShellSelectionMode::InsertBefore)
                    {
                        Ok(message) => message,
                        Err(message) => message,
                    }
                });
            }
            "shell_append_output" => {
                self.backend.status_message = Some(if tail.is_empty() {
                    String::from("shell_append_output: usage: :shell_append_output shell-command")
                } else {
                    match self
                        .run_shell_command_on_selections(tail, ShellSelectionMode::InsertAfter)
                    {
                        Ok(message) => message,
                        Err(message) => message,
                    }
                });
            }
            "shell_keep_pipe" => {
                self.backend.status_message = Some(if tail.is_empty() {
                    String::from("shell_keep_pipe: usage: :shell_keep_pipe shell-command")
                } else {
                    match self
                        .run_shell_command_on_selections(tail, ShellSelectionMode::KeepByStatus)
                    {
                        Ok(message) => message,
                        Err(message) => message,
                    }
                });
            }
            "reset_diff_change" | "diffget" | "diffg" => {
                self.backend.status_message = Some(match self.restore_git_hunk() {
                    Ok(message) => message,
                    Err(message) => message,
                });
            }
            "read" | "r" => {
                self.backend.status_message = Some(if tail.is_empty() {
                    String::from("read: usage: :read path")
                } else {
                    match self.read_file_into_buffer(tail) {
                        Ok(message) => message,
                        Err(message) => message,
                    }
                });
            }
            "move" | "mv" => {
                self.backend.status_message = Some(if tail.is_empty() {
                    String::from("move: usage: :move path")
                } else {
                    match self.move_current_buffer(tail) {
                        Ok(message) => message,
                        Err(message) => message,
                    }
                });
            }
            "echo" => {
                self.backend.status_message = Some(tail.to_owned());
            }
            "encoding" => {
                self.backend.status_message = Some(if tail.is_empty() {
                    format!("encoding: {}", self.config.charset)
                } else {
                    self.config.charset = tail.to_owned();
                    format!("encoding: {}", self.config.charset)
                });
            }
            "clear_register" => {
                self.backend.status_message = Some(match self.clear_register_command(tail) {
                    Ok(message) => message,
                    Err(message) => message,
                });
            }
            "redraw" => {
                self.redraw_requested = true;
                self.backend.status_message = Some(String::from("redraw"));
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
            "bn" | "bnext" | "goto_next_buffer" => self.cycle_buffer_command(true),
            "bp" | "bprev" | "bprevious" | "goto_previous_buffer" => {
                self.cycle_buffer_command(false);
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
            "bd" | "bdelete" | "bc" | "bclose" | "buffer_close" => {
                let id = self.backend.active().id;
                if let Err(message) = self.close_buffers(
                    &[id],
                    false,
                    "unsaved changes (use :write to save or :bc! to force)",
                ) {
                    self.backend.status_message = Some(message);
                }
            }
            "bc!" | "bclose!" | "buffer_close!" => {
                let id = self.backend.active().id;
                if let Err(message) = self.close_buffers(&[id], true, "") {
                    self.backend.status_message = Some(message);
                }
            }
            "buffer_close_others" | "bco" | "bcloseother" => {
                let active_id = self.backend.active().id;
                let ids = self
                    .backend
                    .all_bufs()
                    .iter()
                    .map(|buf| buf.id)
                    .filter(|id| *id != active_id)
                    .collect::<Vec<_>>();
                if let Err(message) = self.close_buffers(
                    &ids,
                    false,
                    "unsaved changes (use :wa to save or :bco! to force)",
                ) {
                    self.backend.status_message = Some(message);
                }
            }
            "bco!" | "bcloseother!" | "buffer_close_others!" => {
                let active_id = self.backend.active().id;
                let ids = self
                    .backend
                    .all_bufs()
                    .iter()
                    .map(|buf| buf.id)
                    .filter(|id| *id != active_id)
                    .collect::<Vec<_>>();
                if let Err(message) = self.close_buffers(&ids, true, "") {
                    self.backend.status_message = Some(message);
                }
            }
            "buffer_close_all" | "bca" | "bcloseall" => {
                if let Err(message) = self.close_all_buffers(false) {
                    self.backend.status_message = Some(message);
                }
            }
            "bca!" | "bcloseall!" | "buffer_close_all!" => {
                if let Err(message) = self.close_all_buffers(true) {
                    self.backend.status_message = Some(message);
                }
            }
            "ls" | "buffers" => {
                let list = self.backend.list_buffers_str();
                self.backend.status_message = Some(list);
            }
            "sp" | "split" | "hs" | "hsplit" => {
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
            "goto" | "g" => {
                let Some(line) = parts.next().and_then(|part| part.parse::<usize>().ok()) else {
                    self.backend.status_message = Some("goto: usage: :goto line-number".to_owned());
                    self.enter_normal_mode();
                    return;
                };
                self.jump_to_line(line.saturating_sub(1));
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
            "rotate_view" | "cycle_view" => {
                self.rotate_view();
            }
            "rotate_view_reverse" => {
                self.rotate_view_reverse();
            }
            "transpose_view" => {
                self.transpose_view();
            }
            "wclose" => {
                self.close_view();
            }
            "wonly" => {
                self.close_other_views();
            }
            "jump_view_left" => {
                self.jump_view(crate::window::ViewDirection::Left);
            }
            "jump_view_down" => {
                self.jump_view(crate::window::ViewDirection::Down);
            }
            "jump_view_up" => {
                self.jump_view(crate::window::ViewDirection::Up);
            }
            "jump_view_right" => {
                self.jump_view(crate::window::ViewDirection::Right);
            }
            "swap_view_left" => {
                self.swap_view(crate::window::ViewDirection::Left);
            }
            "swap_view_down" => {
                self.swap_view(crate::window::ViewDirection::Down);
            }
            "swap_view_up" => {
                self.swap_view(crate::window::ViewDirection::Up);
            }
            "swap_view_right" => {
                self.swap_view(crate::window::ViewDirection::Right);
            }
            "commit_undo_checkpoint" => {
                let _ = self.backend.send_edit("commit_undo_checkpoint", json!([]));
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
            "files" => {
                self.open_file_picker_in_current_directory();
                self.enter_normal_mode();
                return;
            }
            "file_explorer" => {
                self.open_file_explorer();
                self.enter_normal_mode();
                return;
            }
            "file_explorer_in_current_buffer_directory" => {
                self.open_file_explorer_for_buffer_directory();
                self.enter_normal_mode();
                return;
            }
            "file_explorer_in_current_directory" => {
                self.open_file_explorer_in_current_directory();
                self.enter_normal_mode();
                return;
            }
            "file_picker" => {
                self.open_file_picker_for_buffer_directory();
                self.enter_normal_mode();
                return;
            }
            "file_picker_in_current_directory" => {
                self.open_file_picker_in_current_directory();
                self.enter_normal_mode();
                return;
            }
            "bpick" => {
                self.open_buffer_picker();
                self.enter_normal_mode();
                return;
            }
            "buffer_picker" => {
                self.open_buffer_picker();
                self.enter_normal_mode();
                return;
            }
            "jumplist_picker" => {
                self.open_jump_list_picker();
                self.enter_normal_mode();
                return;
            }
            "changed_file_picker" => {
                self.open_changed_file_picker();
                self.enter_normal_mode();
                return;
            }
            "last_picker" => {
                self.reopen_last_picker();
                self.enter_normal_mode();
                return;
            }
            "global_search" => {
                self.open_global_search();
                return;
            }
            "command_palette" => {
                self.open_command_palette();
                return;
            }
            "grep" => {
                let query = parts.collect::<Vec<_>>().join(" ");
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                self.open_picker(PickerState::new_grep(query, cwd));
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
            "bc",
            "bc!",
            "bd",
            "bdelete",
            "bclose",
            "bclose!",
            "bcloseall",
            "bcloseall!",
            "bcloseother",
            "bcloseother!",
            "bca",
            "bca!",
            "bco",
            "bco!",
            "buffer_close!",
            "buffer_close",
            "buffer_close_all",
            "buffer_close_all!",
            "buffer_close_others",
            "buffer_close_others!",
            "bn",
            "bnext",
            "goto_next_buffer",
            "bp",
            "bprev",
            "bprevious",
            "goto_previous_buffer",
            "buffers",
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
            "code_action",
            "complete",
            "completion",
            "command_palette",
            "d",
            "def",
            "definition",
            "goto_declaration",
            "goto_definition",
            "goto_type_definition",
            "goto_reference",
            "goto_implementation",
            "delete",
            "diagnostics",
            "e",
            "goto_last_accessed_file",
            "goto_last_modified_file",
            "e!",
            "edit",
            "goto_window_bottom",
            "goto_window_center",
            "goto_window_top",
            "edit!",
            "g",
            "commands",
            "create_directory",
            "decrement",
            "delete_char_backward",
            "delete_char_forward",
            "delete_word_backward",
            "delete_word_forward",
            "duplicate_line",
            "files",
            "file_explorer",
            "file_explorer_in_current_buffer_directory",
            "file_explorer_in_current_directory",
            "file_picker",
            "file_picker_in_current_directory",
            "format",
            "grep",
            "global_search",
            "buffer_picker",
            "changed_file_picker",
            "gblame",
            "gdiff",
            "ghunkdiff",
            "goto",
            "goto_column",
            "goto_first_change",
            "goto_first_diag",
            "goto_first_nonwhitespace",
            "goto_last_change",
            "goto_last_diag",
            "goto_last_modification",
            "goto_next_change",
            "goto_next_class",
            "goto_next_comment",
            "goto_next_diag",
            "goto_next_function",
            "goto_next_paragraph",
            "goto_next_parameter",
            "goto_next_test",
            "goto_prev_change",
            "goto_prev_class",
            "goto_prev_comment",
            "goto_prev_diag",
            "goto_prev_function",
            "goto_prev_paragraph",
            "goto_prev_parameter",
            "goto_prev_test",
            "goto_word",
            "lang",
            "hs",
            "hsplit",
            "help",
            "hover",
            "increment",
            "insert_newline",
            "insert_register",
            "insert_tab",
            "keymap",
            "kill_line",
            "kill_to_line_end",
            "kill_to_line_start",
            "lcl",
            "lclose",
            "lfirst",
            "llast",
            "lsp_restart",
            "lsp_stop",
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
            "move",
            "move_parent_node_end",
            "move_parent_node_start",
            "mv",
            "n",
            "new",
            "o",
            "open",
            "pipe",
            "pipe_to",
            "pwd",
            "q",
            "q!",
            "qa",
            "qa!",
            "quit",
            "quit!",
            "quit_all",
            "quit_all!",
            "recover",
            "recoverdel",
            "reload_config",
            "reset_diff_change",
            "reindent",
            "rename",
            "references",
            "refs",
            "reload",
            "reload_all",
            "rl",
            "rla",
            "read",
            "redraw",
            "run",
            "run_shell_command",
            "shell_append_output",
            "shell_insert_output",
            "shell_keep_pipe",
            "shell_pipe",
            "shell_pipe_to",
            "selection_for_find",
            "selection_for_replace",
            "select_regex",
            "selection_into_lines",
            "set_language",
            "sh",
            "show_directory",
            "split_selection",
            "split_selection_on_newline",
            "merge_selections",
            "merge_consecutive_selections",
            "trim_selections",
            "align_selections",
            "align_it",
            "collapse_selection",
            "clear_register",
            "flip_selections",
            "echo",
            "encoding",
            "ensure_selections_forward",
            "expand_selection",
            "extend_char_left",
            "extend_char_right",
            "extend_line_above",
            "extend_line_below",
            "extend_line_down",
            "extend_line_up",
            "extend_to_line_bounds",
            "extend_to_file_end",
            "extend_to_file_start",
            "extend_visual_line_down",
            "extend_visual_line_up",
            "join_selections",
            "join_selections_space",
            "jumplist_picker",
            "keep_selections",
            "keep_primary_selection",
            "last_picker",
            "match_brackets",
            "move_line_down",
            "move_line_up",
            "goto_file_end",
            "remove_selections",
            "remove_primary_selection",
            "rotate_selections_backward",
            "rotate_selections_forward",
            "select_line_above",
            "select_line_below",
            "select_all_children",
            "select_all_siblings",
            "symbol_picker",
            "swift",
            "swift_motion",
            "select_textobject_around",
            "select_textobject_inner",
            "select_next_sibling",
            "select_prev_sibling",
            "select_references_to_symbol_under_cursor",
            "shrink_selection",
            "shrink_to_line_bounds",
            "copy_selection_on_next_line",
            "copy_selection_on_prev_line",
            "surround_add",
            "surround_delete",
            "surround_replace",
            "workspace_diagnostics_picker",
            "workspace_symbol_picker",
            "add_newline_above",
            "add_newline_below",
            "reverse_selection_contents",
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
            "rotate_view",
            "cycle_view",
            "rotate_view_reverse",
            "transpose_view",
            "wclose",
            "wonly",
            "jump_view_left",
            "jump_view_down",
            "jump_view_up",
            "jump_view_right",
            "swap_view_left",
            "swap_view_down",
            "swap_view_up",
            "swap_view_right",
            "term",
            "terminal",
            "test",
            "transpose",
            "sort",
            "uniq",
            "dedup",
            "add_selection_above",
            "add_selection_below",
            "change_current_directory",
            "cd",
            "commit_undo_checkpoint",
            "diffget",
            "diffg",
            "|",
            "vs",
            "vsplit",
            "w",
            "w!",
            "wq",
            "wq!",
            "wa",
            "wa!",
            "write!",
            "write_all",
            "write_all!",
            "write_quit",
            "write_quit!",
            "write_quit_all",
            "write_quit_all!",
            "write",
            "wqa",
            "wqa!",
            "x",
            "x!",
            "xa",
            "xa!",
            "y",
            "yank",
            "paste_clipboard_after",
            "paste_clipboard_before",
            "yank_to_clipboard",
            "yank_main_selection_to_clipboard",
            "replace_selections_with_clipboard",
            "paste_primary_clipboard_after",
            "paste_primary_clipboard_before",
            "yank_to_primary_clipboard",
            "yank_main_selection_to_primary_clipboard",
            "replace_selections_with_primary_clipboard",
        ];
        let prefix = self.command_buffer.clone();
        let candidates: Vec<&&str> = COMMANDS.iter().filter(|c| c.starts_with(&*prefix)).collect();
        if let Some(&&first) = candidates.first() {
            self.command_buffer = first.to_owned();
        }
    }

    fn open_help_picker(&mut self, title: &str, items: Vec<String>) {
        self.open_picker(PickerState::new_help(title, items));
        self.enter_normal_mode();
    }

    pub(super) fn current_buffer_language(&self) -> String {
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

    fn cycle_buffer_command(&mut self, forward: bool) {
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

    pub(super) fn goto_last_accessed_file(&mut self) {
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

    pub(super) fn goto_last_modified_file(&mut self) {
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

    pub(super) fn goto_window_top(&mut self) {
        self.goto_window_line(WindowLineTarget::Top);
    }

    pub(super) fn goto_window_center(&mut self) {
        self.goto_window_line(WindowLineTarget::Center);
    }

    pub(super) fn goto_window_bottom(&mut self) {
        self.goto_window_line(WindowLineTarget::Bottom);
    }

    fn goto_window_line(&mut self, target: WindowLineTarget) {
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

    fn active_diagnostic_items(&self) -> Vec<(usize, QfEntry)> {
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

    pub(super) fn populate_diagnostics_location_list(&mut self, selected: usize) -> bool {
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

    pub(super) fn active_cursor_offset(&self) -> usize {
        let buf = self.backend.active();
        let line = self.backend.cursor_line.min(buf.line_count().saturating_sub(1));
        // Bounded: reads only up to cursor line, not full buffer.
        let prefix = buf.line_start_offset(line).unwrap_or(0);
        let col = buf.get_line(line).map(|l| self.backend.cursor_col.min(l.len())).unwrap_or(0);
        prefix + col
    }

    fn goto_adjacent_diagnostic(&mut self, forward: bool) {
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

    fn goto_edge_diagnostic(&mut self, first: bool) {
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

    fn set_current_buffer_language(&mut self, requested: &str) -> Result<String, String> {
        let language = xi_core_lib::tree_sitter_support::canonical_language_name(requested)
            .ok_or_else(|| format!("set_language: unknown language `{requested}`"))?;
        self.syntax_overrides.insert(self.backend.active().id, language.clone());
        Ok(language)
    }

    fn reload_runtime_config(&mut self) -> Result<String, String> {
        let active_path = self.backend.active().path.clone();
        self.config = crate::config::load_config(active_path.as_deref());
        self.key_bindings = crate::keymap::bindings_for(&self.config.keymap);
        self.key_sequences = crate::keymap::sequence_bindings_for(&self.config.keymap);
        self.backend
            .reload_editor_config()
            .map_err(|err| format!("config reload failed: {err}"))?;
        Ok(String::from("config reloaded"))
    }

    fn resolve_workspace_path(&self, target: &str) -> Result<PathBuf, String> {
        let target = target.trim();
        if target.is_empty() {
            return Err(String::from("path cannot be empty"));
        }

        let workspace_root = self.current_workspace_root();
        let relative = if Path::new(target).is_absolute() {
            Path::new(target).strip_prefix(&workspace_root).map_err(|_| {
                format!("path must stay under workspace {}", workspace_root.display())
            })?
        } else {
            Path::new(target)
        };

        let mut resolved = workspace_root.clone();
        for component in relative.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(part) => resolved.push(part),
                Component::ParentDir => {
                    if resolved == workspace_root {
                        return Err(format!(
                            "path must stay under workspace {}",
                            workspace_root.display()
                        ));
                    }
                    resolved.pop();
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(format!(
                        "path must stay under workspace {}",
                        workspace_root.display()
                    ));
                }
            }
        }

        Ok(resolved)
    }

    fn create_directory_in_workspace(&mut self, target: &str) -> Result<String, String> {
        let workspace_root = self.current_workspace_root();
        let path = self
            .resolve_workspace_path(target)
            .map_err(|message| format!("create_directory: {message}"))?;

        if path.exists() && !path.is_dir() {
            return Err(format!(
                "create_directory failed: {} exists and is not a directory",
                path.display()
            ));
        }

        std::fs::create_dir_all(&path).map_err(|err| format!("create_directory failed: {err}"))?;
        let display = path.strip_prefix(&workspace_root).unwrap_or(&path);
        Ok(format!("created {}", display.display()))
    }

    fn read_file_into_buffer(&mut self, path: &str) -> Result<String, String> {
        let path = PathBuf::from(path);
        let content =
            std::fs::read_to_string(&path).map_err(|err| format!("read failed: {err}"))?;
        self.backend
            .send_edit("insert", json!({ "chars": content }))
            .map_err(|err| format!("read failed: {err}"))?;
        Ok(format!("read {}", path.display()))
    }

    fn move_current_buffer(&mut self, target: &str) -> Result<String, String> {
        let Some(source) = self.backend.active().path.clone() else {
            return Err(String::from("move: current buffer has no backing file"));
        };
        let target = PathBuf::from(target);
        if source == target {
            return Err(String::from("move: source and destination are the same"));
        }
        if target.exists() {
            return Err(format!("move failed: {} already exists", target.display()));
        }
        if let Some(parent) = target.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|err| format!("move failed: {err}"))?;
        }

        let buffer_id = self.backend.active().id;
        let pristine =
            self.backend.buffer_pristine(buffer_id).map_err(|err| format!("move failed: {err}"))?;
        if pristine {
            std::fs::rename(&source, &target).map_err(|err| format!("move failed: {err}"))?;
        }
        self.backend
            .set_buffer_path(buffer_id, target.clone())
            .map_err(|err| format!("move failed: {err}"))?;
        self.backend.save_buffer(buffer_id).map_err(|err| format!("move failed: {err}"))?;
        if !pristine {
            std::fs::remove_file(&source).map_err(|err| format!("move failed: {err}"))?;
        }
        Ok(format!("moved {} -> {}", source.display(), target.display()))
    }

    fn clear_register_command(&mut self, target: &str) -> Result<String, String> {
        let target = target.trim();
        if target.is_empty() {
            self.registers.clear(None);
            return Ok(String::from("registers cleared"));
        }
        let mut chars = target.chars();
        let Some(name) = chars.next() else {
            return Err(String::from("clear_register: usage: :clear_register [register]"));
        };
        if chars.next().is_some() {
            return Err(String::from("clear_register: usage: :clear_register [register]"));
        }
        let register = RegisterName::from_char(name)
            .ok_or_else(|| format!("clear_register: invalid register `{name}`"))?;
        self.registers.clear(Some(&register));
        Ok(format!("register {name} cleared"))
    }

    fn insert_register_command(&mut self, target: &str) -> Result<String, String> {
        let target = target.trim();
        let mut chars = target.chars();
        let Some(name) = chars.next() else {
            return Err(String::from("insert_register: usage: :insert_register <register>"));
        };
        if chars.next().is_some() {
            return Err(String::from("insert_register: usage: :insert_register <register>"));
        }
        let register = RegisterName::from_char(name)
            .ok_or_else(|| format!("insert_register: invalid register `{name}`"))?;
        let text = self.registers.get(&register);
        if text.is_empty() {
            return Ok(format!("register {name} empty"));
        }
        self.backend
            .send_edit("insert", json!({ "chars": text }))
            .map_err(|err| format!("insert_register failed: {err}"))?;
        Ok(format!("inserted register {name}"))
    }

    fn save_current_buffer(&mut self) -> Result<(), String> {
        self.backend.save().map_err(|err| format!("save failed: {err}"))
    }

    fn save_all_dirty_buffers(&mut self) -> Result<(), String> {
        use std::collections::HashSet;

        self.backend.flush_all_pending_edits().map_err(|err| format!("save failed: {err}"))?;
        let mut seen_paths = HashSet::new();
        let candidate_ids = self
            .backend
            .all_bufs()
            .iter()
            .rev()
            .filter_map(|buf| {
                let path = buf.path.as_ref()?;
                let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
                seen_paths.insert(key).then_some(buf.id)
            })
            .collect::<Vec<_>>();
        for id in candidate_ids {
            if self.backend.buffer_pristine(id).map_err(|err| format!("save failed: {err}"))? {
                continue;
            }
            self.backend.save_buffer(id).map_err(|err| format!("save failed: {err}"))?;
        }
        Ok(())
    }

    fn reload_all_buffers(&mut self) -> Result<(), String> {
        let ids = self.backend.all_bufs().iter().map(|buf| buf.id).collect::<Vec<_>>();
        for id in ids {
            self.backend.reload_buffer(id).map_err(|err| format!("reload failed: {err}"))?;
        }
        Ok(())
    }

    fn open_scratch_buffer(&mut self) -> Result<(), String> {
        let buf_id = self.backend.open_buffer(None).map_err(|err| format!("open failed: {err}"))?;
        self.backend.switch_to_id(buf_id).map_err(|err| format!("open failed: {err}"))?;
        self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
        self.viewport = Viewport::default();
        Ok(())
    }

    fn close_all_buffers(&mut self, force: bool) -> Result<(), String> {
        if !force && self.backend.all_bufs().iter().any(|buf| !buf.pristine) {
            return Err("unsaved changes (use :wa to save or :bca! to force)".to_owned());
        }

        let keep_id = if self.backend.buf_count() == 1 && self.backend.active().path.is_none() {
            self.backend.active().id
        } else {
            let buf_id =
                self.backend.open_buffer(None).map_err(|err| format!("open failed: {err}"))?;
            self.backend.switch_to_id(buf_id).map_err(|err| format!("open failed: {err}"))?;
            self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
            self.viewport = Viewport::default();
            buf_id
        };

        let ids = self
            .backend
            .all_bufs()
            .iter()
            .map(|buf| buf.id)
            .filter(|id| *id != keep_id)
            .collect::<Vec<_>>();
        self.close_buffers(&ids, true, "")
    }

    fn close_buffers(
        &mut self,
        ids: &[BufferId],
        force: bool,
        unsaved_message: &str,
    ) -> Result<(), String> {
        if !force
            && ids.iter().copied().any(|id| {
                self.backend
                    .all_bufs()
                    .iter()
                    .find(|buf| buf.id == id)
                    .is_some_and(|buf| !buf.pristine)
            })
        {
            return Err(unsaved_message.to_owned());
        }

        let active_id = self.backend.active().id;
        let closed_active = ids.contains(&active_id);
        for id in ids {
            if self.backend.all_bufs().iter().any(|buf| buf.id == *id) {
                self.backend.close_buffer(*id).map_err(|err| format!("close failed: {err}"))?;
            }
        }

        let fallback = self.backend.active().id;
        let valid_buffers = self
            .backend
            .all_bufs()
            .iter()
            .map(|buf| buf.id)
            .collect::<std::collections::HashSet<_>>();
        self.tabs.retarget_invalid_buffers(&valid_buffers, fallback);
        self.tabs.focused_windows_mut().set_focused_buffer(fallback);
        if closed_active {
            self.viewport = Viewport::default();
        }
        Ok(())
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
            "Workspace: :file_picker :file_picker_in_current_directory :buffer_picker :changed_file_picker :symbol_picker :workspace_symbol_picker :diagnostics_picker :workspace_diagnostics_picker :last_picker".to_owned(),
            "Explorer: :file_explorer :file_explorer_in_current_buffer_directory :file_explorer_in_current_directory".to_owned(),
        ]
    }

    fn command_help_items() -> Vec<String> {
        vec![
            ":help open searchable editor help".to_owned(),
            ":commands list ex commands and features".to_owned(),
            ":keymap list high-value normal-mode bindings".to_owned(),
            ":hover request LSP hover at cursor".to_owned(),
            ":command_palette open searchable command reference picker".to_owned(),
            ":term cmd run shell command and open transcript buffer".to_owned(),
            ":!cmd shorthand shell command runner".to_owned(),
            ":run_shell_command / :sh aliases for :term shell-command".to_owned(),
            ":make [args] run cargo build in transcript buffer".to_owned(),
            ":test [args] run cargo test in transcript buffer".to_owned(),
            ":run [args] run cargo run in transcript buffer".to_owned(),
            ":open / :o open file in current view | :new create scratch buffer".to_owned(),
            ":hsplit / :hs open file in horizontal split | :goto / :g jump to line".to_owned(),
            ":goto_column <column> move cursor to 1-based display column on current line".to_owned(),
            ":goto_first_nonwhitespace jump to first non-whitespace character on current line"
                .to_owned(),
            ":goto_last_modification jump to previous entry in change list".to_owned(),
            ":goto_declaration / :goto_definition / :goto_type_definition / :goto_reference / :goto_implementation request LSP navigation at cursor"
                .to_owned(),
            ":goto_next_buffer / :goto_previous_buffer cycle open buffers".to_owned(),
            ":goto_window_top / :goto_window_center / :goto_window_bottom jump cursor inside visible window"
                .to_owned(),
            ":goto_last_accessed_file / :goto_last_modified_file switch recent buffers"
                .to_owned(),
            ":goto_next_diag / :goto_prev_diag / :goto_first_diag / :goto_last_diag jump active-buffer diagnostics"
                .to_owned(),
            ":goto_word move to next word start using normal word semantics".to_owned(),
            ":swift_motion / :swift start visible-window two-char jump with labels".to_owned(),
            ":write! :write_all :write_quit :write_quit_all add Vim-style aliases".to_owned(),
            ":buffer_close :buffer_close_others :buffer_close_all manage buffers".to_owned(),
            ":reload / :reload_all discard edits and reopen from disk".to_owned(),
            ":reload_config refresh frontend config and keymap overrides".to_owned(),
            ":lsp_restart / :lsp_stop restart or stop language-server plugin".to_owned(),
            ":change_current_directory / :cd switch current working directory | :show_directory / :pwd print cwd"
                .to_owned(),
            ":create_directory <path> create directory tree under current workspace root"
                .to_owned(),
            ":rotate_view / :rotate_view_reverse / :transpose_view / :jump_view_* / :swap_view_* / :wclose / :wonly manage split focus and ordering"
                .to_owned(),
            ":set_language / :lang set or show current syntax name".to_owned(),
            ":read / :r insert file contents at cursor".to_owned(),
            ":move / :mv move current buffer to new path".to_owned(),
            ":encoding show or set current buffer encoding metadata".to_owned(),
            ":clear_register [name] clear one register or all registers".to_owned(),
            ":insert_register <name> insert register contents at cursor".to_owned(),
            ":echo print arguments to status line | :redraw clear and repaint UI".to_owned(),
            ":pipe / :| / :pipe_to run shell commands on current selections".to_owned(),
            ":shell_insert_output / :shell_append_output insert shell output around selections"
                .to_owned(),
            ":shell_keep_pipe keep selections whose shell command exits successfully".to_owned(),
            ":gblame show git blame metadata for current line".to_owned(),
            ":gdiff open git diff for current buffer in scratch view".to_owned(),
            ":ghunkdiff open git diff for current hunk in scratch view".to_owned(),
            ":global_search open workspace live-grep picker".to_owned(),
            ":goto_next_change / :goto_prev_change / :goto_first_change / :goto_last_change jump across git hunks"
                .to_owned(),
            ":reset_diff_change / :diffget / :diffg restore current git hunk from HEAD"
                .to_owned(),
            ":complete / :completion open completion picker from backend suggestions".to_owned(),
            ":codeaction / :code_action open backend code-action picker".to_owned(),
            ":rename new_name request backend rename at cursor".to_owned(),
            ":select_references_to_symbol_under_cursor request backend references at cursor"
                .to_owned(),
            ":diagnostics open location list for active-buffer diagnostics".to_owned(),
            ":file_picker / :file_picker_in_current_directory open file pickers rooted at buffer dir or cwd".to_owned(),
            ":file_explorer / :file_explorer_in_current_buffer_directory / :file_explorer_in_current_directory open explorer-style pickers rooted at workspace, buffer dir, or cwd".to_owned(),
            ":buffer_picker / :changed_file_picker open buffer or git-changed-file pickers".to_owned(),
            ":symbol_picker / :workspace_symbol_picker request symbol pickers".to_owned(),
            ":diagnostics_picker / :workspace_diagnostics_picker open diagnostic pickers".to_owned(),
            ":jumplist_picker / :last_picker open jump history or reopen prior picker".to_owned(),
            ":reindent run core reindent on current selection or line".to_owned(),
            ":toggle_comments toggle comments using line comment when available, else block comment"
                .to_owned(),
            ":toggle_line_comments force line-comment toggle on current selection or line"
                .to_owned(),
            ":toggle_block_comments force block-comment toggle on current selection or line"
                .to_owned(),
            ":transpose backend transpose at cursor".to_owned(),
            ":sort sort selected lines or whole buffer when nothing is selected".to_owned(),
            ":uniq / :dedup remove duplicate lines from selection or whole buffer".to_owned(),
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
            ":align_selections pad selections into aligned columns".to_owned(),
            ":align_it [N|*|-N]<delimiter>|/regex/ [l1r1l0] align matched lines tabular-style in selection, range, or contiguous block".to_owned(),
            ":flip_selections / :ensure_selections_forward rewrite selection direction".to_owned(),
            ":move_line_up / :move_line_down swap current line with adjacent line".to_owned(),
            ":match_brackets jump to matching bracket around cursor".to_owned(),
            ":extend_char_left / :extend_char_right / :extend_line_up / :extend_line_down / :extend_visual_line_up / :extend_visual_line_down grow selections with motions"
                .to_owned(),
            ":extend_line_above / :extend_line_below / :select_line_above / :select_line_below rewrite whole-line selections"
                .to_owned(),
            ":goto_file_end / :extend_to_file_start / :extend_to_file_end jump or extend to document edges"
                .to_owned(),
            ":extend_to_line_bounds / :shrink_to_line_bounds rewrite line selections"
                .to_owned(),
            ":join_selections / :join_selections_space join selected lines".to_owned(),
            ":select_textobject_inner <spec> / :select_textobject_around <spec> select text object at cursor"
                .to_owned(),
            ":surround_add <pair> [spec] / :surround_replace <pair> / :surround_delete edit surrounding delimiters"
                .to_owned(),
            ":keep_selections [regex] / :remove_selections [regex] filter selections"
                .to_owned(),
            ":keep_primary_selection / :remove_primary_selection keep or drop primary selection"
                .to_owned(),
            ":rotate_selections_backward / :rotate_selections_forward cycle primary selection"
                .to_owned(),
            ":expand_selection / :shrink_selection grow or restore syntax-node selections"
                .to_owned(),
            ":select_prev_sibling / :select_next_sibling / :select_all_siblings / :select_all_children syntax-tree multi-selection"
                .to_owned(),
            ":move_parent_node_start / :move_parent_node_end jump cursor to parent syntax node boundary"
                .to_owned(),
            ":goto_next_function / :goto_prev_function / :goto_next_class / :goto_prev_class syntax-aware structural jumps"
                .to_owned(),
            ":goto_next_parameter / :goto_prev_parameter / :goto_next_comment / :goto_prev_comment / :goto_next_test / :goto_prev_test / :goto_next_paragraph / :goto_prev_paragraph"
                .to_owned(),
            ":copy_selection_on_next_line / :copy_selection_on_prev_line clone selection to adjacent line"
                .to_owned(),
            ":rotate_selection_contents_backward / :rotate_selection_contents_forward cycle selected text"
                .to_owned(),
            ":reverse_selection_contents reverse characters inside each selection".to_owned(),
            ":commit_undo_checkpoint split subsequent edits into a fresh undo step"
                .to_owned(),
            ":select_all select entire buffer".to_owned(),
            ":delete_word_backward / :delete_word_forward delete adjacent word".to_owned(),
            ":kill_to_line_start / :kill_to_line_end delete to line bound".to_owned(),
            ":kill_line remove current line".to_owned(),
            ":delete_char_backward / :delete_char_forward delete adjacent char".to_owned(),
            ":insert_newline insert line ending at cursor".to_owned(),
            ":add_newline_above / :add_newline_below insert blank line without entering insert mode"
                .to_owned(),
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

struct AlignItCommandSpec {
    pattern: String,
    regex: bool,
    occurrence: i64,
    all: bool,
    format: String,
}

fn parse_align_it_spec(spec: &str) -> Result<AlignItCommandSpec, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("align_it: usage: :align_it [N|*|-N]<delimiter>|/regex/ [l1r1l0]".to_owned());
    }

    let (occurrence, all, rest) = parse_align_it_occurrence(spec)?;
    let rest = rest.trim_start();
    if rest.is_empty() {
        return Err("align_it: usage: :align_it [N|*|-N]<delimiter>|/regex/ [l1r1l0]".to_owned());
    }

    let (pattern, regex, format) = if let Some(regex_body) = rest.strip_prefix('/') {
        let Some(end) = find_align_it_regex_end(regex_body) else {
            return Err("align_it: unterminated regex; use /.../".to_owned());
        };
        let pattern = &regex_body[..end];
        if pattern.is_empty() {
            return Err(
                "align_it: usage: :align_it [N|*|-N]<delimiter>|/regex/ [l1r1l0]".to_owned()
            );
        }
        regex::Regex::new(pattern)
            .map_err(|err| format!("align_it: invalid regex `{pattern}`: {err}"))?;
        let format = regex_body[end + 1..].trim();
        (pattern.to_owned(), true, format.to_owned())
    } else {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let pattern = parts.next().unwrap_or_default();
        if pattern.is_empty() {
            return Err(
                "align_it: usage: :align_it [N|*|-N]<delimiter>|/regex/ [l1r1l0]".to_owned()
            );
        }
        let format = parts.next().unwrap_or_default().trim().to_owned();
        (pattern.to_owned(), false, format)
    };

    if !format.is_empty() {
        validate_align_it_format(&format)?;
    }

    Ok(AlignItCommandSpec { pattern, regex, occurrence, all, format })
}

fn parse_align_it_occurrence(spec: &str) -> Result<(i64, bool, &str), String> {
    if let Some(rest) = spec.strip_prefix('*') {
        return Ok((1, true, rest));
    }

    if let Some(rest) = spec.strip_prefix('-') {
        let digits = rest.chars().take_while(|ch| ch.is_ascii_digit()).count();
        if digits == 0 {
            return Ok((-1, false, rest));
        }
        let value: i64 = rest[..digits]
            .parse()
            .map_err(|_| "align_it: invalid occurrence selector".to_owned())?;
        if value == 0 {
            return Err("align_it: occurrence selector cannot be 0".to_owned());
        }
        return Ok((-value, false, &rest[digits..]));
    }

    let digits = spec.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digits == 0 {
        return Ok((1, false, spec));
    }
    let value: i64 =
        spec[..digits].parse().map_err(|_| "align_it: invalid occurrence selector".to_owned())?;
    if value == 0 {
        return Err("align_it: occurrence selector cannot be 0".to_owned());
    }
    Ok((value, false, &spec[digits..]))
}

fn find_align_it_regex_end(spec: &str) -> Option<usize> {
    let mut escaped = false;
    for (index, ch) in spec.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '/' => return Some(index),
            _ => {}
        }
    }
    None
}

fn validate_align_it_format(spec: &str) -> Result<(), String> {
    let bytes = spec.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'l' | b'r' | b'c' => {}
            _ => {
                return Err(format!(
                    "align_it: invalid format `{spec}`; use repeated l|r|c followed by digits"
                ));
            }
        }
        index += 1;
        let digit_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if digit_start == index {
            return Err(format!(
                "align_it: invalid format `{spec}`; use repeated l|r|c followed by digits"
            ));
        }
    }
    Ok(())
}
