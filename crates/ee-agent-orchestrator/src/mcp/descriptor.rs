//! Session MCP server descriptors (Phase 12).
//!
//! `session/new` carries `mcpServers` entries; the provider captures them
//! once per session as validated [`McpServerDescriptor`]s.  Descriptors keep
//! the raw transport configuration needed to connect (including stdio env
//! values), but every Debug/display/event/log surface shows only the
//! redacted summary — env/header values never reach transcripts, schemas,
//! events, logs, or memory.  Unsupported transports (streamable HTTP, SSE)
//! fail closed at `session/new`.

use std::fmt;

use ee_agent_protocol::{McpServer, McpServerAcpId, McpServerStdio};

/// Cap on a redacted descriptor summary (bounded diagnostics).
const REDACTED_SUMMARY_MAX_CHARS: usize = 200;

/// One validated MCP server captured at `session/new`.
#[derive(Clone)]
pub(crate) struct McpServerDescriptor {
    /// The server's wire name (used for display namespacing; `ee` for the
    /// ee proxy).
    pub name: String,
    /// The transport used to connect.
    pub kind: McpTransportKind,
    /// Secret-free one-line summary for events and diagnostics.
    pub redacted: String,
}

/// Manual Debug: only the redacted summary is ever printed, so stdio env
/// values (which can carry tokens) cannot leak through `{:?}`.
impl fmt::Debug for McpServerDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpServerDescriptor")
            .field("name", &self.name)
            .field("redacted", &self.redacted)
            .finish()
    }
}

/// The transport of one MCP server descriptor.
#[derive(Clone)]
pub(crate) enum McpTransportKind {
    /// ACP-native MCP server hosted by the ACP client (`mcp/connect`).
    Acp {
        /// The opaque server id the host validates in `mcp/connect`.
        server_id: McpServerAcpId,
    },
    /// Stdio server spawned by this agent.
    Stdio(McpServerStdio),
}

impl fmt::Debug for McpTransportKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Acp { server_id } => f.debug_tuple("Acp").field(server_id).finish(),
            Self::Stdio(stdio) => f.debug_tuple("Stdio").field(&stdio.name).finish(),
        }
    }
}

impl McpServerDescriptor {
    /// Validates one `session/new` MCP server entry, fail closed.
    ///
    /// # Errors
    ///
    /// Returns a diagnostic for unsupported transports (streamable HTTP and
    /// SSE are never advertised by this agent) and invalid names/commands.
    pub(crate) fn from_wire(server: McpServer) -> Result<Self, String> {
        match server {
            McpServer::Acp(acp) => {
                if acp.name.is_empty() {
                    return Err("MCP server entry has an empty name".into());
                }
                Ok(Self {
                    name: acp.name.clone(),
                    kind: McpTransportKind::Acp { server_id: acp.server_id },
                    redacted: format!("acp server {:?}", acp.name),
                })
            }
            McpServer::Stdio(stdio) => {
                if stdio.name.is_empty() {
                    return Err("MCP stdio server entry has an empty name".into());
                }
                if stdio.command.as_os_str().is_empty() {
                    return Err(format!("MCP stdio server {:?} has an empty command", stdio.name));
                }
                let redacted = redacted_stdio_summary(&stdio);
                Ok(Self {
                    name: stdio.name.clone(),
                    kind: McpTransportKind::Stdio(stdio),
                    redacted,
                })
            }
            McpServer::Http(http) => Err(format!(
                "MCP server {:?} uses the streamable-http transport, which this agent does not support (advertised transports: acp, stdio)",
                http.name
            )),
            McpServer::Sse(sse) => Err(format!(
                "MCP server {:?} uses the SSE transport, which this agent does not support (advertised transports: acp, stdio)",
                sse.name
            )),
            other => Err(format!("unsupported MCP server transport for {:?}", other_name(&other))),
        }
    }
}

