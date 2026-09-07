use std::fmt::Write as _;
#[cfg(feature = "agents")]
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal as _, Read, Stdout, Write};
use std::path::{Path, PathBuf};
#[cfg(feature = "agents")]
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{Shell, generate};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use xi_core_lib::runtime_loader::{
    RuntimeGrammarHealth, RuntimeGrammarSource, RuntimeHealthReport,
    RuntimeLanguageDetectionSource, RuntimeOperationError, RuntimeOperationErrorKind,
    RuntimeQueryHealth, RuntimeQueryKind, with_default_runtime_loader_mut,
};
use xi_core_lib::text_store::{ByteRange, TextChunkResult, TextStore};
use xi_core_lib::vlf::store::VlfStore;
use xi_core_lib::{plugin_manifest, plugins::PluginCatalog};

#[cfg(feature = "agents")]
mod agent_registry;
#[cfg(feature = "agents")]
mod agent_setup;
mod app;
mod backend;
mod buffer;
mod config;
mod folds;
mod git;
mod highlight;
mod keymap;
mod logs;
// Host-local workspace trust policy (Phase 1 foundation); CLI surface for
// persistent grants arrives in later phases, so module items stay unused
// until then.
mod picker;
#[allow(dead_code)]
mod policy;
mod quickfix;
mod registers;
mod render_metrics;
// Secrets-store contract (ISSUES.md "Host-Bound Encrypted Secrets Store").
// Phase 1 ships the typed contract only; the CLI surface arrives in phase 4,
// so module items stay unused until then.
#[allow(dead_code)]
mod secrets;
mod session;
mod terminal;
mod text;
mod theme;
mod ui;
mod vlf_viewport;
mod window;

#[cfg(test)]
mod tests;

use app::App;
use ui::ui;

const INPUT_POLL_TIMEOUT: Duration = Duration::from_millis(16);
const MAX_INPUT_EVENTS_PER_TICK: usize = 128;
const FILE_PREVIEW_CHUNK_BYTES: u64 = 256 * 1024;
const RUNTIME_REPORT_READ_BYTES: u64 = 8 * 1024;
const EXIT_RUNTIME_CONFIG_MERGE: i32 = 2;
const EXIT_RUNTIME_GRAMMAR_SOURCE: i32 = 3;
const EXIT_RUNTIME_ASSET: i32 = 4;
const LONG_VERSION: &str = env!("EE_LONG_VERSION");

fn terminal_runtime_language_name(language: &str) -> String {
    match language.trim().to_ascii_lowercase().as_str() {
        "c#" => String::from("csharp"),
        "c++" => String::from("cpp"),
        normalized => normalized.to_string(),
    }
}

fn is_repeated_arrow_motion(event: &Event) -> bool {
    let Event::Key(key) = event else { return false };
    key.kind == KeyEventKind::Repeat && is_arrow_motion_key(key)
}

fn is_arrow_motion_key(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right)
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
}

fn coalesce_input_events(events: Vec<Event>) -> Vec<Event> {
    if !events.iter().any(is_repeated_arrow_motion) {
        return events;
    }

    let last_arrow_motion = events
        .iter()
        .rposition(|event| matches!(event, Event::Key(key) if is_arrow_motion_key(key)));

    events
        .into_iter()
        .enumerate()
        .filter_map(|(idx, event)| {
            let is_stale_arrow = matches!(&event, Event::Key(key) if is_arrow_motion_key(key))
                && Some(idx) != last_arrow_motion;
            (!is_stale_arrow).then_some(event)
        })
        .collect()
}
mod args;
mod config_cmd;
mod doctor;
mod file_cmd;
mod language_cmd;
mod plugins_cmd;
mod runtime_cmd;
mod schema_cmd;
mod secrets_cmd;
mod trust_cmd;

