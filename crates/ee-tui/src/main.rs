use std::env;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
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
use serde_json::{Value, json};
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
        app.backend.pump()?;
        terminal.draw(|frame| ui(frame, app))?;
        if event::poll(Duration::from_millis(100))? {
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
    let lines = (0..height)
        .map(|i| {
            let number = if i < line_count { i + 1 } else { 0 };
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
    let text = if app.backend.lines.is_empty() {
        vec![Line::from(Span::styled(
            "empty buffer",
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        ))]
    } else {
        app.backend.lines.iter().map(|line| Line::from(line.as_str())).collect::<Vec<_>>()
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
}

impl App {
    fn from_path(path: Option<PathBuf>) -> io::Result<Self> {
        Ok(Self {
            backend: XiClient::new(path)?,
            mode: Mode::Normal,
            command_buffer: String::new(),
            should_quit: false,
        })
    }

    fn handle_event(&mut self, event: Event) {
        let Event::Key(key) = event else {
            return;
        };

        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return;
        }

        match (self.mode, key) {
            (_, KeyEvent { code: KeyCode::Char('c'), modifiers: KeyModifiers::CONTROL, .. }) => {
                self.should_quit = true;
            }
            (
                Mode::Insert | Mode::Visual | Mode::CommandLine,
                KeyEvent { code: KeyCode::Esc, .. },
            ) => {
                if self.mode == Mode::Visual {
                    let _ = self.backend.send_edit("collapse_selections", json!([]));
                }
                self.enter_normal_mode();
            }
            (Mode::Normal, KeyEvent { code: KeyCode::Char('q'), .. }) => {
                self.should_quit = true;
            }
            (Mode::Normal, KeyEvent { code: KeyCode::Char('i'), .. }) => {
                self.mode = Mode::Insert;
            }
            (Mode::Normal, KeyEvent { code: KeyCode::Char('v'), .. }) => {
                self.mode = Mode::Visual;
            }
            (Mode::Normal | Mode::Visual, KeyEvent { code: KeyCode::Char(':'), .. }) => {
                self.mode = Mode::CommandLine;
                self.command_buffer.clear();
            }
            (Mode::Normal, KeyEvent { code: KeyCode::Left | KeyCode::Char('h'), .. }) => {
                let _ = self.backend.send_edit("move_left", json!([]));
            }
            (Mode::Normal, KeyEvent { code: KeyCode::Right | KeyCode::Char('l'), .. }) => {
                let _ = self.backend.send_edit("move_right", json!([]));
            }
            (Mode::Normal, KeyEvent { code: KeyCode::Up | KeyCode::Char('k'), .. }) => {
                let _ = self.backend.send_edit("move_up", json!([]));
            }
            (Mode::Normal, KeyEvent { code: KeyCode::Down | KeyCode::Char('j'), .. }) => {
                let _ = self.backend.send_edit("move_down", json!([]));
            }
            (Mode::Visual, KeyEvent { code: KeyCode::Left | KeyCode::Char('h'), .. }) => {
                let _ = self.backend.send_edit("move_left_and_modify_selection", json!([]));
            }
            (Mode::Visual, KeyEvent { code: KeyCode::Right | KeyCode::Char('l'), .. }) => {
                let _ = self.backend.send_edit("move_right_and_modify_selection", json!([]));
            }
            (Mode::Visual, KeyEvent { code: KeyCode::Up | KeyCode::Char('k'), .. }) => {
                let _ = self.backend.send_edit("move_up_and_modify_selection", json!([]));
            }
            (Mode::Visual, KeyEvent { code: KeyCode::Down | KeyCode::Char('j'), .. }) => {
                let _ = self.backend.send_edit("move_down_and_modify_selection", json!([]));
            }
            (Mode::Visual, KeyEvent { code: KeyCode::Char('v'), .. }) => {
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.enter_normal_mode();
            }
            (Mode::Insert, KeyEvent { code: KeyCode::Left, .. }) => {
                let _ = self.backend.send_edit("move_left", json!([]));
            }
            (Mode::Insert, KeyEvent { code: KeyCode::Right, .. }) => {
                let _ = self.backend.send_edit("move_right", json!([]));
            }
            (Mode::Insert, KeyEvent { code: KeyCode::Up, .. }) => {
                let _ = self.backend.send_edit("move_up", json!([]));
            }
            (Mode::Insert, KeyEvent { code: KeyCode::Down, .. }) => {
                let _ = self.backend.send_edit("move_down", json!([]));
            }
            (Mode::Insert, KeyEvent { code: KeyCode::Enter, .. }) => {
                let _ = self.backend.send_edit("insert_newline", json!([]));
            }
            (Mode::Insert, KeyEvent { code: KeyCode::Backspace, .. }) => {
                let _ = self.backend.send_edit("delete_backward", json!([]));
            }
            (Mode::Insert, KeyEvent { code: KeyCode::Char(ch), modifiers, .. })
                if !modifiers.contains(KeyModifiers::CONTROL)
                    && !modifiers.contains(KeyModifiers::ALT) =>
            {
                let _ = self.backend.send_edit("insert", json!({ "chars": ch.to_string() }));
            }
            (Mode::CommandLine, KeyEvent { code: KeyCode::Enter, .. }) => {
                self.execute_command();
            }
            (Mode::CommandLine, KeyEvent { code: KeyCode::Backspace, .. }) => {
                self.command_buffer.pop();
            }
            (Mode::CommandLine, KeyEvent { code: KeyCode::Char(ch), modifiers, .. })
                if !modifiers.contains(KeyModifiers::CONTROL)
                    && !modifiers.contains(KeyModifiers::ALT) =>
            {
                self.command_buffer.push(ch);
            }
            _ => {}
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
        let x = (editor_area.x + self.backend.cursor_col as u16).min(max_x);
        let y = (editor_area.y + self.backend.cursor_line as u16).min(max_y);
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug)]
struct XiClient {
    path: Option<PathBuf>,
    tx: Sender<String>,
    rx: Receiver<String>,
    view_id: String,
    next_request_id: u64,
    request_depth: usize,
    pending_refresh: bool,
    lines: Vec<String>,
    cursor_line: usize,
    cursor_col: usize,
    pristine: bool,
    status_message: Option<String>,
}

impl XiClient {
    fn new(path: Option<PathBuf>) -> io::Result<Self> {
        let (to_core_tx, to_core_rx) = mpsc::channel::<String>();
        let (from_core_tx, from_core_rx) = mpsc::channel::<String>();

        thread::spawn(move || {
            let mut core = XiCore::new();
            let mut rpc_loop = RpcLoop::new(ChannelWriter { tx: from_core_tx });
            let _ = rpc_loop.mainloop(|| ChannelReader { rx: to_core_rx }, &mut core);
        });

        let mut client = Self {
            path,
            tx: to_core_tx,
            rx: from_core_rx,
            view_id: String::new(),
            next_request_id: 1,
            request_depth: 0,
            pending_refresh: false,
            lines: Vec::new(),
            cursor_line: 0,
            cursor_col: 0,
            pristine: true,
            status_message: None,
        };

        client.send_notification("client_started", json!({}))?;
        let new_view_params = json!({
            "file_path": client
                .path
                .as_ref()
                .map(|path| path.to_string_lossy().to_string())
        });
        let view_id = client.request("new_view", new_view_params)?;
        client.view_id = view_id
            .as_str()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "new_view returned non-string id")
            })?
            .to_owned();
        client.refresh_contents()?;
        client.pump()?;
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

    fn send_edit(&mut self, method: &str, params: Value) -> io::Result<()> {
        self.apply_local_cursor_hint(method, &params);
        self.send_notification(
            "edit",
            json!({
                "view_id": self.view_id,
                "method": method,
                "params": params,
            }),
        )?;

        for _ in 0..10 {
            match self.rx.recv_timeout(Duration::from_millis(5)) {
                Ok(raw) => {
                    let _ = self.handle_raw_message(&raw, None)?;
                    if self.pending_refresh {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        if self.pending_refresh {
            self.finish_pending_refresh()?;
        } else {
            self.refresh_contents()?;
        }
        self.pump()
    }

    fn pump(&mut self) -> io::Result<()> {
        loop {
            match self.rx.recv_timeout(Duration::from_millis(5)) {
                Ok(raw) => {
                    let _ = self.handle_raw_message(&raw, None)?;
                    while let Ok(raw) = self.rx.try_recv() {
                        let _ = self.handle_raw_message(&raw, None)?;
                    }
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        self.finish_pending_refresh()
    }

    fn refresh_contents(&mut self) -> io::Result<()> {
        if self.view_id.is_empty() {
            return Ok(());
        }

        let contents = self.request("debug_get_contents", json!({ "view_id": self.view_id }))?;
        let text = contents.as_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "debug_get_contents returned non-string")
        })?;

        self.lines = if text.is_empty() {
            Vec::new()
        } else {
            text.split('\n').map(ToOwned::to_owned).collect()
        };
        self.clamp_cursor();
        Ok(())
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

    fn finish_pending_refresh(&mut self) -> io::Result<()> {
        while self.pending_refresh && self.request_depth == 0 {
            self.pending_refresh = false;
            self.refresh_contents()?;
        }
        Ok(())
    }

    fn send_notification(&self, method: &str, params: Value) -> io::Result<()> {
        self.send_message(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
    }

    fn request(&mut self, method: &str, params: Value) -> io::Result<Value> {
        let id = self.next_request_id;
        self.next_request_id += 1;
        self.request_depth += 1;
        self.send_message(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;

        let result = loop {
            let raw = self
                .rx
                .recv()
                .map_err(|err| io::Error::new(io::ErrorKind::BrokenPipe, err.to_string()))?;
            if let Some(response) = self.handle_raw_message(&raw, Some(id))? {
                break response;
            }
        };

        self.request_depth = self.request_depth.saturating_sub(1);
        self.finish_pending_refresh()?;
        result
    }

    fn handle_raw_message(
        &mut self,
        raw: &str,
        expected_response_id: Option<u64>,
    ) -> io::Result<Option<io::Result<Value>>> {
        let message: Value = serde_json::from_str(raw)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

        if let Some(method) = message.get("method").and_then(Value::as_str) {
            if message.get("id").is_some() {
                self.handle_core_request(
                    method,
                    message.get("params").cloned().unwrap_or(Value::Null),
                    message.get("id").cloned().unwrap_or(Value::Null),
                )?;
            } else {
                self.handle_core_notification(
                    method,
                    message.get("params").cloned().unwrap_or(Value::Null),
                )?;
            }
            return Ok(None);
        }

        let response_id = message.get("id").and_then(Value::as_u64);
        if response_id == expected_response_id {
            return Ok(Some(parse_response(message)));
        }

        Ok(None)
    }

    fn handle_core_notification(&mut self, method: &str, params: Value) -> io::Result<()> {
        match method {
            "update" => {
                if let Some(update) = params.get("update") {
                    self.pristine =
                        update.get("pristine").and_then(Value::as_bool).unwrap_or(self.pristine);
                }
                self.pending_refresh = true;
            }
            "scroll_to" => {}
            "alert" => {
                self.status_message =
                    params.get("msg").and_then(Value::as_str).map(ToOwned::to_owned);
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_core_request(&mut self, method: &str, params: Value, id: Value) -> io::Result<()> {
        let response = match method {
            "measure_width" => {
                let widths =
                    params
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
                                        text.as_str().unwrap_or_default().chars().count() as f64
                                    )
                                })
                                .collect::<Vec<_>>()
                        })
                        .collect::<Vec<_>>();
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": widths,
                })
            }
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("unsupported frontend request: {method}"),
                }
            }),
        };
        self.send_message(response)
    }

    fn send_message(&self, value: Value) -> io::Result<()> {
        let message = serde_json::to_string(&value)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        self.tx
            .send(message)
            .map_err(|err| io::Error::new(io::ErrorKind::BrokenPipe, err.to_string()))
    }

    fn apply_local_cursor_hint(&mut self, method: &str, params: &Value) {
        let mut should_clamp = true;
        match method {
            "insert" => {
                let chars = params.get("chars").and_then(Value::as_str).unwrap_or_default();
                self.cursor_col += chars.len();
                should_clamp = false;
            }
            "insert_newline" => {
                self.cursor_line += 1;
                self.cursor_col = 0;
                should_clamp = false;
            }
            "delete_backward" => {
                if self.lines.is_empty() {
                    self.cursor_line = 0;
                    self.cursor_col = 0;
                } else if self.cursor_col > 0 {
                    self.cursor_col =
                        previous_char_boundary(&self.lines[self.cursor_line], self.cursor_col - 1);
                } else if self.cursor_line > 0 {
                    self.cursor_line -= 1;
                    self.cursor_col = self.lines[self.cursor_line].len();
                }
            }
            "move_left" => {
                if self.lines.is_empty() {
                    self.cursor_col = 0;
                } else if self.cursor_col > 0 {
                    self.cursor_col =
                        previous_char_boundary(&self.lines[self.cursor_line], self.cursor_col - 1);
                } else if self.cursor_line > 0 {
                    self.cursor_line -= 1;
                    self.cursor_col = self.lines[self.cursor_line].len();
                }
            }
            "move_right" => {
                if self.lines.is_empty() {
                    self.cursor_col = 0;
                } else {
                    let line_len = self.lines[self.cursor_line].len();
                    if self.cursor_col < line_len {
                        self.cursor_col =
                            next_char_boundary(&self.lines[self.cursor_line], self.cursor_col);
                    } else if self.cursor_line + 1 < self.lines.len() {
                        self.cursor_line += 1;
                        self.cursor_col = 0;
                    }
                }
            }
            "move_up" | "move_up_and_modify_selection" => {
                if self.cursor_line > 0 {
                    self.cursor_line -= 1;
                }
            }
            "move_down" | "move_down_and_modify_selection" => {
                if self.cursor_line + 1 < self.lines.len() {
                    self.cursor_line += 1;
                }
            }
            "move_left_and_modify_selection" => {
                self.apply_local_cursor_hint("move_left", &Value::Null)
            }
            "move_right_and_modify_selection" => {
                self.apply_local_cursor_hint("move_right", &Value::Null)
            }
            _ => {}
        }

        if should_clamp {
            self.clamp_cursor();
        }
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

fn next_char_boundary(line: &str, col: usize) -> usize {
    let mut col = (col + 1).min(line.len());
    while col < line.len() && !line.is_char_boundary(col) {
        col += 1;
    }
    col
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
}
