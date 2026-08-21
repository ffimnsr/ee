use std::env;
use std::fs;
use std::sync::mpsc::{self, TryRecvError};
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::Value;

use crate::app::{App, Mode};
use crate::buffer::BufferManager;
use crate::keymap::{Action, BindingKey, bindings};
use crate::picker::{PickerKind, PickerState};
use crate::quickfix::{QfEntry, QfList};
use crate::tests::helpers::{CurrentDirGuard, cwd_test_lock};
use crate::ui::ui;

#[test]
fn app_uses_configured_keymap_overrides() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join(".ee.toml"),
        r#"
[keymap]
inherit_defaults = true

[[keymap.unbind]]
mode = "normal"
key = "K"

[[keymap.bindings]]
mode = "normal"
key = "H"
action = "request_hover"
"#,
    )
    .unwrap();

    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    env::set_current_dir(temp.path()).unwrap();

    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('K'), KeyModifiers::NONE)));
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('H'), KeyModifiers::NONE)));
    let message = rx.recv().expect("message should be sent");

    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_hover");
}

#[test]
fn configured_keymap_parses_agent_local_actions() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join(".ee.toml"),
        r#"
[[keymap.bindings]]
mode = "agent"
key = "ctrl+r"
action = "agent_history_search_reverse"
"#,
    )
    .unwrap();

    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    env::set_current_dir(temp.path()).unwrap();
    let app = App::from_path(None).unwrap();

    assert_eq!(
        app.key_bindings.get(&BindingKey {
            mode: Mode::Agent,
            key: KeyCode::Char('r'),
            modifiers: KeyModifiers::CONTROL,
            prefix: None,
        }),
        Some(&Action::AgentHistorySearchReverse)
    );
}

#[test]
fn configured_keymap_can_unbind_insert_ctrl_shortcuts() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join(".ee.toml"),
        r#"
[keymap]
inherit_defaults = true

[[keymap.unbind]]
mode = "insert"
key = "ctrl+w"
"#,
    )
    .unwrap();

    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    env::set_current_dir(temp.path()).unwrap();

    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.mode = Mode::Insert;

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)));

    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn configured_keymap_can_bind_picker_navigation() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join(".ee.toml"),
        r#"
[keymap]
inherit_defaults = true

[[keymap.bindings]]
mode = "picker"
key = "j"
action = "picker_move_down"
"#,
    )
    .unwrap();

    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    env::set_current_dir(temp.path()).unwrap();

    let mut app = App::from_path(None).unwrap();
    app.open_picker(PickerState::new_help("Picker", ["first", "second"]));

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));

    assert_eq!(app.picker.as_ref().map(|picker| picker.selected), Some(1));
}

#[test]
fn configured_keymap_can_bind_quickfix_navigation() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join(".ee.toml"),
        r#"
[keymap]
inherit_defaults = true

[[keymap.bindings]]
mode = "quickfix"
key = "x"
action = "quickfix_move_down"
"#,
    )
    .unwrap();

    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    env::set_current_dir(temp.path()).unwrap();

    let mut app = App::from_path(None).unwrap();
    app.quickfix = Some(QfList::new(
        "Quickfix",
        vec![
            QfEntry { path: None, line: 0, col: 0, message: String::from("first") },
            QfEntry { path: None, line: 1, col: 0, message: String::from("second") },
        ],
    ));
    app.quickfix_focused = true;

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)));

    assert_eq!(app.quickfix.as_ref().map(|list| list.selected), Some(1));
}

#[test]
fn configured_keymap_can_bind_substitute_confirm_actions() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join(".ee.toml"),
        r#"
[keymap]
inherit_defaults = true

[[keymap.bindings]]
mode = "substitute_confirm"
key = "x"
action = "substitute_confirm_apply"
"#,
    )
    .unwrap();

    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    env::set_current_dir(temp.path()).unwrap();

    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    for ch in "alpha\nbeta\nalpha".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    app.backend.pump().unwrap();

    app.execute_substitute(0, 2, "a", "A", "c");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)));
    app.backend.pump().unwrap();

    assert_eq!(
        app.backend.lines,
        vec![String::from("Alpha"), String::from("beta"), String::from("alpha")]
    );
}

