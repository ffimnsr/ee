use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use serde_json::{Value, json};
use xi_core_lib::plugin_rpc::{
    CodeActionDescriptor, Diagnostic, DiagnosticSeverity, Range, SymbolItem,
};
use xi_core_lib::rpc::LineReplacement;

use crate::app::{App, Mode, Operator, PendingCharFind};
use crate::backend::{
    BackendEvent, CachedLine, CompletionSuggestion, CoreAnnotation, CoreLine, CoreSyntaxSpan,
    CoreUpdate, CoreUpdateKind, CoreUpdateOp, LineSlot, NavigationTarget, format_location_message,
    invalid_line_ranges, parse_notification,
};
use crate::buffer::{BufState, BufferManager};
use crate::keymap::{Action, BindingKey, bindings};
use crate::picker::PickerKind;
use crate::text::{
    byte_col_to_display_col, display_col_to_byte, find_char_backward, find_char_forward,
    next_char_start, prev_char_start,
};
use crate::ui::ui;

#[test]
fn scratch_title_is_default() {
    let app = App::from_path(None).unwrap();

    assert_eq!(app.backend.title(), "[scratch]");
}

#[test]
fn ctrl_c_quits() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));

    assert!(app.should_quit);
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
fn insert_mode_writes_to_scratch_buffer() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)));
    app.backend.pump().unwrap();

    assert_eq!(app.backend.lines, vec!["ab"]);
    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (0, 2));
}

#[test]
fn enter_splits_line_and_backspace_joins_it() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)));
    app.backend.pump().unwrap();

    assert_eq!(app.backend.lines, vec!["a", "b"]);

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)));
    app.backend.pump().unwrap();

    assert_eq!(app.backend.lines, vec!["a"]);
    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (0, 1));
}

#[test]
fn repeated_enter_tracks_cursor_beyond_visible_rows() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    for _ in 0..50 {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        app.backend.pump().unwrap();
    }

    assert_eq!(app.backend.cursor_line, 50);
    assert_eq!(app.backend.cursor_col, 0);
    assert_eq!(app.backend.lines.len(), 51);
}

#[test]
fn carriage_return_key_is_treated_as_enter() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    for _ in 0..5 {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('\r'), KeyModifiers::NONE)));
        app.backend.pump().unwrap();
    }

    assert_eq!(app.backend.cursor_line, 5);
    assert_eq!(app.backend.cursor_col, 0);
    assert_eq!(app.backend.lines.len(), 6);
}

#[test]
fn ctrl_m_key_is_treated_as_enter() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    for _ in 0..5 {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL)));
        app.backend.pump().unwrap();
    }

    assert_eq!(app.backend.cursor_line, 5);
    assert_eq!(app.backend.cursor_col, 0);
    assert_eq!(app.backend.lines.len(), 6);
}

#[test]
fn ui_render_shows_scrolled_gutter_after_many_enters() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    for _ in 0..50 {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        app.backend.pump().unwrap();
    }

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
        (0..40).map(|x| buffer.cell((x, height - 2)).unwrap().symbol()).collect::<String>();

    // With the gap-fix, top_line is clamped so the last line fills the screen:
    // total_lines(51) - editor_height(47) = 4.
    assert_eq!(app.viewport.top_line, 4);
    assert!(top_gutter.contains("5"), "top gutter row was {top_gutter:?}");
    assert!(status.contains("Ln 51, Col 1"), "status row was {status:?}");
}

#[test]
fn ui_render_prefers_backend_syntax_spans_over_syntect_fallback() {
    fn render_numeric_fg(with_backend_syntax: bool) -> ratatui::style::Color {
        let mut app = App::from_path(None).unwrap();
        let line = String::from("let answer = 42;");

        app.backend.lines = vec![line.clone()];
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

    let syntect_fg = render_numeric_fg(false);
    let backend_fg = render_numeric_fg(true);

    assert_ne!(backend_fg, syntect_fg);
    assert_eq!(backend_fg, ratatui::style::Color::Rgb(211, 120, 70));
}

#[test]
fn backspace_removes_multibyte_char() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('é'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)));
    app.backend.pump().unwrap();

    assert!(app.backend.lines.is_empty());
    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (0, 0));
}

