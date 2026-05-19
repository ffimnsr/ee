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

use clap::Parser;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use serde_json::{Value, json};
use xi_core_lib::open_policy::OpenThresholds;
use xi_core_lib::plugin_rpc::{
    CodeActionDescriptor, Diagnostic, DiagnosticSeverity, Range, SelectionRange, SymbolItem,
};
use xi_core_lib::rpc::LineReplacement;
use xi_core_lib::runtime_loader::{
    RuntimeGrammarHealth, RuntimeHealthReport, RuntimeLanguageDetectionSource, RuntimeQueryHealth,
    RuntimeQueryHealthReport, RuntimeQueryKind, RuntimeRoots,
};

use crate::app::{App, Mode, Operator, PendingCharFind};
use crate::backend::{
    BackendEvent, CachedLine, CompletionSuggestion, CoreAnnotation, CoreLine, CoreSyntaxSpan,
    CoreUpdate, CoreUpdateKind, CoreUpdateOp, LineSlot, NavigationTarget, coalesce_backend_events,
    format_location_message, invalid_line_ranges, invalid_line_ranges_bounded, parse_notification,
    startup_render_ready,
};
use crate::buffer::{BufState, BufferManager, VlfChunkUpdate};
use crate::git::{GitBufferCache, GitBufferStatus, GitHunk, GitSign};
use crate::keymap::{Action, BindingKey, bindings, parse_action_spec};
use crate::picker::PickerKind;
use crate::picker::PickerState;
use crate::quickfix::{QfEntry, QfList};
use crate::registers::{ClipboardSelection, RegisterName, set_test_clipboard};
use crate::text::{
    byte_col_to_display_col, display_col_to_byte, find_char_backward, find_char_forward,
    next_char_start, next_word_end, next_word_start, prev_char_start, prev_word_start,
};
use crate::ui::ui;

fn cwd_test_lock() -> &'static crate::config::TestCwdLock {
    crate::config::test_cwd_lock()
}

fn perf_test_lock() -> &'static Mutex<()> {
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
fn input_loop_coalesces_stale_repeated_arrow_motion() {
    let up_repeat =
        Event::Key(KeyEvent::new_with_kind(KeyCode::Up, KeyModifiers::NONE, KeyEventKind::Repeat));
    let down_press = Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));

    let coalesced =
        crate::coalesce_input_events(vec![up_repeat.clone(), up_repeat, down_press.clone()]);

    assert_eq!(coalesced, vec![down_press]);
}

#[test]
fn input_loop_preserves_non_repeated_input_batch() {
    let first = Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    let second = Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));

    let coalesced = crate::coalesce_input_events(vec![first.clone(), second.clone()]);

    assert_eq!(coalesced, vec![first, second]);
}

#[test]
fn input_loop_does_not_coalesce_typed_repeat_chars() {
    let repeat = Event::Key(KeyEvent::new_with_kind(
        KeyCode::Char('j'),
        KeyModifiers::NONE,
        KeyEventKind::Repeat,
    ));

    let coalesced = crate::coalesce_input_events(vec![repeat.clone(), repeat.clone()]);

    assert_eq!(coalesced, vec![repeat.clone(), repeat]);
}

#[test]
fn cli_utility_commands_live_under_do() {
    let cli = crate::Cli::try_parse_from(["ee", "do", "doctor"]).unwrap();

    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do { command: crate::DoCommands::Doctor })
    ));

    let cli = crate::Cli::try_parse_from([
        "ee",
        "do",
        "runtime",
        "--file",
        "sample.rs",
        "--language",
        "Rust",
    ])
    .unwrap();

    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do { command: crate::DoCommands::Runtime { .. } })
    ));

    let cli = crate::Cli::try_parse_from([
        "ee",
        "do",
        "runtime-fetch",
        "--all",
        "--source-root",
        "target/runtime-sources",
    ])
    .unwrap();

    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do { command: crate::DoCommands::RuntimeFetch { all: true, .. } })
    ));

    let cli = crate::Cli::try_parse_from([
        "ee",
        "do",
        "runtime-build",
        "--language",
        "Rust",
        "--output-root",
        "target/runtime",
        "--skip-load",
    ])
    .unwrap();

    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do {
            command: crate::DoCommands::RuntimeBuild { skip_load: true, .. }
        })
    ));

    let cli =
        crate::Cli::try_parse_from(["ee", "do", "validate", "--config", "custom.ee.toml"]).unwrap();

    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do { command: crate::DoCommands::Validate { .. } })
    ));

    let cli = crate::Cli::try_parse_from(["ee", "do", "completions", "bash"]).unwrap();

    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do { command: crate::DoCommands::Completions { .. } })
    ));

    let cli = crate::Cli::try_parse_from(["ee", "do", "file", "line-check", "sample.txt"]).unwrap();

    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do {
            command: crate::DoCommands::File { command: crate::FileCommands::LineCheck { .. } }
        })
    ));

    let cli =
        crate::Cli::try_parse_from(["ee", "do", "file", "head", "-n", "3", "sample.txt"]).unwrap();

    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do {
            command: crate::DoCommands::File {
                command: crate::FileCommands::Head { lines: 3, .. }
            }
        })
    ));

    let cli = crate::Cli::try_parse_from(["ee", "do", "file", "tail", "sample.txt"]).unwrap();

    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do {
            command: crate::DoCommands::File {
                command: crate::FileCommands::Tail { lines: 10, .. }
            }
        })
    ));
}

#[test]
fn cli_allows_utility_names_as_file_paths() {
    let cli = crate::Cli::try_parse_from(["ee", "doctor", "validate", "completions"]).unwrap();

    assert!(cli.command.is_none());
    assert_eq!(cli.files, ["doctor", "validate", "completions"].map(PathBuf::from));
}

#[test]
fn file_line_check_reuses_streaming_vlf_counter() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("sample.txt");
    fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();

    let count = crate::count_file_line_feeds(&path).unwrap();

    assert_eq!(count, 3);
}

#[test]
fn file_line_check_matches_wc_lf_semantics_without_trailing_newline() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("sample.txt");
    fs::write(&path, "alpha\nbeta\ngamma").unwrap();

    let count = crate::count_file_line_feeds(&path).unwrap();

    assert_eq!(count, 2);
}

#[test]
fn file_head_reads_first_requested_lines() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("sample.txt");
    fs::write(&path, "alpha\nbeta\ngamma\ndelta\n").unwrap();

    let head = crate::read_file_head(&path, 2).unwrap();

    assert_eq!(head, "alpha\nbeta\n");
}

#[test]
fn file_head_keeps_partial_last_line_without_trailing_newline() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("sample.txt");
    fs::write(&path, "alpha\nbeta\ngamma").unwrap();

    let head = crate::read_file_head(&path, 3).unwrap();

    assert_eq!(head, "alpha\nbeta\ngamma");
}

#[test]
fn runtime_report_renders_resolution_and_query_health() {
    let report = RuntimeHealthReport {
        requested_language: Some(String::from("Rust")),
        file_path: Some(PathBuf::from("sample.rs")),
        detection_source: Some(RuntimeLanguageDetectionSource::Explicit),
        language_id: Some(String::from("Rust")),
        display_name: Some(String::from("Rust")),
        asset_source: None,
        effective_runtime_root: Some(PathBuf::from("/runtime")),
        grammar_path: Some(PathBuf::from("/runtime/grammars/libtree-sitter-rust.so")),
        grammar_status: RuntimeGrammarHealth::Loaded,
        query_reports: vec![
            RuntimeQueryHealthReport {
                kind: RuntimeQueryKind::Highlights,
                status: RuntimeQueryHealth::Loaded,
                source_paths: vec![PathBuf::from("/runtime/queries/Rust/highlights.scm")],
            },
            RuntimeQueryHealthReport {
                kind: RuntimeQueryKind::Indents,
                status: RuntimeQueryHealth::Missing,
                source_paths: Vec::new(),
            },
        ],
        runtime_roots: RuntimeRoots::new(
            "/bundle",
            "/user/ee",
            Some(PathBuf::from("/workspace/.ee")),
        ),
    };

    let rendered = crate::render_runtime_report(&report);
    assert!(rendered.contains("resolved language: Rust [Rust] via explicit"));
    assert!(rendered.contains("grammar: loaded"));
    assert!(rendered.contains("highlights  loaded"));
    assert!(rendered.contains("indents     missing"));
    assert!(rendered.contains("effective runtime root: /runtime"));
}

#[test]
fn runtime_report_exit_code_classifies_runtime_failures() {
    let mut healthy = RuntimeHealthReport {
        requested_language: Some(String::from("Rust")),
        file_path: None,
        detection_source: Some(RuntimeLanguageDetectionSource::Explicit),
        language_id: Some(String::from("Rust")),
        display_name: Some(String::from("Rust")),
        asset_source: None,
        effective_runtime_root: None,
        grammar_path: None,
        grammar_status: RuntimeGrammarHealth::Loaded,
        query_reports: Vec::new(),
        runtime_roots: RuntimeRoots::new("/bundle", "/user/ee", None),
    };
    assert_eq!(crate::runtime_report_exit_code(&healthy), 0);

    healthy.language_id = None;
    assert_eq!(crate::runtime_report_exit_code(&healthy), crate::EXIT_RUNTIME_CONFIG_MERGE);

    healthy.language_id = Some(String::from("Rust"));
    healthy.grammar_status = RuntimeGrammarHealth::Missing;
    assert_eq!(crate::runtime_report_exit_code(&healthy), crate::EXIT_RUNTIME_ASSET);

    healthy.grammar_status = RuntimeGrammarHealth::Loaded;
    healthy.query_reports = vec![RuntimeQueryHealthReport {
        kind: RuntimeQueryKind::Highlights,
        status: RuntimeQueryHealth::Missing,
        source_paths: Vec::new(),
    }];
    assert_eq!(crate::runtime_report_exit_code(&healthy), crate::EXIT_RUNTIME_ASSET);
}

#[test]
fn file_tail_reads_last_requested_lines() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("sample.txt");
    fs::write(&path, "alpha\nbeta\ngamma\ndelta\n").unwrap();

    let tail = crate::read_file_tail(&path, 2).unwrap();

    assert_eq!(tail, "gamma\ndelta\n");
}

#[test]
fn file_tail_keeps_partial_last_line_without_trailing_newline() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("sample.txt");
    fs::write(&path, "alpha\nbeta\ngamma").unwrap();

    let tail = crate::read_file_tail(&path, 1).unwrap();

    assert_eq!(tail, "gamma");
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
    assert_eq!(backend_fg, ratatui::style::Color::Rgb(211, 120, 70));
    assert_eq!(plain_fg, ratatui::style::Color::Rgb(213, 216, 224));
    assert_eq!(vlf_fg, plain_fg);
    assert_eq!(vlf_fg, ratatui::style::Color::Rgb(213, 216, 224));
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
fn update_merge_normalizes_line_text() {
    let slot = LineSlot::Known(CachedLine {
        text: String::from("alpha"),
        cursors: Vec::new(),
        syntax_spans: Vec::new(),
    });

    let merged = slot
        .merge(CoreLine {
            text: Some(String::from("beta\n")),
            cursor: Vec::new(),
            syntax_spans: None,
        })
        .expect("update merge should succeed");

    let LineSlot::Known(line) = merged else { panic!("expected known line") };
    assert_eq!(line.text, "beta");
}