#[test]
fn configured_keymap_can_execute_nested_sequences() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join(".ee.toml"),
        r#"
[keymap]
inherit_defaults = true

[[keymap.sequence_bindings]]
mode = "normal"
keys = ["space", "f", "f"]
action = "command_palette"
description = "command palette"

[[keymap.sequence_bindings]]
mode = "normal"
keys = ["space", "f", "b"]
action = "buffer_picker"
description = "buffer picker"
"#,
    )
    .unwrap();

    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    env::set_current_dir(temp.path()).unwrap();

    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)));
    assert_eq!(app.active_key_sequence_label().as_deref(), Some("SPC"));

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)));
    assert_eq!(app.active_key_sequence_label().as_deref(), Some("SPC f"));
    assert!(app.active_key_sequence_node().is_some());

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)));

    let picker = app.picker.as_ref().expect("command palette should open");
    assert_eq!(picker.kind, PickerKind::Help);
    assert_eq!(picker.title, "Command Palette");
    assert!(app.active_key_sequence_node().is_none());
}

#[test]
fn prefix_binding_exposes_key_hints_for_follow_up_keys() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE)));

    assert_eq!(app.active_key_hint_label().as_deref(), Some("z"));
    let entries = app.active_key_hint_entries().expect("z prefix should show hints");
    assert_eq!(
        entries.first().map(|entry| (entry.key.as_str(), entry.description.as_str())),
        Some(("Esc", "cancel"))
    );
    assert!(entries.iter().any(|entry| entry.key == "a" && entry.description == "toggle fold"));
    assert!(entries.iter().any(|entry| entry.key == "o" && entry.description == "open fold"));
    assert!(entries.iter().any(|entry| entry.key == "R" && entry.description == "open all folds"));
    assert!(entries.iter().any(|entry| entry.key == "Esc" && entry.description == "cancel"));
}

#[test]
fn esc_cancels_prefix_hint_state() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE)));
    assert_eq!(app.input_state.prefix, Some('g'));

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));

    assert!(app.input_state.prefix.is_none());
    assert!(app.active_key_hint_label().is_none());
    assert_eq!(app.backend.status_message.as_deref(), Some("pending input cancelled"));
}

#[test]
fn window_command_prefix_exposes_key_hints() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)));

    assert_eq!(app.active_key_hint_label().as_deref(), Some("Ctrl+w"));
    let entries = app.active_key_hint_entries().expect("window prefix should show hints");
    assert!(
        entries.iter().any(|entry| entry.key == "s" && entry.description == "split horizontally")
    );
    assert!(entries.iter().any(|entry| entry.key == "o" && entry.description == "only window"));
}

#[test]
fn register_prefix_exposes_key_hints() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('"'), KeyModifiers::NONE)));

    assert_eq!(app.active_key_hint_label().as_deref(), Some("\""));
    let entries = app.active_key_hint_entries().expect("register prefix should show hints");
    assert!(entries.iter().any(|entry| {
        entry.key == "a-z / A-Z" && entry.description == "named register / append"
    }));
    assert!(
        entries.iter().any(|entry| entry.key == "+" && entry.description == "system clipboard")
    );
    assert!(
        entries.iter().any(|entry| entry.key == "1-9" && entry.description == "delete history")
    );
}

#[test]
fn mark_prefixes_expose_key_hints() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE)));

    assert_eq!(app.active_key_hint_label().as_deref(), Some("m"));
    let set_entries = app.active_key_hint_entries().expect("mark set prefix should show hints");
    assert!(
        set_entries.iter().any(|entry| entry.key == "a-z" && entry.description == "named mark")
    );

    app.input_state.awaiting_mark_set = false;
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('\''), KeyModifiers::NONE)));

    assert_eq!(app.active_key_hint_label().as_deref(), Some("'"));
    let jump_entries = app.active_key_hint_entries().expect("mark jump prefix should show hints");
    assert!(
        jump_entries
            .iter()
            .any(|entry| entry.key == "a-z" && entry.description == "named mark line")
    );
    assert!(
        jump_entries
            .iter()
            .any(|entry| entry.key == "`" && entry.description == "previous jump line")
    );
}

