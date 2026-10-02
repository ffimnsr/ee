//! `:undolist` picker tests: history listing and seeking to an entry.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use crate::app::App;
use crate::buffer::BufferManager;
use crate::picker::PickerKind;
use crate::tests::helpers::{run_ex, wait_until_with_backend};

fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    app.handle_event(Event::Key(KeyEvent::new(code, modifiers)));
}

fn type_text(app: &mut App, text: &str) {
    for ch in text.chars() {
        press(app, KeyCode::Char(ch), KeyModifiers::NONE);
    }
}

fn type_insert(app: &mut App, text: &str) {
    press(app, KeyCode::Char('i'), KeyModifiers::NONE);
    type_text(app, text);
    press(app, KeyCode::Esc, KeyModifiers::NONE);
}

fn run_undolist(app: &mut App) {
    press(app, KeyCode::Char(':'), KeyModifiers::NONE);
    type_text(app, "undolist");
    press(app, KeyCode::Enter, KeyModifiers::NONE);
}

fn open_text_file(text: &str) -> (tempfile::TempDir, App) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("sample.txt");
    std::fs::write(&path, text).unwrap();
    let mut app = App::from_path(Some(path)).unwrap();
    wait_until_with_backend(
        &mut app.backend,
        "line loaded",
        std::time::Duration::from_secs(15),
        |backend| backend.line_count() > 0,
    );
    (temp, app)
}

/// Three groups on the initial "abc": the load edit, an insert, and a second
/// insert forced into its own group via `commit_undo_checkpoint`.
fn app_with_three_groups() -> (tempfile::TempDir, App) {
    let (temp, mut app) = open_text_file("abc");
    type_insert(&mut app, "x");
    run_ex(&mut app, "commit_undo_checkpoint");
    type_insert(&mut app, "y");
    (temp, app)
}

#[test]
fn undolist_lists_history_preselecting_head_and_jumps_to_an_older_entry() {
    let (_temp, mut app) = app_with_three_groups();

    run_undolist(&mut app);
    let picker = app.picker.as_ref().expect("undolist should open picker");
    assert_eq!(picker.kind, PickerKind::UndoList);
    assert_eq!(picker.title, "Undo List");
    assert_eq!(picker.visible_count(), 3, "load + two edits must be three groups");
    // At head the current index equals groups.len(); the picker clamps the
    // preselected row to the last (most recent) entry.
    assert_eq!(picker.selected, 2);

    // Jump to the oldest seekable entry (row 0: the initial load group is
    // never undone): the core must rewind the current state to index 1.
    app.picker.as_mut().expect("picker still open").selected = 0;
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    run_undolist(&mut app);
    let picker = app.picker.as_ref().expect("undolist should reopen");
    assert_eq!(picker.kind, PickerKind::UndoList);
    assert_eq!(
        picker.selected,
        picker.visible_count() - 2,
        "current state must move to the oldest seekable entry"
    );
}

#[test]
fn undolist_after_undos_preselects_the_new_current_row() {
    let (_temp, mut app) = app_with_three_groups();

    // Two undos from head: current state moves two groups back.
    press(&mut app, KeyCode::Char('u'), KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('u'), KeyModifiers::NONE);

    run_undolist(&mut app);
    let picker = app.picker.as_ref().expect("undolist should open picker");
    assert_eq!(picker.kind, PickerKind::UndoList);
    assert_eq!(picker.selected, 1, "current row must be one back from head");
}

#[test]
fn undolist_confirm_on_current_row_is_a_noop() {
    let (_temp, mut app) = app_with_three_groups();

    press(&mut app, KeyCode::Char('u'), KeyModifiers::NONE);

    run_undolist(&mut app);
    let selected = app.picker.as_ref().expect("undolist should open picker").selected;
    assert_eq!(selected, 2, "current row is one back from head after one undo");
    // Confirm the preselected (current) row: history must not move.
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    run_undolist(&mut app);
    let picker = app.picker.as_ref().expect("undolist should reopen");
    assert_eq!(picker.kind, PickerKind::UndoList);
    assert_eq!(picker.selected, selected, "confirming current row must not move history");
}

#[test]
fn undolist_seek_roundtrip_restores_newer_state_with_redo() {
    let (_temp, mut app) = app_with_three_groups();

    // Jump to oldest, then back to head: redo path must restore the index.
    run_undolist(&mut app);
    app.picker.as_mut().expect("picker still open").selected = 0;
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    run_undolist(&mut app);
    let head = app.picker.as_ref().expect("undolist should reopen").visible_count() - 1;
    assert_eq!(head, 2);
    app.picker.as_mut().expect("picker still open").selected = head;
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    run_undolist(&mut app);
    let picker = app.picker.as_ref().expect("undolist should reopen");
    assert_eq!(picker.kind, PickerKind::UndoList);
    assert_eq!(
        picker.selected,
        picker.visible_count() - 1,
        "jumping to head must redo back to the newest entry"
    );
}

#[test]
fn undolist_reports_backend_failures_in_status() {
    let (_tx, backend_rx) = std::sync::mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    // A stub backend answers the undo_list request with an error.
    app.backend = BufferManager::test_new(
        std::sync::mpsc::channel().0,
        backend_rx,
        String::from("view-id-1"),
    );
    let pending = app.backend.pending_requests_for_test();
    let responder = std::thread::spawn(move || {
        for _ in 0..200 {
            let tx = pending.lock().unwrap_or_else(|e| e.into_inner()).values().next().cloned();
            if let Some(tx) = tx {
                let _ = tx.send(serde_json::json!({ "error": { "message": "boom" } }));
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    });

    run_undolist(&mut app);
    responder.join().unwrap();

    let message = app.backend.status_message.as_deref().unwrap_or_default();
    assert!(
        message.contains("undolist failed"),
        "expected undolist failure status, got {message:?}"
    );
    assert!(app.picker.is_none(), "failed undolist must not open a picker");
}
