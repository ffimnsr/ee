// Vim-parity default bindings: `w`/`e`/`ge`/`gd`/`gD`/`go`/`Ctrl-d`/`Ctrl-u`/`Ctrl-c` and the
// normal-mode extras (`x X r R ~ D C Y J = ; , & W E B { } ( ) | + - _ H M L zz gt zx gp gP` etc.).
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use std::sync::mpsc;

use crate::app::{App, Mode, Operator};
use crate::keymap::{
    Action, BindingKey, KeymapSettings, bindings_for, format_action_spec, parse_action_spec,
};

fn normal() -> KeymapSettings {
    KeymapSettings::default()
}

fn key(mode: Mode, key: KeyCode, modifiers: KeyModifiers, prefix: Option<char>) -> BindingKey {
    BindingKey { mode, key, modifiers, prefix }
}

#[test]
fn w_and_e_use_vim_word_parsers() {
    let bindings = bindings_for(&normal());
    assert_eq!(
        bindings.get(&key(Mode::Normal, KeyCode::Char('w'), KeyModifiers::NONE, None)),
        Some(&Action::MoveWordStart { forward: true, long_word: false })
    );
    assert_eq!(
        bindings.get(&key(Mode::Normal, KeyCode::Char('e'), KeyModifiers::NONE, None)),
        Some(&Action::MoveWordEnd { long_word: false })
    );
    // Visual mode extends selection with the same vim word parsers.
    assert_eq!(
        bindings.get(&key(Mode::Visual, KeyCode::Char('w'), KeyModifiers::NONE, None)),
        Some(&Action::MoveWordStart { forward: true, long_word: false })
    );
    assert_eq!(
        bindings.get(&key(Mode::Visual, KeyCode::Char('e'), KeyModifiers::NONE, None)),
        Some(&Action::MoveWordEnd { long_word: false })
    );
}

#[test]
fn g_prefix_matches_vim_semantics() {
    let bindings = bindings_for(&normal());
    let g = Some('g');
    assert_eq!(
        bindings.get(&key(Mode::Normal, KeyCode::Char('e'), KeyModifiers::NONE, g)),
        Some(&Action::MoveWordEndBackward { long_word: false })
    );
    assert_eq!(
        bindings.get(&key(Mode::Normal, KeyCode::Char('d'), KeyModifiers::NONE, g)),
        Some(&Action::RequestDeclaration)
    );
    assert_eq!(
        bindings.get(&key(Mode::Normal, KeyCode::Char('D'), KeyModifiers::NONE, g)),
        Some(&Action::RequestDefinition)
    );
    assert_eq!(
        bindings.get(&key(Mode::Normal, KeyCode::Char('o'), KeyModifiers::NONE, g)),
        Some(&Action::GotoByte)
    );
}

#[test]
fn ctrl_d_and_ctrl_u_scroll_half_page() {
    let bindings = bindings_for(&normal());
    assert_eq!(
        bindings.get(&key(Mode::Normal, KeyCode::Char('d'), KeyModifiers::CONTROL, None)),
        Some(&Action::PageCursorHalfDown)
    );
    assert_eq!(
        bindings.get(&key(Mode::Normal, KeyCode::Char('u'), KeyModifiers::CONTROL, None)),
        Some(&Action::PageCursorHalfUp)
    );
}

