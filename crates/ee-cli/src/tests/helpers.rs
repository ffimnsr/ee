//! Shared test helpers: locks, guards, fixture builders, and utility functions.
//!
//! All items are `pub` so sibling test modules can reach them via
//! `use super::helpers::*;` or `use crate::tests::helpers::*;`.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use xi_core_lib::open_policy::OpenThresholds;

use crate::app::App;
use crate::buffer::{BufState, BufferManager};
use crate::ui::ui;

// ── Test-wide locks ────────────────────────────────────────────────────────────

pub fn cwd_test_lock() -> &'static crate::config::TestCwdLock {
    crate::config::test_cwd_lock()
}

pub fn perf_test_lock() -> &'static Mutex<()> {
    static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub fn tree_sitter_test_lock() -> &'static Mutex<()> {
    static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

// ── Guards ─────────────────────────────────────────────────────────────────────

pub struct CurrentDirGuard(pub PathBuf);

impl CurrentDirGuard {
    pub fn capture() -> Self {
        Self(env::current_dir().unwrap())
    }
}

impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        let _ = env::set_current_dir(&self.0);
    }
}

pub struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    pub fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let previous = env::var_os(key);
        unsafe {
            env::set_var(key, value);
        }
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => unsafe {
                env::set_var(self.key, value);
            },
            None => unsafe {
                env::remove_var(self.key);
            },
        }
    }
}

// ── Backend polling ────────────────────────────────────────────────────────────

pub fn wait_until_with_backend(
    backend: &mut BufferManager,
    label: &str,
    timeout: Duration,
    mut condition: impl FnMut(&mut BufferManager) -> bool,
) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let _ = backend.pump();
        if condition(backend) {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {label}; status={:?}", backend.status_message.as_deref());
}

pub fn wait_until_file_matches(
    backend: &mut BufferManager,
    label: &str,
    path: &Path,
    timeout: Duration,
    mut condition: impl FnMut(&str) -> bool,
) -> String {
    let deadline = Instant::now() + timeout;
    let mut last_text = String::new();
    let mut last_error = None;
    while Instant::now() < deadline {
        let _ = backend.pump();
        match fs::read_to_string(path) {
            Ok(text) => {
                if condition(&text) {
                    return text;
                }
                last_text = text;
                last_error = None;
            }
            Err(error) => last_error = Some(error.to_string()),
        }
        thread::sleep(Duration::from_millis(10));
    }

    panic!(
        "timed out waiting for {label}; path={}; last_text={last_text:?}; last_error={last_error:?}; status={:?}",
        path.display(),
        backend.status_message.as_deref()
    );
}

// ── LSP / plugin helpers ──────────────────────────────────────────────────────

pub fn xi_lsp_crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../xi-lsp-lib")
        .canonicalize()
        .expect("xi-lsp crate dir should resolve")
}

pub fn built_xi_lsp_binary(name: &str) -> PathBuf {
    let env_var = format!("CARGO_BIN_EXE_{name}");
    if let Some(path) = env::var_os(&env_var) {
        return PathBuf::from(path);
    }

    let crate_dir = xi_lsp_crate_dir();
    let workspace_root =
        crate_dir.parent().and_then(|path| path.parent()).expect("workspace root should exist");
    let target_dir = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|path| if path.is_relative() { workspace_root.join(path) } else { path })
        .unwrap_or_else(|| workspace_root.join("target"));
    let binary_name = format!("{name}{}", env::consts::EXE_SUFFIX);
    let candidates = [target_dir.join("debug"), workspace_root.join("target").join("debug")];

    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(workspace_root)
        .args(["build", "--manifest-path"])
        .arg(crate_dir.join("Cargo.toml"))
        .args(["--bin", name])
        .status()
        .expect("cargo build for xi-lsp binary should start");
    assert!(status.success(), "cargo build for xi-lsp binary should succeed");
    candidates
        .iter()
        .find_map(|dir| find_built_binary(dir, &binary_name))
        .unwrap_or_else(|| panic!("xi-lsp binary should exist after build"))
}

