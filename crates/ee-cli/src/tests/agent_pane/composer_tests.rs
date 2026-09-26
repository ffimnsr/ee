//! `impl App` agents-pane tests: composer caret movement and editing.
use super::*;

fn composer_app() -> (App, tempfile::TempDir) {
    let script = base_script().wait_for("session/prompt");
    let (mut app, temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);
    begin_fixture_turn(&mut app, &fake);
    (app, temp)
}

fn draft(app: &App) -> &str {
    &app.agents.threads[0].draft
}

fn cursor(app: &App) -> usize {
    app.agents.threads[0].draft_cursor
}

#[test]
fn left_right_move_caret_and_typing_inserts_at_it() {
    let (mut app, _temp) = composer_app();
    type_text(&mut app, "abc");
    press(&mut app, KeyCode::Left, KeyModifiers::NONE);
    press(&mut app, KeyCode::Left, KeyModifiers::NONE);
    assert_eq!(cursor(&app), 1);
    type_text(&mut app, "X");
    assert_eq!(draft(&app), "aXbc");
    assert_eq!(cursor(&app), 2);
    press(&mut app, KeyCode::Right, KeyModifiers::NONE);
    press(&mut app, KeyCode::Right, KeyModifiers::NONE);
    assert_eq!(cursor(&app), 4);
    type_text(&mut app, "!");
    assert_eq!(draft(&app), "aXbc!");
    // Backspace removes the char before the caret, not the tail.
    press(&mut app, KeyCode::Left, KeyModifiers::NONE);
    press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(draft(&app), "aXb!");
    assert_eq!(cursor(&app), 3);
    app.shutdown_agents();
}

#[test]
fn alt_left_right_skip_words() {
    let (mut app, _temp) = composer_app();
    type_text(&mut app, "one two  three");
    press(&mut app, KeyCode::Left, KeyModifiers::ALT);
    assert_eq!(cursor(&app), 9, "jump to start of three");
    press(&mut app, KeyCode::Left, KeyModifiers::ALT);
    assert_eq!(cursor(&app), 4, "jump to start of two");
    press(&mut app, KeyCode::Right, KeyModifiers::ALT);
    assert_eq!(cursor(&app), 7, "jump past two (word end)");
    press(&mut app, KeyCode::Right, KeyModifiers::ALT);
    assert_eq!(cursor(&app), 9, "jump over the double space to the next word");
    press(&mut app, KeyCode::Right, KeyModifiers::ALT);
    assert_eq!(cursor(&app), 14, "jump to the end of three");
    // Movement clamps at the edges.
    press(&mut app, KeyCode::Left, KeyModifiers::ALT);
    press(&mut app, KeyCode::Right, KeyModifiers::ALT);
    for _ in 0..10 {
        press(&mut app, KeyCode::Right, KeyModifiers::ALT);
    }
    assert_eq!(cursor(&app), 14);
    app.shutdown_agents();
}

#[test]
fn ctrl_backspace_and_alt_backspace_delete_words() {
    let (mut app, _temp) = composer_app();
    type_text(&mut app, "alpha beta gamma");
    press(&mut app, KeyCode::Backspace, KeyModifiers::CONTROL);
    assert_eq!(draft(&app), "alpha beta ");
    press(&mut app, KeyCode::Left, KeyModifiers::ALT);
    assert_eq!(cursor(&app), 6, "caret at start of beta");
    press(&mut app, KeyCode::Backspace, KeyModifiers::ALT);
    assert_eq!(draft(&app), "beta ");
    // Ctrl+H is the terminal alias for Ctrl+Backspace; nothing left to delete
    // at the start of the draft.
    press(&mut app, KeyCode::Char('h'), KeyModifiers::CONTROL);
    assert_eq!(draft(&app), "beta ");
    // From the end, Ctrl+H deletes the whole remaining word; the trailing
    // space stays.
    press(&mut app, KeyCode::Right, KeyModifiers::ALT);
    press(&mut app, KeyCode::Char('h'), KeyModifiers::CONTROL);
    assert_eq!(draft(&app), " ");
    app.shutdown_agents();
}

