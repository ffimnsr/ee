//! Typed persistent trust rules and domain matchers.
//!
//! Schema version 2 uses effect-bearing typed arrays. `filesystem_rules` and
//! `tool_rules` support authority-reducing deny/confirm effects only. Every raw entry uses
//! `deny_unknown_fields`, strict cross-field validation, and conversion into
//! tagged [`TrustRule`] values used by evaluator.
//! Unknown fields, cross-kind fields, invalid enum values, malformed
//! entries, and duplicate ids are rejected — never silently ignored.

use std::fmt;
use std::time::SystemTime;

use serde::de::{Deserializer, Error as _, MapAccess, Visitor};
use serde::{Deserialize, Serialize};

use super::paths::is_protected_segment;
use super::{
    BrowserActionClass, FilesystemOperationKind, NetworkMethodClass, NetworkScheme,
    OperationIdentity, TrustCategory, TrustEffect, TrustOperation, TrustRuleScope,
    WorkspaceIdentity,
};

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

impl TrustRule {
    pub(crate) fn mcp_deny(rule: McpDenyRule) -> Self {
        Self::McpDeny(rule)
    }

    pub(crate) fn filesystem(rule: FilesystemRule) -> Self {
        Self::Filesystem(rule)
    }

    pub(crate) fn tool(rule: ToolRule) -> Self {
        Self::Tool(rule)
    }

    pub(crate) fn with_template(template_id: String, rule: TrustRule) -> Result<Self, String> {
        super::templates::validate_template(&template_id, &rule)?;
        Ok(Self::Template { template_id, rule: Box::new(rule) })
    }

    pub(crate) fn template_id(&self) -> Option<&str> {
        match self {
            TrustRule::Template { template_id, .. } => Some(template_id),
            _ => None,
        }
    }

    pub(crate) fn untemplated(&self) -> &TrustRule {
        match self {
            TrustRule::Template { rule, .. } => rule.untemplated(),
            rule => rule,
        }
    }

    pub(crate) fn id(&self) -> &str {
        match self {
            TrustRule::Command(rule) => &rule.id,
            TrustRule::Mcp(rule) => &rule.id,
            TrustRule::ReadPath(rule) => &rule.id,
            TrustRule::McpRead(rule) => &rule.id,
            TrustRule::McpReadProfile(rule) => &rule.id,
            TrustRule::Profile(rule) => &rule.id,
            TrustRule::Write(rule) => &rule.id,
            TrustRule::Network(rule) => &rule.id,
            TrustRule::McpDeny(rule) => &rule.id,
            TrustRule::Filesystem(rule) => &rule.id,
            TrustRule::Tool(rule) => &rule.id,
            TrustRule::Template { rule, .. } => rule.id(),
        }
    }

    pub(crate) fn scope(&self) -> &TrustRuleScope {
        match self {
            TrustRule::Command(rule) => &rule.scope,
            TrustRule::Mcp(rule) => &rule.scope,
            TrustRule::ReadPath(rule) => &rule.scope,
            TrustRule::McpRead(rule) => &rule.scope,
            TrustRule::McpReadProfile(rule) => &rule.scope,
            TrustRule::Profile(rule) => &rule.scope,
            TrustRule::Write(rule) => &rule.scope,
            TrustRule::Network(rule) => &rule.scope,
            TrustRule::McpDeny(rule) => &rule.scope,
            TrustRule::Filesystem(rule) => &rule.scope,
            TrustRule::Tool(rule) => &rule.scope,
            TrustRule::Template { rule, .. } => rule.scope(),
        }
    }

    pub(crate) fn scope_mut(&mut self) -> &mut TrustRuleScope {
        match self {
            TrustRule::Command(rule) => &mut rule.scope,
            TrustRule::Mcp(rule) => &mut rule.scope,
            TrustRule::ReadPath(rule) => &mut rule.scope,
            TrustRule::McpRead(rule) => &mut rule.scope,
            TrustRule::McpReadProfile(rule) => &mut rule.scope,
            TrustRule::Profile(rule) => &mut rule.scope,
            TrustRule::Write(rule) => &mut rule.scope,
            TrustRule::Network(rule) => &mut rule.scope,
            TrustRule::McpDeny(rule) => &mut rule.scope,
            TrustRule::Filesystem(rule) => &mut rule.scope,
            TrustRule::Tool(rule) => &mut rule.scope,
            TrustRule::Template { rule, .. } => rule.scope_mut(),
        }
    }

    pub(crate) fn effect(&self) -> TrustEffect {
        match self {
            TrustRule::Command(rule) => rule.effect,
            TrustRule::Mcp(rule) => rule.effect,
            TrustRule::ReadPath(rule) => rule.effect,
            TrustRule::McpRead(rule) => rule.effect,
            TrustRule::McpReadProfile(rule) => rule.effect,
            TrustRule::Profile(rule) => rule.effect,
            TrustRule::Write(rule) => rule.effect,
            TrustRule::Network(rule) => rule.effect,
            TrustRule::McpDeny(rule) => rule.effect,
            TrustRule::Filesystem(rule) => rule.effect,
            TrustRule::Tool(rule) => rule.effect,
            TrustRule::Template { rule, .. } => rule.effect(),
        }
    }

