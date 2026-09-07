//! Domain-to-raw conversions for persistence.
use super::validate::format_expiry;
use super::*;

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