#[test]
fn apply_update_merges_copy_update_insert_and_invalidate() {
    let mut client = test_buf_state();
    client.line_cache = vec![
        LineSlot::Known(CachedLine {
            text: "alpha".into(),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
        LineSlot::Known(CachedLine {
            text: "beta".into(),
            cursors: vec![2],
            syntax_spans: vec![CoreSyntaxSpan {
                start_byte: 0,
                end_byte: 4,
                scope: "keyword.control.rust".into(),
            }],
        }),
        LineSlot::Known(CachedLine {
            text: "gamma".into(),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
    ];
    client.rebuild_lines();

    client
        .apply_update(CoreUpdate {
            pristine: false,
            annotations: vec![CoreAnnotation {
                annotation_type: String::from("selection"),
                ranges: vec![[1, 1, 1, 3]],
                payloads: None,
            }],
            ops: vec![
                CoreUpdateOp { op: CoreUpdateKind::Copy, n: 1, lines: Vec::new() },
                CoreUpdateOp {
                    op: CoreUpdateKind::Update,
                    n: 1,
                    lines: vec![CoreLine { text: None, cursor: vec![1], syntax_spans: None }],
                },
                CoreUpdateOp {
                    op: CoreUpdateKind::Insert,
                    n: 1,
                    lines: vec![CoreLine {
                        text: Some("delta".into()),
                        cursor: Vec::new(),
                        syntax_spans: Some(vec![CoreSyntaxSpan {
                            start_byte: 0,
                            end_byte: 5,
                            scope: "entity.name.function.rust".into(),
                        }]),
                    }],
                },
                CoreUpdateOp { op: CoreUpdateKind::Invalidate, n: 2, lines: Vec::new() },
            ],
        })
        .unwrap();

    assert_eq!(client.lines, vec!["alpha", "beta", "delta", "", ""]);
    assert_eq!((client.cursor_line, client.cursor_col), (1, 1));
    let LineSlot::Known(line) = &client.line_cache[1] else { panic!("expected cached line") };
    assert_eq!(line.syntax_spans.len(), 1);
    let LineSlot::Known(line) = &client.line_cache[2] else { panic!("expected cached line") };
    assert_eq!(line.syntax_spans.len(), 1);
    assert_eq!(invalid_line_ranges(&client.line_cache), vec![(3, 5)]);
    assert_eq!(client.annotations.len(), 1);
    assert!(!client.pristine);
}

#[test]
fn parse_notification_decodes_syntax_spans_in_update_lines() {
    let event = parse_notification(
        "update",
        json!({
            "view_id": "view-id-1",
            "update": {
                "pristine": true,
                "annotations": [],
                "ops": [{
                    "op": "ins",
                    "n": 1,
                    "lines": [{
                        "text": "let x = 1",
                        "cursor": [3],
                        "syntax_spans": [
                            { "start_byte": 0, "end_byte": 3, "scope": "keyword.control.rust" },
                            { "start_byte": 8, "end_byte": 9, "scope": "constant.numeric.decimal.rust" }
                        ]
                    }]
                }]
            }
        }),
    )
    .expect("update notification should parse");

    let BackendEvent::Update { update, .. } = event else { panic!("expected update event") };
    let spans = update.ops[0].lines[0].syntax_spans.as_ref().expect("missing syntax spans");
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0].scope, "keyword.control.rust");
}

#[test]
fn open_file_bootstraps_full_buffer_from_updates() {
    let path = unique_temp_path("ee-tui-open");
    let contents = (0..24).map(|i| format!("line-{i}")).collect::<Vec<_>>().join("\n");
    fs::write(&path, &contents).unwrap();

    let app = App::from_path(Some(path.clone())).unwrap();

    fs::remove_file(&path).unwrap();
    assert_eq!(app.backend.lines, contents.split('\n').map(ToOwned::to_owned).collect::<Vec<_>>());
}

#[test]
fn write_command_saves_file() {
    let path = unique_temp_path("ee-tui-save");
    fs::write(&path, "seed").unwrap();

    let mut app = App::from_path(Some(path.clone())).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    for _ in 0..20 {
        let text = fs::read_to_string(&path).unwrap();
        if text.starts_with('!') {
            fs::remove_file(&path).unwrap();
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }

    let final_text = fs::read_to_string(&path).unwrap();
    fs::remove_file(&path).unwrap();
    assert!(final_text.starts_with('!'));
}

#[test]
fn parse_notification_handles_completions() {
    let event = parse_notification(
        "completions",
        json!({
            "view_id": "view-id-1",
            "items": [{
                "label": "println!",
                "detail": "macro",
                "insert_text": "println!($0)"
            }]
        }),
    )
    .expect("completion notification should parse");

    match event {
        BackendEvent::Completions { view_id, items } => {
            assert_eq!(view_id, "view-id-1");
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].label, "println!");
        }
        other => panic!("unexpected event: {:?}", other),
    }
}

#[test]
fn parse_notification_handles_diagnostics() {
    let event = parse_notification(
        "diagnostics",
        json!({
            "view_id": "view-id-1",
            "diagnostics": [{
                "range": { "start": 2, "end": 5 },
                "severity": "warning",
                "message": "watch this",
                "source": "lsp",
                "code": "W1"
            }]
        }),
    )
    .expect("diagnostics notification should parse");

    match event {
        BackendEvent::Diagnostics { view_id, diagnostics } => {
            assert_eq!(view_id, "view-id-1");
            assert_eq!(diagnostics.len(), 1);
            assert_eq!(diagnostics[0].message, "watch this");
        }
        other => panic!("unexpected event: {:?}", other),
    }
}

#[test]
fn parse_notification_handles_update_annotations() {
    let event = parse_notification(
        "update",
        json!({
            "view_id": "view-id-1",
            "update": {
                "ops": [],
                "pristine": true,
                "annotations": [{
                    "type": "selection",
                    "ranges": [[0, 1, 0, 4]],
                    "payloads": ["cursor"],
                    "n": 1
                }]
            }
        }),
    )
    .expect("update notification should parse");

    match event {
        BackendEvent::Update { view_id, update } => {
            assert_eq!(view_id, "view-id-1");
            assert_eq!(update.annotations.len(), 1);
            assert_eq!(update.annotations[0].annotation_type, "selection");
            assert_eq!(update.annotations[0].ranges, vec![[0, 1, 0, 4]]);
        }
        other => panic!("unexpected event: {:?}", other),
    }
}

#[test]
fn parse_notification_handles_code_actions() {
    let event = parse_notification(
        "code_actions",
        json!({
            "view_id": "view-id-1",
            "actions": [{ "title": "Extract variable" }]
        }),
    )
    .expect("code action notification should parse");

    match event {
        BackendEvent::CodeActions { view_id, actions } => {
            assert_eq!(view_id, "view-id-1");
            assert_eq!(actions[0].title, "Extract variable");
        }
        other => panic!("unexpected event: {:?}", other),
    }
}

#[test]
fn request_completion_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.request_completion(Some(2)).expect("completion request should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_completion");
    assert_eq!(value["params"]["params"]["index"], 2);
}

