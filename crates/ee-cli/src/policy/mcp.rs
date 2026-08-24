//! Generic MCP invocation trust (Phase 3).
//!
//! An [`McpInvocation`] is a validated generic MCP tool invocation: server
//! identity, transport identity, tool name, manifest schema version,
//! side-effect class, canonical workspace identity, and canonical exact JSON
//! arguments.  Rule creation runs only after server identity, tool schema
//! validation, and side-effect classification succeed; unknown servers,
//! unknown tools, missing manifests, unknown side-effect classes, and
//! schema-version mismatches prompt instead.

use super::{OperationIdentity, TransportKind, TrustCategory, TrustOperation, WorkspaceIdentity};

/// Validated exact MCP invocation, matched field-for-field by `McpRule`
/// (see `super::rules`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpInvocation {
    pub(crate) workspace: WorkspaceIdentity,
    /// Optional agent id; `None` scopes the rule to any configured agent in
    /// the matching workspace.
    pub(crate) agent: Option<String>,
    pub(crate) transport: TransportKind,
    /// Stable transport identity (e.g. `stdio:ee --mcp-proxy` or `acp:ee`);
    /// grants never cross transports.
    pub(crate) transport_identity: String,
    pub(crate) server: String,
    pub(crate) tool: String,
    pub(crate) tool_schema_version: u64,
    /// Side-effect class mapped to the shared operation category.
    pub(crate) category: TrustCategory,
    /// Canonical compact JSON object (sorted keys, no duplicates).
    pub(crate) arguments_json: String,
}

impl McpInvocation {
    /// Normalized operation for the shared evaluator.
    pub(crate) fn to_operation(&self) -> TrustOperation {
        TrustOperation {
            workspace: self.workspace,
            agent: self.agent.clone(),
            transport: self.transport,
            category: self.category,
            identity: self.to_identity(),
        }
    }

    /// The exact-match operation identity.
    pub(crate) fn to_identity(&self) -> OperationIdentity {
        OperationIdentity::Mcp {
            server: self.server.clone(),
            transport_identity: self.transport_identity.clone(),
            tool: self.tool.clone(),
            tool_schema_version: self.tool_schema_version,
            arguments_json: self.arguments_json.clone(),
        }
    }
}

/// Stable rule id for a newly created MCP grant (`mcp_…`).
pub(crate) fn generate_mcp_rule_id() -> String {
    format!("mcp_{:016x}", rand::random::<u64>())
}
