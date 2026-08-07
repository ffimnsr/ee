//! ACP provider adapter: wraps [`OrchestratorRuntime`] behind the framework's
//! [`AgentProvider`](ee_acp_agent_server::AgentProvider) trait.
//!
//! Agent binaries can serve ACP either framework-only (provider implements
//! [`AgentProvider`] directly) or orchestration-backed: build an
//! [`OrchestratorProvider`] with a [`ModelAdapter`] and the framework owns
//! JSON-RPC dispatch while the orchestrator owns the model–tool loop.  The
//! adapter is provider-neutral — no OpenRouter or other backend code lives
//! here.
//!
//! Session lifecycle:
//! - `session/new` creates a fresh [`OrchestratorRuntime`] per session, so
//!   task graph, memory, and budget state are isolated per session.  MCP
//!   server entries are validated into redacted descriptors and retained per
//!   session (Phase 12).
//! - `session/load` restores a previously persisted (serialized) task graph
//!   and memory store when the adapter still holds them.
//! - `session/prompt` bridges the session's MCP servers for the turn
//!   (connect, `tools/list`, registration, dispatch, disconnect — see
//!   [`crate::mcp`]), then delegates to
//!   [`OrchestratorRuntime::run_turn`]; the framework's cancellation watch is
//!   passed through unchanged, so `session/cancel` and `session/close` stop
//!   the active turn.
//! - `session/close` serializes the session's task/memory state (for a later
//!   `session/load`) and drops the runtime.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ee_acp_agent_server::{
    AgentProvider, ClientBridge, LoadSessionContext, NewSessionContext, PromptContext,
    PromptResult, ProviderError, ProviderFuture, SessionInit, UpdateSink,
};
use ee_agent_protocol::{
    AgentCapabilities, COMPACT_COMMAND_NAME, ContentBlock, ContentChunk, DISCARD_COMMAND_NAME,
    Implementation, McpCapabilities, MessageId, SessionCapabilities, SessionCloseCapabilities,
    SessionId, SessionListCapabilities, SessionResumeCapabilities, SessionUpdate, StopReason,
    TextContent, compact_available_command, discard_available_command, is_resume_command,
    parse_slash_command, resume_available_command,
};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::checkpoint_store::CheckpointStore;
use crate::config::OrchestratorConfig;
use crate::mcp::{
    McpBackedTool, McpDiscoveryDiagnostic, McpServerDescriptor, McpSessionManager, McpToolPolicy,
};
use crate::memory::MemoryStore;
use crate::model::ModelAdapter;
use crate::policy::PolicyEngine;
use crate::recovery::TurnOutcome;
use crate::runtime::OrchestratorRuntime;
use crate::tasks::TaskGraph;

/// Default implementation name advertised in `initialize` responses.
pub const DEFAULT_IMPLEMENTATION_NAME: &str = "ee-agent-orchestrator";
/// Title used when the implementation metadata is left at its default.
pub const DEFAULT_IMPLEMENTATION_TITLE: &str = "Agent Orchestrator";
/// Prefix of provider-generated session ids (`session-1`, `session-2`, ...).
pub const SESSION_ID_PREFIX: &str = "session";

/// Adapter configuration: the orchestrator knobs plus the ACP implementation
/// metadata advertised in `initialize` responses.
#[derive(Debug, Clone)]
pub struct OrchestratorProviderConfig {
    /// Orchestrator loop, tool, subagent, budget, and timeout knobs.
    pub orchestrator: OrchestratorConfig,
    /// ACP implementation metadata returned by [`AgentProvider::info`].
    pub implementation: Implementation,
    /// MCP tool bridging knobs (Phase 12): per-request timeouts and
    /// side-effect classification overrides for session-advertised MCP
    /// servers.
    pub mcp: McpToolPolicy,
}

impl Default for OrchestratorProviderConfig {
    fn default() -> Self {
        Self {
            orchestrator: OrchestratorConfig::default(),
            implementation: Implementation::new(
                DEFAULT_IMPLEMENTATION_NAME,
                env!("CARGO_PKG_VERSION"),
            )
            .title(DEFAULT_IMPLEMENTATION_TITLE),
            mcp: McpToolPolicy::default(),
        }
    }
}

/// One live session's orchestrator runtime and immutable session facts.
struct SessionRuntime {
    runtime: Arc<OrchestratorRuntime>,
    system_context: String,
    /// Validated, secret-redacted MCP server descriptors from `session/new`.
    mcp_servers: Vec<McpServerDescriptor>,
    /// Memory-bounded conversation log (user prompts + agent text chunks as
    /// the client saw them), persisted on close and replayed by
    /// `session/load` (ACP v1 conversation replay).
    conversation: Arc<Mutex<Vec<ConversationMessage>>>,
}

/// One recorded conversation message for `session/load` replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ConversationMessage {
    role: ConversationRole,
    text: String,
}

/// Who produced a recorded conversation message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::enum_variant_names)]
enum ConversationRole {
    User,
    Agent,
}

/// Upper bound of recorded conversation messages; the oldest messages are
/// dropped first (the replay stays memory-bounded).
const CONVERSATION_MAX_MESSAGES: usize = 256;

/// Serialized orchestrator state kept when a session closes so a later
/// `session/load` can restore it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedSession {
    tasks: TaskGraph,
    memory: MemoryStore,
    /// Conversation log captured during the session (memory-bounded);
    /// replayed as `user_message_chunk` / `agent_message_chunk` updates by
    /// `session/load`.
    conversation: Vec<ConversationMessage>,
}

/// ACP provider adapter around [`OrchestratorRuntime`].
///
/// Generic over the injected [`ModelAdapter`].  Instances are cheap to clone
/// (all state is shared behind `Arc`), so providers can keep a probe handle
/// alongside the one handed to the framework server.
pub struct OrchestratorProvider<M> {
    config: OrchestratorProviderConfig,
    model: Arc<M>,
    policy: PolicyEngine,
    sessions: Arc<Mutex<HashMap<String, SessionRuntime>>>,
    persisted: Arc<Mutex<HashMap<String, PersistedSession>>>,
    next_session: Arc<AtomicU64>,
}

impl<M> Clone for OrchestratorProvider<M> {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            model: self.model.clone(),
            policy: self.policy.clone(),
            sessions: self.sessions.clone(),
            persisted: self.persisted.clone(),
            next_session: self.next_session.clone(),
        }
    }
}

impl<M: ModelAdapter> OrchestratorProvider<M> {
    /// Creates an adapter with the default fail-closed policy (reads only).
    #[must_use]
    pub fn new(config: OrchestratorProviderConfig, model: Arc<M>) -> Self {
        Self::with_policy(config, model, PolicyEngine::default())
    }

