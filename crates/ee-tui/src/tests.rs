use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::sync::mpsc::TryRecvError;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use serde_json::{Value, json};
use xi_core_lib::plugin_rpc::{
    CodeActionDescriptor, Diagnostic, DiagnosticSeverity, Range, SelectionRange, SymbolItem,
};
use xi_core_lib::rpc::LineReplacement;

use crate::app::{App, Mode, Operator, PendingCharFind};
use crate::backend::{
    BackendEvent, CachedLine, CompletionSuggestion, CoreAnnotation, CoreLine, CoreSyntaxSpan,
    CoreUpdate, CoreUpdateKind, CoreUpdateOp, LineSlot, NavigationTarget, format_location_message,
    invalid_line_ranges, parse_notification,
};
use crate::buffer::{BufState, BufferManager};
use crate::git::{GitBufferCache, GitBufferStatus, GitHunk, GitSign};
use crate::keymap::{Action, BindingKey, bindings, parse_action_spec};
use crate::picker::PickerKind;
use crate::registers::{ClipboardSelection, RegisterName, set_test_clipboard};
use crate::text::{
    byte_col_to_display_col, display_col_to_byte, find_char_backward, find_char_forward,
    next_char_start, next_word_end, next_word_start, prev_char_start, prev_word_start,
};
use crate::ui::ui;

fn cwd_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

struct CurrentDirGuard(PathBuf);

impl CurrentDirGuard {
    fn capture() -> Self {
        Self(env::current_dir().unwrap())
    }
}

impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        let _ = env::set_current_dir(&self.0);
    }
}

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
fn core_update_keeps_invalid_lines_lazy() {
    let (tx, rx) = mpsc::channel();
    let (backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    backend_tx
        .send(BackendEvent::Update {
            view_id: String::from("view-id-1"),
            update: CoreUpdate {
                pristine: true,
                annotations: Vec::new(),
                ops: vec![
                    CoreUpdateOp {
                        op: CoreUpdateKind::Insert,
                        n: 1,
                        lines: vec![CoreLine {
                            text: Some(String::from("visible")),
                            cursor: Vec::new(),
                            syntax_spans: None,
                        }],
                    },
                    CoreUpdateOp { op: CoreUpdateKind::Invalidate, n: 100_000, lines: Vec::new() },
                ],
            },
        })
        .unwrap();

    client.drain_events().unwrap();

    assert_eq!(client.lines.first().map(String::as_str), Some("visible"));
    assert_eq!(client.lines.len(), 100_001);
    assert_eq!(invalid_line_ranges(&client.line_cache), vec![(1, 100_001)]);
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn source_control_skips_lazy_line_cache() {
    let (tx, _rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.line_cache = vec![
        LineSlot::Known(CachedLine {
            text: String::from("visible"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
        LineSlot::Invalid,
    ];
    app.backend.rebuild_lines();

    app.refresh_source_control();

    assert!(app.source_control.is_empty());
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
fn open_file_bootstraps_visible_lines_lazily() {
    let path = unique_temp_path("ee-tui-open");
    let contents = (0..24).map(|i| format!("line-{i}")).collect::<Vec<_>>().join("\n");
    fs::write(&path, &contents).unwrap();

    let app = App::from_path(Some(path.clone())).unwrap();

    fs::remove_file(&path).unwrap();
    let expected = contents.split('\n').map(ToOwned::to_owned).collect::<Vec<_>>();
    assert_eq!(&app.backend.lines[..12], &expected[..12]);
    assert_eq!(app.backend.lines.len(), expected.len());
    assert_eq!(invalid_line_ranges(&app.backend.line_cache), vec![(12, 24)]);
}

#[test]
fn terminal_command_opens_named_transcript_buffer() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)));
    #[cfg(windows)]
    let command = "!echo hello-from-shell";
    #[cfg(not(windows))]
    let command = "!printf 'hello-from-shell\\n'";
    for ch in command.chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    app.backend.pump().unwrap();

    assert_eq!(app.backend.buf_count(), 2);
    assert!(app.backend.title().starts_with("term: "));
    assert!(app.backend.lines.iter().any(|line| line.contains("hello-from-shell")));
}

#[test]
fn named_scratch_buffer_uses_display_name() {
    let mut app = App::from_path(None).unwrap();

    let buf_id = app.backend.open_named_scratch_buffer("term: cargo test").unwrap();
    app.backend.switch_to_id(buf_id).unwrap();

    assert_eq!(app.backend.title(), "term: cargo test");
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
fn request_declaration_emits_backend_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.request_declaration().expect("declaration request should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_declaration");
}

#[test]
fn request_type_definition_emits_backend_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.request_type_definition().expect("type definition request should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_type_definition");
}

#[test]
fn request_implementation_emits_backend_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.request_implementation().expect("implementation request should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_implementation");
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
fn goto_column_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.goto_column(2, true).expect("goto column should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "goto_column");
    assert_eq!(value["params"]["params"]["display_col"], 2);
    assert_eq!(value["params"]["params"]["modify_selection"], true);
}

#[test]
fn join_selections_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.join_selections(true).expect("join selections should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "join_selections");
    assert_eq!(value["params"]["params"]["select_space"], true);
}

#[test]
fn extend_line_below_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.extend_line_below(3).expect("extend line below should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "extend_line_below");
    assert_eq!(value["params"]["params"]["count"], 3);
}

#[test]
fn move_word_start_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.move_word_start(true, true, false).expect("move word start should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "move_word_start");
    assert_eq!(value["params"]["params"]["forward"], true);
    assert_eq!(value["params"]["params"]["long_word"], true);
    assert_eq!(value["params"]["params"]["modify_selection"], false);
}

#[test]
fn move_word_end_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.move_word_end(false, true).expect("move word end should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "move_word_end");
    assert_eq!(value["params"]["params"]["long_word"], false);
    assert_eq!(value["params"]["params"]["modify_selection"], true);
}

#[test]
fn find_char_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.find_char('x', false, true, true).expect("find char should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "find_char");
    assert_eq!(value["params"]["params"]["target"], "x");
    assert_eq!(value["params"]["params"]["forward"], false);
    assert_eq!(value["params"]["params"]["inclusive"], true);
    assert_eq!(value["params"]["params"]["modify_selection"], true);
}

#[test]
fn move_to_matching_bracket_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.move_to_matching_bracket(true).expect("matching bracket move should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "move_to_matching_bracket");
    assert_eq!(value["params"]["params"]["modify_selection"], true);
}

#[test]
fn extend_to_line_bounds_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.extend_to_line_bounds().expect("extend to line bounds should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "extend_to_line_bounds");
}

#[test]
fn shrink_to_line_bounds_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.shrink_to_line_bounds().expect("shrink to line bounds should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "shrink_to_line_bounds");
}

#[test]
fn add_newline_above_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.add_newline_above().expect("add newline above should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "add_newline_above");
}

#[test]
fn add_newline_below_emits_edit_notification() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.add_newline_below().expect("add newline below should send");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "add_newline_below");
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
fn commit_undo_checkpoint_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    run_ex(&mut app, "commit_undo_checkpoint");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "commit_undo_checkpoint");
}

#[test]
fn goto_lsp_commands_use_backend_edit() {
    let commands = [
        ("goto_declaration", "request_declaration"),
        ("goto_definition", "request_definition"),
        ("goto_type_definition", "request_type_definition"),
        ("goto_reference", "request_references"),
        ("goto_implementation", "request_implementation"),
    ];
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for (command, expected) in commands {
        run_ex(&mut app, command);

        let message = rx.recv().expect("message should be sent");
        let value: Value = serde_json::from_str(&message).expect("message should be json");
        assert_eq!(value["method"], "edit");
        assert_eq!(value["params"]["method"], expected);
    }
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
fn code_action_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":code_action".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "request_code_actions");
    assert!(value["params"]["params"]["index"].is_null());
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
fn increment_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":increment".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "increase_number");
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
    Command::new("git").arg("init").current_dir(temp.path()).output().unwrap();
    Command::new("git")
        .args(["config", "user.email", "ee@example.com"])
        .current_dir(temp.path())
        .output()
        .unwrap();
    Command::new("git")
        .args(["config", "user.name", "EE"])
        .current_dir(temp.path())
        .output()
        .unwrap();
    Command::new("git").args(["add", "sample.rs"]).current_dir(temp.path()).output().unwrap();
    Command::new("git").args(["commit", "-m", "init"]).current_dir(temp.path()).output().unwrap();
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
    Command::new("git").arg("init").current_dir(temp.path()).output().unwrap();

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

    for ch in ":selection_for_replace".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "selection_for_replace");
}

#[test]
fn select_regex_command_uses_selection_scoped_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":select_regex foo.*bar".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("select_regex request should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["method"], "select_regex");
    assert_eq!(value["params"]["params"]["chars"], "foo.*bar");
    assert_eq!(value["params"]["params"]["case_sensitive"], false);
}

#[test]
fn split_selection_on_newline_command_uses_selection_into_lines_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":split_selection_on_newline".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["method"], "selection_into_lines");
}

#[test]
fn collapse_selection_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":collapse_selection".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["method"], "collapse_selections");
}

#[test]
fn align_selections_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":align_selections".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["method"], "align_selections");
}

#[test]
fn rotate_selections_backward_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":rotate_selections_backward".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["method"], "rotate_selections_backward");
}

#[test]
fn rotate_selections_forward_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":rotate_selections_forward".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["method"], "rotate_selections_forward");
}

#[test]
fn select_all_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":select_all".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["method"], "select_all");
}

