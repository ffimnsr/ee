//! ACP-native MCP-over-ACP support (Phase 6b audit + facade).
//!
//! The official ACP Rust SDK (`agent-client-protocol = 2.x`, feature
//! `unstable_mcp_over_acp`) provides the MCP-over-ACP wire types in its
//! **v1** schema module: [`McpServer::Acp`] session metadata entries,
//! `mcp/connect`, `mcp/message`, and `mcp/disconnect` requests, and the
//! runtime-agnostic [`McpServer`] / [`McpServerConnect`] serving machinery.
//! These types are not ACP v2-only: they live in `schema::v1` and are gated
//! only by the unstable feature flag, exactly like the already-accepted
//! `unstable_elicitation` feature.
//!
//! # SDK gap: `agent-client-protocol-rmcp`
//!
//! The upstream `agent-client-protocol-rmcp` crate (which builds MCP tools
//! backed by `rmcp`) is **not compatible** with this workspace's dependency
//! set:
//!
//! - `agent-client-protocol-rmcp 3.0.0` (newest) requires `rmcp ^2.1.0`, but
//!   the workspace uses `rmcp 3.1.0` (a different major version).
//! - `agent-client-protocol-rmcp 2.0.1` requires `agent-client-protocol
//!   ^1.3.0`, incompatible with the workspace's `agent-client-protocol = 2.x`.
//!
//! Adopting it would pull a duplicate `rmcp 2.x` into the dependency graph
//! and expose its API types through ee public APIs, which repository policy
//! forbids.  Until upstream publishes a release built against `rmcp 3.x`,
//! `ee-agent-host` keeps a minimal in-process transport adapter (documented
//! in `ee-agent-host/src/mcp_over_acp.rs`) that bridges the SDK's
//! MCP-over-ACP requests into the existing `rmcp`-based
//! [`ee_mcp::EeMcpProxy`] server surface.  The bridge reuses the SDK's wire
//! types below and the `rmcp` server loop; no ACP/MCP wire structs are
//! handrolled.
//!
//! # Feature-flag decision
//!
//! `unstable_mcp_over_acp` is enabled workspace-wide because:
//!
//! 1. The ACP v1 spec feature is the only SDK surface for `McpServer::Acp`;
//! 2. The project already accepts the sibling `unstable_elicitation`
//!    feature for v1 behavior;
//! 3. It is a compile-time schema gate, not a protocol-version bump: the
//!    host still negotiates ACP v1 only ([`crate::version`]) and the schema
//!    types gate no v2 behavior.
//!
//! Everything here re-exports official SDK types; this module owns only the
//! ee-specific policy constants and the `ee` server-name helper.

use agent_client_protocol::schema::v1::{McpServerAcp, McpServerAcpId};

/// The session-metadata `McpServer` enum from the ACP v1 schema.
///
/// Alias avoids the name clash with the runtime-agnostic
/// [`McpServer`](agent_client_protocol::mcp_server::McpServer) re-export
/// below (the SDK itself names both `McpServer`).
type SchemaMcpServer = agent_client_protocol::schema::v1::McpServer;

/// The server name of the ee-owned MCP proxy (server id on the wire).
///
/// Both the ACP-native entry (`McpServer::Acp`) and the stdio fallback entry
/// (`McpServer::Stdio`) advertise this name, and the two modes are mutually
/// exclusive for it (never both in one `session/new`).
pub const EE_PROXY_SERVER_NAME: &str = "ee";

// ── Re-exports from the official SDK ─────────────────────────────────────────

/// Whether an MCP connection is standalone or ACP-attached.
pub use agent_client_protocol::mcp_server::McpConnectionContext;
/// Connection information handed to an [`McpServerConnect`] implementation.
pub use agent_client_protocol::mcp_server::McpConnectionTo;
/// Runtime-agnostic MCP server wrapper (ACP-attachable with the
/// `unstable_mcp_over_acp` feature).
pub use agent_client_protocol::mcp_server::McpServer;
/// Implement this to serve an MCP server over ACP (or standalone).
pub use agent_client_protocol::mcp_server::McpServerConnect;
/// MCP tool registry types (used by `agent-client-protocol-rmcp` upstream;
/// re-exported here so ee crates never name the rmcp 2.x types directly).
pub use agent_client_protocol::mcp_server::{
    EnabledTools, McpTool, McpToolMetadata, McpToolRegistry, McpToolSchema, RegisteredMcpTool,
};

// ── ee proxy policy ──────────────────────────────────────────────────────────

/// Builds the ACP-native session metadata entry for the ee proxy.
///
/// `server_id` must be the id the hosting connection validates in
/// `mcp/connect`; `ee-agent-host` derives it per agent connection.
#[must_use]
pub fn ee_proxy_acp_entry(server_id: McpServerAcpId) -> SchemaMcpServer {
    SchemaMcpServer::Acp(McpServerAcp::new(EE_PROXY_SERVER_NAME, server_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::ClientMethodNames;
    use agent_client_protocol::schema::v1::McpServer as SchemaMcpServer;

    /// The SDK exposes the MCP-over-ACP method names as part of its public
    /// `CLIENT_METHOD_NAMES` metadata (the "official method metadata" the
    /// host registers handlers from).
    #[test]
    fn sdk_method_metadata_covers_mcp_over_acp() {
        let names: &ClientMethodNames = &crate::CLIENT_METHOD_NAMES;
        assert_eq!(names.mcp_connect, "mcp/connect");
        assert_eq!(names.mcp_message, "mcp/message");
        assert_eq!(names.mcp_disconnect, "mcp/disconnect");
    }

    /// The ee proxy entry is an ACP v1 `McpServer::Acp` carrying the `ee`
    /// name and the given opaque server id.
    #[test]
    fn ee_proxy_acp_entry_uses_sdk_types_and_ee_name() {
        let server_id = McpServerAcpId::new("ee-mcp-proxy:test");
        let entry = ee_proxy_acp_entry(server_id.clone());
        match &entry {
            SchemaMcpServer::Acp(acp) => {
                assert_eq!(acp.name, EE_PROXY_SERVER_NAME);
                assert_eq!(acp.server_id, server_id);
            }
            other => panic!("expected McpServer::Acp, got {other:?}"),
        }
    }

    /// The re-exported [`McpServer`] is exactly the SDK's `mcp_server` type
    /// (type identity), so no duplicate runtime-agnostic server type can
    /// creep into ee public APIs.
    #[test]
    fn mcp_server_reexport_is_the_sdk_type() {
        let accept_facade: fn(McpServer<agent_client_protocol::role::mcp::Client>) = |_| {};
        let accept_sdk: fn(
            agent_client_protocol::mcp_server::McpServer<agent_client_protocol::role::mcp::Client>,
        ) = accept_facade;
        // The assignment above compiles only when both names name the same
        // type (function pointers coerce only between identical signatures).
        let _ = accept_sdk;
    }
}
