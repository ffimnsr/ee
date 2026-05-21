use super::*;

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
    /// Picker overlay focus.
    Picker,
    /// Quickfix panel focus.
    Quickfix,
    /// Location-list panel focus.
    LocationList,
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
            Mode::Picker => "PIC",
            Mode::Quickfix => "QFX",
            Mode::LocationList => "LOC",
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepeatableMotion {
    CharFind { target: char, forward: bool, inclusive: bool },
    MatchingPair,
    Quickfix { forward: bool, is_quickfix: bool },
    GitHunk { forward: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SwiftMotionTarget {
    pub(crate) line: usize,
    pub(crate) display_col: usize,
    pub(crate) end_display_col: usize,
    pub(crate) label: char,
    pub(crate) next_label: Option<char>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct SwiftMotionState {
    pub(crate) query: String,
    pub(crate) label_prefix: Option<char>,
    pub(crate) targets: Vec<SwiftMotionTarget>,
}

impl SwiftMotionState {
    pub(crate) fn prompt(&self) -> String {
        match self.query.chars().count() {
            0 => "swift_motion | type 2 ASCII chars | esc cancel".to_owned(),
            1 => format!("swift_motion | target: {}_ | esc cancel", self.query),
            _ if self.label_prefix.is_some() => format!(
                "swift_motion {} {}_ | choose final label | esc cancel",
                self.query,
                self.label_prefix.unwrap_or_default()
            ),
            _ if self.targets.iter().any(|target| target.next_label.is_some()) => {
                format!("swift_motion {} | choose label group | esc cancel", self.query)
            }
            _ if self.targets.is_empty() => {
                format!("swift_motion {} | no visible labels", self.query)
            }
            _ => format!("swift_motion {} | choose label | esc cancel", self.query),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct InputState {
    pub(crate) count_digits: Vec<u8>,
    pub(crate) prefix: Option<char>,
    pub(crate) key_sequence: Vec<crate::keymap::KeyPress>,
    pub(crate) key_sequence_last_input_at: Option<Instant>,
    pub(crate) pending_find: Option<PendingCharFind>,
    pub(crate) pending_operator: Option<Operator>,
    pub(crate) text_obj_inclusive: Option<bool>,
    /// Set when `"` is pressed; next char selects the target register.
    pub(crate) awaiting_register: bool,
    /// Set when `insert_register` is waiting for register name.
    pub(crate) awaiting_register_insert: bool,
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
    /// Set when `replace` is pressed; next char replaces current selection.
    pub(crate) awaiting_replace_char: bool,
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
        self.key_sequence.clear();
        self.key_sequence_last_input_at = None;
        self.pending_find = None;
        self.pending_operator = None;
        self.text_obj_inclusive = None;
        self.awaiting_register = false;
        self.awaiting_register_insert = false;
        self.pending_register = None;
        self.awaiting_mark_set = false;
        self.awaiting_mark_jump = None;
        self.awaiting_macro_record = false;
        self.awaiting_macro_replay = false;
        self.awaiting_window_cmd = false;
        self.awaiting_replace_char = false;
    }
}

#[derive(Debug)]
pub(crate) struct App {
    pub(crate) config: crate::config::EditorSettings,
    pub(crate) key_bindings: HashMap<crate::keymap::BindingKey, crate::keymap::Action>,
    pub(crate) key_sequences: crate::keymap::SequenceBindings,
    pub(crate) working_dir: PathBuf,
    pub(crate) backend: BufferManager,
    pub(crate) tabs: TabManager,
    pub(crate) mode: Mode,
    pub(crate) command_buffer: String,
    pub(crate) should_quit: bool,
    pub(crate) viewport: Viewport,
    pub(crate) last_editor_height: usize,
    pub(crate) last_editor_width: usize,
    pub(crate) input_state: InputState,
    /// Anchor position (line, col) when a visual mode was entered.
    pub(crate) visual_anchor: Option<(usize, usize)>,
    /// Cursor restored when a VLF visual-line selection is cancelled.
    pub(crate) visual_restore_cursor: Option<(usize, usize)>,
    /// Last visual selection for `gv` restore (mode, anchor_line, anchor_col).
    pub(crate) last_visual: Option<(Mode, usize, usize)>,
    /// Frontend register store.
    pub(crate) registers: RegisterStore,
    /// Last change recorded for `.` repeat.
    pub(crate) last_change: Option<LastChange>,
    /// Text accumulated while in insert mode (for `.` repeat).
    pub(crate) insert_buffer: String,
    /// When `true`, xi edit calls are recorded in `recorded_commands`.
    pub(super) recording: bool,
    /// Accumulates edit commands during operator application.
    pub(super) recorded_commands: Vec<(&'static str, serde_json::Value)>,
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
    pub(super) macro_buffer: Vec<KeyEvent>,
    /// Stored macros keyed by register name `a`–`z`.
    pub(crate) macros: HashMap<char, Vec<KeyEvent>>,
    /// Last register used for macro replay; `@@` replays this.
    pub(crate) last_macro: Option<char>,
    /// Last repeatable motion for Helix-style motion replay.
    pub(crate) last_repeatable_motion: Option<RepeatableMotion>,
    /// `true` while a macro is replaying to suppress nested recording.
    pub(super) macro_replaying: bool,
    // ── Ex command history ─────────────────────────────────────────────────
    /// Previously executed ex commands, oldest first.  Capped at 100.
    pub(super) command_history: Vec<String>,
    /// Current index while navigating history with Up/Down; `None` = off.
    pub(super) history_idx: Option<usize>,
    /// Saved `command_buffer` snapshot taken before history navigation began.
    pub(super) history_draft: String,
    /// Per-buffer syntax override set via `:set_language`.
    pub(crate) syntax_overrides: HashMap<crate::buffer::BufferId, String>,
    // ── Picker overlay ─────────────────────────────────────────────────────
    /// Active picker overlay (file picker, buffer picker, live grep).
    pub(crate) picker: Option<PickerState>,
    /// Last picker opened through the picker overlay.
    pub(crate) last_picker: Option<PickerState>,
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
    pub(super) recovery_last_check: Instant,
    // ── Syntax highlighting ─────────────────────────────────────────────────────
    /// Render-side syntax styling helper for backend-owned syntax spans.
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
    /// Cached git state keyed by buffer id.
    pub(crate) source_control: HashMap<crate::buffer::BufferId, crate::git::GitBufferCache>,
    /// Noncritical startup work that should run only after first frame lands.
    pub(crate) startup_deferred_work_pending: bool,
    /// Last time a user input event reached the app loop.
    pub(crate) last_input_at: Instant,
    /// Active swift-motion session over visible text.
    pub(crate) swift_motion: Option<SwiftMotionState>,
    // ── Substitute confirm state ──────────────────────────────────────────────────
    /// Pending substitutions awaiting `y`/`n`/`a`/`q` confirmation.
    pub(crate) substitute_pending: Option<SubstitutePending>,
    /// Force next frame to clear and redraw the terminal surface.
    pub(crate) redraw_requested: bool,
    /// Per-session render observability counters.
    pub(crate) render_metrics: crate::render_metrics::RenderMetrics,
}

impl App {
    pub(crate) fn from_path(path: Option<PathBuf>) -> io::Result<Self> {
        if let Err(error) = crate::config::configure_runtime_loader_for_file(path.as_deref(), true)
        {
            eprintln!("ee: warning: failed to configure runtime languages: {error}");
        }
        let (config, general_config, initial_overrides) =
            crate::config::xi_config_tables_for_file(path.as_deref());
        let lsp_config = crate::config::lsp_config_table_for_file(path.as_deref());
        let key_bindings = crate::keymap::bindings_for(&config.keymap);
        let key_sequences = crate::keymap::sequence_bindings_for(&config.keymap);
        let working_dir = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
        let mut backend = BufferManager::new_with_initial_config(
            path,
            general_config,
            initial_overrides,
            lsp_config,
        )?;
        let initial_buf_id = backend.active().id;

        // Notify user if a crash-recovery artifact exists for this file.
        if let Some(rp) =
            backend.active().path.as_ref().and_then(|p| crate::buffer::recovery_file_path(p))
            && rp.exists()
        {
            backend.status_message = Some(format!(
                "Recovery file found: {} — use :recover to restore or :recoverdel to discard",
                rp.display()
            ));
        }

        Ok(Self {
            config,
            key_bindings,
            key_sequences,
            working_dir,
            backend,
            tabs: TabManager::new(initial_buf_id),
            mode: Mode::Normal,
            command_buffer: String::new(),
            should_quit: false,
            viewport: Viewport::default(),
            last_editor_height: 0,
            last_editor_width: 0,
            input_state: InputState::default(),
            visual_anchor: None,
            visual_restore_cursor: None,
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
            last_repeatable_motion: None,
            macro_replaying: false,
            command_history: Vec::new(),
            history_idx: None,
            history_draft: String::new(),
            syntax_overrides: HashMap::new(),
            picker: None,
            last_picker: None,
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
            source_control: HashMap::new(),
            startup_deferred_work_pending: true,
            last_input_at: Instant::now(),
            swift_motion: None,
            substitute_pending: None,
            redraw_requested: false,
            render_metrics: crate::render_metrics::RenderMetrics::new(),
        })
    }
}