    /// Creates an adapter with a custom policy engine.
    #[must_use]
    pub fn with_policy(
        config: OrchestratorProviderConfig,
        model: Arc<M>,
        policy: PolicyEngine,
    ) -> Self {
        Self {
            config,
            model,
            policy,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            persisted: Arc::new(Mutex::new(HashMap::new())),
            next_session: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Snapshot of the live task/memory state for one session, when the
    /// session exists.
    #[cfg(test)]
    pub(crate) fn session_state(&self, session_id: &str) -> Option<(TaskGraph, MemoryStore)> {
        let sessions = self.sessions.lock().expect("adapter sessions poisoned");
        let runtime = &sessions.get(session_id)?.runtime;
        Some((runtime.tasks(), runtime.memory()))
    }

    /// Names of the tools currently registered for one session (tests).
    #[cfg(test)]
    pub(crate) fn session_tool_names(&self, session_id: &str) -> Vec<String> {
        let sessions = self.sessions.lock().expect("adapter sessions poisoned");
        sessions.get(session_id).map(|session| session.runtime.tool_names()).unwrap_or_default()
    }

    /// The redacted MCP server descriptors of one session (tests).
    #[cfg(test)]
    pub(crate) fn session_mcp_servers(&self, session_id: &str) -> Vec<McpServerDescriptor> {
        let sessions = self.sessions.lock().expect("adapter sessions poisoned");
        sessions.get(session_id).map(|session| session.mcp_servers.clone()).unwrap_or_default()
    }

    /// Whether a session's serialized state is still held for `session/load`.
    #[cfg(test)]
    pub(crate) fn has_persisted_state(&self, session_id: &str) -> bool {
        self.persisted.lock().expect("adapter persisted poisoned").contains_key(session_id)
    }
}

/// Validates `session/new` MCP server entries into redacted descriptors,
/// fail closed on unsupported transports.
fn validate_mcp_servers(
    servers: &[ee_agent_protocol::McpServer],
) -> Result<Vec<McpServerDescriptor>, ProviderError> {
    let mut descriptors = Vec::with_capacity(servers.len());
    for server in servers {
        let descriptor = McpServerDescriptor::from_wire(server.clone()).map_err(|reason| {
            ProviderError::InvalidRequest(format!("invalid mcpServers entry: {reason}"))
        })?;
        descriptors.push(descriptor);
    }
    Ok(descriptors)
}

/// Emits bounded, secret-free MCP discovery diagnostics through the update
/// sink: per-server failures plus the "no MCP tools registered" explanations
/// (no servers configured, connect/list failed, or policy filtered all
/// tools).  Sink failures are swallowed (best-effort diagnostics).
fn emit_mcp_diagnostics(
    sink: &UpdateSink,
    diagnostics: &[McpDiscoveryDiagnostic],
    policy: &PolicyEngine,
    manager: &McpSessionManager,
) {
    for (index, diagnostic) in diagnostics.iter().enumerate() {
        let _ =
            sink.agent_thought_chunk(format!("mcp-discovery-{}", index + 1), &diagnostic.message);
    }
    let definitions = manager.tool_definitions();
    if !definitions.is_empty() {
        return;
    }
    if !manager.has_servers() {
        let _ = sink.agent_thought_chunk(
            "mcp-diagnostics",
            "no MCP servers were configured for this session (session/new carried no mcpServers)",
        );
        return;
    }
    let reason = if crate::mcp::policy_filters_all(policy, &definitions) {
        "MCP tools were discovered but the active policy denies all of them; allow the relevant side-effect classes to use them"
    } else if diagnostics.is_empty() {
        "MCP servers were configured but no tools were registered"
    } else {
        "MCP tools could not be registered (see the discovery diagnostics above)"
    };
    let _ = sink.agent_thought_chunk("mcp-diagnostics", reason);
}

/// Per-prompt cleanup: deregisters MCP tools and shuts the MCP connections
/// down.  Runs explicitly after the turn and again from [`Drop`] on panic
/// paths (deregistration is synchronous; connection shutdown is spawned).
struct McpTurnCleanup {
    runtime: Arc<OrchestratorRuntime>,
    manager: Option<Arc<McpSessionManager>>,
    registered: Vec<String>,
}

impl McpTurnCleanup {
    async fn finish(mut self) {
        self.cleanup().await;
    }

    async fn cleanup(&mut self) {
        for name in self.registered.drain(..) {
            self.runtime.remove_tool(&name);
        }
        if let Some(manager) = self.manager.take() {
            manager.shutdown().await;
        }
    }
}

impl Drop for McpTurnCleanup {
    fn drop(&mut self) {
        for name in self.registered.drain(..) {
            self.runtime.remove_tool(&name);
        }
        if let Some(manager) = self.manager.take() {
            // Best-effort: a running prompt always has a runtime; without
            // one the manager's connections cancel on drop anyway.
            drop(tokio::spawn(async move { manager.shutdown().await }));
        }
    }
}

/// Appends one recorded user message to the conversation log (bounded).
fn record_user_message(
    conversation: &Arc<Mutex<Vec<ConversationMessage>>>,
    text: impl Into<String>,
) {
    let mut log = conversation.lock().expect("conversation poisoned");
    log.push(ConversationMessage { role: ConversationRole::User, text: text.into() });
    if log.len() > CONVERSATION_MAX_MESSAGES {
        let overflow = log.len() - CONVERSATION_MAX_MESSAGES;
        log.drain(..overflow);
    }
}

/// Streams a recorded conversation to the client as `user_message_chunk` /
/// `agent_message_chunk` updates, in order, with deterministic replay ids
/// (ACP v1 `session/load` conversation replay).  With no sink (or nothing
/// recorded) this is a no-op.
fn replay_conversation(
    sink: Option<&UpdateSink>,
    conversation: &[ConversationMessage],
) -> Result<(), ProviderError> {
    let Some(sink) = sink else { return Ok(()) };
    for (index, message) in conversation.iter().enumerate() {
        let block = ContentBlock::Text(TextContent::new(message.text.clone()));
        match message.role {
            ConversationRole::User => {
                let chunk = ContentChunk::new(block)
                    .message_id(MessageId::new(format!("replay-u-{}", index + 1)));
                sink.raw_update(SessionUpdate::UserMessageChunk(chunk)).map_err(|error| {
                    ProviderError::BackendFailure(format!("failed to replay user message: {error}"))
                })?;
            }
            ConversationRole::Agent => {
                sink.agent_message_chunk(format!("replay-a-{}", index + 1), message.text.clone())
                    .map_err(|error| {
                    ProviderError::BackendFailure(format!(
                        "failed to replay agent message: {error}"
                    ))
                })?;
            }
        }
    }
    Ok(())
}

/// Builds a runtime restored from a pending checkpoint, verifying the
/// provider identity.  The checkpoint stays in the store (the resumed turn
/// decides when to clear it).
fn restore_runtime_from_checkpoint<M: ModelAdapter>(
    checkpoint: &crate::checkpoint::OrchestratorCheckpoint,
    implementation_name: &str,
    model: Arc<M>,
    policy: PolicyEngine,
) -> Result<Arc<OrchestratorRuntime>, ProviderError> {
    if checkpoint.provider != implementation_name {
        return Err(ProviderError::BackendFailure(format!(
            "checkpoint provider {:?} does not match {:?}; refusing restore",
            checkpoint.provider, implementation_name
        )));
    }
    OrchestratorRuntime::from_checkpoint(checkpoint, model, policy)
        .map(Arc::new)
        .map_err(ProviderError::from)
}

/// Replays the pending turn's checkpoint transcript tail as conversation
/// chunks (crash-restore fallback): user and assistant text messages only.
fn replay_checkpoint_transcript(
    sink: Option<&UpdateSink>,
    checkpoint: &crate::checkpoint::OrchestratorCheckpoint,
) -> Result<(), ProviderError> {
    let Some(sink) = sink else { return Ok(()) };
    let Some(resume) = checkpoint.resume.as_ref() else { return Ok(()) };
    for (index, message) in resume.transcript.iter().enumerate() {
        let text = message.text_content();
        if text.is_empty() {
            continue;
        }
        match message.role {
            crate::model::ModelRole::User => {
                let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
                    .message_id(MessageId::new(format!("replay-u-{}", index + 1)));
                sink.raw_update(SessionUpdate::UserMessageChunk(chunk)).map_err(|error| {
                    ProviderError::BackendFailure(format!(
                        "failed to replay checkpoint user message: {error}"
                    ))
                })?;
            }
            crate::model::ModelRole::Assistant => {
                sink.agent_message_chunk(format!("replay-a-{}", index + 1), text).map_err(
                    |error| {
                        ProviderError::BackendFailure(format!(
                            "failed to replay checkpoint agent message: {error}"
                        ))
                    },
                )?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// The initial slash commands advertised for a session: `/compact` always;
/// `/discard` and `/resume` only when recovery is enabled.
fn initial_commands(recovery_enabled: bool) -> Vec<ee_agent_protocol::AvailableCommand> {
    let mut commands = vec![compact_available_command()];
    if recovery_enabled {
        commands.push(discard_available_command());
        commands.push(resume_available_command());
    }
    commands
}

/// Next session number: the in-process counter, raised past every session id
/// that survives in the durable checkpoint store (cross-restart collision
/// guard so a reconnected `session-1` is never shadowed by a fresh one).
fn next_session_number(next_session: &AtomicU64, recovery: &crate::config::RecoveryConfig) -> u64 {
    let base = CheckpointStore::new(recovery)
        .session_ids()
        .into_iter()
        .filter_map(|id| {
            id.strip_prefix(&format!("{SESSION_ID_PREFIX}-"))
                .and_then(|suffix| suffix.parse::<u64>().ok())
        })
        .max()
        .map_or(1, |max| max + 1);
    // Take the next in-process number, then make sure the counter itself
    // stays above the durable base so subsequent allocations never repeat it.
    let previous = next_session.fetch_add(1, Ordering::Relaxed);
    let number = previous.max(base);
    next_session.fetch_max(number + 1, Ordering::Relaxed);
    number
}

fn workspace_system_context(
    cwd: &std::path::Path,
    additional_directories: &[std::path::PathBuf],
) -> String {
    let mut roots = vec![cwd.to_path_buf()];
    for directory in additional_directories {
        if !roots.iter().any(|root| root == directory) {
            roots.push(directory.clone());
        }
    }
    let mut text = format!(
        "Session context:\n- current_working_directory: {}\n- workspace_roots:",
        cwd.display()
    );
    for root in roots {
        text.push_str(&format!("\n  - {}", root.display()));
    }
    text.push_str(
        "\nTool path rules:\n- Built-in file and terminal tools require absolute paths.\n- Resolve relative paths against current_working_directory before calling tools.",
    );
    text
}

impl<M: ModelAdapter> AgentProvider for OrchestratorProvider<M> {
    fn info(&self) -> Implementation {
        self.config.implementation.clone()
    }

    fn capabilities(&self) -> AgentCapabilities {
        // Load is supported (restores persisted state); session listing and
        // closing are handled by the framework.  Recovery-enabled providers
        // also advertise `session/resume` (checkpoint restore without
        // replay).  The provider hosts MCP-over-ACP for every session (the
        // `ClientBridge` mcp/* path), so `mcp_capabilities.acp` is
        // advertised; hosts then append the ee proxy as `McpServer::Acp`
        // instead of the stdio fallback.  Prompt/image capabilities stay at
        // their defaults.
        let mut session_capabilities = SessionCapabilities::new()
            .list(SessionListCapabilities::new())
            .close(SessionCloseCapabilities::new());
        if self.config.orchestrator.recovery.enabled {
            session_capabilities = session_capabilities.resume(SessionResumeCapabilities::new());
        }
        AgentCapabilities::default()
            .load_session(true)
            .mcp_capabilities(McpCapabilities::new().acp(true))
            .session_capabilities(session_capabilities)
    }

    fn new_session(
        &self,
        ctx: NewSessionContext,
    ) -> ProviderFuture<Result<SessionInit, ProviderError>> {
        let config = self.config.orchestrator.clone();
        let model = self.model.clone();
        let policy = self.policy.clone();
        let sessions = self.sessions.clone();
        let next_session = self.next_session.clone();
        let recovery_enabled = config.recovery.enabled;
        Box::pin(async move {
            // Monotonic id per process, raised past ids that survive in the
            // durable checkpoint store so a reconnected session is never
            // shadowed by a fresh one after a restart.
            let number = next_session_number(&next_session, &config.recovery);
            let session_id = SessionId::new(format!("{SESSION_ID_PREFIX}-{number}"));
            let system_context = workspace_system_context(&ctx.cwd, &ctx.additional_directories);
            let mcp_servers = validate_mcp_servers(&ctx.mcp_servers)?;
            let runtime = Arc::new(OrchestratorRuntime::with_policy(config, model, policy));
            runtime.register_builtins(&session_id).map_err(|error| {
                ProviderError::BackendFailure(format!("failed to register built-in tools: {error}"))
            })?;
            sessions.lock().expect("adapter sessions poisoned").insert(
                session_id.to_string(),
                SessionRuntime {
                    runtime,
                    system_context,
                    mcp_servers,
                    conversation: Arc::new(Mutex::new(Vec::new())),
                },
            );
            Ok(SessionInit::new(session_id).commands(initial_commands(recovery_enabled)))
        })
    }

    fn load_session(
        &self,
        ctx: LoadSessionContext,
    ) -> ProviderFuture<Result<SessionInit, ProviderError>> {
        let config = self.config.orchestrator.clone();
        let implementation = self.config.implementation.clone();
        let model = self.model.clone();
        let policy = self.policy.clone();
        let sessions = self.sessions.clone();
        let persisted = self.persisted.clone();
        let session_id = ctx.session_id.clone();
        let recovery_enabled = config.recovery.enabled;
        Box::pin(async move {
            let system_context = workspace_system_context(&ctx.cwd, &ctx.additional_directories);
            let mcp_servers = validate_mcp_servers(&ctx.mcp_servers)?;
            // A live session (same process, e.g. reconnect) is reused
            // as-is, including its recorded conversation.
            if let Some(entry) =
                sessions.lock().expect("adapter sessions poisoned").get(&session_id.to_string())
            {
                replay_conversation(
                    ctx.replay_sink.as_ref(),
                    &entry.conversation.lock().expect("conversation poisoned"),
                )?;
                return Ok(
                    SessionInit::new(session_id).commands(initial_commands(recovery_enabled))
                );
            }
            let state = persisted
                .lock()
                .expect("adapter persisted poisoned")
                .remove(&session_id.to_string());
            let (runtime, conversation) = match state {
                Some(state) => (
                    Arc::new(OrchestratorRuntime::with_state(
                        config,
                        model.clone(),
                        policy.clone(),
                        state.tasks,
                        state.memory,
                    )),
                    state.conversation,
                ),
                None => {
                    // Crash restore: no in-memory state survived, so rebuild
                    // from the durable checkpoint store when the provider
                    // identity matches.
                    let store = CheckpointStore::new(&config.recovery);
                    let Some((_id, checkpoint)) =
                        store.load_latest(&session_id.to_string()).map_err(|error| {
                            ProviderError::BackendFailure(format!(
                                "failed to read pending checkpoint: {error}"
                            ))
                        })?
                    else {
                        return Err(ProviderError::BackendFailure(format!(
                            "no persisted orchestrator state for session {session_id}"
                        )));
                    };
                    // The pending checkpoint stays until the resumed turn
                    // completes or the client discards it.
                    let runtime = restore_runtime_from_checkpoint(
                        &checkpoint,
                        &implementation.name,
                        model.clone(),
                        policy.clone(),
                    )?;
                    // Crash fallback replay: the checkpoint's transcript
                    // tail is the only conversation that survived.
                    replay_checkpoint_transcript(ctx.replay_sink.as_ref(), &checkpoint)?;
                    (runtime, Vec::new())
                }
            };
            runtime.register_builtins(&session_id).map_err(|error| {
                ProviderError::BackendFailure(format!("failed to register built-in tools: {error}"))
            })?;
            replay_conversation(ctx.replay_sink.as_ref(), &conversation)?;
            sessions.lock().expect("adapter sessions poisoned").insert(
                session_id.to_string(),
                SessionRuntime {
                    runtime,
                    system_context,
                    mcp_servers,
                    conversation: Arc::new(Mutex::new(conversation)),
                },
            );
            Ok(SessionInit::new(session_id).commands(initial_commands(recovery_enabled)))
        })
    }

    fn resume_session(
        &self,
        ctx: LoadSessionContext,
    ) -> ProviderFuture<Result<SessionInit, ProviderError>> {
        let config = self.config.orchestrator.clone();
        let implementation = self.config.implementation.clone();
        let model = self.model.clone();
        let policy = self.policy.clone();
        let sessions = self.sessions.clone();
        let session_id = ctx.session_id.clone();
        let recovery_enabled = config.recovery.enabled;
        Box::pin(async move {
            // `session/resume` restores context with NO replay (ACP v1): the
            // interrupted turn is continued by the next `session/prompt` via
            // the pending-checkpoint detection.  A live session (same
            // process) is reused as-is.
            let mcp_servers = validate_mcp_servers(&ctx.mcp_servers)?;
            let mut live = false;
            {
                let mut sessions = sessions.lock().expect("adapter sessions poisoned");
                if let Some(entry) = sessions.get_mut(&session_id.to_string()) {
                    live = true;
                    entry.mcp_servers = mcp_servers.clone();
                }
            }
            if !live {
                let store = CheckpointStore::new(&config.recovery);
                let Some((_id, checkpoint)) =
                    store.load_latest(&session_id.to_string()).map_err(|error| {
                        ProviderError::BackendFailure(format!(
                            "failed to read pending checkpoint: {error}"
                        ))
                    })?
                else {
                    return Err(ProviderError::BackendFailure(format!(
                        "no pending checkpoint for session {session_id}; nothing to resume"
                    )));
                };
                let runtime = restore_runtime_from_checkpoint(
                    &checkpoint,
                    &implementation.name,
                    model.clone(),
                    policy.clone(),
                )?;
                runtime.register_builtins(&session_id).map_err(|error| {
                    ProviderError::BackendFailure(format!(
                        "failed to register built-in tools: {error}"
                    ))
                })?;
                let system_context =
                    workspace_system_context(&ctx.cwd, &ctx.additional_directories);
                // The checkpoint stays pending: the next prompt resumes it.
                sessions.lock().expect("adapter sessions poisoned").insert(
                    session_id.to_string(),
                    SessionRuntime {
                        runtime,
                        system_context,
                        mcp_servers,
                        conversation: Arc::new(Mutex::new(Vec::new())),
                    },
                );
            }
            Ok(SessionInit::new(session_id).commands(initial_commands(recovery_enabled)))
        })
    }

    fn prompt(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
    ) -> ProviderFuture<Result<PromptResult, ProviderError>> {
        let session_id = ctx.session_id.clone();
        let sessions = self.sessions.clone();
        let mcp_policy = self.config.mcp.clone();
        let policy = self.policy.clone();
        let implementation_name = self.config.implementation.name.clone();
        Box::pin(async move {
            let session = {
                let sessions = sessions.lock().expect("adapter sessions poisoned");
                sessions.get(&session_id.to_string()).map(|session| {
                    (
                        session.runtime.clone(),
                        session.system_context.clone(),
                        session.mcp_servers.clone(),
                        session.conversation.clone(),
                    )
                })
            };
            let Some((runtime, system_context, mcp_servers, conversation)) = session else {
                return Err(ProviderError::BackendFailure(format!(
                    "no orchestrator state for session {session_id}"
                )));
            };
            // `/compact` is detected here, before any MCP bridging or tool
            // registration, so compaction turns never connect servers or
            // expose tools; the runtime still routes the turn to its
            // dedicated compaction path (no model–tool loop).
            let prompt_text = ctx
                .prompt
                .iter()
                .filter_map(|block| match block {
                    ee_agent_protocol::ContentBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ");
            let is_compact = parse_slash_command(&prompt_text)
                .is_some_and(|command| command.name == COMPACT_COMMAND_NAME);
            if is_compact {
                return runtime
                    .run_turn_with_system_context(ctx, sink, client, cancel, system_context)
                    .await
                    .map_err(ProviderError::from);
            }
            // `/discard` rejects a paused turn's pending checkpoint: the
            // interrupted work is dropped instead of resumed.  Only valid
            // when recovery is enabled; otherwise it is an ordinary prompt.
            let recovery = runtime.config().recovery.clone();
            let store = runtime.checkpoint_store();
            if recovery.enabled
                && parse_slash_command(&prompt_text)
                    .is_some_and(|command| command.name == DISCARD_COMMAND_NAME)
            {
                store.delete_session(&session_id.to_string());
                let _ = sink
                    .agent_message_chunk("discard", "paused turn discarded; checkpoint cleared");
                return Ok(PromptResult::new(StopReason::EndTurn));
            }
            let has_pending = store.has_pending(&session_id.to_string());
            // `/resume` continues a paused turn without the original prompt
            // (client-crash continuation).  Without a pending checkpoint it
            // is an ordinary prompt whose text reaches the model.
            let resume_command = recovery.enabled && is_resume_command(&prompt_text);
            if !has_pending {
                record_user_message(&conversation, prompt_text);
            }
            // Record the client-visible conversation for `session/load`
            // replay: user prompts here, agent text chunks through a sink
            // observer.
            let recording_conversation = conversation.clone();
            let sink = sink.with_observer(Arc::new(move |update| {
                if let SessionUpdate::AgentMessageChunk(chunk) = update {
                    let text = match &chunk.content {
                        ee_agent_protocol::ContentBlock::Text(text) => text.text.clone(),
                        _ => String::new(),
                    };
                    if !text.is_empty() {
                        let mut log = recording_conversation.lock().expect("conversation poisoned");
                        log.push(ConversationMessage { role: ConversationRole::Agent, text });
                        if log.len() > CONVERSATION_MAX_MESSAGES {
                            let overflow = log.len() - CONVERSATION_MAX_MESSAGES;
                            log.drain(..overflow);
                        }
                    }
                }
            }));
            // Phase 12: bridge the session's MCP servers into the tool
            // registry for this prompt.  The manager is per prompt (the
            // `ClientBridge` is per prompt), while the validated descriptors
            // stay per session.
            let mut manager =
                McpSessionManager::new(mcp_servers, client.clone(), cancel.clone(), mcp_policy);
            let mut diagnostics = manager.discover_all().await;
            let manager = Arc::new(manager);
            let mut registered = Vec::new();
            let definitions = manager.tool_definitions();
            for definition in definitions {
                match runtime.register_tool(Arc::new(McpBackedTool::new(
                    definition.clone(),
                    manager.clone(),
                ))) {
                    Ok(()) => registered.push(definition.name),
                    Err(error) => diagnostics.push(McpDiscoveryDiagnostic {
                        server_id: "mcp".to_string(),
                        message: format!(
                            "failed to register MCP tool {:?}: {error}",
                            definition.name
                        ),
                    }),
                }
            }
            emit_mcp_diagnostics(&sink, &diagnostics, &policy, &manager);
            let cleanup =
                McpTurnCleanup { runtime: runtime.clone(), manager: Some(manager), registered };
            // The framework's cancellation watch flips on `session/cancel`
            // and `session/close`; run_turn observes it and stops promptly.
            let provider_name = implementation_name;
            // Auto-resume needs the prompt inputs after the first run consumed
            // them; clone once up front.
            let resume_ctx = ctx.clone();
            let resume_sink = sink.clone();
            let resume_client = client.clone();
            let resume_cancel = cancel.clone();
            let resume_system = system_context.clone();
            let result = if !recovery.enabled {
                runtime
                    .run_turn_with_system_context(ctx, sink, client, cancel, system_context)
                    .await
                    .map(TurnOutcome::Completed)
            } else if has_pending {
                // A paused turn awaits: the same prompt resumes it from its
                // checkpoint (manual resume), continuing completed work.
                // `/resume` resumes without appending the command text as a
                // user message.
                let resume_ctx = if resume_command {
                    PromptContext::new(ctx.session_id.clone(), Vec::new())
                } else {
                    ctx
                };
                runtime
                    .resume_turn(resume_ctx, sink, client, cancel, system_context, &provider_name)
                    .await
            } else {
                runtime
                    .run_turn_recoverable(ctx, sink, client, cancel, system_context, &provider_name)
                    .await
            };
            let result = match result {
                Ok(TurnOutcome::Completed(result)) => Ok(result),
                Ok(TurnOutcome::Interrupted(interruption)) => {
                    // Safe single auto-resume: transient/deadline faults with
                    // a durable checkpoint and no ambiguous in-flight tool
                    // resume once automatically, capped by the config.
                    let auto_resume = recovery.auto_resume_max > 0
                        && interruption.safe_resume
                        && interruption.resumed_count < recovery.auto_resume_max;
                    if auto_resume {
                        match runtime
                            .resume_turn(
                                resume_ctx,
                                resume_sink,
                                resume_client,
                                resume_cancel,
                                resume_system,
                                &provider_name,
                            )
                            .await
                        {
                            Ok(TurnOutcome::Completed(result)) => Ok(result),
                            Ok(TurnOutcome::Interrupted(again)) => {
                                Err(ProviderError::Recoverable(again.into_wire()))
                            }
                            Err(error) => Err(ProviderError::from(error)),
                        }
                    } else {
                        Err(ProviderError::Recoverable(interruption.into_wire()))
                    }
                }
                Err(error) => Err(ProviderError::from(error)),
            };
            cleanup.finish().await;
            result
        })
    }

    fn cancel_session(&self, _session_id: SessionId) -> ProviderFuture<Result<(), ProviderError>> {
        // The framework flips the prompt's cancellation watch before calling
        // this hook, so the in-flight `run_turn` already observes the cancel.
        // The adapter keeps no other active-turn state to release.
        Box::pin(async { Ok(()) })
    }

    fn close_session(&self, session_id: SessionId) -> ProviderFuture<Result<(), ProviderError>> {
        let sessions = self.sessions.clone();
        let persisted = self.persisted.clone();
        Box::pin(async move {
            // The framework awaits the active prompt's cleanup (bounded)
            // before invoking this hook, so serializing the stores is safe.
            let Some(runtime) =
                sessions.lock().expect("adapter sessions poisoned").remove(&session_id.to_string())
            else {
                // Idempotent: the session was never created here.
                return Ok(());
            };
            // Explicit close finalizes the session: pending recovery
            // checkpoints are deleted (the interrupted work is abandoned
            // unless the host loads the persisted state below).
            runtime.runtime.checkpoint_store().delete_session(&session_id.to_string());
            let conversation = runtime.conversation.lock().expect("conversation poisoned").clone();
            persisted.lock().expect("adapter persisted poisoned").insert(
                session_id.to_string(),
                PersistedSession {
                    tasks: runtime.runtime.tasks(),
                    memory: runtime.runtime.memory(),
                    conversation,
                },
            );
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use ee_acp_agent_server::{
        AcpAgentServer, AcpAgentServerConfig, AcpServerError, MemoryTransport,
        MemoryTransportHandle,
    };
    use ee_agent_protocol::{
        Error as RpcError, RawJsonRpcMessage, RawJsonRpcParams, RequestId, Response,
    };
    use serde_json::{Value, json};
    use tokio::sync::watch;

    use super::*;
    use crate::config::RecoveryConfig;
    use crate::model::{ModelAdapter, ModelError, ModelFuture, ModelRequest, ModelResponse};
    use crate::test_support::FakeModel;

    /// Model that parks in real time before answering, with per-call delays
    /// so a deterministic first-call hang triggers the outer turn timeout
    /// while resumed calls complete well inside the slice.
    #[derive(Clone)]
    struct DelayedModel {
        delays: Arc<Mutex<VecDeque<std::time::Duration>>>,
        default_delay: std::time::Duration,
        inner: FakeModel,
    }

    impl DelayedModel {
        fn new(
            hang_first: std::time::Duration,
            default_delay: std::time::Duration,
            inner: FakeModel,
        ) -> Self {
            let mut delays = VecDeque::new();
            delays.push_back(hang_first);
            Self { delays: Arc::new(Mutex::new(delays)), default_delay, inner }
        }
    }

    impl ModelAdapter for DelayedModel {
        fn complete(
            &self,
            request: ModelRequest,
            cancel: watch::Receiver<bool>,
        ) -> ModelFuture<Result<ModelResponse, ModelError>> {
            let delay = self
                .delays
                .lock()
                .expect("delays poisoned")
                .pop_front()
                .unwrap_or(self.default_delay);
            let inner = self.inner.clone();
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                inner.complete(request, cancel).await
            })
        }
    }

    fn recovery_provider(
        model: Arc<DelayedModel>,
        auto_resume_max: u32,
    ) -> OrchestratorProvider<DelayedModel> {
        let mut recovery = RecoveryConfig::durable(None);
        recovery.auto_resume_max = auto_resume_max;
        OrchestratorProvider::with_policy(
            OrchestratorProviderConfig {
                orchestrator: OrchestratorConfig {
                    turn_timeout: std::time::Duration::from_millis(500),
                    recovery,
                    // Scripted text-only responses make no task-graph
                    // progress; disable the no-progress rule for the test.
                    stuck: crate::stuck::StuckConfig {
                        max_no_progress_iterations: 100,
                        ..crate::stuck::StuckConfig::default()
                    },
                    ..OrchestratorConfig::default()
                },
                ..OrchestratorProviderConfig::default()
            },
            model,
            PolicyEngine::default(),
        )
    }

    /// Script with enough 1 ms answers to complete a resumed slice inside
    /// the 500 ms turn timeout (the first call hangs past the slice).
    fn resume_script() -> FakeModel {
        let mut responses = Vec::new();
        for index in 0..12 {
            let response = if index == 11 {
                ModelResponse::new().text("done").completed()
            } else {
                ModelResponse::new().text(format!("step {index}"))
            };
            responses.push(response);
        }
        FakeModel::new(responses)
    }

    #[tokio::test]
    async fn provider_adapter_surfaces_recoverable_interruption_and_manual_resume() {
        let model = Arc::new(DelayedModel::new(
            std::time::Duration::from_millis(5_000),
            std::time::Duration::from_millis(1),
            resume_script(),
        ));
        let provider = recovery_provider(model, 0);
        let (handle, task) = spawn_server(provider);
        let session_id = new_session(&handle, 1).await;

        // Prompt 1: the first model call hangs past the slice; the provider
        // answers with a JSON-RPC error carrying the recoverable payload.
        handle.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
        let frame = next_response_frame(&handle).await;
        let Response::Error { error, .. } = unwrap_response(frame.clone()) else {
            panic!("expected an error response, got {frame:?}");
        };
        let recoverable =
            &error.data.as_ref().expect("recoverable error carries data")["recoverable"];
        assert_eq!(recoverable["fault"], "deadline");
        assert_eq!(recoverable["safe_resume"], true);
        assert_eq!(recoverable["resumed_count"], 0);
        assert!(
            recoverable["checkpoint_id"].as_str().is_some(),
            "checkpoint id on the wire: {recoverable}"
        );

        // Prompt 2 with the same prompt resumes from the checkpoint and
        // completes; the pending checkpoint is cleared.
        handle.send(request(3, "session/prompt", prompt_params(&session_id, "hello")));
        let result = request_result(next_response_frame(&handle).await);
        assert_eq!(result["stopReason"], "end_turn", "resumed turn completes: {result}");

        handle.shutdown(task).await;
    }

    #[tokio::test]
    async fn provider_adapter_auto_resumes_once_and_completes() {
        let model = Arc::new(DelayedModel::new(
            std::time::Duration::from_millis(5_000),
            std::time::Duration::from_millis(1),
            resume_script(),
        ));
        let provider = recovery_provider(model, 1);
        let (handle, task) = spawn_server(provider);
        let session_id = new_session(&handle, 1).await;

        // One prompt: the first slice times out, the safe single auto-resume
        // continues from the checkpoint and completes.
        handle.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
        let result = request_result(next_response_frame(&handle).await);
        assert_eq!(result["stopReason"], "end_turn", "auto-resume completes: {result}");

        handle.shutdown(task).await;
    }

    #[tokio::test]
    async fn provider_adapter_discard_command_clears_checkpoint() {
        let model = Arc::new(DelayedModel::new(
            std::time::Duration::from_millis(5_000),
            std::time::Duration::from_millis(1),
            resume_script(),
        ));
        let provider = recovery_provider(model, 0);
        let (handle, task) = spawn_server(provider);
        let session_id = new_session(&handle, 1).await;

        // Prompt 1 times out with a recoverable error (not asserted here;
        // the discard flow below is the point).
        handle.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
        let _ = next_response_frame(&handle).await;

        // `/discard` drops the pending checkpoint and answers end_turn.
        handle.send(request(3, "session/prompt", prompt_params(&session_id, "/discard")));
        let result = request_result(next_response_frame(&handle).await);
        assert_eq!(result["stopReason"], "end_turn", "discard answers end_turn: {result}");

        // A later prompt is a fresh turn, not a resume: the full script
        // replays inside the fresh slice.
        handle.send(request(4, "session/prompt", prompt_params(&session_id, "again")));
        let result = request_result(next_response_frame(&handle).await);
        assert_eq!(result["stopReason"], "end_turn", "fresh prompt after discard: {result}");

        handle.shutdown(task).await;
    }

    #[tokio::test]
    async fn provider_adapter_resume_command_continues_paused_turn_without_prompt_text() {
        let model = Arc::new(DelayedModel::new(
            std::time::Duration::from_millis(5_000),
            std::time::Duration::from_millis(1),
            resume_script(),
        ));
        let provider = recovery_provider(model.clone(), 0);
        let (handle, task) = spawn_server(provider);
        let session_id = new_session(&handle, 1).await;

        // Prompt 1 times out and leaves a pending checkpoint.
        handle.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
        let frame = next_response_frame(&handle).await;
        let Response::Error { .. } = unwrap_response(frame) else {
            panic!("expected a recoverable error");
        };

        // `/resume` continues the paused turn without appending the command
        // text to the model transcript.
        handle.send(request(3, "session/prompt", prompt_params(&session_id, "/resume")));
        let result = request_result(next_response_frame(&handle).await);
        assert_eq!(result["stopReason"], "end_turn", "resumed turn completes: {result}");
        for request in model.inner.requests() {
            for message in &request.transcript {
                assert!(
                    !message.text_content().contains("/resume"),
                    "no model message may carry the /resume command text"
                );
            }
        }
        assert!(
            model.inner.requests().iter().any(|request| {
                request.transcript.iter().any(|message| message.text_content().contains("hello"))
            }),
            "the original prompt is present in the resumed transcript"
        );

        handle.shutdown(task).await;
    }

    #[tokio::test]
    async fn provider_adapter_resume_command_without_pending_is_ordinary_prompt() {
        let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("ok").completed()]));
        let provider = recovery_provider(
            Arc::new(DelayedModel::new(
                std::time::Duration::ZERO,
                std::time::Duration::ZERO,
                (*model).clone(),
            )),
            0,
        );
        let (handle, task) = spawn_server(provider);
        let session_id = new_session(&handle, 1).await;

        handle.send(request(2, "session/prompt", prompt_params(&session_id, "/resume")));
        let result = request_result(next_response_frame(&handle).await);
        assert_eq!(result["stopReason"], "end_turn", "ordinary turn runs: {result}");
        let requests = model.requests();
        assert!(
            requests.iter().any(|request| {
                request.transcript.iter().any(|message| message.text_content() == "/resume")
            }),
            "without a pending checkpoint /resume reaches the model as text"
        );

        handle.shutdown(task).await;
    }

    /// Provider with durable recovery in `dir` (crash-restore capable).
    fn durable_recovery_provider(
        model: Arc<DelayedModel>,
        dir: &std::path::Path,
    ) -> OrchestratorProvider<DelayedModel> {
        let mut recovery = RecoveryConfig::durable(Some(dir.to_path_buf()));
        recovery.auto_resume_max = 0;
        OrchestratorProvider::with_policy(
            OrchestratorProviderConfig {
                orchestrator: OrchestratorConfig {
                    turn_timeout: std::time::Duration::from_millis(500),
                    recovery,
                    stuck: crate::stuck::StuckConfig {
                        max_no_progress_iterations: 100,
                        ..crate::stuck::StuckConfig::default()
                    },
                    ..OrchestratorConfig::default()
                },
                ..OrchestratorProviderConfig::default()
            },
            model,
            PolicyEngine::default(),
        )
    }

    #[tokio::test]
    async fn provider_adapter_session_resume_restores_pending_turn_without_replay() {
        let dir = tempfile::TempDir::new().expect("checkpoint dir");
        let model = Arc::new(DelayedModel::new(
            std::time::Duration::from_millis(5_000),
            std::time::Duration::from_millis(1),
            resume_script(),
        ));
        // Provider A pauses the turn and persists a durable checkpoint.
        let provider_a = durable_recovery_provider(model.clone(), dir.path());
        let (handle_a, task_a) = spawn_server(provider_a);
        let session_id = new_session(&handle_a, 1).await;
        handle_a.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
        let frame = next_response_frame(&handle_a).await;
        let Response::Error { .. } = unwrap_response(frame) else {
            panic!("expected a recoverable error");
        };
        handle_a.shutdown(task_a).await;

        // Provider B (fresh process) restores via session/resume: no replay
        // updates precede the `{}` response.
        let provider_b = durable_recovery_provider(model, dir.path());
        let (handle_b, task_b) = spawn_server(provider_b);
        handle_b.send(request(
            1,
            "session/resume",
            json!({ "sessionId": session_id, "cwd": "/work", "mcpServers": [] }),
        ));
        let result = request_result(handle_b.next_frame().await);
        assert_eq!(result, json!({}), "resume answers the empty result object: {result}");
        assert!(handle_b.outbound().is_empty(), "no replay updates before the resume response");

        // The next prompt continues the paused turn from the checkpoint.
        handle_b.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
        let result = request_result(next_response_frame(&handle_b).await);
        assert_eq!(result["stopReason"], "end_turn", "paused turn resumes after session/resume");

        handle_b.shutdown(task_b).await;
    }

    #[tokio::test]
    async fn provider_adapter_session_resume_without_pending_state_is_rejected() {
        let dir = tempfile::TempDir::new().expect("checkpoint dir");
        let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("done").completed()]));
        // Provider A completes the turn: the checkpoint is cleared.
        let provider_a = durable_recovery_provider(
            Arc::new(DelayedModel::new(
                std::time::Duration::ZERO,
                std::time::Duration::ZERO,
                (*model).clone(),
            )),
            dir.path(),
        );
        let (handle_a, task_a) = spawn_server(provider_a);
        let session_id = new_session(&handle_a, 1).await;
        handle_a.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
        let result = request_result(next_response_frame(&handle_a).await);
        assert_eq!(result["stopReason"], "end_turn");
        handle_a.shutdown(task_a).await;

        // Provider B has no in-memory state and no pending checkpoint.
        let provider_b = durable_recovery_provider(
            Arc::new(DelayedModel::new(
                std::time::Duration::ZERO,
                std::time::Duration::ZERO,
                (*model).clone(),
            )),
            dir.path(),
        );
        let (handle_b, task_b) = spawn_server(provider_b);
        handle_b.send(request(
            1,
            "session/resume",
            json!({ "sessionId": session_id, "cwd": "/work", "mcpServers": [] }),
        ));
        let error = request_error(handle_b.next_frame().await);
        assert!(
            error.message.contains("no pending checkpoint"),
            "resume without pending state is rejected: {error:?}"
        );

        handle_b.shutdown(task_b).await;
    }

    #[tokio::test]
    async fn provider_adapter_load_after_crash_replays_checkpoint_transcript() {
        let dir = tempfile::TempDir::new().expect("checkpoint dir");
        let model = Arc::new(DelayedModel::new(
            std::time::Duration::from_millis(5_000),
            std::time::Duration::from_millis(1),
            resume_script(),
        ));
        // Provider A pauses the turn; the checkpoint holds the transcript
        // tail (the user message).
        let provider_a = durable_recovery_provider(model.clone(), dir.path());
        let (handle_a, task_a) = spawn_server(provider_a);
        let session_id = new_session(&handle_a, 1).await;
        handle_a.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
        let frame = next_response_frame(&handle_a).await;
        let Response::Error { .. } = unwrap_response(frame) else {
            panic!("expected a recoverable error");
        };
        handle_a.shutdown(task_a).await;

        // Provider B loads from the checkpoint store: the pending turn's
        // transcript tail is replayed as the crash-restore conversation.
        let provider_b = durable_recovery_provider(model, dir.path());
        let (handle_b, task_b) = spawn_server(provider_b);
        handle_b.send(request(
            1,
            "session/load",
            json!({ "sessionId": session_id, "cwd": "/work", "mcpServers": [] }),
        ));
        let mut replayed_user_texts = Vec::new();
        let mut commands_seen = false;
        let mut response = None;
        for _ in 0..10 {
            let frame = handle_b.next_frame_real().await;
            match &frame {
                RawJsonRpcMessage::Notification(update) => {
                    let params = raw_params_to_value(update.params.clone());
                    match params["update"]["sessionUpdate"].as_str() {
                        Some("user_message_chunk") => {
                            replayed_user_texts.push(
                                params["update"]["content"]["text"]
                                    .as_str()
                                    .unwrap_or_default()
                                    .to_string(),
                            );
                        }
                        Some("available_commands_update") => commands_seen = true,
                        _ => {}
                    }
                }
                RawJsonRpcMessage::Response(_) => {
                    response = Some(frame);
                    break;
                }
                _ => {}
            }
        }
        assert!(
            replayed_user_texts.iter().any(|text| text == "hello"),
            "crash replay carries the pending user message: {replayed_user_texts:?}"
        );
        assert!(commands_seen, "loaded providers re-advertise their commands");
        let result = request_result(response.expect("load response arrives after replay"));
        assert_eq!(result, json!({}), "load responds null after replay");

        // The loaded session resumes the paused turn on the next prompt.
        handle_b.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
        let result = request_result(next_response_frame(&handle_b).await);
        assert_eq!(result["stopReason"], "end_turn", "paused turn resumes after crash load");

        handle_b.shutdown(task_b).await;
    }

    #[tokio::test]
    async fn provider_adapter_initialize_advertises_resume_capability_only_with_recovery() {
        let plain = Arc::new(FakeModel::new(Vec::new()));
        let provider =
            OrchestratorProvider::new(OrchestratorProviderConfig::default(), plain.clone());
        let (handle, task) = spawn_server(provider);
        handle.send(request(1, "initialize", json!({ "protocolVersion": 1 })));
        let result = request_result(handle.next_frame().await);
        assert_eq!(
            result["agentCapabilities"]["sessionCapabilities"]["resume"],
            Value::Null,
            "no resume advertisement without recovery"
        );
        handle.shutdown(task).await;

        let recovered = Arc::new(FakeModel::new(Vec::new()));
        let provider = recovery_provider(
            Arc::new(DelayedModel::new(
                std::time::Duration::ZERO,
                std::time::Duration::ZERO,
                (*recovered).clone(),
            )),
            0,
        );
        let (handle, task) = spawn_server(provider);
        handle.send(request(1, "initialize", json!({ "protocolVersion": 1 })));
        let result = request_result(handle.next_frame().await);
        assert_eq!(
            result["agentCapabilities"]["sessionCapabilities"]["resume"],
            json!({}),
            "recovery advertises session/resume"
        );
        handle.shutdown(task).await;
    }

    #[tokio::test]
    async fn provider_adapter_new_session_avoids_durable_checkpoint_collision() {
        let dir = tempfile::TempDir::new().expect("checkpoint dir");
        let model = Arc::new(DelayedModel::new(
            std::time::Duration::from_millis(5_000),
            std::time::Duration::from_millis(1),
            resume_script(),
        ));
        // Provider A creates `session-1` and pauses it (durable checkpoint).
        let provider_a = durable_recovery_provider(model.clone(), dir.path());
        let (handle_a, task_a) = spawn_server(provider_a);
        let session_id = new_session(&handle_a, 1).await;
        assert_eq!(session_id, "session-1");
        handle_a.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
        let frame = next_response_frame(&handle_a).await;
        let Response::Error { .. } = unwrap_response(frame) else {
            panic!("expected a recoverable error");
        };
        handle_a.shutdown(task_a).await;

        // Provider B (fresh process) must not shadow the reconnected
        // `session-1` when creating a new session.
        let provider_b = durable_recovery_provider(model, dir.path());
        let (handle_b, task_b) = spawn_server(provider_b);
        handle_b.send(request(1, "session/new", session_new_params("/work")));
        let result = request_result(handle_b.next_frame().await);
        assert_eq!(result["sessionId"], "session-2", "fresh id skips durable ids: {result}");
        // Subsequent allocations stay monotonic past the durable base (drain
        // the first session's command advertisement first).
        let frame = handle_b.next_frame().await;
        let RawJsonRpcMessage::Notification(update) = &frame else {
            panic!("expected the available_commands_update, got {frame:?}");
        };
        assert_eq!(
            raw_params_to_value(update.params.clone())["update"]["sessionUpdate"],
            "available_commands_update"
        );
        handle_b.send(request(2, "session/new", session_new_params("/work")));
        let result = request_result(handle_b.next_frame().await);
        assert_eq!(result["sessionId"], "session-3", "ids stay monotonic: {result}");

        handle_b.shutdown(task_b).await;
    }

    /// Waits (real time) for the next response frame, skipping update
    /// notifications.  Parks on a timer so the server task always gets
    /// scheduled.
    async fn next_response_frame(handle: &Harness) -> RawJsonRpcMessage {
        loop {
            let frame = handle.next_frame_real().await;
            if let RawJsonRpcMessage::Response(_) = &frame {
                return frame;
            }
        }
    }

    /// Minimal harness mirroring `ee-acp-agent-server`'s test utilities: feed
    /// inbound frames, read outbound frames in order.
    struct Harness {
        handle: MemoryTransportHandle,
        pending: Arc<Mutex<VecDeque<RawJsonRpcMessage>>>,
    }

    impl Harness {
        fn new(handle: MemoryTransportHandle) -> Self {
            Self { handle, pending: Arc::new(Mutex::new(VecDeque::new())) }
        }

        fn send(&self, frame: RawJsonRpcMessage) -> bool {
            self.handle.send(frame)
        }

        async fn next_frame(&self) -> RawJsonRpcMessage {
            self.next_frames(1).await.remove(0)
        }

        /// Real-time frame poll: parks on a timer between polls so spawned
        /// server tasks are never starved by a busy yield loop.  Overflow
        /// frames stay queued for the next call (never dropped).
        async fn next_frame_real(&self) -> RawJsonRpcMessage {
            loop {
                let ready = {
                    let mut pending = self.pending.lock().expect("harness pending poisoned");
                    if pending.is_empty() {
                        pending.extend(self.handle.take_outbound());
                    }
                    pending.pop_front()
                };
                if let Some(frame) = ready {
                    return frame;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }

        /// Waits for exactly `count` outbound frames, keeping overflow
        /// queued for the next call.
        async fn next_frames(&self, count: usize) -> Vec<RawJsonRpcMessage> {
            for _ in 0..5_000 {
                let ready = {
                    let mut pending = self.pending.lock().expect("harness pending poisoned");
                    if pending.len() < count {
                        pending.extend(self.handle.take_outbound());
                    }
                    if pending.len() >= count {
                        Some(pending.drain(..count).collect())
                    } else {
                        None
                    }
                };
                if let Some(frames) = ready {
                    return frames;
                }
                tokio::task::yield_now().await;
            }
            panic!("timed out waiting for {count} outbound frames");
        }

        async fn shutdown(self, task: tokio::task::JoinHandle<Result<(), AcpServerError>>) {
            drop(self.handle);
            task.await.expect("server task joins").expect("server exits cleanly on EOF");
        }

        /// Snapshot of outbound frames not yet consumed by the harness.
        fn outbound(&self) -> Vec<RawJsonRpcMessage> {
            self.handle.outbound()
        }
    }

    fn spawn_server<M: ModelAdapter>(
        provider: OrchestratorProvider<M>,
    ) -> (Harness, tokio::task::JoinHandle<Result<(), AcpServerError>>) {
        let server = AcpAgentServer::new(provider, AcpAgentServerConfig::default());
        let (transport, handle) = MemoryTransport::new();
        let task = tokio::spawn(async move { server.run_with_transport(transport).await });
        (Harness::new(handle), task)
    }

    fn request(id: i64, method: &str, params: Value) -> RawJsonRpcMessage {
        RawJsonRpcMessage::request(method.to_string(), params, RequestId::Number(id))
            .expect("test request builds")
    }

    fn notification(method: &str, params: Value) -> RawJsonRpcMessage {
        RawJsonRpcMessage::notification(method.to_string(), params)
            .expect("test notification builds")
    }

    fn request_result(frame: RawJsonRpcMessage) -> Value {
        let Response::Result { result, .. } = unwrap_response(frame.clone()) else {
            panic!("expected a result response, got {frame:?}");
        };
        result
    }

    fn unwrap_response(frame: RawJsonRpcMessage) -> Response<Value> {
        let RawJsonRpcMessage::Response(response) = frame else {
            panic!("expected a response frame, got {frame:?}");
        };
        response
    }

    /// Converts raw JSON-RPC params into a plain JSON value (mirrors the
    /// framework server's own conversion; `RawJsonRpcParams` has no
    /// `Serialize`).
    fn raw_params_to_value(params: Option<RawJsonRpcParams>) -> Value {
        match params {
            None => Value::Null,
            Some(RawJsonRpcParams::Object(map)) => Value::Object(map),
            Some(RawJsonRpcParams::Array(array)) => Value::Array(array),
        }
    }

    fn session_new_params(cwd: &str) -> Value {
        json!({
            "cwd": cwd,
            "additionalDirectories": [],
            "mcpServers": [],
        })
    }

    fn session_new_params_with_additional(cwd: &str, additional: &[&str]) -> Value {
        json!({
            "cwd": cwd,
            "additionalDirectories": additional,
            "mcpServers": [],
        })
    }

    fn prompt_params(session_id: &str, text: &str) -> Value {
        json!({
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": text }],
        })
    }

    async fn new_session(handle: &Harness, id: i64) -> String {
        handle.send(request(id, "session/new", session_new_params("/work")));
        let result = request_result(handle.next_frame().await);
        let session_id = result["sessionId"].as_str().expect("session id").to_string();
        // The provider advertises its initial slash commands after the
        // session/new response; drain the update before prompt flows.
        let frame = handle.next_frame().await;
        let RawJsonRpcMessage::Notification(update) = &frame else {
            panic!("expected the available_commands_update, got {frame:?}");
        };
        assert_eq!(
            raw_params_to_value(update.params.clone())["update"]["sessionUpdate"],
            "available_commands_update"
        );
        session_id
    }

    /// Drains the MCP diagnostics thought updates emitted at prompt start
    /// (Phase 12) until the summary `mcp-diagnostics` message is seen.
    /// Every frame here is a thought chunk (diagnostics precede the plan
    /// update), so no push-back is needed.
    async fn drain_mcp_diagnostics(handle: &Harness) {
        loop {
            let frame = handle.next_frame().await;
            let RawJsonRpcMessage::Notification(update) = &frame else {
                panic!("expected an update while draining, got {frame:?}");
            };
            let params = raw_params_to_value(update.params.clone());
            assert_eq!(
                params["update"]["sessionUpdate"], "agent_thought_chunk",
                "only thought updates precede the plan"
            );
            if params["update"]["messageId"] == "mcp-diagnostics" {
                return;
            }
        }
    }

    /// Bounded wait that pumps the runtime until `condition` holds.
    async fn wait_until(condition: impl Fn() -> bool) {
        for _ in 0..10_000 {
            if condition() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("condition never satisfied");
    }

    /// Model that blocks until the cancellation watch flips, then reports
    /// cancellation; proves framework cancellation reaches the running turn.
    struct CancelAwaitingModel {
        calls: Arc<Mutex<usize>>,
    }

    impl ModelAdapter for CancelAwaitingModel {
        fn complete(
            &self,
            _request: ModelRequest,
            mut cancel: watch::Receiver<bool>,
        ) -> ModelFuture<Result<ModelResponse, ModelError>> {
            let calls = self.calls.clone();
            Box::pin(async move {
                *calls.lock().expect("calls poisoned") += 1;
                if *cancel.borrow() {
                    return Err(ModelError::Cancelled);
                }
                let _ = cancel.changed().await;
                Err(ModelError::Cancelled)
            })
        }
    }

    // ── Adapter behavior through the framework server ────────────────────

    #[tokio::test]
    async fn provider_adapter_initialize_metadata_through_memory_transport() {
        let model = Arc::new(FakeModel::new(Vec::new()));
        let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model);
        let (handle, task) = spawn_server(provider);

        handle.send(request(1, "initialize", json!({ "protocolVersion": 1 })));
        let result = request_result(handle.next_frame().await);
        assert_eq!(result["protocolVersion"], 1);
        assert_eq!(result["agentInfo"]["name"], DEFAULT_IMPLEMENTATION_NAME);
        assert_eq!(result["agentInfo"]["title"], DEFAULT_IMPLEMENTATION_TITLE);
        assert_eq!(
            result["agentInfo"]["version"],
            env!("CARGO_PKG_VERSION"),
            "version comes from the adapter crate"
        );
        assert_eq!(result["agentCapabilities"]["loadSession"], true);
        assert!(handle.outbound().is_empty(), "no further frames");

        handle.shutdown(task).await;
    }

    #[tokio::test]
    async fn provider_adapter_new_session_creates_orchestrator_state() {
        let model = Arc::new(FakeModel::new(Vec::new()));
        let provider =
            OrchestratorProvider::new(OrchestratorProviderConfig::default(), model.clone());
        let probe = provider.clone();
        let (handle, task) = spawn_server(provider);

        handle.send(request(1, "session/new", session_new_params("/work")));
        let result = request_result(handle.next_frame().await);
        assert_eq!(result["sessionId"], "session-1", "monotonic provider id");

        let (tasks, memory) = probe.session_state("session-1").expect("session state exists");
        assert_eq!(tasks.len(), 0, "fresh task graph");
        assert!(memory.is_empty(), "fresh memory store");

        handle.shutdown(task).await;
    }

    #[tokio::test]
    async fn provider_adapter_prompt_runs_loop_and_emits_assistant_update() {
        let model =
            Arc::new(FakeModel::new(vec![ModelResponse::new().text("final answer").completed()]));
        let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model);
        let (handle, task) = spawn_server(provider);
        let session_id = new_session(&handle, 1).await;
        assert_eq!(session_id, "session-1");

        handle.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
        // MCP diagnostics (no servers configured), then plan replace, then
        // the assistant message chunk, then the response.
        drain_mcp_diagnostics(&handle).await;
        let frames = handle.next_frames(3).await;
        let RawJsonRpcMessage::Notification(plan) = &frames[0] else {
            panic!("first frame is the plan update, got {:?}", frames[0]);
        };
        assert_eq!(plan.method.as_ref(), "session/update");
        let plan_params = raw_params_to_value(plan.params.clone());
        assert_eq!(plan_params["sessionId"], "session-1");
        assert_eq!(plan_params["update"]["sessionUpdate"], "plan", "plan replacement update");

        let RawJsonRpcMessage::Notification(message) = &frames[1] else {
            panic!("second frame is the message update, got {:?}", frames[1]);
        };
        let message_params = raw_params_to_value(message.params.clone());
        assert_eq!(message_params["sessionId"], "session-1");
        assert_eq!(message_params["update"]["sessionUpdate"], "agent_message_chunk");
        assert!(
            message_params.to_string().contains("final answer"),
            "message chunk carries the assistant text: {message_params}"
        );

        let result = request_result(frames[2].clone());
        assert_eq!(result["stopReason"], "end_turn");

        handle.shutdown(task).await;
    }

    #[tokio::test]
    async fn provider_adapter_prompt_includes_workspace_system_context() {
        let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("ok").completed()]));
        let provider =
            OrchestratorProvider::new(OrchestratorProviderConfig::default(), model.clone());
        let (handle, task) = spawn_server(provider);

        handle.send(request(
            1,
            "session/new",
            session_new_params_with_additional("/work/project", &["/shared/lib"]),
        ));
        let session_id = request_result(handle.next_frame().await)["sessionId"]
            .as_str()
            .expect("session id")
            .to_string();
        // The provider advertises its initial slash commands after the
        // session/new response; drain the update before the prompt flows.
        let frame = handle.next_frame().await;
        let RawJsonRpcMessage::Notification(update) = &frame else {
            panic!("expected the available_commands_update, got {frame:?}");
        };
        assert_eq!(
            raw_params_to_value(update.params.clone())["update"]["sessionUpdate"],
            "available_commands_update"
        );
        handle.send(request(2, "session/prompt", prompt_params(&session_id, "read .ee.toml")));
        drain_mcp_diagnostics(&handle).await;
        let frames = handle.next_frames(3).await;
        assert_eq!(request_result(frames[2].clone())["stopReason"], "end_turn");

        let requests = model.requests();
        let context = requests[0].transcript[0].text_content();
        assert!(context.contains("current_working_directory: /work/project"), "{context}");
        assert!(context.contains("- /shared/lib"), "{context}");
        assert!(context.contains("require absolute paths"), "{context}");
        assert!(context.contains("Resolve relative paths"), "{context}");

        handle.shutdown(task).await;
    }

    #[tokio::test]
    async fn provider_adapter_prompt_executes_tool_through_client_bridge() {
        let model = Arc::new(FakeModel::new(vec![
            ModelResponse::new().tool_intents(vec![crate::tools::ToolIntent::new(
                "tc-1",
                "read_file",
                json!({ "path": "/tmp/notes.txt" }),
            )]),
            ModelResponse::new().text("read it").completed(),
        ]));
        let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model);
        let (handle, task) = spawn_server(provider);
        let session_id = new_session(&handle, 1).await;

        handle.send(request(2, "session/prompt", prompt_params(&session_id, "read a file")));
        // MCP diagnostics, plan, pending tool-call, in-progress tool-call,
        // then the framework-owned fs request.
        drain_mcp_diagnostics(&handle).await;
        let frames = handle.next_frames(4).await;
        let RawJsonRpcMessage::Request(fs_request) = &frames[3] else {
            panic!("fourth frame is the fs request, got {:?}", frames[3]);
        };
        assert_eq!(fs_request.method.as_ref(), "fs/read_text_file");
        let fs_params = raw_params_to_value(fs_request.params.clone());
        assert_eq!(fs_params["path"], "/tmp/notes.txt");
        assert!(matches!(fs_request.id, RequestId::Number(_)), "framework-owned numeric id");

        // Answer the bridge call; the loop appends the observation and asks
        // the model again, streaming the completed tool-call update, then the
        // assistant update, then the response.
        handle.send(RawJsonRpcMessage::response(
            fs_request.id.clone(),
            Ok(json!({ "content": "file contents" })),
        ));
        let frames = handle.next_frames(3).await;
        assert_eq!(
            raw_params_to_value(match &frames[0] {
                RawJsonRpcMessage::Notification(update) => update.params.clone(),
                other => panic!("expected completed tool-call update, got {other:?}"),
            })["update"]["sessionUpdate"],
            "tool_call_update",
            "completion update streamed before the response"
        );
        let RawJsonRpcMessage::Notification(message) = &frames[1] else {
            panic!("expected the message update, got {:?}", frames[1]);
        };
        let message_params = raw_params_to_value(message.params.clone());
        assert_eq!(message_params["update"]["sessionUpdate"], "agent_message_chunk");
        assert!(
            message_params.to_string().contains("read it"),
            "message chunk carries the assistant text: {message_params}"
        );
        let result = request_result(frames[2].clone());
        assert_eq!(result["stopReason"], "end_turn");

        handle.shutdown(task).await;
    }

