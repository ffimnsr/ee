use std::env;
use std::fs;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};

use crate::app::{App, Mode, Operator, PendingCharFind};
use crate::backend::{
    BackendEvent, CachedLine, CompletionSuggestion, CoreLine, CoreUpdate, CoreUpdateKind,
    CoreUpdateOp, LineSlot, NavigationTarget, format_location_message,
    invalid_line_ranges, parse_notification,
};
use crate::buffer::{BufState, BufferManager};
use crate::keymap::{Action, BindingKey, bindings};
use crate::text::{
    byte_col_to_display_col, display_col_to_byte, find_char_backward, find_char_forward,
    next_char_start, prev_char_start,
};

#[test]
fn scratch_title_is_default() {
    let app = App::from_path(None).unwrap();

    assert_eq!(app.backend.title(), "[scratch]");
}

#[test]
fn ctrl_c_quits() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));

    assert!(app.should_quit);
}

#[test]
fn colon_q_quits() {
    let mut app = App::from_path(None).unwrap();
    for ch in [':', 'q'] {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    assert!(app.should_quit);
}

#[test]
fn insert_escape_returns_to_normal() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));

    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn command_line_quit_exits() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    assert_eq!(app.mode, Mode::Normal);
    assert!(app.should_quit);
}

#[test]
fn insert_mode_writes_to_scratch_buffer() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)));
    app.backend.pump().unwrap();

    assert_eq!(app.backend.lines, vec!["ab"]);
    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (0, 2));
}

#[test]
fn enter_splits_line_and_backspace_joins_it() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)));
    app.backend.pump().unwrap();

    assert_eq!(app.backend.lines, vec!["a", "b"]);

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)));
    app.backend.pump().unwrap();

    assert_eq!(app.backend.lines, vec!["a"]);
    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (0, 1));
}

#[test]
fn backspace_removes_multibyte_char() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('é'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)));
    app.backend.pump().unwrap();

    assert!(app.backend.lines.is_empty());
    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (0, 0));
}

#[test]
fn apply_update_merges_copy_update_insert_and_invalidate() {
    let mut client = test_buf_state();
    client.line_cache = vec![
        LineSlot::Known(CachedLine { text: "alpha".into(), cursors: Vec::new() }),
        LineSlot::Known(CachedLine { text: "beta".into(), cursors: vec![2] }),
        LineSlot::Known(CachedLine { text: "gamma".into(), cursors: Vec::new() }),
    ];
    client.rebuild_lines();

    client
        .apply_update(CoreUpdate {
            pristine: false,
            ops: vec![
                CoreUpdateOp { op: CoreUpdateKind::Copy, n: 1, lines: Vec::new() },
                CoreUpdateOp {
                    op: CoreUpdateKind::Update,
                    n: 1,
                    lines: vec![CoreLine { text: None, cursor: vec![1] }],
                },
                CoreUpdateOp {
                    op: CoreUpdateKind::Insert,
                    n: 1,
                    lines: vec![CoreLine { text: Some("delta".into()), cursor: Vec::new() }],
                },
                CoreUpdateOp { op: CoreUpdateKind::Invalidate, n: 2, lines: Vec::new() },
            ],
        })
        .unwrap();

    assert_eq!(client.lines, vec!["alpha", "beta", "delta", "", ""]);
    assert_eq!((client.cursor_line, client.cursor_col), (1, 1));
    assert_eq!(invalid_line_ranges(&client.line_cache), vec![(3, 5)]);
    assert!(!client.pristine);
}

#[test]
fn open_file_bootstraps_full_buffer_from_updates() {
    let path = unique_temp_path("ee-tui-open");
    let contents = (0..24).map(|i| format!("line-{i}")).collect::<Vec<_>>().join("\n");
    fs::write(&path, &contents).unwrap();

    let app = App::from_path(Some(path.clone())).unwrap();

    fs::remove_file(&path).unwrap();
    assert_eq!(app.backend.lines, contents.split('\n').map(ToOwned::to_owned).collect::<Vec<_>>());
}

