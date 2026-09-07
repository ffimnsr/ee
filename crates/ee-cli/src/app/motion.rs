//! `impl App` methods: motion domain.
use super::*;

impl App {
    pub(super) fn execute_search(&mut self) {
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
    pub(super) fn search_current_selection(&mut self, detect_word_boundaries: bool) {
        let Some(chars) = self.current_search_pattern() else {
            return;
        };
        let case_sensitive = smart_case_sensitive(&chars);
        let _ = self.backend.send_edit(
            "find",
            json!({
                "chars": chars,
                "case_sensitive": case_sensitive,
                "regex": false,
                "whole_words": detect_word_boundaries
            }),
        );
        let _ = self.backend.send_edit("highlight_find", json!({ "visible": true }));
        self.search_pattern = self.current_search_pattern();
    }
    pub(super) fn current_search_pattern(&mut self) -> Option<String> {
        if matches!(self.mode, Mode::Visual | Mode::VisualLine | Mode::VisualBlock) {
            let selected = self.selected_text_preview(false);
            if !selected.is_empty() {
                return Some(selected);
            }
        }

        let line = self.backend.get_line(self.backend.cursor_line)?;
        let col = self.backend.cursor_col;
        let ch = line.get(col..)?.chars().next()?;
        if !(ch.is_alphanumeric() || ch == '_') {
            return None;
        }

        let start = line[..col]
            .char_indices()
            .rev()
            .take_while(|(_, current)| current.is_alphanumeric() || *current == '_')
            .last()
            .map(|(idx, _)| idx)
            .unwrap_or(col);
        let end = col
            + line[col..]
                .char_indices()
                .take_while(|(_, current)| current.is_alphanumeric() || *current == '_')
                .last()
                .map(|(idx, current)| idx + current.len_utf8())
                .unwrap_or(0);
        Some(line[start..end].to_owned())
    }
    pub(super) fn current_file_target(&mut self) -> Option<String> {
        if matches!(self.mode, Mode::Visual | Mode::VisualLine | Mode::VisualBlock) {
            let selected = self.selected_text_preview(false);
            let trimmed = selected.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }

        let line = self.backend.get_line(self.backend.cursor_line)?;
        if line.is_empty() {
            return None;
        }

        let mut cursor = self.backend.cursor_col.min(line.len().saturating_sub(1));
        if line[cursor..].chars().next().is_none_or(char::is_whitespace) {
            cursor = line[..cursor]
                .char_indices()
                .rev()
                .find(|(_, ch)| !ch.is_whitespace())
                .map(|(idx, _)| idx)?;
        }

        let start = line[..cursor]
            .char_indices()
            .rev()
            .find(|(_, ch)| ch.is_whitespace())
            .map(|(idx, ch)| idx + ch.len_utf8())
            .unwrap_or(0);
        let end = line[cursor..]
            .char_indices()
            .find(|(_, ch)| ch.is_whitespace())
            .map(|(idx, _)| cursor + idx)
            .unwrap_or(line.len());

        let token = line[start..end]
            .trim_matches(|ch: char| {
                matches!(ch, '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | ';')
            })
            .trim_end_matches(['.', ':']);
        if token.is_empty() { None } else { Some(token.to_owned()) }
    }
    pub(super) fn resolve_file_target_path(&self, target: &str) -> Option<PathBuf> {
        let target = target.strip_prefix("file://").unwrap_or(target);
        if target.is_empty() {
            return None;
        }

        let path = PathBuf::from(target);
        if path.is_absolute() {
            return Some(path);
        }

        self.backend
            .active()
            .path
            .as_deref()
            .and_then(Path::parent)
            .map(|parent| parent.join(&path))
            .or_else(|| std::env::current_dir().ok().map(|cwd| cwd.join(path)))
    }
    pub(super) fn jump_to_char(&mut self, target: char, forward: bool, inclusive: bool) {
        if self.backend.find_char(target, forward, inclusive, self.mode.is_visual()).is_ok() {
            self.last_repeatable_motion =
                Some(RepeatableMotion::CharFind { target, forward, inclusive });
        }
    }
    pub(super) fn jump_matching_bracket(&mut self) {
        let _ = self.backend.move_to_matching_bracket(self.mode.is_visual());
    }
    pub(super) fn move_word_start(&mut self, forward: bool, long_word: bool) {
        let _ = self.backend.move_word_start(forward, long_word, self.mode.is_visual());
    }
    pub(super) fn move_word_end(&mut self, long_word: bool) {
        let _ = self.backend.move_word_end(long_word, self.mode.is_visual());
    }
    pub(super) fn goto_line_from_count(&mut self) {
        let target = self.input_state.count().saturating_sub(1) as usize;
        self.jump_to_line(target);
    }
    pub(super) fn goto_first_nonwhitespace(&mut self) {
        let line = self.backend.get_line(self.backend.cursor_line).unwrap_or_default();
        let target_byte = line
            .char_indices()
            .find_map(|(idx, ch)| (!ch.is_whitespace()).then_some(idx))
            .unwrap_or(0);
        self.goto_column(byte_col_to_display_col(line, target_byte));
    }
    pub(super) fn goto_column(&mut self, display_col: usize) {
        let _ = self.backend.goto_column(display_col, self.mode.is_visual());
    }
    pub(super) fn goto_column_from_count(&mut self) {
        let target = self.input_state.count().saturating_sub(1) as usize;
        self.goto_column(target);
    }
    pub(super) fn goto_file_start_from_count(&mut self) {
        if self.input_state.count_digits.is_empty() {
            self.jump_to_line(0);
        } else {
            self.goto_line_from_count();
        }
    }
    pub(super) fn goto_last_line(&mut self) {
        let last_line = self.backend.line_count().saturating_sub(1);
        self.jump_to_line(last_line);
    }
    pub(super) fn goto_file_under_cursor(&mut self) {
        let Some(target) = self.current_file_target() else {
            self.backend.status_message = Some("goto_file: no file under cursor".to_owned());
            return;
        };

        if target.starts_with("http://") || target.starts_with("https://") {
            self.backend.status_message =
                Some("goto_file: URL targets are not supported yet".to_owned());
            return;
        }

        let Some(path) = self.resolve_file_target_path(&target) else {
            self.backend.status_message = Some(format!("goto_file: invalid target `{target}`"));
            return;
        };

        let existing_id = self
            .backend
            .all_bufs()
            .iter()
            .find(|buf| buf.path.as_ref().is_some_and(|candidate| *candidate == path))
            .map(|buf| buf.id);

        let buf_id = if let Some(id) = existing_id {
            if let Err(err) = self.backend.switch_to_id(id) {
                self.backend.status_message = Some(format!("goto_file failed: {err}"));
                return;
            }
            id
        } else {
            match self.backend.open_buffer(Some(path.clone())) {
                Ok(id) => {
                    if let Err(err) = self.backend.switch_to_id(id) {
                        self.backend.status_message = Some(format!("goto_file failed: {err}"));
                        return;
                    }
                    id
                }
                Err(err) => {
                    self.backend.status_message = Some(format!("goto_file failed: {err}"));
                    return;
                }
            }
        };

        self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
        self.viewport = Viewport::default();
    }
    pub(super) fn page_cursor_half(&mut self, down: bool) {
        let editor_rows = crossterm::terminal::size()
            .map(|(_, height)| height.saturating_sub(2) as usize)
            .unwrap_or(22);
        let delta = (editor_rows / 2).max(1);
        let max_line = self.backend.line_count().saturating_sub(1);
        let next_line = if down {
            self.backend.cursor_line.saturating_add(delta).min(max_line)
        } else {
            self.backend.cursor_line.saturating_sub(delta)
        };
        let next_col = self
            .backend
            .line_len(next_line)
            .map(|len| self.backend.cursor_col.min(len))
            .unwrap_or(0);
        self.push_jump();
        let _ = self.backend.send_edit(
            "gesture",
            json!({ "line": next_line as u64, "col": next_col as u64, "ty": "point_select" }),
        );
        self.viewport.top_line = if down {
            self.viewport.top_line.saturating_add(delta).min(max_line)
        } else {
            self.viewport.top_line.saturating_sub(delta)
        };
    }
    pub(super) fn repeat_last_motion(&mut self) {
        let Some(motion) = self.last_repeatable_motion else {
            return;
        };

        match motion {
            RepeatableMotion::CharFind { target, forward, inclusive } => {
                self.jump_to_char(target, forward, inclusive);
            }
            RepeatableMotion::MatchingPair => self.jump_matching_bracket(),
            RepeatableMotion::Quickfix { forward, is_quickfix } => {
                if forward {
                    self.qf_next(is_quickfix);
                } else {
                    self.qf_prev(is_quickfix);
                }
            }
            RepeatableMotion::GitHunk { forward } => self.jump_to_git_hunk(forward),
        }
    }
}
