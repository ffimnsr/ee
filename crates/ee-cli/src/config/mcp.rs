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
#[cfg(feature = "agents")]
use super::discovery::{ConfigEnvironment, ConfigScope, config_path_for_scope_with_env};
#[cfg(feature = "agents")]
use super::raw::parse_config_document;
use super::raw::{EeToml, McpProxyToml, McpServerToml, McpToml, McpTransportToml};
use super::rubber_duck::validate_rubber_duck_toml;
#[cfg(feature = "agents")]
use super::value::{ensure_named_table, mutate_config_at_scope};
use super::web_context::validate_agent_web_context_config;
use super::workspace_memory::validate_workspace_memory_toml;
use std::collections::{BTreeMap, BTreeSet};
#[cfg(feature = "agents")]
use std::path::PathBuf;

/// Default request timeout for Streamable HTTP MCP servers, in milliseconds.
pub(super) const DEFAULT_MCP_HTTP_TIMEOUT_MS: u64 = 30_000;

/// Resolved shared MCP server configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct McpSettings {
    pub servers: BTreeMap<String, McpServerSettings>,
    /// ee MCP proxy mode. Enabled by default when agents mode is on; the
    /// user can opt out with `[mcp.proxy] enabled = false`.
    pub proxy: McpProxySettings,
    /// Whether the user explicitly configured `[mcp.proxy]` in any layer.
    pub proxy_explicit: bool,
    /// Split-layer partial entries: the setup wizard writes secret env/headers
    /// into the user config layer while the server definition lives in the
    /// workspace layer. Unresolved entries stay inert and never reach
    /// effective config.
    pub(crate) partial: BTreeMap<String, McpServerToml>,
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
            let url = validate_mcp_url(server.url.as_deref().unwrap_or_default())?;
            Ok(McpServerSettings::StreamableHttp {
                url,
                headers: server.headers.clone(),
                timeout_ms: server.timeout_ms.unwrap_or(DEFAULT_MCP_HTTP_TIMEOUT_MS),
            })
        }
    }
}