use args::{
    AgentCommands, AgentTrustCommands, Cli, Commands, ConfigCommands, ConfigSetupCommands,
    DoCommands, FileCommands, LanguageCommands, PluginCommands, RuntimeCommands, SchemaCommands,
};
use config_cmd::{
    cmd_config_get, cmd_config_init, cmd_config_set, cmd_config_setup_agent, cmd_config_show,
};
use doctor::cmd_doctor;
use file_cmd::{cmd_file_head, cmd_file_line_check, cmd_file_tail};
use language_cmd::cmd_language_list;
use plugins_cmd::cmd_plugins_list;
use runtime_cmd::{cmd_runtime, cmd_runtime_build, cmd_runtime_fetch, cmd_runtime_languages};
use schema_cmd::{cmd_completions, cmd_schema_check, cmd_schema_generate, cmd_validate};
use secrets_cmd::cmd_secrets;
use trust_cmd::{cmd_agent_trust_grant, cmd_agent_trust_revoke};

#[cfg(test)]
pub(crate) use args::{ALL_AGENT_TRUST_PROFILES, AgentTrustProfile, SecretsCommands};
#[cfg(test)]
pub(crate) use doctor::doctor_report;
#[cfg(test)]
pub(crate) use file_cmd::{count_file_line_feeds, read_file_head, read_file_tail};
#[cfg(test)]
pub(crate) use plugins_cmd::{plugin_activation_label, plugin_runtime_label};
#[cfg(test)]
pub(crate) use runtime_cmd::{
    EffectiveRuntimeLanguageRow, render_runtime_languages_report, render_runtime_report,
    runtime_report_exit_code,
};
#[cfg(test)]
pub(crate) use trust_cmd::{grant_agent_trust_profiles_at, revoke_agent_trust_profiles_at};

#[derive(Debug, Clone)]
struct StartupLaunch {
    initial_path: Option<PathBuf>,
    additional_paths: Vec<PathBuf>,
    picker_root: Option<PathBuf>,
}
// ── Panic hook ────────────────────────────────────────────────────────────────

/// Install a panic hook that restores the terminal to a sane state before
/// printing the panic message. Without this a panic in raw/alternate-screen
/// mode leaves the terminal unusable.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stderr(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = logs::append_editor_log_line(&format!("panic: {info}"));
        original(info);
    }));
}
fn resolve_startup_launch(files: &[PathBuf]) -> io::Result<StartupLaunch> {
    let Some(first) = files.first().cloned() else {
        return Ok(StartupLaunch {
            initial_path: None,
            additional_paths: Vec::new(),
            picker_root: None,
        });
    };

    if first.is_dir() {
        let picker_root = std::fs::canonicalize(&first)?;
        std::env::set_current_dir(&picker_root)?;
        return Ok(StartupLaunch {
            initial_path: None,
            additional_paths: files.iter().skip(1).cloned().collect(),
            picker_root: Some(picker_root),
        });
    }

    Ok(StartupLaunch {
        initial_path: Some(first),
        additional_paths: files.iter().skip(1).cloned().collect(),
        picker_root: None,
    })
}

fn build_startup_app(launch: StartupLaunch) -> io::Result<(App, Vec<PathBuf>)> {
    let mut app = App::from_path(launch.initial_path)?;
    if let Some(picker_root) = launch.picker_root {
        app.open_picker(picker::PickerState::new_files(picker_root));
    }
    Ok((app, launch.additional_paths))
}

// ── Editor entry point ────────────────────────────────────────────────────────