fn other_name(server: &McpServer) -> String {
    match server {
        McpServer::Acp(acp) => acp.name.clone(),
        McpServer::Stdio(stdio) => stdio.name.clone(),
        McpServer::Http(http) => http.name.clone(),
        McpServer::Sse(sse) => sse.name.clone(),
        _ => "<unknown>".to_string(),
    }
}

/// Secret-free summary of a stdio descriptor: command, argument/env counts.
/// Env values are never rendered.
fn redacted_stdio_summary(stdio: &McpServerStdio) -> String {
    let mut text = format!(
        "stdio server {:?} (command {:?}, {} args, {} env vars)",
        stdio.name,
        stdio.command.display(),
        stdio.args.len(),
        stdio.env.len()
    );
    if text.len() > REDACTED_SUMMARY_MAX_CHARS {
        text.truncate(REDACTED_SUMMARY_MAX_CHARS);
        text.push('…');
    }
    text
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ee_agent_protocol::{EnvVariable, McpServerHttp, McpServerSse, McpServerStdio};

    use super::*;

    fn stdio_entry(name: &str) -> McpServerStdio {
        McpServerStdio::new(name, PathBuf::from("/usr/bin/server"))
            .env(vec![EnvVariable::new("API_TOKEN", "sekrit")])
    }

    #[test]
    fn acp_descriptor_keeps_server_id_and_redacts_nothing_sensitive() {
        let descriptor = McpServerDescriptor::from_wire(ee_agent_protocol::ee_proxy_acp_entry(
            McpServerAcpId::new("ee-mcp-proxy:test"),
        ))
        .expect("acp entry validates");
        assert_eq!(descriptor.name, "ee");
        assert_eq!(descriptor.redacted, "acp server \"ee\"");
        let McpTransportKind::Acp { server_id } = &descriptor.kind else {
            panic!("expected acp kind");
        };
        assert_eq!(server_id.to_string(), "ee-mcp-proxy:test");
    }

    #[test]
    fn stdio_descriptor_keeps_env_for_spawn_but_redacts_values() {
        let descriptor =
            McpServerDescriptor::from_wire(McpServer::Stdio(stdio_entry("filesystem")))
                .expect("stdio entry validates");
        let McpTransportKind::Stdio(stdio) = &descriptor.kind else {
            panic!("expected stdio kind");
        };
        assert_eq!(stdio.env[0].value, "sekrit", "raw env is kept for spawning");
        assert!(!descriptor.redacted.contains("sekrit"), "redacted summary drops values");
        assert!(descriptor.redacted.contains("filesystem"));
        assert!(descriptor.redacted.contains("1 env vars"));
        let debug = format!("{descriptor:?}");
        assert!(!debug.contains("sekrit"), "Debug never renders env values: {debug}");
    }

    #[test]
    fn http_and_sse_entries_fail_closed() {
        let http = McpServer::Http(McpServerHttp::new("remote", "https://example.com/mcp"));
        let error = McpServerDescriptor::from_wire(http).expect_err("http rejected");
        assert!(error.contains("streamable-http"), "{error}");

        let sse = McpServer::Sse(McpServerSse::new("legacy", "https://example.com/sse"));
        let error = McpServerDescriptor::from_wire(sse).expect_err("sse rejected");
        assert!(error.contains("SSE"), "{error}");
    }

    #[test]
    fn empty_names_and_commands_fail_closed() {
        let empty_name =
            McpServer::Stdio(McpServerStdio::new("", PathBuf::from("/usr/bin/server")));
        assert!(McpServerDescriptor::from_wire(empty_name).is_err());

        let empty_command = McpServer::Stdio(McpServerStdio::new("srv", PathBuf::new()));
        assert!(McpServerDescriptor::from_wire(empty_command).is_err());
    }

    #[test]
    fn descriptor_debug_hides_all_env_data() {
        let descriptor = McpServerDescriptor::from_wire(McpServer::Stdio(stdio_entry("s")));
        let debug = format!("{:?}", descriptor);
        for needle in ["sekrit", "API_TOKEN"] {
            assert!(!debug.contains(needle), "{needle} must not appear in {debug}");
        }
    }
}
