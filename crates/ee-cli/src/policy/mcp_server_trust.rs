//! Host-local MCP server trust grants.
//!
//! A grant is an authority ceiling for one pinned MCP server transport.  It
//! never trusts whatever a server advertises at runtime: the persisted tool
//! list is generated from the application-owned `ee` manifest, and matching
//! requires the exact server, transport identity, tool name, schema version,
//! and side-effect category.
//!
//! Workspace trust remains the outer gate.  The app only injects this value
//! while the host-local workspace decision is `trusted`; malformed or stale
//! grants never match and therefore fall back to the normal approval path.

use super::always::AlwaysRulePreview;
use super::{
    McpInvocation, OperationIdentity, TransportKind, TrustCategory, TrustOperation,
    WorkspaceIdentity,
};

/// One pinned tool inside a server grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpServerToolGrant {
    pub(crate) tool: String,
    pub(crate) category: TrustCategory,
    pub(crate) tool_schema_version: u64,
    /// Execute pins only: accept any `ee_terminal_create` command text instead
    /// of requiring a validated simple argv.
    pub(crate) unrestricted: bool,
}

impl McpServerToolGrant {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.tool.is_empty() || self.tool.len() > 160 {
            return Err("mcp server tool name must be 1 to 160 bytes".to_string());
        }
        if self.tool.chars().any(|character| character.is_control()) {
            return Err("mcp server tool name contains control characters".to_string());
        }
        if self.tool_schema_version == 0 {
            return Err("mcp server tool schema version must be at least 1".to_string());
        }
        if self.unrestricted && self.category != TrustCategory::Execute {
            return Err("unrestricted pins are allowed only for execute tools".to_string());
        }
        match self.category {
            TrustCategory::Read
            | TrustCategory::WriteCreate
            | TrustCategory::WriteModify
            | TrustCategory::Delete => {}
            TrustCategory::Execute if self.tool == "ee_terminal_create" => {}
            TrustCategory::Execute => {
                return Err(
                    "mcp server grants can cover execute only for ee_terminal_create".to_string()
                );
            }
            TrustCategory::Network | TrustCategory::Unknown => {
                return Err(format!(
                    "mcp server grants cannot cover {} tools",
                    self.category.as_str()
                ));
            }
        }
        Ok(())
    }
}

/// One pinned server grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpServerGrant {
    pub(crate) id: String,
    pub(crate) workspace: WorkspaceIdentity,
    pub(crate) server: String,
    pub(crate) transport_identity: String,
    pub(crate) granted_at: String,
    pub(crate) tools: Vec<McpServerToolGrant>,
}

impl McpServerGrant {
    pub(crate) fn new(
        id: String,
        workspace: WorkspaceIdentity,
        server: String,
        transport_identity: String,
        granted_at: String,
        mut tools: Vec<McpServerToolGrant>,
    ) -> Result<Self, String> {
        crate::policy::rules::validate_rule_id(&id)?;
        validate_server(&server)?;
        validate_transport_identity(&transport_identity)?;
        validate_granted_at(&granted_at)?;
        tools.sort_by(|left, right| left.tool.cmp(&right.tool));
        if tools.windows(2).any(|pair| pair[0].tool == pair[1].tool) {
            return Err("mcp server grant contains a duplicate tool".to_string());
        }
        if tools.is_empty() {
            return Err("mcp server grant needs at least one pinned tool".to_string());
        }
        for tool in &tools {
            tool.validate()?;
        }
        Ok(Self { id, workspace, server, transport_identity, granted_at, tools })
    }

    pub(crate) fn matches(&self, operation: &TrustOperation) -> bool {
        if self.workspace != operation.workspace {
            return false;
        }
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
        if self.server != *server || self.transport_identity != *transport_identity {
            return false;
        }
        self.tools.iter().any(|pin| {
            pin.tool == *tool
                && pin.tool_schema_version == *tool_schema_version
                && pin.category == operation.category
        })
    }

    pub(crate) fn tool_count(&self) -> usize {
        self.tools.len()
    }