    /// Operation-specific comparison only; scope checks (workspace, agent,
    /// expiry, usage) run in the evaluator before this.
    pub(crate) fn matches(&self, operation: &TrustOperation) -> bool {
        match self {
            TrustRule::Command(rule) => {
                if operation.category != TrustCategory::Execute {
                    return false;
                }
                let OperationIdentity::Command { executable, argv } = &operation.identity else {
                    return false;
                };
                rule.executable == *executable && rule.matches_argv(argv)
            }
            TrustRule::Mcp(rule) => {
                let OperationIdentity::Mcp {
                    server,
                    transport_identity,
                    tool,
                    tool_schema_version,
                    arguments_json,
                } = &operation.identity
                else {
                    return false;
                };
                rule.server == *server
                    && rule.transport_identity == *transport_identity
                    && rule.tool == *tool
                    && rule.tool_schema_version == *tool_schema_version
                    && rule.arguments_json == *arguments_json
            }

            TrustRule::ReadPath(rule) => {
                if operation.category != TrustCategory::Read {
                    return false;
                }
                let OperationIdentity::ReadPath { relative_path, byte_count } = &operation.identity
                else {
                    return false;
                };
                rule.path_prefix.matches(relative_path)
                    && (rule.effect != TrustEffect::Allow || rule.size_ok(*byte_count))
            }
            TrustRule::McpRead(rule) => {
                if operation.category != TrustCategory::Read {
                    return false;
                }
                let OperationIdentity::McpRead {
                    server,
                    transport_identity,
                    tool,
                    tool_schema_version,
                    relative_path,
                    byte_count,
                } = &operation.identity
                else {
                    return false;
                };
                rule.server == *server
                    && rule.transport_identity == *transport_identity
                    && rule.tool == *tool
                    && rule.tool_schema_version == *tool_schema_version
                    && rule.path_prefix.matches(relative_path)
                    && (rule.effect != TrustEffect::Allow || rule.size_ok(*byte_count))
            }
            TrustRule::McpReadProfile(rule) => {
                if operation.category != TrustCategory::Read {
                    return false;
                }
                let (server, transport_identity, tool, tool_schema_version) =
                    match &operation.identity {
                        OperationIdentity::McpRead {
                            server,
                            transport_identity,
                            tool,
                            tool_schema_version,
                            ..
                        }
                        | OperationIdentity::Mcp {
                            server,
                            transport_identity,
                            tool,
                            tool_schema_version,
                            ..
                        } => (server, transport_identity, tool, tool_schema_version),
                        _ => return false,
                    };
                rule.server == *server
                    && rule.transport_identity == *transport_identity
                    && rule.tool_schema_version == *tool_schema_version
                    && super::profiles::mcp_read_profile_matches(&rule.profile, tool)
            }
            TrustRule::Profile(rule) => {
                let OperationIdentity::Profile { profile } = &operation.identity else {
                    return false;
                };
                rule.profile == *profile
            }
            TrustRule::Write(rule) => {
                let OperationIdentity::Write {
                    relative_path,
                    file_count,
                    total_bytes,
                    max_file_bytes,
                } = &operation.identity
                else {
                    return false;
                };
                let category_ok = match rule.operation {
                    WriteOperationKind::Create => operation.category == TrustCategory::WriteCreate,
                    WriteOperationKind::Modify => operation.category == TrustCategory::WriteModify,
                };
                category_ok
                    && rule.path_prefix.matches(relative_path)
                    && (rule.effect != TrustEffect::Allow
                        || (*file_count <= rule.max_files
                            && total_bytes.is_none_or(|bytes| bytes <= rule.max_total_bytes)
                            && max_file_bytes.is_none_or(|bytes| bytes <= rule.max_file_bytes)))
            }
            TrustRule::Network(rule) => rule.matches(operation),
            TrustRule::McpDeny(rule) => rule.matches(operation),
            TrustRule::Filesystem(rule) => rule.matches(operation),
            TrustRule::Tool(rule) => rule.matches(operation),
            TrustRule::Template { rule, .. } => rule.matches(operation),
        }
    }
}

impl CommandRule {
    fn matches_argv(&self, argv: &[String]) -> bool {
        match self.match_mode {
            MatchMode::ArgvExact => self.argv == argv,
            MatchMode::ArgvPrefix => {
                argv.len() >= self.argv.len()
                    && self.argv.iter().zip(argv.iter()).all(|(a, b)| a == b)
            }
        }
    }
}

impl ReadPathRule {
    fn size_ok(&self, byte_count: Option<u64>) -> bool {
        byte_count.is_none_or(|bytes| bytes <= self.max_bytes)
    }
}

impl McpReadRule {
    fn size_ok(&self, byte_count: Option<u64>) -> bool {
        byte_count.is_none_or(|bytes| bytes <= self.max_bytes)
    }
}

impl NetworkRule {
    pub(crate) fn host_match(&self) -> HostMatchMode {
        self.host_match
    }

    pub(crate) fn category(&self) -> TrustCategory {
        TrustCategory::Network
    }

    fn matches(&self, operation: &TrustOperation) -> bool {
        if operation.category != TrustCategory::Network {
            return false;
        }
        let OperationIdentity::Network { scheme, host, port, method, browser_action } =
            &operation.identity
        else {
            return false;
        };
        self.scheme == *scheme
            && host_matches(&self.host, self.host_match, host)
            && self.port == *port
            && self.method == *method
            && self.browser_action == *browser_action
    }
}

impl McpDenyRule {
    fn matches(&self, operation: &TrustOperation) -> bool {
        let (server, transport_identity, tool, tool_schema_version) = match &operation.identity {
            OperationIdentity::Mcp {
                server,
                transport_identity,
                tool,
                tool_schema_version,
                ..
            }
            | OperationIdentity::McpRead {
                server,
                transport_identity,
                tool,
                tool_schema_version,
                ..
            } => (server, transport_identity, tool, tool_schema_version),
            _ => return false,
        };
        self.server == *server
            && self.transport_identity == *transport_identity
            && self.tool == *tool
            && self.tool_schema_version == *tool_schema_version
            && self.category.is_none_or(|category| category == operation.category)
    }
}

impl FilesystemRule {
    fn matches(&self, operation: &TrustOperation) -> bool {
        let OperationIdentity::Filesystem {
            operation: filesystem_operation,
            source_path,
            destination_path,
        } = &operation.identity
        else {
            return false;
        };
        self.operations.contains(filesystem_operation)
            && source_path
                .iter()
                .chain(destination_path.iter())
                .any(|path| self.path_prefix.matches(path))
    }
}

impl ToolRule {
    fn matches(&self, operation: &TrustOperation) -> bool {
        if self.category.is_some_and(|category| category != operation.category) {
            return false;
        }
        match (&self.identity, &operation.identity) {
            (
                ToolRuleIdentity::Native { tool: expected },
                OperationIdentity::NativeTool { tool },
            ) => expected == tool,
            (
                ToolRuleIdentity::Mcp {
                    server: expected_server,
                    transport_identity: expected_transport,
                    tool: expected_tool,
                    tool_schema_version: expected_schema,
                },
                OperationIdentity::Mcp {
                    server,
                    transport_identity,
                    tool,
                    tool_schema_version,
                    ..
                }
                | OperationIdentity::McpRead {
                    server,
                    transport_identity,
                    tool,
                    tool_schema_version,
                    ..
                },
            ) => {
                expected_server == server
                    && expected_transport == transport_identity
                    && expected_tool == tool
                    && expected_schema == tool_schema_version
            }
            _ => false,
        }
    }
}

fn host_matches(expected: &str, mode: HostMatchMode, host: &str) -> bool {
    match mode {
        HostMatchMode::Exact => expected == host,
        HostMatchMode::Suffix => {
            host == expected
                || (host.len() > expected.len()
                    && host.ends_with(expected)
                    && host.as_bytes().get(host.len() - expected.len() - 1) == Some(&b'.'))
        }
    }
}