#[test]
fn normal_mode_extras_are_bound() {
    let bindings = bindings_for(&normal());
    let n = |k: KeyCode, p: Option<char>| key(Mode::Normal, k, KeyModifiers::NONE, p);
    let ctrl = |k: KeyCode| key(Mode::Normal, k, KeyModifiers::CONTROL, None);
    let g = Some('g');
    let z = Some('z');

    assert_eq!(bindings.get(&n(KeyCode::Char('x'), None)), Some(&Action::DeleteCharForward));
    assert_eq!(bindings.get(&n(KeyCode::Char('X'), None)), Some(&Action::DeleteCharBackward));
    assert_eq!(bindings.get(&n(KeyCode::Char('r'), None)), Some(&Action::Replace));
    assert_eq!(bindings.get(&n(KeyCode::Char('R'), None)), Some(&Action::EnterMode(Mode::Replace)));
    assert_eq!(bindings.get(&n(KeyCode::Char('~'), None)), Some(&Action::ToggleCaseChars));
    assert_eq!(bindings.get(&n(KeyCode::Char('D'), None)), Some(&Action::DeleteToLineEnd));
    assert_eq!(bindings.get(&n(KeyCode::Char('C'), None)), Some(&Action::ChangeToLineEnd));
    assert_eq!(bindings.get(&n(KeyCode::Char('Y'), None)), Some(&Action::YankLines));
    assert_eq!(
        bindings.get(&n(KeyCode::Char('J'), None)),
        Some(&Action::JoinLines { select_space: true })
    );
    assert_eq!(
        bindings.get(&n(KeyCode::Char('='), None)),
        Some(&Action::SetOperator(Operator::Reindent))
    );
    assert_eq!(bindings.get(&n(KeyCode::Char(';'), None)), Some(&Action::RepeatLastMotion));
    assert_eq!(bindings.get(&n(KeyCode::Char(','), None)), Some(&Action::RepeatLastMotionReversed));
    assert_eq!(bindings.get(&n(KeyCode::Char('&'), None)), Some(&Action::RepeatSubstitute));
    assert_eq!(
        bindings.get(&n(KeyCode::Char('W'), None)),
        Some(&Action::MoveWordStart { forward: true, long_word: true })
    );
    assert_eq!(
        bindings.get(&n(KeyCode::Char('E'), None)),
        Some(&Action::MoveWordEnd { long_word: true })
    );
    assert_eq!(
        bindings.get(&n(KeyCode::Char('B'), None)),
        Some(&Action::MoveWordStart { forward: false, long_word: true })
    );
    assert_eq!(
        bindings.get(&n(KeyCode::Char('{'), None)),
        Some(&Action::GotoParagraph { forward: false })
    );
    assert_eq!(
        bindings.get(&n(KeyCode::Char('}'), None)),
        Some(&Action::GotoParagraph { forward: true })
    );
    assert_eq!(
        bindings.get(&n(KeyCode::Char('('), None)),
        Some(&Action::GotoSentence { forward: false })
    );
    assert_eq!(
        bindings.get(&n(KeyCode::Char(')'), None)),
        Some(&Action::GotoSentence { forward: true })
    );
    assert_eq!(bindings.get(&n(KeyCode::Char('|'), None)), Some(&Action::GotoColumn));
    assert_eq!(
        bindings.get(&n(KeyCode::Char('+'), None)),
        Some(&Action::GotoLineFirstNonBlank { down: true, zero_based: false })
    );
    assert_eq!(
        bindings.get(&n(KeyCode::Char('-'), None)),
        Some(&Action::GotoLineFirstNonBlank { down: false, zero_based: false })
    );
    assert_eq!(
        bindings.get(&n(KeyCode::Char('_'), None)),
        Some(&Action::GotoLineFirstNonBlank { down: true, zero_based: true })
    );
    assert_eq!(
        bindings.get(&n(KeyCode::Enter, None)),
        Some(&Action::GotoLineFirstNonBlank { down: true, zero_based: false })
    );
    assert_eq!(bindings.get(&n(KeyCode::Backspace, None)), Some(&Action::Edit("move_left")));
    assert_eq!(bindings.get(&n(KeyCode::Char('H'), None)), Some(&Action::GotoWindowTop));
    assert_eq!(bindings.get(&n(KeyCode::Char('M'), None)), Some(&Action::GotoWindowCenter));
    assert_eq!(bindings.get(&n(KeyCode::Char('L'), None)), Some(&Action::GotoWindowBottom));
    assert_eq!(bindings.get(&n(KeyCode::Char('z'), z)), Some(&Action::ViewCenterCursor));
    assert_eq!(bindings.get(&n(KeyCode::Char('t'), z)), Some(&Action::ViewTopCursor));
    assert_eq!(bindings.get(&n(KeyCode::Char('b'), z)), Some(&Action::ViewBottomCursor));
    assert_eq!(bindings.get(&ctrl(KeyCode::Char('f'))), Some(&Action::Edit("scroll_page_down")));
    assert_eq!(bindings.get(&ctrl(KeyCode::Char('b'))), Some(&Action::Edit("scroll_page_up")));
    assert_eq!(bindings.get(&ctrl(KeyCode::Char('e'))), Some(&Action::ScrollLines { down: true }));
    assert_eq!(bindings.get(&ctrl(KeyCode::Char('y'))), Some(&Action::ScrollLines { down: false }));
    assert_eq!(bindings.get(&ctrl(KeyCode::Char('g'))), Some(&Action::FileStatus));
    assert_eq!(bindings.get(&ctrl(KeyCode::Char('^'))), Some(&Action::AlternateBuffer));
    assert_eq!(
        bindings.get(&n(KeyCode::Char('J'), g)),
        Some(&Action::JoinLines { select_space: false })
    );
    assert_eq!(
        bindings.get(&n(KeyCode::Char('E'), g)),
        Some(&Action::MoveWordEndBackward { long_word: true })
    );
    assert_eq!(bindings.get(&n(KeyCode::Char('i'), g)), Some(&Action::InsertAtLastEdit));
    assert_eq!(bindings.get(&n(KeyCode::Char('I'), g)), Some(&Action::InsertAtColumnZero));
    assert_eq!(bindings.get(&n(KeyCode::Char('p'), g)), Some(&Action::PasteAfter));
    assert_eq!(bindings.get(&n(KeyCode::Char('P'), g)), Some(&Action::PasteBefore));
    assert_eq!(bindings.get(&n(KeyCode::Char('q'), g)), Some(&Action::FormatSelections));
    assert_eq!(bindings.get(&n(KeyCode::Char('w'), g)), Some(&Action::FormatSelections));
    assert_eq!(bindings.get(&n(KeyCode::Char('x'), g)), Some(&Action::OpenTargetUnderCursor));
    assert_eq!(bindings.get(&n(KeyCode::Char('8'), g)), Some(&Action::HexDumpChar));
    assert_eq!(bindings.get(&n(KeyCode::Char('_'), g)), Some(&Action::GotoLineLastNonBlank));
    assert_eq!(
        bindings.get(&n(KeyCode::Char('*'), g)),
        Some(&Action::SearchWordUnderCursorLoose { forward: true })
    );
    assert_eq!(
        bindings.get(&n(KeyCode::Char('#'), g)),
        Some(&Action::SearchWordUnderCursorLoose { forward: false })
    );
    assert_eq!(bindings.get(&n(KeyCode::Char('n'), g)), Some(&Action::FindNext));
    assert_eq!(bindings.get(&n(KeyCode::Char('N'), g)), Some(&Action::FindPrevious));
    // `]c`/`[c` are git-hunk aliases.
    assert_eq!(bindings.get(&n(KeyCode::Char('c'), Some(']'))), Some(&Action::GitNextHunk));
    assert_eq!(bindings.get(&n(KeyCode::Char('c'), Some('['))), Some(&Action::GitPrevHunk));
}

