//! Standard `mcpServers` JSON configuration import.
//!
//! This is the emergent cross-client format used by Claude Desktop, Cursor,
//! FastMCP, and similar tools:
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "server-name": {
//!       "command": "executable",
//!       "args": ["arg1", "arg2"],
//!       "env": { "VAR": "value" }
//!     }
//!   }
//! }
//! ```
//!
//! Remote entries use `url` plus optional `headers`; `transport` is optional
//! and inferred from the fields. ee does not implement HTTP+SSE, so an `sse`
//! entry is reported and skipped instead of failing the whole file.
//!
//! Sources are import layers only. ee never rewrites them: `.agents/mcp.json`
//! in workspace directories joins the ancestor layers, and
//! `$XDG_CONFIG_HOME/ee/mcp.json` joins the user layer. Explicit
//! `[mcp.servers.<id>]` TOML entries from the same or a higher layer win on
//! duplicate ids.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::mcp::validate_mcp_url;
use super::raw::{McpServerToml, McpToml, McpTransportToml};

/// Hard cap on one standard MCP JSON file. Metadata parsing never needs more,
/// and the cap keeps a hostile workspace file from stalling config load.
const MAX_STANDARD_MCP_BYTES: u64 = 1024 * 1024;

/// Loads and converts one standard MCP JSON file.
///
/// Returns `None` for missing, oversized, unreadable, or malformed files.
/// Entry-level problems are reported and skipped so one bad server cannot
/// disable the rest of the configuration.
pub(super) fn load_standard_mcp_config(path: &Path, base_dir: &Path) -> Option<McpToml> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.len() > MAX_STANDARD_MCP_BYTES => {
            eprintln!(
                "ee: warning: ignoring standard MCP config {}: file exceeds {} bytes",
                path.display(),
                MAX_STANDARD_MCP_BYTES
            );
            return None;
        }
        Ok(_) => {}
        Err(_) => return None,
    }
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) => {
            eprintln!("ee: warning: cannot read standard MCP config {}: {error}", path.display());
            return None;
        }
    };
    match parse_standard_mcp_config(&contents, base_dir, &path.display().to_string()) {
        Ok(config) => Some(config),
        Err(error) => {
            eprintln!("ee: warning: failed to parse {}: {error}", path.display());
            None
        }
    }
}

/// Parses the standard `mcpServers` document.
///
/// `base_dir` resolves a relative per-server `cwd`; for `.agents/mcp.json`
/// callers pass the workspace directory, not the `.agents` directory.
pub(super) fn parse_standard_mcp_config(
    contents: &str,
    base_dir: &Path,
    source: &str,
) -> Result<McpToml, String> {
    let document: StandardMcpDocument =
        serde_json::from_str(contents).map_err(|error| format!("invalid JSON: {error}"))?;
    let mut servers = BTreeMap::new();
    for (id, value) in document.mcp_servers.unwrap_or_default() {
        if id.trim().is_empty() {
            eprintln!("ee: warning: standard MCP config {source}: server id must not be empty");
            continue;
        }
        let entry = match StandardMcpServer::deserialize(value) {
            Ok(entry) => entry,
            Err(error) => {
                eprintln!(
                    "ee: warning: standard MCP config {source}: server `{id}` skipped: {error}"
                );
                continue;
            }
        };
        match entry.into_toml(base_dir) {
            Ok(Some(server)) => {
                servers.insert(id, server);
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!(
                    "ee: warning: standard MCP config {source}: server `{id}` skipped: {error}"
                );
            }
        }
    }
    Ok(McpToml { servers, proxy: None })
}

/// Top-level standard document. Unknown keys (for example `$schema`) are
/// ignored so files written for other clients keep working.
#[derive(Debug, Deserialize)]
struct StandardMcpDocument {
    #[serde(rename = "mcpServers")]
    mcp_servers: Option<BTreeMap<String, serde_json::Value>>,
}

/// One `mcpServers` entry. Unknown per-server keys are ignored for the same
/// compatibility reason.
#[derive(Debug, Default, Deserialize)]
struct StandardMcpServer {
    command: Option<String>,
    args: Option<Vec<String>>,
    env: Option<BTreeMap<String, String>>,
    url: Option<String>,
    transport: Option<String>,
    /// Some clients (VS Code, Claude Code) spell the transport `type`.
    #[serde(rename = "type")]
    transport_type: Option<String>,
    headers: Option<BTreeMap<String, String>>,
    cwd: Option<PathBuf>,
    timeout_ms: Option<u64>,
    disabled: Option<bool>,
    enabled: Option<bool>,
}

