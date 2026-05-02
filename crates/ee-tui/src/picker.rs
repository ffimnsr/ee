//! Picker overlay: file picker, buffer picker, live grep.
//!
//! All expensive I/O (walking the filesystem, grepping) is done synchronously
//! in `new_*` constructors because the pickers are only opened by an explicit
//! user action and the UI is blocked during construction anyway.  Result sets
//! are capped to avoid unbounded memory usage.

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use xi_core_lib::plugin_rpc::CodeActionDescriptor;

use crate::backend::CompletionSuggestion;

use crate::buffer::BufferId;

// ── Public types ──────────────────────────────────────────────────────────────

/// What kind of items a picker is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickerKind {
    Files,
    Buffers,
    LiveGrep,
    Help,
    Completions,
    CodeActions,
}

/// A single entry in a picker list.
#[derive(Debug, Clone)]
pub(crate) struct PickerItem {
    /// Primary text rendered in the list row.
    pub(crate) label: String,
    /// Short annotation rendered on the right side (directory, match context).
    #[allow(dead_code)]
    pub(crate) detail: Option<String>,
    /// Filesystem path for file / grep results.
    pub(crate) path: Option<PathBuf>,
    /// Buffer to switch to for buffer pickers.
    pub(crate) buf_id: Option<BufferId>,
    /// 0-based line offset inside `path` for grep results.
    pub(crate) line: Option<usize>,
    /// 1-based completion or code-action index sent back to the backend.
    pub(crate) choice_index: Option<usize>,
}

/// Picker overlay state.
#[derive(Debug)]
pub(crate) struct PickerState {
    pub(crate) kind: PickerKind,
    pub(crate) title: String,
    /// Text the user has typed since the picker opened.
    pub(crate) query: String,
    /// Root directory used for filesystem walks.
    cwd: PathBuf,
    /// All candidates (pre-filtered for LiveGrep, full list otherwise).
    items: Vec<PickerItem>,
    /// Indices into `items` that match the current `query`.
    pub(crate) filtered: Vec<usize>,
    /// Currently highlighted row within `filtered`.
    pub(crate) selected: usize,
}

impl PickerState {
    /// Open a file picker rooted at `cwd`.
    pub(crate) fn new_files(cwd: PathBuf) -> Self {
        let items = collect_files(&cwd);
        let filtered = (0..items.len()).collect();
        Self {
            kind: PickerKind::Files,
            title: "Files".to_owned(),
            query: String::new(),
            cwd,
            items,
            filtered,
            selected: 0,
        }
    }

    /// Open a buffer picker from the given `(id, title, path)` triples.
    pub(crate) fn new_buffers(
        bufs: impl IntoIterator<Item = (BufferId, String, Option<PathBuf>)>,
    ) -> Self {
        let items: Vec<PickerItem> = bufs
            .into_iter()
            .map(|(id, title, path)| PickerItem {
                detail: path.as_ref().and_then(|p| p.to_str()).map(str::to_owned),
                label: title,
                path,
                buf_id: Some(id),
                line: None,
                choice_index: None,
            })
            .collect();
        let filtered = (0..items.len()).collect();
        Self {
            kind: PickerKind::Buffers,
            title: "Buffers".to_owned(),
            query: String::new(),
            cwd: PathBuf::from("."),
            items,
            filtered,
            selected: 0,
        }
    }

    /// Open a live-grep picker with an optional seed query.
    pub(crate) fn new_grep(initial_query: String, cwd: PathBuf) -> Self {
        let items =
            if initial_query.is_empty() { Vec::new() } else { grep_files(&initial_query, &cwd) };
        let filtered = (0..items.len()).collect();
        Self {
            kind: PickerKind::LiveGrep,
            title: "Live Grep".to_owned(),
            query: initial_query,
            cwd,
            items,
            filtered,
            selected: 0,
        }
    }

