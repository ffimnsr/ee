use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};
use xi_core_lib::plugin_rpc::SelectionRange;

use crate::app::{App, Mode};
use crate::backend::{CachedLine, LineSlot};
use crate::buffer::BufferManager;
use crate::picker::PickerKind;
use crate::registers::RegisterName;
use crate::tests::helpers::*;

#[test]
fn goto_alias_emits_gesture_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = (0..20).map(|_| String::new()).collect();

    run_ex(&mut app, "g 12");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "gesture");
    assert_eq!(value["params"]["params"]["line"], 11);
}

#[test]
fn write_bang_update_and_x_bang_aliases_save() {
    let first = unique_temp_path("ee-cli-write-bang");
    fs::write(&first, "seed").unwrap();

    let mut app = App::from_path(Some(first.clone())).unwrap();
    insert_text(&mut app, "!");
    run_ex(&mut app, "w!");

    for _ in 0..20 {
        if fs::read_to_string(&first).unwrap().starts_with('!') {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(fs::read_to_string(&first).unwrap().starts_with('!'));

    let second = unique_temp_path("ee-cli-update-bang");
    fs::write(&second, "seed").unwrap();
    let mut update_app = App::from_path(Some(second.clone())).unwrap();
    insert_text(&mut update_app, "?");
    run_ex(&mut update_app, "u");

    for _ in 0..20 {
        if fs::read_to_string(&second).unwrap().starts_with('?') {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(fs::read_to_string(&second).unwrap().starts_with('?'));

    let third = unique_temp_path("ee-cli-x-bang");
    fs::write(&third, "seed").unwrap();
    let mut quit_app = App::from_path(Some(third.clone())).unwrap();
    insert_text(&mut quit_app, "#");
    run_ex(&mut quit_app, "x!");

    for _ in 0..20 {
        if fs::read_to_string(&third).unwrap().starts_with('#') {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(fs::read_to_string(&third).unwrap().starts_with('#'));
    assert!(quit_app.should_quit);

    let _ = fs::remove_file(&first);
    let _ = fs::remove_file(&second);
    let _ = fs::remove_file(&third);
}

#[test]
fn write_all_and_write_quit_all_aliases_cover_hidden_buffers() {
    let first = unique_temp_path("ee-cli-wa-first");
    let second = unique_temp_path("ee-cli-wa-second");
    fs::write(&first, "seed").unwrap();
    fs::write(&second, "seed").unwrap();

    let mut app = App::from_path(Some(first.clone())).unwrap();
    insert_text(&mut app, "1");
    run_ex(&mut app, &format!("e {}", second.display()));
    insert_text(&mut app, "2");
    run_ex(&mut app, "wa");

    for _ in 0..20 {
        let first_saved = fs::read_to_string(&first).unwrap().starts_with('1');
        let second_saved = fs::read_to_string(&second).unwrap().starts_with('2');
        if first_saved && second_saved {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(fs::read_to_string(&first).unwrap().starts_with('1'));
    assert!(fs::read_to_string(&second).unwrap().starts_with('2'));

    insert_text(&mut app, "3");
    run_ex(&mut app, &format!("e {}", first.display()));
    insert_text(&mut app, "4");
    run_ex(&mut app, "xa");

    for _ in 0..20 {
        let first_saved = fs::read_to_string(&first).unwrap().starts_with('4');
        let second_saved = fs::read_to_string(&second).unwrap().starts_with("23");
        if first_saved && second_saved {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(fs::read_to_string(&first).unwrap().starts_with('4'));
    assert!(fs::read_to_string(&second).unwrap().starts_with("23"));
    assert!(app.should_quit);

    let _ = fs::remove_file(&first);
    let _ = fs::remove_file(&second);
}

#[test]
fn quit_all_alias_checks_hidden_dirty_buffers_and_force_variant() {
    let first = unique_temp_path("ee-cli-qa-first");
    let second = unique_temp_path("ee-cli-qa-second");
    fs::write(&first, "seed").unwrap();
    fs::write(&second, "seed").unwrap();

    let mut app = App::from_path(Some(first.clone())).unwrap();
    insert_text(&mut app, "!");
    run_ex(&mut app, &format!("e {}", second.display()));

    run_ex(&mut app, "qa");
    assert!(!app.should_quit);
    assert_eq!(
        app.backend.status_message.as_deref(),
        Some("unsaved changes (use :wa to save or :qa! to force)")
    );

    run_ex(&mut app, "qa!");
    assert!(app.should_quit);

    let _ = fs::remove_file(&first);
    let _ = fs::remove_file(&second);
}

#[test]
fn read_command_inserts_file_contents() {
    let source = unique_temp_path("ee-cli-read-source");
    fs::write(&source, "alpha\nbeta\n").unwrap();

    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    run_ex(&mut app, &format!("r {}", source.display()));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "insert");
    assert_eq!(value["params"]["params"]["chars"], "alpha\nbeta\n");
    let expected = format!("read {}", source.display());
    assert_eq!(app.backend.status_message.as_deref(), Some(expected.as_str()));

    let _ = fs::remove_file(&source);
}

#[test]
fn move_command_moves_dirty_buffer_to_new_path() {
    let source = unique_temp_path("ee-cli-move-source");
    let target = unique_temp_path("ee-cli-move-target");
    fs::write(&source, "seed").unwrap();

    let mut app = App::from_path(Some(source.clone())).unwrap();
    insert_text(&mut app, "!");
    run_ex(&mut app, &format!("mv {}", target.display()));

    for _ in 0..20 {
        let moved = !source.exists() && target.exists();
        let saved = moved && fs::read_to_string(&target).unwrap().starts_with('!');
        if saved {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }

    assert!(!source.exists());
    assert!(target.exists());
    assert_eq!(app.backend.active().path.as_ref(), Some(&target));
    assert!(fs::read_to_string(&target).unwrap().starts_with('!'));

    let _ = fs::remove_file(&target);
}

#[test]
fn reload_config_refreshes_runtime_settings() {
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join(".ee.toml"), "cursor_line = false\n").unwrap();

    env::set_current_dir(temp.path()).unwrap();

    let mut app = App::from_path(None).unwrap();
    assert!(!app.config.cursor_line);

    fs::write(temp.path().join(".ee.toml"), "cursor_line = true\n").unwrap();
    run_ex(&mut app, "reload_config");

    assert!(app.config.cursor_line);
    assert_eq!(app.backend.status_message.as_deref(), Some("config reloaded"));
}

#[test]
fn reload_config_refreshes_runtime_sequence_keymap() {
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join(".ee.toml"),
        r#"
[keymap]
inherit_defaults = true

[[keymap.sequence_bindings]]
mode = "normal"
keys = ["space", "f", "f"]
action = "file_picker"
description = "find files"
"#,
    )
    .unwrap();

    env::set_current_dir(temp.path()).unwrap();

    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)));
    let hints = app.active_key_sequence_node().expect("sequence hints should be active");
    let descriptions =
        hints.hint_entries().into_iter().map(|entry| entry.description).collect::<Vec<_>>();
    assert!(descriptions.iter().any(|description| description == "find files"));

    fs::write(
        temp.path().join(".ee.toml"),
        r#"
[keymap]
inherit_defaults = true

[[keymap.sequence_bindings]]
mode = "normal"
keys = ["space", "f", "f"]
action = "file_picker"
description = "project files"
"#,
    )
    .unwrap();

    app.input_state.reset();
    run_ex(&mut app, "reload_config");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)));
    let hints = app.active_key_sequence_node().expect("reloaded sequence hints should be active");
    let descriptions =
        hints.hint_entries().into_iter().map(|entry| entry.description).collect::<Vec<_>>();
    assert!(descriptions.iter().any(|description| description == "project files"));
}

#[test]
fn language_encoding_echo_register_and_redraw_commands_update_state() {
    let mut app = App::from_path(None).unwrap();

    run_ex(&mut app, "set_language rust");
    assert_eq!(
        app.syntax_overrides.get(&app.backend.active().id).map(String::as_str),
        Some("rust")
    );
    assert_eq!(app.backend.status_message.as_deref(), Some("language: rust"));

    run_ex(&mut app, "set_language");
    assert_eq!(app.backend.status_message.as_deref(), Some("language: rust"));

    run_ex(&mut app, "encoding utf-16");
    assert_eq!(app.config.charset, "utf-16");
    assert_eq!(app.backend.status_message.as_deref(), Some("encoding: utf-16"));

    run_ex(&mut app, "echo hello status");
    assert_eq!(app.backend.status_message.as_deref(), Some("hello status"));

    app.registers.yank(&RegisterName::Named('a'), String::from("alpha"), false);
    run_ex(&mut app, "clear_register a");
    assert!(app.registers.get(&RegisterName::Named('a')).is_empty());
    assert_eq!(app.backend.status_message.as_deref(), Some("register a cleared"));

    run_ex(&mut app, "clear_register");
    assert!(app.registers.get(&RegisterName::Unnamed).is_empty());
    assert_eq!(app.backend.status_message.as_deref(), Some("registers cleared"));

    run_ex(&mut app, "redraw");
    assert!(app.redraw_requested);
    assert_eq!(app.backend.status_message.as_deref(), Some("redraw"));
}

#[test]
fn global_search_and_command_palette_commands_open_expected_pickers() {
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    let temp = tempfile::tempdir().unwrap();
    env::set_current_dir(temp.path()).unwrap();

    let mut app = App::from_path(None).unwrap();
    run_ex(&mut app, "global_search");
    let picker = app.picker.as_ref().expect("global search should open picker");
    assert_eq!(picker.kind, PickerKind::LiveGrep);
    assert_eq!(picker.title, "Global Search");

    app.picker = None;
    run_ex(&mut app, "command_palette");
    let picker = app.picker.as_ref().expect("command palette should open picker");
    assert_eq!(picker.kind, PickerKind::Help);
    assert_eq!(picker.title, "Command Palette");
}

#[test]
fn command_palette_renders_tab_and_historic_alias_rows() {
    let mut app = App::from_path(None).unwrap();

    run_ex(&mut app, "command_palette");

    let picker = app.picker.as_ref().expect("command palette should open picker");
    let items = picker.visible_items_range(0, picker.visible_count());
    assert!(items.iter().any(|line| line.contains(":tabnew / :tabe / :tabedit [path]")));
    assert!(items.iter().any(|line| line.contains(":config_reload")));
    assert!(items.iter().any(|line| line.contains(":bpick / :buffer_picker")));
    assert!(
        items.iter().any(|line| line.contains(":s / :substitute s/pattern/replacement/[flags]"))
    );
}

#[test]
fn ctrl_p_opens_cwd_file_picker_from_normal_mode() {
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    let temp = tempfile::tempdir().unwrap();
    env::set_current_dir(temp.path()).unwrap();
    fs::write(temp.path().join("sample.rs"), "fn main() {}\n").unwrap();

    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)));

    let picker = app.picker.as_ref().expect("ctrl+p should open picker");
    assert_eq!(picker.kind, PickerKind::Files);
    assert_eq!(picker.title, "Files (cwd)");
}

