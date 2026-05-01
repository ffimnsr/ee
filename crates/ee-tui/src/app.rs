use std::collections::HashMap;
use std::io;
use std::path::PathBuf;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Position, Rect};
use serde_json::json;

use crate::backend::XiClient;
use crate::keymap::{Action, BindingKey, bindings};
use crate::registers::{BlockInsert, LastChange, RegisterName, RegisterStore};
use crate::text::{
    byte_col_to_display_col, find_char_backward, find_char_forward, next_char_start,
    prev_char_start,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Operator {
    Delete,
    Change,
    Yank,
    Indent,
    Outdent,
    Uppercase,
    Lowercase,
    CaseToggle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Mode {
    Normal,
    Insert,
    /// Char-wise visual selection (`v`).
    Visual,
    /// Line-wise visual selection (`V`).
    VisualLine,
    /// Column-block visual selection (`Ctrl-V`).
    VisualBlock,
    OperatorPending,
    CommandLine,
    Search,
}

impl Mode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Mode::Normal => "NOR",
            Mode::Insert => "INS",
            Mode::Visual => "VIS",
            Mode::VisualLine => "VLN",
            Mode::VisualBlock => "VBK",
            Mode::OperatorPending => "OPR",
            Mode::CommandLine => "CMD",
            Mode::Search => "SRC",
        }
    }

    /// Returns `true` for any visual-family mode.
    pub(crate) fn is_visual(self) -> bool {
        matches!(self, Mode::Visual | Mode::VisualLine | Mode::VisualBlock)
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
    pub(crate) pending_operator: Option<Operator>,
    pub(crate) text_obj_inclusive: Option<bool>,
    /// Set when `"` is pressed; next char selects the target register.
    pub(crate) awaiting_register: bool,
    /// Register selected via `"<char>` prefix; `None` = unnamed.
    pub(crate) pending_register: Option<RegisterName>,
    /// Set when `m` is pressed in Normal mode; next char sets a mark.
    pub(crate) awaiting_mark_set: bool,
    /// Set when `'` (line_start=true) or `` ` `` (line_start=false) is pressed.
    pub(crate) awaiting_mark_jump: Option<bool>,
    /// Set when `q` is pressed to begin recording a macro.
    pub(crate) awaiting_macro_record: bool,
    /// Set when `@` is pressed to replay a macro.
    pub(crate) awaiting_macro_replay: bool,
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
        self.pending_operator = None;
        self.text_obj_inclusive = None;
        self.awaiting_register = false;
        self.pending_register = None;
        self.awaiting_mark_set = false;
        self.awaiting_mark_jump = None;
        self.awaiting_macro_record = false;
        self.awaiting_macro_replay = false;
    }
}