#[test]
fn custom_prefix_binding_uses_human_readable_action_description() {
    let mut app = App::from_path(None).unwrap();
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('x'),
            modifiers: KeyModifiers::NONE,
            prefix: Some('g'),
        },
        Action::ReplaceSelectionsWithPrimaryClipboard,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE)));

    let entries = app.active_key_hint_entries().expect("g prefix should show hints");
    assert!(entries.iter().any(|entry| {
        entry.key == "x" && entry.description == "replace selections with primary clipboard"
    }));
}

#[test]
fn default_spc_tree_times_out_after_idle() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)));
    assert_eq!(app.active_key_sequence_label().as_deref(), Some("SPC"));

    let now = Instant::now();
    let timeout = Duration::from_millis(app.config.keymap.sequence_timeout_ms);
    app.input_state.key_sequence_last_input_at = Some(now - timeout - Duration::from_millis(1));

    app.expire_key_sequence_if_idle_at(now);

    assert!(app.active_key_sequence_node().is_none());
    assert!(app.active_key_sequence_label().is_none());
}

#[test]
fn configured_key_sequence_timeout_is_applied() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join(".ee.toml"),
        r#"
[keymap]
inherit_defaults = true
sequence_timeout_ms = 25

[[keymap.sequence_bindings]]
mode = "normal"
keys = ["space", "f", "f"]
action = "command_palette"
description = "command palette"
"#,
    )
    .unwrap();

    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    env::set_current_dir(temp.path()).unwrap();

    let mut app = App::from_path(None).unwrap();
    assert_eq!(app.config.keymap.sequence_timeout_ms, 25);

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)));
    assert_eq!(app.active_key_sequence_label().as_deref(), Some("SPC"));

    let now = Instant::now();
    app.input_state.key_sequence_last_input_at = Some(now - Duration::from_millis(26));

    app.expire_key_sequence_if_idle_at(now);

    assert!(app.active_key_sequence_node().is_none());
    assert!(app.active_key_sequence_label().is_none());
}

#[test]
fn default_spc_tree_exposes_root_categories() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)));

    let hints = app.active_key_sequence_node().expect("default SPC tree should activate");
    let descriptions =
        hints.hint_entries().into_iter().map(|entry| entry.description).collect::<Vec<_>>();

    assert!(descriptions.iter().any(|description| description == "files"));
    assert!(descriptions.iter().any(|description| description == "buffers"));
    assert!(descriptions.iter().any(|description| description == "code"));
}

#[test]
fn default_spc_tree_can_open_command_palette() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)));

    let picker = app.picker.as_ref().expect("command palette should open");
    assert_eq!(picker.kind, PickerKind::Help);
    assert_eq!(picker.title, "Command Palette");
}

#[test]
fn default_spc_tree_works_in_visual_mode() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Visual);

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)));

    let picker = app.picker.as_ref().expect("command palette should open from visual mode");
    assert_eq!(picker.kind, PickerKind::Help);
    assert_eq!(picker.title, "Command Palette");
}

#[test]
fn default_spc_tree_stays_disabled_in_insert_mode() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Insert);

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)));
    assert!(app.active_key_sequence_node().is_none());
    assert!(app.active_key_sequence_label().is_none());

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)));
    app.backend.pump().unwrap();

    assert_eq!(app.mode, Mode::Insert);
    assert_eq!(app.backend.lines, vec![String::from(" pp")]);
    assert!(app.picker.is_none());
    assert!(app.active_key_sequence_node().is_none());
}

