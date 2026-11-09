//! `editor.commands` client.

use std::sync::Arc;

use ee_plugin_api::context::ServiceCaller;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::ids::CommandId;
use ee_plugin_api::model::ViewId;
use serde_json::{Value, json};

use crate::client::ServiceClient;

/// Client for `editor.commands@1`.
#[derive(Clone)]
pub struct CommandsClient {
    inner: ServiceClient,
}

impl CommandsClient {
    pub fn new(caller: Arc<dyn ServiceCaller>) -> Self {
        Self { inner: ServiceClient::new(caller, "editor.commands@1") }
    }

    pub fn list(&self) -> Result<Vec<CommandId>, PluginError> {
        self.inner.call_as("list", json!({}))
    }

    pub fn dispatch(
        &self,
        id: &CommandId,
        view: ViewId,
        args: Value,
    ) -> Result<Value, PluginError> {
        self.inner.call("dispatch", json!({ "id": id, "view_id": view, "args": args }))
    }
}
