//! Always-allow rule extraction and authority previews.
//!
//! Candidates derive only from normalized typed operations. UI selects one
//! application-owned candidate; candidate rule and preview then stay immutable
//! through explicit confirmation and atomic persistence.
//!
//! Always-allow rules carry no expiry and no use budget. They are workspace
//! scope only (`agent: None`) and are offered only while the host-local
//! workspace trust decision is `trusted`; the approval layer owns that gate.

use super::{
    BrowserActionClass, CommandInvocation, CommandRule, MatchMode, McpInvocation, McpRule,
    NetworkMethodClass, NetworkRule, NetworkScheme, PathPrefix, TrustEffect, TrustRule,
    TrustRuleScope, WriteOperationKind, WriteRule, generate_command_rule_id, generate_mcp_rule_id,
    generate_network_rule_id, generate_write_rule_id, validate_command_tokens,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AlwaysRuleKind {
    Exact,
    StructuredPrefix { argument_count: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AlwaysRulePreview {
    pub(crate) workspace: String,
    pub(crate) agent: String,
    pub(crate) matcher_fields: Vec<(String, String)>,
    pub(crate) caps: Vec<(String, String)>,
    pub(crate) transport_identity: Option<String>,
    pub(crate) exclusions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AlwaysRuleCandidate {
    pub(crate) kind: AlwaysRuleKind,
    pub(crate) rule: TrustRule,
    pub(crate) preview: AlwaysRulePreview,
}

impl AlwaysRulePreview {
    /// Ordered authority fields used verbatim by UI and snapshot tests. The
    /// preview shows the pattern that would be allowed always: scope, matcher
    /// fields, request caps, transport, and exclusions. No expiry or use
    /// budget is shown because always-allow rules carry none.
    pub(crate) fn authority_fields(&self) -> Vec<(String, String)> {
        [
            ("effect".to_string(), "allow always".to_string()),
            ("workspace".to_string(), self.workspace.clone()),
            ("agent".to_string(), self.agent.clone()),
        ]
        .into_iter()
        .chain(self.matcher_fields.iter().cloned())
        .chain(self.caps.iter().cloned())
        .chain(self.transport_identity.iter().cloned().map(|value| ("transport".into(), value)))
        .chain(std::iter::once(("excludes".to_string(), self.exclusions.join(", "))))
        .collect()
    }
}

impl AlwaysRuleCandidate {
    pub(crate) fn command_exact(invocation: &CommandInvocation) -> Result<Self, String> {
        command_candidate(invocation, MatchMode::ArgvExact, invocation.argv.len())
    }

    pub(crate) fn command_prefix(
        invocation: &CommandInvocation,
        argument_count: usize,
    ) -> Result<Self, String> {
        if argument_count == 0 || argument_count > invocation.argv.len() {
            return Err("command prefix must contain at least one complete argument".into());
        }
        command_candidate(invocation, MatchMode::ArgvPrefix, argument_count)
    }

    pub(crate) fn mcp_exact(invocation: &McpInvocation) -> Result<Self, String> {
        if invocation.server.is_empty()
            || invocation.transport_identity.is_empty()
            || invocation.tool.is_empty()
            || invocation.tool_schema_version == 0
            || invocation.arguments_json.is_empty()
        {
            return Err("MCP allow requires complete exact identity".into());
        }
        let rule = TrustRule::Mcp(McpRule {
            id: generate_mcp_rule_id(),
            effect: TrustEffect::Allow,
            scope: always_scope(invocation.workspace),
            server: invocation.server.clone(),
            transport_identity: invocation.transport_identity.clone(),
            tool: invocation.tool.clone(),
            tool_schema_version: invocation.tool_schema_version,
            arguments_json: invocation.arguments_json.clone(),
        });
        Ok(Self {
            kind: AlwaysRuleKind::Exact,
            preview: preview(
                invocation.workspace.as_string(),
                vec![
                    ("kind".into(), "mcp exact".into()),
                    ("server".into(), invocation.server.clone()),
                    ("tool".into(), invocation.tool.clone()),
                    ("schema".into(), invocation.tool_schema_version.to_string()),
                    ("arguments".into(), "exact canonical JSON".into()),
                ],
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
        operation: WriteOperationKind,
        path_prefix: PathPrefix,
        max_files: u64,
        max_total_bytes: u64,
        max_file_bytes: u64,
    ) -> Result<Self, String> {
        if max_files == 0 || max_total_bytes == 0 || max_file_bytes == 0 {
            return Err("write allow requires non-zero request caps".into());
        }
        let prefix = path_prefix.display().to_string();
        let rule = TrustRule::Write(WriteRule {
            id: generate_write_rule_id(),
            effect: TrustEffect::Allow,
            scope: always_scope(workspace),
            operation,
            path_prefix,
            max_files,
            max_total_bytes,
            max_file_bytes,
        });
        Ok(Self {
            kind: AlwaysRuleKind::StructuredPrefix { argument_count: 0 },
            preview: preview(
                workspace.as_string(),
                vec![
                    ("kind".into(), "workspace path prefix".into()),
                    ("operation".into(), format!("{operation:?}").to_ascii_lowercase()),
                    ("path prefix".into(), prefix),
                ],
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
        scheme: NetworkScheme,
        host: String,
        port: u16,
        method: NetworkMethodClass,
        browser_action: BrowserActionClass,
    ) -> Result<Self, String> {
        if method != NetworkMethodClass::Read
            || !matches!(browser_action, BrowserActionClass::Fetch | BrowserActionClass::Navigate)
        {
            return Err("network allow supports read-only method and action classes only".into());
        }
        let rule = TrustRule::Network(NetworkRule::allow_exact(
            generate_network_rule_id(),
            always_scope(workspace),
            scheme,
            host.clone(),
            port,
            method,
            browser_action,
        )?);
        Ok(Self {
            kind: AlwaysRuleKind::Exact,
            preview: preview(
                workspace.as_string(),
                vec![
                    ("kind".into(), "network exact host".into()),
                    ("scheme".into(), format!("{scheme:?}").to_ascii_lowercase()),
                    ("host".into(), host),
                    ("port".into(), port.to_string()),
                    ("method class".into(), "read".into()),
                    ("browser action".into(), format!("{browser_action:?}").to_ascii_lowercase()),
                ],
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
    match_mode: MatchMode,
    argument_count: usize,
) -> Result<AlwaysRuleCandidate, String> {
    validate_command_tokens(&invocation.executable, &invocation.argv)?;
    let argv = invocation.argv[..argument_count].to_vec();
    if match_mode == MatchMode::ArgvPrefix && argv.is_empty() {
        return Err("command prefix must contain at least one complete argument".into());
    }
    let kind = match match_mode {
        MatchMode::ArgvExact => AlwaysRuleKind::Exact,
        MatchMode::ArgvPrefix => AlwaysRuleKind::StructuredPrefix { argument_count },
    };
    let mode = match match_mode {
        MatchMode::ArgvExact => "exact",
        MatchMode::ArgvPrefix => "token prefix",
    };
    let rule = TrustRule::Command(CommandRule {
        id: generate_command_rule_id(),
        effect: TrustEffect::Allow,
        scope: always_scope(invocation.workspace),
        executable: invocation.executable.clone(),
        match_mode,
        argv,
    });
    Ok(AlwaysRuleCandidate {
        kind,
        preview: preview(
            invocation.workspace.as_string(),
            vec![
                ("kind".into(), "command".into()),
                ("executable".into(), invocation.executable.clone()),
                ("arguments".into(), format!("{mode} · {argument_count} tokens")),
                ("cwd scope".into(), "any canonical directory in workspace".into()),
            ],
            vec![("terminal output bytes".into(), "1048576".into())],
            None,
            vec!["shell wrappers".into(), "environment".into(), "different executable".into()],
        ),
        rule,
    })
}

/// Workspace-wide scope for always-allow rules: no agent binding, no expiry,
/// no use budget.
fn always_scope(workspace: super::WorkspaceIdentity) -> TrustRuleScope {
    TrustRuleScope { workspace, agent: None, expires_at: None, max_uses: None }
}

fn preview(
    workspace: String,
    matcher_fields: Vec<(String, String)>,
    caps: Vec<(String, String)>,
    transport_identity: Option<String>,
    exclusions: Vec<String>,
) -> AlwaysRulePreview {
    AlwaysRulePreview {
        workspace,
        agent: "all agents".to_string(),
        matcher_fields,
        caps,
        transport_identity,
        exclusions,
    }
}
