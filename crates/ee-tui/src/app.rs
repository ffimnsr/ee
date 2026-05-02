use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::time::Instant;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use serde_json::json;
use xi_core_lib::rpc::LineReplacement;
use xi_core_lib::plugin_rpc::CodeActionDescriptor;

use crate::buffer::BufferManager;
use crate::backend::{CompletionSuggestion, PendingUiAction};
use crate::folds::{FoldStore, indent_fold_extent};
use crate::keymap::{Action, BindingKey, bindings};
use crate::picker::PickerState;
use crate::quickfix::{QfEntry, QfList};
use crate::registers::{BlockInsert, LastChange, RegisterName, RegisterStore};
use crate::text::{
    byte_col_to_display_col, find_char_backward, find_char_forward, next_char_start,
    prev_char_start,
};
use crate::window::{SplitDir, TabManager};

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
    /// Awaiting `y`/`n`/`a`/`q` confirmation for a `:s///c` substitute.
    SubstituteConfirm,
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
            Mode::SubstituteConfirm => "SUB",
        }
    }

    /// Returns `true` for any visual-family mode.
    pub(crate) fn is_visual(self) -> bool {
        matches!(self, Mode::Visual | Mode::VisualLine | Mode::VisualBlock)
    }
}

/// Pending substitution state for `:s///c` confirm mode.
#[derive(Debug)]
pub(crate) struct SubstitutePending {
    /// Backend-computed line replacements pending confirmation, in order.
    pub(crate) matches: Vec<LineReplacement>,
    /// Index into `matches` for the current confirmation prompt.
    pub(crate) current: usize,
    /// Count of replacements applied so far.
    pub(crate) applied: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HoverPopup {
    pub(crate) title: String,
    pub(crate) content: String,
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
    /// Set when `Ctrl-W` is pressed; next char is the window command.
    pub(crate) awaiting_window_cmd: bool,
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
        self.awaiting_window_cmd = false;
    }
}

#[derive(Debug)]
pub(crate) struct App {
    pub(crate) config: crate::config::EditorSettings,
    pub(crate) backend: BufferManager,
    pub(crate) tabs: TabManager,
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
    // ── Ex command history ─────────────────────────────────────────────────
    /// Previously executed ex commands, oldest first.  Capped at 100.
    command_history: Vec<String>,
    /// Current index while navigating history with Up/Down; `None` = off.
    history_idx: Option<usize>,
    /// Saved `command_buffer` snapshot taken before history navigation began.
    history_draft: String,
    // ── Picker overlay ─────────────────────────────────────────────────────
    /// Active picker overlay (file picker, buffer picker, live grep).
    pub(crate) picker: Option<PickerState>,
    // ── Quickfix list ───────────────────────────────────────────────────────
    /// Global quickfix list, shared across windows.
    pub(crate) quickfix: Option<QfList>,
    /// Whether the quickfix panel is visible.
    pub(crate) quickfix_open: bool,
    /// Whether keyboard focus is inside the quickfix panel.
    pub(crate) quickfix_focused: bool,
    // ── Location list ─────────────────────────────────────────────────────────
    /// Per-instance location list (location-list variant of quickfix).
    pub(crate) location_list: Option<QfList>,
    /// Whether the location-list panel is visible.
    pub(crate) location_list_open: bool,
    /// Whether keyboard focus is inside the location-list panel.
    pub(crate) location_list_focused: bool,
    // ── Crash recovery ──────────────────────────────────────────────────────────
    /// Timestamp of the last crash-recovery write.
    recovery_last_check: Instant,
    // ── Syntax highlighting ─────────────────────────────────────────────────────
    /// Syntect-backed in-process syntax highlighter (Phase 1).
    pub(crate) highlighter: crate::highlight::Highlighter,
    // ── Fold state ───────────────────────────────────────────────────────────────
    /// Manual fold state keyed by buffer ID.
    pub(crate) folds: FoldStore,
    // ── Search state ─────────────────────────────────────────────────────────────
    /// Last executed search pattern (for highlight and repeat navigation).
    pub(crate) search_pattern: Option<String>,
    /// `true` when the current search was initiated with `?` (backward).
    pub(crate) search_backward: bool,
    /// Active hover popup for the focused editor surface.
    pub(crate) hover_popup: Option<HoverPopup>,
    // ── Substitute confirm state ──────────────────────────────────────────────────
    /// Pending substitutions awaiting `y`/`n`/`a`/`q` confirmation.
    pub(crate) substitute_pending: Option<SubstitutePending>,
}

impl App {
    pub(crate) fn from_path(path: Option<PathBuf>) -> io::Result<Self> {
        let config = crate::config::load_config(path.as_deref());
        let mut backend = BufferManager::new(path)?;
        let initial_buf_id = backend.active().id;

        // Notify user if a crash-recovery artifact exists for this file.
        if let Some(rp) =
            backend.active().path.as_ref().and_then(|p| crate::buffer::recovery_file_path(p))
        {
            if rp.exists() {
                backend.status_message = Some(format!(
                    "Recovery file found: {} — use :recover to restore or :recoverdel to discard",
                    rp.display()
                ));
            }
        }

        Ok(Self {
            config,
            backend,
            tabs: TabManager::new(initial_buf_id),
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
            command_history: Vec::new(),
            history_idx: None,
            history_draft: String::new(),
            picker: None,
            quickfix: None,
            quickfix_open: false,
            quickfix_focused: false,
            location_list: None,
            location_list_open: false,
            location_list_focused: false,
            recovery_last_check: Instant::now(),
            highlighter: crate::highlight::Highlighter::new(),
            folds: FoldStore::new(),
            search_pattern: None,
            search_backward: false,
            hover_popup: None,
            substitute_pending: None,
        })
    }