    /// Whether this grant carries the explicit terminal-execution opt-in.
    pub(crate) fn includes_execution(&self) -> bool {
        self.tools.iter().any(|tool| tool.category == TrustCategory::Execute)
    }

    /// Whether terminal execution accepts arbitrary shell command text.
    pub(crate) fn includes_unrestricted_execution(&self) -> bool {
        self.tools.iter().any(|tool| tool.category == TrustCategory::Execute && tool.unrestricted)
    }

    /// Preserves execute pins from a replaced grant. Re-trusting a server from
    /// a non-execution prompt must never silently drop terminal trust; an
    /// existing unrestricted pin stays unrestricted, and a full revoke remains
    /// the explicit way to remove it.
    pub(crate) fn preserve_execute_pins(&mut self, existing: &Self) {
        for pin in &existing.tools {
            if pin.category != TrustCategory::Execute {
                continue;
            }
            if let Some(current) = self.tools.iter_mut().find(|tool| tool.tool == pin.tool) {
                current.unrestricted |= pin.unrestricted;
                continue;
            }
            self.tools.push(pin.clone());
        }
        self.tools.sort_by(|left, right| left.tool.cmp(&right.tool));
    }
}

/// Effective MCP server trust for one workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct McpServerTrust {
    grants: Vec<McpServerGrant>,
}

impl McpServerTrust {
    pub(crate) fn new(mut grants: Vec<McpServerGrant>) -> Self {
        grants.sort_by(|left, right| left.id.cmp(&right.id));
        Self { grants }
    }

    pub(crate) fn empty() -> Self {
        Self { grants: Vec::new() }
    }

    pub(crate) fn grants(&self) -> &[McpServerGrant] {
        &self.grants
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }

    pub(crate) fn allows(&self, operation: &TrustOperation) -> Option<&McpServerGrant> {
        self.grants.iter().find(|grant| grant.matches(operation))
    }

    pub(crate) fn find(&self, server: &str, transport_identity: &str) -> Option<&McpServerGrant> {
        self.grants
            .iter()
            .find(|grant| grant.server == server && grant.transport_identity == transport_identity)
    }
}

/// Previewed UI candidate for a server-wide trust grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpServerTrustCandidate {
    pub(crate) grant: McpServerGrant,
    pub(crate) preview: AlwaysRulePreview,
    /// True when this candidate adds the explicit terminal-execution opt-in.
    pub(crate) execution: bool,
}

impl McpServerTrustCandidate {
    /// Builds the pinned grant for one validated invocation's transport.
    pub(crate) fn for_invocation(invocation: &McpInvocation) -> Result<Self, String> {
        if invocation.server != "ee" {
            return Err("server trust is available only for the pinned ee manifest".to_string());
        }
        let transport = match invocation.transport {
            TransportKind::McpStdio => ee_mcp::ToolTransport::Stdio,
            TransportKind::McpAcp => ee_mcp::ToolTransport::Acp,
            TransportKind::Acp => {
                return Err("server trust needs an MCP transport".to_string());
            }
        };
        let tools = pinned_server_tools(transport);
        if tools.is_empty() {
            return Err("server trust has no pinned tools for this transport".to_string());
        }
        if !tools.iter().any(|tool| {
            tool.tool == invocation.tool
                && tool.category == invocation.category
                && tool.tool_schema_version == invocation.tool_schema_version
        }) {
            return Err("invoking tool is not covered by a server grant".to_string());
        }
        let grant = McpServerGrant::new(
            generate_mcp_server_grant_id(),
            invocation.workspace,
            invocation.server.clone(),
            invocation.transport_identity.clone(),
            current_granted_at(),
            tools,
        )?;
        let read_count =
            grant.tools.iter().filter(|tool| tool.category == TrustCategory::Read).count();
        let write_count = grant.tool_count().saturating_sub(read_count);
        let preview = AlwaysRulePreview {
            workspace: grant.workspace.as_string(),
            agent: "all agents".to_string(),
            matcher_fields: vec![
                ("kind".into(), "mcp server".into()),
                ("server".into(), grant.server.clone()),
                ("pinned tools".into(), grant.tool_count().to_string()),
                ("read tools".into(), read_count.to_string()),
                ("workspace write tools".into(), write_count.to_string()),
                (
                    "invalidates on".into(),
                    "tool list, schema, transport, or workspace trust change".into(),
                ),
            ],
            caps: vec![
                ("result caps".into(), "application tool limits".into()),
                ("write caps".into(), "workspace write safety maxima".into()),
            ],
            transport_identity: Some(grant.transport_identity.clone()),
            exclusions: vec![
                "terminal execution".into(),
                "external network tools".into(),
                "new tools until re-trusted".into(),
            ],
        };
        Ok(Self { grant, preview, execution: false })
    }

