use std::collections::HashMap;
use std::env;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use serde::Deserialize;
use serde_json::{Value, json};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use xi_core_lib::XiCore;
use xi_rpc::{ReadTransport, RpcLoop, WriteTransport};

fn main() -> io::Result<()> {
    let path = env::args_os().nth(1).map(PathBuf::from);
    let mut app = App::from_path(path)?;
    run(&mut app)
}

fn run(app: &mut App) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    terminal.clear()?;

    let result = run_app(&mut terminal, app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> io::Result<()> {
    while !app.should_quit {
        // L109: drain xi backend events non-blockingly
        app.backend.drain_events()?;

        // L106/L107: update viewport to keep cursor visible, then notify xi
        let size = terminal.size()?;
        // layout: rows[0]=editor, rows[1]=status, rows[2]=prompt
        let editor_height = (size.height as usize).saturating_sub(2);
        app.scroll_into_view(editor_height);
        app.backend.notify_scroll(
            app.viewport.top_line,
            app.viewport.top_line + editor_height,
        )?;

        terminal.draw(|frame| ui(frame, app))?;

        // L109: use a short poll so the event loop stays responsive
        if event::poll(Duration::from_millis(16))? {
            app.handle_event(event::read()?);
        }
    }
    Ok(())
}

fn ui(frame: &mut ratatui::Frame<'_>, app: &App) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(Style::default().bg(Color::Rgb(22, 24, 31))), area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)])
        .split(area);

    let editor = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(6), Constraint::Min(1)])
        .split(rows[0]);

    render_gutter(frame, editor[0], app);
    render_buffer(frame, editor[1], app);
    render_status(frame, rows[1], app);
    render_prompt(frame, rows[2], app);

    let cursor = app.cursor_position(editor[1], rows[2]);
    frame.set_cursor_position(cursor);
}

fn render_gutter(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let height = area.height as usize;
    let line_count = app.backend.lines.len().max(1);
    let top = app.viewport.top_line;
    let lines = (0..height)
        .map(|i| {
            let line_idx = top + i;
            let number = if line_idx < line_count { line_idx + 1 } else { 0 };
            if number == 0 {
                Line::from(" ")
            } else {
                Line::from(Span::styled(
                    format!("{number:>4} "),
                    Style::default().fg(Color::DarkGray),
                ))
            }
        })
        .collect::<Vec<_>>();

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(Color::Rgb(30, 32, 39))),
        area,
    );
}

fn render_buffer(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let height = area.height as usize;
    let top = app.viewport.top_line;
    let left = app.viewport.left_col;

    let text = if app.backend.lines.is_empty() && top == 0 {
        vec![Line::from(Span::styled(
            "empty buffer",
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        ))]
    } else {
        app.backend
            .lines
            .iter()
            .skip(top)
            .take(height)
            .map(|line| {
                // L106: skip left_col display columns from the left
                let byte_start = display_col_to_byte(line, left);
                Line::from(&line[byte_start..])
            })
            .collect::<Vec<_>>()
    };

    frame.render_widget(
        Paragraph::new(text)
            .block(Block::default().borders(Borders::NONE))
            .style(Style::default().fg(Color::Rgb(213, 216, 224)).bg(Color::Rgb(22, 24, 31))),
        area,
    );
}

fn render_status(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let mode = Span::styled(
        format!(" {} ", app.mode.label()),
        Style::default().fg(Color::Rgb(22, 24, 31)).bg(Color::Rgb(137, 220, 235)),
    );
    let file = Span::styled(
        format!(" {}", app.backend.title()),
        Style::default().fg(Color::Rgb(238, 238, 238)).bg(Color::Rgb(49, 54, 68)),
    );
    let modified = if app.backend.pristine {
        Span::raw("")
    } else {
        Span::styled(
            " [+]",
            Style::default().fg(Color::Rgb(250, 179, 135)).bg(Color::Rgb(49, 54, 68)),
        )
    };
    let position = Span::styled(
        format!("  Ln {}, Col {} ", app.backend.cursor_line + 1, app.backend.cursor_col + 1),
        Style::default().fg(Color::Rgb(186, 194, 222)).bg(Color::Rgb(49, 54, 68)),
    );

    frame.render_widget(
        Paragraph::new(Line::from(vec![mode, file, modified, position]))
            .style(Style::default().bg(Color::Rgb(49, 54, 68))),
        area,
    );
}

fn render_prompt(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let prompt = match app.mode {
        Mode::Normal => Line::from(match app.backend.status_message.as_deref() {
            Some(message) => message.to_owned(),
            None => "normal | i insert | v visual | : command | q quit".to_owned(),
        }),
        Mode::Insert => Line::from("insert | esc normal"),
        Mode::Visual => Line::from("visual | move selects | v/esc normal | : command"),
        Mode::CommandLine => {
            Line::from(vec![Span::raw(":"), Span::raw(app.command_buffer.as_str())])
        }
    };

    frame.render_widget(
        Paragraph::new(prompt)
            .style(Style::default().fg(Color::Rgb(166, 173, 200)).bg(Color::Rgb(24, 25, 38))),
        area,
    );
}

