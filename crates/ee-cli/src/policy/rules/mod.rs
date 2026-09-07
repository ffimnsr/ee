//! Typed persistent trust rules and domain matchers.
//!
//! Schema version 2 uses effect-bearing typed arrays. `filesystem_rules` and
//! `tool_rules` support authority-reducing deny/confirm effects only. Every raw entry uses
//! `deny_unknown_fields`, strict cross-field validation, and conversion into
//! tagged [`TrustRule`] values used by evaluator.
//! Unknown fields, cross-kind fields, invalid enum values, malformed
//! entries, and duplicate ids are rejected — never silently ignored.
pub(crate) use std::fmt;
pub(crate) use std::time::SystemTime;

pub(crate) use serde::de::{Deserializer, Error as _, MapAccess, Visitor};
pub(crate) use serde::{Deserialize, Serialize};

use super::{
    BrowserActionClass, FilesystemOperationKind, NetworkMethodClass, NetworkScheme,
    OperationIdentity, TrustCategory, TrustEffect, TrustOperation, TrustRuleScope,
    WorkspaceIdentity,
};
use crate::policy::paths::is_protected_segment;

/// Cap on the canonical `arguments_json` payload of one MCP rule.
pub(crate) const MAX_ARGUMENTS_JSON_BYTES: usize = 4096;

/// Longest allowed authority-granting window. Deny expiration is unbounded.
pub(crate) const MAX_RULE_DURATION: std::time::Duration =
    std::time::Duration::from_secs(30 * 24 * 60 * 60); // 30 days

/// Largest allowed finite use budget for authority-granting rules.
pub(crate) const MAX_RULE_MAX_USES: u64 = 10_000;

/// Application safety maxima for persistent write rules (Phase 5): derived
/// grants are bounded by the approved request and by these ceilings; rules
/// carrying caps above a maximum are rejected at load.
pub(crate) const MAX_WRITE_FILES: u64 = 8;
pub(crate) const MAX_WRITE_TOTAL_BYTES: u64 = 1_048_576; // 1 MiB aggregate
pub(crate) const MAX_WRITE_FILE_BYTES: u64 = 262_144; // 256 KiB per file

/// Secret-like markers (case-insensitive substring match on object keys),
/// mirroring the host redaction policy so the feature-independent policy
/// module needs no `agents` dependency.
const SENSITIVE_KEY_MARKERS: [&str; 6] =
    ["TOKEN", "KEY", "SECRET", "PASSWORD", "AUTH", "CREDENTIAL"];

/// Command argv match mode; the schema names `argv_exact` | `argv_prefix`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MatchMode {
    ArgvExact,
    ArgvPrefix,
}

/// Write operation kind; the schema names `create` | `modify`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WriteOperationKind {
    Create,
    Modify,
}

/// Canonical workspace-relative path segment sequence.  Empty, root-wide,
/// absolute, traversal, glob, regex, and protected prefixes are invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathPrefix {
    segments: Vec<String>,
    display: String,
}

impl PathPrefix {
    /// Validates and canonicalizes a workspace-relative path prefix.
    pub(crate) fn parse(raw: &str) -> Result<Self, String> {
        if raw.is_empty() {
            return Err("path_prefix must not be empty".to_string());
        }
        if raw.starts_with('/') || raw.starts_with('\\') || raw.contains(':') {
            return Err("path_prefix must be workspace-relative".to_string());
        }
        if raw == "." || raw == ".." {
            return Err("root-wide and traversal prefixes are invalid".to_string());
        }
        let mut segments = Vec::new();
        for segment in raw.split('/') {
            if segment.is_empty() {
                return Err("path_prefix contains an empty segment".to_string());
            }
            if segment == "." || segment == ".." {
                return Err("path_prefix contains traversal segments".to_string());
            }
            if segment.chars().any(|c| {
                matches!(
                    c,
                    '*' | '?' | '[' | ']' | '{' | '}' | '(' | ')' | '|' | '^' | '$' | '+' | '\\'
                )
            }) {
                return Err("path_prefix must not contain glob or regex characters".to_string());
            }
            if segment.chars().any(|c| c.is_control() || c == '\u{0}') {
                return Err("path_prefix contains control characters".to_string());
            }
            if segment.starts_with('.') || is_protected_segment(segment) {
                return Err("path_prefix must not contain protected segments".to_string());
            }
            segments.push(segment.to_string());
        }
        Ok(Self { display: segments.join("/"), segments })
    }

    /// Whether the canonical workspace-relative path starts with this
    /// prefix (segment-boundary match).
    pub(crate) fn matches(&self, relative: &str) -> bool {
        if relative.is_empty() {
            return false;
        }
        let operation_segments: Vec<&str> = relative.split('/').collect();
        operation_segments.len() >= self.segments.len()
            && self.segments.iter().zip(operation_segments.iter()).all(|(a, b)| a == b)
    }

