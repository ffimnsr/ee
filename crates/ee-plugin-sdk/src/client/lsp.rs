//! `editor.lsp` client.

use std::sync::Arc;

use ee_plugin_api::context::ServiceCaller;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::model::ViewId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::client::ServiceClient;

/// One typed LSP request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspRequest {
    pub view_id: ViewId,
    pub line: usize,
    pub column: usize,
    #[serde(default)]
    pub new_name: Option<String>,
}

/// Client for `editor.lsp@1`.
#[derive(Clone)]
pub struct LspClient {
    inner: ServiceClient,
}

impl LspClient {
    pub fn new(caller: Arc<dyn ServiceCaller>) -> Self {
        Self { inner: ServiceClient::new(caller, "editor.lsp@1") }
    }

    fn call(&self, method: &str, request: &LspRequest) -> Result<Value, PluginError> {
        let params = serde_json::to_value(request)
            .map_err(|error| PluginError::Schema { message: error.to_string() })?;
        self.inner.call(method, params)
    }

    pub fn hover(&self, request: &LspRequest) -> Result<Value, PluginError> {
        self.call("hover", request)
    }

    pub fn completion(&self, request: &LspRequest) -> Result<Value, PluginError> {
        self.call("completion", request)
    }

    pub fn definition(&self, request: &LspRequest) -> Result<Value, PluginError> {
        self.call("definition", request)
    }

    pub fn references(&self, request: &LspRequest) -> Result<Value, PluginError> {
        self.call("references", request)
    }

    pub fn formatting(&self, request: &LspRequest) -> Result<Value, PluginError> {
        self.call("formatting", request)
    }

    pub fn code_actions(&self, request: &LspRequest) -> Result<Value, PluginError> {
        self.call("code_actions", request)
    }

    pub fn rename_preview(&self, request: &LspRequest) -> Result<Value, PluginError> {
        self.call("rename_preview", request)
    }
}