    /// Builds the pinned grant that adds the explicit terminal-execution
    /// opt-in for `ee_terminal_create` on one MCP transport.
    pub(crate) fn for_execution_invocation(invocation: &McpInvocation) -> Result<Self, String> {
        if invocation.server != "ee" {
            return Err("server trust is available only for the pinned ee manifest".to_string());
        }
        let transport = match invocation.transport {
            TransportKind::McpStdio => ee_mcp::ToolTransport::Stdio,
            TransportKind::McpAcp => ee_mcp::ToolTransport::Acp,
            TransportKind::Acp => {
                return Err("server trust needs an MCP transport".to_string());
            }
        };
        let execution = pinned_execution_tools(transport);
        if execution.is_empty() {
            return Err("terminal execution is not available for this transport".to_string());
        }
        if !execution.iter().any(|tool| {
            tool.tool == invocation.tool
                && tool.category == invocation.category
                && tool.tool_schema_version == invocation.tool_schema_version
        }) {
            return Err("invoking tool is not covered by terminal execution trust".to_string());
        }
        let mut tools = pinned_server_tools(transport);
        tools.extend(execution);
        tools.sort_by(|left, right| left.tool.cmp(&right.tool));
        tools.dedup_by(|left, right| left.tool == right.tool);
        let grant = McpServerGrant::new(
            generate_mcp_server_grant_id(),
            invocation.workspace,
            invocation.server.clone(),
            invocation.transport_identity.clone(),
            current_granted_at(),
            tools,
        )?;
        let read_count =
            grant.tools.iter().filter(|tool| tool.category == TrustCategory::Read).count();
        let write_count = grant
            .tools
            .iter()
            .filter(|tool| {
                matches!(
                    tool.category,
                    TrustCategory::WriteCreate | TrustCategory::WriteModify | TrustCategory::Delete
                )
            })
            .count();
        let preview = AlwaysRulePreview {
            workspace: grant.workspace.as_string(),
            agent: "all agents".to_string(),
            matcher_fields: vec![
                ("kind".into(), "mcp server + terminal".into()),
                ("server".into(), grant.server.clone()),
                ("scope".into(), "ee_terminal_create".into()),
                ("commands".into(), "validated argv only".into()),
                ("read tools".into(), read_count.to_string()),
                ("workspace write tools".into(), write_count.to_string()),
                (
                    "invalidates on".into(),
                    "tool list, schema, transport, or workspace trust change".into(),
                ),
            ],
            caps: vec![
                ("command safeguards".into(), "application-owned".into()),
                ("write caps".into(), "workspace write safety maxima".into()),
            ],
            transport_identity: Some(grant.transport_identity.clone()),
            exclusions: vec![
                "shell wrappers and external cwd".into(),
                "other execute tools".into(),
                "external network tools".into(),
                "new tools until re-trusted".into(),
            ],
        };
        Ok(Self { grant, preview, execution: true })
    }
}

