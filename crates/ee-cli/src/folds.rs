//! Manual fold state management for the TUI editor.
//!
//! Folds are stored per-buffer as sorted `(start, end)` line-index pairs
//! (both 0-based, both inclusive).  When a fold is closed, lines
//! `start+1..=end` are hidden from view and `start` shows a fold marker.

use std::collections::HashMap;

use crate::buffer::BufferId;

// ── FoldStore ─────────────────────────────────────────────────────────────────

/// Stores closed fold ranges keyed by buffer ID.
#[derive(Debug, Default)]
pub(crate) struct FoldStore {
    folds: HashMap<BufferId, Vec<(usize, usize)>>,
}

impl FoldStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Toggle the fold at `line` in `buf_id`.
    ///
    /// If a closed fold starts at `line`, open it.  Otherwise close `extent`.
    /// Does nothing when `extent.1 <= extent.0` (nothing to fold).
    pub(crate) fn toggle(&mut self, buf_id: BufferId, line: usize, extent: (usize, usize)) {
        let folds = self.folds.entry(buf_id).or_default();
        if let Some(pos) = folds.iter().position(|&(start, end)| line >= start && line <= end) {
            folds.remove(pos);
        } else if extent.1 > extent.0 {
            folds.push(extent);
            folds.sort_unstable_by_key(|&(s, _)| s);
        }
    }

    /// Close the fold at `line`.  No-op when already closed or extent trivial.
    pub(crate) fn close(&mut self, buf_id: BufferId, line: usize, extent: (usize, usize)) {
        if extent.1 <= extent.0 {
            return;
        }
        let folds = self.folds.entry(buf_id).or_default();
        if !folds.iter().any(|&(s, _)| s == line) {
            folds.push(extent);
            folds.sort_unstable_by_key(|&(s, _)| s);
        }
    }

    /// Open the closed fold containing `line`, including its header.
    pub(crate) fn open(&mut self, buf_id: BufferId, line: usize) {
        if let Some(folds) = self.folds.get_mut(&buf_id)
            && let Some(pos) = folds.iter().position(|&(start, end)| line >= start && line <= end)
        {
            folds.remove(pos);
        }
    }

    /// Remove all closed folds for `buf_id`.
    pub(crate) fn open_all(&mut self, buf_id: BufferId) {
        self.folds.remove(&buf_id);
    }

    /// Replace all closed folds for `buf_id` with backend-authoritative extents.
    pub(crate) fn replace_all(&mut self, buf_id: BufferId, extents: Vec<(usize, usize)>) {
        let folds = self.folds.entry(buf_id).or_default();
        folds.clear();
        folds.extend(extents.into_iter().filter(|extent| extent.1 > extent.0));
        folds.sort_unstable_by_key(|&(start, _)| start);
    }

    /// Closed folds for `buf_id`, sorted by start line.
    pub(crate) fn get(&self, buf_id: BufferId) -> &[(usize, usize)] {
        self.folds.get(&buf_id).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// `true` when `line_idx` is inside a closed fold body (not the header).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn is_hidden(&self, buf_id: BufferId, line_idx: usize) -> bool {
        self.get(buf_id).iter().any(|&(s, e)| line_idx > s && line_idx <= e)
    }

    /// Moves by rendered lines while treating each closed fold as one line.
    pub(crate) fn line_after_visible_steps(
        &self,
        buf_id: BufferId,
        line_idx: usize,
        forward: bool,
        steps: usize,
        line_count: usize,
    ) -> usize {
        if line_count == 0 {
            return 0;
        }

        let last_line = line_count - 1;
        let mut line = line_idx.min(last_line);
        for _ in 0..steps {
            line = if forward {
                self.fold_at(buf_id, line)
                    .or_else(|| self.containing_fold(buf_id, line))
                    .map_or_else(|| line.saturating_add(1), |(_, end)| end.saturating_add(1))
                    .min(last_line)
            } else if let Some((start, _)) = self.containing_fold(buf_id, line) {
                start
            } else {
                let previous = line.saturating_sub(1);
                self.containing_fold(buf_id, previous).map_or(previous, |(start, _)| start)
            };
        }
        line
    }

    /// Keeps cursor inside viewport using rendered rows, never folded body lines.
    pub(crate) fn viewport_top_for_cursor(
        &self,
        buf_id: BufferId,
        top_line: usize,
        cursor_line: usize,
        viewport_height: usize,
        scroll_offset: usize,
        line_count: usize,
    ) -> usize {
        if viewport_height == 0 || line_count == 0 {
            return 0;
        }

        let last_line = line_count - 1;
        let cursor = self.fold_header_for_line(buf_id, cursor_line.min(last_line));
        let top = self.fold_header_for_line(buf_id, top_line.min(last_line));
        let offset = scroll_offset.min(viewport_height / 2);
        let cursor_row = self.rendered_row_for_line(buf_id, top, cursor);

        let top = if cursor < top || cursor_row.is_none_or(|row| row < offset) {
            self.line_after_visible_steps(buf_id, cursor, false, offset, line_count)
        } else if cursor_row
            .is_some_and(|row| row.saturating_add(offset).saturating_add(1) > viewport_height)
        {
            let rows_above_cursor = viewport_height.saturating_sub(offset).saturating_sub(1);
            self.line_after_visible_steps(buf_id, cursor, false, rows_above_cursor, line_count)
        } else {
            top
        };

        let last_visible_line = self.fold_header_for_line(buf_id, last_line);
        let max_top = self.line_after_visible_steps(
            buf_id,
            last_visible_line,
            false,
            viewport_height.saturating_sub(1),
            line_count,
        );
        top.min(max_top)
    }

    /// Maps one rendered viewport row to its logical buffer line.
    pub(crate) fn line_for_rendered_row(
        &self,
        buf_id: BufferId,
        top_line: usize,
        rendered_row: usize,
        line_count: usize,
    ) -> Option<usize> {
        if top_line >= line_count {
            return None;
        }

        let mut line = top_line;
        for _ in 0..rendered_row {
            line = self
                .fold_at(buf_id, line)
                .map_or_else(|| line.saturating_add(1), |(_, end)| end.saturating_add(1));
            if line >= line_count {
                return None;
            }
        }
        Some(line)
    }

    /// Returns half-open logical line range needed to fill rendered viewport rows.
    pub(crate) fn line_range_for_rendered_rows(
        &self,
        buf_id: BufferId,
        top_line: usize,
        rendered_rows: usize,
        line_count: usize,
    ) -> (usize, usize) {
        let start = top_line.min(line_count);
        let end = self
            .line_for_rendered_row(buf_id, start, rendered_rows, line_count)
            .unwrap_or(line_count);
        (start, end)
    }

    /// Maps one logical buffer line to its rendered viewport row.
    ///
    /// Lines inside a closed fold body map to the fold header row.
    pub(crate) fn rendered_row_for_line(
        &self,
        buf_id: BufferId,
        top_line: usize,
        line_idx: usize,
    ) -> Option<usize> {
        if line_idx < top_line {
            return None;
        }

        let mut line = top_line;
        let mut rendered_row = 0;
        loop {
            if line == line_idx {
                return Some(rendered_row);
            }

            if let Some((_, end)) = self.fold_at(buf_id, line) {
                if line_idx <= end {
                    return Some(rendered_row);
                }
                line = end.saturating_add(1);
            } else {
                line = line.saturating_add(1);
            }
            rendered_row = rendered_row.saturating_add(1);

            if line > line_idx {
                return None;
            }
        }
    }

    /// Returns the fold range starting at `line_idx`, if any.
    pub(crate) fn fold_at(&self, buf_id: BufferId, line_idx: usize) -> Option<(usize, usize)> {
        self.get(buf_id).iter().copied().find(|&(s, _)| s == line_idx)
    }

    fn containing_fold(&self, buf_id: BufferId, line_idx: usize) -> Option<(usize, usize)> {
        self.get(buf_id).iter().copied().find(|&(start, end)| line_idx > start && line_idx <= end)
    }

    fn fold_header_for_line(&self, buf_id: BufferId, line_idx: usize) -> usize {
        self.containing_fold(buf_id, line_idx).map_or(line_idx, |(start, _)| start)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_store_toggle_closes_and_opens() {
        let mut store = FoldStore::new();
        let extent = (0, 1);
        store.toggle(1, 0, extent);
        assert!(store.is_hidden(1, 1));
        assert!(!store.is_hidden(1, 0));
        store.toggle(1, 0, extent);
        assert!(!store.is_hidden(1, 1));
    }

    #[test]
    fn fold_store_toggle_opens_fold_from_hidden_body_line() {
        let mut store = FoldStore::new();
        store.close(1, 0, (0, 3));
        store.close(1, 5, (5, 7));

        store.toggle(1, 2, (2, 2));

        assert_eq!(store.fold_at(1, 0), None);
        assert_eq!(store.fold_at(1, 5), Some((5, 7)));
    }

    #[test]
    fn fold_store_open_all_clears() {
        let mut store = FoldStore::new();
        let extent = (0, 1);
        store.close(1, 0, extent);
        store.open_all(1);
        assert!(!store.is_hidden(1, 1));
    }

    #[test]
    fn fold_store_replace_all_uses_backend_extents() {
        let mut store = FoldStore::new();
        store.replace_all(1, vec![(0, 2)]);
        assert!(store.is_hidden(1, 1));
        assert!(store.is_hidden(1, 2));
        assert!(!store.is_hidden(1, 4));
    }

    #[test]
    fn fold_at_returns_header_fold() {
        let mut store = FoldStore::new();
        let extent = (0, 1);
        store.close(1, 0, extent);
        assert_eq!(store.fold_at(1, 0), Some(extent));
        assert_eq!(store.fold_at(1, 1), None);
    }

    #[test]
    fn visible_motion_skips_closed_fold_bodies_in_both_directions() {
        let mut store = FoldStore::new();
        store.close(1, 2, (2, 5));

        assert_eq!(store.line_after_visible_steps(1, 1, true, 1, 10), 2);
        assert_eq!(store.line_after_visible_steps(1, 2, true, 1, 10), 6);
        assert_eq!(store.line_after_visible_steps(1, 6, false, 1, 10), 2);
        assert_eq!(store.line_after_visible_steps(1, 4, true, 1, 10), 6);
        assert_eq!(store.line_after_visible_steps(1, 4, false, 1, 10), 2);
        assert_eq!(store.line_after_visible_steps(1, 1, true, 2, 10), 6);
    }

    #[test]
    fn viewport_top_uses_rendered_rows_and_never_fold_body() {
        let mut store = FoldStore::new();
        store.close(1, 10, (10, 100));

        assert_eq!(store.viewport_top_for_cursor(1, 0, 101, 20, 0, 120), 0);
        assert_eq!(store.viewport_top_for_cursor(1, 50, 101, 20, 0, 120), 10);
    }

    #[test]
    fn rendered_rows_skip_closed_fold_bodies() {
        let mut store = FoldStore::new();
        store.close(1, 0, (0, 2));
        store.close(1, 3, (3, 4));

        assert_eq!(store.line_for_rendered_row(1, 0, 0, 6), Some(0));
        assert_eq!(store.line_for_rendered_row(1, 0, 1, 6), Some(3));
        assert_eq!(store.line_for_rendered_row(1, 0, 2, 6), Some(5));
        assert_eq!(store.line_for_rendered_row(1, 0, 3, 6), None);
    }

    #[test]
    fn rendered_viewport_range_extends_past_closed_fold_bodies() {
        let mut store = FoldStore::new();
        store.close(1, 10, (10, 14));
        store.close(1, 18, (18, 20));

        assert_eq!(store.line_range_for_rendered_rows(1, 0, 20, 100), (0, 26));
        assert_eq!(store.line_range_for_rendered_rows(1, 95, 20, 100), (95, 100));
    }

    #[test]
    fn logical_lines_map_to_fold_aware_rendered_rows() {
        let mut store = FoldStore::new();
        store.close(1, 1, (1, 3));

        assert_eq!(store.rendered_row_for_line(1, 0, 0), Some(0));
        assert_eq!(store.rendered_row_for_line(1, 0, 1), Some(1));
        assert_eq!(store.rendered_row_for_line(1, 0, 2), Some(1));
        assert_eq!(store.rendered_row_for_line(1, 0, 3), Some(1));
        assert_eq!(store.rendered_row_for_line(1, 0, 4), Some(2));
        assert_eq!(store.rendered_row_for_line(1, 0, 5), Some(3));
        assert_eq!(store.rendered_row_for_line(1, 4, 3), None);
    }
}
