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
        if let Some(pos) = folds.iter().position(|&(s, _)| s == line) {
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

    /// Open (remove) the fold whose header is `line`.
    pub(crate) fn open(&mut self, buf_id: BufferId, line: usize) {
        if let Some(folds) = self.folds.get_mut(&buf_id) {
            folds.retain(|&(s, _)| s != line);
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

    /// Returns the fold range starting at `line_idx`, if any.
    pub(crate) fn fold_at(&self, buf_id: BufferId, line_idx: usize) -> Option<(usize, usize)> {
        self.get(buf_id).iter().copied().find(|&(s, _)| s == line_idx)
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
}
