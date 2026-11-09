//! `editor.syntax` client.

use std::sync::Arc;

use ee_plugin_api::context::ServiceCaller;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::model::{SymbolItem, ViewId};
use serde::Deserialize;
use serde_json::json;

use crate::client::ServiceClient;

/// One syntax scope.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SyntaxScope {
    pub start: usize,
    pub end: usize,
    pub scope: String,
}

/// One fold region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct FoldRange {
    pub header_line: usize,
    pub body_start: usize,
    pub body_end: usize,
}

/// Read client for `editor.syntax@1`.
#[derive(Clone)]
pub struct SyntaxClient {
    inner: ServiceClient,
}

impl SyntaxClient {
    pub fn new(caller: Arc<dyn ServiceCaller>) -> Self {
        Self { inner: ServiceClient::new(caller, "editor.syntax@1") }
    }

    pub fn scopes(&self, view: ViewId) -> Result<Vec<SyntaxScope>, PluginError> {
        self.inner.call_as("scopes", json!({ "view_id": view }))
    }

    pub fn folds(&self, view: ViewId) -> Result<Vec<FoldRange>, PluginError> {
        self.inner.call_as("folds", json!({ "view_id": view }))
    }

    pub fn symbols(&self, view: ViewId) -> Result<Vec<SymbolItem>, PluginError> {
        self.inner.call_as("symbols", json!({ "view_id": view }))
    }

    pub fn grammar_available(&self, language: &str) -> Result<bool, PluginError> {
        self.inner.call_as("grammar", json!({ "language": language }))
    }
}
