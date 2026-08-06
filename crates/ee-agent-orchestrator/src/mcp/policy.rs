//! Side-effect classification of MCP tools (Phase 12).
//!
//! Classification runs on the *original* MCP tool name plus configured
//! metadata — never on sanitized display names — so write/execute tools
//! cannot be laundered into read class by a name sanitizer.  The ee proxy
//! tools have a built-in table mirroring the host's approval semantics;
//! external tools default to the conservative spec: write class with the
//! overwrite destructive subclass, which the default policy denies and any
//! write-allowing policy still gates behind an explicit subclass allowance.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::destructive_policy::SideEffectSubclass;
use crate::tools::SideEffectClass;

use super::names::EE_SERVER_NAME;

/// Default per-request timeout for MCP connect/discover/list/call rounds.
pub(crate) const DEFAULT_MCP_REQUEST_TIMEOUT_MS: u64 = 30_000;

/// One explicit side-effect classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpToolClassSpec {
    /// Side-effect class driving the policy engine.
    pub class: SideEffectClass,
    /// Destructive subclass, when the tool deletes, overwrites, kills, or
    /// touches the network; denied by default policy.
    pub subclass: Option<SideEffectSubclass>,
}

/// MCP tool bridging policy (provider-level knobs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpToolPolicy {
    /// Per-request timeout in milliseconds for MCP connect, discover,
    /// tools/list, and tools/call rounds.
    pub request_timeout_ms: u64,
    /// Exact original-tool-name → classification overrides, consulted before
    /// the built-in ee proxy table and the conservative default.  Keys match
    /// the *original* MCP tool name (e.g. `write.file`), never the sanitized
    /// display name.
    pub classification_overrides: BTreeMap<String, McpToolClassSpec>,
}

impl Default for McpToolPolicy {
    fn default() -> Self {
        Self {
            request_timeout_ms: DEFAULT_MCP_REQUEST_TIMEOUT_MS,
            classification_overrides: BTreeMap::new(),
        }
    }
}

/// The conservative default for unclassified MCP tools.
///
/// Write class plus the overwrite destructive subclass: the default policy
/// denies the call outright, and even a write-allowing policy requires an
/// explicit subclass allowance — an approval gate — before the tool runs.
#[must_use]
pub(crate) fn conservative_default() -> McpToolClassSpec {
    McpToolClassSpec {
        class: SideEffectClass::Write,
        subclass: Some(SideEffectSubclass::Overwrite),
    }
}

/// Classifies one MCP tool from its original name and configured metadata.
#[must_use]
pub(crate) fn classify_tool(
    server_name: &str,
    tool_name: &str,
    policy: &McpToolPolicy,
) -> McpToolClassSpec {
    if let Some(spec) = policy.classification_overrides.get(tool_name) {
        return *spec;
    }
    if server_name == EE_SERVER_NAME
        && let Some(spec) = EE_PROXY_CLASSIFICATIONS
            .iter()
            .find_map(|(name, spec)| (*name == tool_name).then_some(*spec))
    {
        return spec;
    }
    conservative_default()
}

const WRITE: McpToolClassSpec = McpToolClassSpec { class: SideEffectClass::Write, subclass: None };
const WRITE_OVERWRITE: McpToolClassSpec = McpToolClassSpec {
    class: SideEffectClass::Write,
    subclass: Some(SideEffectSubclass::Overwrite),
};
const READ: McpToolClassSpec = McpToolClassSpec { class: SideEffectClass::Read, subclass: None };
const EXECUTE: McpToolClassSpec =
    McpToolClassSpec { class: SideEffectClass::Execute, subclass: None };

