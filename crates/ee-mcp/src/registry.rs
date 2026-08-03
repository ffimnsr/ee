//! Namespaced primitive registry: `<server_id>/<name>` keys, shape
//! validation, and per-category TTL caching.
//!
//! Wire types come from [`rmcp`]; ee-owned code adds namespacing (server
//! tools/prompts cannot collide), response-shape validation (fail closed
//! before anything reaches the host), and the `ttlMs`/`cacheScope` snapshot
//! cache the manager consults before re-listing.

use std::time::{Duration, Instant};

use rmcp::model::{
    CacheScope, ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult,
    ListToolsResult, Prompt, Resource, ResourceTemplate, Tool,
};

use crate::McpError;

/// Namespaces a primitive name under a server id.
#[must_use]
pub fn namespace(server_id: &str, name: &str) -> String {
    format!("{server_id}/{name}")
}

/// Joins a `prompts/get` result's text content into one plain string.
///
/// Non-text content blocks are skipped; blocks are joined with newlines.
/// This is the host-facing extraction the agents pane uses to insert a
/// selected prompt into the prompt draft.
#[must_use]
pub fn prompt_text(result: &rmcp::model::GetPromptResult) -> String {
    let mut text = String::new();
    for message in &result.messages {
        if let rmcp::model::ContentBlock::Text(text_block) = &message.content {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&text_block.text);
        }
    }
    text
}

/// A tool from a specific server, keyed as `<server_id>/<name>`.
#[derive(Debug, Clone)]
pub struct NamespacedTool {
    /// Registry key (`<server_id>/<name>`).
    pub key: String,
    /// The underlying tool.
    pub tool: Tool,
}

/// A prompt from a specific server, keyed as `<server_id>/<name>`.
#[derive(Debug, Clone)]
pub struct NamespacedPrompt {
    /// Registry key (`<server_id>/<name>`).
    pub key: String,
    /// The underlying prompt.
    pub prompt: Prompt,
}

/// A resource from a specific server.
#[derive(Debug, Clone)]
pub struct NamespacedResource {
    /// Server id.
    pub server_id: String,
    /// The underlying resource.
    pub resource: Resource,
}

/// A resource template from a specific server.
#[derive(Debug, Clone)]
pub struct NamespacedResourceTemplate {
    /// Server id.
    pub server_id: String,
    /// The underlying template.
    pub template: ResourceTemplate,
}

/// Compact summary for host browsing (prompt/resource pickers, Phase 6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrimitiveSummary {
    /// Registry key.
    pub key: String,
    /// Display title (falls back to the primitive name).
    pub title: String,
    /// Optional description.
    pub description: Option<String>,
    /// JSON-schema-shaped argument requirements, when the primitive has them.
    pub arguments_schema: Option<serde_json::Value>,
}

/// One cached primitive category with its server-provided freshness window.
#[derive(Debug, Clone)]
struct CategoryCache<T> {
    entries: Vec<T>,
    fetched_at: Instant,
    ttl_ms: u64,
    cache_scope: CacheScope,
}

impl<T> CategoryCache<T> {
    fn store(&mut self, entries: Vec<T>, ttl_ms: u64, cache_scope: CacheScope) {
        self.entries = entries;
        self.fetched_at = Instant::now();
        self.ttl_ms = ttl_ms;
        self.cache_scope = cache_scope;
    }

    fn is_fresh(&self) -> bool {
        // A zero `ttlMs` means the server asked for no caching.
        self.ttl_ms > 0 && self.fetched_at.elapsed() < Duration::from_millis(self.ttl_ms)
    }
}

/// Per-server primitive registry.
#[derive(Debug, Default, Clone)]
pub struct PrimitiveRegistry {
    server_id: String,
    tools: Option<CategoryCache<NamespacedTool>>,
    prompts: Option<CategoryCache<NamespacedPrompt>>,
    resources: Option<CategoryCache<NamespacedResource>>,
    resource_templates: Option<CategoryCache<NamespacedResourceTemplate>>,
}

impl PrimitiveRegistry {
    /// Creates an empty registry for `server_id`.
    #[must_use]
    pub fn new(server_id: impl Into<String>) -> Self {
        Self { server_id: server_id.into(), ..Default::default() }
    }

    /// The server id this registry belongs to.
    #[must_use]
    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    /// Stores a fresh `tools/list` result.
    pub fn store_tools(&mut self, result: &ListToolsResult) -> Result<(), McpError> {
        let entries = result
            .tools
            .iter()
            .map(|tool| {
                validate_tool(&self.server_id, tool)?;
                Ok(NamespacedTool {
                    key: namespace(&self.server_id, &tool.name),
                    tool: tool.clone(),
                })
            })
            .collect::<Result<Vec<_>, McpError>>()?;
        let cache = self.tools.get_or_insert_with(|| CategoryCache {
            entries: Vec::new(),
            fetched_at: Instant::now(),
            ttl_ms: 0,
            cache_scope: CacheScope::Private,
        });
        cache.store(
            entries,
            result.ttl_ms.unwrap_or(0),
            result.cache_scope.unwrap_or(CacheScope::Private),
        );
        Ok(())
    }

