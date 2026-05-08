use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use serde_json::json;
use xi_core_lib::plugin_rpc::{CodeActionDescriptor, SelectionRange};
use xi_core_lib::rpc::LineReplacement;

use crate::backend::{CompletionSuggestion, PendingUiAction};
use crate::buffer::BufferManager;
use crate::folds::{FoldStore, indent_fold_extent};
use crate::git::{self, GitBufferCache, GitBufferStatus};
use crate::keymap::{Action, BindingKey, SequenceNode};
use crate::picker::PickerState;
use crate::quickfix::{QfEntry, QfList};
use crate::registers::{BlockInsert, LastChange, RegisterName, RegisterStore};
use crate::text::byte_col_to_display_col;
use crate::window::{SplitDir, TabManager, ViewDirection};

mod commands;
mod parsing;
mod state;

const VLF_SOURCE_CONTROL_DISABLED_REASON: &str = "requires whole-buffer diff/blame scans";

pub(crate) use parsing::{
    line_col_for_offset, parse_ex_range, parse_substitute_cmd, smart_case_sensitive,
    text_obj_bracket, text_obj_quote, text_obj_tag, text_obj_word,
};
pub(crate) use state::{
    App, HoverPopup, Mode, Operator, PendingCharFind, RepeatableMotion, SubstitutePending, Viewport,
};

impl App {
    pub(crate) fn command_history(&self) -> &[String] {
        &self.command_history
    }

    pub(crate) fn restore_command_history(&mut self, mut history: Vec<String>) {
        const HISTORY_MAX: usize = 100;
        if history.len() > HISTORY_MAX {
            let keep_from = history.len() - HISTORY_MAX;
            history.drain(..keep_from);
        }
        self.command_history = history;
        self.history_idx = None;
        self.history_draft.clear();
    }

