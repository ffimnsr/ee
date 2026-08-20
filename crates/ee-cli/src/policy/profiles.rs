//! Curated command validation profiles (Phase 4).
//!
//! The profile registry is application-owned and versioned: stable profile
//! ids (`git_readonly`, `rust_validate`, `terminal_readonly`) map to fixed structured
//! executable/argv entries, each bound to the workspace cwd, a timeout cap,
//! an output cap, finite use/expiry requirements (via the shared rule
//! scope), and the execute category.  Config stores profile ids only and
//! can never mutate the registry; unknown ids and versions fail closed.

use std::time::Duration;

/// Version of the application-owned profile registry.
pub(crate) const PROFILE_REGISTRY_VERSION: u64 = 2;

/// Built-in terminal profile for safe direct workspace inspection. `pwd` and
/// `ls` are exact entries; the bridge additionally accepts one path-validated
/// `cat` operand only after canonical workspace and protected-path checks.
pub(crate) const TERMINAL_READONLY_PROFILE: &str = "terminal_readonly";

/// Built-in MCP profile for bounded, read-only ee tools. This profile is
/// application-owned: adding a manifest read tool never expands a persisted
/// grant until this list is intentionally updated.
pub(crate) const EE_MCP_SAFE_READ_PROFILE: &str = "ee_mcp_safe_read";
/// Manifest schema version accepted by the built-in safe-read profile.
pub(crate) const EE_MCP_SAFE_READ_TOOL_SCHEMA_VERSION: u64 = 1;

/// Fixed tools covered by [`EE_MCP_SAFE_READ_PROFILE`]. Write, execute, and
/// unknown tools must never appear here.
pub(crate) const EE_MCP_SAFE_READ_TOOLS: &[&str] = &[
    "ee_workspace_roots",
    "ee_list_directory",
    "ee_list_directory_all",
    "ee_search_files",
    "ee_search_files_all",
    "ee_search_text",
    "ee_search_text_regex",
    "ee_search_text_in_files",
    "ee_read_buffer",
    "ee_read_buffer_lines",
    "ee_open_buffers",
    "ee_get_diagnostics",
    "ee_get_file_diagnostics",
    "ee_document_symbols",
    "ee_references",
    "ee_list_code_actions",
    "ee_preview_rename_symbol",
    "ee_read_text_file",
    "ee_terminal_output",
    "ee_terminal_output_since",
    "ee_terminal_wait",
    "ee_terminal_wait_long",
    "ee_git_status",
    "ee_git_diff",
    "ee_git_diff_file",
    "ee_changed_files",
    "ee_review_context",
    "ee_tools_manifest",
    "ee_project_instructions",
    "ee_read_notes",
    "ee_read_note",
    "ee_file_dependency_map",
    "ee_diagnostics",
];

/// Whether `profile` and `tool` form a fixed, application-owned MCP read
/// profile entry. Unknown profiles and non-read manifest tools fail closed.
pub(crate) fn mcp_read_profile_matches(profile: &str, tool: &str) -> bool {
    profile == EE_MCP_SAFE_READ_PROFILE && EE_MCP_SAFE_READ_TOOLS.contains(&tool)
}

/// Whether `profile` identifies an application-owned MCP read profile.
pub(crate) fn is_known_mcp_read_profile(profile: &str) -> bool {
    profile == EE_MCP_SAFE_READ_PROFILE
}

/// One fixed structured entry inside a curated profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProfileEntry {
    pub(crate) executable: &'static str,
    pub(crate) argv: &'static [&'static str],
    /// Bound on one profile command run (registry metadata; the terminal
    /// pipeline caps and cancellation paths are unchanged).
    pub(crate) timeout_cap: Duration,
    /// Bound on retained output for one profile command.
    pub(crate) output_cap: usize,
}

/// One curated profile: stable id plus fixed structured entries.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CuratedProfile {
    pub(crate) id: &'static str,
    pub(crate) entries: &'static [ProfileEntry],
}

/// The application-owned curated profile registry.
///
/// Entries are exact structured argv matches and never include VCS
/// mutation, package install, package scripts, publish, network, or shell
/// commands.
pub(crate) const PROFILES: &[CuratedProfile] = &[
    CuratedProfile {
        id: "git_readonly",
        entries: &[
            ProfileEntry {
                executable: "git",
                argv: &["status"],
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["diff"],
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["log"],
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["show"],
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["branch", "--show-current"],
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
        ],
    },
    CuratedProfile {
        id: TERMINAL_READONLY_PROFILE,
        entries: &[
            ProfileEntry {
                executable: "pwd",
                argv: &[],
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            ProfileEntry {
                executable: "ls",
                argv: &[],
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            ProfileEntry {
                executable: "ls",
                argv: &["-a"],
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            ProfileEntry {
                executable: "ls",
                argv: &["-l"],
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            ProfileEntry {
                executable: "ls",
                argv: &["-la"],
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            ProfileEntry {
                executable: "ls",
                argv: &["-al"],
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
        ],
    },
    CuratedProfile {
        id: "rust_validate",
        entries: &[
            ProfileEntry {
                executable: "cargo",
                argv: &["fmt", "--check"],
                timeout_cap: Duration::from_secs(300),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "cargo",
                argv: &["test", "--quiet"],
                timeout_cap: Duration::from_secs(300),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "cargo",
                argv: &["clippy"],
                timeout_cap: Duration::from_secs(300),
                output_cap: 1024 * 1024,
            },
        ],
    },
];

/// Exact structured lookup of one command in the curated registry; returns
/// the profile id and the matched entry.
pub(crate) fn match_profile_entry(
    executable: &str,
    argv: &[String],
) -> Option<(&'static str, &'static ProfileEntry)> {
    for profile in PROFILES {
        for entry in profile.entries {
            if entry.executable == executable && entry.argv == argv {
                return Some((profile.id, entry));
            }
        }
    }
    None
}

/// Whether `id` is a known curated profile; unknown ids are rejected.
pub(crate) fn is_known_profile(id: &str) -> bool {
    PROFILES.iter().any(|profile| profile.id == id)
}