#[derive(Debug, PartialEq, Eq)]
struct App {
    backend: XiClient,
    mode: Mode,
    command_buffer: String,
    should_quit: bool,
    // L106: explicit viewport model
    viewport: Viewport,
    // L108: input dispatcher state (count digits, prefix key)
    input_state: InputState,
}

impl App {
    fn from_path(path: Option<PathBuf>) -> io::Result<Self> {
        Ok(Self {
            backend: XiClient::new(path)?,
            mode: Mode::Normal,
            command_buffer: String::new(),
            should_quit: false,
            viewport: Viewport::default(),
            input_state: InputState::default(),
        })
    }

    fn handle_event(&mut self, event: Event) {
        let Event::Key(key) = event else {
            return;
        };

        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return;
        }

        // L108: look up action in the bindings table first
        let bkey = BindingKey {
            mode: self.mode,
            key: key.code,
            modifiers: key.modifiers,
            prefix: self.input_state.prefix,
        };
        // Try exact modifiers, then fall back to NONE modifiers (acts as "any")
        let action = bindings()
            .get(&bkey)
            .or_else(|| {
                if key.modifiers != KeyModifiers::NONE {
                    bindings().get(&BindingKey { modifiers: KeyModifiers::NONE, ..bkey })
                } else {
                    None
                }
            })
            .cloned();

        if let Some(action) = action {
            self.dispatch(action, key);
            // Reset count/prefix after a non-digit binding in normal mode
            if self.mode == Mode::Normal
            && !matches!(key.code, KeyCode::Char(c) if c.is_ascii_digit())
        {
            self.input_state.reset();
        }
        } else {
            self.handle_default(key);
        }
    }

    fn dispatch(&mut self, action: Action, _key: KeyEvent) {
        match action {
            Action::Quit => self.should_quit = true,
            Action::EnterMode(mode) => {
                if mode == Mode::Normal {
                    self.enter_normal_mode();
                } else {
                    self.mode = mode;
                }
            }
            Action::EnterCommandMode => {
                self.mode = Mode::CommandLine;
                self.command_buffer.clear();
            }
            Action::Edit(method) => {
                let _ = self.backend.send_edit(method, json!([]));
            }
            Action::CollapseAndEnterNormal => {
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.enter_normal_mode();
            }
            Action::ExecuteCommand => self.execute_command(),
            Action::DeleteBackward => {
                let _ = self.backend.send_edit("delete_backward", json!([]));
            }
            Action::CommandBackspace => {
                self.command_buffer.pop();
            }
        }
    }

    /// Default handler for keys not found in the bindings table.
    fn handle_default(&mut self, key: KeyEvent) {
        let ch = match key.code {
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                c
            }
            _ => return,
        };

        match self.mode {
            Mode::Insert => {
                let _ = self.backend.send_edit("insert", json!({ "chars": ch.to_string() }));
            }
            Mode::CommandLine => {
                self.command_buffer.push(ch);
            }
            Mode::Normal => {
                // Accumulate count digits for future motion repeat
                if let Some(d) = ch.to_digit(10) {
                    self.input_state.count_digits.push(d as u8);
                }
            }
            Mode::Visual => {}
        }
    }

    fn cursor_position(&self, editor_area: Rect, prompt_area: Rect) -> Position {
        if self.mode == Mode::CommandLine {
            let max_x = prompt_area.right().saturating_sub(1);
            let x = (prompt_area.x + 1 + self.command_buffer.len() as u16).min(max_x);
            return Position::new(x, prompt_area.y);
        }

        let max_x = editor_area.right().saturating_sub(1);
        let max_y = editor_area.bottom().saturating_sub(1);

        // L105/L110: convert byte column → display column for correct cursor placement
        let line =
            self.backend.lines.get(self.backend.cursor_line).map(|s| s.as_str()).unwrap_or("");
        let display_col = byte_col_to_display_col(line, self.backend.cursor_col);

        // L106: subtract viewport offsets so cursor tracks the visible window
        let screen_line = self.backend.cursor_line.saturating_sub(self.viewport.top_line);
        let screen_col = display_col.saturating_sub(self.viewport.left_col);

        let x = (editor_area.x + screen_col as u16).min(max_x);
        let y = (editor_area.y + screen_line as u16).min(max_y);
        Position::new(x, y)
    }

    fn enter_normal_mode(&mut self) {
        self.mode = Mode::Normal;
        self.command_buffer.clear();
    }

    fn execute_command(&mut self) {
        match self.command_buffer.trim() {
            "q" | "quit" | "q!" | "quit!" => self.should_quit = true,
            "w" | "write" => {
                if let Err(err) = self.backend.save() {
                    self.backend.status_message = Some(format!("save failed: {err}"));
                }
            }
            "wq" | "x" => {
                if let Err(err) = self.backend.save() {
                    self.backend.status_message = Some(format!("save failed: {err}"));
                } else {
                    self.should_quit = true;
                }
            }
            other if !other.is_empty() => {
                self.backend.status_message = Some(format!("unknown command: {other}"));
            }
            _ => {}
        }
        self.enter_normal_mode();
    }

    /// L106: scroll the viewport so `cursor_line` is visible.
    fn scroll_into_view(&mut self, editor_height: usize) {
        if editor_height == 0 {
            return;
        }
        let cursor_line = self.backend.cursor_line;
        if cursor_line < self.viewport.top_line {
            self.viewport.top_line = cursor_line;
        } else if cursor_line >= self.viewport.top_line + editor_height {
            self.viewport.top_line = cursor_line + 1 - editor_height;
        }
        // Update target column from current cursor for vertical navigation
        let line =
            self.backend.lines.get(cursor_line).map(|s| s.as_str()).unwrap_or("");
        self.viewport.target_col = byte_col_to_display_col(line, self.backend.cursor_col);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Mode {
    Normal,
    Insert,
    Visual,
    CommandLine,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Normal => "NOR",
            Mode::Insert => "INS",
            Mode::Visual => "VIS",
            Mode::CommandLine => "CMD",
        }
    }
}

