use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::{CommandFactory, Parser, Subcommand};
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
use xi_core_lib::vlf::store::VlfStore;

mod app;
mod backend;
mod buffer;
mod config;
mod folds;
mod git;
mod highlight;
mod keymap;
mod picker;
mod quickfix;
mod registers;
mod render_metrics;
mod session;
mod terminal;
mod text;
mod ui;
mod window;

#[cfg(test)]
mod tests;

use app::App;
use ui::ui;

const INPUT_POLL_TIMEOUT: Duration = Duration::from_millis(16);
const MAX_INPUT_EVENTS_PER_TICK: usize = 128;

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

#[derive(Debug, Clone)]
struct StartupLaunch {
    initial_path: Option<PathBuf>,
    additional_paths: Vec<PathBuf>,
    picker_root: Option<PathBuf>,
}

// ── CLI definition ────────────────────────────────────────────────────────────

#[derive(Debug, Parser)]
#[command(
    name = "ee",
    version,
    about = "A terminal editor",
    long_about = None,
)]
struct Cli {
    /// Files to open (multiple allowed)
    #[arg(value_name = "FILE")]
    files: Vec<PathBuf>,

    /// Load a specific config file instead of layered defaults
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Change the working directory before opening files
    #[arg(short = 'w', long, value_name = "DIR")]
    working_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Run editor utility commands
    Do {
        #[command(subcommand)]
        command: DoCommands,
    },
}

#[derive(Debug, Subcommand)]
enum DoCommands {
    /// Check for problems and show config search precedence
    Doctor,
    /// Run file utility commands
    File {
        #[command(subcommand)]
        command: FileCommands,
    },
    /// Validate config file syntax and values
    Validate {
        /// Config file to validate
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
    },
    /// Generate shell completion script
    Completions {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: Shell,
    },
}

#[derive(Debug, Subcommand)]
enum FileCommands {
    /// Count line-feed bytes in a file like `wc -l`
    LineCheck {
        /// File to inspect
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },
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
        original(info);
    }));
}

// ── Subcommand handlers ───────────────────────────────────────────────────────

fn cmd_doctor(config_path: Option<&PathBuf>) {
    println!("ee do doctor");
    println!("─────────");

    if let Some(explicit) = config_path {
        let status = if explicit.exists() { "found" } else { "not found" };
        println!("  --config {explicit:?}  [{status}]");
    } else {
        let report = config::config_search_report(None);
        println!("  anchor {:?}", report.anchor);
        println!("  layers (low -> high)");
        for layer in report.layers {
            let status = if layer.loaded {
                "loaded"
            } else if layer.exists {
                "skipped"
            } else {
                "not found"
            };
            print!("  {:?}  [{}] [{}]", layer.path, layer.kind.label(), status);
            if let Some(root) = layer.root {
                print!(" [root={root}]");
            }
            if let Some(note) = layer.note {
                print!(" {note}");
            }
            println!();
        }
        if !report.editorconfig_applies {
            println!("  .editorconfig  [file-specific] [not evaluated without file path]");
        }
    }

    println!();
    println!("No problems detected.");
}

fn cmd_validate(config_path: Option<&PathBuf>) {
    let paths = if let Some(path) = config_path.cloned() {
        vec![path]
    } else {
        config::default_config_layers(None).into_iter().map(|layer| layer.path).collect::<Vec<_>>()
    };

    if paths.is_empty() {
        eprintln!("No config files found in layered default search path.");
        std::process::exit(1);
    }

    for path in paths {
        if !path.exists() {
            eprintln!("Config file not found: {path:?}");
            std::process::exit(1);
        }
        if let Err(err) = config::validate_config_file(&path) {
            eprintln!("{err}");
            std::process::exit(1);
        }
        println!("Config {path:?} is valid.");
    }
}

fn cmd_completions(shell: Shell) {
    let mut cmd = Cli::command();
    generate(shell, &mut cmd, "ee", &mut io::stdout());
}

fn count_file_line_feeds(path: &Path) -> io::Result<u64> {
    VlfStore::open(path)?.count_lf_streaming()
}

fn cmd_file_line_check(path: &Path) {
    match count_file_line_feeds(path) {
        Ok(count) => println!("{count} {}", path.display()),
        Err(err) => {
            eprintln!("Cannot count lines in {}: {err}", path.display());
            std::process::exit(1);
        }
    }
}

fn resolve_startup_launch(
    files: &[PathBuf],
    saved_session: Option<&session::SessionState>,
) -> io::Result<StartupLaunch> {
    let Some(first) = files.first().cloned() else {
        return Ok(StartupLaunch {
            initial_path: saved_session.and_then(session::SessionState::initial_path),
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

    // Handle subcommands that don't launch the editor.
    match cli.command {
        Some(Commands::Do { command }) => {
            match command {
                DoCommands::Doctor => cmd_doctor(cli.config.as_ref()),
                DoCommands::File { command } => match command {
                    FileCommands::LineCheck { file } => cmd_file_line_check(&file),
                },
                DoCommands::Validate { config } => {
                    let config_path = config.as_ref().or(cli.config.as_ref());
                    cmd_validate(config_path);
                }
                DoCommands::Completions { shell } => cmd_completions(shell),
            }
            return Ok(());
        }
        None => {}
    }

    // Apply --working-dir before opening files.
    if let Some(ref dir) = cli.working_dir {
        std::env::set_current_dir(dir).map_err(|e| {
            io::Error::new(e.kind(), format!("cannot change directory to {dir:?}: {e}"))
        })?;
    }

    install_panic_hook();

    // Atomic flag set by SIGTERM and SIGINT handlers so the main loop can
    // exit cleanly instead of being killed mid-draw.
    let shutdown = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&shutdown))
        .map_err(io::Error::other)?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&shutdown))
        .map_err(io::Error::other)?;

    let saved_session = if cli.files.is_empty() {
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
    let launch = resolve_startup_launch(&cli.files, saved_session.as_ref())?;
    let (mut app, additional_paths) = build_startup_app(launch)?;

    if let Some(state) = saved_session.as_ref()
        && let Err(err) = state.restore(&mut app)
    {
        eprintln!("ee: warning: failed to restore session: {err}");
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
    }

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;

    result
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    shutdown: Arc<AtomicBool>,
) -> io::Result<()> {
    while !app.should_quit && !shutdown.load(Ordering::Relaxed) {
        app.backend.drain_events()?;
        app.handle_pending_ui_actions();
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

        // Apply backend responses from just-handled input before drawing, after
        // dropping stale repeated arrow motion from the same input tick.
        app.backend.drain_events()?;

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
        app.backend.notify_scroll(app.viewport.top_line, app.viewport.top_line + editor_height)?;

        terminal.draw(|frame| ui(frame, app))?;
        app.render_metrics.record_render();
        if app.startup_deferred_work_pending {
            app.startup_deferred_work_pending = false;
            app.refresh_source_control();
        }
    }
    Ok(())
}
