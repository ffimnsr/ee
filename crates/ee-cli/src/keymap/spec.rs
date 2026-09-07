//! Binding- and action-spec parsing/formatting.
use super::*;

pub(crate) fn parse_binding_spec(
    mode: &str,
    key: &str,
    prefix: Option<&str>,
) -> Result<BindingKey, String> {
    let mode = parse_binding_mode(mode)?;
    let key_press = parse_key_press_spec(key)?;
    let prefix = parse_prefix_spec(prefix)?;
    Ok(BindingKey { mode, key: key_press.key, modifiers: key_press.modifiers, prefix })
}

pub(crate) fn parse_binding_mode(spec: &str) -> Result<Mode, String> {
    parse_binding_mode_spec(spec).ok_or_else(|| format!("unknown mode `{spec}`"))
}

pub(crate) fn parse_action_spec(spec: &str) -> Result<Action, String> {
    let spec = spec.trim();

    if let Some(mode) = spec.strip_prefix("enter_mode:") {
        return parse_mode_spec(mode)
            .map(Action::EnterMode)
            .ok_or_else(|| format!("unknown mode `{mode}`"));
    }

    if let Some(method) = spec.strip_prefix("edit:") {
        return parse_edit_method(method)
            .map(Action::Edit)
            .ok_or_else(|| format!("unknown edit method `{method}`"));
    }

    if let Some(prefix) = spec.strip_prefix("set_prefix:") {
        return parse_prefix_char(prefix).map(Action::SetPrefix);
    }

    if let Some(operator) = spec.strip_prefix("set_operator:") {
        return parse_operator_spec(operator)
            .map(Action::SetOperator)
            .ok_or_else(|| format!("unknown operator `{operator}`"));
    }

    if let Some(method) = parse_edit_method(spec) {
        return Ok(Action::Edit(method));
    }

    if let Some(rest) = spec.strip_prefix("pending_char_find:") {
        let mut parts = rest.split(':');
        let direction = parts.next().unwrap_or_default();
        let inclusive = parts.next().unwrap_or_default();
        if parts.next().is_some() {
            return Err(format!("invalid pending_char_find spec `{rest}`"));
        }
        let forward = match direction {
            "forward" => true,
            "backward" => false,
            _ => return Err(format!("unknown direction `{direction}`")),
        };
        let inclusive = match inclusive {
            "inclusive" => true,
            "exclusive" => false,
            _ => return Err(format!("unknown inclusivity `{inclusive}`")),
        };
        return Ok(Action::PendingCharFind { forward, inclusive });
    }

    if let Some(rest) = spec.strip_prefix("move_word_start:") {
        let mut parts = rest.split(':');
        let direction = parts.next().unwrap_or_default();
        let family = parts.next().unwrap_or("word");
        if parts.next().is_some() {
            return Err(format!("invalid move_word_start spec `{rest}`"));
        }
        let forward = match direction {
            "forward" | "next" => true,
            "backward" | "prev" | "previous" => false,
            _ => return Err(format!("unknown direction `{direction}`")),
        };
        let long_word = match family {
            "word" => false,
            "long" | "long_word" | "big" | "big_word" => true,
            _ => return Err(format!("unknown word family `{family}`")),
        };
        return Ok(Action::MoveWordStart { forward, long_word });
    }

    if let Some(family) = spec.strip_prefix("move_word_end:") {
        let long_word = match family {
            "word" => false,
            "long" | "long_word" | "big" | "big_word" => true,
            _ => return Err(format!("unknown word family `{family}`")),
        };
        return Ok(Action::MoveWordEnd { long_word });
    }

    if let Some(direction) = spec.strip_prefix("search_word_under_cursor:") {
        let forward = match direction {
            "forward" => true,
            "backward" => false,
            _ => return Err(format!("unknown direction `{direction}`")),
        };
        return Ok(Action::SearchWordUnderCursor { forward });
    }

    if let Some(mode) = spec.strip_prefix("mark_jump_prefix:") {
        let line_start = match mode {
            "line" | "line_start" => true,
            "exact" => false,
            _ => return Err(format!("unknown mark jump mode `{mode}`")),
        };
        return Ok(Action::MarkJumpPrefix { line_start });
    }

    let action = match spec {
        "no_op" => Action::NoOp,
        "normal_mode" => Action::EnterMode(Mode::Normal),
        "quit" => Action::Quit,
        "enter_command_mode" => Action::EnterCommandMode,
        "collapse_and_enter_normal" => Action::CollapseAndEnterNormal,
        "execute_command" => Action::ExecuteCommand,
        "shell_pipe" => Action::PrefillCommandLine("pipe "),
        "shell_pipe_to" => Action::PrefillCommandLine("pipe_to "),
        "shell_insert_output" => Action::PrefillCommandLine("shell_insert_output "),
        "shell_append_output" => Action::PrefillCommandLine("shell_append_output "),
        "shell_keep_pipe" => Action::PrefillCommandLine("shell_keep_pipe "),
        "complete_command_line" => Action::CompleteCommandLine,
        "delete_backward" => Action::DeleteBackward,
        "command_backspace" => Action::CommandBackspace,
        "search_backspace" => Action::SearchBackspace,
        "enter_search" => Action::EnterSearch,
        "enter_search_backward" => Action::EnterSearchBackward,
        "execute_search" => Action::ExecuteSearch,
        "record_macro" => Action::MacroRecordToggle,
        "replay_macro" => Action::MacroReplayPrefix,
        "search" => Action::EnterSearch,
        "reverse_search" | "rsearch" => Action::EnterSearchBackward,
        "search_next" => Action::FindNext,
        "search_prev" => Action::FindPrevious,
        "global_search" => Action::GlobalSearch,
        "swift_motion" => Action::SwiftMotion,
        "search_selection_detect_word_boundaries" => {
            Action::SearchSelection { detect_word_boundaries: true }
        }
        "search_selection" => Action::SearchSelection { detect_word_boundaries: false },
        "find_next" => Action::FindNext,
        "find_previous" => Action::FindPrevious,
        "goto_line" => Action::GotoLine,
        "goto_column" => Action::GotoColumn,
        "goto_first_nonwhitespace" => Action::GotoFirstNonWhitespace,
        "goto_file_start" => Action::GotoFileStart,
        "goto_last_line" => Action::GotoLastLine,
        "goto_last_modification" => Action::ChangeListOlder,
        "goto_file" => Action::GotoFile,
        "goto_window_top" => Action::GotoWindowTop,
        "goto_window_center" => Action::GotoWindowCenter,
        "goto_window_bottom" => Action::GotoWindowBottom,
        "goto_last_accessed_file" => Action::GotoLastAccessedFile,
        "goto_last_modified_file" => Action::GotoLastModifiedFile,
        "goto_declaration" => Action::RequestDeclaration,
        "goto_definition" => Action::RequestDefinition,
        "goto_type_definition" => Action::RequestTypeDefinition,
        "goto_reference" | "select_references_to_symbol_under_cursor" => Action::RequestReferences,
        "goto_implementation" => Action::RequestImplementation,
        "file_picker" => Action::FilePicker,
        "file_picker_in_current_directory" => Action::FilePickerInCurrentDirectory,
        "file_explorer" => Action::FileExplorer,
        "file_explorer_in_current_buffer_directory" => Action::FileExplorerInCurrentBufferDirectory,
        "file_explorer_in_current_directory" => Action::FileExplorerInCurrentDirectory,
        "buffer_picker" => Action::BufferPicker,
        "jumplist_picker" => Action::JumpListPicker,
        "changed_file_picker" => Action::ChangedFilePicker,
        "symbol_picker" => Action::RequestDocumentSymbols,
        "workspace_symbol_picker" => Action::RequestWorkspaceSymbols,
        "diagnostics_picker" => Action::DiagnosticsPicker,
        "workspace_diagnostics_picker" => Action::WorkspaceDiagnosticsPicker,
        "last_picker" => Action::LastPicker,
        "picker_close" => Action::PickerClose,
        "picker_confirm" => Action::PickerConfirm,
        "picker_move_up" => Action::PickerMoveUp,
        "picker_move_down" => Action::PickerMoveDown,
        "picker_backspace" => Action::PickerBackspace,
        "quickfix_close" => Action::QuickfixClose,
        "quickfix_confirm" => Action::QuickfixConfirm,
        "quickfix_move_up" => Action::QuickfixMoveUp,
        "quickfix_move_down" => Action::QuickfixMoveDown,
        "location_list_close" => Action::LocationListClose,
        "location_list_confirm" => Action::LocationListConfirm,
        "location_list_move_up" => Action::LocationListMoveUp,
        "location_list_move_down" => Action::LocationListMoveDown,
        "substitute_confirm_apply" => Action::SubstituteConfirmApply,
        "substitute_confirm_skip" => Action::SubstituteConfirmSkip,
        "substitute_confirm_apply_all" => Action::SubstituteConfirmApplyAll,
        "substitute_confirm_cancel" => Action::SubstituteConfirmCancel,
        "command_palette" => Action::CommandPalette,
        "agent_history_previous" => Action::AgentHistoryPrevious,
        "agent_history_next" => Action::AgentHistoryNext,
        "agent_history_search_reverse" => Action::AgentHistorySearchReverse,
        "agent_draft_stash" => Action::AgentDraftStash,
        "agent_draft_restore" => Action::AgentDraftRestore,
        "agent_draft_external_edit" => Action::AgentDraftExternalEdit,
        "agent_toggle_transcript_details" => Action::AgentToggleTranscriptDetails,
        "agent_toggle_transcript_raw" => Action::AgentToggleTranscriptRaw,
        "goto_next_function" => Action::Edit("goto_next_function"),
        "goto_prev_function" => Action::Edit("goto_prev_function"),
        "goto_next_class" => Action::Edit("goto_next_class"),
        "goto_prev_class" => Action::Edit("goto_prev_class"),
        "goto_next_parameter" => Action::Edit("goto_next_parameter"),
        "goto_prev_parameter" => Action::Edit("goto_prev_parameter"),
        "goto_next_comment" => Action::Edit("goto_next_comment"),
        "goto_prev_comment" => Action::Edit("goto_prev_comment"),
        "goto_next_test" => Action::Edit("goto_next_test"),
        "goto_prev_test" => Action::Edit("goto_prev_test"),
        "goto_next_paragraph" => Action::Edit("goto_next_paragraph"),
        "goto_prev_paragraph" => Action::Edit("goto_prev_paragraph"),
        "goto_next_change" => Action::GitNextHunk,
        "goto_prev_change" => Action::GitPrevHunk,
        "goto_first_change" => Action::GitFirstHunk,
        "goto_last_change" => Action::GitLastHunk,
        "move_line_up" => Action::Edit("move_up"),
        "move_line_down" => Action::Edit("move_down"),
        "goto_line_start" => Action::Edit("move_to_left_end_of_line"),
        "goto_line_end" => Action::Edit("move_to_right_end_of_line"),
        "page_up" => Action::Edit("scroll_page_up"),
        "page_down" => Action::Edit("scroll_page_down"),
        "page_cursor_half_up" => Action::PageCursorHalfUp,
        "page_cursor_half_down" => Action::PageCursorHalfDown,
        "jump_forward" => Action::JumpListNewer,
        "jump_backward" => Action::JumpListOlder,
        "save_selection" => Action::SaveSelection,
        "repeat_last_motion" => Action::RepeatLastMotion,
        "replace" => Action::Replace,
        "replace_with_yanked" => Action::ReplaceWithYanked,
        "switch_case" => Action::SwitchCase,
        "switch_to_lowercase" => Action::SwitchToLowercase,
        "switch_to_uppercase" => Action::SwitchToUppercase,
        "yank" => Action::YankSelection,
        "yank_to_clipboard" => Action::YankToClipboard,
        "yank_to_primary_clipboard" => Action::YankToPrimaryClipboard,
        "yank_main_selection_to_clipboard" => Action::YankMainSelectionToClipboard,
        "yank_main_selection_to_primary_clipboard" => Action::YankMainSelectionToPrimaryClipboard,
        "indent" => Action::IndentSelection,
        "unindent" => Action::UnindentSelection,
        "format_selections" => Action::FormatSelections,
        "extend_line_below" => Action::ExtendLineBelow,
        "extend_to_line_bounds" => Action::ExtendToLineBounds,
        "shrink_to_line_bounds" => Action::ShrinkToLineBounds,
        "join_selections" => Action::JoinSelections,
        "join_selections_space" => Action::JoinSelectionsSpace,
        "keep_selections" => Action::KeepSelections,
        "remove_selections" => Action::RemoveSelections,
        "expand_selection" => Action::ExpandSelection,
        "shrink_selection" => Action::ShrinkSelection,
        "select_prev_sibling" => Action::SelectPrevSibling,
        "select_next_sibling" => Action::SelectNextSibling,
        "select_all_siblings" => Action::SelectAllSiblings,
        "select_all_children" => Action::SelectAllChildren,
        "move_parent_node_start" => Action::MoveParentNodeStart,
        "move_parent_node_end" => Action::MoveParentNodeEnd,
        "delete_selection" => Action::DeleteSelection { yank: true, enter_insert: false },
        "delete_selection_noyank" => Action::DeleteSelection { yank: false, enter_insert: false },
        "change_selection" => Action::DeleteSelection { yank: true, enter_insert: true },
        "change_selection_noyank" => Action::DeleteSelection { yank: false, enter_insert: true },
        "insert_mode" => Action::EnterMode(Mode::Insert),
        "append_mode" => Action::AppendAfterCursor,
        "visual_mode" | "select_mode" => Action::EnterMode(Mode::Visual),
        "command_mode" => Action::EnterCommandMode,
        "move_next_word_start" | "goto_word" => {
            Action::MoveWordStart { forward: true, long_word: false }
        }
        "move_prev_word_start" => Action::MoveWordStart { forward: false, long_word: false },
        "move_next_word_end" => Action::MoveWordEnd { long_word: false },
        "move_next_long_word_start" => Action::MoveWordStart { forward: true, long_word: true },
        "move_prev_long_word_start" => Action::MoveWordStart { forward: false, long_word: true },
        "move_next_long_word_end" => Action::MoveWordEnd { long_word: true },
        "find_next_char" => Action::PendingCharFind { forward: true, inclusive: true },
        "find_till_char" => Action::PendingCharFind { forward: true, inclusive: false },
        "find_prev_char" => Action::PendingCharFind { forward: false, inclusive: true },
        "till_prev_char" => Action::PendingCharFind { forward: false, inclusive: false },
        "completion" => Action::RequestCompletion,
        "request_hover" | "hover" => Action::RequestHover,
        "request_document_symbols" => Action::RequestDocumentSymbols,
        "request_workspace_symbols" => Action::RequestWorkspaceSymbols,
        "code_action" => Action::RequestCodeActions,
        "rename_symbol" => Action::PrefillCommandLine("rename "),
        "register_prefix" => Action::RegisterPrefix,
        "insert_register" => Action::InsertRegister,
        "mark_set_prefix" => Action::MarkSetPrefix,
        "macro_record_toggle" => Action::MacroRecordToggle,
        "macro_replay_prefix" => Action::MacroReplayPrefix,
        "window_command_prefix" => Action::WindowCommandPrefix,
        "matching_pair" => Action::MatchingPair,
        "match_brackets" => Action::MatchingPair,
        "append_after_cursor" => Action::AppendAfterCursor,
        "append_at_end_of_line" => Action::AppendAtEndOfLine,
        "insert_at_line_start" => Action::InsertAtLineStart,
        "insert_at_line_end" => Action::AppendAtEndOfLine,
        "open_line_below" => Action::OpenLineBelow,
        "open_line_above" => Action::OpenLineAbove,
        "open_below" => Action::OpenLineBelow,
        "open_above" => Action::OpenLineAbove,
        "substitute_char" => Action::SubstituteChar,
        "substitute_line" => Action::SubstituteLine,
        "delete_char_backward" => Action::DeleteBackward,
        "delete_char_forward" => Action::Edit("delete_forward"),
        "delete_word_backward" => Action::DeleteWordBackward,
        "delete_word_forward" => Action::Edit("delete_word_forward"),
        "delete_to_line_start" => Action::DeleteToLineStart,
        "kill_to_line_start" => Action::DeleteToLineStart,
        "kill_to_line_end" => Action::Edit("delete_to_end_of_paragraph"),
        "kill_line" => Action::DeleteCurrentLine,
        "insert_newline" => Action::Edit("insert_newline"),
        "add_newline_below" => Action::AddNewlineBelow,
        "add_newline_above" => Action::AddNewlineAbove,
        "indent_line" => Action::IndentLine,
        "outdent_line" => Action::OutdentLine,
        "undo" => Action::Undo,
        "redo" => Action::Redo,
        "earlier" => Action::Undo,
        "later" => Action::Redo,
        "repeat_last_change" => Action::RepeatLastChange,
        "paste_after" => Action::PasteAfter,
        "paste_before" => Action::PasteBefore,
        "paste_clipboard_after" => Action::PasteClipboardAfter,
        "paste_clipboard_before" => Action::PasteClipboardBefore,
        "paste_primary_clipboard_after" => Action::PastePrimaryClipboardAfter,
        "paste_primary_clipboard_before" => Action::PastePrimaryClipboardBefore,
        "replace_selections_with_clipboard" => Action::ReplaceSelectionsWithClipboard,
        "replace_selections_with_primary_clipboard" => {
            Action::ReplaceSelectionsWithPrimaryClipboard
        }
        "select_register" => Action::RegisterPrefix,
        "enter_visual_line" => Action::EnterVisualLine,
        "enter_visual_block" => Action::EnterVisualBlock,
        "swap_visual_anchor" => Action::SwapVisualAnchor,
        "restore_last_visual" => Action::RestoreLastVisual,
        "visual_block_insert" => Action::VisualBlockInsert,
        "visual_block_append" => Action::VisualBlockAppend,
        "jump_list_older" => Action::JumpListOlder,
        "jump_list_newer" => Action::JumpListNewer,
        "change_list_older" => Action::ChangeListOlder,
        "change_list_newer" => Action::ChangeListNewer,
        "tab_next" => Action::TabNext,
        "tab_prev" => Action::TabPrev,
        "rotate_view" | "cycle_view" => Action::RotateView,
        "rotate_view_reverse" => Action::RotateViewReverse,
        "transpose_view" => Action::TransposeView,
        "wclose" => Action::WindowClose,
        "wonly" => Action::WindowOnly,
        "jump_view_left" => Action::JumpViewLeft,
        "jump_view_down" => Action::JumpViewDown,
        "jump_view_up" => Action::JumpViewUp,
        "jump_view_right" => Action::JumpViewRight,
        "swap_view_left" => Action::SwapViewLeft,
        "swap_view_down" => Action::SwapViewDown,
        "swap_view_up" => Action::SwapViewUp,
        "swap_view_right" => Action::SwapViewRight,
        "command_history_older" => Action::CommandHistoryOlder,
        "command_history_newer" => Action::CommandHistoryNewer,
        "qf_next" => Action::QfNext,
        "qf_prev" => Action::QfPrev,
        "loc_next" => Action::LocNext,
        "loc_prev" => Action::LocPrev,
        "git_next_hunk" => Action::GitNextHunk,
        "git_prev_hunk" => Action::GitPrevHunk,
        "git_first_hunk" => Action::GitFirstHunk,
        "git_last_hunk" => Action::GitLastHunk,
        "git_blame" => Action::GitBlame,
        "git_diff" => Action::GitDiff,
        "fold_toggle" => Action::FoldToggle,
        "fold_open" => Action::FoldOpen,
        "fold_close" => Action::FoldClose,
        "fold_open_all" => Action::FoldOpenAll,
        "fold_close_all" => Action::FoldCloseAll,
        "commit_undo_checkpoint" => Action::CommitUndoCheckpoint,
        "find_all" => Action::FindAll,
        _ => return Err(format!("unknown action `{spec}`")),
    };

    Ok(action)
}

