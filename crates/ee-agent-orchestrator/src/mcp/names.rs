//! Provider-facing MCP tool name resolution (Phase 12).
//!
//! Some upstream model providers reject dots and other punctuation in
//! function/tool names, so every model-facing MCP tool name must be
//! provider-compatible (`[A-Za-z0-9_]` only).  The ee proxy already exposes
//! `ee_*` names, so those pass through after sanitization; every other
//! server's tools are namespaced as `mcp_<server>_<tool>`.  A reversible
//! display → (server, original) mapping is kept for dispatch, and sanitized
//! name collisions fail closed before any tool is advertised to the model.

use std::collections::HashMap;

/// The wire name of the ee MCP proxy (ACP and stdio fallback).
pub(crate) const EE_SERVER_NAME: &str = "ee";

/// Namespace prefix for tools of external (non-ee) MCP servers.
pub(crate) const EXTERNAL_MCP_TOOL_PREFIX: &str = "mcp_";

/// One resolved model-facing tool name with its reversible dispatch target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedToolName {
    /// Provider-compatible name advertised to the model.
    pub display_name: String,
    /// The server's wire name (dispatch target).
    pub server_id: String,
    /// The original MCP tool name (dispatch target).
    pub original_name: String,
}

/// Whether `name` contains characters some providers reject in tool names.
#[must_use]
pub(crate) fn has_disallowed_character(name: &str) -> bool {
    name.chars().any(|ch| !(ch.is_ascii_alphanumeric() || ch == '_'))
}

/// Replaces every character outside `[A-Za-z0-9_]` with `_`.
fn sanitize_component(component: &str) -> String {
    component
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() || ch == '_' { ch } else { '_' })
        .collect()
}

/// Resolves the provider-facing name for one MCP tool.
///
/// - ee proxy tools keep their `ee_<tool>` names (already provider-friendly).
/// - External tools become `mcp_<server>_<tool>`.
///
/// Returns `None` (fail closed) when any component is empty or sanitizes to
/// an empty string.
#[must_use]
pub(crate) fn resolve_tool_name(server_name: &str, tool_name: &str) -> Option<ResolvedToolName> {
    if tool_name.is_empty() {
        return None;
    }
    if server_name == EE_SERVER_NAME {
        let display_name = sanitize_component(tool_name);
        if display_name.is_empty() {
            return None;
        }
        return Some(ResolvedToolName {
            display_name,
            server_id: EE_SERVER_NAME.to_string(),
            original_name: tool_name.to_string(),
        });
    }
    let server = sanitize_component(server_name);
    let tool = sanitize_component(tool_name);
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some(ResolvedToolName {
        display_name: format!("{EXTERNAL_MCP_TOOL_PREFIX}{server}_{tool}"),
        server_id: server_name.to_string(),
        original_name: tool_name.to_string(),
    })
}

/// Fail-closed display-name allocation across all MCP servers of a session.
///
/// The first tool claiming a display name keeps it; later duplicates are
/// rejected with a diagnostic and never advertised to the model.
#[derive(Debug, Default)]
pub(crate) struct DisplayNameAllocator {
    taken: HashMap<String, ()>,
}

impl DisplayNameAllocator {
    /// Creates an empty allocator.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Reserves `display_name`; fails closed on a duplicate.
    ///
    /// # Errors
    ///
    /// Returns a diagnostic when the name was already claimed.
    pub(crate) fn try_reserve(&mut self, display_name: &str) -> Result<(), String> {
        if self.taken.insert(display_name.to_string(), ()).is_some() {
            return Err(format!(
                "MCP tool name collision after sanitization: {display_name} is already advertised"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ee_proxy_tools_pass_through_with_ee_prefix() {
        let resolved = resolve_tool_name("ee", "ee_workspace_roots").expect("resolves");
        assert_eq!(resolved.display_name, "ee_workspace_roots");
        assert_eq!(resolved.server_id, "ee");
        assert_eq!(resolved.original_name, "ee_workspace_roots");
    }

    #[test]
    fn ee_proxy_tools_sanitize_dots_like_the_legacy_rename() {
        let resolved = resolve_tool_name("ee", "ee.workspace_roots").expect("resolves");
        assert_eq!(resolved.display_name, "ee_workspace_roots");
        assert_eq!(resolved.original_name, "ee.workspace_roots", "dispatch keeps the original");
    }

    #[test]
    fn external_tools_are_namespaced_under_mcp_prefix() {
        let resolved = resolve_tool_name("filesystem", "read.file").expect("resolves");
        assert_eq!(resolved.display_name, "mcp_filesystem_read_file");
        assert_eq!(resolved.server_id, "filesystem");
        assert_eq!(resolved.original_name, "read.file");
    }

    #[test]
    fn external_server_ids_with_dots_are_sanitized() {
        let resolved = resolve_tool_name("my.server", "list").expect("resolves");
        assert_eq!(resolved.display_name, "mcp_my_server_list");
        assert_eq!(resolved.server_id, "my.server", "dispatch keeps the wire server id");
    }

    #[test]
    fn empty_components_fail_closed() {
        assert!(resolve_tool_name("", "tool").is_none());
        assert!(resolve_tool_name("server", "").is_none());
    }

    #[test]
    fn punctuation_only_names_sanitize_to_underscores() {
        // Every disallowed character becomes `_`, so a punctuation-only name
        // is still a valid provider-compatible name (never empty).
        let resolved = resolve_tool_name("...", "tool").expect("sanitizes");
        assert_eq!(resolved.display_name, "mcp_____tool");
        let resolved = resolve_tool_name("server", "!!!").expect("sanitizes");
        assert_eq!(resolved.display_name, "mcp_server____");
    }

    #[test]
    fn every_resolved_name_is_provider_compatible() {
        for server in ["ee", "filesystem", "my.server", "data-tools"] {
            for tool in ["plain", "read.file", "do-thing", "x/y", "a b"] {
                if let Some(resolved) = resolve_tool_name(server, tool) {
                    assert!(
                        !has_disallowed_character(&resolved.display_name),
                        "{} must be provider-compatible",
                        resolved.display_name
                    );
                    assert!(!resolved.display_name.contains('.'));
                }
            }
        }
    }

    #[test]
    fn allocator_rejects_duplicates_fail_closed() {
        let mut allocator = DisplayNameAllocator::new();
        assert!(allocator.try_reserve("mcp_a_tool").is_ok());
        let error = allocator.try_reserve("mcp_a_tool").expect_err("duplicate rejected");
        assert!(error.contains("collision"), "{error}");
        assert!(allocator.try_reserve("mcp_a_other").is_ok());
    }
}
