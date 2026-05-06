use super::*;
use crate::buffer::BufferId;

impl App {
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
                let query = tail.to_owned();
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
            "extend_line_below" => {
                self.extend_line_below();
            }
            "extend_to_line_bounds" => {
                self.extend_to_line_bounds();
            }
            "shrink_to_line_bounds" => {
                self.shrink_to_line_bounds();
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
            "ls" | "buffers" | "Buffers" => {
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
            "code_action",
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
            "g",
            "commands",
            "decrement",
            "delete_char_backward",
            "delete_char_forward",
            "delete_word_backward",
            "delete_word_forward",
            "duplicate_line",
            "files",
            "Files",
            "format",
            "grep",
            "Grep",
            "gblame",
            "gdiff",
            "ghunkdiff",
            "goto",
            "goto_column",
            "lang",
            "hs",
            "hsplit",
            "help",
            "hover",
            "increment",
            "insert_newline",
            "insert_tab",
            "keymap",
            "kill_line",
            "kill_to_line_end",
            "kill_to_line_start",
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
            "move",
            "move_parent_node_end",
            "move_parent_node_start",
            "mv",
            "n",
            "new",
            "o",
            "open",
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
            "selection_for_find",
            "selection_for_replace",
            "select_regex",
            "selection_into_lines",
            "set_language",
            "sh",
            "split_selection",
            "split_selection_on_newline",
            "merge_selections",
            "merge_consecutive_selections",
            "trim_selections",
            "collapse_selection",
            "clear_register",
            "flip_selections",
            "echo",
            "encoding",
            "ensure_selections_forward",
            "expand_selection",
            "extend_line_below",
            "extend_to_line_bounds",
            "join_selections",
            "join_selections_space",
            "keep_selections",
            "keep_primary_selection",
            "remove_selections",
            "remove_primary_selection",
            "select_all_children",
            "select_all_siblings",
            "select_next_sibling",
            "select_prev_sibling",
            "shrink_selection",
            "shrink_to_line_bounds",
            "copy_selection_on_next_line",
            "copy_selection_on_prev_line",
            "add_newline_above",
            "add_newline_below",
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

    pub(super) fn current_buffer_language(&self) -> String {
        let buf = self.backend.active();
        self.syntax_overrides
            .get(&buf.id)
            .cloned()
            .or_else(|| self.highlighter.syntax_name_for_path(buf.path.as_deref()))
            .unwrap_or_else(|| String::from("Plain Text"))
    }

    fn set_current_buffer_language(&mut self, requested: &str) -> Result<String, String> {
        let language = self
            .highlighter
            .canonical_syntax_name(requested)
            .ok_or_else(|| format!("set_language: unknown language `{requested}`"))?;
        self.syntax_overrides.insert(self.backend.active().id, language.clone());
        Ok(language)
    }

    fn reload_runtime_config(&mut self) -> Result<String, String> {
        let active_path = self.backend.active().path.clone();
        self.config = crate::config::load_config(active_path.as_deref());
        self.key_bindings = crate::keymap::bindings_for(&self.config.keymap);
        self.backend
            .reload_editor_config()
            .map_err(|err| format!("config reload failed: {err}"))?;
        Ok(String::from("config reloaded"))
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
        let pristine = self.backend.active().pristine;
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

    fn save_current_buffer(&mut self) -> Result<(), String> {
        self.backend.save().map_err(|err| format!("save failed: {err}"))
    }

    fn save_all_dirty_buffers(&mut self) -> Result<(), String> {
        let dirty_ids = self
            .backend
            .all_bufs()
            .iter()
            .filter(|buf| !buf.pristine)
            .map(|buf| buf.id)
            .collect::<Vec<_>>();
        for id in dirty_ids {
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
            "Workspace: :files :bpick :grep :buffers :split :hsplit :vsplit :tabnew :new".to_owned(),
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
            ":run_shell_command / :sh aliases for :term shell-command".to_owned(),
            ":make [args] run cargo build in transcript buffer".to_owned(),
            ":test [args] run cargo test in transcript buffer".to_owned(),
            ":run [args] run cargo run in transcript buffer".to_owned(),
            ":open / :o open file in current view | :new create scratch buffer".to_owned(),
            ":hsplit / :hs open file in horizontal split | :goto / :g jump to line".to_owned(),
            ":goto_column <column> move cursor to 1-based display column on current line".to_owned(),
            ":write! :write_all :write_quit :write_quit_all add Vim-style aliases".to_owned(),
            ":buffer_close :buffer_close_others :buffer_close_all manage buffers".to_owned(),
            ":reload / :reload_all discard edits and reopen from disk".to_owned(),
            ":reload_config refresh frontend config and keymap overrides".to_owned(),
            ":set_language / :lang set or show current syntax name".to_owned(),
            ":read / :r insert file contents at cursor".to_owned(),
            ":move / :mv move current buffer to new path".to_owned(),
            ":encoding show or set current buffer encoding metadata".to_owned(),
            ":clear_register [name] clear one register or all registers".to_owned(),
            ":echo print arguments to status line | :redraw clear and repaint UI".to_owned(),
            ":gblame show git blame metadata for current line".to_owned(),
            ":gdiff open git diff for current buffer in scratch view".to_owned(),
            ":ghunkdiff open git diff for current hunk in scratch view".to_owned(),
            ":complete open completion picker from backend suggestions".to_owned(),
            ":codeaction / :code_action open backend code-action picker".to_owned(),
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
            ":extend_line_below / :extend_to_line_bounds / :shrink_to_line_bounds rewrite line selections"
                .to_owned(),
            ":join_selections / :join_selections_space join selected lines".to_owned(),
            ":keep_selections [regex] / :remove_selections [regex] filter selections"
                .to_owned(),
            ":keep_primary_selection / :remove_primary_selection keep or drop rightmost selection"
                .to_owned(),
            ":expand_selection / :shrink_selection grow or restore syntax-node selections"
                .to_owned(),
            ":select_prev_sibling / :select_next_sibling / :select_all_siblings / :select_all_children syntax-tree multi-selection"
                .to_owned(),
            ":move_parent_node_start / :move_parent_node_end jump cursor to parent syntax node boundary"
                .to_owned(),
            ":copy_selection_on_next_line / :copy_selection_on_prev_line clone selection to adjacent line"
                .to_owned(),
            ":rotate_selection_contents_backward / :rotate_selection_contents_forward cycle selected text"
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
