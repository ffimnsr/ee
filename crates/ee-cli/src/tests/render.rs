use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Instant;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use serde_json::{Value, json};

use crate::app::{App, Mode};
use crate::backend::{CachedLine, CoreAnnotation, CoreSyntaxSpan, LineSlot};
use crate::buffer::BufferManager;
use crate::git::{GitBufferCache, GitBufferStatus, GitHunk, GitSign};
use crate::theme::syntax;
use crate::theme::ui as theme;
use crate::ui::ui;

#[test]
fn ui_render_shows_scrolled_gutter_for_long_buffer() {
    let mut app = App::from_path(None).unwrap();
    app.backend.lines = vec![String::new(); 51];
    app.backend.line_cache = vec![
        LineSlot::Known(CachedLine {
            text: String::new(),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        });
        51
    ];
    app.backend.cursor_line = 50;
    app.backend.cursor_col = 0;

    let width = 80;
    let height = 49;
    let editor_height = (height as usize).saturating_sub(2);
    app.scroll_into_view(editor_height, width as usize);

    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();

    let buffer = terminal.backend().buffer();
    let top_gutter = (0..6).map(|x| buffer.cell((x, 0)).unwrap().symbol()).collect::<String>();
    let status =
        (0..width).map(|x| buffer.cell((x, height - 2)).unwrap().symbol()).collect::<String>();

    // With the gap-fix, top_line is clamped so the last line fills the screen:
    // total_lines(51) - editor_height(47) = 4.
    assert_eq!(app.viewport.top_line, 4);
    assert!(top_gutter.contains("5"), "top gutter row was {top_gutter:?}");
    assert!(status.contains("Ln 51, Col 1"), "status row was {status:?}");
    assert!(status.ends_with("  Ln 51, Col 1 "), "status row was {status:?}");
}