#[test]
fn ctrl_alt_p_opens_command_palette_from_normal_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Char('p'),
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    )));

    let picker = app.picker.as_ref().expect("ctrl+alt+p should open picker");
    assert_eq!(picker.kind, PickerKind::Help);
    assert_eq!(picker.title, "Command Palette");
}

#[test]
fn insert_mode_does_not_use_normal_mode_picker_shortcuts() {
    let mut app = App::from_path(None).unwrap();
    app.mode = Mode::Insert;

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)));
    assert!(app.picker.is_none());

    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Char('p'),
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    )));
    assert!(app.picker.is_none());
}

#[test]
fn insert_register_action_inserts_named_register_contents_in_insert_mode() {
    let mut app = App::from_path(None).unwrap();
    app.registers.yank(&RegisterName::Named('a'), String::from("alpha"), false);
    app.mode = Mode::Insert;

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)));
    assert!(app.input_state.awaiting_register);
    assert!(app.input_state.awaiting_register_insert);
    assert_eq!(app.pending_input_label().as_deref(), Some("insert register | press register name"));

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)));
    wait_until_with_backend(
        &mut app.backend,
        "insert register into scratch buffer",
        Duration::from_secs(1),
        |backend| backend.lines == vec![String::from("alpha")],
    );

    assert_eq!(app.backend.lines, vec![String::from("alpha")]);
    assert!(!app.input_state.awaiting_register);
    assert!(!app.input_state.awaiting_register_insert);
}