#[test]
fn pristine_external_reload_update_clears_changed_flag_for_trailing_blank_line_removal() {
    let path = unique_temp_path("ee-cli-external-reload-state");
    fs::write(&path, "alpha\n\n").unwrap();

    let (tx, _rx) = mpsc::channel();
    let (backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    let buf_id = client.active().id;
    client.set_buffer_path(buf_id, path.clone()).unwrap();
    client.line_cache = vec![
        LineSlot::Known(CachedLine {
            text: String::from("alpha"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
        LineSlot::Known(CachedLine {
            text: String::new(),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
        LineSlot::Known(CachedLine {
            text: String::new(),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
    ];
    client.rebuild_lines();

    let previous_mtime = client.mtime;
    thread::sleep(Duration::from_millis(25));
    fs::write(&path, "alpha\n").unwrap();
    client.check_external_changes();
    assert!(client.externally_modified);

    backend_tx
        .send(BackendEvent::Update {
            view_id: String::from("view-id-1"),
            update: CoreUpdate {
                pristine: true,
                annotations: Vec::new(),
                ops: vec![CoreUpdateOp { op: CoreUpdateKind::Copy, n: 2, lines: Vec::new() }],
            },
        })
        .unwrap();

    client.drain_events().unwrap();

    assert!(!client.externally_modified);
    assert_eq!(client.status_message.as_deref(), Some("reloaded"));
    assert_eq!(client.lines, vec![String::from("alpha"), String::new()]);
    assert_ne!(client.mtime, previous_mtime);

    let _ = fs::remove_file(&path);
}

#[test]
fn stale_view_updates_are_ignored() {
    let (tx, _rx) = mpsc::channel();
    let (backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("live-view"));
    client.line_cache = vec![LineSlot::Known(CachedLine {
        text: String::from("alpha"),
        cursors: Vec::new(),
        syntax_spans: Vec::new(),
    })];
    client.rebuild_lines();

    backend_tx
        .send(BackendEvent::Update {
            view_id: String::from("stale-view"),
            update: CoreUpdate {
                pristine: true,
                annotations: Vec::new(),
                ops: vec![CoreUpdateOp { op: CoreUpdateKind::Skip, n: 2, lines: Vec::new() }],
            },
        })
        .unwrap();

    client.drain_events().unwrap();

    assert_eq!(client.lines, vec![String::from("alpha")]);
    assert_eq!(client.line_cache.len(), 1);
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
fn normal_invalid_line_requests_are_viewport_bounded() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    client.line_cache = vec![LineSlot::Invalid; 10_000];

    client.notify_scroll(1_000, 1_020).unwrap();
    let scroll: Value =
        serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).unwrap()).unwrap();
    assert_eq!(scroll["params"]["method"], "scroll");

    client.sync_pending_events().unwrap();

    let request: Value =
        serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).unwrap()).unwrap();
    assert_eq!(request["params"]["method"], "request_lines");
    assert_eq!(request["params"]["params"], json!([936, 1084]));
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
fn source_control_skips_vlf_buffers_and_clears_stale_cache() {
    let mut app = App::from_path(None).unwrap();
    let buf_id = app.backend.active().id;
    app.backend.is_vlf = true;
    app.backend.line_cache = vec![LineSlot::Known(CachedLine {
        text: String::from("visible"),
        cursors: Vec::new(),
        syntax_spans: Vec::new(),
    })];
    app.source_control.insert(
        buf_id,
        GitBufferCache {
            fingerprint: 123,
            path: Some(PathBuf::from("/tmp/stale.rs")),
            last_refresh: Instant::now(),
            status: Some(GitBufferStatus {
                repo_root: PathBuf::from("/tmp/repo"),
                repo_name: String::from("repo"),
                repo_relative: String::from("src/lib.rs"),
                branch: String::from("main"),
                tracked: true,
                dirty: true,
                hunks: Vec::new(),
                line_signs: HashMap::from([(0, GitSign::Modified)]),
            }),
        },
    );

    app.refresh_source_control();

    assert!(app.source_control.is_empty());
}

#[test]
fn input_idle_gate_blocks_auto_source_control_during_key_bursts() {
    let mut app = App::from_path(None).unwrap();

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
    assert!(!app.input_idle_for(Duration::from_millis(250)));

    app.last_input_at = Instant::now() - Duration::from_millis(300);
    assert!(app.input_idle_for(Duration::from_millis(250)));
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
    let path = unique_temp_path("ee-cli-open");
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
fn startup_render_ready_after_first_visible_line() {
    assert!(!startup_render_ready(&[]));
    assert!(!startup_render_ready(&[LineSlot::Invalid]));
    assert!(startup_render_ready(&[LineSlot::Known(CachedLine {
        text: String::from("line-0"),
        cursors: Vec::new(),
        syntax_spans: Vec::new(),
    })]));
    assert!(!startup_render_ready(&[
        LineSlot::Invalid,
        LineSlot::Known(CachedLine {
            text: String::from("line-1"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
    ]));
}

#[test]
fn open_many_line_20mb_fixture_meets_first_render_budget() {
    assert_open_to_first_render_budget("many-line", budget_many_line);
}

#[test]
fn open_long_line_20mb_fixture_meets_first_render_budget() {
    assert_open_to_first_render_budget("long-line", budget_long_line);
}

#[test]
#[ignore = "manual perf breakdown probe"]
fn open_many_line_20mb_fixture_reports_startup_breakdown() {
    report_open_to_first_render_breakdown("many-line", budget_many_line);
}

#[test]
#[ignore = "manual perf breakdown probe"]
fn open_long_line_20mb_fixture_reports_startup_breakdown() {
    report_open_to_first_render_breakdown("long-line", budget_long_line);
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
    let path = unique_temp_path("ee-cli-save");
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
fn completion_command_alias_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    run_ex(&mut app, "completion");

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
fn resolve_startup_launch_for_dot_opens_picker_in_current_directory() {
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("sample.rs"), "fn main() {}\n").unwrap();
    env::set_current_dir(temp.path()).unwrap();

    let launch = super::resolve_startup_launch(&[PathBuf::from(".")], None).unwrap();
    let (app, additional) = super::build_startup_app(launch).unwrap();

    assert!(additional.is_empty());
    assert!(app.backend.active().path.is_none());
    assert_eq!(env::current_dir().unwrap(), temp.path().canonicalize().unwrap());
    let picker = app.picker.as_ref().expect("directory launch should open picker");
    assert_eq!(picker.kind, PickerKind::Files);
    assert_eq!(picker.title, "Files");
    assert!(
        picker
            .visible_items_range(0, picker.visible_count())
            .iter()
            .any(|item| item == "sample.rs")
    );
}

#[test]
fn resolve_startup_launch_for_directory_path_opens_picker_from_that_directory() {
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    let temp = tempfile::tempdir().unwrap();
    let nested = temp.path().join("nested");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("inside.rs"), "fn inside() {}\n").unwrap();
    env::set_current_dir(temp.path()).unwrap();

    let launch = super::resolve_startup_launch(std::slice::from_ref(&nested), None).unwrap();
    let (app, additional) = super::build_startup_app(launch).unwrap();

    assert!(additional.is_empty());
    assert!(app.backend.active().path.is_none());
    assert_eq!(env::current_dir().unwrap(), nested.canonicalize().unwrap());
    let picker = app.picker.as_ref().expect("directory launch should open picker");
    assert_eq!(picker.kind, PickerKind::Files);
    assert!(
        picker
            .visible_items_range(0, picker.visible_count())
            .iter()
            .any(|item| item == "inside.rs")
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
fn capitalized_command_aliases_are_rejected() {
    for (command, expected_unknown) in
        [("Files", "Files"), ("Grep main", "Grep"), ("Buffers", "Buffers")]
    {
        let mut app = App::from_path(None).unwrap();
        run_ex(&mut app, command);
        let expected = format!("unknown command: {expected_unknown}");

        assert!(app.picker.is_none(), "{command} should not open picker");
        assert_eq!(app.backend.status_message.as_deref(), Some(expected.as_str()));
    }
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
fn ee_cli_sources_do_not_use_raw_lsp_or_plugin_routes() {
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
fn align_it_command_uses_backend_edit_with_pattern_params() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    run_ex(&mut app, "align_it =");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["method"], "align_it");
    assert_eq!(value["params"]["params"]["pattern"], "=");
    assert_eq!(value["params"]["params"]["regex"], false);
    assert_eq!(value["params"]["params"]["occurrence"], 1);
    assert_eq!(value["params"]["params"]["all"], false);
    assert_eq!(value["params"]["params"]["format"], "");
}

#[test]
fn align_it_command_supports_nth_and_format_params() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    run_ex(&mut app, "align_it 2= l0r0l0");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["params"]["pattern"], "=");
    assert_eq!(value["params"]["params"]["occurrence"], 2);
    assert_eq!(value["params"]["params"]["all"], false);
    assert_eq!(value["params"]["params"]["format"], "l0r0l0");
}

#[test]
fn align_it_command_supports_all_matches_selector() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    run_ex(&mut app, "align_it *= r1c1l0");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["params"]["pattern"], "=");
    assert_eq!(value["params"]["params"]["all"], true);
    assert_eq!(value["params"]["params"]["format"], "r1c1l0");
}

#[test]
fn align_it_command_rejects_invalid_regex() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    run_ex(&mut app, "align_it /[/");

    let status = app.backend.status_message.as_deref().expect("status message should be set");
    assert!(status.contains("align_it: invalid regex"));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn align_it_command_rejects_invalid_format() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    run_ex(&mut app, "align_it = x1");

    let status = app.backend.status_message.as_deref().expect("status message should be set");
    assert!(status.contains("align_it: invalid format"));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn reverse_selection_contents_command_uses_backend_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    run_ex(&mut app, "reverse_selection_contents");

    let message = rx.recv().expect("message should be sent");
    let value: Value = serde_json::from_str(&message).expect("message should be json");
    assert_eq!(value["params"]["method"], "reverse_selection_contents");
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
fn fold_close_uses_backend_authoritative_tree_sitter_range() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("fold-close.rs");
    fs::write(&path, "").unwrap();
    let mut app = App::from_path(Some(path)).unwrap();

    insert_text(&mut app, "fn outer() {\n    if true {\n        work();\n    }\n}\n");
    app.backend.pump().unwrap();

    app.backend.cursor_line = 0;

    app.fold_close();

    let buf_id = app.backend.active().id;
    assert_eq!(app.folds.fold_at(buf_id, 0), Some((0, 4)));
    assert!(app.folds.is_hidden(buf_id, 1));
}

#[test]
fn fold_close_all_uses_backend_authoritative_ranges() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("fold-close-all.rs");
    fs::write(&path, "").unwrap();
    let mut app = App::from_path(Some(path)).unwrap();

    insert_text(&mut app, "fn outer() {\n    work();\n}\n\nfn second() {\n    more();\n}\n");
    app.backend.pump().unwrap();

    app.fold_close_all();

    let buf_id = app.backend.active().id;
    assert_eq!(app.folds.fold_at(buf_id, 0), Some((0, 2)));
    assert_eq!(app.folds.fold_at(buf_id, 4), Some((4, 6)));
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
    assert_eq!(buf.cell((spacer_x, 0)).unwrap().bg, ratatui::style::Color::Rgb(22, 24, 31));
    assert_eq!(buf.cell((text_x, 0)).unwrap().symbol(), "a");
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
fn byte_col_to_display_col_expands_tabs() {
    assert_eq!(byte_col_to_display_col("\tabc", 4), 7);
    assert_eq!(byte_col_to_display_col("ab\tcd", 5), 6);
}

#[test]
fn display_col_to_byte_respects_tab_stops() {
    assert_eq!(display_col_to_byte("\tabc", 4), 1);
    assert_eq!(display_col_to_byte("ab\tcd", 4), 3);
    assert_eq!(display_col_to_byte("ab\tcd", 6), 5);
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
fn bindings_table_maps_caret_to_first_non_whitespace() {
    let lookup = bindings()
        .get(&BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('^'),
            modifiers: KeyModifiers::NONE,
            prefix: None,
        })
        .cloned();

    assert_eq!(lookup, Some(Action::GotoFirstNonWhitespace));
}

#[test]
fn overlay_binding_tables_have_defaults() {
    let b = bindings();

    let picker_close = b
        .get(&BindingKey {
            mode: Mode::Picker,
            key: KeyCode::Esc,
            modifiers: KeyModifiers::NONE,
            prefix: None,
        })
        .cloned();
    let quickfix_down = b
        .get(&BindingKey {
            mode: Mode::Quickfix,
            key: KeyCode::Char('j'),
            modifiers: KeyModifiers::NONE,
            prefix: None,
        })
        .cloned();
    let location_close = b
        .get(&BindingKey {
            mode: Mode::LocationList,
            key: KeyCode::Char('q'),
            modifiers: KeyModifiers::NONE,
            prefix: None,
        })
        .cloned();
    let substitute_apply = b
        .get(&BindingKey {
            mode: Mode::SubstituteConfirm,
            key: KeyCode::Char('y'),
            modifiers: KeyModifiers::NONE,
            prefix: None,
        })
        .cloned();

    assert_eq!(picker_close, Some(Action::PickerClose));
    assert_eq!(quickfix_down, Some(Action::QuickfixMoveDown));
    assert_eq!(location_close, Some(Action::LocationListClose));
    assert_eq!(substitute_apply, Some(Action::SubstituteConfirmApply));
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
fn insert_ctrl_bindings_cover_register_and_completion() {
    let b = bindings();
    let insert_register = b
        .get(&BindingKey {
            mode: Mode::Insert,
            key: KeyCode::Char('r'),
            modifiers: KeyModifiers::CONTROL,
            prefix: None,
        })
        .cloned();
    let completion = b
        .get(&BindingKey {
            mode: Mode::Insert,
            key: KeyCode::Char('x'),
            modifiers: KeyModifiers::CONTROL,
            prefix: None,
        })
        .cloned();

    assert_eq!(insert_register, Some(Action::InsertRegister));
    assert_eq!(completion, Some(Action::RequestCompletion));
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
    assert_eq!(crate::keymap::parse_action_spec("global_search").unwrap(), Action::GlobalSearch);
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
    assert_eq!(crate::keymap::parse_action_spec("picker_close").unwrap(), Action::PickerClose);
    assert_eq!(crate::keymap::parse_action_spec("picker_confirm").unwrap(), Action::PickerConfirm);
    assert_eq!(
        crate::keymap::parse_action_spec("quickfix_move_down").unwrap(),
        Action::QuickfixMoveDown
    );
    assert_eq!(
        crate::keymap::parse_action_spec("location_list_confirm").unwrap(),
        Action::LocationListConfirm
    );
    assert_eq!(
        crate::keymap::parse_action_spec("substitute_confirm_apply").unwrap(),
        Action::SubstituteConfirmApply
    );
    assert_eq!(
        crate::keymap::parse_action_spec("substitute_confirm_cancel").unwrap(),
        Action::SubstituteConfirmCancel
    );
    assert_eq!(
        crate::keymap::parse_action_spec("command_palette").unwrap(),
        Action::CommandPalette
    );
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
    assert_eq!(crate::keymap::parse_action_spec("completion").unwrap(), Action::RequestCompletion);
    assert_eq!(
        crate::keymap::parse_action_spec("insert_register").unwrap(),
        Action::InsertRegister
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
    init_test_git_repo(temp.path());

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
    let output = Command::new("git").args(args).current_dir(cwd).output().unwrap();
    assert!(
        output.status.success(),
        "git command failed: {args:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_test_git_repo(cwd: &std::path::Path) {
    fs::create_dir_all(cwd.join(".git-hooks-disabled")).unwrap();
    run_git(cwd, &["init"]);
    run_git(cwd, &["config", "user.email", "test@example.com"]);
    run_git(cwd, &["config", "user.name", "Test User"]);
    run_git(cwd, &["config", "commit.gpgsign", "false"]);
    run_git(cwd, &["config", "core.hooksPath", ".git-hooks-disabled"]);
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
    let target = unique_temp_path("ee-cli-goto-file-target");
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
    assert_eq!(app.active_key_hint_label().as_deref(), Some("replace"));
    let entries = app.active_key_hint_entries().expect("replace wait should show hints");
    assert!(
        entries
            .iter()
            .any(|entry| { entry.key == "char" && entry.description == "replacement character" })
    );
    assert!(entries.iter().any(|entry| entry.key == "Esc" && entry.description == "cancel"));
}

#[test]
fn esc_cancels_replace_wait_state() {
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

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));

    assert!(!app.input_state.awaiting_replace_char);
    assert!(app.active_key_hint_label().is_none());
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
    assert!(entries.iter().any(|entry| entry.key == "a-z / A-Z" && entry.description == "named register / append"));
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
    // Wait until xi-core has fully processed all insert keystrokes.
    // `line_count() >= 3` is not enough: it is satisfied the moment the second
    // Enter is processed (line 2 = "") before the final "alpha" chars arrive.
    // At that point swift motion finds only 1 "al" match and auto-jumps.
    // Waiting until line 2 also contains "al" guarantees both matches exist.
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
    app.backend.pump().unwrap();

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
    app.swift_motion = Some(crate::app::SwiftMotionState {
        query: String::from("al"),
        label_prefix: None,
        targets: vec![
            crate::app::SwiftMotionTarget {
                line: 0,
                display_col: 0,
                end_display_col: 2,
                label: 'a',
                next_label: None,
            },
            crate::app::SwiftMotionTarget {
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
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    app.backend.pump().unwrap();

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
    app.backend.pump().unwrap();

    assert!(app.swift_motion.is_none());
    assert_eq!(app.backend.cursor_line, 26);
    assert_eq!(app.backend.cursor_col, 0);
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
        .find(|(_, line)| line.contains("keys") && line.contains("SPC") && line.contains("f"))
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

fn write_exact_size_ascii_fixture(
    path: &std::path::Path,
    target_bytes: usize,
    line_builder: fn(usize) -> String,
) -> usize {
    let mut bytes = Vec::with_capacity(target_bytes);
    let mut index = 0usize;

    while bytes.len() < target_bytes {
        let remaining = target_bytes - bytes.len();
        if remaining == 1 {
            bytes.push(b'x');
            break;
        }

        let mut line = line_builder(index).into_bytes();
        let max_line_len = remaining.saturating_sub(1);
        if line.len() > max_line_len {
            line.truncate(max_line_len);
        }
        if line.is_empty() {
            line.push(b'x');
        }

        bytes.extend_from_slice(&line);
        if bytes.len() < target_bytes {
            bytes.push(b'\n');
        }
        index += 1;
    }

    let line_count = bytes.split(|&byte| byte == b'\n').count();
    fs::write(path, bytes).unwrap();
    line_count
}

fn timed_open_to_first_render(path: &std::path::Path) -> (App, Duration) {
    let start = Instant::now();
    let app = App::from_path(Some(path.to_path_buf())).unwrap();

    let backend = TestBackend::new(120, 50);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();

    (app, start.elapsed())
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct OpenToFirstRenderBreakdown {
    open: Duration,
    draw: Duration,
    total: Duration,
    startup: crate::buffer::StartupProfile,
}

fn timed_open_to_first_render_breakdown(
    path: &std::path::Path,
) -> (App, OpenToFirstRenderBreakdown) {
    let open_started = Instant::now();
    let app = App::from_path(Some(path.to_path_buf())).unwrap();
    let open = open_started.elapsed();
    let startup = app.backend.startup_profile().clone();

    let backend = TestBackend::new(120, 50);
    let mut terminal = Terminal::new(backend).unwrap();
    let draw_started = Instant::now();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let draw = draw_started.elapsed();

    (app, OpenToFirstRenderBreakdown { startup, open, draw, total: open + draw })
}

fn budget_many_line(i: usize) -> String {
    let thresholds = OpenThresholds::default();
    let target_line_bytes =
        (thresholds.normal_bytes as usize / (thresholds.normal_lines as usize - 2_000)).max(256);
    let prefix = format!("fn item_{i:06}() {{ let value = {}; }} // ", i % 10);
    let suffix_width = target_line_bytes.saturating_sub(prefix.len());
    format!("{prefix}{:0>suffix_width$}", i % 100_000)
}

fn budget_long_line(i: usize) -> String {
    if i.is_multiple_of(2) {
        format!("const LINE_{i}: &str = \"{}\";", "x".repeat(512))
    } else {
        format!("let line_{i} = {i};")
    }
}

fn assert_open_to_first_render_budget(label: &str, line_builder: fn(usize) -> String) {
    let _guard = perf_test_lock().lock().unwrap_or_else(|err| err.into_inner());

    const WARM_BUDGET_MS: u128 = 250;
    const WARM_NOISE_CEILING_MS: u128 = 350;
    const COLD_BUDGET_MS: u128 = 750;
    const WARM_SAMPLE_COUNT: usize = 5;

    let thresholds = OpenThresholds::default();
    let target_bytes = thresholds.normal_bytes as usize - 4096;
    let path = unique_temp_path(&format!("ee-cli-open-budget-{label}"));
    let line_count = write_exact_size_ascii_fixture(&path, target_bytes, line_builder);

    assert!(
        line_count < thresholds.normal_lines as usize,
        "fixture {label} produced {line_count} lines, expected < {}",
        thresholds.normal_lines
    );

    // Warm one-time editor/runtime initialization outside the measured passes.
    drop(App::from_path(None).unwrap());

    let (cold_app, cold_elapsed) = timed_open_to_first_render(&path);
    let mut best_warm = None;
    let mut warm_samples = Vec::with_capacity(WARM_SAMPLE_COUNT);
    for _ in 0..WARM_SAMPLE_COUNT {
        let candidate = timed_open_to_first_render(&path);
        warm_samples.push(candidate.1.as_millis());
        if best_warm.as_ref().is_none_or(|(_, elapsed)| candidate.1 < *elapsed) {
            best_warm = Some(candidate);
        }
    }
    let (warm_app, warm_elapsed) = best_warm.expect("warm pass should run");

    fs::remove_file(&path).unwrap();

    assert!(!cold_app.backend.is_vlf, "fixture {label} unexpectedly opened in VLF mode");
    assert_eq!(
        cold_app.backend.lines.len(),
        line_count,
        "fixture {label} did not stay in normal-mode line cache path"
    );
    assert!(
        !warm_app.backend.is_vlf,
        "fixture {label} unexpectedly opened in VLF mode on warm pass"
    );
    assert_eq!(
        warm_app.backend.lines.len(),
        line_count,
        "fixture {label} warm pass did not stay in normal-mode line cache path"
    );

    assert!(
        cold_elapsed.as_millis() < COLD_BUDGET_MS,
        "cold open-to-first-render for {label} fixture took {}ms, expected < {COLD_BUDGET_MS}ms",
        cold_elapsed.as_millis()
    );
    if warm_elapsed.as_millis() >= WARM_BUDGET_MS {
        eprintln!(
            "warm open-to-first-render for {label} fixture missed target: best={}ms, target<{WARM_BUDGET_MS}ms, samples={warm_samples:?}",
            warm_elapsed.as_millis()
        );
    }
    let strict_budget = env::var_os("EE_STRICT_PERF_BUDGET").is_some();
    let warm_limit_ms = if strict_budget { WARM_BUDGET_MS } else { WARM_NOISE_CEILING_MS };
    let warm_limit_label = if strict_budget {
        "strict budget"
    } else {
        "noise ceiling; set EE_STRICT_PERF_BUDGET=1 to enforce target"
    };
    assert!(
        warm_elapsed.as_millis() < warm_limit_ms,
        "warm open-to-first-render for {label} fixture took {}ms, expected < {warm_limit_ms}ms ({warm_limit_label}); target < {WARM_BUDGET_MS}ms; samples={warm_samples:?}",
        warm_elapsed.as_millis()
    );
}

fn report_open_to_first_render_breakdown(label: &str, line_builder: fn(usize) -> String) {
    let thresholds = OpenThresholds::default();
    let target_bytes = thresholds.normal_bytes as usize - 4096;
    let path = unique_temp_path(&format!("ee-cli-open-breakdown-{label}"));
    let line_count = write_exact_size_ascii_fixture(&path, target_bytes, line_builder);

    assert!(
        line_count < thresholds.normal_lines as usize,
        "fixture {label} produced {line_count} lines, expected < {}",
        thresholds.normal_lines
    );

    drop(App::from_path(None).unwrap());

    let (_cold_app, cold) = timed_open_to_first_render_breakdown(&path);
    let (_warm_app, warm) = timed_open_to_first_render_breakdown(&path);
    fs::remove_file(&path).unwrap();

    eprintln!("cold {label} breakdown: {cold:#?}");
    eprintln!("warm {label} breakdown: {warm:#?}");
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
        save_complete: true,
        last_save_generation: 0,
        completed_save_generation: 0,
        status_message: None,
        last_scroll: None,
        mtime: None,
        externally_modified: false,
        diagnostics: Vec::new(),
        annotations: Vec::new(),
        is_vlf: false,
        vlf_cache_start_line: 0,
        vlf_previous_viewport: None,
        vlf_generation: 0,
        vlf_approx_line_count: 0,
        vlf_line_count_exact: false,
        pending_vlf_tail_jump: false,
        vlf_search_ranges: Vec::new(),
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
    let first = unique_temp_path("ee-cli-swap-a");
    let second = unique_temp_path("ee-cli-swap-b");
    let third = unique_temp_path("ee-cli-swap-c");
    let fourth = unique_temp_path("ee-cli-swap-d");
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
    assert_eq!(parse_action_spec("hover").unwrap(), Action::RequestHover);
    assert_eq!(
        parse_action_spec("select_references_to_symbol_under_cursor").unwrap(),
        Action::RequestReferences
    );
    assert_eq!(parse_action_spec("rename_symbol").unwrap(), Action::PrefillCommandLine("rename "));
}

#[test]
fn create_directory_command_creates_nested_path_in_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    env::set_current_dir(temp.path()).unwrap();

    let mut app = App::from_path(None).unwrap();
    run_ex(&mut app, "create_directory alpha/beta");

    assert!(temp.path().join("alpha/beta").is_dir());
    assert_eq!(app.backend.status_message.as_deref(), Some("created alpha/beta"));
}

#[test]
fn create_directory_command_rejects_workspace_escape() {
    let temp = tempfile::tempdir().unwrap();
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    env::set_current_dir(temp.path()).unwrap();

    let mut app = App::from_path(None).unwrap();
    run_ex(&mut app, "create_directory ../escape");

    assert!(!temp.path().parent().unwrap().join("escape").exists());
    let message = app.backend.status_message.as_deref().unwrap_or_default();
    assert!(message.contains("path must stay under workspace"));
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
    let first = unique_temp_path("ee-cli-write-bang");
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

    let second = unique_temp_path("ee-cli-update-bang");
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

    let third = unique_temp_path("ee-cli-x-bang");
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
    let first = unique_temp_path("ee-cli-wa-first");
    let second = unique_temp_path("ee-cli-wa-second");
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
    let first = unique_temp_path("ee-cli-qa-first");
    let second = unique_temp_path("ee-cli-qa-second");
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
    let source = unique_temp_path("ee-cli-read-source");
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
    let source = unique_temp_path("ee-cli-move-source");
    let target = unique_temp_path("ee-cli-move-target");
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
fn reload_config_refreshes_runtime_sequence_keymap() {
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
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
"#,
    )
    .unwrap();

    env::set_current_dir(temp.path()).unwrap();

    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)));
    let hints = app.active_key_sequence_node().expect("sequence hints should be active");
    let descriptions =
        hints.hint_entries().into_iter().map(|entry| entry.description).collect::<Vec<_>>();
    assert!(descriptions.iter().any(|description| description == "find files"));

    fs::write(
        temp.path().join(".ee.toml"),
        r#"
[keymap]
inherit_defaults = true

[[keymap.sequence_bindings]]
mode = "normal"
keys = ["space", "f", "f"]
action = "file_picker"
description = "project files"
"#,
    )
    .unwrap();

    app.input_state.reset();
    run_ex(&mut app, "reload_config");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)));
    let hints = app.active_key_sequence_node().expect("reloaded sequence hints should be active");
    let descriptions =
        hints.hint_entries().into_iter().map(|entry| entry.description).collect::<Vec<_>>();
    assert!(descriptions.iter().any(|description| description == "project files"));
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
fn global_search_and_command_palette_commands_open_expected_pickers() {
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    let temp = tempfile::tempdir().unwrap();
    env::set_current_dir(temp.path()).unwrap();

    let mut app = App::from_path(None).unwrap();
    run_ex(&mut app, "global_search");
    let picker = app.picker.as_ref().expect("global search should open picker");
    assert_eq!(picker.kind, PickerKind::LiveGrep);
    assert_eq!(picker.title, "Global Search");

    app.picker = None;
    run_ex(&mut app, "command_palette");
    let picker = app.picker.as_ref().expect("command palette should open picker");
    assert_eq!(picker.kind, PickerKind::Help);
    assert_eq!(picker.title, "Command Palette");
}

#[test]
fn ctrl_p_opens_cwd_file_picker_from_normal_mode() {
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    let temp = tempfile::tempdir().unwrap();
    env::set_current_dir(temp.path()).unwrap();
    fs::write(temp.path().join("sample.rs"), "fn main() {}\n").unwrap();

    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)));

    let picker = app.picker.as_ref().expect("ctrl+p should open picker");
    assert_eq!(picker.kind, PickerKind::Files);
    assert_eq!(picker.title, "Files (cwd)");
}

#[test]
fn ctrl_alt_p_opens_command_palette_from_normal_mode() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Char('p'),
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    )));

    let picker = app.picker.as_ref().expect("ctrl+alt+p should open picker");
    assert_eq!(picker.kind, PickerKind::Help);
    assert_eq!(picker.title, "Command Palette");
}

