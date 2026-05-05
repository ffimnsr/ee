use std::collections::HashMap;
use std::sync::OnceLock;

use crossterm::event::{KeyCode, KeyModifiers};

use crate::app::{Mode, Operator};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    NoOp,
    Quit,
    EnterMode(Mode),
    EnterCommandMode,
    Edit(&'static str),
    CollapseAndEnterNormal,
    ExecuteCommand,
    DeleteBackward,
    CommandBackspace,
    SearchBackspace,
    EnterSearch,
    EnterSearchBackward,
    ExecuteSearch,
    CompleteCommandLine,
    FindNext,
    FindPrevious,
    RequestHover,
    RequestDocumentSymbols,
    RequestWorkspaceSymbols,
    RegisterPrefix,
    MarkSetPrefix,
    MarkJumpPrefix {
        line_start: bool,
    },
    MacroRecordToggle,
    MacroReplayPrefix,
    WindowCommandPrefix,
    SetPrefix(char),
    PendingCharFind {
        forward: bool,
        inclusive: bool,
    },
    MoveWordStart {
        forward: bool,
        long_word: bool,
    },
    MoveWordEnd {
        long_word: bool,
    },
    GotoLine,
    SaveSelection,
    RepeatLastMotion,
    PageCursorHalfUp,
    PageCursorHalfDown,
    Replace,
    ReplaceWithYanked,
    SwitchCase,
    SwitchToLowercase,
    SwitchToUppercase,
    YankSelection,
    IndentSelection,
    UnindentSelection,
    FormatSelections,
    DeleteSelection {
        yank: bool,
        enter_insert: bool,
    },
    MatchingPair,
    // Operator-pending mode
    SetOperator(Operator),
    // Insert-entry variants
    AppendAfterCursor,
    AppendAtEndOfLine,
    InsertAtLineStart,
    OpenLineBelow,
    OpenLineAbove,
    SubstituteChar,
    SubstituteLine,
    // Insert mode editing controls
    DeleteWordBackward,
    DeleteToLineStart,
    IndentLine,
    OutdentLine,
    // Undo / Redo
    Undo,
    Redo,
    // Repeat last change
    RepeatLastChange,
    // Paste
    PasteAfter,
    PasteBefore,
    // Visual modes
    EnterVisualLine,
    EnterVisualBlock,
    SwapVisualAnchor,
    RestoreLastVisual,
    // Visual block insert / append
    VisualBlockInsert,
    VisualBlockAppend,
    // Jump list
    JumpListOlder,
    JumpListNewer,
    // Change list
    ChangeListOlder,
    ChangeListNewer,
    // Tab navigation
    TabNext,
    TabPrev,
    // Command-line history
    CommandHistoryOlder,
    CommandHistoryNewer,
    // Quickfix list navigation
    QfNext,
    QfPrev,
    // Location list navigation
    LocNext,
    LocPrev,
    // Git-aware navigation and views
    GitNextHunk,
    GitPrevHunk,
    GitBlame,
    GitDiff,
    // Fold commands (z-prefix)
    FoldToggle,
    FoldOpen,
    FoldClose,
    FoldOpenAll,
    FoldCloseAll,
    // Find-related
    /// Use word under cursor (or selection) as search pattern and jump forward.
    SearchWordUnderCursor {
        forward: bool,
    },
    /// Use current selection or word under cursor as search pattern.
    SearchSelection {
        detect_word_boundaries: bool,
    },
    /// Select all occurrences of current search pattern.
    FindAll,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct BindingKey {
    pub(crate) mode: Mode,
    pub(crate) key: KeyCode,
    pub(crate) modifiers: KeyModifiers,
    pub(crate) prefix: Option<char>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeymapSettings {
    pub(crate) inherit_defaults: bool,
    pub(crate) operations: Vec<KeymapOperation>,
}

impl Default for KeymapSettings {
    fn default() -> Self {
        Self { inherit_defaults: true, operations: Vec::new() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeymapOperation {
    Unbind(BindingKey),
    Bind { binding: BindingKey, action: Action },
}

pub(crate) fn bindings() -> &'static HashMap<BindingKey, Action> {
    static BINDINGS: OnceLock<HashMap<BindingKey, Action>> = OnceLock::new();
    BINDINGS.get_or_init(build_vim_bindings)
}

pub(crate) fn bindings_for(settings: &KeymapSettings) -> HashMap<BindingKey, Action> {
    let mut map = if settings.inherit_defaults { bindings().clone() } else { HashMap::new() };

    for operation in &settings.operations {
        match operation {
            KeymapOperation::Unbind(binding) => {
                map.remove(binding);
            }
            KeymapOperation::Bind { binding, action } => {
                map.insert(*binding, action.clone());
            }
        }
    }

    map
}

pub(crate) fn parse_binding_spec(
    mode: &str,
    key: &str,
    prefix: Option<&str>,
) -> Result<BindingKey, String> {
    let mode = parse_mode_spec(mode).ok_or_else(|| format!("unknown mode `{mode}`"))?;
    let (key, modifiers) = parse_key_spec(key)?;
    let prefix = parse_prefix_spec(prefix)?;
    Ok(BindingKey { mode, key, modifiers, prefix })
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
        "search_selection_detect_word_boundaries" => {
            Action::SearchSelection { detect_word_boundaries: true }
        }
        "search_selection" => Action::SearchSelection { detect_word_boundaries: false },
        "find_next" => Action::FindNext,
        "find_previous" => Action::FindPrevious,
        "goto_line" => Action::GotoLine,
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
        "indent" => Action::IndentSelection,
        "unindent" => Action::UnindentSelection,
        "format_selections" => Action::FormatSelections,
        "delete_selection" => Action::DeleteSelection { yank: true, enter_insert: false },
        "delete_selection_noyank" => Action::DeleteSelection { yank: false, enter_insert: false },
        "change_selection" => Action::DeleteSelection { yank: true, enter_insert: true },
        "change_selection_noyank" => Action::DeleteSelection { yank: false, enter_insert: true },
        "insert_mode" => Action::EnterMode(Mode::Insert),
        "append_mode" => Action::AppendAfterCursor,
        "visual_mode" | "select_mode" => Action::EnterMode(Mode::Visual),
        "command_mode" => Action::EnterCommandMode,
        "move_next_word_start" => Action::MoveWordStart { forward: true, long_word: false },
        "move_prev_word_start" => Action::MoveWordStart { forward: false, long_word: false },
        "move_next_word_end" => Action::MoveWordEnd { long_word: false },
        "move_next_long_word_start" => Action::MoveWordStart { forward: true, long_word: true },
        "move_prev_long_word_start" => Action::MoveWordStart { forward: false, long_word: true },
        "move_next_long_word_end" => Action::MoveWordEnd { long_word: true },
        "find_next_char" => Action::PendingCharFind { forward: true, inclusive: true },
        "find_till_char" => Action::PendingCharFind { forward: true, inclusive: false },
        "find_prev_char" => Action::PendingCharFind { forward: false, inclusive: true },
        "till_prev_char" => Action::PendingCharFind { forward: false, inclusive: false },
        "request_hover" => Action::RequestHover,
        "request_document_symbols" => Action::RequestDocumentSymbols,
        "request_workspace_symbols" => Action::RequestWorkspaceSymbols,
        "register_prefix" => Action::RegisterPrefix,
        "mark_set_prefix" => Action::MarkSetPrefix,
        "macro_record_toggle" => Action::MacroRecordToggle,
        "macro_replay_prefix" => Action::MacroReplayPrefix,
        "window_command_prefix" => Action::WindowCommandPrefix,
        "matching_pair" => Action::MatchingPair,
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
        "delete_word_backward" => Action::DeleteWordBackward,
        "delete_to_line_start" => Action::DeleteToLineStart,
        "indent_line" => Action::IndentLine,
        "outdent_line" => Action::OutdentLine,
        "undo" => Action::Undo,
        "redo" => Action::Redo,
        "earlier" => Action::Undo,
        "later" => Action::Redo,
        "repeat_last_change" => Action::RepeatLastChange,
        "paste_after" => Action::PasteAfter,
        "paste_before" => Action::PasteBefore,
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
        "command_history_older" => Action::CommandHistoryOlder,
        "command_history_newer" => Action::CommandHistoryNewer,
        "qf_next" => Action::QfNext,
        "qf_prev" => Action::QfPrev,
        "loc_next" => Action::LocNext,
        "loc_prev" => Action::LocPrev,
        "git_next_hunk" => Action::GitNextHunk,
        "git_prev_hunk" => Action::GitPrevHunk,
        "git_blame" => Action::GitBlame,
        "git_diff" => Action::GitDiff,
        "fold_toggle" => Action::FoldToggle,
        "fold_open" => Action::FoldOpen,
        "fold_close" => Action::FoldClose,
        "fold_open_all" => Action::FoldOpenAll,
        "fold_close_all" => Action::FoldCloseAll,
        "find_all" => Action::FindAll,
        _ => return Err(format!("unknown action `{spec}`")),
    };

    Ok(action)
}

fn parse_mode_spec(spec: &str) -> Option<Mode> {
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

fn parse_operator_spec(spec: &str) -> Option<Operator> {
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

fn parse_edit_method(spec: &str) -> Option<&'static str> {
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
        "move_right_and_modify_selection" => Some("move_right_and_modify_selection"),
        "move_up_and_modify_selection" => Some("move_up_and_modify_selection"),
        "move_visual_line_up_and_modify_selection" => Some("move_up_and_modify_selection"),
        "move_down_and_modify_selection" => Some("move_down_and_modify_selection"),
        "move_visual_line_down_and_modify_selection" => Some("move_down_and_modify_selection"),
        "move_word_right_and_modify_selection" => Some("move_word_right_and_modify_selection"),
        "move_word_left_and_modify_selection" => Some("move_word_left_and_modify_selection"),
        "move_to_right_end_of_line_and_modify_selection" => {
            Some("move_to_right_end_of_line_and_modify_selection")
        }
        "move_to_beginning_of_paragraph_and_modify_selection" => {
            Some("move_to_beginning_of_paragraph_and_modify_selection")
        }
        "move_to_end_of_document_and_modify_selection" => {
            Some("move_to_end_of_document_and_modify_selection")
        }
        "insert_newline" => Some("insert_newline"),
        _ => None,
    }
}

fn parse_prefix_spec(prefix: Option<&str>) -> Result<Option<char>, String> {
    prefix.map(parse_prefix_char).transpose()
}

fn parse_prefix_char(spec: &str) -> Result<char, String> {
    let mut chars = spec.chars();
    let Some(ch) = chars.next() else {
        return Err(String::from("prefix must contain exactly one character"));
    };
    if chars.next().is_some() {
        return Err(String::from("prefix must contain exactly one character"));
    }
    Ok(ch)
}

fn parse_key_spec(spec: &str) -> Result<(KeyCode, KeyModifiers), String> {
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

fn build_vim_bindings() -> HashMap<BindingKey, Action> {
    use Action::*;
    use Mode::*;

    let none = KeyModifiers::NONE;
    let ctrl = KeyModifiers::CONTROL;

    let mut map = HashMap::new();

    macro_rules! bind {
        ($mode:expr, $key:expr, $mods:expr, $prefix:expr, $action:expr $(,)?) => {
            map.insert(
                BindingKey { mode: $mode, key: $key, modifiers: $mods, prefix: $prefix },
                $action,
            );
        };
    }

    for &mode in &[Normal, Insert, Visual, CommandLine, Search] {
        bind!(mode, KeyCode::Char('c'), ctrl, None, Quit);
    }

    // Quit is available via `:q`, `:quit`, `:q!`, `:quit!`, or Ctrl-C.
    bind!(Normal, KeyCode::Char('i'), none, None, EnterMode(Insert));
    bind!(Normal, KeyCode::Char('v'), none, None, EnterMode(Visual));
    bind!(Normal, KeyCode::Char(':'), none, None, EnterCommandMode);
    bind!(Normal, KeyCode::Char('"'), none, None, RegisterPrefix);
    bind!(Normal, KeyCode::Char('m'), none, None, MarkSetPrefix);
    bind!(Normal, KeyCode::Char('\''), none, None, MarkJumpPrefix { line_start: true });
    bind!(Normal, KeyCode::Char('`'), none, None, MarkJumpPrefix { line_start: false });
    bind!(Normal, KeyCode::Char('q'), none, None, MacroRecordToggle);
    bind!(Normal, KeyCode::Char('@'), none, None, MacroReplayPrefix);
    bind!(Normal, KeyCode::Left, none, None, Edit("move_left"));
    bind!(Normal, KeyCode::Char('h'), none, None, Edit("move_left"));
    bind!(Normal, KeyCode::Right, none, None, Edit("move_right"));
    bind!(Normal, KeyCode::Char('l'), none, None, Edit("move_right"));
    bind!(Normal, KeyCode::Up, none, None, Edit("move_up"));
    bind!(Normal, KeyCode::Char('k'), none, None, Edit("move_up"));
    bind!(Normal, KeyCode::Down, none, None, Edit("move_down"));
    bind!(Normal, KeyCode::Char('j'), none, None, Edit("move_down"));
    bind!(Normal, KeyCode::Char('w'), none, None, Edit("move_word_right"));
    bind!(Normal, KeyCode::Char('e'), none, None, Edit("move_word_right"));
    bind!(Normal, KeyCode::Char('b'), none, None, Edit("move_word_left"));
    bind!(Normal, KeyCode::Char('^'), none, None, Edit("move_to_beginning_of_paragraph"),);
    bind!(Normal, KeyCode::Char('$'), none, None, Edit("move_to_right_end_of_line"));
    bind!(Normal, KeyCode::Char('G'), none, None, Edit("move_to_end_of_document"));
    bind!(Normal, KeyCode::Char('g'), none, None, SetPrefix('g'));
    // `]` / `[` prefix for list navigation (e.g. ]q / [q).
    bind!(Normal, KeyCode::Char(']'), none, None, SetPrefix(']'));
    bind!(Normal, KeyCode::Char('['), none, None, SetPrefix('['));
    // ]q / [q — quickfix next / prev
    bind!(Normal, KeyCode::Char('q'), none, Some(']'), QfNext);
    bind!(Normal, KeyCode::Char('q'), none, Some('['), QfPrev);
    // ]Q / [Q — location list next / prev
    bind!(Normal, KeyCode::Char('Q'), none, Some(']'), LocNext);
    bind!(Normal, KeyCode::Char('Q'), none, Some('['), LocPrev);
    // ]h / [h — git hunk next / prev
    bind!(Normal, KeyCode::Char('h'), none, Some(']'), GitNextHunk);
    bind!(Normal, KeyCode::Char('h'), none, Some('['), GitPrevHunk);
    bind!(Normal, KeyCode::Char('g'), none, Some('g'), Edit("move_to_beginning_of_document"),);
    bind!(Normal, KeyCode::Char('d'), none, Some('g'), Edit("duplicate_line"));
    bind!(Normal, KeyCode::Char('b'), none, Some('g'), GitBlame);
    bind!(Normal, KeyCode::Char('D'), none, Some('g'), GitDiff);
    // g+o — document symbols; g+O — workspace symbols
    bind!(Normal, KeyCode::Char('o'), none, Some('g'), RequestDocumentSymbols);
    bind!(Normal, KeyCode::Char('O'), none, Some('g'), RequestWorkspaceSymbols);
    bind!(Normal, KeyCode::Char('d'), ctrl, None, Edit("scroll_page_down"));
    bind!(Normal, KeyCode::Char('u'), ctrl, None, Edit("scroll_page_up"));
    bind!(Normal, KeyCode::Char('w'), ctrl, None, WindowCommandPrefix);
    bind!(Normal, KeyCode::Char('/'), none, None, EnterSearch);
    bind!(Normal, KeyCode::Char('?'), none, None, EnterSearchBackward);
    bind!(Normal, KeyCode::Char('n'), none, None, FindNext);
    bind!(Normal, KeyCode::Char('N'), none, None, FindPrevious);
    bind!(Normal, KeyCode::Char('K'), none, None, RequestHover);
    bind!(Normal, KeyCode::Up, ctrl, None, Edit("add_selection_above"));
    bind!(Normal, KeyCode::Down, ctrl, None, Edit("add_selection_below"));
    // * / # — search word under cursor forward / backward
    bind!(Normal, KeyCode::Char('*'), none, None, SearchWordUnderCursor { forward: true });
    bind!(Normal, KeyCode::Char('#'), none, None, SearchWordUnderCursor { forward: false });
    // Visual mode: * / # use selection as search pattern
    bind!(Visual, KeyCode::Char('*'), none, None, SearchWordUnderCursor { forward: true });
    bind!(Visual, KeyCode::Char('#'), none, None, SearchWordUnderCursor { forward: false });
    bind!(
        Normal,
        KeyCode::Char('f'),
        none,
        None,
        PendingCharFind { forward: true, inclusive: true },
    );
    bind!(
        Normal,
        KeyCode::Char('F'),
        none,
        None,
        PendingCharFind { forward: false, inclusive: true },
    );
    bind!(
        Normal,
        KeyCode::Char('t'),
        none,
        None,
        PendingCharFind { forward: true, inclusive: false },
    );
    bind!(
        Normal,
        KeyCode::Char('T'),
        none,
        None,
        PendingCharFind { forward: false, inclusive: false },
    );
    bind!(Normal, KeyCode::Char('%'), none, None, MatchingPair);

    // Operator-pending mode: operators
    bind!(Normal, KeyCode::Char('d'), none, None, SetOperator(Operator::Delete));
    bind!(Normal, KeyCode::Char('c'), none, None, SetOperator(Operator::Change));
    bind!(Normal, KeyCode::Char('y'), none, None, SetOperator(Operator::Yank));
    bind!(Normal, KeyCode::Char('>'), none, None, SetOperator(Operator::Indent));
    bind!(Normal, KeyCode::Char('<'), none, None, SetOperator(Operator::Outdent));
    // g-prefixed operators: gu (lowercase), gU (uppercase), g~ (case toggle)
    bind!(Normal, KeyCode::Char('u'), none, Some('g'), SetOperator(Operator::Lowercase));
    bind!(Normal, KeyCode::Char('U'), none, Some('g'), SetOperator(Operator::Uppercase));
    bind!(Normal, KeyCode::Char('~'), none, Some('g'), SetOperator(Operator::CaseToggle));

    // Insert-entry variants
    bind!(Normal, KeyCode::Char('a'), none, None, AppendAfterCursor);
    bind!(Normal, KeyCode::Char('A'), none, None, AppendAtEndOfLine);
    bind!(Normal, KeyCode::Char('I'), none, None, InsertAtLineStart);
    bind!(Normal, KeyCode::Char('o'), none, None, OpenLineBelow);
    bind!(Normal, KeyCode::Char('O'), none, None, OpenLineAbove);
    bind!(Normal, KeyCode::Char('s'), none, None, SubstituteChar);
    bind!(Normal, KeyCode::Char('S'), none, None, SubstituteLine);

    // Insert mode editing controls (bound here for completeness; Ctrl keys
    bind!(Normal, KeyCode::Char('K'), none, None, RequestHover);
    bind!(Normal, KeyCode::Char('a'), ctrl, None, Edit("increase_number"));
    bind!(Normal, KeyCode::Char('x'), ctrl, None, Edit("decrease_number"));
    bind!(Insert, KeyCode::Char('w'), ctrl, None, DeleteWordBackward);
    bind!(Insert, KeyCode::Char('u'), ctrl, None, DeleteToLineStart);
    bind!(Insert, KeyCode::Char('t'), ctrl, None, IndentLine);
    bind!(Insert, KeyCode::Char('d'), ctrl, None, OutdentLine);

    bind!(Visual, KeyCode::Esc, none, None, CollapseAndEnterNormal);
    bind!(Visual, KeyCode::Char('v'), none, None, CollapseAndEnterNormal);
    bind!(Visual, KeyCode::Char(':'), none, None, EnterCommandMode);
    bind!(Visual, KeyCode::Left, none, None, Edit("move_left_and_modify_selection"),);
    bind!(Visual, KeyCode::Char('h'), none, None, Edit("move_left_and_modify_selection"),);
    bind!(Visual, KeyCode::Right, none, None, Edit("move_right_and_modify_selection"),);
    bind!(Visual, KeyCode::Char('l'), none, None, Edit("move_right_and_modify_selection"),);
    bind!(Visual, KeyCode::Up, none, None, Edit("move_up_and_modify_selection"),);
    bind!(Visual, KeyCode::Char('k'), none, None, Edit("move_up_and_modify_selection"),);
    bind!(Visual, KeyCode::Down, none, None, Edit("move_down_and_modify_selection"),);
    bind!(Visual, KeyCode::Char('j'), none, None, Edit("move_down_and_modify_selection"),);
    // Visual char: word motions also extend selection
    bind!(Visual, KeyCode::Char('w'), none, None, Edit("move_word_right_and_modify_selection"),);
    bind!(Visual, KeyCode::Char('b'), none, None, Edit("move_word_left_and_modify_selection"),);
    bind!(
        Visual,
        KeyCode::Char('$'),
        none,
        None,
        Edit("move_to_right_end_of_line_and_modify_selection"),
    );
    bind!(
        Visual,
        KeyCode::Char('^'),
        none,
        None,
        Edit("move_to_beginning_of_paragraph_and_modify_selection"),
    );
    // Anchor swap in visual char
    bind!(Visual, KeyCode::Char('o'), none, None, SwapVisualAnchor);

    // Visual Line mode (V)
    bind!(Normal, KeyCode::Char('V'), none, None, EnterVisualLine);
    bind!(VisualLine, KeyCode::Esc, none, None, CollapseAndEnterNormal);
    bind!(VisualLine, KeyCode::Char('V'), none, None, CollapseAndEnterNormal);
    bind!(VisualLine, KeyCode::Char('v'), none, None, EnterMode(Visual));
    bind!(VisualLine, KeyCode::Up, none, None, Edit("move_up_and_modify_selection"),);
    bind!(VisualLine, KeyCode::Char('k'), none, None, Edit("move_up_and_modify_selection"),);
    bind!(VisualLine, KeyCode::Down, none, None, Edit("move_down_and_modify_selection"),);
    bind!(VisualLine, KeyCode::Char('j'), none, None, Edit("move_down_and_modify_selection"),);
    bind!(
        VisualLine,
        KeyCode::Char('G'),
        none,
        None,
        Edit("move_to_end_of_document_and_modify_selection"),
    );
    bind!(VisualLine, KeyCode::Char('o'), none, None, SwapVisualAnchor);
    bind!(VisualLine, KeyCode::Char(':'), none, None, EnterCommandMode);

    // Visual Block mode (Ctrl-V)
    bind!(Normal, KeyCode::Char('v'), ctrl, None, EnterVisualBlock);
    bind!(VisualBlock, KeyCode::Esc, none, None, CollapseAndEnterNormal);
    bind!(VisualBlock, KeyCode::Char('v'), ctrl, None, CollapseAndEnterNormal);
    bind!(VisualBlock, KeyCode::Left, none, None, Edit("move_left"),);
    bind!(VisualBlock, KeyCode::Char('h'), none, None, Edit("move_left"),);
    bind!(VisualBlock, KeyCode::Right, none, None, Edit("move_right"),);
    bind!(VisualBlock, KeyCode::Char('l'), none, None, Edit("move_right"),);
    bind!(VisualBlock, KeyCode::Up, none, None, Edit("move_up"),);
    bind!(VisualBlock, KeyCode::Char('k'), none, None, Edit("move_up"),);
    bind!(VisualBlock, KeyCode::Down, none, None, Edit("move_down"),);
    bind!(VisualBlock, KeyCode::Char('j'), none, None, Edit("move_down"),);
    bind!(VisualBlock, KeyCode::Char('o'), none, None, SwapVisualAnchor);
    bind!(VisualBlock, KeyCode::Char('I'), none, None, VisualBlockInsert);
    bind!(VisualBlock, KeyCode::Char('A'), none, None, VisualBlockAppend);
    bind!(VisualBlock, KeyCode::Char(':'), none, None, EnterCommandMode);

    // Undo / Redo (Normal mode)
    bind!(Normal, KeyCode::Char('u'), none, None, Undo);
    bind!(Normal, KeyCode::Char('r'), ctrl, None, Redo);

    // Repeat last change
    bind!(Normal, KeyCode::Char('.'), none, None, RepeatLastChange);

    // Paste
    bind!(Normal, KeyCode::Char('p'), none, None, PasteAfter);
    bind!(Normal, KeyCode::Char('P'), none, None, PasteBefore);
    bind!(Visual, KeyCode::Char('p'), none, None, PasteAfter);

    // Restore last visual selection
    bind!(Normal, KeyCode::Char('v'), none, Some('g'), RestoreLastVisual);

    // Jump list navigation (Ctrl-O = older, Ctrl-I = newer)
    bind!(Normal, KeyCode::Char('o'), ctrl, None, JumpListOlder);
    bind!(Normal, KeyCode::BackTab, none, None, JumpListNewer);
    bind!(Normal, KeyCode::Tab, none, None, JumpListNewer);

    // Change list navigation (g; = older, g, = newer)
    bind!(Normal, KeyCode::Char(';'), none, Some('g'), ChangeListOlder);
    bind!(Normal, KeyCode::Char(','), none, Some('g'), ChangeListNewer);

    // Tab navigation (gt = next tab, gT = prev tab)
    bind!(Normal, KeyCode::Char('t'), none, Some('g'), TabNext);
    bind!(Normal, KeyCode::Char('T'), none, Some('g'), TabPrev);

    bind!(Insert, KeyCode::Esc, none, None, EnterMode(Normal));
    bind!(Insert, KeyCode::Left, none, None, Edit("move_left"));
    bind!(Insert, KeyCode::Right, none, None, Edit("move_right"));
    bind!(Insert, KeyCode::Up, none, None, Edit("move_up"));
    bind!(Insert, KeyCode::Down, none, None, Edit("move_down"));
    bind!(Insert, KeyCode::Enter, none, None, Edit("insert_newline"));
    bind!(Insert, KeyCode::Backspace, none, None, DeleteBackward);

    bind!(CommandLine, KeyCode::Esc, none, None, EnterMode(Normal));
    bind!(CommandLine, KeyCode::Enter, none, None, ExecuteCommand);
    bind!(CommandLine, KeyCode::Backspace, none, None, CommandBackspace);
    bind!(CommandLine, KeyCode::Tab, none, None, CompleteCommandLine);
    bind!(CommandLine, KeyCode::Up, none, None, CommandHistoryOlder);
    bind!(CommandLine, KeyCode::Down, none, None, CommandHistoryNewer);

    bind!(Search, KeyCode::Esc, none, None, EnterMode(Normal));
    bind!(Search, KeyCode::Enter, none, None, ExecuteSearch);
    bind!(Search, KeyCode::Backspace, none, None, SearchBackspace);
    // Alt+Enter in search mode: select all occurrences (find_all) and return to Normal.
    bind!(Search, KeyCode::Enter, KeyModifiers::ALT, None, FindAll);

    // z-prefix: fold commands
    bind!(Normal, KeyCode::Char('z'), none, None, SetPrefix('z'));
    bind!(Normal, KeyCode::Char('a'), none, Some('z'), FoldToggle);
    bind!(Normal, KeyCode::Char('o'), none, Some('z'), FoldOpen);
    bind!(Normal, KeyCode::Char('c'), none, Some('z'), FoldClose);
    bind!(Normal, KeyCode::Char('R'), none, Some('z'), FoldOpenAll);
    bind!(Normal, KeyCode::Char('M'), none, Some('z'), FoldCloseAll);

    map
}