pub(crate) fn format_binding_mode(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => "normal",
        Mode::Insert => "insert",
        Mode::Visual => "visual",
        Mode::VisualLine => "visual_line",
        Mode::VisualBlock => "visual_block",
        Mode::OperatorPending => "operator_pending",
        Mode::CommandLine => "command_line",
        Mode::Search => "search",
        Mode::Picker => "picker",
        Mode::Quickfix => "quickfix",
        Mode::LocationList => "location_list",
        Mode::SubstituteConfirm => "substitute_confirm",
        Mode::PrivilegeConfirm => "privilege_confirm",
        Mode::Agent => "agent",
    }
}

pub(crate) fn format_action_spec(action: &Action) -> String {
    match action {
        Action::NoOp => String::from("no_op"),
        Action::AgentHistoryPrevious => String::from("agent_history_previous"),
        Action::AgentHistoryNext => String::from("agent_history_next"),
        Action::AgentHistorySearchReverse => String::from("agent_history_search_reverse"),
        Action::AgentDraftStash => String::from("agent_draft_stash"),
        Action::AgentDraftRestore => String::from("agent_draft_restore"),
        Action::AgentDraftExternalEdit => String::from("agent_draft_external_edit"),
        Action::AgentToggleTranscriptDetails => String::from("agent_toggle_transcript_details"),
        Action::AgentToggleTranscriptRaw => String::from("agent_toggle_transcript_raw"),
        Action::Quit => String::from("quit"),
        Action::EnterMode(mode) => format!("enter_mode:{}", format_mode_spec(*mode)),
        Action::EnterCommandMode => String::from("enter_command_mode"),
        Action::Edit(method) => format!("edit:{method}"),
        Action::CollapseAndEnterNormal => String::from("collapse_and_enter_normal"),
        Action::ExecuteCommand => String::from("execute_command"),
        Action::PrefillCommandLine(prefix) => match *prefix {
            "pipe " => String::from("shell_pipe"),
            "pipe_to " => String::from("shell_pipe_to"),
            "shell_insert_output " => String::from("shell_insert_output"),
            "shell_append_output " => String::from("shell_append_output"),
            "shell_keep_pipe " => String::from("shell_keep_pipe"),
            "rename " => String::from("rename_symbol"),
            other => format!("edit:{other}"),
        },
        Action::DeleteBackward => String::from("delete_backward"),
        Action::CommandBackspace => String::from("command_backspace"),
        Action::SearchBackspace => String::from("search_backspace"),
        Action::EnterSearch => String::from("enter_search"),
        Action::EnterSearchBackward => String::from("enter_search_backward"),
        Action::ExecuteSearch => String::from("execute_search"),
        Action::CompleteCommandLine => String::from("complete_command_line"),
        Action::FindNext => String::from("find_next"),
        Action::FindPrevious => String::from("find_previous"),
        Action::RequestCompletion => String::from("completion"),
        Action::RequestHover => String::from("request_hover"),
        Action::RequestDeclaration => String::from("goto_declaration"),
        Action::RequestDefinition => String::from("goto_definition"),
        Action::RequestTypeDefinition => String::from("goto_type_definition"),
        Action::RequestReferences => String::from("goto_reference"),
        Action::RequestImplementation => String::from("goto_implementation"),
        Action::RequestDocumentSymbols => String::from("request_document_symbols"),
        Action::RequestWorkspaceSymbols => String::from("request_workspace_symbols"),
        Action::RequestCodeActions => String::from("code_action"),
        Action::SwiftMotion => String::from("swift_motion"),
        Action::GlobalSearch => String::from("global_search"),
        Action::CommandPalette => String::from("command_palette"),
        Action::FilePicker => String::from("file_picker"),
        Action::FilePickerInCurrentDirectory => String::from("file_picker_in_current_directory"),
        Action::FileExplorer => String::from("file_explorer"),
        Action::FileExplorerInCurrentBufferDirectory => {
            String::from("file_explorer_in_current_buffer_directory")
        }
        Action::FileExplorerInCurrentDirectory => {
            String::from("file_explorer_in_current_directory")
        }
        Action::BufferPicker => String::from("buffer_picker"),
        Action::JumpListPicker => String::from("jumplist_picker"),
        Action::ChangedFilePicker => String::from("changed_file_picker"),
        Action::DiagnosticsPicker => String::from("diagnostics_picker"),
        Action::WorkspaceDiagnosticsPicker => String::from("workspace_diagnostics_picker"),
        Action::LastPicker => String::from("last_picker"),
        Action::PickerClose => String::from("picker_close"),
        Action::PickerConfirm => String::from("picker_confirm"),
        Action::PickerMoveUp => String::from("picker_move_up"),
        Action::PickerMoveDown => String::from("picker_move_down"),
        Action::PickerBackspace => String::from("picker_backspace"),
        Action::QuickfixClose => String::from("quickfix_close"),
        Action::QuickfixConfirm => String::from("quickfix_confirm"),
        Action::QuickfixMoveUp => String::from("quickfix_move_up"),
        Action::QuickfixMoveDown => String::from("quickfix_move_down"),
        Action::LocationListClose => String::from("location_list_close"),
        Action::LocationListConfirm => String::from("location_list_confirm"),
        Action::LocationListMoveUp => String::from("location_list_move_up"),
        Action::LocationListMoveDown => String::from("location_list_move_down"),
        Action::SubstituteConfirmApply => String::from("substitute_confirm_apply"),
        Action::SubstituteConfirmSkip => String::from("substitute_confirm_skip"),
        Action::SubstituteConfirmApplyAll => String::from("substitute_confirm_apply_all"),
        Action::SubstituteConfirmCancel => String::from("substitute_confirm_cancel"),
        Action::RegisterPrefix => String::from("register_prefix"),
        Action::InsertRegister => String::from("insert_register"),
        Action::MarkSetPrefix => String::from("mark_set_prefix"),
        Action::MarkJumpPrefix { line_start } => {
            format!("mark_jump_prefix:{}", if *line_start { "line" } else { "exact" })
        }
        Action::MacroRecordToggle => String::from("macro_record_toggle"),
        Action::MacroReplayPrefix => String::from("macro_replay_prefix"),
        Action::WindowCommandPrefix => String::from("window_command_prefix"),
        Action::SetPrefix(prefix) => format!("set_prefix:{prefix}"),
        Action::PendingCharFind { forward, inclusive } => format!(
            "pending_char_find:{}:{}",
            if *forward { "forward" } else { "backward" },
            if *inclusive { "inclusive" } else { "exclusive" }
        ),
        Action::MoveWordStart { forward, long_word } => format!(
            "move_word_start:{}:{}",
            if *forward { "forward" } else { "backward" },
            if *long_word { "long_word" } else { "word" }
        ),
        Action::MoveWordEnd { long_word } => {
            format!("move_word_end:{}", if *long_word { "long_word" } else { "word" })
        }
        Action::GotoFirstNonWhitespace => String::from("goto_first_nonwhitespace"),
        Action::GotoLine => String::from("goto_line"),
        Action::GotoColumn => String::from("goto_column"),
        Action::GotoFileStart => String::from("goto_file_start"),
        Action::GotoLastLine => String::from("goto_last_line"),
        Action::GotoFile => String::from("goto_file"),
        Action::GotoWindowTop => String::from("goto_window_top"),
        Action::GotoWindowCenter => String::from("goto_window_center"),
        Action::GotoWindowBottom => String::from("goto_window_bottom"),
        Action::GotoLastAccessedFile => String::from("goto_last_accessed_file"),
        Action::GotoLastModifiedFile => String::from("goto_last_modified_file"),
        Action::SaveSelection => String::from("save_selection"),
        Action::RepeatLastMotion => String::from("repeat_last_motion"),
        Action::PageCursorHalfUp => String::from("page_cursor_half_up"),
        Action::PageCursorHalfDown => String::from("page_cursor_half_down"),
        Action::Replace => String::from("replace"),
        Action::ReplaceWithYanked => String::from("replace_with_yanked"),
        Action::SwitchCase => String::from("switch_case"),
        Action::SwitchToLowercase => String::from("switch_to_lowercase"),
        Action::SwitchToUppercase => String::from("switch_to_uppercase"),
        Action::YankSelection => String::from("yank"),
        Action::YankToClipboard => String::from("yank_to_clipboard"),
        Action::YankToPrimaryClipboard => String::from("yank_to_primary_clipboard"),
        Action::YankMainSelectionToClipboard => String::from("yank_main_selection_to_clipboard"),
        Action::YankMainSelectionToPrimaryClipboard => {
            String::from("yank_main_selection_to_primary_clipboard")
        }
        Action::IndentSelection => String::from("indent"),
        Action::UnindentSelection => String::from("unindent"),
        Action::FormatSelections => String::from("format_selections"),
        Action::ExtendLineBelow => String::from("extend_line_below"),
        Action::ExtendToLineBounds => String::from("extend_to_line_bounds"),
        Action::ShrinkToLineBounds => String::from("shrink_to_line_bounds"),
        Action::JoinSelections => String::from("join_selections"),
        Action::JoinSelectionsSpace => String::from("join_selections_space"),
        Action::KeepSelections => String::from("keep_selections"),
        Action::RemoveSelections => String::from("remove_selections"),
        Action::ExpandSelection => String::from("expand_selection"),
        Action::ShrinkSelection => String::from("shrink_selection"),
        Action::SelectPrevSibling => String::from("select_prev_sibling"),
        Action::SelectNextSibling => String::from("select_next_sibling"),
        Action::SelectAllSiblings => String::from("select_all_siblings"),
        Action::SelectAllChildren => String::from("select_all_children"),
        Action::MoveParentNodeStart => String::from("move_parent_node_start"),
        Action::MoveParentNodeEnd => String::from("move_parent_node_end"),
        Action::DeleteSelection { yank, enter_insert } => match (*yank, *enter_insert) {
            (true, false) => String::from("delete_selection"),
            (false, false) => String::from("delete_selection_noyank"),
            (true, true) => String::from("change_selection"),
            (false, true) => String::from("change_selection_noyank"),
        },
        Action::MatchingPair => String::from("matching_pair"),
        Action::SetOperator(operator) => {
            format!("set_operator:{}", format_operator_spec(*operator))
        }
        Action::AppendAfterCursor => String::from("append_after_cursor"),
        Action::AppendAtEndOfLine => String::from("append_at_end_of_line"),
        Action::InsertAtLineStart => String::from("insert_at_line_start"),
        Action::OpenLineBelow => String::from("open_line_below"),
        Action::OpenLineAbove => String::from("open_line_above"),
        Action::SubstituteChar => String::from("substitute_char"),
        Action::SubstituteLine => String::from("substitute_line"),
        Action::DeleteWordBackward => String::from("delete_word_backward"),
        Action::DeleteToLineStart => String::from("delete_to_line_start"),
        Action::AddNewlineBelow => String::from("add_newline_below"),
        Action::AddNewlineAbove => String::from("add_newline_above"),
        Action::DeleteCurrentLine => String::from("kill_line"),
        Action::IndentLine => String::from("indent_line"),
        Action::OutdentLine => String::from("outdent_line"),
        Action::Undo => String::from("undo"),
        Action::Redo => String::from("redo"),
        Action::RepeatLastChange => String::from("repeat_last_change"),
        Action::PasteAfter => String::from("paste_after"),
        Action::PasteBefore => String::from("paste_before"),
        Action::PasteClipboardAfter => String::from("paste_clipboard_after"),
        Action::PasteClipboardBefore => String::from("paste_clipboard_before"),
        Action::PastePrimaryClipboardAfter => String::from("paste_primary_clipboard_after"),
        Action::PastePrimaryClipboardBefore => String::from("paste_primary_clipboard_before"),
        Action::ReplaceSelectionsWithClipboard => String::from("replace_selections_with_clipboard"),
        Action::ReplaceSelectionsWithPrimaryClipboard => {
            String::from("replace_selections_with_primary_clipboard")
        }
        Action::EnterVisualLine => String::from("enter_visual_line"),
        Action::EnterVisualBlock => String::from("enter_visual_block"),
        Action::SwapVisualAnchor => String::from("swap_visual_anchor"),
        Action::RestoreLastVisual => String::from("restore_last_visual"),
        Action::VisualBlockInsert => String::from("visual_block_insert"),
        Action::VisualBlockAppend => String::from("visual_block_append"),
        Action::JumpListOlder => String::from("jump_list_older"),
        Action::JumpListNewer => String::from("jump_list_newer"),
        Action::ChangeListOlder => String::from("change_list_older"),
        Action::ChangeListNewer => String::from("change_list_newer"),
        Action::TabNext => String::from("tab_next"),
        Action::TabPrev => String::from("tab_prev"),
        Action::RotateView => String::from("rotate_view"),
        Action::RotateViewReverse => String::from("rotate_view_reverse"),
        Action::TransposeView => String::from("transpose_view"),
        Action::WindowClose => String::from("wclose"),
        Action::WindowOnly => String::from("wonly"),
        Action::JumpViewLeft => String::from("jump_view_left"),
        Action::JumpViewDown => String::from("jump_view_down"),
        Action::JumpViewUp => String::from("jump_view_up"),
        Action::JumpViewRight => String::from("jump_view_right"),
        Action::SwapViewLeft => String::from("swap_view_left"),
        Action::SwapViewDown => String::from("swap_view_down"),
        Action::SwapViewUp => String::from("swap_view_up"),
        Action::SwapViewRight => String::from("swap_view_right"),
        Action::CommandHistoryOlder => String::from("command_history_older"),
        Action::CommandHistoryNewer => String::from("command_history_newer"),
        Action::QfNext => String::from("qf_next"),
        Action::QfPrev => String::from("qf_prev"),
        Action::LocNext => String::from("loc_next"),
        Action::LocPrev => String::from("loc_prev"),
        Action::GitNextHunk => String::from("git_next_hunk"),
        Action::GitPrevHunk => String::from("git_prev_hunk"),
        Action::GitFirstHunk => String::from("git_first_hunk"),
        Action::GitLastHunk => String::from("git_last_hunk"),
        Action::GitBlame => String::from("git_blame"),
        Action::GitDiff => String::from("git_diff"),
        Action::FoldToggle => String::from("fold_toggle"),
        Action::FoldOpen => String::from("fold_open"),
        Action::FoldClose => String::from("fold_close"),
        Action::FoldOpenAll => String::from("fold_open_all"),
        Action::FoldCloseAll => String::from("fold_close_all"),
        Action::CommitUndoCheckpoint => String::from("commit_undo_checkpoint"),
        Action::SearchWordUnderCursor { forward } => {
            format!("search_word_under_cursor:{}", if *forward { "forward" } else { "backward" })
        }
        Action::SearchSelection { detect_word_boundaries } => {
            String::from(if *detect_word_boundaries {
                "search_selection_detect_word_boundaries"
            } else {
                "search_selection"
            })
        }
        Action::FindAll => String::from("find_all"),
    }
}