#[test]
fn insert_mode_does_not_use_normal_mode_picker_shortcuts() {
    let mut app = App::from_path(None).unwrap();
    app.mode = Mode::Insert;

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)));
    assert!(app.picker.is_none());

    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Char('p'),
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    )));
    assert!(app.picker.is_none());
}

#[test]
fn insert_register_action_inserts_named_register_contents_in_insert_mode() {
    let mut app = App::from_path(None).unwrap();
    app.registers.yank(&RegisterName::Named('a'), String::from("alpha"), false);
    app.mode = Mode::Insert;

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)));
    assert!(app.input_state.awaiting_register);
    assert!(app.input_state.awaiting_register_insert);
    assert_eq!(app.pending_input_label().as_deref(), Some("insert register | press register name"));

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)));
    app.backend.pump().unwrap();

    assert_eq!(app.backend.lines, vec![String::from("alpha")]);
    assert!(!app.input_state.awaiting_register);
    assert!(!app.input_state.awaiting_register_insert);
}

#[test]
fn cd_pwd_and_lsp_commands_update_status() {
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    let temp = tempfile::tempdir().unwrap();

    let mut app = App::from_path(None).unwrap();
    run_ex(&mut app, &format!("cd {}", temp.path().display()));
    assert_eq!(
        std::env::current_dir().unwrap().canonicalize().unwrap(),
        temp.path().canonicalize().unwrap()
    );
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
    init_test_git_repo(temp.path());

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

#[test]
fn reload_and_reload_all_aliases_refresh_from_disk() {
    let first = unique_temp_path("ee-cli-reload-first");
    let second = unique_temp_path("ee-cli-reload-second");
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
    let first = unique_temp_path("ee-cli-bc-first");
    let second = unique_temp_path("ee-cli-bc-second");
    let third = unique_temp_path("ee-cli-bc-third");
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
    let first = unique_temp_path("ee-cli-goto-buffer-first");
    let second = unique_temp_path("ee-cli-goto-buffer-second");
    let third = unique_temp_path("ee-cli-goto-buffer-third");
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
    let first = unique_temp_path("ee-cli-goto-recent-first");
    let second = unique_temp_path("ee-cli-goto-recent-second");
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

    let annotation_bg = ratatui::style::Color::Rgb(43, 82, 74);
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

#[test]
fn render_single_very_long_ascii_line_under_budget() {
    const LINE_LEN: usize = 1_000_000;
    const FRAME_BUDGET_MS: u128 = 50;

    let mut app = App::from_path(None).unwrap();
    app.backend.lines = vec!["3".repeat(LINE_LEN)];
    app.viewport.left_col = 100_000;

    let backend = TestBackend::new(120, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    let start = std::time::Instant::now();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_millis() < FRAME_BUDGET_MS,
        "render of {LINE_LEN}-byte line took {}ms, expected < {FRAME_BUDGET_MS}ms",
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
    buf.apply_vlf_chunks(VlfChunkUpdate {
        generation: 7,
        line_start: 0,
        lines: &lines,
        syntax_spans: &[],
        approximate_line_count: 3,
        line_count_exact: true,
    });

    assert_eq!(
        buf.line_slot(0).cloned().unwrap(),
        LineSlot::Known(CachedLine {
            text: String::from("alpha"),
            cursors: vec![],
            syntax_spans: vec![]
        })
    );
    assert_eq!(
        buf.line_slot(1).cloned().unwrap(),
        LineSlot::Known(CachedLine {
            text: String::from("beta"),
            cursors: vec![],
            syntax_spans: vec![]
        })
    );
    assert_eq!(buf.line_count(), 3);
    assert!(buf.line_slot(2).is_none());
}

#[test]
fn apply_vlf_chunks_normalizes_crlf_line_endings() {
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.vlf_generation = 2;

    let lines = vec![String::from("alpha\r"), String::from("beta\r")];
    buf.apply_vlf_chunks(VlfChunkUpdate {
        generation: 2,
        line_start: 0,
        lines: &lines,
        syntax_spans: &[],
        approximate_line_count: 2,
        line_count_exact: true,
    });

    assert_eq!(buf.get_line(0), Some("alpha"));
    assert_eq!(buf.get_line(1), Some("beta"));
}

#[test]
fn apply_vlf_chunks_empty_response_preserves_loaded_cache() {
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.vlf_generation = 3;
    buf.vlf_cache_start_line = 40;
    buf.line_cache = vec![
        LineSlot::Known(CachedLine {
            text: String::from("line 40"),
            cursors: vec![],
            syntax_spans: vec![],
        }),
        LineSlot::Known(CachedLine {
            text: String::from("line 41"),
            cursors: vec![],
            syntax_spans: vec![],
        }),
    ];

    buf.apply_vlf_chunks(VlfChunkUpdate {
        generation: 3,
        line_start: 100,
        lines: &[],
        syntax_spans: &[],
        approximate_line_count: 1_000,
        line_count_exact: false,
    });

    assert_eq!(buf.vlf_cache_start_line, 40);
    assert_eq!(buf.get_line(40), Some("line 40"));
    assert_eq!(buf.get_line(41), Some("line 41"));
    assert_eq!(buf.vlf_approx_line_count, 1_000);
}

#[test]
fn apply_vlf_chunks_empty_response_keeps_tail_jump_pending() {
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.vlf_generation = 4;
    buf.pending_vlf_tail_jump = true;

    buf.apply_vlf_chunks(VlfChunkUpdate {
        generation: 4,
        line_start: u64::MAX - 1,
        lines: &[],
        syntax_spans: &[],
        approximate_line_count: 10_000,
        line_count_exact: false,
    });

    assert!(buf.pending_vlf_tail_jump);
    assert_eq!((buf.cursor_line, buf.cursor_col), (0, 0));
}

#[test]
fn apply_vlf_chunks_stale_generation_discarded() {
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.vlf_generation = 5;
    buf.line_cache = vec![LineSlot::Invalid; 2];

    let lines = vec![String::from("stale")];
    // Send with generation 3 (older than current 5) — must be ignored.
    buf.apply_vlf_chunks(VlfChunkUpdate {
        generation: 3,
        line_start: 0,
        lines: &lines,
        syntax_spans: &[],
        approximate_line_count: 2,
        line_count_exact: false,
    });

    assert_eq!(buf.line_cache[0], LineSlot::Invalid, "stale response must not update cache");
}

#[test]
fn apply_vlf_chunks_does_not_grow_cache_to_approximate_count() {
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.vlf_generation = 1;
    buf.line_cache = Vec::new(); // start empty

    let lines: Vec<String> = Vec::new();
    buf.apply_vlf_chunks(VlfChunkUpdate {
        generation: 1,
        line_start: 0,
        lines: &lines,
        syntax_spans: &[],
        approximate_line_count: 1000,
        line_count_exact: false,
    });

    assert_eq!(buf.line_cache.len(), 0, "cache must stay viewport-local");
    assert_eq!(buf.line_count(), 1000);
    assert_eq!(buf.vlf_approx_line_count, 1000);
}

#[test]
fn apply_vlf_chunks_exact_count_replaces_stale_window() {
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.vlf_generation = 1;
    buf.line_cache = vec![LineSlot::Invalid; 1000];

    buf.apply_vlf_chunks(VlfChunkUpdate {
        generation: 1,
        line_start: 10,
        lines: &[String::from("tail")],
        syntax_spans: &[],
        approximate_line_count: 25,
        line_count_exact: true,
    });

    assert_eq!(buf.line_count(), 25);
    assert_eq!(buf.line_cache.len(), 1);
    assert_eq!(buf.vlf_cache_start_line, 10);
    assert_eq!(buf.get_line(10), Some("tail"));
    assert!(buf.vlf_line_count_exact);
}

#[test]
fn vlf_line_count_uses_exact_report_over_stale_cache() {
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.line_cache = vec![LineSlot::Invalid; 1000];
    buf.vlf_approx_line_count = 25;
    buf.vlf_line_count_exact = true;

    assert_eq!(buf.line_count(), 25);
}

#[test]
fn vlf_line_count_keeps_sparse_cache_when_exact_report_missing() {
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.line_cache = vec![LineSlot::Invalid; 500];
    buf.vlf_line_count_exact = true;

    assert_eq!(buf.line_count(), 500);
}

#[test]
fn apply_vlf_chunks_tail_jump_moves_cursor_to_returned_last_line() {
    let mut buf = test_buf_state();
    buf.is_vlf = true;
    buf.vlf_generation = 1;
    buf.pending_vlf_tail_jump = true;

    buf.apply_vlf_chunks(VlfChunkUpdate {
        generation: 1,
        line_start: 995,
        lines: &[String::from("line 998"), String::from("line 999")],
        syntax_spans: &[],
        approximate_line_count: 1000,
        line_count_exact: false,
    });

    assert_eq!((buf.cursor_line, buf.cursor_col), (996, 0));
    assert!(!buf.pending_vlf_tail_jump);
}

#[test]
fn vlf_document_mode_clears_stale_normal_cache_and_retries_viewport() {
    let (tx, rx) = mpsc::channel();
    let (backend_tx, backend_rx) = mpsc::channel();
    let mut mgr = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    mgr.lines = vec![String::from("stale normal line")];

    backend_tx
        .send(BackendEvent::DocumentMode { view_id: String::from("view-id-1"), is_vlf: true })
        .unwrap();
    mgr.drain_events().unwrap();
    assert!(mgr.active().is_vlf);
    assert!(mgr.active().lines.is_empty());
    assert_eq!(mgr.active().line_cache.len(), 200);
    assert!(mgr.active().line_cache.iter().all(|slot| matches!(slot, LineSlot::Invalid)));

    mgr.notify_scroll(0, 4).unwrap();
    let first: Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).unwrap())
        .expect("vlf viewport notification should be json");
    assert_eq!(first["params"]["method"], "vlf_viewport");
    assert_eq!(first["params"]["params"]["line_start"], 0);
    assert_eq!(first["params"]["params"]["line_end"], 200);
    assert_eq!(first["params"]["params"]["generation"], 1);

    mgr.notify_scroll(0, 4).unwrap();
    assert!(matches!(
        rx.recv_timeout(Duration::from_millis(50)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));

    backend_tx
        .send(BackendEvent::VlfChunks {
            view_id: String::from("view-id-1"),
            generation: 1,
            line_start: 0,
            lines: Vec::new(),
            syntax_spans: Vec::new(),
            approximate_line_count: 1000,
            line_count_exact: false,
            index_progress: 0.1,
        })
        .unwrap();
    mgr.drain_events().unwrap();

    mgr.notify_scroll(0, 4).unwrap();
    let retry: Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).unwrap())
        .expect("vlf viewport retry after empty response should be json");
    assert_eq!(retry["params"]["method"], "vlf_viewport");
    assert_eq!(retry["params"]["params"]["line_start"], 0);
    assert_eq!(retry["params"]["params"]["line_end"], 204);
    assert_eq!(retry["params"]["params"]["generation"], 2);
}