// ── Raw TOML forms (canonical field order, deny_unknown_fields) ─────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawCommandRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) agent: Option<String>,
    pub(crate) executable: String,
    #[serde(rename = "match")]
    pub(crate) match_mode: MatchMode,
    pub(crate) argv: Vec<String>,
    pub(crate) expires_at: Option<String>,
    pub(crate) max_uses: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawMcpRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) agent: Option<String>,
    pub(crate) server: String,
    pub(crate) transport_identity: String,
    pub(crate) tool: String,
    pub(crate) tool_schema_version: u64,
    pub(crate) category: Option<TrustCategory>,
    pub(crate) arguments_json: Option<String>,
    pub(crate) expires_at: Option<String>,
    pub(crate) max_uses: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawReadPathRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) agent: Option<String>,
    pub(crate) path_prefix: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) max_bytes: u64,
    pub(crate) expires_at: Option<String>,
    pub(crate) max_uses: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawMcpReadRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) agent: Option<String>,
    pub(crate) server: String,
    pub(crate) transport_identity: String,
    pub(crate) tool: String,
    pub(crate) tool_schema_version: u64,
    pub(crate) path_prefix: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) max_bytes: u64,
    pub(crate) expires_at: Option<String>,
    pub(crate) max_uses: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawMcpReadProfileRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) agent: Option<String>,
    pub(crate) server: String,
    pub(crate) transport_identity: String,
    pub(crate) tool_schema_version: u64,
    pub(crate) profile: String,
    pub(crate) expires_at: Option<String>,
    pub(crate) max_uses: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawProfileRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) agent: Option<String>,
    pub(crate) profile: String,
    pub(crate) expires_at: Option<String>,
    pub(crate) max_uses: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawWriteRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) agent: Option<String>,
    pub(crate) operation: WriteOperationKind,
    pub(crate) path_prefix: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) max_files: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) max_total_bytes: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) max_file_bytes: u64,
    pub(crate) expires_at: Option<String>,
    pub(crate) max_uses: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawNetworkRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) agent: Option<String>,
    pub(crate) scheme: NetworkScheme,
    pub(crate) host: String,
    pub(crate) host_match: HostMatchMode,
    pub(crate) port: u16,
    pub(crate) method: NetworkMethodClass,
    pub(crate) browser_action: BrowserActionClass,
    pub(crate) expires_at: Option<String>,
    pub(crate) max_uses: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawFilesystemRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) agent: Option<String>,
    pub(crate) operations: Vec<FilesystemOperationKind>,
    pub(crate) path_prefix: String,
    pub(crate) expires_at: Option<String>,
    pub(crate) max_uses: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawToolRule {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) agent: Option<String>,
    pub(crate) native_tool: Option<String>,
    pub(crate) server: Option<String>,
    pub(crate) transport_identity: Option<String>,
    pub(crate) tool: Option<String>,
    pub(crate) tool_schema_version: Option<u64>,
    pub(crate) category: Option<TrustCategory>,
    pub(crate) expires_at: Option<String>,
    pub(crate) max_uses: Option<u64>,
}

// ── Raw → domain validation ──────────────────────────────────────────────────

fn require_non_empty(field: &str, value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    Ok(value.to_string())
}

/// Audit-safe stable id: 1-80 ASCII letters, digits, `_`, or `-`.
pub(crate) fn validate_rule_id(value: &str) -> Result<String, String> {
    if value.is_empty() || value.len() > 80 {
        return Err("id must contain 1 to 80 ASCII characters".to_string());
    }
    if !value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')) {
        return Err("id must contain only ASCII alphanumeric, _, or - characters".to_string());
    }
    Ok(value.to_string())
}

fn optional_non_empty(field: &str, value: Option<String>) -> Result<Option<String>, String> {
    value.map(|value| require_non_empty(field, &value)).transpose()
}

fn validate_no_control(field: &str, value: &str) -> Result<(), String> {
    if value.chars().any(|c| c.is_control() || c == '\u{0}') {
        return Err(format!("{field} contains control characters"));
    }
    Ok(())
}

fn parse_required_expiry(raw: Option<String>) -> Result<SystemTime, String> {
    raw.ok_or_else(|| "expires_at is required".to_string()).and_then(|text| parse_expiry(&text))
}

fn parse_optional_expiry(raw: Option<String>) -> Result<Option<SystemTime>, String> {
    raw.as_deref().map(parse_expiry).transpose()
}

fn parse_required_uses(raw: Option<u64>) -> Result<u64, String> {
    let uses = raw.ok_or_else(|| "max_uses is required".to_string())?;
    if uses == 0 {
        return Err("max_uses must be at least 1".to_string());
    }
    Ok(uses)
}

fn parse_optional_uses(raw: Option<u64>) -> Result<Option<u64>, String> {
    match raw {
        None => Ok(None),
        Some(0) => Err("max_uses must be at least 1".to_string()),
        Some(uses) => Ok(Some(uses)),
    }
}

fn parse_effect_scope(
    effect: TrustEffect,
    expires_at: Option<String>,
    max_uses: Option<u64>,
    bounded_allow: bool,
) -> Result<(Option<SystemTime>, Option<u64>), String> {
    match effect {
        TrustEffect::Allow if bounded_allow => {
            Ok((Some(parse_required_expiry(expires_at)?), Some(parse_required_uses(max_uses)?)))
        }
        TrustEffect::Allow => {
            Ok((parse_optional_expiry(expires_at)?, parse_optional_uses(max_uses)?))
        }
        TrustEffect::Deny | TrustEffect::Confirm => {
            if max_uses.is_some() {
                return Err("max_uses is valid only for allow rules".to_string());
            }
            Ok((parse_optional_expiry(expires_at)?, None))
        }
    }
}

