//! Local state for declarative plugin pickers.
//!
//! Filtering and selection happen in the frontend when a contribution declares
//! a local filter mode. The frontend never calls plugin code; it sends bounded
//! `ui_event` messages for changes the backend needs to observe.

use regex::Regex;

use ee_plugin_api::contribution::{FilterMode, PickerColumn};
use ee_plugin_api::ids::ContributionId;
use serde_json::Value;

use super::contributions::PickerRow;
use super::sanitize::{MAX_TEXT_BYTES, sanitize_text};

/// One visible plugin picker.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginPickerState {
    pub id: ContributionId,
    pub title: String,
    pub columns: Vec<PickerColumn>,
    pub filter_mode: FilterMode,
    pub query: String,
    pub revision: u64,
    pub items: Vec<PickerRow>,
    pub filtered: Vec<usize>,
    pub selected: usize,
}

impl PluginPickerState {
    #[must_use]
    pub fn new(
        id: ContributionId,
        title: String,
        columns: Vec<PickerColumn>,
        filter_mode: FilterMode,
        revision: u64,
        items: Vec<PickerRow>,
    ) -> Self {
        let mut picker = Self {
            id,
            title,
            columns,
            filter_mode,
            query: String::new(),
            revision,
            items,
            filtered: Vec::new(),
            selected: 0,
        };
        picker.refilter();
        picker
    }

    /// Apply one dynamic update. A stale revision is rejected and keeps the
    /// previous content.
    pub fn apply_update(&mut self, revision: u64, items: Vec<PickerRow>) -> bool {
        if revision <= self.revision {
            return false;
        }
        self.revision = revision;
        self.items = items;
        self.refilter();
        true
    }

    pub fn push_char(&mut self, ch: char) {
        if self.query.len() + ch.len_utf8() > MAX_TEXT_BYTES {
            return;
        }
        self.query.push(ch);
        self.refilter();
    }

    pub fn pop_char(&mut self) {
        self.query.pop();
        self.refilter();
    }

    pub fn move_up(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        if self.selected + 1 < self.filtered.len() {
            self.selected += 1;
        }
    }

    #[must_use]
    pub fn selected_item(&self) -> Option<&PickerRow> {
        self.filtered.get(self.selected).and_then(|index| self.items.get(*index))
    }

    #[must_use]
    pub fn visible_count(&self) -> usize {
        self.filtered.len()
    }

    fn refilter(&mut self) {
        let query = self.query.trim();
        self.filtered = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, row)| matches(row, self.filter_mode, query))
            .map(|(index, _)| index)
            .collect();
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
    }
}

fn matches(row: &PickerRow, mode: FilterMode, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let haystack = row.search_text().to_lowercase();
    let needle = query.to_lowercase();
    match mode {
        FilterMode::Substring => haystack.contains(&needle),
        FilterMode::Fuzzy => fuzzy_match(&haystack, &needle),
        FilterMode::Regex => regex_matches(&haystack, &needle),
    }
}

fn fuzzy_match(haystack: &str, needle: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|needle| chars.any(|candidate| candidate == needle))
}

fn regex_matches(haystack: &str, needle: &str) -> bool {
    // Invalid regex falls back to substring filtering; the frontend never
    // surfaces a plugin-authored regex error as a crash.
    match Regex::new(needle) {
        Ok(regex) => regex.is_match(haystack),
        Err(_) => haystack.contains(needle),
    }
}

/// Convert one dynamic picker value into display cells using the contribution
/// column metadata. Missing keys fall back to the row label.
#[must_use]
pub fn picker_cells(row: &PickerRow, columns: &[PickerColumn]) -> Vec<String> {
    if columns.is_empty() {
        return vec![row.label()];
    }
    columns
        .iter()
        .map(|column| {
            let cell = row.cell(&column.key);
            if sanitize_text("picker.cell", &cell).is_ok() { cell } else { String::new() }
        })
        .collect()
}

/// Render a picker row as an object suitable for a `ui_event` payload.
#[must_use]
pub fn row_event_payload(row: &PickerRow) -> Value {
    match &row.raw {
        Value::Object(_) => row.raw.clone(),
        other => Value::String(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(label: &str) -> PickerRow {
        PickerRow::parse(&Value::String(label.to_owned()))
    }

    fn picker(mode: FilterMode) -> PluginPickerState {
        PluginPickerState::new(
            ContributionId::new("demo.picker").unwrap(),
            String::from("Demo"),
            Vec::new(),
            mode,
            1,
            vec![row("alpha"), row("beta"), row("gamma")],
        )
    }

    #[test]
    fn local_substring_filter_and_selection_clamp() {
        let mut picker = picker(FilterMode::Substring);
        picker.push_char('a');
        assert_eq!(picker.visible_count(), 3);
        picker.push_char('m');
        assert_eq!(picker.visible_count(), 1);
        picker.move_down();
        assert_eq!(picker.selected, 0);
        picker.pop_char();
        assert_eq!(picker.query, "a");
    }

    #[test]
    fn fuzzy_and_regex_modes_filter_locally() {
        let mut fuzzy = picker(FilterMode::Fuzzy);
        fuzzy.push_char('b');
        fuzzy.push_char('t');
        assert_eq!(fuzzy.visible_count(), 1);
        assert_eq!(fuzzy.selected_item().map(PickerRow::label).as_deref(), Some("beta"));

        let mut regex = picker(FilterMode::Regex);
        regex.push_char('^');
        regex.push_char('g');
        assert_eq!(regex.visible_count(), 1);
    }

    #[test]
    fn stale_update_keeps_previous_content() {
        let mut picker = picker(FilterMode::Substring);
        assert!(!picker.apply_update(1, vec![row("new")]));
        assert_eq!(picker.items.len(), 3);
        assert!(picker.apply_update(2, vec![row("new")]));
        assert_eq!(picker.items.len(), 1);
    }
}
