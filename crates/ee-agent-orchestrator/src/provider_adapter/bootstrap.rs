//! Session bootstrap helpers: advertised capabilities and the session-scoped
//! remote instruction cache.
//!
//! These live outside the `AgentProvider` implementation so the protocol method
//! bodies stay small; nothing here owns state that a session does not already
//! hold.

use std::sync::{Arc, Mutex};

use ee_agent_protocol::{
    AgentCapabilities, McpCapabilities, SessionCapabilities, SessionCloseCapabilities,
    SessionListCapabilities, SessionResumeCapabilities,
};

use super::OrchestratorProviderConfig;
use crate::config::InstructionSourceConfig;
use crate::prompt::{RemoteInstructionFetcher, WorkspaceMessage};

/// Session-scoped cache of fetched remote instructions.
///
/// `None` means "not fetched yet"; an empty vector is a completed fetch that
/// produced nothing. The cache is per session so a reconnect re-evaluates an
/// unreachable URL instead of inheriting a stale failure.
pub(super) type RemoteInstructionCache = Arc<Mutex<Option<Vec<WorkspaceMessage>>>>;

/// Advertised ACP capabilities for one provider configuration.
///
/// Load is supported (restores persisted state); session listing and closing
/// are handled by the framework. Recovery-enabled providers also advertise
/// `session/resume` (checkpoint restore without replay). The provider hosts
/// MCP-over-ACP for every session, so `mcp_capabilities.acp` is advertised and
/// hosts append the ee proxy as `McpServer::Acp` instead of the stdio fallback.
pub(super) fn capabilities(config: &OrchestratorProviderConfig) -> AgentCapabilities {
    let mut session_capabilities = SessionCapabilities::new()
        .list(SessionListCapabilities::new())
        .close(SessionCloseCapabilities::new());
    if config.orchestrator.recovery.is_durable() {
        session_capabilities = session_capabilities.resume(SessionResumeCapabilities::new());
    }
    AgentCapabilities::default()
        .load_session(true)
        .mcp_capabilities(McpCapabilities::new().acp(true))
        .session_capabilities(session_capabilities)
}

/// Configuration and injected client for configured remote instruction URLs.
pub(super) type RemoteInstructionInputs =
    (InstructionSourceConfig, Option<RemoteInstructionFetcher>);

/// Copies the remote-instruction configuration out of a provider configuration.
pub(super) fn remote_instruction_inputs(
    config: &OrchestratorProviderConfig,
) -> RemoteInstructionInputs {
    (config.orchestrator.instructions.clone(), config.remote_instruction_fetcher.clone())
}

/// Returns the session's configured remote instruction messages.
///
/// Fetching happens on the first turn only; later turns reuse the cached set so
/// a long session never re-downloads an external document. Offline mode and an
/// empty URL list short-circuit without touching the network.
pub(super) async fn remote_instructions(
    config: &InstructionSourceConfig,
    fetcher: Option<&RemoteInstructionFetcher>,
    cache: &RemoteInstructionCache,
) -> Vec<WorkspaceMessage> {
    if !config.has_remote() {
        return Vec::new();
    }
    if let Some(cached) = cache.lock().expect("remote instruction cache poisoned").clone() {
        return cached;
    }
    let fetched = crate::prompt::remote::fetch(config, fetcher).await;
    *cache.lock().expect("remote instruction cache poisoned") = Some(fetched.clone());
    fetched
}
