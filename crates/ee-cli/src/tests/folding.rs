use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

use crate::app::App;
use crate::ui::ui;

use super::helpers::{tree_sitter_test_lock, wait_until_with_backend};

fn press(app: &mut App, key: char) {
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE)));
}

#[test]
fn close_all_folds_large_rust_fixture_from_vim_keys() {
    let _tree_sitter_guard = tree_sitter_test_lock().lock().unwrap_or_else(|err| err.into_inner());
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("test_assets/sample-program.rs");
    let mut app = App::from_path(Some(path.clone())).unwrap();
    wait_until_with_backend(
        &mut app.backend,
        "open large Rust fold fixture",
        Duration::from_secs(5),
        |backend| backend.active().path.as_ref() == Some(&path) && backend.lines.len() > 7_000,
    );

    press(&mut app, 'z');
    press(&mut app, 'M');

    let buffer_id = app.backend.active().id;
    assert!(
        !app.folds.get(buffer_id).is_empty(),
        "zM returned no folds; status={:?}",
        app.backend.status_message
    );
}

#[test]
fn markdown_heading_folds_work_from_vim_keys() {
    let _tree_sitter_guard = tree_sitter_test_lock().lock().unwrap_or_else(|err| err.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("heading-folds.md");
    fs::write(
        &path,
        "# Parent\nparent body\n\n## Child\nchild body\n\n### Grandchild\ngrandchild body\n\n## Sibling\nsibling body\n\n# Next\nnext body\n",
    )
    .unwrap();
    let mut app = App::from_path(Some(path.clone())).unwrap();
    wait_until_with_backend(
        &mut app.backend,
        "open Markdown fold fixture",
        Duration::from_secs(5),
        |backend| backend.active().path.as_ref() == Some(&path) && backend.lines.len() >= 14,
    );

    press(&mut app, 'z');
    press(&mut app, 'c');
    let buffer_id = app.backend.active().id;
    assert_eq!(app.folds.fold_at(buffer_id, 0), Some((0, 11)));

    press(&mut app, 'z');
    press(&mut app, 'a');
    assert!(app.folds.get(buffer_id).is_empty());

    press(&mut app, 'z');
    press(&mut app, 'M');
    assert_eq!(app.folds.get(buffer_id), &[(0, 11), (3, 8), (6, 8), (9, 11), (12, 13)]);
}

#[test]
fn mouse_selected_sibling_folds_open_out_of_close_order() {
    let _tree_sitter_guard = tree_sitter_test_lock().lock().unwrap_or_else(|err| err.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("sibling-folds.md");
    fs::write(&path, "# A\nA body\n\n# B\nB body\n").unwrap();
    let mut app = App::from_path(Some(path.clone())).unwrap();
    wait_until_with_backend(
        &mut app.backend,
        "open sibling fold fixture",
        Duration::from_secs(5),
        |backend| backend.active().path.as_ref() == Some(&path) && backend.lines.len() >= 5,
    );
    let area = Rect { x: 0, y: 0, width: 80, height: 24 };

    press(&mut app, 'z');
    press(&mut app, 'a');
    app.handle_mouse_event_in_area(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row: 1,
            modifiers: KeyModifiers::NONE,
        },
        area,
    );
    wait_until_with_backend(
        &mut app.backend,
        "focus second rendered heading",
        Duration::from_secs(1),
        |backend| backend.cursor_line == 3,
    );
    press(&mut app, 'z');
    press(&mut app, 'a');

    app.handle_mouse_event_in_area(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row: 0,
            modifiers: KeyModifiers::NONE,
        },
        area,
    );
    wait_until_with_backend(
        &mut app.backend,
        "focus first rendered heading",
        Duration::from_secs(1),
        |backend| backend.cursor_line == 0,
    );
    press(&mut app, 'z');
    press(&mut app, 'a');

    let buffer_id = app.backend.active().id;
    assert_eq!(app.folds.fold_at(buffer_id, 0), None);
    assert_eq!(app.folds.fold_at(buffer_id, 3), Some((3, 4)));
}

#[test]
fn terminal_cursor_uses_fold_aware_rendered_row() {
    let mut app = App::from_path(None).unwrap();
    app.backend.lines = (0..6).map(|line| format!("line {line}")).collect();
    app.backend.cursor_line = 5;
    let buffer_id = app.backend.active().id;
    app.folds.close(buffer_id, 1, (1, 3));

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();

    assert_eq!(terminal.get_cursor_position().unwrap().y, 3);

    app.backend.cursor_line = 2;
    terminal.draw(|frame| ui(frame, &app)).unwrap();

    assert_eq!(terminal.get_cursor_position().unwrap().y, 1);
}

#[test]
fn scroll_into_view_treats_large_fold_as_one_rendered_row() {
    let mut app = App::from_path(None).unwrap();
    app.backend.lines = (0..120).map(|line| format!("line {line}")).collect();
    app.backend.cursor_line = 101;
    let buffer_id = app.backend.active().id;
    app.folds.close(buffer_id, 10, (10, 100));

    app.scroll_into_view(20, 80);
    assert_eq!(app.viewport.top_line, 0);

    app.viewport.top_line = 50;
    app.scroll_into_view(20, 80);
    assert_eq!(app.viewport.top_line, 6);
    assert!(!app.folds.is_hidden(buffer_id, app.viewport.top_line));
    assert_eq!(app.folds.line_for_rendered_row(buffer_id, app.viewport.top_line, 4, 120), Some(10));
}

#[test]
fn keyboard_motion_treats_sibling_folds_as_single_lines() {
    let _tree_sitter_guard = tree_sitter_test_lock().lock().unwrap_or_else(|err| err.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("keyboard-sibling-folds.md");
    fs::write(&path, "# A\nA body\n\n# B\nB body\n").unwrap();
    let mut app = App::from_path(Some(path.clone())).unwrap();
    wait_until_with_backend(
        &mut app.backend,
        "open keyboard sibling fold fixture",
        Duration::from_secs(5),
        |backend| backend.active().path.as_ref() == Some(&path) && backend.lines.len() >= 5,
    );

    press(&mut app, 'z');
    press(&mut app, 'a');
    press(&mut app, 'j');
    wait_until_with_backend(
        &mut app.backend,
        "skip first folded heading body",
        Duration::from_secs(1),
        |backend| backend.cursor_line == 3,
    );

    press(&mut app, 'z');
    press(&mut app, 'a');
    press(&mut app, 'k');
    wait_until_with_backend(
        &mut app.backend,
        "skip backward over first folded heading body",
        Duration::from_secs(1),
        |backend| backend.cursor_line == 0,
    );
    press(&mut app, 'z');
    press(&mut app, 'a');

    let buffer_id = app.backend.active().id;
    assert_eq!(app.backend.cursor_line, 0);
    assert_eq!(app.folds.fold_at(buffer_id, 0), None);
    assert_eq!(app.folds.fold_at(buffer_id, 3), Some((3, 4)));
}
