//! `editor.views` client.

use std::sync::Arc;

use ee_plugin_api::context::ServiceCaller;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::model::{BufferId, LanguageId, ViewId};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::client::ServiceClient;

/// Visible viewport for one view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Viewport {
    pub view_id: ViewId,
    pub first_line: usize,
    pub height: usize,
}

/// Read client for `editor.views@1`.
#[derive(Clone)]
pub struct ViewsClient {
    inner: ServiceClient,
}

impl ViewsClient {
    pub fn new(caller: Arc<dyn ServiceCaller>) -> Self {
        Self { inner: ServiceClient::new(caller, "editor.views@1") }
    }

    pub fn views_for_buffer(&self, buffer: BufferId) -> Result<Vec<ViewId>, PluginError> {
        self.inner.call_as("views_for_buffer", json!({ "buffer_id": buffer }))
    }

    pub fn active_view(&self) -> Result<Option<ViewId>, PluginError> {
        self.inner.call_as("active_view", json!({}))
    }

    pub fn viewport(&self, view: ViewId) -> Result<Viewport, PluginError> {
        self.inner.call_as("viewport", json!({ "view_id": view }))
    }

    pub fn view_buffer(&self, view: ViewId) -> Result<Option<BufferId>, PluginError> {
        self.inner.call_as("view_buffer", json!({ "view_id": view }))
    }

    pub fn view_language(&self, view: ViewId) -> Result<Option<LanguageId>, PluginError> {
        self.inner.call_as("view_language", json!({ "view_id": view }))
    }

    pub fn reveal(&self, view: ViewId, line: usize, column: usize) -> Result<(), PluginError> {
        self.inner.call_as("reveal", json!({ "view_id": view, "line": line, "column": column }))
    }

    pub fn scroll_to(&self, view: ViewId, line: usize, column: usize) -> Result<(), PluginError> {
        self.inner.call_as("scroll_to", json!({ "view_id": view, "line": line, "column": column }))
    }
}