#[test]
fn cd_pwd_and_lsp_commands_update_status() {
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    let temp = tempfile::tempdir().unwrap();

    let mut app = App::from_path(None).unwrap();
    run_ex(&mut app, &format!("cd {}", temp.path().display()));
    assert_eq!(
        std::env::current_dir().unwrap().canonicalize().unwrap(),
        temp.path().canonicalize().unwrap()
    );
    assert!(
        app.backend.status_message.as_deref().unwrap().contains(&temp.path().display().to_string())
    );

    run_ex(&mut app, "pwd");
    assert!(
        app.backend.status_message.as_deref().unwrap().contains(&temp.path().display().to_string())
    );

    run_ex(&mut app, "lsp_restart");
    assert_eq!(app.backend.status_message.as_deref(), Some("lsp restart requested"));

    run_ex(&mut app, "lsp_stop");
    assert_eq!(app.backend.status_message.as_deref(), Some("lsp stop requested"));
}

#[test]
fn pipe_commands_transform_and_filter_selections() {
    let mut app = App::from_path(None).unwrap();
    insert_text(&mut app, "ab");
    app.backend.pump().unwrap();

    app.backend.set_selections(&[SelectionRange { start: 0, end: 2 }]).unwrap();
    wait_until_with_backend(
        &mut app.backend,
        "initial pipe selection",
        Duration::from_secs(2),
        |backend| {
            backend.annotations.iter().any(|annotation| {
                annotation.annotation_type == "selection"
                    && annotation.ranges.as_slice() == [[0, 0, 0, 2]]
            })
        },
    );
    run_ex(&mut app, "| tr a-z A-Z");
    wait_until_with_backend(
        &mut app.backend,
        "pipe replace output",
        Duration::from_secs(2),
        |backend| backend.lines.as_slice() == [String::from("AB")],
    );
    assert_eq!(app.backend.lines, vec![String::from("AB")]);

    app.backend.set_selections(&[SelectionRange { start: 0, end: 1 }]).unwrap();
    wait_until_with_backend(
        &mut app.backend,
        "insert-output selection",
        Duration::from_secs(2),
        |backend| {
            backend.annotations.iter().any(|annotation| {
                annotation.annotation_type == "selection"
                    && annotation.ranges.as_slice() == [[0, 0, 0, 1]]
            })
        },
    );
    run_ex(&mut app, "shell_insert_output printf x");
    wait_until_with_backend(
        &mut app.backend,
        "insert-output text",
        Duration::from_secs(2),
        |backend| backend.lines.as_slice() == [String::from("xAB")],
    );
    assert_eq!(app.backend.lines, vec![String::from("xAB")]);

    app.backend
        .set_selections(&[SelectionRange { start: 0, end: 1 }, SelectionRange { start: 1, end: 2 }])
        .unwrap();
    wait_until_with_backend(
        &mut app.backend,
        "keep-pipe selections",
        Duration::from_secs(2),
        |backend| {
            backend.annotations.iter().any(|annotation| {
                annotation.annotation_type == "selection"
                    && annotation.ranges.as_slice() == [[0, 0, 0, 1], [0, 1, 0, 2]]
            })
        },
    );
    run_ex(&mut app, "shell_keep_pipe grep -q x");
    wait_until_with_backend(
        &mut app.backend,
        "keep-pipe filtered text",
        Duration::from_secs(2),
        |backend| backend.selected_text_preview(false).ok().as_deref() == Some("x"),
    );
    let kept = app.backend.selected_text_preview(false).unwrap();
    assert_eq!(kept, "x");
}