#[test]
fn visual_mode_bindings_follow_vim() {
    let bindings = bindings_for(&normal());
    let v = |k: KeyCode, p: Option<char>| key(Mode::Visual, k, KeyModifiers::NONE, p);
    let vl = |k: KeyCode, p: Option<char>| key(Mode::VisualLine, k, KeyModifiers::NONE, p);
    let vb = |k: KeyCode, p: Option<char>| key(Mode::VisualBlock, k, KeyModifiers::NONE, p);

    // Charwise: `v` exits (vim same-key rule), `V`/`Ctrl-v` switch to line/block.
    assert_eq!(bindings.get(&v(KeyCode::Char('v'), None)), Some(&Action::CollapseAndEnterNormal));
    assert_eq!(bindings.get(&v(KeyCode::Char('V'), None)), Some(&Action::EnterVisualLine));
    assert_eq!(
        bindings.get(&key(Mode::Visual, KeyCode::Char('v'), KeyModifiers::CONTROL, None)),
        Some(&Action::EnterVisualBlock)
    );
    // Linewise: `v` switches to charwise, `V` exits, Ctrl-v to block.
    assert_eq!(bindings.get(&vl(KeyCode::Char('v'), None)), Some(&Action::EnterMode(Mode::Visual)));
    assert_eq!(bindings.get(&vl(KeyCode::Char('V'), None)), Some(&Action::CollapseAndEnterNormal));
    assert_eq!(
        bindings.get(&key(Mode::VisualLine, KeyCode::Char('v'), KeyModifiers::CONTROL, None)),
        Some(&Action::EnterVisualBlock)
    );
    assert_eq!(bindings.get(&v(KeyCode::Char('^'), None)), Some(&Action::GotoFirstNonWhitespace));
    assert_eq!(bindings.get(&v(KeyCode::Char('%'), None)), Some(&Action::MatchingPair));
    assert_eq!(bindings.get(&v(KeyCode::Char(';'), None)), Some(&Action::RepeatLastMotion));
    assert_eq!(bindings.get(&v(KeyCode::Char(','), None)), Some(&Action::RepeatLastMotionReversed));
    assert_eq!(bindings.get(&v(KeyCode::Char('r'), None)), Some(&Action::Replace));
    assert_eq!(bindings.get(&v(KeyCode::Char('p'), None)), Some(&Action::PasteOverSelection));
    // Linewise gets the same action set.
    assert_eq!(bindings.get(&vl(KeyCode::Char('r'), None)), Some(&Action::Replace));
    assert_eq!(bindings.get(&vl(KeyCode::Char('%'), None)), Some(&Action::MatchingPair));
    assert_eq!(bindings.get(&vl(KeyCode::Char('p'), None)), Some(&Action::PasteOverSelection));
    // Block: `r` replace, `o`/`O` handled by the block char handler, mode
    // switches to charwise/linewise on `v`/`V`, Ctrl-v exits.
    assert_eq!(bindings.get(&vb(KeyCode::Char('r'), None)), Some(&Action::Replace));
    assert_eq!(bindings.get(&vb(KeyCode::Char('o'), None)), None);
    assert_eq!(bindings.get(&vb(KeyCode::Char('v'), None)), Some(&Action::EnterMode(Mode::Visual)));
    assert_eq!(bindings.get(&vb(KeyCode::Char('V'), None)), Some(&Action::EnterVisualLine));
    assert_eq!(
        bindings.get(&key(Mode::VisualBlock, KeyCode::Char('v'), KeyModifiers::CONTROL, None)),
        Some(&Action::CollapseAndEnterNormal)
    );
}

