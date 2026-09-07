//! Session conversation, MCP helpers, snapshots, and workspace context.
use super::*;

/// Validates `session/new` MCP server entries into redacted descriptors,
/// fail closed on unsupported transports.
pub(super) fn validate_mcp_servers(
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
pub(super) fn emit_mcp_diagnostics(
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
pub(super) struct McpTurnCleanup {
    pub(super) runtime: Arc<OrchestratorRuntime>,
    pub(super) manager: Option<Arc<McpSessionManager>>,
    pub(super) registered: Vec<String>,
}

impl McpTurnCleanup {
    pub(super) async fn finish(mut self) {
        self.cleanup().await;
    }

    pub(super) async fn cleanup(&mut self) {
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
pub(super) fn record_user_message(
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

/// Appends streamed agent text to one replay message. Providers may emit tiny
/// deltas (even one character each); persisting each delta as a distinct ACP
/// message makes restored TUI history render one row per fragment.
pub(super) fn record_agent_message(
    conversation: &Arc<Mutex<Vec<ConversationMessage>>>,
    text: impl AsRef<str>,
) {
    let text = text.as_ref();
    if text.is_empty() {
        return;
    }
    let mut log = conversation.lock().expect("conversation poisoned");
    if let Some(last) = log.last_mut()
        && last.role == ConversationRole::Agent
    {
        last.text.push_str(text);
        return;
    }
    log.push(ConversationMessage { role: ConversationRole::Agent, text: text.to_string() });
    if log.len() > CONVERSATION_MAX_MESSAGES {
        let overflow = log.len() - CONVERSATION_MAX_MESSAGES;
        log.drain(..overflow);
    }
}

/// Converts client-visible live-session messages into normalized model history.
/// Host-derived final reports stay excluded because they are not model output.
pub(super) fn model_history(
    conversation: &Arc<Mutex<Vec<ConversationMessage>>>,
) -> Vec<ModelMessage> {
    conversation
        .lock()
        .expect("conversation poisoned")
        .iter()
        .filter_map(|message| match message.role {
            ConversationRole::User => {
                Some(ModelMessage::text(ModelRole::User, message.text.clone()))
            }
            ConversationRole::Agent => {
                Some(ModelMessage::text(ModelRole::Assistant, message.text.clone()))
            }
            ConversationRole::FinalResponse => None,
        })
        .collect()
}

/// Records the host-derived final response as its own replay item. It must
/// never merge into preceding model text, because that would obscure which
/// claims were evidence-gated.
pub(super) fn record_final_response(
    conversation: &Arc<Mutex<Vec<ConversationMessage>>>,
    text: impl Into<String>,
) {
    let mut log = conversation.lock().expect("conversation poisoned");
    log.push(ConversationMessage { role: ConversationRole::FinalResponse, text: text.into() });
    if log.len() > CONVERSATION_MAX_MESSAGES {
        let overflow = log.len() - CONVERSATION_MAX_MESSAGES;
        log.drain(..overflow);
    }
}

/// Streams a recorded conversation to the client as `user_message_chunk` /
/// `agent_message_chunk` updates, in order, with deterministic replay ids
/// (ACP v1 `session/load` conversation replay).  With no sink (or nothing
/// recorded) this is a no-op.
pub(super) fn replay_conversation(
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
            ConversationRole::Agent | ConversationRole::FinalResponse => {
                let message_id = if message.role == ConversationRole::FinalResponse {
                    format!("ee-final-response-replay-{}", index + 1)
                } else {
                    format!("replay-a-{}", index + 1)
                };
                sink.agent_message_chunk(message_id, message.text.clone()).map_err(|error| {
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
pub(super) fn restore_runtime_from_checkpoint(
    checkpoint: &crate::checkpoint::OrchestratorCheckpoint,
    implementation_name: &str,
    models: Arc<crate::model_registry::ModelRegistry>,
    policy: PolicyEngine,
) -> Result<Arc<OrchestratorRuntime>, ProviderError> {
    if checkpoint.provider != implementation_name {
        return Err(ProviderError::BackendFailure(format!(
            "checkpoint provider {:?} does not match {:?}; refusing restore",
            checkpoint.provider, implementation_name
        )));
    }
    OrchestratorRuntime::from_checkpoint_with_model_registry(checkpoint, models, policy)
        .map(Arc::new)
        .map_err(ProviderError::from)
}

/// Initial slash commands for orchestrated sessions. `/rubber-duck` is
/// advertised because this provider owns the verified critic flow; a registry
/// without contrast still returns a typed unavailable reason.
pub(super) fn initial_commands(recovery_enabled: bool) -> Vec<ee_agent_protocol::AvailableCommand> {
    let mut commands = vec![compact_available_command(), rubber_duck_available_command()];
    if recovery_enabled {
        commands.push(discard_available_command());
        commands.push(resume_available_command());
    }
    commands
}

/// Next session number: the in-process counter, raised past every session id
/// that survives in normal-session storage or the recovery checkpoint store.
/// This prevents a restarted provider from shadowing `session-1`.
pub(super) fn next_session_number(
    next_session: &AtomicU64,
    recovery: &crate::config::RecoveryConfig,
    durable_session_ids: Vec<String>,
) -> u64 {
    let base = CheckpointStore::new(recovery)
        .session_ids()
        .into_iter()
        .chain(durable_session_ids)
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

pub(super) fn session_snapshot(session: &SessionRuntime) -> PersistedSession {
    PersistedSession { mode: session.mode.clone() }
}

pub(super) fn persist_session_snapshot(
    store: &SessionStateStore,
    implementation_name: &str,
    session_id: &SessionId,
    sessions: &Arc<Mutex<HashMap<String, SessionRuntime>>>,
) -> Result<(), ProviderError> {
    let (workspace, state) = {
        let sessions = sessions.lock().expect("adapter sessions poisoned");
        let session = sessions.get(&session_id.to_string()).ok_or_else(|| {
            ProviderError::BackendFailure(format!("no orchestrator state for session {session_id}"))
        })?;
        (session.workspace.clone(), session_snapshot(session))
    };
    store
        .save(implementation_name, &workspace, &session_id.to_string(), &state)
        .map_err(ProviderError::from)
}

pub(super) fn workspace_system_context(
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