#[test]
fn pipe_to_and_append_output_commands_run_shell_without_replacing_buffer() {
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("pipe.txt");

    let mut app = App::from_path(None).unwrap();
    insert_text(&mut app, "abc");
    app.backend.pump().unwrap();
    app.backend.set_selections(&[SelectionRange { start: 0, end: 3 }]).unwrap();
    wait_until_with_backend(
        &mut app.backend,
        "pipe-to selection",
        Duration::from_secs(2),
        |backend| {
            backend.annotations.iter().any(|annotation| {
                annotation.annotation_type == "selection"
                    && annotation.ranges.as_slice() == [[0, 0, 0, 3]]
            })
        },
    );

    run_ex(&mut app, &format!("pipe_to cat > {}", output.display()));
    assert_eq!(fs::read_to_string(&output).unwrap(), "abc");
    assert_eq!(app.backend.lines, vec![String::from("abc")]);

    app.backend.set_selections(&[SelectionRange { start: 1, end: 2 }]).unwrap();
    wait_until_with_backend(
        &mut app.backend,
        "append-output selection",
        Duration::from_secs(2),
        |backend| {
            backend.annotations.iter().any(|annotation| {
                annotation.annotation_type == "selection"
                    && annotation.ranges.as_slice() == [[0, 1, 0, 2]]
            })
        },
    );
    run_ex(&mut app, "shell_append_output printf z");
    wait_until_with_backend(
        &mut app.backend,
        "append-output text",
        Duration::from_secs(2),
        |backend| backend.lines.as_slice() == [String::from("abzc")],
    );
    assert_eq!(app.backend.lines, vec![String::from("abzc")]);
}

