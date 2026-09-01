//! Agent manager: the host entry point the editor integration (ee-cli) owns.
//!
//! The manager holds the resolved agent configurations, lazily starts
//! connections on first session creation, and never spawns a subprocess
//! unless a session is actually requested.  Dropping the manager closes
//! every connection (killing subprocesses and resolving pending work).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::connection::{AgentConnection, AgentConnectionOptions};
use crate::error::AgentError;
use crate::events::AgentEvent;
use crate::inbound::ClientRequestHandler;
use crate::process::AgentProcessConfig;
use crate::session::AgentThread;
use crate::turn_evidence::TurnEvidence;
use crate::workspace_memory::{
    WorkspaceMemoryBulkMutationResult, WorkspaceMemoryExportDto, WorkspaceMemoryHost,
    WorkspaceMemoryHostConfig, WorkspaceMemoryHostError, WorkspaceMemoryHostStatus,
    WorkspaceMemoryMutationApproval,
};
use crate::workspace_verified_facts::{
    WorkspaceVerifiedFactCandidate, WorkspaceVerifiedFactCandidateError,
    WorkspaceVerifiedSourceIdentity, derive_workspace_verified_fact_candidates,
};
use ee_agent_orchestrator::{
    ContextPack, ContextPackBuilder, WorkspaceContextFact, WorkspaceFactAuthority,
    WorkspaceFactFreshness, WorkspaceFactSelectionReason, WorkspaceFactState,
    WorkspaceRecallContext,
};
use ee_mcp::{WorkspaceFact, WorkspaceFactMutationResult, WorkspaceFactsResult};

#[cfg(feature = "test-utils")]
use crate::fake::FakeAgentTransport;

/// Builds one fake agent transport for a connection (test-utils only).
///
/// Implementations call [`crate::fake::FakeAgent::spawn`] and return the
/// transport; keeping the spawned [`crate::fake::FakeAgent`] handle is the
/// caller's responsibility (via interior mutability) so tests can assert on
/// the host's requests.
#[cfg(feature = "test-utils")]
pub trait FakeTransportFactory: Send + Sync + 'static {
    /// Builds the transport for one connection.
    fn build(&self) -> FakeAgentTransport;
}

/// Resolved agents-mode configuration the manager operates on.
#[derive(Clone, Default)]
pub struct AgentManagerConfig {
    /// Agent id → launch config.
    pub agents: BTreeMap<String, AgentProcessConfig>,
    /// Whether the ee MCP proxy is configured (arms ACP-native MCP-over-ACP
    /// hosting on every connection; the agent still has to advertise
    /// `mcp_capabilities.acp`).
    pub ee_proxy_enabled: bool,
    /// Resolved, explicit workspace-memory settings. Disabled by default.
    pub workspace_memory: WorkspaceMemoryHostConfig,
    /// Test-only: agent id → fake transport factory.  When present for an
    /// agent, the manager connects over the fake transport instead of
    /// spawning a subprocess.
    #[cfg(feature = "test-utils")]
    pub fake_transports: BTreeMap<String, Arc<dyn FakeTransportFactory>>,
}

