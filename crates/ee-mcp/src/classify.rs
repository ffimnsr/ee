//! ee-pinned tool manifest classification (Phase 3 MCP trust).
//!
//! The ee proxy tool set is an application-owned manifest: tool names,
//! argument schemas, and side-effect classes are pinned here, never derived
//! from agent input or server discovery.  Rule creation runs only after
//! classification succeeds; unknown tools and unknown side-effect classes
//! fail closed (prompt).

/// Side-effect class of one ee proxy tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SideEffectClass {
    Read,
    Write,
    Execute,
    Unknown,
}

impl SideEffectClass {
    /// Machine-readable class name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SideEffectClass::Read => "read",
            SideEffectClass::Write => "write",
            SideEffectClass::Execute => "execute",
            SideEffectClass::Unknown => "unknown",
        }
    }
}

/// Schema version of the current ee tool manifest. Exact MCP rules match
/// this version; incompatible changes require a new tool name.
pub use crate::tool_governance::EE_TOOL_SCHEMA_VERSION;

/// Classifies one ee proxy tool from the canonical governance record. Tools
/// outside the record are `Unknown` and never qualify for trust.
#[must_use]
pub fn side_effect_class(tool: &str) -> SideEffectClass {
    crate::tool_governance::governance(tool)
        .map_or(SideEffectClass::Unknown, |entry| entry.side_effect)
}

/// Whether an ee tool may be granted exact-invocation trust (Phase 3):
///
/// - side-effect class is `write` or `execute` (read rules arrive with the
///   workspace gate in phase 4),
/// - never `ee_terminal_create` (terminal creation uses command trust
///   only),
/// - the argument profile carries no file contents (the trust store never
///   persists file contents, secret-like values, or binary attachments).
///
/// Content-bearing write tools (`ee_write_text_file`, `ee_create_text_file`,
/// `ee_overwrite_text_file`, `ee_replace_text`, `ee_apply_patch`) and
/// unknown tools are therefore never eligible.
#[must_use]
pub fn exact_trust_eligible(tool: &str) -> bool {
    matches!(tool, "ee_apply_code_action" | "ee_format_file" | "ee_rename_symbol")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_tools_classify_as_read() {
        for tool in [
            "ee_workspace_roots",
            "ee_list_directory",
            "ee_search_text",
            "ee_read_text_file",
            "ee_read_buffer",
            "ee_get_diagnostics",
            "ee_document_symbols",
            "ee_terminal_output",
            "ee_terminal_wait",
            "ee_diagnostics",
        ] {
            assert_eq!(side_effect_class(tool), SideEffectClass::Read, "{tool}");
        }
    }

    #[test]
    fn write_tools_classify_as_write() {
        for tool in [
            "ee_replace_text",
            "ee_apply_patch",
            "ee_create_text_file",
            "ee_overwrite_text_file",
            "ee_apply_code_action",
            "ee_format_file",
            "ee_rename_symbol",
            "ee_write_text_file",
        ] {
            assert_eq!(side_effect_class(tool), SideEffectClass::Write, "{tool}");
        }
    }

    #[test]
    fn terminal_tools_classify_as_execute() {
        for tool in ["ee_terminal_create", "ee_terminal_kill", "ee_terminal_release"] {
            assert_eq!(side_effect_class(tool), SideEffectClass::Execute, "{tool}");
        }
    }

    #[test]
    fn unknown_tools_fail_closed() {
        assert_eq!(side_effect_class("ee_fetch_remote"), SideEffectClass::Unknown);
        assert_eq!(side_effect_class("some_server_tool"), SideEffectClass::Unknown);
        assert_eq!(side_effect_class(""), SideEffectClass::Unknown);
        assert_eq!(SideEffectClass::Unknown.as_str(), "unknown");
    }

    #[test]
    fn class_names_are_machine_readable() {
        assert_eq!(SideEffectClass::Read.as_str(), "read");
        assert_eq!(SideEffectClass::Write.as_str(), "write");
        assert_eq!(SideEffectClass::Execute.as_str(), "execute");
    }

    #[test]
    fn manifest_schema_version_is_three() {
        assert_eq!(EE_TOOL_SCHEMA_VERSION, 3);
    }

    #[test]
    fn exact_trust_eligibility_is_restricted_to_persistable_write_tools() {
        for tool in ["ee_apply_code_action", "ee_format_file", "ee_rename_symbol"] {
            assert!(exact_trust_eligible(tool), "{tool} must be eligible");
            assert_eq!(side_effect_class(tool), SideEffectClass::Write, "{tool}");
        }
        // Content-bearing write tools never persist (file contents must
        // never reach the trust store).
        for tool in [
            "ee_write_text_file",
            "ee_create_text_file",
            "ee_overwrite_text_file",
            "ee_replace_text",
            "ee_apply_patch",
        ] {
            assert!(!exact_trust_eligible(tool), "{tool} must not be eligible");
        }
        // Terminal creation uses command trust only.
        assert!(!exact_trust_eligible("ee_terminal_create"));
        // Read tools wait for the phase 4 workspace gate.
        assert!(!exact_trust_eligible("ee_read_text_file"));
        assert!(!exact_trust_eligible("ee_list_directory"));
        // Unknown tools never qualify.
        assert!(!exact_trust_eligible("ee_mystery_tool"));
    }
}