/// Parses and validates an `http(s)` MCP endpoint URL.
pub(super) fn validate_mcp_url(raw_url: &str) -> Result<String, String> {
    let parsed =
        url::Url::parse(raw_url).map_err(|err| format!("invalid mcp url `{raw_url}`: {err}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!("invalid mcp url `{raw_url}`: scheme must be http or https"));
    }
    Ok(parsed.to_string())
}

/// Shape-only validation for one raw `[mcp.servers.<id>]` entry at file
/// validation time. Required transport fields are NOT enforced here: a layer
/// may carry only a patch (for example the user-layer secret env/headers
/// written by the setup wizard) that a higher-priority layer completes.
pub(super) fn validate_mcp_server_shape(id: &str, server: &McpServerToml) -> Result<(), String> {
    if id.trim().is_empty() {
        return Err(String::from("mcp server id must not be empty"));
    }
    if let Some(raw_url) = server.url.as_deref()
        && !raw_url.trim().is_empty()
    {
        validate_mcp_url(raw_url)?;
    }
    Ok(())
}

/// Field-level merge of a higher-priority raw patch onto an already-resolved
/// lower-layer server. Used when both layers carry the same transport.
pub(super) fn merge_mcp_server_onto(
    existing: McpServerSettings,
    patch: &McpServerToml,
) -> Result<McpServerSettings, String> {
    match existing {
        McpServerSettings::Stdio { command, args, env, cwd } => {
            let command = patch
                .command
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .unwrap_or(command);
            let args = patch.args.clone().unwrap_or(args);
            let mut env = env;
            env.extend(patch.env.iter().map(|(key, value)| (key.clone(), value.clone())));
            let cwd = patch.cwd.clone().or(cwd);
            if command.trim().is_empty() {
                Err(String::from("mcp stdio server command must not be empty"))
            } else {
                Ok(McpServerSettings::Stdio { command, args, env, cwd })
            }
        }
        McpServerSettings::StreamableHttp { url, headers, timeout_ms } => {
            let url = match patch.url.as_deref() {
                Some(raw) if !raw.trim().is_empty() => validate_mcp_url(raw)?,
                _ => url,
            };
            let mut headers = headers;
            headers.extend(patch.headers.iter().map(|(key, value)| (key.clone(), value.clone())));
            let timeout_ms = patch.timeout_ms.unwrap_or(timeout_ms);
            Ok(McpServerSettings::StreamableHttp { url, headers, timeout_ms })
        }
    }
}

/// Raw-level merge of two partial entries from different layers (patch wins
/// per field, env/headers union).
pub(super) fn merge_mcp_server_toml(base: &McpServerToml, patch: &McpServerToml) -> McpServerToml {
    let mut env = base.env.clone();
    env.extend(patch.env.clone());
    let mut headers = base.headers.clone();
    headers.extend(patch.headers.clone());
    McpServerToml {
        transport: patch.transport,
        command: patch.command.clone().or_else(|| base.command.clone()),
        args: patch.args.clone().or_else(|| base.args.clone()),
        env,
        cwd: patch.cwd.clone().or_else(|| base.cwd.clone()),
        url: patch.url.clone().or_else(|| base.url.clone()),
        headers,
        timeout_ms: patch.timeout_ms.or(base.timeout_ms),
    }
}

/// True when the entry is a split-layer patch missing the transport-required
/// field; such entries park in `McpSettings::partial` instead of warning.
pub(super) fn is_partial_patch(server: &McpServerToml) -> bool {
    match server.transport {
        McpTransportToml::Stdio => {
            server.command.as_deref().map(str::trim).is_none_or(str::is_empty)
        }
        McpTransportToml::StreamableHttp => {
            server.url.as_deref().map(str::trim).is_none_or(str::is_empty)
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
            validate_mcp_server_shape(id, server)
                .map_err(|err| format!("mcp server `{id}`: {err}"))?;
            if !effective_ids.insert(id.clone()) {
                return Err(format!(
                    "duplicate effective server id `{id}` in agents.servers and mcp.servers"
                ));
            }
        }
    }
    Ok(())
}

// ── Setup wizard writes ───────────────────────────────────────────────────────

#[cfg(feature = "agents")]
/// Config path for the chosen scope, resolved from the process environment.
pub(crate) fn mcp_config_path(scope: ConfigScope) -> Result<PathBuf, String> {
    mcp_config_path_with_env(scope, &ConfigEnvironment::from_process())
}

#[cfg(feature = "agents")]
pub(super) fn mcp_config_path_with_env(
    scope: ConfigScope,
    env: &ConfigEnvironment,
) -> Result<PathBuf, String> {
    config_path_for_scope_with_env(scope, env)
}

#[cfg(feature = "agents")]
/// `(id, transport)` pairs for servers present in the chosen scope's own
/// config layer. Used by the wizard for listing and removal.
pub(crate) fn list_mcp_servers(scope: ConfigScope) -> Result<Vec<(String, String)>, String> {
    list_mcp_servers_with_env(scope, &ConfigEnvironment::from_process())
}

#[cfg(feature = "agents")]
pub(super) fn list_mcp_servers_with_env(
    scope: ConfigScope,
    env: &ConfigEnvironment,
) -> Result<Vec<(String, String)>, String> {
    let path = mcp_config_path_with_env(scope, env)?;
    let document = parse_config_document(&path)?;
    let mut servers = Vec::new();
    if let Some(toml::Value::Table(mcp)) = document.get("mcp")
        && let Some(toml::Value::Table(server_table)) = mcp.get("servers")
    {
        for (id, value) in server_table {
            let transport =
                value.get("transport").and_then(toml::Value::as_str).unwrap_or("?").to_owned();
            servers.push((id.clone(), transport));
        }
    }
    Ok(servers)
}

#[cfg(feature = "agents")]
/// Writes one complete `[mcp.servers.<id>]` entry into the chosen scope's
/// config layer, merging into any existing entry for the same id.
pub(crate) fn write_mcp_server(
    scope: ConfigScope,
    id: &str,
    server: &McpServerToml,
) -> Result<PathBuf, String> {
    write_mcp_server_with_env(scope, id, server, &ConfigEnvironment::from_process())
}

#[cfg(feature = "agents")]
pub(super) fn write_mcp_server_with_env(
    scope: ConfigScope,
    id: &str,
    server: &McpServerToml,
    env: &ConfigEnvironment,
) -> Result<PathBuf, String> {
    mutate_config_at_scope(scope, env, |root| {
        let mcp = ensure_named_table(root, "mcp", "mcp")?;
        let servers = ensure_named_table(mcp, "servers", "mcp.servers")?;
        let value = toml::Value::try_from(server)
            .map_err(|error| format!("cannot serialize mcp server `{id}`: {error}"))?;
        servers.insert(id.to_owned(), value);
        Ok(())
    })
}

#[cfg(feature = "agents")]
/// Writes only secret env/headers for a server into the user config layer as
/// a split-layer patch (transport plus the secret tables). The higher-priority
/// workspace layer supplies the operational fields; merge completes the entry.
pub(crate) fn write_mcp_server_user_partial(
    id: &str,
    transport: McpTransportToml,
    env: &BTreeMap<String, String>,
    headers: &BTreeMap<String, String>,
) -> Result<PathBuf, String> {
    write_mcp_server_user_partial_with_env(
        id,
        transport,
        env,
        headers,
        &ConfigEnvironment::from_process(),
    )
}

#[cfg(feature = "agents")]
pub(super) fn write_mcp_server_user_partial_with_env(
    id: &str,
    transport: McpTransportToml,
    env: &BTreeMap<String, String>,
    headers: &BTreeMap<String, String>,
    env_ctx: &ConfigEnvironment,
) -> Result<PathBuf, String> {
    mutate_config_at_scope(ConfigScope::Global, env_ctx, |root| {
        let mcp = ensure_named_table(root, "mcp", "mcp")?;
        let servers = ensure_named_table(mcp, "servers", "mcp.servers")?;
        let existing = servers
            .entry(id.to_owned())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        let table = match existing {
            toml::Value::Table(table) => table,
            _ => {
                return Err(format!(
                    "config key `mcp.servers.{id}` already exists and is not table"
                ));
            }
        };
        table.insert(
            String::from("transport"),
            toml::Value::String(
                match transport {
                    McpTransportToml::Stdio => "stdio",
                    McpTransportToml::StreamableHttp => "streamable_http",
                }
                .to_owned(),
            ),
        );
        if !env.is_empty() {
            let env_table = ensure_named_table(table, "env", &format!("mcp.servers.{id}.env"))?;
            for (name, value) in env {
                env_table.insert(name.clone(), toml::Value::String(value.clone()));
            }
        }
        if !headers.is_empty() {
            let headers_table =
                ensure_named_table(table, "headers", &format!("mcp.servers.{id}.headers"))?;
            for (name, value) in headers {
                headers_table.insert(name.clone(), toml::Value::String(value.clone()));
            }
        }
        Ok(())
    })
}

#[cfg(feature = "agents")]
/// Removes `[mcp.servers.<id>]` (if present) from the chosen scope's config
/// layer. Absence is not an error.
pub(crate) fn remove_mcp_server(scope: ConfigScope, id: &str) -> Result<PathBuf, String> {
    remove_mcp_server_with_env(scope, id, &ConfigEnvironment::from_process())
}

#[cfg(feature = "agents")]
pub(super) fn remove_mcp_server_with_env(
    scope: ConfigScope,
    id: &str,
    env: &ConfigEnvironment,
) -> Result<PathBuf, String> {
    mutate_config_at_scope(scope, env, |root| {
        let Some(toml::Value::Table(mcp)) = root.get_mut("mcp") else {
            return Ok(());
        };
        let Some(toml::Value::Table(servers)) = mcp.get_mut("servers") else {
            return Ok(());
        };
        servers.remove(id);
        Ok(())
    })
}