#[test]
fn visual_s_changes_selection() {
    let (_temp, mut app) = open_text_file("abc");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)));
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "selection extended",
        std::time::Duration::from_secs(15),
        |backend| backend.cursor_col == 1,
    );
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Insert);
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    // Half-open selection: `s` changes only the anchor char.
    wait_for_line(&mut app, "Xbc");
}

#[test]
fn visual_p_replaces_selection_with_register() {
    let (_temp, mut app) = open_text_file("abc");
    app.registers.yank(&crate::registers::RegisterName::Unnamed, String::from("ZZ"), false);
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)));
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "selection extended",
        std::time::Duration::from_secs(15),
        |backend| backend.cursor_col == 1,
    );
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)));
    // Backend selections are half-open: `vl` selects the char under the anchor
    // only, so the replacement leaves "bc" intact.
    wait_for_line(&mut app, "ZZbc");
    assert_eq!(app.mode, Mode::Normal);
    // vim: visual `p` swaps the replaced text into the unnamed register.
    assert_eq!(app.registers.get(&crate::registers::RegisterName::Unnamed), "a");
}

#[test]
fn visual_tilde_toggles_selection_case() {
    let (_temp, mut app) = open_text_file("abc");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)));
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "selection extended",
        std::time::Duration::from_secs(15),
        |backend| backend.cursor_col == 1,
    );
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('~'), KeyModifiers::NONE)));
    // Half-open selection: only the anchor char is toggled.
    wait_for_line(&mut app, "Abc");
}

#[test]
fn visual_counts_extend_motions() {
    let (_temp, mut app) = open_text_file("ab cd ef");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE)));
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "two words extended",
        std::time::Duration::from_secs(15),
        |backend| backend.cursor_col == 6,
    );
    // Count resets after the motion (no `3w` on the next `w`).
    assert!(app.input_state.count_digits.is_empty());
    assert_eq!(app.mode, Mode::Visual);
}

