//! Default vim-style single-key bindings.
use super::*;

pub(crate) fn build_vim_bindings() -> HashMap<BindingKey, Action> {
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

    bind!(Normal, KeyCode::Char('c'), ctrl, None, NoOp);
    for &mode in &[Insert, Visual, CommandLine, Search] {
        bind!(mode, KeyCode::Char('c'), ctrl, None, EnterMode(Normal));
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
    bind!(Normal, KeyCode::PageDown, none, None, Edit("scroll_page_down"));
    bind!(Normal, KeyCode::PageUp, none, None, Edit("scroll_page_up"));
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
    bind!(Insert, KeyCode::Tab, none, None, Edit("insert_tab"));
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