pub fn find_built_binary(dir: &Path, binary_name: &str) -> Option<PathBuf> {
    let exact = dir.join(binary_name);
    if exact.is_file() {
        return Some(exact);
    }

    [dir.to_path_buf(), dir.join("deps")].into_iter().filter(|path| path.exists()).find_map(
        |search_dir| {
            fs::read_dir(search_dir).ok()?.find_map(|entry| {
                let entry = entry.ok()?;
                let path = entry.path();
                let file_name = path.file_name()?.to_str()?;
                if path.is_file()
                    && file_name.starts_with(binary_name)
                    && !file_name.ends_with(".d")
                {
                    return Some(path);
                }
                None
            })
        },
    )
}

pub fn fake_server_binary_path() -> PathBuf {
    built_xi_lsp_binary("xi_lsp_fake_server")
}

pub fn xi_lsp_plugin_binary_path() -> PathBuf {
    built_xi_lsp_binary("xi-lsp-plugin")
}

pub fn install_test_lsp_plugin(config_root: &Path) {
    let plugin_dir = config_root.join("ee").join("plugins").join("xi-lsp-plugin");
    fs::create_dir_all(plugin_dir.join("bin")).unwrap();
    fs::copy(xi_lsp_crate_dir().join("manifest.toml"), plugin_dir.join("manifest.toml")).unwrap();
    let plugin_binary =
        plugin_dir.join("bin").join(format!("xi-lsp-plugin{}", env::consts::EXE_SUFFIX));
    fs::copy(xi_lsp_plugin_binary_path(), &plugin_binary).unwrap();

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let mut perms = fs::metadata(&plugin_binary).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&plugin_binary, perms).unwrap();
}

pub fn log_contains_methods(path: &Path, methods: &[&str]) -> bool {
    fs::read_to_string(path).ok().is_some_and(|contents| {
        methods.iter().all(|method| contents.contains(&format!("\"method\":\"{method}\"")))
    })
}

pub fn write_lsp_config(path: &Path, command: &Path, log_path: &Path) {
    let command = format!("{:?}", command.to_string_lossy());
    let log_path = format!("{:?}", log_path.to_string_lossy());
    fs::write(
        path,
        format!(
            "[lsp.servers.gleam]\nlanguage_name = \"Gleam\"\ncommand = {command}\nargs = [{log_path}]\nextensions = [\"gleam\"]\n"
        ),
    )
    .unwrap();
}

// ── Git helpers ────────────────────────────────────────────────────────────────

pub fn run_git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git").args(args).current_dir(cwd).output().unwrap();
    assert!(
        output.status.success(),
        "git command failed: {args:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub fn init_test_git_repo(cwd: &Path) {
    fs::create_dir_all(cwd.join(".git-hooks-disabled")).unwrap();
    run_git(cwd, &["init"]);
    run_git(cwd, &["config", "user.email", "test@example.com"]);
    run_git(cwd, &["config", "user.name", "Test User"]);
    run_git(cwd, &["config", "commit.gpgsign", "false"]);
    run_git(cwd, &["config", "core.hooksPath", ".git-hooks-disabled"]);
}

// ── Fixture / temp-path helpers ────────────────────────────────────────────────

pub fn unique_temp_path(prefix: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let pid = std::process::id();
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    env::temp_dir().join(format!("{prefix}-{pid}-{nanos}-{sequence}.txt"))
}