#[test]
fn ui_render_uses_backend_syntax_spans_only() {
    fn render_numeric_fg(with_backend_syntax: bool, is_vlf: bool) -> ratatui::style::Color {
        let mut app = App::from_path(None).unwrap();
        let line = String::from("let answer = 42;");

        app.backend.is_vlf = is_vlf;
        app.backend.lines = if is_vlf { Vec::new() } else { vec![line.clone()] };
        app.backend.path = Some(PathBuf::from("sample.rs"));
        app.backend.line_cache = vec![LineSlot::Known(CachedLine {
            text: line,
            cursors: Vec::new(),
            syntax_spans: if with_backend_syntax {
                vec![CoreSyntaxSpan {
                    start_byte: 13,
                    end_byte: 15,
                    scope: String::from("constant.numeric.decimal.rust"),
                }]
            } else {
                Vec::new()
            },
        })];

        let backend = TestBackend::new(40, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| ui(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();

        let four_x = (0..40)
            .find(|&x| buf.cell((x, 0)).unwrap().symbol() == "4")
            .expect("rendered line should contain numeric literal");
        buf.cell((four_x, 0)).unwrap().fg
    }

    let plain_fg = render_numeric_fg(false, false);
    let backend_fg = render_numeric_fg(true, false);
    let vlf_fg = render_numeric_fg(false, true);

    assert_ne!(backend_fg, plain_fg);
    assert_eq!(backend_fg, syntax::FG_NUMBER);
    assert_eq!(plain_fg, theme::FG_BUFFER);
    assert_eq!(vlf_fg, plain_fg);
    assert_eq!(vlf_fg, theme::FG_BUFFER);
}

#[test]
fn ui_render_sanitizes_carriage_returns_in_buffer_text() {
    let mut app = App::from_path(None).unwrap();
    let line = String::from("alpha\rbeta");
    app.backend.lines = vec![line.clone()];
    app.backend.line_cache = vec![LineSlot::Known(CachedLine {
        text: line,
        cursors: Vec::new(),
        syntax_spans: Vec::new(),
    })];

    let backend = TestBackend::new(40, 6);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let buffer = terminal.backend().buffer();

    let row: String = (0..40).map(|x| buffer.cell((x, 0)).unwrap().symbol()).collect();
    assert!(row.contains("alpha␍beta"), "rendered row should show CR placeholder: {row:?}");
    assert!(!row.contains('\r'), "rendered row should not contain raw carriage return: {row:?}");
}

#[test]
fn ui_render_inserts_blank_column_between_gutter_and_text() {
    let mut app = App::from_path(None).unwrap();
    app.backend.lines = vec![String::from("alpha")];

    let width: u16 = 20;
    let height: u16 = 6;
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let buf = terminal.backend().buffer();

    let spacer_x: u16 = 6;
    let text_x: u16 = 7;
    assert_eq!(buf.cell((spacer_x, 0)).unwrap().symbol(), " ");
    assert_eq!(buf.cell((spacer_x, 0)).unwrap().bg, theme::BG_APP);
    assert_eq!(buf.cell((text_x, 0)).unwrap().symbol(), "a");
}

#[test]
fn ui_render_shows_git_gutter_sign() {
    let mut app = App::from_path(None).unwrap();
    let line = String::from("alpha");
    let buf_id = app.backend.active().id;
    app.backend.lines = vec![line.clone()];
    app.backend.line_cache = vec![LineSlot::Known(CachedLine {
        text: line,
        cursors: vec![0],
        syntax_spans: Vec::new(),
    })];
    app.source_control.insert(
        buf_id,
        GitBufferCache {
            fingerprint: 0,
            path: None,
            last_refresh: Instant::now(),
            status: Some(GitBufferStatus {
                repo_root: PathBuf::from("/tmp/repo"),
                repo_name: String::from("repo"),
                repo_relative: String::from("src/lib.rs"),
                branch: String::from("main"),
                tracked: true,
                dirty: true,
                hunks: vec![GitHunk {
                    old_start: 0,
                    old_count: 1,
                    new_start: 0,
                    new_count: 1,
                    display_line: 0,
                    sign: GitSign::Modified,
                    lines: Vec::new(),
                }],
                line_signs: HashMap::from([(0, GitSign::Modified)]),
            }),
        },
    );

    let backend = TestBackend::new(30, 6);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();

    let buffer = terminal.backend().buffer();
    let gutter = (0..6).map(|x| buffer.cell((x, 0)).unwrap().symbol()).collect::<String>();

    assert!(gutter.contains("~"), "gutter row was {gutter:?}");
}

#[test]
fn ui_render_hides_git_signs_and_shows_vlf_disabled_marker() {
    let mut app = App::from_path(None).unwrap();
    let line = String::from("alpha");
    let buf_id = app.backend.active().id;
    app.backend.is_vlf = true;
    app.backend.lines = Vec::new();
    app.backend.line_cache = vec![LineSlot::Known(CachedLine {
        text: line,
        cursors: vec![0],
        syntax_spans: Vec::new(),
    })];
    app.source_control.insert(
        buf_id,
        GitBufferCache {
            fingerprint: 0,
            path: None,
            last_refresh: Instant::now(),
            status: Some(GitBufferStatus {
                repo_root: PathBuf::from("/tmp/repo"),
                repo_name: String::from("repo"),
                repo_relative: String::from("src/lib.rs"),
                branch: String::from("main"),
                tracked: true,
                dirty: true,
                hunks: vec![GitHunk {
                    old_start: 0,
                    old_count: 1,
                    new_start: 0,
                    new_count: 1,
                    display_line: 0,
                    sign: GitSign::Modified,
                    lines: Vec::new(),
                }],
                line_signs: HashMap::from([(0, GitSign::Modified)]),
            }),
        },
    );

    let backend = TestBackend::new(40, 6);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();

    let buffer = terminal.backend().buffer();
    let gutter = (0..6).map(|x| buffer.cell((x, 0)).unwrap().symbol()).collect::<String>();
    let status = (0..40).map(|x| buffer.cell((x, 4)).unwrap().symbol()).collect::<String>();

    assert!(!gutter.contains("~"), "gutter row was {gutter:?}");
    assert!(status.contains("VLF"), "status row was {status:?}");
    assert!(status.contains("git:off(vlf)"), "status row was {status:?}");
    assert!(!status.contains("main"), "status row was {status:?}");
}

#[test]
fn visual_line_mode_highlights_selected_lines_in_render() {
    let mut app = App::from_path(None).unwrap();

    // Write three lines.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    for ch in "abc\ndef\nghi".chars() {
        let kc = if ch == '\n' { KeyCode::Enter } else { KeyCode::Char(ch) };
        app.handle_event(Event::Key(KeyEvent::new(kc, KeyModifiers::NONE)));
        app.backend.pump().unwrap();
    }
    // Return to normal, move to line 0.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE)));
    app.backend.pump_until(|state| state.cursor_line == 0).expect("cursor returns to first line");
    assert_eq!(app.backend.cursor_line, 0);

    // Enter visual-line mode and extend down one line.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('V'), KeyModifiers::SHIFT)));
    assert_eq!(app.mode, Mode::VisualLine);
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
    app.backend
        .pump_until(|state| state.cursor_line == 1)
        .expect("visual-line cursor moves to second line");
    assert_eq!(app.backend.cursor_line, 1);

    let width: u16 = 40;
    let height: u16 = 10;
    app.scroll_into_view(height as usize - 2, width as usize);

    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let buf = terminal.backend().buffer();

    // Rows 0 and 1 should carry the visual selection background (Rgb(68,71,90)).
    let vis_bg = theme::BG_SELECTION;
    let row0_has_vis = (0..width).any(|x| buf.cell((x, 0)).unwrap().bg == vis_bg);
    let row1_has_vis = (0..width).any(|x| buf.cell((x, 1)).unwrap().bg == vis_bg);
    // Row 2 (line "ghi") is outside the selection — should NOT be highlighted.
    let row2_has_vis = (0..width).any(|x| buf.cell((x, 2)).unwrap().bg == vis_bg);

    assert!(row0_has_vis, "row 0 should be highlighted in visual-line mode");
    assert!(row1_has_vis, "row 1 should be highlighted in visual-line mode");
    assert!(!row2_has_vis, "row 2 should not be highlighted outside selection");
}

#[test]
fn visual_char_mode_highlights_single_line_selection() {
    let mut app = App::from_path(None).unwrap();

    // Write one line with several characters.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    for ch in "hello world".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
        app.backend.pump().unwrap();
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));

    // Move to col 0, enter charwise visual, extend 4 chars.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Visual);
    for _ in 0..3 {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)));
    }

    let width: u16 = 40;
    let height: u16 = 10;
    app.scroll_into_view(height as usize - 2, width as usize);

    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let buf = terminal.backend().buffer();

    let vis_bg = theme::BG_SELECTION;
    // Gutter occupies ~4 cols; buffer adds one black padding col before text.
    // Columns 5..9 (display cols 0..3) should be highlighted.
    let gutter_width: u16 = 4;
    let row_has_vis =
        (gutter_width + 1..gutter_width + 5).any(|x| buf.cell((x, 0)).unwrap().bg == vis_bg);
    assert!(row_has_vis, "selected chars should carry visual-selection background");
}