#[test]
fn write_command_saves_file() {
    let path = unique_temp_path("ee-tui-save");
    fs::write(&path, "seed").unwrap();

    let mut app = App::from_path(Some(path.clone())).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    for _ in 0..20 {
        let text = fs::read_to_string(&path).unwrap();
        if text.starts_with('!') {
            fs::remove_file(&path).unwrap();
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }

    let final_text = fs::read_to_string(&path).unwrap();
    fs::remove_file(&path).unwrap();
    assert!(final_text.starts_with('!'));
}

#[test]
fn parse_notification_handles_show_completions() {
    let event = parse_notification(
        "show_completions",
        json!({
            "items": [{
                "label": "println!",
                "detail": "macro",
                "insert_text": "println!($0)"
            }]
        }),
    )
    .expect("completion notification should parse");

    match event {
        BackendEvent::ShowCompletions(items) => {
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].label, "println!");
        }
        other => panic!("unexpected event: {:?}", other),
    }
}

#[test]
fn send_plugin_rpc_emits_plugin_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client
        .send_plugin_rpc("xi-lsp-plugin", "lsp.definition", json!({}))
        .expect("plugin rpc should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "plugin");
    assert_eq!(value["params"]["command"], "plugin_rpc");
    assert_eq!(value["params"]["receiver"], "xi-lsp-plugin");
    assert_eq!(value["params"]["rpc"]["method"], "lsp.definition");
}

#[test]
fn byte_col_to_display_col_ascii() {
    assert_eq!(byte_col_to_display_col("hello", 3), 3);
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

#[test]
fn viewport_scrolls_down_when_cursor_leaves_view() {
    let mut app = App::from_path(None).unwrap();
    app.backend.cursor_line = 25;
    app.scroll_into_view(20);
    assert_eq!(app.viewport.top_line, 6);
}

#[test]
fn viewport_scrolls_up_when_cursor_above_top() {
    let mut app = App::from_path(None).unwrap();
    app.viewport.top_line = 10;
    app.backend.cursor_line = 5;
    app.scroll_into_view(20);
    assert_eq!(app.viewport.top_line, 5);
}

#[test]
fn bindings_table_has_normal_hjkl() {
    let b = bindings();
    let lookup = |key| {
        b.get(&BindingKey { mode: Mode::Normal, key, modifiers: KeyModifiers::NONE, prefix: None })
            .cloned()
    };
    assert_eq!(lookup(KeyCode::Char('h')), Some(Action::Edit("move_left")));
    assert_eq!(lookup(KeyCode::Char('l')), Some(Action::Edit("move_right")));
    assert_eq!(lookup(KeyCode::Char('k')), Some(Action::Edit("move_up")));
    assert_eq!(lookup(KeyCode::Char('j')), Some(Action::Edit("move_down")));
}

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

#[test]
fn format_location_message_formats_empty_and_single_results() {
    assert_eq!(format_location_message("definition", &[]), "definition: no locations");
    assert_eq!(
        format_location_message(
            "definition",
            &[NavigationTarget {
                path: String::from("/tmp/main.rs"),
                line: 2,
                column: 4,
                end_line: 2,
                end_column: 7,
            }],
        ),
        "definition: /tmp/main.rs:3:5"
    );
}

#[test]
fn completion_suggestion_deserializes_optional_fields() {
    let item: CompletionSuggestion = serde_json::from_value(json!({
        "label": "println!"
    }))
    .unwrap();
    assert_eq!(item.label, "println!");
    assert_eq!(item.detail, None);
    assert_eq!(item.insert_text, None);
}

fn unique_temp_path(prefix: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    env::temp_dir().join(format!("{prefix}-{nanos}.txt"))
}

fn test_buf_state() -> BufState {
    BufState {
        id: 1,
        path: None,
        view_id: String::new(),
        pending_line_request: false,
        line_cache: Vec::new(),
        lines: Vec::new(),
        cursor_line: 0,
        cursor_col: 0,
        pristine: true,
        status_message: None,
        last_scroll: None,
    }
}

// ── Insert-entry variants ─────────────────────────────────────────────────────

#[test]
fn a_enters_insert_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Insert);
}

#[test]
fn capital_a_enters_insert_at_eol() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Insert);
}

#[test]
fn capital_i_enters_insert_at_line_start() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('I'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Insert);
}

#[test]
fn o_enters_insert_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Insert);
}