impl StandardMcpServer {
    /// Converts one entry into ee's resolved TOML shape.
    ///
    /// `Ok(None)` means the entry is explicitly disabled and must not start.
    fn into_toml(self, base_dir: &Path) -> Result<Option<McpServerToml>, String> {
        if self.disabled == Some(true) || self.enabled == Some(false) {
            return Ok(None);
        }

        let command = self.command.map(|value| value.trim().to_owned()).filter(|v| !v.is_empty());
        let url = self.url.map(|value| value.trim().to_owned()).filter(|v| !v.is_empty());
        let declared_transport = self.transport.as_deref().or(self.transport_type.as_deref());
        let transport = match declared_transport.map(str::trim).filter(|v| !v.is_empty()) {
            Some(raw) => Some(parse_transport(raw)?),
            None => None,
        };

        let transport = match transport {
            Some(transport) => {
                validate_explicit_transport(transport, command.is_some(), url.is_some())?;
                transport
            }
            None => match (command.is_some(), url.is_some()) {
                (true, false) => McpTransportToml::Stdio,
                (false, true) => {
                    if url.as_deref().is_some_and(looks_like_sse) {
                        return Err(String::from(
                            "URL looks like HTTP+SSE, which ee does not support; use a streamable HTTP endpoint",
                        ));
                    }
                    McpTransportToml::StreamableHttp
                }
                (true, true) => {
                    return Err(String::from(
                        "ambiguous server defines both `command` and `url`; add `transport` or keep one",
                    ));
                }
                (false, false) => {
                    return Err(String::from("server must define `command` or `url`"));
                }
            },
        };

        match transport {
            McpTransportToml::Stdio => {
                let command =
                    command.ok_or_else(|| String::from("stdio server requires `command`"))?;
                Ok(Some(McpServerToml {
                    transport,
                    command: Some(command),
                    args: Some(self.args.unwrap_or_default()),
                    env: self.env.unwrap_or_default(),
                    cwd: self.cwd.map(|cwd| resolve_cwd(base_dir, cwd)),
                    url: None,
                    headers: BTreeMap::new(),
                    timeout_ms: None,
                }))
            }
            McpTransportToml::StreamableHttp => {
                let url = url.ok_or_else(|| String::from("http server requires `url`"))?;
                let url = validate_mcp_url(&url)?;
                Ok(Some(McpServerToml {
                    transport,
                    command: None,
                    args: None,
                    env: BTreeMap::new(),
                    cwd: None,
                    url: Some(url),
                    headers: self.headers.unwrap_or_default(),
                    timeout_ms: self.timeout_ms,
                }))
            }
        }
    }
}

fn parse_transport(raw: &str) -> Result<McpTransportToml, String> {
    let normalized = raw.trim().to_ascii_lowercase().replace('_', "-");
    match normalized.as_str() {
        "stdio" => Ok(McpTransportToml::Stdio),
        "http" | "streamable-http" | "streamablehttp" => Ok(McpTransportToml::StreamableHttp),
        "sse" | "http-sse" => {
            Err(String::from("HTTP+SSE transport is not supported; use a streamable HTTP endpoint"))
        }
        other => Err(format!("unsupported transport `{other}`")),
    }
}

fn validate_explicit_transport(
    transport: McpTransportToml,
    has_command: bool,
    has_url: bool,
) -> Result<(), String> {
    match transport {
        McpTransportToml::Stdio => {
            if !has_command {
                return Err(String::from("stdio server requires `command`"));
            }
            if has_url {
                return Err(String::from(
                    "stdio server cannot define `url`; use transport \"http\"",
                ));
            }
        }
        McpTransportToml::StreamableHttp => {
            if !has_url {
                return Err(String::from("http server requires `url`"));
            }
            if has_command {
                return Err(String::from(
                    "http server cannot define `command`; use transport \"stdio\"",
                ));
            }
        }
    }
    Ok(())
}

fn resolve_cwd(base_dir: &Path, cwd: PathBuf) -> PathBuf {
    if cwd.is_absolute() { cwd } else { base_dir.join(cwd) }
}