    pub(crate) fn handle_event(&mut self, event: Event) {
        match event {
            Event::Mouse(m) => {
                self.handle_mouse_event(m);
                return;
            }
            Event::Paste(text) => {
                self.handle_paste(text);
                return;
            }
            _ => {}
        }

        let Event::Key(mut key) = event else {
            return;
        };

        if matches!(key.code, KeyCode::Char('\r' | '\n'))
            || (key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('m' | 'j')))
        {
            key.code = KeyCode::Enter;
            key.modifiers.remove(KeyModifiers::CONTROL);
        }

        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return;
        }

        // Picker overlay intercepts all keys while active.
        if self.picker.is_some() {
            self.handle_picker_event(key);
            return;
        }

        if self.hover_popup.is_some() && key.code == KeyCode::Esc {
            self.hover_popup = None;
            return;
        }

        // Quickfix panel focus intercepts all keys while focused.
        if self.quickfix_focused {
            self.handle_qf_focused_event(key, true);
            return;
        }
        // Location-list panel focus intercepts all keys while focused.
        if self.location_list_focused {
            self.handle_qf_focused_event(key, false);
            return;
        }

        // SubstituteConfirm mode: y/n/a/q consume all keys.
        if self.mode == Mode::SubstituteConfirm {
            self.handle_substitute_confirm(key.code);
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
                let reg = if c == '@' {
                    self.last_macro
                } else if c.is_ascii_lowercase() {
                    Some(c)
                } else {
                    None
                };
                if let Some(r) = reg {
                    self.replay_macro(r);
                }
            }
            return;
        }

        if self.input_state.awaiting_window_cmd {
            self.input_state.awaiting_window_cmd = false;
            if let KeyCode::Char(c) = key.code {
                self.handle_window_cmd(c);
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
                self.search_backward = false;
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
                let _ = self.backend.send_edit("highlight_find", json!({ "visible": true }));
            }
            Action::EnterSearchBackward => {
                self.mode = Mode::Search;
                self.search_backward = true;
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
                let _ = self.backend.send_edit("highlight_find", json!({ "visible": true }));
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
                let case_sensitive = smart_case_sensitive(&chars);
                let _ = self.backend.send_edit(
                    "find",
                    json!({
                        "chars": chars,
                        "case_sensitive": case_sensitive,
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
            Action::RequestHover => {
                let position = Some((self.backend.cursor_line, self.backend.cursor_col));
                if let Err(err) = self.backend.request_hover(position) {
                    self.backend.status_message = Some(format!("hover failed: {err}"));
                }
            }
            Action::SearchWordUnderCursor { forward } => {
                // Use word under cursor (or visual selection) as search pattern via xi's
                // selection_for_find RPC, then navigate forward or backward.
                let _ =
                    self.backend.send_edit("selection_for_find", json!({ "case_sensitive": true }));
                let _ = self.backend.send_edit("highlight_find", json!({ "visible": true }));
                self.push_jump();
                if forward {
                    let _ = self.backend.send_edit(
                        "find_next",
                        json!({ "wrap_around": true, "allow_same": false }),
                    );
                } else {
                    let _ = self.backend.send_edit(
                        "find_previous",
                        json!({ "wrap_around": true, "allow_same": false }),
                    );
                }
                // Mirror search_pattern from word under cursor for frontend highlighting.
                if let Some(line) = self.backend.lines.get(self.backend.cursor_line) {
                    let col = self.backend.cursor_col;
                    let ch = line[col..].chars().next();
                    if ch.map(|c| c.is_alphanumeric() || c == '_').unwrap_or(false) {
                        let start = line[..col]
                            .char_indices()
                            .rev()
                            .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
                            .last()
                            .map(|(i, _)| i)
                            .unwrap_or(col);
                        let end = col
                            + line[col..]
                                .char_indices()
                                .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
                                .last()
                                .map(|(i, c)| i + c.len_utf8())
                                .unwrap_or(0);
                        self.search_pattern = Some(line[start..end].to_owned());
                    }
                }
                if matches!(self.mode, Mode::Visual | Mode::VisualLine | Mode::VisualBlock) {
                    self.enter_normal_mode();
                }
            }
            Action::FindAll => {
                // Select all occurrences of the current search pattern in the buffer.
                let _ = self.backend.send_edit("find_all", json!([]));
                self.mode = Mode::Visual;
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
            // ── Tab navigation ───────────────────────────────────────────────
            Action::TabNext => {
                let new_vp = self.tabs.focus_next(self.viewport);
                self.viewport = new_vp;
                let new_buf = self.tabs.focused_windows().focused_window().buffer_id;
                let _ = self.backend.switch_to_id(new_buf);
            }
            Action::TabPrev => {
                let new_vp = self.tabs.focus_prev(self.viewport);
                self.viewport = new_vp;
                let new_buf = self.tabs.focused_windows().focused_window().buffer_id;
                let _ = self.backend.switch_to_id(new_buf);
            }
            Action::CommandHistoryOlder => self.history_older(),
            Action::CommandHistoryNewer => self.history_newer(),
            // ── Quickfix / location-list navigation ─────────────────────────────
            Action::QfNext => self.qf_next(true),
            Action::QfPrev => self.qf_prev(true),
            Action::LocNext => self.qf_next(false),
            Action::LocPrev => self.qf_prev(false),
            // ── Fold commands ────────────────────────────────────────────────────
            Action::FoldToggle => self.fold_toggle(),
            Action::FoldOpen => self.fold_open(),
            Action::FoldClose => self.fold_close(),
            Action::FoldOpenAll => self.fold_open_all(),
            Action::FoldCloseAll => self.fold_close_all(),
        }
    }

    fn handle_default(&mut self, key: KeyEvent) {
        // Cancel operator-pending on Escape
        if key.code == KeyCode::Esc && self.mode == Mode::OperatorPending {
            self.enter_normal_mode();
            self.input_state.reset();
            return;
        }

        // Tab completion in command-line mode.
        if key.code == KeyCode::Tab && self.mode == Mode::CommandLine {
            self.complete_command();
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
                } else if self.mode == Mode::Normal && c == 'w' {
                    // Ctrl-W: window command prefix.
                    self.input_state.awaiting_window_cmd = true;
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
                // Any typed char resets history navigation.
                self.history_idx = None;
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
                let case_sensitive = smart_case_sensitive(&chars);
                let _ = self.backend.send_edit(
                    "find",
                    json!({
                        "chars": chars,
                        "case_sensitive": case_sensitive,
                        "regex": false,
                        "whole_words": false
                    }),
                );
            }
            Mode::Visual | Mode::VisualLine | Mode::VisualBlock => {
                self.handle_visual_char(ch);
            }
            // SubstituteConfirm only accepts key codes (handled before we reach here);
            // any stray char is a no-op.
            Mode::SubstituteConfirm => {}
        }
    }

    // ── Mouse and bracketed-paste handling ────────────────────────────────────

    fn handle_mouse_event(&mut self, m: MouseEvent) {
        let Ok((width, height)) = crossterm::terminal::size() else {
            return;
        };
        self.handle_mouse_event_in_area(m, Rect { x: 0, y: 0, width, height });
    }

    pub(crate) fn handle_mouse_event_in_area(&mut self, m: MouseEvent, area: Rect) {
        match m.kind {
            MouseEventKind::ScrollUp => {
                let _ = self.backend.send_edit("scroll_up", json!([]));
            }
            MouseEventKind::ScrollDown => {
                let _ = self.backend.send_edit("scroll_down", json!([]));
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let Some((row, col)) = crate::ui::hit_test_buffer_cell(area, self, m.column, m.row)
                else {
                    return;
                };
                let line_count = self.backend.lines.len();
                if row < line_count {
                    let line = &self.backend.lines[row];
                    // Convert display column back to a byte offset.
                    let byte_col = crate::text::display_col_to_byte(line, col);
                    let _ = self.backend.send_edit(
                        "gesture",
                        json!({
                            "line": row as u64,
                            "col": byte_col as u64,
                            "ty": {
                                "select": {
                                    "granularity": "point",
                                    "multi": false
                                }
                            }
                        }),
                    );
                    // Exit any special mode on click.
                    if !matches!(self.mode, Mode::Normal | Mode::Insert) {
                        self.enter_normal_mode();
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some((row, col)) = crate::ui::hit_test_buffer_cell(area, self, m.column, m.row)
                else {
                    return;
                };
                let line_count = self.backend.lines.len();
                if row < line_count {
                    let line = &self.backend.lines[row];
                    let byte_col = crate::text::display_col_to_byte(line, col);
                    let _ = self.backend.send_edit(
                        "gesture",
                        json!({
                            "line": row as u64,
                            "col": byte_col as u64,
                            "ty": "drag"
                        }),
                    );
                }
            }
            _ => {}
        }
    }

    fn handle_paste(&mut self, text: String) {
        match self.mode {
            Mode::Insert => {
                // Bracketed paste stays backend-owned for undo grouping and multicursor behavior.
                self.insert_buffer.push_str(&text);
                let _ = self.backend.send_edit("paste", json!({ "chars": text }));
            }
            Mode::CommandLine | Mode::Search => {
                // Paste into the command/search buffer.
                self.command_buffer.push_str(&text);
                if self.mode == Mode::Search {
                    let chars = self.command_buffer.clone();
                    let case_sensitive = smart_case_sensitive(&chars);
                    let _ = self.backend.send_edit(
                        "find",
                        json!({
                            "chars": chars,
                            "case_sensitive": case_sensitive,
                            "regex": false,
                            "whole_words": false
                        }),
                    );
                }
            }
            Mode::Normal => {
                // In normal mode enter insert and paste the text, like pressing `i` then typing.
                self.mode = Mode::Insert;
                self.insert_buffer.push_str(&text);
                let _ = self.backend.send_edit("paste", json!({ "chars": text }));
            }
            _ => {}
        }
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
            let _ =
                self.backend.send_edit("move_to_left_end_of_line_and_modify_selection", json!([]));
            self.apply_operator(op);
            return;
        }

        // Priority 8: char-find operators (f/F/t/T).
        match ch {
            'f' | 'F' | 't' | 'T' => {
                let forward = matches!(ch, 'f' | 't');
                let inclusive = matches!(ch, 'f' | 'F');
                self.input_state.prefix = None;
                self.input_state.pending_find = Some(PendingCharFind { forward, inclusive });
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
                let _ = self.backend.send_edit("move_to_beginning_of_paragraph", json!([]));
                let _ = self
                    .backend
                    .send_edit("move_to_end_of_paragraph_and_modify_selection", json!([]));
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
        let case_sensitive = smart_case_sensitive(&chars);
        let _ = self.backend.send_edit(
            "find",
            json!({
                "chars": chars,
                "case_sensitive": case_sensitive,
                "regex": false,
                "whole_words": false
            }),
        );
        if self.search_backward {
            let _ = self
                .backend
                .send_edit("find_previous", json!({ "wrap_around": true, "allow_same": false }));
        } else {
            let _ = self
                .backend
                .send_edit("find_next", json!({ "wrap_around": true, "allow_same": false }));
        }
        // Store for repeated n/N navigation and buffer highlighting.
        self.search_pattern = if chars.is_empty() { None } else { Some(chars) };
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
        let raw = self.command_buffer.trim().to_owned();

        // Push non-empty commands to history (deduplicate consecutive duplicates).
        if !raw.is_empty() && self.command_history.last().map(|s| s.as_str()) != Some(&raw) {
            self.command_history.push(raw.clone());
            const HISTORY_MAX: usize = 100;
            if self.command_history.len() > HISTORY_MAX {
                self.command_history.remove(0);
            }
        }
        self.history_idx = None;

        // Parse an optional line-address range from the front of the command.
        let cursor_line = self.backend.cursor_line;
        let line_count = self.backend.lines.len().max(1);
        let (range, rest) = parse_ex_range(&raw, cursor_line, line_count, &self.marks);
        let command = rest.trim_start();

        // Bare range (e.g. `:5`, `:.`, `:%`) with no following command → jump.
        if command.is_empty() {
            if let Some((start, _end)) = range {
                self.jump_to_line(start);
                self.enter_normal_mode();
                return;
            }
            self.enter_normal_mode();
            return;
        }

        let mut parts = command.split_whitespace();
        match parts.next().unwrap_or_default() {
            "q" | "quit" => {
                // Guard against unsaved changes.
                if !self.backend.pristine {
                    self.backend.status_message =
                        Some("unsaved changes (use :w to save or :q! to force)".to_owned());
                    self.enter_normal_mode();
                    return;
                }
                self.should_quit = true;
            }
            "q!" | "quit!" => self.should_quit = true,
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
            // ── Substitute (s/pattern/replacement/flags) ─────────────────
            cmd if cmd == "s"
                || cmd == "substitute"
                || cmd.starts_with("s/")
                || cmd.starts_with("s!")
                || cmd.starts_with("s|")
                || cmd.starts_with("s,") =>
            {
                // Strip the leading command name, leaving the delimiter and body.
                let body = if cmd == "s" || cmd == "substitute" {
                    // delimiter and body are the next token (rest of command line).
                    let leftover = parts.collect::<Vec<_>>().join(" ");
                    if leftover.is_empty() {
                        self.backend.status_message =
                            Some("substitute: usage: s/pattern/replacement/[flags]".to_owned());
                        self.enter_normal_mode();
                        return;
                    }
                    leftover
                } else {
                    // cmd is like "s/pattern/..." — strip the leading "s".
                    cmd[1..].to_owned()
                };
                // Determine range: default current line, `%` already parsed.
                let (start, end) = range.unwrap_or((cursor_line, cursor_line));
                match parse_substitute_cmd(&body) {
                    Some((pattern, replacement, flags)) => {
                        self.execute_substitute(start, end, &pattern, &replacement, &flags);
                    }
                    None => {
                        self.backend.status_message =
                            Some("substitute: usage: s/pattern/replacement/[flags]".to_owned());
                    }
                }
                self.enter_normal_mode();
                return;
            }
            // ── Range-aware line operations ───────────────────────────────
            "d" | "delete" => {
                let (start, end) = range.unwrap_or((cursor_line, cursor_line));
                self.delete_line_range(start, end);
            }
            "y" | "yank" => {
                let (start, end) = range.unwrap_or((cursor_line, cursor_line));
                self.yank_line_range(start, end);
            }
            "format" => {
                if let Err(err) = self.backend.format_document() {
                    self.backend.status_message = Some(format!("format failed: {err}"));
                }
            }
            "complete" => {
                if let Err(err) = self.backend.request_completion(None) {
                    self.backend.status_message = Some(format!("completion failed: {err}"));
                }
            }
            "definition" | "def" => {
                if let Err(err) = self.backend.request_definition() {
                    self.backend.status_message = Some(format!("definition failed: {err}"));
                }
            }
            "references" | "refs" => {
                if let Err(err) = self.backend.request_references() {
                    self.backend.status_message = Some(format!("references failed: {err}"));
                }
            }
            "codeaction" | "codeactions" => {
                let action_index = parts.next().and_then(|part| part.parse::<usize>().ok());
                if let Err(err) = self.backend.request_code_actions(action_index) {
                    self.backend.status_message = Some(format!("code action failed: {err}"));
                }
            }
            "rename" => {
                let new_name = parts.collect::<Vec<_>>().join(" ");
                if new_name.is_empty() {
                    self.backend.status_message = Some(String::from("rename: usage: :rename new_name"));
                } else if let Err(err) = self.backend.request_rename(&new_name) {
                    self.backend.status_message = Some(format!("rename failed: {err}"));
                }
            }
            "diagnostics" => {
                self.open_diagnostics_location_list();
            }
            "hover" => {
                let position = Some((self.backend.cursor_line, self.backend.cursor_col));
                if let Err(err) = self.backend.request_hover(position) {
                    self.backend.status_message = Some(format!("hover failed: {err}"));
                }
            }
            "reindent" => {
                let _ = self.backend.send_edit("reindent", json!([]));
            }
            "help" => {
                self.open_help_picker("Help", Self::help_items());
                return;
            }
            "commands" => {
                self.open_help_picker("Commands", Self::command_help_items());
                return;
            }
            "keymap" => {
                self.open_help_picker("Keymap", Self::keymap_help_items());
                return;
            }
            "protocol" => {
                self.open_help_picker("Protocol", Self::protocol_help_items());
                return;
            }
            "selectionforfind" => {
                let _ =
                    self.backend.send_edit("selection_for_find", json!({ "case_sensitive": true }));
                let _ = self.backend.send_edit("highlight_find", json!({ "visible": true }));
            }
            "selectionforreplace" => {
                let _ = self.backend.send_edit("selection_for_replace", json!([]));
            }
            "transpose" => {
                let _ = self.backend.send_edit("transpose", json!([]));
            }
            "duplicateline" => {
                let _ = self.backend.send_edit("duplicate_line", json!([]));
            }
            "increasenumber" => {
                let _ = self.backend.send_edit("increase_number", json!([]));
            }
            "decreasenumber" => {
                let _ = self.backend.send_edit("decrease_number", json!([]));
            }
            "multifind" => {
                let terms = parts.collect::<Vec<_>>();
                if terms.is_empty() {
                    self.backend.status_message =
                        Some("multifind: usage: :multifind term [term ...]".to_owned());
                } else {
                    let queries = terms
                        .into_iter()
                        .enumerate()
                        .map(|(index, term)| {
                            json!({
                                "id": index,
                                "chars": term,
                                "case_sensitive": smart_case_sensitive(term),
                                "regex": false,
                                "whole_words": false,
                            })
                        })
                        .collect::<Vec<_>>();
                    let _ = self.backend.send_edit("multi_find", json!({ "queries": queries }));
                }
            }
            "selectionintolines" | "splitselection" => {
                let _ = self.backend.send_edit("selection_into_lines", json!([]));
            }
            "addselabove" => {
                let _ = self.backend.send_edit("add_selection_above", json!([]));
            }
            "addselbelow" => {
                let _ = self.backend.send_edit("add_selection_below", json!([]));
            }
            "inserttab" => {
                let _ = self.backend.send_edit("insert_tab", json!([]));
            }
            // ── Buffer management ─────────────────────────────────────────
            "e" | "edit" => {
                let path = parts.next().map(PathBuf::from);
                match self.backend.open_buffer(path) {
                    Ok(buf_id) => {
                        let _ = self.backend.switch_to_id(buf_id);
                        self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
                        self.viewport = Viewport::default();
                    }
                    Err(err) => {
                        self.backend.status_message = Some(format!("open failed: {err}"));
                    }
                }
            }
            // Force-reload the current buffer from disk, discarding unsaved edits.
            "e!" | "edit!" => {
                let id = self.backend.active().id;
                match self.backend.reload_buffer(id) {
                    Ok(()) => {
                        self.viewport = Viewport::default();
                    }
                    Err(err) => {
                        self.backend.status_message = Some(format!("reload failed: {err}"));
                    }
                }
            }
            // Restore the crash-recovery artifact for the current buffer.
            "recover" => {
                let recovery_path = self
                    .backend
                    .active()
                    .path
                    .as_ref()
                    .and_then(|p| crate::buffer::recovery_file_path(p));
                match recovery_path {
                    Some(rp) if rp.exists() => match self.backend.open_buffer(Some(rp)) {
                        Ok(buf_id) => {
                            let _ = self.backend.switch_to_id(buf_id);
                            self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
                            self.viewport = Viewport::default();
                        }
                        Err(err) => {
                            self.backend.status_message = Some(format!("recover failed: {err}"));
                        }
                    },
                    Some(_) => {
                        self.backend.status_message = Some("no recovery file found".to_owned());
                    }
                    None => {
                        self.backend.status_message =
                            Some("current buffer has no backing file".to_owned());
                    }
                }
            }
            // Delete the crash-recovery artifact for the current buffer.
            "recoverdel" => {
                let recovery_path = self
                    .backend
                    .active()
                    .path
                    .as_ref()
                    .and_then(|p| crate::buffer::recovery_file_path(p));
                match recovery_path {
                    Some(rp) if rp.exists() => match std::fs::remove_file(&rp) {
                        Ok(()) => {
                            self.backend.status_message = Some(format!("deleted {}", rp.display()));
                        }
                        Err(err) => {
                            self.backend.status_message = Some(format!("recoverdel failed: {err}"));
                        }
                    },
                    _ => {
                        self.backend.status_message = Some("no recovery file found".to_owned());
                    }
                }
            }
            // ── Quickfix list ───────────────────────────────────────────────
            "copen" | "cope" => {
                self.quickfix_open = true;
                if self.quickfix.as_ref().is_some_and(|q| !q.is_empty()) {
                    self.quickfix_focused = true;
                }
            }
            "cclose" | "ccl" => {
                self.quickfix_open = false;
                self.quickfix_focused = false;
            }
            "cn" | "cnext" => self.qf_next(true),
            "cp" | "cprev" | "cprevious" => self.qf_prev(true),
            "cfirst" => {
                if let Some(qf) = self.quickfix.as_mut() {
                    let entry = qf.first_entry().cloned();
                    if let Some(e) = entry {
                        self.navigate_to_qf_entry(e);
                    }
                }
            }
            "clast" => {
                if let Some(qf) = self.quickfix.as_mut() {
                    let entry = qf.last_entry().cloned();
                    if let Some(e) = entry {
                        self.navigate_to_qf_entry(e);
                    }
                }
            }
            "cc" => {
                let n = parts.next().and_then(|s| s.parse::<usize>().ok()).unwrap_or(1);
                if let Some(qf) = self.quickfix.as_mut() {
                    let entry = qf.select_one_based(n).cloned();
                    if let Some(e) = entry {
                        self.navigate_to_qf_entry(e);
                    }
                }
            }
            "clist" | "cl" => {
                let msg = match &self.quickfix {
                    None => "no quickfix list".to_owned(),
                    Some(qf) if qf.is_empty() => "quickfix list is empty".to_owned(),
                    Some(qf) => qf
                        .entries
                        .iter()
                        .enumerate()
                        .map(|(i, e)| {
                            let marker = if i == qf.selected { ">" } else { " " };
                            format!("{marker}{}: {}", i + 1, e.display_label())
                        })
                        .collect::<Vec<_>>()
                        .join("  "),
                };
                self.backend.status_message = Some(msg);
            }
            // ── Location list ──────────────────────────────────────────────
            "lopen" | "lop" => {
                self.location_list_open = true;
                if self.location_list.as_ref().is_some_and(|l| !l.is_empty()) {
                    self.location_list_focused = true;
                }
            }
            "lclose" | "lcl" => {
                self.location_list_open = false;
                self.location_list_focused = false;
            }
            "lnext" | "ln" => self.qf_next(false),
            "lprev" | "lp" | "lprevious" => self.qf_prev(false),
            "lfirst" => {
                if let Some(ll) = self.location_list.as_mut() {
                    let entry = ll.first_entry().cloned();
                    if let Some(e) = entry {
                        self.navigate_to_qf_entry(e);
                    }
                }
            }
            "llast" => {
                if let Some(ll) = self.location_list.as_mut() {
                    let entry = ll.last_entry().cloned();
                    if let Some(e) = entry {
                        self.navigate_to_qf_entry(e);
                    }
                }
            }
            "ll" => {
                let n = parts.next().and_then(|s| s.parse::<usize>().ok()).unwrap_or(1);
                if let Some(ll) = self.location_list.as_mut() {
                    let entry = ll.select_one_based(n).cloned();
                    if let Some(e) = entry {
                        self.navigate_to_qf_entry(e);
                    }
                }
            }
            "bn" | "bnext" => {
                let old = self.backend.active().id;
                self.backend.next_buffer();
                let new = self.backend.active().id;
                if old != new {
                    self.tabs.focused_windows_mut().set_focused_buffer(new);
                    self.viewport = Viewport::default();
                }
            }
            "bp" | "bprev" | "bprevious" => {
                let old = self.backend.active().id;
                self.backend.prev_buffer();
                let new = self.backend.active().id;
                if old != new {
                    self.tabs.focused_windows_mut().set_focused_buffer(new);
                    self.viewport = Viewport::default();
                }
            }
            "b#" => match self.backend.switch_alternate() {
                Ok(()) => {
                    let new = self.backend.active().id;
                    self.tabs.focused_windows_mut().set_focused_buffer(new);
                    self.viewport = Viewport::default();
                }
                Err(err) => {
                    self.backend.status_message = Some(format!("{err}"));
                }
            },
            "bd" | "bdelete" => {
                let id = self.backend.active().id;
                if let Err(err) = self.backend.close_buffer(id) {
                    self.backend.status_message = Some(format!("close failed: {err}"));
                } else {
                    let new = self.backend.active().id;
                    self.tabs.focused_windows_mut().set_focused_buffer(new);
                    self.viewport = Viewport::default();
                }
            }
            "ls" | "buffers" | "Buffers" => {
                let list = self.backend.list_buffers_str();
                self.backend.status_message = Some(list);
            }
            // ── Window splits ─────────────────────────────────────────────
            "sp" | "split" => {
                let path = parts.next().map(PathBuf::from);
                let buf_id = if let Some(p) = path {
                    match self.backend.open_buffer(Some(p)) {
                        Ok(id) => id,
                        Err(err) => {
                            self.backend.status_message = Some(format!("open failed: {err}"));
                            self.enter_normal_mode();
                            return;
                        }
                    }
                } else {
                    self.backend.active().id
                };
                let (_, new_vp) = self.tabs.focused_windows_mut().split(
                    SplitDir::Horizontal,
                    buf_id,
                    self.viewport,
                );
                self.viewport = new_vp;
                let _ = self.backend.switch_to_id(buf_id);
            }
            "vs" | "vsplit" => {
                let path = parts.next().map(PathBuf::from);
                let buf_id = if let Some(p) = path {
                    match self.backend.open_buffer(Some(p)) {
                        Ok(id) => id,
                        Err(err) => {
                            self.backend.status_message = Some(format!("open failed: {err}"));
                            self.enter_normal_mode();
                            return;
                        }
                    }
                } else {
                    self.backend.active().id
                };
                let (_, new_vp) = self.tabs.focused_windows_mut().split(
                    SplitDir::Vertical,
                    buf_id,
                    self.viewport,
                );
                self.viewport = new_vp;
                let _ = self.backend.switch_to_id(buf_id);
            }
            // ── Tab pages ─────────────────────────────────────────────────
            "tabnew" | "tabe" | "tabedit" => {
                let path = parts.next().map(PathBuf::from);
                let buf_id = match self.backend.open_buffer(path) {
                    Ok(id) => id,
                    Err(err) => {
                        self.backend.status_message = Some(format!("open failed: {err}"));
                        self.enter_normal_mode();
                        return;
                    }
                };
                let new_vp = self.tabs.new_tab(buf_id, self.viewport);
                self.viewport = new_vp;
                let _ = self.backend.switch_to_id(buf_id);
            }
            "tabc" | "tabclose" => {
                if let Some(new_vp) = self.tabs.close_tab(self.viewport) {
                    self.viewport = new_vp;
                    let new_buf = self.tabs.focused_windows().focused_window().buffer_id;
                    let _ = self.backend.switch_to_id(new_buf);
                } else {
                    // Only one tab: just quit.
                    self.should_quit = true;
                }
            }
            "tabn" | "tabnext" => {
                let new_vp = self.tabs.focus_next(self.viewport);
                self.viewport = new_vp;
                let new_buf = self.tabs.focused_windows().focused_window().buffer_id;
                let _ = self.backend.switch_to_id(new_buf);
            }
            "tabp" | "tabprev" | "tabprevious" => {
                let new_vp = self.tabs.focus_prev(self.viewport);
                self.viewport = new_vp;
                let new_buf = self.tabs.focused_windows().focused_window().buffer_id;
                let _ = self.backend.switch_to_id(new_buf);
            }
            "tabs" => {
                let info = (0..self.tabs.tab_count())
                    .map(|i| {
                        let marker = if i == self.tabs.focused_idx() { '>' } else { ' ' };
                        format!("{marker} Tab {}", i + 1)
                    })
                    .collect::<Vec<_>>()
                    .join("  ");
                self.backend.status_message = Some(info);
            }
            // ── Pickers ───────────────────────────────────────────────────
            "files" | "Files" => {
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                self.picker = Some(PickerState::new_files(cwd));
                self.enter_normal_mode();
                return;
            }
            "bpick" => {
                let entries: Vec<_> = self
                    .backend
                    .all_bufs()
                    .iter()
                    .map(|b| (b.id, b.title(), b.path.clone()))
                    .collect();
                self.picker = Some(PickerState::new_buffers(entries));
                self.enter_normal_mode();
                return;
            }
            "grep" | "Grep" => {
                let query = parts.collect::<Vec<_>>().join(" ");
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                self.picker = Some(PickerState::new_grep(query, cwd));
                self.enter_normal_mode();
                return;
            }
            // ── :set option[=value] ──────────────────────────────────────────
            "set" => {
                let opt = parts.next().unwrap_or_default();
                self.apply_set_option(opt);
            }
            // ── :noh — clear search highlight ────────────────────────────────
            "noh" | "nohlsearch" => {
                self.search_pattern = None;
                let _ = self.backend.send_edit("highlight_find", json!({ "visible": false }));
                self.backend.status_message = Some("search highlight cleared".to_owned());
            }
            other if !other.is_empty() => {
                self.backend.status_message = Some(format!("unknown command: {other}"));
            }
            _ => {}
        }
        self.enter_normal_mode();
    }

    /// Parse and apply a single `:set` option string (e.g. `"wrap"`, `"nowrap"`,
    /// `"scrolloff=5"`, `"colorcolumn=80"`, `"colorcolumn="`).
    fn apply_set_option(&mut self, opt: &str) {
        use crate::config::{NumberStyle, StatuslineFormat};

        // Split on `=` for options that take a value.
        if let Some((key, val)) = opt.split_once('=') {
            match key {
                "scrolloff" | "so" => {
                    if let Ok(n) = val.parse::<usize>() {
                        self.config.scroll_offset = n;
                    }
                }
                "colorcolumn" | "cc" => {
                    self.config.color_column = val.parse::<usize>().ok().filter(|&n| n > 0);
                }
                "statusline" | "stl" => match val {
                    "default" => self.config.statusline_format = StatuslineFormat::Default,
                    "minimal" => self.config.statusline_format = StatuslineFormat::Minimal,
                    _ => {}
                },
                "number" | "nu" | "nonu" | "nonumber" => {} // handled below
                _ => {
                    self.backend.status_message = Some(format!("unknown option: {key}"));
                    return;
                }
            }
        } else {
            // Boolean / enum options (no `=`).
            match opt {
                "number" | "nu" => self.config.number_style = NumberStyle::Absolute,
                "nonumber" | "nonu" => self.config.number_style = NumberStyle::Absolute,
                "relativenumber" | "rnu" => {
                    self.config.number_style = NumberStyle::Relative;
                }
                "norelativenumber" | "nornu" => {
                    self.config.number_style = NumberStyle::Absolute;
                }
                "relativenumberabsolute" | "rnua" => {
                    self.config.number_style = NumberStyle::RelativeAbsolute;
                }
                "wrap" => self.config.wrap_lines = true,
                "nowrap" => self.config.wrap_lines = false,
                "cursorline" | "cul" => self.config.cursor_line = true,
                "nocursorline" | "nocul" => self.config.cursor_line = false,
                "list" => self.config.show_visible_whitespace = true,
                "nolist" => self.config.show_visible_whitespace = false,
                "signcolumn" | "smc" => self.config.sign_column = true,
                "nosigncolumn" | "nosmc" => self.config.sign_column = false,
                other => {
                    self.backend.status_message = Some(format!("unknown option: {other}"));
                    return;
                }
            }
        }
        self.backend.status_message = Some(format!("set: {opt}"));
    }

    // ── Ex command history ──────────────────────────────────────────────────

    /// Navigate to the previous (older) command in history.
    fn history_older(&mut self) {
        if self.command_history.is_empty() {
            return;
        }
        let new_idx = match self.history_idx {
            None => {
                // Save whatever the user was typing before browsing history.
                self.history_draft = self.command_buffer.clone();
                self.command_history.len().saturating_sub(1)
            }
            Some(i) if i > 0 => i - 1,
            Some(i) => i,
        };
        self.history_idx = Some(new_idx);
        self.command_buffer = self.command_history[new_idx].clone();
    }

    /// Navigate to the next (newer) command in history, or back to the draft.
    fn history_newer(&mut self) {
        let Some(idx) = self.history_idx else { return };
        if idx + 1 >= self.command_history.len() {
            self.history_idx = None;
            self.command_buffer = self.history_draft.clone();
        } else {
            let new_idx = idx + 1;
            self.history_idx = Some(new_idx);
            self.command_buffer = self.command_history[new_idx].clone();
        }
    }

    // ── Tab completion ──────────────────────────────────────────────────────

    /// Complete `command_buffer` to the first matching command name.
    fn complete_command(&mut self) {
        const COMMANDS: &[&str] = &[
            "b#",
            "bd",
            "bdelete",
            "bn",
            "bnext",
            "bp",
            "bprev",
            "bprevious",
            "buffers",
            "Buffers",
            "cc",
            "ccl",
            "cclose",
            "cfirst",
            "cl",
            "clast",
            "clist",
            "cn",
            "cnext",
            "cope",
            "copen",
            "cp",
            "cprev",
            "cprevious",
            "codeaction",
            "codeactions",
            "complete",
            "d",
            "def",
            "definition",
            "delete",
            "diagnostics",
            "e",
            "e!",
            "edit",
            "edit!",
            "commands",
            "decreasenumber",
            "duplicateline",
            "files",
            "Files",
            "format",
            "grep",
            "Grep",
            "help",
            "hover",
            "increasenumber",
            "inserttab",
            "keymap",
            "lcl",
            "lclose",
            "lfirst",
            "llast",
            "ln",
            "lnext",
            "lop",
            "lopen",
            "lp",
            "lprev",
            "lprevious",
            "ls",
            "multifind",
            "protocol",
            "q",
            "q!",
            "quit",
            "quit!",
            "recover",
            "recoverdel",
            "reindent",
            "rename",
            "references",
            "refs",
            "selectionforfind",
            "selectionforreplace",
            "selectionintolines",
            "splitselection",
            "sp",
            "split",
            "tabc",
            "tabclose",
            "tabe",
            "tabedit",
            "tabn",
            "tabnext",
            "tabnew",
            "tabp",
            "tabprev",
            "tabprevious",
            "tabs",
            "transpose",
            "addselabove",
            "addselbelow",
            "vs",
            "vsplit",
            "w",
            "wq",
            "write",
            "x",
            "y",
            "yank",
        ];
        let prefix = self.command_buffer.clone();
        let candidates: Vec<&&str> = COMMANDS.iter().filter(|c| c.starts_with(&*prefix)).collect();
        if let Some(&&first) = candidates.first() {
            self.command_buffer = first.to_owned();
        }
    }

    fn open_help_picker(&mut self, title: &str, items: Vec<String>) {
        self.picker = Some(PickerState::new_help(title, items));
        self.enter_normal_mode();
    }

    fn help_items() -> Vec<String> {
        vec![
            "Discovery: :commands | :keymap | :protocol".to_owned(),
            "Modes: i insert | v visual | V visual-line | Ctrl-V visual-block | : command".to_owned(),
            "Move: h j k l | w b e | gg G | % | * # | n N".to_owned(),
            "Edit: d c y operators | p/P register paste | u undo | Ctrl-R redo | . repeat".to_owned(),
            "IDE: :hover :complete :codeaction :definition :references :rename :diagnostics".to_owned(),
            "Backend ops: :transpose :duplicateline :increasenumber :decreasenumber :reindent".to_owned(),
            "Selections: :selectionforfind :selectionforreplace :selectionintolines :addselabove :addselbelow".to_owned(),
            "Search sets: :multifind term [term ...]".to_owned(),
            "Workspace: :files :bpick :grep :buffers :split :vsplit :tabnew".to_owned(),
        ]
    }

    fn command_help_items() -> Vec<String> {
        vec![
            ":help open searchable editor help".to_owned(),
            ":commands list ex commands and features".to_owned(),
            ":keymap list high-value normal-mode bindings".to_owned(),
            ":protocol show exposed vs retired xi-core protocol surface".to_owned(),
            ":hover request LSP hover at cursor".to_owned(),
            ":complete open completion picker from backend suggestions".to_owned(),
            ":codeaction open backend code-action picker".to_owned(),
            ":rename new_name request backend rename at cursor".to_owned(),
            ":diagnostics open location list for active-buffer diagnostics".to_owned(),
            ":reindent run core reindent on current selection or line".to_owned(),
            ":transpose backend transpose at cursor".to_owned(),
            ":duplicateline backend duplicate current selection line(s)".to_owned(),
            ":increasenumber / :decreasenumber adjust number under cursor".to_owned(),
            ":selectionforfind / :selectionforreplace lift selection into find or replace"
                .to_owned(),
            ":selectionintolines split selection into per-line cursors".to_owned(),
            ":addselabove / :addselbelow grow multi-cursor set".to_owned(),
            ":multifind term [term ...] run backend multi-find queries".to_owned(),
        ]
    }

    fn keymap_help_items() -> Vec<String> {
        vec![
            "K request hover".to_owned(),
            "Ctrl-A increase number under cursor".to_owned(),
            "Ctrl-X decrease number under cursor".to_owned(),
            "Ctrl-Up add selection above".to_owned(),
            "Ctrl-Down add selection below".to_owned(),
            "gd duplicate current line or selection".to_owned(),
            "* / # selection-for-find forward/backward".to_owned(),
            "gt / gT next and previous tab".to_owned(),
            "]q / [q quickfix next and previous".to_owned(),
            "]Q / [Q location list next and previous".to_owned(),
            "z a o c R M fold toggle/open/close/open-all/close-all".to_owned(),
            "Ctrl-O / Tab jump list older/newer".to_owned(),
            "g; / g, change list older/newer".to_owned(),
        ]
    }

    fn protocol_help_items() -> Vec<String> {
        vec![
            "Exposed backend edits: request_hover transpose duplicate_line increase_number decrease_number multi_find reindent".to_owned(),
            "Exposed selection edits: selection_for_find selection_for_replace selection_into_lines add_selection_above add_selection_below insert_tab".to_owned(),
            "Canonical mouse path: gesture.select and gesture.drag; legacy click/drag shims avoided in ee-tui".to_owned(),
            "Frontend-owned by design: paste, cut, copy, registers, clipboard, viewport resize".to_owned(),
            "Removed from frontend protocol: get_config debug_get_contents set_theme modify_user_config tracing_config save_trace set_language cut copy".to_owned(),
            "Removed legacy edit protocol commands no longer supported by xi crates".to_owned(),
            "Intentional remaining legacy exposure: reindent via :reindent only".to_owned(),
        ]
    }

    // ── Range-based line operations ─────────────────────────────────────────

    /// Delete lines `start..=end` (0-based, inclusive).
    fn delete_line_range(&mut self, start: usize, end: usize) {
        let _ = self.backend.delete_line_range(start, end);
        self.push_change();
    }

    /// Yank lines `start..=end` (0-based, inclusive) into the active register.
    fn yank_line_range(&mut self, start: usize, end: usize) {
        let line_count = self.backend.lines.len();
        if line_count == 0 {
            return;
        }
        let start = start.min(line_count.saturating_sub(1));
        let end = end.min(line_count.saturating_sub(1));
        let mut text = String::new();
        for line in &self.backend.lines[start..=end] {
            text.push_str(line);
            text.push('\n');
        }
        let reg = self.take_register();
        self.registers.yank(&reg, text, false);
        let count = end.saturating_sub(start) + 1;
        self.backend.status_message = Some(format!("{count} line(s) yanked"));
    }

    // ── Cursor jump ────────────────────────────────────────────────────────

    /// Jump the cursor to `line` (0-based), clamped to the buffer length.
    fn jump_to_line(&mut self, line: usize) {
        let clamped = line.min(self.backend.lines.len().saturating_sub(1));
        self.push_jump();
        let _ = self.backend.send_edit(
            "gesture",
            json!({ "line": clamped as u64, "col": 0u64, "ty": "point_select" }),
        );
    }

    // ── Quickfix and location list ──────────────────────────────────────────

    /// Handle key events when the quickfix or location-list panel is focused.
    /// `is_quickfix=true` for the quickfix list, `false` for the location list.
    fn handle_qf_focused_event(&mut self, key: KeyEvent, is_quickfix: bool) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                if is_quickfix {
                    self.quickfix_focused = false;
                } else {
                    self.location_list_focused = false;
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if is_quickfix {
                    if let Some(qf) = self.quickfix.as_mut() {
                        qf.move_down();
                    }
                } else if let Some(ll) = self.location_list.as_mut() {
                    ll.move_down();
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if is_quickfix {
                    if let Some(qf) = self.quickfix.as_mut() {
                        qf.move_up();
                    }
                } else if let Some(ll) = self.location_list.as_mut() {
                    ll.move_up();
                }
            }
            KeyCode::Enter => {
                let entry = if is_quickfix {
                    self.quickfix.as_ref().and_then(|q| q.current()).cloned()
                } else {
                    self.location_list.as_ref().and_then(|l| l.current()).cloned()
                };
                if let Some(e) = entry {
                    self.navigate_to_qf_entry(e);
                }
                // Return focus to the editor after navigation.
                if is_quickfix {
                    self.quickfix_focused = false;
                } else {
                    self.location_list_focused = false;
                }
            }
            _ => {}
        }
    }

    /// Navigate the quickfix list (is_quickfix=true) or location list forward.
    fn qf_next(&mut self, is_quickfix: bool) {
        let entry = if is_quickfix {
            self.quickfix.as_mut().and_then(|q| q.next_entry()).cloned()
        } else {
            self.location_list.as_mut().and_then(|l| l.next_entry()).cloned()
        };
        if let Some(e) = entry {
            self.navigate_to_qf_entry(e);
        }
    }

    /// Navigate the quickfix list (is_quickfix=true) or location list backward.
    fn qf_prev(&mut self, is_quickfix: bool) {
        let entry = if is_quickfix {
            self.quickfix.as_mut().and_then(|q| q.prev_entry()).cloned()
        } else {
            self.location_list.as_mut().and_then(|l| l.prev_entry()).cloned()
        };
        if let Some(e) = entry {
            self.navigate_to_qf_entry(e);
        }
    }

    /// Open the file for `entry` and jump to the recorded line/column.
    fn navigate_to_qf_entry(&mut self, entry: QfEntry) {
        let Some(path) = entry.path.clone() else {
            self.backend.status_message = Some(format!("quickfix: line {}", entry.line + 1));
            return;
        };
        // Reuse an already-open buffer if possible.
        let existing_id = self
            .backend
            .all_bufs()
            .iter()
            .find(|b| b.path.as_ref().is_some_and(|p| *p == path))
            .map(|b| b.id);
        let buf_id = if let Some(id) = existing_id {
            let _ = self.backend.switch_to_id(id);
            id
        } else {
            match self.backend.open_buffer(Some(path)) {
                Ok(id) => {
                    let _ = self.backend.switch_to_id(id);
                    id
                }
                Err(err) => {
                    self.backend.status_message = Some(format!("quickfix: {err}"));
                    return;
                }
            }
        };
        self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
        self.viewport = Viewport::default();
        self.jump_to_line(entry.line);
    }

    /// Drain pending location results from the backend and populate the
    /// quickfix list.  Called each frame from the main loop.
    pub(crate) fn handle_pending_ui_actions(&mut self) {
        let active_view_id = self.backend.active().view_id.clone();
        for action in self.backend.drain_pending_ui_actions() {
            match action {
                PendingUiAction::Hover { view_id, content } if view_id == active_view_id => {
                    self.hover_popup = Some(HoverPopup {
                        title: String::from("Hover"),
                        content,
                    });
                }
                PendingUiAction::Completions { view_id, items } if view_id == active_view_id => {
                    self.open_completion_picker(&items);
                }
                PendingUiAction::CodeActions { view_id, actions } if view_id == active_view_id => {
                    self.open_code_action_picker(&actions);
                }
                _ => {}
            }
        }
    }

    pub(crate) fn handle_pending_locations(&mut self) {
        let locations = self.backend.drain_pending_locations();
        let active_view_id = self.backend.active().view_id.clone();
        for (view_id, title, targets) in locations {
            if view_id != active_view_id {
                continue;
            }
            if targets.is_empty() {
                continue;
            }
            // Single same-file result: jump directly and skip opening quickfix.
            if targets.len() == 1 {
                let t = &targets[0];
                let same_file = self
                    .backend
                    .active()
                    .path
                    .as_ref()
                    .is_some_and(|p| p.to_string_lossy() == t.path);
                if same_file {
                    let _ = self.backend.send_edit("goto_line", json!({ "line": t.line }));
                    continue;
                }
                // Different file with one result: navigate and skip panel.
                let path = PathBuf::from(&t.path);
                let line = t.line;
                match self.backend.open_buffer(Some(path)) {
                    Ok(buf_id) => {
                        let _ = self.backend.switch_to_id(buf_id);
                        self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
                        self.viewport = Viewport::default();
                        self.jump_to_line(line);
                    }
                    Err(err) => {
                        self.backend.status_message = Some(format!("{title}: {err}"));
                    }
                }
                continue;
            }
            // Multiple results: populate quickfix and open the panel.
            let entries: Vec<QfEntry> = targets
                .iter()
                .map(|t| QfEntry {
                    path: Some(PathBuf::from(&t.path)),
                    line: t.line,
                    col: t.column,
                    message: format!("line {}", t.line + 1),
                })
                .collect();
            self.quickfix = Some(QfList::new(title, entries));
            self.quickfix_open = true;
            self.quickfix_focused = true;
        }
    }

    fn open_completion_picker(&mut self, items: &[CompletionSuggestion]) {
        self.hover_popup = None;
        if items.is_empty() {
            self.picker = None;
            self.backend.status_message = Some(String::from("no completions"));
            return;
        }
        self.picker = Some(PickerState::new_completions(items));
    }

    fn open_code_action_picker(&mut self, actions: &[CodeActionDescriptor]) {
        self.hover_popup = None;
        if actions.is_empty() {
            self.picker = None;
            self.backend.status_message = Some(String::from("no code actions"));
            return;
        }
        self.picker = Some(PickerState::new_code_actions(actions));
    }

    fn open_diagnostics_location_list(&mut self) {
        let buf = self.backend.active();
        if buf.diagnostics.is_empty() {
            self.backend.status_message = Some(String::from("no diagnostics"));
            return;
        }

        let entries = buf
            .diagnostics
            .iter()
            .map(|diagnostic| {
                let (line, col) = line_col_for_offset(&buf.lines, diagnostic.range.start);
                let severity = match diagnostic.severity {
                    xi_core_lib::plugin_rpc::DiagnosticSeverity::Error => "error",
                    xi_core_lib::plugin_rpc::DiagnosticSeverity::Warning => "warning",
                    xi_core_lib::plugin_rpc::DiagnosticSeverity::Information => "info",
                    xi_core_lib::plugin_rpc::DiagnosticSeverity::Hint => "hint",
                };
                QfEntry {
                    path: buf.path.clone(),
                    line,
                    col,
                    message: format!("[{severity}] {}", diagnostic.message),
                }
            })
            .collect::<Vec<_>>();
        self.location_list = Some(QfList::new("Diagnostics", entries));
        self.location_list_open = true;
        self.location_list_focused = true;
    }

    // ── Fold commands ─────────────────────────────────────────────────────────

    fn fold_extent_at_cursor(&self) -> Option<(usize, usize)> {
        indent_fold_extent(&self.backend.lines, self.backend.cursor_line)
    }

    fn fold_toggle(&mut self) {
        let line = self.backend.cursor_line;
        let extent = self.fold_extent_at_cursor().unwrap_or((line, line));
        let buf_id = self.backend.active().id;
        self.folds.toggle(buf_id, line, extent);
    }

    fn fold_open(&mut self) {
        let line = self.backend.cursor_line;
        let buf_id = self.backend.active().id;
        self.folds.open(buf_id, line);
    }

    fn fold_close(&mut self) {
        let line = self.backend.cursor_line;
        let buf_id = self.backend.active().id;
        if let Some(extent) = self.fold_extent_at_cursor() {
            self.folds.close(buf_id, line, extent);
        }
    }

    fn fold_open_all(&mut self) {
        let buf_id = self.backend.active().id;
        self.folds.open_all(buf_id);
    }

    fn fold_close_all(&mut self) {
        let buf_id = self.backend.active().id;
        let lines: Vec<String> = self.backend.lines.clone();
        self.folds.close_all(buf_id, &lines);
    }

    // ── Crash recovery ──────────────────────────────────────────────────────

    /// Write crash-recovery artifacts for all modified buffers with a backing
    /// file.  Called periodically from the main loop (every ~30 s).
    pub(crate) fn write_recovery_if_due(&mut self) {
        const INTERVAL_SECS: u64 = 30;
        let now = Instant::now();
        if now.duration_since(self.recovery_last_check).as_secs() < INTERVAL_SECS {
            return;
        }
        self.recovery_last_check = now;

        for buf in self.backend.all_bufs() {
            // Skip clean buffers and scratch buffers.
            if buf.pristine || buf.path.is_none() || buf.lines.is_empty() {
                continue;
            }
            let path = buf.path.as_ref().unwrap();
            let Some(recovery_path) = crate::buffer::recovery_file_path(path) else {
                continue;
            };
            if let Some(parent) = recovery_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let content: String = buf.lines.iter().flat_map(|l| [l.as_str(), "\n"]).collect();
            let _ = std::fs::write(&recovery_path, content);
        }
    }

    // ── Picker overlay ──────────────────────────────────────────────────────

    /// Route a key event to the active picker overlay.
    fn handle_picker_event(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.picker = None;
            }
            KeyCode::Enter => {
                self.handle_picker_confirm();
            }
            KeyCode::Up => {
                if let Some(p) = self.picker.as_mut() {
                    p.move_up();
                }
            }
            KeyCode::Down => {
                if let Some(p) = self.picker.as_mut() {
                    p.move_down();
                }
            }
            KeyCode::Backspace => {
                if let Some(p) = self.picker.as_mut() {
                    p.pop_char();
                }
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                if let Some(p) = self.picker.as_mut() {
                    p.push_char(c);
                }
            }
            _ => {}
        }
    }

    /// Confirm the currently selected picker item and close the overlay.
    fn handle_picker_confirm(&mut self) {
        let Some(picker) = self.picker.take() else { return };
        let Some(item) = picker.selected_item().cloned() else { return };

        match picker.kind {
            crate::picker::PickerKind::Files | crate::picker::PickerKind::LiveGrep => {
                let Some(path) = item.path else { return };
                match self.backend.open_buffer(Some(path)) {
                    Ok(buf_id) => {
                        let _ = self.backend.switch_to_id(buf_id);
                        self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
                        self.viewport = Viewport::default();
                        if let Some(line) = item.line {
                            self.jump_to_line(line);
                        }
                    }
                    Err(err) => {
                        self.backend.status_message = Some(format!("open failed: {err}"));
                    }
                }
            }
            crate::picker::PickerKind::Buffers => {
                let Some(buf_id) = item.buf_id else { return };
                if self.backend.switch_to_id(buf_id).is_ok() {
                    self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
                    self.viewport = Viewport::default();
                }
            }
            crate::picker::PickerKind::Completions => {
                let Some(index) = item.choice_index else { return };
                if let Err(err) = self.backend.request_completion(Some(index)) {
                    self.backend.status_message = Some(format!("completion failed: {err}"));
                }
            }
            crate::picker::PickerKind::CodeActions => {
                let Some(index) = item.choice_index else { return };
                if let Err(err) = self.backend.request_code_actions(Some(index)) {
                    self.backend.status_message = Some(format!("code action failed: {err}"));
                }
            }
            crate::picker::PickerKind::Help => {}
        }
    }

    pub(crate) fn scroll_into_view(&mut self, editor_height: usize) {
        if editor_height == 0 {
            return;
        }
        let cursor_line = self.backend.cursor_line;
        // Clamp scroll_offset to half the editor height to avoid pathological cases.
        let off = self.config.scroll_offset.min(editor_height / 2);

        if cursor_line < self.viewport.top_line + off {
            self.viewport.top_line = cursor_line.saturating_sub(off);
        } else if cursor_line + off + 1 > self.viewport.top_line + editor_height {
            self.viewport.top_line = cursor_line + off + 1 - editor_height;
        }
        // Clamp top_line so we never show blank rows at the bottom when there
        // are enough lines above to fill the editor area.
        let total_lines = self.backend.lines.len().max(1);
        let max_top = total_lines.saturating_sub(editor_height);
        if self.viewport.top_line > max_top {
            self.viewport.top_line = max_top;
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

    // ── Window management ──────────────────────────────────────────────────

    /// Handle a key pressed after `Ctrl-W` in Normal mode.
    fn handle_window_cmd(&mut self, c: char) {
        match c {
            // Horizontal split (same buffer).
            's' => {
                let buf_id = self.backend.active().id;
                let (_, new_vp) = self.tabs.focused_windows_mut().split(
                    SplitDir::Horizontal,
                    buf_id,
                    self.viewport,
                );
                self.viewport = new_vp;
            }
            // Vertical split (same buffer).
            'v' => {
                let buf_id = self.backend.active().id;
                let (_, new_vp) = self.tabs.focused_windows_mut().split(
                    SplitDir::Vertical,
                    buf_id,
                    self.viewport,
                );
                self.viewport = new_vp;
            }
            // Focus next window.
            'w' => {
                let new_vp = self.tabs.focused_windows_mut().focus_next(self.viewport);
                self.viewport = new_vp;
                let new_buf = self.tabs.focused_windows_mut().focused_window().buffer_id;
                let _ = self.backend.switch_to_id(new_buf);
            }
            // Focus previous window.
            'W' | 'p' => {
                let new_vp = self.tabs.focused_windows_mut().focus_prev(self.viewport);
                self.viewport = new_vp;
                let new_buf = self.tabs.focused_windows_mut().focused_window().buffer_id;
                let _ = self.backend.switch_to_id(new_buf);
            }
            // Close focused window.
            'c' | 'q' => {
                if let Some(new_vp) = self.tabs.focused_windows_mut().close_focused() {
                    self.viewport = new_vp;
                    let new_buf = self.tabs.focused_windows_mut().focused_window().buffer_id;
                    let _ = self.backend.switch_to_id(new_buf);
                }
            }
            _ => {}
        }
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
        let ((sl, sc), (el, ec)) =
            if (al, ac) <= (cl, cc) { ((al, ac), (cl, cc)) } else { ((cl, cc), (al, ac)) };
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
        self.record_edit("paste_register", json!({ "chars": text, "before": before }));
        self.push_change();
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
        let _ = self.backend.send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
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
        let bottom_len = self.backend.lines.get(bottom).map(|s| s.len()).unwrap_or(0);
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
        let (al, ac) =
            self.visual_anchor.unwrap_or((self.backend.cursor_line, self.backend.cursor_col));
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
        let _ = self.backend.delete_block(top, bottom, left_col, right_col);
        self.push_change();
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
        let (al, ac) =
            self.visual_anchor.unwrap_or((self.backend.cursor_line, self.backend.cursor_col));
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
        if bi.line_start < bi.line_end {
            let _ = self.backend.replay_block_insert(
                bi.line_start + 1,
                bi.line_end,
                bi.col,
                &text,
                bi.append,
            );
        }
    }

    // ── Substitute confirm mode ───────────────────────────────────────────

    /// Handle a key event while in `SubstituteConfirm` mode.
    pub(crate) fn handle_substitute_confirm(&mut self, key: crossterm::event::KeyCode) {
        use crossterm::event::KeyCode;
        match key {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.apply_substitute_current();
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                self.advance_substitute_confirm();
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                // Apply all remaining.
                while self.substitute_pending.as_ref().is_some_and(|s| s.current < s.matches.len())
                {
                    self.apply_substitute_current();
                }
            }
            KeyCode::Char('q') | KeyCode::Esc => {
                let applied = self.substitute_pending.as_ref().map(|s| s.applied).unwrap_or(0);
                self.substitute_pending = None;
                self.mode = Mode::Normal;
                self.backend.status_message = Some(format!("{applied} substitution(s) applied"));
            }
            _ => {}
        }
    }

    /// Apply the current pending substitution match and advance.
    fn apply_substitute_current(&mut self) {
        let Some(state) = self.substitute_pending.as_mut() else {
            return;
        };
        let idx = state.current;
        if idx >= state.matches.len() {
            return;
        }
        let replacement = state.matches[idx].clone();
        let _ = self.backend.apply_line_replacements(&[replacement]);
        state.applied += 1;
        state.current += 1;
        self.advance_substitute_confirm_inner();
    }

    /// Skip the current pending match and advance.
    fn advance_substitute_confirm(&mut self) {
        let Some(state) = self.substitute_pending.as_mut() else {
            return;
        };
        state.current += 1;
        self.advance_substitute_confirm_inner();
    }

    /// Finish confirmation if all matches exhausted, otherwise jump to next.
    fn advance_substitute_confirm_inner(&mut self) {
        let done = self.substitute_pending.as_ref().is_none_or(|s| s.current >= s.matches.len());
        if done {
            let applied = self.substitute_pending.as_ref().map(|s| s.applied).unwrap_or(0);
            self.substitute_pending = None;
            self.mode = Mode::Normal;
            self.backend.status_message = Some(format!("{applied} substitution(s) applied"));
        } else {
            let li = self.substitute_pending.as_ref().unwrap().matches
                [self.substitute_pending.as_ref().unwrap().current]
                .line;
            self.jump_to_line(li);
            let total = self.substitute_pending.as_ref().unwrap().matches.len();
            let current = self.substitute_pending.as_ref().unwrap().current;
            self.backend.status_message =
                Some(format!("substitute ({}/{total}) replace? [y/n/a/q]", current + 1));
        }
    }

    // ── Substitute helper ─────────────────────────────────────────────────

    /// Execute a `:s/pattern/replacement/flags` command on `lines[start..=end]`.
    ///
    /// Flags: `g` = replace all occurrences per line (default: first only),
    ///        `i` = case-insensitive (default: smart-case),
    ///        `c` = confirm each change interactively.
    ///
    /// Delegates substitute preview and apply work to xi-core so range and
    /// confirm semantics always operate on the authoritative rope.
    pub(crate) fn execute_substitute(
        &mut self,
        start: usize,
        end: usize,
        pattern: &str,
        replacement: &str,
        flags: &str,
    ) {
        if pattern.is_empty() {
            self.backend.status_message = Some("substitute: empty pattern".to_owned());
            return;
        }
        let global = flags.contains('g');
        let case_insensitive =
            flags.contains('i') || (!flags.contains('I') && !smart_case_sensitive(pattern));
        let confirm = flags.contains('c');
        let changes = match self.backend.substitute_preview(
            start,
            end,
            pattern,
            replacement,
            global,
            !case_insensitive,
        ) {
            Ok(changes) => changes,
            Err(err) => {
                self.backend.status_message = Some(format!("substitute: {err}"));
                return;
            }
        };

        if changes.is_empty() {
            self.backend.status_message = Some("substitute: pattern not found".to_owned());
            return;
        }

        if confirm {
            // Enter confirm mode.
            let total = changes.len();
            let first_line = changes[0].line;
            self.substitute_pending =
                Some(SubstitutePending { matches: changes, current: 0, applied: 0 });
            self.mode = Mode::SubstituteConfirm;
            self.jump_to_line(first_line);
            self.backend.status_message =
                Some(format!("substitute (1/{total}) replace? [y/n/a/q]"));
        } else {
            // Apply authoritative replacements in one backend-owned edit.
            let count = changes.len();
            let _ = self.backend.apply_line_replacements(&changes);
            self.push_change();
            self.backend.status_message =
                Some(format!("{count} substitution(s) on {count} line(s)"));
        }
    }
}

// ── Text object helpers (free functions) ─────────────────────────────────────

fn line_col_for_offset(lines: &[String], offset: usize) -> (usize, usize) {
    let mut remaining = offset;
    for (line_index, line) in lines.iter().enumerate() {
        let line_len = line.len();
        if remaining <= line_len {
            return (line_index, remaining);
        }
        remaining = remaining.saturating_sub(line_len + 1);
    }
    let line = lines.len().saturating_sub(1);
    let col = lines.get(line).map(|line| line.len()).unwrap_or(0);
    (line, col)
}

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
                if inner_start <= close { Some((inner_start, close)) } else { None }
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
        if inner_start <= close_pos { Some((inner_start, close_pos)) } else { None }
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
    let close_start =
        line[content_start..].find(close_tag.as_str()).map(|off| content_start + off)?;

    if inclusive {
        Some((open_angle, close_start + close_tag.len()))
    } else {
        Some((content_start, close_start))
    }
}

// ── Ex command range parser ───────────────────────────────────────────────────

/// Returns `true` if `query` should be treated as case-sensitive (smart-case:
/// treat as case-sensitive only when the query contains at least one uppercase
/// character).
pub(crate) fn smart_case_sensitive(query: &str) -> bool {
    query.chars().any(|c| c.is_uppercase())
}

/// Parse a `:s/pattern/replacement/flags` command body (everything after the
/// command name / whitespace). The delimiter is the first character after the
/// optional leading whitespace. Returns `(pattern, replacement, flags)` or
/// `None` when the syntax is invalid.
pub(crate) fn parse_substitute_cmd(body: &str) -> Option<(String, String, String)> {
    let delim = body.chars().next()?;
    if delim.is_alphanumeric() || delim == ' ' {
        return None; // delimiter must be a punctuation char
    }
    let rest = &body[delim.len_utf8()..];
    // Split on the unescaped delimiter (max 3 parts: pattern / replacement / flags).
    let mut parts: Vec<String> = Vec::with_capacity(3);
    let mut current = String::new();
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&nc) = chars.peek() {
                if nc == delim {
                    chars.next();
                    current.push(nc);
                    continue;
                }
            }
            current.push(c);
        } else if c == delim {
            parts.push(std::mem::take(&mut current));
            if parts.len() == 3 {
                break;
            }
        } else {
            current.push(c);
        }
    }
    // Remaining chars after the last delimiter (or the flags if only 2 delimiters seen).
    if parts.len() < 3 {
        parts.push(current);
    }

    let pattern = parts.first().cloned().unwrap_or_default();
    let replacement = parts.get(1).cloned().unwrap_or_default();
    let flags = parts.get(2).cloned().unwrap_or_default();
    Some((pattern, replacement, flags))
}