#[test]
fn visual_block_o_swaps_horizontal_corner() {
    let (_temp, mut app) = open_text_file("ab\ncd");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)));
    // Extend the block down-right.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "block extended",
        std::time::Duration::from_secs(15),
        |backend| backend.cursor_line == 1 && backend.cursor_col == 1,
    );
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE)));
    // Anchor keeps its row, column swaps to the cursor's column.
    assert_eq!(app.visual_anchor, Some((0, 1)));
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "corner swapped",
        std::time::Duration::from_secs(15),
        |backend| (backend.cursor_line, backend.cursor_col) == (1, 0),
    );
}

#[test]
fn visual_block_tilde_toggles_case_in_block() {
    let (_temp, mut app) = open_text_file("ab\ncd");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "block extended",
        std::time::Duration::from_secs(15),
        |backend| backend.cursor_line == 1 && backend.cursor_col == 1,
    );
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('~'), KeyModifiers::NONE)));
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "block cased",
        std::time::Duration::from_secs(15),
        |backend| backend.get_line(0) == Some("Ab") && backend.get_line(1) == Some("Cd"),
    );
}

#[test]
fn visual_block_r_replaces_block_columns() {
    let (_temp, mut app) = open_text_file("ab\ncd");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "block extended",
        std::time::Duration::from_secs(15),
        |backend| backend.cursor_line == 1 && backend.cursor_col == 1,
    );
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)));
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "block replaced",
        std::time::Duration::from_secs(15),
        |backend| backend.get_line(0) == Some("xb") && backend.get_line(1) == Some("xd"),
    );
}

#[test]
fn visual_block_shift_o_swaps_vertical_corner() {
    let (_temp, mut app) = open_text_file("ab\ncd");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "block extended",
        std::time::Duration::from_secs(15),
        |backend| backend.cursor_line == 1 && backend.cursor_col == 1,
    );
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('O'), KeyModifiers::NONE)));
    // `O` swaps the vertical corner: anchor picks up the cursor row, the
    // cursor jumps to the anchor row on the same column.
    assert_eq!(app.visual_anchor, Some((1, 0)));
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "corner swapped",
        std::time::Duration::from_secs(15),
        |backend| (backend.cursor_line, backend.cursor_col) == (0, 1),
    );
}

#[test]
fn visual_block_dollar_extends_to_longest_line() {
    let (_temp, mut app) = open_text_file("ab\ncdef");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "block extended",
        std::time::Duration::from_secs(15),
        |backend| backend.cursor_line == 1,
    );
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('$'), KeyModifiers::NONE)));
    // Right edge extends to the longest line (4 chars); cursor keeps its row.
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "right edge extended",
        std::time::Duration::from_secs(15),
        |backend| (backend.cursor_line, backend.cursor_col) == (1, 4),
    );
}

#[test]
fn visual_r_replaces_multiline_selection_keeping_newlines() {
    let (_temp, mut app) = open_text_file("ab\ncd");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "multi-line selection",
        std::time::Duration::from_secs(15),
        |backend| backend.cursor_line == 1,
    );
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)));
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "replaced per line",
        std::time::Duration::from_secs(15),
        |backend| backend.get_line(0) == Some("xx") && backend.get_line(1) == Some("cd"),
    );
}

#[test]
fn visual_block_d_deletes_block_columns() {
    let (_temp, mut app) = open_text_file("ab\ncd");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "block extended",
        std::time::Duration::from_secs(15),
        |backend| backend.cursor_line == 1 && backend.cursor_col == 1,
    );
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "block columns deleted",
        std::time::Duration::from_secs(15),
        |backend| backend.get_line(0) == Some("b") && backend.get_line(1) == Some("d"),
    );
}

#[test]
fn visual_block_c_changes_block_columns() {
    let (_temp, mut app) = open_text_file("ab\ncd");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "block extended",
        std::time::Duration::from_secs(15),
        |backend| backend.cursor_line == 1 && backend.cursor_col == 1,
    );
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Insert);
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "block columns changed",
        std::time::Duration::from_secs(15),
        |backend| backend.get_line(0) == Some("b") && backend.get_line(1) == Some("d"),
    );
}

