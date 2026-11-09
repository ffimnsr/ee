//! `editor.document` and `editor.document.edit` clients.

use std::sync::Arc;

use ee_plugin_api::context::ServiceCaller;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::model::{BufferId, EditAck, EditRequest, LanguageId, Range, TextEdit};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::client::ServiceClient;

/// Immutable document metadata returned by `editor.document.snapshot`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentSnapshot {
    pub buffer_id: BufferId,
    pub rev: u64,
    pub len: usize,
    pub line_count: usize,
    #[serde(default)]
    pub path: Option<String>,
    pub language: LanguageId,
    #[serde(default)]
    pub vlf: bool,
}

/// Read client for `editor.document@1`.
#[derive(Clone)]
pub struct DocumentClient {
    read: ServiceClient,
    edit: ServiceClient,
}

impl DocumentClient {
    pub fn new(caller: Arc<dyn ServiceCaller>) -> Self {
        Self {
            read: ServiceClient::new(Arc::clone(&caller), "editor.document@1"),
            edit: ServiceClient::new(caller, "editor.document.edit@1"),
        }
    }

    pub fn snapshot(&self, buffer: BufferId) -> Result<DocumentSnapshot, PluginError> {
        self.read.call_as("snapshot", json!({ "buffer_id": buffer }))
    }

    pub fn revision(&self, buffer: BufferId) -> Result<u64, PluginError> {
        self.read.call_as("revision", json!({ "buffer_id": buffer }))
    }

    pub fn length(&self, buffer: BufferId) -> Result<usize, PluginError> {
        self.read.call_as("length", json!({ "buffer_id": buffer }))
    }

    pub fn line_count(&self, buffer: BufferId) -> Result<usize, PluginError> {
        self.read.call_as("line_count", json!({ "buffer_id": buffer }))
    }

    pub fn text_range(&self, buffer: BufferId, range: Range) -> Result<String, PluginError> {
        self.read.call_as(
            "text_range",
            json!({ "buffer_id": buffer, "start": range.start, "end": range.end }),
        )
    }

    pub fn document_text(&self, buffer: BufferId, max_bytes: usize) -> Result<String, PluginError> {
        self.read.call_as("document_text", json!({ "buffer_id": buffer, "max_bytes": max_bytes }))
    }

    pub fn apply(&self, request: &EditRequest) -> Result<EditAck, PluginError> {
        self.edit.call_as("apply", serde_json::to_value(request).map_err(schema)?)
    }

    pub fn replace_range(
        &self,
        view_id: ee_plugin_api::model::ViewId,
        rev: u64,
        range: Range,
        text: impl Into<String>,
        author: impl Into<String>,
    ) -> Result<EditAck, PluginError> {
        let request = EditRequest {
            view_id,
            rev,
            edits: vec![TextEdit { range, new_text: text.into() }],
            priority: 0,
            after_cursor: false,
            undo_group: None,
            author: author.into(),
        };
        self.apply(&request)
    }
}

fn schema(error: serde_json::Error) -> PluginError {
    PluginError::Schema { message: error.to_string() }
}
