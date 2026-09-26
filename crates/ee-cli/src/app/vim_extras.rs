//! `impl App` methods: vim normal-mode extras.
//!
//! Frontend-owned motions and actions for vim keybindings that have no
//! dedicated backend primitive (sentence/paragraph jumps, line-first-nonblank
//! moves, viewport centering, case toggle, char deletes, join, replace mode,
//! info/status helpers). Line-cache based, with a bounded scan so VLF and
//! large buffers stay responsive.

use super::*;

/// Maximum lines scanned when searching for paragraph/sentence boundaries.
const BOUNDARY_SCAN_LIMIT: usize = 2_000;

impl App {
    /// `x`: delete `count` chars forward, yanking into the register.
    pub(super) fn delete_char_forward(&mut self) {
        let count = self.input_state.count().max(1) as usize;
        let text = self
            .backend
            .get_line(self.backend.cursor_line)
            .map(|line| {
                let from = self.backend.cursor_col.min(line.len());
                line[from..].chars().take(count).collect::<String>()
            })
            .unwrap_or_default();
        let reg = self.take_register();
        self.registers.delete(&reg, text, false);
        if self.backend.active().is_vlf {
            self.try_vlf_delete_forward(count);
        } else {
            for _ in 0..count {
                let _ = self.backend.send_edit("delete_forward", json!([]));
            }
        }
        self.push_change();
    }

    /// `X`: delete `count` chars before the cursor, yanking into the register.
    pub(super) fn delete_char_backward(&mut self) {
        let count = self.input_state.count().max(1) as usize;
        let text = self
            .backend
            .get_line(self.backend.cursor_line)
            .map(|line| {
                let end = self.backend.cursor_col.min(line.len());
                // Walk back over up to `count` chars, collecting their bytes.
                let mut chars = Vec::new();
                let mut col = end;
                for _ in 0..count {
                    if col == 0 {
                        break;
                    }
                    let mut start = col - 1;
                    while start > 0 && !line.is_char_boundary(start) {
                        start -= 1;
                    }
                    if line.is_char_boundary(start) {
                        chars.push(&line[start..col]);
                        col = start;
                    } else {
                        break;
                    }
                }
                chars.into_iter().rev().collect::<String>()
            })
            .unwrap_or_default();
        let reg = self.take_register();
        self.registers.delete(&reg, text, false);
        if self.backend.active().is_vlf {
            self.try_vlf_delete_backward(count);
        } else {
            for _ in 0..count {
                let _ = self.backend.send_edit("delete_backward", json!([]));
            }
        }
        self.push_change();
    }

    /// `~`: toggle ASCII case of `count` chars under the cursor.
    pub(super) fn toggle_case_chars(&mut self) {
        let count = self.input_state.count().max(1) as usize;
        let Some(line) = self.backend.get_line(self.backend.cursor_line) else {
            return;
        };
        let from = self.backend.cursor_col.min(line.len());
        let toggled: String = line[from..]
            .chars()
            .take(count)
            .map(|ch| {
                if ch.is_ascii_lowercase() {
                    ch.to_ascii_uppercase()
                } else if ch.is_ascii_uppercase() {
                    ch.to_ascii_lowercase()
                } else {
                    ch
                }
            })
            .collect();
        if toggled.is_empty() {
            return;
        }
        if !self.select_chars_from_cursor(count) {
            return;
        }
        let _ = self.backend.send_edit("delete_forward", json!([]));
        let _ = self.backend.send_edit("insert", json!({ "chars": toggled }));
        self.push_change();
    }

    /// `Replace` mode: overwrite the character under the cursor.
    pub(super) fn replace_mode_insert(&mut self, ch: char) {
        if self.backend.active().is_vlf {
            self.try_vlf_delete_forward(1);
            self.try_vlf_insert_text(&ch.to_string());
            return;
        }
        if !self.select_chars_from_cursor(1) {
            // No char under cursor (end of line): fall back to plain insert.
            let _ = self.backend.send_edit("insert", json!({ "chars": ch.to_string() }));
            return;
        }
        let _ = self.backend.send_edit("delete_forward", json!([]));
        let _ = self.backend.send_edit("insert", json!({ "chars": ch.to_string() }));
        self.push_change();
    }

    /// `D`: delete to end of line (yanking into the register).
    pub(super) fn delete_to_line_end(&mut self) {
        let _ = self.backend.send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
        self.apply_operator(Operator::Delete);
    }

    /// `C`: change to end of line, then enter insert.
    pub(super) fn change_to_line_end(&mut self) {
        let _ = self.backend.send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
        self.apply_operator(Operator::Change);
    }