/// L106: explicit viewport model tracking visible region of the buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Viewport {
    /// Index of the first visible line.
    top_line: usize,
    /// First visible display column (for horizontal scrolling).
    left_col: usize,
    /// Remembered display column used to restore position after vertical navigation.
    target_col: usize,
}

/// L108: accumulated state between key presses (count digits and prefix key).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct InputState {
    /// Digits entered so far for a count prefix (e.g. `3` before `j`).
    count_digits: Vec<u8>,
    /// A single-char prefix key held pending the next key (e.g. `g`).
    prefix: Option<char>,
}

impl InputState {
    /// Returns the accumulated count (default 1 when no digits have been entered).
    #[allow(dead_code)]
    pub(crate) fn count(&self) -> u32 {
        if self.count_digits.is_empty() {
            return 1;
        }
        self.count_digits
            .iter()
            .fold(0u32, |acc, &d| acc.saturating_mul(10).saturating_add(d as u32))
    }

    fn reset(&mut self) {
        self.count_digits.clear();
        self.prefix = None;
    }
}

/// L108: action dispatched by the table-driven input handler.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Action {
    Quit,
    EnterMode(Mode),
    EnterCommandMode,
    /// Send a named edit command to xi core with empty params.
    Edit(&'static str),
    /// Collapse visual selection then return to normal mode.
    CollapseAndEnterNormal,
    ExecuteCommand,
    DeleteBackward,
    CommandBackspace,
}

/// L108: lookup key for the flat bindings table.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct BindingKey {
    mode: Mode,
    key: KeyCode,
    modifiers: KeyModifiers,
    prefix: Option<char>,
}

/// L108: returns the static, lazily-initialised bindings table.
fn bindings() -> &'static HashMap<BindingKey, Action> {
    static BINDINGS: OnceLock<HashMap<BindingKey, Action>> = OnceLock::new();
    BINDINGS.get_or_init(build_bindings)
}

fn build_bindings() -> HashMap<BindingKey, Action> {
    use Action::*;
    use Mode::*;

    let none = KeyModifiers::NONE;
    let ctrl = KeyModifiers::CONTROL;

    let mut m: HashMap<BindingKey, Action> = HashMap::new();

    macro_rules! bind {
        ($mode:expr, $key:expr, $mods:expr, $prefix:expr, $action:expr) => {
            m.insert(
                BindingKey { mode: $mode, key: $key, modifiers: $mods, prefix: $prefix },
                $action,
            );
        };
    }

    // Ctrl-C quits from any mode
    for &mode in &[Normal, Insert, Visual, CommandLine] {
        bind!(mode, KeyCode::Char('c'), ctrl, None, Quit);
    }

    // Normal mode
    bind!(Normal, KeyCode::Char('q'), none, None, Quit);
    bind!(Normal, KeyCode::Char('i'), none, None, EnterMode(Insert));
    bind!(Normal, KeyCode::Char('v'), none, None, EnterMode(Visual));
    bind!(Normal, KeyCode::Char(':'), none, None, EnterCommandMode);
    bind!(Normal, KeyCode::Left, none, None, Edit("move_left"));
    bind!(Normal, KeyCode::Char('h'), none, None, Edit("move_left"));
    bind!(Normal, KeyCode::Right, none, None, Edit("move_right"));
    bind!(Normal, KeyCode::Char('l'), none, None, Edit("move_right"));
    bind!(Normal, KeyCode::Up, none, None, Edit("move_up"));
    bind!(Normal, KeyCode::Char('k'), none, None, Edit("move_up"));
    bind!(Normal, KeyCode::Down, none, None, Edit("move_down"));
    bind!(Normal, KeyCode::Char('j'), none, None, Edit("move_down"));

    // Visual mode
    bind!(Visual, KeyCode::Esc, none, None, CollapseAndEnterNormal);
    bind!(Visual, KeyCode::Char('v'), none, None, CollapseAndEnterNormal);
    bind!(Visual, KeyCode::Char(':'), none, None, EnterCommandMode);
    bind!(Visual, KeyCode::Left, none, None, Edit("move_left_and_modify_selection"));
    bind!(Visual, KeyCode::Char('h'), none, None, Edit("move_left_and_modify_selection"));
    bind!(Visual, KeyCode::Right, none, None, Edit("move_right_and_modify_selection"));
    bind!(Visual, KeyCode::Char('l'), none, None, Edit("move_right_and_modify_selection"));
    bind!(Visual, KeyCode::Up, none, None, Edit("move_up_and_modify_selection"));
    bind!(Visual, KeyCode::Char('k'), none, None, Edit("move_up_and_modify_selection"));
    bind!(Visual, KeyCode::Down, none, None, Edit("move_down_and_modify_selection"));
    bind!(Visual, KeyCode::Char('j'), none, None, Edit("move_down_and_modify_selection"));

    // Insert mode
    bind!(Insert, KeyCode::Esc, none, None, EnterMode(Normal));
    bind!(Insert, KeyCode::Left, none, None, Edit("move_left"));
    bind!(Insert, KeyCode::Right, none, None, Edit("move_right"));
    bind!(Insert, KeyCode::Up, none, None, Edit("move_up"));
    bind!(Insert, KeyCode::Down, none, None, Edit("move_down"));
    bind!(Insert, KeyCode::Enter, none, None, Edit("insert_newline"));
    bind!(Insert, KeyCode::Backspace, none, None, DeleteBackward);

    // CommandLine mode
    bind!(CommandLine, KeyCode::Esc, none, None, EnterMode(Normal));
    bind!(CommandLine, KeyCode::Enter, none, None, ExecuteCommand);
    bind!(CommandLine, KeyCode::Backspace, none, None, CommandBackspace);

    m
}

