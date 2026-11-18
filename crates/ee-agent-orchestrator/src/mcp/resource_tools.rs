//! Model-facing MCP resource and event tools.
//!
//! These tools sit beside the discovered MCP tool surface and use the same
//! per-prompt [`McpSessionManager`] connections. They are read-class: listing,
//! reading, and observing server notifications never mutate the workspace.
//!
//! `watch_mcp_event` opens `subscriptions/listen`, drains at most a bounded
//! number of notifications with a bounded wait, and closes the subscription
//! when it returns. There is no background queue: the tool returns what it saw
//! and the model can call it again.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{ReadResourceRequestParams, SubscriptionFilter};
use serde_json::{Value, json};

use crate::tools::{
    ServerTool, SideEffectClass, ToolDefinition, ToolErrorKind, ToolFuture, ToolResult,
};

use super::manager::McpSessionManager;

/// Maximum resource entries returned by one listing.
const MAX_RESOURCE_ENTRIES: usize = 200;
/// Maximum serialized bytes for one resource read.
const MAX_RESOURCE_RESULT_BYTES: usize = 64 * 1024;
/// Maximum serialized bytes for one notification.
const MAX_EVENT_BYTES: usize = 16 * 1024;
/// Maximum notifications drained by one watch call.
const MAX_WATCH_EVENTS: usize = 16;
/// Default notifications drained by one watch call.
const DEFAULT_WATCH_EVENTS: usize = 1;
/// Default watch wait.
const DEFAULT_WATCH_TIMEOUT_MS: u64 = 5_000;
/// Maximum watch wait.
const MAX_WATCH_TIMEOUT_MS: u64 = 30_000;

impl McpSessionManager {
    /// Lists resources and resource templates across `server` (or every
    /// configured server when `None`).
    pub(crate) async fn list_mcp_resources(&self, server: Option<&str>) -> Result<Value, String> {
        let mut resources = Vec::new();
        let mut templates = Vec::new();
        let mut errors = Vec::new();
        let mut truncated = false;
        for connection in &self.connections {
            if server.is_some_and(|name| name != connection.descriptor.name) {
                continue;
            }
            let guard = connection.running.lock().await;
            let Some(running) = guard.as_ref() else {
                continue;
            };
            match tokio::time::timeout(self.timeout, running.list_all_resources()).await {
                Ok(Ok(entries)) => {
                    for entry in entries {
                        if resources.len() + templates.len() >= MAX_RESOURCE_ENTRIES {
                            truncated = true;
                            break;
                        }
                        let mut value = serde_json::to_value(&entry)
                            .map_err(|error| format!("invalid resource payload: {error}"))?;
                        if let Value::Object(map) = &mut value {
                            map.insert(
                                String::from("server"),
                                Value::String(connection.descriptor.name.clone()),
                            );
                        }
                        resources.push(value);
                    }
                }
                Ok(Err(error)) => errors.push(json!({
                    "server": connection.descriptor.name,
                    "message": error.to_string(),
                })),
                Err(_) => errors.push(json!({
                    "server": connection.descriptor.name,
                    "message": "resources/list timed out",
                })),
            }
            if resources.len() + templates.len() >= MAX_RESOURCE_ENTRIES {
                truncated = true;
                continue;
            }
            match tokio::time::timeout(self.timeout, running.list_all_resource_templates()).await {
                Ok(Ok(entries)) => {
                    for entry in entries {
                        if resources.len() + templates.len() >= MAX_RESOURCE_ENTRIES {
                            truncated = true;
                            break;
                        }
                        let mut value = serde_json::to_value(&entry).map_err(|error| {
                            format!("invalid resource template payload: {error}")
                        })?;
                        if let Value::Object(map) = &mut value {
                            map.insert(
                                String::from("server"),
                                Value::String(connection.descriptor.name.clone()),
                            );
                        }
                        templates.push(value);
                    }
                }
                Ok(Err(error)) => errors.push(json!({
                    "server": connection.descriptor.name,
                    "message": error.to_string(),
                })),
                Err(_) => errors.push(json!({
                    "server": connection.descriptor.name,
                    "message": "resources/templates/list timed out",
                })),
            }
        }
        Ok(json!({
            "resources": resources,
            "templates": templates,
            "errors": errors,
            "truncated": truncated,
        }))
    }

    /// Reads one resource by server and URI.
    pub(crate) async fn read_mcp_resource(&self, server: &str, uri: &str) -> Result<Value, String> {
        let connection = self
            .connections
            .iter()
            .find(|connection| connection.descriptor.name == server)
            .ok_or_else(|| format!("unknown MCP server {server:?}"))?;
        let guard = connection.running.lock().await;
        let running =
            guard.as_ref().ok_or_else(|| format!("MCP server {server:?} is not connected"))?;
        let params = ReadResourceRequestParams::new(uri);
        let result = tokio::time::timeout(self.timeout, running.read_resource(params))
            .await
            .map_err(|_| format!("resources/read for {server:?} timed out"))?
            .map_err(|error| format!("resources/read for {server:?} failed: {error}"))?;
        let value = serde_json::to_value(&result)
            .map_err(|error| format!("invalid resource payload from {server:?}: {error}"))?;
        Ok(bound_json(value, MAX_RESOURCE_RESULT_BYTES))
    }