#[test]
fn nested_sequence_hints_render_in_bottom_panel() {
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

[[keymap.sequence_bindings]]
mode = "normal"
keys = ["space", "f", "b"]
action = "buffer_picker"
description = "list buffers"
"#,
    )
    .unwrap();

    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    env::set_current_dir(temp.path()).unwrap();

    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)));

    let backend = TestBackend::new(80, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let buffer = terminal.backend().buffer();
    let mut screen = String::new();
    for y in 0..12 {
        for x in 0..80 {
            screen.push_str(buffer.cell((x, y)).unwrap().symbol());
        }
        screen.push('\n');
    }

    assert!(screen.contains("keys"), "screen missing active sequence title label: {screen}");
    assert!(screen.contains("SPC"), "screen missing active sequence prefix: {screen}");
    assert!(screen.contains("f"), "screen missing active sequence tail: {screen}");
    assert!(screen.contains("find files"), "screen missing leaf description: {screen}");
    assert!(screen.contains("list buffers"), "screen missing sibling description: {screen}");
    assert!(
        !screen.contains("->"),
        "sequence hints should match prefix styling without arrow markers: {screen}"
    );
}

#[test]
fn nested_sequence_hints_fill_columns_top_to_bottom_and_mute_prefix_title() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join(".ee.toml"),
        r#"
[keymap]
inherit_defaults = false

[[keymap.sequence_bindings]]
mode = "normal"
keys = ["space", "f"]
action = "no_op"
description = "files"

[[keymap.sequence_bindings]]
mode = "normal"
keys = ["space", "f", "a"]
action = "file_picker"
description = "alpha"

[[keymap.sequence_bindings]]
mode = "normal"
keys = ["space", "f", "b"]
action = "buffer_picker"
description = "beta"

[[keymap.sequence_bindings]]
mode = "normal"
keys = ["space", "f", "c"]
action = "command_palette"
description = "gamma"

[[keymap.sequence_bindings]]
mode = "normal"
keys = ["space", "f", "d"]
action = "global_search"
description = "delta"
"#,
    )
    .unwrap();

    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    env::set_current_dir(temp.path()).unwrap();

    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)));

    let backend = TestBackend::new(50, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let buffer = terminal.backend().buffer();

    let mut screen = String::new();
    for y in 0..12 {
        for x in 0..50 {
            screen.push_str(buffer.cell((x, y)).unwrap().symbol());
        }
        screen.push('\n');
    }

    assert!(screen.contains("keys"), "screen missing title label: {screen}");
    assert!(screen.contains("SPC"), "screen missing sequence prefix in title: {screen}");
    assert!(screen.contains("f"), "screen missing current sequence key in title: {screen}");
    assert!(screen.contains("Esc cancel"), "screen missing sequence cancel hint: {screen}");
    let alpha_row = screen.lines().position(|line| line.contains("alpha")).unwrap();
    let beta_row = screen.lines().position(|line| line.contains("beta")).unwrap();
    let gamma_row = screen.lines().position(|line| line.contains("gamma")).unwrap();
    let delta_row = screen.lines().position(|line| line.contains("delta")).unwrap();
    let esc_row = screen.lines().position(|line| line.contains("Esc cancel")).unwrap();
    assert_eq!(esc_row, gamma_row, "expected first row to hold cancel and gamma columns: {screen}");
    assert_eq!(
        alpha_row, delta_row,
        "expected second row to hold alpha and delta columns: {screen}"
    );
    assert!(
        beta_row > alpha_row,
        "expected beta to flow into the last row after cancel takes first cell: {screen}"
    );

    let (title_y, title_line) = screen
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("keys") && line.contains("SPC") && line.contains('f'))
        .unwrap();
    let spc_x = title_line.find("SPC").unwrap() as u16;
    let f_x =
        title_line[spc_x as usize + 3..].find('f').map(|offset| spc_x + 3 + offset as u16).unwrap();
    let spc_cell = buffer.cell((spc_x, title_y as u16)).unwrap();
    let f_cell = buffer.cell((f_x, title_y as u16)).unwrap();
    assert_eq!(spc_cell.bg, f_cell.bg, "title keys should not use different background fills");
    assert_ne!(spc_cell.fg, f_cell.fg, "prefix key should be more muted than current key");
}