fn looks_like_sse(url: &str) -> bool {
    url::Url::parse(url).ok().is_some_and(|url| url.path().trim_end_matches('/').ends_with("/sse"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(contents: &str) -> McpToml {
        parse_standard_mcp_config(contents, Path::new("/workspace"), "test").expect("parse")
    }

    #[test]
    fn stdio_entry_parses_command_args_and_env() {
        let config = parse(
            r#"{
                "mcpServers": {
                    "filesystem": {
                        "command": "npx",
                        "args": ["-y", "@modelcontextprotocol/server-filesystem"],
                        "env": { "TOKEN": "secret://mcp-token" }
                    }
                }
            }"#,
        );

        let server = config.servers.get("filesystem").expect("server");
        assert_eq!(server.transport, McpTransportToml::Stdio);
        assert_eq!(server.command.as_deref(), Some("npx"));
        assert_eq!(
            server.args.as_deref(),
            Some(&["-y".to_owned(), "@modelcontextprotocol/server-filesystem".to_owned()][..])
        );
        assert_eq!(server.env.get("TOKEN").map(String::as_str), Some("secret://mcp-token"));
        assert!(server.cwd.is_none());
    }

    #[test]
    fn remote_entry_parses_url_headers_and_timeout() {
        let config = parse(
            r#"{
                "mcpServers": {
                    "weather": {
                        "url": "https://weather.example.com/mcp",
                        "transport": "http",
                        "headers": { "X-Api-Key": "token" },
                        "timeout_ms": 5000
                    }
                }
            }"#,
        );

        let server = config.servers.get("weather").expect("server");
        assert_eq!(server.transport, McpTransportToml::StreamableHttp);
        assert_eq!(server.url.as_deref(), Some("https://weather.example.com/mcp"));
        assert_eq!(server.headers.get("X-Api-Key").map(String::as_str), Some("token"));
        assert_eq!(server.timeout_ms, Some(5000));
    }

    #[test]
    fn transport_is_inferred_from_command_or_url() {
        let config = parse(
            r#"{
                "mcpServers": {
                    "local": { "command": "server" },
                    "remote": { "url": "https://example.com/mcp" }
                }
            }"#,
        );

        assert_eq!(config.servers["local"].transport, McpTransportToml::Stdio);
        assert_eq!(config.servers["remote"].transport, McpTransportToml::StreamableHttp);
    }

    #[test]
    fn disabled_entries_are_skipped() {
        let config = parse(
            r#"{
                "mcpServers": {
                    "off": { "command": "server", "disabled": true },
                    "off2": { "command": "server", "enabled": false },
                    "on": { "command": "server", "enabled": true }
                }
            }"#,
        );

        assert!(!config.servers.contains_key("off"));
        assert!(!config.servers.contains_key("off2"));
        assert!(config.servers.contains_key("on"));
    }

    #[test]
    fn unsupported_sse_and_unknown_transports_are_skipped() {
        let config = parse(
            r#"{
                "mcpServers": {
                    "sse": { "url": "https://example.com/sse", "transport": "sse" },
                    "unknown": { "url": "https://example.com/mcp", "transport": "websocket" },
                    "ok": { "url": "https://example.com/mcp" }
                }
            }"#,
        );

        assert!(!config.servers.contains_key("sse"));
        assert!(!config.servers.contains_key("unknown"));
        assert!(config.servers.contains_key("ok"));
    }

    #[test]
    fn ambiguous_or_incomplete_entries_are_skipped() {
        let config = parse(
            r#"{
                "mcpServers": {
                    "both": { "command": "x", "url": "https://example.com/mcp" },
                    "neither": {},
                    "bad_stdio": { "command": "x", "url": "https://example.com/mcp", "transport": "stdio" },
                    "bad_http": { "command": "x", "transport": "http" }
                }
            }"#,
        );

        assert!(config.servers.is_empty());
    }

    #[test]
    fn invalid_urls_are_skipped_per_entry() {
        let config = parse(
            r#"{
                "mcpServers": {
                    "bad": { "url": "ftp://example.com/mcp" },
                    "good": { "url": "https://example.com/mcp" }
                }
            }"#,
        );

        assert!(!config.servers.contains_key("bad"));
        assert!(config.servers.contains_key("good"));
    }

    #[test]
    fn type_alias_is_accepted_for_transport() {
        let config = parse(
            r#"{
                "mcpServers": {
                    "local": { "type": "stdio", "command": "server" },
                    "remote": { "type": "http", "url": "https://example.com/mcp" }
                }
            }"#,
        );

        assert_eq!(config.servers["local"].transport, McpTransportToml::Stdio);
        assert_eq!(config.servers["remote"].transport, McpTransportToml::StreamableHttp);
    }

    #[test]
    fn inferred_sse_url_is_rejected() {
        let config = parse(
            r#"{
                "mcpServers": {
                    "sse": { "url": "https://example.com/sse?token=1" },
                    "ok": { "url": "https://example.com/mcp" }
                }
            }"#,
        );

        assert!(!config.servers.contains_key("sse"));
        assert!(config.servers.contains_key("ok"));
    }

    #[test]
    fn relative_cwd_resolves_against_workspace_base() {
        let config = parse(r#"{ "mcpServers": { "local": { "command": "x", "cwd": "tools" } } }"#);

        assert_eq!(config.servers["local"].cwd, Some(PathBuf::from("/workspace/tools")));
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let config = parse(
            r#"{
                "$schema": "https://example.com/schema.json",
                "mcpServers": {
                    "filesystem": {
                        "command": "npx",
                        "autoApprove": ["read_file"],
                        "alwaysAllow": true
                    }
                }
            }"#,
        );

        assert!(config.servers.contains_key("filesystem"));
    }

    #[test]
    fn empty_document_and_missing_key_are_valid() {
        assert!(parse("{}").servers.is_empty());
        assert!(parse(r#"{ "mcpServers": {} }"#).servers.is_empty());
    }

    #[test]
    fn invalid_json_is_a_document_error() {
        let error =
            parse_standard_mcp_config("{ not json", Path::new("/workspace"), "test").unwrap_err();
        assert!(error.contains("invalid JSON"));
    }
}
