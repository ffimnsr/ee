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
        match ch {
            // Operators
            'd' | 'x' => {
                self.begin_record();
                if self.mode == Mode::VisualLine {
                    self.apply_visual_line_delete();
                } else if self.mode == Mode::VisualBlock {
                    self.apply_visual_block_op(Operator::Delete);
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
                } else if self.mode == Mode::VisualBlock {
                    self.apply_visual_block_op(Operator::Yank);
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
                    self.record_edit("delete_forward", json!([]));
                    self.enter_normal_mode();
                    self.mode = Mode::Insert;
                } else if self.mode == Mode::VisualBlock {
                    self.apply_visual_block_op(Operator::Change);
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
            '>' => {
                self.begin_record();
                self.record_edit("indent", json!([]));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.end_record();
                self.enter_normal_mode();
            }
            '<' => {
                self.begin_record();
                self.record_edit("outdent", json!([]));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.end_record();
                self.enter_normal_mode();
            }
            'U' => {
                self.begin_record();
                self.record_edit("uppercase", json!([]));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.end_record();
                self.enter_normal_mode();
            }
            'u' => {
                self.begin_record();
                self.record_edit("lowercase", json!([]));
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