pub(crate) fn format_mode_spec(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => "normal",
        Mode::Insert => "insert",
        Mode::Visual => "visual",
        Mode::VisualLine => "visual_line",
        Mode::VisualBlock => "visual_block",
        Mode::OperatorPending => "operator_pending",
        Mode::CommandLine => "command_line",
        Mode::Search => "search",
        Mode::Picker
        | Mode::Quickfix
        | Mode::LocationList
        | Mode::SubstituteConfirm
        | Mode::PrivilegeConfirm
        | Mode::Agent => "normal",
    }
}

pub(crate) fn format_operator_spec(operator: Operator) -> &'static str {
    match operator {
        Operator::Delete => "delete",
        Operator::Change => "change",
        Operator::Yank => "yank",
        Operator::Indent => "indent",
        Operator::Outdent => "outdent",
        Operator::Uppercase => "uppercase",
        Operator::Lowercase => "lowercase",
        Operator::CaseToggle => "case_toggle",
    }
}

pub(crate) fn parse_mode_spec(spec: &str) -> Option<Mode> {
    match spec.trim().to_ascii_lowercase().as_str() {
        "normal" => Some(Mode::Normal),
        "insert" => Some(Mode::Insert),
        "visual" => Some(Mode::Visual),
        "visual_line" | "visualline" | "line_visual" => Some(Mode::VisualLine),
        "visual_block" | "visualblock" | "block_visual" => Some(Mode::VisualBlock),
        "operator_pending" | "operator" => Some(Mode::OperatorPending),
        "command_line" | "command" => Some(Mode::CommandLine),
        "search" => Some(Mode::Search),
        "substitute_confirm" | "substitute" => Some(Mode::SubstituteConfirm),
        _ => None,
    }
}

