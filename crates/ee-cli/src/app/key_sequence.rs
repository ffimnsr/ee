//! `impl App` methods: key_sequence domain.
use super::*;

impl App {
    pub(crate) fn command_history(&self) -> &[String] {
        &self.command_history
    }
    pub(crate) fn restore_command_history(&mut self, mut history: Vec<String>) {
        const HISTORY_MAX: usize = 100;
        if history.len() > HISTORY_MAX {
            let keep_from = history.len() - HISTORY_MAX;
            history.drain(..keep_from);
        }
        self.command_history = history;
        self.history_idx = None;
        self.history_draft.clear();
    }
    pub(crate) fn handle_event(&mut self, event: Event) {
        self.expire_key_sequence_if_idle();
        self.last_input_at = Instant::now();

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

        if self.picker.is_some() {
            if self.dispatch_context_key(key, Mode::Picker) {
                return;
            }
            self.handle_picker_text_input(key);
            return;
        }

        if self.hover_popup.is_some() && key.code == KeyCode::Esc {
            self.hover_popup = None;
            return;
        }

        if self.quickfix_focused {
            self.dispatch_context_key(key, Mode::Quickfix);
            return;
        }
        if self.location_list_focused {
            self.dispatch_context_key(key, Mode::LocationList);
            return;
        }

        if self.mode == Mode::SubstituteConfirm {
            self.dispatch_context_key(key, Mode::SubstituteConfirm);
            return;
        }
        if self.mode == Mode::PrivilegeConfirm {
            self.handle_privileged_save_confirm_key(key);
            return;
        }

        // Focused agents pane owns all keys (feature `agents`).
        #[cfg(feature = "agents")]
        if self.agents_focused() {
            if let Some(action) = self.lookup_action(key, Mode::Agent, None)
                && self.handle_agent_keybinding_action(action)
            {
                return;
            }
            self.handle_agent_key(key);
            return;
        }

        if self.handle_swift_motion_key(key) {
            return;
        }

        if key.code == KeyCode::Esc && self.has_pending_input_state() {
            self.cancel_pending_input_state();
            return;
        }

        // Capture keystrokes for the active macro recording (before processing).
        // We always push first and pop afterward if this key stops recording,
        // so the terminating `q` is not stored in the macro.
        if self.macro_register.is_some() && !self.macro_replaying {
            self.macro_buffer.push(key);
        }

        // Two-char awaiting states consume the next key unconditionally.
        if self.input_state.awaiting_register {
            self.input_state.awaiting_register = false;
            if self.input_state.awaiting_register_insert {
                self.input_state.awaiting_register_insert = false;
                if let KeyCode::Char(c) = key.code
                    && let Some(register) = RegisterName::from_char(c)
                {
                    self.insert_register_for_current_mode(register);
                }
                return;
            }
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
            if let KeyCode::Char(c) = key.code
                && c.is_ascii_lowercase()
            {
                self.set_mark(c);
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
            if let KeyCode::Char(c) = key.code
                && c.is_ascii_lowercase()
            {
                self.start_macro_record(c);
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

        if self.input_state.awaiting_replace_char {
            self.input_state.awaiting_replace_char = false;
            if let KeyCode::Char(c) = key.code {
                self.replace_with_char(c);
            }
            return;
        }

        if self.handle_key_sequence(key) {
            return;
        }

        let action = self.lookup_action(key, self.mode, self.input_state.prefix);

        if let Some(action) = action {
            self.dispatch(action, key);
            if self.mode == Mode::Normal
                && !matches!(key.code, KeyCode::Char(c) if c.is_ascii_digit())
                && self.input_state.prefix.is_none()
                && self.input_state.pending_find.is_none()
                && !self.input_state.awaiting_register
                && !self.input_state.awaiting_mark_set
                && self.input_state.awaiting_mark_jump.is_none()
                && !self.input_state.awaiting_macro_record
                && !self.input_state.awaiting_macro_replay
                && !self.input_state.awaiting_window_cmd
                && !self.input_state.awaiting_replace_char
            {
                self.input_state.reset();
            }
        } else {
            self.handle_default(key);
        }
    }
    pub(super) fn lookup_action(
        &self,
        key: KeyEvent,
        mode: Mode,
        prefix: Option<char>,
    ) -> Option<Action> {
        let binding_key = BindingKey { mode, key: key.code, modifiers: key.modifiers, prefix };

        self.key_bindings
            .get(&binding_key)
            .or_else(|| {
                if key.modifiers != KeyModifiers::NONE {
                    self.key_bindings
                        .get(&BindingKey { modifiers: KeyModifiers::NONE, ..binding_key })
                } else {
                    None
                }
            })
            .cloned()
    }
    pub(crate) fn active_key_sequence_node(&self) -> Option<&SequenceNode> {
        if self.input_state.key_sequence.is_empty() {
            return None;
        }
        self.key_sequences.node_for_sequence(self.mode, &self.input_state.key_sequence)
    }
    pub(crate) fn active_key_sequence_label(&self) -> Option<String> {
        self.active_key_sequence_node()?;
        Some(crate::keymap::format_key_sequence(&self.input_state.key_sequence))
    }
    pub(crate) fn active_key_hint_label(&self) -> Option<String> {
        if let Some(label) = self.active_key_sequence_label() {
            return Some(label);
        }
        if let Some(prefix) = self.input_state.prefix {
            return Some(prefix.to_string());
        }
        if self.input_state.awaiting_register {
            return Some(if self.input_state.awaiting_register_insert {
                String::from("Ctrl+r")
            } else {
                String::from("\"")
            });
        }
        if self.input_state.awaiting_mark_set {
            return Some(String::from("m"));
        }
        if let Some(line_start) = self.input_state.awaiting_mark_jump {
            return Some(if line_start { String::from("'") } else { String::from("`") });
        }
        if self.input_state.awaiting_macro_record {
            return Some(String::from("record macro"));
        }
        if self.input_state.awaiting_macro_replay {
            return Some(String::from("replay macro"));
        }
        if self.input_state.awaiting_window_cmd {
            return Some(String::from("Ctrl+w"));
        }
        if self.input_state.awaiting_replace_char {
            return Some(String::from("replace"));
        }
        None
    }
    pub(crate) fn active_key_hint_entries(&self) -> Option<Vec<crate::keymap::KeyHintEntry>> {
        let entries = if let Some(node) = self.active_key_sequence_node() {
            Some(node.hint_entries())
        } else if let Some(prefix) = self.input_state.prefix {
            let entries = crate::keymap::prefix_hint_entries(&self.key_bindings, self.mode, prefix);
            if entries.is_empty() { None } else { Some(entries) }
        } else if self.input_state.awaiting_register {
            Some(crate::keymap::register_hint_entries())
        } else if self.input_state.awaiting_mark_set {
            Some(crate::keymap::mark_set_hint_entries())
        } else if let Some(line_start) = self.input_state.awaiting_mark_jump {
            Some(crate::keymap::mark_jump_hint_entries(line_start))
        } else if self.input_state.awaiting_macro_record {
            Some(crate::keymap::macro_record_hint_entries())
        } else if self.input_state.awaiting_macro_replay {
            Some(crate::keymap::macro_replay_hint_entries())
        } else if self.input_state.awaiting_window_cmd {
            Some(crate::keymap::window_command_hint_entries())
        } else if self.input_state.awaiting_replace_char {
            Some(crate::keymap::replace_char_hint_entries())
        } else {
            None
        }?;

        Some(crate::keymap::with_cancel_hint(entries))
    }
    pub(super) fn has_pending_input_state(&self) -> bool {
        !self.input_state.key_sequence.is_empty()
            || self.input_state.prefix.is_some()
            || self.input_state.pending_find.is_some()
            || self.input_state.awaiting_register
            || self.input_state.awaiting_register_insert
            || self.input_state.awaiting_mark_set
            || self.input_state.awaiting_mark_jump.is_some()
            || self.input_state.awaiting_macro_record
            || self.input_state.awaiting_macro_replay
            || self.input_state.awaiting_window_cmd
            || self.input_state.awaiting_replace_char
    }
    pub(super) fn cancel_pending_input_state(&mut self) {
        self.clear_active_key_sequence();
        self.input_state.reset();
        self.backend.status_message = Some(String::from("pending input cancelled"));
    }
    pub(crate) fn pending_input_label(&self) -> Option<String> {
        if self.input_state.awaiting_register {
            return Some(if self.input_state.awaiting_register_insert {
                String::from("insert register | press register name")
            } else {
                String::from("register | press register name")
            });
        }
        if self.input_state.awaiting_mark_set {
            return Some(String::from("set mark | press a-z"));
        }
        if let Some(line_start) = self.input_state.awaiting_mark_jump {
            return Some(if line_start {
                String::from("jump to mark line | press mark")
            } else {
                String::from("jump to mark | press mark")
            });
        }
        if self.input_state.awaiting_macro_record {
            return Some(String::from("record macro | press a-z"));
        }
        if self.input_state.awaiting_macro_replay {
            return Some(String::from("replay macro | press a-z or @"));
        }
        if self.input_state.awaiting_replace_char {
            return Some(String::from("replace | press character"));
        }
        if self.mode != Mode::OperatorPending
            && let Some(find) = self.input_state.pending_find
        {
            let direction = if find.forward { "forward" } else { "backward" };
            let kind = if find.inclusive { "find" } else { "till" };
            return Some(format!("{kind} char {direction} | press target"));
        }
        None
    }
    pub(crate) fn expire_key_sequence_if_idle(&mut self) {
        self.expire_key_sequence_if_idle_at(Instant::now());
    }
    pub(crate) fn expire_key_sequence_if_idle_at(&mut self, now: Instant) {
        let Some(last_input_at) = self.input_state.key_sequence_last_input_at else {
            return;
        };
        let timeout_ms = self.config.keymap.sequence_timeout_ms;
        if timeout_ms == 0 {
            return;
        }
        if now.duration_since(last_input_at) >= Duration::from_millis(timeout_ms) {
            self.clear_active_key_sequence();
        }
    }
    pub(super) fn clear_active_key_sequence(&mut self) {
        self.input_state.key_sequence.clear();
        self.input_state.key_sequence_last_input_at = None;
    }
    pub(super) fn can_start_key_sequence(&self) -> bool {
        matches!(
            self.mode,
            Mode::Normal | Mode::Insert | Mode::Visual | Mode::VisualLine | Mode::VisualBlock
        ) && self.input_state.pending_find.is_none()
            && self.input_state.pending_operator.is_none()
            && self.input_state.text_obj_inclusive.is_none()
            && self.input_state.prefix.is_none()
            && !self.input_state.awaiting_register
            && !self.input_state.awaiting_register_insert
            && !self.input_state.awaiting_mark_set
            && self.input_state.awaiting_mark_jump.is_none()
            && !self.input_state.awaiting_macro_record
            && !self.input_state.awaiting_macro_replay
            && !self.input_state.awaiting_window_cmd
            && !self.input_state.awaiting_replace_char
            && self.mode != Mode::OperatorPending
    }
    pub(super) fn handle_key_sequence(&mut self, key: KeyEvent) -> bool {
        let now = Instant::now();

        if !self.key_sequences.has_mode(self.mode) {
            self.clear_active_key_sequence();
            return false;
        }

        if key.code == KeyCode::Esc && !self.input_state.key_sequence.is_empty() {
            self.clear_active_key_sequence();
            self.backend.status_message = Some(String::from("key sequence cancelled"));
            return true;
        }

        if self.input_state.key_sequence.is_empty() && !self.can_start_key_sequence() {
            return false;
        }

        let key_press = crate::keymap::key_press_from_event(key);
        let attempted = if self.input_state.key_sequence.is_empty() {
            vec![key_press]
        } else {
            let mut sequence = self.input_state.key_sequence.clone();
            sequence.push(key_press);
            sequence
        };

        let Some((matched_sequence, has_children, action)) = self
            .key_sequences
            .advance(self.mode, &self.input_state.key_sequence, key_press)
            .map(|(matched_sequence, node)| {
                (matched_sequence, !node.children.is_empty(), node.action.clone())
            })
        else {
            if self.input_state.key_sequence.is_empty() {
                return false;
            }
            if self.mode == Mode::Insert
                && let Some(text) = self.literal_text_for_key_sequence(&attempted)
            {
                self.clear_active_key_sequence();
                self.insert_buffer.push_str(&text);
                let _ = self.backend.send_edit("insert", json!({ "chars": text }));
                self.backend.status_message = None;
                return true;
            }
            self.clear_active_key_sequence();
            self.backend.status_message =
                Some(format!("no binding: {}", crate::keymap::format_key_sequence(&attempted)));
            return true;
        };

        self.input_state.key_sequence = matched_sequence;
        self.input_state.key_sequence_last_input_at = Some(now);
        self.backend.status_message = None;

        if !has_children {
            let Some(action) = action else {
                self.clear_active_key_sequence();
                return true;
            };
            self.clear_active_key_sequence();
            self.dispatch(action, key);
        }

        true
    }
    pub(super) fn literal_text_for_key_sequence(
        &self,
        sequence: &[crate::keymap::KeyPress],
    ) -> Option<String> {
        let mut text = String::new();
        for key in sequence {
            if key.modifiers.contains(KeyModifiers::CONTROL)
                || key.modifiers.contains(KeyModifiers::ALT)
            {
                return None;
            }
            match key.key {
                KeyCode::Char(ch) => text.push(ch),
                KeyCode::Enter => text.push('\n'),
                _ => return None,
            }
        }
        Some(text)
    }
    pub(super) fn dispatch_context_key(&mut self, key: KeyEvent, mode: Mode) -> bool {
        let Some(action) = self.lookup_action(key, mode, None) else {
            return false;
        };
        self.dispatch(action, key);
        true
    }
    pub(super) fn handle_picker_text_input(&mut self, key: KeyEvent) {
        if let KeyCode::Char(c) = key.code
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
            && let Some(picker) = self.picker.as_mut()
        {
            picker.push_char(c);
        }
    }
}
