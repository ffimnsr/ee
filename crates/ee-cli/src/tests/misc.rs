use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use serde_json::json;
use xi_core_lib::plugin_rpc::{Diagnostic, DiagnosticSeverity, Range, SelectionRange};

use crate::app::App;
use crate::backend::{CachedLine, LineSlot};
use crate::keymap::bindings;
use crate::picker::PickerKind;
use crate::tests::helpers::*;

// ── Picker / command tests ─────────────────────────────────────────────────────

#[test]
fn diagnostics_command_opens_location_list() {
    let mut app = App::from_path(None).unwrap();
    app.backend.diagnostics = vec![Diagnostic {
        range: Range { start: 0, end: 3 },
        severity: DiagnosticSeverity::Warning,
        message: String::from("warn"),
        source: Some(String::from("lsp")),
        code: None,
    }];

    for ch in ":diagnostics".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    assert!(app.location_list_open);
    assert_eq!(app.location_list.as_ref().map(|list| list.len()), Some(1));
}

#[test]
fn diagnostics_picker_command_opens_picker() {
    let mut app = App::from_path(None).unwrap();
    app.backend.diagnostics = vec![Diagnostic {
        range: Range { start: 0, end: 3 },
        severity: DiagnosticSeverity::Warning,
        message: String::from("warn"),
        source: Some(String::from("lsp")),
        code: None,
    }];

    run_ex(&mut app, "diagnostics_picker");

    let picker = app.picker.as_ref().expect("diagnostics picker should open");
    assert_eq!(picker.kind, PickerKind::Locations);
    assert_eq!(picker.title, "Diagnostics");
    assert_eq!(picker.visible_count(), 1);
}

#[test]
fn logs_command_opens_log_picker_modal() {
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    let temp = tempfile::tempdir().unwrap();
    let state_home = temp.path().join("state-home");
    let editor_log = temp.path().join("ee.log");
    let plugin_log = temp.path().join("xi-lsp-plugin.log");
    let _state_guard = EnvVarGuard::set("XDG_STATE_HOME", &state_home);
    let _editor_log_guard = EnvVarGuard::set("EE_EDITOR_LOG", &editor_log);
    let _plugin_log_guard = EnvVarGuard::set("EE_PLUGIN_LOG", &plugin_log);
    env::set_current_dir(temp.path()).unwrap();
    fs::write(&editor_log, "editor\n").unwrap();
    fs::write(&plugin_log, "plugin\n").unwrap();

    let mut app = App::from_path(None).unwrap();

    run_ex(&mut app, "logs");

    let picker = app.picker.as_ref().expect("logs should open picker");
    assert_eq!(picker.kind, PickerKind::Locations);
    assert_eq!(picker.title, "Logs");
    assert_eq!(picker.visible_count(), 2);
}

#[test]
fn append_editor_log_line_creates_discoverable_editor_log() {
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    let temp = tempfile::tempdir().unwrap();
    let state_home = temp.path().join("state-home");
    env::set_current_dir(temp.path()).unwrap();
    let _state_guard = EnvVarGuard::set("XDG_STATE_HOME", &state_home);

    let path = crate::logs::append_editor_log_line("test startup").unwrap();
    assert!(path.is_file());
    assert!(crate::logs::discover_log_paths().iter().any(|candidate| candidate.path == path));
}

#[test]
fn workspace_diagnostics_picker_command_aggregates_open_buffers() {
    let first = unique_temp_path("workspace-diag-a");
    let second = unique_temp_path("workspace-diag-b");
    fs::write(&first, "alpha\n").unwrap();
    fs::write(&second, "beta\n").unwrap();

    let mut app = App::from_path(Some(first.clone())).unwrap();
    let first_id = app.backend.active().id;
    let second_id = app.backend.open_buffer(Some(second.clone())).unwrap();
    app.backend.diagnostics = vec![Diagnostic {
        range: Range { start: 0, end: 1 },
        severity: DiagnosticSeverity::Warning,
        message: String::from("first warn"),
        source: None,
        code: None,
    }];
    app.backend.switch_to_id(second_id).unwrap();
    app.backend.diagnostics = vec![Diagnostic {
        range: Range { start: 0, end: 1 },
        severity: DiagnosticSeverity::Error,
        message: String::from("second err"),
        source: None,
        code: None,
    }];
    app.backend.switch_to_id(first_id).unwrap();

    run_ex(&mut app, "workspace_diagnostics_picker");

    let picker = app.picker.as_ref().expect("workspace diagnostics picker should open");
    assert_eq!(picker.kind, PickerKind::Locations);
    assert_eq!(picker.visible_count(), 2);
}