pub(crate) fn parse_binding_mode_spec(spec: &str) -> Option<Mode> {
    parse_mode_spec(spec).or_else(|| match spec.trim().to_ascii_lowercase().as_str() {
        "picker" => Some(Mode::Picker),
        "quickfix" => Some(Mode::Quickfix),
        "location_list" | "locationlist" | "location" => Some(Mode::LocationList),
        "agent" | "agents" => Some(Mode::Agent),
        _ => None,
    })
}

pub(crate) fn parse_operator_spec(spec: &str) -> Option<Operator> {
    match spec.trim().to_ascii_lowercase().as_str() {
        "delete" => Some(Operator::Delete),
        "change" => Some(Operator::Change),
        "yank" => Some(Operator::Yank),
        "indent" => Some(Operator::Indent),
        "outdent" => Some(Operator::Outdent),
        "uppercase" => Some(Operator::Uppercase),
        "lowercase" => Some(Operator::Lowercase),
        "case_toggle" | "casetoggle" => Some(Operator::CaseToggle),
        _ => None,
    }
}

pub(crate) fn parse_edit_method(spec: &str) -> Option<&'static str> {
    match spec.trim() {
        "move_left" => Some("move_left"),
        "move_char_left" => Some("move_left"),
        "move_right" => Some("move_right"),
        "move_char_right" => Some("move_right"),
        "move_up" => Some("move_up"),
        "move_visual_line_up" => Some("move_up"),
        "move_down" => Some("move_down"),
        "move_visual_line_down" => Some("move_down"),
        "move_word_right" => Some("move_word_right"),
        "move_word_left" => Some("move_word_left"),
        "move_to_beginning_of_paragraph" => Some("move_to_beginning_of_paragraph"),
        "move_to_right_end_of_line" => Some("move_to_right_end_of_line"),
        "move_to_end_of_document" => Some("move_to_end_of_document"),
        "duplicate_line" => Some("duplicate_line"),
        "scroll_page_down" => Some("scroll_page_down"),
        "scroll_page_up" => Some("scroll_page_up"),
        "add_selection_above" => Some("add_selection_above"),
        "add_selection_below" => Some("add_selection_below"),
        "increase_number" => Some("increase_number"),
        "decrease_number" => Some("decrease_number"),
        "move_left_and_modify_selection" => Some("move_left_and_modify_selection"),
        "extend_char_left" => Some("move_left_and_modify_selection"),
        "move_right_and_modify_selection" => Some("move_right_and_modify_selection"),
        "extend_char_right" => Some("move_right_and_modify_selection"),
        "move_up_and_modify_selection" => Some("move_up_and_modify_selection"),
        "extend_line_up" => Some("move_up_and_modify_selection"),
        "move_visual_line_up_and_modify_selection" => Some("move_up_and_modify_selection"),
        "extend_visual_line_up" => Some("move_up_and_modify_selection"),
        "move_down_and_modify_selection" => Some("move_down_and_modify_selection"),
        "extend_line_down" => Some("move_down_and_modify_selection"),
        "move_visual_line_down_and_modify_selection" => Some("move_down_and_modify_selection"),
        "extend_visual_line_down" => Some("move_down_and_modify_selection"),
        "move_word_right_and_modify_selection" => Some("move_word_right_and_modify_selection"),
        "move_word_left_and_modify_selection" => Some("move_word_left_and_modify_selection"),
        "extend_line_above" => Some("extend_line_above"),
        "select_line_above" => Some("select_line_above"),
        "select_line_below" => Some("select_line_below"),
        "goto_file_end" => Some("move_to_end_of_document"),
        "extend_to_file_start" => Some("move_to_beginning_of_document_and_modify_selection"),
        "move_to_right_end_of_line_and_modify_selection" => {
            Some("move_to_right_end_of_line_and_modify_selection")
        }
        "move_to_beginning_of_paragraph_and_modify_selection" => {
            Some("move_to_beginning_of_paragraph_and_modify_selection")
        }
        "move_to_beginning_of_document_and_modify_selection" => {
            Some("move_to_beginning_of_document_and_modify_selection")
        }
        "move_to_end_of_document_and_modify_selection" => {
            Some("move_to_end_of_document_and_modify_selection")
        }
        "extend_to_file_end" => Some("move_to_end_of_document_and_modify_selection"),
        "insert_newline" => Some("insert_newline"),
        _ => None,
    }
}

