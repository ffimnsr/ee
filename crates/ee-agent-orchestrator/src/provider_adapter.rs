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
    AgentCapabilities, COMPACT_COMMAND_NAME, Implementation, McpCapabilities, SessionCapabilities,
    SessionCloseCapabilities, SessionId, SessionListCapabilities, compact_available_command,
    parse_slash_command,
};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::config::OrchestratorConfig;
use crate::mcp::{
    McpBackedTool, McpDiscoveryDiagnostic, McpServerDescriptor, McpSessionManager, McpToolPolicy,
};
use crate::memory::MemoryStore;
use crate::model::ModelAdapter;
use crate::policy::PolicyEngine;
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
}

/// Serialized orchestrator state kept when a session closes so a later
/// `session/load` can restore it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedSession {
    tasks: TaskGraph,
    memory: MemoryStore,
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
        // closing are handled by the framework.  The provider hosts
        // MCP-over-ACP for every session (the `ClientBridge` mcp/* path), so
        // `mcp_capabilities.acp` is advertised; hosts then append the ee
        // proxy as `McpServer::Acp` instead of the stdio fallback.  Prompt/
        // image capabilities stay at their defaults.
        AgentCapabilities::default()
            .load_session(true)
            .mcp_capabilities(McpCapabilities::new().acp(true))
            .session_capabilities(
                SessionCapabilities::new()
                    .list(SessionListCapabilities::new())
                    .close(SessionCloseCapabilities::new()),
            )
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
        Box::pin(async move {
            // Process-local monotonic id; deterministic (`session-1`, ...) for
            // tests.
            let number = next_session.fetch_add(1, Ordering::Relaxed);
            let session_id = SessionId::new(format!("{SESSION_ID_PREFIX}-{number}"));
            let system_context = workspace_system_context(&ctx.cwd, &ctx.additional_directories);
            let mcp_servers = validate_mcp_servers(&ctx.mcp_servers)?;
            let runtime = Arc::new(OrchestratorRuntime::with_policy(config, model, policy));
            runtime.register_builtins(&session_id).map_err(|error| {
                ProviderError::BackendFailure(format!("failed to register built-in tools: {error}"))
            })?;
            sessions.lock().expect("adapter sessions poisoned").insert(
                session_id.to_string(),
                SessionRuntime { runtime, system_context, mcp_servers },
            );
            Ok(SessionInit::new(session_id).commands(vec![compact_available_command()]))
        })
    }

    fn load_session(
        &self,
        ctx: LoadSessionContext,
    ) -> ProviderFuture<Result<SessionInit, ProviderError>> {
        let config = self.config.orchestrator.clone();
        let model = self.model.clone();
        let policy = self.policy.clone();
        let sessions = self.sessions.clone();
        let persisted = self.persisted.clone();
        let session_id = ctx.session_id.clone();
        Box::pin(async move {
            let Some(state) = persisted
                .lock()
                .expect("adapter persisted poisoned")
                .remove(&session_id.to_string())
            else {
                return Err(ProviderError::BackendFailure(format!(
                    "no persisted orchestrator state for session {session_id}"
                )));
            };
            let system_context = workspace_system_context(&ctx.cwd, &ctx.additional_directories);
            let mcp_servers = validate_mcp_servers(&ctx.mcp_servers)?;
            let runtime = Arc::new(OrchestratorRuntime::with_state(
                config,
                model,
                policy,
                state.tasks,
                state.memory,
            ));
            runtime.register_builtins(&session_id).map_err(|error| {
                ProviderError::BackendFailure(format!("failed to register built-in tools: {error}"))
            })?;
            sessions.lock().expect("adapter sessions poisoned").insert(
                session_id.to_string(),
                SessionRuntime { runtime, system_context, mcp_servers },
            );
            Ok(SessionInit::new(session_id).commands(vec![compact_available_command()]))
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
        Box::pin(async move {
            let session = {
                let sessions = sessions.lock().expect("adapter sessions poisoned");
                sessions.get(&session_id.to_string()).map(|session| {
                    (
                        session.runtime.clone(),
                        session.system_context.clone(),
                        session.mcp_servers.clone(),
                    )
                })
            };
            let Some((runtime, system_context, mcp_servers)) = session else {
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
            let result = runtime
                .run_turn_with_system_context(ctx, sink, client, cancel, system_context)
                .await;
            cleanup.finish().await;
            result.map_err(ProviderError::from)
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
            persisted.lock().expect("adapter persisted poisoned").insert(
                session_id.to_string(),
                PersistedSession {
                    tasks: runtime.runtime.tasks(),
                    memory: runtime.runtime.memory(),
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
    use crate::model::{ModelError, ModelFuture, ModelRequest, ModelResponse};
    use crate::test_support::FakeModel;

    // ── Server-over-memory-transport harness ─────────────────────────────

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
        let Response::Result { result, .. } = unwrap_response(frame) else {
            panic!("expected a result response");
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

        // Loading restores the persisted graph; the next turn keeps it.
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
        let result = request_result(handle.next_frame().await);
        assert_eq!(
            result,
            json!({}),
            "LoadSessionResponse carries no session id; restore is proven by state"
        );
        // Restored providers re-advertise their initial commands.
        let frame = handle.next_frame().await;
        let RawJsonRpcMessage::Notification(update) = &frame else {
            panic!("expected the available_commands_update, got {frame:?}");
        };
        assert_eq!(
            raw_params_to_value(update.params.clone())["update"]["sessionUpdate"],
            "available_commands_update"
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
