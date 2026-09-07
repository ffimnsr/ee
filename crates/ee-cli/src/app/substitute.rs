//! `impl App` methods: substitute domain.
use super::*;

impl App {
    pub(super) fn apply_all_substitute_matches(&mut self) {
        while self.substitute_pending.as_ref().is_some_and(|s| s.current < s.matches.len()) {
            self.apply_substitute_current();
        }
    }
    pub(super) fn cancel_substitute_confirm(&mut self) {
        let applied = self.substitute_pending.as_ref().map(|s| s.applied).unwrap_or(0);
        self.substitute_pending = None;
        self.mode = Mode::Normal;
        self.backend.status_message = Some(format!("{applied} substitution(s) applied"));
    }
    /// Apply the current pending substitution match and advance.
    pub(super) fn apply_substitute_current(&mut self) {
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
    pub(super) fn advance_substitute_confirm(&mut self) {
        let Some(state) = self.substitute_pending.as_mut() else {
            return;
        };
        state.current += 1;
        self.advance_substitute_confirm_inner();
    }
    /// Finish confirmation if all matches exhausted, otherwise jump to next.
    pub(super) fn advance_substitute_confirm_inner(&mut self) {
        let done = self.substitute_pending.as_ref().is_none_or(|s| s.current >= s.matches.len());
        if done {
            let applied = self.substitute_pending.as_ref().map(|s| s.applied).unwrap_or(0);
            self.substitute_pending = None;
            self.mode = Mode::Normal;
            self.backend.status_message = Some(format!("{applied} substitution(s) applied"));
        } else {
            let (li, total, current) = if let Some(pending) = &self.substitute_pending {
                (pending.matches[pending.current].line, pending.matches.len(), pending.current)
            } else {
                return;
            };
            self.jump_to_line(li);
            self.backend.status_message =
                Some(format!("substitute ({}/{total}) replace? [y/n/a/q]", current + 1));
        }
    }
    pub(super) fn handle_privileged_save_confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => self.confirm_privileged_save(),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.cancel_privileged_save_confirm()
            }
            _ => {}
        }
    }
    pub(super) fn start_privileged_save_confirm(&mut self) -> Result<(), String> {
        let (draft_path, target_path, saved_rev_id) = self
            .backend
            .prepare_elevated_save_draft()
            .map_err(|err| format!("prepare elevated save failed: {err}"))?;
        let Some(tool) = crate::terminal::detect_elevation_tool() else {
            let _ = std::fs::remove_file(&draft_path);
            return Err(String::from(
                "save failed: permission denied and no `sudo`, `run0`, or `su` found in PATH",
            ));
        };
        let command = crate::terminal::build_elevated_copy_command(tool, &draft_path, &target_path);
        let helper_name = tool.binary_name();
        self.privileged_save_pending = Some(PrivilegedSavePending {
            draft_path,
            target_path: target_path.clone(),
            saved_rev_id,
            command: command.clone(),
            helper_name,
        });
        self.mode = Mode::PrivilegeConfirm;
        self.backend.status_message = Some(format!(
            "permission denied: retry save to {} with {helper_name}? [y/N] cmd: {command}",
            target_path.display()
        ));
        Ok(())
    }
    pub(super) fn cancel_privileged_save_confirm(&mut self) {
        if let Some(pending) = self.privileged_save_pending.take() {
            let _ = std::fs::remove_file(&pending.draft_path);
        }
        self.mode = Mode::Normal;
        self.backend.status_message = Some(String::from("privileged save cancelled"));
    }
    pub(super) fn confirm_privileged_save(&mut self) {
        let Some(pending) = self.privileged_save_pending.take() else {
            self.mode = Mode::Normal;
            return;
        };
        let cwd = self.shell_working_dir();
        let command = crate::terminal::TerminalCommand {
            title: format!("privileged save: {}", pending.target_path.display()),
            command: pending.command.clone(),
        };
        let result = crate::terminal::run_command(&command, &cwd);
        match result {
            Ok(result) if result.success => {
                if let Err(err) = self
                    .backend
                    .finalize_elevated_save(&pending.target_path, pending.saved_rev_id.clone())
                {
                    self.backend.status_message =
                        Some(format!("finalize elevated save failed: {err}"));
                } else {
                    self.backend.status_message = Some(format!(
                        "saved {} with {}",
                        pending.target_path.display(),
                        pending.helper_name
                    ));
                }
            }
            Ok(result) => {
                let title = command.title.clone();
                let body = crate::terminal::render_transcript(&result);
                self.open_generated_buffer(&title, &body);
                self.backend.status_message =
                    Some(format!("privileged save failed via {}", pending.helper_name));
            }
            Err(err) => {
                self.backend.status_message = Some(format!("privileged save failed: {err}"));
            }
        }
        let _ = std::fs::remove_file(&pending.draft_path);
        self.mode = Mode::Normal;
    }
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