struct ChannelReader {
    rx: Receiver<String>,
}

impl ReadTransport for ChannelReader {
    fn read_message(&mut self, buf: &mut String) -> io::Result<usize> {
        match self.rx.recv() {
            Ok(message) => {
                let len = message.len();
                buf.push_str(&message);
                Ok(len)
            }
            Err(_) => Ok(0),
        }
    }
}

struct ChannelWriter {
    tx: Sender<String>,
}

impl WriteTransport for ChannelWriter {
    fn write_message(&mut self, data: &[u8]) -> io::Result<()> {
        let message = String::from_utf8(data.to_vec())
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        self.tx
            .send(message)
            .map_err(|err| io::Error::new(io::ErrorKind::BrokenPipe, err.to_string()))
    }
}

/// L109: events produced by the xi reader thread and consumed by the main thread.
#[derive(Debug)]
enum BackendEvent {
    Update(CoreUpdate),
    Alert(String),
    /// xi hint: scroll viewport so (line, col) is visible.
    ScrollTo { line: usize, col: usize },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CachedLine {
    text: String,
    cursors: Vec<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LineSlot {
    Known(CachedLine),
    Invalid,
}

#[derive(Debug, Deserialize)]
struct CoreNotificationParams {
    update: CoreUpdate,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct CoreUpdate {
    ops: Vec<CoreUpdateOp>,
    pristine: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct CoreUpdateOp {
    op: CoreUpdateKind,
    n: usize,
    #[serde(default)]
    lines: Vec<CoreLine>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum CoreUpdateKind {
    #[serde(rename = "ins")]
    Insert,
    Skip,
    Invalidate,
    Copy,
    Update,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
struct CoreLine {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    cursor: Vec<usize>,
}

#[derive(Debug)]
struct XiClient {
    path: Option<PathBuf>,
    /// Sender half for commands going to xi core.
    tx: Sender<String>,
    /// L109: processed backend events from the xi reader thread.
    backend_rx: Receiver<BackendEvent>,
    view_id: String,
    pending_line_request: bool,
    line_cache: Vec<LineSlot>,
    lines: Vec<String>,
    cursor_line: usize,
    cursor_col: usize,
    pristine: bool,
    status_message: Option<String>,
    /// Last scroll range sent to xi, to avoid redundant notifications (L107).
    last_scroll: Option<(usize, usize)>,
}

impl XiClient {
    fn new(path: Option<PathBuf>) -> io::Result<Self> {
        let (to_core_tx, to_core_rx) = mpsc::channel::<String>();
        let (from_core_tx, from_core_rx) = mpsc::channel::<String>();
        let (backend_tx, backend_rx) = mpsc::channel::<BackendEvent>();

        thread::spawn(move || {
            let mut core = XiCore::new();
            let mut rpc_loop = RpcLoop::new(ChannelWriter { tx: from_core_tx });
            let _ = rpc_loop.mainloop(|| ChannelReader { rx: to_core_rx }, &mut core);
        });

        // ── Synchronous init: client_started + new_view ──────────────────────
        send_rpc_notification(&to_core_tx, "client_started", json!({}))?;

        let new_view_id: u64 = 1;
        send_rpc_request(
            &to_core_tx,
            new_view_id,
            "new_view",
            json!({ "file_path": path.as_ref().map(|p| p.to_string_lossy().to_string()) }),
        )?;

        // Block until we get the new_view response; handle any measure_width
        // requests that arrive first.
        let view_id = block_for_response(&from_core_rx, &to_core_tx, new_view_id)?;
        let view_id = view_id
            .as_str()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "new_view returned non-string id")
            })?
            .to_owned();

        // Drain initial update notifications (xi sends them immediately).
        let init_events = drain_sync_notifications(&from_core_rx, &to_core_tx);

        // ── Hand from_core_rx to the background reader thread (L109) ─────────
        let tx_clone = to_core_tx.clone();
        thread::spawn(move || xi_reader_thread(from_core_rx, tx_clone, backend_tx));

        let mut client = Self {
            path,
            tx: to_core_tx,
            backend_rx,
            view_id,
            pending_line_request: false,
            line_cache: Vec::new(),
            lines: Vec::new(),
            cursor_line: 0,
            cursor_col: 0,
            pristine: true,
            status_message: None,
            last_scroll: None,
        };

        for event in init_events {
            client.apply_backend_event(event)?;
        }
        // Wait for any pending request_lines responses from xi to arrive through
        // the reader thread. Loop until the cache is fully populated or we time out.
        client.pump_init()?;
        Ok(client)
    }

    fn title(&self) -> String {
        self.path
            .as_ref()
            .and_then(|path| path.file_name())
            .and_then(|name| name.to_str())
            .unwrap_or("[scratch]")
            .to_owned()
    }

    fn save(&mut self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "scratch buffer has no path"));
        };

        self.send_notification(
            "save",
            json!({
                "view_id": self.view_id,
                "file_path": path.to_string_lossy().to_string(),
            }),
        )?;
        self.status_message = Some(format!("saved {}", path.display()));
        Ok(())
    }

