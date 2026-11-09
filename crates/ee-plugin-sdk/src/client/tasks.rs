//! `editor.tasks` client.

use std::sync::Arc;

use ee_plugin_api::context::ServiceCaller;
use ee_plugin_api::effect::TaskRequest;
use ee_plugin_api::error::PluginError;
use serde_json::{Value, json};

use crate::client::ServiceClient;

/// Client for `editor.tasks@1`.
#[derive(Clone)]
pub struct TasksClient {
    inner: ServiceClient,
}

impl TasksClient {
    pub fn new(caller: Arc<dyn ServiceCaller>) -> Self {
        Self { inner: ServiceClient::new(caller, "editor.tasks@1") }
    }

    pub fn run(&self, kind: &str, payload: Value, timeout_ms: u64) -> Result<Value, PluginError> {
        self.inner
            .call("run", json!({ "kind": kind, "payload": payload, "timeout_ms": timeout_ms }))
    }

    pub fn run_request(&self, request: &TaskRequest) -> Result<Value, PluginError> {
        self.run(&request.task_kind, request.payload.clone(), request.timeout_ms)
    }
}