fn main() -> io::Result<()> {
    let cli = Cli::parse();
    let launch_agent_shell = matches!(
        &cli.command,
        Some(Commands::Do { command: DoCommands::Agent { command: AgentCommands::Shell } })
    );

    // Apply --working-dir before any file or utility command resolution.
    if let Some(ref dir) = cli.working_dir {
        std::env::set_current_dir(dir).map_err(|e| {
            io::Error::new(e.kind(), format!("cannot change directory to {dir:?}: {e}"))
        })?;
    }

    // Hidden proxy mode: `ee --mcp-proxy` speaks MCP 2026-07-28 over stdio
    // on behalf of the editor's proxy listener (agents feature).  The socket
    // path and token arrive through the environment set by the forwarded
    // `mcpServers` config.
    #[cfg(feature = "agents")]
    if cli.mcp_proxy {
        let socket = std::env::var("EE_MCP_PROXY_SOCKET").map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "EE_MCP_PROXY_SOCKET is not set")
        })?;
        let token = std::env::var("EE_MCP_PROXY_TOKEN").map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "EE_MCP_PROXY_TOKEN is not set")
        })?;
        return app::agents_mcp::run_proxy_stdio(PathBuf::from(socket), token);
    }

    // Handle subcommands that don't launch the editor.
    match cli.command {
        Some(Commands::Do { command }) => {
            match command {
                DoCommands::Doctor => cmd_doctor(cli.config.as_ref()),
                DoCommands::Config { command } => match command {
                    ConfigCommands::Setup { command: ConfigSetupCommands::Agent } => {
                        cmd_config_setup_agent()
                    }
                    ConfigCommands::Init { global } => cmd_config_init(if global {
                        config::ConfigScope::Global
                    } else {
                        config::ConfigScope::Local
                    }),
                    ConfigCommands::Show => cmd_config_show(),
                    ConfigCommands::Get { scope, key } => cmd_config_get(scope.scope(), &key),
                    ConfigCommands::Set { scope, key, value } => {
                        cmd_config_set(scope.scope(), &key, &value)
                    }
                },
                DoCommands::Agent { command: AgentCommands::Shell } => {}
                DoCommands::Agent {
                    command:
                        AgentCommands::Trust { command: AgentTrustCommands::Grant { profiles } },
                } => {
                    cmd_agent_trust_grant(&profiles)?;
                }
                DoCommands::Agent {
                    command:
                        AgentCommands::Trust { command: AgentTrustCommands::Revoke { profiles } },
                } => {
                    cmd_agent_trust_revoke(&profiles)?;
                }
                DoCommands::Plugins { command } => match command {
                    PluginCommands::List => cmd_plugins_list(),
                },
                DoCommands::Language { command } => match command {
                    LanguageCommands::List { dir, file } => {
                        cmd_language_list(dir.as_deref().or(file.as_deref()))
                    }
                },
                DoCommands::Runtime { file, language, injection_language, command } => {
                    match command {
                        None => cmd_runtime(
                            file.as_deref(),
                            language.as_deref(),
                            injection_language.as_deref(),
                        ),
                        Some(RuntimeCommands::Languages) => cmd_runtime_languages(file.as_deref()),
                        Some(RuntimeCommands::Fetch {
                            all,
                            languages,
                            source_root,
                            force,
                            trust_workspace,
                        }) => cmd_runtime_fetch(
                            &languages,
                            all,
                            source_root.as_deref(),
                            force,
                            trust_workspace,
                        ),
                        Some(RuntimeCommands::Build {
                            all,
                            languages,
                            source_root,
                            output_root,
                            force,
                            skip_load,
                            trust_workspace,
                        }) => cmd_runtime_build(
                            &languages,
                            all,
                            source_root.as_deref(),
                            output_root.as_deref(),
                            force,
                            skip_load,
                            trust_workspace,
                        ),
                    }
                }
                DoCommands::File { command } => match command {
                    FileCommands::LineCheck { file } => cmd_file_line_check(&file),
                    FileCommands::Head { lines, file } => cmd_file_head(&file, lines),
                    FileCommands::Tail { lines, file } => cmd_file_tail(&file, lines),
                },
                DoCommands::Validate { config } => {
                    let config_path = config.as_ref().or(cli.config.as_ref());
                    cmd_validate(config_path);
                }
                DoCommands::Schema { command } => match command {
                    SchemaCommands::Generate { output } => cmd_schema_generate(&output),
                    SchemaCommands::Check { schema } => cmd_schema_check(&schema),
                },
                DoCommands::Completions { shell } => cmd_completions(shell),
                DoCommands::Secrets { command } => cmd_secrets(command),
            }
            if !launch_agent_shell {
                return Ok(());
            }
        }
        None => {}
    }

    install_panic_hook();
    let _ = logs::append_editor_log_line("ee startup");

    // Atomic flag set by SIGTERM and SIGINT handlers so the main loop can
    // exit cleanly instead of being killed mid-draw.
    let shutdown = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&shutdown))
        .map_err(io::Error::other)?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&shutdown))
        .map_err(io::Error::other)?;

    let saved_session = if cli.restore_session && cli.files.is_empty() {
        match session::SessionState::load() {
            Ok(state) => state,
            Err(err) => {
                eprintln!("ee: warning: failed to load session: {err}");
                None
            }
        }
    } else {
        None
    };
    let launch = resolve_startup_launch(&cli.files)?;
    let (mut app, additional_paths) = build_startup_app(launch)?;

    if let Some(state) = saved_session.as_ref()
        && let Err(err) = state.restore(&mut app)
    {
        eprintln!("ee: warning: failed to restore session: {err}");
    }

    if launch_agent_shell {
        app.open_agents_shell();
    }

    // Open any additional files as extra buffers.
    for path in additional_paths {
        let _ = app.backend.open_buffer(Some(path));
    }

    run(&mut app, shutdown)
}