    /// Open a static help or discovery picker.
    pub(crate) fn new_help(
        title: impl Into<String>,
        lines: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        let items: Vec<PickerItem> = lines
            .into_iter()
            .map(|line| PickerItem {
                label: line.into(),
                detail: None,
                path: None,
                buf_id: None,
                line: None,
                choice_index: None,
            })
            .collect();
        let filtered = (0..items.len()).collect();
        Self {
            kind: PickerKind::Help,
            title: title.into(),
            query: String::new(),
            cwd: PathBuf::from("."),
            items,
            filtered,
            selected: 0,
        }
    }

    pub(crate) fn new_completions(items: &[CompletionSuggestion]) -> Self {
        let items: Vec<PickerItem> = items
            .iter()
            .enumerate()
            .map(|(index, item)| PickerItem {
                label: item.label.clone(),
                detail: item.detail.clone(),
                path: None,
                buf_id: None,
                line: None,
                choice_index: Some(index + 1),
            })
            .collect();
        let filtered = (0..items.len()).collect();
        Self {
            kind: PickerKind::Completions,
            title: String::from("Completions"),
            query: String::new(),
            cwd: PathBuf::from("."),
            items,
            filtered,
            selected: 0,
        }
    }

    pub(crate) fn new_code_actions(actions: &[CodeActionDescriptor]) -> Self {
        let items: Vec<PickerItem> = actions
            .iter()
            .enumerate()
            .map(|(index, action)| PickerItem {
                label: action.title.clone(),
                detail: None,
                path: None,
                buf_id: None,
                line: None,
                choice_index: Some(index + 1),
            })
            .collect();
        let filtered = (0..items.len()).collect();
        Self {
            kind: PickerKind::CodeActions,
            title: String::from("Code Actions"),
            query: String::new(),
            cwd: PathBuf::from("."),
            items,
            filtered,
            selected: 0,
        }
    }

    /// Append a character to the query and update filtered results.
    pub(crate) fn push_char(&mut self, c: char) {
        self.query.push(c);
        self.refresh();
    }

    /// Remove the last character from the query and update filtered results.
    pub(crate) fn pop_char(&mut self) {
        self.query.pop();
        self.refresh();
    }

    /// Move the selection cursor up.
    pub(crate) fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    /// Move the selection cursor down.
    pub(crate) fn move_down(&mut self) {
        if self.selected + 1 < self.filtered.len() {
            self.selected += 1;
        }
    }

    /// Return the currently highlighted item, if any.
    pub(crate) fn selected_item(&self) -> Option<&PickerItem> {
        self.filtered.get(self.selected).map(|&i| &self.items[i])
    }

    /// Number of items visible after filtering.
    #[cfg(test)]
    pub(crate) fn visible_count(&self) -> usize {
        self.filtered.len()
    }

    /// Return a slice of display labels for `count` items starting at `offset`
    /// within the filtered list.
    pub(crate) fn visible_items_range(&self, offset: usize, count: usize) -> Vec<String> {
        self.filtered
            .iter()
            .skip(offset)
            .take(count)
            .map(|&i| self.items[i].label.clone())
            .collect()
    }

    // ── Private ───────────────────────────────────────────────────────────

    fn refresh(&mut self) {
        if matches!(self.kind, PickerKind::LiveGrep) {
            // Re-run grep; the query IS the grep pattern.
            self.items =
                if self.query.is_empty() { Vec::new() } else { grep_files(&self.query, &self.cwd) };
            self.filtered = (0..self.items.len()).collect();
        } else {
            self.refilter();
        }
        self.selected = 0;
    }

    /// Fuzzy-substring filter for Files/Buffers pickers.
    fn refilter(&mut self) {
        if self.query.is_empty() {
            self.filtered = (0..self.items.len()).collect();
        } else {
            let q = self.query.to_lowercase();
            self.filtered = self
                .items
                .iter()
                .enumerate()
                .filter(|(_, item)| item.label.to_lowercase().contains(&q))
                .map(|(i, _)| i)
                .collect();
        }
        self.selected = 0;
    }
}

