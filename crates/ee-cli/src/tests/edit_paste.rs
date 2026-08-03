use std::sync::mpsc;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;
use xi_core_lib::plugin_rpc::SelectionRange;

use crate::app::{App, Mode};
use crate::buffer::BufferManager;
use crate::keymap::{Action, BindingKey};
use crate::registers::{ClipboardSelection, RegisterName, set_test_clipboard};
use crate::tests::helpers::*;

#[test]
fn normal_mode_paste_uses_backend_register_paste() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.registers.yank(&crate::registers::RegisterName::Unnamed, String::from("hello"), false);

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "paste_register");
    assert_eq!(value["params"]["params"]["chars"], "hello");
    assert_eq!(value["params"]["params"]["before"], false);
}

#[test]
fn clipboard_paste_commands_use_expected_clipboard_register() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    set_test_clipboard(ClipboardSelection::Clipboard, "clip");
    set_test_clipboard(ClipboardSelection::Primary, "prim");

    for (command, expected, before) in [
        ("paste_clipboard_after", "clip", false),
        ("paste_clipboard_before", "clip", true),
        ("paste_primary_clipboard_after", "prim", false),
        ("paste_primary_clipboard_before", "prim", true),
    ] {
        run_ex(&mut app, command);

        let message = rx.recv().expect("message should be sent");
        let value: Value = serde_json::from_str(&message).expect("message should be json");
        assert_eq!(value["method"], "edit");
        assert_eq!(value["params"]["method"], "paste_register");
        assert_eq!(value["params"]["params"]["chars"], expected);
        assert_eq!(value["params"]["params"]["before"], before);
    }
}

#[test]
fn clipboard_yank_and_replace_commands_use_test_clipboards() {
    let mut app = App::from_path(None).unwrap();
    insert_text(&mut app, "alpha beta");
    wait_until_with_backend(
        &mut app.backend,
        "seed clipboard buffer",
        std::time::Duration::from_secs(5),
        |backend| backend.lines == vec![String::from("alpha beta")],
    );

    app.backend.set_selections(&[SelectionRange { start: 0, end: 5 }]).unwrap();
    run_ex(&mut app, "yank_to_clipboard");
    assert_eq!(app.registers.get(&RegisterName::Clipboard), "alpha");

    app.backend
        .set_selections(&[
            SelectionRange { start: 0, end: 5 },
            SelectionRange { start: 6, end: 10 },
        ])
        .unwrap();
    app.backend.cursor_line = 1;
    app.backend.cursor_col = 0;
    run_ex(&mut app, "yank_main_selection_to_primary_clipboard");
    assert_eq!(app.registers.get(&RegisterName::PrimaryClipboard), "beta");

    set_test_clipboard(ClipboardSelection::Clipboard, "CLIP");
    app.backend.set_selections(&[SelectionRange { start: 0, end: 5 }]).unwrap();
    run_ex(&mut app, "replace_selections_with_clipboard");
    wait_until_with_backend(
        &mut app.backend,
        "replace selections with clipboard",
        std::time::Duration::from_secs(5),
        |backend| backend.lines == vec![String::from("CLIP beta")],
    );
    assert_eq!(app.backend.lines, vec![String::from("CLIP beta")]);

    set_test_clipboard(ClipboardSelection::Primary, "PRIM");
    app.backend.set_selections(&[SelectionRange { start: 5, end: 9 }]).unwrap();
    run_ex(&mut app, "replace_selections_with_primary_clipboard");
    wait_until_with_backend(
        &mut app.backend,
        "replace selections with primary clipboard",
        std::time::Duration::from_secs(5),
        |backend| backend.lines == vec![String::from("CLIP PRIM")],
    );
    assert_eq!(app.backend.lines, vec![String::from("CLIP PRIM")]);
}

#[test]
fn duplicate_line_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":duplicate_line".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "duplicate_line");
}

#[test]
fn move_line_down_command_swaps_with_next_line() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = vec!["alpha".to_owned(), "beta".to_owned(), "gamma".to_owned()];
    app.backend.cursor_line = 0;
    app.backend.cursor_col = 2;

    run_ex(&mut app, "move_line_down");

    let first: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(first["params"]["method"], "gesture");
    assert_eq!(first["params"]["params"]["line"], 0);
    assert_eq!(first["params"]["params"]["col"], 0);

    let second: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(second["params"]["method"], "gesture");
    assert_eq!(second["params"]["params"]["line"], 1);
    assert_eq!(second["params"]["params"]["col"], 4);

    let third: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(third["params"]["method"], "insert");
    assert_eq!(third["params"]["params"]["chars"], "beta\nalpha");

    let fourth: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(fourth["params"]["method"], "gesture");
    assert_eq!(fourth["params"]["params"]["line"], 1);
    assert_eq!(fourth["params"]["params"]["col"], 2);
}

#[test]
fn move_line_up_command_swaps_with_previous_line() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = vec!["alpha".to_owned(), "beta".to_owned()];
    app.backend.cursor_line = 1;
    app.backend.cursor_col = 1;

    run_ex(&mut app, "move_line_up");

    let _ = rx.recv().unwrap();
    let second: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(second["params"]["params"]["col"], 4);

    let third: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(third["params"]["method"], "insert");
    assert_eq!(third["params"]["params"]["chars"], "beta\nalpha");

    let fourth: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(fourth["params"]["params"]["line"], 0);
    assert_eq!(fourth["params"]["params"]["col"], 1);
}