/// Pinned tools eligible for a server grant on one transport.
///
/// Eligibility is application-owned:
/// - read tools that require no separate approval and have no external-network
///   capability,
/// - every write tool in the stable manifest, including content-bearing
///   writes, filesystem mutations, and workspace-memory mutations.
///
/// Execute tools and external-network tools never appear here: command
/// execution and outbound network access keep their dedicated approval paths.
pub(crate) fn pinned_server_tools(transport: ee_mcp::ToolTransport) -> Vec<McpServerToolGrant> {
    let mut tools = ee_mcp::tool_names_for_transport(transport)
        .into_iter()
        .filter_map(|tool| {
            let governance = ee_mcp::governance(tool)?;
            let (category, tool_schema_version) = match governance.side_effect {
                ee_mcp::SideEffectClass::Read
                    if governance.approval == "none"
                        && !governance.required_capabilities.contains(&"ExternalNetwork") =>
                {
                    (TrustCategory::Read, crate::policy::EE_MCP_SAFE_READ_TOOL_SCHEMA_VERSION)
                }
                ee_mcp::SideEffectClass::Write => {
                    (write_tool_category(tool), ee_mcp::EE_TOOL_SCHEMA_VERSION)
                }
                _ => return None,
            };
            Some(McpServerToolGrant {
                tool: tool.to_string(),
                category,
                tool_schema_version,
                unrestricted: false,
            })
        })
        .collect::<Vec<_>>();
    tools.sort_by(|left, right| left.tool.cmp(&right.tool));
    tools.dedup_by(|left, right| left.tool == right.tool);
    tools
}

/// Operation category produced by the approval path for one write tool.
/// Most write tools normalize to `WriteModify`; filesystem mutations keep
/// their distinct create/delete categories.
fn write_tool_category(tool: &str) -> TrustCategory {
    match tool {
        "ee_create_directory" | "ee_copy_path" => TrustCategory::WriteCreate,
        "ee_delete_path" => TrustCategory::Delete,
        _ => TrustCategory::WriteModify,
    }
}

/// Pinned terminal-execution tools for the explicit execute opt-in.
///
/// Only `ee_terminal_create` qualifies. Shell wrappers and external cwd stay
/// outside because the app still validates the command shape before either
/// offering or applying execution trust.
pub(crate) fn pinned_execution_tools(transport: ee_mcp::ToolTransport) -> Vec<McpServerToolGrant> {
    let tool = "ee_terminal_create";
    match ee_mcp::governance(tool) {
        Some(governance)
            if governance.side_effect == ee_mcp::SideEffectClass::Execute
                && governance.transports.contains(&transport) =>
        {
            vec![McpServerToolGrant {
                tool: tool.to_string(),
                category: TrustCategory::Execute,
                tool_schema_version: ee_mcp::EE_TOOL_SCHEMA_VERSION,
                unrestricted: false,
            }]
        }
        _ => Vec::new(),
    }
}

/// Pinned terminal-execution tools that accept arbitrary shell command text.
/// Only `ee_terminal_create` qualifies; the host still requires an explicit
/// workspace-local grant, a non-read-only mode, and the terminal sandbox.
pub(crate) fn pinned_unrestricted_execution_tools(
    transport: ee_mcp::ToolTransport,
) -> Vec<McpServerToolGrant> {
    pinned_execution_tools(transport)
        .into_iter()
        .map(|mut tool| {
            tool.unrestricted = true;
            tool
        })
        .collect()
}

/// Stable rule id for a newly created server grant (`mcp_server_…`).
pub(crate) fn generate_mcp_server_grant_id() -> String {
    format!("mcp_server_{:016x}", rand::random::<u64>())
}

