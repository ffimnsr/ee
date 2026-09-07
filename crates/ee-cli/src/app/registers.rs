//! `impl App` methods: registers domain.
use super::*;

impl App {
    /// Send a xi edit and, if recording is active, append it to the command log.
    pub(super) fn record_edit(&mut self, method: &'static str, params: serde_json::Value) {
        if self.recording {
            self.recorded_commands.push((method, params.clone()));
        }
        let _ = self.backend.send_edit(method, params);
    }
    pub(super) fn begin_record(&mut self) {
        self.recording = true;
        self.recorded_commands.clear();
    }
    pub(super) fn end_record(&mut self) {
        self.recording = false;
        let cmds = std::mem::take(&mut self.recorded_commands);
        self.last_change = Some(LastChange::Commands(cmds));
    }
    /// Start recording keystrokes into register `c`.
    pub(super) fn start_macro_record(&mut self, c: char) {
        self.macro_register = Some(c);
        self.macro_buffer.clear();
        self.backend.status_message = Some(format!("recording @{c}"));
    }
    /// Stop recording and store the accumulated keystrokes in the macros map.
    pub(super) fn stop_macro_record(&mut self) {
        let Some(c) = self.macro_register.take() else { return };
        // The key that triggered stop_macro_record ('q') was already pushed to
        // macro_buffer by handle_event; remove it.
        self.macro_buffer.pop();
        let keys = std::mem::take(&mut self.macro_buffer);
        self.macros.insert(c, keys);
        self.last_macro = Some(c);
        self.backend.status_message = None;
    }
    /// Replay the macro stored in register `c` (count times).
    pub(super) fn replay_macro(&mut self, c: char) {
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
    /// Consume the pending register, falling back to `Unnamed`.
    pub(super) fn take_register(&mut self) -> RegisterName {
        self.input_state.pending_register.take().unwrap_or(RegisterName::Unnamed)
    }
    pub(super) fn insert_register_for_current_mode(&mut self, reg: RegisterName) {
        let text = self.registers.get(&reg);
        if text.is_empty() {
            return;
        }

        match self.mode {
            Mode::Insert => {
                self.insert_buffer.push_str(&text);
                let _ = self.backend.send_edit("insert", json!({ "chars": text }));
            }
            Mode::CommandLine => {
                self.history_idx = None;
                self.command_buffer.push_str(&text);
            }
            Mode::Search => {
                self.command_buffer.push_str(&text);
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
            _ => {}
        }
    }
    pub(super) fn selected_text_preview(&mut self, linewise: bool) -> String {
        if self.mode == Mode::VisualBlock {
            let (al, ac) =
                self.visual_anchor.unwrap_or((self.backend.cursor_line, self.backend.cursor_col));
            let cl = self.backend.cursor_line;
            let cc = self.backend.cursor_col;
            let (top, bottom) = if al <= cl { (al, cl) } else { (cl, al) };
            let (left_col, right_col) = if ac <= cc { (ac, cc) } else { (cc, ac) };
            return self
                .backend
                .block_text_preview(top, bottom, left_col, right_col)
                .unwrap_or_default();
        }

        self.backend.selected_text_preview(linewise).unwrap_or_default()
    }
    /// Paste register content.  `before` = `P` (before cursor), else `p`.
    pub(super) fn paste(&mut self, before: bool) {
        let reg = self.take_register();
        let text = self.registers.get(&reg);
        if text.is_empty() {
            return;
        }
        self.record_edit("paste_register", json!({ "chars": text, "before": before }));
        self.push_change();
    }
    pub(super) fn paste_from_register(&mut self, reg: RegisterName, before: bool) {
        self.input_state.pending_register = Some(reg);
        self.paste(before);
    }
    pub(super) fn repeat_last_change(&mut self) {
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
}
