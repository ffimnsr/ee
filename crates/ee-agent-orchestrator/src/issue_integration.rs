//! Issue checklist integration: markdown checklist items stay in sync with
//! recorded passing criteria.
//!
//! [`IssueChecklist`] parses `- [ ]` / `- [x]` items from configured issue
//! files, matches completed task criteria to items by a stable key
//! (`(criteria:KEY)` or `[criteria:KEY]` marker) or by stable text, and only
//! marks an item complete when the recorded validation evidence contains a
//! passing run of the criterion.  Marking produces a [`ChecklistEdit`]
//! (file, line, original, replacement) that the caller routes through an
//! approved write/edit tool; the integration itself never writes files.
//! Integration is optional and scoped to the configured files.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::OrchestratorError;
use crate::final_response::ValidationRecorder;

/// Marker prefix of stable criteria keys inside checklist item text.
pub const CRITERIA_KEY_MARKER: &str = "criteria:";
/// Cap on one parsed criteria key's length.
const MAX_KEY_CHARS: usize = 128;

/// Which issue files are integrated, and whether passing validation is
/// required before marking items complete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct IssueChecklistConfig {
    /// The only files whose checklist items may be parsed or edited.
    pub files: Vec<PathBuf>,
    /// When true (default), an item is only marked complete after the
    /// verification criterion recorded a passing validation run.
    pub require_validation: bool,
}

impl Default for IssueChecklistConfig {
    fn default() -> Self {
        Self { files: Vec::new(), require_validation: true }
    }
}

/// One parsed checklist item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ChecklistItem {
    /// The issue file containing the item.
    pub file: PathBuf,
    /// 1-based line number of the item's marker.
    pub line: usize,
    /// Item text after the `- [ ] ` marker, trimmed, including any criteria
    /// key marker.
    pub text: String,
    /// Whether the item is already marked complete.
    pub checked: bool,
    /// Stable criteria key from a `(criteria:KEY)` or `[criteria:KEY]`
    /// marker, when present.
    pub criteria_key: Option<String>,
    /// The full original line (edit source).
    pub original_line: String,
}

impl ChecklistItem {
    /// Item text with the criteria key marker removed, trimmed; the stable
    /// text used for matching.
    #[must_use]
    pub fn display_text(&self) -> String {
        strip_key(&self.text)
    }
}

/// One line edit that marks a checklist item complete; the caller applies it
/// through an approved write/edit tool.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChecklistEdit {
    /// The issue file to edit.
    pub file: PathBuf,
    /// 1-based line to replace.
    pub line: usize,
    /// The original line.
    pub original: String,
    /// The replacement line with the checkbox flipped to `[x]`.
    pub replacement: String,
}

/// Parsed checklist state for the configured issue files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IssueChecklist {
    config: IssueChecklistConfig,
    items: Vec<ChecklistItem>,
}

impl IssueChecklist {
    /// Creates an empty checklist bound to the given config.
    #[must_use]
    pub fn new(config: IssueChecklistConfig) -> Self {
        Self { config, items: Vec::new() }
    }

    /// Parses checklist items from `content` of `file`, appending them in
    /// line order.  Unconfigured files fail closed.  Returns the number of
    /// items parsed.
    pub fn parse_file(&mut self, file: PathBuf, content: &str) -> Result<usize, OrchestratorError> {
        if !self.config.files.iter().any(|configured| configured == &file) {
            return Err(OrchestratorError::PolicyDenied(format!(
                "issue file {} is not configured for checklist integration",
                file.display()
            )));
        }
        let mut parsed = 0usize;
        for (index, line) in content.lines().enumerate() {
            let Some((checked, text)) = parse_checklist_line(line) else { continue };
            let criteria_key = extract_key(&text);
            self.items.push(ChecklistItem {
                file: file.clone(),
                line: index + 1,
                text,
                checked,
                criteria_key,
                original_line: line.to_string(),
            });
            parsed += 1;
        }
        Ok(parsed)
    }

    /// All parsed items in parse order.
    #[must_use]
    pub fn items(&self) -> &[ChecklistItem] {
        &self.items
    }

    /// First item carrying the stable criteria key, if any.
    #[must_use]
    pub fn find_by_key(&self, key: &str) -> Option<&ChecklistItem> {
        self.items.iter().find(|item| item.criteria_key.as_deref() == Some(key))
    }