fn current_granted_at() -> String {
    let now: chrono::DateTime<chrono::Utc> = std::time::SystemTime::now().into();
    now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// RFC3339 UTC grant timestamp used by the CLI and approval candidate.
pub(crate) fn granted_at_now() -> String {
    current_granted_at()
}

pub(crate) fn validate_server(server: &str) -> Result<(), String> {
    if server.is_empty() || server.len() > 160 {
        return Err("mcp server name must be 1 to 160 bytes".to_string());
    }
    if server.chars().any(|character| character.is_control()) {
        return Err("mcp server name contains control characters".to_string());
    }
    Ok(())
}

pub(crate) fn validate_transport_identity(transport_identity: &str) -> Result<(), String> {
    if transport_identity.is_empty() || transport_identity.len() > 256 {
        return Err("mcp transport identity must be 1 to 256 bytes".to_string());
    }
    if transport_identity.chars().any(|character| character.is_control()) {
        return Err("mcp transport identity contains control characters".to_string());
    }
    Ok(())
}

pub(crate) fn validate_granted_at(granted_at: &str) -> Result<(), String> {
    if granted_at.is_empty() || granted_at.len() > 64 {
        return Err("mcp grant timestamp must be 1 to 64 bytes".to_string());
    }
    if granted_at.chars().any(|character| character.is_control()) {
        return Err("mcp grant timestamp contains control characters".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> WorkspaceIdentity {
        WorkspaceIdentity::from_canonical_root_bytes(b"/workspace")
    }

    fn operation(
        workspace: WorkspaceIdentity,
        transport_identity: &str,
        tool: &str,
        category: TrustCategory,
        tool_schema_version: u64,
    ) -> TrustOperation {
        TrustOperation {
            workspace,
            agent: None,
            transport: TransportKind::McpStdio,
            category,
            identity: OperationIdentity::Mcp {
                server: "ee".to_string(),
                transport_identity: transport_identity.to_string(),
                tool: tool.to_string(),
                tool_schema_version,
                arguments_json: r#"{"path":"src/lib.rs"}"#.to_string(),
            },
        }
    }

    fn grant(transport_identity: &str) -> McpServerGrant {
        McpServerGrant::new(
            "mcp_server_test".to_string(),
            workspace(),
            "ee".to_string(),
            transport_identity.to_string(),
            "2026-11-24T00:00:00Z".to_string(),
            pinned_server_tools(ee_mcp::ToolTransport::Stdio),
        )
        .unwrap()
    }

    #[test]
    fn pinned_tools_cover_workspace_reads_and_writes_only() {
        let tools = pinned_server_tools(ee_mcp::ToolTransport::Stdio);
        assert!(tools.iter().any(|tool| tool.tool == "ee_read_text_file"));
        assert!(tools.iter().any(|tool| tool.tool == "ee_apply_code_action"));
        assert!(tools.iter().any(|tool| tool.tool == "ee_format_file"));
        assert!(tools.iter().any(|tool| tool.tool == "ee_rename_symbol"));
        assert!(tools.iter().any(|tool| tool.tool == "ee_write_text_file"));
        assert!(tools.iter().any(|tool| tool.tool == "ee_replace_text"));
        assert!(tools.iter().any(|tool| {
            tool.tool == "ee_delete_path" && tool.category == TrustCategory::Delete
        }));
        assert!(tools.iter().any(|tool| tool.tool == "ee_clear_workspace_memory"));
        for excluded in [
            "ee_terminal_create",
            "ee_terminal_kill",
            "ee_fetch_url",
            "ee_web_search",
            "ee_export_workspace_memory",
        ] {
            assert!(
                !tools.iter().any(|tool| tool.tool == excluded),
                "{excluded} must not be server-trusted"
            );
        }
        for tool in &tools {
            assert!(matches!(
                tool.category,
                TrustCategory::Read
                    | TrustCategory::WriteCreate
                    | TrustCategory::WriteModify
                    | TrustCategory::Delete
            ));
            assert_ne!(tool.tool_schema_version, 0);
        }
    }

    #[test]
    fn execution_opt_in_is_separate_and_pins_terminal_create() {
        let invocation = McpInvocation {
            workspace: workspace(),
            agent: None,
            transport: TransportKind::McpStdio,
            transport_identity: "stdio:ee --mcp-proxy".to_string(),
            server: "ee".to_string(),
            tool: "ee_terminal_create".to_string(),
            tool_schema_version: ee_mcp::EE_TOOL_SCHEMA_VERSION,
            category: TrustCategory::Execute,
            arguments_json: "{}".to_string(),
        };
        assert!(McpServerTrustCandidate::for_invocation(&invocation).is_err());
        let candidate = McpServerTrustCandidate::for_execution_invocation(&invocation).unwrap();
        assert!(candidate.execution);
        assert!(candidate.grant.includes_execution());
        assert!(candidate.grant.tools.iter().any(|tool| {
            tool.tool == "ee_terminal_create"
                && tool.category == TrustCategory::Execute
                && tool.tool_schema_version == ee_mcp::EE_TOOL_SCHEMA_VERSION
        }));
        assert!(candidate.grant.tools.iter().any(|tool| tool.tool == "ee_write_text_file"));

        let trust = McpServerTrust::new(vec![candidate.grant]);
        let terminal = operation(
            workspace(),
            "stdio:ee --mcp-proxy",
            "ee_terminal_create",
            TrustCategory::Execute,
            ee_mcp::EE_TOOL_SCHEMA_VERSION,
        );
        assert!(trust.allows(&terminal).is_some());
    }

    #[test]
    fn general_retrust_preserves_terminal_execution_pins() {
        let mut existing = grant("stdio:ee --mcp-proxy");
        existing.tools.extend(pinned_execution_tools(ee_mcp::ToolTransport::Stdio));
        assert!(existing.includes_execution());
        let mut replacement = grant("stdio:ee --mcp-proxy");
        assert!(!replacement.includes_execution());
        replacement.preserve_execute_pins(&existing);
        assert!(replacement.includes_execution());
        assert!(replacement.tools.iter().any(|tool| {
            tool.tool == "ee_terminal_create" && tool.category == TrustCategory::Execute
        }));
    }

    #[test]
    fn grant_matches_only_the_pinned_identity() {
        let trust = McpServerTrust::new(vec![grant("stdio:ee --mcp-proxy")]);
        let matching = operation(
            workspace(),
            "stdio:ee --mcp-proxy",
            "ee_apply_code_action",
            TrustCategory::WriteModify,
            ee_mcp::EE_TOOL_SCHEMA_VERSION,
        );
        assert!(trust.allows(&matching).is_some());

        let other_transport = operation(
            workspace(),
            "acp:ee",
            "ee_apply_code_action",
            TrustCategory::WriteModify,
            ee_mcp::EE_TOOL_SCHEMA_VERSION,
        );
        assert!(trust.allows(&other_transport).is_none());

        let other_workspace = operation(
            WorkspaceIdentity::from_canonical_root_bytes(b"/other"),
            "stdio:ee --mcp-proxy",
            "ee_apply_code_action",
            TrustCategory::WriteModify,
            ee_mcp::EE_TOOL_SCHEMA_VERSION,
        );
        assert!(trust.allows(&other_workspace).is_none());

        let other_schema = operation(
            workspace(),
            "stdio:ee --mcp-proxy",
            "ee_apply_code_action",
            TrustCategory::WriteModify,
            ee_mcp::EE_TOOL_SCHEMA_VERSION + 1,
        );
        assert!(trust.allows(&other_schema).is_none());

        let unpinned = operation(
            workspace(),
            "stdio:ee --mcp-proxy",
            "ee_terminal_create",
            TrustCategory::Execute,
            ee_mcp::EE_TOOL_SCHEMA_VERSION,
        );
        assert!(trust.allows(&unpinned).is_none());

        let content_write = operation(
            workspace(),
            "stdio:ee --mcp-proxy",
            "ee_write_text_file",
            TrustCategory::WriteModify,
            ee_mcp::EE_TOOL_SCHEMA_VERSION,
        );
        assert!(trust.allows(&content_write).is_some());

        let delete = operation(
            workspace(),
            "stdio:ee --mcp-proxy",
            "ee_delete_path",
            TrustCategory::Delete,
            ee_mcp::EE_TOOL_SCHEMA_VERSION,
        );
        assert!(trust.allows(&delete).is_some());

        let memory_write = operation(
            workspace(),
            "stdio:ee --mcp-proxy",
            "ee_remember_workspace_fact",
            TrustCategory::WriteModify,
            ee_mcp::EE_TOOL_SCHEMA_VERSION,
        );
        assert!(trust.allows(&memory_write).is_some());

        let wrong_category = operation(
            workspace(),
            "stdio:ee --mcp-proxy",
            "ee_apply_code_action",
            TrustCategory::Delete,
            ee_mcp::EE_TOOL_SCHEMA_VERSION,
        );
        assert!(trust.allows(&wrong_category).is_none());
    }

    #[test]
    fn read_grants_use_the_read_schema_version() {
        let trust = McpServerTrust::new(vec![grant("stdio:ee --mcp-proxy")]);
        let read = operation(
            workspace(),
            "stdio:ee --mcp-proxy",
            "ee_read_text_file",
            TrustCategory::Read,
            crate::policy::EE_MCP_SAFE_READ_TOOL_SCHEMA_VERSION,
        );
        assert!(trust.allows(&read).is_some());
        let write_schema = operation(
            workspace(),
            "stdio:ee --mcp-proxy",
            "ee_read_text_file",
            TrustCategory::Read,
            ee_mcp::EE_TOOL_SCHEMA_VERSION,
        );
        assert!(trust.allows(&write_schema).is_none());
    }

    #[test]
    fn malformed_and_dangerous_tool_categories_are_rejected() {
        for category in [TrustCategory::Execute, TrustCategory::Network, TrustCategory::Unknown] {
            let tool = McpServerToolGrant {
                tool: "ee_delete_path".to_string(),
                category,
                tool_schema_version: 1,
                unrestricted: false,
            };
            assert!(tool.validate().is_err(), "{category:?} must be rejected");
        }
        let delete = McpServerToolGrant {
            tool: "ee_delete_path".to_string(),
            category: TrustCategory::Delete,
            tool_schema_version: 1,
            unrestricted: false,
        };
        assert!(delete.validate().is_ok(), "application-owned delete pins are allowed");
        let terminal = McpServerToolGrant {
            tool: "ee_terminal_create".to_string(),
            category: TrustCategory::Execute,
            tool_schema_version: ee_mcp::EE_TOOL_SCHEMA_VERSION,
            unrestricted: true,
        };
        assert!(terminal.validate().is_ok(), "terminal execution opt-in is allowed");
        let wrong_execute = McpServerToolGrant {
            tool: "ee_format_file".to_string(),
            category: TrustCategory::Execute,
            tool_schema_version: ee_mcp::EE_TOOL_SCHEMA_VERSION,
            unrestricted: false,
        };
        assert!(wrong_execute.validate().is_err(), "other execute pins are rejected");
        assert!(
            McpServerGrant::new(
                "mcp_server_test".to_string(),
                workspace(),
                "ee".to_string(),
                "stdio:ee --mcp-proxy".to_string(),
                "2026-11-24T00:00:00Z".to_string(),
                Vec::new(),
            )
            .is_err()
        );
        let duplicate = McpServerToolGrant {
            tool: "ee_read_text_file".to_string(),
            category: TrustCategory::Read,
            tool_schema_version: 1,
            unrestricted: false,
        };
        assert!(
            McpServerGrant::new(
                "mcp_server_test".to_string(),
                workspace(),
                "ee".to_string(),
                "stdio:ee --mcp-proxy".to_string(),
                "2026-11-24T00:00:00Z".to_string(),
                vec![duplicate.clone(), duplicate],
            )
            .is_err()
        );
    }

    #[test]
    fn candidate_pins_the_invocation_transport() {
        let invocation = McpInvocation {
            workspace: workspace(),
            agent: None,
            transport: TransportKind::McpAcp,
            transport_identity: "acp:ee".to_string(),
            server: "ee".to_string(),
            tool: "ee_apply_code_action".to_string(),
            tool_schema_version: ee_mcp::EE_TOOL_SCHEMA_VERSION,
            category: TrustCategory::WriteModify,
            arguments_json: r#"{"path":"src/lib.rs"}"#.to_string(),
        };
        let candidate = McpServerTrustCandidate::for_invocation(&invocation).unwrap();
        assert_eq!(candidate.grant.transport_identity, "acp:ee");
        assert!(!candidate.grant.tools.is_empty());
        assert_eq!(candidate.preview.transport_identity.as_deref(), Some("acp:ee"));
        assert!(
            candidate
                .preview
                .authority_fields()
                .iter()
                .any(|(label, value)| label == "server" && value == "ee")
        );
    }
}
