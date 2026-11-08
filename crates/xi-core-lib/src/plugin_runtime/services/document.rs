//! `editor.document` read service and `editor.document.edit` write service.

use ee_plugin_api::error::PluginError;
use ee_plugin_api::model::{BufferId, EditAck, EditRequest, Range};
use ee_plugin_api::service::Service;
use serde::Deserialize;

use crate::plugin_runtime::core_api::{DocumentSnapshot, SharedEditorAccess};

/// Maximum bytes a single read may return.
pub const MAX_READ_BYTES: usize = 1024 * 1024;
/// Maximum number of edits in one request.
pub const MAX_EDITS: usize = 1024;

/// Read-only document operations.
#[derive(Clone)]
pub struct DocumentService {
    access: SharedEditorAccess,
}

impl DocumentService {
    pub fn new(access: SharedEditorAccess) -> Self {
        Self { access }
    }

    pub fn snapshot(&self, buffer: BufferId) -> Result<DocumentSnapshot, PluginError> {
        self.access.document_snapshot(buffer)
    }

    pub fn revision(&self, buffer: BufferId) -> Result<u64, PluginError> {
        Ok(self.snapshot(buffer)?.rev)
    }

    pub fn length(&self, buffer: BufferId) -> Result<usize, PluginError> {
        Ok(self.snapshot(buffer)?.len)
    }

    pub fn line_count(&self, buffer: BufferId) -> Result<usize, PluginError> {
        Ok(self.snapshot(buffer)?.line_count)
    }

    pub fn text_range(&self, buffer: BufferId, range: Range) -> Result<String, PluginError> {
        if range.start > range.end {
            return Err(PluginError::Schema { message: "range start exceeds end".to_owned() });
        }
        if range.end - range.start > MAX_READ_BYTES {
            return Err(PluginError::Resource {
                message: format!("read range exceeds {MAX_READ_BYTES} bytes"),
            });
        }
        let snapshot = self.snapshot(buffer)?;
        if range.end > snapshot.len {
            return Err(PluginError::Schema {
                message: format!(
                    "range {}..{} exceeds document length {}",
                    range.start, range.end, snapshot.len
                ),
            });
        }
        self.access.read_range(buffer, range.start, range.end)
    }

    pub fn document_text(&self, buffer: BufferId, max_bytes: usize) -> Result<String, PluginError> {
        let cap = max_bytes.min(MAX_READ_BYTES);
        self.access.read_document(buffer, cap)
    }
}

impl Service for DocumentService {
    const ID: &'static str = "editor.document@1";
}

/// Revision-checked document mutations.
#[derive(Clone)]
pub struct DocumentEditService {
    access: SharedEditorAccess,
}

impl DocumentEditService {
    pub fn new(access: SharedEditorAccess) -> Self {
        Self { access }
    }

    /// Apply a validated edit request.
    pub fn apply(&self, request: EditRequest) -> Result<EditAck, PluginError> {
        let snapshot = self.access.document_snapshot(
            self.access.view_buffer(request.view_id).ok_or_else(|| PluginError::Schema {
                message: format!("view {} is not open", request.view_id.0),
            })?,
        )?;
        self.validate(&request, snapshot.len, snapshot.rev)?;
        let ack = self.access.apply_edit(&request)?;
        if !ack.applied {
            return Err(PluginError::Runtime {
                message: ack.reason.unwrap_or_else(|| "edit rejected".to_owned()),
            });
        }
        Ok(ack)
    }

    /// Replace one range, keeping the request revision and undo grouping.
    pub fn replace_range(&self, request: ReplaceRangeRequest) -> Result<EditAck, PluginError> {
        self.apply(EditRequest {
            view_id: request.view_id,
            rev: request.rev,
            edits: vec![ee_plugin_api::model::TextEdit {
                range: Range { start: request.start, end: request.end },
                new_text: request.text,
            }],
            priority: request.priority,
            after_cursor: request.after_cursor,
            undo_group: request.undo_group,
            author: request.author,
        })
    }

    /// Insert text at a byte offset.
    pub fn insert(&self, request: InsertRequest) -> Result<EditAck, PluginError> {
        self.replace_range(ReplaceRangeRequest {
            view_id: request.view_id,
            rev: request.rev,
            start: request.offset,
            end: request.offset,
            text: request.text,
            priority: request.priority,
            after_cursor: request.after_cursor,
            undo_group: request.undo_group,
            author: request.author,
        })
    }

    /// Delete a byte range.
    pub fn delete(&self, request: DeleteRequest) -> Result<EditAck, PluginError> {
        self.replace_range(ReplaceRangeRequest {
            view_id: request.view_id,
            rev: request.rev,
            start: request.start,
            end: request.end,
            text: String::new(),
            priority: request.priority,
            after_cursor: request.after_cursor,
            undo_group: request.undo_group,
            author: request.author,
        })
    }

    fn validate(&self, request: &EditRequest, len: usize, rev: u64) -> Result<(), PluginError> {
        if request.rev != rev {
            return Err(PluginError::Runtime {
                message: format!("stale revision {} (current {rev})", request.rev),
            });
        }
        if self.access.is_vlf(request.view_id) {
            return Err(PluginError::Permission {
                permission: "editor.document.write (VLF buffer)".to_owned(),
            });
        }
        if request.edits.len() > MAX_EDITS {
            return Err(PluginError::Resource {
                message: format!("edit count {} exceeds {MAX_EDITS}", request.edits.len()),
            });
        }

        let mut last_end = 0;
        let mut inserted = 0usize;
        for edit in &request.edits {
            if edit.range.start > edit.range.end || edit.range.end > len {
                return Err(PluginError::Schema {
                    message: format!(
                        "edit range {}..{} is invalid for length {len}",
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
            inserted = inserted.saturating_add(edit.new_text.len());
        }
        if inserted > MAX_READ_BYTES {
            return Err(PluginError::Resource {
                message: format!("inserted text exceeds {MAX_READ_BYTES} bytes"),
            });
        }
        Ok(())
    }
}

impl Service for DocumentEditService {
    const ID: &'static str = "editor.document.edit@1";
}

/// Range replacement request.
#[derive(Debug, Clone, Deserialize)]
pub struct ReplaceRangeRequest {
    pub view_id: ee_plugin_api::model::ViewId,
    pub rev: u64,
    pub start: usize,
    pub end: usize,
    pub text: String,
    #[serde(default)]
    pub priority: u64,
    #[serde(default)]
    pub after_cursor: bool,
    #[serde(default)]
    pub undo_group: Option<usize>,
    #[serde(default = "default_author")]
    pub author: String,
}

/// Insert request.
#[derive(Debug, Clone, Deserialize)]
pub struct InsertRequest {
    pub view_id: ee_plugin_api::model::ViewId,
    pub rev: u64,
    pub offset: usize,
    pub text: String,
    #[serde(default)]
    pub priority: u64,
    #[serde(default)]
    pub after_cursor: bool,
    #[serde(default)]
    pub undo_group: Option<usize>,
    #[serde(default = "default_author")]
    pub author: String,
}

/// Delete request.
#[derive(Debug, Clone, Deserialize)]
pub struct DeleteRequest {
    pub view_id: ee_plugin_api::model::ViewId,
    pub rev: u64,
    pub start: usize,
    pub end: usize,
    #[serde(default)]
    pub priority: u64,
    #[serde(default)]
    pub after_cursor: bool,
    #[serde(default)]
    pub undo_group: Option<usize>,
    #[serde(default = "default_author")]
    pub author: String,
}

fn default_author() -> String {
    "plugin".to_owned()
}