#[test]
fn request_definition_emits_backend_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.request_definition().expect("definition request should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_definition");
}

#[test]
fn request_hover_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.request_hover(Some((3, 7))).expect("hover request should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_hover");
    assert_eq!(value["params"]["params"]["position"]["line"], 3);
    assert_eq!(value["params"]["params"]["position"]["column"], 7);
}

#[test]
fn request_code_actions_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.request_code_actions(Some(2)).expect("code action request should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_code_actions");
    assert_eq!(value["params"]["params"]["index"], 2);
}

#[test]
fn request_rename_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.request_rename("renamed_symbol").expect("rename request should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_rename");
    assert_eq!(value["params"]["params"]["new_name"], "renamed_symbol");
}

#[test]
fn delete_line_range_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.delete_line_range(3, 5).expect("line delete should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "delete_line_range");
    assert_eq!(value["params"]["params"]["start_line"], 3);
    assert_eq!(value["params"]["params"]["end_line"], 5);
}

#[test]
fn replay_block_insert_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.replay_block_insert(2, 4, 6, "abc", true).expect("block insert replay should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "replay_block_insert");
    assert_eq!(value["params"]["params"]["start_line"], 2);
    assert_eq!(value["params"]["params"]["end_line"], 4);
    assert_eq!(value["params"]["params"]["column"], 6);
    assert_eq!(value["params"]["params"]["text"], "abc");
    assert_eq!(value["params"]["params"]["append"], true);
}

#[test]
fn paste_register_emits_backend_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.paste_register("hello", false).expect("register paste should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "paste_register");
    assert_eq!(value["params"]["params"]["chars"], "hello");
    assert_eq!(value["params"]["params"]["before"], false);
}