#[test]
fn vlf_notify_scroll_prefetches_beyond_ready_visible_rows() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut mgr = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    mgr.is_vlf = true;
    mgr.vlf_cache_start_line = 0;
    mgr.vlf_approx_line_count = 10_000;
    mgr.line_cache = (0..4)
        .map(|line| {
            LineSlot::Known(CachedLine {
                text: format!("line {line}"),
                cursors: Vec::new(),
                syntax_spans: Vec::new(),
            })
        })
        .collect();

    mgr.notify_scroll(0, 4).unwrap();
    let scroll: Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).unwrap())
        .expect("vlf viewport notification should be json");

    assert_eq!(scroll["params"]["method"], "vlf_viewport");
    assert_eq!(scroll["params"]["params"]["line_start"], 0);
    assert_eq!(scroll["params"]["params"]["line_end"], 204);
}

#[test]
fn vlf_invalid_cache_does_not_request_normal_lines() {
    let (tx, rx) = mpsc::channel();
    let (backend_tx, backend_rx) = mpsc::channel();
    let mut mgr = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    backend_tx
        .send(BackendEvent::DocumentMode { view_id: String::from("view-id-1"), is_vlf: true })
        .unwrap();
    backend_tx
        .send(BackendEvent::VlfChunks {
            view_id: String::from("view-id-1"),
            generation: 0,
            line_start: 0,
            lines: Vec::new(),
            syntax_spans: Vec::new(),
            approximate_line_count: 1000,
            line_count_exact: false,
            index_progress: 0.1,
        })
        .unwrap();
    mgr.drain_events().unwrap();

    mgr.pump().unwrap();
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn vlf_git_diff_command_reports_clear_status() {
    let mut app = App::from_path(None).unwrap();
    app.backend.is_vlf = true;

    run_ex(&mut app, "gdiff");

    assert_eq!(
        app.backend.status_message.as_deref(),
        Some("git diff disabled in VLF: requires whole-buffer diff/blame scans")
    );
}

#[test]
fn vlf_ignores_normal_update_after_document_mode() {
    let (tx, _rx) = mpsc::channel();
    let (backend_tx, backend_rx) = mpsc::channel();
    let mut mgr = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    backend_tx
        .send(BackendEvent::DocumentMode { view_id: String::from("view-id-1"), is_vlf: true })
        .unwrap();
    backend_tx
        .send(BackendEvent::Update {
            view_id: String::from("view-id-1"),
            update: CoreUpdate {
                ops: vec![CoreUpdateOp {
                    op: CoreUpdateKind::Insert,
                    n: 1,
                    lines: vec![CoreLine {
                        text: Some(String::from("stale normal line")),
                        cursor: Vec::new(),
                        syntax_spans: Some(Vec::new()),
                    }],
                }],
                pristine: true,
                annotations: Vec::new(),
            },
        })
        .unwrap();

    mgr.drain_events().unwrap();
    assert!(mgr.active().is_vlf);
    assert!(
        mgr.active().line_cache.iter().all(|slot| matches!(slot, LineSlot::Invalid)),
        "VLF must ignore normal update payloads"
    );
}

#[test]
fn vlf_local_navigation_moves_cursor_without_core_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.line_cache = vec![
        LineSlot::Known(CachedLine {
            text: String::from("alpha"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
        LineSlot::Known(CachedLine {
            text: String::from("beta"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
    ];

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));

    assert_eq!(app.backend.cursor_line, 1);
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn vlf_insert_key_uses_overlay_edit_rpc_without_cursor_jump() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.vlf_cache_start_line = 40;
    app.backend.vlf_approx_line_count = 100;
    app.backend.vlf_line_count_exact = true;
    app.backend.cursor_line = 41;
    app.backend.cursor_col = 2;
    app.backend.line_cache = vec![
        LineSlot::Known(CachedLine {
            text: String::from("alpha"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
        LineSlot::Known(CachedLine {
            text: String::from("beta"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
    ];
    app.last_editor_height = 6;

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)));

    assert_eq!(app.mode, Mode::Insert);
    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (41, 3));
    assert_eq!(app.backend.status_message, None);
    assert_eq!(app.backend.get_line(41), Some("bexta"));

    let first: Value = serde_json::from_str(
        &rx.recv_timeout(Duration::from_secs(1)).expect("vlf edit rpc should be sent"),
    )
    .expect("message should be json");
    assert_eq!(first["method"], "edit");
    assert_eq!(first["params"]["method"], "vlf_replace_range");
    assert_eq!(first["params"]["params"]["start_line"], 41);
    assert_eq!(first["params"]["params"]["start_col"], 2);
    assert_eq!(first["params"]["params"]["end_line"], 41);
    assert_eq!(first["params"]["params"]["end_col"], 2);
    assert_eq!(first["params"]["params"]["text"], "x");

    let second: Value = serde_json::from_str(
        &rx.recv_timeout(Duration::from_secs(1)).expect("viewport refresh should follow vlf edit"),
    )
    .expect("message should be json");
    assert_eq!(second["params"]["method"], "vlf_viewport");
}

#[test]
fn vlf_insert_preserves_untouched_syntax_spans_before_viewport_reply() {
    let (tx, _rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.vlf_cache_start_line = 41;
    app.backend.vlf_approx_line_count = 100;
    app.backend.vlf_line_count_exact = true;
    app.backend.cursor_line = 41;
    app.backend.cursor_col = 2;
    app.backend.line_cache = vec![LineSlot::Known(CachedLine {
        text: String::from("beta"),
        cursors: Vec::new(),
        syntax_spans: vec![
            CoreSyntaxSpan { start_byte: 0, end_byte: 2, scope: String::from("prefix") },
            CoreSyntaxSpan { start_byte: 2, end_byte: 4, scope: String::from("suffix") },
        ],
    })];
    app.last_editor_height = 6;

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)));

    let spans = match app.backend.line_slot(41) {
        Some(LineSlot::Known(line)) => line.syntax_spans.clone(),
        other => panic!("expected cached VLF line, got {other:?}"),
    };
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0].scope, "prefix");
    assert_eq!((spans[0].start_byte, spans[0].end_byte), (0, 2));
    assert_eq!(spans[1].scope, "suffix");
    assert_eq!((spans[1].start_byte, spans[1].end_byte), (3, 5));
}

#[test]
fn vlf_insert_forces_viewport_refresh_when_current_range_is_already_cached() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.vlf_cache_start_line = 40;
    app.backend.vlf_approx_line_count = 100;
    app.backend.vlf_line_count_exact = true;
    app.backend.cursor_line = 41;
    app.backend.cursor_col = 2;
    app.backend.last_scroll = Some((40, 46));
    app.viewport.top_line = 40;
    app.last_editor_height = 6;
    app.backend.line_cache = vec![
        LineSlot::Known(CachedLine {
            text: String::from("alpha"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
        LineSlot::Known(CachedLine {
            text: String::from("beta"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
        LineSlot::Known(CachedLine {
            text: String::from("gamma"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
        LineSlot::Known(CachedLine {
            text: String::from("delta"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
        LineSlot::Known(CachedLine {
            text: String::from("epsilon"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
        LineSlot::Known(CachedLine {
            text: String::from("zeta"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
    ];

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)));

    let first: Value = serde_json::from_str(
        &rx.recv_timeout(Duration::from_secs(1)).expect("vlf edit rpc should be sent"),
    )
    .expect("message should be json");
    assert_eq!(first["params"]["method"], "vlf_replace_range");

    let second: Value = serde_json::from_str(
        &rx.recv_timeout(Duration::from_secs(1))
            .expect("forced viewport refresh should be sent for cached range"),
    )
    .expect("message should be json");
    assert_eq!(second["params"]["method"], "vlf_viewport");
    assert_eq!(second["params"]["params"]["line_start"], 40);
    assert_eq!(second["params"]["params"]["line_end"], 46);
}

#[test]
fn vlf_insert_newline_updates_local_cache_before_viewport_reply() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.vlf_cache_start_line = 40;
    app.backend.vlf_approx_line_count = 100;
    app.backend.vlf_line_count_exact = true;
    app.backend.cursor_line = 41;
    app.backend.cursor_col = 2;
    app.backend.line_cache = vec![
        LineSlot::Known(CachedLine {
            text: String::from("alpha"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
        LineSlot::Known(CachedLine {
            text: String::from("beta"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
    ];
    app.last_editor_height = 6;

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    assert_eq!(app.mode, Mode::Insert);
    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (42, 0));
    assert_eq!(app.backend.get_line(41), Some("be"));
    assert_eq!(app.backend.get_line(42), Some("ta"));
    assert_eq!(app.backend.line_count(), 101);

    let first: Value = serde_json::from_str(
        &rx.recv_timeout(Duration::from_secs(1)).expect("newline edit rpc should be sent"),
    )
    .expect("message should be json");
    assert_eq!(first["params"]["method"], "vlf_replace_range");
    assert_eq!(first["params"]["params"]["text"], "\n");

    let second: Value = serde_json::from_str(
        &rx.recv_timeout(Duration::from_secs(1)).expect("viewport refresh should follow"),
    )
    .expect("message should be json");
    assert_eq!(second["params"]["method"], "vlf_viewport");
}

#[test]
fn vlf_backspace_updates_local_cache_before_viewport_reply() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.vlf_cache_start_line = 41;
    app.backend.vlf_approx_line_count = 100;
    app.backend.vlf_line_count_exact = true;
    app.backend.cursor_line = 42;
    app.backend.cursor_col = 0;
    app.backend.line_cache = vec![
        LineSlot::Known(CachedLine {
            text: String::from("be"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
        LineSlot::Known(CachedLine {
            text: String::from("ta"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
    ];
    app.last_editor_height = 6;

    app.mode = Mode::Insert;
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)));

    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (41, 2));
    assert_eq!(app.backend.get_line(41), Some("beta"));
    assert_eq!(app.backend.get_line(42), None);
    assert_eq!(app.backend.line_count(), 99);

    let first: Value = serde_json::from_str(
        &rx.recv_timeout(Duration::from_secs(1)).expect("backspace edit rpc should be sent"),
    )
    .expect("message should be json");
    assert_eq!(first["params"]["method"], "vlf_replace_range");
    assert_eq!(first["params"]["params"]["start_line"], 41);
    assert_eq!(first["params"]["params"]["start_col"], 2);
    assert_eq!(first["params"]["params"]["end_line"], 42);
    assert_eq!(first["params"]["params"]["end_col"], 0);
    assert_eq!(first["params"]["params"]["text"], "");

    let second: Value = serde_json::from_str(
        &rx.recv_timeout(Duration::from_secs(1)).expect("viewport refresh should follow"),
    )
    .expect("message should be json");
    assert_eq!(second["params"]["method"], "vlf_viewport");
}

#[test]
fn vlf_delete_char_forward_command_uses_overlay_edit_rpc() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.vlf_cache_start_line = 41;
    app.backend.vlf_approx_line_count = 100;
    app.backend.vlf_line_count_exact = true;
    app.backend.cursor_line = 41;
    app.backend.cursor_col = 2;
    app.backend.line_cache = vec![
        LineSlot::Known(CachedLine {
            text: String::from("be"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
        LineSlot::Known(CachedLine {
            text: String::from("ta"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        }),
    ];
    app.last_editor_height = 6;

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)));
    for ch in "delete_char_forward".chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    assert_eq!(app.backend.get_line(41), Some("beta"));
    assert_eq!(app.backend.get_line(42), None);

    let first: Value = serde_json::from_str(
        &rx.recv_timeout(Duration::from_secs(1)).expect("command edit rpc should be sent"),
    )
    .expect("message should be json");
    assert_eq!(first["params"]["method"], "vlf_replace_range");
    assert_eq!(first["params"]["params"]["start_line"], 41);
    assert_eq!(first["params"]["params"]["start_col"], 2);
    assert_eq!(first["params"]["params"]["end_line"], 42);
    assert_eq!(first["params"]["params"]["end_col"], 0);
    assert_eq!(first["params"]["params"]["text"], "");

    let second: Value = serde_json::from_str(
        &rx.recv_timeout(Duration::from_secs(1)).expect("viewport refresh should follow"),
    )
    .expect("message should be json");
    assert_eq!(second["params"]["method"], "vlf_viewport");
}

#[test]
fn vlf_zero_moves_to_line_start_without_core_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.cursor_line = 9;
    app.backend.cursor_col = 4;
    app.backend.line_cache = vec![LineSlot::Invalid; 20];
    app.backend.line_cache[9] = LineSlot::Known(CachedLine {
        text: String::from("alpha"),
        cursors: Vec::new(),
        syntax_spans: Vec::new(),
    });

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE)));

    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (9, 0));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn vlf_goto_last_line_uses_sparse_line_count_without_core_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.line_cache = vec![LineSlot::Invalid; 500];
    app.backend.vlf_line_count_exact = true;

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)));

    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (499, 0));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn vlf_goto_last_line_uses_reported_line_count_without_core_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.line_cache = vec![LineSlot::Invalid; 500];
    app.backend.vlf_approx_line_count = 10_000;
    app.backend.vlf_line_count_exact = true;

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE)));

    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (9_999, 0));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn vlf_goto_last_line_requests_tail_viewport_when_count_is_approximate() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.line_cache = vec![LineSlot::Invalid; 500];
    app.backend.vlf_approx_line_count = 10_000;
    app.backend.vlf_line_count_exact = false;
    app.last_editor_height = 40;

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE)));

    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (0, 0));
    let message: Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).unwrap())
        .expect("tail viewport request should be json");
    assert_eq!(message["params"]["method"], "vlf_viewport");
    assert_eq!(message["params"]["params"]["line_start"], u64::MAX);
    assert_eq!(message["params"]["params"]["line_end"], 4095);
    assert!(app.backend.pending_vlf_tail_jump);
}

#[test]
fn vlf_navigation_away_from_pending_tail_jump_cancels_tail_request() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.line_cache = vec![LineSlot::Invalid; 500];
    app.backend.vlf_approx_line_count = 10_000;
    app.backend.vlf_line_count_exact = false;
    app.last_editor_height = 40;

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE)));
    let tail_request: Value =
        serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).unwrap())
            .expect("tail viewport request should be json");
    assert_eq!(tail_request["params"]["params"]["line_start"], u64::MAX);
    assert!(app.backend.pending_vlf_tail_jump);

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE)));

    assert_eq!(app.backend.cursor_line, 0);
    assert!(!app.backend.pending_vlf_tail_jump);
    assert!(!app.backend.pending_line_request);

    app.backend.notify_scroll(0, 40).unwrap();
    let top_request: Value =
        serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).unwrap())
            .expect("top viewport request should be json");
    assert_eq!(top_request["params"]["method"], "vlf_viewport");
    assert_eq!(top_request["params"]["params"]["line_start"], 0);
    assert_eq!(top_request["params"]["params"]["line_end"], 240);
}