#[test]
fn jumplist_picker_command_opens_picker() {
    let mut app = App::from_path(None).unwrap();
    app.jump_list.push((1, 2));
    app.jump_list.push((3, 4));

    run_ex(&mut app, "jumplist_picker");

    let picker = app.picker.as_ref().expect("jumplist picker should open");
    assert_eq!(picker.kind, PickerKind::Locations);
    assert_eq!(picker.title, "Jumplist");
    assert_eq!(picker.visible_count(), 2);
}

#[test]
fn last_picker_command_reopens_previous_picker() {
    let mut app = App::from_path(None).unwrap();

    run_ex(&mut app, "buffer_picker");
    app.picker = None;
    run_ex(&mut app, "last_picker");

    let picker = app.picker.as_ref().expect("last picker should reopen picker");
    assert_eq!(picker.kind, PickerKind::Buffers);
}

#[test]
fn changed_file_picker_command_opens_picker() {
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    let temp = tempfile::tempdir().unwrap();
    env::set_current_dir(temp.path()).unwrap();

    let file = temp.path().join("sample.rs");
    fs::write(&file, "fn main() {}\n").unwrap();
    init_test_git_repo(temp.path());
    run_git(temp.path(), &["add", "sample.rs"]);
    run_git(temp.path(), &["commit", "-m", "init"]);
    fs::write(&file, "fn main() { println!(\"hi\"); }\n").unwrap();

    let mut app = App::from_path(Some(file.clone())).unwrap();
    run_ex(&mut app, "changed_file_picker");

    let picker = app.picker.as_ref().expect("changed file picker should open");
    assert_eq!(picker.kind, PickerKind::Locations);
    assert_eq!(picker.title, "Changed Files");
    assert!(picker.visible_items_range(0, 8).iter().any(|item| item.contains("sample.rs")));
}

#[test]
fn file_explorer_command_opens_workspace_root_picker() {
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    let temp = tempfile::tempdir().unwrap();
    env::set_current_dir(temp.path()).unwrap();

    let nested = temp.path().join("nested");
    fs::create_dir_all(&nested).unwrap();
    let root_file = temp.path().join("root.txt");
    let nested_file = nested.join("sample.rs");
    fs::write(&root_file, "root\n").unwrap();
    fs::write(&nested_file, "fn main() {}\n").unwrap();
    init_test_git_repo(temp.path());

    let mut app = App::from_path(Some(nested_file)).unwrap();
    run_ex(&mut app, "file_explorer");

    let picker = app.picker.as_ref().expect("file explorer should open");
    assert_eq!(picker.kind, PickerKind::Files);
    assert_eq!(picker.title, "Explorer");
    assert!(
        picker
            .visible_items_range(0, picker.visible_count())
            .iter()
            .any(|item| item.contains("root.txt"))
    );
}

#[test]
fn lowercase_files_command_opens_picker() {
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    let temp = tempfile::tempdir().unwrap();
    env::set_current_dir(temp.path()).unwrap();
    fs::write(temp.path().join("sample.rs"), "fn main() {}\n").unwrap();

    let mut app = App::from_path(None).unwrap();
    run_ex(&mut app, "files");

    let picker = app.picker.as_ref().expect("files should open picker");
    assert_eq!(picker.kind, PickerKind::Files);
    assert_eq!(picker.title, "Files (cwd)");
}

#[test]
fn lowercase_grep_command_opens_live_grep_picker() {
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    let temp = tempfile::tempdir().unwrap();
    env::set_current_dir(temp.path()).unwrap();
    fs::write(temp.path().join("sample.rs"), "fn main() {}\nlet value = 1;\n").unwrap();

    let mut app = App::from_path(None).unwrap();
    run_ex(&mut app, "grep value");

    let picker = app.picker.as_ref().expect("grep should open picker");
    assert_eq!(picker.kind, PickerKind::LiveGrep);
    assert_eq!(picker.visible_count(), 1);
}

