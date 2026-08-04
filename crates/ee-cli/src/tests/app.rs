use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use crate::app::{App, Mode};
use crate::backend::{BackendEvent, CoreUpdate};
use crate::keymap::{Action, BindingKey};

#[test]
fn scratch_title_is_default() {
    let app = App::from_path(None).unwrap();

    assert_eq!(app.backend.title(), "[scratch]");
}

#[test]
fn ctrl_c_does_not_quit() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));

    assert!(!app.should_quit);
}

#[test]
fn ctrl_c_cancels_to_normal_mode() {
    let mut app = App::from_path(None).unwrap();
    app.mode = Mode::Insert;

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));

    assert!(!app.should_quit);
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn colon_q_quits() {
    let mut app = App::from_path(None).unwrap();
    for ch in [':', 'q'] {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
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
fn backend_event_marks_only_render_critical_startup_work() {
    assert!(
        BackendEvent::Update {
            view_id: String::from("view"),
            update: CoreUpdate { ops: Vec::new(), pristine: true, annotations: Vec::new() },
        }
        .is_startup_critical()
    );
    assert!(
        BackendEvent::DocumentMode { view_id: String::from("view"), is_vlf: false }
            .is_startup_critical()
    );
    assert!(
        !BackendEvent::Diagnostics { view_id: String::from("view"), diagnostics: Vec::new() }
            .is_startup_critical()
    );
    assert!(!BackendEvent::Alert(String::from("plugin started")).is_startup_critical());
}

#[test]
fn insert_visual_and_command_aliases_change_modes() {
    let mut app = App::from_path(None).unwrap();
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('i'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::EnterMode(Mode::Insert),
    );
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('v'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::EnterMode(Mode::Visual),
    );
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char(':'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::EnterCommandMode,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::ALT)));
    assert_eq!(app.mode, Mode::Insert);

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT)));
    assert_eq!(app.mode, Mode::Visual);

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::ALT)));
    assert_eq!(app.mode, Mode::CommandLine);
}

#[test]
fn a_enters_insert_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Insert);
}

#[test]
fn capital_a_enters_insert_at_eol() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Insert);
}

#[test]
fn capital_i_enters_insert_at_line_start() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('I'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Insert);
}

#[test]
fn o_enters_insert_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Insert);
}

#[test]
fn capital_o_enters_insert_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('O'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Insert);
}

#[test]
fn s_enters_insert_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Insert);
}

#[test]
fn capital_s_enters_insert_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Insert);
}