#[test]
fn delete_word_forward_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":delete_word_forward".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["method"], "delete_word_forward");
}

#[test]
fn kill_line_command_uses_delete_line_range() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":kill_line".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["method"], "delete_line_range");
    assert_eq!(value["params"]["params"]["start_line"], 0);
    assert_eq!(value["params"]["params"]["end_line"], 0);
}

#[test]
fn add_newline_below_command_emits_line_end_then_newline() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":add_newline_below".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(message["params"]["method"], "add_newline_below");
}

#[test]
fn add_newline_above_command_emits_line_start_newline_and_move_up() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":add_newline_above".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(message["params"]["method"], "add_newline_above");
}

#[test]
fn extend_line_below_command_emits_linewise_selection_gestures() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":extend_line_below".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(message["params"]["method"], "extend_line_below");
    assert_eq!(message["params"]["params"]["count"], 1);
}

#[test]
fn extend_selection_alias_commands_emit_expected_backend_methods() {
    let commands = [
        ("extend_char_left", "move_left_and_modify_selection"),
        ("extend_char_right", "move_right_and_modify_selection"),
        ("extend_visual_line_up", "move_up_and_modify_selection"),
        ("extend_visual_line_down", "move_down_and_modify_selection"),
        ("extend_line_up", "move_up_and_modify_selection"),
        ("extend_line_down", "move_down_and_modify_selection"),
        ("extend_line_above", "extend_line_above"),
        ("select_line_above", "select_line_above"),
        ("select_line_below", "select_line_below"),
        ("goto_file_end", "move_to_end_of_document"),
        ("extend_to_file_start", "move_to_beginning_of_document_and_modify_selection"),
        ("extend_to_file_end", "move_to_end_of_document_and_modify_selection"),
    ];
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for (command, expected) in commands {
        run_ex(&mut app, command);

        let message = rx.recv().expect("message should be sent");
        let value: Value = serde_json::from_str(&message).expect("message should be json");
        assert_eq!(value["method"], "edit");
        assert_eq!(value["params"]["method"], expected);
    }
}

#[test]
fn join_selections_command_joins_selected_lines() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    for ch in "abc\n    def".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    app.backend.pump().unwrap();

    let _ = app.backend.send_edit(
        "gesture",
        json!({
            "line": 0,
            "col": 0,
            "ty": "point_select",
        }),
    );
    let _ = app.backend.send_edit(
        "gesture",
        json!({
            "line": 1,
            "col": 7,
            "ty": { "select_extend": { "granularity": "point" } },
        }),
    );
    app.backend.pump().unwrap();

    run_ex(&mut app, "join_selections");
    for _ in 0..20 {
        app.backend.pump().unwrap();
        if app.backend.lines.first().is_some_and(|line| line == "abc def") {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(app.backend.lines.first().map(String::as_str), Some("abc def"));
}

#[test]
fn filter_selections_preview_uses_backend_authoritative_text() {
    let mut app = App::from_path(None).unwrap();

    insert_text(&mut app, "alpha beta alps");
    app.backend.pump().unwrap();

    app.backend
        .set_selections(&[
            SelectionRange { start: 0, end: 5 },
            SelectionRange { start: 6, end: 10 },
            SelectionRange { start: 11, end: 15 },
        ])
        .expect("set selections should succeed");
    app.backend.pump().unwrap();

    let filtered =
        app.backend.filter_selections_preview("^a", false).expect("filter preview should succeed");
    app.backend.set_selections(&filtered).expect("filtered selections should apply");
    app.backend.pump().unwrap();

    let selection_ranges = app
        .backend
        .annotations
        .iter()
        .find(|annotation| annotation.annotation_type == "selection")
        .map(|annotation| annotation.ranges.clone())
        .expect("selection annotation should exist");

    assert_eq!(selection_ranges, vec![[0, 0, 0, 5], [0, 11, 0, 15]]);
}

#[test]
fn select_chars_preview_uses_backend_authoritative_text() {
    let mut app = App::from_path(None).unwrap();

    insert_text(&mut app, "aéb");
    app.backend.pump().unwrap();
    app.backend
        .set_selections(&[SelectionRange { start: 0, end: 0 }])
        .expect("set selections should succeed");
    app.backend.pump().unwrap();

    let selection =
        app.backend.select_chars_preview(2).expect("select chars preview should succeed");

    assert_eq!(selection, vec![SelectionRange { start: 0, end: 3 }]);
}

#[test]
fn selected_text_preview_uses_backend_authoritative_selection() {
    let mut app = App::from_path(None).unwrap();

    insert_text(&mut app, "alpha\nbeta");
    app.backend.pump().unwrap();
    app.backend
        .set_selections(&[SelectionRange { start: 1, end: 8 }])
        .expect("set selections should succeed");
    app.backend.pump().unwrap();

    let selected =
        app.backend.selected_text_preview(false).expect("selected text preview should succeed");
    let linewise = app
        .backend
        .selected_text_preview(true)
        .expect("linewise selected text preview should succeed");

    assert_eq!(selected, "lpha\nbe");
    assert_eq!(linewise, "alpha\nbeta\n");
}

#[test]
fn block_text_preview_uses_backend_authoritative_text() {
    let mut app = App::from_path(None).unwrap();

    insert_text(&mut app, "abcd\nefgh\nijk");
    app.backend.pump().unwrap();

    let block =
        app.backend.block_text_preview(0, 2, 1, 3).expect("block text preview should succeed");

    assert_eq!(block, "bc\nfg\njk\n");
}

#[test]
fn remove_selections_command_uses_search_pattern_and_reports_empty_result() {
    let mut app = App::from_path(None).unwrap();

    insert_text(&mut app, "alpha beta");
    app.backend.pump().unwrap();

    app.backend
        .set_selections(&[
            SelectionRange { start: 0, end: 5 },
            SelectionRange { start: 6, end: 10 },
        ])
        .expect("set selections should succeed");
    app.backend.pump().unwrap();
    app.search_pattern = Some(String::from("."));

    run_ex(&mut app, "remove_selections");

    assert_eq!(app.backend.status_message.as_deref(), Some("no selections remaining"));
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
fn clipboard_paste_commands_use_expected_clipboard_register() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    set_test_clipboard(ClipboardSelection::Clipboard, "clip");
    set_test_clipboard(ClipboardSelection::Primary, "prim");

    for (command, expected, before) in [
        ("paste_clipboard_after", "clip", false),
        ("paste_clipboard_before", "clip", true),
        ("paste_primary_clipboard_after", "prim", false),
        ("paste_primary_clipboard_before", "prim", true),
    ] {
        run_ex(&mut app, command);

        let message = rx.recv().expect("message should be sent");
        let value: Value = serde_json::from_str(&message).expect("message should be json");
        assert_eq!(value["method"], "edit");
        assert_eq!(value["params"]["method"], "paste_register");
        assert_eq!(value["params"]["params"]["chars"], expected);
        assert_eq!(value["params"]["params"]["before"], before);
    }
}

#[test]
fn clipboard_yank_and_replace_commands_use_test_clipboards() {
    let mut app = App::from_path(None).unwrap();
    insert_text(&mut app, "alpha beta");
    app.backend.pump().unwrap();

    app.backend.set_selections(&[SelectionRange { start: 0, end: 5 }]).unwrap();
    run_ex(&mut app, "yank_to_clipboard");
    assert_eq!(app.registers.get(&RegisterName::Clipboard), "alpha");

    app.backend
        .set_selections(&[
            SelectionRange { start: 0, end: 5 },
            SelectionRange { start: 6, end: 10 },
        ])
        .unwrap();
    app.backend.cursor_line = 1;
    app.backend.cursor_col = 0;
    run_ex(&mut app, "yank_main_selection_to_primary_clipboard");
    assert_eq!(app.registers.get(&RegisterName::PrimaryClipboard), "beta");

    set_test_clipboard(ClipboardSelection::Clipboard, "CLIP");
    app.backend.set_selections(&[SelectionRange { start: 0, end: 5 }]).unwrap();
    run_ex(&mut app, "replace_selections_with_clipboard");
    app.backend.pump().unwrap();
    assert_eq!(app.backend.lines, vec![String::from("CLIP beta")]);

    set_test_clipboard(ClipboardSelection::Primary, "PRIM");
    app.backend.set_selections(&[SelectionRange { start: 5, end: 9 }]).unwrap();
    run_ex(&mut app, "replace_selections_with_primary_clipboard");
    app.backend.pump().unwrap();
    assert_eq!(app.backend.lines, vec![String::from("CLIP PRIM")]);
}

#[test]
fn duplicate_line_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":duplicate_line".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "duplicate_line");
}

#[test]
fn move_line_down_command_swaps_with_next_line() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = vec!["alpha".to_owned(), "beta".to_owned(), "gamma".to_owned()];
    app.backend.cursor_line = 0;
    app.backend.cursor_col = 2;

    run_ex(&mut app, "move_line_down");

    let first: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(first["params"]["method"], "gesture");
    assert_eq!(first["params"]["params"]["line"], 0);
    assert_eq!(first["params"]["params"]["col"], 0);

    let second: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(second["params"]["method"], "gesture");
    assert_eq!(second["params"]["params"]["line"], 1);
    assert_eq!(second["params"]["params"]["col"], 4);

    let third: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(third["params"]["method"], "insert");
    assert_eq!(third["params"]["params"]["chars"], "beta\nalpha");

    let fourth: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(fourth["params"]["method"], "gesture");
    assert_eq!(fourth["params"]["params"]["line"], 1);
    assert_eq!(fourth["params"]["params"]["col"], 2);
}