#[test]
fn ctrl_u_clears_draft_and_resets_caret() {
    let (mut app, _temp) = composer_app();
    type_text(&mut app, "hello draft");
    press(&mut app, KeyCode::Left, KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
    assert!(draft(&app).is_empty());
    assert_eq!(cursor(&app), 0);
    app.shutdown_agents();
}

#[test]
fn home_end_jump_to_current_line_edges() {
    let (mut app, _temp) = composer_app();
    type_text(&mut app, "ab cd");
    for _ in 0..4 {
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
    }
    assert_eq!(cursor(&app), 1);
    press(&mut app, KeyCode::Home, KeyModifiers::NONE);
    assert_eq!(cursor(&app), 0, "Home goes to start of line");
    press(&mut app, KeyCode::End, KeyModifiers::NONE);
    assert_eq!(cursor(&app), 5, "End goes to end of line");
    // Multiline: Home/End stay on the current line. "ab cd\n ef" has the
    // second line starting at char 6 and ending at char 9.
    app.agents.threads[0].draft = String::from("ab cd\n ef");
    app.agents.threads[0].draft_cursor = 9;
    press(&mut app, KeyCode::Home, KeyModifiers::NONE);
    assert_eq!(cursor(&app), 6, "Home lands at start of second line");
    press(&mut app, KeyCode::End, KeyModifiers::NONE);
    assert_eq!(cursor(&app), 9, "End lands at end of second line");
    app.shutdown_agents();
}

#[test]
fn insert_opens_editor_and_enter_adds_newline_without_submitting() {
    let (mut app, _temp) = composer_app();
    type_text(&mut app, "hello");
    press(&mut app, KeyCode::Insert, KeyModifiers::NONE);
    assert!(app.agents.threads[0].prompt_editor_snapshot.is_some(), "Insert opens the editor");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        app.agents.threads[0].prompt_editor_snapshot.is_some(),
        "Enter must not submit while the editor is open"
    );
    assert_eq!(draft(&app), "hello\n");
    type_text(&mut app, "world");
    assert_eq!(draft(&app), "hello\nworld");
    // Caret editing still works inside the editor: Home + word-right lands at
    // the end of the second line, then typing inserts there.
    press(&mut app, KeyCode::Home, KeyModifiers::NONE);
    press(&mut app, KeyCode::Right, KeyModifiers::CONTROL);
    type_text(&mut app, "!");
    assert_eq!(draft(&app), "hello\nworld!");
    app.shutdown_agents();
}

#[test]
fn editor_esc_cancels_and_ctrl_enter_accepts() {
    let (mut app, _temp) = composer_app();
    type_text(&mut app, "original");
    press(&mut app, KeyCode::Insert, KeyModifiers::NONE);
    type_text(&mut app, "X");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.agents.threads[0].prompt_editor_snapshot.is_none(), "Esc closes the editor");
    assert_eq!(draft(&app), "original", "Esc restores the pre-open draft");

    press(&mut app, KeyCode::Insert, KeyModifiers::NONE);
    type_text(&mut app, "+");
    press(&mut app, KeyCode::Enter, KeyModifiers::CONTROL);
    assert!(app.agents.threads[0].prompt_editor_snapshot.is_none());
    assert_eq!(draft(&app), "original+", "Ctrl-Enter keeps the edited draft");
    app.shutdown_agents();
}

#[test]
fn editor_accepts_bracketed_paste_and_cancel_discards_it() {
    let (mut app, _temp) = composer_app();
    press(&mut app, KeyCode::Insert, KeyModifiers::NONE);
    app.handle_event(Event::Paste(String::from("line1\r\nline2")));
    assert_eq!(draft(&app), "line1\nline2");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(draft(&app), "", "cancelling discards the pasted content");
    app.shutdown_agents();
}