    /// `Y`: yank `count` whole lines (vim `yy`).
    pub(super) fn yank_lines(&mut self) {
        let count = self.input_state.count().max(1) as usize;
        for _ in 0..count {
            self.apply_operator_to_line(Operator::Yank);
        }
    }

    /// `J`/`gJ`: join `count` lines below the cursor. `select_space` mirrors
    /// the backend `join_selections` parameter.
    pub(super) fn join_lines_with_selection(&mut self, select_space: bool) {
        let count = self.input_state.count().max(1) as usize;
        // The backend joins the current line with the next for a caret region;
        // extend the selection first only when more than two lines are joined.
        if count > 1 {
            let start_line = self.backend.cursor_line;
            let end_line = (start_line + count.saturating_sub(1))
                .min(self.backend.line_count().saturating_sub(1));
            let end_col = self.backend.line_len(end_line).unwrap_or(0);
            let _ = self.backend.send_edit(
                "gesture",
                json!({
                    "line": start_line as u64,
                    "col": self.backend.cursor_col as u64,
                    "ty": { "select": { "granularity": "point", "multi": false } },
                }),
            );
            let _ = self.backend.send_edit(
                "gesture",
                json!({
                    "line": end_line as u64,
                    "col": end_col as u64,
                    "ty": { "select_extend": { "granularity": "point" } },
                }),
            );
        }
        let _ = self.backend.send_edit("join_selections", json!({ "select_space": select_space }));
        let _ = self.backend.send_edit("collapse_selections", json!([]));
        self.push_change();
    }

    /// `{`/`}`: jump to paragraph boundary (frontend scan so visual mode can
    /// extend selection via gestures).
    pub(super) fn goto_paragraph_boundary(&mut self, forward: bool) {
        let count = self.input_state.count().max(1) as usize;
        let mut line = self.backend.cursor_line;
        for _ in 0..count {
            match self.scan_paragraph_boundary(line, forward) {
                Some(next) => line = next,
                None => break,
            }
        }
        self.jump_motion_to(line, 0);
    }

    /// `(`/`)`: jump to sentence boundary (same strategy as paragraphs).
    pub(super) fn goto_sentence_boundary(&mut self, forward: bool) {
        let count = self.input_state.count().max(1) as usize;
        let mut line = self.backend.cursor_line;
        let mut col = self.backend.cursor_col;
        for _ in 0..count {
            match self.scan_sentence_boundary(line, col, forward) {
                Some((next_line, next_col)) => {
                    line = next_line;
                    col = next_col;
                }
                None => break,
            }
        }
        self.jump_motion_to(line, col);
    }

    /// `+`/`Enter`/`-`/`_`: line offset move landing on first non-blank.
    pub(super) fn goto_line_first_nonblank(&mut self, down: bool, zero_based: bool) {
        let count = self.input_state.count().max(1) as usize;
        let offset = if zero_based { count.saturating_sub(1) } else { count };
        let line_count = self.backend.line_count().max(1);
        let target = if down {
            (self.backend.cursor_line + offset).min(line_count.saturating_sub(1))
        } else {
            self.backend.cursor_line.saturating_sub(offset)
        };
        let col = self
            .backend
            .get_line(target)
            .and_then(|line| line.find(|ch: char| !ch.is_whitespace()))
            .unwrap_or(0);
        self.jump_motion_to(target, col);
    }

