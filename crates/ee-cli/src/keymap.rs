use std::collections::HashMap;
use std::sync::OnceLock;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
    PrefillCommandLine(&'static str),
    DeleteBackward,
    CommandBackspace,
    SearchBackspace,
    EnterSearch,
    EnterSearchBackward,
    ExecuteSearch,
    CompleteCommandLine,
    FindNext,
    FindPrevious,
    RequestCompletion,
    RequestHover,
    RequestDeclaration,
    RequestDefinition,
    RequestTypeDefinition,
    RequestReferences,
    RequestImplementation,
    RequestDocumentSymbols,
    RequestWorkspaceSymbols,
    RequestCodeActions,
    SwiftMotion,
    GlobalSearch,
    CommandPalette,
    FilePicker,
    FilePickerInCurrentDirectory,
    FileExplorer,
    FileExplorerInCurrentBufferDirectory,
    FileExplorerInCurrentDirectory,
    BufferPicker,
    JumpListPicker,
    ChangedFilePicker,
    DiagnosticsPicker,
    WorkspaceDiagnosticsPicker,
    LastPicker,
    PickerClose,
    PickerConfirm,
    PickerMoveUp,
    PickerMoveDown,
    PickerBackspace,
    QuickfixClose,
    QuickfixConfirm,
    QuickfixMoveUp,
    QuickfixMoveDown,
    LocationListClose,
    LocationListConfirm,
    LocationListMoveUp,
    LocationListMoveDown,
    SubstituteConfirmApply,
    SubstituteConfirmSkip,
    SubstituteConfirmApplyAll,
    SubstituteConfirmCancel,
    RegisterPrefix,
    InsertRegister,
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
    GotoFirstNonWhitespace,
    GotoLine,
    GotoColumn,
    GotoFileStart,
    GotoLastLine,
    GotoFile,
    GotoWindowTop,
    GotoWindowCenter,
    GotoWindowBottom,
    GotoLastAccessedFile,
    GotoLastModifiedFile,
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
    YankToClipboard,
    YankToPrimaryClipboard,
    YankMainSelectionToClipboard,
    YankMainSelectionToPrimaryClipboard,
    IndentSelection,
    UnindentSelection,
    FormatSelections,
    ExtendLineBelow,
    ExtendToLineBounds,
    ShrinkToLineBounds,
    JoinSelections,
    JoinSelectionsSpace,
    KeepSelections,
    RemoveSelections,
    ExpandSelection,
    ShrinkSelection,
    SelectPrevSibling,
    SelectNextSibling,
    SelectAllSiblings,
    SelectAllChildren,
    MoveParentNodeStart,
    MoveParentNodeEnd,
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
    AddNewlineBelow,
    AddNewlineAbove,
    DeleteCurrentLine,
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
    PasteClipboardAfter,
    PasteClipboardBefore,
    PastePrimaryClipboardAfter,
    PastePrimaryClipboardBefore,
    ReplaceSelectionsWithClipboard,
    ReplaceSelectionsWithPrimaryClipboard,
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
    RotateView,
    RotateViewReverse,
    TransposeView,
    WindowClose,
    WindowOnly,
    JumpViewLeft,
    JumpViewDown,
    JumpViewUp,
    JumpViewRight,
    SwapViewLeft,
    SwapViewDown,
    SwapViewUp,
    SwapViewRight,
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
    GitFirstHunk,
    GitLastHunk,
    GitBlame,
    GitDiff,
    // Fold commands (z-prefix)
    FoldToggle,
    FoldOpen,
    FoldClose,
    FoldOpenAll,
    FoldCloseAll,
    CommitUndoCheckpoint,
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
pub(crate) struct KeyPress {
    pub(crate) key: KeyCode,
    pub(crate) modifiers: KeyModifiers,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SequenceBinding {
    pub(crate) mode: Mode,
    pub(crate) sequence: Vec<KeyPress>,
    pub(crate) action: Action,
    pub(crate) description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeyHintEntry {
    pub(crate) key: String,
    pub(crate) description: String,
    pub(crate) is_group: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct SequenceNode {
    pub(crate) action: Option<Action>,
    pub(crate) description: Option<String>,
    pub(crate) children: HashMap<KeyPress, SequenceNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct SequenceBindings {
    roots: HashMap<Mode, SequenceNode>,
}

impl SequenceNode {
    fn insert(&mut self, sequence: &[KeyPress], action: Action, description: String) {
        if let Some((head, tail)) = sequence.split_first() {
            self.children.entry(*head).or_default().insert(tail, action, description);
            return;
        }
        self.action = Some(action);
        self.description = Some(description);
    }

    fn collect_descriptions(&self, out: &mut Vec<String>, limit: usize) {
        if out.len() >= limit {
            return;
        }
        if let Some(description) = &self.description
            && !out.iter().any(|existing| existing == description)
        {
            out.push(description.clone());
        }
        for child in self.children.values() {
            if out.len() >= limit {
                break;
            }
            child.collect_descriptions(out, limit);
        }
    }

    fn hint_description(&self) -> String {
        if let Some(description) = &self.description {
            return description.clone();
        }

        let mut collected = Vec::new();
        self.collect_descriptions(&mut collected, 3);
        if collected.is_empty() {
            return String::from("prefix");
        }

        let mut summary = collected.join(", ");
        if self.children.len() > collected.len() {
            summary.push_str(", ...");
        }
        summary
    }

    pub(crate) fn hint_entries(&self) -> Vec<KeyHintEntry> {
        let mut entries = self
            .children
            .iter()
            .map(|(key, child)| KeyHintEntry {
                key: format_key_press(*key),
                description: child.hint_description(),
                is_group: !child.children.is_empty(),
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.key.cmp(&right.key));
        entries
    }
}

impl SequenceBindings {
    pub(crate) fn has_mode(&self, mode: Mode) -> bool {
        self.roots.contains_key(&mode)
    }

    pub(crate) fn node_for_sequence(
        &self,
        mode: Mode,
        sequence: &[KeyPress],
    ) -> Option<&SequenceNode> {
        let mut node = self.roots.get(&mode)?;
        for key in sequence {
            node = node.children.get(key)?;
        }
        Some(node)
    }

    pub(crate) fn advance(
        &self,
        mode: Mode,
        sequence: &[KeyPress],
        key: KeyPress,
    ) -> Option<(Vec<KeyPress>, &SequenceNode)> {
        let node = self.node_for_sequence(mode, sequence)?;
        let matched = node.children.get(&key).map(|child| (key, child)).or_else(|| {
            if key.modifiers == KeyModifiers::NONE {
                None
            } else {
                let fallback = KeyPress { modifiers: KeyModifiers::NONE, ..key };
                node.children.get(&fallback).map(|child| (fallback, child))
            }
        })?;
        let mut next = sequence.to_vec();
        next.push(matched.0);
        Some((next, matched.1))
    }
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
    /// Timeout in milliseconds before an in-progress key sequence is cancelled.
    /// `0` disables the timeout.
    pub(crate) sequence_timeout_ms: u64,
    pub(crate) operations: Vec<KeymapOperation>,
    pub(crate) sequence_bindings: Vec<SequenceBinding>,
}

impl Default for KeymapSettings {
    fn default() -> Self {
        Self {
            inherit_defaults: true,
            sequence_timeout_ms: 1_000,
            operations: Vec::new(),
            sequence_bindings: Vec::new(),
        }
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

pub(crate) fn sequence_bindings_for(settings: &KeymapSettings) -> SequenceBindings {
    let mut roots = HashMap::new();
    let bindings = if settings.inherit_defaults {
        let mut bindings = default_sequence_bindings().clone();
        bindings.extend(settings.sequence_bindings.iter().cloned());
        bindings
    } else {
        settings.sequence_bindings.clone()
    };

    for binding in &bindings {
        roots
            .entry(binding.mode)
            .or_insert_with(|| SequenceNode {
                action: None,
                description: None,
                children: HashMap::new(),
            })
            .insert(&binding.sequence, binding.action.clone(), binding.description.clone());
    }
    SequenceBindings { roots }
}

pub(crate) fn key_press_from_event(key: KeyEvent) -> KeyPress {
    KeyPress { key: key.code, modifiers: key.modifiers }
}

pub(crate) fn parse_key_sequence_spec(specs: &[String]) -> Result<Vec<KeyPress>, String> {
    if specs.is_empty() {
        return Err(String::from("keys must contain at least one entry"));
    }
    specs.iter().map(|spec| parse_key_press_spec(spec)).collect()
}

pub(crate) fn format_key_sequence(sequence: &[KeyPress]) -> String {
    sequence.iter().map(|key| format_key_press(*key)).collect::<Vec<_>>().join(" ")
}

pub(crate) fn format_key_press(key: KeyPress) -> String {
    let mut parts = Vec::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("Ctrl".to_owned());
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        parts.push("Alt".to_owned());
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        parts.push("Shift".to_owned());
    }

    let key_name = match key.key {
        KeyCode::Left => String::from("Left"),
        KeyCode::Right => String::from("Right"),
        KeyCode::Up => String::from("Up"),
        KeyCode::Down => String::from("Down"),
        KeyCode::Enter => String::from("Enter"),
        KeyCode::Backspace => String::from("Backspace"),
        KeyCode::Tab => String::from("Tab"),
        KeyCode::BackTab => String::from("BackTab"),
        KeyCode::Esc => String::from("Esc"),
        KeyCode::Char(' ') => String::from("SPC"),
        KeyCode::Char(ch) => ch.to_string(),
        other => format!("{other:?}"),
    };

    if parts.is_empty() {
        key_name
    } else {
        parts.push(key_name);
        parts.join("+")
    }
}

pub(crate) fn prefix_hint_entries(
    bindings: &HashMap<BindingKey, Action>,
    mode: Mode,
    prefix: char,
) -> Vec<KeyHintEntry> {
    let mut entries = bindings
        .iter()
        .filter(|(binding, _)| binding.mode == mode && binding.prefix == Some(prefix))
        .map(|(binding, action)| KeyHintEntry {
            key: format_key_press(KeyPress { key: binding.key, modifiers: binding.modifiers }),
            description: action_hint_description(action),
            is_group: false,
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.key.cmp(&right.key));
    entries
}

pub(crate) fn register_hint_entries() -> Vec<KeyHintEntry> {
    vec![
        KeyHintEntry {
            key: String::from("\""),
            description: String::from("unnamed register"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("0"),
            description: String::from("last yank"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("1-9"),
            description: String::from("delete history"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("a-z / A-Z"),
            description: String::from("named register / append"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("_"),
            description: String::from("black hole"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("/"),
            description: String::from("search register"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("+"),
            description: String::from("system clipboard"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("*"),
            description: String::from("primary clipboard"),
            is_group: false,
        },
    ]
}

pub(crate) fn mark_set_hint_entries() -> Vec<KeyHintEntry> {
    vec![KeyHintEntry {
        key: String::from("a-z"),
        description: String::from("named mark"),
        is_group: false,
    }]
}

pub(crate) fn mark_jump_hint_entries(line_start: bool) -> Vec<KeyHintEntry> {
    vec![
        KeyHintEntry {
            key: String::from("a-z"),
            description: String::from(if line_start {
                "named mark line"
            } else {
                "named mark exact"
            }),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("'"),
            description: String::from(if line_start {
                "previous jump line"
            } else {
                "previous jump exact"
            }),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("`"),
            description: String::from(if line_start {
                "previous jump line"
            } else {
                "previous jump exact"
            }),
            is_group: false,
        },
    ]
}

pub(crate) fn macro_record_hint_entries() -> Vec<KeyHintEntry> {
    vec![KeyHintEntry {
        key: String::from("a-z"),
        description: String::from("macro register"),
        is_group: false,
    }]
}

pub(crate) fn macro_replay_hint_entries() -> Vec<KeyHintEntry> {
    vec![
        KeyHintEntry {
            key: String::from("a-z"),
            description: String::from("named macro"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("@"),
            description: String::from("last macro"),
            is_group: false,
        },
    ]
}

pub(crate) fn replace_char_hint_entries() -> Vec<KeyHintEntry> {
    vec![KeyHintEntry {
        key: String::from("char"),
        description: String::from("replacement character"),
        is_group: false,
    }]
}

pub(crate) fn with_cancel_hint(mut entries: Vec<KeyHintEntry>) -> Vec<KeyHintEntry> {
    if entries.iter().all(|entry| entry.key != "Esc") {
        entries.insert(
            0,
            KeyHintEntry {
                key: String::from("Esc"),
                description: String::from("cancel"),
                is_group: false,
            },
        );
    }
    entries
}

pub(crate) fn window_command_hint_entries() -> Vec<KeyHintEntry> {
    let mut entries = vec![
        KeyHintEntry {
            key: String::from("s"),
            description: String::from("split horizontally"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("v"),
            description: String::from("split vertically"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("w"),
            description: String::from("next window"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("W"),
            description: String::from("previous window"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("p"),
            description: String::from("previous window"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("h"),
            description: String::from("focus left"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("j"),
            description: String::from("focus down"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("k"),
            description: String::from("focus up"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("l"),
            description: String::from("focus right"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("t"),
            description: String::from("transpose windows"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("H"),
            description: String::from("swap left"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("J"),
            description: String::from("swap down"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("K"),
            description: String::from("swap up"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("L"),
            description: String::from("swap right"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("c"),
            description: String::from("close window"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("q"),
            description: String::from("close window"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("o"),
            description: String::from("only window"),
            is_group: false,
        },
    ];
    entries.sort_by(|left, right| left.key.cmp(&right.key));
    entries
}

fn action_hint_description(action: &Action) -> String {
    match action {
        Action::NoOp => String::from("prefix"),
        Action::EnterMode(mode) => format!("enter {}", mode_hint_description(*mode)),
        Action::Edit(method) => edit_hint_description(method),
        Action::PrefillCommandLine(prefix) => {
            format!("command: {}", prefix.trim())
        }
        Action::PendingCharFind { forward, inclusive } => {
            let direction = if *forward { "forward" } else { "backward" };
            let kind = if *inclusive { "find char" } else { "till char" };
            format!("{kind} {direction}")
        }
        Action::MoveWordStart { forward, long_word } => {
            let direction = if *forward { "next" } else { "previous" };
            let family = if *long_word { "long word" } else { "word" };
            format!("{direction} {family} start")
        }
        Action::MoveWordEnd { long_word } => {
            let family = if *long_word { "long word" } else { "word" };
            format!("next {family} end")
        }
        Action::SetOperator(operator) => match operator {
            Operator::Delete => String::from("delete operator"),
            Operator::Change => String::from("change operator"),
            Operator::Yank => String::from("yank operator"),
            Operator::Indent => String::from("indent operator"),
            Operator::Outdent => String::from("outdent operator"),
            Operator::Uppercase => String::from("uppercase operator"),
            Operator::Lowercase => String::from("lowercase operator"),
            Operator::CaseToggle => String::from("toggle case operator"),
        },
        Action::MarkJumpPrefix { line_start } => {
            String::from(if *line_start { "jump to mark line" } else { "jump to exact mark" })
        }
        Action::DeleteSelection { yank, enter_insert } => match (*yank, *enter_insert) {
            (true, false) => String::from("delete selection"),
            (false, false) => String::from("delete selection no yank"),
            (true, true) => String::from("change selection"),
            (false, true) => String::from("change selection no yank"),
        },
        Action::GotoFileStart => String::from("file start"),
        Action::GotoLastLine => String::from("last line"),
        Action::GotoFile => String::from("goto file"),
        Action::RequestDocumentSymbols => String::from("document symbols"),
        Action::RequestWorkspaceSymbols => String::from("workspace symbols"),
        Action::RestoreLastVisual => String::from("restore last visual"),
        Action::ChangeListOlder => String::from("older change"),
        Action::ChangeListNewer => String::from("newer change"),
        Action::TabNext => String::from("next tab"),
        Action::TabPrev => String::from("previous tab"),
        Action::QfNext => String::from("next quickfix"),
        Action::QfPrev => String::from("previous quickfix"),
        Action::LocNext => String::from("next location"),
        Action::LocPrev => String::from("previous location"),
        Action::GitNextHunk => String::from("next hunk"),
        Action::GitPrevHunk => String::from("previous hunk"),
        Action::GitBlame => String::from("git blame"),
        Action::GitDiff => String::from("git diff"),
        Action::FoldToggle => String::from("toggle fold"),
        Action::FoldOpen => String::from("open fold"),
        Action::FoldClose => String::from("close fold"),
        Action::FoldOpenAll => String::from("open all folds"),
        Action::FoldCloseAll => String::from("close all folds"),
        Action::SearchWordUnderCursor { forward } => {
            String::from(if *forward { "search word forward" } else { "search word backward" })
        }
        Action::SearchSelection { detect_word_boundaries } => {
            String::from(if *detect_word_boundaries {
                "search selection with word boundaries"
            } else {
                "search selection"
            })
        }
        other => humanize_action_debug(&format!("{other:?}")),
    }
}

fn edit_hint_description(method: &str) -> String {
    match method {
        "move_to_left_end_of_line" => String::from("line start"),
        "move_to_right_end_of_line" => String::from("line end"),
        "duplicate_line" => String::from("duplicate line"),
        other => humanize_identifier(other),
    }
}

fn mode_hint_description(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => "normal mode",
        Mode::Insert => "insert mode",
        Mode::Visual => "visual mode",
        Mode::VisualLine => "visual line mode",
        Mode::VisualBlock => "visual block mode",
        Mode::OperatorPending => "operator pending",
        Mode::CommandLine => "command line",
        Mode::Search => "search",
        Mode::Picker => "picker",
        Mode::Quickfix => "quickfix",
        Mode::LocationList => "location list",
        Mode::SubstituteConfirm => "substitute confirm",
    }
}

fn humanize_action_debug(debug: &str) -> String {
    let name = debug.split(['(', ' ', '{']).next().unwrap_or(debug);
    humanize_identifier(name)
}

fn humanize_identifier(identifier: &str) -> String {
    let mut out = String::new();
    let mut prev_is_lower_or_digit = false;
    for ch in identifier.chars() {
        if ch == '_' {
            out.push(' ');
            prev_is_lower_or_digit = false;
            continue;
        }
        if ch.is_ascii_uppercase() && prev_is_lower_or_digit {
            out.push(' ');
        }
        out.push(ch.to_ascii_lowercase());
        prev_is_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
    }
    out
}

fn default_sequence_bindings() -> &'static Vec<SequenceBinding> {
    static BINDINGS: OnceLock<Vec<SequenceBinding>> = OnceLock::new();
    BINDINGS.get_or_init(build_default_sequence_bindings)
}

fn build_default_sequence_bindings() -> Vec<SequenceBinding> {
    use Action::*;

    fn sequence(keys: &[&str]) -> Vec<KeyPress> {
        keys.iter()
            .map(|key| parse_key_press_spec(key).expect("default key sequence must parse"))
            .collect()
    }

    fn bind(
        modes: &[Mode],
        keys: &[&str],
        action: Action,
        description: &str,
    ) -> Vec<SequenceBinding> {
        let sequence = sequence(keys);
        modes
            .iter()
            .copied()
            .map(|mode| SequenceBinding {
                mode,
                sequence: sequence.clone(),
                action: action.clone(),
                description: description.to_owned(),
            })
            .collect()
    }

    let normal_modes = [Mode::Normal, Mode::Visual, Mode::VisualLine, Mode::VisualBlock];

    let mut bindings = Vec::new();
    bindings.extend(bind(&normal_modes, &["space", "f"], NoOp, "files"));
    bindings.extend(bind(&normal_modes, &["space", "f", "f"], FilePicker, "find files"));
    bindings.extend(bind(
        &normal_modes,
        &["space", "f", "d"],
        FilePickerInCurrentDirectory,
        "files in cwd",
    ));
    bindings.extend(bind(&normal_modes, &["space", "f", "e"], FileExplorer, "file explorer"));
    bindings.extend(bind(&normal_modes, &["space", "f", "r"], ChangedFilePicker, "recent changes"));
    bindings.extend(bind(&normal_modes, &["space", "b"], NoOp, "buffers"));
    bindings.extend(bind(&normal_modes, &["space", "b", "b"], BufferPicker, "switch buffer"));
    bindings.extend(bind(
        &normal_modes,
        &["space", "b", "d"],
        DiagnosticsPicker,
        "buffer diagnostics",
    ));
    bindings.extend(bind(&normal_modes, &["space", "b", "l"], LastPicker, "last picker"));
    bindings.extend(bind(&normal_modes, &["space", "s"], NoOp, "search"));
    bindings.extend(bind(&normal_modes, &["space", "s", "s"], GlobalSearch, "search workspace"));
    bindings.extend(bind(&normal_modes, &["space", "s", "m"], SwiftMotion, "swift motion"));
    bindings.extend(bind(
        &normal_modes,
        &["space", "s", "d"],
        RequestDocumentSymbols,
        "document symbols",
    ));
    bindings.extend(bind(
        &normal_modes,
        &["space", "s", "w"],
        RequestWorkspaceSymbols,
        "workspace symbols",
    ));
    bindings.extend(bind(&normal_modes, &["space", "g"], NoOp, "git"));
    bindings.extend(bind(&normal_modes, &["space", "g", "b"], GitBlame, "git blame"));
    bindings.extend(bind(&normal_modes, &["space", "g", "d"], GitDiff, "git diff"));
    bindings.extend(bind(&normal_modes, &["space", "g", "n"], GitNextHunk, "next hunk"));
    bindings.extend(bind(&normal_modes, &["space", "g", "p"], GitPrevHunk, "previous hunk"));
    bindings.extend(bind(&normal_modes, &["space", "c"], NoOp, "code"));
    bindings.extend(bind(&normal_modes, &["space", "c", "a"], RequestCodeActions, "code actions"));
    bindings.extend(bind(&normal_modes, &["space", "c", "h"], RequestHover, "hover"));
    bindings.extend(bind(&normal_modes, &["space", "c", "d"], RequestDefinition, "definition"));
    bindings.extend(bind(&normal_modes, &["space", "c", "r"], RequestReferences, "references"));
    bindings.extend(bind(
        &normal_modes,
        &["space", "c", "i"],
        RequestImplementation,
        "implementation",
    ));
    bindings.extend(bind(&normal_modes, &["space", "w"], NoOp, "windows"));
    bindings.extend(bind(&normal_modes, &["space", "w", "h"], JumpViewLeft, "focus left"));
    bindings.extend(bind(&normal_modes, &["space", "w", "j"], JumpViewDown, "focus down"));
    bindings.extend(bind(&normal_modes, &["space", "w", "k"], JumpViewUp, "focus up"));
    bindings.extend(bind(&normal_modes, &["space", "w", "l"], JumpViewRight, "focus right"));
    bindings.extend(bind(&normal_modes, &["space", "w", "r"], RotateView, "rotate windows"));
    bindings.extend(bind(&normal_modes, &["space", "w", "o"], WindowOnly, "only window"));
    bindings.extend(bind(&normal_modes, &["space", "p"], NoOp, "project"));
    bindings.extend(bind(&normal_modes, &["space", "p", "p"], CommandPalette, "command palette"));
    bindings.extend(bind(&normal_modes, &["space", "p", "f"], FilePicker, "project files"));

    bindings
}

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

fn parse_binding_mode_spec(spec: &str) -> Option<Mode> {
    parse_mode_spec(spec).or_else(|| match spec.trim().to_ascii_lowercase().as_str() {
        "picker" => Some(Mode::Picker),
        "quickfix" => Some(Mode::Quickfix),
        "location_list" | "locationlist" | "location" => Some(Mode::LocationList),
        _ => None,
    })
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

fn parse_key_press_spec(spec: &str) -> Result<KeyPress, String> {
    let (key, modifiers) = parse_key_spec(spec)?;
    Ok(KeyPress { key, modifiers })
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
    let ctrl_alt = KeyModifiers::CONTROL | KeyModifiers::ALT;

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

    bind!(Picker, KeyCode::Esc, none, None, PickerClose);
    bind!(Picker, KeyCode::Enter, none, None, PickerConfirm);
    bind!(Picker, KeyCode::Up, none, None, PickerMoveUp);
    bind!(Picker, KeyCode::Down, none, None, PickerMoveDown);
    bind!(Picker, KeyCode::Backspace, none, None, PickerBackspace);

    bind!(Quickfix, KeyCode::Esc, none, None, QuickfixClose);
    bind!(Quickfix, KeyCode::Char('q'), none, None, QuickfixClose);
    bind!(Quickfix, KeyCode::Char('j'), none, None, QuickfixMoveDown);
    bind!(Quickfix, KeyCode::Down, none, None, QuickfixMoveDown);
    bind!(Quickfix, KeyCode::Char('k'), none, None, QuickfixMoveUp);
    bind!(Quickfix, KeyCode::Up, none, None, QuickfixMoveUp);
    bind!(Quickfix, KeyCode::Enter, none, None, QuickfixConfirm);

    bind!(LocationList, KeyCode::Esc, none, None, LocationListClose);
    bind!(LocationList, KeyCode::Char('q'), none, None, LocationListClose);
    bind!(LocationList, KeyCode::Char('j'), none, None, LocationListMoveDown);
    bind!(LocationList, KeyCode::Down, none, None, LocationListMoveDown);
    bind!(LocationList, KeyCode::Char('k'), none, None, LocationListMoveUp);
    bind!(LocationList, KeyCode::Up, none, None, LocationListMoveUp);
    bind!(LocationList, KeyCode::Enter, none, None, LocationListConfirm);

    bind!(SubstituteConfirm, KeyCode::Char('y'), none, None, SubstituteConfirmApply);
    bind!(SubstituteConfirm, KeyCode::Char('Y'), none, None, SubstituteConfirmApply);
    bind!(SubstituteConfirm, KeyCode::Char('n'), none, None, SubstituteConfirmSkip);
    bind!(SubstituteConfirm, KeyCode::Char('N'), none, None, SubstituteConfirmSkip);
    bind!(SubstituteConfirm, KeyCode::Char('a'), none, None, SubstituteConfirmApplyAll);
    bind!(SubstituteConfirm, KeyCode::Char('A'), none, None, SubstituteConfirmApplyAll);
    bind!(SubstituteConfirm, KeyCode::Char('q'), none, None, SubstituteConfirmCancel);
    bind!(SubstituteConfirm, KeyCode::Char('Q'), none, None, SubstituteConfirmCancel);
    bind!(SubstituteConfirm, KeyCode::Esc, none, None, SubstituteConfirmCancel);

    bind!(Normal, KeyCode::Char('p'), ctrl, None, FilePickerInCurrentDirectory);
    bind!(Normal, KeyCode::Char('p'), ctrl_alt, None, CommandPalette);

    // Normal mode: unprefixed bindings.
    // Quit is available via `:q`, `:quit`, `:q!`, `:quit!`.
    bind!(Normal, KeyCode::Char('i'), none, None, EnterMode(Insert));
    bind!(Normal, KeyCode::Char('v'), none, None, EnterMode(Visual));
    bind!(Normal, KeyCode::Char('V'), none, None, EnterVisualLine);
    bind!(Normal, KeyCode::Char(':'), none, None, EnterCommandMode);
    bind!(Normal, KeyCode::Char('/'), none, None, EnterSearch);
    bind!(Normal, KeyCode::Char('?'), none, None, EnterSearchBackward);
    bind!(Normal, KeyCode::Char('"'), none, None, RegisterPrefix);
    bind!(Normal, KeyCode::Char('m'), none, None, MarkSetPrefix);
    bind!(Normal, KeyCode::Char('\''), none, None, MarkJumpPrefix { line_start: true });
    bind!(Normal, KeyCode::Char('`'), none, None, MarkJumpPrefix { line_start: false });
    bind!(Normal, KeyCode::Char('q'), none, None, MacroRecordToggle);
    bind!(Normal, KeyCode::Char('@'), none, None, MacroReplayPrefix);
    bind!(Normal, KeyCode::Char('z'), none, None, SetPrefix('z'));
    bind!(Normal, KeyCode::Char('g'), none, None, SetPrefix('g'));
    bind!(Normal, KeyCode::Char('['), none, None, SetPrefix('['));
    bind!(Normal, KeyCode::Char(']'), none, None, SetPrefix(']'));
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
    bind!(Normal, KeyCode::Char('^'), none, None, GotoFirstNonWhitespace);
    bind!(Normal, KeyCode::Char('$'), none, None, Edit("move_to_right_end_of_line"));
    bind!(Normal, KeyCode::Char('G'), none, None, Edit("move_to_end_of_document"));
    bind!(Normal, KeyCode::Char('d'), ctrl, None, Edit("scroll_page_down"));
    bind!(Normal, KeyCode::Char('u'), ctrl, None, Edit("scroll_page_up"));
    bind!(Normal, KeyCode::Char('w'), ctrl, None, WindowCommandPrefix);
    bind!(Normal, KeyCode::Char('o'), ctrl, None, JumpListOlder);
    bind!(Normal, KeyCode::Tab, none, None, JumpListNewer);
    bind!(Normal, KeyCode::BackTab, none, None, JumpListNewer);
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
    bind!(Normal, KeyCode::Char('a'), ctrl, None, Edit("increase_number"));
    bind!(Normal, KeyCode::Char('x'), ctrl, None, Edit("decrease_number"));

    // Normal mode: g-prefixed bindings.
    bind!(Normal, KeyCode::Char('g'), none, Some('g'), GotoFileStart);
    bind!(Normal, KeyCode::Char('e'), none, Some('g'), GotoLastLine);
    bind!(Normal, KeyCode::Char('f'), none, Some('g'), GotoFile);
    bind!(Normal, KeyCode::Char('h'), none, Some('g'), Edit("move_to_left_end_of_line"));
    bind!(Normal, KeyCode::Char('l'), none, Some('g'), Edit("move_to_right_end_of_line"));
    bind!(Normal, KeyCode::Char('d'), none, Some('g'), Edit("duplicate_line"));
    bind!(Normal, KeyCode::Char('b'), none, Some('g'), GitBlame);
    bind!(Normal, KeyCode::Char('D'), none, Some('g'), GitDiff);
    bind!(Normal, KeyCode::Char('o'), none, Some('g'), RequestDocumentSymbols);
    bind!(Normal, KeyCode::Char('O'), none, Some('g'), RequestWorkspaceSymbols);
    bind!(Normal, KeyCode::Char('u'), none, Some('g'), SetOperator(Operator::Lowercase));
    bind!(Normal, KeyCode::Char('U'), none, Some('g'), SetOperator(Operator::Uppercase));
    bind!(Normal, KeyCode::Char('~'), none, Some('g'), SetOperator(Operator::CaseToggle));
    bind!(Normal, KeyCode::Char('v'), none, Some('g'), RestoreLastVisual);
    bind!(Normal, KeyCode::Char(';'), none, Some('g'), ChangeListOlder);
    bind!(Normal, KeyCode::Char(','), none, Some('g'), ChangeListNewer);
    bind!(Normal, KeyCode::Char('t'), none, Some('g'), TabNext);
    bind!(Normal, KeyCode::Char('T'), none, Some('g'), TabPrev);

    // Normal mode: list navigation prefixes.
    bind!(Normal, KeyCode::Char('q'), none, Some(']'), QfNext);
    bind!(Normal, KeyCode::Char('Q'), none, Some(']'), LocNext);
    bind!(Normal, KeyCode::Char('h'), none, Some(']'), GitNextHunk);
    bind!(Normal, KeyCode::Char('q'), none, Some('['), QfPrev);
    bind!(Normal, KeyCode::Char('Q'), none, Some('['), LocPrev);
    bind!(Normal, KeyCode::Char('h'), none, Some('['), GitPrevHunk);

    // Normal mode: z-prefixed fold bindings.
    bind!(Normal, KeyCode::Char('a'), none, Some('z'), FoldToggle);
    bind!(Normal, KeyCode::Char('o'), none, Some('z'), FoldOpen);
    bind!(Normal, KeyCode::Char('c'), none, Some('z'), FoldClose);
    bind!(Normal, KeyCode::Char('R'), none, Some('z'), FoldOpenAll);
    bind!(Normal, KeyCode::Char('M'), none, Some('z'), FoldCloseAll);

    // Visual mode: unprefixed bindings.
    bind!(Visual, KeyCode::Esc, none, None, CollapseAndEnterNormal);
    bind!(Visual, KeyCode::Char('v'), none, None, CollapseAndEnterNormal);
    bind!(Visual, KeyCode::Char(':'), none, None, EnterCommandMode);
    bind!(Visual, KeyCode::Char('o'), none, None, SwapVisualAnchor);
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
    bind!(Visual, KeyCode::Char('p'), none, None, PasteAfter);

    // Visual line mode: unprefixed bindings.
    bind!(VisualLine, KeyCode::Esc, none, None, CollapseAndEnterNormal);
    bind!(VisualLine, KeyCode::Char('V'), none, None, CollapseAndEnterNormal);
    bind!(VisualLine, KeyCode::Char('v'), none, None, EnterMode(Visual));
    bind!(VisualLine, KeyCode::Char('o'), none, None, SwapVisualAnchor);
    bind!(VisualLine, KeyCode::Char(':'), none, None, EnterCommandMode);
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

    // Visual block mode: unprefixed bindings.
    bind!(Normal, KeyCode::Char('v'), ctrl, None, EnterVisualBlock);
    bind!(VisualBlock, KeyCode::Esc, none, None, CollapseAndEnterNormal);
    bind!(VisualBlock, KeyCode::Char('v'), ctrl, None, CollapseAndEnterNormal);
    bind!(VisualBlock, KeyCode::Char('o'), none, None, SwapVisualAnchor);
    bind!(VisualBlock, KeyCode::Char('I'), none, None, VisualBlockInsert);
    bind!(VisualBlock, KeyCode::Char('A'), none, None, VisualBlockAppend);
    bind!(VisualBlock, KeyCode::Char(':'), none, None, EnterCommandMode);
    bind!(VisualBlock, KeyCode::Left, none, None, Edit("move_left"),);
    bind!(VisualBlock, KeyCode::Char('h'), none, None, Edit("move_left"),);
    bind!(VisualBlock, KeyCode::Right, none, None, Edit("move_right"),);
    bind!(VisualBlock, KeyCode::Char('l'), none, None, Edit("move_right"),);
    bind!(VisualBlock, KeyCode::Up, none, None, Edit("move_up"),);
    bind!(VisualBlock, KeyCode::Char('k'), none, None, Edit("move_up"),);
    bind!(VisualBlock, KeyCode::Down, none, None, Edit("move_down"),);
    bind!(VisualBlock, KeyCode::Char('j'), none, None, Edit("move_down"),);

    // Insert mode: unprefixed bindings.
    bind!(Insert, KeyCode::Esc, none, None, EnterMode(Normal));
    bind!(Insert, KeyCode::Left, none, None, Edit("move_left"));
    bind!(Insert, KeyCode::Right, none, None, Edit("move_right"));
    bind!(Insert, KeyCode::Up, none, None, Edit("move_up"));
    bind!(Insert, KeyCode::Down, none, None, Edit("move_down"));
    bind!(Insert, KeyCode::Enter, none, None, Edit("insert_newline"));
    bind!(Insert, KeyCode::Backspace, none, None, DeleteBackward);
    bind!(Insert, KeyCode::Char('r'), ctrl, None, InsertRegister);
    bind!(Insert, KeyCode::Char('w'), ctrl, None, DeleteWordBackward);
    bind!(Insert, KeyCode::Char('x'), ctrl, None, RequestCompletion);
    bind!(Insert, KeyCode::Char('u'), ctrl, None, DeleteToLineStart);
    bind!(Insert, KeyCode::Char('t'), ctrl, None, IndentLine);
    bind!(Insert, KeyCode::Char('d'), ctrl, None, OutdentLine);

    // Command-line mode: unprefixed bindings.
    bind!(CommandLine, KeyCode::Esc, none, None, EnterMode(Normal));
    bind!(CommandLine, KeyCode::Enter, none, None, ExecuteCommand);
    bind!(CommandLine, KeyCode::Backspace, none, None, CommandBackspace);
    bind!(CommandLine, KeyCode::Tab, none, None, CompleteCommandLine);
    bind!(CommandLine, KeyCode::Up, none, None, CommandHistoryOlder);
    bind!(CommandLine, KeyCode::Down, none, None, CommandHistoryNewer);

    // Search mode: unprefixed bindings.
    bind!(Search, KeyCode::Esc, none, None, EnterMode(Normal));
    bind!(Search, KeyCode::Enter, none, None, ExecuteSearch);
    bind!(Search, KeyCode::Backspace, none, None, SearchBackspace);
    bind!(Search, KeyCode::Enter, KeyModifiers::ALT, None, FindAll);

    // Normal mode: edit history and repeat.
    bind!(Normal, KeyCode::Char('u'), none, None, Undo);
    bind!(Normal, KeyCode::Char('r'), ctrl, None, Redo);
    bind!(Normal, KeyCode::Char('.'), none, None, RepeatLastChange);

    // Normal and visual mode: paste.
    bind!(Normal, KeyCode::Char('p'), none, None, PasteAfter);
    bind!(Normal, KeyCode::Char('P'), none, None, PasteBefore);

    map
}
