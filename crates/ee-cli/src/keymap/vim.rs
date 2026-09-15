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
    for &mode in &[Insert, Mode::Replace, Visual, CommandLine, Search] {
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
    bind!(Normal, KeyCode::Char('R'), none, None, EnterMode(Mode::Replace));
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
    bind!(Normal, KeyCode::Char('^'), none, None, GotoFirstNonWhitespace);
    bind!(Normal, KeyCode::Char('$'), none, None, Edit("move_to_right_end_of_line"));
    bind!(Normal, KeyCode::Char('G'), none, None, Edit("move_to_end_of_document"));
    // vim `w`/`e` use the vim word-boundary parsers in the backend; `b` keeps
    // the line-based word-start motion (matches vim `b`).
    bind!(
        Normal,
        KeyCode::Char('w'),
        none,
        None,
        MoveWordStart { forward: true, long_word: false }
    );
    bind!(Normal, KeyCode::Char('e'), none, None, MoveWordEnd { long_word: false });
    bind!(Normal, KeyCode::Char('b'), none, None, Edit("move_word_left"));
    bind!(Normal, KeyCode::PageDown, none, None, Edit("scroll_page_down"));
    bind!(Normal, KeyCode::PageUp, none, None, Edit("scroll_page_up"));
    // vim `Ctrl-d`/`Ctrl-u`: scroll half a window (count = lines).
    bind!(Normal, KeyCode::Char('d'), ctrl, None, PageCursorHalfDown);
    bind!(Normal, KeyCode::Char('u'), ctrl, None, PageCursorHalfUp);
    bind!(Normal, KeyCode::Char('w'), ctrl, None, WindowCommandPrefix);
    bind!(Normal, KeyCode::Char('o'), ctrl, None, JumpListOlder);
    bind!(Normal, KeyCode::Tab, none, None, JumpListNewer);
    bind!(Normal, KeyCode::BackTab, none, None, JumpListNewer);
    bind!(Normal, KeyCode::Char('n'), none, None, FindNext);
    bind!(Normal, KeyCode::Char('N'), none, None, FindPrevious);
    // `gn`/`gN`: backend find selects the next match, so these match vim's
    // "select next/prev search match" semantics.
    bind!(Normal, KeyCode::Char('n'), none, Some('g'), FindNext);
    bind!(Normal, KeyCode::Char('N'), none, Some('g'), FindPrevious);
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

    bind!(Normal, KeyCode::Char('g'), none, Some('g'), GotoFileStart);
    bind!(Normal, KeyCode::Char('e'), none, Some('g'), MoveWordEndBackward { long_word: false });
    bind!(Normal, KeyCode::Char('f'), none, Some('g'), GotoFile);
    bind!(Normal, KeyCode::Char('h'), none, Some('g'), Edit("move_to_left_end_of_line"));
    bind!(Normal, KeyCode::Char('l'), none, Some('g'), Edit("move_to_right_end_of_line"));
    bind!(Normal, KeyCode::Char('d'), none, Some('g'), RequestDeclaration);
    bind!(Normal, KeyCode::Char('D'), none, Some('g'), RequestDefinition);
    bind!(Normal, KeyCode::Char('o'), none, Some('g'), GotoByte);
    bind!(Normal, KeyCode::Char('b'), none, Some('g'), GitBlame);
    bind!(Normal, KeyCode::Char('u'), none, Some('g'), SetOperator(Operator::Lowercase));
    bind!(Normal, KeyCode::Char('U'), none, Some('g'), SetOperator(Operator::Uppercase));
    bind!(Normal, KeyCode::Char('~'), none, Some('g'), SetOperator(Operator::CaseToggle));
    bind!(Normal, KeyCode::Char('v'), none, Some('g'), RestoreLastVisual);
    bind!(Normal, KeyCode::Char(';'), none, Some('g'), ChangeListOlder);
    bind!(Normal, KeyCode::Char(','), none, Some('g'), ChangeListNewer);
    bind!(Normal, KeyCode::Char('t'), none, Some('g'), TabNext);
    bind!(Normal, KeyCode::Char('T'), none, Some('g'), TabPrev);
    // g-extras: vim insert-pendings, joins, formats, info.
    bind!(Normal, KeyCode::Char('i'), none, Some('g'), InsertAtLastEdit);
    bind!(Normal, KeyCode::Char('I'), none, Some('g'), InsertAtColumnZero);
    bind!(Normal, KeyCode::Char('J'), none, Some('g'), JoinLines { select_space: false });
    bind!(Normal, KeyCode::Char('p'), none, Some('g'), PasteAfter);
    bind!(Normal, KeyCode::Char('P'), none, Some('g'), PasteBefore);
    bind!(Normal, KeyCode::Char('q'), none, Some('g'), FormatSelections);
    bind!(Normal, KeyCode::Char('w'), none, Some('g'), FormatSelections);
    bind!(Normal, KeyCode::Char('x'), none, Some('g'), OpenTargetUnderCursor);
    bind!(Normal, KeyCode::Char('8'), none, Some('g'), HexDumpChar);
    bind!(Normal, KeyCode::Char('_'), none, Some('g'), GotoLineLastNonBlank);
    bind!(
        Normal,
        KeyCode::Char('*'),
        none,
        Some('g'),
        SearchWordUnderCursorLoose { forward: true }
    );
    bind!(
        Normal,
        KeyCode::Char('#'),
        none,
        Some('g'),
        SearchWordUnderCursorLoose { forward: false }
    );
    bind!(Normal, KeyCode::Char('E'), none, Some('g'), MoveWordEndBackward { long_word: true });

    // Normal mode: list navigation prefixes.
    bind!(Normal, KeyCode::Char('q'), none, Some(']'), QfNext);
    bind!(Normal, KeyCode::Char('Q'), none, Some(']'), LocNext);
    bind!(Normal, KeyCode::Char('h'), none, Some(']'), GitNextHunk);
    bind!(Normal, KeyCode::Char('c'), none, Some(']'), GitNextHunk);
    bind!(Normal, KeyCode::Char('q'), none, Some('['), QfPrev);
    bind!(Normal, KeyCode::Char('Q'), none, Some('['), LocPrev);
    bind!(Normal, KeyCode::Char('h'), none, Some('['), GitPrevHunk);
    bind!(Normal, KeyCode::Char('c'), none, Some('['), GitPrevHunk);

    // Normal mode: z-prefixed fold bindings.
    bind!(Normal, KeyCode::Char('a'), none, Some('z'), FoldToggle);
    bind!(Normal, KeyCode::Char('o'), none, Some('z'), FoldOpen);
    bind!(Normal, KeyCode::Char('c'), none, Some('z'), FoldClose);
    bind!(Normal, KeyCode::Char('R'), none, Some('z'), FoldOpenAll);
    bind!(Normal, KeyCode::Char('M'), none, Some('z'), FoldCloseAll);

    // Visual mode: unprefixed bindings.
    bind!(Visual, KeyCode::Esc, none, None, CollapseAndEnterNormal);
    // vim: pressing the mode key you're already in exits visual mode;
    // pressing a *different* mode key switches (V / Ctrl-v below).
    bind!(Visual, KeyCode::Char('v'), none, None, CollapseAndEnterNormal);
    bind!(Visual, KeyCode::Char('V'), none, None, EnterVisualLine);
    bind!(Visual, KeyCode::Char('v'), ctrl, None, EnterVisualBlock);
    bind!(Visual, KeyCode::Char(':'), none, None, EnterCommandMode);
    bind!(Visual, KeyCode::Char('o'), none, None, SwapVisualAnchor);
    bind!(Visual, KeyCode::Char('r'), none, None, Action::Replace);
    bind!(Visual, KeyCode::Char('%'), none, None, MatchingPair);
    bind!(Visual, KeyCode::Char(';'), none, None, RepeatLastMotion);
    bind!(Visual, KeyCode::Char(','), none, None, RepeatLastMotionReversed);
    bind!(Visual, KeyCode::Char('p'), none, None, PasteOverSelection);
    bind!(Visual, KeyCode::Left, none, None, Edit("move_left_and_modify_selection"),);
    bind!(Visual, KeyCode::Char('h'), none, None, Edit("move_left_and_modify_selection"),);
    bind!(Visual, KeyCode::Right, none, None, Edit("move_right_and_modify_selection"),);
    bind!(Visual, KeyCode::Char('l'), none, None, Edit("move_right_and_modify_selection"),);
    bind!(Visual, KeyCode::Up, none, None, Edit("move_up_and_modify_selection"),);
    bind!(Visual, KeyCode::Char('k'), none, None, Edit("move_up_and_modify_selection"),);
    bind!(Visual, KeyCode::Down, none, None, Edit("move_down_and_modify_selection"),);
    bind!(Visual, KeyCode::Char('j'), none, None, Edit("move_down_and_modify_selection"),);
    // Visual char: vim word motions also extend selection
    bind!(
        Visual,
        KeyCode::Char('w'),
        none,
        None,
        MoveWordStart { forward: true, long_word: false },
    );
    bind!(Visual, KeyCode::Char('e'), none, None, MoveWordEnd { long_word: false },);
    bind!(Visual, KeyCode::Char('W'), none, None, MoveWordStart { forward: true, long_word: true },);
    bind!(Visual, KeyCode::Char('E'), none, None, MoveWordEnd { long_word: true },);
    bind!(
        Visual,
        KeyCode::Char('B'),
        none,
        None,
        MoveWordStart { forward: false, long_word: true },
    );
    bind!(Visual, KeyCode::Char('e'), none, Some('g'), MoveWordEndBackward { long_word: false },);
    bind!(Visual, KeyCode::Char('E'), none, Some('g'), MoveWordEndBackward { long_word: true },);
    bind!(Visual, KeyCode::Char('b'), none, None, Edit("move_word_left_and_modify_selection"),);
    bind!(Visual, KeyCode::Char('{'), none, None, GotoParagraph { forward: false },);
    bind!(Visual, KeyCode::Char('}'), none, None, GotoParagraph { forward: true },);
    bind!(Visual, KeyCode::Char('('), none, None, GotoSentence { forward: false },);
    bind!(Visual, KeyCode::Char(')'), none, None, GotoSentence { forward: true },);
    bind!(Visual, KeyCode::Char('|'), none, None, GotoColumn);
    bind!(
        Visual,
        KeyCode::Char('+'),
        none,
        None,
        GotoLineFirstNonBlank { down: true, zero_based: false },
    );
    bind!(
        Visual,
        KeyCode::Char('-'),
        none,
        None,
        GotoLineFirstNonBlank { down: false, zero_based: false },
    );
    bind!(
        Visual,
        KeyCode::Char('_'),
        none,
        None,
        GotoLineFirstNonBlank { down: true, zero_based: true },
    );
    bind!(Visual, KeyCode::Char('_'), none, Some('g'), GotoLineLastNonBlank);
    bind!(
        Visual,
        KeyCode::Char('$'),
        none,
        None,
        Edit("move_to_right_end_of_line_and_modify_selection"),
    );
    bind!(Visual, KeyCode::Char('^'), none, None, GotoFirstNonWhitespace,);

    // Visual line mode: unprefixed bindings.
    bind!(VisualLine, KeyCode::Esc, none, None, CollapseAndEnterNormal);
    bind!(VisualLine, KeyCode::Char('V'), none, None, CollapseAndEnterNormal);
    bind!(VisualLine, KeyCode::Char('v'), none, None, EnterMode(Visual));
    bind!(VisualLine, KeyCode::Char('v'), ctrl, None, EnterVisualBlock);
    bind!(VisualLine, KeyCode::Char('o'), none, None, SwapVisualAnchor);
    bind!(VisualLine, KeyCode::Char(':'), none, None, EnterCommandMode);
    bind!(VisualLine, KeyCode::Char('r'), none, None, Action::Replace);
    bind!(VisualLine, KeyCode::Char('%'), none, None, MatchingPair);
    bind!(VisualLine, KeyCode::Char(';'), none, None, RepeatLastMotion);
    bind!(VisualLine, KeyCode::Char(','), none, None, RepeatLastMotionReversed);
    bind!(VisualLine, KeyCode::Char('p'), none, None, PasteOverSelection);
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
    // vim: `v` switches blockwise to charwise, `V` to linewise.
    bind!(VisualBlock, KeyCode::Char('v'), none, None, EnterMode(Visual));
    bind!(VisualBlock, KeyCode::Char('V'), none, None, EnterVisualLine);
    bind!(VisualBlock, KeyCode::Char('r'), none, None, Action::Replace);
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

    // Replace mode: mirror insert-mode movement plus Exit.
    bind!(Mode::Replace, KeyCode::Esc, none, None, EnterMode(Normal));
    bind!(Mode::Replace, KeyCode::Left, none, None, Edit("move_left"));
    bind!(Mode::Replace, KeyCode::Right, none, None, Edit("move_right"));
    bind!(Mode::Replace, KeyCode::Up, none, None, Edit("move_up"));
    bind!(Mode::Replace, KeyCode::Down, none, None, Edit("move_down"));
    bind!(Mode::Replace, KeyCode::Enter, none, None, Edit("insert_newline"));
    bind!(Mode::Replace, KeyCode::Backspace, none, None, DeleteBackward);

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
    // vim insert-mode keys: Ctrl-n/p completion (also navigate in the picker),
    // Ctrl-o one-shot normal, Ctrl-a re-insert last insert, Ctrl-@ same + exit,
    // Ctrl-e/y scroll without leaving insert.  Ctrl-v (literal insert) is a
    // no-op: ee inserts typed chars literally, so the default already matches.
    bind!(Insert, KeyCode::Char('n'), ctrl, None, RequestCompletion);
    bind!(Insert, KeyCode::Char('p'), ctrl, None, RequestCompletion);
    bind!(Insert, KeyCode::Char('o'), ctrl, None, OneShotNormal);
    bind!(Insert, KeyCode::Char('a'), ctrl, None, RepeatLastInsert);
    bind!(Insert, KeyCode::Char('@'), ctrl, None, RepeatLastInsertAndExit);
    bind!(Insert, KeyCode::Char('e'), ctrl, None, ScrollLines { down: true });
    bind!(Insert, KeyCode::Char('y'), ctrl, None, ScrollLines { down: false });
    bind!(Insert, KeyCode::Char('v'), ctrl, None, NoOp);
    // vim completion menu navigation (Ctrl-n next, Ctrl-p previous).
    bind!(Picker, KeyCode::Char('n'), ctrl, None, PickerMoveDown);
    bind!(Picker, KeyCode::Char('p'), ctrl, None, PickerMoveUp);

    // Command-line mode: unprefixed bindings.
    bind!(CommandLine, KeyCode::Esc, none, None, EnterMode(Normal));
    bind!(CommandLine, KeyCode::Enter, none, None, ExecuteCommand);
    bind!(CommandLine, KeyCode::Backspace, none, None, CommandBackspace);
    bind!(CommandLine, KeyCode::Tab, none, None, CompleteCommandLine);
    bind!(CommandLine, KeyCode::Up, none, None, CommandHistoryOlder);
    bind!(CommandLine, KeyCode::Down, none, None, CommandHistoryNewer);
    // vim cmdline editing: Ctrl-w delete word, Ctrl-u clear, Ctrl-h = backspace,
    // Ctrl-r insert register, Ctrl-f history window (`q:`).  Ctrl-v is a no-op
    // here: the cmdline has no magic expansions, so literal insert is already
    // the default — the binding exists so the key stays visible/rebindable.
    bind!(CommandLine, KeyCode::Char('w'), ctrl, None, CommandDeleteWord);
    bind!(CommandLine, KeyCode::Char('u'), ctrl, None, CommandClearLine);
    bind!(CommandLine, KeyCode::Char('h'), ctrl, None, CommandBackspace);
    bind!(CommandLine, KeyCode::Char('r'), ctrl, None, InsertRegister);
    bind!(CommandLine, KeyCode::Char('f'), ctrl, None, CommandHistoryWindow);
    bind!(CommandLine, KeyCode::Char('v'), ctrl, None, NoOp);

    // Search mode: unprefixed bindings.
    bind!(Search, KeyCode::Esc, none, None, EnterMode(Normal));
    bind!(Search, KeyCode::Enter, none, None, ExecuteSearch);
    bind!(Search, KeyCode::Backspace, none, None, SearchBackspace);
    bind!(Search, KeyCode::Enter, KeyModifiers::ALT, None, FindAll);
    // vim search-line editing: same word/clear/backspace/register keys.
    bind!(Search, KeyCode::Char('w'), ctrl, None, CommandDeleteWord);
    bind!(Search, KeyCode::Char('u'), ctrl, None, CommandClearLine);
    bind!(Search, KeyCode::Char('h'), ctrl, None, SearchBackspace);
    bind!(Search, KeyCode::Char('r'), ctrl, None, InsertRegister);

    // Normal mode: edit history and repeat.
    bind!(Normal, KeyCode::Char('f'), ctrl, None, Edit("scroll_page_down"));
    bind!(Normal, KeyCode::Char('b'), ctrl, None, Edit("scroll_page_up"));
    bind!(Normal, KeyCode::Char('e'), ctrl, None, ScrollLines { down: true });
    bind!(Normal, KeyCode::Char('y'), ctrl, None, ScrollLines { down: false });
    bind!(Normal, KeyCode::Char('g'), ctrl, None, FileStatus);
    bind!(Normal, KeyCode::Char('^'), ctrl, None, AlternateBuffer);
    bind!(Normal, KeyCode::Char('r'), ctrl, None, Redo);
    bind!(Normal, KeyCode::Char('.'), none, None, RepeatLastChange);
    bind!(Normal, KeyCode::Char(';'), none, None, RepeatLastMotion);
    bind!(Normal, KeyCode::Char(','), none, None, RepeatLastMotionReversed);
    bind!(Normal, KeyCode::Char('&'), none, None, RepeatSubstitute);
    bind!(Normal, KeyCode::Char('~'), none, None, ToggleCaseChars);
    bind!(Normal, KeyCode::Char('x'), none, None, DeleteCharForward);
    bind!(Normal, KeyCode::Char('X'), none, None, DeleteCharBackward);
    bind!(Normal, KeyCode::Char('r'), none, None, Action::Replace);
    bind!(Normal, KeyCode::Char('D'), none, None, DeleteToLineEnd);
    bind!(Normal, KeyCode::Char('C'), none, None, ChangeToLineEnd);
    bind!(Normal, KeyCode::Char('Y'), none, None, YankLines);
    bind!(Normal, KeyCode::Char('J'), none, None, JoinLines { select_space: true });
    bind!(Normal, KeyCode::Char('='), none, None, SetOperator(Operator::Reindent));
    bind!(Normal, KeyCode::Char('W'), none, None, MoveWordStart { forward: true, long_word: true });
    bind!(Normal, KeyCode::Char('E'), none, None, MoveWordEnd { long_word: true });
    bind!(
        Normal,
        KeyCode::Char('B'),
        none,
        None,
        MoveWordStart { forward: false, long_word: true }
    );
    bind!(Normal, KeyCode::Char('{'), none, None, GotoParagraph { forward: false });
    bind!(Normal, KeyCode::Char('}'), none, None, GotoParagraph { forward: true });
    bind!(Normal, KeyCode::Char('('), none, None, GotoSentence { forward: false });
    bind!(Normal, KeyCode::Char(')'), none, None, GotoSentence { forward: true });
    bind!(Normal, KeyCode::Char('|'), none, None, GotoColumn);
    bind!(
        Normal,
        KeyCode::Char('+'),
        none,
        None,
        GotoLineFirstNonBlank { down: true, zero_based: false }
    );
    bind!(
        Normal,
        KeyCode::Char('-'),
        none,
        None,
        GotoLineFirstNonBlank { down: false, zero_based: false }
    );
    bind!(
        Normal,
        KeyCode::Char('_'),
        none,
        None,
        GotoLineFirstNonBlank { down: true, zero_based: true }
    );
    bind!(
        Normal,
        KeyCode::Enter,
        none,
        None,
        GotoLineFirstNonBlank { down: true, zero_based: false }
    );
    bind!(Normal, KeyCode::Backspace, none, None, Edit("move_left"));
    bind!(Normal, KeyCode::Char('H'), none, None, GotoWindowTop);
    bind!(Normal, KeyCode::Char('M'), none, None, GotoWindowCenter);
    bind!(Normal, KeyCode::Char('L'), none, None, GotoWindowBottom);
    bind!(Normal, KeyCode::Char('z'), none, Some('z'), ViewCenterCursor);
    bind!(Normal, KeyCode::Char('t'), none, Some('z'), ViewTopCursor);
    bind!(Normal, KeyCode::Char('b'), none, Some('z'), ViewBottomCursor);

    // Normal and visual mode: paste.
    bind!(Normal, KeyCode::Char('p'), none, None, PasteAfter);
    bind!(Normal, KeyCode::Char('P'), none, None, PasteBefore);

    map
}