fn run(app: &mut App, shutdown: Arc<AtomicBool>) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    terminal.clear()?;

    let result = run_app(&mut terminal, app, shutdown);

    if let Err(err) = session::SessionState::save(app) {
        eprintln!("ee: warning: failed to save session: {err}");
        let _ = logs::append_editor_log_line(&format!("warning: failed to save session: {err}"));
    }

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;

    if let Err(err) = &result {
        let _ = logs::append_editor_log_line(&format!("io error: {err}"));
    } else {
        let _ = logs::append_editor_log_line("ee shutdown");
    }
    result
}

#[cfg(feature = "agents")]
fn external_editor_command() -> Result<String, String> {
    let value = std::env::var("VISUAL")
        .ok()
        .or_else(|| std::env::var("EDITOR").ok())
        .ok_or_else(|| String::from("set VISUAL or EDITOR to a single executable name"))?;
    let command = value.trim();
    if command.is_empty()
        || command.chars().any(char::is_whitespace)
        || command.contains([';', '|', '&', '>', '<', '`', '$'])
    {
        return Err(String::from(
            "VISUAL/EDITOR must be one executable name without shell arguments",
        ));
    }
    Ok(command.to_string())
}

#[cfg(feature = "agents")]
fn external_draft_path() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!("ee-agent-draft-{}-{nonce}.md", std::process::id()))
}