#[test]
fn help_command_opens_help_picker() {
    let mut app = App::from_path(None).unwrap();

    for ch in [':', 'h', 'e', 'l', 'p'] {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let picker = app.picker.as_ref().expect("help picker should open");
    assert_eq!(picker.kind, PickerKind::Help);
    assert_eq!(picker.title, "Help");
    assert!(picker.visible_items_range(0, 8).iter().any(|line| line.contains(":commands")));
    assert!(!picker.visible_items_range(0, 8).iter().any(|line| line.contains(":protocol")));
}

// ── LSP / workspace config ─────────────────────────────────────────────────────

#[test]
fn markdown_symbols_use_tree_sitter_when_lsp_is_disabled() {
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    let temp = tempfile::tempdir().unwrap();
    let config_home = temp.path().join("xdg-config");
    fs::create_dir_all(&config_home).unwrap();
    let _xdg_guard = EnvVarGuard::set("XDG_CONFIG_HOME", &config_home);
    install_test_lsp_plugin(&config_home);

    env::set_current_dir(temp.path()).unwrap();
    fs::write(temp.path().join(".ee.toml"), "[lsp.servers.markdown]\nenabled = false\n").unwrap();
    let file = temp.path().join("README.md");
    fs::write(&file, "# Parent\n\n## Child\n\nNext\n====\n").unwrap();

    let mut app = App::from_path(Some(file)).unwrap();
    wait_until_with_backend(
        &mut app.backend,
        "LSP plugin startup",
        Duration::from_secs(5),
        |backend| {
            backend
                .available_plugins_for_current_view()
                .iter()
                .any(|plugin| plugin.name == crate::config::LSP_PLUGIN_NAME && plugin.running)
        },
    );

    app.backend.request_document_symbols().unwrap();
    wait_until_with_backend(
        &mut app.backend,
        "Tree-sitter document symbols",
        Duration::from_secs(5),
        |backend| !backend.pending_symbols.is_empty(),
    );

    let pending = app.backend.drain_pending_symbols();
    assert_eq!(pending.len(), 1);
    let symbols = &pending[0].2;
    assert_eq!(symbols.len(), 2);
    assert_eq!(symbols[0].name, "Parent");
    assert_eq!(symbols[0].children.len(), 1);
    assert_eq!(symbols[0].children[0].name, "Child");
    assert_eq!(symbols[1].name, "Next");
}

#[test]
fn project_lsp_config_starts_and_reloads_custom_server() {
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    let temp = tempfile::tempdir().unwrap();
    let config_home = temp.path().join("xdg-config");
    fs::create_dir_all(&config_home).unwrap();
    let _xdg_guard = EnvVarGuard::set("XDG_CONFIG_HOME", &config_home);
    install_test_lsp_plugin(&config_home);

    env::set_current_dir(temp.path()).unwrap();

    let fake_server = fake_server_binary_path();
    let config_path = temp.path().join(".ee.toml");
    let file = temp.path().join("main.gleam");
    let log_one = temp.path().join("lsp-one.jsonl");
    let log_two = temp.path().join("lsp-two.jsonl");
    fs::write(&file, "pub fn main() { 1 }\n").unwrap();
    write_lsp_config(&config_path, &fake_server, &log_one);

    let mut app = App::from_path(Some(file.clone())).unwrap();

    wait_until_with_backend(
        &mut app.backend,
        "initial custom LSP startup",
        Duration::from_secs(5),
        |_| log_contains_methods(&log_one, &["initialize", "textDocument/didOpen"]),
    );

    write_lsp_config(&config_path, &fake_server, &log_two);
    app.backend.reload_editor_config().unwrap();

    wait_until_with_backend(
        &mut app.backend,
        "reloaded custom LSP startup",
        Duration::from_secs(5),
        |_| log_contains_methods(&log_two, &["initialize", "textDocument/didOpen"]),
    );
    wait_until_with_backend(
        &mut app.backend,
        "original LSP shutdown",
        Duration::from_secs(5),
        |_| log_contains_methods(&log_one, &["shutdown", "exit"]),
    );
}

// ── Open / benchmark ──────────────────────────────────────────────────────────

#[test]
fn open_many_line_20mb_fixture_meets_first_render_budget() {
    assert_open_to_first_render_budget("many-line", budget_many_line);
}

#[test]
fn open_long_line_20mb_fixture_meets_first_render_budget() {
    assert_open_to_first_render_budget("long-line", budget_long_line);
}

// ── Insert-entry variants ─────────────────────────────────────────────────────

#[test]
fn open_hsplit_and_new_aliases_work() {
    let first = unique_temp_path("ee-cli-open-first");
    let second = unique_temp_path("ee-cli-open-second");
    fs::write(&first, "one\ntwo\nthree\n").unwrap();
    fs::write(&second, "alpha\nbeta\ngamma\n").unwrap();

    let mut app = App::from_path(Some(first.clone())).unwrap();

    run_ex(&mut app, &format!("open {}", second.display()));
    assert_eq!(app.backend.active().path.as_ref(), Some(&second));

    run_ex(&mut app, &format!("hs {}", first.display()));
    assert_eq!(app.tabs.focused_windows().windows().len(), 2);
    assert_eq!(app.tabs.focused_windows().split_dir, crate::window::SplitDir::Horizontal);
    assert_eq!(app.backend.active().path.as_ref(), Some(&first));

    run_ex(&mut app, "n");
    assert!(app.backend.active().path.is_none());

    let _ = fs::remove_file(&first);
    let _ = fs::remove_file(&second);
}

#[test]
fn same_file_vsplit_reuses_buffer_and_keeps_content() {
    let path = unique_temp_path("ee-cli-same-file-vsplit");
    fs::write(&path, "FIRST-SPLIT-LINE\nSECOND-SPLIT-LINE\nTHIRD-SPLIT-LINE\n").unwrap();

    let mut app = App::from_path(Some(path.clone())).unwrap();
    wait_until_with_backend(
        &mut app.backend,
        "initial split file",
        Duration::from_secs(2),
        |backend| backend.get_line(1).is_some(),
    );
    app.backend.cursor_line = 1;
    app.backend.cursor_col = 0;
    app.viewport.top_line = 1;

    run_ex(&mut app, &format!("vs {}", path.display()));

    let windows = app.tabs.focused_windows().windows();
    assert_eq!(windows.len(), 2);
    assert_eq!(app.tabs.focused_windows().split_dir, crate::window::SplitDir::Vertical);
    assert_eq!(windows[0].buffer_id, windows[1].buffer_id);
    assert_eq!(
        app.backend.all_bufs().iter().filter(|buf| buf.path.as_ref() == Some(&path)).count(),
        1
    );

    let rows = render_screen_rows(&app, 80, 8);
    assert!(
        count_rendered_occurrences(&rows, "SECOND-SPLIT-LINE") >= 2,
        "render should show same file in both vertical panes: {rows:?}"
    );

    app.backend.cursor_line = 0;
    app.backend.cursor_col = 0;
    app.viewport.top_line = 0;
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE)));
    wait_until_with_backend(
        &mut app.backend,
        "shared vertical split insert",
        Duration::from_secs(2),
        |backend| backend.get_line(0) == Some("XFIRST-SPLIT-LINE"),
    );
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    assert_eq!(app.backend.get_line(0), Some("XFIRST-SPLIT-LINE"));

    run_ex(&mut app, "jump_view_left");
    assert_eq!(app.backend.active().path.as_ref(), Some(&path));
    assert_eq!(app.backend.get_line(0), Some("XFIRST-SPLIT-LINE"));

    let rows = render_screen_rows(&app, 80, 8);
    assert!(
        count_rendered_occurrences(&rows, "XFIRST-SPLIT-LINE") >= 1,
        "edited shared buffer should stay visible in peer vertical pane: {rows:?}"
    );

    run_ex(&mut app, "wclose");
    assert_eq!(app.tabs.focused_windows().windows().len(), 1);
    assert_eq!(app.backend.active().path.as_ref(), Some(&path));
    assert_eq!(app.backend.get_line(0), Some("XFIRST-SPLIT-LINE"));

    let _ = fs::remove_file(&path);
}