#[test]
fn visual_block_y_yanks_block_columns() {
    let (_temp, mut app) = open_text_file("ab\ncd");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "block extended",
        std::time::Duration::from_secs(15),
        |backend| backend.cursor_line == 1 && backend.cursor_col == 1,
    );
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
    // Block preview includes each line's trailing newline.
    assert_eq!(app.registers.get(&crate::registers::RegisterName::Unnamed), "a\nc\n");
}

#[test]
fn visual_line_d_deletes_full_lines() {
    let (_temp, mut app) = open_text_file("ab\ncd");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('V'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "line selection extended",
        std::time::Duration::from_secs(15),
        |backend| backend.cursor_line == 1,
    );
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "full lines deleted",
        std::time::Duration::from_secs(15),
        |backend| backend.line_count() == 0,
    );
}

#[test]
fn visual_shift_s_changes_whole_lines() {
    let (_temp, mut app) = open_text_file("abc");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)));
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "selection extended",
        std::time::Duration::from_secs(15),
        |backend| backend.cursor_col == 1,
    );
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Insert);
    // Backend "end of line" includes the newline: the whole line is removed.
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "whole line changed",
        std::time::Duration::from_secs(15),
        |backend| backend.line_count() == 0,
    );
}

#[test]
fn visual_shift_d_deletes_to_end_of_line() {
    let (_temp, mut app) = open_text_file("ab cd");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)));
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "selection extended",
        std::time::Duration::from_secs(15),
        |backend| backend.cursor_col == 1,
    );
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
    // Backend "end of line" includes the newline: the whole line is removed.
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "to EOL deleted",
        std::time::Duration::from_secs(15),
        |backend| backend.line_count() == 0,
    );
}

#[test]
fn visual_shift_x_deletes_to_line_start() {
    let (_temp, mut app) = open_text_file("ab cd");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "to line start deleted",
        std::time::Duration::from_secs(15),
        |backend| backend.get_line(0) == Some(" cd"),
    );
}

#[test]
fn visual_shift_j_joins_selected_lines() {
    let (_temp, mut app) = open_text_file("ab\ncd");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "selection extended",
        std::time::Duration::from_secs(15),
        |backend| backend.cursor_line == 1,
    );
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "lines joined",
        std::time::Duration::from_secs(15),
        |backend| backend.get_line(0) == Some("ab cd"),
    );
}

#[test]
fn replace_mode_has_bindings() {
    let bindings = bindings_for(&normal());
    assert_eq!(
        bindings.get(&key(Mode::Replace, KeyCode::Esc, KeyModifiers::NONE, None)),
        Some(&Action::EnterMode(Mode::Normal))
    );
    assert_eq!(
        bindings.get(&key(Mode::Replace, KeyCode::Char('c'), KeyModifiers::CONTROL, None)),
        Some(&Action::EnterMode(Mode::Normal))
    );
    assert_eq!(
        bindings.get(&key(Mode::Replace, KeyCode::Enter, KeyModifiers::NONE, None)),
        Some(&Action::Edit("insert_newline"))
    );
}

fn open_text_file(text: &str) -> (tempfile::TempDir, App) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("sample.txt");
    std::fs::write(&path, text).unwrap();
    let mut app = App::from_path(Some(path)).unwrap();
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "line loaded",
        std::time::Duration::from_secs(15),
        |backend| backend.line_count() > 0,
    );
    (temp, app)
}

fn wait_for_line(app: &mut App, expected: &str) {
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "buffer updated",
        std::time::Duration::from_secs(15),
        |backend| backend.get_line(0).map(|l| l == expected).unwrap_or(false),
    );
}

#[test]
fn x_deletes_char_and_yanks_unnamed_register() {
    let (_temp, mut app) = open_text_file("abcd");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)));
    wait_for_line(&mut app, "bcd");
    assert_eq!(app.registers.get(&crate::registers::RegisterName::Unnamed), "a");
}

#[test]
fn x_respects_count() {
    let (_temp, mut app) = open_text_file("abcdef");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)));
    wait_for_line(&mut app, "def");
}

