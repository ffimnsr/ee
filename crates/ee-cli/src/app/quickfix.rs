//! `impl App` methods: quickfix domain.
use super::*;

impl App {
    /// Jump the cursor to `line` (0-based), clamped to the buffer length.
    pub(super) fn jump_to_line(&mut self, line: usize) {
        let clamped = line.min(self.backend.line_count().saturating_sub(1));
        self.push_jump();
        if self.backend.is_vlf {
            self.backend.cancel_vlf_tail_jump();
            self.backend.cursor_line = clamped;
            self.backend.cursor_col = 0;
            return;
        }
        let _ = self.backend.send_edit(
            "gesture",
            json!({ "line": clamped as u64, "col": 0u64, "ty": "point_select" }),
        );
    }
    pub(super) fn move_focused_list(&mut self, is_quickfix: bool, forward: bool) {
        if is_quickfix {
            if let Some(qf) = self.quickfix.as_mut() {
                if forward {
                    qf.move_down();
                } else {
                    qf.move_up();
                }
            }
        } else if let Some(ll) = self.location_list.as_mut() {
            if forward {
                ll.move_down();
            } else {
                ll.move_up();
            }
        }
    }
    pub(super) fn confirm_focused_list(&mut self, is_quickfix: bool) {
        let entry = if is_quickfix {
            self.quickfix.as_ref().and_then(|q| q.current()).cloned()
        } else {
            self.location_list.as_ref().and_then(|l| l.current()).cloned()
        };
        if let Some(entry) = entry {
            self.navigate_to_qf_entry(entry);
        }
        if is_quickfix {
            self.quickfix_focused = false;
        } else {
            self.location_list_focused = false;
        }
    }
    /// Navigate the quickfix list (is_quickfix=true) or location list forward.
    pub(super) fn qf_next(&mut self, is_quickfix: bool) {
        let entry = if is_quickfix {
            self.quickfix.as_mut().and_then(|q| q.next_entry()).cloned()
        } else {
            self.location_list.as_mut().and_then(|l| l.next_entry()).cloned()
        };
        if let Some(e) = entry {
            self.last_repeatable_motion =
                Some(RepeatableMotion::Quickfix { forward: true, is_quickfix });
            self.navigate_to_qf_entry(e);
        }
    }
    /// Navigate the quickfix list (is_quickfix=true) or location list backward.
    pub(super) fn qf_prev(&mut self, is_quickfix: bool) {
        let entry = if is_quickfix {
            self.quickfix.as_mut().and_then(|q| q.prev_entry()).cloned()
        } else {
            self.location_list.as_mut().and_then(|l| l.prev_entry()).cloned()
        };
        if let Some(e) = entry {
            self.last_repeatable_motion =
                Some(RepeatableMotion::Quickfix { forward: false, is_quickfix });
            self.navigate_to_qf_entry(e);
        }
    }
    /// Open the file for `entry` and jump to the recorded line/column.
    pub(super) fn navigate_to_qf_entry(&mut self, entry: QfEntry) {
        let Some(path) = entry.path.clone() else {
            self.backend.status_message = Some(format!("quickfix: line {}", entry.line + 1));
            return;
        };
        // Reuse an already-open buffer if possible.
        let existing_id = self
            .backend
            .all_bufs()
            .iter()
            .find(|b| b.path.as_ref().is_some_and(|p| *p == path))
            .map(|b| b.id);
        let buf_id = if let Some(id) = existing_id {
            let _ = self.backend.switch_to_id(id);
            id
        } else {
            match self.backend.open_buffer(Some(path)) {
                Ok(id) => {
                    let _ = self.backend.switch_to_id(id);
                    id
                }
                Err(err) => {
                    self.backend.status_message = Some(format!("quickfix: {err}"));
                    return;
                }
            }
        };
        self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
        self.viewport = Viewport::default();
        self.jump_to_line(entry.line);
    }
}
