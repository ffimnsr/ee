//! Agent host for ee agents mode: owns agent subprocess lifecycle, session
//! state, approval gates, and lazy MCP server startup.
//!
//! Protocol types and transport helpers come from the official ACP Rust SDK
//! (re-exported through `ee-agent-protocol`); this crate adds editor-facing
//! integration: session lifecycle, update reduction, the permission broker,
//! and connection lifecycle with timeouts and cancellation.  Agent
//! subprocesses must start lazily after the user enables agents mode and
//! opens the agents pane or runs an agents command.  All agent terminal
//! executions and file writes require an approval path before execution,
//! and agent file writes must route through existing buffer/edit/save
//! semantics (Phase 4 bridges implement those handlers).
//!
//! The crate is UI-free: it exposes deterministic [`AgentEvent`] values and
//! command methods; rendering lives in `ee-cli`.

pub mod connection;
pub mod critic;
pub mod error;
pub mod events;
pub mod inbound;
pub mod manager;
pub mod mcp_over_acp;
pub mod permission;
pub mod process;
pub mod redact;
pub mod reducer;
pub mod session;
pub mod turn_evidence;
pub mod web_context;
mod workspace_memory;
mod workspace_verified_facts;

#[cfg(feature = "test-utils")]
pub mod fake;

pub use connection::{
    AgentConnection, AgentConnectionOptions, DEFAULT_HANDSHAKE_TIMEOUT,
    DEFAULT_MAX_CONCURRENT_PROMPTS, DEFAULT_REQUEST_TIMEOUT,
};
pub use critic::{
    CriticAgentBroker, CriticBackendSelection, CriticRevisionObserver,
    DEFAULT_EXTERNAL_CRITIC_TIMEOUT, ExternalCriticAttribution, ExternalCriticConfig,
    ExternalCriticPreview, ExternalCriticTrust, ExternalCriticUnavailable,
    ExternalCritiqueCompleted, ExternalCritiqueOutcome, ExternalCritiqueRequest,
    MAX_EXTERNAL_CRITIC_CONTEXT_BYTES, MAX_EXTERNAL_CRITIC_ROOTS,
};
pub use ee_agent_memory::MemoryRetention;
pub use ee_agent_orchestrator::{
    CritiqueTarget, ReportEvidence, ResolvedRubberDuckConfig, RubberDuckBackend, RubberDuckConfig,
    RubberDuckConfigError, RubberDuckConfigUnavailable, RubberDuckMode, VerifiedCritiqueReport,
    finding_counts,
};
pub use error::AgentError;
pub use events::{
    AgentConnectionState, AgentEvent, ConnectionCloseReason, PermissionRequestId,
    PermissionRequestInfo, ThreadCloseReason, TurnMetrics,
};
pub use inbound::{
    ClientRequest, ClientRequestHandler, ClientRequestResponse, ClientRequestResult,
    DenyAllHandler, HandlerCapabilities, ProxyTextEdit, RecordingHandler,
    WorkspaceMemoryMutationOperation,
};
#[cfg(feature = "test-utils")]
pub use manager::FakeTransportFactory;
pub use manager::{AgentManager, AgentManagerConfig, build_context_pack_with_workspace_recaller};
pub use mcp_over_acp::{EeProxyMode, EeProxyToolProfile, MCP_OVER_ACP_MAX_FRAME_BYTES};
pub use permission::PermissionBroker;
pub use process::{AgentProcessConfig, STDERR_MAX_LINE_BYTES, STDERR_MAX_LINES, StderrCapture};
pub use reducer::{
    MessageKind, ReducedMessage, SessionState, ToolCallState, UsageInfo, apply_update,
};
pub use session::AgentThread;
pub use turn_evidence::{
    EvidenceCheck, EvidenceRecord, EvidenceRevision, HostValidationRecord, MAX_EVIDENCE_FILES,
    MAX_TURN_OBSERVATIONS, PromptTerminalOutcome, SafeFollowUp, TurnBlocker, TurnEvidence,
    TurnEvidenceError, TurnEvidenceSummary, TurnKey, TurnObservation, TurnTerminalStatus,
    WriteEvidenceOutcome, WriteTransactionStage,
};
pub use web_context::{
    AgentWebContextConfig, BrowserRunRetryPolicy, ReqwestWebTransport, WebContextConfigError,
    WebContextError, WebContextErrorCode, WebContextLimits, WebContextService, WebFetchRequest,
    WebFetchResponse, WebSearchProvenance, WebSearchRequest, WebSearchResponse, WebSearchResult,
    WebTransport, WebTransportRequest, WebTransportResponse,
};
pub use workspace_memory::{
    DEFAULT_WORKSPACE_MEMORY_CANDIDATE_RETENTION_DAYS, DEFAULT_WORKSPACE_MEMORY_EXPIRY_DAYS,
    DEFAULT_WORKSPACE_MEMORY_STALE_RETENTION_DAYS,
    DEFAULT_WORKSPACE_MEMORY_SUPERSEDED_RETENTION_DAYS, WorkspaceMemoryAvailability,
    WorkspaceMemoryBulkMutationResult, WorkspaceMemoryExportDto, WorkspaceMemoryExportProvenance,
    WorkspaceMemoryExportedFact, WorkspaceMemoryHostConfig, WorkspaceMemoryHostError,
    WorkspaceMemoryHostErrorCode, WorkspaceMemoryHostStatus, WorkspaceMemoryMutationApproval,
    WorkspaceMemoryQuotas,
};
pub use workspace_verified_facts::{
    WorkspaceVerifiedFactAuthority, WorkspaceVerifiedFactCandidate,
    WorkspaceVerifiedFactCandidateError, WorkspaceVerifiedFactFreshness,
    WorkspaceVerifiedSourceIdentity, derive_workspace_verified_fact_candidates,
};

/// ACP version the host speaks.
pub const ACP_VERSION: &str = ee_agent_protocol::SUPPORTED_ACP_VERSION;

/// MCP protocol version the host speaks.
pub const MCP_PROTOCOL_VERSION: &str = ee_mcp::MCP_PROTOCOL_VERSION;

/// Returns `(ACP version, MCP protocol version)` supported by this host build.
pub fn supported_protocol_versions() -> (&'static str, &'static str) {
    (ACP_VERSION, MCP_PROTOCOL_VERSION)
}

#[cfg(test)]
mod tests {
    #[test]
    fn host_speaks_latest_acp_and_mcp_versions() {
        let (acp, mcp) = super::supported_protocol_versions();
        assert_eq!(acp, "1");
        assert_eq!(mcp, "2026-07-28");
    }
}