    /// Opens a bounded notification subscription and drains at most
    /// `max_events` notifications within `timeout`.
    pub(crate) async fn watch_mcp_event(
        &self,
        server: &str,
        uri: Option<&str>,
        max_events: usize,
        timeout: Duration,
    ) -> Result<Value, String> {
        let connection = self
            .connections
            .iter()
            .find(|connection| connection.descriptor.name == server)
            .ok_or_else(|| format!("unknown MCP server {server:?}"))?;
        let filter = match uri {
            Some(uri) => SubscriptionFilter::builder().resource_subscription(uri).build(),
            None => SubscriptionFilter::builder()
                .tools_list_changed()
                .prompts_list_changed()
                .resources_list_changed()
                .build(),
        };
        let capacity = NonZeroUsize::new(MAX_WATCH_EVENTS).expect("non-zero watch capacity");
        let mut subscription = {
            let guard = connection.running.lock().await;
            let running =
                guard.as_ref().ok_or_else(|| format!("MCP server {server:?} is not connected"))?;
            running
                .listen_with_capacity(filter, capacity)
                .await
                .map_err(|error| format!("subscriptions/listen for {server:?} failed: {error}"))?
        };
        let mut events = Vec::new();
        let deadline = tokio::time::Instant::now() + timeout;
        while events.len() < max_events {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, subscription.next()).await {
                Ok(Ok(Some(notification))) => {
                    let value = serde_json::to_value(&notification)
                        .map_err(|error| format!("invalid notification payload: {error}"))?;
                    events.push(bound_json(value, MAX_EVENT_BYTES));
                }
                Ok(Ok(None)) => break,
                Ok(Err(error)) => {
                    return Err(format!("MCP subscription for {server:?} failed: {error}"));
                }
                Err(_) => break,
            }
        }
        Ok(json!({
            "server": server,
            "events": events,
            "timed_out": events.is_empty(),
        }))
    }
}

/// Which MCP resource operation a tool implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceToolKind {
    List,
    Read,
    Watch,
}

/// One model-facing MCP resource/event tool.
pub(crate) struct McpResourceTool {
    kind: ResourceToolKind,
    manager: Arc<McpSessionManager>,
}

impl McpResourceTool {
    fn new(kind: ResourceToolKind, manager: Arc<McpSessionManager>) -> Self {
        Self { kind, manager }
    }

    fn definition_for(kind: ResourceToolKind) -> ToolDefinition {
        let definition = match kind {
            ResourceToolKind::List => ToolDefinition::new(
                "list_mcp_resources",
                "List documents, records, and other data exposed by MCP servers. Use it when the user refers to something that is not a local file, then load a match with read_mcp_resource. Omit server to list every configured server.",
            )
            .input_schema(json!({
                "type": "object",
                "properties": { "server": { "type": "string" } },
                "additionalProperties": false,
            })),
            ResourceToolKind::Read => ToolDefinition::new(
                "read_mcp_resource",
                "Read one MCP resource by server and URI. Not for local files. Oversized results are truncated with explicit metadata; prefer a narrower resource when that happens.",
            )
            .input_schema(json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string" },
                    "uri": { "type": "string" },
                },
                "required": ["server", "uri"],
                "additionalProperties": false,
            })),
            ResourceToolKind::Watch => ToolDefinition::new(
                "watch_mcp_event",
                "Wait for bounded MCP notifications from one server instead of polling. Provide uri to watch one resource, or omit it to watch list-changed notifications. Returns at most max_events (default 1) within timeout_ms (default 5000). No background queue is kept.",
            )
            .input_schema(json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string" },
                    "uri": { "type": "string" },
                    "max_events": { "type": "integer", "minimum": 1 },
                    "timeout_ms": { "type": "integer", "minimum": 100 },
                },
                "required": ["server"],
                "additionalProperties": false,
            })),
        };
        definition.side_effect_class(SideEffectClass::Read)
    }
}

impl ServerTool for McpResourceTool {
    fn definition(&self) -> ToolDefinition {
        Self::definition_for(self.kind)
    }

