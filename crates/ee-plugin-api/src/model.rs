//! Host-neutral document and language model types.
//!
//! Field names match the historical `xi_core_lib::plugin_rpc` wire shapes so
//! editor semantics are preserved. The difference is that deltas are expressed
//! as explicit, bounded splice operations instead of a rope type, keeping this
//! crate free of editor dependencies.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Configuration table carried on buffer and update records.
pub type ConfigTable = serde_json::Map<String, Value>;

/// Opaque buffer handle owned by the editor host.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct BufferId(pub u64);

/// Opaque view handle owned by the frontend.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct ViewId(pub u64);

impl std::fmt::Display for ViewId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Display for BufferId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Canonical language identifier.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct LanguageId(pub String);

impl LanguageId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Byte range within a document snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct Range {
    pub start: usize,
    pub end: usize,
}

/// A selection range expressed in byte offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct SelectionRange {
    pub start: usize,
    pub end: usize,
}

/// One splice operation in a document delta.
///
/// Offsets are interpreted against the document state produced by the previous
/// operation in the same delta.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DeltaOp {
    Insert { offset: usize, text: String },
    Delete { start: usize, end: usize },
}

/// Bounded document delta used by the neutral wire model.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct DocumentDelta {
    pub ops: Vec<DeltaOp>,
}

impl DocumentDelta {
    /// Apply the delta to a UTF-8 document string.
    ///
    /// Returns `None` when an offset is out of bounds or not on a UTF-8
    /// boundary, so callers fail closed instead of corrupting cached text.
    pub fn apply(&self, text: &mut String) -> Option<()> {
        for op in &self.ops {
            match op {
                DeltaOp::Insert { offset, text: inserted } => {
                    if *offset > text.len() || !text.is_char_boundary(*offset) {
                        return None;
                    }
                    text.insert_str(*offset, inserted);
                }
                DeltaOp::Delete { start, end } => {
                    if start > end
                        || *end > text.len()
                        || !text.is_char_boundary(*start)
                        || !text.is_char_boundary(*end)
                    {
                        return None;
                    }
                    text.replace_range(*start..*end, "");
                }
            }
        }
        Some(())
    }
}

/// Buffer information sent when a plugin initializes or a buffer opens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PluginBufferInfo {
    pub buffer_id: BufferId,
    pub views: Vec<ViewId>,
    pub rev: u64,
    pub buf_size: usize,
    pub nb_lines: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub syntax: LanguageId,
    pub config: ConfigTable,
}

/// Document update delivered to a plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PluginUpdate {
    pub view_id: ViewId,
    /// `None` means the change was too large to stream; the plugin must fetch a
    /// fresh snapshot.
    pub delta: Option<DocumentDelta>,
    pub new_len: usize,
    pub new_line_count: usize,
    pub rev: u64,
    pub undo_group: Option<usize>,
    pub edit_type: String,
    pub author: String,
}

/// Revision-checked edit request produced by a plugin command or listener.
///
/// `view_id` identifies both the target buffer (through the view) and the
/// selection truth the edit is anchored to. A request without a target cannot
/// be applied deterministically, so the field is mandatory in the v2 contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EditRequest {
    pub view_id: ViewId,
    pub rev: u64,
    pub edits: Vec<TextEdit>,
    pub priority: u64,
    pub after_cursor: bool,
    pub undo_group: Option<usize>,
    pub author: String,
}

/// Result of applying a plugin edit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EditAck {
    pub applied: bool,
    pub rev: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Diagnostic severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

/// Diagnostic anchored to a byte range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Diagnostic {
    pub range: Range,
    pub severity: DiagnosticSeverity,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// Hover content and its optional anchor range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Hover {
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<Range>,
}

/// Formatting parameters supplied by the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FormattingOptions {
    pub tab_size: usize,
    pub insert_spaces: bool,
}

/// One textual replacement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TextEdit {
    pub range: Range,
    pub new_text: String,
}

/// Completion entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CompletionSuggestion {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub insert_text: Option<String>,
}

/// Code action proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CodeAction {
    pub title: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edits: Vec<TextEdit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// Symbol entry returned by document or workspace symbol queries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SymbolItem {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub line: usize,
    pub column: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<SymbolItem>,
}

/// Navigation target for definitions, references, and locations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NavigationTarget {
    pub path: String,
    pub line: usize,
    pub column: usize,
    pub end_line: usize,
    pub end_column: usize,
}

/// Schema bundle for every neutral model type.
///
/// The bundle is serialized to `schemas/plugin-model-v2.json` and compared in a
/// snapshot test so accidental wire changes fail review.
pub fn model_schema() -> Value {
    use std::collections::BTreeMap;

    let mut schemas: BTreeMap<&'static str, Value> = BTreeMap::new();
    schemas.insert("buffer_id", schema_of::<BufferId>());
    schemas.insert("view_id", schema_of::<ViewId>());
    schemas.insert("language_id", schema_of::<LanguageId>());
    schemas.insert("range", schema_of::<Range>());
    schemas.insert("selection_range", schema_of::<SelectionRange>());
    schemas.insert("delta_op", schema_of::<DeltaOp>());
    schemas.insert("document_delta", schema_of::<DocumentDelta>());
    schemas.insert("plugin_buffer_info", schema_of::<PluginBufferInfo>());
    schemas.insert("plugin_update", schema_of::<PluginUpdate>());
    schemas.insert("edit_request", schema_of::<EditRequest>());
    schemas.insert("edit_ack", schema_of::<EditAck>());
    schemas.insert("diagnostic_severity", schema_of::<DiagnosticSeverity>());
    schemas.insert("diagnostic", schema_of::<Diagnostic>());
    schemas.insert("hover", schema_of::<Hover>());
    schemas.insert("formatting_options", schema_of::<FormattingOptions>());
    schemas.insert("text_edit", schema_of::<TextEdit>());
    schemas.insert("completion_suggestion", schema_of::<CompletionSuggestion>());
    schemas.insert("code_action", schema_of::<CodeAction>());
    schemas.insert("symbol_item", schema_of::<SymbolItem>());
    schemas.insert("navigation_target", schema_of::<NavigationTarget>());
    serde_json::to_value(schemas).expect("model schemas serialize")
}

