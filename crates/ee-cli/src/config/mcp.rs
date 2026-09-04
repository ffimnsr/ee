//! Editor configuration loading for ee.
//!
//! Settings are resolved by merging layers in priority order (lowest first):
//!   1. built-in defaults
//!   2. `/etc/ee/config.toml`
//!   3. `$XDG_CONFIG_HOME/ee/config.toml` or `~/.config/ee/config.toml`
//!   4. fallback `~/.ee.toml` when XDG user config is missing
//!   5. every ancestor `.ee.toml` from outermost to innermost
//!   6. `.editorconfig` (walked up from the open file, per spec)
//!
//! Later layers override earlier ones for any key that is explicitly set.

use super::agents_settings::validate_agent_server;
use super::raw::{EeToml, McpProxyToml, McpServerToml, McpToml, McpTransportToml};
use super::rubber_duck::validate_rubber_duck_toml;
use super::web_context::validate_agent_web_context_config;
use super::workspace_memory::validate_workspace_memory_toml;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Default request timeout for Streamable HTTP MCP servers, in milliseconds.
pub(super) const DEFAULT_MCP_HTTP_TIMEOUT_MS: u64 = 30_000;

/// Resolved shared MCP server configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct McpSettings {
    pub servers: BTreeMap<String, McpServerSettings>,
    /// Optional ee MCP proxy mode (off by default).
    pub proxy: McpProxySettings,
}

/// Resolved ee MCP proxy runtime settings.
///
/// The proxy exposes `ee_*` tools (file read/write, terminal create,
/// diagnostics) as a local MCP server that ACP agents can connect to; every
/// tool call routes through the same approval and bridge paths as direct ACP
/// client methods.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct McpProxySettings {
    /// Whether the proxy is started when agents mode is enabled.
    pub enabled: bool,
}

/// Resolved MCP server transport.  Only stdio and Streamable HTTP are
/// supported; HTTP+SSE and other transports are not implemented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum McpServerSettings {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<PathBuf>,
    },
    StreamableHttp {
        url: String,
        headers: BTreeMap<String, String>,
        timeout_ms: u64,
    },
}

pub(super) fn resolve_mcp_server(
    id: &str,
    server: &McpServerToml,
) -> Result<McpServerSettings, String> {
    if id.trim().is_empty() {
        return Err(String::from("mcp server id must not be empty"));
    }
    match server.transport {
        McpTransportToml::Stdio => {
            let command = server.command.as_deref().unwrap_or_default().trim();
            if command.is_empty() {
                return Err(String::from("mcp stdio server command must not be empty"));
            }
            Ok(McpServerSettings::Stdio {
                command: command.to_owned(),
                args: server.args.clone().unwrap_or_default(),
                env: server.env.clone(),
                cwd: server.cwd.clone(),
            })
        }
        McpTransportToml::StreamableHttp => {
            let raw_url = server.url.as_deref().unwrap_or_default();
            let parsed = url::Url::parse(raw_url)
                .map_err(|err| format!("invalid mcp url `{raw_url}`: {err}"))?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(format!("invalid mcp url `{raw_url}`: scheme must be http or https"));
            }
            Ok(McpServerSettings::StreamableHttp {
                url: parsed.to_string(),
                headers: server.headers.clone(),
                timeout_ms: server.timeout_ms.unwrap_or(DEFAULT_MCP_HTTP_TIMEOUT_MS),
            })
        }
    }
}

// ── Loading helpers ───────────────────────────────────────────────────────────

pub(super) fn mcp_settings_to_toml(mcp: &McpSettings) -> Option<McpToml> {
    if mcp.servers.is_empty() && !mcp.proxy.enabled {
        return None;
    }
    Some(McpToml {
        servers: mcp
            .servers
            .iter()
            .map(|(id, server)| {
                let toml = match server {
                    McpServerSettings::Stdio { command, args, env, cwd } => McpServerToml {
                        transport: McpTransportToml::Stdio,
                        command: Some(command.clone()),
                        args: Some(args.clone()),
                        env: env.clone(),
                        cwd: cwd.clone(),
                        url: None,
                        headers: BTreeMap::new(),
                        timeout_ms: None,
                    },
                    McpServerSettings::StreamableHttp { url, headers, timeout_ms } => {
                        McpServerToml {
                            transport: McpTransportToml::StreamableHttp,
                            command: None,
                            args: None,
                            env: BTreeMap::new(),
                            cwd: None,
                            url: Some(url.clone()),
                            headers: headers.clone(),
                            timeout_ms: Some(*timeout_ms),
                        }
                    }
                };
                (id.clone(), toml)
            })
            .collect(),
        proxy: mcp.proxy.enabled.then_some(McpProxyToml { enabled: Some(true) }),
    })
}

pub(super) fn validate_agents_mcp_config(parsed: &EeToml) -> Result<(), String> {
    let mut effective_ids = BTreeSet::new();
    if let Some(agents) = &parsed.agents {
        if let Some(workspace_memory) = &agents.workspace_memory {
            validate_workspace_memory_toml(workspace_memory)?;
        }
        if let Some(web_context) = &agents.web_context {
            validate_agent_web_context_config(web_context)?;
        }
        if let Some(rubber_duck) = &agents.rubber_duck {
            validate_rubber_duck_toml(rubber_duck)?;
        }
        for (id, server) in &agents.servers {
            // Validation checks shape and reference grammar only; layer
            // provenance and required effective fields are enforced during
            // the merge, because this file may contain only a server patch.
            validate_agent_server(id, server)
                .map_err(|err| format!("agents server `{id}`: {err}"))?;
            effective_ids.insert(id.clone());
        }
    }
    if let Some(mcp) = &parsed.mcp {
        for (id, server) in &mcp.servers {
            resolve_mcp_server(id, server).map_err(|err| format!("mcp server `{id}`: {err}"))?;
            if !effective_ids.insert(id.clone()) {
                return Err(format!(
                    "duplicate effective server id `{id}` in agents.servers and mcp.servers"
                ));
            }
        }
    }
    Ok(())
}