    fn execute(
        &self,
        arguments: serde_json::Value,
        _client: ee_agent_acp_server::ClientBridge,
        _cancel: tokio::sync::watch::Receiver<bool>,
        _context: crate::tools::ToolCallContext,
    ) -> ToolFuture<ToolResult> {
        let kind = self.kind;
        let manager = self.manager.clone();
        Box::pin(async move {
            let outcome = match kind {
                ResourceToolKind::List => {
                    let server = arguments.get("server").and_then(serde_json::Value::as_str);
                    manager.list_mcp_resources(server).await.map(|value| {
                        let count = value
                            .get("resources")
                            .and_then(serde_json::Value::as_array)
                            .map_or(0, Vec::len);
                        (format!("listed {count} MCP resource(s)"), value)
                    })
                }
                ResourceToolKind::Read => {
                    let server = match required_str(&arguments, "server") {
                        Ok(value) => value,
                        Err(reason) => {
                            return ToolResult::failure(ToolErrorKind::InvalidArguments, reason);
                        }
                    };
                    let uri = match required_str(&arguments, "uri") {
                        Ok(value) => value,
                        Err(reason) => {
                            return ToolResult::failure(ToolErrorKind::InvalidArguments, reason);
                        }
                    };
                    manager.read_mcp_resource(server, uri).await.map(|value| {
                        let truncated = value
                            .get("truncated")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false);
                        let summary = if truncated {
                            format!("read MCP resource {server}:{uri} (truncated)")
                        } else {
                            format!("read MCP resource {server}:{uri}")
                        };
                        (summary, value)
                    })
                }
                ResourceToolKind::Watch => {
                    let server = match required_str(&arguments, "server") {
                        Ok(value) => value,
                        Err(reason) => {
                            return ToolResult::failure(ToolErrorKind::InvalidArguments, reason);
                        }
                    };
                    let uri = arguments.get("uri").and_then(serde_json::Value::as_str);
                    let max_events = arguments
                        .get("max_events")
                        .and_then(serde_json::Value::as_u64)
                        .map_or(DEFAULT_WATCH_EVENTS, |value| {
                            usize::try_from(value)
                                .unwrap_or(MAX_WATCH_EVENTS)
                                .clamp(1, MAX_WATCH_EVENTS)
                        });
                    let timeout_ms = arguments
                        .get("timeout_ms")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(DEFAULT_WATCH_TIMEOUT_MS)
                        .clamp(100, MAX_WATCH_TIMEOUT_MS);
                    manager
                        .watch_mcp_event(server, uri, max_events, Duration::from_millis(timeout_ms))
                        .await
                        .map(|value| {
                            let count = value
                                .get("events")
                                .and_then(serde_json::Value::as_array)
                                .map_or(0, Vec::len);
                            (format!("watched {server}: {count} MCP event(s)"), value)
                        })
                }
            };
            match outcome {
                Ok((summary, value)) => ToolResult::success_structured(summary, value),
                Err(reason) => ToolResult::failure(ToolErrorKind::Backend, reason),
            }
        })
    }
}

fn required_str<'a>(arguments: &'a serde_json::Value, name: &str) -> Result<&'a str, String> {
    arguments
        .get(name)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("{name} must be a non-empty string"))
}

/// Returns `value` unchanged when its serialization fits `cap`; otherwise
/// returns a bounded preview with explicit truncation metadata.
fn bound_json(value: Value, cap: usize) -> Value {
    match serde_json::to_string(&value) {
        Ok(text) if text.len() <= cap => value,
        Ok(text) => {
            let mut end = cap.min(text.len());
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            json!({
                "truncated": true,
                "original_bytes": text.len(),
                "preview": &text[..end],
            })
        }
        Err(_) => json!({
            "truncated": true,
            "original_bytes": 0,
            "preview": "<unserializable MCP payload>",
        }),
    }
}

/// Model-facing MCP resource/event tools bound to one prompt's connections.
pub(crate) fn resource_tools(manager: Arc<McpSessionManager>) -> Vec<Arc<dyn ServerTool>> {
    [ResourceToolKind::List, ResourceToolKind::Read, ResourceToolKind::Watch]
        .into_iter()
        .map(|kind| Arc::new(McpResourceTool::new(kind, manager.clone())) as Arc<dyn ServerTool>)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definitions_are_read_class_and_bounded() {
        for kind in [ResourceToolKind::List, ResourceToolKind::Read, ResourceToolKind::Watch] {
            let definition = McpResourceTool::definition_for(kind);
            assert_eq!(definition.side_effect_class, SideEffectClass::Read);
            assert!(!definition.name.is_empty());
        }
    }

    #[test]
    fn bound_json_truncates_oversized_payloads_at_char_boundaries() {
        let value = json!({ "text": "é".repeat(100) });
        let bounded = bound_json(value, 32);
        assert_eq!(bounded["truncated"], json!(true));
        let preview = bounded["preview"].as_str().expect("preview");
        assert!(preview.len() <= 32);
    }

    #[test]
    fn required_str_rejects_missing_and_blank_values() {
        assert!(required_str(&json!({}), "server").is_err());
        assert!(required_str(&json!({ "server": "  " }), "server").is_err());
        assert_eq!(required_str(&json!({ "server": "s" }), "server").unwrap(), "s");
    }
}
