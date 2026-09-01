//! Versioned governance data for stable `ee_` proxy tools.
//!
//! This module is sole owner of tool-policy metadata. Tool schemas remain next
//! to their dispatch implementations in [`crate::proxy`], while discovery,
//! classification, manifest output, transport filtering, and compatibility
//! checks all read these records. Incompatible changes require a new tool name.

use crate::classify::SideEffectClass;

/// Current version of every stable ee proxy schema contract.
///
/// Bump only when adding a compatible manifest field or tool. An incompatible
/// argument or result change must use a new tool name instead.
pub const EE_TOOL_SCHEMA_VERSION: u64 = 6;

/// MCP routes over which one tool is implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolTransport {
    Stdio,
    Acp,
}

impl ToolTransport {
    /// Stable manifest spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Acp => "acp",
        }
    }
}

/// Stable policy metadata for a single `ee_` tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolGovernance {
    pub side_effect: SideEffectClass,
    pub approval: &'static str,
    pub transports: &'static [ToolTransport],
    pub required_capabilities: &'static [&'static str],
    pub output_cap_kind: &'static str,
    pub output_cap: u64,
    pub redaction_rules: &'static [&'static str],
    pub error_classes: &'static [&'static str],
    pub deprecated: bool,
    pub replacement: Option<&'static str>,
}

const ALL_TRANSPORTS: &[ToolTransport] = &[ToolTransport::Stdio, ToolTransport::Acp];
const ACP_ONLY: &[ToolTransport] = &[ToolTransport::Acp];
const NO_CAPABILITIES: &[&str] = &[];
const LSP_CAPABILITY: &[&str] = &["language_server"];
const TERMINAL_CAPABILITY: &[&str] = &["terminal"];
const GIT_CAPABILITY: &[&str] = &["git"];
const DEPENDENCY_INDEX_CAPABILITY: &[&str] = &["dependency_index"];
/// Web retrieval requires host web support and explicit outbound-network policy.
const WEB_CONTEXT_CAPABILITY: &[&str] = &["web_context", "ExternalNetwork"];

/// Exact stable failures returned by remote web-context tools.
pub const WEB_CONTEXT_ERROR_CLASSES: &[&str] = &[
    "web_disabled",
    "web_search_unavailable",
    "network_approval_required",
    "url_rejected",
    "dns_rejected",
    "redirect_rejected",
    "unsupported_content_type",
    "response_too_large",
    "network_timeout",
    "network_failure",
];

const STANDARD_ERRORS: &[&str] = &[
    "invalid_arguments",
    "unsupported_tool",
    "permission_denied",
    "backend_failure",
    "output_truncated",
];
const DEPENDENCY_INDEX_ERRORS: &[&str] = &[
    "invalid_arguments",
    "unsupported_tool",
    "permission_denied",
    "dependency_index_unavailable",
    "dependency_index_stale",
    "backend_failure",
    "output_truncated",
];
const WRITE_ERRORS: &[&str] = &[
    "invalid_arguments",
    "unsupported_tool",
    "permission_denied",
    "stale_revision",
    "backend_failure",
];
const TERMINAL_ERRORS: &[&str] = &[
    "invalid_arguments",
    "unsupported_tool",
    "permission_denied",
    "terminal_not_owned",
    "backend_failure",
    "output_truncated",
];
const DEFAULT_REDACTION: &[&str] = &["secret_like_values", "sensitive_diagnostics"];
const TERMINAL_REDACTION: &[&str] =
    &["secret_like_environment_values", "secret_like_environment_keys", "sensitive_diagnostics"];
const TURN_EVIDENCE_REDACTION: &[&str] = &[
    "host_owned_evidence_summary_only",
    "no_transcript",
    "no_raw_paths",
    "no_prompts",
    "no_terminal_output",
    "secret_like_values",
];
const TURN_EVIDENCE_ERRORS: &[&str] =
    &["invalid_arguments", "unsupported_tool", "evidence_unavailable", "backend_failure"];
/// Remote responses are data, never instructions. The session-local cache retains
/// normalized public fields only and expires entries after 60 seconds.
const WEB_CONTEXT_REDACTION: &[&str] = &[
    "secret_like_values",
    "sensitive_diagnostics",
    "external_content_untrusted",
    "external_content_not_instructions",
    "session_local_cache_normalized_public_fields_only_60_seconds",
];

