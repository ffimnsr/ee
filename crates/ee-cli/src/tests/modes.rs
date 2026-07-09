//! Mode transition tests: Normal, Insert, Visual, Operator-pending, and
//! text-object helper functions.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use crate::app::{
    App, Mode, Operator, PendingCharFind, text_obj_bracket, text_obj_quote, text_obj_tag,
    text_obj_word,
};
use crate::keymap::{Action, BindingKey};
use crate::tests::helpers::*;
use xi_core_lib::plugin_rpc::SelectionRange;

// ── Normal/Insert mode transitions ──────────────────────────────────────

#[test]
fn normal_mode_alias_returns_from_insert() {
    let mut app = App::from_path(None).unwrap();
    app.mode = Mode::Insert;
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Insert,
            key: KeyCode::Char('n'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::EnterMode(Mode::Normal),
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::ALT)));

    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn change_selection_alias_enters_insert_mode() {
    let mut app = App::from_path(None).unwrap();
    insert_text(&mut app, "abc");
    app.backend.pump().unwrap();
    app.backend
        .set_selections(&[SelectionRange { start: 0, end: 0 }])
        .expect("set selections should succeed");
    app.backend.pump().unwrap();
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('c'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::DeleteSelection { yank: true, enter_insert: true },
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::ALT)));

    assert_eq!(app.mode, Mode::Insert);
}

#[test]
fn no_op_action_leaves_mode_unchanged() {
    let mut app = App::from_path(None).unwrap();
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('z'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::NoOp,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::ALT)));

    assert_eq!(app.mode, Mode::Normal);
}

// ── Operator-pending mode ───────────────────────────────────────────────

#[test]
fn d_enters_operator_pending_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::OperatorPending);
    assert_eq!(app.input_state.pending_operator, Some(Operator::Delete));
}

#[test]
fn c_enters_operator_pending_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::OperatorPending);
    assert_eq!(app.input_state.pending_operator, Some(Operator::Change));
}

#[test]
fn y_enters_operator_pending_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::OperatorPending);
    assert_eq!(app.input_state.pending_operator, Some(Operator::Yank));
}

#[test]
fn indent_operator_enters_operator_pending() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('>'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::OperatorPending);
    assert_eq!(app.input_state.pending_operator, Some(Operator::Indent));
}

#[test]
fn escape_cancels_operator_pending() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::OperatorPending);
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.input_state.pending_operator, None);
}

#[test]
fn operator_pending_motion_returns_to_normal() {
    let mut app = App::from_path(None).unwrap();
    // d + w → sends motion + delete and returns to normal
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.input_state.pending_operator, None);
}

#[test]
fn change_operator_motion_enters_insert() {
    let mut app = App::from_path(None).unwrap();
    // c + w → enters insert mode after deletion
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Insert);
}

#[test]
fn double_d_applies_to_line() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.input_state.pending_operator, None);
}

#[test]
fn double_c_enters_insert() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Insert);
}

#[test]
fn g_lowercase_u_sets_lowercase_operator() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::OperatorPending);
    assert_eq!(app.input_state.pending_operator, Some(Operator::Lowercase));
}

#[test]
fn operator_text_object_prefix_i_sets_inner() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    // Still in operator-pending waiting for text object specifier
    assert_eq!(app.mode, Mode::OperatorPending);
    assert_eq!(app.input_state.text_obj_inclusive, Some(false));
}

#[test]
fn operator_text_object_prefix_a_sets_outer() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::OperatorPending);
    assert_eq!(app.input_state.text_obj_inclusive, Some(true));
}

#[test]
fn operator_text_object_unknown_specifier_cancels() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    // Unknown text object specifier → cancel
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn operator_f_sets_pending_find_in_operator_pending() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::OperatorPending);
    assert_eq!(
        app.input_state.pending_find,
        Some(PendingCharFind { forward: true, inclusive: true })
    );
}

// ── Text object helpers ─────────────────────────────────────────────────