#[test]
fn multi_line_core_annotation_highlights_rendered_rows() {
    let mut app = App::from_path(None).unwrap();
    app.backend.lines = vec![String::from("alpha"), String::from("beta"), String::from("gamma")];
    app.backend.annotations = vec![CoreAnnotation {
        annotation_type: String::from("lint"),
        ranges: vec![[0, 1, 1, 2]],
        payloads: Some(vec![json!("todo")]),
    }];

    let width: u16 = 40;
    let height: u16 = 10;
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let buf = terminal.backend().buffer();

    let annotation_bg = theme::BG_ANNOTATION;
    let gutter_width: u16 = 5;
    let row0_has_annotation =
        (gutter_width + 2..gutter_width + 6).any(|x| buf.cell((x, 0)).unwrap().bg == annotation_bg);
    let row1_has_annotation =
        (gutter_width + 1..gutter_width + 3).any(|x| buf.cell((x, 1)).unwrap().bg == annotation_bg);
    let row2_has_annotation =
        (gutter_width + 1..gutter_width + 6).any(|x| buf.cell((x, 2)).unwrap().bg == annotation_bg);

    assert!(row0_has_annotation, "row 0 should show annotation highlight");
    assert!(row1_has_annotation, "row 1 should show annotation highlight");
    assert!(!row2_has_annotation, "row 2 should not show annotation highlight");
}

#[test]
fn payload_backed_annotation_renders_gutter_marker() {
    let mut app = App::from_path(None).unwrap();
    app.backend.lines = vec![String::from("alpha")];
    app.backend.annotations = vec![CoreAnnotation {
        annotation_type: String::from("lint"),
        ranges: vec![[0, 0, 0, 5]],
        payloads: Some(vec![json!({ "label": "todo" })]),
    }];

    let width: u16 = 20;
    let height: u16 = 6;
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let buf = terminal.backend().buffer();

    assert_eq!(buf.cell((1, 0)).unwrap().symbol(), "T");
}

#[test]
fn mouse_click_uses_canonical_select_gesture() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = vec![String::from("hello")];

    app.handle_mouse_event_in_area(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 0,
            modifiers: KeyModifiers::NONE,
        },
        Rect { x: 0, y: 0, width: 80, height: 24 },
    );

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "gesture");
    assert_eq!(value["params"]["params"]["ty"]["select"]["granularity"], "point");
    assert_eq!(value["params"]["params"]["ty"]["select"]["multi"], false);
}

