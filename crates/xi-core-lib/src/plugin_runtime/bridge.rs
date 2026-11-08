//! Conversion and emission helpers between core state and typed plugin events.

use ee_plugin_api::error::PluginError;
use ee_plugin_api::model::{DeltaOp, DocumentDelta, TextEdit};
use xi_rope::{DeltaBuilder, DeltaElement, Interval, Rope, RopeDelta};

use crate::plugin_runtime::events::{
    ConfigChanged, DocumentChanged, DocumentSaved, LanguageChanged, SelectionChanged, ViewClosed,
    ViewOpened,
};
use crate::plugin_runtime::host::EditorPluginHost;

/// Convert a rope delta into the bounded splice model.
///
/// The model applies operations sequentially to the evolving document text, so
/// deletion offsets are computed in new-document coordinates.
pub fn rope_delta_to_model(delta: &RopeDelta) -> DocumentDelta {
    let mut ops = Vec::new();
    let mut new_cursor = 0usize;
    let mut base_cursor = 0usize;

    for element in &delta.els {
        match element {
            DeltaElement::Copy(start, end) => {
                if *start > base_cursor {
                    let gap = start - base_cursor;
                    ops.push(DeltaOp::Delete { start: new_cursor, end: new_cursor + gap });
                }
                new_cursor += end - start;
                base_cursor = *end;
            }
            DeltaElement::Insert(node) => {
                let text = node.to_string();
                new_cursor += text.len();
                ops.push(DeltaOp::Insert { offset: new_cursor - text.len(), text });
            }
        }
    }

    if base_cursor < delta.base_len {
        let gap = delta.base_len - base_cursor;
        ops.push(DeltaOp::Delete { start: new_cursor, end: new_cursor + gap });
    }

    DocumentDelta { ops }
}

/// Build a rope delta from validated text edits.
pub fn text_edits_to_delta(base_len: usize, edits: &[TextEdit]) -> Result<RopeDelta, PluginError> {
    let mut builder = DeltaBuilder::new(base_len);
    let mut last_end = 0usize;
    for edit in edits {
        if edit.range.start > edit.range.end || edit.range.end > base_len {
            return Err(PluginError::Schema {
                message: format!(
                    "edit range {}..{} is invalid for length {base_len}",
                    edit.range.start, edit.range.end
                ),
            });
        }
        if edit.range.start < last_end {
            return Err(PluginError::Schema {
                message: "edits must be sorted and non-overlapping".to_owned(),
            });
        }
        last_end = edit.range.end;
        builder.replace(
            Interval::new(edit.range.start, edit.range.end),
            Rope::from(edit.new_text.as_str()),
        );
    }
    Ok(builder.build())
}

/// Emit `editor.document.before_edit` when a host is attached.
pub fn document_changed_before(
    host: Option<&EditorPluginHost>,
    view_id: crate::tabs::ViewId,
    rev: u64,
    edit_type: &str,
) {
    if let Some(host) = host {
        let _ = host.emit(&crate::plugin_runtime::events::DocumentBeforeEdit {
            view_id: api_view(view_id),
            rev,
            edit_type: edit_type.to_owned(),
        });
    }
}

/// Core `ViewId` to wire [`ee_plugin_api::model::ViewId`].
pub fn api_view(view: crate::tabs::ViewId) -> ee_plugin_api::model::ViewId {
    ee_plugin_api::model::ViewId(view.0 as u64)
}

/// Core `BufferId` to wire [`ee_plugin_api::model::BufferId`].
pub fn api_buffer(buffer: crate::tabs::BufferId) -> ee_plugin_api::model::BufferId {
    ee_plugin_api::model::BufferId(buffer.0 as u64)
}

/// Emit `editor.document.changed` when a host is attached.
pub fn document_changed(host: Option<&EditorPluginHost>, event: &DocumentChanged) {
    if let Some(host) = host {
        let _ = host.emit(event);
    }
}

pub fn document_saved(host: Option<&EditorPluginHost>, event: &DocumentSaved) {
    if let Some(host) = host {
        let _ = host.emit(event);
    }
}

pub fn view_opened(host: Option<&EditorPluginHost>, event: &ViewOpened) {
    if let Some(host) = host {
        let _ = host.emit(event);
    }
}

pub fn view_closed(host: Option<&EditorPluginHost>, event: &ViewClosed) {
    if let Some(host) = host {
        let _ = host.emit(event);
    }
}

pub fn selection_changed(host: Option<&EditorPluginHost>, event: &SelectionChanged) {
    if let Some(host) = host {
        let _ = host.emit(event);
    }
}

pub fn language_changed(host: Option<&EditorPluginHost>, event: &LanguageChanged) {
    if let Some(host) = host {
        let _ = host.emit(event);
    }
}

pub fn config_changed(host: Option<&EditorPluginHost>, event: &ConfigChanged) {
    if let Some(host) = host {
        let _ = host.emit(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xi_rope::DeltaBuilder;

    #[test]
    fn pure_insert_converts_to_offset() {
        let mut builder = DeltaBuilder::new(5);
        builder.replace(Interval::new(2, 2), Rope::from("XY"));
        let model = rope_delta_to_model(&builder.build());
        assert_eq!(model.ops, vec![DeltaOp::Insert { offset: 2, text: "XY".to_owned() }]);
    }

    #[test]
    fn delete_and_replace_convert_to_sorted_splices() {
        let mut builder = DeltaBuilder::new(6);
        builder.replace(Interval::new(1, 3), Rope::from("Z"));
        let model = rope_delta_to_model(&builder.build());
        let mut text = "abcdef".to_owned();
        model.apply(&mut text).unwrap();
        assert_eq!(text, "aZdef");
    }

    #[test]
    fn text_edits_build_a_round_tripping_delta() {
        let edits = vec![TextEdit {
            range: ee_plugin_api::model::Range { start: 1, end: 2 },
            new_text: "Z".to_owned(),
        }];
        let delta = text_edits_to_delta(6, &edits).unwrap();
        let model = rope_delta_to_model(&delta);
        let mut text = "abcdef".to_owned();
        model.apply(&mut text).unwrap();
        assert_eq!(text, "aZcdef");
    }

    #[test]
    fn unsorted_edits_are_rejected() {
        let edits = vec![
            TextEdit {
                range: ee_plugin_api::model::Range { start: 2, end: 3 },
                new_text: "x".to_owned(),
            },
            TextEdit {
                range: ee_plugin_api::model::Range { start: 1, end: 2 },
                new_text: "y".to_owned(),
            },
        ];
        assert!(text_edits_to_delta(6, &edits).is_err());
    }
}
