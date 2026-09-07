//! `impl App` methods: marks domain.
use super::*;

impl App {
    /// Set mark `c` to the current cursor position.
    pub(super) fn set_mark(&mut self, c: char) {
        let pos = (self.backend.cursor_line, self.backend.cursor_col);
        self.marks.insert(c, pos);
    }
    /// Jump to mark `c`.  `line_start=true` moves to the first non-whitespace
    /// on the mark's line; `false` jumps to the exact saved byte column.
    pub(super) fn jump_to_mark(&mut self, c: char, line_start: bool) {
        // Special marks: `'` / `` ` `` jump to position before last jump.
        let pos = if c == '\'' || c == '`' {
            // Re-use the last jump list entry as the "previous position" mark.
            match self.jump_list.last().copied() {
                Some(p) => p,
                None => return,
            }
        } else {
            match self.marks.get(&c).copied() {
                Some(p) => p,
                None => return,
            }
        };

        self.push_jump();
        let (line, col) = pos;
        let col = if line_start {
            // Bounded: reads only the target line, not full buffer.
            self.backend
                .get_line(line)
                .map(|l| l.find(|ch: char| !ch.is_whitespace()).unwrap_or(0))
                .unwrap_or(0)
        } else {
            col
        };
        self.move_cursor_to(line, col);
    }
    pub(super) const JUMP_LIST_MAX: usize = 100;
    /// Push the current cursor position onto the jump list.
    /// Resets navigation to the head of the list.
    pub(crate) fn push_jump(&mut self) {
        let pos = (self.backend.cursor_line, self.backend.cursor_col);
        // Avoid duplicates at the head.
        if self.jump_list.last() == Some(&pos) {
            self.jump_list_idx = self.jump_list.len();
            return;
        }
        self.jump_list.push(pos);
        if self.jump_list.len() > Self::JUMP_LIST_MAX {
            self.jump_list.remove(0);
        }
        self.jump_list_idx = self.jump_list.len();
    }
    pub(super) fn jump_list_older(&mut self) {
        if self.jump_list.is_empty() {
            return;
        }
        // First Ctrl-O saves current position, then steps back.
        if self.jump_list_idx == self.jump_list.len() {
            let pos = (self.backend.cursor_line, self.backend.cursor_col);
            if self.jump_list.last() != Some(&pos) {
                self.jump_list.push(pos);
                if self.jump_list.len() > Self::JUMP_LIST_MAX {
                    self.jump_list.remove(0);
                }
                self.jump_list_idx = self.jump_list.len();
            }
        }
        if self.jump_list_idx == 0 {
            return;
        }
        self.jump_list_idx -= 1;
        let (line, col) = self.jump_list[self.jump_list_idx];
        self.move_cursor_to(line, col);
    }
    pub(super) fn jump_list_newer(&mut self) {
        if self.jump_list_idx + 1 >= self.jump_list.len() {
            return;
        }
        self.jump_list_idx += 1;
        let (line, col) = self.jump_list[self.jump_list_idx];
        self.move_cursor_to(line, col);
    }
    pub(super) const CHANGE_LIST_MAX: usize = 100;
    /// Push the current cursor position onto the change list.
    /// Called after any buffer-modifying operation.
    pub(crate) fn push_change(&mut self) {
        self.backend.note_buffer_modified(self.backend.active().id);
        let pos = (self.backend.cursor_line, self.backend.cursor_col);
        if self.change_list.last() == Some(&pos) {
            self.change_list_idx = self.change_list.len().saturating_sub(1);
            return;
        }
        self.change_list.push(pos);
        if self.change_list.len() > Self::CHANGE_LIST_MAX {
            self.change_list.remove(0);
        }
        self.change_list_idx = self.change_list.len().saturating_sub(1);
    }
    pub(super) fn change_list_older(&mut self) {
        if self.change_list.is_empty() {
            return;
        }
        let (line, col) = self.change_list[self.change_list_idx];
        self.move_cursor_to(line, col);
        self.change_list_idx = self.change_list_idx.saturating_sub(1);
    }
    pub(super) fn change_list_newer(&mut self) {
        if self.change_list.is_empty() {
            return;
        }
        let next = (self.change_list_idx + 1).min(self.change_list.len().saturating_sub(1));
        self.change_list_idx = next;
        let (line, col) = self.change_list[self.change_list_idx];
        self.move_cursor_to(line, col);
    }
    /// Move the xi cursor to the given (line, byte_col) via a gesture point_select.
    pub(super) fn move_cursor_to(&mut self, line: usize, col: usize) {
        let _ = self.backend.send_edit(
            "gesture",
            json!({ "line": line as u64, "col": col as u64, "ty": "point_select" }),
        );
    }
}