#[test]
fn apply_line_replacements_emits_backend_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client
        .apply_line_replacements(&[LineReplacement { line: 2, text: String::from("beta") }])
        .expect("line replacements should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "apply_line_replacements");
    assert_eq!(value["params"]["params"]["replacements"][0]["line"], 2);
    assert_eq!(value["params"]["params"]["replacements"][0]["text"], "beta");
}

#[test]
fn definition_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in [':', 'd', 'e', 'f'] {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_definition");
}

#[test]
fn codeaction_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":codeaction 3".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_code_actions");
    assert_eq!(value["params"]["params"]["index"], 3);
}

#[test]
fn complete_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":complete".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_completion");
    assert!(value["params"]["params"]["index"].is_null());
}

#[test]
fn rename_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":rename fresh_name".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_rename");
    assert_eq!(value["params"]["params"]["new_name"], "fresh_name");
}

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
fn pending_completion_notification_opens_picker() {
    let mut app = App::from_path(None).unwrap();
    app.backend.pending_ui_actions.push(crate::backend::PendingUiAction::Completions {
        view_id: app.backend.view_id.clone(),
        items: vec![CompletionSuggestion {
            label: String::from("println!"),
            detail: Some(String::from("macro")),
            insert_text: None,
        }],
    });

    app.handle_pending_ui_actions();

    assert_eq!(app.picker.as_ref().map(|picker| picker.kind), Some(PickerKind::Completions));
}

#[test]
fn pending_code_actions_notification_opens_picker() {
    let mut app = App::from_path(None).unwrap();
    app.backend.pending_ui_actions.push(crate::backend::PendingUiAction::CodeActions {
        view_id: app.backend.view_id.clone(),
        actions: vec![CodeActionDescriptor { title: String::from("Extract variable") }],
    });

    app.handle_pending_ui_actions();

    assert_eq!(app.picker.as_ref().map(|picker| picker.kind), Some(PickerKind::CodeActions));
}

#[test]
fn pending_hover_notification_opens_popup() {
    let mut app = App::from_path(None).unwrap();
    app.backend.pending_ui_actions.push(crate::backend::PendingUiAction::Hover {
        view_id: app.backend.view_id.clone(),
        content: String::from("hover text"),
    });

    app.handle_pending_ui_actions();

    assert_eq!(app.hover_popup.as_ref().map(|popup| popup.content.as_str()), Some("hover text"));
}

#[test]
fn plugin_terminated_notification_updates_status_message() {
    let params = json!({
        "view_id": "view-id-1",
        "plugin": "rust-analyzer",
        "reason": {
            "kind": "rpc_timed_out",
            "limit_ms": 250,
            "method": "update"
        }
    });

    let event =
        parse_notification("plugin_terminated", params).expect("plugin_terminated should parse");
    match event {
        BackendEvent::Alert(message) => {
            assert_eq!(
                message,
                "plugin rust-analyzer terminated: rpc update timed out after 250 ms"
            );
        }
        other => panic!("unexpected backend event: {other:?}"),
    }
}

#[test]
fn ee_tui_sources_do_not_use_raw_lsp_or_plugin_routes() {
    let app_src = include_str!("app/mod.rs");
    let buffer_src = include_str!("buffer.rs");
    let backend_src = include_str!("backend.rs");

    assert!(!app_src.contains("xi-lsp-plugin"));
    assert!(!app_src.contains("lsp."));
    assert!(!app_src.contains("line_cache"));

    assert!(!buffer_src.contains("xi-lsp-plugin"));
    assert!(!buffer_src.contains("lsp."));

    assert!(!backend_src.contains("show_hover"));
    assert!(!backend_src.contains("show_completions"));
    assert!(!backend_src.contains("show_locations"));
}

#[test]
fn transpose_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in [':', 't', 'r', 'a', 'n', 's', 'p', 'o', 's', 'e'] {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "transpose");
}

#[test]
fn selection_for_replace_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in [
        ':', 's', 'e', 'l', 'e', 'c', 't', 'i', 'o', 'n', 'f', 'o', 'r', 'r', 'e', 'p', 'l', 'a',
        'c', 'e',
    ] {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "selection_for_replace");
}

#[test]
fn substitute_range_uses_backend_authoritative_path() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    for ch in "alpha\nbeta\nalpha".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    app.backend.pump().unwrap();

    app.execute_substitute(1, 2, "a", "A", "");
    app.backend.pump().unwrap();

    assert_eq!(app.backend.lines, vec!["alpha", "betA", "Alpha"]);
}