    /// Stored tools, if present.
    #[must_use]
    pub fn tools(&self) -> &[NamespacedTool] {
        self.tools.as_ref().map(|cache| cache.entries.as_slice()).unwrap_or(&[])
    }

    /// Whether the stored tools are within their TTL.
    #[must_use]
    pub fn tools_fresh(&self) -> bool {
        self.tools.as_ref().is_some_and(CategoryCache::is_fresh)
    }

    /// Invalidates the tools cache (list-changed notification).
    pub fn invalidate_tools(&mut self) {
        self.tools = None;
    }

    /// Stores a fresh `prompts/list` result.
    pub fn store_prompts(&mut self, result: &ListPromptsResult) -> Result<(), McpError> {
        let entries = result
            .prompts
            .iter()
            .map(|prompt| {
                validate_prompt(&self.server_id, prompt)?;
                Ok(NamespacedPrompt {
                    key: namespace(&self.server_id, &prompt.name),
                    prompt: prompt.clone(),
                })
            })
            .collect::<Result<Vec<_>, McpError>>()?;
        let cache = self.prompts.get_or_insert_with(|| CategoryCache {
            entries: Vec::new(),
            fetched_at: Instant::now(),
            ttl_ms: 0,
            cache_scope: CacheScope::Private,
        });
        cache.store(
            entries,
            result.ttl_ms.unwrap_or(0),
            result.cache_scope.unwrap_or(CacheScope::Private),
        );
        Ok(())
    }

    /// Stored prompts, if present.
    #[must_use]
    pub fn prompts(&self) -> &[NamespacedPrompt] {
        self.prompts.as_ref().map(|cache| cache.entries.as_slice()).unwrap_or(&[])
    }

    /// Whether the stored prompts are within their TTL.
    #[must_use]
    pub fn prompts_fresh(&self) -> bool {
        self.prompts.as_ref().is_some_and(CategoryCache::is_fresh)
    }

    /// Invalidates the prompts cache (list-changed notification).
    pub fn invalidate_prompts(&mut self) {
        self.prompts = None;
    }

    /// Stores a fresh `resources/list` result.
    pub fn store_resources(&mut self, result: &ListResourcesResult) -> Result<(), McpError> {
        let entries = result
            .resources
            .iter()
            .map(|resource| {
                validate_resource(&self.server_id, resource)?;
                Ok(NamespacedResource {
                    server_id: self.server_id.clone(),
                    resource: resource.clone(),
                })
            })
            .collect::<Result<Vec<_>, McpError>>()?;
        let cache = self.resources.get_or_insert_with(|| CategoryCache {
            entries: Vec::new(),
            fetched_at: Instant::now(),
            ttl_ms: 0,
            cache_scope: CacheScope::Private,
        });
        cache.store(
            entries,
            result.ttl_ms.unwrap_or(0),
            result.cache_scope.unwrap_or(CacheScope::Private),
        );
        Ok(())
    }

    /// Stored resources, if present.
    #[must_use]
    pub fn resources(&self) -> &[NamespacedResource] {
        self.resources.as_ref().map(|cache| cache.entries.as_slice()).unwrap_or(&[])
    }

    /// Whether the stored resources are within their TTL.
    #[must_use]
    pub fn resources_fresh(&self) -> bool {
        self.resources.as_ref().is_some_and(CategoryCache::is_fresh)
    }

    /// Invalidates the resources cache (list-changed notification).
    pub fn invalidate_resources(&mut self) {
        self.resources = None;
    }

    /// Stores a fresh `resources/templates/list` result.
    pub fn store_resource_templates(
        &mut self,
        result: &ListResourceTemplatesResult,
    ) -> Result<(), McpError> {
        let entries = result
            .resource_templates
            .iter()
            .map(|template| {
                validate_resource_template(&self.server_id, template)?;
                Ok(NamespacedResourceTemplate {
                    server_id: self.server_id.clone(),
                    template: template.clone(),
                })
            })
            .collect::<Result<Vec<_>, McpError>>()?;
        let cache = self.resource_templates.get_or_insert_with(|| CategoryCache {
            entries: Vec::new(),
            fetched_at: Instant::now(),
            ttl_ms: 0,
            cache_scope: CacheScope::Private,
        });
        cache.store(
            entries,
            result.ttl_ms.unwrap_or(0),
            result.cache_scope.unwrap_or(CacheScope::Private),
        );
        Ok(())
    }

    /// Stored resource templates, if present.
    #[must_use]
    pub fn resource_templates(&self) -> &[NamespacedResourceTemplate] {
        self.resource_templates.as_ref().map(|cache| cache.entries.as_slice()).unwrap_or(&[])
    }

    /// Whether the stored templates are within their TTL.
    #[must_use]
    pub fn resource_templates_fresh(&self) -> bool {
        self.resource_templates.as_ref().is_some_and(CategoryCache::is_fresh)
    }

    /// Invalidates the resource-template cache (list-changed notification).
    pub fn invalidate_resource_templates(&mut self) {
        self.resource_templates = None;
    }