    /// L109: send edit command to xi; the response arrives asynchronously via the reader thread.
    fn send_edit(&mut self, method: &str, params: Value) -> io::Result<()> {
        self.send_notification(
            "edit",
            json!({
                "view_id": self.view_id,
                "method": method,
                "params": params,
            }),
        )
    }

    /// L109: drain all pending backend events; called each frame from the main loop.
    fn drain_events(&mut self) -> io::Result<()> {
        while let Ok(event) = self.backend_rx.try_recv() {
            self.apply_backend_event(event)?;
        }
        Ok(())
    }

    /// Blocking init drain: wait until all invalidated lines are resolved or time out.
    /// Used once during `new()` to ensure the buffer is fully populated before returning.
    fn pump_init(&mut self) -> io::Result<()> {
        use std::sync::mpsc::RecvTimeoutError;
        loop {
            if invalid_line_ranges(&self.line_cache).is_empty() {
                break;
            }
            match self.backend_rx.recv_timeout(Duration::from_millis(20)) {
                Ok(event) => {
                    self.apply_backend_event(event)?;
                    while let Ok(event) = self.backend_rx.try_recv() {
                        self.apply_backend_event(event)?;
                    }
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        Ok(())
    }

    /// Apply one backend event produced by the xi reader thread.
    fn apply_backend_event(&mut self, event: BackendEvent) -> io::Result<()> {
        match event {
            BackendEvent::Update(update) => {
                self.pending_line_request = false;
                self.apply_update(update)?;
                self.request_invalidated_lines()?;
            }
            // L105: xi tells us where the cursor/viewport should be; trust it
            // and let scroll_into_view (called in run_app) handle the viewport.
            BackendEvent::ScrollTo { line, col } => {
                self.cursor_line = line;
                self.cursor_col = col;
                self.clamp_cursor();
            }
            BackendEvent::Alert(msg) => {
                self.status_message = Some(msg);
            }
        }
        Ok(())
    }

    /// L107: notify xi which lines are currently visible so it can prioritise updates.
    fn notify_scroll(&mut self, first_line: usize, last_line: usize) -> io::Result<()> {
        let range = (first_line, last_line);
        if self.last_scroll == Some(range) || self.view_id.is_empty() {
            return Ok(());
        }
        self.last_scroll = Some(range);
        self.send_notification(
            "edit",
            json!({
                "view_id": self.view_id,
                "method": "scroll",
                "params": [first_line, last_line],
            }),
        )
    }

    fn clamp_cursor(&mut self) {
        if self.lines.is_empty() {
            self.cursor_line = 0;
            self.cursor_col = 0;
            return;
        }

        self.cursor_line = self.cursor_line.min(self.lines.len().saturating_sub(1));
        self.cursor_col = previous_char_boundary(&self.lines[self.cursor_line], self.cursor_col);
    }

    fn send_notification(&self, method: &str, params: Value) -> io::Result<()> {
        self.send_message(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
    }

    fn send_message(&self, value: Value) -> io::Result<()> {
        let message = serde_json::to_string(&value)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        self.tx
            .send(message)
            .map_err(|err| io::Error::new(io::ErrorKind::BrokenPipe, err.to_string()))
    }

    fn apply_update(&mut self, update: CoreUpdate) -> io::Result<()> {
        let previous = std::mem::take(&mut self.line_cache);
        let mut next_cache = Vec::new();
        let mut source_index = 0;

        self.pristine = update.pristine;

        for op in update.ops {
            match op.op {
                CoreUpdateKind::Insert => {
                    if op.lines.len() != op.n {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "insert op length mismatch: expected {}, got {}",
                                op.n,
                                op.lines.len()
                            ),
                        ));
                    }
                    next_cache.extend(op.lines.into_iter().map(LineSlot::from));
                }
                CoreUpdateKind::Skip => {
                    source_index = checked_advance(source_index, op.n, previous.len(), "skip")?;
                }
                CoreUpdateKind::Invalidate => {
                    next_cache.extend(std::iter::repeat_n(LineSlot::Invalid, op.n));
                }
                CoreUpdateKind::Copy => {
                    let end = checked_advance(source_index, op.n, previous.len(), "copy")?;
                    next_cache.extend(previous[source_index..end].iter().cloned());
                    source_index = end;
                }
                CoreUpdateKind::Update => {
                    if op.lines.len() != op.n {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "update op length mismatch: expected {}, got {}",
                                op.n,
                                op.lines.len()
                            ),
                        ));
                    }

                    let end = checked_advance(source_index, op.n, previous.len(), "update")?;
                    for (slot, line) in
                        previous[source_index..end].iter().cloned().zip(op.lines.into_iter())
                    {
                        next_cache.push(slot.merge(line)?);
                    }
                    source_index = end;
                }
            }
        }

        self.line_cache = next_cache;
        self.rebuild_lines();
        self.sync_cursor_from_cache();
        Ok(())
    }

    fn rebuild_lines(&mut self) {
        self.lines = self
            .line_cache
            .iter()
            .map(|slot| match slot {
                LineSlot::Known(line) => line.text.clone(),
                LineSlot::Invalid => String::new(),
            })
            .collect();

        if matches!(self.line_cache.as_slice(), [LineSlot::Known(CachedLine { text, .. })] if text.is_empty())
        {
            self.lines.clear();
        }
    }

    fn sync_cursor_from_cache(&mut self) {
        for (line_index, slot) in self.line_cache.iter().enumerate() {
            let LineSlot::Known(line) = slot else {
                continue;
            };
            if let Some(&cursor_col) = line.cursors.first() {
                self.cursor_line = line_index;
                // L105: byte offset from xi payload; display-col conversion happens at render time
                self.cursor_col = previous_char_boundary(&line.text, cursor_col);
                self.clamp_cursor();
                return;
            }
        }

        self.clamp_cursor();
    }

    fn request_invalidated_lines(&mut self) -> io::Result<()> {
        if self.pending_line_request || self.view_id.is_empty() {
            return Ok(());
        }

        let invalid_ranges = invalid_line_ranges(&self.line_cache);
        if invalid_ranges.is_empty() {
            return Ok(());
        }

        for (start, end) in invalid_ranges {
            self.send_notification(
                "edit",
                json!({
                    "view_id": self.view_id,
                    "method": "request_lines",
                    "params": [start, end],
                }),
            )?;
        }
        self.pending_line_request = true;
        Ok(())
    }

    /// Test helper: sleep briefly then drain all pending events.
    #[cfg(test)]
    fn pump(&mut self) -> io::Result<()> {
        use std::sync::mpsc::RecvTimeoutError;
        for _ in 0..6 {
            match self.backend_rx.recv_timeout(Duration::from_millis(10)) {
                Ok(event) => {
                    self.apply_backend_event(event)?;
                    while let Ok(event) = self.backend_rx.try_recv() {
                        self.apply_backend_event(event)?;
                    }
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        Ok(())
    }
}

impl PartialEq for XiClient {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
            && self.view_id == other.view_id
            && self.lines == other.lines
            && self.cursor_line == other.cursor_line
            && self.cursor_col == other.cursor_col
            && self.pristine == other.pristine
            && self.status_message == other.status_message
    }
}

impl Eq for XiClient {}

impl From<CoreLine> for LineSlot {
    fn from(line: CoreLine) -> Self {
        LineSlot::Known(CachedLine { text: normalize_line_text(line.text), cursors: line.cursor })
    }
}

impl LineSlot {
    fn merge(self, update: CoreLine) -> io::Result<Self> {
        match self {
            LineSlot::Known(mut line) => {
                if let Some(text) = update.text {
                    line.text = text;
                }
                line.cursors = update.cursor;
                Ok(LineSlot::Known(line))
            }
            LineSlot::Invalid => {
                if update.text.is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "update op cannot patch invalid line without text",
                    ));
                }
                Ok(LineSlot::from(update))
            }
        }
    }
}

