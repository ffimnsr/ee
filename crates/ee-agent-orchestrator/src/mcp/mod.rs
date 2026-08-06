//! Phase 12: bridge session-advertised MCP servers into the orchestrated
//! tool registry.
//!
//! The host appends the ee MCP proxy to `session/new` — ACP-native
//! `McpServer::Acp` when the agent advertises `mcp_capabilities.acp`, the
//! stdio fallback otherwise.  This module gives the orchestrator provider a
//! per-prompt [`McpSessionManager`] that:
//!
//! - connects each advertised server (ACP-native through the framework
//!   `ClientBridge` `mcp/*` methods; stdio through an rmcp child-process
//!   transport), bounded by a per-request timeout and the prompt's
//!   cancellation watch;
//! - lists tools and translates them into provider-compatible
//!   [`ToolDefinition`](crate::tools::ToolDefinition)s with a reversible
//!   display → (server, original) dispatch mapping;
//! - classifies side effects from the *original* MCP tool names and
//!   configured metadata (never sanitized display names), defaulting unknown
//!   external tools to a conservative write/overwrite class that policy
//!   denies by default;
//! - executes model tool intents through MCP `tools/call`, mapping `isError`
//!   results to failed [`ToolResult`](crate::tools::ToolResult)s without
//!   crashing the turn;
//! - closes every connection on prompt end (disconnect / child kill).
//!
//! Secrets in server configuration (stdio env values, headers) are redacted
//! from every event, log, diagnostic, schema, and transcript surface.
//!
//! Transport policy: the official `rmcp` SDK is the only MCP wire
//! implementation.  The ACP bridge ([`acp_transport`]) is transport
//! plumbing only — it maps the SDK's ACP `mcp/*` types onto rmcp's
//! `Transport<RoleClient>` trait, mirroring the host's server-side bridge in
//! `ee-agent-host`; no ACP or MCP wire structs are handrolled.

mod acp_transport;
mod descriptor;
mod manager;
mod names;
mod policy;
mod schema;

pub(crate) use descriptor::McpServerDescriptor;
pub(crate) use manager::policy_filters_all;
pub(crate) use manager::{McpBackedTool, McpDiscoveryDiagnostic, McpSessionManager};
/// Provider-facing MCP tool classification knobs (also re-exported from the
/// crate root for provider configuration).
pub use policy::{McpToolClassSpec, McpToolPolicy};