#[test]
fn same_file_hsplit_reuses_buffer_and_keeps_content() {
    let path = unique_temp_path("ee-cli-same-file-hsplit");
    fs::write(&path, "FIRST-HSPLIT-LINE\nSECOND-HSPLIT-LINE\nTHIRD-HSPLIT-LINE\n").unwrap();

    let mut app = App::from_path(Some(path.clone())).unwrap();
    wait_until_with_backend(
        &mut app.backend,
        "initial split file",
        Duration::from_secs(2),
        |backend| backend.get_line(0).is_some(),
    );

    run_ex(&mut app, &format!("split {}", path.display()));

    let windows = app.tabs.focused_windows().windows();
    assert_eq!(windows.len(), 2);
    assert_eq!(app.tabs.focused_windows().split_dir, crate::window::SplitDir::Horizontal);
    assert_eq!(windows[0].buffer_id, windows[1].buffer_id);
    assert_eq!(
        app.backend.all_bufs().iter().filter(|buf| buf.path.as_ref() == Some(&path)).count(),
        1
    );

    let rows = render_screen_rows(&app, 80, 10);
    assert!(
        count_rendered_occurrences(&rows, "FIRST-HSPLIT-LINE") >= 2,
        "render should show same file in both horizontal panes: {rows:?}"
    );

    run_ex(&mut app, "jump_view_up");
    assert_eq!(app.backend.active().path.as_ref(), Some(&path));

    app.backend.cursor_line = 0;
    app.backend.cursor_col = 0;
    app.viewport.top_line = 0;
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::NONE)));
    wait_until_with_backend(
        &mut app.backend,
        "shared horizontal split insert",
        Duration::from_secs(2),
        |backend| backend.get_line(0) == Some("YFIRST-HSPLIT-LINE"),
    );
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    assert_eq!(app.backend.get_line(0), Some("YFIRST-HSPLIT-LINE"));

    let rows = render_screen_rows(&app, 80, 10);
    assert!(
        count_rendered_occurrences(&rows, "YFIRST-HSPLIT-LINE") >= 2,
        "edit should stay visible in both horizontal panes: {rows:?}"
    );

    run_ex(&mut app, "wclose");
    assert_eq!(app.tabs.focused_windows().windows().len(), 1);
    assert_eq!(app.backend.active().path.as_ref(), Some(&path));
    assert_eq!(app.backend.get_line(0), Some("YFIRST-HSPLIT-LINE"));

    let _ = fs::remove_file(&path);
}

