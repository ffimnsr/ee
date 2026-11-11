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
    observer: Option<crate::events::EventObserver>,
) -> Result<Arc<OrchestratorRuntime>, ProviderError> {
    if checkpoint.provider != implementation_name {
        return Err(ProviderError::BackendFailure(format!(
            "checkpoint provider {:?} does not match {:?}; refusing restore",
            checkpoint.provider, implementation_name
        )));
    }
    let runtime =
        OrchestratorRuntime::from_checkpoint_with_model_registry(checkpoint, models, policy)
            .map_err(ProviderError::from)?;
    let runtime = match observer {
        Some(observer) => runtime.with_plugin_observer(observer),
        None => runtime,
    };
    Ok(Arc::new(runtime))
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

/// Normalizes the reported workspace roots: canonicalized where possible,
/// deduplicated, and sorted so the runtime snapshot is deterministic.
pub(super) fn normalize_workspace_roots(
    cwd: &std::path::Path,
    additional_directories: &[std::path::PathBuf],
) -> Vec<std::path::PathBuf> {
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    for directory in std::iter::once(cwd).chain(additional_directories.iter().map(|p| p.as_path()))
    {
        let root = std::fs::canonicalize(directory).unwrap_or_else(|_| directory.to_path_buf());
        if !roots.iter().any(|existing| existing == &root) {
            roots.push(root);
        }
    }
    roots.sort();
    roots
}

/// Provider and model labels for the deployment persona prefix.
pub(super) fn default_model_labels(
    models: &crate::model_registry::ModelRegistry,
) -> (String, String) {
    models
        .advertised()
        .into_iter()
        .find(|info| info.id == crate::model_registry::DEFAULT_MODEL_ID)
        .map_or_else(
            || ("ee".to_string(), crate::model_registry::DEFAULT_MODEL_ID.to_string()),
            |info| (info.identity.provider_id, info.identity.model_id),
        )
}

/// Inputs for one prompt-bundle build.
pub(super) struct PromptBundleInputs<'a> {
    pub(super) cwd: &'a std::path::Path,
    pub(super) workspace_roots: &'a [std::path::PathBuf],
    pub(super) provider: &'a str,
    pub(super) model: &'a str,
    pub(super) mode: &'a SessionModeId,
    pub(super) policy: &'a ToolPolicy,
    pub(super) advertised_tools: &'a [String],
    pub(super) mcp_servers: &'a [String],
    /// Test seam for trusted user instructions; `None` reads the process env.
    pub(super) global_config_root: Option<&'a std::path::Path>,
}

/// Builds the stable prompt and runtime snapshot for one session mode.
pub(super) fn prompt_bundle(inputs: PromptBundleInputs<'_>) -> Result<PromptBundle, ProviderError> {
    let mut bundle = crate::prompt::build(&PromptInputs {
        mode: inputs.mode,
        provider: inputs.provider,
        model: inputs.model,
        cwd: inputs.cwd,
        workspace_roots: inputs.workspace_roots,
        sandbox: SandboxMode::from_policy(inputs.policy),
        allow_execute: inputs.policy.allow_execute,
        approval: ApprovalMode::from_policy(inputs.policy),
        allow_delegate: inputs.policy.allow_delegate,
        agent: AgentIdentity {
            name: "root",
            depth: 0,
            max_depth: Some(inputs.policy.max_delegate_depth),
        },
        turn_guidance: None,
        advertised_tools: inputs.advertised_tools,
        mcp_servers: inputs.mcp_servers,
        preset: PromptPreset::Standard,
    })
    .map_err(|error| ProviderError::BackendFailure(format!("prompt assembly failed: {error}")))?;
    // Recompute every turn: instruction files are attack surface, so a stale
    // snapshot is never carried in history. `TurnSystemContext` replays only
    // the current snapshot ahead of the current prompt.
    bundle.global = match inputs.global_config_root {
        Some(root) => crate::prompt::global::discover_at(root),
        None => crate::prompt::global::discover(),
    };
    bundle.workspace = crate::prompt::workspace::discover(inputs.cwd, inputs.workspace_roots);
    Ok(bundle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust::TrustLevel;

    #[test]
    fn prompt_bundle_loads_workspace_instructions_as_untrusted() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("AGENTS.md"), "project rule").expect("write rule");
        let roots = vec![temp.path().to_path_buf()];
        let mode = SessionModeId::new("write");
        let bundle = prompt_bundle(PromptBundleInputs {
            cwd: temp.path(),
            workspace_roots: &roots,
            provider: "test",
            model: "test-model",
            mode: &mode,
            policy: &ToolPolicy::default(),
            advertised_tools: &[],
            mcp_servers: &[],
            global_config_root: None,
        })
        .expect("bundle");

        assert_eq!(bundle.workspace.len(), 1);
        let message = &bundle.workspace[0].message;
        assert_eq!(message.trust, TrustLevel::WorkspaceInstructionUntrusted);
        assert!(message.text_content().contains("project rule"));
        assert!(bundle.workspace[0].path.contains("AGENTS.md"));
    }

    #[test]
    fn prompt_bundle_loads_trusted_global_instructions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let global = temp.path().join("ee");
        std::fs::create_dir_all(&global).expect("global config dir");
        std::fs::write(global.join("AGENTS.md"), "global rule").expect("write global rule");
        std::fs::write(temp.path().join("AGENTS.md"), "workspace rule").expect("write rule");
        let roots = vec![temp.path().to_path_buf()];
        let mode = SessionModeId::new("write");
        let bundle = prompt_bundle(PromptBundleInputs {
            cwd: temp.path(),
            workspace_roots: &roots,
            provider: "test",
            model: "test-model",
            mode: &mode,
            policy: &ToolPolicy::default(),
            advertised_tools: &[],
            mcp_servers: &[],
            global_config_root: Some(&global),
        })
        .expect("bundle");

        assert_eq!(bundle.global.len(), 1);
        assert_eq!(bundle.global[0].message.trust, TrustLevel::UserInstructionTrusted);
        assert!(bundle.global[0].message.text_content().contains("global rule"));
        assert_eq!(bundle.workspace.len(), 1);
        assert_eq!(bundle.workspace[0].message.trust, TrustLevel::WorkspaceInstructionUntrusted);
        assert!(!bundle.stable.contains("global rule"), "global text never joins stable prompt");
        assert!(
            !bundle.stable.contains("workspace rule"),
            "workspace text never joins stable prompt"
        );
    }

    #[test]
    fn zero_delegate_depth_emits_the_cap_line() {
        let roots: Vec<std::path::PathBuf> = Vec::new();
        let mode = SessionModeId::new("write");
        let policy =
            ToolPolicy { allow_delegate: true, max_delegate_depth: 0, ..ToolPolicy::default() };
        let bundle = prompt_bundle(PromptBundleInputs {
            cwd: std::path::Path::new("/workspace"),
            workspace_roots: &roots,
            provider: "test",
            model: "test-model",
            mode: &mode,
            policy: &policy,
            advertised_tools: &[],
            mcp_servers: &[],
            global_config_root: None,
        })
        .expect("bundle");
        let runtime = bundle.runtime.expect("runtime context");
        assert!(
            runtime.text.contains("current 0, maximum 0, remaining levels 0"),
            "{}",
            runtime.text
        );
    }
}