/// Parse a vim-style address range from the start of `input`.
///
/// Returns `(resolved_range, remaining_command_text)`.  The range is a pair of
/// 0-based line indices `(start, end)`.  Returns `None` for the range when no
/// valid address is found at the start of `input`.
///
/// Supported addresses: number (1-based), `.` (current), `$` (last), `%`
/// (whole file), `'c` (mark), with optional `+N`/`-N` offsets.
/// A single address without a `,` separator resolves to `(addr, addr)`.
pub(crate) fn parse_ex_range<'a>(
    input: &'a str,
    cursor_line: usize,
    line_count: usize,
    marks: &HashMap<char, (usize, usize)>,
) -> (Option<(usize, usize)>, &'a str) {
    let mut pos = 0;
    let bytes = input.as_bytes();

    // `%` shorthand for the whole file.
    if bytes.first() == Some(&b'%') {
        let end = line_count.saturating_sub(1);
        return (Some((0, end)), &input[1..]);
    }

    let Some(a1) = parse_addr(input, &mut pos, cursor_line, line_count, marks) else {
        return (None, input);
    };

    // Optional `,` separator for a second address.
    if bytes.get(pos) == Some(&b',') {
        pos += 1;
        let start_pos = pos;
        if let Some(a2) = parse_addr(input, &mut pos, cursor_line, line_count, marks) {
            return (Some((a1, a2)), &input[pos..]);
        }
        // Comma with no valid second address — treat first address as range.
        let _ = start_pos;
    }

    (Some((a1, a1)), &input[pos..])
}

