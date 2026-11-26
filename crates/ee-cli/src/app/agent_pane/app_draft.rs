//! `impl App`: composer draft editing and the floating prompt editor.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::super::*;

impl App {
    pub(super) fn agents_draft_backspace(&mut self) {
        if let Some(active) = self.agents.active_thread_index() {
            let thread = &mut self.agents.threads[active];
            thread.prompt_history_cursor = None;
            thread.prompt_history_restore_draft = None;
            thread.draft_backspace_at_cursor();
        } else {
            self.agents.pending_draft.pop();
        }
    }

    pub(super) fn agents_draft_delete_forward(&mut self) {
        if let Some(active) = self.agents.active_thread_index() {
            let thread = &mut self.agents.threads[active];
            thread.prompt_history_cursor = None;
            thread.prompt_history_restore_draft = None;
            thread.draft_delete_forward_at_cursor();
        }
    }

    pub(super) fn agents_draft_cursor_move(&mut self, delta: isize) {
        if let Some(active) = self.agents.active_thread_index() {
            let thread = &mut self.agents.threads[active];
            thread.prompt_history_cursor = None;
            thread.prompt_history_restore_draft = None;
            thread.draft_cursor_move(delta);
        }
    }

    pub(super) fn agents_draft_cursor_move_word(&mut self, delta: isize) {
        if let Some(active) = self.agents.active_thread_index() {
            let thread = &mut self.agents.threads[active];
            thread.prompt_history_cursor = None;
            thread.prompt_history_restore_draft = None;
            thread.draft_cursor_move_word(delta);
        }
    }

    pub(super) fn agents_draft_delete_word(&mut self) {
        if let Some(active) = self.agents.active_thread_index() {
            let thread = &mut self.agents.threads[active];
            thread.prompt_history_cursor = None;
            thread.prompt_history_restore_draft = None;
            thread.draft_delete_word_backward();
        }
    }

    pub(super) fn agents_draft_cursor_line_start(&mut self) {
        if let Some(active) = self.agents.active_thread_index() {
            let thread = &mut self.agents.threads[active];
            thread.prompt_history_cursor = None;
            thread.prompt_history_restore_draft = None;
            thread.draft_cursor_line_start();
        }
    }

    pub(super) fn agents_draft_cursor_line_end(&mut self) {
        if let Some(active) = self.agents.active_thread_index() {
            let thread = &mut self.agents.threads[active];
            thread.prompt_history_cursor = None;
            thread.prompt_history_restore_draft = None;
            thread.draft_cursor_line_end();
        }
    }

    /// Opens the floating prompt editor, snapshotting the draft so Esc can
    /// cancel. Triggered by the Insert key.
    pub(super) fn agents_open_prompt_editor(&mut self) {
        if let Some(active) = self.agents.active_thread_index() {
            let thread = &mut self.agents.threads[active];
            thread.prompt_editor_snapshot = Some(thread.draft.clone());
            self.backend.status_message =
                Some(String::from("prompt editor: Enter newline · Insert accept · Esc cancel"));
        }
    }

    /// Closes the floating prompt editor. `accept` keeps the edited draft;
    /// otherwise the pre-open snapshot is restored.
    pub(super) fn agents_close_prompt_editor(&mut self, accept: bool) {
        let Some(active) = self.agents.active_thread_index() else {
            return;
        };
        let thread = &mut self.agents.threads[active];
        let snapshot = thread.prompt_editor_snapshot.take();
        if let Some(restore) = snapshot.filter(|_| !accept) {
            thread.draft = restore;
        }
        thread.draft_cursor_to_end();
        self.backend.status_message = Some(if accept {
            String::from("prompt editor: accepted")
        } else {
            String::from("prompt editor: cancelled")
        });
    }

    /// Key handling while the floating prompt editor is open. Every key edits
    /// the live draft; pastes arrive through [`App::handle_paste`] and insert
    /// at the caret.
    pub(super) fn handle_prompt_editor_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.agents_close_prompt_editor(false);
            }
            // INSERT toggles: the same key that opened the editor accepts it.
            KeyCode::Insert => {
                self.agents_close_prompt_editor(true);
            }
            KeyCode::Enter => {
                self.agents_append_draft("\n");
            }
            KeyCode::Backspace
                if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.agents_draft_delete_word();
            }
            KeyCode::Backspace => self.agents_draft_backspace(),
            KeyCode::Delete => self.agents_draft_delete_forward(),
            KeyCode::Left if key.modifiers.contains(KeyModifiers::ALT) => {
                self.agents_draft_cursor_move_word(-1);
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::ALT) => {
                self.agents_draft_cursor_move_word(1);
            }
            KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.agents_draft_cursor_move_word(-1);
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.agents_draft_cursor_move_word(1);
            }
            KeyCode::Left => self.agents_draft_cursor_move(-1),
            KeyCode::Right => self.agents_draft_cursor_move(1),
            KeyCode::Home => self.agents_draft_cursor_line_start(),
            KeyCode::End => self.agents_draft_cursor_line_end(),
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.agents_draft_delete_word();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.agents_clear_draft();
            }
            KeyCode::Char(c)
                if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.agents_append_draft(&c.to_string());
            }
            _ => {}
        }
    }
}
