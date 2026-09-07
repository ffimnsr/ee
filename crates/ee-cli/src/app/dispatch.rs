//! `impl App` methods: dispatch domain.
use super::*;

impl App {
    pub(super) fn dispatch(&mut self, action: Action, _key: KeyEvent) {
        match &action {
            Action::NoOp
            | Action::AgentHistoryPrevious
            | Action::AgentHistoryNext
            | Action::AgentHistorySearchReverse
            | Action::AgentDraftStash
            | Action::AgentDraftRestore
            | Action::AgentDraftExternalEdit
            | Action::AgentToggleTranscriptDetails
            | Action::AgentToggleTranscriptRaw
            | Action::SetPrefix(_)
            | Action::PendingCharFind { .. }
            | Action::SetOperator(_)
            | Action::SwiftMotion => {}
            _ => {
                self.input_state.prefix = None;
                self.input_state.pending_find = None;
            }
        }

        match action {
            Action::NoOp
            | Action::AgentHistoryPrevious
            | Action::AgentHistoryNext
            | Action::AgentHistorySearchReverse
            | Action::AgentDraftStash
            | Action::AgentDraftRestore
            | Action::AgentDraftExternalEdit
            | Action::AgentToggleTranscriptDetails
            | Action::AgentToggleTranscriptRaw => {}
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
                self.command_mode_origin = Some(self.mode);
                self.mode = Mode::CommandLine;
                self.command_buffer.clear();
            }
            Action::PrefillCommandLine(template) => {
                self.command_mode_origin = Some(self.mode);
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
                if self.handle_vlf_direct_edit_action(method, count as usize) {
                    return;
                }
                if let Some(motion) = self.fold_vertical_motion(method, count as usize) {
                    if self.handle_vlf_navigation(method, motion.line_count as u64) {
                        return;
                    }
                    let _ = self.backend.send_edit(
                        "move_vertical",
                        json!({
                            "lines": motion.line_count,
                            "up": motion.up,
                            "modify_selection": motion.modify_selection,
                        }),
                    );
                    return;
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
                if self.try_vlf_delete_backward(count as usize) {
                    return;
                }
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
            Action::SwiftMotion => self.start_swift_motion(),
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
                if self.backend.active().is_vlf {
                    self.move_vlf_cursor_right();
                    self.mode = Mode::Insert;
                    return;
                }
                let _ = self.backend.send_edit("move_right", json!([]));
                self.mode = Mode::Insert;
            }
            Action::AppendAtEndOfLine => {
                if self.backend.active().is_vlf {
                    self.move_vlf_cursor_to_line_end();
                    self.mode = Mode::Insert;
                    return;
                }
                let _ = self.backend.send_edit("move_to_right_end_of_line", json!([]));
                self.mode = Mode::Insert;
            }
            Action::InsertAtLineStart => {
                if self.backend.active().is_vlf {
                    self.backend.cursor_col = 0;
                    self.backend.clamp_cursor();
                    self.mode = Mode::Insert;
                    return;
                }
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                self.mode = Mode::Insert;
            }
            Action::OpenLineBelow => {
                if self.backend.active().is_vlf {
                    self.move_vlf_cursor_to_line_end();
                    let _ = self.send_vlf_replace_range_at_cursor("\n");
                    self.mode = Mode::Insert;
                    return;
                }
                let _ = self.backend.send_edit("move_to_right_end_of_line", json!([]));
                let _ = self.backend.send_edit("insert_newline", json!([]));
                self.mode = Mode::Insert;
            }
            Action::OpenLineAbove => {
                if self.backend.active().is_vlf {
                    let _ = self.block_vlf_editing("open line above");
                    return;
                }
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                let _ = self.backend.send_edit("insert_newline", json!([]));
                let _ = self.backend.send_edit("move_up", json!([]));
                self.mode = Mode::Insert;
            }
            Action::SubstituteChar => {
                if self.backend.active().is_vlf {
                    let count = self.input_state.count();
                    let _ = self.try_vlf_delete_forward(count as usize);
                    self.mode = Mode::Insert;
                    return;
                }
                let count = self.input_state.count();
                for _ in 0..count {
                    let _ = self.backend.send_edit("delete_forward", json!([]));
                }
                self.mode = Mode::Insert;
            }
            Action::SubstituteLine => {
                if self.backend.active().is_vlf {
                    let line = self.backend.cursor_line;
                    if let Some(text) = self.backend.get_line(line) {
                        let _ = self.backend.vlf_replace_range(line, 0, line, text.len(), "");
                        self.backend.cursor_line = line;
                        self.backend.cursor_col = 0;
                        self.backend.clamp_cursor();
                        self.refresh_vlf_viewport();
                    }
                    self.mode = Mode::Insert;
                    return;
                }
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
}
