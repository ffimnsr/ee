use std::io;
use std::path::PathBuf;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Position, Rect};
use serde_json::json;

use crate::backend::XiClient;
use crate::keymap::{Action, BindingKey, bindings};
use crate::text::{
    byte_col_to_display_col, find_char_backward, find_char_forward, next_char_start,
    prev_char_start,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Mode {
    Normal,
    Insert,
    Visual,
    CommandLine,
    Search,
}

impl Mode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Mode::Normal => "NOR",
            Mode::Insert => "INS",
            Mode::Visual => "VIS",
            Mode::CommandLine => "CMD",
            Mode::Search => "SRC",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Viewport {
    pub(crate) top_line: usize,
    pub(crate) left_col: usize,
    pub(crate) target_col: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PendingCharFind {
    pub(crate) forward: bool,
    pub(crate) inclusive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct InputState {
    pub(crate) count_digits: Vec<u8>,
    pub(crate) prefix: Option<char>,
    pub(crate) pending_find: Option<PendingCharFind>,
}

impl InputState {
    pub(crate) fn count(&self) -> u32 {
        if self.count_digits.is_empty() {
            return 1;
        }
        self.count_digits
            .iter()
            .fold(0_u32, |acc, &digit| acc.saturating_mul(10).saturating_add(digit as u32))
    }

    pub(crate) fn reset(&mut self) {
        self.count_digits.clear();
        self.prefix = None;
        self.pending_find = None;
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct App {
    pub(crate) backend: XiClient,
    pub(crate) mode: Mode,
    pub(crate) command_buffer: String,
    pub(crate) should_quit: bool,
    pub(crate) viewport: Viewport,
    pub(crate) input_state: InputState,
}

impl App {
    pub(crate) fn from_path(path: Option<PathBuf>) -> io::Result<Self> {
        Ok(Self {
            backend: XiClient::new(path)?,
            mode: Mode::Normal,
            command_buffer: String::new(),
            should_quit: false,
            viewport: Viewport::default(),
            input_state: InputState::default(),
        })
    }

    pub(crate) fn handle_event(&mut self, event: Event) {
        let Event::Key(key) = event else {
            return;
        };

        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return;
        }

        let binding_key = BindingKey {
            mode: self.mode,
            key: key.code,
            modifiers: key.modifiers,
            prefix: self.input_state.prefix,
        };
        let action = bindings()
            .get(&binding_key)
            .or_else(|| {
                if key.modifiers != KeyModifiers::NONE {
                    bindings().get(&BindingKey { modifiers: KeyModifiers::NONE, ..binding_key })
                } else {
                    None
                }
            })
            .cloned();

        if let Some(action) = action {
            self.dispatch(action, key);
            if self.mode == Mode::Normal
                && !matches!(key.code, KeyCode::Char(c) if c.is_ascii_digit())
                && self.input_state.prefix.is_none()
                && self.input_state.pending_find.is_none()
            {
                self.input_state.reset();
            }
        } else {
            self.handle_default(key);
        }
    }

    fn dispatch(&mut self, action: Action, _key: KeyEvent) {
        match &action {
            Action::SetPrefix(_) | Action::PendingCharFind { .. } => {}
            _ => {
                self.input_state.prefix = None;
                self.input_state.pending_find = None;
            }
        }

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
            Action::EnterSearch => {
                self.mode = Mode::Search;
                self.command_buffer.clear();
                let _ = self.backend.send_edit(
                    "find",
                    json!({
                        "chars": "",
                        "case_sensitive": false,
                        "regex": false,
                        "whole_words": false
                    }),
                );
            }
            Action::ExecuteSearch => self.execute_search(),
            Action::Edit(method) => {
                let count = self.input_state.count();
                for _ in 0..count {
                    let _ = self.backend.send_edit(method, json!([]));
                }
            }
            Action::CollapseAndEnterNormal => {
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.enter_normal_mode();
            }
            Action::ExecuteCommand => self.execute_command(),
            Action::DeleteBackward => {
                let count = self.input_state.count();
                for _ in 0..count {
                    let _ = self.backend.send_edit("delete_backward", json!([]));
                }
            }
            Action::CommandBackspace => {
                self.command_buffer.pop();
            }
            Action::SearchBackspace => {
                self.command_buffer.pop();
                let chars = self.command_buffer.clone();
                let _ = self.backend.send_edit(
                    "find",
                    json!({
                        "chars": chars,
                        "case_sensitive": false,
                        "regex": false,
                        "whole_words": false
                    }),
                );
            }
            Action::FindNext => {
                let _ = self
                    .backend
                    .send_edit("find_next", json!({ "wrap_around": true, "allow_same": false }));
            }
            Action::FindPrevious => {
                let _ = self.backend.send_edit(
                    "find_previous",
                    json!({ "wrap_around": true, "allow_same": false }),
                );
            }
            Action::SetPrefix(c) => {
                self.input_state.prefix = Some(c);
            }
            Action::PendingCharFind { forward, inclusive } => {
                self.input_state.pending_find = Some(PendingCharFind { forward, inclusive });
            }
            Action::MatchingPair => self.jump_matching_bracket(),
        }
    }

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
                if let Some(find) = self.input_state.pending_find.take() {
                    let count = self.input_state.count();
                    self.input_state.reset();
                    for _ in 0..count {
                        self.jump_to_char(ch, find.forward, find.inclusive);
                    }
                    return;
                }
                if ch == '0' {
                    if self.input_state.count_digits.is_empty() {
                        let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                        self.input_state.reset();
                    } else {
                        self.input_state.count_digits.push(0);
                    }
                    return;
                }
                if let Some(digit) = ch.to_digit(10) {
                    self.input_state.count_digits.push(digit as u8);
                }
            }
            Mode::Search => {
                self.command_buffer.push(ch);
                let chars = self.command_buffer.clone();
                let _ = self.backend.send_edit(
                    "find",
                    json!({
                        "chars": chars,
                        "case_sensitive": false,
                        "regex": false,
                        "whole_words": false
                    }),
                );
            }
            Mode::Visual => {}
        }
    }

    pub(crate) fn cursor_position(&self, editor_area: Rect, prompt_area: Rect) -> Position {
        if matches!(self.mode, Mode::CommandLine | Mode::Search) {
            let max_x = prompt_area.right().saturating_sub(1);
            let x = (prompt_area.x + 1 + self.command_buffer.len() as u16).min(max_x);
            return Position::new(x, prompt_area.y);
        }

        let max_x = editor_area.right().saturating_sub(1);
        let max_y = editor_area.bottom().saturating_sub(1);

        let line =
            self.backend.lines.get(self.backend.cursor_line).map(|s| s.as_str()).unwrap_or("");
        let display_col = byte_col_to_display_col(line, self.backend.cursor_col);

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

    fn execute_search(&mut self) {
        let chars = self.command_buffer.clone();
        let _ = self.backend.send_edit(
            "find",
            json!({
                "chars": chars,
                "case_sensitive": false,
                "regex": false,
                "whole_words": false
            }),
        );
        let _ = self
            .backend
            .send_edit("find_next", json!({ "wrap_around": true, "allow_same": false }));
        self.enter_normal_mode();
    }

    fn jump_to_char(&mut self, target: char, forward: bool, inclusive: bool) {
        let line_idx = self.backend.cursor_line;
        let cursor_byte = self.backend.cursor_col;
        let line = match self.backend.lines.get(line_idx) {
            Some(line) => line.clone(),
            None => return,
        };

        let col_opt = if forward {
            find_char_forward(&line, cursor_byte, target).and_then(|pos| {
                if inclusive {
                    Some(pos)
                } else if pos > 0 {
                    Some(prev_char_start(&line, pos))
                } else {
                    None
                }
            })
        } else {
            find_char_backward(&line, cursor_byte, target)
                .map(|pos| if inclusive { pos } else { next_char_start(&line, pos) })
        };

        if let Some(col) = col_opt {
            let _ = self.backend.send_edit(
                "gesture",
                json!({ "line": line_idx as u64, "col": col as u64, "ty": "point_select" }),
            );
        }
    }

    fn jump_matching_bracket(&mut self) {
        let line_idx = self.backend.cursor_line;
        let cursor_byte = self.backend.cursor_col;
        let ch = match self.backend.lines.get(line_idx) {
            Some(line) => line[cursor_byte..].chars().next(),
            None => return,
        };
        let Some(ch) = ch else {
            return;
        };

        let (open, close, forward) = match ch {
            '(' => ('(', ')', true),
            ')' => ('(', ')', false),
            '[' => ('[', ']', true),
            ']' => ('[', ']', false),
            '{' => ('{', '}', true),
            '}' => ('{', '}', false),
            _ => return,
        };

        let lines = self.backend.lines.clone();
        if forward {
            let mut depth = 0_i32;
            'fwd: for (li, text) in lines.iter().enumerate().skip(line_idx) {
                let (scan, base) = if li == line_idx {
                    (&text[cursor_byte..], cursor_byte)
                } else {
                    (text.as_str(), 0)
                };
                for (off, c) in scan.char_indices() {
                    if c == open {
                        depth += 1;
                    } else if c == close {
                        depth -= 1;
                        if depth == 0 {
                            let col = base + off;
                            let _ = self.backend.send_edit(
                                "gesture",
                                json!({ "line": li as u64, "col": col as u64, "ty": "point_select" }),
                            );
                            break 'fwd;
                        }
                    }
                }
            }
        } else {
            let mut depth = 0_i32;
            'bwd: for li in (0..=line_idx).rev() {
                let text = &lines[li];
                let scan_end =
                    if li == line_idx { cursor_byte + ch.len_utf8() } else { text.len() };
                for (off, c) in
                    text[..scan_end].char_indices().collect::<Vec<_>>().into_iter().rev()
                {
                    if c == close {
                        depth += 1;
                    } else if c == open {
                        depth -= 1;
                        if depth == 0 {
                            let _ = self.backend.send_edit(
                                "gesture",
                                json!({ "line": li as u64, "col": off as u64, "ty": "point_select" }),
                            );
                            break 'bwd;
                        }
                    }
                }
            }
        }
    }

    fn execute_command(&mut self) {
        let command = self.command_buffer.trim();
        let mut parts = command.split_whitespace();
        match parts.next().unwrap_or_default() {
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
            "format" => {
                if let Err(err) =
                    self.backend.send_plugin_rpc("xi-lsp-plugin", "lsp.format_document", json!({}))
                {
                    self.backend.status_message = Some(format!("format failed: {err}"));
                }
            }
            "complete" => {
                if let Err(err) =
                    self.backend.send_plugin_rpc("xi-lsp-plugin", "lsp.completion", json!({}))
                {
                    self.backend.status_message = Some(format!("completion failed: {err}"));
                }
            }
            "definition" | "def" => {
                if let Err(err) =
                    self.backend.send_plugin_rpc("xi-lsp-plugin", "lsp.definition", json!({}))
                {
                    self.backend.status_message = Some(format!("definition failed: {err}"));
                }
            }
            "references" | "refs" => {
                if let Err(err) =
                    self.backend.send_plugin_rpc("xi-lsp-plugin", "lsp.references", json!({}))
                {
                    self.backend.status_message = Some(format!("references failed: {err}"));
                }
            }
            "codeaction" | "codeactions" => {
                let action_index = parts.next().and_then(|part| part.parse::<usize>().ok());
                if let Err(err) = self.backend.send_plugin_rpc(
                    "xi-lsp-plugin",
                    "lsp.code_action",
                    json!({ "index": action_index }),
                ) {
                    self.backend.status_message = Some(format!("code action failed: {err}"));
                }
            }
            other if !other.is_empty() => {
                self.backend.status_message = Some(format!("unknown command: {other}"));
            }
            _ => {}
        }
        self.enter_normal_mode();
    }

    pub(crate) fn scroll_into_view(&mut self, editor_height: usize) {
        if editor_height == 0 {
            return;
        }
        let cursor_line = self.backend.cursor_line;
        if cursor_line < self.viewport.top_line {
            self.viewport.top_line = cursor_line;
        } else if cursor_line >= self.viewport.top_line + editor_height {
            self.viewport.top_line = cursor_line + 1 - editor_height;
        }
        let line = self.backend.lines.get(cursor_line).map(|s| s.as_str()).unwrap_or("");
        self.viewport.target_col = byte_col_to_display_col(line, self.backend.cursor_col);
    }
}
