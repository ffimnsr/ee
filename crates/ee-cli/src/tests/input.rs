use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::app::{App, Mode, PendingCharFind};
use crate::text::{
    byte_col_to_display_col, display_col_to_byte, find_char_backward, find_char_forward,
    next_char_start, next_word_end, next_word_start, prev_char_start, prev_word_start,
};

// ── Input coalescing ────────────────────────────────────────────────────────────

#[test]
fn input_loop_coalesces_stale_repeated_arrow_motion() {
    let up_repeat =
        Event::Key(KeyEvent::new_with_kind(KeyCode::Up, KeyModifiers::NONE, KeyEventKind::Repeat));
    let down_press = Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));

    let coalesced =
        crate::coalesce_input_events(vec![up_repeat.clone(), up_repeat, down_press.clone()]);

    assert_eq!(coalesced, vec![down_press]);
}

#[test]
fn input_loop_preserves_non_repeated_input_batch() {
    let first = Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    let second = Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));

    let coalesced = crate::coalesce_input_events(vec![first.clone(), second.clone()]);

    assert_eq!(coalesced, vec![first, second]);
}

#[test]
fn input_loop_does_not_coalesce_typed_repeat_chars() {
    let repeat = Event::Key(KeyEvent::new_with_kind(
        KeyCode::Char('j'),
        KeyModifiers::NONE,
        KeyEventKind::Repeat,
    ));

    let coalesced = crate::coalesce_input_events(vec![repeat.clone(), repeat.clone()]);

    assert_eq!(coalesced, vec![repeat.clone(), repeat]);
}

// ── Count / prefix ──────────────────────────────────────────────────────────────

#[test]
fn count_digits_accumulate_in_normal_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE)));
    assert_eq!(app.input_state.count(), 35);
}

#[test]
fn count_resets_after_motion() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
    assert_eq!(app.input_state.count(), 1);
}

#[test]
fn zero_as_motion_when_no_count_active() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE)));
    assert_eq!(app.input_state.count_digits, Vec::<u8>::new());
}

#[test]
fn zero_extends_count_when_digits_already_present() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE)));
    assert_eq!(app.input_state.count(), 10);
}

#[test]
fn g_key_sets_prefix_and_is_not_reset_immediately() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE)));
    assert_eq!(app.input_state.prefix, Some('g'));
}

#[test]
fn gg_prefix_clears_after_second_g() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE)));
    assert_eq!(app.input_state.prefix, None);
}

#[test]
fn f_key_enters_pending_find_state() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)));
    assert_eq!(
        app.input_state.pending_find,
        Some(PendingCharFind { forward: true, inclusive: true })
    );
}

#[test]
fn t_key_enters_pending_find_exclusive_state() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE)));
    assert_eq!(
        app.input_state.pending_find,
        Some(PendingCharFind { forward: true, inclusive: false })
    );
}

#[test]
fn pending_find_clears_after_target_char() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)));
    assert_eq!(app.input_state.pending_find, None);
}

// ── Search ──────────────────────────────────────────────────────────────────────

#[test]
fn slash_enters_search_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Search);
    assert!(app.command_buffer.is_empty());
}

#[test]
fn search_esc_returns_to_normal() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn search_chars_accumulate_in_command_buffer() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE)));
    assert_eq!(app.command_buffer, "foo");
    assert_eq!(app.mode, Mode::Search);
}

#[test]
fn search_backspace_removes_last_char() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)));
    assert_eq!(app.command_buffer, "a");
}

#[test]
fn search_enter_returns_to_normal() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
}

// ── Find char ───────────────────────────────────────────────────────────────────

#[test]
fn find_char_forward_finds_next_occurrence() {
    assert_eq!(find_char_forward("abcabc", 0, 'b'), Some(1));
}

#[test]
fn find_char_forward_skips_char_under_cursor() {
    assert_eq!(find_char_forward("abcabc", 1, 'b'), Some(4));
}

#[test]
fn find_char_forward_returns_none_when_absent() {
    assert_eq!(find_char_forward("abc", 0, 'z'), None);
}

#[test]
fn find_char_backward_finds_previous_occurrence() {
    assert_eq!(find_char_backward("abcabc", 6, 'b'), Some(4));
}

#[test]
fn find_char_backward_stops_before_cursor() {
    assert_eq!(find_char_backward("abcabc", 4, 'b'), Some(1));
}

#[test]
fn find_char_backward_returns_none_when_absent() {
    assert_eq!(find_char_backward("abc", 3, 'z'), None);
}

// ── Word / char motions ─────────────────────────────────────────────────────────

#[test]
fn next_word_start_moves_to_following_identifier() {
    assert_eq!(next_word_start("alpha beta", 0, false), Some(6));
}

#[test]
fn prev_word_start_moves_to_current_identifier_start() {
    assert_eq!(prev_word_start("alpha beta", 8, false), Some(6));
}

#[test]
fn next_word_end_stops_at_identifier_end() {
    assert_eq!(next_word_end("alpha beta", 0, false), Some(4));
}

#[test]
fn long_word_motion_treats_punctuation_as_word_content() {
    assert_eq!(next_word_start("alpha::beta gamma", 0, true), Some(12));
    assert_eq!(next_word_end("alpha::beta gamma", 0, true), Some(10));
}

#[test]
fn prev_char_start_ascii() {
    assert_eq!(prev_char_start("hello", 3), 2);
}

#[test]
fn prev_char_start_multibyte() {
    let s = "aé";
    assert_eq!(prev_char_start(s, 3), 1);
}

#[test]
fn next_char_start_ascii() {
    assert_eq!(next_char_start("hello", 1), 2);
}

#[test]
fn next_char_start_multibyte() {
    let s = "aé";
    assert_eq!(next_char_start(s, 1), 3);
}

// ── Display column ──────────────────────────────────────────────────────────────

#[test]
fn byte_col_to_display_col_ascii() {
    assert_eq!(byte_col_to_display_col("hello", 3), 3);
}

#[test]
fn byte_col_to_display_col_expands_tabs() {
    assert_eq!(byte_col_to_display_col("\tabc", 4), 7);
    assert_eq!(byte_col_to_display_col("ab\tcd", 5), 6);
}

#[test]
fn display_col_to_byte_respects_tab_stops() {
    assert_eq!(display_col_to_byte("\tabc", 4), 1);
    assert_eq!(display_col_to_byte("ab\tcd", 4), 3);
    assert_eq!(display_col_to_byte("ab\tcd", 6), 5);
}

#[test]
fn byte_col_to_display_col_wide_char() {
    let s = "日本";
    assert_eq!(byte_col_to_display_col(s, 3), 2);
    assert_eq!(byte_col_to_display_col(s, 6), 4);
}

#[test]
fn display_col_to_byte_wide_char() {
    let s = "日本";
    assert_eq!(display_col_to_byte(s, 0), 0);
    assert_eq!(display_col_to_byte(s, 2), 3);
    assert_eq!(display_col_to_byte(s, 4), 6);
}
