//! `impl App` methods: visual domain.
use super::*;

impl App {
    pub(super) fn enter_visual_line(&mut self) {
        if self.backend.is_vlf {
            self.visual_restore_cursor = Some((self.backend.cursor_line, self.backend.cursor_col));
        } else {
            self.visual_restore_cursor = None;
        }
        let anchor = (self.backend.cursor_line, 0);
        self.visual_anchor = Some(anchor);
        self.mode = Mode::VisualLine;
        if self.backend.is_vlf {
            self.backend.cursor_col = 0;
            return;
        }
        // Immediately select the whole current line.
        let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
        let _ = self.backend.send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
    }
    pub(super) fn enter_visual_block(&mut self) {
        let anchor = (self.backend.cursor_line, self.backend.cursor_col);
        self.visual_anchor = Some(anchor);
        self.mode = Mode::VisualBlock;
        // No xi selection yet; block region is defined by anchor + cursor.
    }
    /// Re-send xi line-wise selection from the anchor line to the cursor line.
    pub(super) fn sync_visual_line_selection(&mut self) {
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
        // Bounded: reads only the bottom line length, not full buffer.
        let bottom_len = self.backend.line_len(bottom).unwrap_or(0);
        let _ = self.backend.send_edit(
            "gesture",
            json!({
                "line": bottom as u64,
                "col": bottom_len as u64,
                "ty": { "select_extend": { "granularity": "point" } }
            }),
        );
    }
    pub(super) fn swap_visual_anchor(&mut self) {
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
    pub(super) fn restore_last_visual(&mut self) {
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
    pub(super) fn handle_visual_char(&mut self, ch: char) {
        // Counts: accumulate digits like normal mode so `3w` extends 3 words.
        if let Some(digit) =
            ch.to_digit(10).filter(|d| *d > 0 || !self.input_state.count_digits.is_empty())
        {
            self.input_state.count_digits.push(digit as u8);
            return;
        }
        if ch == '0' && self.input_state.count_digits.is_empty() {
            let _ =
                self.backend.send_edit("move_to_left_end_of_line_and_modify_selection", json!([]));
            return;
        }

        // Visual block: corner / EOL / replace / case ops are block-local.
        if self.mode == Mode::VisualBlock {
            self.handle_visual_block_char(ch);
            return;
        }

        match ch {
            // Operators
            'd' | 'x' => {
                self.begin_record();
                if self.mode == Mode::VisualLine {
                    self.apply_visual_line_delete();
                } else {
                    let reg = self.take_register();
                    let text = self.selected_text_preview(false);
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
                } else {
                    let reg = self.take_register();
                    let text = self.selected_text_preview(false);
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
                    let text = self.selected_text_preview(true);
                    self.registers.delete(&reg, text, false);
                    self.sync_linewise_selection_if_needed();
                    self.record_edit("delete_forward", json!([]));
                    self.enter_normal_mode();
                    self.mode = Mode::Insert;
                } else {
                    let reg = self.take_register();
                    let text = self.selected_text_preview(false);
                    self.registers.delete(&reg, text, false);
                    self.record_edit("delete_forward", json!([]));
                    self.enter_normal_mode();
                    self.mode = Mode::Insert;
                }
                self.end_record();
            }
            // vim visual `s` = change; `S` = change whole lines.
            's' => {
                self.handle_visual_char('c');
            }
            'S' => {
                self.begin_record();
                if self.mode == Mode::VisualLine {
                    self.sync_linewise_selection_if_needed();
                } else {
                    let _ = self.backend.send_edit("extend_to_line_bounds", json!([]));
                }
                let reg = self.take_register();
                let text = self.selected_text_preview(true);
                self.registers.delete(&reg, text, false);
                self.record_edit("delete_forward", json!([]));
                self.end_record();
                self.enter_normal_mode();
                self.mode = Mode::Insert;
            }
            // vim visual `D` = delete to end of line; `X` = to line start.
            'D' => {
                self.begin_record();
                let _ = self
                    .backend
                    .send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
                let reg = self.take_register();
                let text = self.selected_text_preview(false);
                self.registers.delete(&reg, text, false);
                self.record_edit("delete_forward", json!([]));
                self.end_record();
                self.enter_normal_mode();
            }
            'X' => {
                self.begin_record();
                let _ = self
                    .backend
                    .send_edit("move_to_left_end_of_line_and_modify_selection", json!([]));
                let reg = self.take_register();
                let text = self.selected_text_preview(false);
                self.registers.delete(&reg, text, false);
                self.record_edit("delete_forward", json!([]));
                self.end_record();
                self.enter_normal_mode();
            }
            // vim visual `J` = join the selected lines.
            'J' => {
                self.begin_record();
                if self.mode == Mode::VisualLine {
                    self.sync_linewise_selection_if_needed();
                } else {
                    let _ = self.backend.send_edit("extend_to_line_bounds", json!([]));
                }
                let _ = self.backend.send_edit("join_selections", json!({ "select_space": true }));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.end_record();
                self.push_change();
                self.enter_normal_mode();
            }
            // vim visual `~` = toggle case of the selection.
            '~' => {
                self.begin_record();
                self.sync_linewise_selection_if_needed();
                let text = self.selected_text_preview(false);
                let toggled: String = text
                    .chars()
                    .map(|c| {
                        if c.is_ascii_lowercase() {
                            c.to_ascii_uppercase()
                        } else if c.is_ascii_uppercase() {
                            c.to_ascii_lowercase()
                        } else {
                            c
                        }
                    })
                    .collect();
                if !toggled.is_empty() {
                    self.record_edit("delete_forward", json!([]));
                    let _ = self.backend.send_edit("insert", json!({ "chars": toggled }));
                }
                self.end_record();
                self.enter_normal_mode();
            }
            '>' => {
                self.begin_record();
                self.sync_linewise_selection_if_needed();
                self.record_edit("indent", json!([]));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.end_record();
                self.enter_normal_mode();
            }
            '<' => {
                self.begin_record();
                self.sync_linewise_selection_if_needed();
                self.record_edit("outdent", json!([]));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.end_record();
                self.enter_normal_mode();
            }
            'U' => {
                self.begin_record();
                self.sync_linewise_selection_if_needed();
                self.record_edit("uppercase", json!([]));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.end_record();
                self.enter_normal_mode();
            }
            'u' => {
                self.begin_record();
                self.sync_linewise_selection_if_needed();
                self.record_edit("lowercase", json!([]));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.end_record();
                self.enter_normal_mode();
            }
            '=' => {
                self.begin_record();
                self.sync_linewise_selection_if_needed();
                self.record_edit("reindent", json!([]));
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
    /// Handle a char in visual block mode: corners, EOL extension, replace and
    /// case-toggle are block-local (the backend only sees a caret here).
    fn handle_visual_block_char(&mut self, ch: char) {
        let (al, ac) =
            self.visual_anchor.unwrap_or((self.backend.cursor_line, self.backend.cursor_col));
        let (cl, cc) = (self.backend.cursor_line, self.backend.cursor_col);
        match ch {
            // vim block `o`: other corner on the same line (swap columns).
            'o' => {
                self.visual_anchor = Some((al, cc));
                self.move_cursor_to(cl, ac);
            }
            // vim block `O`: other corner on the same column (swap rows).
            'O' => {
                self.visual_anchor = Some((cl, ac));
                self.move_cursor_to(al, cc);
            }
            // vim block `$`: extend the right edge to the longest line in the block.
            '$' => {
                let (top, bottom) = if al <= cl { (al, cl) } else { (cl, al) };
                let max_len = (top..=bottom)
                    .filter_map(|line| self.backend.line_len(line))
                    .max()
                    .unwrap_or(cc);
                self.move_cursor_to(cl, max_len);
            }
            // vim block `~`: toggle ASCII case of every char in the block columns.
            '~' => self.block_toggle_case(),
            // vim block operators on the rectangle: `d`/`x` delete, `c` change,
            // `y` yank.
            'd' | 'x' => self.apply_visual_block_op(Operator::Delete),
            'c' => self.apply_visual_block_op(Operator::Change),
            'y' => self.apply_visual_block_op(Operator::Yank),
            // Backend single-char deletes/cuts also work per block via the
            // existing operators; ignore anything else here.
            _ => {}
        }
    }

    /// vim block `r<char>`: replace the block columns with the char (per line,
    /// clamped to existing text — no padding).
    pub(super) fn block_replace_char(&mut self, ch: char) {
        let (al, ac) =
            self.visual_anchor.unwrap_or((self.backend.cursor_line, self.backend.cursor_col));
        let (cl, cc) = (self.backend.cursor_line, self.backend.cursor_col);
        let (top, bottom) = if al <= cl { (al, cl) } else { (cl, al) };
        let (left, right) = if ac <= cc { (ac, cc) } else { (cc, ac) };
        if right <= left {
            self.enter_normal_mode();
            return;
        }
        let mut replacements = Vec::new();
        for line in top..=bottom {
            let Some(text) = self.backend.get_line(line).map(str::to_owned) else {
                continue;
            };
            let start = left.min(text.len());
            let end = right.min(text.len());
            if start < end {
                let mut chars: Vec<char> = text.chars().collect();
                for idx in byte_cols_to_char_indices(&text, start..end) {
                    chars[idx] = ch;
                }
                replacements.push(xi_core_lib::rpc::LineReplacement {
                    line,
                    text: chars.into_iter().collect(),
                });
            }
        }
        if !replacements.is_empty() {
            let _ = self.backend.apply_line_replacements(&replacements);
            self.push_change();
        }
        self.enter_normal_mode();
    }

    /// vim block `~`: toggle ASCII case of every char in the block columns.
    pub(super) fn block_toggle_case(&mut self) {
        let (al, ac) =
            self.visual_anchor.unwrap_or((self.backend.cursor_line, self.backend.cursor_col));
        let (cl, cc) = (self.backend.cursor_line, self.backend.cursor_col);
        let (top, bottom) = if al <= cl { (al, cl) } else { (cl, al) };
        let (left, right) = if ac <= cc { (ac, cc) } else { (cc, ac) };
        let mut replacements = Vec::new();
        for line in top..=bottom {
            let Some(text) = self.backend.get_line(line).map(str::to_owned) else {
                continue;
            };
            let start = left.min(text.len());
            let end = right.min(text.len());
            if start < end {
                let mut chars: Vec<char> = text.chars().collect();
                for idx in byte_cols_to_char_indices(&text, start..end) {
                    let c = chars[idx];
                    chars[idx] = if c.is_ascii_lowercase() {
                        c.to_ascii_uppercase()
                    } else if c.is_ascii_uppercase() {
                        c.to_ascii_lowercase()
                    } else {
                        c
                    };
                }
                replacements.push(xi_core_lib::rpc::LineReplacement {
                    line,
                    text: chars.into_iter().collect(),
                });
            }
        }
        if !replacements.is_empty() {
            let _ = self.backend.apply_line_replacements(&replacements);
            self.push_change();
        }
        self.enter_normal_mode();
    }

    /// Re-send the full-line selection when an operator runs in VisualLine
    /// mode, so partial columns never leak into linewise ops (c/S/J/~, > < U u =).
    fn sync_linewise_selection_if_needed(&mut self) {
        if self.mode == Mode::VisualLine {
            self.sync_visual_line_selection();
        }
    }
    pub(super) fn apply_visual_line_delete(&mut self) {
        let reg = self.take_register();
        let text = self.selected_text_preview(true);
        self.registers.delete(&reg, text, false);
        self.sync_visual_line_selection();
        self.record_edit("delete_forward", json!([]));
        self.enter_normal_mode();
    }
    pub(super) fn apply_visual_line_yank(&mut self) {
        let reg = self.take_register();
        let text = self.selected_text_preview(true);
        self.registers.yank(&reg, text, false);
        // Collapse without deleting.
        let _ = self.backend.send_edit("collapse_selections", json!([]));
        self.enter_normal_mode();
    }
    /// Apply an operator to each line in the block visual selection.
    pub(super) fn apply_visual_block_op(&mut self, op: Operator) {
        let (al, ac) =
            self.visual_anchor.unwrap_or((self.backend.cursor_line, self.backend.cursor_col));
        let cl = self.backend.cursor_line;
        let cc = self.backend.cursor_col;
        let (top, bottom) = if al <= cl { (al, cl) } else { (cl, al) };
        let (left_col, right_col) = if ac <= cc { (ac, cc) } else { (cc, ac) };
        let extracted =
            self.backend.block_text_preview(top, bottom, left_col, right_col).unwrap_or_default();
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
    /// Set up a block-insert (`I`) or block-append (`A`) for the current block
    /// visual selection.  Actual text is applied on leaving insert mode.
    pub(super) fn visual_block_insert(&mut self, append: bool) {
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
    pub(super) fn apply_block_insert(&mut self) {
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
}

/// Map a byte range to the indices of the chars intersecting it (used for
/// block-column replacements so multibyte chars are never split).
fn byte_cols_to_char_indices(text: &str, byte_range: std::ops::Range<usize>) -> Vec<usize> {
    text.char_indices()
        .enumerate()
        .filter(|(_, (byte, ch))| {
            let byte_end = byte + ch.len_utf8();
            byte_range.start < byte_end && byte_range.end > *byte
        })
        .map(|(idx, _)| idx)
        .collect()
}