    /// First item whose display text equals `text` (trimmed), if any.
    #[must_use]
    pub fn find_by_text(&self, text: &str) -> Option<&ChecklistItem> {
        let wanted = text.trim();
        self.items.iter().find(|item| item.display_text() == wanted)
    }

    /// Marks the item with stable key `key` complete when the recorded
    /// validation evidence contains a passing run of `verification` (unless
    /// `require_validation` is off).  Returns the edit the caller should
    /// route through an approved write/edit tool.  Fails closed when the
    /// item is unknown or already complete, or when the criteria have no
    /// recorded passing validation.
    pub fn mark_complete(
        &mut self,
        key: &str,
        verification: &str,
        validation: &ValidationRecorder,
    ) -> Result<ChecklistEdit, OrchestratorError> {
        let index = self
            .items
            .iter()
            .position(|item| item.criteria_key.as_deref() == Some(key))
            .ok_or_else(|| {
                OrchestratorError::InvalidState(format!(
                    "unknown checklist item with criteria key {key}"
                ))
            })?;
        self.apply_mark(index, verification, validation)
    }

    /// Marks the item whose display text equals `text` complete; see
    /// [`IssueChecklist::mark_complete`].
    pub fn mark_complete_by_text(
        &mut self,
        text: &str,
        verification: &str,
        validation: &ValidationRecorder,
    ) -> Result<ChecklistEdit, OrchestratorError> {
        let wanted = text.trim();
        let index =
            self.items.iter().position(|item| item.display_text() == wanted).ok_or_else(|| {
                OrchestratorError::InvalidState(format!("unknown checklist item {wanted}"))
            })?;
        self.apply_mark(index, verification, validation)
    }

    fn apply_mark(
        &mut self,
        index: usize,
        verification: &str,
        validation: &ValidationRecorder,
    ) -> Result<ChecklistEdit, OrchestratorError> {
        let item = &self.items[index];
        if item.checked {
            return Err(OrchestratorError::InvalidState(format!(
                "checklist item on line {} is already complete",
                item.line
            )));
        }
        if self.config.require_validation
            && !validation.passed_commands().iter().any(|command| command == verification)
        {
            return Err(OrchestratorError::PolicyDenied(format!(
                "criteria {verification} for checklist item on line {} has no recorded passing validation",
                item.line
            )));
        }
        let item = &mut self.items[index];
        item.checked = true;
        Ok(ChecklistEdit {
            file: item.file.clone(),
            line: item.line,
            original: item.original_line.clone(),
            replacement: format!("- [x] {}", item.text),
        })
    }
}

/// Parses one checklist line: `- [ ] text` or `- [x] text` (optional leading
/// whitespace).  Returns `(checked, text)`.
fn parse_checklist_line(line: &str) -> Option<(bool, String)> {
    let body = line.trim_start();
    let rest = body.strip_prefix("- [")?;
    let bytes = rest.as_bytes();
    let marker = *bytes.first()?;
    if !matches!(marker, b' ' | b'x' | b'X') {
        return None;
    }
    if bytes.get(1) != Some(&b']') {
        return None;
    }
    let text = rest[2..].trim().to_string();
    Some((marker != b' ', text))
}

/// Stable criteria key from a `(criteria:KEY)` or `[criteria:KEY]` marker,
/// if present.
fn extract_key(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index + CRITERIA_KEY_MARKER.len() <= bytes.len() {
        if bytes[index..].starts_with(CRITERIA_KEY_MARKER.as_bytes()) {
            let opener = if index > 0 { bytes[index - 1] } else { 0 };
            let closer = match opener {
                b'(' => b')',
                b'[' => b']',
                _ => {
                    index += 1;
                    continue;
                }
            };
            let start = index + CRITERIA_KEY_MARKER.len();
            let mut end = start;
            while end < bytes.len() && bytes[end] != closer && end - start < MAX_KEY_CHARS {
                end += 1;
            }
            if end < bytes.len() && bytes[end] == closer {
                let key = text[start..end].trim().to_string();
                if !key.is_empty() {
                    return Some(key);
                }
            }
        }
        index += 1;
    }
    None
}