#[test]
fn view_rotation_and_directional_jump_commands_follow_split_axis() {
    let first = unique_temp_path("ee-cli-view-a");
    let second = unique_temp_path("ee-cli-view-b");
    let third = unique_temp_path("ee-cli-view-c");
    fs::write(&first, "one\n").unwrap();
    fs::write(&second, "two\n").unwrap();
    fs::write(&third, "three\n").unwrap();

    let mut app = App::from_path(Some(first.clone())).unwrap();
    run_ex(&mut app, &format!("vs {}", second.display()));
    run_ex(&mut app, &format!("vs {}", third.display()));

    assert_eq!(app.backend.active().path.as_ref(), Some(&third));

    run_ex(&mut app, "jump_view_left");
    assert_eq!(app.backend.active().path.as_ref(), Some(&second));

    run_ex(&mut app, "jump_view_up");
    assert_eq!(app.backend.active().path.as_ref(), Some(&second));

    run_ex(&mut app, "jump_view_right");
    assert_eq!(app.backend.active().path.as_ref(), Some(&third));

    run_ex(&mut app, "rotate_view");
    assert_eq!(app.backend.active().path.as_ref(), Some(&first));

    run_ex(&mut app, "cycle_view");
    assert_eq!(app.backend.active().path.as_ref(), Some(&second));

    let _ = fs::remove_file(&first);
    let _ = fs::remove_file(&second);
    let _ = fs::remove_file(&third);
}