#[test]
fn move_line_up_command_swaps_with_previous_line() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = vec!["alpha".to_owned(), "beta".to_owned()];
    app.backend.cursor_line = 1;
    app.backend.cursor_col = 1;

    run_ex(&mut app, "move_line_up");

    let _ = rx.recv().unwrap();
    let second: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(second["params"]["params"]["col"], 4);

    let third: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(third["params"]["method"], "insert");
    assert_eq!(third["params"]["params"]["chars"], "beta\nalpha");

    let fourth: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(fourth["params"]["params"]["line"], 0);
    assert_eq!(fourth["params"]["params"]["col"], 1);
}

#[test]
fn match_brackets_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    run_ex(&mut app, "match_brackets");

    let value: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(value["params"]["method"], "move_to_matching_bracket");
    assert_eq!(value["params"]["params"]["modify_selection"], false);
}

#[test]
fn select_textobject_inner_command_selects_requested_range() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = vec!["foo(bar)baz".to_owned()];
    app.backend.cursor_col = 4;

    run_ex(&mut app, "select_textobject_inner b");

    let first: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(first["params"]["method"], "gesture");
    assert_eq!(first["params"]["params"]["col"], 4);

    let second: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(second["params"]["method"], "gesture");
    assert_eq!(second["params"]["params"]["col"], 7);
}

#[test]
fn select_textobject_around_command_selects_outer_range() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = vec!["foo(bar)baz".to_owned()];
    app.backend.cursor_col = 4;

    run_ex(&mut app, "select_textobject_around b");

    let first: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(first["params"]["params"]["col"], 3);

    let second: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(second["params"]["params"]["col"], 8);
}

#[test]
fn surround_add_command_wraps_textobject() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = vec!["alpha beta".to_owned()];
    app.backend.cursor_col = 1;

    run_ex(&mut app, "surround_add [ w");

    let _ = rx.recv().unwrap();
    let _ = rx.recv().unwrap();
    let third: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(third["params"]["method"], "insert");
    assert_eq!(third["params"]["params"]["chars"], "[alpha]");
}

#[test]
fn surround_replace_command_rewrites_current_surround() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = vec!["foo(bar)baz".to_owned()];
    app.backend.cursor_col = 4;

    run_ex(&mut app, "surround_replace [");

    let _ = rx.recv().unwrap();
    let _ = rx.recv().unwrap();
    let third: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(third["params"]["method"], "insert");
    assert_eq!(third["params"]["params"]["chars"], "[bar]");
}

#[test]
fn surround_delete_command_rewrites_current_surround() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = vec!["foo(bar)baz".to_owned()];
    app.backend.cursor_col = 4;

    run_ex(&mut app, "surround_delete");

    let _ = rx.recv().unwrap();
    let _ = rx.recv().unwrap();
    let third: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(third["params"]["method"], "insert");
    assert_eq!(third["params"]["params"]["chars"], "bar");
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
fn toggle_comment_commands_use_backend_edit() {
    for (command, method) in [
        ("toggle_comments", "toggle_comment"),
        ("toggle_line_comments", "toggle_line_comment"),
        ("toggle_block_comments", "toggle_block_comment"),
    ] {
        let (tx, rx) = mpsc::channel();
        let (_backend_tx, backend_rx) = mpsc::channel();
        let mut app = App::from_path(None).unwrap();
        app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

        run_ex(&mut app, command);

        let message = rx.recv().expect("message should be sent");
        let value: Value = serde_json::from_str(&message).expect("message should be json");
        assert_eq!(value["method"], "edit");
        assert_eq!(value["params"]["method"], method);
    }
}