    pub(crate) fn handle_event(&mut self, event: Event) {
        self.expire_key_sequence_if_idle();
        self.last_input_at = Instant::now();

        match event {
            Event::Mouse(m) => {
                self.handle_mouse_event(m);
                return;
            }
            Event::Paste(text) => {
                self.handle_paste(text);
                return;
            }
            _ => {}
        }

        let Event::Key(mut key) = event else {
            return;
        };

        if matches!(key.code, KeyCode::Char('\r' | '\n'))
            || (key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('m' | 'j')))
        {
            key.code = KeyCode::Enter;
            key.modifiers.remove(KeyModifiers::CONTROL);
        }

        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return;
        }

        if self.picker.is_some() {
            if self.dispatch_context_key(key, Mode::Picker) {
                return;
            }
            self.handle_picker_text_input(key);
            return;
        }

        if self.hover_popup.is_some() && key.code == KeyCode::Esc {
            self.hover_popup = None;
            return;
        }

        if self.quickfix_focused {
            self.dispatch_context_key(key, Mode::Quickfix);
            return;
        }
        if self.location_list_focused {
            self.dispatch_context_key(key, Mode::LocationList);
            return;
        }

        if self.mode == Mode::SubstituteConfirm {
            self.dispatch_context_key(key, Mode::SubstituteConfirm);
            return;
        }

        // Capture keystrokes for the active macro recording (before processing).
        // We always push first and pop afterward if this key stops recording,
        // so the terminating `q` is not stored in the macro.
        if self.macro_register.is_some() && !self.macro_replaying {
            self.macro_buffer.push(key);
        }

        // Two-char awaiting states consume the next key unconditionally.
        if self.input_state.awaiting_register {
            self.input_state.awaiting_register = false;
            if self.input_state.awaiting_register_insert {
                self.input_state.awaiting_register_insert = false;
                if let KeyCode::Char(c) = key.code
                    && let Some(register) = RegisterName::from_char(c)
                {
                    self.insert_register_for_current_mode(register);
                }
                return;
            }
            if let KeyCode::Char(c) = key.code {
                let append = RegisterName::is_append_char(c);
                self.input_state.pending_register = RegisterName::from_char(c);
                if append {
                    let _ = append;
                }
            }
            return;
        }

        if self.input_state.awaiting_mark_set {
            self.input_state.awaiting_mark_set = false;
            if let KeyCode::Char(c) = key.code {
                if c.is_ascii_lowercase() {
                    self.set_mark(c);
                }
            }
            return;
        }

        if let Some(line_start) = self.input_state.awaiting_mark_jump.take() {
            if let KeyCode::Char(c) = key.code {
                self.jump_to_mark(c, line_start);
            }
            return;
        }

        if self.input_state.awaiting_macro_record {
            self.input_state.awaiting_macro_record = false;
            if let KeyCode::Char(c) = key.code {
                if c.is_ascii_lowercase() {
                    self.start_macro_record(c);
                }
            }
            return;
        }

        if self.input_state.awaiting_macro_replay {
            self.input_state.awaiting_macro_replay = false;
            if let KeyCode::Char(c) = key.code {
                let reg = if c == '@' {
                    self.last_macro
                } else if c.is_ascii_lowercase() {
                    Some(c)
                } else {
                    None
                };
                if let Some(r) = reg {
                    self.replay_macro(r);
                }
            }
            return;
        }

        if self.input_state.awaiting_window_cmd {
            self.input_state.awaiting_window_cmd = false;
            if let KeyCode::Char(c) = key.code {
                self.handle_window_cmd(c);
            }
            return;
        }

        if self.input_state.awaiting_replace_char {
            self.input_state.awaiting_replace_char = false;
            if let KeyCode::Char(c) = key.code {
                self.replace_with_char(c);
            }
            return;
        }

        if self.handle_key_sequence(key) {
            return;
        }

        let action = self.lookup_action(key, self.mode, self.input_state.prefix);

        if let Some(action) = action {
            self.dispatch(action, key);
            if self.mode == Mode::Normal
                && !matches!(key.code, KeyCode::Char(c) if c.is_ascii_digit())
                && self.input_state.prefix.is_none()
                && self.input_state.pending_find.is_none()
                && !self.input_state.awaiting_register
                && !self.input_state.awaiting_mark_set
                && self.input_state.awaiting_mark_jump.is_none()
                && !self.input_state.awaiting_macro_record
                && !self.input_state.awaiting_macro_replay
                && !self.input_state.awaiting_window_cmd
                && !self.input_state.awaiting_replace_char
            {
                self.input_state.reset();
            }
        } else {
            self.handle_default(key);
        }
    }

    fn lookup_action(&self, key: KeyEvent, mode: Mode, prefix: Option<char>) -> Option<Action> {
        let binding_key = BindingKey { mode, key: key.code, modifiers: key.modifiers, prefix };

        self.key_bindings
            .get(&binding_key)
            .or_else(|| {
                if key.modifiers != KeyModifiers::NONE {
                    self.key_bindings
                        .get(&BindingKey { modifiers: KeyModifiers::NONE, ..binding_key })
                } else {
                    None
                }
            })
            .cloned()
    }

    pub(crate) fn active_key_sequence_node(&self) -> Option<&SequenceNode> {
        if self.input_state.key_sequence.is_empty() {
            return None;
        }
        self.key_sequences.node_for_sequence(self.mode, &self.input_state.key_sequence)
    }

    pub(crate) fn active_key_sequence_label(&self) -> Option<String> {
        self.active_key_sequence_node()?;
        Some(crate::keymap::format_key_sequence(&self.input_state.key_sequence))
    }

    pub(crate) fn expire_key_sequence_if_idle(&mut self) {
        self.expire_key_sequence_if_idle_at(Instant::now());
    }

    pub(crate) fn expire_key_sequence_if_idle_at(&mut self, now: Instant) {
        let Some(last_input_at) = self.input_state.key_sequence_last_input_at else {
            return;
        };
        let timeout_ms = self.config.keymap.sequence_timeout_ms;
        if timeout_ms == 0 {
            return;
        }
        if now.duration_since(last_input_at) >= Duration::from_millis(timeout_ms) {
            self.clear_active_key_sequence();
        }
    }

    fn clear_active_key_sequence(&mut self) {
        self.input_state.key_sequence.clear();
        self.input_state.key_sequence_last_input_at = None;
    }

    fn can_start_key_sequence(&self) -> bool {
        matches!(
            self.mode,
            Mode::Normal | Mode::Insert | Mode::Visual | Mode::VisualLine | Mode::VisualBlock
        ) && self.input_state.pending_find.is_none()
            && self.input_state.pending_operator.is_none()
            && self.input_state.text_obj_inclusive.is_none()
            && self.input_state.prefix.is_none()
            && !self.input_state.awaiting_register
            && !self.input_state.awaiting_register_insert
            && !self.input_state.awaiting_mark_set
            && self.input_state.awaiting_mark_jump.is_none()
            && !self.input_state.awaiting_macro_record
            && !self.input_state.awaiting_macro_replay
            && !self.input_state.awaiting_window_cmd
            && !self.input_state.awaiting_replace_char
            && self.mode != Mode::OperatorPending
    }

    fn handle_key_sequence(&mut self, key: KeyEvent) -> bool {
        let now = Instant::now();

        if !self.key_sequences.has_mode(self.mode) {
            self.clear_active_key_sequence();
            return false;
        }

        if key.code == KeyCode::Esc && !self.input_state.key_sequence.is_empty() {
            self.clear_active_key_sequence();
            self.backend.status_message = Some(String::from("key sequence cancelled"));
            return true;
        }

        if self.input_state.key_sequence.is_empty() && !self.can_start_key_sequence() {
            return false;
        }

        let key_press = crate::keymap::key_press_from_event(key);
        let attempted = if self.input_state.key_sequence.is_empty() {
            vec![key_press]
        } else {
            let mut sequence = self.input_state.key_sequence.clone();
            sequence.push(key_press);
            sequence
        };

        let Some((matched_sequence, has_children, action)) = self
            .key_sequences
            .advance(self.mode, &self.input_state.key_sequence, key_press)
            .map(|(matched_sequence, node)| {
                (matched_sequence, !node.children.is_empty(), node.action.clone())
            })
        else {
            if self.input_state.key_sequence.is_empty() {
                return false;
            }
            if self.mode == Mode::Insert
                && let Some(text) = self.literal_text_for_key_sequence(&attempted)
            {
                self.clear_active_key_sequence();
                self.insert_buffer.push_str(&text);
                let _ = self.backend.send_edit("insert", json!({ "chars": text }));
                self.backend.status_message = None;
                return true;
            }
            self.clear_active_key_sequence();
            self.backend.status_message =
                Some(format!("no binding: {}", crate::keymap::format_key_sequence(&attempted)));
            return true;
        };

        self.input_state.key_sequence = matched_sequence;
        self.input_state.key_sequence_last_input_at = Some(now);
        self.backend.status_message = None;

        if !has_children {
            let Some(action) = action else {
                self.clear_active_key_sequence();
                return true;
            };
            self.clear_active_key_sequence();
            self.dispatch(action, key);
        }

        true
    }

    fn literal_text_for_key_sequence(
        &self,
        sequence: &[crate::keymap::KeyPress],
    ) -> Option<String> {
        let mut text = String::new();
        for key in sequence {
            if key.modifiers.contains(KeyModifiers::CONTROL)
                || key.modifiers.contains(KeyModifiers::ALT)
            {
                return None;
            }
            match key.key {
                KeyCode::Char(ch) => text.push(ch),
                KeyCode::Enter => text.push('\n'),
                _ => return None,
            }
        }
        Some(text)
    }

    fn dispatch_context_key(&mut self, key: KeyEvent, mode: Mode) -> bool {
        let Some(action) = self.lookup_action(key, mode, None) else {
            return false;
        };
        self.dispatch(action, key);
        true
    }

    fn handle_picker_text_input(&mut self, key: KeyEvent) {
        if let KeyCode::Char(c) = key.code
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
            && let Some(picker) = self.picker.as_mut()
        {
            picker.push_char(c);
        }
    }

    fn dispatch(&mut self, action: Action, _key: KeyEvent) {
        match &action {
            Action::NoOp
            | Action::SetPrefix(_)
            | Action::PendingCharFind { .. }
            | Action::SetOperator(_) => {}
            _ => {
                self.input_state.prefix = None;
                self.input_state.pending_find = None;
            }
        }

        match action {
            Action::NoOp => {}
            Action::Quit => self.should_quit = true,
            Action::EnterMode(mode) => {
                if mode == Mode::Normal {
                    self.enter_normal_mode();
                } else {
                    if mode.is_visual() {
                        // Set anchor at current cursor position.
                        self.visual_anchor =
                            Some((self.backend.cursor_line, self.backend.cursor_col));
                    }
                    self.mode = mode;
                }
            }
            Action::EnterCommandMode => {
                self.mode = Mode::CommandLine;
                self.command_buffer.clear();
            }
            Action::PrefillCommandLine(template) => {
                self.mode = Mode::CommandLine;
                self.command_buffer = template.to_owned();
            }
            Action::EnterSearch => {
                self.mode = Mode::Search;
                self.search_backward = false;
                self.command_buffer.clear();
                let _ = self.backend.send_edit(
                    "find",
                    json!({
                        "chars": "",
                        "case_sensitive": false,
                        "regex": false,
                        "whole_words": false
                    }),
                );
                let _ = self.backend.send_edit("highlight_find", json!({ "visible": true }));
            }
            Action::EnterSearchBackward => {
                self.mode = Mode::Search;
                self.search_backward = true;
                self.command_buffer.clear();
                let _ = self.backend.send_edit(
                    "find",
                    json!({
                        "chars": "",
                        "case_sensitive": false,
                        "regex": false,
                        "whole_words": false
                    }),
                );
                let _ = self.backend.send_edit("highlight_find", json!({ "visible": true }));
            }
            Action::ExecuteSearch => self.execute_search(),
            Action::CompleteCommandLine => self.complete_command(),
            Action::Edit(method) => {
                let count = self.input_state.count();
                // Push jump list before document-level jumps.
                match method {
                    "move_to_beginning_of_document"
                    | "move_to_end_of_document"
                    | "scroll_page_down"
                    | "scroll_page_up" => self.push_jump(),
                    _ => {}
                }
                if self.handle_vlf_navigation(method, u64::from(count)) {
                    return;
                }
                for _ in 0..count {
                    let _ = self.backend.send_edit(method, json!([]));
                }
            }
            Action::CollapseAndEnterNormal => {
                if !self.backend.is_vlf {
                    let _ = self.backend.send_edit("collapse_selections", json!([]));
                }
                self.enter_normal_mode();
            }
            Action::ExecuteCommand => self.execute_command(),
            Action::DeleteBackward => {
                let count = self.input_state.count();
                for _ in 0..count {
                    let _ = self.backend.send_edit("delete_backward", json!([]));
                }
            }
            Action::CommandBackspace => {
                self.command_buffer.pop();
            }
            Action::SearchBackspace => {
                self.command_buffer.pop();
                let chars = self.command_buffer.clone();
                let case_sensitive = smart_case_sensitive(&chars);
                let _ = self.backend.send_edit(
                    "find",
                    json!({
                        "chars": chars,
                        "case_sensitive": case_sensitive,
                        "regex": false,
                        "whole_words": false
                    }),
                );
            }
            Action::FindNext => {
                self.push_jump();
                let _ = self
                    .backend
                    .send_edit("find_next", json!({ "wrap_around": true, "allow_same": false }));
            }
            Action::FindPrevious => {
                self.push_jump();
                let _ = self.backend.send_edit(
                    "find_previous",
                    json!({ "wrap_around": true, "allow_same": false }),
                );
            }
            Action::RequestCompletion => {
                if let Err(err) = self.backend.request_completion(None) {
                    self.backend.status_message = Some(format!("completion failed: {err}"));
                }
            }
            Action::RequestHover => {
                let position = Some((self.backend.cursor_line, self.backend.cursor_col));
                if let Err(err) = self.backend.request_hover(position) {
                    self.backend.status_message = Some(format!("hover failed: {err}"));
                }
            }
            Action::RequestDeclaration => {
                if let Err(err) = self.backend.request_declaration() {
                    self.backend.status_message = Some(format!("declaration failed: {err}"));
                }
            }
            Action::RequestDefinition => {
                if let Err(err) = self.backend.request_definition() {
                    self.backend.status_message = Some(format!("definition failed: {err}"));
                }
            }
            Action::RequestTypeDefinition => {
                if let Err(err) = self.backend.request_type_definition() {
                    self.backend.status_message = Some(format!("type definition failed: {err}"));
                }
            }
            Action::RequestReferences => {
                if let Err(err) = self.backend.request_references() {
                    self.backend.status_message = Some(format!("references failed: {err}"));
                }
            }
            Action::RequestImplementation => {
                if let Err(err) = self.backend.request_implementation() {
                    self.backend.status_message = Some(format!("implementation failed: {err}"));
                }
            }
            Action::RequestDocumentSymbols => {
                if let Err(err) = self.backend.request_document_symbols() {
                    self.backend.status_message = Some(format!("symbols failed: {err}"));
                }
            }
            Action::RequestWorkspaceSymbols => {
                if let Err(err) = self.backend.request_workspace_symbols("") {
                    self.backend.status_message = Some(format!("workspace symbols failed: {err}"));
                }
            }
            Action::RequestCodeActions => {
                if let Err(err) = self.backend.request_code_actions(None) {
                    self.backend.status_message = Some(format!("code action failed: {err}"));
                }
            }
            Action::FilePicker => self.open_file_picker_for_buffer_directory(),
            Action::FilePickerInCurrentDirectory => self.open_file_picker_in_current_directory(),
            Action::FileExplorer => self.open_file_explorer(),
            Action::FileExplorerInCurrentBufferDirectory => {
                self.open_file_explorer_for_buffer_directory()
            }
            Action::FileExplorerInCurrentDirectory => {
                self.open_file_explorer_in_current_directory()
            }
            Action::BufferPicker => self.open_buffer_picker(),
            Action::JumpListPicker => self.open_jump_list_picker(),
            Action::ChangedFilePicker => self.open_changed_file_picker(),
            Action::DiagnosticsPicker => self.open_diagnostics_picker(),
            Action::WorkspaceDiagnosticsPicker => self.open_workspace_diagnostics_picker(),
            Action::LastPicker => self.reopen_last_picker(),
            Action::PickerClose => {
                self.picker = None;
            }
            Action::PickerConfirm => self.handle_picker_confirm(),
            Action::PickerMoveUp => {
                if let Some(picker) = self.picker.as_mut() {
                    picker.move_up();
                }
            }
            Action::PickerMoveDown => {
                if let Some(picker) = self.picker.as_mut() {
                    picker.move_down();
                }
            }
            Action::PickerBackspace => {
                if let Some(picker) = self.picker.as_mut() {
                    picker.pop_char();
                }
            }
            Action::QuickfixClose => {
                self.quickfix_focused = false;
            }
            Action::QuickfixConfirm => self.confirm_focused_list(true),
            Action::QuickfixMoveUp => self.move_focused_list(true, false),
            Action::QuickfixMoveDown => self.move_focused_list(true, true),
            Action::LocationListClose => {
                self.location_list_focused = false;
            }
            Action::LocationListConfirm => self.confirm_focused_list(false),
            Action::LocationListMoveUp => self.move_focused_list(false, false),
            Action::LocationListMoveDown => self.move_focused_list(false, true),
            Action::SubstituteConfirmApply => self.apply_substitute_current(),
            Action::SubstituteConfirmSkip => self.advance_substitute_confirm(),
            Action::SubstituteConfirmApplyAll => self.apply_all_substitute_matches(),
            Action::SubstituteConfirmCancel => self.cancel_substitute_confirm(),
            Action::RegisterPrefix => {
                self.input_state.awaiting_register = true;
            }
            Action::InsertRegister => {
                self.input_state.awaiting_register = true;
                self.input_state.awaiting_register_insert = true;
            }
            Action::GlobalSearch => self.open_global_search(),
            Action::CommandPalette => self.open_command_palette(),
            Action::MarkSetPrefix => {
                self.input_state.awaiting_mark_set = true;
            }
            Action::MarkJumpPrefix { line_start } => {
                self.input_state.awaiting_mark_jump = Some(line_start);
            }
            Action::MacroRecordToggle => {
                if self.macro_register.is_some() {
                    self.stop_macro_record();
                } else {
                    self.input_state.awaiting_macro_record = true;
                }
            }
            Action::MacroReplayPrefix => {
                self.input_state.awaiting_macro_replay = true;
            }
            Action::WindowCommandPrefix => {
                self.input_state.awaiting_window_cmd = true;
            }
            Action::SearchWordUnderCursor { forward } => {
                // Use word under cursor (or visual selection) as search pattern via xi's
                // selection_for_find RPC, then navigate forward or backward.
                let _ =
                    self.backend.send_edit("selection_for_find", json!({ "case_sensitive": true }));
                let _ = self.backend.send_edit("highlight_find", json!({ "visible": true }));
                self.push_jump();
                if forward {
                    let _ = self.backend.send_edit(
                        "find_next",
                        json!({ "wrap_around": true, "allow_same": false }),
                    );
                } else {
                    let _ = self.backend.send_edit(
                        "find_previous",
                        json!({ "wrap_around": true, "allow_same": false }),
                    );
                }
                self.search_pattern = self.current_search_pattern();
                if matches!(self.mode, Mode::Visual | Mode::VisualLine | Mode::VisualBlock) {
                    self.enter_normal_mode();
                }
            }
            Action::SearchSelection { detect_word_boundaries } => {
                self.search_current_selection(detect_word_boundaries);
            }
            Action::FindAll => {
                // Select all occurrences of the current search pattern in the buffer.
                let _ = self.backend.send_edit("find_all", json!([]));
                self.mode = Mode::Visual;
            }
            Action::SetPrefix(c) => {
                self.input_state.prefix = Some(c);
            }
            Action::PendingCharFind { forward, inclusive } => {
                self.input_state.pending_find = Some(PendingCharFind { forward, inclusive });
            }
            Action::MoveWordStart { forward, long_word } => {
                self.move_word_start(forward, long_word);
            }
            Action::MoveWordEnd { long_word } => {
                self.move_word_end(long_word);
            }
            Action::GotoFirstNonWhitespace => self.goto_first_nonwhitespace(),
            Action::GotoLine => self.goto_line_from_count(),
            Action::GotoColumn => self.goto_column_from_count(),
            Action::GotoFileStart => self.goto_file_start_from_count(),
            Action::GotoLastLine => self.goto_last_line(),
            Action::GotoFile => self.goto_file_under_cursor(),
            Action::GotoWindowTop => self.goto_window_top(),
            Action::GotoWindowCenter => self.goto_window_center(),
            Action::GotoWindowBottom => self.goto_window_bottom(),
            Action::GotoLastAccessedFile => self.goto_last_accessed_file(),
            Action::GotoLastModifiedFile => self.goto_last_modified_file(),
            Action::SaveSelection => self.push_jump(),
            Action::RepeatLastMotion => self.repeat_last_motion(),
            Action::PageCursorHalfUp => self.page_cursor_half(false),
            Action::PageCursorHalfDown => self.page_cursor_half(true),
            Action::Replace => {
                self.input_state.awaiting_replace_char = true;
            }
            Action::ReplaceWithYanked => self.replace_with_yanked(),
            Action::SwitchCase => self.apply_case_transform("capitalize"),
            Action::SwitchToLowercase => self.apply_case_transform("lowercase"),
            Action::SwitchToUppercase => self.apply_case_transform("uppercase"),
            Action::YankSelection => self.yank_selection(),
            Action::YankToClipboard => self.yank_selection_to_register(RegisterName::Clipboard),
            Action::YankToPrimaryClipboard => {
                self.yank_selection_to_register(RegisterName::PrimaryClipboard)
            }
            Action::YankMainSelectionToClipboard => {
                self.yank_main_selection_to_register(RegisterName::Clipboard)
            }
            Action::YankMainSelectionToPrimaryClipboard => {
                self.yank_main_selection_to_register(RegisterName::PrimaryClipboard)
            }
            Action::IndentSelection => self.apply_selection_edit("indent", true),
            Action::UnindentSelection => self.apply_selection_edit("outdent", true),
            Action::FormatSelections => self.format_selections(),
            Action::ExtendLineBelow => self.extend_line_below(),
            Action::ExtendToLineBounds => self.extend_to_line_bounds(),
            Action::ShrinkToLineBounds => self.shrink_to_line_bounds(),
            Action::JoinSelections => self.join_selections(false),
            Action::JoinSelectionsSpace => self.join_selections(true),
            Action::KeepSelections => self.filter_selections_from_search(false),
            Action::RemoveSelections => self.filter_selections_from_search(true),
            Action::ExpandSelection => {
                let _ = self.backend.send_edit("expand_selection", json!([]));
            }
            Action::ShrinkSelection => {
                let _ = self.backend.send_edit("shrink_selection", json!([]));
            }
            Action::SelectPrevSibling => {
                let _ = self.backend.send_edit("select_prev_sibling", json!([]));
            }
            Action::SelectNextSibling => {
                let _ = self.backend.send_edit("select_next_sibling", json!([]));
            }
            Action::SelectAllSiblings => {
                let _ = self.backend.send_edit("select_all_siblings", json!([]));
            }
            Action::SelectAllChildren => {
                let _ = self.backend.send_edit("select_all_children", json!([]));
            }
            Action::MoveParentNodeStart => {
                let _ = self.backend.send_edit("move_parent_node_start", json!([]));
            }
            Action::MoveParentNodeEnd => {
                let _ = self.backend.send_edit("move_parent_node_end", json!([]));
            }
            Action::DeleteSelection { yank, enter_insert } => {
                self.delete_selection(yank, enter_insert)
            }
            Action::MatchingPair => {
                self.push_jump();
                self.last_repeatable_motion = Some(RepeatableMotion::MatchingPair);
                self.jump_matching_bracket();
            }
            // Operator-pending mode
            Action::SetOperator(op) => {
                self.input_state.prefix = None;
                self.input_state.pending_operator = Some(op);
                self.mode = Mode::OperatorPending;
            }
            // Insert-entry variants
            Action::AppendAfterCursor => {
                let _ = self.backend.send_edit("move_right", json!([]));
                self.mode = Mode::Insert;
            }
            Action::AppendAtEndOfLine => {
                let _ = self.backend.send_edit("move_to_right_end_of_line", json!([]));
                self.mode = Mode::Insert;
            }
            Action::InsertAtLineStart => {
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                self.mode = Mode::Insert;
            }
            Action::OpenLineBelow => {
                let _ = self.backend.send_edit("move_to_right_end_of_line", json!([]));
                let _ = self.backend.send_edit("insert_newline", json!([]));
                self.mode = Mode::Insert;
            }
            Action::OpenLineAbove => {
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                let _ = self.backend.send_edit("insert_newline", json!([]));
                let _ = self.backend.send_edit("move_up", json!([]));
                self.mode = Mode::Insert;
            }
            Action::SubstituteChar => {
                let count = self.input_state.count();
                for _ in 0..count {
                    let _ = self.backend.send_edit("delete_forward", json!([]));
                }
                self.mode = Mode::Insert;
            }
            Action::SubstituteLine => {
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                let _ = self.backend.send_edit("delete_to_end_of_paragraph", json!([]));
                self.mode = Mode::Insert;
            }
            // Insert mode editing controls
            Action::DeleteWordBackward => {
                let _ = self.backend.send_edit("delete_word_backward", json!([]));
            }
            Action::DeleteToLineStart => {
                let _ = self.backend.send_edit("delete_to_beginning_of_line", json!([]));
            }
            Action::AddNewlineBelow => {
                self.add_newline_below();
            }
            Action::AddNewlineAbove => {
                self.add_newline_above();
            }
            Action::DeleteCurrentLine => {
                let count = usize::try_from(self.input_state.count()).unwrap_or(usize::MAX);
                let start = self.backend.cursor_line;
                let end = start.saturating_add(count.saturating_sub(1));
                self.delete_line_range(start, end);
            }
            Action::IndentLine => {
                let _ = self.backend.send_edit("indent", json!([]));
            }
            Action::OutdentLine => {
                let _ = self.backend.send_edit("outdent", json!([]));
            }
            // ── Undo / Redo ──────────────────────────────────────────────────
            Action::Undo => {
                let _ = self.backend.send_edit("undo", json!([]));
            }
            Action::Redo => {
                let _ = self.backend.send_edit("redo", json!([]));
            }
            // ── Repeat last change (`.`) ─────────────────────────────────────
            Action::RepeatLastChange => self.repeat_last_change(),
            // ── Paste ────────────────────────────────────────────────────────
            Action::PasteAfter => self.paste(false),
            Action::PasteBefore => self.paste(true),
            Action::PasteClipboardAfter => self.paste_from_register(RegisterName::Clipboard, false),
            Action::PasteClipboardBefore => self.paste_from_register(RegisterName::Clipboard, true),
            Action::PastePrimaryClipboardAfter => {
                self.paste_from_register(RegisterName::PrimaryClipboard, false)
            }
            Action::PastePrimaryClipboardBefore => {
                self.paste_from_register(RegisterName::PrimaryClipboard, true)
            }
            Action::ReplaceSelectionsWithClipboard => {
                self.replace_selections_with_register(RegisterName::Clipboard)
            }
            Action::ReplaceSelectionsWithPrimaryClipboard => {
                self.replace_selections_with_register(RegisterName::PrimaryClipboard)
            }
            // ── Visual modes ─────────────────────────────────────────────────
            Action::EnterVisualLine => self.enter_visual_line(),
            Action::EnterVisualBlock => self.enter_visual_block(),
            Action::SwapVisualAnchor => self.swap_visual_anchor(),
            Action::RestoreLastVisual => self.restore_last_visual(),
            // ── Visual block insert / append ─────────────────────────────────
            Action::VisualBlockInsert => self.visual_block_insert(false),
            Action::VisualBlockAppend => self.visual_block_insert(true),
            // ── Jump list ────────────────────────────────────────────────────
            Action::JumpListOlder => self.jump_list_older(),
            Action::JumpListNewer => self.jump_list_newer(),
            // ── Change list ──────────────────────────────────────────────────
            Action::ChangeListOlder => self.change_list_older(),
            Action::ChangeListNewer => self.change_list_newer(),
            // ── Tab navigation ───────────────────────────────────────────────
            Action::TabNext => {
                let new_vp = self.tabs.focus_next(self.viewport);
                self.viewport = new_vp;
                let new_buf = self.tabs.focused_windows().focused_window().buffer_id;
                let _ = self.backend.switch_to_id(new_buf);
            }
            Action::TabPrev => {
                let new_vp = self.tabs.focus_prev(self.viewport);
                self.viewport = new_vp;
                let new_buf = self.tabs.focused_windows().focused_window().buffer_id;
                let _ = self.backend.switch_to_id(new_buf);
            }
            Action::RotateView => self.rotate_view(),
            Action::RotateViewReverse => self.rotate_view_reverse(),
            Action::TransposeView => self.transpose_view(),
            Action::WindowClose => self.close_view(),
            Action::WindowOnly => self.close_other_views(),
            Action::JumpViewLeft => self.jump_view(ViewDirection::Left),
            Action::JumpViewDown => self.jump_view(ViewDirection::Down),
            Action::JumpViewUp => self.jump_view(ViewDirection::Up),
            Action::JumpViewRight => self.jump_view(ViewDirection::Right),
            Action::SwapViewLeft => self.swap_view(ViewDirection::Left),
            Action::SwapViewDown => self.swap_view(ViewDirection::Down),
            Action::SwapViewUp => self.swap_view(ViewDirection::Up),
            Action::SwapViewRight => self.swap_view(ViewDirection::Right),
            Action::CommandHistoryOlder => self.history_older(),
            Action::CommandHistoryNewer => self.history_newer(),
            // ── Quickfix / location-list navigation ─────────────────────────────
            Action::QfNext => self.qf_next(true),
            Action::QfPrev => self.qf_prev(true),
            Action::LocNext => self.qf_next(false),
            Action::LocPrev => self.qf_prev(false),
            Action::GitNextHunk => self.jump_to_git_hunk(true),
            Action::GitPrevHunk => self.jump_to_git_hunk(false),
            Action::GitFirstHunk => self.jump_to_git_hunk_edge(true),
            Action::GitLastHunk => self.jump_to_git_hunk_edge(false),
            Action::GitBlame => self.show_git_blame(),
            Action::GitDiff => self.open_git_diff_view(false),
            // ── Fold commands ────────────────────────────────────────────────────
            Action::FoldToggle => self.fold_toggle(),
            Action::FoldOpen => self.fold_open(),
            Action::FoldClose => self.fold_close(),
            Action::FoldOpenAll => self.fold_open_all(),
            Action::FoldCloseAll => self.fold_close_all(),
            Action::CommitUndoCheckpoint => {
                let _ = self.backend.send_edit("commit_undo_checkpoint", json!([]));
            }
        }
    }

    fn handle_default(&mut self, key: KeyEvent) {
        // Cancel operator-pending on Escape
        if key.code == KeyCode::Esc && self.mode == Mode::OperatorPending {
            self.enter_normal_mode();
            self.input_state.reset();
            return;
        }

        let ch = match key.code {
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                c
            }
            KeyCode::Char(_) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return;
            }
            _ => return,
        };

        match self.mode {
            Mode::OperatorPending => {
                self.handle_operator_pending(ch);
            }
            Mode::Insert => {
                let s = ch.to_string();
                self.insert_buffer.push(ch);
                let _ = self.backend.send_edit("insert", json!({ "chars": s }));
            }
            Mode::CommandLine => {
                // Any typed char resets history navigation.
                self.history_idx = None;
                self.command_buffer.push(ch);
            }
            Mode::Normal => {
                if let Some(find) = self.input_state.pending_find.take() {
                    let count = self.input_state.count();
                    self.input_state.reset();
                    for _ in 0..count {
                        self.jump_to_char(ch, find.forward, find.inclusive);
                    }
                    return;
                }
                if ch == '0' {
                    if self.input_state.count_digits.is_empty() {
                        if !self.handle_vlf_navigation("move_to_left_end_of_line", 1) {
                            let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                        }
                        self.input_state.reset();
                    } else {
                        self.input_state.count_digits.push(0);
                    }
                    return;
                }
                if let Some(digit) = ch.to_digit(10) {
                    self.input_state.count_digits.push(digit as u8);
                }
            }
            Mode::Search => {
                self.command_buffer.push(ch);
                let chars = self.command_buffer.clone();
                let case_sensitive = smart_case_sensitive(&chars);
                let _ = self.backend.send_edit(
                    "find",
                    json!({
                        "chars": chars,
                        "case_sensitive": case_sensitive,
                        "regex": false,
                        "whole_words": false
                    }),
                );
            }
            Mode::Visual | Mode::VisualLine | Mode::VisualBlock => {
                self.handle_visual_char(ch);
            }
            // Context-specific pseudo-modes are intercepted before `handle_default`.
            Mode::Picker | Mode::Quickfix | Mode::LocationList => {}
            // SubstituteConfirm only accepts key codes (handled before we reach here);
            // any stray char is a no-op.
            Mode::SubstituteConfirm => {}
        }
    }

    // ── Mouse and bracketed-paste handling ────────────────────────────────────

    fn handle_mouse_event(&mut self, m: MouseEvent) {
        let Ok((width, height)) = crossterm::terminal::size() else {
            return;
        };
        self.handle_mouse_event_in_area(m, Rect { x: 0, y: 0, width, height });
    }

    pub(crate) fn handle_mouse_event_in_area(&mut self, m: MouseEvent, area: Rect) {
        match m.kind {
            MouseEventKind::ScrollUp => {
                let _ = self.backend.send_edit("scroll_up", json!([]));
            }
            MouseEventKind::ScrollDown => {
                let _ = self.backend.send_edit("scroll_down", json!([]));
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let Some((row, col)) = crate::ui::hit_test_buffer_cell(area, self, m.column, m.row)
                else {
                    return;
                };
                let line_count = self.backend.line_count();
                if row < line_count {
                    let byte_col = if let Some(line) = self.backend.get_line(row) {
                        crate::text::display_col_to_byte(line, col)
                    } else {
                        0
                    };
                    let _ = self.backend.send_edit(
                        "gesture",
                        json!({
                            "line": row as u64,
                            "col": byte_col as u64,
                            "ty": {
                                "select": {
                                    "granularity": "point",
                                    "multi": false
                                }
                            }
                        }),
                    );
                    // Exit any special mode on click.
                    if !matches!(self.mode, Mode::Normal | Mode::Insert) {
                        self.enter_normal_mode();
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some((row, col)) = crate::ui::hit_test_buffer_cell(area, self, m.column, m.row)
                else {
                    return;
                };
                let line_count = self.backend.line_count();
                if row < line_count {
                    let byte_col = if let Some(line) = self.backend.get_line(row) {
                        crate::text::display_col_to_byte(line, col)
                    } else {
                        0
                    };
                    let _ = self.backend.send_edit(
                        "gesture",
                        json!({
                            "line": row as u64,
                            "col": byte_col as u64,
                            "ty": "drag"
                        }),
                    );
                }
            }
            _ => {}
        }
    }

    fn handle_paste(&mut self, text: String) {
        match self.mode {
            Mode::Insert => {
                // Bracketed paste stays backend-owned for undo grouping and multicursor behavior.
                self.insert_buffer.push_str(&text);
                let _ = self.backend.send_edit("paste", json!({ "chars": text }));
            }
            Mode::CommandLine | Mode::Search => {
                // Paste into the command/search buffer.
                self.command_buffer.push_str(&text);
                if self.mode == Mode::Search {
                    let chars = self.command_buffer.clone();
                    let case_sensitive = smart_case_sensitive(&chars);
                    let _ = self.backend.send_edit(
                        "find",
                        json!({
                            "chars": chars,
                            "case_sensitive": case_sensitive,
                            "regex": false,
                            "whole_words": false
                        }),
                    );
                }
            }
            Mode::Normal => {
                // In normal mode enter insert and paste the text, like pressing `i` then typing.
                self.mode = Mode::Insert;
                self.insert_buffer.push_str(&text);
                let _ = self.backend.send_edit("paste", json!({ "chars": text }));
            }
            _ => {}
        }
    }

    fn enter_normal_mode(&mut self) {
        if self.mode == Mode::Insert {
            // Flush accumulated block insert if pending.
            if self.block_insert.is_some() {
                self.apply_block_insert();
            }
            // Record insert mode text for `.` repeat.
            if !self.insert_buffer.is_empty() {
                self.last_change = Some(LastChange::Insert(self.insert_buffer.clone()));
                self.push_change();
            }
            self.insert_buffer.clear();
        }
        // Save visual selection for `gv`.
        if self.mode.is_visual() {
            if let Some((al, ac)) = self.visual_anchor {
                self.last_visual = Some((self.mode, al, ac));
            }
            self.visual_anchor = None;
            if self.backend.is_vlf {
                if let Some((line, col)) = self.visual_restore_cursor.take() {
                    self.backend.cursor_line =
                        line.min(self.backend.line_count().saturating_sub(1));
                    let max_col = self
                        .backend
                        .get_line(self.backend.cursor_line)
                        .map(str::len)
                        .unwrap_or(col);
                    self.backend.cursor_col = col.min(max_col);
                }
            } else {
                self.visual_restore_cursor = None;
            }
            if !self.backend.is_vlf {
                let _ = self.backend.send_edit("collapse_selections", json!([]));
            }
        }
        self.mode = Mode::Normal;
        self.command_buffer.clear();
    }

    // ── Operator-pending mode ──────────────────────────────────────────────

    /// Apply operator to the current xi selection, then return to Normal (or
    /// Insert for Change).  Resets input state before returning.
    fn apply_operator(&mut self, op: Operator) {
        match op {
            Operator::Delete => {
                let reg = self.take_register();
                let text = self.selected_text_preview(false);
                self.registers.delete(&reg, text, false);
                self.record_edit("delete_forward", json!([]));
                self.push_change();
                self.input_state.reset();
                self.enter_normal_mode();
            }
            Operator::Change => {
                let reg = self.take_register();
                let text = self.selected_text_preview(false);
                self.registers.delete(&reg, text, false);
                self.record_edit("delete_forward", json!([]));
                self.push_change();
                self.input_state.reset();
                self.enter_normal_mode();
                self.mode = Mode::Insert;
            }
            Operator::Yank => {
                let reg = self.take_register();
                let text = self.selected_text_preview(false);
                self.registers.yank(&reg, text, false);
                // Collapse selection without modifying buffer.
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.input_state.reset();
                self.enter_normal_mode();
            }
            Operator::Indent => {
                self.record_edit("indent", json!([]));
                self.push_change();
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.input_state.reset();
                self.enter_normal_mode();
            }
            Operator::Outdent => {
                self.record_edit("outdent", json!([]));
                self.push_change();
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.input_state.reset();
                self.enter_normal_mode();
            }
            Operator::Uppercase => {
                self.record_edit("uppercase", json!([]));
                self.push_change();
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.input_state.reset();
                self.enter_normal_mode();
            }
            Operator::Lowercase => {
                self.record_edit("lowercase", json!([]));
                self.push_change();
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.input_state.reset();
                self.enter_normal_mode();
            }
            Operator::CaseToggle => {
                // xi has no char-level toggle; capitalize (first letter of each word)
                // is the closest available primitive.
                self.record_edit("capitalize", json!([]));
                self.push_change();
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.input_state.reset();
                self.enter_normal_mode();
            }
        }
    }

    /// Apply operator to the current line (double-operator: dd, cc, yy, >>, <<, …).
    fn apply_operator_to_line(&mut self, op: Operator) {
        match op {
            Operator::Delete => {
                // Move to start, select to end, delete line content, then delete newline.
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                let _ = self
                    .backend
                    .send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
                let _ = self.backend.send_edit("delete_forward", json!([]));
                let _ = self.backend.send_edit("delete_forward", json!([]));
            }
            Operator::Change => {
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                let _ = self
                    .backend
                    .send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
                let _ = self.backend.send_edit("delete_forward", json!([]));
            }
            Operator::Yank => {
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                let _ = self
                    .backend
                    .send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
                let _ = self.backend.send_edit("delete_forward", json!([]));
                let _ = self.backend.send_edit("yank", json!([]));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
            }
            Operator::Indent => {
                let _ = self.backend.send_edit("indent", json!([]));
            }
            Operator::Outdent => {
                let _ = self.backend.send_edit("outdent", json!([]));
            }
            Operator::Uppercase => {
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                let _ = self
                    .backend
                    .send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
                let _ = self.backend.send_edit("uppercase", json!([]));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
            }
            Operator::Lowercase => {
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                let _ = self
                    .backend
                    .send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
                let _ = self.backend.send_edit("lowercase", json!([]));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
            }
            Operator::CaseToggle => {
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                let _ = self
                    .backend
                    .send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
                let _ = self.backend.send_edit("capitalize", json!([]));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
            }
        }
        if op == Operator::Change {
            self.input_state.reset();
            self.enter_normal_mode();
            self.mode = Mode::Insert;
        } else {
            self.input_state.reset();
            self.enter_normal_mode();
        }
    }

    /// Handle a char in operator-pending mode.
    fn handle_operator_pending(&mut self, ch: char) {
        let op = match self.input_state.pending_operator {
            Some(op) => op,
            None => {
                self.enter_normal_mode();
                self.input_state.reset();
                return;
            }
        };

        // Priority 1: consume a pending char-find target.
        if let Some(find) = self.input_state.pending_find.take() {
            let count = self.input_state.count();
            self.input_state.reset();
            for _ in 0..count {
                self.jump_to_char_selecting(ch, find.forward, find.inclusive);
            }
            self.apply_operator(op);
            return;
        }

        // Priority 2: consume a text-object specifier (iw, aw, i", …).
        if let Some(inclusive) = self.input_state.text_obj_inclusive.take() {
            self.apply_text_object_operator(op, inclusive, ch);
            return;
        }

        let count = self.input_state.count();

        // Priority 3: double operator means "act on whole line".
        let is_double = matches!(
            (op, ch),
            (Operator::Delete, 'd')
                | (Operator::Change, 'c')
                | (Operator::Yank, 'y')
                | (Operator::Indent, '>')
                | (Operator::Outdent, '<')
                | (Operator::Uppercase, 'U')
                | (Operator::Lowercase, 'u')
                | (Operator::CaseToggle, '~')
        );
        if is_double {
            for _ in 0..count {
                self.apply_operator_to_line(op);
            }
            return;
        }

        // Priority 4: text-object prefix.
        if ch == 'i' && self.input_state.prefix.is_none() {
            self.input_state.text_obj_inclusive = Some(false);
            return;
        }
        if ch == 'a' && self.input_state.prefix.is_none() {
            self.input_state.text_obj_inclusive = Some(true);
            return;
        }

        // Priority 5: 'g' prefix for gg motion.
        if ch == 'g' && self.input_state.prefix.is_none() {
            self.input_state.prefix = Some('g');
            return;
        }

        // Priority 6: count digits.
        if ch.is_ascii_digit() {
            let d = ch as u8 - b'0';
            if d > 0 || !self.input_state.count_digits.is_empty() {
                self.input_state.count_digits.push(d);
                return;
            }
        }

        // Priority 7: '0' as line-start motion when no count is active.
        if ch == '0' && self.input_state.count_digits.is_empty() {
            let _ =
                self.backend.send_edit("move_to_left_end_of_line_and_modify_selection", json!([]));
            self.apply_operator(op);
            return;
        }

        // Priority 8: char-find operators (f/F/t/T).
        match ch {
            'f' | 'F' | 't' | 'T' => {
                let forward = matches!(ch, 'f' | 't');
                let inclusive = matches!(ch, 'f' | 'F');
                self.input_state.prefix = None;
                self.input_state.pending_find = Some(PendingCharFind { forward, inclusive });
                return;
            }
            _ => {}
        }

        // Priority 9: motions that extend selection.
        let motion_cmd = match (ch, self.input_state.prefix) {
            ('h', None) => Some("move_left_and_modify_selection"),
            ('l', None) => Some("move_right_and_modify_selection"),
            ('j', None) => Some("move_down_and_modify_selection"),
            ('k', None) => Some("move_up_and_modify_selection"),
            ('w', None) | ('e', None) => Some("move_word_right_and_modify_selection"),
            ('b', None) => Some("move_word_left_and_modify_selection"),
            ('$', None) => Some("move_to_right_end_of_line_and_modify_selection"),
            ('^', None) => Some("move_to_beginning_of_paragraph_and_modify_selection"),
            ('G', None) => Some("move_to_end_of_document_and_modify_selection"),
            ('g', Some('g')) => Some("move_to_beginning_of_document_and_modify_selection"),
            _ => None,
        };
        if let Some(cmd) = motion_cmd {
            for _ in 0..count {
                let _ = self.backend.send_edit(cmd, json!([]));
            }
            self.apply_operator(op);
            return;
        }

        // Unknown key – cancel.
        self.enter_normal_mode();
        self.input_state.reset();
    }

    /// Like [`jump_to_char`] but uses `select_extend` so the region from the
    /// current cursor position to the found char is selected (for operators).
    fn jump_to_char_selecting(&mut self, target: char, forward: bool, inclusive: bool) {
        let _ = self.backend.find_char(target, forward, inclusive, true);
    }

    fn select_range(
        &mut self,
        start_line: usize,
        start_col: usize,
        end_line: usize,
        end_col: usize,
    ) {
        let _ = self.backend.send_edit(
            "gesture",
            json!({
                "line": start_line as u64,
                "col": start_col as u64,
                "ty": { "select": { "granularity": "point", "multi": false } }
            }),
        );
        let _ = self.backend.send_edit(
            "gesture",
            json!({
                "line": end_line as u64,
                "col": end_col as u64,
                "ty": { "select_extend": { "granularity": "point" } }
            }),
        );
    }

    fn select_range_on_line(&mut self, line_idx: usize, start: usize, end: usize) {
        self.select_range(line_idx, start, line_idx, end);
    }

    /// Select `(start, end)` byte range on `line_idx` and apply the operator.
    fn select_range_and_apply(&mut self, line_idx: usize, start: usize, end: usize, op: Operator) {
        self.select_range_on_line(line_idx, start, end);
        self.apply_operator(op);
    }

    fn line_text_object_range(
        &self,
        inclusive: bool,
        spec: char,
    ) -> Option<(usize, usize, usize, String)> {
        let line_idx = self.backend.cursor_line;
        let line = self.backend.lines.get(line_idx)?.clone();
        let cursor_byte = self.backend.cursor_col.min(line.len());
        let range = match spec {
            'w' | 'W' => {
                let big_word = spec == 'W';
                text_obj_word(&line, cursor_byte, inclusive, big_word)
            }
            '"' | '\'' | '`' => text_obj_quote(&line, cursor_byte, spec, inclusive),
            '(' | ')' | 'b' => text_obj_bracket(&line, cursor_byte, '(', ')', inclusive),
            '[' | ']' => text_obj_bracket(&line, cursor_byte, '[', ']', inclusive),
            '{' | '}' | 'B' => text_obj_bracket(&line, cursor_byte, '{', '}', inclusive),
            '<' | '>' => text_obj_bracket(&line, cursor_byte, '<', '>', inclusive),
            't' => text_obj_tag(&line, cursor_byte, inclusive),
            _ => None,
        }?;
        Some((line_idx, range.0, range.1, line))
    }

    fn select_text_object(&mut self, inclusive: bool, spec: char) -> Result<(), String> {
        match spec {
            'p' => {
                let _ = self.backend.send_edit("move_to_beginning_of_paragraph", json!([]));
                let _ = self
                    .backend
                    .send_edit("move_to_end_of_paragraph_and_modify_selection", json!([]));
                Ok(())
            }
            's' => {
                let line_idx = self.backend.cursor_line;
                let line_len = self.backend.lines.get(line_idx).map(|line| line.len()).unwrap_or(0);
                self.select_range_on_line(line_idx, 0, line_len);
                Ok(())
            }
            _ => {
                let Some((line_idx, start, end, _)) = self.line_text_object_range(inclusive, spec)
                else {
                    return Err(format!("textobject: unsupported specifier `{spec}`"));
                };
                self.select_range_on_line(line_idx, start, end);
                Ok(())
            }
        }
    }

    fn line_ending_str(&self) -> &'static str {
        match self.config.end_of_line {
            crate::config::EndOfLine::Lf => "\n",
            crate::config::EndOfLine::CrLf => "\r\n",
            crate::config::EndOfLine::Cr => "\r",
        }
    }

    fn move_current_line_adjacent(&mut self, down: bool) -> Result<(), String> {
        let current_line = self.backend.cursor_line;
        let Some(current_text) = self.backend.lines.get(current_line).cloned() else {
            return Err("move_line: cursor is outside buffer".to_owned());
        };

        let Some(target_line) =
            (if down { current_line.checked_add(1) } else { current_line.checked_sub(1) })
        else {
            return Err(if down {
                "move_line_down: already at last line".to_owned()
            } else {
                "move_line_up: already at first line".to_owned()
            });
        };

        let Some(target_text) = self.backend.lines.get(target_line).cloned() else {
            return Err(if down {
                "move_line_down: already at last line".to_owned()
            } else {
                "move_line_up: already at first line".to_owned()
            });
        };

        let start_line = current_line.min(target_line);
        let end_line = current_line.max(target_line);
        let end_col = self.backend.lines.get(end_line).map(|line| line.len()).unwrap_or(0);
        let replacement = if down {
            format!("{target_text}{}{current_text}", self.line_ending_str())
        } else {
            format!("{current_text}{}{target_text}", self.line_ending_str())
        };

        self.begin_record();
        self.select_range(start_line, 0, end_line, end_col);
        self.record_edit("insert", json!({ "chars": replacement }));
        self.end_record();

        let cursor_col = self.backend.cursor_col.min(current_text.len());
        self.move_cursor_to(target_line, cursor_col);
        self.push_change();
        Ok(())
    }

    fn current_surrounding_range(&self) -> Option<(usize, usize, usize, String)> {
        fn take_best(
            best: &mut Option<(usize, usize, String)>,
            line: &str,
            start: usize,
            end: usize,
        ) {
            let Some(inner) = line.get(start + 1..end.saturating_sub(1)) else { return };
            let replace = match best.as_ref() {
                Some((best_start, best_end, _)) => end - start < best_end - best_start,
                None => true,
            };
            if replace {
                *best = Some((start, end, inner.to_owned()));
            }
        }

        let line_idx = self.backend.cursor_line;
        let line = self.backend.lines.get(line_idx)?.clone();
        let cursor_byte = self.backend.cursor_col.min(line.len());
        let mut best = None;

        if let Some((start, end)) = text_obj_quote(&line, cursor_byte, '"', true) {
            take_best(&mut best, &line, start, end);
        }
        if let Some((start, end)) = text_obj_quote(&line, cursor_byte, '\'', true) {
            take_best(&mut best, &line, start, end);
        }
        if let Some((start, end)) = text_obj_quote(&line, cursor_byte, '`', true) {
            take_best(&mut best, &line, start, end);
        }
        if let Some((start, end)) = text_obj_bracket(&line, cursor_byte, '(', ')', true) {
            take_best(&mut best, &line, start, end);
        }
        if let Some((start, end)) = text_obj_bracket(&line, cursor_byte, '[', ']', true) {
            take_best(&mut best, &line, start, end);
        }
        if let Some((start, end)) = text_obj_bracket(&line, cursor_byte, '{', '}', true) {
            take_best(&mut best, &line, start, end);
        }
        if let Some((start, end)) = text_obj_bracket(&line, cursor_byte, '<', '>', true) {
            take_best(&mut best, &line, start, end);
        }

        let (start, end, inner) = best?;
        Some((line_idx, start, end, inner))
    }

    fn surround_add(&mut self, pair_spec: &str, textobject: Option<char>) -> Result<(), String> {
        let Some((open, close)) = parse_surround_pair(pair_spec) else {
            return Err(format!("surround_add: unsupported pair `{pair_spec}`"));
        };

        if let Some(spec) = textobject {
            let Some((line_idx, start, end, line)) = self.line_text_object_range(false, spec)
            else {
                return Err(format!("surround_add: unsupported textobject `{spec}`"));
            };
            let Some(selected) = line.get(start..end) else {
                return Err("surround_add: invalid textobject range".to_owned());
            };
            self.begin_record();
            self.select_range_on_line(line_idx, start, end);
            self.record_edit("insert", json!({ "chars": format!("{open}{selected}{close}") }));
            self.end_record();
            self.push_change();
            return Ok(());
        }

        let selected = self
            .backend
            .selected_text_preview(false)
            .map_err(|err| format!("surround_add failed: {err}"))?;
        if selected.is_empty() {
            return Err("surround_add: usage: :surround_add <pair> [textobject]".to_owned());
        }

        self.begin_record();
        self.record_edit("insert", json!({ "chars": format!("{open}{selected}{close}") }));
        self.end_record();
        self.push_change();
        Ok(())
    }

    fn surround_replace(&mut self, pair_spec: &str) -> Result<(), String> {
        let Some((open, close)) = parse_surround_pair(pair_spec) else {
            return Err(format!("surround_replace: unsupported pair `{pair_spec}`"));
        };
        let Some((line_idx, start, end, inner)) = self.current_surrounding_range() else {
            return Err("surround_replace: no surrounding pair at cursor".to_owned());
        };

        self.begin_record();
        self.select_range_on_line(line_idx, start, end);
        self.record_edit("insert", json!({ "chars": format!("{open}{inner}{close}") }));
        self.end_record();
        self.push_change();
        Ok(())
    }

    fn surround_delete(&mut self) -> Result<(), String> {
        let Some((line_idx, start, end, inner)) = self.current_surrounding_range() else {
            return Err("surround_delete: no surrounding pair at cursor".to_owned());
        };

        self.begin_record();
        self.select_range_on_line(line_idx, start, end);
        self.record_edit("insert", json!({ "chars": inner }));
        self.end_record();
        self.push_change();
        Ok(())
    }

    /// Apply operator to the text object specified by `inclusive`+`spec`.
    fn apply_text_object_operator(&mut self, op: Operator, inclusive: bool, spec: char) {
        let range = match spec {
            'p' => {
                // Paragraph: use xi's paragraph motions instead of byte range.
                let _ = self.backend.send_edit("move_to_beginning_of_paragraph", json!([]));
                let _ = self
                    .backend
                    .send_edit("move_to_end_of_paragraph_and_modify_selection", json!([]));
                self.apply_operator(op);
                return;
            }
            's' => {
                // Sentence: treat as current line for simplicity.
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                let _ = self
                    .backend
                    .send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
                self.apply_operator(op);
                return;
            }
            _ => {
                self.line_text_object_range(inclusive, spec).map(|(_, start, end, _)| (start, end))
            }
        };

        match range {
            Some((start, end)) => {
                let line_idx = self.backend.cursor_line;
                self.select_range_and_apply(line_idx, start, end, op)
            }
            None => {
                self.enter_normal_mode();
                self.input_state.reset();
            }
        }
    }

    fn execute_search(&mut self) {
        let chars = self.command_buffer.clone();
        let case_sensitive = smart_case_sensitive(&chars);
        let _ = self.backend.send_edit(
            "find",
            json!({
                "chars": chars,
                "case_sensitive": case_sensitive,
                "regex": false,
                "whole_words": false
            }),
        );
        if self.search_backward {
            let _ = self
                .backend
                .send_edit("find_previous", json!({ "wrap_around": true, "allow_same": false }));
        } else {
            let _ = self
                .backend
                .send_edit("find_next", json!({ "wrap_around": true, "allow_same": false }));
        }
        // Store for repeated n/N navigation and buffer highlighting.
        self.search_pattern = if chars.is_empty() { None } else { Some(chars) };
        self.enter_normal_mode();
    }

    fn search_current_selection(&mut self, detect_word_boundaries: bool) {
        let Some(chars) = self.current_search_pattern() else {
            return;
        };
        let case_sensitive = smart_case_sensitive(&chars);
        let _ = self.backend.send_edit(
            "find",
            json!({
                "chars": chars,
                "case_sensitive": case_sensitive,
                "regex": false,
                "whole_words": detect_word_boundaries
            }),
        );
        let _ = self.backend.send_edit("highlight_find", json!({ "visible": true }));
        self.search_pattern = self.current_search_pattern();
    }

    fn current_search_pattern(&mut self) -> Option<String> {
        if matches!(self.mode, Mode::Visual | Mode::VisualLine | Mode::VisualBlock) {
            let selected = self.selected_text_preview(false);
            if !selected.is_empty() {
                return Some(selected);
            }
        }

        let line = self.backend.lines.get(self.backend.cursor_line)?;
        let col = self.backend.cursor_col;
        let ch = line.get(col..)?.chars().next()?;
        if !(ch.is_alphanumeric() || ch == '_') {
            return None;
        }

        let start = line[..col]
            .char_indices()
            .rev()
            .take_while(|(_, current)| current.is_alphanumeric() || *current == '_')
            .last()
            .map(|(idx, _)| idx)
            .unwrap_or(col);
        let end = col
            + line[col..]
                .char_indices()
                .take_while(|(_, current)| current.is_alphanumeric() || *current == '_')
                .last()
                .map(|(idx, current)| idx + current.len_utf8())
                .unwrap_or(0);
        Some(line[start..end].to_owned())
    }

    fn current_file_target(&mut self) -> Option<String> {
        if matches!(self.mode, Mode::Visual | Mode::VisualLine | Mode::VisualBlock) {
            let selected = self.selected_text_preview(false);
            let trimmed = selected.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }

        let line = self.backend.lines.get(self.backend.cursor_line)?;
        if line.is_empty() {
            return None;
        }

        let mut cursor = self.backend.cursor_col.min(line.len().saturating_sub(1));
        if line[cursor..].chars().next().is_none_or(char::is_whitespace) {
            cursor = line[..cursor]
                .char_indices()
                .rev()
                .find(|(_, ch)| !ch.is_whitespace())
                .map(|(idx, _)| idx)?;
        }

        let start = line[..cursor]
            .char_indices()
            .rev()
            .find(|(_, ch)| ch.is_whitespace())
            .map(|(idx, ch)| idx + ch.len_utf8())
            .unwrap_or(0);
        let end = line[cursor..]
            .char_indices()
            .find(|(_, ch)| ch.is_whitespace())
            .map(|(idx, _)| cursor + idx)
            .unwrap_or(line.len());

        let token = line[start..end]
            .trim_matches(|ch: char| {
                matches!(ch, '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | ';')
            })
            .trim_end_matches(['.', ':']);
        if token.is_empty() { None } else { Some(token.to_owned()) }
    }

    fn resolve_file_target_path(&self, target: &str) -> Option<PathBuf> {
        let target = target.strip_prefix("file://").unwrap_or(target);
        if target.is_empty() {
            return None;
        }

        let path = PathBuf::from(target);
        if path.is_absolute() {
            return Some(path);
        }

        self.backend
            .active()
            .path
            .as_deref()
            .and_then(Path::parent)
            .map(|parent| parent.join(&path))
            .or_else(|| std::env::current_dir().ok().map(|cwd| cwd.join(path)))
    }

    fn jump_to_char(&mut self, target: char, forward: bool, inclusive: bool) {
        if self.backend.find_char(target, forward, inclusive, self.mode.is_visual()).is_ok() {
            self.last_repeatable_motion =
                Some(RepeatableMotion::CharFind { target, forward, inclusive });
        }
    }

    fn jump_matching_bracket(&mut self) {
        let _ = self.backend.move_to_matching_bracket(self.mode.is_visual());
    }

    fn move_word_start(&mut self, forward: bool, long_word: bool) {
        let _ = self.backend.move_word_start(forward, long_word, self.mode.is_visual());
    }

    fn move_word_end(&mut self, long_word: bool) {
        let _ = self.backend.move_word_end(long_word, self.mode.is_visual());
    }

    fn goto_line_from_count(&mut self) {
        let target = self.input_state.count().saturating_sub(1) as usize;
        self.jump_to_line(target);
    }

    fn goto_first_nonwhitespace(&mut self) {
        let line = self
            .backend
            .lines
            .get(self.backend.cursor_line)
            .map(String::as_str)
            .unwrap_or_default();
        let target_byte = line
            .char_indices()
            .find_map(|(idx, ch)| (!ch.is_whitespace()).then_some(idx))
            .unwrap_or(0);
        self.goto_column(byte_col_to_display_col(line, target_byte));
    }

    fn goto_column(&mut self, display_col: usize) {
        let _ = self.backend.goto_column(display_col, self.mode.is_visual());
    }

    fn goto_column_from_count(&mut self) {
        let target = self.input_state.count().saturating_sub(1) as usize;
        self.goto_column(target);
    }

    fn goto_file_start_from_count(&mut self) {
        if self.input_state.count_digits.is_empty() {
            self.jump_to_line(0);
        } else {
            self.goto_line_from_count();
        }
    }

    fn goto_last_line(&mut self) {
        let last_line = self.backend.line_count().saturating_sub(1);
        self.jump_to_line(last_line);
    }

    fn goto_file_under_cursor(&mut self) {
        let Some(target) = self.current_file_target() else {
            self.backend.status_message = Some("goto_file: no file under cursor".to_owned());
            return;
        };

        if target.starts_with("http://") || target.starts_with("https://") {
            self.backend.status_message =
                Some("goto_file: URL targets are not supported yet".to_owned());
            return;
        }

        let Some(path) = self.resolve_file_target_path(&target) else {
            self.backend.status_message = Some(format!("goto_file: invalid target `{target}`"));
            return;
        };

        let existing_id = self
            .backend
            .all_bufs()
            .iter()
            .find(|buf| buf.path.as_ref().is_some_and(|candidate| *candidate == path))
            .map(|buf| buf.id);

        let buf_id = if let Some(id) = existing_id {
            if let Err(err) = self.backend.switch_to_id(id) {
                self.backend.status_message = Some(format!("goto_file failed: {err}"));
                return;
            }
            id
        } else {
            match self.backend.open_buffer(Some(path.clone())) {
                Ok(id) => {
                    if let Err(err) = self.backend.switch_to_id(id) {
                        self.backend.status_message = Some(format!("goto_file failed: {err}"));
                        return;
                    }
                    id
                }
                Err(err) => {
                    self.backend.status_message = Some(format!("goto_file failed: {err}"));
                    return;
                }
            }
        };

        self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
        self.viewport = Viewport::default();
    }

    fn page_cursor_half(&mut self, down: bool) {
        let editor_rows = crossterm::terminal::size()
            .map(|(_, height)| height.saturating_sub(2) as usize)
            .unwrap_or(22);
        let delta = (editor_rows / 2).max(1);
        let max_line = self.backend.lines.len().saturating_sub(1);
        let next_line = if down {
            self.backend.cursor_line.saturating_add(delta).min(max_line)
        } else {
            self.backend.cursor_line.saturating_sub(delta)
        };
        let next_col = self
            .backend
            .lines
            .get(next_line)
            .map(|line| self.backend.cursor_col.min(line.len()))
            .unwrap_or(0);
        self.push_jump();
        let _ = self.backend.send_edit(
            "gesture",
            json!({ "line": next_line as u64, "col": next_col as u64, "ty": "point_select" }),
        );
        self.viewport.top_line = if down {
            self.viewport.top_line.saturating_add(delta).min(max_line)
        } else {
            self.viewport.top_line.saturating_sub(delta)
        };
    }

    fn repeat_last_motion(&mut self) {
        let Some(motion) = self.last_repeatable_motion else {
            return;
        };

        match motion {
            RepeatableMotion::CharFind { target, forward, inclusive } => {
                self.jump_to_char(target, forward, inclusive);
            }
            RepeatableMotion::MatchingPair => self.jump_matching_bracket(),
            RepeatableMotion::Quickfix { forward, is_quickfix } => {
                if forward {
                    self.qf_next(is_quickfix);
                } else {
                    self.qf_prev(is_quickfix);
                }
            }
            RepeatableMotion::GitHunk { forward } => self.jump_to_git_hunk(forward),
        }
    }

    fn apply_case_transform(&mut self, method: &'static str) {
        let count = self.input_state.count() as usize;
        let had_visual = self.mode.is_visual();
        if !had_visual && !self.select_chars_from_cursor(count) {
            return;
        }
        let _ = self.backend.send_edit(method, json!([]));
        self.push_change();
        let _ = self.backend.send_edit("collapse_selections", json!([]));
        if had_visual {
            self.enter_normal_mode();
        }
    }

    fn yank_selection(&mut self) {
        let count = self.input_state.count() as usize;
        let had_visual = self.mode.is_visual();
        if !had_visual && !self.select_chars_from_cursor(count) {
            return;
        }
        let reg = self.take_register();
        let text = self.selected_text_preview(false);
        self.registers.yank(&reg, text, false);
        let _ = self.backend.send_edit("collapse_selections", json!([]));
        if had_visual {
            self.enter_normal_mode();
        }
    }

    fn yank_selection_to_register(&mut self, reg: RegisterName) {
        let count = self.input_state.count() as usize;
        let had_visual = self.mode.is_visual();
        let has_selection = self
            .backend
            .selections_preview()
            .map(|selections| selections.iter().any(|selection| selection.start != selection.end))
            .unwrap_or(false);
        if !had_visual && !has_selection && !self.select_chars_from_cursor(count) {
            return;
        }

        let text = self.selected_text_preview(false);
        if text.is_empty() {
            return;
        }

        self.registers.yank(&reg, text, false);
        let _ = self.backend.send_edit("collapse_selections", json!([]));
        if had_visual {
            self.enter_normal_mode();
        }
    }

    fn apply_selection_edit(&mut self, method: &'static str, push_change: bool) {
        let had_visual = self.mode.is_visual();
        if !had_visual {
            let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
            let _ =
                self.backend.send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
        }
        let _ = self.backend.send_edit(method, json!([]));
        if push_change {
            self.push_change();
        }
        let _ = self.backend.send_edit("collapse_selections", json!([]));
        if had_visual {
            self.enter_normal_mode();
        }
    }

    fn format_selections(&mut self) {
        if self.backend.format_document().is_ok() {
            self.push_change();
            if self.mode.is_visual() {
                self.enter_normal_mode();
            }
        }
    }

    fn extend_line_below(&mut self) {
        let _ = self.backend.extend_line_below(self.input_state.count() as usize);
    }

    fn extend_to_line_bounds(&mut self) {
        let _ = self.backend.extend_to_line_bounds();
    }

    fn shrink_to_line_bounds(&mut self) {
        let _ = self.backend.shrink_to_line_bounds();
    }

    fn filter_selections_from_search(&mut self, remove: bool) {
        let Some(pattern) = self.search_pattern.clone() else {
            self.backend.status_message = Some(format!(
                "{}: search pattern required",
                if remove { "remove_selections" } else { "keep_selections" }
            ));
            return;
        };
        self.filter_selections(&pattern, remove);
    }

    fn filter_selections(&mut self, pattern: &str, remove: bool) {
        if regex::Regex::new(pattern).is_err() {
            self.backend.status_message = Some(format!(
                "{}: invalid regex",
                if remove { "remove_selections" } else { "keep_selections" }
            ));
            return;
        }

        let filtered = match self.backend.filter_selections_preview(pattern, remove) {
            Ok(filtered) => filtered,
            Err(err) => {
                self.backend.status_message = Some(format!(
                    "{}: {err}",
                    if remove { "remove_selections" } else { "keep_selections" }
                ));
                return;
            }
        };

        if filtered.is_empty() {
            self.backend.status_message = Some("no selections remaining".to_owned());
            return;
        }

        if self.backend.set_selections(&filtered).is_ok()
            && filtered.iter().any(|range| range.start != range.end)
        {
            self.mode = Mode::Visual;
        }
    }

    fn join_selections(&mut self, select_space: bool) {
        if self.backend.join_selections(select_space).is_ok() {
            self.push_change();
        }
    }

    fn sort_selected_or_all_lines(&mut self) -> Result<String, String> {
        self.transform_selected_or_all_lines("sort", |lines| lines.sort())
    }

    fn sort_line_range(&mut self, start_line: usize, end_line: usize) -> Result<String, String> {
        self.transform_line_range("sort", start_line, end_line, |lines| lines.sort())
    }

    fn dedup_selected_or_all_lines(&mut self) -> Result<String, String> {
        self.transform_selected_or_all_lines("dedup", |lines| {
            let mut seen = std::collections::HashSet::new();
            lines.retain(|line| seen.insert(line.clone()));
        })
    }

    fn dedup_line_range(&mut self, start_line: usize, end_line: usize) -> Result<String, String> {
        self.transform_line_range("dedup", start_line, end_line, |lines| {
            let mut seen = std::collections::HashSet::new();
            lines.retain(|line| seen.insert(line.clone()));
        })
    }

    fn delete_selection(&mut self, yank: bool, enter_insert: bool) {
        let count = self.input_state.count() as usize;
        let had_visual = self.mode.is_visual();
        if !had_visual && !self.select_chars_from_cursor(count) {
            return;
        }
        if yank {
            let reg = self.take_register();
            let text = self.selected_text_preview(false);
            self.registers.delete(&reg, text, false);
        }
        self.record_edit("delete_forward", json!([]));
        self.push_change();
        if had_visual {
            self.enter_normal_mode();
        }
        if enter_insert {
            self.mode = Mode::Insert;
        }
    }

    fn replace_with_char(&mut self, ch: char) {
        let repeat = if self.mode.is_visual() {
            self.selected_text_preview(false)
                .chars()
                .filter(|current| *current != '\n')
                .count()
                .max(1)
        } else {
            self.input_state.count() as usize
        };
        if !self.mode.is_visual() && !self.select_chars_from_cursor(repeat) {
            return;
        }
        let _ = self.backend.send_edit("delete_forward", json!([]));
        let _ = self
            .backend
            .send_edit("insert", json!({ "chars": ch.to_string().repeat(repeat.max(1)) }));
        self.push_change();
        if self.mode.is_visual() {
            self.enter_normal_mode();
        }
    }

    fn replace_with_yanked(&mut self) {
        let reg = self.take_register();
        let text = self.registers.get(&reg);
        if text.is_empty() {
            return;
        }
        let count = self.input_state.count() as usize;
        if !self.mode.is_visual() && !self.select_chars_from_cursor(count) {
            return;
        }
        let _ = self.backend.send_edit("delete_forward", json!([]));
        let _ = self.backend.send_edit("paste_register", json!({ "chars": text, "before": true }));
        self.push_change();
        if self.mode.is_visual() {
            self.enter_normal_mode();
        }
    }

    fn replace_selections_with_register(&mut self, reg: RegisterName) {
        let text = self.registers.get(&reg);
        if text.is_empty() {
            return;
        }

        let count = self.input_state.count() as usize;
        let had_visual = self.mode.is_visual();
        let has_selection = self
            .backend
            .selections_preview()
            .map(|selections| selections.iter().any(|selection| selection.start != selection.end))
            .unwrap_or(false);
        if !had_visual && !has_selection && !self.select_chars_from_cursor(count) {
            return;
        }

        let _ = self.backend.send_edit("delete_forward", json!([]));
        let _ = self.backend.send_edit("paste_register", json!({ "chars": text, "before": true }));
        self.push_change();
        if had_visual {
            self.enter_normal_mode();
        }
    }

    fn select_chars_from_cursor(&mut self, count: usize) -> bool {
        let selections = match self.backend.select_chars_preview(count.max(1)) {
            Ok(selections) => selections,
            Err(_) => return false,
        };
        if selections.is_empty() {
            return false;
        }
        self.backend.set_selections(&selections).is_ok()
    }

    fn ensure_editable_selections(&mut self) -> Result<Vec<SelectionRange>, String> {
        let mut selections = self
            .backend
            .selections_preview()
            .map_err(|err| format!("selection preview failed: {err}"))?;
        if selections.is_empty()
            || selections.iter().all(|selection| selection.start == selection.end)
        {
            if !self.select_chars_from_cursor(1) {
                return Err(String::from("no selection available"));
            }
            selections = self
                .backend
                .selections_preview()
                .map_err(|err| format!("selection preview failed: {err}"))?;
        }
        if selections.is_empty() {
            Err(String::from("no selection available"))
        } else {
            Ok(selections)
        }
    }

    fn primary_selection_preview(&mut self) -> Result<Option<SelectionRange>, String> {
        let selections = self
            .backend
            .selections_preview()
            .map_err(|err| format!("selection preview failed: {err}"))?;
        if selections.is_empty() {
            return Ok(None);
        }

        let cursor = self.active_cursor_offset();
        Ok(selections
            .into_iter()
            .find(|selection| {
                let start = selection.start.min(selection.end);
                let end = selection.start.max(selection.end);
                selection.end == cursor
                    || selection.start == cursor
                    || (start <= cursor && cursor <= end)
            })
            .or(Some(SelectionRange { start: cursor, end: cursor })))
    }

    fn primary_selection_text(&mut self) -> Result<String, String> {
        let Some(selection) = self.primary_selection_preview()? else {
            return Ok(String::new());
        };
        let start = selection.start.min(selection.end);
        let end = selection.start.max(selection.end);
        if start == end {
            return Ok(String::new());
        }

        let buffer = self.current_buffer_text();
        buffer
            .get(start..end)
            .map(str::to_owned)
            .ok_or_else(|| String::from("primary selection range is invalid"))
    }

    fn yank_main_selection_to_register(&mut self, reg: RegisterName) {
        let count = self.input_state.count() as usize;
        let had_visual = self.mode.is_visual();
        let initial = self.primary_selection_text().unwrap_or_default();
        if !had_visual && initial.is_empty() && !self.select_chars_from_cursor(count) {
            return;
        }

        let text = match self.primary_selection_text() {
            Ok(text) => text,
            Err(message) => {
                self.backend.status_message = Some(message);
                return;
            }
        };
        if text.is_empty() {
            return;
        }

        self.registers.yank(&reg, text, false);
        let _ = self.backend.send_edit("collapse_selections", json!([]));
        if had_visual {
            self.enter_normal_mode();
        }
    }

    fn current_buffer_text(&self) -> String {
        self.backend.lines.join("\n")
    }

    fn line_start_offset(&self, line: usize) -> usize {
        self.backend.lines.iter().take(line).map(|current| current.len() + 1).sum()
    }

    fn replace_line_block(
        &mut self,
        start_line: usize,
        end_line: usize,
        lines: &[String],
    ) -> Result<(), String> {
        self.backend
            .replace_line_range(start_line, end_line, lines)
            .map_err(|err| format!("replace failed: {err}"))
    }

    fn transform_selected_or_all_lines<F>(
        &mut self,
        label: &str,
        mut transform: F,
    ) -> Result<String, String>
    where
        F: FnMut(&mut Vec<String>),
    {
        let selected_text = self
            .backend
            .selected_text_preview(false)
            .map_err(|err| format!("{label}: selection preview failed: {err}"))?;
        let selected = self
            .backend
            .selected_text_preview(true)
            .map_err(|err| format!("{label}: selection preview failed: {err}"))?;

        let changed = if !selected_text.is_empty() {
            let original: Vec<String> = selected.lines().map(str::to_owned).collect();
            let mut updated = original.clone();
            transform(&mut updated);
            if original == updated {
                false
            } else {
                let _ = self.backend.send_edit("extend_to_line_bounds", json!([]));
                let expanded = self
                    .backend
                    .selected_text_preview(false)
                    .map_err(|err| format!("{label}: selection preview failed: {err}"))?;
                let mut replacement = updated.join("\n");
                if expanded.ends_with('\n') {
                    replacement.push('\n');
                }
                let _ = self.backend.send_edit("delete_forward", json!([]));
                if !replacement.is_empty() {
                    let _ = self.backend.send_edit("insert", json!({ "chars": replacement }));
                }
                true
            }
        } else {
            if self.backend.lines.is_empty() {
                return Ok(format!("{label}: no lines"));
            }

            let original = self.backend.lines.clone();
            let mut updated = original.clone();
            transform(&mut updated);
            if original == updated {
                false
            } else {
                self.replace_line_block(0, original.len().saturating_sub(1), &updated)?;
                true
            }
        };

        if changed {
            self.push_change();
            Ok(format!("{label}: applied"))
        } else {
            Ok(format!("{label}: no changes"))
        }
    }

    fn transform_line_range<F>(
        &mut self,
        label: &str,
        start_line: usize,
        end_line: usize,
        mut transform: F,
    ) -> Result<String, String>
    where
        F: FnMut(&mut Vec<String>),
    {
        if self.backend.lines.is_empty() {
            return Ok(format!("{label}: no lines"));
        }

        let last_line = self.backend.lines.len().saturating_sub(1);
        let start_line = start_line.min(last_line);
        let end_line = end_line.min(last_line).max(start_line);
        let original = self.backend.lines[start_line..=end_line].to_vec();
        let mut updated = original.clone();
        transform(&mut updated);

        if original == updated {
            return Ok(format!("{label}: no changes"));
        }

        self.replace_line_block(start_line, end_line, &updated)?;

        self.push_change();
        Ok(format!("{label}: applied"))
    }

    fn replace_range_with_text(&mut self, range: SelectionRange, text: &str) -> Result<(), String> {
        let is_non_empty = range.start != range.end;
        self.backend
            .set_selections(std::slice::from_ref(&range))
            .map_err(|err| format!("replace failed: {err}"))?;
        self.backend.sync_pending_events().map_err(|err| format!("replace failed: {err}"))?;
        if is_non_empty {
            let _ = self.backend.send_edit("delete_forward", json!([]));
        }
        if !text.is_empty() {
            let _ = self.backend.send_edit("insert", json!({ "chars": text }));
        }
        Ok(())
    }

    fn insert_at_offset(&mut self, offset: usize, text: &str) -> Result<(), String> {
        self.backend
            .set_selections(&[SelectionRange { start: offset, end: offset }])
            .map_err(|err| format!("insert failed: {err}"))?;
        self.backend.sync_pending_events().map_err(|err| format!("insert failed: {err}"))?;
        if !text.is_empty() {
            let _ = self.backend.send_edit("insert", json!({ "chars": text }));
        }
        Ok(())
    }

    fn run_shell_command_on_selections(
        &mut self,
        command: &str,
        mode: ShellSelectionMode,
    ) -> Result<String, String> {
        let cwd = std::env::current_dir().map_err(|err| format!("shell failed: {err}"))?;
        let selections = self.ensure_editable_selections()?;
        let buffer_text = self.current_buffer_text();

        let mut outputs = Vec::with_capacity(selections.len());
        let mut kept = Vec::new();
        for selection in &selections {
            let start = selection.start.min(selection.end);
            let end = selection.start.max(selection.end);
            let input = buffer_text
                .get(start..end)
                .ok_or_else(|| String::from("shell command encountered invalid selection range"))?;
            let result = crate::terminal::run_command_with_input(command, &cwd, Some(input))
                .map_err(|err| format!("shell failed: {err}"))?;
            if !result.success && !matches!(mode, ShellSelectionMode::KeepByStatus) {
                let stderr = result.stderr.trim();
                let detail = if stderr.is_empty() {
                    String::from("command exited with failure")
                } else {
                    stderr.to_owned()
                };
                return Err(format!("shell failed: {detail}"));
            }
            if matches!(mode, ShellSelectionMode::KeepByStatus) {
                if result.success {
                    kept.push(selection.clone());
                }
            } else {
                outputs.push(result.stdout);
            }
        }

        match mode {
            ShellSelectionMode::Replace => {
                for (selection, output) in selections.iter().zip(outputs.iter()).rev() {
                    self.replace_range_with_text(selection.clone(), output)?;
                }
                self.push_change();
                Ok(format!("pipe: {} selection(s)", selections.len()))
            }
            ShellSelectionMode::IgnoreOutput => {
                Ok(format!("pipe_to: {} selection(s)", selections.len()))
            }
            ShellSelectionMode::InsertBefore => {
                for (selection, output) in selections.iter().zip(outputs.iter()).rev() {
                    self.insert_at_offset(selection.start.min(selection.end), output)?;
                }
                self.push_change();
                Ok(format!("shell_insert_output: {} selection(s)", selections.len()))
            }
            ShellSelectionMode::InsertAfter => {
                for (selection, output) in selections.iter().zip(outputs.iter()).rev() {
                    self.insert_at_offset(selection.start.max(selection.end), output)?;
                }
                self.push_change();
                Ok(format!("shell_append_output: {} selection(s)", selections.len()))
            }
            ShellSelectionMode::KeepByStatus => {
                if kept.is_empty() {
                    return Ok(String::from("no selections remaining"));
                }
                self.backend
                    .set_selections(&kept)
                    .map_err(|err| format!("shell_keep_pipe failed: {err}"))?;
                Ok(format!("shell_keep_pipe: {} selection(s)", kept.len()))
            }
        }
    }

    fn restore_git_hunk(&mut self) -> Result<String, String> {
        if self.block_active_vlf_source_control("git hunk reset") {
            return Err(Self::source_control_disabled_message("git hunk reset"));
        }

        let status = self
            .refresh_active_git_status()
            .ok_or_else(|| String::from("git: current buffer not in repository"))?;
        let hunk = status
            .hunk_at_line(self.backend.cursor_line)
            .cloned()
            .ok_or_else(|| String::from("git: cursor not inside changed hunk"))?;

        let start_offset = self.line_start_offset(hunk.new_start.min(self.backend.lines.len()));
        let end_offset = if hunk.new_count == 0 {
            start_offset
        } else {
            let after_line = (hunk.new_start + hunk.new_count).min(self.backend.lines.len());
            if after_line < self.backend.lines.len() {
                self.line_start_offset(after_line)
            } else {
                self.current_buffer_text().len()
            }
        };
        let old_lines = hunk
            .lines
            .iter()
            .filter(|line| matches!(line.kind, crate::git::DiffLineKind::Removed))
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();
        let replacement = format_git_hunk_replacement(
            &old_lines,
            hunk.new_start,
            hunk.new_count,
            self.backend.lines.len(),
        );

        self.replace_range_with_text(
            SelectionRange { start: start_offset, end: end_offset },
            &replacement,
        )?;
        self.push_change();
        Ok(format!("git hunk reset: line {}", hunk.display_line + 1))
    }

    // ── Range-based line operations ─────────────────────────────────────────

    /// Delete lines `start..=end` (0-based, inclusive).
    fn delete_line_range(&mut self, start: usize, end: usize) {
        let _ = self.backend.delete_line_range(start, end);
        self.push_change();
    }

    fn add_newline_below(&mut self) {
        if self.backend.add_newline_below().is_ok() {
            self.push_change();
        }
    }

    fn add_newline_above(&mut self) {
        if self.backend.add_newline_above().is_ok() {
            self.push_change();
        }
    }

    /// Yank lines `start..=end` (0-based, inclusive) into the active register.
    fn yank_line_range(&mut self, start: usize, end: usize) {
        let reg = self.take_register();
        self.yank_line_range_into_register(start, end, reg);
    }

    fn yank_line_range_into_register(&mut self, start: usize, end: usize, reg: RegisterName) {
        let line_count = self.backend.line_count();
        if line_count == 0 {
            return;
        }
        let start = start.min(line_count.saturating_sub(1));
        let end = end.min(line_count.saturating_sub(1));
        let mut text = String::new();
        for line in &self.backend.lines[start..=end] {
            text.push_str(line);
            text.push('\n');
        }
        self.registers.yank(&reg, text, false);
        let count = end.saturating_sub(start) + 1;
        self.backend.status_message = Some(format!("{count} line(s) yanked"));
    }

    // ── Cursor jump ────────────────────────────────────────────────────────

    /// Jump the cursor to `line` (0-based), clamped to the buffer length.
    fn jump_to_line(&mut self, line: usize) {
        let clamped = line.min(self.backend.line_count().saturating_sub(1));
        self.push_jump();
        if self.backend.is_vlf {
            self.backend.cursor_line = clamped;
            self.backend.cursor_col = 0;
            return;
        }
        let _ = self.backend.send_edit(
            "gesture",
            json!({ "line": clamped as u64, "col": 0u64, "ty": "point_select" }),
        );
    }

    // ── Quickfix and location list ──────────────────────────────────────────

    fn move_focused_list(&mut self, is_quickfix: bool, forward: bool) {
        if is_quickfix {
            if let Some(qf) = self.quickfix.as_mut() {
                if forward {
                    qf.move_down();
                } else {
                    qf.move_up();
                }
            }
        } else if let Some(ll) = self.location_list.as_mut() {
            if forward {
                ll.move_down();
            } else {
                ll.move_up();
            }
        }
    }

    fn confirm_focused_list(&mut self, is_quickfix: bool) {
        let entry = if is_quickfix {
            self.quickfix.as_ref().and_then(|q| q.current()).cloned()
        } else {
            self.location_list.as_ref().and_then(|l| l.current()).cloned()
        };
        if let Some(entry) = entry {
            self.navigate_to_qf_entry(entry);
        }
        if is_quickfix {
            self.quickfix_focused = false;
        } else {
            self.location_list_focused = false;
        }
    }

    /// Navigate the quickfix list (is_quickfix=true) or location list forward.
    fn qf_next(&mut self, is_quickfix: bool) {
        let entry = if is_quickfix {
            self.quickfix.as_mut().and_then(|q| q.next_entry()).cloned()
        } else {
            self.location_list.as_mut().and_then(|l| l.next_entry()).cloned()
        };
        if let Some(e) = entry {
            self.last_repeatable_motion =
                Some(RepeatableMotion::Quickfix { forward: true, is_quickfix });
            self.navigate_to_qf_entry(e);
        }
    }

    /// Navigate the quickfix list (is_quickfix=true) or location list backward.
    fn qf_prev(&mut self, is_quickfix: bool) {
        let entry = if is_quickfix {
            self.quickfix.as_mut().and_then(|q| q.prev_entry()).cloned()
        } else {
            self.location_list.as_mut().and_then(|l| l.prev_entry()).cloned()
        };
        if let Some(e) = entry {
            self.last_repeatable_motion =
                Some(RepeatableMotion::Quickfix { forward: false, is_quickfix });
            self.navigate_to_qf_entry(e);
        }
    }

    /// Open the file for `entry` and jump to the recorded line/column.
    fn navigate_to_qf_entry(&mut self, entry: QfEntry) {
        let Some(path) = entry.path.clone() else {
            self.backend.status_message = Some(format!("quickfix: line {}", entry.line + 1));
            return;
        };
        // Reuse an already-open buffer if possible.
        let existing_id = self
            .backend
            .all_bufs()
            .iter()
            .find(|b| b.path.as_ref().is_some_and(|p| *p == path))
            .map(|b| b.id);
        let buf_id = if let Some(id) = existing_id {
            let _ = self.backend.switch_to_id(id);
            id
        } else {
            match self.backend.open_buffer(Some(path)) {
                Ok(id) => {
                    let _ = self.backend.switch_to_id(id);
                    id
                }
                Err(err) => {
                    self.backend.status_message = Some(format!("quickfix: {err}"));
                    return;
                }
            }
        };
        self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
        self.viewport = Viewport::default();
        self.jump_to_line(entry.line);
    }

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

    fn open_completion_picker(&mut self, items: &[CompletionSuggestion]) {
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

    fn open_code_action_picker(&mut self, actions: &[CodeActionDescriptor]) {
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

    fn open_diagnostics_location_list(&mut self) {
        if !self.populate_diagnostics_location_list(0) {
            return;
        }
        self.location_list_open = true;
        self.location_list_focused = true;
    }

    // ── Fold commands ─────────────────────────────────────────────────────────

    fn fold_extent_at_cursor(&self) -> Option<(usize, usize)> {
        indent_fold_extent(&self.backend.lines, self.backend.cursor_line)
    }

    fn fold_toggle(&mut self) {
        let line = self.backend.cursor_line;
        let extent = self.fold_extent_at_cursor().unwrap_or((line, line));
        let buf_id = self.backend.active().id;
        self.folds.toggle(buf_id, line, extent);
    }

    fn fold_open(&mut self) {
        let line = self.backend.cursor_line;
        let buf_id = self.backend.active().id;
        self.folds.open(buf_id, line);
    }

    fn fold_close(&mut self) {
        let line = self.backend.cursor_line;
        let buf_id = self.backend.active().id;
        if let Some(extent) = self.fold_extent_at_cursor() {
            self.folds.close(buf_id, line, extent);
        }
    }

    fn fold_open_all(&mut self) {
        let buf_id = self.backend.active().id;
        self.folds.open_all(buf_id);
    }

    fn fold_close_all(&mut self) {
        let buf_id = self.backend.active().id;
        let lines: Vec<String> = self.backend.lines.clone();
        self.folds.close_all(buf_id, &lines);
    }

    // ── Crash recovery ──────────────────────────────────────────────────────

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

    // ── Picker overlay ──────────────────────────────────────────────────────

    /// Confirm the currently selected picker item and close the overlay.
    fn handle_picker_confirm(&mut self) {
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
        let cursor_line = self.backend.cursor_line;
        // Clamp scroll_offset to half the editor height to avoid pathological cases.
        let off = self.config.scroll_offset.min(editor_height / 2);

        if cursor_line < self.viewport.top_line + off {
            self.viewport.top_line = cursor_line.saturating_sub(off);
        } else if cursor_line + off + 1 > self.viewport.top_line + editor_height {
            self.viewport.top_line = cursor_line + off + 1 - editor_height;
        }
        // Clamp top_line so we never show blank rows at the bottom when there
        // are enough lines above to fill the editor area.
        let total_lines = self.backend.line_count().max(1);
        let max_top = total_lines.saturating_sub(editor_height);
        if self.viewport.top_line > max_top {
            self.viewport.top_line = max_top;
        }
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

    fn handle_vlf_navigation(&mut self, method: &str, count: u64) -> bool {
        if !self.backend.is_vlf {
            return false;
        }

        let count = usize::try_from(count).unwrap_or(usize::MAX);
        let line_count = self.backend.line_count().max(1);
        let line = self.backend.cursor_line;
        let col = self.backend.cursor_col;
        let move_to_end = matches!(
            method,
            "move_to_end_of_document" | "move_to_end_of_document_and_modify_selection"
        );
        let target = match method {
            "move_up" | "move_up_and_modify_selection" => Some((line.saturating_sub(count), col)),
            "move_down" | "move_down_and_modify_selection" => {
                Some((line.saturating_add(count).min(line_count.saturating_sub(1)), col))
            }
            "move_left" | "move_left_and_modify_selection" => {
                Some((line, col.saturating_sub(count)))
            }
            "move_right" | "move_right_and_modify_selection" => {
                let max_col = self.backend.get_line(line).map(str::len).unwrap_or(col);
                Some((line, col.saturating_add(count).min(max_col)))
            }
            "move_to_left_end_of_line" => Some((line, 0)),
            "move_to_right_end_of_line" | "move_to_right_end_of_line_and_modify_selection" => {
                Some((line, self.backend.get_line(line).map(str::len).unwrap_or(0)))
            }
            "scroll_page_down" => Some((
                line.saturating_add(self.last_editor_height.max(1) * count)
                    .min(line_count.saturating_sub(1)),
                col,
            )),
            "scroll_page_up" => {
                Some((line.saturating_sub(self.last_editor_height.max(1) * count), col))
            }
            "move_to_beginning_of_document"
            | "move_to_beginning_of_document_and_modify_selection" => Some((0, 0)),
            "move_to_end_of_document" | "move_to_end_of_document_and_modify_selection" => {
                Some((line_count.saturating_sub(1), 0))
            }
            _ => None,
        };

        let Some((line, col)) = target else {
            self.backend.status_message = Some(format!("{method}: disabled in VLF"));
            return true;
        };

        if move_to_end && !self.backend.vlf_line_count_exact {
            let viewport_lines = self.last_editor_height.max(1);
            let _ = self.backend.request_vlf_tail_viewport(viewport_lines);
            self.backend.status_message = Some(String::from("VLF: jumping to file end"));
            return true;
        }

        self.backend.cursor_line = line.min(line_count.saturating_sub(1));
        let max_col = self.backend.get_line(self.backend.cursor_line).map(str::len).unwrap_or(0);
        self.backend.cursor_col = col.min(max_col);
        true
    }

    pub(crate) fn refresh_source_control(&mut self) {
        let vlf_buf_ids = self
            .backend
            .all_bufs()
            .iter()
            .filter(|buf| buf.is_vlf)
            .map(|buf| buf.id)
            .collect::<Vec<_>>();
        for buf_id in vlf_buf_ids {
            self.source_control.remove(&buf_id);
        }

        let now = Instant::now();
        let snapshots = self
            .backend
            .all_bufs()
            .iter()
            .filter(|buf| !buf.is_vlf && buf.is_fully_cached())
            .filter(|buf| {
                self.source_control.get(&buf.id).is_none_or(|cached| {
                    now.duration_since(cached.last_refresh) >= Duration::from_secs(2)
                })
            })
            .map(|buf| (buf.id, buf.path.clone(), buf.lines.clone()))
            .collect::<Vec<_>>();

        for (buf_id, path, lines) in snapshots {
            let fingerprint = git::buffer_fingerprint(path.as_deref(), &lines);
            let stale = self.source_control.get(&buf_id).is_none_or(|cached| {
                cached.fingerprint != fingerprint
                    || cached.path != path
                    || now.duration_since(cached.last_refresh) >= Duration::from_secs(2)
            });
            if !stale {
                continue;
            }

            let status = path
                .as_deref()
                .and_then(|file_path| git::inspect_buffer(file_path, &lines).ok().flatten());
            self.source_control
                .insert(buf_id, GitBufferCache { fingerprint, path, last_refresh: now, status });
        }
    }

    pub(crate) fn input_idle_for(&self, duration: Duration) -> bool {
        Instant::now().duration_since(self.last_input_at) >= duration
    }

    pub(crate) fn git_status(&self, buf_id: crate::buffer::BufferId) -> Option<&GitBufferStatus> {
        if self.backend.all_bufs().iter().any(|buf| buf.id == buf_id && buf.is_vlf) {
            return None;
        }

        self.source_control.get(&buf_id).and_then(|cached| cached.status.as_ref())
    }

    pub(crate) fn current_git_status(&self) -> Option<&GitBufferStatus> {
        self.git_status(self.backend.active().id)
    }

    fn refresh_active_git_status(&mut self) -> Option<GitBufferStatus> {
        if self.block_active_vlf_source_control("git status") {
            return None;
        }

        let buf = self.backend.active();
        if !buf.is_fully_cached() {
            self.backend.status_message =
                Some(String::from("git: status unavailable until buffer lines are loaded"));
            return None;
        }
        let fingerprint = git::buffer_fingerprint(buf.path.as_deref(), &buf.lines);
        let status = buf
            .path
            .as_deref()
            .and_then(|path| git::inspect_buffer(path, &buf.lines).ok().flatten());
        self.source_control.insert(
            buf.id,
            GitBufferCache {
                fingerprint,
                path: buf.path.clone(),
                last_refresh: Instant::now(),
                status: status.clone(),
            },
        );
        status
    }

    fn jump_to_git_hunk(&mut self, forward: bool) {
        self.last_repeatable_motion = Some(RepeatableMotion::GitHunk { forward });
        let Some(status) = self.refresh_active_git_status() else {
            if self.backend.status_message.is_none() {
                self.backend.status_message =
                    Some(String::from("git: current buffer not in repository"));
            }
            return;
        };
        if status.hunks.is_empty() {
            self.backend.status_message = Some(String::from("git: no hunks in current buffer"));
            return;
        }
        let target = if forward {
            status.next_hunk_line(self.backend.cursor_line)
        } else {
            status.prev_hunk_line(self.backend.cursor_line)
        };
        if let Some(line) = target {
            self.jump_to_line(line);
            self.backend.status_message = Some(format!(
                "git hunk: line {} ({}/{})",
                line + 1,
                status.branch,
                status.repo_relative
            ));
        }
    }

    fn jump_to_git_hunk_edge(&mut self, first: bool) {
        let Some(status) = self.refresh_active_git_status() else {
            if self.backend.status_message.is_none() {
                self.backend.status_message =
                    Some(String::from("git: current buffer not in repository"));
            }
            return;
        };
        if status.hunks.is_empty() {
            self.backend.status_message = Some(String::from("git: no hunks in current buffer"));
            return;
        }
        let target = if first { status.first_hunk_line() } else { status.last_hunk_line() };
        if let Some(line) = target {
            self.jump_to_line(line);
            self.backend.status_message = Some(format!(
                "git hunk: line {} ({}/{})",
                line + 1,
                status.branch,
                status.repo_relative
            ));
        }
    }

    fn show_git_blame(&mut self) {
        if self.block_active_vlf_source_control("git blame") {
            return;
        }

        let Some(path) = self.backend.active().path.clone() else {
            self.backend.status_message =
                Some(String::from("git blame unavailable for scratch buffer"));
            return;
        };
        match git::blame_line(&path, self.backend.cursor_line) {
            Ok(Some(blame)) => {
                let content = git::format_blame(&blame, self.backend.cursor_line);
                self.hover_popup = Some(HoverPopup {
                    title: format!("Git Blame {}", self.backend.cursor_line + 1),
                    content: content.clone(),
                });
                self.backend.status_message = Some(content);
            }
            Ok(None) => {
                self.backend.status_message =
                    Some(String::from("git blame unavailable for current line"));
            }
            Err(err) => {
                self.backend.status_message = Some(format!("git blame failed: {err}"));
            }
        }
    }

    fn open_generated_buffer(&mut self, title: &str, body: &str) {
        match self.backend.open_named_scratch_buffer(title) {
            Ok(buf_id) => {
                let _ = self.backend.switch_to_id(buf_id);
                self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
                self.viewport = Viewport::default();
                if !body.is_empty() {
                    let _ = self.backend.send_edit("insert", json!({ "chars": body }));
                }
                self.backend.status_message = Some(title.to_owned());
            }
            Err(err) => {
                self.backend.status_message = Some(format!("{title} failed: {err}"));
            }
        }
    }

    fn run_terminal_command(&mut self, command: crate::terminal::TerminalCommand) {
        let cwd = match std::env::current_dir() {
            Ok(path) => path,
            Err(err) => {
                self.backend.status_message = Some(format!("shell failed: {err}"));
                return;
            }
        };

        match crate::terminal::run_command(&command, &cwd) {
            Ok(result) => {
                let title = command.title.clone();
                let body = crate::terminal::render_transcript(&result);
                self.open_generated_buffer(&title, &body);
                self.backend.status_message = Some(if result.success {
                    format!("{} finished", title)
                } else {
                    format!("{} failed", title)
                });
            }
            Err(err) => {
                self.backend.status_message = Some(format!("shell failed: {err}"));
            }
        }
    }

    fn open_git_diff_view(&mut self, current_hunk_only: bool) {
        let feature = if current_hunk_only { "git hunk diff" } else { "git diff" };
        if self.block_active_vlf_source_control(feature) {
            return;
        }

        let Some(status) = self.refresh_active_git_status() else {
            if self.backend.status_message.is_none() {
                self.backend.status_message =
                    Some(String::from("git diff unavailable for current buffer"));
            }
            return;
        };
        let selected_hunk =
            if current_hunk_only { status.hunk_at_line(self.backend.cursor_line) } else { None };
        if current_hunk_only && selected_hunk.is_none() {
            self.backend.status_message = Some(String::from("git: cursor not inside changed hunk"));
            return;
        }
        let title = if current_hunk_only { "git hunk diff" } else { "git diff" };
        let rendered = git::render_diff(&status, selected_hunk);
        self.open_generated_buffer(title, &rendered);
    }

    fn source_control_disabled_message(feature: &str) -> String {
        format!("{feature} disabled in VLF: {VLF_SOURCE_CONTROL_DISABLED_REASON}")
    }

    fn block_active_vlf_source_control(&mut self, feature: &str) -> bool {
        let (is_vlf, buf_id) = {
            let buf = self.backend.active();
            (buf.is_vlf, buf.id)
        };
        if !is_vlf {
            return false;
        }

        self.source_control.remove(&buf_id);
        self.backend.status_message = Some(Self::source_control_disabled_message(feature));
        true
    }

    // ── Recording helpers ──────────────────────────────────────────────────

    /// Send a xi edit and, if recording is active, append it to the command log.
    fn record_edit(&mut self, method: &'static str, params: serde_json::Value) {
        if self.recording {
            self.recorded_commands.push((method, params.clone()));
        }
        let _ = self.backend.send_edit(method, params);
    }

    fn begin_record(&mut self) {
        self.recording = true;
        self.recorded_commands.clear();
    }

    fn end_record(&mut self) {
        self.recording = false;
        let cmds = self.recorded_commands.drain(..).collect();
        self.last_change = Some(LastChange::Commands(cmds));
    }

    // ── Window management ──────────────────────────────────────────────────

    fn sync_backend_to_focused_window(&mut self) {
        let new_buf = self.tabs.focused_windows().focused_window().buffer_id;
        let _ = self.backend.switch_to_id(new_buf);
    }

    fn rotate_view(&mut self) {
        let new_vp = self.tabs.focused_windows_mut().focus_next(self.viewport);
        self.viewport = new_vp;
        self.sync_backend_to_focused_window();
    }

    fn rotate_view_reverse(&mut self) {
        let new_vp = self.tabs.focused_windows_mut().focus_prev(self.viewport);
        self.viewport = new_vp;
        self.sync_backend_to_focused_window();
    }

    fn transpose_view(&mut self) {
        self.tabs.focused_windows_mut().transpose();
    }

    fn jump_view(&mut self, direction: ViewDirection) {
        let new_vp = self.tabs.focused_windows_mut().focus_direction(direction, self.viewport);
        self.viewport = new_vp;
        self.sync_backend_to_focused_window();
    }

    fn swap_view(&mut self, direction: ViewDirection) {
        if self.tabs.focused_windows_mut().swap_focused_with_direction(direction) {
            self.sync_backend_to_focused_window();
        }
    }

    fn close_view(&mut self) {
        if let Some(new_vp) = self.tabs.focused_windows_mut().close_focused() {
            self.viewport = new_vp;
            self.sync_backend_to_focused_window();
        }
    }

    fn close_other_views(&mut self) {
        let new_vp = self.tabs.focused_windows_mut().close_others(self.viewport);
        self.viewport = new_vp;
        self.sync_backend_to_focused_window();
    }

    /// Handle a key pressed after `Ctrl-W` in Normal mode.
    fn handle_window_cmd(&mut self, c: char) {
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

    // ── Marks ──────────────────────────────────────────────────────────────

    /// Set mark `c` to the current cursor position.
    fn set_mark(&mut self, c: char) {
        let pos = (self.backend.cursor_line, self.backend.cursor_col);
        self.marks.insert(c, pos);
    }

    /// Jump to mark `c`.  `line_start=true` moves to the first non-whitespace
    /// on the mark's line; `false` jumps to the exact saved byte column.
    fn jump_to_mark(&mut self, c: char, line_start: bool) {
        // Special marks: `'` / `` ` `` jump to position before last jump.
        let pos = if c == '\'' || c == '`' {
            // Re-use the last jump list entry as the "previous position" mark.
            match self.jump_list.last().copied() {
                Some(p) => p,
                None => return,
            }
        } else {
            match self.marks.get(&c).copied() {
                Some(p) => p,
                None => return,
            }
        };

        self.push_jump();
        let (line, col) = pos;
        let col = if line_start {
            // Find first non-whitespace byte offset on the target line.
            self.backend
                .lines
                .get(line)
                .map(|l| l.find(|ch: char| !ch.is_whitespace()).unwrap_or(0))
                .unwrap_or(0)
        } else {
            col
        };
        self.move_cursor_to(line, col);
    }

    // ── Jump list ──────────────────────────────────────────────────────────

    const JUMP_LIST_MAX: usize = 100;

    /// Push the current cursor position onto the jump list.
    /// Resets navigation to the head of the list.
    pub(crate) fn push_jump(&mut self) {
        let pos = (self.backend.cursor_line, self.backend.cursor_col);
        // Avoid duplicates at the head.
        if self.jump_list.last() == Some(&pos) {
            self.jump_list_idx = self.jump_list.len();
            return;
        }
        self.jump_list.push(pos);
        if self.jump_list.len() > Self::JUMP_LIST_MAX {
            self.jump_list.remove(0);
        }
        self.jump_list_idx = self.jump_list.len();
    }

    fn jump_list_older(&mut self) {
        if self.jump_list.is_empty() {
            return;
        }
        // First Ctrl-O saves current position, then steps back.
        if self.jump_list_idx == self.jump_list.len() {
            let pos = (self.backend.cursor_line, self.backend.cursor_col);
            if self.jump_list.last() != Some(&pos) {
                self.jump_list.push(pos);
                if self.jump_list.len() > Self::JUMP_LIST_MAX {
                    self.jump_list.remove(0);
                }
                self.jump_list_idx = self.jump_list.len();
            }
        }
        if self.jump_list_idx == 0 {
            return;
        }
        self.jump_list_idx -= 1;
        let (line, col) = self.jump_list[self.jump_list_idx];
        self.move_cursor_to(line, col);
    }

    fn jump_list_newer(&mut self) {
        if self.jump_list_idx + 1 >= self.jump_list.len() {
            return;
        }
        self.jump_list_idx += 1;
        let (line, col) = self.jump_list[self.jump_list_idx];
        self.move_cursor_to(line, col);
    }

    // ── Change list ────────────────────────────────────────────────────────

    const CHANGE_LIST_MAX: usize = 100;

    /// Push the current cursor position onto the change list.
    /// Called after any buffer-modifying operation.
    pub(crate) fn push_change(&mut self) {
        self.backend.note_buffer_modified(self.backend.active().id);
        let pos = (self.backend.cursor_line, self.backend.cursor_col);
        if self.change_list.last() == Some(&pos) {
            self.change_list_idx = self.change_list.len().saturating_sub(1);
            return;
        }
        self.change_list.push(pos);
        if self.change_list.len() > Self::CHANGE_LIST_MAX {
            self.change_list.remove(0);
        }
        self.change_list_idx = self.change_list.len().saturating_sub(1);
    }

    fn change_list_older(&mut self) {
        if self.change_list.is_empty() {
            return;
        }
        let (line, col) = self.change_list[self.change_list_idx];
        self.move_cursor_to(line, col);
        self.change_list_idx = self.change_list_idx.saturating_sub(1);
    }

    fn change_list_newer(&mut self) {
        if self.change_list.is_empty() {
            return;
        }
        let next = (self.change_list_idx + 1).min(self.change_list.len().saturating_sub(1));
        self.change_list_idx = next;
        let (line, col) = self.change_list[self.change_list_idx];
        self.move_cursor_to(line, col);
    }

    // ── Cursor movement helper ──────────────────────────────────────────────

    /// Move the xi cursor to the given (line, byte_col) via a gesture point_select.
    fn move_cursor_to(&mut self, line: usize, col: usize) {
        let _ = self.backend.send_edit(
            "gesture",
            json!({ "line": line as u64, "col": col as u64, "ty": "point_select" }),
        );
    }

    // ── Macro recording / replay ────────────────────────────────────────────

    /// Start recording keystrokes into register `c`.
    fn start_macro_record(&mut self, c: char) {
        self.macro_register = Some(c);
        self.macro_buffer.clear();
        self.backend.status_message = Some(format!("recording @{c}"));
    }

    /// Stop recording and store the accumulated keystrokes in the macros map.
    fn stop_macro_record(&mut self) {
        let Some(c) = self.macro_register.take() else { return };
        // The key that triggered stop_macro_record ('q') was already pushed to
        // macro_buffer by handle_event; remove it.
        self.macro_buffer.pop();
        let keys = self.macro_buffer.drain(..).collect();
        self.macros.insert(c, keys);
        self.last_macro = Some(c);
        self.backend.status_message = None;
    }

    /// Replay the macro stored in register `c` (count times).
    fn replay_macro(&mut self, c: char) {
        let count = self.input_state.count();
        self.input_state.reset();
        let keys = match self.macros.get(&c).cloned() {
            Some(k) if !k.is_empty() => k,
            _ => return,
        };
        self.last_macro = Some(c);
        self.macro_replaying = true;
        for _ in 0..count {
            for key in keys.iter().copied() {
                self.handle_event(Event::Key(key));
            }
        }
        self.macro_replaying = false;
    }

    // ── Register helpers ───────────────────────────────────────────────────

    /// Consume the pending register, falling back to `Unnamed`.
    fn take_register(&mut self) -> RegisterName {
        self.input_state.pending_register.take().unwrap_or(RegisterName::Unnamed)
    }

    fn insert_register_for_current_mode(&mut self, reg: RegisterName) {
        let text = self.registers.get(&reg);
        if text.is_empty() {
            return;
        }

        match self.mode {
            Mode::Insert => {
                self.insert_buffer.push_str(&text);
                let _ = self.backend.send_edit("insert", json!({ "chars": text }));
            }
            Mode::CommandLine => {
                self.history_idx = None;
                self.command_buffer.push_str(&text);
            }
            Mode::Search => {
                self.command_buffer.push_str(&text);
                let chars = self.command_buffer.clone();
                let case_sensitive = smart_case_sensitive(&chars);
                let _ = self.backend.send_edit(
                    "find",
                    json!({
                        "chars": chars,
                        "case_sensitive": case_sensitive,
                        "regex": false,
                        "whole_words": false
                    }),
                );
            }
            _ => {}
        }
    }

    fn selected_text_preview(&mut self, linewise: bool) -> String {
        if self.mode == Mode::VisualBlock {
            let (al, ac) =
                self.visual_anchor.unwrap_or((self.backend.cursor_line, self.backend.cursor_col));
            let cl = self.backend.cursor_line;
            let cc = self.backend.cursor_col;
            let (top, bottom) = if al <= cl { (al, cl) } else { (cl, al) };
            let (left_col, right_col) = if ac <= cc { (ac, cc) } else { (cc, ac) };
            return self
                .backend
                .block_text_preview(top, bottom, left_col, right_col)
                .unwrap_or_default();
        }

        self.backend.selected_text_preview(linewise).unwrap_or_default()
    }

    // ── Paste ──────────────────────────────────────────────────────────────

    /// Paste register content.  `before` = `P` (before cursor), else `p`.
    fn paste(&mut self, before: bool) {
        let reg = self.take_register();
        let text = self.registers.get(&reg);
        if text.is_empty() {
            return;
        }
        self.record_edit("paste_register", json!({ "chars": text, "before": before }));
        self.push_change();
    }

    fn paste_from_register(&mut self, reg: RegisterName, before: bool) {
        self.input_state.pending_register = Some(reg);
        self.paste(before);
    }

    // ── Repeat last change ──────────────────────────────────────────────────

    fn repeat_last_change(&mut self) {
        let change = match self.last_change.clone() {
            Some(c) => c,
            None => return,
        };
        match change {
            LastChange::Insert(text) => {
                let _ = self.backend.send_edit("insert", json!({ "chars": text }));
            }
            LastChange::Commands(cmds) => {
                for (method, params) in cmds {
                    let _ = self.backend.send_edit(method, params);
                }
            }
        }
    }

    // ── Visual mode helpers ─────────────────────────────────────────────────

    fn enter_visual_line(&mut self) {
        if self.backend.is_vlf {
            self.visual_restore_cursor = Some((self.backend.cursor_line, self.backend.cursor_col));
        } else {
            self.visual_restore_cursor = None;
        }
        let anchor = (self.backend.cursor_line, 0);
        self.visual_anchor = Some(anchor);
        self.mode = Mode::VisualLine;
        if self.backend.is_vlf {
            self.backend.cursor_col = 0;
            return;
        }
        // Immediately select the whole current line.
        let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
        let _ = self.backend.send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
    }

    fn enter_visual_block(&mut self) {
        let anchor = (self.backend.cursor_line, self.backend.cursor_col);
        self.visual_anchor = Some(anchor);
        self.mode = Mode::VisualBlock;
        // No xi selection yet; block region is defined by anchor + cursor.
    }

    /// Re-send xi line-wise selection from the anchor line to the cursor line.
    fn sync_visual_line_selection(&mut self) {
        let (al, _ac) = match self.visual_anchor {
            Some(a) => a,
            None => return,
        };
        let cl = self.backend.cursor_line;
        let (top, bottom) = if al <= cl { (al, cl) } else { (cl, al) };
        // Select from beginning of top line to end of bottom line.
        let _ = self.backend.send_edit(
            "gesture",
            json!({
                "line": top as u64,
                "col": 0u64,
                "ty": { "select": { "granularity": "point", "multi": false } }
            }),
        );
        let bottom_len = self.backend.lines.get(bottom).map(|s| s.len()).unwrap_or(0);
        let _ = self.backend.send_edit(
            "gesture",
            json!({
                "line": bottom as u64,
                "col": bottom_len as u64,
                "ty": { "select_extend": { "granularity": "point" } }
            }),
        );
    }

    fn swap_visual_anchor(&mut self) {
        if let Some((al, ac)) = self.visual_anchor {
            let old_cursor = (self.backend.cursor_line, self.backend.cursor_col);
            // Move xi cursor to the old anchor position.
            let _ = self.backend.send_edit(
                "gesture",
                json!({
                    "line": al as u64,
                    "col": ac as u64,
                    "ty": { "select": { "granularity": "point", "multi": false } }
                }),
            );
            self.visual_anchor = Some(old_cursor);
            if self.mode == Mode::VisualLine {
                self.sync_visual_line_selection();
            }
        }
    }

    fn restore_last_visual(&mut self) {
        if let Some((saved_mode, al, ac)) = self.last_visual {
            self.visual_anchor = Some((al, ac));
            self.mode = saved_mode;
            // Position xi cursor at anchor (selection will be set by movement).
            let _ = self.backend.send_edit(
                "gesture",
                json!({
                    "line": al as u64,
                    "col": ac as u64,
                    "ty": { "select": { "granularity": "point", "multi": false } }
                }),
            );
            if saved_mode == Mode::VisualLine {
                self.sync_visual_line_selection();
            }
        }
    }

    /// Handle a character key while in any visual mode.
    fn handle_visual_char(&mut self, ch: char) {
        match ch {
            // Operators
            'd' => {
                self.begin_record();
                if self.mode == Mode::VisualLine {
                    self.apply_visual_line_delete();
                } else if self.mode == Mode::VisualBlock {
                    self.apply_visual_block_op(Operator::Delete);
                } else {
                    let reg = self.take_register();
                    let text = self.selected_text_preview(false);
                    self.registers.delete(&reg, text, false);
                    self.record_edit("delete_forward", json!([]));
                    self.enter_normal_mode();
                }
                self.end_record();
            }
            'y' => {
                self.begin_record();
                if self.mode == Mode::VisualLine {
                    self.apply_visual_line_yank();
                } else if self.mode == Mode::VisualBlock {
                    self.apply_visual_block_op(Operator::Yank);
                } else {
                    let reg = self.take_register();
                    let text = self.selected_text_preview(false);
                    self.registers.yank(&reg, text, false);
                    let _ = self.backend.send_edit("collapse_selections", json!([]));
                    self.enter_normal_mode();
                }
                self.end_record();
            }
            'c' => {
                self.begin_record();
                if self.mode == Mode::VisualLine {
                    let reg = self.take_register();
                    let text = self.selected_text_preview(true);
                    self.registers.delete(&reg, text, false);
                    self.record_edit("delete_forward", json!([]));
                    self.enter_normal_mode();
                    self.mode = Mode::Insert;
                } else if self.mode == Mode::VisualBlock {
                    self.apply_visual_block_op(Operator::Change);
                    self.mode = Mode::Insert;
                } else {
                    let reg = self.take_register();
                    let text = self.selected_text_preview(false);
                    self.registers.delete(&reg, text, false);
                    self.record_edit("delete_forward", json!([]));
                    self.enter_normal_mode();
                    self.mode = Mode::Insert;
                }
                self.end_record();
            }
            '>' => {
                self.begin_record();
                self.record_edit("indent", json!([]));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.end_record();
                self.enter_normal_mode();
            }
            '<' => {
                self.begin_record();
                self.record_edit("outdent", json!([]));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.end_record();
                self.enter_normal_mode();
            }
            'U' => {
                self.begin_record();
                self.record_edit("uppercase", json!([]));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.end_record();
                self.enter_normal_mode();
            }
            'u' => {
                self.begin_record();
                self.record_edit("lowercase", json!([]));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.end_record();
                self.enter_normal_mode();
            }
            // `o` — swap anchor (handled as Action::SwapVisualAnchor in bindings,
            // but also catch it here for VisualLine/VisualBlock where not bound).
            'o' => self.swap_visual_anchor(),
            _ => {}
        }
    }

    fn apply_visual_line_delete(&mut self) {
        let reg = self.take_register();
        let text = self.selected_text_preview(true);
        self.registers.delete(&reg, text, false);
        self.sync_visual_line_selection();
        self.record_edit("delete_forward", json!([]));
        self.enter_normal_mode();
    }

    fn apply_visual_line_yank(&mut self) {
        let reg = self.take_register();
        let text = self.selected_text_preview(true);
        self.registers.yank(&reg, text, false);
        // Collapse without deleting.
        let _ = self.backend.send_edit("collapse_selections", json!([]));
        self.enter_normal_mode();
    }

    /// Apply an operator to each line in the block visual selection.
    fn apply_visual_block_op(&mut self, op: Operator) {
        let (al, ac) =
            self.visual_anchor.unwrap_or((self.backend.cursor_line, self.backend.cursor_col));
        let cl = self.backend.cursor_line;
        let cc = self.backend.cursor_col;
        let (top, bottom) = if al <= cl { (al, cl) } else { (cl, al) };
        let (left_col, right_col) = if ac <= cc { (ac, cc) } else { (cc, ac) };
        let extracted =
            self.backend.block_text_preview(top, bottom, left_col, right_col).unwrap_or_default();
        if op == Operator::Yank {
            let reg = self.take_register();
            self.registers.yank(&reg, extracted, false);
            let _ = self.backend.send_edit("collapse_selections", json!([]));
            self.enter_normal_mode();
            return;
        }
        // For delete/change: iterate lines from bottom to top to preserve offsets.
        let _ = self.backend.delete_block(top, bottom, left_col, right_col);
        self.push_change();
        let reg = self.take_register();
        self.registers.delete(&reg, extracted, false);
        self.enter_normal_mode();
        if op == Operator::Change {
            self.mode = Mode::Insert;
        }
    }

    // ── Visual block insert / append ────────────────────────────────────────

    /// Set up a block-insert (`I`) or block-append (`A`) for the current block
    /// visual selection.  Actual text is applied on leaving insert mode.
    fn visual_block_insert(&mut self, append: bool) {
        let (al, ac) =
            self.visual_anchor.unwrap_or((self.backend.cursor_line, self.backend.cursor_col));
        let cl = self.backend.cursor_line;
        let cc = self.backend.cursor_col;
        let (top, bottom) = if al <= cl { (al, cl) } else { (cl, al) };
        let col = if append {
            ac.max(cc) // right edge
        } else {
            ac.min(cc) // left edge
        };
        self.block_insert = Some(BlockInsert { line_start: top, line_end: bottom, col, append });
        // Position cursor at the insertion column on the top line.
        let _ = self.backend.send_edit(
            "gesture",
            json!({
                "line": top as u64,
                "col": col as u64,
                "ty": "point_select"
            }),
        );
        self.visual_anchor = None;
        self.mode = Mode::Insert;
    }

    /// Apply deferred block-insert text.  Called when leaving insert mode.
    fn apply_block_insert(&mut self) {
        let bi = match self.block_insert.take() {
            Some(b) => b,
            None => return,
        };
        let text = self.insert_buffer.clone();
        if text.is_empty() {
            return;
        }
        if bi.line_start < bi.line_end {
            let _ = self.backend.replay_block_insert(
                bi.line_start + 1,
                bi.line_end,
                bi.col,
                &text,
                bi.append,
            );
        }
    }

    // ── Substitute confirm mode ───────────────────────────────────────────

    fn apply_all_substitute_matches(&mut self) {
        while self.substitute_pending.as_ref().is_some_and(|s| s.current < s.matches.len()) {
            self.apply_substitute_current();
        }
    }

    fn cancel_substitute_confirm(&mut self) {
        let applied = self.substitute_pending.as_ref().map(|s| s.applied).unwrap_or(0);
        self.substitute_pending = None;
        self.mode = Mode::Normal;
        self.backend.status_message = Some(format!("{applied} substitution(s) applied"));
    }

    /// Apply the current pending substitution match and advance.
    fn apply_substitute_current(&mut self) {
        let Some(state) = self.substitute_pending.as_mut() else {
            return;
        };
        let idx = state.current;
        if idx >= state.matches.len() {
            return;
        }
        let replacement = state.matches[idx].clone();
        let _ = self.backend.apply_line_replacements(&[replacement]);
        state.applied += 1;
        state.current += 1;
        self.advance_substitute_confirm_inner();
    }

    /// Skip the current pending match and advance.
    fn advance_substitute_confirm(&mut self) {
        let Some(state) = self.substitute_pending.as_mut() else {
            return;
        };
        state.current += 1;
        self.advance_substitute_confirm_inner();
    }

    /// Finish confirmation if all matches exhausted, otherwise jump to next.
    fn advance_substitute_confirm_inner(&mut self) {
        let done = self.substitute_pending.as_ref().is_none_or(|s| s.current >= s.matches.len());
        if done {
            let applied = self.substitute_pending.as_ref().map(|s| s.applied).unwrap_or(0);
            self.substitute_pending = None;
            self.mode = Mode::Normal;
            self.backend.status_message = Some(format!("{applied} substitution(s) applied"));
        } else {
            let (li, total, current) = if let Some(pending) = &self.substitute_pending {
                (pending.matches[pending.current].line, pending.matches.len(), pending.current)
            } else {
                return;
            };
            self.jump_to_line(li);
            self.backend.status_message =
                Some(format!("substitute ({}/{total}) replace? [y/n/a/q]", current + 1));
        }
    }

    // ── Substitute helper ─────────────────────────────────────────────────

    /// Execute a `:s/pattern/replacement/flags` command on `lines[start..=end]`.
    ///
    /// Flags: `g` = replace all occurrences per line (default: first only),
    ///        `i` = case-insensitive (default: smart-case),
    ///        `c` = confirm each change interactively.
    ///
    /// Delegates substitute preview and apply work to xi-core so range and
    /// confirm semantics always operate on the authoritative rope.
    pub(crate) fn execute_substitute(
        &mut self,
        start: usize,
        end: usize,
        pattern: &str,
        replacement: &str,
        flags: &str,
    ) {
        if pattern.is_empty() {
            self.backend.status_message = Some("substitute: empty pattern".to_owned());
            return;
        }
        let global = flags.contains('g');
        let case_insensitive =
            flags.contains('i') || (!flags.contains('I') && !smart_case_sensitive(pattern));
        let confirm = flags.contains('c');
        let changes = match self.backend.substitute_preview(
            start,
            end,
            pattern,
            replacement,
            global,
            !case_insensitive,
        ) {
            Ok(changes) => changes,
            Err(err) => {
                self.backend.status_message = Some(format!("substitute: {err}"));
                return;
            }
        };

        if changes.is_empty() {
            self.backend.status_message = Some("substitute: pattern not found".to_owned());
            return;
        }

        if confirm {
            // Enter confirm mode.
            let total = changes.len();
            let first_line = changes[0].line;
            self.substitute_pending =
                Some(SubstitutePending { matches: changes, current: 0, applied: 0 });
            self.mode = Mode::SubstituteConfirm;
            self.jump_to_line(first_line);
            self.backend.status_message =
                Some(format!("substitute (1/{total}) replace? [y/n/a/q]"));
        } else {
            // Apply authoritative replacements in one backend-owned edit.
            let count = changes.len();
            let _ = self.backend.apply_line_replacements(&changes);
            self.push_change();
            self.backend.status_message =
                Some(format!("{count} substitution(s) on {count} line(s)"));
        }
    }
}

fn parse_surround_pair(spec: &str) -> Option<(String, String)> {
    match spec {
        "(" | ")" | "b" => Some(("(".to_owned(), ")".to_owned())),
        "[" | "]" => Some(("[".to_owned(), "]".to_owned())),
        "{" | "}" | "B" => Some(("{".to_owned(), "}".to_owned())),
        "<" | ">" => Some(("<".to_owned(), ">".to_owned())),
        "\"" => Some(("\"".to_owned(), "\"".to_owned())),
        "'" => Some(("'".to_owned(), "'".to_owned())),
        "`" => Some(("`".to_owned(), "`".to_owned())),
        _ => {
            let mut chars = spec.chars();
            let open = chars.next()?;
            let close = chars.next()?;
            if chars.next().is_some() { None } else { Some((open.to_string(), close.to_string())) }
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum ShellSelectionMode {
    Replace,
    IgnoreOutput,
    InsertBefore,
    InsertAfter,
    KeepByStatus,
}

pub(super) fn format_git_hunk_replacement(
    old_lines: &[&str],
    new_start: usize,
    new_count: usize,
    total_lines: usize,
) -> String {
    if old_lines.is_empty() {
        return String::new();
    }

    let joined = old_lines.join("\n");
    let has_following_line = new_start + new_count < total_lines;
    if new_count == 0 {
        if total_lines == 0 {
            joined
        } else if has_following_line {
            format!("{joined}\n")
        } else {
            format!("\n{joined}")
        }
    } else if has_following_line {
        format!("{joined}\n")
    } else {
        joined
    }
}
