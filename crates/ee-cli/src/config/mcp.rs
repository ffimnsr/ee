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
use super::discovery::ConfigLayerKind;
#[cfg(feature = "agents")]
use super::discovery::{ConfigEnvironment, ConfigScope, config_path_for_scope_with_env};
#[cfg(feature = "agents")]
use super::raw::parse_config_document;
use super::raw::{EeToml, McpProxyToml, McpServerToml, McpToml, McpTransportToml};
use super::rubber_duck::validate_rubber_duck_toml;
use super::secret_value::ConfigSecretValue;
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
    /// Split-layer partial entries: a lower layer may supply only part of a
    /// server (for example env in user config and the transport in the
    /// workspace layer). Field provenance is retained so workspace trust
    /// applies to exactly the values a workspace layer supplied. Unresolved
    /// entries stay inert and never reach effective config.
    pub(crate) partial: BTreeMap<String, McpPartialServer>,
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
///
/// Env/header values carry their config-layer provenance so workspace trust
/// gates exactly the `secret://` references a workspace layer supplies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum McpServerSettings {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, ConfigSecretValue>,
        cwd: Option<PathBuf>,
    },
    StreamableHttp {
        url: String,
        headers: BTreeMap<String, ConfigSecretValue>,
        timeout_ms: u64,
    },
}

/// Accumulated split-layer MCP entry with per-field layer provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpPartialServer {
    transport: McpTransportToml,
    command: Option<String>,
    args: Option<Vec<String>>,
    env: BTreeMap<String, ConfigSecretValue>,
    cwd: Option<PathBuf>,
    url: Option<String>,
    headers: BTreeMap<String, ConfigSecretValue>,
    timeout_ms: Option<u64>,
}

impl McpPartialServer {
    pub(super) fn new(transport: McpTransportToml) -> Self {
        Self {
            transport,
            command: None,
            args: None,
            env: BTreeMap::new(),
            cwd: None,
            url: None,
            headers: BTreeMap::new(),
            timeout_ms: None,
        }
    }

    /// Overlays a higher-priority layer's raw entry: patch wins per field,
    /// env/headers union with the patch's provenance.
    pub(super) fn overlay(
        &mut self,
        id: &str,
        patch: &McpServerToml,
        kind: ConfigLayerKind,
    ) -> Result<(), String> {
        self.transport = patch.transport;
        if patch.command.is_some() {
            self.command = patch.command.clone();
        }
        if patch.args.is_some() {
            self.args = patch.args.clone();
        }
        if patch.cwd.is_some() {
            self.cwd = patch.cwd.clone();
        }
        if patch.url.is_some() {
            self.url = patch.url.clone();
        }
        if patch.timeout_ms.is_some() {
            self.timeout_ms = patch.timeout_ms;
        }
        self.env.extend(tag_values(id, "env", patch.env.iter(), kind)?);
        self.headers.extend(tag_values(id, "headers", patch.headers.iter(), kind)?);
        Ok(())
    }

    /// True once the transport-required field is present.
    pub(super) fn is_complete(&self) -> bool {
        match self.transport {
            McpTransportToml::Stdio => {
                self.command.as_deref().is_some_and(|command| !command.trim().is_empty())
            }
            McpTransportToml::StreamableHttp => {
                self.url.as_deref().is_some_and(|url| !url.trim().is_empty())
            }
        }
    }

    /// Resolves the accumulated entry into effective settings. Reference
    /// grammar is revalidated here so a programmatically built entry can
    /// never bypass config-file validation.
    pub(super) fn resolve(self, id: &str) -> Result<McpServerSettings, String> {
        match self.transport {
            McpTransportToml::Stdio => {
                let command = self.command.as_deref().unwrap_or_default().trim();
                if command.is_empty() {
                    return Err(String::from("mcp stdio server command must not be empty"));
                }
                validate_reference_values(id, "env", &self.env, false)?;
                Ok(McpServerSettings::Stdio {
                    command: command.to_owned(),
                    args: self.args.unwrap_or_default(),
                    env: self.env,
                    cwd: self.cwd,
                })
            }
            McpTransportToml::StreamableHttp => {
                let url = validate_mcp_url(self.url.as_deref().unwrap_or_default())?;
                validate_reference_values(id, "headers", &self.headers, true)?;
                Ok(McpServerSettings::StreamableHttp {
                    url,
                    headers: self.headers,
                    timeout_ms: self.timeout_ms.unwrap_or(DEFAULT_MCP_HTTP_TIMEOUT_MS),
                })
            }
        }
    }
}