/// Item text with the `(criteria:KEY)` / `[criteria:KEY]` marker removed.
fn strip_key(text: &str) -> String {
    let Some(key) = extract_key(text) else { return text.trim().to_string() };
    let marker_len = CRITERIA_KEY_MARKER.len() + key.len();
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index + marker_len <= bytes.len() {
        if bytes[index..].starts_with(CRITERIA_KEY_MARKER.as_bytes())
            && index > 0
            && matches!(bytes[index - 1], b'(' | b'[')
            && bytes.get(index + marker_len).copied()
                == Some(if bytes[index - 1] == b'(' { b')' } else { b']' })
            && text[index + CRITERIA_KEY_MARKER.len()..index + marker_len] == key
        {
            let start = index - 1;
            let end = start + marker_len + 2;
            let mut result = String::with_capacity(text.len());
            result.push_str(&text[..start]);
            result.push_str(&text[end..]);
            return result.trim().to_string();
        }
        index += 1;
    }
    text.trim().to_string()
}

/// Whether `file` is a configured issue file (path equality).
#[must_use]
pub fn is_configured(config: &IssueChecklistConfig, file: &Path) -> bool {
    config.files.iter().any(|configured| configured == file)
}

#[cfg(test)]
mod tests {
    use crate::final_response::{ValidationOutcome, ValidationRecorder};

    use super::*;

    const ISSUE_CONTENT: &str = "\
# Issue

## Task

- [ ] implement phase 1 (criteria:cargo-test-phase-1)
- [x] implement phase 2 (criteria:cargo-test-phase-2)
- [ ] implement phase 3
- not a checklist item
  - [ ] nested item [criteria:nested-key]
";

    fn config() -> IssueChecklistConfig {
        IssueChecklistConfig { files: vec![PathBuf::from("ISSUES.md")], require_validation: true }
    }

    fn parsed() -> IssueChecklist {
        let mut checklist = IssueChecklist::new(config());
        checklist
            .parse_file(PathBuf::from("ISSUES.md"), ISSUE_CONTENT)
            .expect("parses configured file");
        checklist
    }

