//! `impl App` methods: shell domain.
use super::*;

impl App {
    pub(super) fn shell_working_dir(&self) -> PathBuf {
        self.backend
            .active()
            .path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .filter(|path| path.is_dir())
            .or_else(|| self.working_dir.is_dir().then(|| self.working_dir.clone()))
            .unwrap_or_else(std::env::temp_dir)
    }
    pub(super) fn run_shell_command_on_selections(
        &mut self,
        command: &str,
        mode: ShellSelectionMode,
    ) -> Result<String, String> {
        let cwd = self.shell_working_dir();
        let selections = self.ensure_editable_selections()?;
        let buffer_text = self.current_buffer_text();

        let mut outputs = Vec::with_capacity(selections.len());
        let mut kept = Vec::new();
        for selection in &selections {
            let start = selection.start.min(selection.end);
            let end = selection.start.max(selection.end);
            let input = buffer_text
                .get(start..end)
                .ok_or_else(|| String::from("shell command encountered invalid selection range"))?;
            let result = crate::terminal::run_command_with_input(command, &cwd, Some(input))
                .map_err(|err| format!("shell failed: {err}"))?;
            if !result.success && !matches!(mode, ShellSelectionMode::KeepByStatus) {
                let stderr = result.stderr.trim();
                let detail = if stderr.is_empty() {
                    String::from("command exited with failure")
                } else {
                    stderr.to_owned()
                };
                return Err(format!("shell failed: {detail}"));
            }
            if matches!(mode, ShellSelectionMode::KeepByStatus) {
                if result.success {
                    kept.push(selection.clone());
                }
            } else {
                outputs.push(result.stdout);
            }
        }

        match mode {
            ShellSelectionMode::Replace => {
                for (selection, output) in selections.iter().zip(outputs.iter()).rev() {
                    self.replace_range_with_text(selection.clone(), output)?;
                }
                self.backend.sync_pending_events().map_err(|err| format!("pipe failed: {err}"))?;
                self.push_change();
                Ok(format!("pipe: {} selection(s)", selections.len()))
            }
            ShellSelectionMode::IgnoreOutput => {
                Ok(format!("pipe_to: {} selection(s)", selections.len()))
            }
            ShellSelectionMode::InsertBefore => {
                for (selection, output) in selections.iter().zip(outputs.iter()).rev() {
                    self.insert_at_offset(selection.start.min(selection.end), output)?;
                }
                self.backend
                    .sync_pending_events()
                    .map_err(|err| format!("shell_insert_output failed: {err}"))?;
                self.push_change();
                Ok(format!("shell_insert_output: {} selection(s)", selections.len()))
            }
            ShellSelectionMode::InsertAfter => {
                for (selection, output) in selections.iter().zip(outputs.iter()).rev() {
                    self.insert_at_offset(selection.start.max(selection.end), output)?;
                }
                self.backend
                    .sync_pending_events()
                    .map_err(|err| format!("shell_append_output failed: {err}"))?;
                self.push_change();
                Ok(format!("shell_append_output: {} selection(s)", selections.len()))
            }
            ShellSelectionMode::KeepByStatus => {
                if kept.is_empty() {
                    return Ok(String::from("no selections remaining"));
                }
                self.backend
                    .set_selections(&kept)
                    .map_err(|err| format!("shell_keep_pipe failed: {err}"))?;
                self.backend
                    .sync_pending_events()
                    .map_err(|err| format!("shell_keep_pipe failed: {err}"))?;
                Ok(format!("shell_keep_pipe: {} selection(s)", kept.len()))
            }
        }
    }
    pub(super) fn restore_git_hunk(&mut self) -> Result<String, String> {
        if self.block_active_vlf_source_control("git hunk reset") {
            return Err(Self::source_control_disabled_message("git hunk reset"));
        }

        let status = self
            .refresh_active_git_status()
            .ok_or_else(|| String::from("git: current buffer not in repository"))?;
        let hunk = status
            .hunk_at_line(self.backend.cursor_line)
            .cloned()
            .ok_or_else(|| String::from("git: cursor not inside changed hunk"))?;

        let line_count = self.backend.line_count();
        let start_offset = self.line_start_offset(hunk.new_start.min(line_count));
        let end_offset = if hunk.new_count == 0 {
            start_offset
        } else {
            let after_line = (hunk.new_start + hunk.new_count).min(line_count);
            if after_line < line_count {
                self.line_start_offset(after_line)
            } else {
                self.current_buffer_text().len()
            }
        };
        let old_lines = hunk
            .lines
            .iter()
            .filter(|line| matches!(line.kind, crate::git::DiffLineKind::Removed))
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();
        let replacement =
            format_git_hunk_replacement(&old_lines, hunk.new_start, hunk.new_count, line_count);

        self.replace_range_with_text(
            SelectionRange { start: start_offset, end: end_offset },
            &replacement,
        )?;
        self.push_change();
        Ok(format!("git hunk reset: line {}", hunk.display_line + 1))
    }
}