/// Parse a single vim address (number / `.` / `$` / `'c`) with optional
/// `+N`/`-N` offsets at `bytes[*pos..]`.  Advances `*pos` past the address.
fn parse_addr(
    input: &str,
    pos: &mut usize,
    cursor_line: usize,
    line_count: usize,
    marks: &HashMap<char, (usize, usize)>,
) -> Option<usize> {
    let bytes = input.as_bytes();
    let base: usize = match bytes.get(*pos)? {
        b'.' => {
            *pos += 1;
            cursor_line
        }
        b'$' => {
            *pos += 1;
            line_count.saturating_sub(1)
        }
        b'\'' => {
            *pos += 1;
            let ch = input[*pos..].chars().next()?;
            *pos += ch.len_utf8();
            marks.get(&ch).map(|&(l, _)| l)?
        }
        b if b.is_ascii_digit() => {
            let start = *pos;
            while *pos < input.len() && input.as_bytes()[*pos].is_ascii_digit() {
                *pos += 1;
            }
            let n: usize = input[start..*pos].parse().ok()?;
            // Vim line numbers are 1-based; convert to 0-based.
            n.saturating_sub(1).min(line_count.saturating_sub(1))
        }
        _ => return None,
    };

    // Optional `+N` / `-N` offset.
    let mut val = base;
    loop {
        match bytes.get(*pos) {
            Some(b'+') => {
                *pos += 1;
                let n = parse_number(input, pos).unwrap_or(1);
                val = val.saturating_add(n);
            }
            Some(b'-') => {
                *pos += 1;
                let n = parse_number(input, pos).unwrap_or(1);
                val = val.saturating_sub(n);
            }
            _ => break,
        }
    }

    Some(val.min(line_count.saturating_sub(1)))
}

