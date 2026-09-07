//! `impl App` command methods: execute.
use super::*;

impl App {
    pub(in crate::app) fn execute_command(&mut self) {
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
        let command = Self::rewrite_command_alias(command);
        let command = command.as_ref();

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
                    && let Err(message) = self.save_current_buffer().map(|_| ())
                {
                    self.backend.status_message = Some(message);
                }
            }
            "wq" | "x" | "wq!" | "x!" | "write_quit" | "write_quit!" => {
                match self.save_current_buffer() {
                    Err(message) => self.backend.status_message = Some(message),
                    Ok(SaveOutcome::Saved) => self.should_quit = true,
                    Ok(SaveOutcome::AwaitingPrivilegeConfirm) => {}
                }
            }
            "wa" | "wa!" | "write_all" | "write_all!" => {
                if let Err(message) = self.save_all_dirty_buffers() {
                    self.backend.status_message = Some(message);
                }
            }
            "wqa" | "xa" | "wqa!" | "xa!" | "write_quit_all" | "write_quit_all!" => {
                match self.save_all_dirty_buffers() {
                    Err(message) => self.backend.status_message = Some(message),
                    Ok(SaveOutcome::Saved) => self.should_quit = true,
                    Ok(SaveOutcome::AwaitingPrivilegeConfirm) => {}
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
            "expandtab" => {
                let _ = self.backend.send_edit(
                    "expand_tabs",
                    json!({ "range": range.map(|(start, end)| [start as i64, end as i64]) }),
                );
            }
            "reindent" => {
                let _ = self.backend.send_edit("reindent", json!([]));
            }
            "reflow" => {
                let Some(width) = parts
                    .next()
                    .and_then(|part| part.parse::<usize>().ok())
                    .filter(|width| *width > 0)
                else {
                    self.backend.status_message = Some("reflow: usage: :reflow <width>".to_owned());
                    return;
                };
                if parts.next().is_some() {
                    self.backend.status_message = Some("reflow: usage: :reflow <width>".to_owned());
                    return;
                }
                let _ = self.backend.send_edit(
                    "reflow_lines",
                    json!({
                        "width": width,
                        "range": range.map(|(start, end)| [start as i64, end as i64]),
                    }),
                );
            }
            "renormalize" => {
                let _ = self
                    .backend
                    .send_edit("normalize_line_endings", json!({ "line_ending": "\n" }));
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
            "logs" => {
                self.open_logs_picker();
                self.enter_normal_mode();
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
                let _ = self.backend.send_edit(
                    "sort_lines",
                    json!({
                        "descending": false,
                        "range": range.map(|(start, end)| [start as i64, end as i64]),
                    }),
                );
            }
            "rsort" => {
                let _ = self.backend.send_edit(
                    "sort_lines",
                    json!({
                        "descending": true,
                        "range": range.map(|(start, end)| [start as i64, end as i64]),
                    }),
                );
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
            "edit_config" => {
                if let Err(message) = self.edit_nearest_config() {
                    self.backend.status_message = Some(message);
                }
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
                            Ok(path) => {
                                self.working_dir = path.clone();
                                format!("cwd: {}", path.display())
                            }
                            Err(err) => format!("cd failed: {err}"),
                        },
                        Err(err) => format!("cd failed: {err}"),
                    }
                });
            }
            "show_directory" | "pwd" => {
                self.backend.status_message = Some(format!("cwd: {}", self.working_dir.display()));
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
                let buf_id = if let Some(path) = path {
                    match self.open_or_reuse_buffer(path) {
                        Ok(id) => id,
                        Err(err) => {
                            self.backend.status_message = Some(err);
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
                let buf_id = if let Some(path) = path {
                    match self.open_or_reuse_buffer(path) {
                        Ok(id) => id,
                        Err(err) => {
                            self.backend.status_message = Some(err);
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
            "agents"
            | "agents_clear"
            | "agents_close"
            | "agents_config"
            | "agents_config_set"
            | "agents_config_toggle"
            | "agents_layout"
            | "agents_thoughts"
            | "agents_mcp"
            | "agents_mode_next"
            | "agents_mode_prev"
            | "agents_new"
            | "agents_threads"
            | "agents_next"
            | "agents_prev"
            | "agents_stop"
            | "agents_resume"
            | "agents_discard"
            | "agents_reconnect"
                if self.dispatch_agents_command(head, tail) =>
            {
                // The agents pane keeps keyboard focus (or restores its
                // previous editor mode); skip the trailing
                // `enter_normal_mode` but still clear the command line.
                self.command_buffer.clear();
                // Commands run from the pane's `:` command line must
                // return focus to the pane.
                if self.mode == Mode::CommandLine && self.agents_pane_open() {
                    self.mode = Mode::Agent;
                }
                return;
            }
            "agents"
            | "agents_clear"
            | "agents_close"
            | "agents_config"
            | "agents_config_set"
            | "agents_config_toggle"
            | "agents_layout"
            | "agents_thoughts"
            | "agents_mcp"
            | "agents_mode_next"
            | "agents_mode_prev"
            | "agents_new"
            | "agents_threads"
            | "agents_next"
            | "agents_prev"
            | "agents_stop"
            | "agents_resume"
            | "agents_discard"
            | "agents_reconnect" => {}
            other if !other.is_empty() => {
                self.backend.status_message = Some(format!("unknown command: {other}"));
            }
            _ => {}
        }
        self.enter_normal_mode();
    }
}