const fn read(
    capabilities: &'static [&'static str],
    cap_kind: &'static str,
    cap: u64,
) -> ToolGovernance {
    ToolGovernance {
        side_effect: SideEffectClass::Read,
        approval: "none",
        transports: ALL_TRANSPORTS,
        required_capabilities: capabilities,
        output_cap_kind: cap_kind,
        output_cap: cap,
        redaction_rules: DEFAULT_REDACTION,
        error_classes: STANDARD_ERRORS,
        deprecated: false,
        replacement: None,
    }
}

const fn web_context_read(cap_kind: &'static str, cap: u64) -> ToolGovernance {
    ToolGovernance {
        side_effect: SideEffectClass::Read,
        // Remote reads disclose agent input to an external service and always
        // require host approval through the external-network policy path.
        approval: "required",
        transports: ALL_TRANSPORTS,
        required_capabilities: WEB_CONTEXT_CAPABILITY,
        output_cap_kind: cap_kind,
        output_cap: cap,
        redaction_rules: WEB_CONTEXT_REDACTION,
        error_classes: WEB_CONTEXT_ERROR_CLASSES,
        deprecated: false,
        replacement: None,
    }
}

const fn turn_evidence_read() -> ToolGovernance {
    ToolGovernance {
        side_effect: SideEffectClass::Read,
        approval: "none",
        transports: ACP_ONLY,
        required_capabilities: NO_CAPABILITIES,
        output_cap_kind: "evidence_ids",
        output_cap: 128,
        redaction_rules: TURN_EVIDENCE_REDACTION,
        error_classes: TURN_EVIDENCE_ERRORS,
        deprecated: false,
        replacement: None,
    }
}

const fn dependency_index_read() -> ToolGovernance {
    ToolGovernance {
        side_effect: SideEffectClass::Read,
        approval: "none",
        transports: ALL_TRANSPORTS,
        required_capabilities: DEPENDENCY_INDEX_CAPABILITY,
        output_cap_kind: "result_items",
        output_cap: 500,
        redaction_rules: DEFAULT_REDACTION,
        error_classes: DEPENDENCY_INDEX_ERRORS,
        deprecated: false,
        replacement: None,
    }
}

const fn write() -> ToolGovernance {
    ToolGovernance {
        side_effect: SideEffectClass::Write,
        approval: "required",
        transports: ALL_TRANSPORTS,
        required_capabilities: NO_CAPABILITIES,
        output_cap_kind: "result_items",
        output_cap: 500,
        redaction_rules: DEFAULT_REDACTION,
        error_classes: WRITE_ERRORS,
        deprecated: false,
        replacement: None,
    }
}

const fn execute(transports: &'static [ToolTransport]) -> ToolGovernance {
    ToolGovernance {
        side_effect: SideEffectClass::Execute,
        approval: "required",
        transports,
        required_capabilities: TERMINAL_CAPABILITY,
        output_cap_kind: "bytes",
        output_cap: 1024 * 1024,
        redaction_rules: TERMINAL_REDACTION,
        error_classes: TERMINAL_ERRORS,
        deprecated: false,
        replacement: None,
    }
}