#[test]
fn reverse_transpose_and_window_close_commands_manage_views() {
    let first = unique_temp_path("ee-cli-view-rev-a");
    let second = unique_temp_path("ee-cli-view-rev-b");
    let third = unique_temp_path("ee-cli-view-rev-c");
    fs::write(&first, "one\n").unwrap();
    fs::write(&second, "two\n").unwrap();
    fs::write(&third, "three\n").unwrap();

    let mut app = App::from_path(Some(first.clone())).unwrap();
    wait_until_with_backend(
        &mut app.backend,
        "first view open",
        Duration::from_secs(5),
        |backend| backend.lines.first().map(String::as_str) == Some("one"),
    );
    run_ex(&mut app, &format!("vs {}", second.display()));
    wait_until_with_backend(
        &mut app.backend,
        "second view open",
        Duration::from_secs(5),
        |backend| {
            backend.active().path.as_ref() == Some(&second)
                && backend.lines.first().map(String::as_str) == Some("two")
        },
    );
    run_ex(&mut app, &format!("vs {}", third.display()));
    wait_until_with_backend(
        &mut app.backend,
        "third view open",
        Duration::from_secs(5),
        |backend| {
            backend.active().path.as_ref() == Some(&third)
                && backend.lines.first().map(String::as_str) == Some("three")
        },
    );

    run_ex(&mut app, "rotate_view_reverse");
    assert_eq!(app.backend.active().path.as_ref(), Some(&second));

    run_ex(&mut app, "transpose_view");
    assert_eq!(app.tabs.focused_windows().split_dir, crate::window::SplitDir::Horizontal);

    run_ex(&mut app, "wclose");
    assert_eq!(window_paths(&app), vec![first.clone(), third.clone()]);
    assert_eq!(app.backend.active().path.as_ref(), Some(&third));

    run_ex(&mut app, "wonly");
    assert_eq!(window_paths(&app), vec![third.clone()]);
    assert_eq!(app.backend.active().path.as_ref(), Some(&third));

    let _ = fs::remove_file(&first);
    let _ = fs::remove_file(&second);
    let _ = fs::remove_file(&third);
}

#[test]
fn swap_view_commands_reorder_windows_on_matching_axis() {
    let first = unique_temp_path("ee-cli-swap-a");
    let second = unique_temp_path("ee-cli-swap-b");
    let third = unique_temp_path("ee-cli-swap-c");
    let fourth = unique_temp_path("ee-cli-swap-d");
    fs::write(&first, "one\n").unwrap();
    fs::write(&second, "two\n").unwrap();
    fs::write(&third, "three\n").unwrap();
    fs::write(&fourth, "four\n").unwrap();

    let mut vertical = App::from_path(Some(first.clone())).unwrap();
    wait_until_with_backend(
        &mut vertical.backend,
        "vertical first view open",
        Duration::from_secs(5),
        |backend| backend.lines.first().map(String::as_str) == Some("one"),
    );
    run_ex(&mut vertical, &format!("vs {}", second.display()));
    wait_until_with_backend(
        &mut vertical.backend,
        "vertical second view open",
        Duration::from_secs(5),
        |backend| {
            backend.active().path.as_ref() == Some(&second)
                && backend.lines.first().map(String::as_str) == Some("two")
        },
    );
    run_ex(&mut vertical, &format!("vs {}", third.display()));
    wait_until_with_backend(
        &mut vertical.backend,
        "vertical third view open",
        Duration::from_secs(5),
        |backend| {
            backend.active().path.as_ref() == Some(&third)
                && backend.lines.first().map(String::as_str) == Some("three")
        },
    );
    run_ex(&mut vertical, "swap_view_left");
    assert_eq!(window_paths(&vertical), vec![first.clone(), third.clone(), second.clone()]);
    assert_eq!(vertical.backend.active().path.as_ref(), Some(&third));

    run_ex(&mut vertical, "swap_view_up");
    assert_eq!(window_paths(&vertical), vec![first.clone(), third.clone(), second.clone()]);
    assert_eq!(vertical.backend.active().path.as_ref(), Some(&third));

    let mut horizontal = App::from_path(Some(first.clone())).unwrap();
    wait_until_with_backend(
        &mut horizontal.backend,
        "horizontal first view open",
        Duration::from_secs(5),
        |backend| backend.lines.first().map(String::as_str) == Some("one"),
    );
    run_ex(&mut horizontal, &format!("hs {}", fourth.display()));
    wait_until_with_backend(
        &mut horizontal.backend,
        "horizontal fourth view open",
        Duration::from_secs(5),
        |backend| {
            backend.active().path.as_ref() == Some(&fourth)
                && backend.lines.first().map(String::as_str) == Some("four")
        },
    );
    run_ex(&mut horizontal, &format!("hs {}", second.display()));
    wait_until_with_backend(
        &mut horizontal.backend,
        "horizontal second view open",
        Duration::from_secs(5),
        |backend| {
            backend.active().path.as_ref() == Some(&second)
                && backend.lines.first().map(String::as_str) == Some("two")
        },
    );
    run_ex(&mut horizontal, "swap_view_up");
    assert_eq!(window_paths(&horizontal), vec![first.clone(), second.clone(), fourth.clone()]);
    assert_eq!(horizontal.backend.active().path.as_ref(), Some(&second));

    run_ex(&mut horizontal, "swap_view_left");
    assert_eq!(window_paths(&horizontal), vec![first.clone(), second.clone(), fourth.clone()]);
    assert_eq!(horizontal.backend.active().path.as_ref(), Some(&second));

    let _ = fs::remove_file(&first);
    let _ = fs::remove_file(&second);
    let _ = fs::remove_file(&third);
    let _ = fs::remove_file(&fourth);
}