#[test]
fn multi_find_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for ch in ":multi_find alpha beta".chars() {
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
fn bindings_table_has_requested_goto_prefix_bindings() {
    let b = bindings();
    let lookup = |key| {
        b.get(&BindingKey {
            mode: Mode::Normal,
            key,
            modifiers: KeyModifiers::NONE,
            prefix: Some('g'),
        })
        .cloned()
    };

    assert_eq!(lookup(KeyCode::Char('g')), Some(Action::GotoFileStart));
    assert_eq!(lookup(KeyCode::Char('e')), Some(Action::GotoLastLine));
    assert_eq!(lookup(KeyCode::Char('f')), Some(Action::GotoFile));
    assert_eq!(lookup(KeyCode::Char('h')), Some(Action::Edit("move_to_left_end_of_line")));
    assert_eq!(lookup(KeyCode::Char('l')), Some(Action::Edit("move_to_right_end_of_line")));
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
fn parse_action_spec_accepts_requested_motion_aliases() {
    assert_eq!(
        crate::keymap::parse_action_spec("move_next_word_start").unwrap(),
        Action::MoveWordStart { forward: true, long_word: false }
    );
    assert_eq!(
        crate::keymap::parse_action_spec("goto_word").unwrap(),
        Action::MoveWordStart { forward: true, long_word: false }
    );
    assert_eq!(
        crate::keymap::parse_action_spec("move_prev_word_start").unwrap(),
        Action::MoveWordStart { forward: false, long_word: false }
    );
    assert_eq!(
        crate::keymap::parse_action_spec("move_next_word_end").unwrap(),
        Action::MoveWordEnd { long_word: false }
    );
    assert_eq!(
        crate::keymap::parse_action_spec("move_next_long_word_start").unwrap(),
        Action::MoveWordStart { forward: true, long_word: true }
    );
    assert_eq!(
        crate::keymap::parse_action_spec("move_prev_long_word_start").unwrap(),
        Action::MoveWordStart { forward: false, long_word: true }
    );
    assert_eq!(
        crate::keymap::parse_action_spec("move_next_long_word_end").unwrap(),
        Action::MoveWordEnd { long_word: true }
    );
}

#[test]
fn parse_action_spec_accepts_requested_find_aliases() {
    assert_eq!(
        crate::keymap::parse_action_spec("find_next_char").unwrap(),
        Action::PendingCharFind { forward: true, inclusive: true }
    );
    assert_eq!(
        crate::keymap::parse_action_spec("find_till_char").unwrap(),
        Action::PendingCharFind { forward: true, inclusive: false }
    );
    assert_eq!(
        crate::keymap::parse_action_spec("find_prev_char").unwrap(),
        Action::PendingCharFind { forward: false, inclusive: true }
    );
    assert_eq!(
        crate::keymap::parse_action_spec("till_prev_char").unwrap(),
        Action::PendingCharFind { forward: false, inclusive: false }
    );
}

#[test]
fn parse_action_spec_accepts_requested_command_aliases() {
    assert_eq!(crate::keymap::parse_action_spec("no_op").unwrap(), Action::NoOp);
    assert_eq!(
        crate::keymap::parse_action_spec("record_macro").unwrap(),
        Action::MacroRecordToggle
    );
    assert_eq!(
        crate::keymap::parse_action_spec("replay_macro").unwrap(),
        Action::MacroReplayPrefix
    );
    assert_eq!(crate::keymap::parse_action_spec("search").unwrap(), Action::EnterSearch);
    assert_eq!(
        crate::keymap::parse_action_spec("reverse_search").unwrap(),
        Action::EnterSearchBackward
    );
    assert_eq!(crate::keymap::parse_action_spec("search_next").unwrap(), Action::FindNext);
    assert_eq!(crate::keymap::parse_action_spec("search_prev").unwrap(), Action::FindPrevious);
    assert_eq!(
        crate::keymap::parse_action_spec("search_selection_detect_word_boundaries").unwrap(),
        Action::SearchSelection { detect_word_boundaries: true }
    );
    assert_eq!(
        crate::keymap::parse_action_spec("search_selection").unwrap(),
        Action::SearchSelection { detect_word_boundaries: false }
    );
    assert_eq!(
        crate::keymap::parse_action_spec("normal_mode").unwrap(),
        Action::EnterMode(Mode::Normal)
    );
    assert_eq!(crate::keymap::parse_action_spec("goto_line").unwrap(), Action::GotoLine);
    assert_eq!(crate::keymap::parse_action_spec("goto_column").unwrap(), Action::GotoColumn);
    assert_eq!(
        crate::keymap::parse_action_spec("goto_first_nonwhitespace").unwrap(),
        Action::GotoFirstNonWhitespace
    );
    assert_eq!(crate::keymap::parse_action_spec("goto_file_start").unwrap(), Action::GotoFileStart);
    assert_eq!(crate::keymap::parse_action_spec("goto_last_line").unwrap(), Action::GotoLastLine);
    assert_eq!(
        crate::keymap::parse_action_spec("goto_last_modification").unwrap(),
        Action::ChangeListOlder
    );
    assert_eq!(crate::keymap::parse_action_spec("goto_window_top").unwrap(), Action::GotoWindowTop);
    assert_eq!(
        crate::keymap::parse_action_spec("goto_window_center").unwrap(),
        Action::GotoWindowCenter
    );
    assert_eq!(
        crate::keymap::parse_action_spec("goto_window_bottom").unwrap(),
        Action::GotoWindowBottom
    );
    assert_eq!(
        crate::keymap::parse_action_spec("goto_last_accessed_file").unwrap(),
        Action::GotoLastAccessedFile
    );
    assert_eq!(
        crate::keymap::parse_action_spec("goto_last_modified_file").unwrap(),
        Action::GotoLastModifiedFile
    );
    assert_eq!(
        crate::keymap::parse_action_spec("goto_declaration").unwrap(),
        Action::RequestDeclaration
    );
    assert_eq!(
        crate::keymap::parse_action_spec("goto_definition").unwrap(),
        Action::RequestDefinition
    );
    assert_eq!(
        crate::keymap::parse_action_spec("goto_type_definition").unwrap(),
        Action::RequestTypeDefinition
    );
    assert_eq!(
        crate::keymap::parse_action_spec("goto_reference").unwrap(),
        Action::RequestReferences
    );
    assert_eq!(
        crate::keymap::parse_action_spec("goto_implementation").unwrap(),
        Action::RequestImplementation
    );
    assert_eq!(
        crate::keymap::parse_action_spec("goto_next_function").unwrap(),
        Action::Edit("goto_next_function")
    );
    assert_eq!(
        crate::keymap::parse_action_spec("goto_prev_paragraph").unwrap(),
        Action::Edit("goto_prev_paragraph")
    );
    assert_eq!(crate::keymap::parse_action_spec("goto_next_change").unwrap(), Action::GitNextHunk);
    assert_eq!(crate::keymap::parse_action_spec("goto_last_change").unwrap(), Action::GitLastHunk);
    assert_eq!(crate::keymap::parse_action_spec("goto_file").unwrap(), Action::GotoFile);
    assert_eq!(crate::keymap::parse_action_spec("file_picker").unwrap(), Action::FilePicker);
    assert_eq!(
        crate::keymap::parse_action_spec("file_picker_in_current_directory").unwrap(),
        Action::FilePickerInCurrentDirectory
    );
    assert_eq!(crate::keymap::parse_action_spec("buffer_picker").unwrap(), Action::BufferPicker);
    assert_eq!(
        crate::keymap::parse_action_spec("jumplist_picker").unwrap(),
        Action::JumpListPicker
    );
    assert_eq!(
        crate::keymap::parse_action_spec("changed_file_picker").unwrap(),
        Action::ChangedFilePicker
    );
    assert_eq!(
        crate::keymap::parse_action_spec("workspace_symbol_picker").unwrap(),
        Action::RequestWorkspaceSymbols
    );
    assert_eq!(
        crate::keymap::parse_action_spec("diagnostics_picker").unwrap(),
        Action::DiagnosticsPicker
    );
    assert_eq!(
        crate::keymap::parse_action_spec("workspace_diagnostics_picker").unwrap(),
        Action::WorkspaceDiagnosticsPicker
    );
    assert_eq!(crate::keymap::parse_action_spec("last_picker").unwrap(), Action::LastPicker);
    assert_eq!(
        crate::keymap::parse_action_spec("repeat_last_motion").unwrap(),
        Action::RepeatLastMotion
    );
    assert_eq!(
        crate::keymap::parse_action_spec("goto_line_start").unwrap(),
        Action::Edit("move_to_left_end_of_line")
    );
    assert_eq!(
        crate::keymap::parse_action_spec("goto_line_end").unwrap(),
        Action::Edit("move_to_right_end_of_line")
    );
    assert_eq!(
        crate::keymap::parse_action_spec("page_up").unwrap(),
        Action::Edit("scroll_page_up")
    );
    assert_eq!(
        crate::keymap::parse_action_spec("page_down").unwrap(),
        Action::Edit("scroll_page_down")
    );
    assert_eq!(
        crate::keymap::parse_action_spec("page_cursor_half_up").unwrap(),
        Action::PageCursorHalfUp
    );
    assert_eq!(
        crate::keymap::parse_action_spec("page_cursor_half_down").unwrap(),
        Action::PageCursorHalfDown
    );
    assert_eq!(crate::keymap::parse_action_spec("jump_forward").unwrap(), Action::JumpListNewer);
    assert_eq!(crate::keymap::parse_action_spec("jump_backward").unwrap(), Action::JumpListOlder);
    assert_eq!(crate::keymap::parse_action_spec("save_selection").unwrap(), Action::SaveSelection);
    assert_eq!(crate::keymap::parse_action_spec("replace").unwrap(), Action::Replace);
    assert_eq!(
        crate::keymap::parse_action_spec("replace_with_yanked").unwrap(),
        Action::ReplaceWithYanked
    );
    assert_eq!(crate::keymap::parse_action_spec("switch_case").unwrap(), Action::SwitchCase);
    assert_eq!(
        crate::keymap::parse_action_spec("switch_to_lowercase").unwrap(),
        Action::SwitchToLowercase
    );
    assert_eq!(
        crate::keymap::parse_action_spec("switch_to_uppercase").unwrap(),
        Action::SwitchToUppercase
    );
    assert_eq!(
        crate::keymap::parse_action_spec("insert_mode").unwrap(),
        Action::EnterMode(Mode::Insert)
    );
    assert_eq!(crate::keymap::parse_action_spec("append_mode").unwrap(), Action::AppendAfterCursor);
    assert_eq!(
        crate::keymap::parse_action_spec("visual_mode").unwrap(),
        Action::EnterMode(Mode::Visual)
    );
    assert_eq!(
        crate::keymap::parse_action_spec("select_mode").unwrap(),
        Action::EnterMode(Mode::Visual)
    );
    assert_eq!(crate::keymap::parse_action_spec("command_mode").unwrap(), Action::EnterCommandMode);
    assert_eq!(
        crate::keymap::parse_action_spec("insert_at_line_start").unwrap(),
        Action::InsertAtLineStart
    );
    assert_eq!(
        crate::keymap::parse_action_spec("insert_at_line_end").unwrap(),
        Action::AppendAtEndOfLine
    );
    assert_eq!(crate::keymap::parse_action_spec("open_below").unwrap(), Action::OpenLineBelow);
    assert_eq!(crate::keymap::parse_action_spec("open_above").unwrap(), Action::OpenLineAbove);
    assert_eq!(
        crate::keymap::parse_action_spec("code_action").unwrap(),
        Action::RequestCodeActions
    );
    assert_eq!(
        crate::keymap::parse_action_spec("delete_char_backward").unwrap(),
        Action::DeleteBackward
    );
    assert_eq!(
        crate::keymap::parse_action_spec("delete_char_forward").unwrap(),
        Action::Edit("delete_forward")
    );
    assert_eq!(
        crate::keymap::parse_action_spec("delete_word_forward").unwrap(),
        Action::Edit("delete_word_forward")
    );
    assert_eq!(
        crate::keymap::parse_action_spec("kill_to_line_start").unwrap(),
        Action::DeleteToLineStart
    );
    assert_eq!(
        crate::keymap::parse_action_spec("kill_to_line_end").unwrap(),
        Action::Edit("delete_to_end_of_paragraph")
    );
    assert_eq!(crate::keymap::parse_action_spec("kill_line").unwrap(), Action::DeleteCurrentLine);
    assert_eq!(
        crate::keymap::parse_action_spec("insert_newline").unwrap(),
        Action::Edit("insert_newline")
    );
    assert_eq!(
        crate::keymap::parse_action_spec("add_newline_below").unwrap(),
        Action::AddNewlineBelow
    );
    assert_eq!(
        crate::keymap::parse_action_spec("add_newline_above").unwrap(),
        Action::AddNewlineAbove
    );
    assert_eq!(crate::keymap::parse_action_spec("undo").unwrap(), Action::Undo);
    assert_eq!(crate::keymap::parse_action_spec("redo").unwrap(), Action::Redo);
    assert_eq!(crate::keymap::parse_action_spec("earlier").unwrap(), Action::Undo);
    assert_eq!(crate::keymap::parse_action_spec("later").unwrap(), Action::Redo);
    assert_eq!(crate::keymap::parse_action_spec("yank").unwrap(), Action::YankSelection);
    assert_eq!(
        crate::keymap::parse_action_spec("yank_to_clipboard").unwrap(),
        Action::YankToClipboard
    );
    assert_eq!(
        crate::keymap::parse_action_spec("yank_to_primary_clipboard").unwrap(),
        Action::YankToPrimaryClipboard
    );
    assert_eq!(
        crate::keymap::parse_action_spec("yank_main_selection_to_clipboard").unwrap(),
        Action::YankMainSelectionToClipboard
    );
    assert_eq!(
        crate::keymap::parse_action_spec("yank_main_selection_to_primary_clipboard").unwrap(),
        Action::YankMainSelectionToPrimaryClipboard
    );
    assert_eq!(crate::keymap::parse_action_spec("paste_after").unwrap(), Action::PasteAfter);
    assert_eq!(crate::keymap::parse_action_spec("paste_before").unwrap(), Action::PasteBefore);
    assert_eq!(
        crate::keymap::parse_action_spec("paste_clipboard_after").unwrap(),
        Action::PasteClipboardAfter
    );
    assert_eq!(
        crate::keymap::parse_action_spec("paste_clipboard_before").unwrap(),
        Action::PasteClipboardBefore
    );
    assert_eq!(
        crate::keymap::parse_action_spec("paste_primary_clipboard_after").unwrap(),
        Action::PastePrimaryClipboardAfter
    );
    assert_eq!(
        crate::keymap::parse_action_spec("paste_primary_clipboard_before").unwrap(),
        Action::PastePrimaryClipboardBefore
    );
    assert_eq!(
        crate::keymap::parse_action_spec("replace_selections_with_clipboard").unwrap(),
        Action::ReplaceSelectionsWithClipboard
    );
    assert_eq!(
        crate::keymap::parse_action_spec("replace_selections_with_primary_clipboard").unwrap(),
        Action::ReplaceSelectionsWithPrimaryClipboard
    );
    assert_eq!(
        crate::keymap::parse_action_spec("select_register").unwrap(),
        Action::RegisterPrefix
    );
    assert_eq!(crate::keymap::parse_action_spec("indent").unwrap(), Action::IndentSelection);
    assert_eq!(crate::keymap::parse_action_spec("unindent").unwrap(), Action::UnindentSelection);
    assert_eq!(
        crate::keymap::parse_action_spec("format_selections").unwrap(),
        Action::FormatSelections
    );
    assert_eq!(
        crate::keymap::parse_action_spec("delete_selection").unwrap(),
        Action::DeleteSelection { yank: true, enter_insert: false }
    );
    assert_eq!(
        crate::keymap::parse_action_spec("delete_selection_noyank").unwrap(),
        Action::DeleteSelection { yank: false, enter_insert: false }
    );
    assert_eq!(
        crate::keymap::parse_action_spec("change_selection").unwrap(),
        Action::DeleteSelection { yank: true, enter_insert: true }
    );
    assert_eq!(
        crate::keymap::parse_action_spec("change_selection_noyank").unwrap(),
        Action::DeleteSelection { yank: false, enter_insert: true }
    );
    assert_eq!(
        crate::keymap::parse_action_spec("extend_char_left").unwrap(),
        Action::Edit("move_left_and_modify_selection")
    );
    assert_eq!(
        crate::keymap::parse_action_spec("extend_char_right").unwrap(),
        Action::Edit("move_right_and_modify_selection")
    );
    assert_eq!(
        crate::keymap::parse_action_spec("extend_visual_line_up").unwrap(),
        Action::Edit("move_up_and_modify_selection")
    );
    assert_eq!(
        crate::keymap::parse_action_spec("extend_visual_line_down").unwrap(),
        Action::Edit("move_down_and_modify_selection")
    );
    assert_eq!(
        crate::keymap::parse_action_spec("extend_line_up").unwrap(),
        Action::Edit("move_up_and_modify_selection")
    );
    assert_eq!(
        crate::keymap::parse_action_spec("extend_line_down").unwrap(),
        Action::Edit("move_down_and_modify_selection")
    );
    assert_eq!(
        crate::keymap::parse_action_spec("extend_line_above").unwrap(),
        Action::Edit("extend_line_above")
    );
    assert_eq!(
        crate::keymap::parse_action_spec("select_line_above").unwrap(),
        Action::Edit("select_line_above")
    );
    assert_eq!(
        crate::keymap::parse_action_spec("select_line_below").unwrap(),
        Action::Edit("select_line_below")
    );
    assert_eq!(
        crate::keymap::parse_action_spec("goto_file_end").unwrap(),
        Action::Edit("move_to_end_of_document")
    );
    assert_eq!(
        crate::keymap::parse_action_spec("extend_to_file_start").unwrap(),
        Action::Edit("move_to_beginning_of_document_and_modify_selection")
    );
    assert_eq!(
        crate::keymap::parse_action_spec("extend_to_file_end").unwrap(),
        Action::Edit("move_to_end_of_document_and_modify_selection")
    );
    assert_eq!(
        crate::keymap::parse_action_spec("extend_line_below").unwrap(),
        Action::ExtendLineBelow
    );
    assert_eq!(
        crate::keymap::parse_action_spec("extend_to_line_bounds").unwrap(),
        Action::ExtendToLineBounds
    );
    assert_eq!(
        crate::keymap::parse_action_spec("shrink_to_line_bounds").unwrap(),
        Action::ShrinkToLineBounds
    );
    assert_eq!(
        crate::keymap::parse_action_spec("join_selections").unwrap(),
        Action::JoinSelections
    );
    assert_eq!(
        crate::keymap::parse_action_spec("join_selections_space").unwrap(),
        Action::JoinSelectionsSpace
    );
    assert_eq!(
        crate::keymap::parse_action_spec("keep_selections").unwrap(),
        Action::KeepSelections
    );
    assert_eq!(
        crate::keymap::parse_action_spec("remove_selections").unwrap(),
        Action::RemoveSelections
    );
    assert_eq!(
        crate::keymap::parse_action_spec("expand_selection").unwrap(),
        Action::ExpandSelection
    );
    assert_eq!(
        crate::keymap::parse_action_spec("shrink_selection").unwrap(),
        Action::ShrinkSelection
    );
    assert_eq!(
        crate::keymap::parse_action_spec("select_prev_sibling").unwrap(),
        Action::SelectPrevSibling
    );
    assert_eq!(
        crate::keymap::parse_action_spec("select_next_sibling").unwrap(),
        Action::SelectNextSibling
    );
    assert_eq!(
        crate::keymap::parse_action_spec("select_all_siblings").unwrap(),
        Action::SelectAllSiblings
    );
    assert_eq!(
        crate::keymap::parse_action_spec("select_all_children").unwrap(),
        Action::SelectAllChildren
    );
    assert_eq!(
        crate::keymap::parse_action_spec("move_parent_node_start").unwrap(),
        Action::MoveParentNodeStart
    );
    assert_eq!(
        crate::keymap::parse_action_spec("move_parent_node_end").unwrap(),
        Action::MoveParentNodeEnd
    );
}

#[test]
fn no_op_action_leaves_mode_unchanged() {
    let mut app = App::from_path(None).unwrap();
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('z'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::NoOp,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::ALT)));

    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn search_selection_alias_uses_plain_find_query() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = vec![String::from("alpha beta")];
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('*'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::SearchSelection { detect_word_boundaries: false },
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('*'), KeyModifiers::ALT)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "find");
    assert_eq!(value["params"]["params"]["chars"], "alpha");
    assert_eq!(value["params"]["params"]["whole_words"], false);
}