/// Returns governance for a stable proxy tool. Unknown names return `None` and
/// must not be advertised, trusted, or dispatched as ee tools.
#[must_use]
pub fn governance(tool: &str) -> Option<ToolGovernance> {
    let entry = match tool {
        "ee_workspace_roots"
        | "ee_open_buffers"
        | "ee_changed_files"
        | "ee_review_context"
        | "ee_project_instructions"
        | "ee_read_notes"
        | "ee_tools_manifest"
        | "ee_diagnostics" => read(NO_CAPABILITIES, "result_items", 500),
        "ee_recall_workspace_facts" => read(NO_CAPABILITIES, "result_items", 8),
        "ee_read_workspace_fact" => read(NO_CAPABILITIES, "result_items", 1),
        "ee_list_workspace_facts" => read(NO_CAPABILITIES, "result_items", 256),
        "ee_export_workspace_memory" => {
            let mut entry = read(NO_CAPABILITIES, "bytes", 512 * 1024);
            entry.approval = "required";
            entry
        }
        "ee_list_directory"
        | "ee_list_directory_all"
        | "ee_search_files"
        | "ee_search_files_all"
        | "ee_read_note" => read(NO_CAPABILITIES, "result_items", 500),
        "ee_search_text" | "ee_search_text_regex" | "ee_search_text_in_files" => {
            read(NO_CAPABILITIES, "result_items", 200)
        }
        "ee_web_search"
        | "ee_fetch_url"
        | "ee_browser_run_content"
        | "ee_browser_run_screenshot"
        | "ee_browser_run_markdown"
        | "ee_browser_run_scrape"
        | "ee_browser_run_json"
        | "ee_browser_run_links" => web_context_read("response_bytes", 1024 * 1024),
        "ee_read_buffer" | "ee_read_text_file" => read(NO_CAPABILITIES, "bytes", 1024 * 1024),
        "ee_read_buffer_lines" => read(NO_CAPABILITIES, "result_items", 500),
        "ee_get_diagnostics"
        | "ee_get_file_diagnostics"
        | "ee_document_symbols"
        | "ee_references"
        | "ee_list_code_actions"
        | "ee_preview_rename_symbol" => read(LSP_CAPABILITY, "result_items", 500),
        "ee_git_status" => read(GIT_CAPABILITY, "result_items", 500),
        "ee_git_diff" | "ee_git_diff_staged" | "ee_git_diff_file" => {
            read(GIT_CAPABILITY, "bytes", 256 * 1024)
        }
        "ee_file_dependency_map" => read(DEPENDENCY_INDEX_CAPABILITY, "result_items", 500),
        "ee_symbol_dependency_map" => dependency_index_read(),
        "ee_turn_evidence_summary" => turn_evidence_read(),
        "ee_terminal_output"
        | "ee_terminal_output_since"
        | "ee_terminal_wait"
        | "ee_terminal_wait_long" => {
            let mut entry = execute(ACP_ONLY);
            entry.side_effect = SideEffectClass::Read;
            entry.approval = "none";
            entry
        }
        "ee_replace_text"
        | "ee_apply_patch"
        | "ee_create_text_file"
        | "ee_overwrite_text_file"
        | "ee_create_directory"
        | "ee_delete_path"
        | "ee_copy_path"
        | "ee_move_path"
        | "ee_apply_code_action"
        | "ee_format_file"
        | "ee_rename_symbol"
        | "ee_write_text_file"
        | "ee_save_note" => write(),
        "ee_remember_workspace_fact"
        | "ee_forget_workspace_fact"
        | "ee_retract_workspace_fact"
        | "ee_import_workspace_memory"
        | "ee_clear_workspace_memory" => {
            let mut entry = write();
            entry.output_cap = 1;
            entry
        }
        "ee_verify_workspace_fact" => {
            let mut entry = write();
            entry.transports = ACP_ONLY;
            entry.output_cap = 1;
            entry
        }
        "ee_terminal_create" => execute(ALL_TRANSPORTS),
        "ee_terminal_kill" | "ee_terminal_release" => execute(ACP_ONLY),
        _ => return None,
    };
    Some(entry)
}

/// Whether a stable tool is implemented on `transport`.
#[must_use]
pub fn supports_transport(tool: &str, transport: ToolTransport) -> bool {
    governance(tool).is_some_and(|entry| entry.transports.contains(&transport))
}

/// Returns known stable tool names for one transport in deterministic order.
#[must_use]
pub fn tool_names_for_transport(transport: ToolTransport) -> Vec<&'static str> {
    STABLE_TOOL_NAMES.iter().copied().filter(|tool| supports_transport(tool, transport)).collect()
}

/// Returns approval-free read-only tool names suitable for critic use.
///
/// Terminal-backed reads are excluded even though their side-effect class is
/// read, because they still expose command execution state.
#[must_use]
pub fn critic_read_only_tool_names(transport: ToolTransport) -> Vec<&'static str> {
    STABLE_TOOL_NAMES
        .iter()
        .copied()
        .filter(|tool| {
            governance(tool).is_some_and(|entry| {
                entry.transports.contains(&transport)
                    && entry.side_effect == SideEffectClass::Read
                    && entry.approval == "none"
                    && !entry.required_capabilities.contains(&"terminal")
            })
        })
        .collect()
}

