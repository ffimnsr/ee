use std::collections::HashMap;
use std::sync::OnceLock;

use crossterm::event::{KeyCode, KeyModifiers};

use crate::app::{Mode, Operator};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
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
    ExecuteSearch,
    FindNext,
    FindPrevious,
    SetPrefix(char),
    PendingCharFind { forward: bool, inclusive: bool },
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
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct BindingKey {
    pub(crate) mode: Mode,
    pub(crate) key: KeyCode,
    pub(crate) modifiers: KeyModifiers,
    pub(crate) prefix: Option<char>,
}

pub(crate) fn bindings() -> &'static HashMap<BindingKey, Action> {
    static BINDINGS: OnceLock<HashMap<BindingKey, Action>> = OnceLock::new();
    BINDINGS.get_or_init(build_bindings)
}

fn build_bindings() -> HashMap<BindingKey, Action> {
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

    // `q` is NOT bound here; it is handled in handle_default for macro recording.
    // Quit is available via `:q`, `:quit`, `:q!`, `:quit!`, or Ctrl-C.
    bind!(Normal, KeyCode::Char('i'), none, None, EnterMode(Insert));
    bind!(Normal, KeyCode::Char('v'), none, None, EnterMode(Visual));
    bind!(Normal, KeyCode::Char(':'), none, None, EnterCommandMode);
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
    bind!(Normal, KeyCode::Char('g'), none, Some('g'), Edit("move_to_beginning_of_document"),);
    bind!(Normal, KeyCode::Char('d'), ctrl, None, Edit("scroll_page_down"));
    bind!(Normal, KeyCode::Char('u'), ctrl, None, Edit("scroll_page_up"));
    bind!(Normal, KeyCode::Char('/'), none, None, EnterSearch);
    bind!(Normal, KeyCode::Char('n'), none, None, FindNext);
    bind!(Normal, KeyCode::Char('N'), none, None, FindPrevious);
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
    // are also handled in handle_default for robustness).
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
    bind!(Visual, KeyCode::Char('$'), none, None, Edit("move_to_right_end_of_line_and_modify_selection"),);
    bind!(Visual, KeyCode::Char('^'), none, None, Edit("move_to_beginning_of_paragraph_and_modify_selection"),);
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
    bind!(VisualLine, KeyCode::Char('G'), none, None, Edit("move_to_end_of_document_and_modify_selection"),);
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
    bind!(CommandLine, KeyCode::Up, none, None, CommandHistoryOlder);
    bind!(CommandLine, KeyCode::Down, none, None, CommandHistoryNewer);

    bind!(Search, KeyCode::Esc, none, None, EnterMode(Normal));
    bind!(Search, KeyCode::Enter, none, None, ExecuteSearch);
    bind!(Search, KeyCode::Backspace, none, None, SearchBackspace);

    map
}
