use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::app::{App, Mode, SwiftMotionState, SwiftMotionTarget};
use crate::ui::ui;

#[test]
fn swift_motion_sequence_starts_and_jumps_to_labeled_visible_match() {
    let mut app = App::from_path(None).unwrap();
    app.last_editor_height = 10;
    app.last_editor_width = 40;

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    for ch in "alpha\nbeta\nalpha".chars() {
        let key = if ch == '\n' { KeyCode::Enter } else { KeyCode::Char(ch) };
        app.handle_event(Event::Key(KeyEvent::new(key, KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    app.backend.pump_until(|buf| buf.get_line(2).is_some_and(|l| l.contains("al"))).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE)));
    assert!(app.swift_motion.is_some());

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)));

    let state = app.swift_motion.as_ref().expect("swift motion should await label");
    assert_eq!(state.query, "al");
    assert_eq!(state.targets.len(), 2);
    let second_label = state.targets[1].label;

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(second_label), KeyModifiers::NONE)));
    // Bounded wait: the jump lands through an async cursor request; under
    // parallel test load the single pump budget can lapse.
    app.backend
        .pump_until(|state| state.cursor_line == 2 && state.cursor_col == 0)
        .expect("swift-motion jump lands");

    assert!(app.swift_motion.is_none());
    assert_eq!(app.backend.cursor_line, 2);
    assert_eq!(app.backend.cursor_col, 0);
}

#[test]
fn swift_motion_command_enters_prompt_state() {
    let mut app = App::from_path(None).unwrap();

    for key in [':', 's', 'w', 'i', 'f', 't', '_', 'm', 'o', 't', 'i', 'o', 'n'] {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    assert_eq!(app.mode, Mode::Normal);
    assert!(app.swift_motion.is_some());
    assert_eq!(app.swift_motion.as_ref().unwrap().query, "");
}

#[test]
fn swift_motion_prompt_renders_active_query() {
    let mut app = App::from_path(None).unwrap();
    app.last_editor_height = 8;
    app.last_editor_width = 30;
    app.swift_motion = Some(SwiftMotionState {
        query: String::from("al"),
        label_prefix: None,
        targets: vec![
            SwiftMotionTarget {
                line: 0,
                display_col: 0,
                end_display_col: 2,
                label: 'a',
                next_label: None,
            },
            SwiftMotionTarget {
                line: 1,
                display_col: 0,
                end_display_col: 2,
                label: 'b',
                next_label: None,
            },
        ],
    });

    let backend = TestBackend::new(40, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let buffer = terminal.backend().buffer();
    let mut screen = String::new();
    for y in 0..8 {
        for x in 0..40 {
            screen.push_str(buffer.cell((x, y)).unwrap().symbol());
        }
        screen.push('\n');
    }

    assert!(
        screen.contains("swift_motion al | choose label"),
        "screen missing swift motion prompt: {screen}"
    );
}

#[test]
fn swift_motion_dense_matches_narrow_then_jump() {
    let mut app = App::from_path(None).unwrap();
    app.last_editor_height = 40;
    app.last_editor_width = 20;

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    for index in 0..27 {
        for ch in "ab".chars() {
            app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
        }
        if index != 26 {
            app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        }
        app.backend.pump().unwrap();
    }
    // Bounded wait: xi-core applies edits asynchronously; under parallel test
    // load the fixed pump budget can lapse, so wait until the dense buffer
    // is fully materialized (every one of the 27 lines holds "ab").
    let dense_ready = |state: &crate::buffer::BufState| {
        (0..27).all(|index| state.get_line(index).is_some_and(|line| line.contains("ab")))
    };
    app.backend.pump_until(dense_ready).expect("dense buffer materialized");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    // Esc re-syncs the buffer; a stale revision snapshot can momentarily
    // regress the line count, so keep syncing until the dense buffer is
    // present again before the query runs.
    app.backend.pump_until(dense_ready).expect("dense buffer survives the mode switch");
    app.backend.pump_until(dense_ready).expect("dense buffer before the query");

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)));
    for ch in "swift_motion".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)));

    let state = app.swift_motion.as_ref().expect("swift motion should await dense labels");
    assert_eq!(state.query, "ab");
    assert_eq!(state.targets.len(), 27);
    assert!(state.targets.iter().any(|target| target.next_label.is_some()));

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)));

    let state = app.swift_motion.as_ref().expect("swift motion should narrow to second stage");
    assert_eq!(state.label_prefix, Some('a'));
    assert_eq!(state.targets.len(), 2);
    assert_eq!(state.targets[0].label, 'a');
    assert_eq!(state.targets[1].label, 'b');

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)));
    // Bounded wait: the jump lands through an async cursor request; under
    // parallel test load the single pump budget can lapse.
    app.backend
        .pump_until(|state| state.cursor_line == 26 && state.cursor_col == 0)
        .expect("swift-motion jump lands");

    assert!(app.swift_motion.is_none());
    assert_eq!(app.backend.cursor_line, 26);
    assert_eq!(app.backend.cursor_col, 0);
}