pub fn write_exact_size_ascii_fixture(
    path: &Path,
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

// ── Perf / open-to-first-render helpers ───────────────────────────────────────

pub fn timed_open_to_first_render(path: &Path) -> (App, Duration) {
    let start = Instant::now();
    let app = App::from_path(Some(path.to_path_buf())).unwrap();

    let backend = TestBackend::new(120, 50);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();

    (app, start.elapsed())
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct OpenToFirstRenderBreakdown {
    pub open: Duration,
    pub draw: Duration,
    pub total: Duration,
    pub startup: crate::buffer::StartupProfile,
}

pub fn timed_open_to_first_render_breakdown(path: &Path) -> (App, OpenToFirstRenderBreakdown) {
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

pub fn budget_many_line(i: usize) -> String {
    let thresholds = OpenThresholds::default();
    let target_line_bytes =
        (thresholds.normal_bytes as usize / (thresholds.normal_lines as usize - 2_000)).max(256);
    let prefix = format!("fn item_{i:06}() {{ let value = {}; }} // ", i % 10);
    let suffix_width = target_line_bytes.saturating_sub(prefix.len());
    format!("{prefix}{:0>suffix_width$}", i % 100_000)
}

pub fn budget_long_line(i: usize) -> String {
    if i.is_multiple_of(2) {
        format!("const LINE_{i}: &str = \"{}\";", "x".repeat(512))
    } else {
        format!("let line_{i} = {i};")
    }
}

pub fn assert_open_to_first_render_budget(label: &str, line_builder: fn(usize) -> String) {
    let _cwd_lock = cwd_test_lock().lock().unwrap();
    let _guard = perf_test_lock().lock().unwrap_or_else(|err| err.into_inner());
    let isolated_config = tempfile::tempdir().unwrap();
    let isolated_runtime = tempfile::tempdir().unwrap();
    let _xdg_guard = EnvVarGuard::set("XDG_CONFIG_HOME", isolated_config.path());
    let _runtime_guard = EnvVarGuard::set("EE_RUNTIME_DIR", isolated_runtime.path());

    const WARM_BUDGET_MS: u128 = 250;
    const WARM_NOISE_CEILING_MS: u128 = 600;
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

    // Warm one-time editor/runtime initialisation outside the measured passes.
    drop(App::from_path(None).unwrap());

    let (mut cold_app, cold_elapsed) = timed_open_to_first_render(&path);
    let mut best_warm = None;
    let mut warm_samples = Vec::with_capacity(WARM_SAMPLE_COUNT);
    for _ in 0..WARM_SAMPLE_COUNT {
        let candidate = timed_open_to_first_render(&path);
        warm_samples.push(candidate.1.as_millis());
        if best_warm.as_ref().is_none_or(|(_, elapsed)| candidate.1 < *elapsed) {
            best_warm = Some(candidate);
        }
    }
    let (mut warm_app, warm_elapsed) = best_warm.expect("warm pass should run");

    // First-render timing intentionally stops before asynchronous xi-core
    // notifications finish. Drain them before asserting cache shape or
    // deleting fixture so parallel suite load cannot turn a valid open into a
    // zero-line observation.
    wait_until_with_backend(
        &mut cold_app.backend,
        "cold normal-mode line cache",
        Duration::from_secs(5),
        |backend| backend.lines.len() == line_count,
    );
    wait_until_with_backend(
        &mut warm_app.backend,
        "warm normal-mode line cache",
        Duration::from_secs(5),
        |backend| backend.lines.len() == line_count,
    );

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

    let strict_budget = env::var_os("EE_STRICT_PERF_BUDGET").is_some();
    if cold_elapsed.as_millis() >= COLD_BUDGET_MS {
        eprintln!(
            "cold open-to-first-render for {label} fixture missed target: {}ms, target<{COLD_BUDGET_MS}ms, startup={:?}",
            cold_elapsed.as_millis(),
            cold_app.backend.startup_profile()
        );
    }
    if strict_budget {
        assert!(
            cold_elapsed.as_millis() < COLD_BUDGET_MS,
            "cold open-to-first-render for {label} fixture took {}ms, expected < {COLD_BUDGET_MS}ms; startup={:?}",
            cold_elapsed.as_millis(),
            cold_app.backend.startup_profile()
        );
    }
    if warm_elapsed.as_millis() >= WARM_BUDGET_MS {
        eprintln!(
            "warm open-to-first-render for {label} fixture missed target: best={}ms, target<{WARM_BUDGET_MS}ms, samples={warm_samples:?}",
            warm_elapsed.as_millis()
        );
    }
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

pub fn report_open_to_first_render_breakdown(label: &str, line_builder: fn(usize) -> String) {
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

// ── App test helpers ──────────────────────────────────────────────────────────

pub fn run_ex(app: &mut App, command: &str) {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    if app.mode == crate::app::Mode::Agent {
        app.mode = crate::app::Mode::CommandLine;
        app.command_buffer.clear();
        app.command_buffer.push_str(command);
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        return;
    }

    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)));
    for ch in command.chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
}

pub fn insert_text(app: &mut App, text: &str) {
    app.backend
        .send_edit("insert", serde_json::json!({ "chars": text }))
        .expect("insert_text edit should send");
    app.backend.selections_preview().expect("insert_text edit barrier should succeed");
    app.backend
        .sync_pending_events_for_whole_document()
        .expect("insert_text document sync should succeed");
}

pub fn test_buf_state() -> BufState {
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
        last_save_result_generation: 0,
        last_save_succeeded: true,
        last_save_permission_denied: false,
        last_save_error_message: None,
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

pub fn window_paths(app: &App) -> Vec<PathBuf> {
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

pub fn render_screen_rows(app: &App, width: u16, height: u16) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, app)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| (0..width).map(|x| buffer.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect()
}

pub fn count_rendered_occurrences(rows: &[String], needle: &str) -> usize {
    rows.iter().map(|row| row.matches(needle).count()).sum()
}

// ── Render benchmark helpers ───────────────────────────────────────────────────

pub mod fixture {
    /// Returns a Vec of `n` lines of uniform width `line_len`.
    pub fn many_line_fixture(n: usize, line_len: usize) -> Vec<String> {
        (0..n).map(|i| format!("{i:>0width$}", width = line_len.min(20))).collect()
    }

    /// Returns a Vec of `n` lines that each contain exactly one very long line
    /// interleaved with short lines.
    pub fn long_line_fixture(n: usize, long_len: usize) -> Vec<String> {
        (0..n)
            .map(|i| if i % 2 == 0 { "x".repeat(long_len) } else { format!("line {i}") })
            .collect()
    }

    /// Returns a Vec of `n` mixed-indentation source-like lines (simulates a
    /// 300 K LOC Rust source file).
    pub fn source_fixture(n: usize) -> Vec<String> {
        let snippets = ["fn foo() {", "    let x = 1;", "    let y = 2;", "    x + y", "}"];
        (0..n).map(|i| snippets[i % snippets.len()].to_owned()).collect()
    }

    /// Returns a Vec of `n` lines with alternating LF and CRLF endings
    /// stripped (the `lines` vec stores text only, endings live in the rope).
    pub fn mixed_crlf_fixture(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("line {i}")).collect()
    }
}

/// Renders `lines` into a 120×50 terminal and returns the measured elapsed
/// duration.
///
/// Measurement is one warm-up draw plus the minimum of three timed draws:
/// parallel test runs share the CPU, so a single scheduler preemption must
/// not fail a frame-budget regression test.  A real render regression stays
/// visible because every sample slows down.
pub fn timed_render(lines: Vec<String>) -> Duration {
    let mut app = App::from_path(None).unwrap();
    app.backend.lines = lines;

    let backend = TestBackend::new(120, 50);
    let mut terminal = Terminal::new(backend).unwrap();

    // Warm-up: populate allocation pools and any lazily built structures.
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    (0..3)
        .map(|_| {
            let start = Instant::now();
            terminal.draw(|frame| ui(frame, &app)).unwrap();
            start.elapsed()
        })
        .min()
        .expect("three samples")
}