/// Runs user-configured external editor only while EE TUI has released its
/// foreground terminal. No shell is involved; return text remains local until
/// user explicitly submits composer.
#[cfg(feature = "agents")]
fn edit_agent_draft_externally(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    draft: &str,
) -> Result<String, String> {
    const MAX_DRAFT_EDITOR_BYTES: usize = 256 * 1024;
    if draft.len() > MAX_DRAFT_EDITOR_BYTES {
        return Err(format!(
            "draft exceeds external-editor limit ({MAX_DRAFT_EDITOR_BYTES} bytes)"
        ));
    }
    let command = external_editor_command()?;
    let path = external_draft_path();
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .map_err(|error| format!("cannot create private draft file: {error}"))?;
    if let Err(error) = file.write_all(draft.as_bytes()) {
        let _ = fs::remove_file(&path);
        return Err(format!("cannot write private draft file: {error}"));
    }
    if let Err(error) = file.sync_all() {
        let _ = fs::remove_file(&path);
        return Err(format!("cannot prepare private draft file: {error}"));
    }
    drop(file);

    let suspend = (|| -> io::Result<()> {
        terminal.show_cursor()?;
        disable_raw_mode()?;
        execute!(
            terminal.backend_mut(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        )?;
        Ok(())
    })();
    if let Err(error) = suspend {
        // Best-effort restore even when halfway through terminal suspension.
        let _ = execute!(
            terminal.backend_mut(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        );
        let _ = enable_raw_mode();
        let _ = terminal.clear();
        let _ = fs::remove_file(&path);
        return Err(format!("cannot release terminal for editor: {error}"));
    }

    let editor_result = Command::new(&command).arg(&path).status();
    let content_result = match &editor_result {
        Ok(status) if status.success() => fs::read(&path)
            .map_err(|error| format!("cannot read edited draft: {error}"))
            .and_then(|bytes| {
                if bytes.len() > MAX_DRAFT_EDITOR_BYTES {
                    Err(format!("edited draft exceeds limit ({MAX_DRAFT_EDITOR_BYTES} bytes)"))
                } else {
                    String::from_utf8(bytes)
                        .map_err(|_| String::from("edited draft is not valid UTF-8"))
                }
            }),
        Ok(status) => Err(format!("external editor exited with {status}")),
        Err(error) => Err(format!("cannot launch `{command}`: {error}")),
    };
    let _ = fs::remove_file(&path);

    let restore = (|| -> io::Result<()> {
        execute!(
            terminal.backend_mut(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        enable_raw_mode()?;
        terminal.clear()?;
        Ok(())
    })();
    if let Err(error) = restore {
        return Err(format!("external editor finished, but terminal restore failed: {error}"));
    }
    content_result
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    shutdown: Arc<AtomicBool>,
) -> io::Result<()> {
    while !app.should_quit && !shutdown.load(Ordering::Relaxed) {
        app.backend.drain_events()?;
        app.handle_pending_ui_actions();
        // Pump agents host events into the pane state (feature `agents`).
        #[cfg(feature = "agents")]
        app.pump_agents();
        app.expire_key_sequence_if_idle();
        // Dispatch pending location results (definition, references, …) to the
        // quickfix list before drawing so the panel opens in the same frame.
        app.handle_pending_locations();
        // Dispatch pending symbol results (document/workspace symbols) to picker.
        app.handle_pending_symbols();
        if !app.startup_deferred_work_pending && app.input_idle_for(Duration::from_millis(250)) {
            app.refresh_source_control();
        }
        // Periodically check for external file changes.
        app.backend.check_external_changes();
        // Warn the user when a backing file has been modified externally.
        for buf in app.backend.all_bufs() {
            if buf.externally_modified {
                let title = buf.title();
                app.backend.status_message = Some(format!(
                    "'{title}' changed on disk — use :e! to reload or continue editing"
                ));
                // Only show one warning per frame; the flag stays set until reload.
                break;
            }
        }
        // Write crash-recovery artifacts every ~30 s for modified buffers.
        app.write_recovery_if_due();

        if event::poll(INPUT_POLL_TIMEOUT)? {
            let mut events = Vec::new();
            loop {
                events.push(event::read()?);
                if app.should_quit
                    || shutdown.load(Ordering::Relaxed)
                    || events.len() >= MAX_INPUT_EVENTS_PER_TICK
                    || !event::poll(Duration::ZERO)?
                {
                    break;
                }
            }

            for event in coalesce_input_events(events) {
                match event {
                    // SIGWINCH arrives as Event::Resize from crossterm; force a
                    // full redraw by clearing the terminal buffer.
                    Event::Resize(_, _) => {
                        terminal.clear()?;
                    }
                    ev => app.handle_event(ev),
                }
                if app.should_quit || shutdown.load(Ordering::Relaxed) {
                    break;
                }
            }
        }

        // Interactive external editor must run in terminal-owning loop, after
        // all input dispatch but before next draw. It never submits prompt.
        #[cfg(feature = "agents")]
        if let Some(request) = app.take_agent_external_editor_request() {
            let result = edit_agent_draft_externally(terminal, &request.draft);
            app.apply_agent_external_editor_result(request, result);
            terminal.clear()?;
        }

        // Apply backend responses from just-handled input before drawing, after
        // dropping stale repeated arrow motion from the same input tick.
        app.backend.drain_events()?;
        app.sync_status_toast();

        if app.redraw_requested {
            terminal.clear()?;
            app.redraw_requested = false;
        }

        let size = terminal.size()?;
        let term_rect =
            ratatui::layout::Rect { x: 0, y: 0, width: size.width, height: size.height };
        let editor_height = ui::compute_editor_height(term_rect, app);
        let editor_width = ui::compute_editor_width(term_rect, app);
        app.scroll_into_view(editor_height, editor_width);
        let active = app.backend.active();
        let viewport_range = app.folds.line_range_for_rendered_rows(
            active.id,
            app.viewport.top_line,
            editor_height,
            active.line_count(),
        );
        app.backend.notify_scroll(viewport_range.0, viewport_range.1)?;

        terminal.draw(|frame| ui(frame, app))?;
        app.render_metrics.record_render();
        if app.startup_deferred_work_pending {
            app.startup_deferred_work_pending = false;
            app.refresh_source_control();
        }
    }

    // Phase 7 shutdown orchestration: cancel agent turns, resolve pending
    // approvals/elicitations, kill agent terminals, stop MCP servers and
    // agent subprocesses before the process exits.
    #[cfg(feature = "agents")]
    app.shutdown_agents();
    Ok(())
}