#[test]
fn vlf_pending_tail_jump_blocks_regular_viewport_scroll() {
    let (tx, rx) = mpsc::channel();
    let (backend_tx, backend_rx) = mpsc::channel();
    let mut mgr = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    mgr.is_vlf = true;
    mgr.vlf_approx_line_count = 10_000;

    mgr.request_vlf_tail_viewport(40).unwrap();
    let first: Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).unwrap())
        .expect("tail viewport request should be json");
    assert_eq!(first["params"]["params"]["line_start"], u64::MAX);

    mgr.notify_scroll(9_950, 9_990).unwrap();
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    assert!(mgr.pending_vlf_tail_jump);

    let tail_lines = (0..40).map(|idx| format!("tail {idx}")).collect::<Vec<_>>();
    backend_tx
        .send(BackendEvent::VlfChunks {
            view_id: String::from("view-id-1"),
            generation: 1,
            line_start: 9_960,
            lines: tail_lines,
            syntax_spans: Vec::new(),
            approximate_line_count: 10_000,
            line_count_exact: false,
            index_progress: 0.1,
        })
        .unwrap();
    mgr.drain_events().unwrap();

    assert!(!mgr.pending_vlf_tail_jump);
    assert_eq!(mgr.cursor_line, 9_999);
    mgr.notify_scroll(9_960, 10_000).unwrap();
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn vlf_completed_tail_jump_then_top_restores_cached_top_viewport() {
    let (tx, rx) = mpsc::channel();
    let (backend_tx, backend_rx) = mpsc::channel();
    let mut mgr = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    mgr.is_vlf = true;
    mgr.vlf_approx_line_count = 10_000;
    mgr.line_cache = (0..240)
        .map(|line| {
            LineSlot::Known(CachedLine {
                text: format!("top {line}"),
                cursors: Vec::new(),
                syntax_spans: Vec::new(),
            })
        })
        .collect();

    mgr.request_vlf_tail_viewport(40).unwrap();
    let first: Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).unwrap())
        .expect("tail viewport request should be json");
    assert_eq!(first["params"]["params"]["line_start"], u64::MAX);

    let tail_lines = (0..40).map(|idx| format!("tail {idx}")).collect::<Vec<_>>();
    backend_tx
        .send(BackendEvent::VlfChunks {
            view_id: String::from("view-id-1"),
            generation: 1,
            line_start: 9_960,
            lines: tail_lines,
            syntax_spans: Vec::new(),
            approximate_line_count: 10_000,
            line_count_exact: false,
            index_progress: 0.1,
        })
        .unwrap();
    mgr.drain_events().unwrap();

    assert_eq!(mgr.vlf_cache_start_line, 9_960);
    assert_eq!(mgr.get_line(9_960), Some("tail 0"));

    mgr.cursor_line = 0;
    mgr.notify_scroll(0, 40).unwrap();

    assert_eq!(mgr.vlf_cache_start_line, 0);
    assert_eq!(mgr.get_line(0), Some("top 0"));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn vlf_startup_pump_requests_initial_viewport_after_document_mode() {
    let path = unique_temp_path("ee-cli-vlf-startup");
    fs::write(&path, "alpha\nbeta\n").unwrap();

    let (tx, rx) = mpsc::channel();
    let (backend_tx, backend_rx) = mpsc::channel();
    let mut mgr = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    mgr.path = Some(path.clone());

    backend_tx
        .send(BackendEvent::DocumentMode { view_id: String::from("view-id-1"), is_vlf: true })
        .unwrap();

    mgr.pump_init().unwrap();

    let message: Value = serde_json::from_str(
        &rx.recv_timeout(Duration::from_secs(1)).expect("initial VLF viewport should be sent"),
    )
    .expect("viewport request should be json");
    assert_eq!(message["params"]["method"], "vlf_viewport");
    assert_eq!(message["params"]["params"]["line_start"], 0);
    assert_eq!(message["params"]["params"]["line_end"], 200);
    assert!(mgr.pending_line_request);

    fs::remove_file(path).unwrap();
}