#[test]
fn substitute_confirm_uses_backend_preview_and_apply() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    for ch in "alpha\nbeta\nalpha".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    app.backend.pump().unwrap();

    app.execute_substitute(0, 2, "a", "A", "c");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)));
    app.backend.pump().unwrap();

    assert_eq!(app.backend.lines, vec!["Alpha", "beta", "alpha"]);
}

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
fn duplicate_line_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in [':', 'd', 'u', 'p', 'l', 'i', 'c', 'a', 't', 'e', 'l', 'i', 'n', 'e'] {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "duplicate_line");
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
fn multifind_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":multifind alpha beta".chars() {
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
            column: 6,
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
fn byte_col_to_display_col_ascii() {
    assert_eq!(byte_col_to_display_col("hello", 3), 3);
}

#[test]
fn byte_col_to_display_col_wide_char() {
    let s = "日本";
    assert_eq!(byte_col_to_display_col(s, 3), 2);
    assert_eq!(byte_col_to_display_col(s, 6), 4);
}

#[test]
fn display_col_to_byte_wide_char() {
    let s = "日本";
    assert_eq!(display_col_to_byte(s, 0), 0);
    assert_eq!(display_col_to_byte(s, 2), 3);
    assert_eq!(display_col_to_byte(s, 4), 6);
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

#[test]
fn bindings_table_has_normal_hjkl() {
    let b = bindings();
    let lookup = |key| {
        b.get(&BindingKey { mode: Mode::Normal, key, modifiers: KeyModifiers::NONE, prefix: None })
            .cloned()
    };
    assert_eq!(lookup(KeyCode::Char('h')), Some(Action::Edit("move_left")));
    assert_eq!(lookup(KeyCode::Char('l')), Some(Action::Edit("move_right")));
    assert_eq!(lookup(KeyCode::Char('k')), Some(Action::Edit("move_up")));
    assert_eq!(lookup(KeyCode::Char('j')), Some(Action::Edit("move_down")));
}

#[test]
fn k_binding_requests_hover() {
    let b = bindings();
    let lookup = b
        .get(&BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('K'),
            modifiers: KeyModifiers::NONE,
            prefix: None,
        })
        .cloned();
    assert_eq!(lookup, Some(Action::RequestHover));
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

#[test]
fn count_digits_accumulate_in_normal_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE)));
    assert_eq!(app.input_state.count(), 35);
}

#[test]
fn count_resets_after_motion() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
    assert_eq!(app.input_state.count(), 1);
}

#[test]
fn zero_as_motion_when_no_count_active() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE)));
    assert_eq!(app.input_state.count_digits, Vec::<u8>::new());
}

#[test]
fn zero_extends_count_when_digits_already_present() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE)));
    assert_eq!(app.input_state.count(), 10);
}

#[test]
fn g_key_sets_prefix_and_is_not_reset_immediately() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE)));
    assert_eq!(app.input_state.prefix, Some('g'));
}

#[test]
fn gg_prefix_clears_after_second_g() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE)));
    assert_eq!(app.input_state.prefix, None);
}

#[test]
fn f_key_enters_pending_find_state() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)));
    assert_eq!(
        app.input_state.pending_find,
        Some(PendingCharFind { forward: true, inclusive: true })
    );
}

#[test]
fn t_key_enters_pending_find_exclusive_state() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE)));
    assert_eq!(
        app.input_state.pending_find,
        Some(PendingCharFind { forward: true, inclusive: false })
    );
}

#[test]
fn pending_find_clears_after_target_char() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)));
    assert_eq!(app.input_state.pending_find, None);
}

#[test]
fn slash_enters_search_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Search);
    assert!(app.command_buffer.is_empty());
}

#[test]
fn search_esc_returns_to_normal() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn search_chars_accumulate_in_command_buffer() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE)));
    assert_eq!(app.command_buffer, "foo");
    assert_eq!(app.mode, Mode::Search);
}

#[test]
fn search_backspace_removes_last_char() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)));
    assert_eq!(app.command_buffer, "a");
}

#[test]
fn search_enter_returns_to_normal() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn find_char_forward_finds_next_occurrence() {
    assert_eq!(find_char_forward("abcabc", 0, 'b'), Some(1));
}

#[test]
fn find_char_forward_skips_char_under_cursor() {
    assert_eq!(find_char_forward("abcabc", 1, 'b'), Some(4));
}

