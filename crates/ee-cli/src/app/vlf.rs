//! `impl App` methods: vlf domain.
use super::*;

impl App {
    pub(super) fn handle_vlf_navigation(&mut self, method: &str, count: u64) -> bool {
        if !self.backend.is_vlf {
            return false;
        }

        let count = usize::try_from(count).unwrap_or(usize::MAX);
        let line_count = self.backend.line_count().max(1);
        let line = self.backend.cursor_line;
        let col = self.backend.cursor_col;
        let move_to_end = matches!(
            method,
            "move_to_end_of_document" | "move_to_end_of_document_and_modify_selection"
        );
        let target = match method {
            "move_up" | "move_up_and_modify_selection" => Some((line.saturating_sub(count), col)),
            "move_down" | "move_down_and_modify_selection" => {
                Some((line.saturating_add(count).min(line_count.saturating_sub(1)), col))
            }
            "move_left" | "move_left_and_modify_selection" => {
                Some((line, col.saturating_sub(count)))
            }
            "move_right" | "move_right_and_modify_selection" => {
                let max_col = self.backend.get_line(line).map(str::len).unwrap_or(col);
                Some((line, col.saturating_add(count).min(max_col)))
            }
            "move_to_left_end_of_line" => Some((line, 0)),
            "move_to_right_end_of_line" | "move_to_right_end_of_line_and_modify_selection" => {
                Some((line, self.backend.get_line(line).map(str::len).unwrap_or(0)))
            }
            "scroll_page_down" => Some((
                line.saturating_add((self.last_editor_height.max(1) / 2).max(1) * count)
                    .min(line_count.saturating_sub(1)),
                col,
            )),
            "scroll_page_up" => Some((
                line.saturating_sub((self.last_editor_height.max(1) / 2).max(1) * count),
                col,
            )),
            "move_to_beginning_of_document"
            | "move_to_beginning_of_document_and_modify_selection" => Some((0, 0)),
            "move_to_end_of_document" | "move_to_end_of_document_and_modify_selection" => {
                Some((line_count.saturating_sub(1), 0))
            }
            _ => None,
        };

        let Some((line, col)) = target else {
            self.backend.status_message = Some(format!("{method}: disabled in VLF"));
            return true;
        };

        if move_to_end && !self.backend.vlf_line_count_exact {
            let viewport_lines = self.last_editor_height.max(1);
            let _ = self.backend.request_vlf_tail_viewport(viewport_lines);
            self.backend.status_message = Some(String::from("VLF: jumping to file end"));
            return true;
        }

        if !move_to_end {
            self.backend.cancel_vlf_tail_jump();
        }

        self.backend.cursor_line = line.min(line_count.saturating_sub(1));
        let max_col = self.backend.get_line(self.backend.cursor_line).map(str::len).unwrap_or(0);
        self.backend.cursor_col = col.min(max_col);
        true
    }
    pub(super) fn block_active_vlf_source_control(&mut self, feature: &str) -> bool {
        let (is_vlf, buf_id) = {
            let buf = self.backend.active();
            (buf.is_vlf, buf.id)
        };
        if !is_vlf {
            return false;
        }

        self.source_control.remove(&buf_id);
        self.backend.status_message = Some(Self::source_control_disabled_message(feature));
        true
    }
    pub(super) fn block_vlf_editing(&mut self, feature: &str) -> bool {
        if !self.backend.is_vlf {
            return false;
        }

        self.backend.status_message =
            Some(format!("{feature} disabled in VLF: unsupported in sparse overlay mode"));
        true
    }
    pub(super) fn handle_vlf_direct_edit_action(&mut self, method: &str, count: usize) -> bool {
        if !self.backend.active().is_vlf {
            return false;
        }

        match method {
            "insert_newline" => {
                for _ in 0..count.max(1) {
                    let _ = self.send_vlf_replace_range_at_cursor("\n");
                }
                true
            }
            "delete_forward" => self.try_vlf_delete_forward(count.max(1)),
            "delete_backward" => self.try_vlf_delete_backward(count.max(1)),
            _ => false,
        }
    }
    pub(super) fn try_vlf_insert_text(&mut self, text: &str) -> bool {
        if !self.backend.active().is_vlf {
            return false;
        }
        let _ = self.send_vlf_replace_range_at_cursor(text);
        true
    }
    pub(super) fn apply_vlf_replace_range(
        &mut self,
        start_line: usize,
        start_col: usize,
        end_line: usize,
        end_col: usize,
        text: &str,
    ) -> io::Result<()> {
        self.backend.vlf_replace_range(start_line, start_col, end_line, end_col, text)?;
        let _ = self
            .backend
            .apply_local_vlf_replace_range(start_line, start_col, end_line, end_col, text);
        Ok(())
    }
    pub(super) fn send_vlf_replace_range_at_cursor(&mut self, text: &str) -> io::Result<()> {
        let line = self.backend.cursor_line;
        let col = self.backend.cursor_col;
        self.apply_vlf_replace_range(line, col, line, col, text)?;
        self.advance_vlf_cursor_after_insert(text);
        self.refresh_vlf_viewport();
        Ok(())
    }
    pub(super) fn try_vlf_delete_backward(&mut self, count: usize) -> bool {
        if !self.backend.active().is_vlf {
            return false;
        }
        for _ in 0..count {
            let line = self.backend.cursor_line;
            let col = self.backend.cursor_col;
            if col > 0 {
                let Some(line_text) = self.backend.get_line(line) else { break };
                let start_col = previous_char_boundary(line_text, col.saturating_sub(1));
                let _ = self.apply_vlf_replace_range(line, start_col, line, col, "");
                self.backend.cursor_line = line;
                self.backend.cursor_col = start_col;
                self.backend.clamp_cursor();
            } else if line > 0 {
                let prev_line = line - 1;
                let Some(prev_text) = self.backend.get_line(prev_line) else { break };
                let prev_len = prev_text.len();
                let _ = self.apply_vlf_replace_range(prev_line, prev_len, line, 0, "");
                self.backend.cursor_line = prev_line;
                self.backend.cursor_col = prev_len;
                self.backend.clamp_cursor();
            } else {
                break;
            }
        }
        self.refresh_vlf_viewport();
        true
    }
    pub(super) fn try_vlf_delete_forward(&mut self, count: usize) -> bool {
        if !self.backend.active().is_vlf {
            return false;
        }
        for _ in 0..count {
            let line = self.backend.cursor_line;
            let col = self.backend.cursor_col;
            let Some(line_text) = self.backend.get_line(line) else { break };
            if col < line_text.len() {
                let start = previous_char_boundary(line_text, col);
                let end = line_text[start..]
                    .chars()
                    .next()
                    .map(|ch| start + ch.len_utf8())
                    .unwrap_or(start);
                let _ = self.apply_vlf_replace_range(line, start, line, end, "");
            } else if line + 1 < self.backend.line_count() {
                let _ = self.apply_vlf_replace_range(line, col, line + 1, 0, "");
            } else {
                break;
            }
        }
        self.refresh_vlf_viewport();
        true
    }
    pub(super) fn advance_vlf_cursor_after_insert(&mut self, text: &str) {
        let mut line = self.backend.cursor_line;
        let mut col = self.backend.cursor_col;
        let parts: Vec<&str> = text.split('\n').collect();
        if parts.len() == 1 {
            col = col.saturating_add(text.len());
        } else {
            line = line.saturating_add(parts.len().saturating_sub(1));
            col = parts.last().map_or(0, |part| part.len());
        }
        self.backend.cursor_line = line;
        self.backend.cursor_col = col;
        self.backend.clamp_cursor();
    }
    pub(super) fn refresh_vlf_viewport(&mut self) {
        if !self.backend.active().is_vlf {
            return;
        }
        let top = self.viewport.top_line;
        let height = self.last_editor_height.max(1);
        let _ = self.backend.force_vlf_viewport_refresh(top, top.saturating_add(height));
    }
    pub(super) fn move_vlf_cursor_right(&mut self) {
        let line = self.backend.cursor_line;
        let col = self.backend.cursor_col;
        if let Some(text) = self.backend.get_line(line)
            && col < text.len()
        {
            let next = text[col..].chars().next().map(|ch| col + ch.len_utf8()).unwrap_or(col);
            self.backend.cursor_line = line;
            self.backend.cursor_col = next;
            self.backend.clamp_cursor();
        }
    }
    pub(super) fn move_vlf_cursor_to_line_end(&mut self) {
        let line = self.backend.cursor_line;
        let col = self.backend.get_line(line).map_or(self.backend.cursor_col, str::len);
        self.backend.cursor_line = line;
        self.backend.cursor_col = col;
        self.backend.clamp_cursor();
    }
}
