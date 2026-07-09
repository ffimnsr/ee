use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use clap::{CommandFactory, Parser};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};
use xi_core_lib::runtime_loader::{
    RuntimeGrammarHealth, RuntimeHealthReport, RuntimeInjectionMatch,
    RuntimeLanguageDetectionSource, RuntimeQueryHealth, RuntimeQueryHealthReport, RuntimeQueryKind,
    RuntimeRoots,
};

use crate::app::{App, Mode};
use crate::backend::BackendEvent;
use crate::buffer::BufferManager;
use crate::picker::PickerKind;
use crate::tests::helpers::*;

#[test]
fn cli_restore_session_flag_parses() {
    let cli = crate::Cli::try_parse_from(["ee", "--restore-session"]).unwrap();

    assert!(cli.restore_session);
    assert!(cli.files.is_empty());
    assert!(cli.command.is_none());
}

#[test]
fn cli_utility_commands_live_under_do() {
    let cli = crate::Cli::try_parse_from(["ee", "do", "doctor"]).unwrap();

    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do { command: crate::DoCommands::Doctor })
    ));

    let cli = crate::Cli::try_parse_from(["ee", "do", "config", "show"]).unwrap();

    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do {
            command: crate::DoCommands::Config { command: crate::ConfigCommands::Show }
        })
    ));

    let cli = crate::Cli::try_parse_from(["ee", "do", "config", "get", "--global", "wrap_lines"])
        .unwrap();

    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do {
            command: crate::DoCommands::Config { command: crate::ConfigCommands::Get { .. } }
        })
    ));

    let cli = crate::Cli::try_parse_from([
        "ee",
        "do",
        "config",
        "set",
        "--local",
        "lsp.servers.rust.command",
        "rust-analyzer",
    ])
    .unwrap();

    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do {
            command: crate::DoCommands::Config { command: crate::ConfigCommands::Set { .. } }
        })
    ));

    let cli = crate::Cli::try_parse_from(["ee", "do", "plugins", "list"]).unwrap();

    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do {
            command: crate::DoCommands::Plugins { command: crate::PluginCommands::List }
        })
    ));

    let cli = crate::Cli::try_parse_from(["ee", "do", "plugins", "ls"]).unwrap();

    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do {
            command: crate::DoCommands::Plugins { command: crate::PluginCommands::List }
        })
    ));

    let cli =
        crate::Cli::try_parse_from(["ee", "do", "language", "list", "--dir", "sample"]).unwrap();

    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do {
            command: crate::DoCommands::Language { command: crate::LanguageCommands::List { .. } }
        })
    ));

    let cli = crate::Cli::try_parse_from([
        "ee",
        "do",
        "runtime",
        "--file",
        "sample.rs",
        "--language",
        "rust",
    ])
    .unwrap();

    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do { command: crate::DoCommands::Runtime { command: None, .. } })
    ));

    let cli = crate::Cli::try_parse_from([
        "ee",
        "do",
        "runtime",
        "fetch",
        "--all",
        "--source-root",
        "target/runtime-sources",
    ])
    .unwrap();

    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do {
            command: crate::DoCommands::Runtime {
                command: Some(crate::RuntimeCommands::Fetch {
                    all: true,
                    trust_workspace: false,
                    ..
                }),
                ..
            }
        })
    ));

    let cli =
        crate::Cli::try_parse_from(["ee", "do", "runtime", "fetch", "--all", "--trust-workspace"])
            .unwrap();

    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do {
            command: crate::DoCommands::Runtime {
                command: Some(crate::RuntimeCommands::Fetch {
                    all: true,
                    trust_workspace: true,
                    ..
                }),
                ..
            }
        })
    ));

    let cli = crate::Cli::try_parse_from([
        "ee",
        "do",
        "runtime",
        "build",
        "--language",
        "rust",
        "--output-root",
        "target/runtime",
        "--skip-load",
    ])
    .unwrap();

    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do {
            command: crate::DoCommands::Runtime {
                command: Some(crate::RuntimeCommands::Build {
                    skip_load: true,
                    trust_workspace: false,
                    ..
                }),
                ..
            }
        })
    ));

    let cli = crate::Cli::try_parse_from([
        "ee",
        "do",
        "runtime",
        "build",
        "--language",
        "rust",
        "--trust-workspace",
    ])
    .unwrap();

    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do {
            command: crate::DoCommands::Runtime {
                command: Some(crate::RuntimeCommands::Build { trust_workspace: true, .. }),
                ..
            }
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
fn cli_long_version_includes_build_metadata() {
    let command = crate::Cli::command();
    let long_version = command.get_long_version().expect("long version missing");

    assert!(long_version.starts_with(env!("CARGO_PKG_VERSION")));
    assert!(long_version.contains("git "));
    assert!(long_version.contains("commit "));
    assert!(long_version.contains("built "));
    assert!(long_version.contains("profile "));
    assert!(long_version.contains("rustc "));
}

#[test]
fn cli_allows_utility_names_as_file_paths() {
    let cli = crate::Cli::try_parse_from(["ee", "doctor", "validate", "completions"]).unwrap();

    assert!(cli.command.is_none());
    assert_eq!(cli.files, ["doctor", "validate", "completions"].map(PathBuf::from));
}

#[test]
fn discover_log_paths_lists_editor_and_plugin_candidates_for_doctor() {
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    let temp = tempfile::tempdir().unwrap();
    let state_home = temp.path().join("state-home");
    let editor_override = temp.path().join("custom-editor.log");
    let plugin_override = temp.path().join("custom-plugin.log");
    env::set_current_dir(temp.path()).unwrap();
    let _editor_guard = EnvVarGuard::set("EE_EDITOR_LOG", &editor_override);
    let _plugin_guard = EnvVarGuard::set("EE_PLUGIN_LOG", &plugin_override);
    let _state_guard = EnvVarGuard::set("XDG_STATE_HOME", &state_home);

    let paths = crate::logs::discover_log_paths()
        .into_iter()
        .map(|candidate| (candidate.label, candidate.path))
        .collect::<Vec<_>>();

    assert!(paths.contains(&("editor", editor_override.clone())));
    assert!(paths.contains(&("plugin", plugin_override.clone())));
    assert!(paths.contains(&("editor", temp.path().join("ee.log"))));
    assert!(paths.contains(&("editor", temp.path().join("editor.log"))));
    assert!(paths.contains(&("plugin", temp.path().join("xi-lsp-plugin.log"))));
    assert!(paths.contains(&("editor", state_home.join("ee").join("editor.log"))));
    assert!(paths.contains(&("plugin", state_home.join("ee").join("xi-lsp-plugin.log"))));
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
        requested_language: Some(String::from("C++")),
        requested_injection_language: None,
        file_path: Some(PathBuf::from("sample.rs")),
        detection_source: Some(RuntimeLanguageDetectionSource::Explicit),
        language_id: Some(String::from("C++")),
        display_name: Some(String::from("C++")),
        injection_match: None,
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
    assert!(rendered.contains("requested language: cpp"));
    assert!(rendered.contains("resolved language: cpp [cpp] via explicit"));
    assert!(rendered.contains("grammar: loaded"));
    assert!(rendered.contains("highlights  loaded"));
    assert!(rendered.contains("indents     missing"));
    assert!(rendered.contains("effective runtime root: /runtime"));
}

#[test]
fn runtime_report_exit_code_classifies_runtime_failures() {
    let mut healthy = RuntimeHealthReport {
        requested_language: Some(String::from("rust")),
        requested_injection_language: None,
        file_path: None,
        detection_source: Some(RuntimeLanguageDetectionSource::Explicit),
        language_id: Some(String::from("rust")),
        display_name: Some(String::from("rust")),
        injection_match: None,
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

    healthy.language_id = Some(String::from("rust"));
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
fn runtime_report_renders_injection_resolution() {
    let report = RuntimeHealthReport {
        requested_language: None,
        requested_injection_language: Some(String::from("tsx")),
        file_path: None,
        detection_source: None,
        language_id: Some(String::from("C#")),
        display_name: Some(String::from("C#")),
        injection_match: Some(RuntimeInjectionMatch {
            canonical_id: String::from("C#"),
            display_name: String::from("C#"),
        }),
        asset_source: None,
        effective_runtime_root: None,
        grammar_path: None,
        grammar_status: RuntimeGrammarHealth::Loaded,
        query_reports: Vec::new(),
        runtime_roots: RuntimeRoots::new("/bundle", "/user/ee", None),
    };

    let rendered = crate::render_runtime_report(&report);
    assert!(rendered.contains("requested injection language: tsx"));
    assert!(rendered.contains("injection language: csharp [csharp]"));
}

#[test]
fn runtime_languages_report_renders_effective_rows() {
    let row = crate::EffectiveRuntimeLanguageRow {
        canonical_id: String::from("C#"),
        display_name: String::from("C#"),
        asset_source: String::from("Bundled"),
        fetch_status: String::from("bundled"),
        grammar_status: String::from("loaded"),
        query_status: String::from("loaded 4, missing 0, unsupported 4, errors 0"),
        file_types: vec![String::from("rs")],
        globs: vec![String::from("*.rs.in")],
        shebangs: Vec::new(),
        query_language: String::from("C++"),
        scope: Some(String::from("source.rust")),
        grammar_library: Some(String::from("tree-sitter-rust")),
        grammar_symbol: Some(String::from("tree_sitter_rust")),
        grammar_source: Some(String::from("crate tree-sitter-rust@0.23.0")),
        injection_regex: Some(String::from("^(rust|rs)$")),
        match_priority: 10,
    };

    let runtime_rendered = crate::render_runtime_languages_report(
        "ee do runtime languages",
        std::slice::from_ref(&row),
    );
    assert!(runtime_rendered.contains("ee do runtime languages"));
    assert!(runtime_rendered.contains("csharp [csharp]"));
    assert!(runtime_rendered.contains("fetch status: bundled"));
    assert!(runtime_rendered.contains("grammar status: loaded"));
    assert!(runtime_rendered.contains("queries: loaded 4, missing 0, unsupported 4, errors 0"));
    assert!(runtime_rendered.contains("query language: cpp"));
    assert!(runtime_rendered.contains("grammar source: crate tree-sitter-rust@0.23.0"));
    assert!(runtime_rendered.contains("injection regex: ^(rust|rs)$"));

    let language_rendered = crate::render_runtime_languages_report("ee do language list", &[row]);
    assert!(language_rendered.contains("ee do language list"));
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
    wait_until_with_backend(
        &mut app.backend,
        "terminal transcript body",
        Duration::from_secs(2),
        |backend| backend.lines.iter().any(|line| line.contains("hello-from-shell")),
    );

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
fn write_non_permission_error_does_not_enter_privilege_confirm() {
    let mut app = App::from_path(None).unwrap();
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE)));
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    assert_eq!(app.mode, Mode::Normal);
    assert!(app.privileged_save_pending.is_none());
    assert!(
        app.backend
            .status_message
            .as_deref()
            .unwrap_or_default()
            .contains("scratch buffer has no path")
    );
}

#[test]
fn save_buffer_returns_permission_denied_when_save_result_reports_permission_error() {
    let path = unique_temp_path("ee-cli-save-permission-denied");
    fs::write(&path, "seed\n").unwrap();

    let (tx, rx) = mpsc::channel();
    let (backend_tx, backend_rx) = mpsc::channel();
    let mut client = BufferManager::test_new(tx, backend_rx, String::from("view-id-1"));
    let pending = client.pending_requests_for_test();
    let buf_id = client.active().id;
    client.set_buffer_path(buf_id, path.clone()).unwrap();

    let save_thread = thread::spawn(move || client.save_buffer(buf_id));
    let mut save_seen = false;
    let deadline = Instant::now() + Duration::from_secs(2);

    while Instant::now() < deadline {
        if save_seen && save_thread.is_finished() {
            break;
        }

        let raw = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(raw) => raw,
            Err(_) => continue,
        };
        let message: Value = serde_json::from_str(&raw).unwrap();
        let method = message.get("method").and_then(Value::as_str).unwrap_or_default();
        if let Some(id) = message.get("id").and_then(Value::as_u64) {
            let response = match method {
                "selections_preview" => Some(json!({ "jsonrpc": "2.0", "id": id, "result": null })),
                "save_status" => Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "generation": 1, "complete": true }
                })),
                _ => None,
            };
            if let Some(response) = response {
                let sender = pending.lock().unwrap().remove(&id).expect("pending rpc sender");
                sender.send(response).unwrap();
            }
            continue;
        }

        if method == "save" {
            save_seen = true;
            backend_tx
                .send(BackendEvent::SaveProgress {
                    view_id: String::from("view-id-1"),
                    complete: true,
                    generation: 1,
                })
                .unwrap();
            backend_tx
                .send(BackendEvent::SaveResult {
                    view_id: String::from("view-id-1"),
                    generation: 1,
                    success: false,
                    permission_denied: true,
                    message: Some(String::from("permission denied")),
                })
                .unwrap();
        }
    }

    assert!(save_seen, "save notification not observed");
    assert!(save_thread.is_finished(), "save thread did not finish");
    let err = save_thread.join().unwrap().expect_err("save should fail");
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(err.to_string().contains("permission denied"));

    fs::remove_file(&path).unwrap();
}