fn parse_response(message: Value) -> io::Result<Value> {
    if let Some(result) = message.get("result") {
        return Ok(result.clone());
    }

    if let Some(error) = message.get("error") {
        let message =
            error.get("message").and_then(Value::as_str).unwrap_or("rpc error").to_owned();
        return Err(io::Error::other(message));
    }

    Err(io::Error::new(io::ErrorKind::InvalidData, "rpc response missing result and error"))
}

fn previous_char_boundary(line: &str, col: usize) -> usize {
    let mut col = col.min(line.len());
    while col > 0 && !line.is_char_boundary(col) {
        col -= 1;
    }
    col
}

fn normalize_line_text(text: Option<String>) -> String {
    let Some(text) = text else {
        return String::new();
    };
    let text = text.strip_suffix('\n').unwrap_or(&text);
    let text = text.strip_suffix('\r').unwrap_or(text);
    text.to_owned()
}

fn checked_advance(current: usize, amount: usize, len: usize, op: &str) -> io::Result<usize> {
    let next = current.checked_add(amount).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, format!("{op} op overflowed source index"))
    })?;
    if next > len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{op} op exceeded cached line count"),
        ));
    }
    Ok(next)
}

fn invalid_line_ranges(line_cache: &[LineSlot]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = None;

    for (index, slot) in line_cache.iter().enumerate() {
        match (slot, start) {
            (LineSlot::Invalid, None) => start = Some(index),
            (LineSlot::Known(_), Some(range_start)) => {
                ranges.push((range_start, index));
                start = None;
            }
            _ => {}
        }
    }

    if let Some(range_start) = start {
        ranges.push((range_start, line_cache.len()));
    }

    ranges
}