#[test]
#[ignore = "manual real-fixture check; requires test_assets/vbig-100.txt"]
fn vlf_goto_vbig_100_matches_wc_last_line() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test_assets/vbig-100.txt");
    if !path.exists() {
        eprintln!("missing {}", path.display());
        return;
    }

    let wc = Command::new("wc").arg("-l").arg(&path).output().expect("wc -l should run");
    assert!(wc.status.success(), "wc -l failed: {wc:?}");
    let stdout = String::from_utf8(wc.stdout).expect("wc output should be utf8");
    let expected_last_line = stdout
        .split_whitespace()
        .next()
        .expect("wc output should include count")
        .parse::<usize>()
        .expect("wc count should parse");
    let expected_text = String::from_utf8(
        Command::new("tail").arg("-n").arg("1").arg(&path).output().unwrap().stdout,
    )
    .unwrap()
    .trim_end_matches('\n')
    .to_owned();

    let mut app = App::from_path(Some(path)).unwrap();
    for _ in 0..20 {
        app.backend.pump().unwrap();
        if app.backend.is_vlf {
            break;
        }
    }
    assert!(app.backend.is_vlf, "fixture should open in VLF");

    app.last_editor_height = 40;
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE)));

    for _ in 0..40 {
        app.backend.pump().unwrap();
        if !app.backend.pending_vlf_tail_jump && app.backend.cursor_line == expected_last_line {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(app.backend.cursor_line, expected_last_line);
    assert_eq!(app.backend.get_line(app.backend.cursor_line), Some(expected_text.as_str()));
}

#[test]
#[ignore = "manual real-fixture check; requires test_assets/vbig-2gb.txt"]
fn vlf_open_vbig_2gb_populates_initial_and_tail_scroll_cache() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test_assets/vbig-2gb.txt");
    if !path.exists() {
        eprintln!("missing {}", path.display());
        return;
    }

    let expected_text = String::from_utf8(
        Command::new("tail").arg("-n").arg("1").arg(&path).output().unwrap().stdout,
    )
    .unwrap()
    .trim_end_matches('\n')
    .to_owned();

    let mut app = App::from_path(Some(path)).unwrap();
    for _ in 0..80 {
        app.backend.pump().unwrap();
        if app.backend.is_vlf && app.backend.get_line(0).is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(app.backend.is_vlf, "fixture should open in VLF");
    assert!(app.backend.get_line(0).is_some(), "initial viewport should not stay Loading");

    app.last_editor_height = 40;
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE)));

    for _ in 0..120 {
        app.backend.pump().unwrap();
        if !app.backend.pending_vlf_tail_jump
            && app.backend.get_line(app.backend.cursor_line) == Some(expected_text.as_str())
        {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(app.backend.get_line(app.backend.cursor_line), Some(expected_text.as_str()));
    let scroll_up_line = app.backend.cursor_line.saturating_sub(80);
    assert!(
        app.backend.get_line(scroll_up_line).is_some(),
        "tail prefetch should cover nearby scroll-up line {scroll_up_line}"
    );
}

#[test]
#[ignore = "manual real-fixture check; requires test_assets/world92.txt"]
fn vlf_open_world92_populates_initial_viewport() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test_assets/world92.txt");
    if !path.exists() {
        eprintln!("missing {}", path.display());
        return;
    }

    let expected_text = String::from_utf8(
        Command::new("head").arg("-n").arg("1").arg(&path).output().unwrap().stdout,
    )
    .unwrap()
    .trim_end_matches(['\r', '\n'])
    .to_owned();

    let mut app = App::from_path(Some(path)).unwrap();
    for _ in 0..80 {
        app.backend.pump().unwrap();
        if app.backend.is_vlf && app.backend.get_line(0).is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    assert!(app.backend.is_vlf, "fixture should open in VLF");
    assert_eq!(app.backend.get_line(0), Some(expected_text.as_str()));
}

#[test]
#[ignore = "manual real-fixture check; requires test_assets/world92.txt"]
fn vlf_world92_populates_top_and_line_100_viewports() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test_assets/world92.txt");
    if !path.exists() {
        eprintln!("missing {}", path.display());
        return;
    }

    let mut app = App::from_path(Some(path)).unwrap();
    for _ in 0..80 {
        app.backend.pump().unwrap();
        if app.backend.is_vlf && app.backend.get_line(0).is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(app.backend.is_vlf, "fixture should open in VLF");

    app.backend.notify_scroll(0, 40).unwrap();
    for _ in 0..120 {
        app.backend.pump().unwrap();
        if (0..40).all(|line| app.backend.get_line(line).is_some()) {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let missing_top =
        (0..40).filter(|&line| app.backend.get_line(line).is_none()).collect::<Vec<_>>();
    assert!(missing_top.is_empty(), "missing top VLF lines: {missing_top:?}");

    app.backend.cursor_line = 100;
    app.backend.notify_scroll(100, 140).unwrap();
    for _ in 0..120 {
        app.backend.pump().unwrap();
        if (100..140).all(|line| app.backend.get_line(line).is_some()) {
            break;
        }
        app.backend.notify_scroll(100, 140).unwrap();
        thread::sleep(Duration::from_millis(10));
    }
    let missing_100 =
        (100..140).filter(|&line| app.backend.get_line(line).is_none()).collect::<Vec<_>>();
    assert!(missing_100.is_empty(), "missing line-100 VLF lines: {missing_100:?}");
}

#[test]
#[ignore = "manual real-fixture check; requires test_assets/world92.txt"]
fn vlf_world92_tail_scroll_back_populates_full_viewport() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test_assets/world92.txt");
    if !path.exists() {
        eprintln!("missing {}", path.display());
        return;
    }

    let wc = Command::new("wc").arg("-l").arg(&path).output().expect("wc -l should run");
    assert!(wc.status.success(), "wc -l failed: {wc:?}");
    let stdout = String::from_utf8(wc.stdout).expect("wc output should be utf8");
    let expected_line_count = stdout
        .split_whitespace()
        .next()
        .expect("wc output should include count")
        .parse::<usize>()
        .expect("wc count should parse");
    let ends_with_newline = fs::read(&path).unwrap().last().is_some_and(|byte| *byte == b'\n');
    let expected_logical_line_count = expected_line_count + usize::from(ends_with_newline);

    let mut app = App::from_path(Some(path)).unwrap();
    for _ in 0..80 {
        app.backend.pump().unwrap();
        if app.backend.is_vlf && app.backend.get_line(0).is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(app.backend.is_vlf, "fixture should open in VLF");

    app.last_editor_height = 40;
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE)));
    for _ in 0..120 {
        app.backend.pump().unwrap();
        if !app.backend.pending_vlf_tail_jump && app.backend.cursor_line > 65_000 {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    assert!(!app.backend.pending_vlf_tail_jump, "tail jump should complete");
    assert_eq!(app.backend.cursor_line, expected_logical_line_count.saturating_sub(1));
    let top = app.backend.cursor_line.saturating_sub(40);
    app.backend.notify_scroll(top, top + 40).unwrap();
    for _ in 0..120 {
        app.backend.pump().unwrap();
        if (top..top + 40).all(|line| app.backend.get_line(line).is_some()) {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    let missing =
        (top..top + 40).filter(|&line| app.backend.get_line(line).is_none()).collect::<Vec<_>>();
    assert!(missing.is_empty(), "missing VLF lines after tail scroll-back: {missing:?}");
}

#[test]
#[ignore = "manual real-fixture check; requires test_assets/vbig-10gb.txt"]
fn vlf_goto_vbig_10gb_tail_returns_without_full_count() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test_assets/vbig-10gb.txt");
    if !path.exists() {
        eprintln!("missing {}", path.display());
        return;
    }

    let expected_text = String::from_utf8(
        Command::new("tail").arg("-n").arg("1").arg(&path).output().unwrap().stdout,
    )
    .unwrap()
    .trim_end_matches('\n')
    .to_owned();

    let mut app = App::from_path(Some(path)).unwrap();
    for _ in 0..80 {
        app.backend.pump().unwrap();
        if app.backend.is_vlf && app.backend.get_line(0).is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(app.backend.is_vlf, "fixture should open in VLF");
    assert!(app.backend.get_line(0).is_some(), "initial viewport should not stay Loading");

    app.last_editor_height = 40;
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE)));

    for _ in 0..120 {
        app.backend.pump().unwrap();
        if !app.backend.pending_vlf_tail_jump
            && app.backend.get_line(app.backend.cursor_line) == Some(expected_text.as_str())
        {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(app.backend.get_line(app.backend.cursor_line), Some(expected_text.as_str()));
    let scroll_up_line = app.backend.cursor_line.saturating_sub(80);
    assert!(
        app.backend.get_line(scroll_up_line).is_some(),
        "tail prefetch should cover nearby scroll-up line {scroll_up_line}"
    );
}

#[test]
fn vlf_visual_line_escape_restores_cursor_without_core_edit() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.cursor_line = 7;
    app.backend.cursor_col = 3;
    app.backend.line_cache = vec![LineSlot::Invalid; 10];
    app.backend.line_cache[7] = LineSlot::Known(CachedLine {
        text: String::from("abcdef"),
        cursors: Vec::new(),
        syntax_spans: Vec::new(),
    });

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('V'), KeyModifiers::NONE)));
    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (7, 0));

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));

    assert_eq!(app.mode, Mode::Normal);
    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (7, 3));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn vlf_visual_line_does_not_send_core_motion() {
    let (tx, rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    app.backend.is_vlf = true;
    app.backend.cursor_line = 7;
    app.backend.cursor_col = 3;
    app.backend.line_cache = vec![LineSlot::Invalid; 10];

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('V'), KeyModifiers::NONE)));

    assert_eq!(app.mode, Mode::VisualLine);
    assert_eq!(app.visual_anchor, Some((7, 0)));
    assert_eq!((app.backend.cursor_line, app.backend.cursor_col), (7, 0));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn vlf_chunks_backend_event_parsed() {
    let params = json!({
        "view_id": "view-1",
        "generation": 42,
        "line_start": 10,
        "lines": ["hello", "world"],
        "syntax_spans": [[{ "start_byte": 0, "end_byte": 5, "scope": "keyword.control" }], []],
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
            syntax_spans,
            approximate_line_count,
            line_count_exact,
            index_progress,
        } => {
            assert_eq!(view_id, "view-1");
            assert_eq!(generation, 42);
            assert_eq!(line_start, 10);
            assert_eq!(lines, vec!["hello", "world"]);
            assert_eq!(syntax_spans.len(), 2);
            assert_eq!(syntax_spans[0][0].scope, "keyword.control");
            assert_eq!(approximate_line_count, 500);
            assert!(!line_count_exact);
            assert!((index_progress - 0.42).abs() < 1e-9);
        }
        other => panic!("expected VlfChunks, got {:?}", other),
    }
}