#[derive(Debug)]
pub(crate) struct App {
    pub(crate) backend: XiClient,
    pub(crate) mode: Mode,
    pub(crate) command_buffer: String,
    pub(crate) should_quit: bool,
    pub(crate) viewport: Viewport,
    pub(crate) input_state: InputState,
    /// Anchor position (line, col) when a visual mode was entered.
    pub(crate) visual_anchor: Option<(usize, usize)>,
    /// Last visual selection for `gv` restore (mode, anchor_line, anchor_col).
    pub(crate) last_visual: Option<(Mode, usize, usize)>,
    /// Frontend register store.
    pub(crate) registers: RegisterStore,
    /// Last change recorded for `.` repeat.
    pub(crate) last_change: Option<LastChange>,
    /// Text accumulated while in insert mode (for `.` repeat).
    pub(crate) insert_buffer: String,
    /// When `true`, xi edit calls are recorded in `recorded_commands`.
    recording: bool,
    /// Accumulates edit commands during operator application.
    recorded_commands: Vec<(&'static str, serde_json::Value)>,
    /// Deferred block-insert region applied when leaving insert mode.
    pub(crate) block_insert: Option<BlockInsert>,
    // ── Marks ──────────────────────────────────────────────────────────────
    /// Named marks: `a`–`z` map to (line, byte_col).
    pub(crate) marks: HashMap<char, (usize, usize)>,
    // ── Jump list ──────────────────────────────────────────────────────────
    /// Jump positions, oldest first.  Capped at 100 entries.
    pub(crate) jump_list: Vec<(usize, usize)>,
    /// Index into `jump_list` during backward traversal.
    /// `jump_list.len()` means "at the current (not yet jumped-away) position".
    pub(crate) jump_list_idx: usize,
    // ── Change list ────────────────────────────────────────────────────────
    /// Positions at which the buffer was last modified, oldest first.
    pub(crate) change_list: Vec<(usize, usize)>,
    /// Index into `change_list` for `g;`/`g,` navigation.
    pub(crate) change_list_idx: usize,
    // ── Macros ─────────────────────────────────────────────────────────────
    /// Which named register is being recorded into (`Some` while recording).
    pub(crate) macro_register: Option<char>,
    /// Keystrokes accumulated during the current macro recording.
    macro_buffer: Vec<KeyEvent>,
    /// Stored macros keyed by register name `a`–`z`.
    pub(crate) macros: HashMap<char, Vec<KeyEvent>>,
    /// Last register used for macro replay; `@@` replays this.
    pub(crate) last_macro: Option<char>,
    /// `true` while a macro is replaying to suppress nested recording.
    macro_replaying: bool,
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
            visual_anchor: None,
            last_visual: None,
            registers: RegisterStore::new(),
            last_change: None,
            insert_buffer: String::new(),
            recording: false,
            recorded_commands: Vec::new(),
            block_insert: None,
            marks: HashMap::new(),
            jump_list: Vec::new(),
            jump_list_idx: 0,
            change_list: Vec::new(),
            change_list_idx: 0,
            macro_register: None,
            macro_buffer: Vec::new(),
            macros: HashMap::new(),
            last_macro: None,
            macro_replaying: false,
        })
    }

    pub(crate) fn handle_event(&mut self, event: Event) {
        let Event::Key(key) = event else {
            return;
        };

        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return;
        }

        // Capture keystrokes for the active macro recording (before processing).
        // We always push first and pop afterward if this key stops recording,
        // so the terminating `q` is not stored in the macro.
        if self.macro_register.is_some() && !self.macro_replaying {
            self.macro_buffer.push(key);
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

        // Two-char awaiting states consume the next key unconditionally.
        if self.input_state.awaiting_register {
            self.input_state.awaiting_register = false;
            if let KeyCode::Char(c) = key.code {
                let append = RegisterName::is_append_char(c);
                self.input_state.pending_register = RegisterName::from_char(c);
                if append {
                    let _ = append;
                }
            }
            return;
        }

        if self.input_state.awaiting_mark_set {
            self.input_state.awaiting_mark_set = false;
            if let KeyCode::Char(c) = key.code {
                if c.is_ascii_lowercase() {
                    self.set_mark(c);
                }
            }
            return;
        }

        if let Some(line_start) = self.input_state.awaiting_mark_jump.take() {
            if let KeyCode::Char(c) = key.code {
                self.jump_to_mark(c, line_start);
            }
            return;
        }

        if self.input_state.awaiting_macro_record {
            self.input_state.awaiting_macro_record = false;
            if let KeyCode::Char(c) = key.code {
                if c.is_ascii_lowercase() {
                    self.start_macro_record(c);
                }
            }
            return;
        }

        if self.input_state.awaiting_macro_replay {
            self.input_state.awaiting_macro_replay = false;
            if let KeyCode::Char(c) = key.code {
                let reg = if c == '@' { self.last_macro } else if c.is_ascii_lowercase() { Some(c) } else { None };
                if let Some(r) = reg {
                    self.replay_macro(r);
                }
            }
            return;
        }

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
            Action::SetPrefix(_) | Action::PendingCharFind { .. } | Action::SetOperator(_) => {}
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
                    if mode.is_visual() {
                        // Set anchor at current cursor position.
                        self.visual_anchor =
                            Some((self.backend.cursor_line, self.backend.cursor_col));
                    }
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
                // Push jump list before document-level jumps.
                match method {
                    "move_to_beginning_of_document"
                    | "move_to_end_of_document"
                    | "scroll_page_down"
                    | "scroll_page_up" => self.push_jump(),
                    _ => {}
                }
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
                self.push_jump();
                let _ = self
                    .backend
                    .send_edit("find_next", json!({ "wrap_around": true, "allow_same": false }));
            }
            Action::FindPrevious => {
                self.push_jump();
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
            Action::MatchingPair => {
                self.push_jump();
                self.jump_matching_bracket();
            }
            // Operator-pending mode
            Action::SetOperator(op) => {
                self.input_state.prefix = None;
                self.input_state.pending_operator = Some(op);
                self.mode = Mode::OperatorPending;
            }
            // Insert-entry variants
            Action::AppendAfterCursor => {
                let _ = self.backend.send_edit("move_right", json!([]));
                self.mode = Mode::Insert;
            }
            Action::AppendAtEndOfLine => {
                let _ = self.backend.send_edit("move_to_right_end_of_line", json!([]));
                self.mode = Mode::Insert;
            }
            Action::InsertAtLineStart => {
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                self.mode = Mode::Insert;
            }
            Action::OpenLineBelow => {
                let _ = self.backend.send_edit("move_to_right_end_of_line", json!([]));
                let _ = self.backend.send_edit("insert_newline", json!([]));
                self.mode = Mode::Insert;
            }
            Action::OpenLineAbove => {
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                let _ = self.backend.send_edit("insert_newline", json!([]));
                let _ = self.backend.send_edit("move_up", json!([]));
                self.mode = Mode::Insert;
            }
            Action::SubstituteChar => {
                let count = self.input_state.count();
                for _ in 0..count {
                    let _ = self.backend.send_edit("delete_forward", json!([]));
                }
                self.mode = Mode::Insert;
            }
            Action::SubstituteLine => {
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                let _ = self.backend.send_edit("delete_to_end_of_paragraph", json!([]));
                self.mode = Mode::Insert;
            }
            // Insert mode editing controls
            Action::DeleteWordBackward => {
                let _ = self.backend.send_edit("delete_word_backward", json!([]));
            }
            Action::DeleteToLineStart => {
                let _ = self.backend.send_edit("delete_to_beginning_of_line", json!([]));
            }
            Action::IndentLine => {
                let _ = self.backend.send_edit("indent", json!([]));
            }
            Action::OutdentLine => {
                let _ = self.backend.send_edit("outdent", json!([]));
            }
            // ── Undo / Redo ──────────────────────────────────────────────────
            Action::Undo => {
                let _ = self.backend.send_edit("undo", json!([]));
            }
            Action::Redo => {
                let _ = self.backend.send_edit("redo", json!([]));
            }
            // ── Repeat last change (`.`) ─────────────────────────────────────
            Action::RepeatLastChange => self.repeat_last_change(),
            // ── Paste ────────────────────────────────────────────────────────
            Action::PasteAfter => self.paste(false),
            Action::PasteBefore => self.paste(true),
            // ── Visual modes ─────────────────────────────────────────────────
            Action::EnterVisualLine => self.enter_visual_line(),
            Action::EnterVisualBlock => self.enter_visual_block(),
            Action::SwapVisualAnchor => self.swap_visual_anchor(),
            Action::RestoreLastVisual => self.restore_last_visual(),
            // ── Visual block insert / append ─────────────────────────────────
            Action::VisualBlockInsert => self.visual_block_insert(false),
            Action::VisualBlockAppend => self.visual_block_insert(true),
            // ── Jump list ────────────────────────────────────────────────────
            Action::JumpListOlder => self.jump_list_older(),
            Action::JumpListNewer => self.jump_list_newer(),
            // ── Change list ──────────────────────────────────────────────────
            Action::ChangeListOlder => self.change_list_older(),
            Action::ChangeListNewer => self.change_list_newer(),
        }
    }

    fn handle_default(&mut self, key: KeyEvent) {
        // Cancel operator-pending on Escape
        if key.code == KeyCode::Esc && self.mode == Mode::OperatorPending {
            self.enter_normal_mode();
            self.input_state.reset();
            return;
        }

        let ch = match key.code {
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                c
            }
            KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Handle Ctrl+key in insert mode
                if self.mode == Mode::Insert {
                    match c {
                        'w' => {
                            let _ = self.backend.send_edit("delete_word_backward", json!([]));
                        }
                        'u' => {
                            let _ =
                                self.backend.send_edit("delete_to_beginning_of_line", json!([]));
                        }
                        't' => {
                            let _ = self.backend.send_edit("indent", json!([]));
                        }
                        'd' => {
                            let _ = self.backend.send_edit("outdent", json!([]));
                        }
                        _ => {}
                    }
                }
                return;
            }
            _ => return,
        };

        match self.mode {
            Mode::OperatorPending => {
                self.handle_operator_pending(ch);
            }
            Mode::Insert => {
                let s = ch.to_string();
                self.insert_buffer.push(ch);
                let _ = self.backend.send_edit("insert", json!({ "chars": s }));
            }
            Mode::CommandLine => {
                self.command_buffer.push(ch);
            }
            Mode::Normal => {
                // `"` — start register prefix.
                if ch == '"' && self.input_state.prefix.is_none() {
                    self.input_state.awaiting_register = true;
                    return;
                }
                // `m` — set mark.
                if ch == 'm' && self.input_state.prefix.is_none() {
                    self.input_state.awaiting_mark_set = true;
                    return;
                }
                // `'` — jump to mark (line start).
                if ch == '\'' && self.input_state.prefix.is_none() {
                    self.input_state.awaiting_mark_jump = Some(true);
                    return;
                }
                // `` ` `` — jump to mark (exact position).
                if ch == '`' && self.input_state.prefix.is_none() {
                    self.input_state.awaiting_mark_jump = Some(false);
                    return;
                }
                // `q` — start/stop macro recording.
                if ch == 'q' && self.input_state.prefix.is_none() {
                    if self.macro_register.is_some() {
                        self.stop_macro_record();
                    } else {
                        self.input_state.awaiting_macro_record = true;
                    }
                    return;
                }
                // `@` — replay macro.
                if ch == '@' && self.input_state.prefix.is_none() {
                    self.input_state.awaiting_macro_replay = true;
                    return;
                }
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
            Mode::Visual | Mode::VisualLine | Mode::VisualBlock => {
                self.handle_visual_char(ch);
            }
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
        if self.mode == Mode::Insert {
            // Flush accumulated block insert if pending.
            if self.block_insert.is_some() {
                self.apply_block_insert();
            }
            // Record insert mode text for `.` repeat.
            if !self.insert_buffer.is_empty() {
                self.last_change = Some(LastChange::Insert(self.insert_buffer.clone()));
                self.push_change();
            }
            self.insert_buffer.clear();
        }
        // Save visual selection for `gv`.
        if self.mode.is_visual() {
            if let Some((al, ac)) = self.visual_anchor {
                self.last_visual = Some((self.mode, al, ac));
            }
            self.visual_anchor = None;
            // Collapse xi selection.
            let _ = self.backend.send_edit("collapse_selections", json!([]));
        }
        self.mode = Mode::Normal;
        self.command_buffer.clear();
    }

    // ── Operator-pending mode ──────────────────────────────────────────────

    /// Apply operator to the current xi selection, then return to Normal (or
    /// Insert for Change).  Resets input state before returning.
    fn apply_operator(&mut self, op: Operator) {
        match op {
            Operator::Delete => {
                let reg = self.take_register();
                let text = self.extract_selected_text();
                self.registers.delete(&reg, text, false);
                self.record_edit("delete_forward", json!([]));
                self.push_change();
                self.input_state.reset();
                self.enter_normal_mode();
            }
            Operator::Change => {
                let reg = self.take_register();
                let text = self.extract_selected_text();
                self.registers.delete(&reg, text, false);
                self.record_edit("delete_forward", json!([]));
                self.push_change();
                self.input_state.reset();
                self.enter_normal_mode();
                self.mode = Mode::Insert;
            }
            Operator::Yank => {
                let reg = self.take_register();
                let text = self.extract_selected_text();
                self.registers.yank(&reg, text, false);
                // Collapse selection without modifying buffer.
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.input_state.reset();
                self.enter_normal_mode();
            }
            Operator::Indent => {
                self.record_edit("indent", json!([]));
                self.push_change();
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.input_state.reset();
                self.enter_normal_mode();
            }
            Operator::Outdent => {
                self.record_edit("outdent", json!([]));
                self.push_change();
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.input_state.reset();
                self.enter_normal_mode();
            }
            Operator::Uppercase => {
                self.record_edit("uppercase", json!([]));
                self.push_change();
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.input_state.reset();
                self.enter_normal_mode();
            }
            Operator::Lowercase => {
                self.record_edit("lowercase", json!([]));
                self.push_change();
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.input_state.reset();
                self.enter_normal_mode();
            }
            Operator::CaseToggle => {
                // xi has no char-level toggle; capitalize (first letter of each word)
                // is the closest available primitive.
                self.record_edit("capitalize", json!([]));
                self.push_change();
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.input_state.reset();
                self.enter_normal_mode();
            }
        }
    }

    /// Apply operator to the current line (double-operator: dd, cc, yy, >>, <<, …).
    fn apply_operator_to_line(&mut self, op: Operator) {
        match op {
            Operator::Delete => {
                // Move to start, select to end, delete line content, then delete newline.
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                let _ = self
                    .backend
                    .send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
                let _ = self.backend.send_edit("delete_forward", json!([]));
                let _ = self.backend.send_edit("delete_forward", json!([]));
            }
            Operator::Change => {
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                let _ = self
                    .backend
                    .send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
                let _ = self.backend.send_edit("delete_forward", json!([]));
            }
            Operator::Yank => {
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                let _ = self
                    .backend
                    .send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
                let _ = self.backend.send_edit("delete_forward", json!([]));
                let _ = self.backend.send_edit("yank", json!([]));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
            }
            Operator::Indent => {
                let _ = self.backend.send_edit("indent", json!([]));
            }
            Operator::Outdent => {
                let _ = self.backend.send_edit("outdent", json!([]));
            }
            Operator::Uppercase => {
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                let _ = self
                    .backend
                    .send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
                let _ = self.backend.send_edit("uppercase", json!([]));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
            }
            Operator::Lowercase => {
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                let _ = self
                    .backend
                    .send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
                let _ = self.backend.send_edit("lowercase", json!([]));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
            }
            Operator::CaseToggle => {
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                let _ = self
                    .backend
                    .send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
                let _ = self.backend.send_edit("capitalize", json!([]));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
            }
        }
        if op == Operator::Change {
            self.input_state.reset();
            self.enter_normal_mode();
            self.mode = Mode::Insert;
        } else {
            self.input_state.reset();
            self.enter_normal_mode();
        }
    }

    /// Handle a char in operator-pending mode.
    fn handle_operator_pending(&mut self, ch: char) {
        let op = match self.input_state.pending_operator {
            Some(op) => op,
            None => {
                self.enter_normal_mode();
                self.input_state.reset();
                return;
            }
        };

        // Priority 1: consume a pending char-find target.
        if let Some(find) = self.input_state.pending_find.take() {
            let count = self.input_state.count();
            self.input_state.reset();
            for _ in 0..count {
                self.jump_to_char_selecting(ch, find.forward, find.inclusive);
            }
            self.apply_operator(op);
            return;
        }

        // Priority 2: consume a text-object specifier (iw, aw, i", …).
        if let Some(inclusive) = self.input_state.text_obj_inclusive.take() {
            self.apply_text_object_operator(op, inclusive, ch);
            return;
        }

        let count = self.input_state.count();

        // Priority 3: double operator means "act on whole line".
        let is_double = matches!(
            (op, ch),
            (Operator::Delete, 'd')
                | (Operator::Change, 'c')
                | (Operator::Yank, 'y')
                | (Operator::Indent, '>')
                | (Operator::Outdent, '<')
                | (Operator::Uppercase, 'U')
                | (Operator::Lowercase, 'u')
                | (Operator::CaseToggle, '~')
        );
        if is_double {
            for _ in 0..count {
                self.apply_operator_to_line(op);
            }
            return;
        }

        // Priority 4: text-object prefix.
        if ch == 'i' && self.input_state.prefix.is_none() {
            self.input_state.text_obj_inclusive = Some(false);
            return;
        }
        if ch == 'a' && self.input_state.prefix.is_none() {
            self.input_state.text_obj_inclusive = Some(true);
            return;
        }

        // Priority 5: 'g' prefix for gg motion.
        if ch == 'g' && self.input_state.prefix.is_none() {
            self.input_state.prefix = Some('g');
            return;
        }

        // Priority 6: count digits.
        if ch.is_ascii_digit() {
            let d = ch as u8 - b'0';
            if d > 0 || !self.input_state.count_digits.is_empty() {
                self.input_state.count_digits.push(d);
                return;
            }
        }

        // Priority 7: '0' as line-start motion when no count is active.
        if ch == '0' && self.input_state.count_digits.is_empty() {
            let _ = self
                .backend
                .send_edit("move_to_left_end_of_line_and_modify_selection", json!([]));
            self.apply_operator(op);
            return;
        }

        // Priority 8: char-find operators (f/F/t/T).
        match ch {
            'f' | 'F' | 't' | 'T' => {
                let forward = matches!(ch, 'f' | 't');
                let inclusive = matches!(ch, 'f' | 'F');
                self.input_state.prefix = None;
                self.input_state.pending_find =
                    Some(PendingCharFind { forward, inclusive });
                return;
            }
            _ => {}
        }

        // Priority 9: motions that extend selection.
        let motion_cmd = match (ch, self.input_state.prefix) {
            ('h', None) => Some("move_left_and_modify_selection"),
            ('l', None) => Some("move_right_and_modify_selection"),
            ('j', None) => Some("move_down_and_modify_selection"),
            ('k', None) => Some("move_up_and_modify_selection"),
            ('w', None) | ('e', None) => Some("move_word_right_and_modify_selection"),
            ('b', None) => Some("move_word_left_and_modify_selection"),
            ('$', None) => Some("move_to_right_end_of_line_and_modify_selection"),
            ('^', None) => Some("move_to_beginning_of_paragraph_and_modify_selection"),
            ('G', None) => Some("move_to_end_of_document_and_modify_selection"),
            ('g', Some('g')) => Some("move_to_beginning_of_document_and_modify_selection"),
            _ => None,
        };
        if let Some(cmd) = motion_cmd {
            for _ in 0..count {
                let _ = self.backend.send_edit(cmd, json!([]));
            }
            self.apply_operator(op);
            return;
        }

        // Unknown key – cancel.
        self.enter_normal_mode();
        self.input_state.reset();
    }

    /// Like [`jump_to_char`] but uses `select_extend` so the region from the
    /// current cursor position to the found char is selected (for operators).
    fn jump_to_char_selecting(&mut self, target: char, forward: bool, inclusive: bool) {
        let line_idx = self.backend.cursor_line;
        let cursor_byte = self.backend.cursor_col;
        let line = match self.backend.lines.get(line_idx) {
            Some(l) => l.clone(),
            None => return,
        };

        // Compute the byte position to extend the selection to.
        let col_opt = if forward {
            find_char_forward(&line, cursor_byte, target).map(|pos| {
                if inclusive {
                    // Include the found char in the selection.
                    pos + target.len_utf8()
                } else {
                    pos
                }
            })
        } else {
            find_char_backward(&line, cursor_byte, target).map(|pos| {
                if inclusive {
                    pos
                } else {
                    // Exclude the found char.
                    pos + target.len_utf8()
                }
            })
        };

        if let Some(col) = col_opt {
            let _ = self.backend.send_edit(
                "gesture",
                json!({
                    "line": line_idx as u64,
                    "col": col as u64,
                    "ty": { "select_extend": { "granularity": "point" } }
                }),
            );
        }
    }

    /// Select `(start, end)` byte range on `line_idx` and apply the operator.
    fn select_range_and_apply(&mut self, line_idx: usize, start: usize, end: usize, op: Operator) {
        let _ = self.backend.send_edit(
            "gesture",
            json!({
                "line": line_idx as u64,
                "col": start as u64,
                "ty": { "select": { "granularity": "point", "multi": false } }
            }),
        );
        let _ = self.backend.send_edit(
            "gesture",
            json!({
                "line": line_idx as u64,
                "col": end as u64,
                "ty": { "select_extend": { "granularity": "point" } }
            }),
        );
        self.apply_operator(op);
    }

    /// Apply operator to the text object specified by `inclusive`+`spec`.
    fn apply_text_object_operator(&mut self, op: Operator, inclusive: bool, spec: char) {
        let line_idx = self.backend.cursor_line;
        let cursor_byte = self.backend.cursor_col;
        let line = match self.backend.lines.get(line_idx) {
            Some(l) => l.clone(),
            None => {
                self.enter_normal_mode();
                self.input_state.reset();
                return;
            }
        };

        let range = match spec {
            'w' | 'W' => {
                let big_word = spec == 'W';
                text_obj_word(&line, cursor_byte, inclusive, big_word)
            }
            '"' | '\'' | '`' => text_obj_quote(&line, cursor_byte, spec, inclusive),
            '(' | ')' | 'b' => text_obj_bracket(&line, cursor_byte, '(', ')', inclusive),
            '[' | ']' => text_obj_bracket(&line, cursor_byte, '[', ']', inclusive),
            '{' | '}' | 'B' => text_obj_bracket(&line, cursor_byte, '{', '}', inclusive),
            '<' | '>' => text_obj_bracket(&line, cursor_byte, '<', '>', inclusive),
            'p' => {
                // Paragraph: use xi's paragraph motions instead of byte range.
                let _ = self
                    .backend
                    .send_edit("move_to_beginning_of_paragraph", json!([]));
                let _ = self.backend.send_edit(
                    "move_to_end_of_paragraph_and_modify_selection",
                    json!([]),
                );
                self.apply_operator(op);
                return;
            }
            's' => {
                // Sentence: treat as current line for simplicity.
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                let _ = self
                    .backend
                    .send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
                self.apply_operator(op);
                return;
            }
            't' => {
                // Tag-like pair: find <tag>…</tag> on the same line.
                text_obj_tag(&line, cursor_byte, inclusive)
            }
            _ => None,
        };

        match range {
            Some((start, end)) => self.select_range_and_apply(line_idx, start, end, op),
            None => {
                self.enter_normal_mode();
                self.input_state.reset();
            }
        }
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

    // ── Recording helpers ──────────────────────────────────────────────────

    /// Send a xi edit and, if recording is active, append it to the command log.
    fn record_edit(&mut self, method: &'static str, params: serde_json::Value) {
        if self.recording {
            self.recorded_commands.push((method, params.clone()));
        }
        let _ = self.backend.send_edit(method, params);
    }

    fn begin_record(&mut self) {
        self.recording = true;
        self.recorded_commands.clear();
    }

    fn end_record(&mut self) {
        self.recording = false;
        let cmds = self.recorded_commands.drain(..).collect();
        self.last_change = Some(LastChange::Commands(cmds));
    }

    // ── Marks ──────────────────────────────────────────────────────────────

    /// Set mark `c` to the current cursor position.
    fn set_mark(&mut self, c: char) {
        let pos = (self.backend.cursor_line, self.backend.cursor_col);
        self.marks.insert(c, pos);
    }

    /// Jump to mark `c`.  `line_start=true` moves to the first non-whitespace
    /// on the mark's line; `false` jumps to the exact saved byte column.
    fn jump_to_mark(&mut self, c: char, line_start: bool) {
        // Special marks: `'` / `` ` `` jump to position before last jump.
        let pos = if c == '\'' || c == '`' {
            // Re-use the last jump list entry as the "previous position" mark.
            match self.jump_list.last().copied() {
                Some(p) => p,
                None => return,
            }
        } else {
            match self.marks.get(&c).copied() {
                Some(p) => p,
                None => return,
            }
        };

        self.push_jump();
        let (line, col) = pos;
        let col = if line_start {
            // Find first non-whitespace byte offset on the target line.
            self.backend
                .lines
                .get(line)
                .map(|l| l.find(|ch: char| !ch.is_whitespace()).unwrap_or(0))
                .unwrap_or(0)
        } else {
            col
        };
        self.move_cursor_to(line, col);
    }

    // ── Jump list ──────────────────────────────────────────────────────────

    const JUMP_LIST_MAX: usize = 100;

    /// Push the current cursor position onto the jump list.
    /// Resets navigation to the head of the list.
    pub(crate) fn push_jump(&mut self) {
        let pos = (self.backend.cursor_line, self.backend.cursor_col);
        // Avoid duplicates at the head.
        if self.jump_list.last() == Some(&pos) {
            self.jump_list_idx = self.jump_list.len();
            return;
        }
        self.jump_list.push(pos);
        if self.jump_list.len() > Self::JUMP_LIST_MAX {
            self.jump_list.remove(0);
        }
        self.jump_list_idx = self.jump_list.len();
    }

    fn jump_list_older(&mut self) {
        if self.jump_list.is_empty() {
            return;
        }
        // First Ctrl-O saves current position, then steps back.
        if self.jump_list_idx == self.jump_list.len() {
            let pos = (self.backend.cursor_line, self.backend.cursor_col);
            if self.jump_list.last() != Some(&pos) {
                self.jump_list.push(pos);
                if self.jump_list.len() > Self::JUMP_LIST_MAX {
                    self.jump_list.remove(0);
                }
                self.jump_list_idx = self.jump_list.len();
            }
        }
        if self.jump_list_idx == 0 {
            return;
        }
        self.jump_list_idx -= 1;
        let (line, col) = self.jump_list[self.jump_list_idx];
        self.move_cursor_to(line, col);
    }

    fn jump_list_newer(&mut self) {
        if self.jump_list_idx + 1 >= self.jump_list.len() {
            return;
        }
        self.jump_list_idx += 1;
        let (line, col) = self.jump_list[self.jump_list_idx];
        self.move_cursor_to(line, col);
    }

    // ── Change list ────────────────────────────────────────────────────────

    const CHANGE_LIST_MAX: usize = 100;

    /// Push the current cursor position onto the change list.
    /// Called after any buffer-modifying operation.
    pub(crate) fn push_change(&mut self) {
        let pos = (self.backend.cursor_line, self.backend.cursor_col);
        if self.change_list.last() == Some(&pos) {
            self.change_list_idx = self.change_list.len().saturating_sub(1);
            return;
        }
        self.change_list.push(pos);
        if self.change_list.len() > Self::CHANGE_LIST_MAX {
            self.change_list.remove(0);
        }
        self.change_list_idx = self.change_list.len().saturating_sub(1);
    }

    fn change_list_older(&mut self) {
        if self.change_list.is_empty() {
            return;
        }
        let (line, col) = self.change_list[self.change_list_idx];
        self.move_cursor_to(line, col);
        self.change_list_idx = self.change_list_idx.saturating_sub(1);
    }

    fn change_list_newer(&mut self) {
        if self.change_list.is_empty() {
            return;
        }
        let next = (self.change_list_idx + 1).min(self.change_list.len().saturating_sub(1));
        self.change_list_idx = next;
        let (line, col) = self.change_list[self.change_list_idx];
        self.move_cursor_to(line, col);
    }

    // ── Cursor movement helper ──────────────────────────────────────────────

    /// Move the xi cursor to the given (line, byte_col) via a gesture point_select.
    fn move_cursor_to(&mut self, line: usize, col: usize) {
        let _ = self.backend.send_edit(
            "gesture",
            json!({ "line": line as u64, "col": col as u64, "ty": "point_select" }),
        );
    }

    // ── Macro recording / replay ────────────────────────────────────────────

    /// Start recording keystrokes into register `c`.
    fn start_macro_record(&mut self, c: char) {
        self.macro_register = Some(c);
        self.macro_buffer.clear();
        self.backend.status_message = Some(format!("recording @{c}"));
    }

    /// Stop recording and store the accumulated keystrokes in the macros map.
    fn stop_macro_record(&mut self) {
        let Some(c) = self.macro_register.take() else { return };
        // The key that triggered stop_macro_record ('q') was already pushed to
        // macro_buffer by handle_event; remove it.
        self.macro_buffer.pop();
        let keys = self.macro_buffer.drain(..).collect();
        self.macros.insert(c, keys);
        self.last_macro = Some(c);
        self.backend.status_message = None;
    }

    /// Replay the macro stored in register `c` (count times).
    fn replay_macro(&mut self, c: char) {
        let count = self.input_state.count();
        self.input_state.reset();
        let keys = match self.macros.get(&c).cloned() {
            Some(k) if !k.is_empty() => k,
            _ => return,
        };
        self.last_macro = Some(c);
        self.macro_replaying = true;
        for _ in 0..count {
            for key in keys.iter().copied() {
                self.handle_event(Event::Key(key));
            }
        }
        self.macro_replaying = false;
    }

    // ── Register helpers ───────────────────────────────────────────────────

    /// Consume the pending register, falling back to `Unnamed`.
    fn take_register(&mut self) -> RegisterName {
        self.input_state.pending_register.take().unwrap_or(RegisterName::Unnamed)
    }

    /// Extract the text that xi currently has selected, using the local line
    /// cache as a best-effort source.  Returns an empty string when no anchor
    /// is set or the selection is a bare caret.
    fn extract_selected_text(&self) -> String {
        let cl = self.backend.cursor_line;
        let cc = self.backend.cursor_col;
        let (al, ac) = match self.visual_anchor {
            Some(a) => a,
            None => return String::new(),
        };
        // Normalise so (start_line, start_col) ≤ (end_line, end_col).
        let ((sl, sc), (el, ec)) = if (al, ac) <= (cl, cc) {
            ((al, ac), (cl, cc))
        } else {
            ((cl, cc), (al, ac))
        };
        let lines = &self.backend.lines;
        if sl >= lines.len() {
            return String::new();
        }
        if sl == el {
            let line = &lines[sl];
            let s = sc.min(line.len());
            let e = ec.min(line.len());
            return line[s..e].to_owned();
        }
        let mut out = String::new();
        let first = &lines[sl];
        out.push_str(&first[sc.min(first.len())..]);
        out.push('\n');
        for line in &lines[sl + 1..el.min(lines.len())] {
            out.push_str(line);
            out.push('\n');
        }
        if el < lines.len() {
            let last = &lines[el];
            out.push_str(&last[..ec.min(last.len())]);
        }
        out
    }

    // ── Paste ──────────────────────────────────────────────────────────────

    /// Paste register content.  `before` = `P` (before cursor), else `p`.
    fn paste(&mut self, before: bool) {
        let reg = self.take_register();
        let text = self.registers.get(&reg);
        if text.is_empty() {
            return;
        }
        if !before {
            // Move cursor right one position so insert lands after cursor.
            let _ = self.backend.send_edit("move_right", json!([]));
        }
        let _ = self.backend.send_edit("insert", json!({ "chars": text }));
    }

    // ── Repeat last change ──────────────────────────────────────────────────

    fn repeat_last_change(&mut self) {
        let change = match self.last_change.clone() {
            Some(c) => c,
            None => return,
        };
        match change {
            LastChange::Insert(text) => {
                let _ = self.backend.send_edit("insert", json!({ "chars": text }));
            }
            LastChange::Commands(cmds) => {
                for (method, params) in cmds {
                    let _ = self.backend.send_edit(method, params);
                }
            }
        }
    }

    // ── Visual mode helpers ─────────────────────────────────────────────────

    fn enter_visual_line(&mut self) {
        let anchor = (self.backend.cursor_line, self.backend.cursor_col);
        self.visual_anchor = Some(anchor);
        self.mode = Mode::VisualLine;
        // Immediately select the whole current line.
        let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
        let _ = self
            .backend
            .send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
    }

    fn enter_visual_block(&mut self) {
        let anchor = (self.backend.cursor_line, self.backend.cursor_col);
        self.visual_anchor = Some(anchor);
        self.mode = Mode::VisualBlock;
        // No xi selection yet; block region is defined by anchor + cursor.
    }

    /// Re-send xi line-wise selection from the anchor line to the cursor line.
    fn sync_visual_line_selection(&mut self) {
        let (al, _ac) = match self.visual_anchor {
            Some(a) => a,
            None => return,
        };
        let cl = self.backend.cursor_line;
        let (top, bottom) = if al <= cl { (al, cl) } else { (cl, al) };
        // Select from beginning of top line to end of bottom line.
        let _ = self.backend.send_edit(
            "gesture",
            json!({
                "line": top as u64,
                "col": 0u64,
                "ty": { "select": { "granularity": "point", "multi": false } }
            }),
        );
        let bottom_len = self
            .backend
            .lines
            .get(bottom)
            .map(|s| s.len())
            .unwrap_or(0);
        let _ = self.backend.send_edit(
            "gesture",
            json!({
                "line": bottom as u64,
                "col": bottom_len as u64,
                "ty": { "select_extend": { "granularity": "point" } }
            }),
        );
    }

    fn swap_visual_anchor(&mut self) {
        if let Some((al, ac)) = self.visual_anchor {
            let old_cursor = (self.backend.cursor_line, self.backend.cursor_col);
            // Move xi cursor to the old anchor position.
            let _ = self.backend.send_edit(
                "gesture",
                json!({
                    "line": al as u64,
                    "col": ac as u64,
                    "ty": { "select": { "granularity": "point", "multi": false } }
                }),
            );
            self.visual_anchor = Some(old_cursor);
            if self.mode == Mode::VisualLine {
                self.sync_visual_line_selection();
            }
        }
    }

    fn restore_last_visual(&mut self) {
        if let Some((saved_mode, al, ac)) = self.last_visual {
            self.visual_anchor = Some((al, ac));
            self.mode = saved_mode;
            // Position xi cursor at anchor (selection will be set by movement).
            let _ = self.backend.send_edit(
                "gesture",
                json!({
                    "line": al as u64,
                    "col": ac as u64,
                    "ty": { "select": { "granularity": "point", "multi": false } }
                }),
            );
            if saved_mode == Mode::VisualLine {
                self.sync_visual_line_selection();
            }
        }
    }

    /// Handle a character key while in any visual mode.
    fn handle_visual_char(&mut self, ch: char) {
        match ch {
            // Operators
            'd' => {
                self.begin_record();
                if self.mode == Mode::VisualLine {
                    self.apply_visual_line_delete();
                } else if self.mode == Mode::VisualBlock {
                    self.apply_visual_block_op(Operator::Delete);
                } else {
                    let reg = self.take_register();
                    let text = self.extract_selected_text();
                    self.registers.delete(&reg, text, false);
                    self.record_edit("delete_forward", json!([]));
                    self.enter_normal_mode();
                }
                self.end_record();
            }
            'y' => {
                self.begin_record();
                if self.mode == Mode::VisualLine {
                    self.apply_visual_line_yank();
                } else if self.mode == Mode::VisualBlock {
                    self.apply_visual_block_op(Operator::Yank);
                } else {
                    let reg = self.take_register();
                    let text = self.extract_selected_text();
                    self.registers.yank(&reg, text, false);
                    let _ = self.backend.send_edit("collapse_selections", json!([]));
                    self.enter_normal_mode();
                }
                self.end_record();
            }
            'c' => {
                self.begin_record();
                if self.mode == Mode::VisualLine {
                    let reg = self.take_register();
                    let text = self.extract_selected_line_text();
                    self.registers.delete(&reg, text, false);
                    self.record_edit("delete_forward", json!([]));
                    self.enter_normal_mode();
                    self.mode = Mode::Insert;
                } else if self.mode == Mode::VisualBlock {
                    self.apply_visual_block_op(Operator::Change);
                    self.mode = Mode::Insert;
                } else {
                    let reg = self.take_register();
                    let text = self.extract_selected_text();
                    self.registers.delete(&reg, text, false);
                    self.record_edit("delete_forward", json!([]));
                    self.enter_normal_mode();
                    self.mode = Mode::Insert;
                }
                self.end_record();
            }
            '>' => {
                self.begin_record();
                self.record_edit("indent", json!([]));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.end_record();
                self.enter_normal_mode();
            }
            '<' => {
                self.begin_record();
                self.record_edit("outdent", json!([]));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.end_record();
                self.enter_normal_mode();
            }
            'U' => {
                self.begin_record();
                self.record_edit("uppercase", json!([]));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.end_record();
                self.enter_normal_mode();
            }
            'u' => {
                self.begin_record();
                self.record_edit("lowercase", json!([]));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.end_record();
                self.enter_normal_mode();
            }
            // `o` — swap anchor (handled as Action::SwapVisualAnchor in bindings,
            // but also catch it here for VisualLine/VisualBlock where not bound).
            'o' => self.swap_visual_anchor(),
            _ => {}
        }
    }

    /// Extract text for all selected lines (VisualLine).
    fn extract_selected_line_text(&self) -> String {
        let (al, _) = self.visual_anchor.unwrap_or((self.backend.cursor_line, 0));
        let cl = self.backend.cursor_line;
        let (top, bottom) = if al <= cl { (al, cl) } else { (cl, al) };
        let lines = &self.backend.lines;
        let mut out = String::new();
        for line in &lines[top..=bottom.min(lines.len().saturating_sub(1))] {
            out.push_str(line);
            out.push('\n');
        }
        out
    }

    fn apply_visual_line_delete(&mut self) {
        let reg = self.take_register();
        let text = self.extract_selected_line_text();
        self.registers.delete(&reg, text, false);
        self.sync_visual_line_selection();
        self.record_edit("delete_forward", json!([]));
        self.enter_normal_mode();
    }

    fn apply_visual_line_yank(&mut self) {
        let reg = self.take_register();
        let text = self.extract_selected_line_text();
        self.registers.yank(&reg, text, false);
        // Collapse without deleting.
        let _ = self.backend.send_edit("collapse_selections", json!([]));
        self.enter_normal_mode();
    }

    /// Apply an operator to each line in the block visual selection.
    fn apply_visual_block_op(&mut self, op: Operator) {
        let (al, ac) = self.visual_anchor.unwrap_or((self.backend.cursor_line, self.backend.cursor_col));
        let cl = self.backend.cursor_line;
        let cc = self.backend.cursor_col;
        let (top, bottom) = if al <= cl { (al, cl) } else { (cl, al) };
        let (left_col, right_col) = if ac <= cc { (ac, cc) } else { (cc, ac) };
        // Clone to avoid holding borrow across mutable backend calls.
        let lines: Vec<String> = self.backend.lines.clone();
        let mut extracted = String::new();
        for line in &lines[top..=bottom.min(lines.len().saturating_sub(1))] {
            let s = left_col.min(line.len());
            let e = right_col.min(line.len());
            extracted.push_str(&line[s..e]);
            extracted.push('\n');
        }
        if op == Operator::Yank {
            let reg = self.take_register();
            self.registers.yank(&reg, extracted, false);
            let _ = self.backend.send_edit("collapse_selections", json!([]));
            self.enter_normal_mode();
            return;
        }
        // For delete/change: iterate lines from bottom to top to preserve offsets.
        for li in (top..=bottom.min(lines.len().saturating_sub(1))).rev() {
            let line_len = lines[li].len();
            let s = left_col.min(line_len);
            let e = right_col.min(line_len);
            if s >= e {
                continue;
            }
            // Select the column range on this line and delete.
            let _ = self.backend.send_edit(
                "gesture",
                json!({
                    "line": li as u64,
                    "col": s as u64,
                    "ty": { "select": { "granularity": "point", "multi": false } }
                }),
            );
            let _ = self.backend.send_edit(
                "gesture",
                json!({
                    "line": li as u64,
                    "col": e as u64,
                    "ty": { "select_extend": { "granularity": "point" } }
                }),
            );
            self.record_edit("delete_forward", json!([]));
        }
        let reg = self.take_register();
        self.registers.delete(&reg, extracted, false);
        self.enter_normal_mode();
        if op == Operator::Change {
            self.mode = Mode::Insert;
        }
    }

    // ── Visual block insert / append ────────────────────────────────────────

    /// Set up a block-insert (`I`) or block-append (`A`) for the current block
    /// visual selection.  Actual text is applied on leaving insert mode.
    fn visual_block_insert(&mut self, append: bool) {
        let (al, ac) = self.visual_anchor.unwrap_or((self.backend.cursor_line, self.backend.cursor_col));
        let cl = self.backend.cursor_line;
        let cc = self.backend.cursor_col;
        let (top, bottom) = if al <= cl { (al, cl) } else { (cl, al) };
        let col = if append {
            ac.max(cc) // right edge
        } else {
            ac.min(cc) // left edge
        };
        self.block_insert = Some(BlockInsert { line_start: top, line_end: bottom, col, append });
        // Position cursor at the insertion column on the top line.
        let _ = self.backend.send_edit(
            "gesture",
            json!({
                "line": top as u64,
                "col": col as u64,
                "ty": "point_select"
            }),
        );
        self.visual_anchor = None;
        self.mode = Mode::Insert;
    }

    /// Apply deferred block-insert text.  Called when leaving insert mode.
    fn apply_block_insert(&mut self) {
        let bi = match self.block_insert.take() {
            Some(b) => b,
            None => return,
        };
        let text = self.insert_buffer.clone();
        if text.is_empty() {
            return;
        }
        // Apply to each line in the block range (skip first – it already has it).
        let lines = self.backend.lines.clone();
        let range = (bi.line_start + 1)..=bi.line_end.min(lines.len().saturating_sub(1));
        for (idx, line) in lines[range.clone()].iter().enumerate() {
            let li = bi.line_start + 1 + idx;
            let col = bi.col.min(line.len());
            let _ = self.backend.send_edit(
                "gesture",
                json!({ "line": li as u64, "col": col as u64, "ty": "point_select" }),
            );
            let _ = self.backend.send_edit("insert", json!({ "chars": text }));
        }
    }

}

// ── Text object helpers (free functions) ─────────────────────────────────────

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_big_word_char(c: char) -> bool {
    !c.is_whitespace()
}

/// Inner / outer word text object.  `big_word` = WORD (non-whitespace) mode.
pub(crate) fn text_obj_word(
    line: &str,
    cursor: usize,
    inclusive: bool,
    big_word: bool,
) -> Option<(usize, usize)> {
    let pred: fn(char) -> bool = if big_word { is_big_word_char } else { is_word_char };

    // Must be on a word character.
    let first = line[cursor..].chars().next()?;
    if !pred(first) {
        return None;
    }

    // Scan backward for start.
    let mut start = cursor;
    for (i, c) in line[..cursor].char_indices().collect::<Vec<_>>().iter().rev() {
        if pred(*c) {
            start = *i;
        } else {
            break;
        }
    }

    // Scan forward for end.
    let mut end = cursor + first.len_utf8();
    for (off, c) in line[cursor + first.len_utf8()..].char_indices() {
        if pred(c) {
            end = cursor + first.len_utf8() + off + c.len_utf8();
        } else {
            break;
        }
    }

    if inclusive {
        // Outer: include trailing whitespace.
        for (off, c) in line[end..].char_indices() {
            if c == ' ' || c == '\t' {
                end += c.len_utf8();
                let _ = off; // suppress unused warning
            } else {
                break;
            }
        }
    }
    Some((start, end))
}

/// Inner / outer quote text object (`"`, `'`, `` ` ``).
pub(crate) fn text_obj_quote(
    line: &str,
    cursor: usize,
    quote: char,
    inclusive: bool,
) -> Option<(usize, usize)> {
    let positions: Vec<usize> =
        line.char_indices().filter(|(_, c)| *c == quote).map(|(i, _)| i).collect();

    // Find the adjacent pair that contains or touches the cursor.
    let mut i = 0;
    while i + 1 < positions.len() {
        let open = positions[i];
        let close = positions[i + 1];
        if open <= cursor && cursor <= close {
            return if inclusive {
                Some((open, close + quote.len_utf8()))
            } else {
                let inner_start = open + quote.len_utf8();
                if inner_start <= close {
                    Some((inner_start, close))
                } else {
                    None
                }
            };
        }
        i += 2;
    }
    None
}

/// Inner / outer bracket text object.  Finds the innermost matching pair
/// that contains the cursor on the current line.
pub(crate) fn text_obj_bracket(
    line: &str,
    cursor: usize,
    open: char,
    close: char,
    inclusive: bool,
) -> Option<(usize, usize)> {
    let chars: Vec<(usize, char)> = line.char_indices().collect();

    // Find the cursor's index in the chars vec.
    let cur_idx = chars.partition_point(|(i, _)| *i < cursor);

    // Scan backward for the unmatched open bracket.
    let mut depth = 0i32;
    let mut open_pos = None;
    for &(i, c) in chars[..cur_idx.min(chars.len())].iter().rev() {
        if c == close {
            depth += 1;
        } else if c == open {
            if depth == 0 {
                open_pos = Some(i);
                break;
            }
            depth -= 1;
        }
    }
    let open_pos = open_pos?;

    // Scan forward from open_pos for the matching close bracket.
    let start_idx = chars.partition_point(|(i, _)| *i <= open_pos);
    let mut depth = 1i32;
    let mut close_pos = None;
    for &(i, c) in &chars[start_idx..] {
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                close_pos = Some(i);
                break;
            }
        }
    }
    let close_pos = close_pos?;

    if inclusive {
        Some((open_pos, close_pos + close.len_utf8()))
    } else {
        let inner_start = open_pos + open.len_utf8();
        if inner_start <= close_pos {
            Some((inner_start, close_pos))
        } else {
            None
        }
    }
}

/// Inner / outer tag text object (`<tag>…</tag>`).  Only handles same-line
/// tags; returns `None` for cross-line or malformed markup.
pub(crate) fn text_obj_tag(line: &str, cursor: usize, inclusive: bool) -> Option<(usize, usize)> {
    // Find `<…>` opening tag at or before the cursor.
    let open_angle = line[..=cursor.min(line.len().saturating_sub(1))]
        .rfind('<')
        .filter(|&pos| line[pos..].contains('>'))?;
    let open_close_angle = open_angle + line[open_angle..].find('>')?;
    let tag_body = &line[open_angle + 1..open_close_angle];

    // Only accept simple opening tags (not </…> or self-closing).
    if tag_body.starts_with('/') || tag_body.ends_with('/') {
        return None;
    }
    let tag_name: &str = tag_body.split_whitespace().next()?;

    // Find matching closing tag `</tag>` after the opening tag.
    let content_start = open_close_angle + 1;
    let close_tag = format!("</{}>", tag_name);
    let close_start = line[content_start..].find(close_tag.as_str()).map(|off| content_start + off)?;

    if inclusive {
        Some((open_angle, close_start + close_tag.len()))
    } else {
        Some((content_start, close_start))
    }
}