    /// Invalidates every cached category (discovery refresh / reconnect).
    pub fn invalidate_all(&mut self) {
        self.tools = None;
        self.prompts = None;
        self.resources = None;
        self.resource_templates = None;
    }
}

/// Validates one tool shape; rejects empty names and namespaces.
fn validate_tool(server_id: &str, tool: &Tool) -> Result<(), McpError> {
    if server_id.trim().is_empty() {
        return Err(McpError::InvalidPrimitiveResult("server id must not be empty".into()));
    }
    if tool.name.trim().is_empty() {
        return Err(McpError::InvalidPrimitiveResult(format!(
            "server {server_id} returned a tool with an empty name"
        )));
    }
    Ok(())
}

/// Validates one prompt shape.
fn validate_prompt(server_id: &str, prompt: &Prompt) -> Result<(), McpError> {
    if server_id.trim().is_empty() {
        return Err(McpError::InvalidPrimitiveResult("server id must not be empty".into()));
    }
    if prompt.name.trim().is_empty() {
        return Err(McpError::InvalidPrimitiveResult(format!(
            "server {server_id} returned a prompt with an empty name"
        )));
    }
    Ok(())
}

/// Validates one resource shape (URI required).
fn validate_resource(server_id: &str, resource: &Resource) -> Result<(), McpError> {
    if resource.uri.trim().is_empty() {
        return Err(McpError::InvalidPrimitiveResult(format!(
            "server {server_id} returned a resource with an empty uri"
        )));
    }
    Ok(())
}

/// Validates one resource template shape (URI template required).
fn validate_resource_template(
    server_id: &str,
    template: &ResourceTemplate,
) -> Result<(), McpError> {
    if template.uri_template.trim().is_empty() {
        return Err(McpError::InvalidPrimitiveResult(format!(
            "server {server_id} returned a resource template with an empty uriTemplate"
        )));
    }
    Ok(())
}

impl NamespacedTool {
    /// Compact summary for host browsing.
    #[must_use]
    pub fn summary(&self) -> PrimitiveSummary {
        PrimitiveSummary {
            key: self.key.clone(),
            title: self.tool.title.clone().unwrap_or_else(|| self.tool.name.to_string()),
            description: self.tool.description.as_deref().map(ToOwned::to_owned),
            arguments_schema: Some(serde_json::Value::Object((*self.tool.input_schema).clone())),
        }
    }
}

impl NamespacedPrompt {
    /// Compact summary for host browsing.
    #[must_use]
    pub fn summary(&self) -> PrimitiveSummary {
        PrimitiveSummary {
            key: self.key.clone(),
            title: self.prompt.name.to_string(),
            description: self.prompt.description.as_deref().map(ToOwned::to_owned),
            arguments_schema: self.prompt.arguments.as_ref().map(|args| {
                serde_json::json!({
                    "type": "object",
                    "properties": args.iter().map(|arg| {
                        let required = arg.required.unwrap_or(false);
                        (arg.name.clone(), serde_json::json!({ "required": required }))
                    }).collect::<serde_json::Map<_, _>>(),
                })
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::ListToolsResult;

    fn sample_tool(name: &str) -> Tool {
        Tool::new(
            name.to_owned(),
            "",
            serde_json::json!({ "type": "object" }).as_object().unwrap().clone(),
        )
    }

    fn tools_result(tools: Vec<Tool>) -> ListToolsResult {
        ListToolsResult {
            tools,
            next_cursor: None,
            ttl_ms: Some(10_000),
            cache_scope: Some(CacheScope::Private),
            meta: None,
            result_type: Some(rmcp::model::ResultType::COMPLETE),
        }
    }

    #[test]
    fn namespacing_prevents_collisions() {
        assert_eq!(namespace("alpha", "read"), "alpha/read");
        assert_eq!(namespace("beta", "read"), "beta/read");
    }

    #[test]
    fn tools_are_namespaced_and_validated() {
        let mut registry = PrimitiveRegistry::new("srv");
        registry
            .store_tools(&tools_result(vec![sample_tool("read"), sample_tool("write")]))
            .expect("valid");
        assert_eq!(registry.tools().len(), 2);
        assert_eq!(registry.tools()[0].key, "srv/read");
        assert!(registry.tools_fresh());
    }

    #[test]
    fn empty_tool_names_are_rejected() {
        let mut registry = PrimitiveRegistry::new("srv");
        let error = registry
            .store_tools(&tools_result(vec![sample_tool("")]))
            .expect_err("empty name rejected");
        assert!(matches!(error, McpError::InvalidPrimitiveResult(_)));
    }

    #[test]
    fn ttl_expiry_marks_category_stale() {
        let mut registry = PrimitiveRegistry::new("srv");
        let mut result = tools_result(vec![sample_tool("read")]);
        result.ttl_ms = Some(0); // no caching requested
        registry.store_tools(&result).expect("valid");
        assert!(!registry.tools_fresh(), "zero ttl means always stale");
        registry.invalidate_tools();
        assert!(registry.tools().is_empty());
    }
}
