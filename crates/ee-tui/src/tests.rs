use std::env;
use std::fs;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};

use crate::app::{App, Mode, PendingCharFind};
use crate::backend::{
    BackendEvent, CachedLine, CompletionSuggestion, CoreLine, CoreUpdate, CoreUpdateKind,
    CoreUpdateOp, LineSlot, NavigationTarget, XiClient, format_location_message,
    invalid_line_ranges, parse_notification,
};
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
fn normal_q_quits() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));

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
    let mut client = test_client();
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
    let client = XiClient {
        path: None,
        tx,
        backend_rx,
        view_id: String::from("view-id-1"),
        pending_line_request: false,
        line_cache: Vec::new(),
        lines: Vec::new(),
        cursor_line: 0,
        cursor_col: 0,
        pristine: true,
        status_message: None,
        last_scroll: None,
    };

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

fn test_client() -> XiClient {
    let (tx, _rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    XiClient {
        path: None,
        tx,
        backend_rx,
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
