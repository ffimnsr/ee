use std::fs;
use std::sync::mpsc;
use std::sync::mpsc::TryRecvError;
use std::thread;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};
use xi_core_lib::plugin_rpc::SelectionRange;

use crate::app::App;
use crate::buffer::BufferManager;
use crate::tests::helpers::*;

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
fn insert_tab_writes_default_soft_tab_to_scratch_buffer() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));

    wait_until_with_backend(&mut app.backend, "insert tab", Duration::from_secs(1), |backend| {
        backend.lines == vec![String::from("    ")]
            && backend.cursor_line == 0
            && backend.cursor_col == 4
    });

    assert_eq!(app.backend.lines, vec!["    "]);
    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (0, 4));
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
fn repeated_enter_tracks_cursor_beyond_visible_rows() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    for _ in 0..50 {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        app.backend.pump().unwrap();
    }

    assert_eq!(app.backend.cursor_line, 50);
    assert_eq!(app.backend.cursor_col, 0);
    assert_eq!(app.backend.lines.len(), 51);
}

#[test]
fn carriage_return_key_is_treated_as_enter() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    for _ in 0..5 {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('\r'), KeyModifiers::NONE)));
    }

    wait_until_with_backend(
        &mut app.backend,
        "carriage return enter normalization",
        Duration::from_secs(1),
        |backend| backend.cursor_line == 5 && backend.cursor_col == 0 && backend.lines.len() == 6,
    );

    assert_eq!(app.backend.cursor_line, 5);
    assert_eq!(app.backend.cursor_col, 0);
    assert_eq!(app.backend.lines.len(), 6);
}

#[test]
fn ctrl_m_key_is_treated_as_enter() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    for _ in 0..5 {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL)));
        app.backend.pump().unwrap();
    }

    assert_eq!(app.backend.cursor_line, 5);
    assert_eq!(app.backend.cursor_col, 0);
    assert_eq!(app.backend.lines.len(), 6);
}

#[test]
fn bracketed_paste_normalizes_legacy_and_windows_line_endings() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Paste(String::from("alpha\rbeta\r\ngamma")));
    app.backend.pump().unwrap();

    assert_eq!(
        app.backend.lines,
        vec![String::from("alpha"), String::from("beta"), String::from("gamma")]
    );
    assert_eq!(app.insert_buffer, "alpha\nbeta\ngamma");
    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (2, 5));
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
fn transpose_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in [':', 't', 'r', 'a', 'n', 's', 'p', 'o', 's', 'e'] {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "transpose");
}

#[test]
fn selection_for_replace_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":selection_for_replace".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "selection_for_replace");
}

#[test]
fn select_regex_command_uses_selection_scoped_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":select_regex foo.*bar".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("select_regex request should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["method"], "select_regex");
    assert_eq!(value["params"]["params"]["chars"], "foo.*bar");
    assert_eq!(value["params"]["params"]["case_sensitive"], false);
}

#[test]
fn split_selection_on_newline_command_uses_selection_into_lines_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":split_selection_on_newline".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["method"], "selection_into_lines");
}

#[test]
fn collapse_selection_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":collapse_selection".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["method"], "collapse_selections");
}

#[test]
fn align_selections_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":align_selections".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["method"], "align_selections");
}

#[test]
fn align_it_command_uses_backend_edit_with_pattern_params() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    run_ex(&mut app, "align_it =");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["method"], "align_it");
    assert_eq!(value["params"]["params"]["pattern"], "=");
    assert_eq!(value["params"]["params"]["regex"], false);
    assert_eq!(value["params"]["params"]["occurrence"], 1);
    assert_eq!(value["params"]["params"]["all"], false);
    assert_eq!(value["params"]["params"]["format"], "");
}

#[test]
fn align_it_command_supports_nth_and_format_params() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    run_ex(&mut app, "align_it 2= l0r0l0");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["params"]["pattern"], "=");
    assert_eq!(value["params"]["params"]["occurrence"], 2);
    assert_eq!(value["params"]["params"]["all"], false);
    assert_eq!(value["params"]["params"]["format"], "l0r0l0");
}

#[test]
fn align_it_command_supports_all_matches_selector() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    run_ex(&mut app, "align_it *= r1c1l0");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["params"]["pattern"], "=");
    assert_eq!(value["params"]["params"]["all"], true);
    assert_eq!(value["params"]["params"]["format"], "r1c1l0");
}