/// Built-in classification of the ee proxy's fixed tool list, mirroring the
/// host's approval semantics (reads allowed, mutations and terminal creation
/// gated).
pub(crate) const EE_PROXY_CLASSIFICATIONS: [(&str, McpToolClassSpec); 28] = [
    ("ee_workspace_roots", READ),
    ("ee_list_directory", READ),
    ("ee_list_directory_all", READ),
    ("ee_search_files", READ),
    ("ee_search_files_all", READ),
    ("ee_search_text", READ),
    ("ee_search_text_regex", READ),
    ("ee_search_text_in_files", READ),
    ("ee_read_buffer", READ),
    ("ee_read_buffer_lines", READ),
    ("ee_open_buffers", READ),
    ("ee_get_diagnostics", READ),
    ("ee_get_file_diagnostics", READ),
    ("ee_document_symbols", READ),
    ("ee_references", READ),
    ("ee_list_code_actions", READ),
    ("ee_preview_rename_symbol", READ),
    ("ee_read_text_file", READ),
    ("ee_diagnostics", READ),
    ("ee_replace_text", WRITE_OVERWRITE),
    ("ee_apply_patch", WRITE_OVERWRITE),
    ("ee_create_text_file", WRITE),
    ("ee_overwrite_text_file", WRITE_OVERWRITE),
    ("ee_apply_code_action", WRITE_OVERWRITE),
    ("ee_format_file", WRITE_OVERWRITE),
    ("ee_rename_symbol", WRITE_OVERWRITE),
    ("ee_write_text_file", WRITE_OVERWRITE),
    ("ee_terminal_create", EXECUTE),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> McpToolPolicy {
        McpToolPolicy::default()
    }

    #[test]
    fn every_ee_proxy_tool_is_classified() {
        for (name, spec) in EE_PROXY_CLASSIFICATIONS {
            assert_eq!(classify_tool("ee", name, &policy()), spec, "{name}");
        }
    }

    #[test]
    fn write_tools_keep_overwrite_subclass() {
        let spec = classify_tool("ee", "ee_overwrite_text_file", &policy());
        assert_eq!(spec.class, SideEffectClass::Write);
        assert_eq!(spec.subclass, Some(SideEffectSubclass::Overwrite));
    }

    #[test]
    fn create_tool_has_no_destructive_subclass() {
        let spec = classify_tool("ee", "ee_create_text_file", &policy());
        assert_eq!(spec.class, SideEffectClass::Write);
        assert_eq!(spec.subclass, None);
    }

    #[test]
    fn terminal_create_is_execute() {
        let spec = classify_tool("ee", "ee_terminal_create", &policy());
        assert_eq!(spec.class, SideEffectClass::Execute);
        assert_eq!(spec.subclass, None);
    }

    #[test]
    fn unknown_external_tools_default_to_conservative_write_overwrite() {
        let spec = classify_tool("external", "read.file", &policy());
        assert_eq!(spec, conservative_default());
        let spec = classify_tool("external", "any_tool", &policy());
        assert_eq!(spec, conservative_default());
    }

    #[test]
    fn unknown_ee_tools_also_fall_back_to_conservative() {
        let spec = classify_tool("ee", "ee_some_future_tool", &policy());
        assert_eq!(spec, conservative_default());
    }

    #[test]
    fn overrides_win_over_builtin_table_and_defaults() {
        let mut overridden = policy();
        overridden.classification_overrides.insert(
            "ee_overwrite_text_file".to_string(),
            McpToolClassSpec { class: SideEffectClass::Read, subclass: None },
        );
        overridden.classification_overrides.insert(
            "custom.tool".to_string(),
            McpToolClassSpec {
                class: SideEffectClass::Execute,
                subclass: Some(SideEffectSubclass::TerminalKill),
            },
        );
        assert_eq!(
            classify_tool("ee", "ee_overwrite_text_file", &overridden).class,
            SideEffectClass::Read
        );
        assert_eq!(
            classify_tool("external", "custom.tool", &overridden),
            McpToolClassSpec {
                class: SideEffectClass::Execute,
                subclass: Some(SideEffectSubclass::TerminalKill),
            }
        );
    }

    #[test]
    fn policy_defaults_fail_closed() {
        let policy = McpToolPolicy::default();
        assert!(policy.request_timeout_ms > 0);
        assert!(policy.classification_overrides.is_empty());
    }
}
