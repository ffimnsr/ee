//! v1 → v2 session MCP server conversion.
//!
//! ACP v2 requires a transport `type` discriminator on every session MCP
//! server and supports the same three host-side transports as the SDK schema:
//! stdio, HTTP, and the unstable ACP-native transport (`session.mcp.acp`).
//! The ACP-native entry is what carries the application-owned `ee` proxy on a
//! v2 connection, so dropping it silently removes every client tool from the
//! agent.

use super::*;

/// Maps the host's v1 MCP server configs onto the v2 wire.
///
/// Every representable transport keeps its identity (`name`, transport fields,
/// and `_meta`). Unknown transports are dropped with a warning rather than
/// failing the session request.
pub(super) fn mcp_servers_to_v2(servers: &[McpServer]) -> Vec<v2::McpServer> {
    servers.iter().filter_map(mcp_server_to_v2).collect()
}

fn mcp_server_to_v2(server: &McpServer) -> Option<v2::McpServer> {
    match server {
        McpServer::Stdio(stdio) => {
            let env = stdio
                .env
                .iter()
                .filter_map(|variable| {
                    serde_json::from_value(serde_json::to_value(variable).ok()?).ok()
                })
                .collect::<Vec<v2::EnvVariable>>();
            let mut converted = v2::McpServerStdio::new(
                stdio.name.clone(),
                v2::AbsolutePath::new(stdio.command.clone()),
            )
            .args(stdio.args.clone())
            .env(env);
            if let Some(meta) = &stdio.meta {
                converted = converted.meta(meta.clone());
            }
            Some(v2::McpServer::Stdio(converted))
        }
        McpServer::Http(http) => Some(v2::McpServer::Http(
            serde_json::from_value(serde_json::to_value(http).ok()?).ok()?,
        )),
        McpServer::Acp(acp) => {
            let mut converted = v2::McpServerAcp::new(
                acp.name.clone(),
                v2::McpServerAcpId::new(acp.server_id.0.to_string()),
            );
            if let Some(meta) = &acp.meta {
                converted = converted.meta(meta.clone());
            }
            Some(v2::McpServer::Acp(converted))
        }
        other => {
            tracing::warn!(?other, "dropping MCP server config with no v2 representation");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ee_agent_protocol::{McpServerAcp, McpServerHttp};

    #[test]
    fn acp_native_server_survives_v2_conversion() {
        let server =
            McpServer::Acp(McpServerAcp::new("ee", McpServerAcpId::new("ee-mcp-proxy:test")));
        let converted = mcp_servers_to_v2(&[server]);
        let [v2::McpServer::Acp(acp)] = converted.as_slice() else {
            panic!("expected the ACP-native server to survive, got {converted:?}");
        };
        assert_eq!(acp.name, "ee");
        assert_eq!(acp.server_id.0.as_ref(), "ee-mcp-proxy:test");
    }

    #[test]
    fn stdio_and_http_servers_keep_their_identity() {
        let servers = vec![
            McpServer::Stdio(McpServerStdio::new("fs", "/usr/local/bin/mcp-fs")),
            McpServer::Http(McpServerHttp::new("api", "https://example.com/mcp")),
        ];
        let converted = mcp_servers_to_v2(&servers);
        assert_eq!(converted.len(), 2);
        let v2::McpServer::Stdio(stdio) = &converted[0] else {
            panic!("expected stdio first, got {converted:?}");
        };
        assert_eq!(stdio.name, "fs");
        assert_eq!(stdio.command.0, PathBuf::from("/usr/local/bin/mcp-fs"));
        let v2::McpServer::Http(http) = &converted[1] else {
            panic!("expected HTTP second, got {converted:?}");
        };
        assert_eq!(http.name, "api");
        assert_eq!(http.url, "https://example.com/mcp");
    }

    #[test]
    fn legacy_sse_transport_is_dropped_not_fatal() {
        let sse: McpServer = serde_json::from_value(serde_json::json!({
            "type": "sse",
            "name": "legacy",
            "url": "https://example.com/sse",
            "headers": []
        }))
        .expect("v1 SSE server parses");
        let converted = mcp_servers_to_v2(&[sse]);
        assert!(converted.is_empty(), "removed transports must be dropped");
    }
}
