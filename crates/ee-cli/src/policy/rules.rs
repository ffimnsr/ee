//! Typed persistent trust rules and their domain matchers (Phase 1
//! foundation).
//!
//! The store document defines six typed rule arrays (`command_allow`,
//! `mcp_allow`, `read_path_allow`, `mcp_read_allow`, `profile_allow`,
//! `write_allow`).  Each raw entry is deserialized with
//! `deny_unknown_fields` semantics, validated against the schema rules, and
//! converted into the tagged [`TrustRule`] enum used by the evaluator.
//! Unknown fields, cross-kind fields, invalid enum values, malformed
//! entries, and duplicate ids are rejected — never silently ignored.

use std::fmt;
use std::time::SystemTime;

use serde::de::{Deserializer, Error as _, MapAccess, Visitor};
use serde::{Deserialize, Serialize};

use super::paths::is_protected_segment;
use super::{OperationIdentity, TrustCategory, TrustOperation, TrustRuleScope, WorkspaceIdentity};

/// Cap on the canonical `arguments_json` payload of one MCP rule.
pub(crate) const MAX_ARGUMENTS_JSON_BYTES: usize = 4096;

/// Longest allowed grant window for any persistent rule (Phase 6):
/// hand-written documents with far-future expirations are rejected at load.
pub(crate) const MAX_RULE_DURATION: std::time::Duration =
    std::time::Duration::from_secs(30 * 24 * 60 * 60); // 30 days

/// Largest allowed finite use budget for any persistent rule (Phase 6).
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
}