#[test]
fn shell_selection_commands_use_app_working_dir_not_process_cwd() {
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    let deleted_cwd = tempfile::tempdir().unwrap();

    let mut app = App::from_path(None).unwrap();
    insert_text(&mut app, "a");
    app.backend.pump().unwrap();
    app.backend.set_selections(&[SelectionRange { start: 0, end: 1 }]).unwrap();
    wait_until_with_backend(
        &mut app.backend,
        "stable shell selection",
        Duration::from_secs(2),
        |backend| {
            backend.annotations.iter().any(|annotation| {
                annotation.annotation_type == "selection"
                    && annotation.ranges.as_slice() == [[0, 0, 0, 1]]
            })
        },
    );

    env::set_current_dir(deleted_cwd.path()).unwrap();
    drop(deleted_cwd);

    run_ex(&mut app, "shell_append_output printf b");
    wait_until_with_backend(
        &mut app.backend,
        "stable shell output",
        Duration::from_secs(2),
        |backend| backend.lines.as_slice() == [String::from("ab")],
    );
    assert_eq!(app.backend.lines, vec![String::from("ab")]);
}

#[test]
fn sort_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    run_ex(&mut app, "sort");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "sort_lines");
    assert_eq!(value["params"]["params"]["descending"], Value::Bool(false));
    assert_eq!(value["params"]["params"]["range"], Value::Null);
}

#[test]
fn rsort_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    run_ex(&mut app, "rsort");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "sort_lines");
    assert_eq!(value["params"]["params"]["descending"], Value::Bool(true));
    assert_eq!(value["params"]["params"]["range"], Value::Null);
}