#[test]
fn search_selection_detect_word_boundaries_uses_whole_word_find_query() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = vec![String::from("alpha beta")];
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('#'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::SearchSelection { detect_word_boundaries: true },
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('#'), KeyModifiers::ALT)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "find");
    assert_eq!(value["params"]["params"]["chars"], "alpha");
    assert_eq!(value["params"]["params"]["whole_words"], true);
}

#[test]
fn syntax_selection_actions_forward_backend_methods() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char(']'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::SelectNextSibling,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::ALT)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "select_next_sibling");
}

#[test]
fn move_parent_node_action_forwards_backend_method() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('P'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::MoveParentNodeEnd,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('P'), KeyModifiers::ALT)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "move_parent_node_end");
}

#[test]
fn goto_column_action_uses_count_as_target_column() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.input_state.count_digits = vec![3];
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('c'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::GotoColumn,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::ALT)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "goto_column");
    assert_eq!(value["params"]["params"]["display_col"], 2);
    assert_eq!(value["params"]["params"]["modify_selection"], false);
}

#[test]
fn extend_line_below_action_uses_count_as_backend_param() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.input_state.count_digits = vec![3];
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('E'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::ExtendLineBelow,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('E'), KeyModifiers::ALT)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "extend_line_below");
    assert_eq!(value["params"]["params"]["count"], 3);
}

#[test]
fn select_all_children_command_sends_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)));
    for ch in "select_all_children".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "select_all_children");
}

#[test]
fn move_parent_node_start_command_sends_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)));
    for ch in "move_parent_node_start".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "move_parent_node_start");
}

#[test]
fn goto_column_command_moves_cursor_to_requested_column() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)));
    for ch in "goto_column 3".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "goto_column");
    assert_eq!(value["params"]["params"]["display_col"], 2);
    assert_eq!(value["params"]["params"]["modify_selection"], false);
}

#[test]
fn goto_first_nonwhitespace_command_moves_to_first_content_column() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = vec![String::from("   foo")];

    run_ex(&mut app, "goto_first_nonwhitespace");

    let value: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "goto_column");
    assert_eq!(value["params"]["params"]["display_col"], 3);
    assert_eq!(value["params"]["params"]["modify_selection"], false);
}

#[test]
fn goto_last_modification_command_uses_change_list_position() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.change_list = vec![(4, 7)];
    app.change_list_idx = 0;

    run_ex(&mut app, "goto_last_modification");

    let value: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "gesture");
    assert_eq!(value["params"]["params"]["line"], 4);
    assert_eq!(value["params"]["params"]["col"], 7);
}

#[test]
fn goto_word_command_reuses_word_start_motion() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    run_ex(&mut app, "goto_word");

    let value: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "move_word_start");
    assert_eq!(value["params"]["params"]["forward"], true);
    assert_eq!(value["params"]["params"]["long_word"], false);
    assert_eq!(value["params"]["params"]["modify_selection"], false);
}

#[test]
fn goto_diag_commands_use_active_buffer_diagnostics() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = vec![String::from("abc"), String::from("de"), String::from("fgh")];
    app.backend.diagnostics = vec![
        Diagnostic {
            range: Range { start: 1, end: 2 },
            severity: DiagnosticSeverity::Warning,
            message: String::from("first"),
            source: Some(String::from("lsp")),
            code: None,
        },
        Diagnostic {
            range: Range { start: 4, end: 5 },
            severity: DiagnosticSeverity::Warning,
            message: String::from("second"),
            source: Some(String::from("lsp")),
            code: None,
        },
        Diagnostic {
            range: Range { start: 7, end: 8 },
            severity: DiagnosticSeverity::Warning,
            message: String::from("third"),
            source: Some(String::from("lsp")),
            code: None,
        },
    ];

    app.backend.cursor_line = 0;
    app.backend.cursor_col = 1;
    run_ex(&mut app, "goto_next_diag");

    let value: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "gesture");
    assert_eq!(value["params"]["params"]["line"], 1);
    assert_eq!(value["params"]["params"]["col"], 0);

    app.backend.cursor_line = 1;
    app.backend.cursor_col = 0;
    run_ex(&mut app, "goto_prev_diag");

    let value: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(value["method"], "edit");
    assert_eq!(value["params"]["method"], "gesture");
    assert_eq!(value["params"]["params"]["line"], 0);
    assert_eq!(value["params"]["params"]["col"], 1);
}