// ── Filesystem helpers ────────────────────────────────────────────────────────

/// Maximum files returned by the file picker to keep UI responsive.
const FILE_LIMIT: usize = 10_000;

/// Collect all files reachable from `cwd`, honoring `.gitignore`.
fn collect_files(cwd: &Path) -> Vec<PickerItem> {
    let mut items = Vec::new();
    let walker = WalkBuilder::new(cwd).hidden(false).git_ignore(true).max_depth(Some(10)).build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.into_path();
        let label = path.strip_prefix(cwd).unwrap_or(&path).to_string_lossy().into_owned();
        items.push(PickerItem {
            label,
            detail: None,
            path: Some(path),
            buf_id: None,
            line: None,
            choice_index: None,
        });
        if items.len() >= FILE_LIMIT {
            break;
        }
    }
    items
}

/// Maximum grep matches returned to prevent unbounded memory usage.
const GREP_LIMIT: usize = 500;

/// Search all files under `cwd` for lines containing `query` (case-insensitive).
fn grep_files(query: &str, cwd: &Path) -> Vec<PickerItem> {
    if query.is_empty() {
        return Vec::new();
    }
    let query_lower = query.to_lowercase();
    let mut items = Vec::new();
    let walker = WalkBuilder::new(cwd).hidden(false).git_ignore(true).max_depth(Some(10)).build();
    'outer: for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.into_path();
        // Skip unreadable / binary files.
        let Ok(content) = std::fs::read_to_string(&path) else { continue };
        let rel = path.strip_prefix(cwd).unwrap_or(&path).to_string_lossy().into_owned();
        for (line_num, line_text) in content.lines().enumerate() {
            if line_text.to_lowercase().contains(&query_lower) {
                items.push(PickerItem {
                    label: format!("{}:{}: {}", rel, line_num + 1, line_text.trim()),
                    detail: Some(rel.clone()),
                    path: Some(path.clone()),
                    buf_id: None,
                    line: Some(line_num),
                    choice_index: None,
                });
                if items.len() >= GREP_LIMIT {
                    break 'outer;
                }
            }
        }
    }
    items
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_picker_filters_by_query() {
        let bufs = vec![
            (1u32, "main.rs".to_owned(), None),
            (2u32, "lib.rs".to_owned(), None),
            (3u32, "README.md".to_owned(), None),
        ];
        let mut picker = PickerState::new_buffers(bufs);
        assert_eq!(picker.visible_count(), 3);

        picker.push_char('r');
        // "main.rs", "lib.rs", "README.md" all contain 'r' (case-insensitive).
        assert_eq!(picker.visible_count(), 3);

        picker.push_char('s'); // "rs" -> "main.rs", "lib.rs"
        assert_eq!(picker.visible_count(), 2);
    }

    #[test]
    fn picker_navigation_clamps() {
        let bufs = vec![(1u32, "a.rs".to_owned(), None), (2u32, "b.rs".to_owned(), None)];
        let mut picker = PickerState::new_buffers(bufs);
        picker.move_up(); // already at 0, should stay
        assert_eq!(picker.selected, 0);
        picker.move_down();
        assert_eq!(picker.selected, 1);
        picker.move_down(); // past end, should stay
        assert_eq!(picker.selected, 1);
    }

    #[test]
    fn picker_pop_char_restores_filter() {
        let bufs = vec![(1u32, "foo.rs".to_owned(), None), (2u32, "bar.rs".to_owned(), None)];
        let mut picker = PickerState::new_buffers(bufs);
        picker.push_char('f'); // only "foo.rs"
        assert_eq!(picker.visible_count(), 1);
        picker.pop_char(); // back to all
        assert_eq!(picker.visible_count(), 2);
    }
}