// ── Domain rule variants ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandRule {
    pub(crate) id: String,
    pub(crate) scope: TrustRuleScope,
    pub(crate) executable: String,
    pub(crate) match_mode: MatchMode,
    pub(crate) argv: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpRule {
    pub(crate) id: String,
    pub(crate) scope: TrustRuleScope,
    pub(crate) server: String,
    pub(crate) transport_identity: String,
    pub(crate) tool: String,
    pub(crate) tool_schema_version: u64,
    /// Canonical compact JSON object (sorted keys, no duplicates).
    pub(crate) arguments_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadPathRule {
    pub(crate) id: String,
    pub(crate) scope: TrustRuleScope,
    pub(crate) path_prefix: PathPrefix,
    pub(crate) max_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpReadRule {
    pub(crate) id: String,
    pub(crate) scope: TrustRuleScope,
    pub(crate) server: String,
    pub(crate) transport_identity: String,
    pub(crate) tool: String,
    pub(crate) tool_schema_version: u64,
    pub(crate) path_prefix: PathPrefix,
    pub(crate) max_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProfileRule {
    pub(crate) id: String,
    pub(crate) scope: TrustRuleScope,
    pub(crate) profile: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WriteRule {
    pub(crate) id: String,
    pub(crate) scope: TrustRuleScope,
    pub(crate) operation: WriteOperationKind,
    pub(crate) path_prefix: PathPrefix,
    pub(crate) max_files: u64,
    pub(crate) max_total_bytes: u64,
    pub(crate) max_file_bytes: u64,
}

/// Tagged persistent trust rule: command, MCP exact invocation, read path,
/// MCP read, curated profile, or bounded write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TrustRule {
    Command(CommandRule),
    Mcp(McpRule),
    ReadPath(ReadPathRule),
    McpRead(McpReadRule),
    Profile(ProfileRule),
    Write(WriteRule),
}

impl TrustRule {
    pub(crate) fn id(&self) -> &str {
        match self {
            TrustRule::Command(rule) => &rule.id,
            TrustRule::Mcp(rule) => &rule.id,
            TrustRule::ReadPath(rule) => &rule.id,
            TrustRule::McpRead(rule) => &rule.id,
            TrustRule::Profile(rule) => &rule.id,
            TrustRule::Write(rule) => &rule.id,
        }
    }

    pub(crate) fn scope(&self) -> &TrustRuleScope {
        match self {
            TrustRule::Command(rule) => &rule.scope,
            TrustRule::Mcp(rule) => &rule.scope,
            TrustRule::ReadPath(rule) => &rule.scope,
            TrustRule::McpRead(rule) => &rule.scope,
            TrustRule::Profile(rule) => &rule.scope,
            TrustRule::Write(rule) => &rule.scope,
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
                rule.path_prefix.matches(relative_path) && rule.size_ok(*byte_count)
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
                    && rule.size_ok(*byte_count)
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
                    && *file_count <= rule.max_files
                    && total_bytes.is_none_or(|bytes| bytes <= rule.max_total_bytes)
                    && max_file_bytes.is_none_or(|bytes| bytes <= rule.max_file_bytes)
            }
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

// ── Raw TOML forms (canonical field order, deny_unknown_fields) ─────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawCommandRule {
    pub(crate) id: String,
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
    pub(crate) agent: Option<String>,
    pub(crate) server: String,
    pub(crate) transport_identity: String,
    pub(crate) tool: String,
    pub(crate) tool_schema_version: u64,
    pub(crate) arguments_json: String,
    pub(crate) expires_at: Option<String>,
    pub(crate) max_uses: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawReadPathRule {
    pub(crate) id: String,
    pub(crate) agent: Option<String>,
    pub(crate) path_prefix: String,
    pub(crate) max_bytes: u64,
    pub(crate) expires_at: Option<String>,
    pub(crate) max_uses: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawMcpReadRule {
    pub(crate) id: String,
    pub(crate) agent: Option<String>,
    pub(crate) server: String,
    pub(crate) transport_identity: String,
    pub(crate) tool: String,
    pub(crate) tool_schema_version: u64,
    pub(crate) path_prefix: String,
    pub(crate) max_bytes: u64,
    pub(crate) expires_at: Option<String>,
    pub(crate) max_uses: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawProfileRule {
    pub(crate) id: String,
    pub(crate) agent: Option<String>,
    pub(crate) profile: String,
    pub(crate) expires_at: Option<String>,
    pub(crate) max_uses: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawWriteRule {
    pub(crate) id: String,
    pub(crate) agent: Option<String>,
    pub(crate) operation: WriteOperationKind,
    pub(crate) path_prefix: String,
    pub(crate) max_files: u64,
    pub(crate) max_total_bytes: u64,
    pub(crate) max_file_bytes: u64,
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

fn parse_positive(field: &str, value: u64) -> Result<u64, String> {
    if value == 0 {
        return Err(format!("{field} must be at least 1"));
    }
    Ok(value)
}

fn parse_path_prefix(raw: String) -> Result<PathPrefix, String> {
    PathPrefix::parse(&raw)
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
        let id = require_non_empty("id", &raw.id)?;
        let agent = optional_non_empty("agent", raw.agent)?;
        let executable = require_non_empty("executable", &raw.executable)?;
        // Shell wrappers, control characters, and empty tokens are rejected
        // during rule creation and load.
        super::command::validate_executable(&executable)?;
        let argv = raw.argv;
        super::command::validate_argv_tokens(&argv)?;
        match raw.match_mode {
            MatchMode::ArgvExact => {}
            MatchMode::ArgvPrefix if argv.is_empty() => {
                return Err("argv_prefix requires at least one argv token".to_string());
            }
            MatchMode::ArgvPrefix => {}
        }
        Ok(Self {
            id,
            scope: TrustRuleScope {
                workspace,
                agent,
                expires_at: Some(parse_required_expiry(raw.expires_at)?),
                max_uses: Some(parse_required_uses(raw.max_uses)?),
            },
            executable,
            match_mode: raw.match_mode,
            argv,
        })
    }
}

impl McpRule {
    pub(crate) fn from_raw(raw: RawMcpRule, workspace: WorkspaceIdentity) -> Result<Self, String> {
        let id = require_non_empty("id", &raw.id)?;
        let agent = optional_non_empty("agent", raw.agent)?;
        parse_identity_fields(
            &raw.server,
            &raw.transport_identity,
            &raw.tool,
            raw.tool_schema_version,
        )?;
        let arguments_json = canonicalize_arguments_json(&raw.arguments_json)?;
        Ok(Self {
            id,
            scope: TrustRuleScope {
                workspace,
                agent,
                // MCP exact-invocation grants may cover write or execute
                // tools, so every mcp_allow is finite: expiry and a use
                // budget are mandatory (Phase 6 lifecycle).
                expires_at: Some(parse_required_expiry(raw.expires_at)?),
                max_uses: Some(parse_required_uses(raw.max_uses)?),
            },
            server: raw.server,
            transport_identity: raw.transport_identity,
            tool: raw.tool,
            tool_schema_version: raw.tool_schema_version,
            arguments_json,
        })
    }
}

impl ReadPathRule {
    pub(crate) fn from_raw(
        raw: RawReadPathRule,
        workspace: WorkspaceIdentity,
    ) -> Result<Self, String> {
        let id = require_non_empty("id", &raw.id)?;
        let agent = optional_non_empty("agent", raw.agent)?;
        let max_bytes = parse_positive("max_bytes", raw.max_bytes)?;
        Ok(Self {
            id,
            scope: TrustRuleScope {
                workspace,
                agent,
                expires_at: parse_optional_expiry(raw.expires_at)?,
                max_uses: parse_optional_uses(raw.max_uses)?,
            },
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
        let id = require_non_empty("id", &raw.id)?;
        let agent = optional_non_empty("agent", raw.agent)?;
        parse_identity_fields(
            &raw.server,
            &raw.transport_identity,
            &raw.tool,
            raw.tool_schema_version,
        )?;
        let max_bytes = parse_positive("max_bytes", raw.max_bytes)?;
        Ok(Self {
            id,
            scope: TrustRuleScope {
                workspace,
                agent,
                expires_at: parse_optional_expiry(raw.expires_at)?,
                max_uses: parse_optional_uses(raw.max_uses)?,
            },
            server: raw.server,
            transport_identity: raw.transport_identity,
            tool: raw.tool,
            tool_schema_version: raw.tool_schema_version,
            path_prefix: parse_path_prefix(raw.path_prefix)?,
            max_bytes,
        })
    }
}

impl ProfileRule {
    pub(crate) fn from_raw(
        raw: RawProfileRule,
        workspace: WorkspaceIdentity,
    ) -> Result<Self, String> {
        let id = require_non_empty("id", &raw.id)?;
        let agent = optional_non_empty("agent", raw.agent)?;
        let profile = require_non_empty("profile", &raw.profile)?;
        validate_no_control("profile", &profile)?;
        // Profile ids come from the application-owned curated registry only;
        // unknown ids are rejected rather than granted (Phase 4).
        if !super::profiles::is_known_profile(&profile) {
            return Err(format!("unknown curated profile id: {profile}"));
        }
        Ok(Self {
            id,
            scope: TrustRuleScope {
                workspace,
                agent,
                expires_at: Some(parse_required_expiry(raw.expires_at)?),
                max_uses: Some(parse_required_uses(raw.max_uses)?),
            },
            profile,
        })
    }
}

impl WriteRule {
    pub(crate) fn from_raw(
        raw: RawWriteRule,
        workspace: WorkspaceIdentity,
    ) -> Result<Self, String> {
        let id = require_non_empty("id", &raw.id)?;
        let agent = optional_non_empty("agent", raw.agent)?;
        let max_files = parse_positive("max_files", raw.max_files)?;
        let max_total_bytes = parse_positive("max_total_bytes", raw.max_total_bytes)?;
        let max_file_bytes = parse_positive("max_file_bytes", raw.max_file_bytes)?;
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
        Ok(Self {
            id,
            scope: TrustRuleScope {
                workspace,
                agent,
                expires_at: Some(parse_required_expiry(raw.expires_at)?),
                max_uses: Some(parse_required_uses(raw.max_uses)?),
            },
            operation: raw.operation,
            path_prefix: parse_path_prefix(raw.path_prefix)?,
            max_files,
            max_total_bytes,
            max_file_bytes,
        })
    }
}

// ── Domain → raw (canonical schema order) ────────────────────────────────────

impl From<&CommandRule> for RawCommandRule {
    fn from(rule: &CommandRule) -> Self {
        Self {
            id: rule.id.clone(),
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
            agent: rule.scope.agent.clone(),
            server: rule.server.clone(),
            transport_identity: rule.transport_identity.clone(),
            tool: rule.tool.clone(),
            tool_schema_version: rule.tool_schema_version,
            arguments_json: rule.arguments_json.clone(),
            expires_at: rule.scope.expires_at.map(format_expiry),
            max_uses: rule.scope.max_uses,
        }
    }
}

impl From<&ReadPathRule> for RawReadPathRule {
    fn from(rule: &ReadPathRule) -> Self {
        Self {
            id: rule.id.clone(),
            agent: rule.scope.agent.clone(),
            path_prefix: rule.path_prefix.display().to_string(),
            max_bytes: rule.max_bytes,
            expires_at: rule.scope.expires_at.map(format_expiry),
            max_uses: rule.scope.max_uses,
        }
    }
}

impl From<&McpReadRule> for RawMcpReadRule {
    fn from(rule: &McpReadRule) -> Self {
        Self {
            id: rule.id.clone(),
            agent: rule.scope.agent.clone(),
            server: rule.server.clone(),
            transport_identity: rule.transport_identity.clone(),
            tool: rule.tool.clone(),
            tool_schema_version: rule.tool_schema_version,
            path_prefix: rule.path_prefix.display().to_string(),
            max_bytes: rule.max_bytes,
            expires_at: rule.scope.expires_at.map(format_expiry),
            max_uses: rule.scope.max_uses,
        }
    }
}

impl From<&ProfileRule> for RawProfileRule {
    fn from(rule: &ProfileRule) -> Self {
        Self {
            id: rule.id.clone(),
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
            agent: rule.scope.agent.clone(),
            operation: rule.operation,
            path_prefix: rule.path_prefix.display().to_string(),
            max_files: rule.max_files,
            max_total_bytes: rule.max_total_bytes,
            max_file_bytes: rule.max_file_bytes,
            expires_at: rule.scope.expires_at.map(format_expiry),
            max_uses: rule.scope.max_uses,
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