#[test]
fn capital_o_enters_insert_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('O'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Insert);
}

#[test]
fn s_enters_insert_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Insert);
}

#[test]
fn capital_s_enters_insert_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Insert);
}

// ── Operator-pending mode ─────────────────────────────────────────────────────

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

// ── Text object helpers ───────────────────────────────────────────────────────

use crate::app::{text_obj_bracket, text_obj_quote, text_obj_tag, text_obj_word};

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

// ── Insert mode controls ──────────────────────────────────────────────────────

#[test]
fn ctrl_w_in_insert_sends_delete_word_backward() {
    let mut app = App::from_path(None).unwrap();
    // Enter insert mode, then Ctrl+W
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Char('w'),
        KeyModifiers::CONTROL,
    )));
    // Still in insert mode
    assert_eq!(app.mode, Mode::Insert);
}

#[test]
fn ctrl_u_in_insert_sends_delete_to_line_start() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Char('u'),
        KeyModifiers::CONTROL,
    )));
    assert_eq!(app.mode, Mode::Insert);
}

// ── New feature tests ─────────────────────────────────────────────────────────

#[test]
fn capital_v_enters_visual_line_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Char('V'),
        KeyModifiers::NONE,
    )));
    assert_eq!(app.mode, Mode::VisualLine);
}

#[test]
fn ctrl_v_enters_visual_block_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Char('v'),
        KeyModifiers::CONTROL,
    )));
    assert_eq!(app.mode, Mode::VisualBlock);
}

#[test]
fn esc_from_visual_line_returns_to_normal() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Char('V'),
        KeyModifiers::NONE,
    )));
    assert_eq!(app.mode, Mode::VisualLine);
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn esc_from_visual_block_returns_to_normal() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Char('v'),
        KeyModifiers::CONTROL,
    )));
    assert_eq!(app.mode, Mode::VisualBlock);
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn u_dispatches_undo() {
    use crate::registers::RegisterName;
    let mut app = App::from_path(None).unwrap();
    // Drive `u` — should send undo edit without crashing.
    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Char('u'),
        KeyModifiers::NONE,
    )));
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn ctrl_r_dispatches_redo() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Char('r'),
        KeyModifiers::CONTROL,
    )));
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn dot_with_no_last_change_is_noop() {
    let mut app = App::from_path(None).unwrap();
    // `.` should not crash when no last_change is recorded.
    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Char('.'),
        KeyModifiers::NONE,
    )));
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn register_yank_stores_and_retrieves() {
    use crate::registers::{RegisterName, RegisterStore};
    let mut store = RegisterStore::default();
    store.yank(&RegisterName::Named('a'), "hello".to_owned(), false);
    assert_eq!(store.get(&RegisterName::Named('a')), "hello");
    // Unnamed should also be set.
    assert_eq!(store.get(&RegisterName::Unnamed), "hello");
}

#[test]
fn register_prefix_sets_pending_register() {
    use crate::registers::RegisterName;
    let mut app = App::from_path(None).unwrap();
    // `"` then `a` should set pending register to Named('a').
    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Char('"'),
        KeyModifiers::NONE,
    )));
    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Char('a'),
        KeyModifiers::NONE,
    )));
    assert_eq!(
        app.input_state.pending_register,
        Some(RegisterName::Named('a'))
    );
}

#[test]
fn visual_anchor_set_on_visual_enter() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Char('v'),
        KeyModifiers::NONE,
    )));
    assert!(app.visual_anchor.is_some());
}

// ── Marks ─────────────────────────────────────────────────────────────────

#[test]
fn set_mark_stores_cursor_position() {
    let mut app = App::from_path(None).unwrap();
    // Press `m` then `a` — should store current (0,0) position under 'a'.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE)));
    assert!(app.input_state.awaiting_mark_set);
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)));
    assert!(!app.input_state.awaiting_mark_set);
    assert_eq!(app.marks.get(&'a').copied(), Some((0, 0)));
}

#[test]
fn uppercase_mark_is_ignored() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE)));
    // Only lowercase marks are supported; 'A' should not be stored.
    assert!(app.marks.get(&'A').is_none());
}