#[test]
fn mouse_click_accounts_for_gutter_and_viewport_offsets() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = (0..120).map(|idx| format!("line {idx:03}")).collect();
    app.viewport.top_line = 50;
    app.viewport.left_col = 7;

    app.handle_mouse_event_in_area(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 7,
            row: 5,
            modifiers: KeyModifiers::NONE,
        },
        Rect { x: 0, y: 0, width: 80, height: 44 },
    );

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["params"]["line"], 55);
    assert_eq!(value["params"]["params"]["col"], 7);
}

#[test]
fn mouse_click_in_gutter_targets_line_start() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = (0..120).map(|idx| format!("line {idx:03}")).collect();
    app.viewport.top_line = 50;
    app.viewport.left_col = 7;

    app.handle_mouse_event_in_area(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 5,
            modifiers: KeyModifiers::NONE,
        },
        Rect { x: 0, y: 0, width: 80, height: 44 },
    );

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["params"]["line"], 55);
    assert_eq!(value["params"]["params"]["col"], 0);
}

#[test]
fn mouse_click_outside_editor_rows_is_ignored() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = (0..120).map(|idx| format!("line {idx:03}")).collect();

    app.handle_mouse_event_in_area(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 6,
            row: 42,
            modifiers: KeyModifiers::NONE,
        },
        Rect { x: 0, y: 0, width: 80, height: 44 },
    );

    assert!(rx.try_recv().is_err());
}

#[test]
fn viewport_scrolls_down_when_cursor_leaves_view() {
    let mut app = App::from_path(None).unwrap();
    // Populate enough lines so the clamp doesn't pull top_line back.
    // 40 lines, height 20: max_top = 20, cursor scroll gives 11 < 20, no clamp.
    app.backend.lines = (0..40).map(|i| format!("line {i}")).collect();
    app.backend.cursor_line = 25;
    app.scroll_into_view(20, 80);
    // scroll_offset=5: top = cursor(25) + off(5) + 1 - height(20) = 11
    assert_eq!(app.viewport.top_line, 11);
}

#[test]
fn viewport_scrolls_up_when_cursor_above_top() {
    let mut app = App::from_path(None).unwrap();
    app.viewport.top_line = 10;
    app.backend.cursor_line = 5;
    app.scroll_into_view(20, 80);
    // scroll_offset=5: top = cursor(5).saturating_sub(off(5)) = 0
    assert_eq!(app.viewport.top_line, 0);
}

#[test]
fn horizontal_scroll_tracks_cursor_right() {
    let mut app = App::from_path(None).unwrap();
    // Three lines: short, short, long. Cursor on the long line past viewport width.
    app.backend.lines = vec!["a".to_string(), "bc".to_string(), "x".repeat(200)];
    app.backend.cursor_line = 2;
    // Place cursor byte-col 150, which is display col 150 for ASCII.
    app.backend.cursor_col = 150;
    app.scroll_into_view(20, 80);
    // Cursor at display col 150 must be visible in 80-wide view.
    assert!(app.viewport.left_col <= 150);
    assert!(150 < app.viewport.left_col + 80);
}

#[test]
fn horizontal_scroll_resets_when_cursor_moves_left() {
    let mut app = App::from_path(None).unwrap();
    app.backend.lines = vec!["a".to_string(), "bc".to_string(), "x".repeat(200)];
    app.backend.cursor_line = 2;
    app.backend.cursor_col = 150;
    app.scroll_into_view(20, 80);
    let scrolled = app.viewport.left_col;
    assert!(scrolled > 0, "should have scrolled right");

    // Now move cursor back to column 0 on a short line.
    app.backend.cursor_line = 0;
    app.backend.cursor_col = 0;
    app.scroll_into_view(20, 80);
    assert_eq!(app.viewport.left_col, 0, "left_col should reset when cursor at col 0");
}

#[test]
fn wrap_mode_resets_left_col_to_zero() {
    let mut app = App::from_path(None).unwrap();
    app.backend.lines = vec!["a".to_string(), "bc".to_string(), "x".repeat(200)];
    app.backend.cursor_line = 2;
    app.backend.cursor_col = 150;
    // Scroll right in non-wrap mode first.
    app.config.wrap_lines = false;
    app.scroll_into_view(20, 80);
    assert!(app.viewport.left_col > 0, "should have scrolled right in no-wrap mode");

    // Enable wrap mode — left_col must be clamped back to 0.
    app.config.wrap_lines = true;
    app.scroll_into_view(20, 80);
    assert_eq!(app.viewport.left_col, 0, "wrap mode must reset left_col to 0");
}