#[test]
fn find_char_forward_returns_none_when_absent() {
    assert_eq!(find_char_forward("abc", 0, 'z'), None);
}

#[test]
fn find_char_backward_finds_previous_occurrence() {
    assert_eq!(find_char_backward("abcabc", 6, 'b'), Some(4));
}

#[test]
fn find_char_backward_stops_before_cursor() {
    assert_eq!(find_char_backward("abcabc", 4, 'b'), Some(1));
}

#[test]
fn find_char_backward_returns_none_when_absent() {
    assert_eq!(find_char_backward("abc", 3, 'z'), None);
}

#[test]
fn prev_char_start_ascii() {
    assert_eq!(prev_char_start("hello", 3), 2);
}

#[test]
fn prev_char_start_multibyte() {
    let s = "aé";
    assert_eq!(prev_char_start(s, 3), 1);
}

#[test]
fn next_char_start_ascii() {
    assert_eq!(next_char_start("hello", 1), 2);
}

#[test]
fn next_char_start_multibyte() {
    let s = "aé";
    assert_eq!(next_char_start(s, 1), 3);
}

#[test]
fn format_location_message_formats_empty_and_single_results() {
    assert_eq!(format_location_message("definition", &[]), "definition: no locations");
    assert_eq!(
        format_location_message(
            "definition",
            &[NavigationTarget {
                path: String::from("/tmp/main.rs"),
                line: 2,
                column: 4,
                end_line: 2,
                end_column: 7,
            }],
        ),
        "definition: /tmp/main.rs:3:5"
    );
}

#[test]
fn completion_suggestion_deserializes_optional_fields() {
    let item: CompletionSuggestion = serde_json::from_value(json!({
        "label": "println!"
    }))
    .unwrap();
    assert_eq!(item.label, "println!");
    assert_eq!(item.detail, None);
    assert_eq!(item.insert_text, None);
}

fn unique_temp_path(prefix: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    env::temp_dir().join(format!("{prefix}-{nanos}.txt"))
}

fn test_buf_state() -> BufState {
    BufState {
        id: 1,
        path: None,
        view_id: String::new(),
        pending_line_request: false,
        line_cache: Vec::new(),
        lines: Vec::new(),
        cursor_line: 0,
        cursor_col: 0,
        pristine: true,
        status_message: None,
        last_scroll: None,
        mtime: None,
        externally_modified: false,
        diagnostics: Vec::new(),
        annotations: Vec::new(),
    }
}

// ── Insert-entry variants ─────────────────────────────────────────────────────

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

// ── Operator-pending mode ─────────────────────────────────────────────────────

#[test]
fn d_enters_operator_pending_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::OperatorPending);
    assert_eq!(app.input_state.pending_operator, Some(Operator::Delete));
}

#[test]
fn c_enters_operator_pending_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::OperatorPending);
    assert_eq!(app.input_state.pending_operator, Some(Operator::Change));
}

#[test]
fn y_enters_operator_pending_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::OperatorPending);
    assert_eq!(app.input_state.pending_operator, Some(Operator::Yank));
}

#[test]
fn indent_operator_enters_operator_pending() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('>'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::OperatorPending);
    assert_eq!(app.input_state.pending_operator, Some(Operator::Indent));
}

#[test]
fn escape_cancels_operator_pending() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::OperatorPending);
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.input_state.pending_operator, None);
}

#[test]
fn operator_pending_motion_returns_to_normal() {
    let mut app = App::from_path(None).unwrap();
    // d + w → sends motion + delete and returns to normal
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.input_state.pending_operator, None);
}

#[test]
fn change_operator_motion_enters_insert() {
    let mut app = App::from_path(None).unwrap();
    // c + w → enters insert mode after deletion
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Insert);
}

#[test]
fn double_d_applies_to_line() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.input_state.pending_operator, None);
}

#[test]
fn double_c_enters_insert() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Insert);
}

#[test]
fn g_lowercase_u_sets_lowercase_operator() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::OperatorPending);
    assert_eq!(app.input_state.pending_operator, Some(Operator::Lowercase));
}

#[test]
fn operator_text_object_prefix_i_sets_inner() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    // Still in operator-pending waiting for text object specifier
    assert_eq!(app.mode, Mode::OperatorPending);
    assert_eq!(app.input_state.text_obj_inclusive, Some(false));
}

#[test]
fn operator_text_object_prefix_a_sets_outer() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::OperatorPending);
    assert_eq!(app.input_state.text_obj_inclusive, Some(true));
}