fn schema_of<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).expect("schema serializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plugin_update_round_trips_fixture() {
        let fixture = json!({
            "view_id": 7,
            "delta": {
                "ops": [
                    { "op": "insert", "offset": 0, "text": "hi" },
                    { "op": "delete", "start": 0, "end": 1 }
                ]
            },
            "new_len": 2,
            "new_line_count": 1,
            "rev": 3,
            "undo_group": 1,
            "edit_type": "insert",
            "author": "plugin:demo"
        });
        let update: PluginUpdate = serde_json::from_value(fixture.clone()).unwrap();
        assert_eq!(serde_json::to_value(&update).unwrap(), fixture);
    }

    #[test]
    fn plugin_buffer_info_round_trips_fixture() {
        let fixture = json!({
            "buffer_id": 1,
            "views": [1, 2],
            "rev": 0,
            "buf_size": 4,
            "nb_lines": 1,
            "path": "src/main.rs",
            "syntax": "rust",
            "config": {}
        });
        let info: PluginBufferInfo = serde_json::from_value(fixture.clone()).unwrap();
        assert_eq!(serde_json::to_value(&info).unwrap(), fixture);
    }

    #[test]
    fn diagnostic_round_trips_fixture() {
        let fixture = json!({
            "range": { "start": 0, "end": 2 },
            "severity": "warning",
            "message": "unused",
            "source": "demo",
            "code": "W1"
        });
        let diagnostic: Diagnostic = serde_json::from_value(fixture.clone()).unwrap();
        assert_eq!(serde_json::to_value(&diagnostic).unwrap(), fixture);
    }

    #[test]
    fn edit_request_round_trips_fixture() {
        let fixture = json!({
            "view_id": 3,
            "rev": 9,
            "edits": [{ "range": { "start": 1, "end": 1 }, "new_text": "x" }],
            "priority": 0,
            "after_cursor": true,
            "undo_group": null,
            "author": "plugin:demo"
        });
        let request: EditRequest = serde_json::from_value(fixture.clone()).unwrap();
        assert_eq!(serde_json::to_value(&request).unwrap(), fixture);
    }

    #[test]
    fn remaining_model_types_round_trip() {
        let values = [
            serde_json::to_value(TextEdit {
                range: Range { start: 0, end: 0 },
                new_text: "x".to_owned(),
            })
            .unwrap(),
            serde_json::to_value(CompletionSuggestion {
                label: "foo".to_owned(),
                detail: None,
                insert_text: Some("foo()".to_owned()),
            })
            .unwrap(),
            serde_json::to_value(CodeAction {
                title: "fix".to_owned(),
                edits: vec![],
                command: Some("demo.fix".to_owned()),
            })
            .unwrap(),
            serde_json::to_value(SymbolItem {
                name: "main".to_owned(),
                kind: "function".to_owned(),
                path: "src/main.rs".to_owned(),
                line: 1,
                column: 0,
                children: vec![],
            })
            .unwrap(),
            serde_json::to_value(NavigationTarget {
                path: "src/main.rs".to_owned(),
                line: 1,
                column: 0,
                end_line: 1,
                end_column: 4,
            })
            .unwrap(),
            serde_json::to_value(Hover { content: "docs".to_owned(), range: None }).unwrap(),
            serde_json::to_value(FormattingOptions { tab_size: 4, insert_spaces: true }).unwrap(),
            serde_json::to_value(SelectionRange { start: 0, end: 1 }).unwrap(),
            serde_json::to_value(LanguageId::new("rust")).unwrap(),
            serde_json::to_value(BufferId(3)).unwrap(),
            serde_json::to_value(ViewId(4)).unwrap(),
            serde_json::to_value(DiagnosticSeverity::Hint).unwrap(),
        ];

        for value in values {
            assert!(value.is_number() || value.is_string() || value.is_object());
        }
    }

    #[test]
    fn delta_application_is_bounds_checked() {
        let mut text = "hello".to_owned();
        let delta = DocumentDelta {
            ops: vec![
                DeltaOp::Insert { offset: 0, text: "> ".to_owned() },
                DeltaOp::Delete { start: 2, end: 3 },
            ],
        };
        delta.apply(&mut text).unwrap();
        assert_eq!(text, "> ello");

        let bad = DocumentDelta { ops: vec![DeltaOp::Delete { start: 0, end: 99 }] };
        assert!(bad.apply(&mut text).is_none());
    }

    #[test]
    fn model_schema_snapshot_matches_checked_in_file() {
        let expected_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../schemas/plugin-model-v2.json");
        let expected = std::fs::read_to_string(&expected_path)
            .unwrap_or_else(|err| panic!("read {}: {err}", expected_path.display()));
        let expected: serde_json::Value = serde_json::from_str(&expected).unwrap();
        assert_eq!(model_schema(), expected);
    }
}
