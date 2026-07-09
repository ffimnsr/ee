use std::fs;
use std::sync::mpsc;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;
use xi_core_lib::plugin_rpc::{Diagnostic, DiagnosticSeverity, Range};

use crate::app::{App, Mode};
use crate::buffer::BufferManager;
use crate::keymap::{Action, BindingKey};
use crate::tests::helpers::*;

#[test]
fn goto_column_command_moves_cursor_to_requested_column() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)));
    for ch in "goto_column 3".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "goto_column");
    assert_eq!(value["params"]["params"]["display_col"], 2);
    assert_eq!(value["params"]["params"]["modify_selection"], false);
}

#[test]
fn goto_first_nonwhitespace_command_moves_to_first_content_column() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = vec![String::from("   foo")];

    run_ex(&mut app, "goto_first_nonwhitespace");

    let value: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "goto_column");
    assert_eq!(value["params"]["params"]["display_col"], 3);
    assert_eq!(value["params"]["params"]["modify_selection"], false);
}

#[test]
fn goto_last_modification_command_uses_change_list_position() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.change_list = vec![(4, 7)];
    app.change_list_idx = 0;

    run_ex(&mut app, "goto_last_modification");

    let value: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "gesture");
    assert_eq!(value["params"]["params"]["line"], 4);
    assert_eq!(value["params"]["params"]["col"], 7);
}

#[test]
fn goto_word_command_reuses_word_start_motion() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    run_ex(&mut app, "goto_word");

    let value: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "move_word_start");
    assert_eq!(value["params"]["params"]["forward"], true);
    assert_eq!(value["params"]["params"]["long_word"], false);
    assert_eq!(value["params"]["params"]["modify_selection"], false);
}

#[test]
fn goto_diag_commands_use_active_buffer_diagnostics() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = vec![String::from("abc"), String::from("de"), String::from("fgh")];
    app.backend.diagnostics = vec![
        Diagnostic {
            range: Range { start: 1, end: 2 },
            severity: DiagnosticSeverity::Warning,
            message: String::from("first"),
            source: Some(String::from("lsp")),
            code: None,
        },
        Diagnostic {
            range: Range { start: 4, end: 5 },
            severity: DiagnosticSeverity::Warning,
            message: String::from("second"),
            source: Some(String::from("lsp")),
            code: None,
        },
        Diagnostic {
            range: Range { start: 7, end: 8 },
            severity: DiagnosticSeverity::Warning,
            message: String::from("third"),
            source: Some(String::from("lsp")),
            code: None,
        },
    ];

    app.backend.cursor_line = 0;
    app.backend.cursor_col = 1;
    run_ex(&mut app, "goto_next_diag");

    let value: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "gesture");
    assert_eq!(value["params"]["params"]["line"], 1);
    assert_eq!(value["params"]["params"]["col"], 0);

    app.backend.cursor_line = 1;
    app.backend.cursor_col = 0;
    run_ex(&mut app, "goto_prev_diag");

    let value: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "gesture");
    assert_eq!(value["params"]["params"]["line"], 0);
    assert_eq!(value["params"]["params"]["col"], 1);
}

#[test]
fn goto_edge_diag_commands_jump_to_first_and_last_entries() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = vec![String::from("abc"), String::from("de"), String::from("fgh")];
    app.backend.diagnostics = vec![
        Diagnostic {
            range: Range { start: 1, end: 2 },
            severity: DiagnosticSeverity::Warning,
            message: String::from("first"),
            source: Some(String::from("lsp")),
            code: None,
        },
        Diagnostic {
            range: Range { start: 7, end: 8 },
            severity: DiagnosticSeverity::Warning,
            message: String::from("last"),
            source: Some(String::from("lsp")),
            code: None,
        },
    ];

    run_ex(&mut app, "goto_first_diag");

    let value: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(value["params"]["method"], "gesture");
    assert_eq!(value["params"]["params"]["line"], 0);
    assert_eq!(value["params"]["params"]["col"], 1);

    run_ex(&mut app, "goto_last_diag");

    let value: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(value["params"]["method"], "gesture");
    assert_eq!(value["params"]["params"]["line"], 2);
    assert_eq!(value["params"]["params"]["col"], 0);
}

#[test]
fn goto_syntax_and_paragraph_commands_forward_backend_methods() {
    let commands = [
        "goto_next_function",
        "goto_prev_function",
        "goto_next_class",
        "goto_prev_class",
        "goto_next_parameter",
        "goto_prev_parameter",
        "goto_next_comment",
        "goto_prev_comment",
        "goto_next_test",
        "goto_prev_test",
        "goto_next_paragraph",
        "goto_prev_paragraph",
    ];
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for command in commands {
        run_ex(&mut app, command);

        let value: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
            .expect("message should be json");
        assert_eq!(value["method"], "edit");
        assert_eq!(value["params"]["method"], command);
    }
}