#[test]
fn word_obj_inner_finds_word_boundaries() {
    // "hello world", cursor on 'h' (byte 0)
    let (start, end) = text_obj_word("hello world", 0, false, false).unwrap();
    assert_eq!(&"hello world"[start..end], "hello");
}

#[test]
fn word_obj_inner_mid_word() {
    // cursor on 'l' at byte 2
    let (start, end) = text_obj_word("hello world", 2, false, false).unwrap();
    assert_eq!(&"hello world"[start..end], "hello");
}

#[test]
fn word_obj_outer_includes_trailing_space() {
    let (start, end) = text_obj_word("hello world", 0, true, false).unwrap();
    assert_eq!(&"hello world"[start..end], "hello ");
}

#[test]
fn word_obj_not_on_word_char_returns_none() {
    // cursor on space
    assert!(text_obj_word("hello world", 5, false, false).is_none());
}

#[test]
fn quote_obj_inner_finds_content() {
    let (start, end) = text_obj_quote("say \"hello\" here", 5, '"', false).unwrap();
    assert_eq!(&"say \"hello\" here"[start..end], "hello");
}

#[test]
fn quote_obj_outer_includes_quotes() {
    let (start, end) = text_obj_quote("say \"hello\" here", 5, '"', true).unwrap();
    assert_eq!(&"say \"hello\" here"[start..end], "\"hello\"");
}

#[test]
fn bracket_obj_inner_finds_content() {
    let (start, end) = text_obj_bracket("foo(bar)baz", 4, '(', ')', false).unwrap();
    assert_eq!(&"foo(bar)baz"[start..end], "bar");
}

#[test]
fn bracket_obj_outer_includes_brackets() {
    let (start, end) = text_obj_bracket("foo(bar)baz", 4, '(', ')', true).unwrap();
    assert_eq!(&"foo(bar)baz"[start..end], "(bar)");
}

#[test]
fn tag_obj_inner_finds_content() {
    let (start, end) = text_obj_tag("<b>bold</b>", 4, false).unwrap();
    assert_eq!(&"<b>bold</b>"[start..end], "bold");
}

#[test]
fn tag_obj_outer_includes_tags() {
    let (start, end) = text_obj_tag("<b>bold</b>", 4, true).unwrap();
    assert_eq!(&"<b>bold</b>"[start..end], "<b>bold</b>");
}

// ── Insert mode controls ────────────────────────────────────────────────

#[test]
fn ctrl_w_in_insert_sends_delete_word_backward() {
    let mut app = App::from_path(None).unwrap();
    // Enter insert mode, then Ctrl+W
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)));
    // Still in insert mode
    assert_eq!(app.mode, Mode::Insert);
}

#[test]
fn ctrl_u_in_insert_sends_delete_to_line_start() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)));
    assert_eq!(app.mode, Mode::Insert);
}

// ── Visual mode transitions ─────────────────────────────────────────────

#[test]
fn capital_v_enters_visual_line_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('V'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::VisualLine);
}

#[test]
fn ctrl_v_enters_visual_block_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)));
    assert_eq!(app.mode, Mode::VisualBlock);
}

#[test]
fn esc_from_visual_line_returns_to_normal() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('V'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::VisualLine);
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn esc_from_visual_block_returns_to_normal() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)));
    assert_eq!(app.mode, Mode::VisualBlock);
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn visual_anchor_set_on_visual_enter() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)));
    assert!(app.visual_anchor.is_some());
}

// ── Undo / Redo ─────────────────────────────────────────────────────────

#[test]
fn u_dispatches_undo() {
    let mut app = App::from_path(None).unwrap();
    // Drive `u` — should send undo edit without crashing.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn ctrl_r_dispatches_redo() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)));
    assert_eq!(app.mode, Mode::Normal);
}

// ── Dot repeat ──────────────────────────────────────────────────────────

#[test]
fn dot_with_no_last_change_is_noop() {
    let mut app = App::from_path(None).unwrap();
    // `.` should not crash when no last_change is recorded.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('.'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
}