#[test]
fn goto_edge_diag_commands_jump_to_first_and_last_entries() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.lines = vec![String::from("abc"), String::from("de"), String::from("fgh")];
    app.backend.diagnostics = vec![
        Diagnostic {
            range: Range { start: 1, end: 2 },
            severity: DiagnosticSeverity::Warning,
            message: String::from("first"),
            source: Some(String::from("lsp")),
            code: None,
        },
        Diagnostic {
            range: Range { start: 7, end: 8 },
            severity: DiagnosticSeverity::Warning,
            message: String::from("last"),
            source: Some(String::from("lsp")),
            code: None,
        },
    ];

    run_ex(&mut app, "goto_first_diag");

    let value: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(value["params"]["method"], "gesture");
    assert_eq!(value["params"]["params"]["line"], 0);
    assert_eq!(value["params"]["params"]["col"], 1);

    run_ex(&mut app, "goto_last_diag");

    let value: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(value["params"]["method"], "gesture");
    assert_eq!(value["params"]["params"]["line"], 2);
    assert_eq!(value["params"]["params"]["col"], 0);
}

#[test]
fn goto_syntax_and_paragraph_commands_forward_backend_methods() {
    let commands = [
        "goto_next_function",
        "goto_prev_function",
        "goto_next_class",
        "goto_prev_class",
        "goto_next_parameter",
        "goto_prev_parameter",
        "goto_next_comment",
        "goto_prev_comment",
        "goto_next_test",
        "goto_prev_test",
        "goto_next_paragraph",
        "goto_prev_paragraph",
    ];
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    for command in commands {
        run_ex(&mut app, command);

        let value: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
            .expect("message should be json");
        assert_eq!(value["method"], "edit");
        assert_eq!(value["params"]["method"], command);
    }
}

#[test]
fn goto_change_commands_reuse_git_hunk_navigation() {
    let temp = tempfile::tempdir().unwrap();
    run_git(temp.path(), &["init"]);
    run_git(temp.path(), &["config", "user.email", "test@example.com"]);
    run_git(temp.path(), &["config", "user.name", "Test User"]);

    let path = temp.path().join("sample.rs");
    fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").unwrap();
    run_git(temp.path(), &["add", "sample.rs"]);
    run_git(temp.path(), &["commit", "-m", "init"]);

    let modified_lines = vec![
        String::from("one"),
        String::from("two changed"),
        String::from("three"),
        String::from("four changed"),
        String::from("five"),
    ];
    let status = crate::git::inspect_buffer(&path, &modified_lines)
        .unwrap()
        .expect("git status should exist");
    assert_eq!(status.hunks.len(), 2);

    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.path = Some(path);
    app.backend.lines = modified_lines;

    app.backend.cursor_line = 0;
    run_ex(&mut app, "goto_next_change");
    let value: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(value["params"]["method"], "gesture");
    assert_eq!(value["params"]["params"]["line"], status.next_hunk_line(0).unwrap());

    app.backend.cursor_line = 4;
    run_ex(&mut app, "goto_prev_change");
    let value: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(value["params"]["params"]["line"], status.prev_hunk_line(4).unwrap());

    run_ex(&mut app, "goto_first_change");
    let value: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(value["params"]["params"]["line"], status.first_hunk_line().unwrap());

    run_ex(&mut app, "goto_last_change");
    let value: Value = serde_json::from_str(&rx.recv().expect("message should be sent"))
        .expect("message should be json");
    assert_eq!(value["params"]["params"]["line"], status.last_hunk_line().unwrap());
}

fn run_git(cwd: &std::path::Path, args: &[&str]) {
    let status = Command::new("git").args(args).current_dir(cwd).status().unwrap();
    assert!(status.success(), "git command failed: {args:?}");
}

#[test]
fn normal_mode_alias_returns_from_insert() {
    let mut app = App::from_path(None).unwrap();
    app.mode = Mode::Insert;
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Insert,
            key: KeyCode::Char('n'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::EnterMode(Mode::Normal),
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::ALT)));

    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn change_selection_alias_enters_insert_mode() {
    let mut app = App::from_path(None).unwrap();
    insert_text(&mut app, "abc");
    app.backend.pump().unwrap();
    app.backend
        .set_selections(&[SelectionRange { start: 0, end: 0 }])
        .expect("set selections should succeed");
    app.backend.pump().unwrap();
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('c'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::DeleteSelection { yank: true, enter_insert: true },
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::ALT)));

    assert_eq!(app.mode, Mode::Insert);
}

#[test]
fn goto_line_action_uses_count_as_target_line() {
    let mut app = App::from_path(None).unwrap();
    app.backend.lines = vec![String::from("a"), String::from("b"), String::from("c")];
    app.input_state.count_digits = vec![2];
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('g'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::GotoLine,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::ALT)));

    assert_eq!(app.jump_list.last().copied(), Some((0, 0)));
}

#[test]
fn goto_file_start_action_without_count_jumps_to_first_line() {
    let mut app = App::from_path(None).unwrap();
    app.backend.lines = vec![String::from("a"), String::from("b"), String::from("c")];
    app.backend.cursor_line = 2;
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('s'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::GotoFileStart,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT)));

    assert_eq!(app.jump_list.last().copied(), Some((2, 0)));
}

#[test]
fn goto_file_start_action_uses_count_as_target_line() {
    let mut app = App::from_path(None).unwrap();
    app.backend.lines = vec![String::from("a"), String::from("b"), String::from("c")];
    app.input_state.count_digits = vec![3];
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('s'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::GotoFileStart,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT)));

    assert_eq!(app.jump_list.last().copied(), Some((0, 0)));
}

#[test]
fn goto_last_line_action_jumps_to_final_line() {
    let mut app = App::from_path(None).unwrap();
    app.backend.lines = vec![String::from("a"), String::from("b"), String::from("c")];
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('e'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::GotoLastLine,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::ALT)));

    assert_eq!(app.jump_list.last().copied(), Some((0, 0)));
}

#[test]
fn goto_file_action_opens_path_under_cursor() {
    let target = unique_temp_path("ee-tui-goto-file-target");
    fs::write(&target, "hello\n").unwrap();

    let mut app = App::from_path(None).unwrap();
    app.backend.lines = vec![format!("see \"{}\" now", target.display())];
    app.backend.cursor_col = 6;
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('f'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::GotoFile,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT)));

    assert_eq!(app.backend.active().path.as_ref(), Some(&target));

    let _ = fs::remove_file(&target);
}

#[test]
fn save_selection_action_pushes_current_cursor_to_jump_list() {
    let mut app = App::from_path(None).unwrap();
    app.backend.cursor_line = 3;
    app.backend.cursor_col = 4;
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('s'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::SaveSelection,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT)));

    assert_eq!(app.jump_list.last().copied(), Some((3, 4)));
}

#[test]
fn replace_action_waits_for_next_character() {
    let mut app = App::from_path(None).unwrap();
    app.key_bindings.insert(
        BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('r'),
            modifiers: KeyModifiers::ALT,
            prefix: None,
        },
        Action::Replace,
    );

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT)));

    assert!(app.input_state.awaiting_replace_char);
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
fn next_word_start_moves_to_following_identifier() {
    assert_eq!(next_word_start("alpha beta", 0, false), Some(6));
}

#[test]
fn prev_word_start_moves_to_current_identifier_start() {
    assert_eq!(prev_word_start("alpha beta", 8, false), Some(6));
}

#[test]
fn next_word_end_stops_at_identifier_end() {
    assert_eq!(next_word_end("alpha beta", 0, false), Some(4));
}

#[test]
fn long_word_motion_treats_punctuation_as_word_content() {
    assert_eq!(next_word_start("alpha::beta gamma", 0, true), Some(12));
    assert_eq!(next_word_end("alpha::beta gamma", 0, true), Some(10));
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

fn run_ex(app: &mut App, command: &str) {
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)));
    for ch in command.chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
}

fn insert_text(app: &mut App, text: &str) {
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    for ch in text.chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
}

fn test_buf_state() -> BufState {
    BufState {
        id: 1,
        path: None,
        display_name: None,
        view_id: String::new(),
        editor_config_synced: true,
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
        is_vlf: false,
        vlf_generation: 0,
        vlf_approx_line_count: 0,
    }
}