// ── L109: Xi reader thread ────────────────────────────────────────────────────

/// Background thread: reads raw JSON from xi core, handles synchronous frontend
/// requests (measure_width) inline, and forwards notifications as BackendEvents.
fn xi_reader_thread(rx: Receiver<String>, tx: Sender<String>, backend_tx: Sender<BackendEvent>) {
    while let Ok(raw) = rx.recv() {
        let msg: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(method) = msg.get("method").and_then(Value::as_str) {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            if let Some(id) = msg.get("id").cloned() {
                // Core → frontend synchronous request (e.g. measure_width)
                respond_to_frontend_request(method, params, id, &tx);
            } else {
                // Notification → turn into a BackendEvent
                if let Some(event) = parse_notification(method, params) {
                    let _ = backend_tx.send(event);
                }
            }
        }
        // Responses to our own requests are handled in block_for_response before
        // the reader thread starts, so we never see them here.
    }
}

/// Send the JSON-RPC response for a core → frontend request.
fn respond_to_frontend_request(method: &str, params: Value, id: Value, tx: &Sender<String>) {
    let response = match method {
        // L110: use actual display width instead of char count
        "measure_width" => {
            let widths = params
                .as_array()
                .into_iter()
                .flatten()
                .map(|req| {
                    req.get("strings")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .map(|text| {
                            Value::from(
                                UnicodeWidthStr::width(text.as_str().unwrap_or_default()) as f64,
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            json!({ "jsonrpc": "2.0", "id": id, "result": widths })
        }
        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("unsupported frontend request: {method}") }
        }),
    };
    if let Ok(raw) = serde_json::to_string(&response) {
        let _ = tx.send(raw);
    }
}

/// Parse a xi core notification into a BackendEvent.
fn parse_notification(method: &str, params: Value) -> Option<BackendEvent> {
    match method {
        "update" => {
            let p = serde_json::from_value::<CoreNotificationParams>(params).ok()?;
            Some(BackendEvent::Update(p.update))
        }
        "scroll_to" => {
            let line = params.get("line").and_then(Value::as_u64)? as usize;
            let col = params.get("col").and_then(Value::as_u64)? as usize;
            Some(BackendEvent::ScrollTo { line, col })
        }
        "alert" => {
            let msg = params.get("msg").and_then(Value::as_str)?.to_owned();
            Some(BackendEvent::Alert(msg))
        }
        _ => None,
    }
}

// ── Init helpers (used before the reader thread starts) ──────────────────────

fn send_rpc_notification(tx: &Sender<String>, method: &str, params: Value) -> io::Result<()> {
    let raw = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    }))
    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    tx.send(raw).map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))
}

fn send_rpc_request(
    tx: &Sender<String>,
    id: u64,
    method: &str,
    params: Value,
) -> io::Result<()> {
    let raw = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    }))
    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    tx.send(raw).map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))
}

/// Block until we receive the JSON-RPC response for `expected_id`.
/// Handles measure_width requests and discards other notifications inline.
fn block_for_response(
    rx: &Receiver<String>,
    tx: &Sender<String>,
    expected_id: u64,
) -> io::Result<Value> {
    loop {
        let raw = rx.recv().map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))?;
        let msg: Value = serde_json::from_str(&raw)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        if let Some(method) = msg.get("method").and_then(Value::as_str) {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            if let Some(id) = msg.get("id").cloned() {
                respond_to_frontend_request(method, params, id, tx);
            }
            // notifications during init are collected by drain_sync_notifications
            continue;
        }

        if msg.get("id").and_then(Value::as_u64) == Some(expected_id) {
            return parse_response(msg);
        }
    }
}

/// Drain pending notifications from xi right after `new_view`.
/// Handles measure_width requests inline; returns other events as BackendEvents.
fn drain_sync_notifications(
    rx: &Receiver<String>,
    tx: &Sender<String>,
) -> Vec<BackendEvent> {
    let mut events = Vec::new();
    while let Ok(raw) = rx.recv_timeout(Duration::from_millis(20)) {
        let msg: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(method) = msg.get("method").and_then(Value::as_str) {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            if let Some(id) = msg.get("id").cloned() {
                respond_to_frontend_request(method, params, id, tx);
            } else if let Some(event) = parse_notification(method, params) {
                events.push(event);
            }
        }
    }
    events
}

// ── L110: display-width helpers ───────────────────────────────────────────────

/// Convert a byte offset within `line` to the number of display columns
/// needed to reach that position, using Unicode display widths.
fn byte_col_to_display_col(line: &str, byte_col: usize) -> usize {
    let safe = previous_char_boundary(line, byte_col.min(line.len()));
    UnicodeWidthStr::width(&line[..safe])
}