fn parse_number(input: &str, pos: &mut usize) -> Option<usize> {
    let start = *pos;
    while *pos < input.len() && input.as_bytes()[*pos].is_ascii_digit() {
        *pos += 1;
    }
    if *pos == start {
        return None;
    }
    input[start..*pos].parse().ok()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod range_tests {
    use super::parse_ex_range;
    use std::collections::HashMap;

    fn no_marks() -> HashMap<char, (usize, usize)> {
        HashMap::new()
    }

    #[test]
    fn bare_number_jumps_to_line() {
        let (range, rest) = parse_ex_range("5", 0, 10, &no_marks());
        assert_eq!(range, Some((4, 4))); // 1-based → 0-based
        assert_eq!(rest, "");
    }

    #[test]
    fn number_with_command() {
        let (range, rest) = parse_ex_range("3d", 0, 10, &no_marks());
        assert_eq!(range, Some((2, 2)));
        assert_eq!(rest, "d");
    }

    #[test]
    fn percent_is_whole_file() {
        let (range, rest) = parse_ex_range("%d", 0, 10, &no_marks());
        assert_eq!(range, Some((0, 9)));
        assert_eq!(rest, "d");
    }

    #[test]
    fn comma_range() {
        let (range, rest) = parse_ex_range("1,5d", 0, 10, &no_marks());
        assert_eq!(range, Some((0, 4)));
        assert_eq!(rest, "d");
    }

    #[test]
    fn dot_is_current_line() {
        let (range, rest) = parse_ex_range(".d", 3, 10, &no_marks());
        assert_eq!(range, Some((3, 3)));
        assert_eq!(rest, "d");
    }

    #[test]
    fn dollar_is_last_line() {
        let (range, rest) = parse_ex_range("$", 0, 10, &no_marks());
        assert_eq!(range, Some((9, 9)));
        assert_eq!(rest, "");
    }

    #[test]
    fn dot_comma_dollar() {
        let (range, rest) = parse_ex_range(".,$ d", 2, 10, &no_marks());
        assert_eq!(range, Some((2, 9)));
        assert_eq!(rest, " d");
    }

    #[test]
    fn offset_plus() {
        let (range, rest) = parse_ex_range(".+2d", 3, 10, &no_marks());
        assert_eq!(range, Some((5, 5)));
        assert_eq!(rest, "d");
    }

    #[test]
    fn no_range_returns_none() {
        let (range, rest) = parse_ex_range("w", 0, 10, &no_marks());
        assert_eq!(range, None);
        assert_eq!(rest, "w");
    }
}