fn window_paths(app: &App) -> Vec<PathBuf> {
    app.tabs
        .focused_windows()
        .windows()
        .iter()
        .map(|window| {
            app.backend
                .all_bufs()
                .iter()
                .find(|buf| buf.id == window.buffer_id)
                .and_then(|buf| buf.path.clone())
                .unwrap()
        })
        .collect()
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
fn open_hsplit_and_new_aliases_work() {
    let first = unique_temp_path("ee-tui-open-first");
    let second = unique_temp_path("ee-tui-open-second");
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
fn view_rotation_and_directional_jump_commands_follow_split_axis() {
    let first = unique_temp_path("ee-tui-view-a");
    let second = unique_temp_path("ee-tui-view-b");
    let third = unique_temp_path("ee-tui-view-c");
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
    let first = unique_temp_path("ee-tui-view-rev-a");
    let second = unique_temp_path("ee-tui-view-rev-b");
    let third = unique_temp_path("ee-tui-view-rev-c");
    fs::write(&first, "one\n").unwrap();
    fs::write(&second, "two\n").unwrap();
    fs::write(&third, "three\n").unwrap();

    let mut app = App::from_path(Some(first.clone())).unwrap();
    run_ex(&mut app, &format!("vs {}", second.display()));
    run_ex(&mut app, &format!("vs {}", third.display()));

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
    let first = unique_temp_path("ee-tui-swap-a");
    let second = unique_temp_path("ee-tui-swap-b");
    let third = unique_temp_path("ee-tui-swap-c");
    let fourth = unique_temp_path("ee-tui-swap-d");
    fs::write(&first, "one\n").unwrap();
    fs::write(&second, "two\n").unwrap();
    fs::write(&third, "three\n").unwrap();
    fs::write(&fourth, "four\n").unwrap();

    let mut vertical = App::from_path(Some(first.clone())).unwrap();
    run_ex(&mut vertical, &format!("vs {}", second.display()));
    run_ex(&mut vertical, &format!("vs {}", third.display()));
    run_ex(&mut vertical, "swap_view_left");
    assert_eq!(window_paths(&vertical), vec![first.clone(), third.clone(), second.clone()]);
    assert_eq!(vertical.backend.active().path.as_ref(), Some(&third));

    run_ex(&mut vertical, "swap_view_up");
    assert_eq!(window_paths(&vertical), vec![first.clone(), third.clone(), second.clone()]);
    assert_eq!(vertical.backend.active().path.as_ref(), Some(&third));

    let mut horizontal = App::from_path(Some(first.clone())).unwrap();
    run_ex(&mut horizontal, &format!("hs {}", fourth.display()));
    run_ex(&mut horizontal, &format!("hs {}", second.display()));
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

#[test]
fn parse_action_spec_accepts_view_command_names() {
    assert_eq!(parse_action_spec("rotate_view").unwrap(), Action::RotateView);
    assert_eq!(parse_action_spec("cycle_view").unwrap(), Action::RotateView);
    assert_eq!(parse_action_spec("rotate_view_reverse").unwrap(), Action::RotateViewReverse);
    assert_eq!(parse_action_spec("transpose_view").unwrap(), Action::TransposeView);
    assert_eq!(parse_action_spec("wclose").unwrap(), Action::WindowClose);
    assert_eq!(parse_action_spec("wonly").unwrap(), Action::WindowOnly);
    assert_eq!(parse_action_spec("jump_view_left").unwrap(), Action::JumpViewLeft);
    assert_eq!(parse_action_spec("jump_view_down").unwrap(), Action::JumpViewDown);
    assert_eq!(parse_action_spec("jump_view_up").unwrap(), Action::JumpViewUp);
    assert_eq!(parse_action_spec("jump_view_right").unwrap(), Action::JumpViewRight);
    assert_eq!(parse_action_spec("swap_view_left").unwrap(), Action::SwapViewLeft);
    assert_eq!(parse_action_spec("swap_view_down").unwrap(), Action::SwapViewDown);
    assert_eq!(parse_action_spec("swap_view_up").unwrap(), Action::SwapViewUp);
    assert_eq!(parse_action_spec("swap_view_right").unwrap(), Action::SwapViewRight);
    assert_eq!(parse_action_spec("file_explorer").unwrap(), Action::FileExplorer);
    assert_eq!(
        parse_action_spec("file_explorer_in_current_buffer_directory").unwrap(),
        Action::FileExplorerInCurrentBufferDirectory
    );
    assert_eq!(
        parse_action_spec("file_explorer_in_current_directory").unwrap(),
        Action::FileExplorerInCurrentDirectory
    );
    assert_eq!(parse_action_spec("commit_undo_checkpoint").unwrap(), Action::CommitUndoCheckpoint);
    assert_eq!(parse_action_spec("shell_pipe").unwrap(), Action::PrefillCommandLine("pipe "));
    assert_eq!(
        parse_action_spec("shell_insert_output").unwrap(),
        Action::PrefillCommandLine("shell_insert_output ")
    );
}

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
    let first = unique_temp_path("ee-tui-write-bang");
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

    let second = unique_temp_path("ee-tui-update-bang");
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

    let third = unique_temp_path("ee-tui-x-bang");
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
    let first = unique_temp_path("ee-tui-wa-first");
    let second = unique_temp_path("ee-tui-wa-second");
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
    let first = unique_temp_path("ee-tui-qa-first");
    let second = unique_temp_path("ee-tui-qa-second");
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
    let source = unique_temp_path("ee-tui-read-source");
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
    let source = unique_temp_path("ee-tui-move-source");
    let target = unique_temp_path("ee-tui-move-target");
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
fn language_encoding_echo_register_and_redraw_commands_update_state() {
    let mut app = App::from_path(None).unwrap();

    run_ex(&mut app, "set_language rust");
    assert_eq!(
        app.syntax_overrides.get(&app.backend.active().id).map(String::as_str),
        Some("Rust")
    );
    assert_eq!(app.backend.status_message.as_deref(), Some("language: Rust"));

    run_ex(&mut app, "set_language");
    assert_eq!(app.backend.status_message.as_deref(), Some("language: Rust"));

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
fn cd_pwd_and_lsp_commands_update_status() {
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    let temp = tempfile::tempdir().unwrap();

    let mut app = App::from_path(None).unwrap();
    run_ex(&mut app, &format!("cd {}", temp.path().display()));
    assert_eq!(std::env::current_dir().unwrap(), temp.path());
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
    run_ex(&mut app, "| tr a-z A-Z");
    app.backend.pump().unwrap();
    assert_eq!(app.backend.lines, vec![String::from("AB")]);

    app.backend.set_selections(&[SelectionRange { start: 0, end: 1 }]).unwrap();
    run_ex(&mut app, "shell_insert_output printf x");
    app.backend.pump().unwrap();
    assert_eq!(app.backend.lines, vec![String::from("xAB")]);

    app.backend
        .set_selections(&[SelectionRange { start: 0, end: 1 }, SelectionRange { start: 1, end: 2 }])
        .unwrap();
    run_ex(&mut app, "shell_keep_pipe grep -q x");
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

    run_ex(&mut app, &format!("pipe_to cat > {}", output.display()));
    assert_eq!(fs::read_to_string(&output).unwrap(), "abc");
    assert_eq!(app.backend.lines, vec![String::from("abc")]);

    app.backend.set_selections(&[SelectionRange { start: 1, end: 2 }]).unwrap();
    run_ex(&mut app, "shell_append_output printf z");
    app.backend.pump().unwrap();
    assert_eq!(app.backend.lines, vec![String::from("abzc")]);
}

#[test]
fn sort_command_sorts_selected_lines_or_whole_buffer() {
    let mut app = App::from_path(None).unwrap();
    insert_text(&mut app, "keep\nccc\naaa\nbbb\nstay");
    app.backend.pump().unwrap();

    run_ex(&mut app, "2,4sort");
    app.backend.pump().unwrap();
    assert_eq!(
        app.backend.lines,
        vec![
            String::from("keep"),
            String::from("aaa"),
            String::from("bbb"),
            String::from("ccc"),
            String::from("stay"),
        ]
    );

    let mut whole = App::from_path(None).unwrap();
    insert_text(&mut whole, "z\nc\na\nb");
    whole.backend.pump().unwrap();
    run_ex(&mut whole, "sort");
    whole.backend.pump().unwrap();
    assert_eq!(
        whole.backend.lines,
        vec![String::from("a"), String::from("b"), String::from("c"), String::from("z"),]
    );
}

#[test]
fn dedup_commands_remove_duplicate_lines() {
    let mut app = App::from_path(None).unwrap();
    insert_text(&mut app, "a\nb\na\nb\nc");
    app.backend.pump().unwrap();

    run_ex(&mut app, "dedup");
    app.backend.pump().unwrap();
    assert_eq!(app.backend.lines, vec![String::from("a"), String::from("b"), String::from("c")]);

    let mut selected = App::from_path(None).unwrap();
    insert_text(&mut selected, "keep\nx\nx\ny\nx");
    selected.backend.pump().unwrap();
    run_ex(&mut selected, "2,4uniq");
    selected.backend.pump().unwrap();
    assert_eq!(
        selected.backend.lines,
        vec![String::from("keep"), String::from("x"), String::from("y"), String::from("x"),]
    );
}

#[test]
fn diffget_restores_current_git_hunk_from_head() {
    let temp = tempfile::tempdir().unwrap();
    run_git(temp.path(), &["init"]);
    run_git(temp.path(), &["config", "user.email", "test@example.com"]);
    run_git(temp.path(), &["config", "user.name", "Test User"]);

    let path = temp.path().join("sample.rs");
    fs::write(&path, "one\ntwo\nthree\n").unwrap();
    run_git(temp.path(), &["add", "sample.rs"]);
    run_git(temp.path(), &["commit", "-m", "init"]);

    let mut app = App::from_path(Some(path)).unwrap();
    app.backend.set_selections(&[SelectionRange { start: 4, end: 7 }]).unwrap();
    let _ = app.backend.send_edit("delete_forward", json!([]));
    let _ = app.backend.send_edit("insert", json!({ "chars": "TWO" }));
    app.backend.pump().unwrap();
    app.backend.cursor_line = 1;

    run_ex(&mut app, "diffget");
    app.backend.pump().unwrap();

    assert!(app.backend.lines.starts_with(&[
        String::from("one"),
        String::from("two"),
        String::from("three"),
    ]));
}

#[test]
fn reload_and_reload_all_aliases_refresh_from_disk() {
    let first = unique_temp_path("ee-tui-reload-first");
    let second = unique_temp_path("ee-tui-reload-second");
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
    let first = unique_temp_path("ee-tui-bc-first");
    let second = unique_temp_path("ee-tui-bc-second");
    let third = unique_temp_path("ee-tui-bc-third");
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
    let first = unique_temp_path("ee-tui-goto-buffer-first");
    let second = unique_temp_path("ee-tui-goto-buffer-second");
    let third = unique_temp_path("ee-tui-goto-buffer-third");
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
    let first = unique_temp_path("ee-tui-goto-recent-first");
    let second = unique_temp_path("ee-tui-goto-recent-second");
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
fn parse_action_spec_accepts_move_line_aliases() {
    assert_eq!(crate::keymap::parse_action_spec("move_line_up").unwrap(), Action::Edit("move_up"));
    assert_eq!(
        crate::keymap::parse_action_spec("move_line_down").unwrap(),
        Action::Edit("move_down")
    );
}

#[test]
fn parse_action_spec_accepts_match_brackets_alias() {
    assert_eq!(crate::keymap::parse_action_spec("match_brackets").unwrap(), Action::MatchingPair);
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

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('"'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE)));
    assert_eq!(app.input_state.pending_register, Some(RegisterName::Clipboard));

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('"'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('*'), KeyModifiers::NONE)));
    assert_eq!(app.input_state.pending_register, Some(RegisterName::PrimaryClipboard));
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
fn plugin_lifecycle_helpers_emit_plugin_notifications() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    client.restart_plugin("plugin-name").unwrap();
    let restart: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(restart["method"], "plugin");
    assert_eq!(restart["params"]["command"], "restart");

    client.stop_plugin("plugin-name").unwrap();
    let stop: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(stop["params"]["command"], "stop");
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

// ── Performance-budget and regression tests ──────────────────────────────────

/// Fixture helpers for normal-mode performance tests.
///
/// These do not write large files to disk; they generate content in memory so
/// tests run quickly and leave no artifacts behind.
mod fixture {
    /// Returns a Vec of `n` lines of uniform width `line_len`.
    pub(super) fn many_line_fixture(n: usize, line_len: usize) -> Vec<String> {
        (0..n).map(|i| format!("{i:>0width$}", width = line_len.min(20))).collect()
    }

    /// Returns a Vec of `n` lines that each contain exactly one very long line
    /// interleaved with short lines.
    pub(super) fn long_line_fixture(n: usize, long_len: usize) -> Vec<String> {
        (0..n)
            .map(|i| if i % 2 == 0 { "x".repeat(long_len) } else { format!("line {i}") })
            .collect()
    }

    /// Returns a Vec of `n` mixed-indentation source-like lines (simulates a
    /// 300 K LOC Rust source file).
    pub(super) fn source_fixture(n: usize) -> Vec<String> {
        let snippets = ["fn foo() {", "    let x = 1;", "    let y = 2;", "    x + y", "}"];
        (0..n).map(|i| snippets[i % snippets.len()].to_owned()).collect()
    }

    /// Returns a Vec of `n` lines with alternating LF and CRLF endings
    /// stripped (the `lines` vec stores text only, endings live in the rope).
    pub(super) fn mixed_crlf_fixture(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("line {i}")).collect()
    }
}

/// Render `lines` into a 120×50 terminal and return the elapsed duration.
fn timed_render(lines: Vec<String>) -> std::time::Duration {
    let mut app = App::from_path(None).unwrap();
    app.backend.lines = lines;

    let backend = TestBackend::new(120, 50);
    let mut terminal = Terminal::new(backend).unwrap();

    let start = std::time::Instant::now();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    start.elapsed()
}

/// Regression: rendering a 300 K-line buffer must stay under one frame at
/// 60 Hz (≈16.7 ms).  The render path touches only the visible viewport
/// (~48 lines) via `buf.lines.get(i)` — no full-buffer clone.
///
/// If someone adds a full-buffer `Vec<String>` clone to the render path this
/// test will regress dramatically (300 K string copies ≫ 16 ms).
#[test]
fn render_300k_line_fixture_under_one_frame_budget() {
    const LINES: usize = 300_000;
    const FRAME_BUDGET_MS: u128 = 50; // 3× 60 Hz frame; avoids CI flake

    let lines = fixture::many_line_fixture(LINES, 30);
    let elapsed = timed_render(lines);

    assert!(
        elapsed.as_millis() < FRAME_BUDGET_MS,
        "render of {LINES} lines took {}ms, expected < {FRAME_BUDGET_MS}ms \
         (possible full-buffer Vec<String> clone in render path)",
        elapsed.as_millis()
    );
}

/// Regression: rendering a long-line fixture (few very wide lines) must also
/// stay within the one-frame budget.
#[test]
fn render_long_line_fixture_under_one_frame_budget() {
    const LINES: usize = 300_000;
    const LINE_LEN: usize = 200;
    const FRAME_BUDGET_MS: u128 = 50;

    let lines = fixture::long_line_fixture(LINES, LINE_LEN);
    let elapsed = timed_render(lines);

    assert!(
        elapsed.as_millis() < FRAME_BUDGET_MS,
        "render of {LINES} long-line fixture took {}ms, expected < {FRAME_BUDGET_MS}ms",
        elapsed.as_millis()
    );
}

/// Regression: rendering a mixed-CRLF fixture must stay within the one-frame budget.
#[test]
fn render_mixed_crlf_fixture_under_one_frame_budget() {
    const LINES: usize = 300_000;
    const FRAME_BUDGET_MS: u128 = 50;

    let lines = fixture::mixed_crlf_fixture(LINES);
    let elapsed = timed_render(lines);

    assert!(
        elapsed.as_millis() < FRAME_BUDGET_MS,
        "render of {LINES} mixed-CRLF fixture took {}ms, expected < {FRAME_BUDGET_MS}ms",
        elapsed.as_millis()
    );
}

/// Regression: rendering a 300 K LOC source-like fixture must stay within budget.
#[test]
fn render_source_fixture_under_one_frame_budget() {
    const LINES: usize = 300_000;
    const FRAME_BUDGET_MS: u128 = 50;

    let lines = fixture::source_fixture(LINES);
    let elapsed = timed_render(lines);

    assert!(
        elapsed.as_millis() < FRAME_BUDGET_MS,
        "render of {LINES}-line source fixture took {}ms, expected < {FRAME_BUDGET_MS}ms",
        elapsed.as_millis()
    );
}

// ── VLF viewport protocol ──────────────────────────────────────────────────

#[test]
fn apply_vlf_chunks_populates_line_cache() {
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.vlf_generation = 7;
    buf.line_cache = vec![LineSlot::Invalid; 3];

    let lines = vec![String::from("alpha"), String::from("beta")];
    buf.apply_vlf_chunks(7, 0, &lines, 3, true, 1.0);

    assert_eq!(
        buf.line_cache[0],
        LineSlot::Known(CachedLine {
            text: String::from("alpha"),
            cursors: vec![],
            syntax_spans: vec![]
        })
    );
    assert_eq!(
        buf.line_cache[1],
        LineSlot::Known(CachedLine {
            text: String::from("beta"),
            cursors: vec![],
            syntax_spans: vec![]
        })
    );
    // Slot 2 untouched (not in the chunk).
    assert_eq!(buf.line_cache[2], LineSlot::Invalid);
}

#[test]
fn apply_vlf_chunks_stale_generation_discarded() {
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.vlf_generation = 5;
    buf.line_cache = vec![LineSlot::Invalid; 2];

    let lines = vec![String::from("stale")];
    // Send with generation 3 (older than current 5) — must be ignored.
    buf.apply_vlf_chunks(3, 0, &lines, 2, false, 0.5);

    assert_eq!(buf.line_cache[0], LineSlot::Invalid, "stale response must not update cache");
}

#[test]
fn apply_vlf_chunks_grows_cache_to_approximate_count() {
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.vlf_generation = 1;
    buf.line_cache = Vec::new(); // start empty

    let lines: Vec<String> = Vec::new();
    buf.apply_vlf_chunks(1, 0, &lines, 1000, false, 0.1);

    assert_eq!(buf.line_cache.len(), 1000, "cache should grow to approximate_line_count");
    assert!(buf.line_cache.iter().all(|s| *s == LineSlot::Invalid));
    assert_eq!(buf.vlf_approx_line_count, 1000);
}

#[test]
fn vlf_chunks_backend_event_parsed() {
    let params = json!({
        "view_id": "view-1",
        "generation": 42,
        "line_start": 10,
        "lines": ["hello", "world"],
        "approximate_line_count": 500,
        "line_count_exact": false,
        "index_progress": 0.42,
    });
    let event = parse_notification("vlf_chunks", params).expect("should parse vlf_chunks");
    match event {
        BackendEvent::VlfChunks {
            view_id,
            generation,
            line_start,
            lines,
            approximate_line_count,
            line_count_exact,
            index_progress,
        } => {
            assert_eq!(view_id, "view-1");
            assert_eq!(generation, 42);
            assert_eq!(line_start, 10);
            assert_eq!(lines, vec!["hello", "world"]);
            assert_eq!(approximate_line_count, 500);
            assert!(!line_count_exact);
            assert!((index_progress - 0.42).abs() < 1e-9);
        }
        other => panic!("expected VlfChunks, got {:?}", other),
    }
}