/// Stable tool names. Keep this list in existing compatibility order.
pub const STABLE_TOOL_NAMES: &[&str] = &[
    "ee_workspace_roots",
    "ee_list_directory",
    "ee_list_directory_all",
    "ee_search_files",
    "ee_search_files_all",
    "ee_search_text",
    "ee_search_text_regex",
    "ee_search_text_in_files",
    "ee_web_search",
    "ee_fetch_url",
    "ee_browser_run_content",
    "ee_browser_run_screenshot",
    "ee_browser_run_markdown",
    "ee_browser_run_scrape",
    "ee_browser_run_json",
    "ee_browser_run_links",
    "ee_replace_text",
    "ee_apply_patch",
    "ee_create_text_file",
    "ee_overwrite_text_file",
    "ee_create_directory",
    "ee_delete_path",
    "ee_copy_path",
    "ee_move_path",
    "ee_read_buffer",
    "ee_read_buffer_lines",
    "ee_open_buffers",
    "ee_get_diagnostics",
    "ee_get_file_diagnostics",
    "ee_document_symbols",
    "ee_references",
    "ee_list_code_actions",
    "ee_apply_code_action",
    "ee_format_file",
    "ee_preview_rename_symbol",
    "ee_rename_symbol",
    "ee_read_text_file",
    "ee_write_text_file",
    "ee_terminal_create",
    "ee_terminal_output",
    "ee_terminal_output_since",
    "ee_terminal_wait",
    "ee_terminal_wait_long",
    "ee_terminal_kill",
    "ee_terminal_release",
    "ee_git_status",
    "ee_git_diff",
    "ee_git_diff_staged",
    "ee_git_diff_file",
    "ee_changed_files",
    "ee_review_context",
    "ee_turn_evidence_summary",
    "ee_project_instructions",
    "ee_save_note",
    "ee_read_notes",
    "ee_read_note",
    "ee_remember_workspace_fact",
    "ee_verify_workspace_fact",
    "ee_recall_workspace_facts",
    "ee_read_workspace_fact",
    "ee_forget_workspace_fact",
    "ee_list_workspace_facts",
    "ee_retract_workspace_fact",
    "ee_export_workspace_memory",
    "ee_import_workspace_memory",
    "ee_clear_workspace_memory",
    "ee_file_dependency_map",
    "ee_symbol_dependency_map",
    "ee_tools_manifest",
    "ee_diagnostics",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_stable_tools_have_governance_and_a_transport() {
        for tool in STABLE_TOOL_NAMES {
            let entry = governance(tool).expect("stable tool has governance");
            assert!(!entry.transports.is_empty(), "{tool}");
            assert!(!entry.error_classes.is_empty(), "{tool}");
        }
    }

    #[test]
    fn filesystem_writes_require_approval_on_stdio_and_acp() {
        for tool in ["ee_create_directory", "ee_delete_path", "ee_copy_path", "ee_move_path"] {
            let entry = governance(tool).expect("filesystem tool governance");
            assert_eq!(entry.side_effect, SideEffectClass::Write, "{tool}");
            assert_eq!(entry.approval, "required", "{tool}");
            assert_eq!(entry.transports, ALL_TRANSPORTS, "{tool}");
        }
    }

    #[test]
    fn workspace_memory_tools_have_required_governance_on_both_transports() {
        for tool in
            ["ee_recall_workspace_facts", "ee_read_workspace_fact", "ee_list_workspace_facts"]
        {
            let entry = governance(tool).expect("workspace-memory read governance");
            assert_eq!(entry.side_effect, SideEffectClass::Read, "{tool}");
            assert_eq!(entry.approval, "none", "{tool}");
            assert_eq!(entry.transports, ALL_TRANSPORTS, "{tool}");
            assert_eq!(
                entry.output_cap,
                match tool {
                    "ee_recall_workspace_facts" => 8,
                    "ee_list_workspace_facts" => 256,
                    _ => 1,
                }
            );
        }
        for tool in [
            "ee_remember_workspace_fact",
            "ee_forget_workspace_fact",
            "ee_retract_workspace_fact",
            "ee_import_workspace_memory",
            "ee_clear_workspace_memory",
        ] {
            let entry = governance(tool).expect("workspace-memory write governance");
            assert_eq!(entry.side_effect, SideEffectClass::Write, "{tool}");
            assert_eq!(entry.approval, "required", "{tool}");
            assert_eq!(entry.transports, ALL_TRANSPORTS, "{tool}");
            assert_eq!(entry.output_cap, 1, "{tool}");
        }
        let export = governance("ee_export_workspace_memory").expect("export governance");
        assert_eq!(export.side_effect, SideEffectClass::Read);
        assert_eq!(export.approval, "required");
        assert_eq!(export.transports, ALL_TRANSPORTS);
        assert_eq!(export.output_cap, 512 * 1024);
    }

    #[test]
    fn stdio_does_not_advertise_acp_only_tools() {
        let stdio = tool_names_for_transport(ToolTransport::Stdio);
        assert!(stdio.contains(&"ee_terminal_create"));
        assert!(!stdio.contains(&"ee_terminal_output"));
        assert!(!stdio.contains(&"ee_terminal_kill"));
        assert!(!stdio.contains(&"ee_turn_evidence_summary"));
    }

    #[test]
    fn critic_read_only_filters_tools_by_governance() {
        let tools = critic_read_only_tool_names(ToolTransport::Acp);

        assert!(tools.contains(&"ee_workspace_roots"));
        assert!(tools.contains(&"ee_get_diagnostics"));
        assert!(tools.contains(&"ee_tools_manifest"));

        assert!(!tools.contains(&"ee_replace_text"));
        assert!(!tools.contains(&"ee_terminal_create"));
        assert!(!tools.contains(&"ee_terminal_output"));
        assert!(!tools.contains(&"ee_web_search"));
        assert!(!tools.contains(&"unknown_tool"));
    }

    #[test]
    fn turn_evidence_summary_is_acp_only_and_redacted() {
        let entry = governance("ee_turn_evidence_summary").expect("evidence governance");
        assert_eq!(entry.side_effect, SideEffectClass::Read);
        assert_eq!(entry.approval, "none");
        assert_eq!(entry.transports, ACP_ONLY);
        assert_eq!(entry.output_cap_kind, "evidence_ids");
        assert_eq!(entry.output_cap, 128);
        assert_eq!(entry.redaction_rules, TURN_EVIDENCE_REDACTION);
        assert_eq!(entry.error_classes, TURN_EVIDENCE_ERRORS);
    }

    #[test]
    fn web_context_tools_require_external_network_approval_on_all_transports() {
        for tool in [
            "ee_web_search",
            "ee_fetch_url",
            "ee_browser_run_content",
            "ee_browser_run_screenshot",
            "ee_browser_run_markdown",
            "ee_browser_run_scrape",
            "ee_browser_run_json",
            "ee_browser_run_links",
        ] {
            let entry = governance(tool).expect("web tool has governance");
            assert_eq!(entry.side_effect, SideEffectClass::Read);
            assert_eq!(entry.approval, "required");
            assert_eq!(entry.transports, ALL_TRANSPORTS);
            assert_eq!(entry.required_capabilities, WEB_CONTEXT_CAPABILITY);
            assert_eq!(entry.output_cap_kind, "response_bytes");
            assert_eq!(entry.output_cap, 1024 * 1024);
            assert_eq!(entry.redaction_rules, WEB_CONTEXT_REDACTION);
            assert_eq!(entry.error_classes, WEB_CONTEXT_ERROR_CLASSES);
        }
    }

    #[test]
    fn web_context_metadata_declares_network_cache_and_untrusted_content_rules() {
        assert_eq!(WEB_CONTEXT_CAPABILITY, ["web_context", "ExternalNetwork"]);
        assert_eq!(
            WEB_CONTEXT_REDACTION,
            [
                "secret_like_values",
                "sensitive_diagnostics",
                "external_content_untrusted",
                "external_content_not_instructions",
                "session_local_cache_normalized_public_fields_only_60_seconds",
            ]
        );
    }

    #[test]
    fn web_context_error_classes_are_stable_and_complete() {
        assert_eq!(
            WEB_CONTEXT_ERROR_CLASSES,
            [
                "web_disabled",
                "web_search_unavailable",
                "network_approval_required",
                "url_rejected",
                "dns_rejected",
                "redirect_rejected",
                "unsupported_content_type",
                "response_too_large",
                "network_timeout",
                "network_failure",
            ]
        );
    }
}