#[test]
fn tilde_toggles_case_of_char_under_cursor() {
    let (_temp, mut app) = open_text_file("abc");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('~'), KeyModifiers::NONE)));
    wait_for_line(&mut app, "Abc");
}

#[test]
fn replace_mode_overwrites_chars() {
    let (_temp, mut app) = open_text_file("abcd");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Replace);
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    wait_for_line(&mut app, "Zbcd");
    assert_eq!(app.mode, Mode::Normal);
}

fn test_app_with_lines(lines: &[&str]) -> App {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = crate::buffer::BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = lines.iter().map(|s| s.to_string()).collect();
    let _ = rx;
    app
}

#[test]
fn paragraph_scanner_lands_on_first_nonblank_after_blank_run() {
    let app = test_app_with_lines(&["a", "b", "", "c", "d", "", "e", "f"]);
    // Forward: from paragraph A, `}` lands after the blank run (line 3).
    assert_eq!(app.scan_paragraph_boundary(0, true), Some(3));
    assert_eq!(app.scan_paragraph_boundary(1, true), Some(3));
    // Forward from paragraph B lands in paragraph C.
    assert_eq!(app.scan_paragraph_boundary(4, true), Some(6));
    // Backward: from the middle of paragraph C lands at its start.
    assert_eq!(app.scan_paragraph_boundary(6, false), Some(6));
    assert_eq!(app.scan_paragraph_boundary(7, false), Some(6));
    // From the middle of paragraph B lands at its start.
    assert_eq!(app.scan_paragraph_boundary(3, false), Some(3));
    assert_eq!(app.scan_paragraph_boundary(4, false), Some(3));
    // Buffer edges.
    assert_eq!(app.scan_paragraph_boundary(0, false), None);
    assert_eq!(app.scan_paragraph_boundary(7, true), None);
}

#[test]
fn sentence_scanner_finds_boundaries() {
    let app = test_app_with_lines(&["One. Two.", "Three?"]);
    // Forward from the start lands after the first sentence.
    assert_eq!(app.scan_sentence_boundary(0, 0, true), Some((0, 5)));
    // Forward at the end of the line continues on the next line.
    assert_eq!(app.scan_sentence_boundary(0, 6, true), Some((1, 0)));
    // Backward from the next line lands at the start of the previous sentence's
    // following text (line start).
    assert_eq!(app.scan_sentence_boundary(1, 0, false), Some((1, 0)));
    // Backward from the middle of the second sentence lands after "One.".
    assert_eq!(app.scan_sentence_boundary(0, 8, false), Some((0, 5)));
}

#[test]
fn x_deletes_multibyte_char_backward() {
    // `X` on a multibyte char must not panic or split the char (regression:
    // the original walk could underflow on non-char-boundary columns).
    let (_temp, mut app) = open_text_file("a\u{e9}b");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)));
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "cursor on b",
        std::time::Duration::from_secs(15),
        |backend| backend.cursor_col == 3,
    );
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE)));
    wait_for_line(&mut app, "ab");
    assert_eq!(app.registers.get(&crate::registers::RegisterName::Unnamed), "\u{e9}");
}

#[test]
fn y_and_yy_yank_line_without_deleting() {
    let (_temp, mut app) = open_text_file("a\nb");
    // `Y` then `yy`: both must leave the buffer untouched and fill the register.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::NONE)));
    let unnamed = app.registers.get(&crate::registers::RegisterName::Unnamed);
    assert!(unnamed == "a" || unnamed == "a\n", "register content: {unnamed:?}");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)));
    let unnamed = app.registers.get(&crate::registers::RegisterName::Unnamed);
    assert!(unnamed == "a" || unnamed == "a\n", "register content: {unnamed:?}");
    assert_eq!(app.backend.get_line(0), Some("a"));
    assert_eq!(app.backend.get_line(1), Some("b"));
}

#[test]
fn j_joins_lines_with_space() {
    let mut app = App::from_path(None).unwrap();
    // Seed by typing like the existing join ex-command test (manual pump in
    // tests; no background pump thread).
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    for ch in "a\nb".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "seed join buffer",
        std::time::Duration::from_secs(15),
        |backend| backend.lines == vec![String::from("a"), String::from("b")],
    );
    // Move to the first line: joining a caret on the last line is a no-op.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE)));
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "cursor on first line",
        std::time::Duration::from_secs(15),
        |backend| backend.cursor_line == 0,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::NONE)));
    wait_for_line(&mut app, "a b");
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "lines merged",
        std::time::Duration::from_secs(15),
        |backend| backend.lines == vec![String::from("a b")],
    );
}