fn parse_positive(field: &str, value: u64) -> Result<u64, String> {
    if value == 0 {
        return Err(format!("{field} must be at least 1"));
    }
    Ok(value)
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn parse_allow_ceiling(effect: TrustEffect, field: &str, value: u64) -> Result<u64, String> {
    if effect != TrustEffect::Allow {
        return Ok(0);
    }
    parse_positive(field, value)
}

fn parse_path_prefix(raw: String) -> Result<PathPrefix, String> {
    PathPrefix::parse(&raw)
}

fn validate_category(category: Option<TrustCategory>) -> Result<Option<TrustCategory>, String> {
    if category == Some(TrustCategory::Unknown) {
        return Err("unknown category cannot scope a rule".to_string());
    }
    Ok(category)
}

fn parse_identity_fields(
    server: &str,
    transport_identity: &str,
    tool: &str,
    tool_schema_version: u64,
) -> Result<(), String> {
    require_non_empty("server", server)?;
    validate_no_control("server", server)?;
    require_non_empty("transport_identity", transport_identity)?;
    validate_no_control("transport_identity", transport_identity)?;
    require_non_empty("tool", tool)?;
    validate_no_control("tool", tool)?;
    if tool_schema_version == 0 {
        return Err("tool_schema_version must be at least 1".to_string());
    }
    Ok(())
}

fn parse_expiry(text: &str) -> Result<SystemTime, String> {
    let parsed = chrono::DateTime::parse_from_rfc3339(text)
        .map_err(|error| format!("invalid expires_at {text:?}: {error}"))?;
    Ok(parsed.with_timezone(&chrono::Utc).into())
}

fn format_expiry(time: SystemTime) -> String {
    let datetime: chrono::DateTime<chrono::Utc> = time.into();
    datetime.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

impl CommandRule {
    pub(crate) fn from_raw(
        raw: RawCommandRule,
        workspace: WorkspaceIdentity,
    ) -> Result<Self, String> {
        let id = validate_rule_id(&raw.id)?;
        let agent = optional_non_empty("agent", raw.agent)?;
        let executable = require_non_empty("executable", &raw.executable)?;
        validate_no_control("executable", &executable)?;
        // Deny/confirm may target shell wrappers because neither grants
        // authority. Persistent allow retains shell-wrapper rejection.
        if raw.effect == TrustEffect::Allow {
            super::command::validate_executable(&executable)?;
        }
        let argv = raw.argv;
        super::command::validate_argv_tokens(&argv)?;
        match raw.match_mode {
            MatchMode::ArgvExact => {}
            MatchMode::ArgvPrefix if argv.is_empty() => {
                return Err("argv_prefix requires at least one argv token".to_string());
            }
            MatchMode::ArgvPrefix => {}
        }
        let (expires_at, max_uses) =
            parse_effect_scope(raw.effect, raw.expires_at, raw.max_uses, true)?;
        Ok(Self {
            id,
            effect: raw.effect,
            scope: TrustRuleScope { workspace, agent, expires_at, max_uses },
            executable,
            match_mode: raw.match_mode,
            argv,
        })
    }
}

impl TrustRule {
    pub(crate) fn from_raw_mcp(
        raw: RawMcpRule,
        workspace: WorkspaceIdentity,
    ) -> Result<Self, String> {
        let id = validate_rule_id(&raw.id)?;
        let agent = optional_non_empty("agent", raw.agent)?;
        parse_identity_fields(
            &raw.server,
            &raw.transport_identity,
            &raw.tool,
            raw.tool_schema_version,
        )?;
        let (expires_at, max_uses) =
            parse_effect_scope(raw.effect, raw.expires_at, raw.max_uses, true)?;
        let scope = TrustRuleScope { workspace, agent, expires_at, max_uses };
        if raw.effect != TrustEffect::Allow {
            if raw.arguments_json.is_some() {
                return Err(
                    "arguments_json is incompatible with MCP deny/confirm rules".to_string()
                );
            }
            let scoped = McpDenyRule {
                id: id.clone(),
                effect: raw.effect,
                scope: scope.clone(),
                server: raw.server,
                transport_identity: raw.transport_identity,
                tool: raw.tool,
                tool_schema_version: raw.tool_schema_version,
                category: validate_category(raw.category)?,
            };
            return Ok(Self::mcp_deny(scoped));
        }
        if raw.category.is_some() {
            return Err("category is valid only for MCP deny/confirm rules".to_string());
        }
        let arguments_json = raw
            .arguments_json
            .ok_or_else(|| "arguments_json is required for MCP allow rules".to_string())?;
        Ok(Self::Mcp(McpRule {
            id,
            effect: raw.effect,
            scope,
            server: raw.server,
            transport_identity: raw.transport_identity,
            tool: raw.tool,
            tool_schema_version: raw.tool_schema_version,
            arguments_json: canonicalize_arguments_json(&arguments_json)?,
        }))
    }
}

impl ReadPathRule {
    pub(crate) fn from_raw(
        raw: RawReadPathRule,
        workspace: WorkspaceIdentity,
    ) -> Result<Self, String> {
        let id = validate_rule_id(&raw.id)?;
        let agent = optional_non_empty("agent", raw.agent)?;
        let max_bytes = parse_allow_ceiling(raw.effect, "max_bytes", raw.max_bytes)?;
        let (expires_at, max_uses) =
            parse_effect_scope(raw.effect, raw.expires_at, raw.max_uses, false)?;
        Ok(Self {
            id,
            effect: raw.effect,
            scope: TrustRuleScope { workspace, agent, expires_at, max_uses },
            path_prefix: parse_path_prefix(raw.path_prefix)?,
            max_bytes,
        })
    }
}

impl McpReadRule {
    pub(crate) fn from_raw(
        raw: RawMcpReadRule,
        workspace: WorkspaceIdentity,
    ) -> Result<Self, String> {
        let id = validate_rule_id(&raw.id)?;
        let agent = optional_non_empty("agent", raw.agent)?;
        parse_identity_fields(
            &raw.server,
            &raw.transport_identity,
            &raw.tool,
            raw.tool_schema_version,
        )?;
        let max_bytes = parse_allow_ceiling(raw.effect, "max_bytes", raw.max_bytes)?;
        let (expires_at, max_uses) =
            parse_effect_scope(raw.effect, raw.expires_at, raw.max_uses, false)?;
        Ok(Self {
            id,
            effect: raw.effect,
            scope: TrustRuleScope { workspace, agent, expires_at, max_uses },
            server: raw.server,
            transport_identity: raw.transport_identity,
            tool: raw.tool,
            tool_schema_version: raw.tool_schema_version,
            path_prefix: parse_path_prefix(raw.path_prefix)?,
            max_bytes,
        })
    }
}

impl McpReadProfileRule {
    pub(crate) fn from_raw(
        raw: RawMcpReadProfileRule,
        workspace: WorkspaceIdentity,
    ) -> Result<Self, String> {
        let id = validate_rule_id(&raw.id)?;
        let agent = optional_non_empty("agent", raw.agent)?;
        parse_identity_fields(
            &raw.server,
            &raw.transport_identity,
            "ee_mcp_safe_read",
            raw.tool_schema_version,
        )?;
        let profile = require_non_empty("profile", &raw.profile)?;
        validate_no_control("profile", &profile)?;
        if !super::profiles::is_known_mcp_read_profile(&profile) {
            return Err(format!("unknown MCP read profile id: {profile}"));
        }
        let (expires_at, max_uses) =
            parse_effect_scope(raw.effect, raw.expires_at, raw.max_uses, false)?;
        Ok(Self {
            id,
            effect: raw.effect,
            scope: TrustRuleScope { workspace, agent, expires_at, max_uses },
            server: raw.server,
            transport_identity: raw.transport_identity,
            tool_schema_version: raw.tool_schema_version,
            profile,
        })
    }
}

impl ProfileRule {
    pub(crate) fn from_raw(
        raw: RawProfileRule,
        workspace: WorkspaceIdentity,
    ) -> Result<Self, String> {
        let id = validate_rule_id(&raw.id)?;
        let agent = optional_non_empty("agent", raw.agent)?;
        let profile = require_non_empty("profile", &raw.profile)?;
        validate_no_control("profile", &profile)?;
        // Profile ids come from the application-owned curated registry only;
        // unknown ids are rejected rather than granted (Phase 4).
        if !super::profiles::is_known_profile(&profile) {
            return Err(format!("unknown curated profile id: {profile}"));
        }
        let (expires_at, max_uses) =
            parse_effect_scope(raw.effect, raw.expires_at, raw.max_uses, true)?;
        Ok(Self {
            id,
            effect: raw.effect,
            scope: TrustRuleScope { workspace, agent, expires_at, max_uses },
            profile,
        })
    }
}

impl WriteRule {
    pub(crate) fn from_raw(
        raw: RawWriteRule,
        workspace: WorkspaceIdentity,
    ) -> Result<Self, String> {
        let id = validate_rule_id(&raw.id)?;
        let agent = optional_non_empty("agent", raw.agent)?;
        let max_files = parse_allow_ceiling(raw.effect, "max_files", raw.max_files)?;
        let max_total_bytes =
            parse_allow_ceiling(raw.effect, "max_total_bytes", raw.max_total_bytes)?;
        let max_file_bytes = parse_allow_ceiling(raw.effect, "max_file_bytes", raw.max_file_bytes)?;
        // Bounded write trust stays within the application safety maxima;
        // larger caps are rejected rather than clamped (Phase 5).
        if max_files > MAX_WRITE_FILES {
            return Err(format!(
                "max_files {max_files} exceeds the safety maximum {MAX_WRITE_FILES}"
            ));
        }
        if max_total_bytes > MAX_WRITE_TOTAL_BYTES {
            return Err(format!(
                "max_total_bytes {max_total_bytes} exceeds the safety maximum {MAX_WRITE_TOTAL_BYTES}"
            ));
        }
        if max_file_bytes > MAX_WRITE_FILE_BYTES {
            return Err(format!(
                "max_file_bytes {max_file_bytes} exceeds the safety maximum {MAX_WRITE_FILE_BYTES}"
            ));
        }
        if max_file_bytes > max_total_bytes {
            return Err("max_file_bytes must not exceed max_total_bytes".to_string());
        }
        let (expires_at, max_uses) =
            parse_effect_scope(raw.effect, raw.expires_at, raw.max_uses, true)?;
        Ok(Self {
            id,
            effect: raw.effect,
            scope: TrustRuleScope { workspace, agent, expires_at, max_uses },
            operation: raw.operation,
            path_prefix: parse_path_prefix(raw.path_prefix)?,
            max_files,
            max_total_bytes,
            max_file_bytes,
        })
    }
}

// ── Domain → raw (canonical schema order) ────────────────────────────────────

impl NetworkRule {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn allow_exact(
        id: String,
        scope: TrustRuleScope,
        scheme: NetworkScheme,
        host: String,
        port: u16,
        method: NetworkMethodClass,
        browser_action: BrowserActionClass,
    ) -> Result<Self, String> {
        if scope.expires_at.is_none() || scope.max_uses.is_none() {
            return Err("network allow requires expiration and use budget".to_string());
        }
        if method != NetworkMethodClass::Read
            || !matches!(browser_action, BrowserActionClass::Fetch | BrowserActionClass::Navigate)
        {
            return Err(
                "network allow supports read-only method and action classes only".to_string()
            );
        }
        Ok(Self {
            id: validate_rule_id(&id)?,
            effect: TrustEffect::Allow,
            scope,
            scheme,
            host: normalize_host(&host, HostMatchMode::Exact)?,
            host_match: HostMatchMode::Exact,
            port: if port == 0 { return Err("port must be at least 1".to_string()) } else { port },
            method,
            browser_action,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn deny(
        id: String,
        scope: TrustRuleScope,
        scheme: NetworkScheme,
        host: String,
        host_match: HostMatchMode,
        port: u16,
        method: NetworkMethodClass,
        browser_action: BrowserActionClass,
    ) -> Result<Self, String> {
        let id = validate_rule_id(&id)?;
        let host = normalize_host(&host, host_match)?;
        if port == 0 {
            return Err("port must be at least 1".to_string());
        }
        if scope.max_uses.is_some() {
            return Err("max_uses is valid only for allow rules".to_string());
        }
        Ok(Self {
            id,
            effect: TrustEffect::Deny,
            scope,
            scheme,
            host,
            host_match,
            port,
            method,
            browser_action,
        })
    }

    pub(crate) fn from_raw(
        raw: RawNetworkRule,
        workspace: WorkspaceIdentity,
    ) -> Result<Self, String> {
        let id = validate_rule_id(&raw.id)?;
        let agent = optional_non_empty("agent", raw.agent)?;
        let host = normalize_host(&raw.host, raw.host_match)?;
        if raw.port == 0 {
            return Err("port must be at least 1".to_string());
        }
        if raw.effect == TrustEffect::Allow
            && (raw.host_match != HostMatchMode::Exact
                || raw.method != NetworkMethodClass::Read
                || !matches!(
                    raw.browser_action,
                    BrowserActionClass::Fetch | BrowserActionClass::Navigate
                ))
        {
            return Err(
                "network allow requires exact host and read-only method/action classes".to_string()
            );
        }
        let (expires_at, max_uses) =
            parse_effect_scope(raw.effect, raw.expires_at, raw.max_uses, true)?;
        Ok(Self {
            id,
            effect: raw.effect,
            scope: TrustRuleScope { workspace, agent, expires_at, max_uses },
            scheme: raw.scheme,
            host,
            host_match: raw.host_match,
            port: raw.port,
            method: raw.method,
            browser_action: raw.browser_action,
        })
    }
}

impl FilesystemRule {
    pub(crate) fn from_raw(
        raw: RawFilesystemRule,
        workspace: WorkspaceIdentity,
    ) -> Result<Self, String> {
        if raw.effect == TrustEffect::Allow {
            return Err("filesystem_rules support only deny or confirm effect".to_string());
        }
        if raw.operations.is_empty() {
            return Err("operations must contain at least one operation".to_string());
        }
        let mut operations = raw.operations;
        operations.sort_unstable();
        operations.dedup();
        let agent = optional_non_empty("agent", raw.agent)?;
        let (expires_at, max_uses) =
            parse_effect_scope(raw.effect, raw.expires_at, raw.max_uses, false)?;
        Ok(Self {
            id: validate_rule_id(&raw.id)?,
            effect: raw.effect,
            scope: TrustRuleScope { workspace, agent, expires_at, max_uses },
            operations,
            path_prefix: parse_path_prefix(raw.path_prefix)?,
        })
    }

    pub(crate) fn into_trust_rule(self) -> TrustRule {
        TrustRule::Filesystem(self)
    }
}

impl ToolRule {
    pub(crate) fn from_raw(raw: RawToolRule, workspace: WorkspaceIdentity) -> Result<Self, String> {
        if raw.effect == TrustEffect::Allow {
            return Err("tool_rules support only deny or confirm effect".to_string());
        }
        let identity = match (
            raw.native_tool,
            raw.server,
            raw.transport_identity,
            raw.tool,
            raw.tool_schema_version,
        ) {
            (Some(tool), None, None, None, None) => {
                let tool = require_non_empty("native_tool", &tool)?;
                validate_no_control("native_tool", &tool)?;
                ToolRuleIdentity::Native { tool }
            }
            (None, Some(server), Some(transport), Some(tool), Some(schema)) => {
                parse_identity_fields(&server, &transport, &tool, schema)?;
                ToolRuleIdentity::Mcp {
                    server,
                    transport_identity: transport,
                    tool,
                    tool_schema_version: schema,
                }
            }
            _ => {
                return Err(
                    "tool rule must contain exactly native_tool or complete MCP identity fields"
                        .to_string(),
                );
            }
        };
        let agent = optional_non_empty("agent", raw.agent)?;
        let (expires_at, max_uses) =
            parse_effect_scope(raw.effect, raw.expires_at, raw.max_uses, false)?;
        Ok(Self {
            id: validate_rule_id(&raw.id)?,
            effect: raw.effect,
            scope: TrustRuleScope { workspace, agent, expires_at, max_uses },
            identity,
            category: validate_category(raw.category)?,
        })
    }

    pub(crate) fn into_trust_rule(self) -> TrustRule {
        TrustRule::Tool(self)
    }
}

pub(crate) fn normalize_host(raw: &str, mode: HostMatchMode) -> Result<String, String> {
    let host = raw.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() || host.contains('*') || host.chars().any(char::is_control) {
        return Err("host must be non-empty and contain no wildcard or control characters".into());
    }
    if host.parse::<std::net::IpAddr>().is_ok() {
        if mode == HostMatchMode::Suffix {
            return Err("host suffix must not be an IP address".into());
        }
        return Ok(host);
    }
    if host.len() > 253
        || host.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return Err("host must be a valid ASCII DNS name or exact IP address".into());
    }
    if mode == HostMatchMode::Suffix && !host.contains('.') {
        return Err("host suffix must contain at least two DNS labels".into());
    }
    Ok(host)
}

impl From<&CommandRule> for RawCommandRule {
    fn from(rule: &CommandRule) -> Self {
        Self {
            id: rule.id.clone(),
            effect: rule.effect,
            agent: rule.scope.agent.clone(),
            executable: rule.executable.clone(),
            match_mode: rule.match_mode,
            argv: rule.argv.clone(),
            expires_at: rule.scope.expires_at.map(format_expiry),
            max_uses: rule.scope.max_uses,
        }
    }
}

impl From<&McpRule> for RawMcpRule {
    fn from(rule: &McpRule) -> Self {
        Self {
            id: rule.id.clone(),
            effect: rule.effect,
            agent: rule.scope.agent.clone(),
            server: rule.server.clone(),
            transport_identity: rule.transport_identity.clone(),
            tool: rule.tool.clone(),
            tool_schema_version: rule.tool_schema_version,
            category: None,
            arguments_json: Some(rule.arguments_json.clone()),
            expires_at: rule.scope.expires_at.map(format_expiry),
            max_uses: rule.scope.max_uses,
        }
    }
}

impl From<&McpDenyRule> for RawMcpRule {
    fn from(rule: &McpDenyRule) -> Self {
        Self {
            id: rule.id.clone(),
            effect: rule.effect,
            agent: rule.scope.agent.clone(),
            server: rule.server.clone(),
            transport_identity: rule.transport_identity.clone(),
            tool: rule.tool.clone(),
            tool_schema_version: rule.tool_schema_version,
            category: rule.category,
            arguments_json: None,
            expires_at: rule.scope.expires_at.map(format_expiry),
            max_uses: None,
        }
    }
}

impl From<&ReadPathRule> for RawReadPathRule {
    fn from(rule: &ReadPathRule) -> Self {
        Self {
            id: rule.id.clone(),
            effect: rule.effect,
            agent: rule.scope.agent.clone(),
            path_prefix: rule.path_prefix.display().to_string(),
            max_bytes: if rule.effect == TrustEffect::Allow { rule.max_bytes } else { 0 },
            expires_at: rule.scope.expires_at.map(format_expiry),
            max_uses: rule.scope.max_uses,
        }
    }
}

impl From<&McpReadRule> for RawMcpReadRule {
    fn from(rule: &McpReadRule) -> Self {
        Self {
            id: rule.id.clone(),
            effect: rule.effect,
            agent: rule.scope.agent.clone(),
            server: rule.server.clone(),
            transport_identity: rule.transport_identity.clone(),
            tool: rule.tool.clone(),
            tool_schema_version: rule.tool_schema_version,
            path_prefix: rule.path_prefix.display().to_string(),
            max_bytes: if rule.effect == TrustEffect::Allow { rule.max_bytes } else { 0 },
            expires_at: rule.scope.expires_at.map(format_expiry),
            max_uses: rule.scope.max_uses,
        }
    }
}

impl From<&McpReadProfileRule> for RawMcpReadProfileRule {
    fn from(rule: &McpReadProfileRule) -> Self {
        Self {
            id: rule.id.clone(),
            effect: rule.effect,
            agent: rule.scope.agent.clone(),
            server: rule.server.clone(),
            transport_identity: rule.transport_identity.clone(),
            tool_schema_version: rule.tool_schema_version,
            profile: rule.profile.clone(),
            expires_at: rule.scope.expires_at.map(format_expiry),
            max_uses: rule.scope.max_uses,
        }
    }
}

impl From<&ProfileRule> for RawProfileRule {
    fn from(rule: &ProfileRule) -> Self {
        Self {
            id: rule.id.clone(),
            effect: rule.effect,
            agent: rule.scope.agent.clone(),
            profile: rule.profile.clone(),
            expires_at: rule.scope.expires_at.map(format_expiry),
            max_uses: rule.scope.max_uses,
        }
    }
}

impl From<&WriteRule> for RawWriteRule {
    fn from(rule: &WriteRule) -> Self {
        Self {
            id: rule.id.clone(),
            effect: rule.effect,
            agent: rule.scope.agent.clone(),
            operation: rule.operation,
            path_prefix: rule.path_prefix.display().to_string(),
            max_files: if rule.effect == TrustEffect::Allow { rule.max_files } else { 0 },
            max_total_bytes: if rule.effect == TrustEffect::Allow {
                rule.max_total_bytes
            } else {
                0
            },
            max_file_bytes: if rule.effect == TrustEffect::Allow { rule.max_file_bytes } else { 0 },
            expires_at: rule.scope.expires_at.map(format_expiry),
            max_uses: rule.scope.max_uses,
        }
    }
}

impl From<&NetworkRule> for RawNetworkRule {
    fn from(rule: &NetworkRule) -> Self {
        Self {
            id: rule.id.clone(),
            effect: rule.effect,
            agent: rule.scope.agent.clone(),
            scheme: rule.scheme,
            host: rule.host.clone(),
            host_match: rule.host_match,
            port: rule.port,
            method: rule.method,
            browser_action: rule.browser_action,
            expires_at: rule.scope.expires_at.map(format_expiry),
            max_uses: rule.scope.max_uses,
        }
    }
}

impl From<&FilesystemRule> for RawFilesystemRule {
    fn from(rule: &FilesystemRule) -> Self {
        Self {
            id: rule.id.clone(),
            effect: rule.effect,
            agent: rule.scope.agent.clone(),
            operations: rule.operations.clone(),
            path_prefix: rule.path_prefix.display().to_string(),
            expires_at: rule.scope.expires_at.map(format_expiry),
            max_uses: None,
        }
    }
}

impl From<&ToolRule> for RawToolRule {
    fn from(rule: &ToolRule) -> Self {
        let (native_tool, server, transport_identity, tool, tool_schema_version) = match &rule
            .identity
        {
            ToolRuleIdentity::Native { tool } => (Some(tool.clone()), None, None, None, None),
            ToolRuleIdentity::Mcp { server, transport_identity, tool, tool_schema_version } => (
                None,
                Some(server.clone()),
                Some(transport_identity.clone()),
                Some(tool.clone()),
                Some(*tool_schema_version),
            ),
        };
        Self {
            id: rule.id.clone(),
            effect: rule.effect,
            agent: rule.scope.agent.clone(),
            native_tool,
            server,
            transport_identity,
            tool,
            tool_schema_version,
            category: rule.category,
            expires_at: rule.scope.expires_at.map(format_expiry),
            max_uses: None,
        }
    }
}

// ── Canonical arguments_json ─────────────────────────────────────────────────

/// Strict JSON value: duplicate object keys anywhere in the payload are
/// rejected, unlike `serde_json::Value` which silently keeps the last one.
#[derive(Debug, Clone, PartialEq)]
enum StrictJson {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<StrictJson>),
    Object(std::collections::BTreeMap<String, StrictJson>),
}

impl<'de> Deserialize<'de> for StrictJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(StrictJsonVisitor)
    }
}

struct StrictJsonVisitor;

impl<'de> Visitor<'de> for StrictJsonVisitor {
    type Value = StrictJson;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_unit<E>(self) -> Result<StrictJson, E> {
        Ok(StrictJson::Null)
    }

    fn visit_none<E>(self) -> Result<StrictJson, E> {
        Ok(StrictJson::Null)
    }

    fn visit_bool<E>(self, value: bool) -> Result<StrictJson, E> {
        Ok(StrictJson::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<StrictJson, E> {
        Ok(StrictJson::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<StrictJson, E> {
        Ok(StrictJson::Number(value.into()))
    }

    fn visit_f64<E>(self, value: f64) -> Result<StrictJson, E>
    where
        E: serde::de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(StrictJson::Number)
            .ok_or_else(|| E::custom("invalid JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<StrictJson, E> {
        Ok(StrictJson::String(value.to_string()))
    }

    fn visit_string<E>(self, value: String) -> Result<StrictJson, E> {
        Ok(StrictJson::String(value))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<StrictJson, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        let mut items = Vec::new();
        while let Some(item) = sequence.next_element::<StrictJson>()? {
            items.push(item);
        }
        Ok(StrictJson::Array(items))
    }

    fn visit_map<A>(self, mut map: A) -> Result<StrictJson, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut entries = std::collections::BTreeMap::new();
        while let Some(key) = map.next_key::<String>()? {
            if entries.contains_key(&key) {
                return Err(A::Error::custom(format!("duplicate object key: {key}")));
            }
            let value = map.next_value::<StrictJson>()?;
            entries.insert(key, value);
        }
        Ok(StrictJson::Object(entries))
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    SENSITIVE_KEY_MARKERS.iter().any(|marker| upper.contains(marker))
}

fn reject_sensitive(value: &StrictJson) -> Result<(), String> {
    match value {
        StrictJson::Object(entries) => {
            for (key, value) in entries {
                if key.chars().any(|c| c.is_control() || c == '\u{0}') {
                    return Err("argument key contains control characters".to_string());
                }
                if is_sensitive_key(key) {
                    return Err(format!("sensitive argument key: {key}"));
                }
                reject_sensitive(value)?;
            }
            Ok(())
        }
        StrictJson::Array(items) => {
            for item in items {
                reject_sensitive(item)?;
            }
            Ok(())
        }
        StrictJson::String(text) => {
            if text.chars().any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r')) {
                return Err("string value contains binary or control characters".to_string());
            }
            Ok(())
        }
        StrictJson::Null | StrictJson::Bool(_) | StrictJson::Number(_) => Ok(()),
    }
}

fn strict_json_to_value(value: StrictJson) -> serde_json::Value {
    match value {
        StrictJson::Null => serde_json::Value::Null,
        StrictJson::Bool(value) => serde_json::Value::Bool(value),
        StrictJson::Number(value) => serde_json::Value::Number(value),
        StrictJson::String(value) => serde_json::Value::String(value),
        StrictJson::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(strict_json_to_value).collect())
        }
        StrictJson::Object(entries) => serde_json::Value::Object(
            entries.into_iter().map(|(key, value)| (key, strict_json_to_value(value))).collect(),
        ),
    }
}

/// Parses, validates, and canonicalizes `arguments_json`: it must be a JSON
/// object, free of duplicate keys, sensitive keys, binary/control content,
/// and oversized payloads.  The canonical form sorts object keys and uses
/// compact whitespace.
pub(crate) fn canonicalize_arguments_json(raw: &str) -> Result<String, String> {
    if raw.len() > MAX_ARGUMENTS_JSON_BYTES {
        return Err(format!("arguments_json exceeds the {MAX_ARGUMENTS_JSON_BYTES} byte cap"));
    }
    let parsed: StrictJson = serde_json::from_str(raw)
        .map_err(|error| format!("arguments_json is not valid JSON: {error}"))?;
    if !matches!(parsed, StrictJson::Object(_)) {
        return Err("arguments_json must be a JSON object".to_string());
    }
    reject_sensitive(&parsed)?;
    Ok(strict_json_to_value(parsed).to_string())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{TransportKind, TrustOperation};

    fn workspace() -> WorkspaceIdentity {
        WorkspaceIdentity::from_canonical_root_bytes(b"/phase9")
    }

    fn operation(category: TrustCategory, identity: OperationIdentity) -> TrustOperation {
        TrustOperation {
            workspace: workspace(),
            agent: None,
            transport: TransportKind::McpStdio,
            category,
            identity,
        }
    }

    #[test]
    fn persistent_deny_command_allows_shell_wrapper_but_keeps_token_validation() {
        let raw = RawCommandRule {
            id: "deny-shell".into(),
            effect: TrustEffect::Deny,
            agent: None,
            executable: "sh".into(),
            match_mode: MatchMode::ArgvPrefix,
            argv: vec!["-c".into()],
            expires_at: None,
            max_uses: None,
        };
        let rule = TrustRule::Command(CommandRule::from_raw(raw.clone(), workspace()).unwrap());
        assert!(rule.matches(&operation(
            TrustCategory::Execute,
            OperationIdentity::Command {
                executable: "sh".into(),
                argv: vec!["-c".into(), "echo safe".into()],
            },
        )));
        assert!(
            CommandRule::from_raw(
                RawCommandRule { effect: TrustEffect::Allow, ..raw },
                workspace()
            )
            .is_err()
        );
    }

    #[test]
    fn persistent_deny_read_ignores_allow_only_ceiling() {
        let rule = TrustRule::ReadPath(
            ReadPathRule::from_raw(
                RawReadPathRule {
                    id: "deny-read".into(),
                    effect: TrustEffect::Deny,
                    agent: None,
                    path_prefix: "target/cache".into(),
                    max_bytes: u64::MAX,
                    expires_at: None,
                    max_uses: None,
                },
                workspace(),
            )
            .unwrap(),
        );
        assert!(rule.matches(&operation(
            TrustCategory::Read,
            OperationIdentity::ReadPath {
                relative_path: "target/cache/blob".into(),
                byte_count: Some(u64::MAX),
            },
        )));
    }

    #[test]
    fn persistent_deny_mcp_category_does_not_require_arguments() {
        let rule = TrustRule::from_raw_mcp(
            RawMcpRule {
                id: "deny-mcp".into(),
                effect: TrustEffect::Deny,
                agent: None,
                server: "ee".into(),
                transport_identity: "stdio:ee".into(),
                tool: "ee_apply_patch".into(),
                tool_schema_version: 2,
                category: Some(TrustCategory::WriteModify),
                arguments_json: None,
                expires_at: None,
                max_uses: None,
            },
            workspace(),
        )
        .unwrap();
        let identity = OperationIdentity::Mcp {
            server: "ee".into(),
            transport_identity: "stdio:ee".into(),
            tool: "ee_apply_patch".into(),
            tool_schema_version: 2,
            arguments_json: "{\"path\":\"secret\"}".into(),
        };
        assert!(rule.matches(&operation(TrustCategory::WriteModify, identity.clone())));
        assert!(!rule.matches(&operation(TrustCategory::Read, identity)));
    }

    #[test]
    fn persistent_deny_filesystem_matches_source_or_destination_boundary() {
        let rule = FilesystemRule::from_raw(
            RawFilesystemRule {
                id: "deny-fs".into(),
                effect: TrustEffect::Deny,
                agent: None,
                operations: vec![FilesystemOperationKind::Rename],
                path_prefix: "deploy/prod".into(),
                expires_at: None,
                max_uses: None,
            },
            workspace(),
        )
        .unwrap()
        .into_trust_rule();
        let identity = OperationIdentity::filesystem(
            FilesystemOperationKind::Rename,
            Some("tmp/file"),
            Some("deploy/prod/file"),
        )
        .unwrap();
        assert!(rule.matches(&operation(TrustCategory::WriteModify, identity)));
        let outside = OperationIdentity::filesystem(
            FilesystemOperationKind::Rename,
            Some("tmp/file"),
            Some("deploy/production/file"),
        )
        .unwrap();
        assert!(!rule.matches(&operation(TrustCategory::WriteModify, outside)));
    }

    #[test]
    fn bounded_rule_extraction_rejects_broad_or_writing_network_allow_from_schema() {
        for (host_match, method, action) in [
            (HostMatchMode::Suffix, NetworkMethodClass::Read, BrowserActionClass::Fetch),
            (HostMatchMode::Exact, NetworkMethodClass::Write, BrowserActionClass::Upload),
        ] {
            let result = NetworkRule::from_raw(
                RawNetworkRule {
                    id: "allow-net".into(),
                    effect: TrustEffect::Allow,
                    agent: None,
                    scheme: NetworkScheme::Https,
                    host: "api.example.com".into(),
                    host_match,
                    port: 443,
                    method,
                    browser_action: action,
                    expires_at: Some("1970-01-01T01:00:00Z".into()),
                    max_uses: Some(1),
                },
                workspace(),
            );
            assert!(result.is_err());
        }
    }

    #[test]
    fn persistent_deny_network_suffix_uses_dns_label_boundary() {
        let rule = TrustRule::Network(
            NetworkRule::from_raw(
                RawNetworkRule {
                    id: "deny-net".into(),
                    effect: TrustEffect::Deny,
                    agent: None,
                    scheme: NetworkScheme::Https,
                    host: "Example.COM.".into(),
                    host_match: HostMatchMode::Suffix,
                    port: 443,
                    method: NetworkMethodClass::Write,
                    browser_action: BrowserActionClass::Upload,
                    expires_at: None,
                    max_uses: None,
                },
                workspace(),
            )
            .unwrap(),
        );
        let matching = OperationIdentity::network(
            NetworkScheme::Https,
            "api.example.com",
            443,
            NetworkMethodClass::Write,
            BrowserActionClass::Upload,
        )
        .unwrap();
        let boundary_miss = OperationIdentity::network(
            NetworkScheme::Https,
            "notexample.com",
            443,
            NetworkMethodClass::Write,
            BrowserActionClass::Upload,
        )
        .unwrap();
        assert!(rule.matches(&operation(TrustCategory::Network, matching)));
        assert!(!rule.matches(&operation(TrustCategory::Network, boundary_miss)));
        assert!(normalize_host("127.0.0.1", HostMatchMode::Suffix).is_err());
    }

    #[test]
    fn persistent_deny_tool_rejects_mixed_identity_and_unsafe_ids() {
        let raw = RawToolRule {
            id: "deny-tool".into(),
            effect: TrustEffect::Deny,
            agent: None,
            native_tool: Some("terminal".into()),
            server: Some("ee".into()),
            transport_identity: None,
            tool: None,
            tool_schema_version: None,
            category: None,
            expires_at: None,
            max_uses: None,
        };
        assert!(ToolRule::from_raw(raw, workspace()).is_err());
        assert!(validate_rule_id("contains space").is_err());
        assert!(validate_rule_id(&"a".repeat(81)).is_err());
        assert!(validate_rule_id("deny_tool-1").is_ok());
    }
}
