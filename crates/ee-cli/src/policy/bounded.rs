//! Bounded structured allow-rule extraction and authority previews.
//!
//! Candidates derive only from normalized typed operations. UI selects one
//! application-owned candidate; candidate rule and preview then stay immutable
//! through explicit confirmation and atomic persistence.

use std::time::{Duration, SystemTime};

use super::{
    BrowserActionClass, CommandInvocation, CommandRule, MatchMode, McpInvocation, McpRule,
    NetworkMethodClass, NetworkRule, NetworkScheme, PathPrefix, TrustEffect, TrustRule,
    TrustRuleScope, WriteOperationKind, WriteRule, generate_command_rule_id, generate_mcp_rule_id,
    generate_network_rule_id, generate_write_rule_id, validate_command_tokens,
};

pub(crate) const EXECUTE_GRANT_DURATION: Duration = Duration::from_secs(60 * 60);
pub(crate) const EXECUTE_GRANT_MAX_USES: u64 = 20;
pub(crate) const EXECUTE_SHORT_GRANT_DURATION: Duration = Duration::from_secs(10 * 60);
pub(crate) const EXECUTE_SHORT_GRANT_MAX_USES: u64 = 5;
pub(crate) const WRITE_GRANT_DURATION: Duration = Duration::from_secs(60 * 60);
pub(crate) const WRITE_GRANT_MAX_USES: u64 = 5;
pub(crate) const WRITE_SHORT_GRANT_DURATION: Duration = Duration::from_secs(10 * 60);
pub(crate) const WRITE_SHORT_GRANT_MAX_USES: u64 = 1;
pub(crate) const NETWORK_GRANT_DURATION: Duration = Duration::from_secs(60 * 60);
pub(crate) const NETWORK_GRANT_MAX_USES: u64 = 20;
pub(crate) const NETWORK_SHORT_GRANT_DURATION: Duration = Duration::from_secs(10 * 60);
pub(crate) const NETWORK_SHORT_GRANT_MAX_USES: u64 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoundedRuleKind {
    Exact,
    StructuredPrefix { argument_count: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundedRulePreview {
    pub(crate) workspace: String,
    pub(crate) agent: String,
    pub(crate) matcher_fields: Vec<(String, String)>,
    pub(crate) expires_at: SystemTime,
    pub(crate) max_uses: u64,
    pub(crate) caps: Vec<(String, String)>,
    pub(crate) transport_identity: Option<String>,
    pub(crate) exclusions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundedRuleCandidate {
    pub(crate) kind: BoundedRuleKind,
    pub(crate) rule: TrustRule,
    pub(crate) preview: BoundedRulePreview,
}

impl BoundedRulePreview {
    /// Ordered authority fields used verbatim by UI and snapshot tests.
    pub(crate) fn authority_fields(&self) -> Vec<(String, String)> {
        let expiry: chrono::DateTime<chrono::Utc> = self.expires_at.into();
        [
            ("effect".to_string(), "allow".to_string()),
            ("workspace".to_string(), self.workspace.clone()),
            ("agent".to_string(), self.agent.clone()),
        ]
        .into_iter()
        .chain(self.matcher_fields.iter().cloned())
        .chain(std::iter::once((
            "expires".to_string(),
            expiry.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        )))
        .chain(std::iter::once(("maximum uses".to_string(), self.max_uses.to_string())))
        .chain(self.caps.iter().cloned())
        .chain(self.transport_identity.iter().cloned().map(|value| ("transport".into(), value)))
        .chain(std::iter::once(("excludes".to_string(), self.exclusions.join(", "))))
        .collect()
    }
}

impl BoundedRuleCandidate {
    pub(crate) fn command_exact(
        invocation: &CommandInvocation,
        agent: Option<&str>,
        now: SystemTime,
    ) -> Result<Self, String> {
        command_candidate(
            invocation,
            agent,
            now,
            MatchMode::ArgvExact,
            invocation.argv.len(),
            EXECUTE_GRANT_DURATION,
            EXECUTE_GRANT_MAX_USES,
        )
    }

    pub(crate) fn command_exact_short(
        invocation: &CommandInvocation,
        agent: Option<&str>,
        now: SystemTime,
    ) -> Result<Self, String> {
        command_candidate(
            invocation,
            agent,
            now,
            MatchMode::ArgvExact,
            invocation.argv.len(),
            EXECUTE_SHORT_GRANT_DURATION,
            EXECUTE_SHORT_GRANT_MAX_USES,
        )
    }

    pub(crate) fn command_prefix(
        invocation: &CommandInvocation,
        agent: Option<&str>,
        argument_count: usize,
        now: SystemTime,
    ) -> Result<Self, String> {
        if argument_count == 0 || argument_count > invocation.argv.len() {
            return Err("command prefix must contain at least one complete argument".into());
        }
        command_candidate(
            invocation,
            agent,
            now,
            MatchMode::ArgvPrefix,
            argument_count,
            EXECUTE_GRANT_DURATION,
            EXECUTE_GRANT_MAX_USES,
        )
    }

    pub(crate) fn command_prefix_short(
        invocation: &CommandInvocation,
        agent: Option<&str>,
        argument_count: usize,
        now: SystemTime,
    ) -> Result<Self, String> {
        if argument_count == 0 || argument_count > invocation.argv.len() {
            return Err("command prefix must contain at least one complete argument".into());
        }
        command_candidate(
            invocation,
            agent,
            now,
            MatchMode::ArgvPrefix,
            argument_count,
            EXECUTE_SHORT_GRANT_DURATION,
            EXECUTE_SHORT_GRANT_MAX_USES,
        )
    }

    pub(crate) fn mcp_exact(
        invocation: &McpInvocation,
        agent: Option<&str>,
        now: SystemTime,
    ) -> Result<Self, String> {
        if invocation.server.is_empty()
            || invocation.transport_identity.is_empty()
            || invocation.tool.is_empty()
            || invocation.tool_schema_version == 0
            || invocation.arguments_json.is_empty()
        {
            return Err("MCP allow requires complete exact identity".into());
        }
        Self::mcp_exact_with_limits(
            invocation,
            agent,
            now,
            EXECUTE_GRANT_DURATION,
            EXECUTE_GRANT_MAX_USES,
        )
    }

    pub(crate) fn mcp_exact_short(
        invocation: &McpInvocation,
        agent: Option<&str>,
        now: SystemTime,
    ) -> Result<Self, String> {
        Self::mcp_exact_with_limits(
            invocation,
            agent,
            now,
            EXECUTE_SHORT_GRANT_DURATION,
            EXECUTE_SHORT_GRANT_MAX_USES,
        )
    }

    fn mcp_exact_with_limits(
        invocation: &McpInvocation,
        agent: Option<&str>,
        now: SystemTime,
        duration: Duration,
        max_uses: u64,
    ) -> Result<Self, String> {
        if invocation.server.is_empty()
            || invocation.transport_identity.is_empty()
            || invocation.tool.is_empty()
            || invocation.tool_schema_version == 0
            || invocation.arguments_json.is_empty()
        {
            return Err("MCP allow requires complete exact identity".into());
        }
        let expires_at = now + duration;
        let scope = bounded_scope(invocation.workspace, agent, expires_at, max_uses);
        let rule = TrustRule::Mcp(McpRule {
            id: generate_mcp_rule_id(),
            effect: TrustEffect::Allow,
            scope,
            server: invocation.server.clone(),
            transport_identity: invocation.transport_identity.clone(),
            tool: invocation.tool.clone(),
            tool_schema_version: invocation.tool_schema_version,
            arguments_json: invocation.arguments_json.clone(),
        });
        Ok(Self {
            kind: BoundedRuleKind::Exact,
            preview: preview(
                invocation.workspace.as_string(),
                agent,
                vec![
                    ("kind".into(), "mcp exact".into()),
                    ("server".into(), invocation.server.clone()),
                    ("tool".into(), invocation.tool.clone()),
                    ("schema".into(), invocation.tool_schema_version.to_string()),
                    ("arguments".into(), "exact canonical JSON".into()),
                ],
                expires_at,
                max_uses,
                vec![("result cap".into(), "application tool limit".into())],
                Some(invocation.transport_identity.clone()),
                vec![
                    "argument changes".into(),
                    "schema changes".into(),
                    "transport changes".into(),
                ],
            ),
            rule,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn write_prefix(
        workspace: super::WorkspaceIdentity,
        agent: Option<&str>,
        operation: WriteOperationKind,
        path_prefix: PathPrefix,
        max_files: u64,
        max_total_bytes: u64,
        max_file_bytes: u64,
        now: SystemTime,
    ) -> Result<Self, String> {
        Self::write_prefix_with_limits(
            workspace,
            agent,
            operation,
            path_prefix,
            max_files,
            max_total_bytes,
            max_file_bytes,
            now,
            WRITE_GRANT_DURATION,
            WRITE_GRANT_MAX_USES,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn write_prefix_short(
        workspace: super::WorkspaceIdentity,
        agent: Option<&str>,
        operation: WriteOperationKind,
        path_prefix: PathPrefix,
        max_files: u64,
        max_total_bytes: u64,
        max_file_bytes: u64,
        now: SystemTime,
    ) -> Result<Self, String> {
        Self::write_prefix_with_limits(
            workspace,
            agent,
            operation,
            path_prefix,
            max_files,
            max_total_bytes,
            max_file_bytes,
            now,
            WRITE_SHORT_GRANT_DURATION,
            WRITE_SHORT_GRANT_MAX_USES,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn write_prefix_with_limits(
        workspace: super::WorkspaceIdentity,
        agent: Option<&str>,
        operation: WriteOperationKind,
        path_prefix: PathPrefix,
        max_files: u64,
        max_total_bytes: u64,
        max_file_bytes: u64,
        now: SystemTime,
        duration: Duration,
        max_uses: u64,
    ) -> Result<Self, String> {
        if max_files == 0 || max_total_bytes == 0 || max_file_bytes == 0 {
            return Err("write allow requires non-zero request caps".into());
        }
        let expires_at = now + duration;
        let prefix = path_prefix.display().to_string();
        let rule = TrustRule::Write(WriteRule {
            id: generate_write_rule_id(),
            effect: TrustEffect::Allow,
            scope: bounded_scope(workspace, agent, expires_at, max_uses),
            operation,
            path_prefix,
            max_files,
            max_total_bytes,
            max_file_bytes,
        });
        Ok(Self {
            kind: BoundedRuleKind::StructuredPrefix { argument_count: 0 },
            preview: preview(
                workspace.as_string(),
                agent,
                vec![
                    ("kind".into(), "workspace path prefix".into()),
                    ("operation".into(), format!("{operation:?}").to_ascii_lowercase()),
                    ("path prefix".into(), prefix),
                ],
                expires_at,
                max_uses,
                vec![
                    ("maximum files".into(), max_files.to_string()),
                    ("maximum total bytes".into(), max_total_bytes.to_string()),
                    ("maximum file bytes".into(), max_file_bytes.to_string()),
                ],
                None,
                vec!["workspace root".into(), "protected paths".into(), "path traversal".into()],
            ),
            rule,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn network_exact_read(
        workspace: super::WorkspaceIdentity,
        agent: Option<&str>,
        scheme: NetworkScheme,
        host: String,
        port: u16,
        method: NetworkMethodClass,
        browser_action: BrowserActionClass,
        now: SystemTime,
    ) -> Result<Self, String> {
        Self::network_exact_read_with_limits(
            workspace,
            agent,
            scheme,
            host,
            port,
            method,
            browser_action,
            now,
            NETWORK_GRANT_DURATION,
            NETWORK_GRANT_MAX_USES,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn network_exact_read_short(
        workspace: super::WorkspaceIdentity,
        agent: Option<&str>,
        scheme: NetworkScheme,
        host: String,
        port: u16,
        method: NetworkMethodClass,
        browser_action: BrowserActionClass,
        now: SystemTime,
    ) -> Result<Self, String> {
        Self::network_exact_read_with_limits(
            workspace,
            agent,
            scheme,
            host,
            port,
            method,
            browser_action,
            now,
            NETWORK_SHORT_GRANT_DURATION,
            NETWORK_SHORT_GRANT_MAX_USES,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn network_exact_read_with_limits(
        workspace: super::WorkspaceIdentity,
        agent: Option<&str>,
        scheme: NetworkScheme,
        host: String,
        port: u16,
        method: NetworkMethodClass,
        browser_action: BrowserActionClass,
        now: SystemTime,
        duration: Duration,
        max_uses: u64,
    ) -> Result<Self, String> {
        if method != NetworkMethodClass::Read
            || !matches!(browser_action, BrowserActionClass::Fetch | BrowserActionClass::Navigate)
        {
            return Err("network allow supports read-only method and action classes only".into());
        }
        let expires_at = now + duration;
        let rule = TrustRule::Network(NetworkRule::allow_exact(
            generate_network_rule_id(),
            bounded_scope(workspace, agent, expires_at, max_uses),
            scheme,
            host.clone(),
            port,
            method,
            browser_action,
        )?);
        Ok(Self {
            kind: BoundedRuleKind::Exact,
            preview: preview(
                workspace.as_string(),
                agent,
                vec![
                    ("kind".into(), "network exact host".into()),
                    ("scheme".into(), format!("{scheme:?}").to_ascii_lowercase()),
                    ("host".into(), host),
                    ("port".into(), port.to_string()),
                    ("method class".into(), "read".into()),
                    ("browser action".into(), format!("{browser_action:?}").to_ascii_lowercase()),
                ],
                expires_at,
                max_uses,
                vec![("result cap".into(), "application network limit".into())],
                None,
                vec!["other hosts".into(), "redirect hosts".into(), "write/connect actions".into()],
            ),
            rule,
        })
    }
}

fn command_candidate(
    invocation: &CommandInvocation,
    agent: Option<&str>,
    now: SystemTime,
    match_mode: MatchMode,
    argument_count: usize,
    duration: Duration,
    max_uses: u64,
) -> Result<BoundedRuleCandidate, String> {
    validate_command_tokens(&invocation.executable, &invocation.argv)?;
    let argv = invocation.argv[..argument_count].to_vec();
    if match_mode == MatchMode::ArgvPrefix && argv.is_empty() {
        return Err("command prefix must contain at least one complete argument".into());
    }
    let expires_at = now + duration;
    let kind = match match_mode {
        MatchMode::ArgvExact => BoundedRuleKind::Exact,
        MatchMode::ArgvPrefix => BoundedRuleKind::StructuredPrefix { argument_count },
    };
    let mode = match match_mode {
        MatchMode::ArgvExact => "exact",
        MatchMode::ArgvPrefix => "token prefix",
    };
    let rule = TrustRule::Command(CommandRule {
        id: generate_command_rule_id(),
        effect: TrustEffect::Allow,
        scope: bounded_scope(invocation.workspace, agent, expires_at, max_uses),
        executable: invocation.executable.clone(),
        match_mode,
        argv,
    });
    Ok(BoundedRuleCandidate {
        kind,
        preview: preview(
            invocation.workspace.as_string(),
            agent,
            vec![
                ("kind".into(), "command".into()),
                ("executable".into(), invocation.executable.clone()),
                ("arguments".into(), format!("{mode} · {argument_count} tokens")),
                ("cwd scope".into(), "any canonical directory in workspace".into()),
            ],
            expires_at,
            max_uses,
            vec![("terminal output bytes".into(), "1048576".into())],
            None,
            vec!["shell wrappers".into(), "environment".into(), "different executable".into()],
        ),
        rule,
    })
}

fn bounded_scope(
    workspace: super::WorkspaceIdentity,
    agent: Option<&str>,
    expires_at: SystemTime,
    max_uses: u64,
) -> TrustRuleScope {
    TrustRuleScope {
        workspace,
        agent: agent.map(str::to_string),
        expires_at: Some(expires_at),
        max_uses: Some(max_uses),
    }
}

#[allow(clippy::too_many_arguments)]
fn preview(
    workspace: String,
    agent: Option<&str>,
    matcher_fields: Vec<(String, String)>,
    expires_at: SystemTime,
    max_uses: u64,
    caps: Vec<(String, String)>,
    transport_identity: Option<String>,
    exclusions: Vec<String>,
) -> BoundedRulePreview {
    BoundedRulePreview {
        workspace,
        agent: agent.unwrap_or("all agents").to_string(),
        matcher_fields,
        expires_at,
        max_uses,
        caps,
        transport_identity,
        exclusions,
    }
}