#[test]
fn operator_text_object_unknown_specifier_cancels() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    // Unknown text object specifier → cancel
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn operator_f_sets_pending_find_in_operator_pending() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::OperatorPending);
    assert_eq!(
        app.input_state.pending_find,
        Some(PendingCharFind { forward: true, inclusive: true })
    );
}

// ── Text object helpers ───────────────────────────────────────────────────────

use crate::app::{text_obj_bracket, text_obj_quote, text_obj_tag, text_obj_word};

#[test]
fn word_obj_inner_finds_word_boundaries() {
    // "hello world", cursor on 'h' (byte 0)
    let (start, end) = text_obj_word("hello world", 0, false, false).unwrap();
    assert_eq!(&"hello world"[start..end], "hello");
}

#[test]
fn word_obj_inner_mid_word() {
    // cursor on 'l' at byte 2
    let (start, end) = text_obj_word("hello world", 2, false, false).unwrap();
    assert_eq!(&"hello world"[start..end], "hello");
}

#[test]
fn word_obj_outer_includes_trailing_space() {
    let (start, end) = text_obj_word("hello world", 0, true, false).unwrap();
    assert_eq!(&"hello world"[start..end], "hello ");
}

#[test]
fn word_obj_not_on_word_char_returns_none() {
    // cursor on space
    assert!(text_obj_word("hello world", 5, false, false).is_none());
}

#[test]
fn quote_obj_inner_finds_content() {
    let (start, end) = text_obj_quote("say \"hello\" here", 5, '"', false).unwrap();
    assert_eq!(&"say \"hello\" here"[start..end], "hello");
}

#[test]
fn quote_obj_outer_includes_quotes() {
    let (start, end) = text_obj_quote("say \"hello\" here", 5, '"', true).unwrap();
    assert_eq!(&"say \"hello\" here"[start..end], "\"hello\"");
}

#[test]
fn bracket_obj_inner_finds_content() {
    let (start, end) = text_obj_bracket("foo(bar)baz", 4, '(', ')', false).unwrap();
    assert_eq!(&"foo(bar)baz"[start..end], "bar");
}

#[test]
fn bracket_obj_outer_includes_brackets() {
    let (start, end) = text_obj_bracket("foo(bar)baz", 4, '(', ')', true).unwrap();
    assert_eq!(&"foo(bar)baz"[start..end], "(bar)");
}

#[test]
fn tag_obj_inner_finds_content() {
    let (start, end) = text_obj_tag("<b>bold</b>", 4, false).unwrap();
    assert_eq!(&"<b>bold</b>"[start..end], "bold");
}

#[test]
fn tag_obj_outer_includes_tags() {
    let (start, end) = text_obj_tag("<b>bold</b>", 4, true).unwrap();
    assert_eq!(&"<b>bold</b>"[start..end], "<b>bold</b>");
}

// ── Insert mode controls ──────────────────────────────────────────────────────

#[test]
fn ctrl_w_in_insert_sends_delete_word_backward() {
    let mut app = App::from_path(None).unwrap();
    // Enter insert mode, then Ctrl+W
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)));
    // Still in insert mode
    assert_eq!(app.mode, Mode::Insert);
}

#[test]
fn ctrl_u_in_insert_sends_delete_to_line_start() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)));
    assert_eq!(app.mode, Mode::Insert);
}

// ── New feature tests ─────────────────────────────────────────────────────────

#[test]
fn capital_v_enters_visual_line_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('V'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::VisualLine);
}

#[test]
fn ctrl_v_enters_visual_block_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)));
    assert_eq!(app.mode, Mode::VisualBlock);
}

#[test]
fn esc_from_visual_line_returns_to_normal() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('V'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::VisualLine);
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn esc_from_visual_block_returns_to_normal() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)));
    assert_eq!(app.mode, Mode::VisualBlock);
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn u_dispatches_undo() {
    let mut app = App::from_path(None).unwrap();
    // Drive `u` — should send undo edit without crashing.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn ctrl_r_dispatches_redo() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)));
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn dot_with_no_last_change_is_noop() {
    let mut app = App::from_path(None).unwrap();
    // `.` should not crash when no last_change is recorded.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('.'), KeyModifiers::NONE)));
    assert_eq!(app.mode, Mode::Normal);
}

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
}

#[test]
fn visual_anchor_set_on_visual_enter() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)));
    assert!(app.visual_anchor.is_some());
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