    pub(crate) fn display(&self) -> &str {
        &self.display
    }

    pub(crate) fn segments(&self) -> &[String] {
        &self.segments
    }
}

// ── Domain rule variants ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) scope: TrustRuleScope,
    pub(crate) executable: String,
    pub(crate) match_mode: MatchMode,
    pub(crate) argv: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) scope: TrustRuleScope,
    pub(crate) server: String,
    pub(crate) transport_identity: String,
    pub(crate) tool: String,
    pub(crate) tool_schema_version: u64,
    /// Canonical compact JSON object (sorted keys, no duplicates).
    pub(crate) arguments_json: String,
}

/// Deny/confirm MCP identity. Arguments intentionally absent; optional
/// category narrows matching without requiring request arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpDenyRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) scope: TrustRuleScope,
    pub(crate) server: String,
    pub(crate) transport_identity: String,
    pub(crate) tool: String,
    pub(crate) tool_schema_version: u64,
    pub(crate) category: Option<TrustCategory>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadPathRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) scope: TrustRuleScope,
    pub(crate) path_prefix: PathPrefix,
    pub(crate) max_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpReadRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) scope: TrustRuleScope,
    pub(crate) server: String,
    pub(crate) transport_identity: String,
    pub(crate) tool: String,
    pub(crate) tool_schema_version: u64,
    pub(crate) path_prefix: PathPrefix,
    pub(crate) max_bytes: u64,
}

/// Fixed application-owned MCP read-tool profile. Server, transport, and
/// manifest schema remain exact matches; the profile id determines its fixed
/// read-only tool list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpReadProfileRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) scope: TrustRuleScope,
    pub(crate) server: String,
    pub(crate) transport_identity: String,
    pub(crate) tool_schema_version: u64,
    pub(crate) profile: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProfileRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) scope: TrustRuleScope,
    pub(crate) profile: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WriteRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) scope: TrustRuleScope,
    pub(crate) operation: WriteOperationKind,
    pub(crate) path_prefix: PathPrefix,
    pub(crate) max_files: u64,
    pub(crate) max_total_bytes: u64,
    pub(crate) max_file_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HostMatchMode {
    Exact,
    Suffix,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NetworkRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) scope: TrustRuleScope,
    scheme: NetworkScheme,
    host: String,
    host_match: HostMatchMode,
    port: u16,
    method: NetworkMethodClass,
    browser_action: BrowserActionClass,
}

/// Deny/confirm filesystem matcher. Either source or destination prefix matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FilesystemRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) scope: TrustRuleScope,
    pub(crate) operations: Vec<FilesystemOperationKind>,
    pub(crate) path_prefix: PathPrefix,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolRuleIdentity {
    Native { tool: String },
    Mcp { server: String, transport_identity: String, tool: String, tool_schema_version: u64 },
}

/// Deny/confirm stable tool/category matcher used when richer fields are absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) scope: TrustRuleScope,
    pub(crate) identity: ToolRuleIdentity,
    pub(crate) category: Option<TrustCategory>,
}

/// Tagged persistent trust rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TrustRule {
    Command(CommandRule),
    Mcp(McpRule),
    ReadPath(ReadPathRule),
    McpRead(McpReadRule),
    McpReadProfile(McpReadProfileRule),
    Profile(ProfileRule),
    Write(WriteRule),
    Network(NetworkRule),
    McpDeny(McpDenyRule),
    Filesystem(FilesystemRule),
    Tool(ToolRule),
    /// Versioned application template metadata wrapping explicit matcher fields.
    Template {
        template_id: String,
        rule: Box<TrustRule>,
    },
}
/// Stable rule id for a newly created write grant (`write_…`).
pub(crate) fn generate_write_rule_id() -> String {
    format!("write_{:016x}", rand::random::<u64>())
}

pub(crate) fn generate_filesystem_rule_id() -> String {
    format!("filesystem_{:016x}", rand::random::<u64>())
}

pub(crate) fn generate_network_rule_id() -> String {
    format!("network_{:016x}", rand::random::<u64>())
}

pub(crate) fn generate_tool_rule_id() -> String {
    format!("tool_{:016x}", rand::random::<u64>())
}

pub(crate) use json::canonicalize_arguments_json;
pub(crate) use parse::normalize_host;
pub(crate) use raw::{
    RawCommandRule, RawFilesystemRule, RawMcpReadProfileRule, RawMcpReadRule, RawMcpRule,
    RawNetworkRule, RawProfileRule, RawReadPathRule, RawToolRule, RawWriteRule,
};
pub(crate) use validate::validate_rule_id;

mod convert;
mod evaluate;
mod json;
mod parse;
mod raw;
mod validate;

#[cfg(test)]
mod tests;
