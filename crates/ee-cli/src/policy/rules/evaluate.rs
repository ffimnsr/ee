//! Rule matching: TrustRule dispatch and per-rule matchers.
use super::*;

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
        crate::policy::templates::validate_template(&template_id, &rule)?;
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
                    && crate::policy::profiles::mcp_read_profile_matches(&rule.profile, tool)
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
    pub(crate) fn matches_argv(&self, argv: &[String]) -> bool {
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
    pub(crate) fn size_ok(&self, byte_count: Option<u64>) -> bool {
        byte_count.is_none_or(|bytes| bytes <= self.max_bytes)
    }
}

impl McpReadRule {
    pub(crate) fn size_ok(&self, byte_count: Option<u64>) -> bool {
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

    pub(crate) fn matches(&self, operation: &TrustOperation) -> bool {
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
    pub(crate) fn matches(&self, operation: &TrustOperation) -> bool {
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
    pub(crate) fn matches(&self, operation: &TrustOperation) -> bool {
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
    pub(crate) fn matches(&self, operation: &TrustOperation) -> bool {
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

pub(crate) fn host_matches(expected: &str, mode: HostMatchMode, host: &str) -> bool {
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