    /// `g_`: last non-blank character of line `count - 1` lines down.
    pub(super) fn goto_line_last_nonblank(&mut self) {
        let count = self.input_state.count().max(1) as usize;
        let target = (self.backend.cursor_line + count.saturating_sub(1))
            .min(self.backend.line_count().saturating_sub(1));
        let col = self
            .backend
            .get_line(target)
            .map(|line| {
                line.char_indices()
                    .rev()
                    .find(|(_, ch)| !ch.is_whitespace())
                    .map(|(idx, _)| idx)
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        self.jump_motion_to(target, col);
    }

    /// `zz`/`zt`/`zb`: reposition the viewport around the cursor.
    pub(super) fn view_center_cursor(&mut self) {
        let rows = self.viewport_rows();
        self.set_viewport_top_line(self.backend.cursor_line.saturating_sub(rows / 2));
    }

    pub(super) fn view_top_cursor(&mut self) {
        self.set_viewport_top_line(self.backend.cursor_line);
    }

    pub(super) fn view_bottom_cursor(&mut self) {
        let rows = self.viewport_rows();
        self.set_viewport_top_line(self.backend.cursor_line.saturating_sub(rows.saturating_sub(1)));
    }

    /// `Ctrl-g`: file status in the status line.
    pub(super) fn show_file_status(&mut self) {
        let buf = self.backend.active();
        let name = buf
            .path
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| String::from("[No Name]"));
        let lines = self.backend.line_count();
        let modified = if buf.pristine { "" } else { " [Modified]" };
        self.backend.status_message = Some(format!("{name} — {lines} lines{modified}"));
    }

    /// `g8`: show UTF-8 bytes of the char under the cursor.
    pub(super) fn hex_dump_char(&mut self) {
        let Some(line) = self.backend.get_line(self.backend.cursor_line) else {
            return;
        };
        let col = self.backend.cursor_col.min(line.len());
        let Some(ch) = line.get(col..).and_then(|s| s.chars().next()) else {
            self.backend.status_message = Some(String::from("g8: no character under cursor"));
            return;
        };
        let bytes: Vec<String> = ch.to_string().bytes().map(|b| format!("{b:02x}")).collect();
        self.backend.status_message =
            Some(format!("g8: U+{:04X} — bytes [{}]", ch as u32, bytes.join(" ")));
    }

    /// `gx`: open the target under the cursor with the system opener.
    pub(super) fn open_target_under_cursor(&mut self) {
        let Some(target) = self.current_file_target() else {
            self.backend.status_message = Some(String::from("gx: no target under cursor"));
            return;
        };
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        let result = crate::terminal::run_command(
            &crate::terminal::TerminalCommand {
                title: format!("open: {target}"),
                command: format!("{opener} {target}"),
            },
            &self.shell_working_dir(),
        );
        match result {
            Ok(result) if result.success => {
                self.backend.status_message = Some(format!("opened {target}"));
            }
            Ok(result) => {
                self.backend.status_message =
                    Some(format!("gx: opener failed: {}", result.stdout.trim()));
            }
            Err(err) => {
                self.backend.status_message = Some(format!("gx: {err}"));
            }
        }
    }

    /// `gi`: insert at the last change position (change-list head).
    pub(super) fn insert_at_last_edit(&mut self) {
        if let Some(&(line, col)) = self.change_list.last() {
            self.move_cursor_to(line, col);
        }
        self.mode = Mode::Insert;
    }

    /// `gI`: insert at column zero of the current line.
    pub(super) fn insert_at_column_zero(&mut self) {
        self.move_cursor_to(self.backend.cursor_line, 0);
        self.mode = Mode::Insert;
    }

    /// `&`: re-run the last substitute command from the command history.
    pub(super) fn repeat_substitute(&mut self) {
        let last = self
            .command_history
            .iter()
            .rev()
            .find(|entry| entry.starts_with("s/") || entry.starts_with("substitute/"));
        let Some(command) = last.cloned() else {
            self.backend.status_message = Some(String::from("&: no previous substitute"));
            return;
        };
        self.command_buffer = command;
        self.execute_command();
    }

    /// `Ctrl-^`: jump to the alternate buffer.
    pub(super) fn alternate_buffer(&mut self) {
        match self.backend.switch_alternate() {
            Ok(()) => {
                self.viewport = Viewport::default();
            }
            Err(err) => {
                self.backend.status_message = Some(format!("no alternate buffer: {err}"));
            }
        }
    }

    /// Repeat the last char-find motion; `reversed` flips direction (`,` in vim).
    pub(super) fn repeat_last_motion_with_direction(&mut self, reversed: bool) {
        let Some(motion) = self.last_repeatable_motion else {
            return;
        };
        let motion = match motion {
            RepeatableMotion::CharFind { target, forward, inclusive } => {
                RepeatableMotion::CharFind {
                    target,
                    forward: if reversed { !forward } else { forward },
                    inclusive,
                }
            }
            other => other,
        };
        self.last_repeatable_motion = Some(motion);
        self.repeat_last_motion();
    }

    /// `Ctrl-e`/`Ctrl-y`: scroll the view one line without moving the cursor.
    pub(super) fn scroll_lines(&mut self, down: bool) {
        // A viewport-only scroll is the user looking elsewhere: it must not be
        // overridden by a pending goto-end sentinel.
        self.backend.cancel_vlf_tail_jump();
        let height = self.last_editor_height.max(3).saturating_sub(2) as i64;
        let top = self.viewport.top_line as i64;
        let first = if down { top + 1 } else { (top - 1).max(0) };
        let _ = self.backend.send_edit("scroll", json!({ "first": first, "last": first + height }));
    }

    /// `g*`/`g#`: search word under cursor without word boundaries, then jump.
    pub(super) fn search_word_under_cursor_loose(&mut self, forward: bool) {
        self.search_current_selection(false);
        self.push_jump();
        let _ = if forward {
            self.backend.send_edit("find_next", json!({ "wrap_around": true, "allow_same": false }))
        } else {
            self.backend
                .send_edit("find_previous", json!({ "wrap_around": true, "allow_same": false }))
        };
        if matches!(self.mode, Mode::Visual | Mode::VisualLine | Mode::VisualBlock) {
            self.enter_normal_mode();
        }
    }

    // ── Shared helpers ──

    fn viewport_rows(&self) -> usize {
        crossterm::terminal::size()
            .map(|(_, height)| height.saturating_sub(2) as usize)
            .unwrap_or(22)
            .max(1)
    }

    fn set_viewport_top_line(&mut self, target: usize) {
        // Repositioning the window (zz/zt/zb) abandons a pending goto-end jump:
        // the user chose a different part of the document. Cursor-moving
        // commands (H/M/L, marks) are covered by the manager's own abandonment
        // check once their gesture reaches the core.
        self.backend.cancel_vlf_tail_jump();
        let max_line = self.backend.line_count().saturating_sub(1);
        self.viewport.top_line = target.min(max_line);
    }

    /// Move to `(line, col)`, extending the selection in visual modes.
    fn jump_motion_to(&mut self, line: usize, col: usize) {
        if self.mode.is_visual() {
            let _ = self.backend.send_edit(
                "gesture",
                json!({
                    "line": line as u64,
                    "col": col as u64,
                    "ty": { "select_extend": { "granularity": "point" } },
                }),
            );
        } else {
            self.move_cursor_to(line, col);
        }
    }

    /// Scan for the next/previous paragraph boundary line (blank or
    /// whitespace-only lines delimit paragraphs). Returns `None` at buffer ends.
    pub(crate) fn scan_paragraph_boundary(&self, from_line: usize, forward: bool) -> Option<usize> {
        let line_count = self.backend.line_count().max(1);
        let is_blank =
            |line: usize| self.backend.get_line(line).map(|l| l.trim().is_empty()).unwrap_or(true);
        let mut line = from_line;
        for _ in 0..BOUNDARY_SCAN_LIMIT {
            let next = if forward { line + 1 } else { line.saturating_sub(1) };
            if forward && next >= line_count {
                return None;
            }
            if !forward && next == line {
                return None;
            }
            if is_blank(next) {
                // Skip the blank run, then land on the first non-blank line.
                if forward {
                    let mut target = next;
                    while target + 1 < line_count && is_blank(target + 1) {
                        target += 1;
                    }
                    if target + 1 < line_count {
                        return Some(target + 1);
                    }
                    return None;
                }
                // Backward: land on the first non-blank line below the blank
                // run found above the cursor.
                return (next + 1 < line_count).then_some(next + 1);
            }
            line = next;
        }
        None
    }

    /// Scan for the next/previous sentence start (after `.`/`!`/`?` followed
    /// by whitespace or end of line). Best effort on the cache.
    pub(crate) fn scan_sentence_boundary(
        &self,
        from_line: usize,
        from_col: usize,
        forward: bool,
    ) -> Option<(usize, usize)> {
        let line_count = self.backend.line_count().max(1);
        let mut line = from_line;
        let mut col = from_col;
        for _ in 0..BOUNDARY_SCAN_LIMIT {
            let text = self.backend.get_line(line)?;
            if forward {
                let rest = &text[col.min(text.len())..];
                let mut next_col = None;
                for (idx, ch) in rest.char_indices() {
                    if matches!(ch, '.' | '!' | '?') {
                        // Next sentence starts after trailing whitespace.
                        let mut candidate = col + idx + ch.len_utf8();
                        while candidate < text.len()
                            && matches!(text.as_bytes()[candidate], b' ' | b'\t')
                        {
                            candidate += 1;
                        }
                        if candidate >= text.len() {
                            // Sentence ended at line end: continue on the next line.
                            if line + 1 >= line_count {
                                return None;
                            }
                            return Some((line + 1, 0));
                        }
                        next_col = Some(candidate);
                        break;
                    }
                }
                if let Some(next_col) = next_col {
                    return Some((line, next_col));
                }
                line += 1;
                if line >= line_count {
                    return None;
                }
                col = 0;
            } else {
                let scan_end = col.min(text.len());
                let mut end_idx = None;
                for (idx, ch) in text[..scan_end].char_indices() {
                    if matches!(ch, '.' | '!' | '?') {
                        end_idx = Some(idx + ch.len_utf8());
                    }
                }
                if let Some(end_idx) = end_idx {
                    let mut start = end_idx;
                    while start < scan_end && matches!(text.as_bytes()[start], b' ' | b'\t') {
                        start += 1;
                    }
                    // A sentence ending exactly at the line end starts the next
                    // sentence on the following line.
                    if start >= text.len() && line + 1 < line_count {
                        return Some((line + 1, 0));
                    }
                    return Some((line, start.min(scan_end)));
                }
                if line == 0 {
                    return None;
                }
                line -= 1;
                col = self.backend.line_len(line).unwrap_or(0);
            }
        }
        None
    }
}