/// Convert a display-column offset to the byte offset of the first character
/// that starts at or after that visual column.
fn display_col_to_byte(line: &str, display_col: usize) -> usize {
    let mut col = 0usize;
    for (byte_idx, ch) in line.char_indices() {
        if col >= display_col {
            return byte_idx;
        }
        col += UnicodeWidthChar::width(ch).unwrap_or(0);
    }
    line.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn scratch_title_is_default() {
        let app = App::from_path(None).unwrap();

        assert_eq!(app.backend.title(), "[scratch]");
    }

    #[test]
    fn normal_q_quits() {
        let mut app = App::from_path(None).unwrap();

        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));

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
        let mut client = test_client();
        client.line_cache = vec![
            LineSlot::Known(CachedLine { text: "alpha".into(), cursors: Vec::new() }),
            LineSlot::Known(CachedLine { text: "beta".into(), cursors: vec![2] }),
            LineSlot::Known(CachedLine { text: "gamma".into(), cursors: Vec::new() }),
        ];
        client.rebuild_lines();

        client
            .apply_update(CoreUpdate {
                pristine: false,
                ops: vec![
                    CoreUpdateOp { op: CoreUpdateKind::Copy, n: 1, lines: Vec::new() },
                    CoreUpdateOp {
                        op: CoreUpdateKind::Update,
                        n: 1,
                        lines: vec![CoreLine { text: None, cursor: vec![1] }],
                    },
                    CoreUpdateOp {
                        op: CoreUpdateKind::Insert,
                        n: 1,
                        lines: vec![CoreLine { text: Some("delta".into()), cursor: Vec::new() }],
                    },
                    CoreUpdateOp { op: CoreUpdateKind::Invalidate, n: 2, lines: Vec::new() },
                ],
            })
            .unwrap();

        assert_eq!(client.lines, vec!["alpha", "beta", "delta", "", ""]);
        assert_eq!((client.cursor_line, client.cursor_col), (1, 1));
        assert_eq!(invalid_line_ranges(&client.line_cache), vec![(3, 5)]);
        assert!(!client.pristine);
    }

    #[test]
    fn open_file_bootstraps_full_buffer_from_updates() {
        let path = unique_temp_path("ee-tui-open");
        let contents = (0..24).map(|i| format!("line-{i}")).collect::<Vec<_>>().join("\n");
        fs::write(&path, &contents).unwrap();

        let app = App::from_path(Some(path.clone())).unwrap();

        fs::remove_file(&path).unwrap();
        assert_eq!(
            app.backend.lines,
            contents.split('\n').map(ToOwned::to_owned).collect::<Vec<_>>()
        );
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

    fn unique_temp_path(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        env::temp_dir().join(format!("{prefix}-{nanos}.txt"))
    }

    fn test_client() -> XiClient {
        let (tx, _rx) = mpsc::channel();
        let (_backend_tx, backend_rx) = mpsc::channel();
        XiClient {
            path: None,
            tx,
            backend_rx,
            view_id: String::new(),
            pending_line_request: false,
            line_cache: Vec::new(),
            lines: Vec::new(),
            cursor_line: 0,
            cursor_col: 0,
            pristine: true,
            status_message: None,
            last_scroll: None,
        }
    }

    // ── New tests for L106, L108, L110 ──────────────────────────────────────

    #[test]
    fn byte_col_to_display_col_ascii() {
        assert_eq!(byte_col_to_display_col("hello", 3), 3);
    }

    #[test]
    fn byte_col_to_display_col_wide_char() {
        // '日' is 3 UTF-8 bytes and 2 display columns
        let s = "日本";
        assert_eq!(byte_col_to_display_col(s, 3), 2); // after first kanji
        assert_eq!(byte_col_to_display_col(s, 6), 4); // after second kanji
    }

    #[test]
    fn display_col_to_byte_wide_char() {
        let s = "日本";
        assert_eq!(display_col_to_byte(s, 0), 0);
        assert_eq!(display_col_to_byte(s, 2), 3); // start of second kanji
        assert_eq!(display_col_to_byte(s, 4), 6); // end of string
    }

    #[test]
    fn viewport_scrolls_down_when_cursor_leaves_view() {
        let mut app = App::from_path(None).unwrap();
        // Place cursor below visible area
        app.backend.cursor_line = 25;
        app.scroll_into_view(20);
        assert_eq!(app.viewport.top_line, 6); // 25 + 1 - 20
    }

    #[test]
    fn viewport_scrolls_up_when_cursor_above_top() {
        let mut app = App::from_path(None).unwrap();
        app.viewport.top_line = 10;
        app.backend.cursor_line = 5;
        app.scroll_into_view(20);
        assert_eq!(app.viewport.top_line, 5);
    }

    #[test]
    fn bindings_table_has_normal_hjkl() {
        let b = bindings();
        let lookup = |key| {
            b.get(&BindingKey {
                mode: Mode::Normal,
                key,
                modifiers: KeyModifiers::NONE,
                prefix: None,
            })
            .cloned()
        };
        assert_eq!(lookup(KeyCode::Char('h')), Some(Action::Edit("move_left")));
        assert_eq!(lookup(KeyCode::Char('l')), Some(Action::Edit("move_right")));
        assert_eq!(lookup(KeyCode::Char('k')), Some(Action::Edit("move_up")));
        assert_eq!(lookup(KeyCode::Char('j')), Some(Action::Edit("move_down")));
    }

    #[test]
    fn count_digits_accumulate_in_normal_mode() {
        let mut app = App::from_path(None).unwrap();
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE)));
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE)));
        assert_eq!(app.input_state.count(), 35);
    }
}
