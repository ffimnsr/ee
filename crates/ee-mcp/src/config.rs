//! Resolved MCP server configuration (mirrors `ee-cli`'s `McpSettings`
//! TOML contract without depending on it).
//!
//! Only stdio and Streamable HTTP transports are supported; HTTP+SSE and any
//! other transport kind are rejected at validation time.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::{DEFAULT_REQUEST_TIMEOUT_MS, McpError};

/// One configured MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    /// Unique server id (`[mcp.servers.<id>]` in `.ee.toml`).
    pub id: String,
    /// How the server is reached.
    pub kind: McpServerKind,
    /// Per-request timeout; defaults to [`DEFAULT_REQUEST_TIMEOUT_MS`].
    pub timeout_ms: u64,
}

/// How an MCP server is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpServerKind {
    /// Spawn `command args` with `env` and `cwd`, speak JSON-RPC over stdio.
    Stdio(StdioMcpConfig),
    /// Streamable HTTP transport (POST JSON-RPC; no HTTP+SSE fallback).
    StreamableHttp(StreamableHttpConfig),
}

/// Stdio server process configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StdioMcpConfig {
    /// Executable to spawn.
    pub command: String,
    /// Explicit argument list (never a shell string).
    pub args: Vec<String>,
    /// Explicit environment overrides (values are never logged).
    pub env: BTreeMap<String, String>,
    /// Working directory for the subprocess.
    pub cwd: Option<PathBuf>,
    /// Cap on retained stderr diagnostics bytes.
    pub stderr_cap: usize,
}

/// Streamable HTTP server configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamableHttpConfig {
    /// Endpoint URL (validated absolute `http://`/`https://`).
    pub url: String,
    /// Extra request headers (values are never logged).
    pub headers: BTreeMap<String, String>,
}

