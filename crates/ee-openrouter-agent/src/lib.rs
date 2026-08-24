//! OpenRouter ACP v1 agent provider built on `ee-acp-agent-server`.
//!
//! [`provider::OpenRouterProvider`] carries the business logic (session
//! history, prompt turns, the bounded tool loop) while the framework owns
//! JSON-RPC dispatch, version negotiation, the session store, typed updates,
//! and agent → client requests.  The binary in `main.rs` only parses
//! arguments and runs the framework over stdio.
//!
//! All `OPENROUTER_*` configuration variables are documented on
//! [`config::Args`]; `.env` parsing lives in [`dotenv`] and never mutates
//! the process environment.  `OPENROUTER_API_KEY` is used only for the
//! Authorization header and is never logged.
//!
//! Orchestrated mode is the default: [`orchestrated::OpenRouterModelAdapter`]
//! feeds OpenRouter into `ee-agent-orchestrator`'s bounded model–tool loop
//! instead of the simple provider mode. `OPENROUTER_ORCHESTRATED=0` is a
//! temporary fallback for diagnostics.
//!
//! # MCP servers (Phase 12)
//!
//! Orchestrated sessions discover session-advertised MCP servers and bridge
//! their tools into the model–tool loop:
//!
//! - The provider advertises `mcp_capabilities.acp`, so the host appends the
//!   ee MCP proxy as an ACP-native `McpServer::Acp` entry and the agent
//!   drives it with `mcp/connect` / `mcp/message` / `mcp/disconnect`.
//! - Stdio `mcpServers` entries are spawned by the agent (the ee proxy stdio
//!   fallback is the same path).  Streamable-HTTP and SSE entries fail
//!   closed at `session/new` (never advertised).
//! - MCP tools reach the model under provider-compatible names: the ee proxy
//!   keeps its `ee_*` names (`ee_workspace_roots`, `ee_search_text`, edit,
//!   format, rename, and terminal tools); external servers are namespaced
//!   `mcp_<server>_<tool>`.  Side effects are classified from the original
//!   MCP names, so write/execute tools stay behind the orchestrator policy
//!   (the default orchestrated policy allows reads, executes, and delegates
//!   but denies writes).
//! - Server configuration secrets (stdio env values) are redacted from every
//!   transcript, schema, event, log, diagnostic, and error surface.
//! - Discovery failures surface as bounded diagnostics; the turn continues
//!   without the failed server's tools.

pub mod compaction;
pub mod config;
pub mod dotenv;
pub mod openrouter;
pub mod orchestrated;
pub mod provider;
pub mod tools;