    #[tokio::test]
    async fn provider_adapter_cancel_stops_active_turn() {
        let calls = Arc::new(Mutex::new(0usize));
        let model = Arc::new(CancelAwaitingModel { calls: calls.clone() });
        let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model);
        let (handle, task) = spawn_server(provider);
        let session_id = new_session(&handle, 1).await;

        handle.send(request(2, "session/prompt", prompt_params(&session_id, "blocking prompt")));
        drain_mcp_diagnostics(&handle).await;
        let _plan = handle.next_frame().await; // plan update proves the turn started
        wait_until(|| *calls.lock().expect("calls poisoned") == 1).await;

        handle.send(notification("session/cancel", json!({ "sessionId": session_id })));
        let result = request_result(handle.next_frame().await);
        assert_eq!(result["stopReason"], "cancelled", "deterministic cancelled result");
        assert_eq!(*calls.lock().expect("calls poisoned"), 1, "no second model call");
        assert!(handle.outbound().is_empty(), "no updates after cancellation");

        handle.shutdown(task).await;
    }

    #[tokio::test]
    async fn provider_adapter_close_removes_session_state() {
        let model = Arc::new(FakeModel::new(Vec::new()));
        let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model);
        let probe = provider.clone();
        let (handle, task) = spawn_server(provider);
        let session_id = new_session(&handle, 1).await;
        assert!(probe.session_state(&session_id).is_some());

        handle.send(request(2, "session/close", json!({ "sessionId": session_id })));
        let result = request_result(handle.next_frame().await);
        assert_eq!(result, json!({}));
        assert!(probe.session_state(&session_id).is_none(), "live state removed");
        assert!(probe.has_persisted_state(&session_id), "state persisted for load");

        // The framework store no longer knows the session either.
        handle.send(request(3, "session/prompt", prompt_params(&session_id, "hello")));
        let error = request_error(handle.next_frame().await);
        assert!(error.message.contains("session"), "unknown session rejected");

        handle.shutdown(task).await;
    }

    #[tokio::test]
    async fn provider_adapter_load_session_restores_persisted_state() {
        let model = Arc::new(FakeModel::new(vec![
            ModelResponse::new().text("first turn").completed(),
            ModelResponse::new().text("second turn").completed(),
        ]));
        let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model);
        let probe = provider.clone();
        let (handle, task) = spawn_server(provider);
        let session_id = new_session(&handle, 1).await;

        // One turn creates a root task; closing persists the task graph.
        handle.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
        drain_mcp_diagnostics(&handle).await;
        handle.next_frames(3).await; // plan, message, response
        assert_eq!(probe.session_state(&session_id).expect("state").0.len(), 1);
        handle.send(request(3, "session/close", json!({ "sessionId": session_id })));
        let _ = request_result(handle.next_frame().await);

        // Loading restores the persisted graph and replays the recorded
        // conversation (user prompt + agent text) before the response.
        handle.send(request(
            4,
            "session/load",
            json!({
                "sessionId": session_id,
                "cwd": "/work",
                "additionalDirectories": [],
                "mcpServers": [],
            }),
        ));
        // Replay order: user message, agent message, then the re-advertised
        // initial commands, then the response (all updates precede it).
        let frames = handle.next_frames(4).await;
        let RawJsonRpcMessage::Notification(user) = &frames[0] else {
            panic!("expected the replayed user message, got {:?}", frames[0]);
        };
        let user_params = raw_params_to_value(user.params.clone());
        assert_eq!(user_params["update"]["sessionUpdate"], "user_message_chunk");
        assert_eq!(user_params["update"]["messageId"], "replay-u-1");
        assert_eq!(user_params["update"]["content"]["text"], "hello");
        let RawJsonRpcMessage::Notification(agent) = &frames[1] else {
            panic!("expected the replayed agent message, got {:?}", frames[1]);
        };
        let agent_params = raw_params_to_value(agent.params.clone());
        assert_eq!(agent_params["update"]["sessionUpdate"], "agent_message_chunk");
        assert_eq!(agent_params["update"]["messageId"], "replay-a-2");
        assert_eq!(agent_params["update"]["content"]["text"], "first turn");
        let RawJsonRpcMessage::Notification(commands) = &frames[2] else {
            panic!("expected the available_commands_update, got {:?}", frames[2]);
        };
        assert_eq!(
            raw_params_to_value(commands.params.clone())["update"]["sessionUpdate"],
            "available_commands_update"
        );
        let result = request_result(frames[3].clone());
        assert_eq!(
            result,
            json!({}),
            "LoadSessionResponse carries no session id; restore is proven by state"
        );
        let (tasks, _memory) = probe.session_state(&session_id).expect("restored state");
        assert_eq!(tasks.len(), 1, "persisted root task restored");

        handle.send(request(5, "session/prompt", prompt_params(&session_id, "again")));
        drain_mcp_diagnostics(&handle).await;
        let frames = handle.next_frames(3).await;
        assert_eq!(request_result(frames[2].clone())["stopReason"], "end_turn");
        assert_eq!(
            probe.session_state(&session_id).expect("state").0.len(),
            2,
            "restored graph carries into the new turn"
        );

        handle.shutdown(task).await;
    }

    fn request_error(frame: RawJsonRpcMessage) -> RpcError {
        let Response::Error { error, .. } = unwrap_response(frame) else {
            panic!("expected an error response");
        };
        error
    }

    // ── Phase 12: MCP bridge through the framework server ────────────────

    fn session_new_params_with_mcp(cwd: &str, mcp_servers: Value) -> Value {
        json!({
            "cwd": cwd,
            "additionalDirectories": [],
            "mcpServers": mcp_servers,
        })
    }

    /// Answers outbound client requests as a fake ACP MCP host while a
    /// prompt runs, collecting thought updates; returns when the prompt
    /// response frame arrives.
    struct PromptMcpRunner {
        inner: std::collections::HashMap<String, Value>,
        calls: std::collections::HashMap<String, Value>,
        fail_connect: bool,
        /// Every inner MCP request logged as `method: params`.
        mcp_requests: std::sync::Mutex<Vec<String>>,
    }

    impl PromptMcpRunner {
        fn new() -> Self {
            Self {
                inner: std::collections::HashMap::new(),
                calls: std::collections::HashMap::new(),
                fail_connect: false,
                mcp_requests: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn answer(&mut self, method: &str, result: Value) {
            self.inner.insert(method.to_string(), result);
        }

        fn answer_call(&mut self, tool_name: &str, result: Value) {
            self.calls.insert(tool_name.to_string(), result);
        }

        fn log(&self) -> Vec<String> {
            self.mcp_requests.lock().expect("runner log poisoned").clone()
        }

        /// Standard ee proxy discovery answers (connect + discover + list).
        fn standard_ee_answers(tools: Value) -> Self {
            let mut runner = Self::new();
            runner.answer(
                "server/discover",
                json!({
                    "resultType": "complete",
                    "supportedVersions": ["2026-07-28"],
                    "capabilities": { "tools": {} },
                    "ttlMs": 0,
                    "cacheScope": "private",
                }),
            );
            runner.answer(
                "tools/list",
                json!({ "tools": tools, "resultType": "complete", "ttlMs": 0, "cacheScope": "private" }),
            );
            runner
        }

        /// Drives the harness until the prompt response; returns the thought
        /// updates and the stop reason.
        async fn run(&mut self, handle: &Harness) -> (Vec<String>, String) {
            let mut thoughts = Vec::new();
            loop {
                let frame = handle.next_frame().await;
                match frame {
                    RawJsonRpcMessage::Request(request) => {
                        let params = raw_params_to_value(request.params.clone());
                        let method = request.method.to_string();
                        let response = self.response_for(&method, &params);
                        handle.send(RawJsonRpcMessage::response(request.id.clone(), Ok(response)));
                    }
                    RawJsonRpcMessage::Notification(notification) => {
                        let params = raw_params_to_value(notification.params.clone());
                        if params["update"]["sessionUpdate"] == "agent_thought_chunk" {
                            thoughts.push(
                                params["update"]["content"]["text"]
                                    .as_str()
                                    .unwrap_or_default()
                                    .to_string(),
                            );
                        }
                    }
                    RawJsonRpcMessage::Response(response) => {
                        let Response::Result { result, .. } = response else {
                            panic!("unexpected prompt error response");
                        };
                        let stop_reason =
                            result["stopReason"].as_str().unwrap_or_default().to_string();
                        return (thoughts, stop_reason);
                    }
                }
            }
        }

        fn response_for(&mut self, method: &str, params: &Value) -> Value {
            match method {
                "mcp/connect" => {
                    if self.fail_connect {
                        json!({})
                    } else {
                        json!({ "connectionId": "conn-1" })
                    }
                }
                "mcp/disconnect" => json!({}),
                "mcp/message" => {
                    let inner_method =
                        params.get("method").and_then(Value::as_str).unwrap_or_default();
                    self.mcp_requests
                        .lock()
                        .expect("runner log poisoned")
                        .push(format!("{inner_method}: {params}"));
                    if inner_method == "tools/call" {
                        let tool_name = params
                            .pointer("/params/name")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        self.calls.get(tool_name).cloned().unwrap_or_else(|| {
                            panic!("no canned tools/call response for {tool_name:?}")
                        })
                    } else {
                        self.inner.get(inner_method).cloned().unwrap_or_else(|| {
                            panic!("no canned inner response for {inner_method:?}")
                        })
                    }
                }
                other => panic!("unexpected client request {other}"),
            }
        }
    }

    fn ee_proxy_acp_mcp_servers() -> Value {
        json!([{ "type": "acp", "name": "ee", "serverId": "ee-mcp-proxy:test" }])
    }

    fn ee_tool(name: &str) -> Value {
        json!({ "name": name, "description": format!("{name} tool"), "inputSchema": { "type": "object", "properties": {} } })
    }

    async fn mcp_new_session(handle: &Harness, id: i64, mcp_servers: Value) -> String {
        handle.send(request(id, "session/new", session_new_params_with_mcp("/work", mcp_servers)));
        let result = request_result(handle.next_frame().await);
        result["sessionId"].as_str().expect("session id").to_string()
    }

    #[tokio::test]
    async fn provider_adapter_advertises_mcp_capabilities_acp() {
        let model = Arc::new(FakeModel::new(Vec::new()));
        let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model);
        let (handle, task) = spawn_server(provider);

        handle.send(request(1, "initialize", json!({ "protocolVersion": 1 })));
        let result = request_result(handle.next_frame().await);
        assert_eq!(
            result["agentCapabilities"]["mcpCapabilities"]["acp"], true,
            "orchestrated providers host MCP-over-ACP"
        );

        handle.shutdown(task).await;
    }

    #[tokio::test]
    async fn provider_adapter_session_new_retains_redacted_mcp_servers() {
        let model = Arc::new(FakeModel::new(Vec::new()));
        let provider =
            OrchestratorProvider::new(OrchestratorProviderConfig::default(), model.clone());
        let probe = provider.clone();
        let (handle, task) = spawn_server(provider);

        let session_id = mcp_new_session(
            &handle,
            1,
            json!([
                { "type": "acp", "name": "ee", "serverId": "ee-mcp-proxy:test" },
                {
                    "name": "filesystem",
                    "command": "/usr/bin/server",
                    "args": [],
                    "env": [{ "name": "API_TOKEN", "value": "sekrit-value" }],
                },
            ]),
        )
        .await;

        let descriptors = probe.session_mcp_servers(&session_id);
        assert_eq!(descriptors.len(), 2, "descriptors retained per session");
        let debug = format!("{descriptors:?}");
        assert!(
            !debug.contains("sekrit-value") && !debug.contains("API_TOKEN"),
            "env secrets must never reach Debug output: {debug}"
        );
        assert!(debug.contains("filesystem"), "server names stay visible");

        handle.shutdown(task).await;
    }

    #[tokio::test]
    async fn provider_adapter_session_new_rejects_unsupported_mcp_transport() {
        let model = Arc::new(FakeModel::new(Vec::new()));
        let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model);
        let (handle, task) = spawn_server(provider);

        handle.send(request(
            1,
            "session/new",
            session_new_params_with_mcp(
                "/work",
                json!([{
                    "type": "http",
                    "name": "remote",
                    "url": "https://example.com/mcp",
                    "headers": [],
                }]),
            ),
        ));
        let error = request_error(handle.next_frame().await);
        assert!(
            error.message.contains("streamable-http"),
            "fail closed with a clear reason: {}",
            error.message
        );

        handle.shutdown(task).await;
    }

    #[tokio::test]
    async fn provider_adapter_prompt_exposes_ee_proxy_tools_and_dispatches_calls() {
        let model = Arc::new(FakeModel::new(vec![
            ModelResponse::new().tool_intents(vec![crate::tools::ToolIntent::new(
                "tc-1",
                "ee_workspace_roots",
                json!({}),
            )]),
            ModelResponse::new().text("roots listed").completed(),
        ]));
        let provider =
            OrchestratorProvider::new(OrchestratorProviderConfig::default(), model.clone());
        let probe = provider.clone();
        let (handle, task) = spawn_server(provider);
        let session_id = mcp_new_session(&handle, 1, ee_proxy_acp_mcp_servers()).await;

        let mut runner =
            PromptMcpRunner::standard_ee_answers(json!([ee_tool("ee_workspace_roots")]));
        runner.answer_call(
            "ee_workspace_roots",
            json!({
                "resultType": "complete",
                "content": [{ "type": "text", "text": "/work\n/shared" }],
                "structuredContent": { "roots": ["/work", "/shared"] },
            }),
        );

        handle.send(request(2, "session/prompt", prompt_params(&session_id, "list roots")));
        let (thoughts, stop_reason) = runner.run(&handle).await;
        assert_eq!(stop_reason, "end_turn");

        // The model received the MCP tool with a provider-compatible name.
        let requests = model.requests();
        let tools = &requests[0].tools;
        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
        assert!(names.contains(&"ee_workspace_roots"), "{names:?}");
        assert!(
            tools.iter().all(|tool| !tool.name.contains('.')),
            "no provider-rejected characters: {names:?}"
        );

        // The model's call dispatched to MCP tools/call with the original name.
        let log = runner.log();
        assert!(
            log.iter()
                .any(|line| line.contains("tools/call") && line.contains("ee_workspace_roots")),
            "{log:?}"
        );

        // The tool result reached the transcript.
        let transcript = requests[1].transcript.clone();
        let all_text: String = transcript
            .iter()
            .flat_map(|message| message.content.iter())
            .map(|block| match block {
                crate::model::ModelContent::Text(text) => text.clone(),
                crate::model::ModelContent::ToolResult { result, .. } => result.summary_text(),
                _ => String::new(),
            })
            .collect();
        assert!(all_text.contains("/work"), "tool output in transcript: {all_text}");

        // MCP tools are deregistered after the prompt; builtins remain.
        let names = probe.session_tool_names(&session_id);
        assert!(!names.contains(&"ee_workspace_roots".to_string()), "{names:?}");
        assert!(names.contains(&"read_file".to_string()), "{names:?}");

        // No discovery diagnostics on the happy path.
        assert!(thoughts.is_empty(), "{thoughts:?}");

        handle.shutdown(task).await;
    }

    #[tokio::test]
    async fn provider_adapter_policy_denies_mcp_write_tool_without_wire_call() {
        let model = Arc::new(FakeModel::new(vec![
            ModelResponse::new().tool_intents(vec![crate::tools::ToolIntent::new(
                "tc-1",
                "ee_write_text_file",
                json!({ "path": "/work/x.txt", "content": "data" }),
            )]),
            ModelResponse::new().text("denied, continuing").completed(),
        ]));
        // Default provider policy: reads only — MCP write tools stay denied.
        let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model);
        let (handle, task) = spawn_server(provider);
        let session_id = mcp_new_session(&handle, 1, ee_proxy_acp_mcp_servers()).await;

        let mut runner = PromptMcpRunner::standard_ee_answers(json!([
            ee_tool("ee_workspace_roots"),
            ee_tool("ee_write_text_file"),
        ]));

        handle.send(request(2, "session/prompt", prompt_params(&session_id, "write a file")));
        let (_thoughts, stop_reason) = runner.run(&handle).await;
        assert_eq!(stop_reason, "end_turn", "denial does not crash the turn");

        // The write tool was never dispatched to the MCP server.
        let log = runner.log();
        assert!(
            log.iter()
                .all(|line| !line.contains("tools/call") || !line.contains("ee_write_text_file")),
            "policy must block the wire call: {log:?}"
        );

        handle.shutdown(task).await;
    }

    #[tokio::test]
    async fn provider_adapter_mcp_failures_surface_diagnostics_without_secrets() {
        let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("ok").completed()]));
        let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model);
        let (handle, task) = spawn_server(provider);
        let session_id = mcp_new_session(
            &handle,
            1,
            json!([
                { "type": "acp", "name": "ee", "serverId": "ee-mcp-proxy:test" },
                {
                    "name": "filesystem",
                    "command": "/nonexistent/ee-server",
                    "args": [],
                    "env": [{ "name": "API_TOKEN", "value": "sekrit-value" }],
                },
            ]),
        )
        .await;

        // The ee proxy connect fails; the stdio spawn fails (no binary).
        let mut runner = PromptMcpRunner::new();
        runner.fail_connect = true;

        handle.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
        let (thoughts, stop_reason) = runner.run(&handle).await;
        assert_eq!(stop_reason, "end_turn", "discovery failures do not crash the turn");
        assert!(
            thoughts.iter().any(|thought| thought.contains("unavailable")),
            "connect failure surfaced: {thoughts:?}"
        );
        assert!(
            thoughts.iter().any(|thought| thought.contains("no MCP tools were registered")
                || thought.contains("could not be registered")),
            "no-tools diagnostic surfaced: {thoughts:?}"
        );
        let all = thoughts.join("\n");
        assert!(
            !all.contains("sekrit-value") && !all.contains("API_TOKEN"),
            "secrets must never reach diagnostics: {all}"
        );

        handle.shutdown(task).await;
    }

    #[tokio::test]
    async fn provider_adapter_model_can_list_mcp_tools_beyond_read_file() {
        // Regression for "what MCP tools do I have": with the ee proxy
        // present, the model's tool list includes the MCP tools.
        let model = Arc::new(FakeModel::new(vec![
            ModelResponse::new()
                .text("You have ee_workspace_roots, ee_search_text, and read_file")
                .completed(),
        ]));
        let provider =
            OrchestratorProvider::new(OrchestratorProviderConfig::default(), model.clone());
        let (handle, task) = spawn_server(provider);
        let session_id = mcp_new_session(&handle, 1, ee_proxy_acp_mcp_servers()).await;

        let mut runner = PromptMcpRunner::standard_ee_answers(json!([
            ee_tool("ee_workspace_roots"),
            ee_tool("ee_search_text"),
        ]));

        handle.send(request(
            2,
            "session/prompt",
            prompt_params(&session_id, "what MCP tools do I have"),
        ));
        let (_thoughts, stop_reason) = runner.run(&handle).await;
        assert_eq!(stop_reason, "end_turn");

        let tools = &model.requests()[0].tools;
        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
        assert!(names.contains(&"ee_workspace_roots"), "{names:?}");
        assert!(names.contains(&"ee_search_text"), "{names:?}");
        assert!(names.contains(&"read_file"), "builtins still present: {names:?}");
        assert!(tools.len() > 1, "more than a single tool: {names:?}");
        assert!(
            tools.iter().all(|tool| !tool.name.contains('.')),
            "no dots in model-facing names: {names:?}"
        );

        handle.shutdown(task).await;
    }

    #[tokio::test]
    async fn provider_adapter_compact_prompt_skips_mcp_discovery_and_tools() {
        let model =
            Arc::new(FakeModel::new(vec![ModelResponse::new().text("SUMMARY TEXT").completed()]));
        let provider =
            OrchestratorProvider::new(OrchestratorProviderConfig::default(), model.clone());
        let probe = provider.clone();
        let (handle, task) = spawn_server(provider);
        let session_id = new_session(&handle, 1).await;

        handle.send(request(2, "session/prompt", prompt_params(&session_id, "/compact")));
        // No MCP diagnostics and no plan update: the compaction path bypasses
        // server discovery and the model–tool loop entirely.
        let frames = handle.next_frames(2).await;
        let RawJsonRpcMessage::Notification(report) = &frames[0] else {
            panic!("expected the compaction report update, got {:?}", frames[0]);
        };
        let params = raw_params_to_value(report.params.clone());
        assert_eq!(params["sessionId"], session_id);
        assert_eq!(params["update"]["sessionUpdate"], "agent_message_chunk");
        assert!(
            params["update"]["content"]["text"].as_str().unwrap().contains("Session compacted:"),
            "{params}"
        );
        let result = request_result(frames[1].clone());
        assert_eq!(result["stopReason"], "end_turn");
        assert!(handle.outbound().is_empty(), "no further frames: {:?}", handle.outbound());

        // The model saw exactly one request with no tools; the summary is
        // stored as session memory and no task was created.
        let requests = model.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].tools.is_empty(), "no tools exposed during compaction");
        let (tasks, memory) = probe.session_state(&session_id).expect("session state");
        assert_eq!(tasks.len(), 0, "no task graph entry for compaction");
        assert_eq!(memory.query("summary:session").expect("stored").value, "SUMMARY TEXT");

        handle.shutdown(task).await;
    }
}
