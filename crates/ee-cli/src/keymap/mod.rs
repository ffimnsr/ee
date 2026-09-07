pub(crate) use std::collections::HashMap;
pub(crate) use std::sync::OnceLock;

pub(crate) use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub(crate) use crate::app::{Mode, Operator};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    NoOp,
    /// Agents-pane-local actions. These only dispatch while `mode = "agent"`.
    AgentHistoryPrevious,
    AgentHistoryNext,
    AgentHistorySearchReverse,
    AgentDraftStash,
    AgentDraftRestore,
    AgentDraftExternalEdit,
    AgentToggleTranscriptDetails,
    AgentToggleTranscriptRaw,
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

#[allow(unused_imports)] // tests use `bindings`; the bin build does not
pub(crate) use hints::{
    bindings, bindings_for, format_key_press, format_key_sequence, key_press_from_event,
    macro_record_hint_entries, macro_replay_hint_entries, mark_jump_hint_entries,
    mark_set_hint_entries, parse_key_sequence_spec, prefix_hint_entries, register_hint_entries,
    replace_char_hint_entries, sequence_bindings_for, window_command_hint_entries,
    with_cancel_hint,
};
pub(crate) use spec::{
    format_action_spec, format_binding_mode, parse_action_spec, parse_binding_mode,
    parse_binding_spec,
};

mod defaults;
mod hints;
mod spec;
mod vim;
