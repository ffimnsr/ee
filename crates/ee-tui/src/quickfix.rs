//! Quickfix list and location-list state.
//!
//! The global quickfix list is populated from search results, diagnostics, and
//! build errors.  The location list mirrors the same structure but is
//! window-scoped; here we store it at the `App` level since ee-tui has one
//! focused editing context at a time.

use std::path::PathBuf;

// ── Entry ─────────────────────────────────────────────────────────────────────

/// A single entry in a quickfix or location list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QfEntry {
    /// Source file, if known.
    pub(crate) path: Option<PathBuf>,
    /// 0-based line number.
    pub(crate) line: usize,
    /// 0-based byte column.
    pub(crate) col: usize,
    /// Descriptive message (error text, match context, …).
    pub(crate) message: String,
}

impl QfEntry {
    /// Short label suitable for the quickfix panel list.
    pub(crate) fn display_label(&self) -> String {
        let path_part = self
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("?");
        format!("{}:{}: {}", path_part, self.line + 1, self.message)
    }
}

// ── List ──────────────────────────────────────────────────────────────────────

/// A navigable list of file locations.
#[derive(Debug, Default)]
pub(crate) struct QfList {
    /// Human-readable title shown in the panel header.
    pub(crate) title: String,
    pub(crate) entries: Vec<QfEntry>,
    /// Currently highlighted entry index (0-based).
    pub(crate) selected: usize,
}

impl QfList {
    pub(crate) fn new(title: impl Into<String>, entries: Vec<QfEntry>) -> Self {
        Self { title: title.into(), entries, selected: 0 }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Return the selected entry, if any.
    pub(crate) fn current(&self) -> Option<&QfEntry> {
        self.entries.get(self.selected)
    }

    /// Advance to the next entry (clamps at last).
    pub(crate) fn next_entry(&mut self) -> Option<&QfEntry> {
        if self.entries.is_empty() {
            return None;
        }
        self.selected = (self.selected + 1).min(self.entries.len() - 1);
        self.entries.get(self.selected)
    }

    /// Retreat to the previous entry (clamps at first).
    pub(crate) fn prev_entry(&mut self) -> Option<&QfEntry> {
        if self.entries.is_empty() {
            return None;
        }
        self.selected = self.selected.saturating_sub(1);
        self.entries.get(self.selected)
    }

    /// Select by 1-based index; clamps to valid range.
    pub(crate) fn select_one_based(&mut self, n: usize) -> Option<&QfEntry> {
        if self.entries.is_empty() {
            return None;
        }
        let idx = n.saturating_sub(1).min(self.entries.len() - 1);
        self.selected = idx;
        self.entries.get(idx)
    }

    /// Move selection to the first entry.
    pub(crate) fn first_entry(&mut self) -> Option<&QfEntry> {
        self.selected = 0;
        self.entries.first()
    }

    /// Move selection to the last entry.
    pub(crate) fn last_entry(&mut self) -> Option<&QfEntry> {
        self.selected = self.entries.len().saturating_sub(1);
        self.entries.last()
    }

    /// Move selection cursor down (panel navigation).
    pub(crate) fn move_down(&mut self) {
        if self.selected + 1 < self.entries.len() {
            self.selected += 1;
        }
    }

    /// Move selection cursor up (panel navigation).
    pub(crate) fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_list(n: usize) -> QfList {
        let entries = (0..n)
            .map(|i| QfEntry { path: None, line: i, col: 0, message: format!("msg {i}") })
            .collect();
        QfList::new("test", entries)
    }

    #[test]
    fn next_entry_clamps_at_end() {
        let mut list = make_list(3);
        list.next_entry();
        list.next_entry();
        // Third call must not advance past index 2.
        list.next_entry();
        assert_eq!(list.selected, 2);
    }

    #[test]
    fn prev_entry_clamps_at_start() {
        let mut list = make_list(3);
        list.prev_entry();
        assert_eq!(list.selected, 0);
    }

    #[test]
    fn select_one_based_converts_correctly() {
        let mut list = make_list(5);
        list.select_one_based(3);
        assert_eq!(list.selected, 2);
    }

    #[test]
    fn select_one_based_clamps_to_last() {
        let mut list = make_list(3);
        list.select_one_based(99);
        assert_eq!(list.selected, 2);
    }

    #[test]
    fn empty_list_operations_are_safe() {
        let mut list = QfList::default();
        assert!(list.next_entry().is_none());
        assert!(list.prev_entry().is_none());
        assert!(list.current().is_none());
        assert!(list.first_entry().is_none());
        assert!(list.last_entry().is_none());
    }

    #[test]
    fn display_label_formats_correctly() {
        let entry = QfEntry {
            path: Some(PathBuf::from("/src/main.rs")),
            line: 4,
            col: 0,
            message: "unused variable".to_owned(),
        };
        assert_eq!(entry.display_label(), "main.rs:5: unused variable");
    }
}