#[test]
fn parse_notification_handles_symbols() {
    let event = parse_notification(
        "symbols",
        json!({
            "view_id": "view-id-1",
            "title": "Document Symbols",
            "symbols": [{
                "name": "my_func",
                "kind": "function",
                "path": "/src/lib.rs",
                "line": 10,
                "column": 0
            }]
        }),
    )
    .expect("symbols notification should parse");

    match event {
        BackendEvent::Symbols { view_id, title, symbols } => {
            assert_eq!(view_id, "view-id-1");
            assert_eq!(title, "Document Symbols");
            assert_eq!(symbols.len(), 1);
            assert_eq!(symbols[0].name, "my_func");
            assert_eq!(symbols[0].kind, "function");
            assert_eq!(symbols[0].line, 10);
        }
        other => panic!("unexpected event: {:?}", other),
    }
}

#[test]
fn request_document_symbols_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.request_document_symbols().expect("document symbols request should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_document_symbols");
}

#[test]
fn request_workspace_symbols_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.request_workspace_symbols("Foo").expect("workspace symbols request should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_workspace_symbols");
    assert_eq!(value["params"]["params"]["query"], "Foo");
}

#[test]
fn symbols_command_sends_document_symbols_request() {
    let mut app = App::from_path(None).unwrap();
    // Drain initial events so tests start clean.
    let _ = app.backend.drain_events();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)));
    for ch in "symbols".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    // Executing the command should not panic; LSP may not be active in test env.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
}

#[test]
fn symbols_notification_populates_picker() {
    let (tx, rx) = mpsc::channel();
    let (backend_tx, backend_rx) = mpsc::channel();
    let mut mgr = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    backend_tx
        .send(BackendEvent::Symbols {
            view_id: String::from("view-id-1"),
            title: String::from("Document Symbols"),
            symbols: vec![SymbolItem {
                name: String::from("do_thing"),
                kind: String::from("function"),
                path: String::from("/src/lib.rs"),
                line: 5,
                column: 0,
            }],
        })
        .expect("send should succeed");

    mgr.drain_events().expect("drain should not fail");

    let pending = mgr.drain_pending_symbols();
    assert_eq!(pending.len(), 1);
    let (vid, title, syms) = &pending[0];
    assert_eq!(vid, "view-id-1");
    assert_eq!(title, "Document Symbols");
    assert_eq!(syms.len(), 1);
    assert_eq!(syms[0].name, "do_thing");

    // Verify the rx channel is empty (no RPC was emitted by the notification).
    assert!(rx.try_recv().is_err(), "no RPC should be emitted for symbols notification");
}

// ── Visual-mode rendering ──────────────────────────────────────────────────

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
    app.backend.pump().unwrap(); // wait for cursor-at-line-0 update from xi-core

    // Enter visual-line mode and extend down one line.
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('V'), KeyModifiers::SHIFT)));
    assert_eq!(app.mode, Mode::VisualLine);
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
    app.backend.pump().unwrap(); // wait for move_down_and_modify_selection update

    let width: u16 = 40;
    let height: u16 = 10;
    app.scroll_into_view(height as usize - 2, width as usize);

    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let buf = terminal.backend().buffer();

    // Rows 0 and 1 should carry the visual selection background (Rgb(68,71,90)).
    let vis_bg = ratatui::style::Color::Rgb(68, 71, 90);
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

    let vis_bg = ratatui::style::Color::Rgb(68, 71, 90);
    // Gutter occupies ~4 cols; text starts at col 4.
    // Columns 4..8 (display cols 0..3) should be highlighted.
    let gutter_width: u16 = 4;
    let row_has_vis =
        (gutter_width..gutter_width + 4).any(|x| buf.cell((x, 0)).unwrap().bg == vis_bg);
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

    let annotation_bg = ratatui::style::Color::Rgb(43, 82, 74);
    let gutter_width: u16 = 5;
    let row0_has_annotation =
        (gutter_width + 1..gutter_width + 5).any(|x| buf.cell((x, 0)).unwrap().bg == annotation_bg);
    let row1_has_annotation =
        (gutter_width..gutter_width + 2).any(|x| buf.cell((x, 1)).unwrap().bg == annotation_bg);
    let row2_has_annotation =
        (gutter_width..gutter_width + 5).any(|x| buf.cell((x, 2)).unwrap().bg == annotation_bg);

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
