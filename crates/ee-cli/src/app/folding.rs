//! `impl App` methods: folding domain.
use super::*;

impl App {
    pub(super) fn fold_extent_at_cursor(&mut self) -> Option<(usize, usize)> {
        match self.backend.fold_range_at_cursor() {
            Ok(extent) => extent,
            Err(err) => {
                self.backend.status_message = Some(format!("fold query failed: {err}"));
                None
            }
        }
    }
    pub(crate) fn fold_toggle(&mut self) {
        let line = self.backend.cursor_line;
        let extent = self.fold_extent_at_cursor().unwrap_or((line, line));
        let buf_id = self.backend.active().id;
        self.folds.toggle(buf_id, line, extent);
    }
    pub(crate) fn fold_open(&mut self) {
        let line = self.backend.cursor_line;
        let buf_id = self.backend.active().id;
        self.folds.open(buf_id, line);
    }
    pub(crate) fn fold_close(&mut self) {
        let line = self.backend.cursor_line;
        let buf_id = self.backend.active().id;
        if let Some(extent) = self.fold_extent_at_cursor() {
            self.folds.close(buf_id, line, extent);
        }
    }
    pub(crate) fn fold_open_all(&mut self) {
        let buf_id = self.backend.active().id;
        self.folds.open_all(buf_id);
    }
    pub(crate) fn fold_close_all(&mut self) {
        let buf_id = self.backend.active().id;
        match self.backend.fold_ranges_preview(None, None) {
            Ok(ranges) => self.folds.replace_all(buf_id, ranges),
            Err(err) => {
                self.backend.status_message = Some(format!("fold query failed: {err}"));
            }
        }
    }
}