#[test]
fn backtick_enter_awaiting_mark_jump_exact() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('`'), KeyModifiers::NONE)));
    assert_eq!(app.input_state.awaiting_mark_jump, Some(false));
}

#[test]
fn quote_enter_awaiting_mark_jump_line_start() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('\''), KeyModifiers::NONE)));
    assert_eq!(app.input_state.awaiting_mark_jump, Some(true));
}

// ── Jump list ─────────────────────────────────────────────────────────────

#[test]
fn push_jump_adds_to_list() {
    let mut app = App::from_path(None).unwrap();
    app.push_jump();
    assert_eq!(app.jump_list.len(), 1);
    assert_eq!(app.jump_list[0], (0, 0));
}

#[test]
fn push_jump_deduplicates_head() {
    let mut app = App::from_path(None).unwrap();
    app.push_jump();
    app.push_jump();
    assert_eq!(app.jump_list.len(), 1);
}

#[test]
fn jump_list_idx_reset_to_len_after_push() {
    let mut app = App::from_path(None).unwrap();
    app.push_jump();
    assert_eq!(app.jump_list_idx, app.jump_list.len());
}

#[test]
fn ctrl_o_is_bound_to_jump_list_older() {
    use crate::keymap::{Action, BindingKey};
    let key = BindingKey {
        mode: crate::app::Mode::Normal,
        key: KeyCode::Char('o'),
        modifiers: KeyModifiers::CONTROL,
        prefix: None,
    };
    assert_eq!(bindings().get(&key), Some(&Action::JumpListOlder));
}

// ── Change list ───────────────────────────────────────────────────────────

#[test]
fn push_change_adds_position() {
    let mut app = App::from_path(None).unwrap();
    app.push_change();
    assert_eq!(app.change_list.len(), 1);
    assert_eq!(app.change_list[0], (0, 0));
}

#[test]
fn push_change_deduplicates_head() {
    let mut app = App::from_path(None).unwrap();
    app.push_change();
    app.push_change();
    assert_eq!(app.change_list.len(), 1);
}

#[test]
fn g_semicolon_bound_to_change_list_older() {
    use crate::keymap::{Action, BindingKey};
    let key = BindingKey {
        mode: crate::app::Mode::Normal,
        key: KeyCode::Char(';'),
        modifiers: KeyModifiers::NONE,
        prefix: Some('g'),
    };
    assert_eq!(bindings().get(&key), Some(&Action::ChangeListOlder));
}

// ── Macro recording / replay ─────────────────────────────────────────────

#[test]
fn q_then_char_starts_recording() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));
    assert!(app.input_state.awaiting_macro_record);
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)));
    assert_eq!(app.macro_register, Some('a'));
}

#[test]
fn q_while_recording_stops_recording() {
    let mut app = App::from_path(None).unwrap();
    // Start recording into 'a'.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)));
    assert_eq!(app.macro_register, Some('a'));
    // Stop recording.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));
    assert!(app.macro_register.is_none());
    // Macro should be stored (may be empty or have keys from the stop 'q').
    assert!(app.macros.contains_key(&'a'));
    // Terminating 'q' must NOT be part of the stored macro.
    let stored = &app.macros[&'a'];
    assert!(!stored.iter().any(|k| k.code == KeyCode::Char('q')));
}

#[test]
fn macro_records_and_replays_keystrokes() {
    let mut app = App::from_path(None).unwrap();
    // Record: `qa` <some key> `q`
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)));
    // Record a simple keystroke (move_right via 'l').
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)));
    // Stop recording.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));

    let stored = app.macros.get(&'a').cloned().unwrap_or_default();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].code, KeyCode::Char('l'));
}

#[test]
fn at_at_replays_last_macro() {
    let mut app = App::from_path(None).unwrap();
    // Record `qa l q`.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));
    assert_eq!(app.last_macro, Some('a'));

    // `@@` should replay 'a'.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('@'), KeyModifiers::NONE)));
    // awaiting_macro_replay should be set.
    assert!(app.input_state.awaiting_macro_replay);
    // Sending '@' again (@@) should consume and trigger replay.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('@'), KeyModifiers::NONE)));
    // No crash; macro_register is None (not recording).
    assert!(app.macro_register.is_none());
}