    #[test]
    fn parse_finds_items_with_lines_keys_and_state() {
        let checklist = parsed();
        let items = checklist.items();
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].line, 5);
        assert!(!items[0].checked);
        assert_eq!(items[0].criteria_key.as_deref(), Some("cargo-test-phase-1"));
        assert_eq!(items[0].file, PathBuf::from("ISSUES.md"));
        assert_eq!(items[1].line, 6);
        assert!(items[1].checked);
        assert_eq!(items[2].criteria_key, None);
        assert_eq!(items[3].criteria_key.as_deref(), Some("nested-key"));
        assert_eq!(items[3].display_text(), "nested item");
    }

    #[test]
    fn unconfigured_files_fail_closed() {
        let mut checklist = IssueChecklist::new(config());
        let error = checklist
            .parse_file(PathBuf::from("OTHER.md"), "# nothing")
            .expect_err("unconfigured file rejected");
        assert!(
            matches!(error, OrchestratorError::PolicyDenied(ref reason) if reason.contains("not configured"))
        );
        assert!(checklist.items().is_empty());
    }

    #[test]
    fn find_by_key_and_text_work() {
        let checklist = parsed();
        let by_key = checklist.find_by_key("cargo-test-phase-1").expect("found by key");
        assert_eq!(by_key.line, 5);
        assert_eq!(checklist.find_by_key("missing"), None);
        let by_text = checklist.find_by_text("implement phase 3").expect("found by text");
        assert_eq!(by_text.line, 7);
        assert_eq!(checklist.find_by_text("no such item"), None);
    }

    #[test]
    fn marks_item_only_after_recorded_passing_validation() {
        let mut checklist = parsed();
        let mut validation = ValidationRecorder::new();
        let error = checklist
            .mark_complete("cargo-test-phase-1", "cargo test --quiet", &validation)
            .expect_err("no recorded validation yet");
        assert!(
            matches!(error, OrchestratorError::PolicyDenied(ref reason) if reason.contains("no recorded passing validation"))
        );
        assert!(!checklist.find_by_key("cargo-test-phase-1").expect("item").checked);

        validation.record(
            "cargo test --quiet",
            ValidationOutcome::Passed,
            Some("clean".into()),
            None,
        );
        let edit = checklist
            .mark_complete("cargo-test-phase-1", "cargo test --quiet", &validation)
            .expect("marks after passing validation");
        assert_eq!(edit.file, PathBuf::from("ISSUES.md"));
        assert_eq!(edit.line, 5);
        assert!(edit.original.starts_with("- [ ] implement phase 1"));
        assert_eq!(edit.replacement, "- [x] implement phase 1 (criteria:cargo-test-phase-1)");
        assert!(checklist.find_by_key("cargo-test-phase-1").expect("item").checked);
    }

    #[test]
    fn failed_criteria_do_not_mark_item_complete() {
        let mut checklist = parsed();
        let mut validation = ValidationRecorder::new();
        validation.record(
            "cargo test --quiet",
            ValidationOutcome::Failed,
            Some("boom".into()),
            None,
        );
        let error = checklist
            .mark_complete("cargo-test-phase-1", "cargo test --quiet", &validation)
            .expect_err("failed criteria must not mark");
        assert!(matches!(error, OrchestratorError::PolicyDenied(_)));
        assert!(!checklist.find_by_key("cargo-test-phase-1").expect("item").checked);
    }

    #[test]
    fn mark_complete_by_text_uses_stable_text() {
        let mut checklist = parsed();
        let mut validation = ValidationRecorder::new();
        validation.record("cargo check", ValidationOutcome::Passed, None, None);
        let edit = checklist
            .mark_complete_by_text("implement phase 3", "cargo check", &validation)
            .expect("marks by text");
        assert_eq!(edit.line, 7);
        assert_eq!(edit.replacement, "- [x] implement phase 3");
    }

    #[test]
    fn already_checked_items_are_rejected() {
        let mut checklist = parsed();
        let mut validation = ValidationRecorder::new();
        validation.record("cargo test", ValidationOutcome::Passed, None, None);
        let error = checklist
            .mark_complete("cargo-test-phase-2", "cargo test", &validation)
            .expect_err("already complete");
        assert!(error.to_string().contains("already complete"));
    }

    #[test]
    fn unknown_items_are_rejected() {
        let mut checklist = parsed();
        let error = checklist
            .mark_complete("no-such-key", "cargo test", &ValidationRecorder::new())
            .expect_err("unknown key");
        assert!(error.to_string().contains("unknown checklist item"));
        let mut checklist = parsed();
        let error = checklist
            .mark_complete_by_text("nothing here", "cargo test", &ValidationRecorder::new())
            .expect_err("unknown text");
        assert!(error.to_string().contains("unknown checklist item"));
    }

    #[test]
    fn validation_can_be_disabled_explicitly() {
        let mut checklist = IssueChecklist::new(IssueChecklistConfig {
            files: vec![PathBuf::from("ISSUES.md")],
            require_validation: false,
        });
        checklist
            .parse_file(PathBuf::from("ISSUES.md"), "- [ ] item (criteria:key-1)")
            .expect("parses");
        let edit = checklist
            .mark_complete("key-1", "cargo test", &ValidationRecorder::new())
            .expect("marks without validation evidence");
        assert_eq!(edit.replacement, "- [x] item (criteria:key-1)");
    }

    #[test]
    fn key_extraction_handles_both_marker_forms_and_ignores_plain_text() {
        let text = "add feature (criteria:add-feature)";
        assert_eq!(extract_key(text).as_deref(), Some("add-feature"));
        assert_eq!(
            extract_key("add feature [criteria:bracket-key]").as_deref(),
            Some("bracket-key")
        );
        assert_eq!(extract_key("add feature"), None);
        assert_eq!(extract_key("mention criteria:not-a-key here"), None);
        assert_eq!(strip_key("add feature (criteria:add-feature)"), "add feature");
        assert_eq!(strip_key("add feature [criteria:bracket-key]"), "add feature");
        assert_eq!(strip_key("plain item"), "plain item");
    }

    #[test]
    fn checklist_types_roundtrip_through_json() {
        let config = IssueChecklistConfig {
            files: vec![PathBuf::from("ISSUES.md")],
            require_validation: false,
        };
        let json = serde_json::to_string(&config).expect("serializes");
        let restored: IssueChecklistConfig = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, config);

        let item = ChecklistItem {
            file: PathBuf::from("ISSUES.md"),
            line: 5,
            text: "- [ ] x (criteria:k)".to_string(),
            checked: false,
            criteria_key: Some("k".to_string()),
            original_line: "- [ ] x (criteria:k)".to_string(),
        };
        let json = serde_json::to_string(&item).expect("serializes");
        let restored: ChecklistItem = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, item);
    }
}