#[test]
fn key_hint_panel_and_prompt_share_chrome_background() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)));

    let backend = TestBackend::new(80, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let buffer = terminal.backend().buffer();

    let mut prompt_bg = None;
    let mut hint_bg = None;
    for y in 0..10 {
        for x in 0..80 {
            let cell = buffer.cell((x, y)).unwrap();
            if prompt_bg.is_none() && cell.symbol() == "k" {
                let next = buffer.cell((x + 1, y)).unwrap();
                if next.symbol() == "e" {
                    prompt_bg = Some(cell.bg);
                }
            }
            if hint_bg.is_none() && cell.symbol() == "E" {
                let next = buffer.cell((x + 1, y)).unwrap();
                if next.symbol() == "s" {
                    hint_bg = Some(cell.bg);
                }
            }
        }
    }

    assert_eq!(prompt_bg, hint_bg, "prompt and key hint chrome should share background");
}

#[test]
fn git_bindings_are_registered() {
    let b = bindings();
    let lookup = |key, prefix| {
        b.get(&BindingKey { mode: Mode::Normal, key, modifiers: KeyModifiers::NONE, prefix })
            .cloned()
    };

    assert_eq!(lookup(KeyCode::Char('h'), Some(']')), Some(Action::GitNextHunk));
    assert_eq!(lookup(KeyCode::Char('h'), Some('[')), Some(Action::GitPrevHunk));
    assert_eq!(lookup(KeyCode::Char('b'), Some('g')), Some(Action::GitBlame));
    assert_eq!(lookup(KeyCode::Char('D'), Some('g')), Some(Action::GitDiff));
}

#[test]
fn ctrl_up_and_down_bind_multi_cursor_actions() {
    let b = bindings();
    let up = b
        .get(&BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Up,
            modifiers: KeyModifiers::CONTROL,
            prefix: None,
        })
        .cloned();
    let down = b
        .get(&BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Down,
            modifiers: KeyModifiers::CONTROL,
            prefix: None,
        })
        .cloned();
    assert_eq!(up, Some(Action::Edit("add_selection_above")));
    assert_eq!(down, Some(Action::Edit("add_selection_below")));
}

#[test]
fn ctrl_a_and_x_bind_number_adjustments() {
    let b = bindings();
    let up = b
        .get(&BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('a'),
            modifiers: KeyModifiers::CONTROL,
            prefix: None,
        })
        .cloned();
    let down = b
        .get(&BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('x'),
            modifiers: KeyModifiers::CONTROL,
            prefix: None,
        })
        .cloned();
    assert_eq!(up, Some(Action::Edit("increase_number")));
    assert_eq!(down, Some(Action::Edit("decrease_number")));
}

#[test]
fn ctrl_p_and_ctrl_alt_p_bind_normal_mode_picker_shortcuts() {
    let b = bindings();
    let file_picker = b
        .get(&BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('p'),
            modifiers: KeyModifiers::CONTROL,
            prefix: None,
        })
        .cloned();
    let command_palette = b
        .get(&BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('p'),
            modifiers: KeyModifiers::CONTROL | KeyModifiers::ALT,
            prefix: None,
        })
        .cloned();
    let insert_file_picker = b
        .get(&BindingKey {
            mode: Mode::Insert,
            key: KeyCode::Char('p'),
            modifiers: KeyModifiers::CONTROL,
            prefix: None,
        })
        .cloned();
    let insert_command_palette = b
        .get(&BindingKey {
            mode: Mode::Insert,
            key: KeyCode::Char('p'),
            modifiers: KeyModifiers::CONTROL | KeyModifiers::ALT,
            prefix: None,
        })
        .cloned();

    assert_eq!(file_picker, Some(Action::FilePickerInCurrentDirectory));
    assert_eq!(command_palette, Some(Action::CommandPalette));
    assert_eq!(insert_file_picker, None);
    assert_eq!(insert_command_palette, None);
}

#[test]
fn gd_binds_duplicate_line() {
    let b = bindings();
    let lookup = b
        .get(&BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('d'),
            modifiers: KeyModifiers::NONE,
            prefix: Some('g'),
        })
        .cloned();
    assert_eq!(lookup, Some(Action::Edit("duplicate_line")));
}