#[test]
fn reflow_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    run_ex(&mut app, "reflow 10");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "reflow_lines");
    assert_eq!(value["params"]["params"]["width"], json!(10));
    assert_eq!(value["params"]["params"]["range"], Value::Null);
}

#[test]
fn expandtab_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    run_ex(&mut app, "expandtab");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "expand_tabs");
    assert_eq!(value["params"]["params"]["range"], Value::Null);
}

#[test]
fn renormalize_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    run_ex(&mut app, "renormalize");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "normalize_line_endings");
    assert_eq!(value["params"]["params"]["line_ending"], Value::String(String::from("\n")));
}

#[test]
fn dedup_commands_remove_duplicate_lines() {
    let mut app = App::from_path(None).unwrap();
    insert_text(&mut app, "a\nb\na\nb\nc");
    wait_until_with_backend(
        &mut app.backend,
        "seed dedup whole buffer",
        Duration::from_secs(1),
        |backend| {
            backend.lines
                == vec![
                    String::from("a"),
                    String::from("b"),
                    String::from("a"),
                    String::from("b"),
                    String::from("c"),
                ]
        },
    );

    run_ex(&mut app, "dedup");
    wait_until_with_backend(
        &mut app.backend,
        "dedup whole buffer",
        Duration::from_secs(1),
        |backend| backend.lines == vec![String::from("a"), String::from("b"), String::from("c")],
    );
    assert_eq!(app.backend.lines, vec![String::from("a"), String::from("b"), String::from("c")]);

    let mut selected = App::from_path(None).unwrap();
    insert_text(&mut selected, "keep\nx\nx\ny\nx");
    wait_until_with_backend(
        &mut selected.backend,
        "seed dedup line range",
        Duration::from_secs(1),
        |backend| {
            backend.lines
                == vec![
                    String::from("keep"),
                    String::from("x"),
                    String::from("x"),
                    String::from("y"),
                    String::from("x"),
                ]
        },
    );
    run_ex(&mut selected, "2,4uniq");
    wait_until_with_backend(
        &mut selected.backend,
        "dedup line range",
        Duration::from_secs(1),
        |backend| {
            backend.lines
                == vec![
                    String::from("keep"),
                    String::from("x"),
                    String::from("y"),
                    String::from("x"),
                ]
        },
    );
    assert_eq!(
        selected.backend.lines,
        vec![String::from("keep"), String::from("x"), String::from("y"), String::from("x"),]
    );
}

#[test]
fn diffget_restores_current_git_hunk_from_head() {
    let temp = tempfile::tempdir().unwrap();
    init_test_git_repo(temp.path());

    let path = temp.path().join("sample.rs");
    fs::write(&path, "one\ntwo\nthree\n").unwrap();
    run_git(temp.path(), &["add", "sample.rs"]);
    run_git(temp.path(), &["commit", "-m", "init"]);

    let mut app = App::from_path(Some(path)).unwrap();
    app.backend.set_selections(&[SelectionRange { start: 4, end: 7 }]).unwrap();
    let _ = app.backend.send_edit("delete_forward", json!([]));
    let _ = app.backend.send_edit("insert", json!({ "chars": "TWO" }));
    wait_until_with_backend(
        &mut app.backend,
        "seed diff hunk",
        Duration::from_secs(1),
        |backend| {
            backend.lines.starts_with(&[
                String::from("one"),
                String::from("TWO"),
                String::from("three"),
            ])
        },
    );
    app.backend.cursor_line = 1;

    run_ex(&mut app, "diffget");
    wait_until_with_backend(
        &mut app.backend,
        "diffget restore hunk",
        Duration::from_secs(1),
        |backend| {
            backend.lines.starts_with(&[
                String::from("one"),
                String::from("two"),
                String::from("three"),
            ])
        },
    );

    assert!(app.backend.lines.starts_with(&[
        String::from("one"),
        String::from("two"),
        String::from("three"),
    ]));
}

