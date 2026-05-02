//! Manual fold state management for the TUI editor.
//!
//! Folds are stored per-buffer as sorted `(start, end)` line-index pairs
//! (both 0-based, both inclusive).  When a fold is closed, lines
//! `start+1..=end` are hidden from view and `start` shows a fold marker.
//! Fold extents are detected by leading-indentation heuristics.

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

    /// Detect and close every indent-driven fold in `lines` for `buf_id`.
    pub(crate) fn close_all(&mut self, buf_id: BufferId, lines: &[String]) {
        let folds = self.folds.entry(buf_id).or_default();
        folds.clear();
        let mut i = 0;
        while i < lines.len() {
            if let Some(extent) = indent_fold_extent(lines, i) {
                folds.push(extent);
                i = extent.1 + 1;
            } else {
                i += 1;
            }
        }
    }

    /// Closed folds for `buf_id`, sorted by start line.
    pub(crate) fn get(&self, buf_id: BufferId) -> &[(usize, usize)] {
        self.folds.get(&buf_id).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// `true` when `line_idx` is inside a closed fold body (not the header).
    pub(crate) fn is_hidden(&self, buf_id: BufferId, line_idx: usize) -> bool {
        self.get(buf_id).iter().any(|&(s, e)| line_idx > s && line_idx <= e)
    }

    /// Returns the fold range starting at `line_idx`, if any.
    pub(crate) fn fold_at(&self, buf_id: BufferId, line_idx: usize) -> Option<(usize, usize)> {
        self.get(buf_id).iter().copied().find(|&(s, _)| s == line_idx)
    }
}

// ── Fold extent detection ─────────────────────────────────────────────────────

/// Compute the indent-driven fold extent starting at `start_line`.
///
/// Returns `Some((start_line, end_line))` covering all consecutive lines with
/// strictly greater leading indentation.  Blank lines are included without
/// breaking the extent.  Returns `None` when no deeper lines follow.
pub(crate) fn indent_fold_extent(lines: &[String], start_line: usize) -> Option<(usize, usize)> {
    let line_count = lines.len();
    if start_line + 1 >= line_count {
        return None;
    }
    let base_indent = leading_indent(&lines[start_line]);
    let mut end = start_line;
    for i in (start_line + 1)..line_count {
        let line = &lines[i];
        if line.trim().is_empty() {
            // Blank lines are part of the fold body but do not terminate it.
            continue;
        }
        if leading_indent(line) > base_indent {
            end = i;
        } else {
            break;
        }
    }
    if end > start_line {
        Some((start_line, end))
    } else {
        None
    }
}

fn leading_indent(line: &str) -> usize {
    let mut n = 0usize;
    for ch in line.chars() {
        match ch {
            ' ' => n += 1,
            // Treat tab as 4 spaces for indent comparison purposes.
            '\t' => n += 4,
            _ => break,
        }
    }
    n
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn indent_fold_extent_detects_deeper_block() {
        let lines = s(&["fn foo() {", "    let x = 1;", "    let y = 2;", "}"]);
        assert_eq!(indent_fold_extent(&lines, 0), Some((0, 2)));
    }

    #[test]
    fn indent_fold_extent_single_line_returns_none() {
        let lines = s(&["no_body"]);
        assert_eq!(indent_fold_extent(&lines, 0), None);
    }

    #[test]
    fn indent_fold_extent_last_line_returns_none() {
        let lines = s(&["fn a() {", "    x;", "fn b() {"]);
        assert_eq!(indent_fold_extent(&lines, 2), None);
    }

    #[test]
    fn indent_fold_extent_blank_lines_included() {
        let lines = s(&["fn foo() {", "    x;", "", "    y;", "}"]);
        assert_eq!(indent_fold_extent(&lines, 0), Some((0, 3)));
    }

    #[test]
    fn fold_store_toggle_closes_and_opens() {
        let mut store = FoldStore::new();
        let lines = s(&["fn foo() {", "    let x = 1;", "}"]);
        let extent = indent_fold_extent(&lines, 0).unwrap();
        store.toggle(1, 0, extent);
        assert!(store.is_hidden(1, 1));
        assert!(!store.is_hidden(1, 0));
        store.toggle(1, 0, extent);
        assert!(!store.is_hidden(1, 1));
    }

    #[test]
    fn fold_store_open_all_clears() {
        let mut store = FoldStore::new();
        let lines = s(&["fn foo() {", "    x;", "}"]);
        let extent = indent_fold_extent(&lines, 0).unwrap();
        store.close(1, 0, extent);
        store.open_all(1);
        assert!(!store.is_hidden(1, 1));
    }

    #[test]
    fn fold_store_close_all_detects_folds() {
        let mut store = FoldStore::new();
        let lines = s(&["fn foo() {", "    x;", "    y;", "}", "fn bar() {"]);
        store.close_all(1, &lines);
        assert!(store.is_hidden(1, 1));
        assert!(store.is_hidden(1, 2));
        assert!(!store.is_hidden(1, 4));
    }

    #[test]
    fn fold_at_returns_header_fold() {
        let mut store = FoldStore::new();
        let lines = s(&["fn a() {", "    x;", "}"]);
        let extent = indent_fold_extent(&lines, 0).unwrap();
        store.close(1, 0, extent);
        assert_eq!(store.fold_at(1, 0), Some(extent));
        assert_eq!(store.fold_at(1, 1), None);
    }
}