#[test]
fn goto_change_commands_reuse_git_hunk_navigation() {
    let temp = tempfile::tempdir().unwrap();
    init_test_git_repo(temp.path());

    let path = temp.path().join("sample.rs");
    fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").unwrap();
    run_git(temp.path(), &["add", "sample.rs"]);
    run_git(temp.path(), &["commit", "-m", "init"]);

    let modified_lines = vec![
        String::from("one"),
        String::from("two changed"),
        String::from("three"),
        String::from("four changed"),
        String::from("five"),
    ];
    let status = crate::git::inspect_buffer(&path, &modified_lines)
        .unwrap()
        .expect("git status should exist");
    assert_eq!(status.hunks.len(), 2);

    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.path = Some(path);
    app.backend.lines = modified_lines;

    app.backend.cursor_line = 0;
    run_ex(&mut app, "goto_next_change");
    let value: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(value["params"]["method"], "gesture");
    assert_eq!(value["params"]["params"]["line"], status.next_hunk_line(0).unwrap());

    app.backend.cursor_line = 4;
    run_ex(&mut app, "goto_prev_change");
    let value: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(value["params"]["params"]["line"], status.prev_hunk_line(4).unwrap());

    run_ex(&mut app, "goto_first_change");
    let value: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(value["params"]["params"]["line"], status.first_hunk_line().unwrap());

    run_ex(&mut app, "goto_last_change");
    let value: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(value["params"]["params"]["line"], status.last_hunk_line().unwrap());
}

#[test]
fn goto_line_action_uses_count_as_target_line() {
    let mut app = App::from_path(None).unwrap();
    app.backend.lines = vec![String::from("a"), String::from("b"), String::from("c")];
    app.input_state.count_digits = vec![2];
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('g'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::GotoLine,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::ALT)));

    assert_eq!(app.jump_list.last().copied(), Some((0, 0)));
}

#[test]
fn goto_file_start_action_without_count_jumps_to_first_line() {
    let mut app = App::from_path(None).unwrap();
    app.backend.lines = vec![String::from("a"), String::from("b"), String::from("c")];
    app.backend.cursor_line = 2;
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('s'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::GotoFileStart,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT)));

    assert_eq!(app.jump_list.last().copied(), Some((2, 0)));
}

#[test]
fn goto_file_start_action_uses_count_as_target_line() {
    let mut app = App::from_path(None).unwrap();
    app.backend.lines = vec![String::from("a"), String::from("b"), String::from("c")];
    app.input_state.count_digits = vec![3];
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('s'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::GotoFileStart,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT)));

    assert_eq!(app.jump_list.last().copied(), Some((0, 0)));
}

#[test]
fn goto_last_line_action_jumps_to_final_line() {
    let mut app = App::from_path(None).unwrap();
    app.backend.lines = vec![String::from("a"), String::from("b"), String::from("c")];
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('e'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::GotoLastLine,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::ALT)));

    assert_eq!(app.jump_list.last().copied(), Some((0, 0)));
}

#[test]
fn goto_file_action_opens_path_under_cursor() {
    let target = unique_temp_path("ee-cli-goto-file-target");
    fs::write(&target, "hello\n").unwrap();

    let mut app = App::from_path(None).unwrap();
    app.backend.lines = vec![format!("see \"{}\" now", target.display())];
    app.backend.cursor_col = 6;
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('f'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::GotoFile,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT)));

    assert_eq!(app.backend.active().path.as_ref(), Some(&target));

    let _ = fs::remove_file(&target);
}

#[test]
fn save_selection_action_pushes_current_cursor_to_jump_list() {
    let mut app = App::from_path(None).unwrap();
    app.backend.cursor_line = 3;
    app.backend.cursor_col = 4;
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('s'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::SaveSelection,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT)));

    assert_eq!(app.jump_list.last().copied(), Some((3, 4)));
}

#[test]
fn replace_action_waits_for_next_character() {
    let mut app = App::from_path(None).unwrap();
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('r'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::Replace,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT)));

    assert!(app.input_state.awaiting_replace_char);
    assert_eq!(app.active_key_hint_label().as_deref(), Some("replace"));
    let entries = app.active_key_hint_entries().expect("replace wait should show hints");
    assert!(
        entries
            .iter()
            .any(|entry| { entry.key == "char" && entry.description == "replacement character" })
    );
    assert!(entries.iter().any(|entry| entry.key == "Esc" && entry.description == "cancel"));
}

#[test]
fn esc_cancels_replace_wait_state() {
    let mut app = App::from_path(None).unwrap();
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('r'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::Replace,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT)));
    assert!(app.input_state.awaiting_replace_char);

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    assert!(!app.input_state.awaiting_replace_char);
}