#[test]
fn align_it_command_rejects_invalid_regex() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    run_ex(&mut app, "align_it /[/");

    let status = app.backend.status_message.as_deref().expect("status message should be set");
    assert!(status.contains("align_it: invalid regex"));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn align_it_command_rejects_invalid_format() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    run_ex(&mut app, "align_it = x1");

    let status = app.backend.status_message.as_deref().expect("status message should be set");
    assert!(status.contains("align_it: invalid format"));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn reverse_selection_contents_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    run_ex(&mut app, "reverse_selection_contents");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["method"], "reverse_selection_contents");
}

#[test]
fn rotate_selections_backward_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":rotate_selections_backward".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["method"], "rotate_selections_backward");
}

#[test]
fn rotate_selections_forward_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":rotate_selections_forward".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["method"], "rotate_selections_forward");
}

#[test]
fn select_all_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":select_all".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["method"], "select_all");
}

#[test]
fn delete_word_forward_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":delete_word_forward".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["method"], "delete_word_forward");
}

#[test]
fn kill_line_command_uses_delete_line_range() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":kill_line".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["method"], "delete_line_range");
    assert_eq!(value["params"]["params"]["start_line"], 0);
    assert_eq!(value["params"]["params"]["end_line"], 0);
}

#[test]
fn add_newline_below_command_emits_line_end_then_newline() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":add_newline_below".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(message["params"]["method"], "add_newline_below");
}

#[test]
fn add_newline_above_command_emits_line_start_newline_and_move_up() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":add_newline_above".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(message["params"]["method"], "add_newline_above");
}

#[test]
fn extend_line_below_command_emits_linewise_selection_gestures() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":extend_line_below".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(message["params"]["method"], "extend_line_below");
    assert_eq!(message["params"]["params"]["count"], 1);
}

#[test]
fn extend_selection_alias_commands_emit_expected_backend_methods() {
    let commands = [
        ("extend_char_left", "move_left_and_modify_selection"),
        ("extend_char_right", "move_right_and_modify_selection"),
        ("extend_visual_line_up", "move_up_and_modify_selection"),
        ("extend_visual_line_down", "move_down_and_modify_selection"),
        ("extend_line_up", "move_up_and_modify_selection"),
        ("extend_line_down", "move_down_and_modify_selection"),
        ("extend_line_above", "extend_line_above"),
        ("select_line_above", "select_line_above"),
        ("select_line_below", "select_line_below"),
        ("goto_file_end", "move_to_end_of_document"),
        ("extend_to_file_start", "move_to_beginning_of_document_and_modify_selection"),
        ("extend_to_file_end", "move_to_end_of_document_and_modify_selection"),
    ];
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for (command, expected) in commands {
        run_ex(&mut app, command);

        let message = rx.recv().expect("message should be sent");
        let value: Value = serde_json::from_str(&message).expect("message should be json");
        assert_eq!(value["method"], "edit");
        assert_eq!(value["params"]["method"], expected);
    }
}