#[test]
fn coalesce_backend_events_keeps_latest_noisy_view_events() {
    let events = vec![
        BackendEvent::VlfSearchStatus {
            view_id: String::from("view-1"),
            query: String::from("needle"),
            scanned_bytes: 10,
            total_bytes: 100,
            complete: false,
            stored_match_count: 1,
            ranges: Vec::new(),
        },
        BackendEvent::VlfChunks {
            view_id: String::from("view-1"),
            generation: 1,
            line_start: 0,
            lines: vec![String::from("old")],
            syntax_spans: Vec::new(),
            approximate_line_count: 10,
            line_count_exact: false,
            index_progress: 0.1,
        },
        BackendEvent::VlfSearchStatus {
            view_id: String::from("view-1"),
            query: String::from("needle"),
            scanned_bytes: 100,
            total_bytes: 100,
            complete: true,
            stored_match_count: 4,
            ranges: Vec::new(),
        },
        BackendEvent::VlfChunks {
            view_id: String::from("view-1"),
            generation: 2,
            line_start: 5,
            lines: vec![String::from("new")],
            syntax_spans: Vec::new(),
            approximate_line_count: 10,
            line_count_exact: false,
            index_progress: 0.2,
        },
    ];

    let coalesced = coalesce_backend_events(events);

    assert_eq!(coalesced.len(), 2);
    match &coalesced[0] {
        BackendEvent::VlfSearchStatus { complete, scanned_bytes, .. } => {
            assert!(*complete);
            assert_eq!(*scanned_bytes, 100);
        }
        other => panic!("expected latest search status, got {other:?}"),
    }
    match &coalesced[1] {
        BackendEvent::VlfChunks { generation, line_start, lines, .. } => {
            assert_eq!(*generation, 2);
            assert_eq!(*line_start, 5);
            assert_eq!(lines, &vec![String::from("new")]);
        }
        other => panic!("expected latest vlf chunks, got {other:?}"),
    }
}