#[test]
fn match_brackets_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    run_ex(&mut app, "match_brackets");

    let value: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(value["params"]["method"], "move_to_matching_bracket");
    assert_eq!(value["params"]["params"]["modify_selection"], false);
}

#[test]
fn select_textobject_inner_command_selects_requested_range() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = vec!["foo(bar)baz".to_owned()];
    app.backend.cursor_col = 4;

    run_ex(&mut app, "select_textobject_inner b");

    let first: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(first["params"]["method"], "gesture");
    assert_eq!(first["params"]["params"]["col"], 4);

    let second: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(second["params"]["method"], "gesture");
    assert_eq!(second["params"]["params"]["col"], 7);
}

#[test]
fn select_textobject_around_command_selects_outer_range() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = vec!["foo(bar)baz".to_owned()];
    app.backend.cursor_col = 4;

    run_ex(&mut app, "select_textobject_around b");

    let first: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(first["params"]["params"]["col"], 3);

    let second: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(second["params"]["params"]["col"], 8);
}

#[test]
fn surround_add_command_wraps_textobject() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = vec!["alpha beta".to_owned()];
    app.backend.cursor_col = 1;

    run_ex(&mut app, "surround_add [ w");

    let _ = rx.recv().unwrap();
    let _ = rx.recv().unwrap();
    let third: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(third["params"]["method"], "insert");
    assert_eq!(third["params"]["params"]["chars"], "[alpha]");
}

#[test]
fn surround_replace_command_rewrites_current_surround() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = vec!["foo(bar)baz".to_owned()];
    app.backend.cursor_col = 4;

    run_ex(&mut app, "surround_replace [");

    let _ = rx.recv().unwrap();
    let _ = rx.recv().unwrap();
    let third: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(third["params"]["method"], "insert");
    assert_eq!(third["params"]["params"]["chars"], "[bar]");
}

#[test]
fn surround_delete_command_rewrites_current_surround() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = vec!["foo(bar)baz".to_owned()];
    app.backend.cursor_col = 4;

    run_ex(&mut app, "surround_delete");

    let _ = rx.recv().unwrap();
    let _ = rx.recv().unwrap();
    let third: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(third["params"]["method"], "insert");
    assert_eq!(third["params"]["params"]["chars"], "bar");
}

#[test]
fn reindent_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in [':', 'r', 'e', 'i', 'n', 'd', 'e', 'n', 't'] {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "reindent");
}

#[test]
fn expandtab_command_uses_backend_edit_from_command_mode() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in [':', 'e', 'x', 'p', 'a', 'n', 'd', 't', 'a', 'b'] {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "expand_tabs");
}

#[test]
fn toggle_comment_commands_use_backend_edit() {
    for (command, method) in [
        ("toggle_comments", "toggle_comment"),
        ("toggle_line_comments", "toggle_line_comment"),
        ("toggle_block_comments", "toggle_block_comment"),
    ] {
        let (tx, rx) = mpsc::channel();
        let (_backend_tx, backend_rx) = mpsc::channel();
        let mut app = App::from_path(None).unwrap();
        app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

        run_ex(&mut app, command);

        let message = rx.recv().expect("message should be sent");
        let value: Value = serde_json::from_str(&message).expect("message should be json");
        assert_eq!(value["method"], "edit");
        assert_eq!(value["params"]["method"], method);
    }
}

#[test]
fn multi_find_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":multi_find alpha beta".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "multi_find");
    assert_eq!(value["params"]["params"]["queries"].as_array().map(Vec::len), Some(2));
}

#[test]
fn search_selection_alias_uses_plain_find_query() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = vec![String::from("alpha beta")];
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('*'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::SearchSelection { detect_word_boundaries: false },
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('*'), KeyModifiers::ALT)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "find");
    assert_eq!(value["params"]["params"]["chars"], "alpha");
    assert_eq!(value["params"]["params"]["whole_words"], false);
}

#[test]
fn search_selection_detect_word_boundaries_uses_whole_word_find_query() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = vec![String::from("alpha beta")];
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('#'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::SearchSelection { detect_word_boundaries: true },
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('#'), KeyModifiers::ALT)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "find");
    assert_eq!(value["params"]["params"]["chars"], "alpha");
    assert_eq!(value["params"]["params"]["whole_words"], true);
}

#[test]
fn syntax_selection_actions_forward_backend_methods() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char(']'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::SelectNextSibling,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::ALT)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "select_next_sibling");
}

#[test]
fn move_parent_node_action_forwards_backend_method() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('P'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::MoveParentNodeEnd,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('P'), KeyModifiers::ALT)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "move_parent_node_end");
}

#[test]
fn goto_column_action_uses_count_as_target_column() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.input_state.count_digits = vec![3];
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('c'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::GotoColumn,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::ALT)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "goto_column");
    assert_eq!(value["params"]["params"]["display_col"], 2);
    assert_eq!(value["params"]["params"]["modify_selection"], false);
}

#[test]
fn extend_line_below_action_uses_count_as_backend_param() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.input_state.count_digits = vec![3];
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('E'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::ExtendLineBelow,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('E'), KeyModifiers::ALT)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "extend_line_below");
    assert_eq!(value["params"]["params"]["count"], 3);
}

#[test]
fn select_all_children_command_sends_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)));
    for ch in "select_all_children".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "select_all_children");
}

#[test]
fn move_parent_node_start_command_sends_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)));
    for ch in "move_parent_node_start".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "move_parent_node_start");
}