#[test]
fn ctrl_e_scrolls_view_one_line() {
    use std::time::Duration;

    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = crate::buffer::BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.viewport.top_line = 10;
    app.last_editor_height = 24;

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL)));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut saw_scroll = false;
    while std::time::Instant::now() < deadline && !saw_scroll {
        if let Ok(message) = rx.recv_timeout(Duration::from_millis(50)) {
            let value: serde_json::Value = serde_json::from_str(&message).unwrap();
            if value["params"]["method"] == "scroll" {
                assert_eq!(value["params"]["params"]["first"], 11);
                saw_scroll = true;
            }
        }
    }
    assert!(saw_scroll, "expected a scroll edit to be sent");
}

#[test]
fn move_word_end_backward_action_roundtrips_through_spec() {
    let spec = "move_word_end_backward:word";
    assert_eq!(parse_action_spec(spec).unwrap(), Action::MoveWordEndBackward { long_word: false });
    assert_eq!(
        format_action_spec(&Action::MoveWordEndBackward { long_word: true }),
        "move_word_end_backward:long_word"
    );
}

#[test]
fn goto_byte_action_roundtrips_through_spec() {
    assert_eq!(parse_action_spec("goto_byte").unwrap(), Action::GotoByte);
    assert_eq!(format_action_spec(&Action::GotoByte), "goto_byte");
}

#[test]
fn ctrl_c_cancels_operator_pending_like_escape() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::OperatorPending);
    assert_eq!(app.input_state.pending_operator, Some(Operator::Delete));

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.input_state.pending_operator, None);
}

#[test]
fn ctrl_c_cancels_pending_char_find_and_register() {
    let mut app = App::from_path(None).unwrap();
    // Pending `f` find.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)));
    assert!(app.input_state.pending_find.is_some());
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    assert!(app.input_state.pending_find.is_none());

    // Pending register prefix.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('"'), KeyModifiers::NONE)));
    assert!(app.input_state.awaiting_register);
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    assert!(!app.input_state.awaiting_register);
}

#[test]
fn page_cursor_half_uses_count_when_present() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("sample.txt");
    std::fs::write(&path, (0..100).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n"))
        .unwrap();

    let mut app = App::from_path(Some(path)).unwrap();
    crate::tests::helpers::wait_until_with_backend(
        &mut app.backend,
        "lines loaded",
        std::time::Duration::from_secs(15),
        |backend| backend.line_count() > 90,
    );
    app.viewport.top_line = 40;

    // `3` then `Ctrl-d` = scroll 3 lines (vim count semantics).
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)));

    assert_eq!(app.viewport.top_line, 43);
}

#[test]
fn word_motions_apply_count() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = crate::buffer::BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    // `3w` must emit three word-start motions. Messages are relayed through a
    // test thread, so poll until the expected count arrives.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE)));

    let word_starts =
        drain_word_motion_count(&rx, "move_word_start", 3, std::time::Duration::from_secs(5));
    assert_eq!(word_starts, 3);

    // `2e` must emit two word-end motions.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE)));
    assert!(app.input_state.count_digits == [2u8]);
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)));
    assert!(app.input_state.count_digits.is_empty(), "count reset after dispatch");

    let word_ends =
        drain_word_motion_count(&rx, "move_word_end", 2, std::time::Duration::from_secs(5));
    assert_eq!(word_ends, 2);
}

fn drain_word_motion_count(
    rx: &std::sync::mpsc::Receiver<String>,
    method: &str,
    expected: usize,
    timeout: std::time::Duration,
) -> usize {
    let deadline = std::time::Instant::now() + timeout;
    let mut count = 0;
    while count < expected && std::time::Instant::now() < deadline {
        match rx.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(message) => {
                let value: serde_json::Value = serde_json::from_str(&message).unwrap();
                if value["params"]["method"] == method {
                    count += 1;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    count
}