/// Tags one layer's raw env/header values with provenance. System layers can
/// never reference secrets.
fn tag_values<'a>(
    id: &str,
    table: &str,
    values: impl Iterator<Item = (&'a String, &'a String)>,
    kind: ConfigLayerKind,
) -> Result<BTreeMap<String, ConfigSecretValue>, String> {
    let mut tagged = BTreeMap::new();
    for (key, raw) in values {
        if kind == ConfigLayerKind::System && crate::secrets::is_secret_reference_text(raw) {
            return Err(format!(
                "secret references are not allowed in system config layers, \
                 but mcp.servers.{id}.{table}.{key} comes from {} config",
                kind.label()
            ));
        }
        tagged.insert(key.clone(), ConfigSecretValue::new(kind, raw.clone()));
    }
    Ok(tagged)
}

/// Validates reference grammar for one resolved env/header table.
fn validate_reference_values(
    id: &str,
    table: &str,
    values: &BTreeMap<String, ConfigSecretValue>,
    allow_embedded: bool,
) -> Result<(), String> {
    for (key, value) in values {
        crate::secrets::resolve::validate_secret_value(&value.raw, allow_embedded).map_err(
            |error| format!("invalid secret reference in mcp.servers.{id}.{table}.{key}: {error}"),
        )?;
    }
    Ok(())
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
/// may carry only a patch that a higher-priority layer completes.
/// `secret://` grammar IS validated for env values (exact references) and
/// header values (exact references or templates embedding one token).
pub(super) fn validate_mcp_server_shape(id: &str, server: &McpServerToml) -> Result<(), String> {
    if id.trim().is_empty() {
        return Err(String::from("mcp server id must not be empty"));
    }
    if let Some(raw_url) = server.url.as_deref()
        && !raw_url.trim().is_empty()
    {
        validate_mcp_url(raw_url)?;
    }
    for (key, value) in &server.env {
        crate::secrets::resolve::validate_secret_value(value, false).map_err(|error| {
            format!("invalid secret reference in mcp.servers.{id}.env.{key}: {error}")
        })?;
    }
    for (key, value) in &server.headers {
        crate::secrets::resolve::validate_secret_value(value, true).map_err(|error| {
            format!("invalid secret reference in mcp.servers.{id}.headers.{key}: {error}")
        })?;
    }
    Ok(())
}

/// Field-level merge of a higher-priority raw patch onto an already-resolved
/// lower-layer server. Used when both layers carry the same transport.
pub(super) fn merge_mcp_server_onto(
    id: &str,
    existing: McpServerSettings,
    patch: &McpServerToml,
    kind: ConfigLayerKind,
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
            env.extend(tag_values(id, "env", patch.env.iter(), kind)?);
            let cwd = patch.cwd.clone().or(cwd);
            if command.trim().is_empty() {
                Err(String::from("mcp stdio server command must not be empty"))
            } else {
                validate_reference_values(id, "env", &env, false)?;
                Ok(McpServerSettings::Stdio { command, args, env, cwd })
            }
        }
        McpServerSettings::StreamableHttp { url, headers, timeout_ms } => {
            let url = match patch.url.as_deref() {
                Some(raw) if !raw.trim().is_empty() => validate_mcp_url(raw)?,
                _ => url,
            };
            let mut headers = headers;
            headers.extend(tag_values(id, "headers", patch.headers.iter(), kind)?);
            let timeout_ms = patch.timeout_ms.unwrap_or(timeout_ms);
            validate_reference_values(id, "headers", &headers, true)?;
            Ok(McpServerSettings::StreamableHttp { url, headers, timeout_ms })
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
                        env: env
                            .iter()
                            .map(|(key, value)| (key.clone(), value.raw.clone()))
                            .collect(),
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
                            headers: headers
                                .iter()
                                .map(|(key, value)| (key.clone(), value.raw.clone()))
                                .collect(),
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