#[test]
fn vlf_source_control_commands_report_disabled_reason() {
    let mut app = App::from_path(None).unwrap();
    app.backend.is_vlf = true;
    app.backend.path = Some(PathBuf::from("/tmp/huge.rs"));
    app.backend.line_cache = vec![LineSlot::Known(CachedLine {
        text: String::from("visible"),
        cursors: vec![0],
        syntax_spans: Vec::new(),
    })];

    for command in ["goto_next_change", "gblame", "gdiff", "ghunkdiff", "diffget"] {
        app.backend.status_message = None;
        run_ex(&mut app, command);

        let message = app.backend.status_message.clone().unwrap_or_default();
        assert!(message.contains("disabled in VLF"), "command {command} message was {message:?}");
        assert!(
            message.contains("whole-buffer diff/blame scans"),
            "command {command} message was {message:?}"
        );
    }
}

#[test]
fn reload_and_reload_all_aliases_refresh_from_disk() {
    let first = unique_temp_path("ee-cli-reload-first");
    let second = unique_temp_path("ee-cli-reload-second");
    fs::write(&first, "old-one\n").unwrap();
    fs::write(&second, "old-two\n").unwrap();

    let mut app = App::from_path(Some(first.clone())).unwrap();
    run_ex(&mut app, &format!("e {}", second.display()));
    fs::write(&first, "new-one\n").unwrap();
    fs::write(&second, "new-two\n").unwrap();

    run_ex(&mut app, "rl");
    for _ in 0..20 {
        app.backend.pump().unwrap();
        if app.backend.lines.first().is_some_and(|line| line == "new-two") {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(app.backend.lines.first().map(String::as_str), Some("new-two"));

    run_ex(&mut app, "rla");
    for _ in 0..20 {
        app.backend.pump().unwrap();
        let all_loaded = app.backend.all_bufs().iter().all(|buf| match buf.path.as_ref() {
            Some(path) if path == &first => buf.lines.first().is_some_and(|line| line == "new-one"),
            Some(path) if path == &second => {
                buf.lines.first().is_some_and(|line| line == "new-two")
            }
            _ => true,
        });
        if all_loaded {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(app.backend.all_bufs().iter().any(|buf| {
        buf.path.as_ref() == Some(&first) && buf.lines.first().is_some_and(|line| line == "new-one")
    }));
    assert!(app.backend.all_bufs().iter().any(|buf| {
        buf.path.as_ref() == Some(&second)
            && buf.lines.first().is_some_and(|line| line == "new-two")
    }));

    let _ = fs::remove_file(&first);
    let _ = fs::remove_file(&second);
}

#[test]
fn buffer_close_aliases_and_force_variants_work() {
    let first = unique_temp_path("ee-cli-bc-first");
    let second = unique_temp_path("ee-cli-bc-second");
    let third = unique_temp_path("ee-cli-bc-third");
    fs::write(&first, "one\n").unwrap();
    fs::write(&second, "two\n").unwrap();
    fs::write(&third, "three\n").unwrap();

    let mut app = App::from_path(Some(first.clone())).unwrap();
    run_ex(&mut app, &format!("e {}", second.display()));
    insert_text(&mut app, "!");

    run_ex(&mut app, "bc");
    assert_eq!(app.backend.buf_count(), 2);
    assert_eq!(
        app.backend.status_message.as_deref(),
        Some("unsaved changes (use :write to save or :bc! to force)")
    );

    run_ex(&mut app, "bc!");
    assert_eq!(app.backend.buf_count(), 1);
    assert_eq!(app.backend.active().path.as_ref(), Some(&first));

    run_ex(&mut app, &format!("e {}", second.display()));
    run_ex(&mut app, &format!("e {}", third.display()));
    assert_eq!(app.backend.buf_count(), 3);

    run_ex(&mut app, "bco");
    assert_eq!(app.backend.buf_count(), 1);
    assert_eq!(app.backend.active().path.as_ref(), Some(&third));

    run_ex(&mut app, &format!("e {}", first.display()));
    run_ex(&mut app, "bca");
    assert_eq!(app.backend.buf_count(), 1);
    assert!(app.backend.active().path.is_none());

    let _ = fs::remove_file(&first);
    let _ = fs::remove_file(&second);
    let _ = fs::remove_file(&third);
}

#[test]
fn goto_buffer_commands_cycle_open_buffers() {
    let first = unique_temp_path("ee-cli-goto-buffer-first");
    let second = unique_temp_path("ee-cli-goto-buffer-second");
    let third = unique_temp_path("ee-cli-goto-buffer-third");
    fs::write(&first, "one\n").unwrap();
    fs::write(&second, "two\n").unwrap();
    fs::write(&third, "three\n").unwrap();

    let mut app = App::from_path(Some(first.clone())).unwrap();
    run_ex(&mut app, &format!("e {}", second.display()));
    run_ex(&mut app, &format!("e {}", third.display()));

    run_ex(&mut app, "goto_next_buffer");
    assert_eq!(app.backend.active().path.as_ref(), Some(&first));

    run_ex(&mut app, "goto_previous_buffer");
    assert_eq!(app.backend.active().path.as_ref(), Some(&third));

    let _ = fs::remove_file(&first);
    let _ = fs::remove_file(&second);
    let _ = fs::remove_file(&third);
}

#[test]
fn goto_recent_file_commands_follow_access_and_modify_history() {
    let first = unique_temp_path("ee-cli-goto-recent-first");
    let second = unique_temp_path("ee-cli-goto-recent-second");
    fs::write(&first, "one\n").unwrap();
    fs::write(&second, "two\n").unwrap();

    let mut app = App::from_path(Some(first.clone())).unwrap();
    run_ex(&mut app, &format!("e {}", second.display()));

    run_ex(&mut app, "goto_last_accessed_file");
    assert_eq!(app.backend.active().path.as_ref(), Some(&first));

    app.push_change();
    run_ex(&mut app, "goto_next_buffer");
    app.push_change();
    run_ex(&mut app, "goto_last_modified_file");
    assert_eq!(app.backend.active().path.as_ref(), Some(&first));

    let _ = fs::remove_file(&first);
    let _ = fs::remove_file(&second);
}

#[test]
fn goto_window_commands_jump_within_visible_viewport() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = (0..100).map(|idx| format!("line {idx}")).collect();
    app.viewport.top_line = 10;
    app.last_editor_height = 20;

    for (command, expected_line) in [
        ("goto_window_top", 15_u64),
        ("goto_window_center", 19_u64),
        ("goto_window_bottom", 24_u64),
    ] {
        run_ex(&mut app, command);

        let value: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
            .expect("message should be json");
        assert_eq!(value["method"], "edit");
        assert_eq!(value["params"]["method"], "gesture");
        assert_eq!(value["params"]["params"]["line"], expected_line);
        assert_eq!(value["params"]["params"]["col"], 0);
    }
}

#[test]
fn create_directory_command_creates_nested_path_in_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    env::set_current_dir(temp.path()).unwrap();

    let mut app = App::from_path(None).unwrap();
    run_ex(&mut app, "create_directory alpha/beta");

    assert!(temp.path().join("alpha/beta").is_dir());
    assert_eq!(app.backend.status_message.as_deref(), Some("created alpha/beta"));
}

#[test]
fn create_directory_command_rejects_workspace_escape() {
    let temp = tempfile::tempdir().unwrap();
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    env::set_current_dir(temp.path()).unwrap();

    let mut app = App::from_path(None).unwrap();
    run_ex(&mut app, "create_directory ../escape");

    assert!(!temp.path().parent().unwrap().join("escape").exists());
    let message = app.backend.status_message.as_deref().unwrap_or_default();
    assert!(message.contains("path must stay under workspace"));
}