// ── Git ────────────────────────────────────────────────────────────────────────

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
        Duration::from_secs(5),
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
        Duration::from_secs(5),
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

// ── Registers ─────────────────────────────────────────────────────────────────

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
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('"'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)));
    assert_eq!(app.input_state.pending_register, Some(RegisterName::Named('a')));

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('"'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE)));
    assert_eq!(app.input_state.pending_register, Some(RegisterName::Clipboard));

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('"'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('*'), KeyModifiers::NONE)));
    assert_eq!(app.input_state.pending_register, Some(RegisterName::PrimaryClipboard));
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
    assert!(!app.marks.contains_key(&'A'));
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
    assert_eq!(app.active_key_hint_label().as_deref(), Some("record macro"));
    let entries = app.active_key_hint_entries().expect("macro record wait should show hints");
    assert!(
        entries.iter().any(|entry| entry.key == "a-z" && entry.description == "macro register")
    );
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
    assert_eq!(app.active_key_hint_label().as_deref(), Some("replay macro"));
    let entries = app.active_key_hint_entries().expect("macro replay wait should show hints");
    assert!(entries.iter().any(|entry| entry.key == "@" && entry.description == "last macro"));
    assert!(entries.iter().any(|entry| entry.key == "a-z" && entry.description == "named macro"));
    // Sending '@' again (@@) should consume and trigger replay.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('@'), KeyModifiers::NONE)));
    // No crash; macro_register is None (not recording).
    assert!(app.macro_register.is_none());
}

// ── Tab page tests ────────────────────────────────────────────────────────────

#[test]
fn tabnew_command_opens_second_tab() {
    let mut app = App::from_path(None).unwrap();
    assert_eq!(app.tabs.tab_count(), 1);

    // :tabnew opens a new tab.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)));
    for ch in "tabnew".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    assert_eq!(app.tabs.tab_count(), 2);
    assert_eq!(app.tabs.focused_idx(), 1);
}

#[test]
fn tabc_command_closes_tab() {
    let mut app = App::from_path(None).unwrap();

    // Open two more tabs so there are 3 total.
    for cmd in [":tabnew", ":tabnew"] {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)));
        for ch in cmd[1..].chars() {
            app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
        }
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    }
    assert_eq!(app.tabs.tab_count(), 3);

    // :tabc closes current tab.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)));
    for ch in "tabc".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    assert_eq!(app.tabs.tab_count(), 2);
}

#[test]
fn tabn_cycles_to_next_tab() {
    let mut app = App::from_path(None).unwrap();

    // Open a second tab.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)));
    for ch in "tabnew".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    assert_eq!(app.tabs.focused_idx(), 1);

    // :tabn wraps around to tab 0.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)));
    for ch in "tabn".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    assert_eq!(app.tabs.focused_idx(), 0);
}

#[test]
fn gt_binding_moves_to_next_tab() {
    let mut app = App::from_path(None).unwrap();

    // Open a second tab via ex command.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)));
    for ch in "tabnew".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    assert_eq!(app.tabs.focused_idx(), 1);

    // `gt` (g prefix then t) should wrap to tab 0.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE)));

    assert_eq!(app.tabs.focused_idx(), 0);
}

#[test]
fn tabmanager_starts_with_one_tab() {
    let app = App::from_path(None).unwrap();
    assert_eq!(app.tabs.tab_count(), 1);
    assert_eq!(app.tabs.focused_idx(), 0);
}