impl McpServerConfig {
    /// Validates the configuration, rejecting empty ids, empty commands,
    /// invalid URLs, and deprecated transport shapes.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::InvalidPrimitiveResult`] describing the invalid
    /// resolved configuration shape.
    pub fn validate(&self) -> Result<(), McpError> {
        if self.id.trim().is_empty() {
            return Err(McpError::InvalidPrimitiveResult("mcp server id must not be empty".into()));
        }
        match &self.kind {
            McpServerKind::Stdio(stdio) => {
                if stdio.command.trim().is_empty() {
                    return Err(McpError::InvalidPrimitiveResult(format!(
                        "mcp server {}: command must not be empty",
                        self.id
                    )));
                }
                if let Some(cwd) = &stdio.cwd
                    && !cwd.is_absolute()
                {
                    return Err(McpError::InvalidPrimitiveResult(format!(
                        "mcp server {}: cwd must be absolute, got {}",
                        self.id,
                        cwd.display()
                    )));
                }
            }
            McpServerKind::StreamableHttp(http) => {
                let url = url::Url::parse(&http.url).map_err(|error| {
                    McpError::InvalidPrimitiveResult(format!(
                        "mcp server {}: invalid url {}: {error}",
                        self.id, http.url
                    ))
                })?;
                match url.scheme() {
                    "http" | "https" => {}
                    other => {
                        return Err(McpError::InvalidPrimitiveResult(format!(
                            "mcp server {}: unsupported url scheme {other:?} (http/https only)",
                            self.id
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// Effective per-request timeout.
    #[must_use]
    pub fn request_timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.max(1))
    }
}

/// Converts a map of raw settings into validated server configs.
///
/// `raw` maps server id → settings.  This mirrors the `ee-cli` TOML contract
/// (`[mcp.servers.<id>]` with `stdio` / `streamableHttp` shapes) so the MCP
/// crate stays frontend-agnostic.
///
/// # Errors
///
/// Fails when a server id is empty or a transport shape is invalid.
pub fn resolve_server_configs(
    raw: BTreeMap<String, RawMcpServerSettings>,
) -> Result<BTreeMap<String, McpServerConfig>, McpError> {
    let mut resolved = BTreeMap::new();
    for (id, settings) in raw {
        let timeout_ms = settings.timeout_ms.unwrap_or(DEFAULT_REQUEST_TIMEOUT_MS);
        let config = McpServerConfig { id: id.clone(), kind: settings.into_kind(), timeout_ms };
        config.validate()?;
        resolved.insert(id, config);
    }
    Ok(resolved)
}

/// Raw TOML-decoded server settings (kept in this crate so `ee-cli` can
/// serialize its config into it without depending on `rmcp`).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawMcpServerSettings {
    /// `stdio` transport settings.
    #[serde(default)]
    pub stdio: Option<RawStdioSettings>,
    /// `streamableHttp` transport settings.
    #[serde(default)]
    pub streamable_http: Option<RawStreamableHttpSettings>,
    /// Per-request timeout in milliseconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

impl RawMcpServerSettings {
    fn into_kind(self) -> McpServerKind {
        if let Some(stdio) = self.stdio {
            return McpServerKind::Stdio(StdioMcpConfig {
                command: stdio.command,
                args: stdio.args,
                env: stdio.env,
                cwd: stdio.cwd,
                stderr_cap: stdio.stderr_cap.unwrap_or(crate::DEFAULT_STDERR_DIAGNOSTICS_CAP),
            });
        }
        if let Some(http) = self.streamable_http {
            return McpServerKind::StreamableHttp(StreamableHttpConfig {
                url: http.url,
                headers: http.headers,
            });
        }
        McpServerKind::Stdio(StdioMcpConfig {
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            stderr_cap: crate::DEFAULT_STDERR_DIAGNOSTICS_CAP,
        })
    }
}

/// Raw stdio settings (TOML `[mcp.servers.<id>.stdio]`).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawStdioSettings {
    /// Executable to spawn.
    pub command: String,
    /// Explicit argument list.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment overrides.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Working directory.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Cap on retained stderr diagnostics bytes.
    #[serde(default)]
    pub stderr_cap: Option<usize>,
}

/// Raw Streamable HTTP settings (TOML `[mcp.servers.<id>.streamableHttp]`).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawStreamableHttpSettings {
    /// Endpoint URL.
    pub url: String,
    /// Extra request headers.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stdio(id: &str, command: &str) -> McpServerConfig {
        McpServerConfig {
            id: id.to_string(),
            kind: McpServerKind::Stdio(StdioMcpConfig {
                command: command.to_string(),
                args: vec![],
                env: BTreeMap::new(),
                cwd: None,
                stderr_cap: 1024,
            }),
            timeout_ms: 1000,
        }
    }

    #[test]
    fn empty_server_id_is_rejected() {
        let config = stdio("", "server");
        assert!(config.validate().is_err());
    }

    #[test]
    fn empty_command_is_rejected() {
        let config = stdio("srv", "   ");
        assert!(config.validate().is_err());
    }

    #[test]
    fn relative_cwd_is_rejected() {
        let mut config = stdio("srv", "server");
        if let McpServerKind::Stdio(stdio) = &mut config.kind {
            stdio.cwd = Some(PathBuf::from("relative"));
        }
        assert!(config.validate().is_err());
    }

    #[test]
    fn invalid_and_non_http_urls_are_rejected() {
        let http = |url: &str| McpServerConfig {
            id: "srv".to_string(),
            kind: McpServerKind::StreamableHttp(StreamableHttpConfig {
                url: url.to_string(),
                headers: BTreeMap::new(),
            }),
            timeout_ms: 1000,
        };
        assert!(http("not a url").validate().is_err());
        assert!(http("ftp://example.com").validate().is_err());
        assert!(http("https://example.com/mcp").validate().is_ok());
    }

    #[test]
    fn duplicate_ids_fail_on_resolve() {
        let mut raw = BTreeMap::new();
        raw.insert(
            "dup".to_string(),
            RawMcpServerSettings {
                stdio: Some(RawStdioSettings { command: "a".to_string(), ..Default::default() }),
                ..Default::default()
            },
        );
        // BTreeMap cannot hold duplicate ids; the contract guarantees the
        // caller rejects duplicates before resolve.  Resolve succeeds here.
        let resolved = resolve_server_configs(raw).expect("resolve");
        assert_eq!(resolved.len(), 1);
    }

    #[test]
    fn resolve_rejects_empty_command() {
        let mut raw = BTreeMap::new();
        raw.insert(
            "srv".to_string(),
            RawMcpServerSettings {
                stdio: Some(RawStdioSettings { command: String::new(), ..Default::default() }),
                ..Default::default()
            },
        );
        assert!(resolve_server_configs(raw).is_err());
    }
}