#[test]
fn edit_config_command_opens_nearest_workspace_config() {
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    let src = project.join("src");
    fs::create_dir_all(&src).unwrap();
    let config_path = project.join(".ee.toml");
    let file_path = src.join("main.rs");
    fs::write(&config_path, "cursor_line = true\n").unwrap();
    fs::write(&file_path, "fn main() {}\n").unwrap();
    env::set_current_dir(&src).unwrap();

    let mut app = App::from_path(Some(file_path)).unwrap();

    run_ex(&mut app, "edit_config");

    assert_eq!(app.backend.active().path.as_deref(), Some(config_path.as_path()));
    assert!(app.picker.is_none());
}

#[test]
fn resolve_startup_launch_without_files_opens_no_file() {
    let launch = crate::resolve_startup_launch(&[]).unwrap();
    let (app, additional) = crate::build_startup_app(launch).unwrap();

    assert!(additional.is_empty());
    assert!(app.backend.active().path.is_none());
    assert!(app.picker.is_none());
}

#[test]
fn resolve_startup_launch_for_dot_opens_picker_in_current_directory() {
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _cwd_guard = CurrentDirGuard::capture();
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("sample.rs"), "fn main() {}\n").unwrap();
    env::set_current_dir(temp.path()).unwrap();

    let launch = crate::resolve_startup_launch(&[PathBuf::from(".")]).unwrap();
    let (app, additional) = crate::build_startup_app(launch).unwrap();

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

    let launch = crate::resolve_startup_launch(std::slice::from_ref(&nested)).unwrap();
    let (app, additional) = crate::build_startup_app(launch).unwrap();

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