impl std::fmt::Debug for AgentManagerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentManagerConfig")
            .field("agents", &self.agents.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// Owns lazy agent connections and session creation.
#[derive(Clone)]
pub struct AgentManager {
    config: Arc<AgentManagerConfig>,
    handler: Arc<dyn ClientRequestHandler>,
    events: mpsc::UnboundedSender<AgentEvent>,
    options: AgentConnectionOptions,
    workspace_memory: Arc<WorkspaceMemoryHost>,
    connections: Arc<Mutex<BTreeMap<String, AgentConnection>>>,
}

impl std::fmt::Debug for AgentManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentManager")
            .field("agents", &self.config.agents.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl AgentManager {
    /// Creates a manager over the resolved config.
    ///
    /// No subprocess is started here; connections start lazily on
    /// [`Self::new_session`].
    #[must_use]
    pub fn new(
        config: AgentManagerConfig,
        handler: Arc<dyn ClientRequestHandler>,
        events: mpsc::UnboundedSender<AgentEvent>,
    ) -> Self {
        Self::with_options(config, handler, events, AgentConnectionOptions::default())
    }

    /// Creates a manager with explicit connection tuning (tests).
    #[must_use]
    pub fn with_options(
        config: AgentManagerConfig,
        handler: Arc<dyn ClientRequestHandler>,
        events: mpsc::UnboundedSender<AgentEvent>,
        options: AgentConnectionOptions,
    ) -> Self {
        let workspace_memory = WorkspaceMemoryHost::new(&config.workspace_memory);
        Self::with_options_and_workspace_memory(config, handler, events, options, workspace_memory)
    }

    fn with_options_and_workspace_memory(
        config: AgentManagerConfig,
        handler: Arc<dyn ClientRequestHandler>,
        events: mpsc::UnboundedSender<AgentEvent>,
        options: AgentConnectionOptions,
        workspace_memory: Arc<WorkspaceMemoryHost>,
    ) -> Self {
        Self {
            config: Arc::new(config),
            handler,
            events,
            options,
            workspace_memory,
            connections: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Sanitized workspace-memory status. Never exposes database path or raw storage errors.
    #[must_use]
    pub fn workspace_memory_status(&self) -> WorkspaceMemoryHostStatus {
        self.workspace_memory.status()
    }

    /// Lists bounded active facts from configured primary workspace.
    pub fn workspace_memory_list(
        &self,
        limit: usize,
    ) -> Result<WorkspaceFactsResult, WorkspaceMemoryHostError> {
        self.workspace_memory.list_primary(limit)
    }

    /// Recalls bounded matching facts from configured primary workspace.
    pub fn workspace_memory_recall(
        &self,
        query: impl Into<String>,
        limit: usize,
    ) -> Result<WorkspaceFactsResult, WorkspaceMemoryHostError> {
        self.workspace_memory.recall_primary(query.into(), limit)
    }

    /// Builds a context pack after bounded workspace-memory recall.
    ///
    /// Every deterministic query is bounded by the context-pack fact cap. Any
    /// recall or DTO validation failure discards the complete recall set.
    #[must_use]
    pub fn build_context_pack_with_workspace_recall(
        &self,
        builder: ContextPackBuilder,
        context: &WorkspaceRecallContext,
    ) -> ContextPack {
        let include_stale = context.freshness_policy
            == ee_agent_orchestrator::WorkspaceRecallFreshnessPolicy::IncludePotentiallyStaleWithWarning;
        build_context_pack_with_workspace_recaller(builder, context, |query, limit| {
            self.workspace_memory.recall_primary_with_stale(query.to_string(), limit, include_stale)
        })
    }

    /// Reads one exact `default` namespace fact from configured primary workspace.
    pub fn workspace_memory_read(
        &self,
        key: impl Into<String>,
    ) -> Result<WorkspaceFact, WorkspaceMemoryHostError> {
        self.workspace_memory.read_primary(key.into())
    }

    /// Stores one explicitly confirmed user assertion in configured primary workspace.
    pub fn workspace_memory_remember_approved(
        &self,
        key: impl Into<String>,
        value: impl Into<String>,
        approval: WorkspaceMemoryMutationApproval,
    ) -> Result<WorkspaceFactMutationResult, WorkspaceMemoryHostError> {
        self.workspace_memory.remember_primary_approved(key.into(), value.into(), approval)
    }

    /// Derives narrow host-verified candidates from one immutable turn snapshot.
    pub fn workspace_memory_derive_verified_candidates(
        &self,
        evidence: &TurnEvidence,
    ) -> Result<Vec<WorkspaceVerifiedFactCandidate>, WorkspaceVerifiedFactCandidateError> {
        derive_workspace_verified_fact_candidates(evidence)
    }

    /// Revalidates evidence proof, then stores one explicitly approved candidate.
    pub fn workspace_memory_promote_verified_approved(
        &self,
        candidate: WorkspaceVerifiedFactCandidate,
        evidence: &TurnEvidence,
        approval: WorkspaceMemoryMutationApproval,
    ) -> Result<WorkspaceFactMutationResult, WorkspaceMemoryHostError> {
        self.workspace_memory.promote_verified_primary_approved(candidate, evidence, approval)
    }

    /// Marks revision-bound facts stale after host observes changed source identity.
    pub fn workspace_memory_invalidate_verified_source(
        &self,
        observed: WorkspaceVerifiedSourceIdentity,
    ) -> Result<WorkspaceMemoryBulkMutationResult, WorkspaceMemoryHostError> {
        self.workspace_memory.invalidate_verified_source(observed)
    }

    /// Forgets all versions of one explicitly confirmed exact key.
    pub fn workspace_memory_forget_approved(
        &self,
        key: impl Into<String>,
        approval: WorkspaceMemoryMutationApproval,
    ) -> Result<WorkspaceFactMutationResult, WorkspaceMemoryHostError> {
        self.workspace_memory.forget_primary_approved(key.into(), approval)
    }

    /// Retracts one explicitly confirmed active exact key.
    pub fn workspace_memory_retract_approved(
        &self,
        key: impl Into<String>,
        approval: WorkspaceMemoryMutationApproval,
    ) -> Result<WorkspaceFactMutationResult, WorkspaceMemoryHostError> {
        self.workspace_memory.retract_primary_approved(key.into(), approval)
    }

    /// Clears configured primary workspace after explicit frontend confirmation.
    pub fn workspace_memory_clear_approved(
        &self,
        approval: WorkspaceMemoryMutationApproval,
    ) -> Result<WorkspaceMemoryBulkMutationResult, WorkspaceMemoryHostError> {
        self.workspace_memory.clear_primary_approved(approval)
    }

    /// Exports configured primary workspace after explicit frontend confirmation.
    pub fn workspace_memory_export_approved(
        &self,
        include_values: bool,
        approval: WorkspaceMemoryMutationApproval,
    ) -> Result<WorkspaceMemoryExportDto, WorkspaceMemoryHostError> {
        self.workspace_memory.export_primary_approved(include_values, approval)
    }

    /// Imports a versioned export after explicit frontend confirmation.
    pub fn workspace_memory_import_approved(
        &self,
        export: WorkspaceMemoryExportDto,
        approval: WorkspaceMemoryMutationApproval,
    ) -> Result<WorkspaceMemoryBulkMutationResult, WorkspaceMemoryHostError> {
        self.workspace_memory.import_primary_approved(export, approval)
    }

    /// The configured agent ids.
    #[must_use]
    pub fn agent_ids(&self) -> Vec<String> {
        self.config.agents.keys().cloned().collect()
    }

    /// Whether `agent_id` is configured.
    #[must_use]
    pub fn has_agent(&self, agent_id: &str) -> bool {
        self.config.agents.contains_key(agent_id)
    }

    /// Current state of an already-live connection.
    ///
    /// Returns `None` without starting the configured agent when no connection
    /// exists yet. Picker and status UIs use this to preserve lazy startup.
    #[must_use]
    pub fn connection_state(&self, agent_id: &str) -> Option<crate::events::AgentConnectionState> {
        self.connections
            .lock()
            .expect("connections poisoned")
            .get(agent_id)
            .map(AgentConnection::state)
    }

    /// The default agent id (`agents.default_agent` resolved by the caller),
    /// falling back to the single configured agent when unambiguous.
    #[must_use]
    pub fn resolve_default_agent(&self, default_agent: Option<&str>) -> Option<String> {
        if let Some(default) = default_agent {
            return self.config.agents.contains_key(default).then(|| default.to_string());
        }
        match self.config.agents.len() {
            1 => self.config.agents.keys().next().cloned(),
            _ => None,
        }
    }

    /// Returns the live connection for `agent_id`, starting it lazily when
    /// needed.
    ///
    /// # Errors
    ///
    /// Fails when the agent is not configured or the subprocess cannot
    /// start.
    pub async fn connection(&self, agent_id: &str) -> Result<AgentConnection, AgentError> {
        let mut options = self.options;
        options.ee_proxy_enabled = self.config.ee_proxy_enabled;
        self.connection_with_options(agent_id, options).await
    }

    /// Like [`Self::connection`] with explicit options (the config's
    /// `ee_proxy_enabled` still wins over the caller's options).
    async fn connection_with_options(
        &self,
        agent_id: &str,
        options: AgentConnectionOptions,
    ) -> Result<AgentConnection, AgentError> {
        let mut options = options;
        options.ee_proxy_enabled = self.config.ee_proxy_enabled;
        if let Some(connection) =
            self.connections.lock().expect("connections poisoned").get(agent_id)
        {
            return Ok(connection.clone());
        }
        let config = self
            .config
            .agents
            .get(agent_id)
            .cloned()
            .ok_or_else(|| AgentError::UnknownAgent(agent_id.to_string()))?;
        #[cfg(feature = "test-utils")]
        if let Some(factory) = self.config.fake_transports.get(agent_id) {
            let connection = AgentConnection::connect_with_transport_and_workspace_memory(
                agent_id.to_string(),
                self.handler.clone(),
                self.events.clone(),
                options,
                self.workspace_memory.clone(),
                factory.build(),
            )?;
            self.connections
                .lock()
                .expect("connections poisoned")
                .insert(agent_id.to_string(), connection.clone());
            return Ok(connection);
        }
        let connection = AgentConnection::connect_with_workspace_memory(
            agent_id.to_string(),
            config,
            self.handler.clone(),
            self.events.clone(),
            options,
            self.workspace_memory.clone(),
        )
        .await?;
        self.connections
            .lock()
            .expect("connections poisoned")
            .insert(agent_id.to_string(), connection.clone());
        Ok(connection)
    }

    /// Starts a new session on `agent_id` (lazily starting the connection).
    ///
    /// `worktree_roots` are absolute paths; the first root becomes the ACP
    /// `cwd` and the rest are forwarded as `additionalDirectories`.
    /// `mcp_servers` are forwarded as ACP `session/new` `mcpServers` (the
    /// agent connects to them directly; ee never starts them itself here).
    /// `ee_proxy_stdio_fallback` carries the stdio `ee --mcp-proxy` entry
    /// when proxy mode is configured; the host swaps it for an ACP-native
    /// [`ee_agent_protocol::McpServer::Acp`] entry when the agent supports
    /// MCP-over-ACP.
    ///
    /// # Errors
    ///
    /// Fails when the agent is unknown, the connection handshake fails, or
    /// the agent rejects `session/new`.
    pub async fn new_session(
        &self,
        agent_id: &str,
        worktree_roots: Vec<PathBuf>,
        mcp_servers: Vec<ee_agent_protocol::McpServer>,
        ee_proxy_stdio_fallback: Option<ee_agent_protocol::McpServerStdio>,
    ) -> Result<AgentThread, AgentError> {
        self.connection(agent_id)
            .await?
            .new_session(worktree_roots, mcp_servers, ee_proxy_stdio_fallback)
            .await
    }

    /// Loads an existing session; only when the agent advertises the
    /// `load_session` capability.
    ///
    /// # Errors
    ///
    /// Fails when the capability is missing, roots are invalid, or the agent
    /// rejects the load.
    pub async fn load_session(
        &self,
        agent_id: &str,
        session_id: ee_agent_protocol::SessionId,
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        mcp_servers: Vec<ee_agent_protocol::McpServer>,
    ) -> Result<AgentThread, AgentError> {
        self.connection(agent_id)
            .await?
            .load_session(session_id, cwd, additional_directories, mcp_servers)
            .await
    }

    /// Lists existing sessions on `agent_id`; only when the agent advertises
    /// `sessionCapabilities.list`.
    pub async fn list_sessions(
        &self,
        agent_id: &str,
        cwd: Option<PathBuf>,
        cursor: Option<String>,
    ) -> Result<ee_agent_protocol::ListSessionsResponse, AgentError> {
        self.connection(agent_id).await?.list_sessions(cwd, cursor).await
    }

    /// Deletes one existing session on `agent_id`; only when the agent
    /// advertises `sessionCapabilities.delete`.
    pub async fn delete_session(
        &self,
        agent_id: &str,
        session_id: ee_agent_protocol::SessionId,
    ) -> Result<ee_agent_protocol::DeleteSessionResponse, AgentError> {
        self.connection(agent_id).await?.delete_session(session_id).await
    }

    /// Resumes one existing session on `agent_id`; only when the agent
    /// advertises `sessionCapabilities.resume`.
    pub async fn resume_session(
        &self,
        agent_id: &str,
        session_id: ee_agent_protocol::SessionId,
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        mcp_servers: Vec<ee_agent_protocol::McpServer>,
    ) -> Result<AgentThread, AgentError> {
        self.connection(agent_id)
            .await?
            .resume_session(session_id, cwd, additional_directories, mcp_servers)
            .await
    }

    /// Closes one active session on `agent_id`; only when the agent
    /// advertises `sessionCapabilities.close`.
    pub async fn close_session(
        &self,
        agent_id: &str,
        session_id: ee_agent_protocol::SessionId,
    ) -> Result<ee_agent_protocol::CloseSessionResponse, AgentError> {
        self.connection(agent_id).await?.close_session(session_id).await
    }

    /// Closes the connection for `agent_id` (kills the subprocess, resolves
    /// pending work).  Idempotent.
    pub async fn close_agent(&self, agent_id: &str) {
        let connection = self.connections.lock().expect("connections poisoned").remove(agent_id);
        if let Some(connection) = connection {
            connection.close().await;
        }
    }

    /// Closes every connection.  Called on app shutdown.
    pub async fn shutdown(&self) {
        let connections = {
            let mut guard = self.connections.lock().expect("connections poisoned");
            std::mem::take(&mut *guard)
        };
        for (_, connection) in connections {
            connection.close().await;
        }
    }

    pub(crate) fn handler_for_isolated_connection(&self) -> Arc<dyn ClientRequestHandler> {
        self.handler.clone()
    }

    pub(crate) fn connection_options(&self) -> AgentConnectionOptions {
        self.options
    }

    /// Builds a lazy isolated manager for one configured agent using a distinct
    /// inbound handler and connection profile. Used by host-owned ephemeral
    /// critic work so root sessions and critic sessions never share process,
    /// connection, permission, MCP, or thread state.
    pub(crate) fn isolated_for_agent(
        &self,
        agent_id: &str,
        handler: Arc<dyn ClientRequestHandler>,
        options: AgentConnectionOptions,
    ) -> Result<Self, AgentError> {
        let process = self
            .config
            .agents
            .get(agent_id)
            .cloned()
            .ok_or_else(|| AgentError::UnknownAgent(agent_id.to_string()))?;
        let config = AgentManagerConfig {
            agents: BTreeMap::from([(agent_id.to_string(), process)]),
            ee_proxy_enabled: self.config.ee_proxy_enabled,
            workspace_memory: self.config.workspace_memory.clone(),
            #[cfg(feature = "test-utils")]
            fake_transports: self
                .config
                .fake_transports
                .get(agent_id)
                .map(|factory| BTreeMap::from([(agent_id.to_string(), factory.clone())]))
                .unwrap_or_default(),
        };
        // Ephemeral critic lifecycle must never appear as a root/editor thread.
        // Dropped receiver intentionally makes critic events private; broker emits
        // only verified, privacy-safe critic outcomes through its own channel.
        let (events, _private_events) = mpsc::unbounded_channel();
        Ok(Self::with_options_and_workspace_memory(
            config,
            handler,
            events,
            options,
            self.workspace_memory.clone(),
        ))
    }

    /// Number of live connections (tests and status lines).
    #[must_use]
    pub fn live_connection_count(&self) -> usize {
        self.connections.lock().expect("connections poisoned").len()
    }
}

/// Production-callable bounded-recall integration with an injectable retrieval seam.
///
/// Host adapters can use this when recall transport differs from [`AgentManager`].
/// Any retrieval or malformed fact fails closed to an empty workspace-memory section.
#[must_use]
pub fn build_context_pack_with_workspace_recaller<E>(
    mut builder: ContextPackBuilder,
    context: &WorkspaceRecallContext,
    mut recall: impl FnMut(&str, usize) -> Result<WorkspaceFactsResult, E>,
) -> ContextPack {
    if let Some(task) = &context.active_task {
        builder = builder.with_active_task(task);
    }
    builder = builder
        .with_focus_keys(&context.focus_keys)
        .with_workspace_memory_freshness_policy(context.freshness_policy);

    let mut facts = Vec::new();
    let mut failed = false;
    for query in context.deterministic_queries() {
        match recall(&query, ee_agent_orchestrator::DEFAULT_MAX_WORKSPACE_MEMORY_FACTS) {
            Ok(result) => {
                for fact in result.facts {
                    match workspace_context_fact(fact) {
                        Some(fact)
                            if !facts.iter().any(|existing: &WorkspaceContextFact| {
                                existing.key == fact.key && existing.source_id == fact.source_id
                            }) =>
                        {
                            facts.push(fact);
                        }
                        Some(_) => {}
                        None => {
                            failed = true;
                            break;
                        }
                    }
                }
            }
            Err(_) => failed = true,
        }
        if failed {
            break;
        }
    }

    if failed {
        builder.with_workspace_memory_result::<()>(Err(())).build()
    } else {
        builder.with_workspace_memory_result::<()>(Ok(facts)).build()
    }
}

fn workspace_context_fact(fact: WorkspaceFact) -> Option<WorkspaceContextFact> {
    if fact.namespace != "default"
        || fact.key.trim().is_empty()
        || fact.value.trim().is_empty()
        || fact.provenance.source_kind.trim().is_empty()
        || fact.provenance.source_id.trim().is_empty()
    {
        return None;
    }
    let authority = match fact.authority.as_str() {
        "user_asserted" => WorkspaceFactAuthority::UserAsserted,
        "host_verified" => WorkspaceFactAuthority::HostVerified,
        "agent_candidate" => WorkspaceFactAuthority::AgentCandidate,
        _ => return None,
    };
    let freshness = match fact.freshness.as_str() {
        "current" => WorkspaceFactFreshness::Current,
        "revision_bound" => WorkspaceFactFreshness::RevisionBound,
        "stale" => WorkspaceFactFreshness::Stale,
        _ => return None,
    };
    let state = match fact.state.as_str() {
        "candidate" => WorkspaceFactState::Candidate,
        "active" => WorkspaceFactState::Active,
        "stale" => WorkspaceFactState::Stale,
        "superseded" => WorkspaceFactState::Superseded,
        "retracted" => WorkspaceFactState::Retracted,
        _ => return None,
    };
    let selection_reason = match fact.selection_reason.as_deref() {
        Some("exact_key") => WorkspaceFactSelectionReason::ExactKey,
        Some("key_prefix") => WorkspaceFactSelectionReason::KeyPrefix,
        Some("full_text") => WorkspaceFactSelectionReason::FullText,
        Some("semantic") => WorkspaceFactSelectionReason::Semantic,
        _ => return None,
    };
    Some(WorkspaceContextFact::new(
        fact.key,
        fact.value,
        authority,
        freshness,
        state,
        fact.provenance.source_id,
        selection_reason,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbound::DenyAllHandler;
    use crate::process::AgentProcessConfig;
    use ee_agent_orchestrator::{
        ContextPackConfig, POTENTIALLY_STALE_WORKSPACE_MEMORY_WARNING,
        WorkspaceRecallFreshnessPolicy,
    };
    use ee_mcp::WorkspaceFactProvenance;

    fn recalled_fact(key: &str) -> WorkspaceFact {
        WorkspaceFact {
            id: 1,
            namespace: "default".to_string(),
            key: key.to_string(),
            value: "tree-sitter stays backend-owned".to_string(),
            kind: "architecture".to_string(),
            authority: "host_verified".to_string(),
            freshness: "current".to_string(),
            state: "active".to_string(),
            provenance: WorkspaceFactProvenance {
                source_kind: "validation".to_string(),
                source_id: format!("source:{key}"),
                revision: None,
                fingerprint: None,
                verified_at: None,
            },
            selection_reason: Some("exact_key".to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            expires_at: None,
            content_hash: "hash".to_string(),
            schema_version: 1,
        }
    }

    fn manager() -> AgentManager {
        let mut config = AgentManagerConfig::default();
        config.agents.insert("primary".into(), AgentProcessConfig::new("echo"));
        config.agents.insert("secondary".into(), AgentProcessConfig::new("cat"));
        let (events, _rx) = mpsc::unbounded_channel();
        AgentManager::new(config, Arc::new(DenyAllHandler), events)
    }

    #[test]
    fn bounded_recall_calls_every_query_and_projects_deduplicated_facts() {
        let context = WorkspaceRecallContext {
            current_request: "parser ownership".to_string(),
            active_files: vec!["src/parser.rs".to_string()],
            resolved_symbols: vec!["parse_file".to_string()],
            focus_keys: vec!["architecture:parser".to_string()],
            ..WorkspaceRecallContext::default()
        };
        let mut called = Vec::new();

        let pack = build_context_pack_with_workspace_recaller(
            ContextPackBuilder::new(ContextPackConfig::default()),
            &context,
            |query, limit| {
                called.push((query.to_string(), limit));
                Ok::<_, ()>(WorkspaceFactsResult {
                    facts: vec![recalled_fact("architecture:parser")],
                    total: 1,
                    omitted: 0,
                    truncated: false,
                })
            },
        );

        assert_eq!(
            called.iter().map(|(query, _)| query.as_str()).collect::<Vec<_>>(),
            vec![
                "parser ownership",
                "parser",
                "ownership",
                "src/parser.rs",
                "parse_file",
                "architecture:parser",
            ]
        );
        assert!(called.iter().all(|(_, limit)| {
            *limit == ee_agent_orchestrator::DEFAULT_MAX_WORKSPACE_MEMORY_FACTS
        }));
        assert_eq!(pack.workspace_memory.len(), 1);
        assert_eq!(pack.workspace_memory[0].key, "architecture:parser");
    }

    #[test]
    fn bounded_recall_failure_or_malformed_fact_fails_closed() {
        let context = WorkspaceRecallContext {
            current_request: "parser".to_string(),
            ..WorkspaceRecallContext::default()
        };
        let failed = build_context_pack_with_workspace_recaller(
            ContextPackBuilder::new(ContextPackConfig::default()),
            &context,
            |_, _| Err::<WorkspaceFactsResult, _>("unavailable"),
        );
        let malformed = build_context_pack_with_workspace_recaller(
            ContextPackBuilder::new(ContextPackConfig::default()),
            &context,
            |_, _| {
                let mut fact = recalled_fact("architecture:parser");
                fact.authority = "unknown".to_string();
                Ok::<_, ()>(WorkspaceFactsResult {
                    facts: vec![fact],
                    total: 1,
                    omitted: 0,
                    truncated: false,
                })
            },
        );

        assert!(failed.workspace_memory.is_empty());
        assert!(malformed.workspace_memory.is_empty());
    }

    #[test]
    fn bounded_recall_includes_stale_only_under_explicit_policy() {
        let context = WorkspaceRecallContext {
            current_request: "parser".to_string(),
            freshness_policy: WorkspaceRecallFreshnessPolicy::IncludePotentiallyStaleWithWarning,
            ..WorkspaceRecallContext::default()
        };
        let pack = build_context_pack_with_workspace_recaller(
            ContextPackBuilder::new(ContextPackConfig::default()),
            &context,
            |_, _| {
                let mut fact = recalled_fact("architecture:parser");
                fact.freshness = "revision_bound".to_string();
                fact.state = "stale".to_string();
                Ok::<_, ()>(WorkspaceFactsResult {
                    facts: vec![fact],
                    total: 1,
                    omitted: 0,
                    truncated: false,
                })
            },
        );

        assert_eq!(pack.workspace_memory.len(), 1);
        assert_eq!(
            pack.workspace_memory_warnings,
            vec![POTENTIALLY_STALE_WORKSPACE_MEMORY_WARNING.to_string()]
        );
    }

    #[tokio::test]
    async fn manager_starts_with_no_connections() {
        let manager = manager();
        assert_eq!(manager.live_connection_count(), 0);
        assert_eq!(manager.agent_ids(), vec!["primary".to_string(), "secondary".to_string()]);
    }

    #[tokio::test]
    async fn resolve_default_agent_uses_explicit_or_singleton() {
        let manager = manager();
        assert_eq!(manager.resolve_default_agent(Some("secondary")), Some("secondary".into()));
        assert_eq!(manager.resolve_default_agent(Some("missing")), None);
        assert_eq!(manager.resolve_default_agent(None), None); // ambiguous

        let mut single = AgentManagerConfig::default();
        single.agents.insert("only".into(), AgentProcessConfig::new("echo"));
        let (events, _rx) = mpsc::unbounded_channel();
        let single_manager = AgentManager::new(single, Arc::new(DenyAllHandler), events);
        assert_eq!(single_manager.resolve_default_agent(None), Some("only".into()));
    }

    #[tokio::test]
    async fn unknown_agent_fails_typed() {
        let manager = manager();
        let error = manager
            .new_session("nope", vec![PathBuf::from("/tmp")], Vec::new(), None)
            .await
            .unwrap_err();
        assert!(matches!(error, AgentError::UnknownAgent(_)));
    }
}