#[test]
fn vlf_search_status_backend_event_parsed() {
    let params = json!({
        "view_id": "view-1",
        "query": "needle",
        "scanned_bytes": 1024,
        "total_bytes": 4096,
        "complete": false,
        "stored_match_count": 2,
        "ranges": [
            { "line": 3, "start_col": 2, "end_col": 8 },
            { "line": 7, "start_col": 0, "end_col": 6 }
        ]
    });
    let event =
        parse_notification("vlf_search_status", params).expect("should parse vlf search status");
    match event {
        BackendEvent::VlfSearchStatus {
            view_id,
            query,
            scanned_bytes,
            total_bytes,
            complete,
            stored_match_count,
            ranges,
        } => {
            assert_eq!(view_id, "view-1");
            assert_eq!(query, "needle");
            assert_eq!(scanned_bytes, 1024);
            assert_eq!(total_bytes, 4096);
            assert!(!complete);
            assert_eq!(stored_match_count, 2);
            assert_eq!(ranges.len(), 2);
            assert_eq!(ranges[0].line, 3);
            assert_eq!(ranges[0].start_col, 2);
            assert_eq!(ranges[0].end_col, 8);
        }
        other => panic!("expected VlfSearchStatus, got {:?}", other),
    }
}

// ── Regression counters/tests: no full line-cache clone on hot paths ──────────

#[test]
fn source_control_skips_constrained_sized_buffers() {
    // Buffers with more than CONSTRAINED_GIT_REFRESH_MAX_LINES (50_000) lines
    // must be skipped by the periodic background refresh to avoid an expensive
    // whole-buffer clone + diff on the UI thread.
    let (tx, _rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    let buf_id = app.backend.active().id;

    // Build a fully-cached line cache above the constrained threshold.
    let line_count = 50_001;
    app.backend.line_cache = (0..line_count)
        .map(|i| {
            LineSlot::Known(CachedLine {
                text: format!("line {i}"),
                cursors: Vec::new(),
                syntax_spans: Vec::new(),
            })
        })
        .collect();
    app.backend.rebuild_lines();
    assert_eq!(app.backend.lines.len(), line_count);
    assert!(app.backend.is_fully_cached());

    // No source-control entry yet — periodic refresh should still skip it.
    app.refresh_source_control();

    assert!(
        !app.source_control.contains_key(&buf_id),
        "background refresh must not clone or diff a constrained-sized buffer"
    );
}

#[test]
fn apply_update_large_cache_insert_does_not_clone_non_copy_range() {
    // Prove that a Copy op over a large prefix followed by an Insert only
    // allocates what is actually needed: the copy range and the new line.
    // The whole line_cache length must match the op total exactly.
    let large_line_count = 60_000usize;
    let mut state = test_buf_state();
    state.line_cache = (0..large_line_count)
        .map(|i| {
            LineSlot::Known(CachedLine {
                text: format!("existing {i}"),
                cursors: Vec::new(),
                syntax_spans: Vec::new(),
            })
        })
        .collect();
    state.rebuild_lines();

    state
        .apply_update(CoreUpdate {
            pristine: true,
            annotations: Vec::new(),
            ops: vec![
                // Copy entire existing cache — must not scan non-copy lines.
                CoreUpdateOp { op: CoreUpdateKind::Copy, n: large_line_count, lines: Vec::new() },
                // Append one new line.
                CoreUpdateOp {
                    op: CoreUpdateKind::Insert,
                    n: 1,
                    lines: vec![CoreLine {
                        text: Some(String::from("new-tail")),
                        cursor: Vec::new(),
                        syntax_spans: None,
                    }],
                },
            ],
        })
        .unwrap();

    assert_eq!(state.line_cache.len(), large_line_count + 1);
    // Existing lines must be preserved through the copy.
    match &state.line_cache[0] {
        LineSlot::Known(l) => assert_eq!(l.text, "existing 0"),
        other => panic!("expected known slot at 0, got {other:?}"),
    }
    // New line must appear at the tail.
    match &state.line_cache[large_line_count] {
        LineSlot::Known(l) => assert_eq!(l.text, "new-tail"),
        other => panic!("expected known slot at tail, got {other:?}"),
    }
    assert_eq!(state.lines.len(), large_line_count + 1);
    assert_eq!(state.lines[large_line_count], "new-tail");
}

#[test]
fn invalidate_op_large_count_does_not_allocate_text() {
    // Ensure that an Invalidate op for a huge line range produces Invalid
    // slots with no text allocation — the `lines` mirror gets empty strings,
    // but the slot type itself must be Invalid (no text cloned from previous).
    let mut state = test_buf_state();

    state
        .apply_update(CoreUpdate {
            pristine: true,
            annotations: Vec::new(),
            ops: vec![CoreUpdateOp {
                op: CoreUpdateKind::Invalidate,
                n: 100_000,
                lines: Vec::new(),
            }],
        })
        .unwrap();

    assert_eq!(state.line_cache.len(), 100_000);
    assert!(
        state.line_cache.iter().all(|s| matches!(s, LineSlot::Invalid)),
        "all slots from Invalidate op must be Invalid"
    );
    // The lines mirror has empty strings for invalid slots — no content.
    assert!(state.lines.iter().all(|s| s.is_empty()));
}

#[test]
fn bounded_invalid_range_scan_stops_at_window_boundary() {
    // invalid_line_ranges_bounded must not iterate outside [start, end).
    // This is the primitive that keeps scroll from scanning the full cache.
    let cache_size = 10_000usize;
    let viewport_start = 4_000usize;
    let viewport_end = 4_050usize;

    let mut cache = vec![LineSlot::Invalid; cache_size];
    // Mark all lines outside the viewport as Known — they must never appear
    // in the returned ranges.
    for slot in &mut cache[..viewport_start] {
        *slot = LineSlot::Known(CachedLine {
            text: String::from("before"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        });
    }
    for slot in &mut cache[viewport_end..] {
        *slot = LineSlot::Known(CachedLine {
            text: String::from("after"),
            cursors: Vec::new(),
            syntax_spans: Vec::new(),
        });
    }

    let ranges = invalid_line_ranges_bounded(&cache, viewport_start, viewport_end);

    // The entire viewport window is invalid, so exactly one range covers it.
    assert_eq!(ranges, vec![(viewport_start, viewport_end)]);
    // No range must extend outside the requested window.
    for (start, end) in &ranges {
        assert!(*start >= viewport_start, "range started before viewport");
        assert!(*end <= viewport_end, "range extended past viewport");
    }
}

#[test]
fn normal_render_large_line_cache_only_displays_viewport_rows() {
    // Prove that rendering a buffer with a large line cache only shows lines
    // that fit in the terminal height — the render path must not expand all
    // Invalid slots or panic on a huge cache.
    let total_lines = 10_000usize;
    let width: u16 = 80;
    let height: u16 = 10;

    let (tx, _rx) = mpsc::channel();
    let (_backend_tx, backend_rx) = mpsc::channel();
    let mut app = App::from_path(None).unwrap();
    app.backend = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));

    // First line is Known so the render path sees at least one valid row.
    let mut cache: Vec<LineSlot> = vec![LineSlot::Known(CachedLine {
        text: String::from("first line"),
        cursors: vec![0],
        syntax_spans: Vec::new(),
    })];
    cache.extend(std::iter::repeat_n(LineSlot::Invalid, total_lines - 1));
    app.backend.line_cache = cache;
    app.backend.rebuild_lines();

    // Rendering must complete without panic even though most slots are Invalid.
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| crate::ui::ui(frame, &app)).unwrap();

    let buf = terminal.backend().buffer();
    // The first row must contain the known line text.
    let first_row: String = (0..width).map(|x| buf.cell((x, 0)).unwrap().symbol()).collect();
    assert!(
        first_row.contains("first line"),
        "first row should render the known line, got: {first_row:?}"
    );
}
