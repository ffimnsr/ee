//! Raw-to-domain conversions and host normalization.
use super::validate::*;
use super::*;

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
            crate::policy::command::validate_executable(&executable)?;
        }
        let argv = raw.argv;
        crate::policy::command::validate_argv_tokens(&argv)?;
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
        if !crate::policy::profiles::is_known_mcp_read_profile(&profile) {
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
        if !crate::policy::profiles::is_known_profile(&profile) {
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