#[test]
fn join_selections_command_joins_selected_lines() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    for ch in "abc\n    def".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    app.backend.pump().unwrap();

    let _ = app.backend.send_edit(
        "gesture",
        json!({
            "line": 0,
            "col": 0,
            "ty": "point_select",
        }),
    );
    let _ = app.backend.send_edit(
        "gesture",
        json!({
            "line": 1,
            "col": 7,
            "ty": { "select_extend": { "granularity": "point" } },
        }),
    );
    app.backend.pump().unwrap();

    run_ex(&mut app, "join_selections");
    for _ in 0..20 {
        app.backend.pump().unwrap();
        if app.backend.lines.first().is_some_and(|line| line == "abc def") {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(app.backend.lines.first().map(String::as_str), Some("abc def"));
}

#[test]
fn filter_selections_preview_uses_backend_authoritative_text() {
    let mut app = App::from_path(None).unwrap();

    insert_text(&mut app, "alpha beta alps");
    app.backend.pump().unwrap();

    app.backend
        .set_selections(&[
            SelectionRange { start: 0, end: 5 },
            SelectionRange { start: 6, end: 10 },
            SelectionRange { start: 11, end: 15 },
        ])
        .expect("set selections should succeed");
    app.backend.pump().unwrap();

    let filtered =
        app.backend.filter_selections_preview("^a", false).expect("filter preview should succeed");
    app.backend.set_selections(&filtered).expect("filtered selections should apply");

    wait_until_with_backend(
        &mut app.backend,
        "filtered selection annotations",
        Duration::from_secs(1),
        |backend| {
            backend.annotations.iter().any(|annotation| {
                annotation.annotation_type == "selection"
                    && annotation.ranges == vec![[0, 0, 0, 5], [0, 11, 0, 15]]
            })
        },
    );

    let selection_ranges = app
        .backend
        .annotations
        .iter()
        .find(|annotation| annotation.annotation_type == "selection")
        .map(|annotation| annotation.ranges.clone())
        .expect("selection annotation should exist");

    assert_eq!(selection_ranges, vec![[0, 0, 0, 5], [0, 11, 0, 15]]);
}

#[test]
fn select_chars_preview_uses_backend_authoritative_text() {
    let mut app = App::from_path(None).unwrap();

    insert_text(&mut app, "aéb");
    app.backend.pump().unwrap();
    app.backend
        .set_selections(&[SelectionRange { start: 0, end: 0 }])
        .expect("set selections should succeed");
    app.backend.pump().unwrap();

    let selection =
        app.backend.select_chars_preview(2).expect("select chars preview should succeed");

    assert_eq!(selection, vec![SelectionRange { start: 0, end: 3 }]);
}

#[test]
fn selected_text_preview_uses_backend_authoritative_selection() {
    let mut app = App::from_path(None).unwrap();

    insert_text(&mut app, "alpha\nbeta");
    app.backend.pump().unwrap();
    app.backend
        .set_selections(&[SelectionRange { start: 1, end: 8 }])
        .expect("set selections should succeed");
    app.backend.pump().unwrap();

    let selected =
        app.backend.selected_text_preview(false).expect("selected text preview should succeed");
    let linewise = app
        .backend
        .selected_text_preview(true)
        .expect("linewise selected text preview should succeed");

    assert_eq!(selected, "lpha\nbe");
    assert_eq!(linewise, "alpha\nbeta\n");
}

#[test]
fn block_text_preview_uses_backend_authoritative_text() {
    let mut app = App::from_path(None).unwrap();

    insert_text(&mut app, "abcd\nefgh\nijk");
    app.backend.pump().unwrap();

    let block =
        app.backend.block_text_preview(0, 2, 1, 3).expect("block text preview should succeed");

    assert_eq!(block, "bc\nfg\njk\n");
}

#[test]
fn fold_close_uses_backend_authoritative_tree_sitter_range() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("fold-close.rs");
    fs::write(&path, "").unwrap();
    let mut app = App::from_path(Some(path)).unwrap();

    insert_text(&mut app, "fn outer() {\n    if true {\n        work();\n    }\n}\n");
    app.backend.pump().unwrap();

    app.backend.cursor_line = 0;

    app.fold_close();

    let buf_id = app.backend.active().id;
    assert_eq!(app.folds.fold_at(buf_id, 0), Some((0, 4)));
    assert!(app.folds.is_hidden(buf_id, 1));
}

#[test]
fn fold_close_all_uses_backend_authoritative_ranges() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("fold-close-all.rs");
    fs::write(&path, "").unwrap();
    let mut app = App::from_path(Some(path)).unwrap();

    insert_text(&mut app, "fn outer() {\n    work();\n}\n\nfn second() {\n    more();\n}\n");
    app.backend.pump().unwrap();

    app.fold_close_all();

    let buf_id = app.backend.active().id;
    assert_eq!(app.folds.fold_at(buf_id, 0), Some((0, 2)));
    assert_eq!(app.folds.fold_at(buf_id, 4), Some((4, 6)));
}

#[test]
fn remove_selections_command_uses_search_pattern_and_reports_empty_result() {
    let mut app = App::from_path(None).unwrap();

    insert_text(&mut app, "alpha beta");
    app.backend.pump().unwrap();

    app.backend
        .set_selections(&[
            SelectionRange { start: 0, end: 5 },
            SelectionRange { start: 6, end: 10 },
        ])
        .expect("set selections should succeed");
    app.backend.pump().unwrap();
    app.search_pattern = Some(String::from("."));

    run_ex(&mut app, "remove_selections");

    assert_eq!(app.backend.status_message.as_deref(), Some("no selections remaining"));
}

#[test]
fn substitute_range_uses_backend_authoritative_path() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    for ch in "alpha\nbeta\nalpha".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    app.backend.pump().unwrap();

    app.execute_substitute(1, 2, "a", "A", "");
    app.backend.pump().unwrap();

    assert_eq!(app.backend.lines, vec!["alpha", "betA", "Alpha"]);
}

#[test]
fn substitute_confirm_uses_backend_preview_and_apply() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    for ch in "alpha\nbeta\nalpha".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    app.backend.pump().unwrap();

    app.execute_substitute(0, 2, "a", "A", "c");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)));
    app.backend.pump().unwrap();

    assert_eq!(app.backend.lines, vec!["Alpha", "beta", "alpha"]);
}
