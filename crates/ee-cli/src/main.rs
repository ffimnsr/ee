use std::io::{self, Read, Stdout, Write};
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
use xi_core_lib::runtime_loader::{
    RuntimeGrammarHealth, RuntimeHealthReport, RuntimeLanguageDetectionSource,
    RuntimeOperationError, RuntimeOperationErrorKind, RuntimeQueryHealth, RuntimeQueryKind,
    with_default_runtime_loader_mut,
};
use xi_core_lib::text_store::{ByteRange, TextChunkResult, TextStore};
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
mod theme;
mod ui;
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
    long_version = LONG_VERSION,
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
    /// Show runtime grammar and query resolution for a file or language
    Runtime {
        /// File to resolve through runtime language detection
        #[arg(long, value_name = "FILE")]
        file: Option<PathBuf>,
        /// Explicit language name to resolve before path/content detection
        #[arg(long, value_name = "LANGUAGE")]
        language: Option<String>,
        #[command(subcommand)]
        command: Option<RuntimeCommands>,
    },
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
    /// Generate or check repository config schema
    Schema {
        #[command(subcommand)]
        command: SchemaCommands,
    },
    /// Generate shell completion script
    Completions {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: Shell,
    },
}

#[derive(Debug, Subcommand)]
enum RuntimeCommands {
    /// Materialize pinned grammar sources from the cargo registry
    Fetch {
        /// Fetch all configured runtime grammars
        #[arg(long, action = clap::ArgAction::SetTrue)]
        all: bool,
        /// Fetch only selected runtime languages
        #[arg(long = "language", value_name = "LANGUAGE")]
        languages: Vec<String>,
        /// Directory used to stage grammar source trees
        #[arg(long, value_name = "DIR")]
        source_root: Option<PathBuf>,
        /// Replace any existing staged source trees
        #[arg(long, action = clap::ArgAction::SetTrue)]
        force: bool,
    },
    /// Build runtime grammar libraries and query assets
    Build {
        /// Build all configured runtime grammars
        #[arg(long, action = clap::ArgAction::SetTrue)]
        all: bool,
        /// Build only selected runtime languages
        #[arg(long = "language", value_name = "LANGUAGE")]
        languages: Vec<String>,
        /// Directory used to stage grammar source trees
        #[arg(long, value_name = "DIR")]
        source_root: Option<PathBuf>,
        /// Directory that will receive `grammars/` and `queries/`
        #[arg(long, value_name = "DIR")]
        output_root: Option<PathBuf>,
        /// Replace existing grammar libraries before rebuilding
        #[arg(long, action = clap::ArgAction::SetTrue)]
        force: bool,
        /// Skip host-side dynamic library load validation after compile
        #[arg(long, action = clap::ArgAction::SetTrue)]
        skip_load: bool,
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
    /// Print first lines of a file like `head`
    Head {
        /// Number of lines to print
        #[arg(short = 'n', long = "lines", default_value_t = 10, value_name = "LINES")]
        lines: usize,
        /// File to inspect
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },
    /// Print last lines of a file like `tail`
    Tail {
        /// Number of lines to print
        #[arg(short = 'n', long = "lines", default_value_t = 10, value_name = "LINES")]
        lines: usize,
        /// File to inspect
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum SchemaCommands {
    /// Write generated config JSON Schema to schemas/
    Generate {
        /// Schema output path
        #[arg(long, default_value = "schemas/ee-config.schema.json", value_name = "FILE")]
        output: PathBuf,
    },
    /// Fail when checked-in config schema differs from generated output
    Check {
        /// Schema path to compare against generated output
        #[arg(long, default_value = "schemas/ee-config.schema.json", value_name = "FILE")]
        schema: PathBuf,
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

fn cmd_schema_generate(output: &Path) {
    if let Err(err) = config::write_config_schema(output) {
        eprintln!("{err}");
        std::process::exit(1);
    }
    println!("Generated config schema: {}", output.display());
}

fn cmd_schema_check(schema: &Path) {
    if let Err(err) = config::check_config_schema(schema) {
        eprintln!("{err}");
        std::process::exit(1);
    }
    println!("Config schema is up to date: {}", schema.display());
}

fn cmd_completions(shell: Shell) {
    let mut cmd = Cli::command();
    generate(shell, &mut cmd, "ee", &mut io::stdout());
}

fn read_runtime_probe(path: &Path) -> io::Result<(Option<String>, Option<String>)> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(RUNTIME_REPORT_READ_BYTES).read_to_end(&mut bytes)?;
    let sample = String::from_utf8_lossy(&bytes).into_owned();
    let first_line = sample.lines().next().map(str::to_string);
    Ok((first_line, (!sample.is_empty()).then_some(sample)))
}

fn query_kind_label(kind: RuntimeQueryKind) -> &'static str {
    match kind {
        RuntimeQueryKind::Highlights => "highlights",
        RuntimeQueryKind::Injections => "injections",
        RuntimeQueryKind::Locals => "locals",
        RuntimeQueryKind::Tags => "tags",
        RuntimeQueryKind::Textobjects => "textobjects",
        RuntimeQueryKind::Indents => "indents",
        RuntimeQueryKind::Folds => "folds",
        RuntimeQueryKind::Rainbows => "rainbows",
    }
}

fn detection_source_label(source: RuntimeLanguageDetectionSource) -> &'static str {
    match source {
        RuntimeLanguageDetectionSource::Explicit => "explicit",
        RuntimeLanguageDetectionSource::Shebang => "shebang",
        RuntimeLanguageDetectionSource::Glob => "glob",
        RuntimeLanguageDetectionSource::FileType => "file-type",
        RuntimeLanguageDetectionSource::FirstLineRegex => "first-line-regex",
        RuntimeLanguageDetectionSource::ContentRegex => "content-regex",
    }
}

fn grammar_health_label(status: &RuntimeGrammarHealth) -> String {
    match status {
        RuntimeGrammarHealth::Unresolved => String::from("unresolved"),
        RuntimeGrammarHealth::Loaded => String::from("loaded"),
        RuntimeGrammarHealth::Missing => String::from("missing"),
        RuntimeGrammarHealth::Error(error) => format!("error: {error}"),
    }
}

fn query_health_label(status: &RuntimeQueryHealth) -> String {
    match status {
        RuntimeQueryHealth::Unsupported => String::from("unsupported"),
        RuntimeQueryHealth::Missing => String::from("missing"),
        RuntimeQueryHealth::Loaded => String::from("loaded"),
        RuntimeQueryHealth::Error(error) => format!("error: {error}"),
    }
}

fn runtime_operation_exit_code(kind: RuntimeOperationErrorKind) -> i32 {
    match kind {
        RuntimeOperationErrorKind::ConfigMerge => EXIT_RUNTIME_CONFIG_MERGE,
        RuntimeOperationErrorKind::GrammarSource => EXIT_RUNTIME_GRAMMAR_SOURCE,
        RuntimeOperationErrorKind::RuntimeAsset => EXIT_RUNTIME_ASSET,
    }
}

fn runtime_report_exit_code(report: &RuntimeHealthReport) -> i32 {
    if report.language_id.is_none() {
        return EXIT_RUNTIME_CONFIG_MERGE;
    }
    if matches!(
        report.grammar_status,
        RuntimeGrammarHealth::Missing | RuntimeGrammarHealth::Error(_)
    ) {
        return EXIT_RUNTIME_ASSET;
    }
    if report.query_reports.iter().any(|query| {
        matches!(query.status, RuntimeQueryHealth::Missing | RuntimeQueryHealth::Error(_))
    }) {
        return EXIT_RUNTIME_ASSET;
    }
    0
}

fn exit_with_runtime_operation_error(context: &str, error: RuntimeOperationError) -> ! {
    eprintln!("{context}: {error}");
    std::process::exit(runtime_operation_exit_code(error.kind()));
}

fn render_runtime_report(report: &RuntimeHealthReport) -> String {
    let mut out = String::from("ee do runtime\n────────────\n");
    if let Some(requested_language) = &report.requested_language {
        out.push_str(&format!("requested language: {requested_language}\n"));
    }
    if let Some(file_path) = &report.file_path {
        out.push_str(&format!("file: {}\n", file_path.display()));
    }

    match (&report.language_id, &report.display_name, report.detection_source) {
        (Some(language_id), Some(display_name), Some(source)) => {
            out.push_str(&format!(
                "resolved language: {display_name} [{language_id}] via {}\n",
                detection_source_label(source)
            ));
        }
        _ => out.push_str("resolved language: <none>\n"),
    }

    out.push_str("runtime roots:\n");
    out.push_str(&format!("  bundled: {}\n", report.runtime_roots.bundled_root().display()));
    out.push_str(&format!("  user: {}\n", report.runtime_roots.user_root().display()));
    match report.runtime_roots.workspace_root() {
        Some(root) => out.push_str(&format!("  workspace: {}\n", root.display())),
        None => out.push_str("  workspace: <disabled>\n"),
    }

    if let Some(asset_source) = report.asset_source {
        out.push_str(&format!("asset source: {:?}\n", asset_source));
    }
    if let Some(root) = &report.effective_runtime_root {
        out.push_str(&format!("effective runtime root: {}\n", root.display()));
    }
    if let Some(grammar_path) = &report.grammar_path {
        out.push_str(&format!("grammar path: {}\n", grammar_path.display()));
    }
    out.push_str(&format!("grammar: {}\n", grammar_health_label(&report.grammar_status)));
    out.push_str("queries:\n");
    for query_report in &report.query_reports {
        out.push_str(&format!(
            "  {:<11} {}",
            query_kind_label(query_report.kind),
            query_health_label(&query_report.status)
        ));
        if !query_report.source_paths.is_empty() {
            let joined = query_report
                .source_paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!(" [{}]", joined));
        }
        out.push('\n');
    }

    out
}

fn cmd_runtime(file_path: Option<&Path>, explicit_language: Option<&str>) {
    let (first_line, content) = match file_path {
        Some(path) => match read_runtime_probe(path) {
            Ok(probe) => probe,
            Err(error) => {
                eprintln!("Cannot inspect runtime inputs for {}: {error}", path.display());
                std::process::exit(1);
            }
        },
        None => (None, None),
    };

    let report = with_default_runtime_loader_mut(|loader| {
        loader.runtime_health_report(
            explicit_language,
            file_path,
            first_line.as_deref(),
            content.as_deref(),
        )
    });
    print!("{}", render_runtime_report(&report));
    let exit_code = runtime_report_exit_code(&report);
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}

fn default_runtime_source_root() -> PathBuf {
    with_default_runtime_loader_mut(|loader| loader.default_user_source_root())
}

fn default_runtime_output_root() -> PathBuf {
    with_default_runtime_loader_mut(|loader| loader.runtime_roots().user_root().to_path_buf())
}

fn cmd_runtime_fetch(
    languages: &[String],
    include_all: bool,
    source_root: Option<&Path>,
    force: bool,
) {
    let source_root =
        source_root.map(Path::to_path_buf).unwrap_or_else(default_runtime_source_root);
    let fetched = with_default_runtime_loader_mut(|loader| {
        loader.fetch_grammar_sources(languages, include_all, &source_root, force)
    })
    .unwrap_or_else(|error| exit_with_runtime_operation_error("runtime fetch failed", error));

    println!("fetched {} grammar source trees into {}", fetched.len(), source_root.display());
    for grammar in fetched {
        println!("  {} -> {}", grammar.language_id, grammar.source_dir.display());
    }
}

fn cmd_runtime_build(
    languages: &[String],
    include_all: bool,
    source_root: Option<&Path>,
    output_root: Option<&Path>,
    force: bool,
    skip_load: bool,
) {
    let source_root =
        source_root.map(Path::to_path_buf).unwrap_or_else(default_runtime_source_root);
    let output_root =
        output_root.map(Path::to_path_buf).unwrap_or_else(default_runtime_output_root);
    let built = with_default_runtime_loader_mut(|loader| {
        loader.build_runtime_assets(
            languages,
            include_all,
            &source_root,
            &output_root,
            force,
            skip_load,
        )
    })
    .unwrap_or_else(|error| exit_with_runtime_operation_error("runtime build failed", error));

    println!("built {} runtime grammars into {}", built.len(), output_root.display());
    for grammar in built {
        let query_summary = if grammar.query_paths.is_empty() {
            String::from("no standard queries copied")
        } else {
            format!("{} query files", grammar.query_paths.len())
        };
        println!(
            "  {} -> {} ({query_summary})",
            grammar.language_id,
            grammar.grammar_path.display()
        );
    }
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

fn read_text_range(store: &dyn TextStore, range: ByteRange) -> io::Result<(String, ByteRange)> {
    if let TextChunkResult::Ready(chunk) = store.read_byte_range(range) {
        return Ok((chunk.text, chunk.byte_range));
    }

    let mut text = String::new();
    let mut decoded_start = None;
    let mut decoded_end = range.start;
    for result in store.iter_chunks(range) {
        let TextChunkResult::Ready(chunk) = result else {
            return Err(io::Error::other("failed to read requested text range"));
        };
        decoded_start.get_or_insert(chunk.byte_range.start);
        decoded_end = chunk.byte_range.end;
        text.push_str(&chunk.text);
    }

    Ok((text, ByteRange { start: decoded_start.unwrap_or(range.start), end: decoded_end }))
}

fn read_exact_text_range(store: &dyn TextStore, range: ByteRange) -> io::Result<String> {
    let (text, decoded_range) = read_text_range(store, range)?;
    if decoded_range == range {
        return Ok(text);
    }

    let start = usize::try_from(range.start.0.saturating_sub(decoded_range.start.0))
        .map_err(|_| io::Error::other("range start overflow"))?;
    let end = usize::try_from(range.end.0.saturating_sub(decoded_range.start.0))
        .map_err(|_| io::Error::other("range end overflow"))?;
    if end > text.len() || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
        return Err(io::Error::other("requested range split utf-8 boundary"));
    }

    Ok(text[start..end].to_owned())
}

fn read_file_head(path: &Path, lines: usize) -> io::Result<String> {
    if lines == 0 {
        return Ok(String::new());
    }

    let store = VlfStore::open(path)?;
    let mut next_start = 0u64;
    let mut pending = String::new();
    let mut out = String::new();
    let mut lines_emitted = 0usize;

    while next_start < store.len_bytes() {
        let next_end = next_start.saturating_add(FILE_PREVIEW_CHUNK_BYTES).min(store.len_bytes());
        pending.push_str(&read_exact_text_range(&store, ByteRange::new(next_start, next_end))?);
        next_start = next_end;

        while lines_emitted < lines {
            let Some(newline_idx) = pending.find('\n') else { break };
            let line_end = newline_idx + 1;
            out.push_str(&pending[..line_end]);
            pending.drain(..line_end);
            lines_emitted += 1;
            if lines_emitted == lines {
                return Ok(out);
            }
        }
    }

    if lines_emitted < lines {
        out.push_str(&pending);
    }
    Ok(out)
}

fn read_file_tail(path: &Path, lines: usize) -> io::Result<String> {
    if lines == 0 {
        return Ok(String::new());
    }

    let store = VlfStore::open(path)?;
    let mut start = store.len_bytes();
    let mut text = String::new();
    let mut newline_count = 0usize;

    while start > 0 && newline_count <= lines {
        let chunk_start = start.saturating_sub(FILE_PREVIEW_CHUNK_BYTES);
        let chunk = read_exact_text_range(&store, ByteRange::new(chunk_start, start))?;
        newline_count += chunk.as_bytes().iter().filter(|&&byte| byte == b'\n').count();
        text.insert_str(0, &chunk);
        start = chunk_start;
    }

    if text.is_empty() {
        return Ok(text);
    }

    let bytes = text.as_bytes();
    let mut idx = bytes.len();
    if idx > 0 && bytes[idx - 1] == b'\n' {
        idx -= 1;
    }

    let mut lines_seen = 0usize;
    while idx > 0 {
        idx -= 1;
        if bytes[idx] == b'\n' {
            lines_seen += 1;
            if lines_seen == lines {
                return Ok(text[idx + 1..].to_owned());
            }
        }
    }

    Ok(text)
}

fn cmd_file_head(path: &Path, lines: usize) {
    match read_file_head(path, lines) {
        Ok(text) => {
            print!("{text}");
            let _ = io::stdout().flush();
        }
        Err(err) => {
            eprintln!("Cannot read head for {}: {err}", path.display());
            std::process::exit(1);
        }
    }
}

fn cmd_file_tail(path: &Path, lines: usize) {
    match read_file_tail(path, lines) {
        Ok(text) => {
            print!("{text}");
            let _ = io::stdout().flush();
        }
        Err(err) => {
            eprintln!("Cannot read tail for {}: {err}", path.display());
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
                DoCommands::Runtime { file, language, command } => match command {
                    None => cmd_runtime(file.as_deref(), language.as_deref()),
                    Some(RuntimeCommands::Fetch { all, languages, source_root, force }) => {
                        cmd_runtime_fetch(&languages, all, source_root.as_deref(), force)
                    }
                    Some(RuntimeCommands::Build {
                        all,
                        languages,
                        source_root,
                        output_root,
                        force,
                        skip_load,
                    }) => cmd_runtime_build(
                        &languages,
                        all,
                        source_root.as_deref(),
                        output_root.as_deref(),
                        force,
                        skip_load,
                    ),
                },
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
