use std::collections::HashMap;
use std::sync::OnceLock;

use crossterm::event::{KeyCode, KeyModifiers};

use crate::app::Mode;

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

    bind!(Normal, KeyCode::Char('q'), none, None, Quit);
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

    bind!(Search, KeyCode::Esc, none, None, EnterMode(Normal));
    bind!(Search, KeyCode::Enter, none, None, ExecuteSearch);
    bind!(Search, KeyCode::Backspace, none, None, SearchBackspace);

    map
}