pub(crate) fn parse_prefix_spec(prefix: Option<&str>) -> Result<Option<char>, String> {
    prefix.map(parse_prefix_char).transpose()
}

pub(crate) fn parse_prefix_char(spec: &str) -> Result<char, String> {
    let mut chars = spec.chars();
    let Some(ch) = chars.next() else {
        return Err(String::from("prefix must contain exactly one character"));
    };
    if chars.next().is_some() {
        return Err(String::from("prefix must contain exactly one character"));
    }
    Ok(ch)
}

pub(crate) fn parse_key_press_spec(spec: &str) -> Result<KeyPress, String> {
    let (key, modifiers) = parse_key_spec(spec)?;
    Ok(KeyPress { key, modifiers })
}

pub(crate) fn parse_key_spec(spec: &str) -> Result<(KeyCode, KeyModifiers), String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(String::from("key cannot be empty"));
    }

    let parts: Vec<_> = spec.split('+').collect();
    let mut modifiers = KeyModifiers::NONE;
    let key_token = if parts.len() == 1 {
        parts[0]
    } else {
        for modifier in &parts[..parts.len() - 1] {
            match modifier.trim().to_ascii_lowercase().as_str() {
                "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
                "alt" => modifiers |= KeyModifiers::ALT,
                "shift" => modifiers |= KeyModifiers::SHIFT,
                other => return Err(format!("unknown modifier `{other}`")),
            }
        }
        parts[parts.len() - 1]
    };

    let key = match key_token.trim().to_ascii_lowercase().as_str() {
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "enter" | "return" => KeyCode::Enter,
        "backspace" => KeyCode::Backspace,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "esc" | "escape" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        "plus" => KeyCode::Char('+'),
        _ => {
            let mut chars = key_token.chars();
            let Some(ch) = chars.next() else {
                return Err(String::from("key cannot be empty"));
            };
            if chars.next().is_some() {
                return Err(format!("unknown key `{key_token}`"));
            }
            KeyCode::Char(ch)
        }
    };

    Ok((key, modifiers))
}
