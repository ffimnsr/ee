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

/// Converts the retained normalized transcript into the history handed to the
/// next turn. Tool calls and tool results are preserved; host-derived final
/// reports never enter the model transcript. The transcript is kept bounded by
/// [`record_model_transcript`].
pub(super) fn model_history(transcript: &Arc<Mutex<Vec<ModelMessage>>>) -> Vec<ModelMessage> {
    transcript.lock().expect("model transcript poisoned").clone()
}

/// Replaces the retained transcript with the latest normalized turn transcript,
/// dropping rebuilt prompt planes and bounding the result to
/// [`CONVERSATION_MAX_MESSAGES`].
pub(super) fn record_model_transcript(
    transcript: &Arc<Mutex<Vec<ModelMessage>>>,
    messages: Vec<ModelMessage>,
) {
    let mut normalized = normalize_model_messages(messages);
    if normalized.len() > CONVERSATION_MAX_MESSAGES {
        let overflow = normalized.len() - CONVERSATION_MAX_MESSAGES;
        normalized.drain(..overflow);
    }
    *transcript.lock().expect("model transcript poisoned") = normalized;
}

/// History used for compaction: the append-only session log carries every
/// model-visible message, including turns that already aged out of the bounded
/// transcript. Falls back to the retained transcript when the log is empty.
pub(super) fn session_compaction_history(
    runtime: &OrchestratorRuntime,
    session_id: &str,
    transcript: &Arc<Mutex<Vec<ModelMessage>>>,
) -> Vec<ModelMessage> {
    let logged = runtime
        .replay_session_messages(session_id)
        .map(|records| normalize_model_messages(records.into_iter().map(|(_, message)| message)))
        .unwrap_or_default();
    if logged.is_empty() { model_history(transcript) } else { logged }
}

/// Restores the replay conversation and normalized model transcript from the
/// durable session log. Used by `session/load` when no in-memory session
/// survived (crash/restart or an imported session), so the loaded session can
/// continue from its recorded history.
pub(super) fn restore_planes_from_log(
    runtime: &OrchestratorRuntime,
    session_id: &str,
    conversation: &Arc<Mutex<Vec<ConversationMessage>>>,
    model_transcript: &Arc<Mutex<Vec<ModelMessage>>>,
) {
    let Ok(records) = runtime.replay_session_messages(session_id) else { return };
    if records.is_empty() {
        return;
    }
    let messages = normalize_model_messages(records.into_iter().map(|(_, message)| message));
    if messages.is_empty() {
        return;
    }
    let mut restored = Vec::new();
    for message in &messages {
        let text = message.text_content();
        if text.trim().is_empty() {
            continue;
        }
        let role = match message.role {
            ModelRole::User => ConversationRole::User,
            ModelRole::Assistant | ModelRole::Subagent => ConversationRole::Agent,
            _ => continue,
        };
        restored.push(ConversationMessage { role, text });
    }
    if restored.len() > CONVERSATION_MAX_MESSAGES {
        let overflow = restored.len() - CONVERSATION_MAX_MESSAGES;
        restored.drain(..overflow);
    }
    *conversation.lock().expect("conversation poisoned") = restored;
    record_model_transcript(model_transcript, messages);
}

fn normalize_model_messages(messages: impl IntoIterator<Item = ModelMessage>) -> Vec<ModelMessage> {
    messages
        .into_iter()
        .filter(|message| {
            message.role != ModelRole::System && !message.metadata.contains_key("ee_prompt")
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
                    format!("{FINAL_RESPONSE_MESSAGE_ID_PREFIX}replay-{}", index + 1)
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
    Ok(runtime.into_shared())
}

/// Initial slash commands for orchestrated sessions. `/rubber-duck` is
/// advertised because this provider owns the verified critic flow; a registry
/// without contrast still returns a typed unavailable reason.
pub(super) fn initial_commands(recovery_enabled: bool) -> Vec<ee_agent_protocol::AvailableCommand> {
    let mut commands = vec![
        compact_available_command(),
        primary_agent_available_command(),
        rubber_duck_available_command(),
        reflect_available_command(),
        harness_available_command(),
        branch_available_command(),
        revert_available_command(),
        diff_available_command(),
        export_session_available_command(),
        import_session_available_command(),
    ];
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
    PersistedSession {
        mode: session.mode.clone(),
        model_id: session.runtime.selected_model(),
        agent: session.runtime.active_user_agent_name(),
    }
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

#[cfg(test)]
mod tests {
    use super::super::prompt_bundle::{PromptBundleInputs, prompt_bundle};
    use super::*;
    use crate::trust::TrustLevel;

    #[test]
    fn prompt_bundle_injects_workspace_agent_as_untrusted_instructions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let roots = vec![temp.path().to_path_buf()];
        let mode = SessionModeId::new("write");
        let agent = crate::user_agents::UserAgentDefinition {
            name: "reviewer".into(),
            description: "Reviewer".into(),
            model: None,
            variant: None,
            mode: crate::user_agents::UserAgentMode::Primary,
            hidden: false,
            steps: None,
            prompt: "Review the change and cite files.".into(),
            permissions: None,
            scope: crate::user_agents::UserAgentScope::Workspace,
            path: temp.path().join(".agents/agents/reviewer.md"),
        };
        let bundle = prompt_bundle(PromptBundleInputs {
            cwd: temp.path(),
            workspace_roots: &roots,
            harness_source: crate::harness_source::HarnessSource::Workspace,
            provider: "test",
            model: "test-model",
            mode: &mode,
            policy: &ToolPolicy::default(),
            advertised_tools: &[],
            mcp_servers: &[],
            global_config_root: None,
            background_jobs: None,
            remote_instructions: &[],
            user_agent: Some(&agent),
        })
        .expect("bundle");

        let injected = bundle
            .workspace
            .iter()
            .find(|message| message.message.metadata.contains_key("ee_agent"))
            .expect("agent prompt injected");
        assert_eq!(injected.message.trust, TrustLevel::WorkspaceInstructionUntrusted);
        assert!(injected.message.text_content().contains("Review the change"));
    }

    #[test]
    fn prompt_bundle_carries_guardrails_with_scope_trust() {
        let temp = tempfile::tempdir().expect("tempdir");
        let guardrails = temp.path().join(".agents/guardrails.md");
        std::fs::create_dir_all(guardrails.parent().expect("parent")).expect("guardrails dir");
        std::fs::write(&guardrails, "Never force-push.").expect("write guardrails");
        let roots = vec![temp.path().to_path_buf()];
        let mode = SessionModeId::new("write");
        let bundle = prompt_bundle(PromptBundleInputs {
            cwd: temp.path(),
            workspace_roots: &roots,
            harness_source: crate::harness_source::HarnessSource::Workspace,
            provider: "test",
            model: "test-model",
            mode: &mode,
            policy: &ToolPolicy::default(),
            advertised_tools: &[],
            mcp_servers: &[],
            global_config_root: None,
            background_jobs: None,
            remote_instructions: &[],
            user_agent: None,
        })
        .expect("bundle");

        let message = bundle
            .workspace
            .iter()
            .find(|message| message.message.metadata.contains_key("ee_guardrails_scope"))
            .expect("guardrails section injected");
        assert_eq!(message.message.trust, TrustLevel::WorkspaceInstructionUntrusted);
        assert!(message.message.text_content().contains("Never force-push."));
        assert_eq!(message.path, guardrails.display().to_string());
    }

    #[test]
    fn prompt_bundle_applies_tool_guidance_override() {
        let temp = tempfile::tempdir().expect("tempdir");
        let override_file = temp.path().join(".agents/tool_guidance.md");
        std::fs::create_dir_all(override_file.parent().expect("parent")).expect("override dir");
        std::fs::write(&override_file, "## write_file\nWrite one whole file at a time.\n")
            .expect("write override");
        let roots = vec![temp.path().to_path_buf()];
        let tools = vec!["read_file".to_string(), "write_file".to_string()];
        let mode = SessionModeId::new("write");
        let bundle = prompt_bundle(PromptBundleInputs {
            cwd: temp.path(),
            workspace_roots: &roots,
            harness_source: crate::harness_source::HarnessSource::Workspace,
            provider: "test",
            model: "test-model",
            mode: &mode,
            policy: &ToolPolicy::default(),
            advertised_tools: &tools,
            mcp_servers: &[],
            global_config_root: None,
            background_jobs: None,
            remote_instructions: &[],
            user_agent: None,
        })
        .expect("bundle");

        assert!(
            !bundle.stable.contains("`write_file` replaces the whole file"),
            "the override replaces the builtin write guidance"
        );
        assert!(bundle.stable.contains("Use `read_file`"), "other guidance stays intact");
        let message = bundle
            .workspace
            .iter()
            .find(|message| message.message.metadata.contains_key("ee_tool_guidance_scope"))
            .expect("override section injected");
        assert_eq!(message.message.trust, TrustLevel::WorkspaceInstructionUntrusted);
        assert!(
            message.message.text_content().contains("Write one whole file at a time."),
            "override body reaches the prompt"
        );
    }

    #[test]
    fn prompt_bundle_catalogs_scripts_for_terminal_sessions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let script = temp.path().join(".agents/scripts/report.py");
        std::fs::create_dir_all(script.parent().expect("parent")).expect("scripts dir");
        std::fs::write(&script, "print('ok')\n").expect("write script");
        let roots = vec![temp.path().to_path_buf()];
        let tools = vec!["create_terminal".to_string()];
        let mode = SessionModeId::new("write");
        let bundle = prompt_bundle(PromptBundleInputs {
            cwd: temp.path(),
            workspace_roots: &roots,
            harness_source: crate::harness_source::HarnessSource::Workspace,
            provider: "test",
            model: "test-model",
            mode: &mode,
            policy: &ToolPolicy::default(),
            advertised_tools: &tools,
            mcp_servers: &[],
            global_config_root: None,
            background_jobs: None,
            remote_instructions: &[],
            user_agent: None,
        })
        .expect("bundle");

        let message = bundle
            .workspace
            .iter()
            .find(|message| message.message.metadata.contains_key("ee_scripts_scope"))
            .expect("script catalog injected");
        assert_eq!(message.message.trust, TrustLevel::WorkspaceInstructionUntrusted);
        assert!(message.message.text_content().contains(".agents/scripts/report.py"));

        let read_only = vec!["read_file".to_string()];
        let bundle = prompt_bundle(PromptBundleInputs {
            cwd: temp.path(),
            workspace_roots: &roots,
            harness_source: crate::harness_source::HarnessSource::Workspace,
            provider: "test",
            model: "test-model",
            mode: &mode,
            policy: &ToolPolicy::default(),
            advertised_tools: &read_only,
            mcp_servers: &[],
            global_config_root: None,
            background_jobs: None,
            remote_instructions: &[],
            user_agent: None,
        })
        .expect("bundle");
        assert!(
            bundle
                .workspace
                .iter()
                .all(|message| !message.message.metadata.contains_key("ee_scripts_scope")),
            "no terminal tool means no script catalog"
        );
    }

    #[test]
    fn prompt_bundle_injects_a_bounded_experience_catalog() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join(".agents/experience/rust/entries/1-task");
        std::fs::create_dir_all(&dir).expect("experience dir");
        std::fs::write(
            dir.join("EXPERIENCE.md"),
            "---\nseq: 1\ndomain: rust\ntask: task\nconfidence: medium\n---\n## Edits\ncomponent: tool_guidance.md\nchange: added rule\nobserved: contrast\nverdict: helped\n## Experience\ncue: reopen written outputs\nlesson: add verify-after-write\nevidence: c2 vs base\n",
        )
        .expect("write experience");
        let roots = vec![temp.path().to_path_buf()];
        let mode = SessionModeId::new("write");
        let bundle = prompt_bundle(PromptBundleInputs {
            cwd: temp.path(),
            workspace_roots: &roots,
            harness_source: crate::harness_source::HarnessSource::Workspace,
            provider: "test",
            model: "test-model",
            mode: &mode,
            policy: &ToolPolicy::default(),
            advertised_tools: &[],
            mcp_servers: &[],
            global_config_root: None,
            background_jobs: None,
            remote_instructions: &[],
            user_agent: None,
        })
        .expect("bundle");

        let catalog = bundle
            .workspace
            .iter()
            .find(|message| message.message.metadata.contains_key("ee_experience_scope"))
            .expect("experience catalog injected");
        assert_eq!(catalog.message.trust, TrustLevel::WorkspaceInstructionUntrusted);
        let text = catalog.message.text_content();
        assert!(text.contains("reopen written outputs"));
        assert!(text.contains("load_experience"));
    }

    #[test]
    fn prompt_bundle_renders_an_override_harness_source() {
        let workspace = tempfile::tempdir().expect("workspace");
        let candidate = tempfile::tempdir().expect("candidate");
        let agents = candidate.path().join(".agents");
        std::fs::create_dir_all(agents.join("skills/atlas")).expect("candidate dirs");
        std::fs::write(agents.join("guardrails.md"), "Candidate rule.\n").expect("guardrails");
        std::fs::write(
            agents.join("skills/atlas/SKILL.md"),
            "---\nname: atlas\ndescription: graph lookup\n---\nUse the graph.\n",
        )
        .expect("skill");
        let roots = vec![workspace.path().to_path_buf()];
        let mode = SessionModeId::new("write");
        let inputs = PromptBundleInputs {
            cwd: workspace.path(),
            workspace_roots: &roots,
            provider: "test",
            model: "test-model",
            mode: &mode,
            policy: &ToolPolicy::default(),
            advertised_tools: &[],
            mcp_servers: &[],
            global_config_root: None,
            background_jobs: None,
            remote_instructions: &[],
            user_agent: None,
            harness_source: crate::harness_source::HarnessSource::Workspace,
        };
        let workspace_bundle = prompt_bundle(inputs).expect("workspace bundle");

        let candidate_bundle = prompt_bundle(PromptBundleInputs {
            cwd: workspace.path(),
            workspace_roots: &roots,
            provider: "test",
            model: "test-model",
            mode: &mode,
            policy: &ToolPolicy::default(),
            advertised_tools: &[],
            mcp_servers: &[],
            global_config_root: None,
            background_jobs: None,
            remote_instructions: &[],
            user_agent: None,
            harness_source: crate::harness_source::HarnessSource::Override(
                candidate.path().to_path_buf(),
            ),
        })
        .expect("candidate bundle");

        assert!(candidate_bundle.workspace.iter().any(|message| {
            message.message.metadata.contains_key("ee_guardrails_scope")
                && message.message.text_content().contains("Candidate rule.")
        }));
        assert!(
            candidate_bundle
                .workspace
                .iter()
                .any(|message| message.message.text_content().contains("graph lookup"))
        );
        assert!(
            workspace_bundle.workspace.is_empty(),
            "the workspace session is unchanged by the candidate files"
        );
        assert_eq!(workspace_bundle.stable, candidate_bundle.stable);
    }

    #[test]
    fn prompt_bundle_loads_workspace_instructions_as_untrusted() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("AGENTS.md"), "project rule").expect("write rule");
        let roots = vec![temp.path().to_path_buf()];
        let mode = SessionModeId::new("write");
        let bundle = prompt_bundle(PromptBundleInputs {
            cwd: temp.path(),
            workspace_roots: &roots,
            harness_source: crate::harness_source::HarnessSource::Workspace,
            provider: "test",
            model: "test-model",
            mode: &mode,
            policy: &ToolPolicy::default(),
            advertised_tools: &[],
            mcp_servers: &[],
            global_config_root: None,
            background_jobs: None,
            remote_instructions: &[],
            user_agent: None,
        })
        .expect("bundle");

        assert_eq!(bundle.workspace.len(), 1);
        let message = &bundle.workspace[0].message;
        assert_eq!(message.trust, TrustLevel::WorkspaceInstructionUntrusted);
        assert!(message.text_content().contains("project rule"));
        assert!(bundle.workspace[0].path.contains("AGENTS.md"));
    }

    #[test]
    fn prompt_bundle_injects_a_bounded_memory_catalog() {
        let temp = tempfile::tempdir().expect("tempdir");
        let memory = temp.path().join(".agents/memories/testing.md");
        std::fs::create_dir_all(memory.parent().expect("parent")).expect("memory dir");
        std::fs::write(
            &memory,
            "---\ntags: [rust]\nimportance: critical\n---\nRun tests with --quiet.\n",
        )
        .expect("write memory");
        let roots = vec![temp.path().to_path_buf()];
        let mode = SessionModeId::new("write");
        let bundle = prompt_bundle(PromptBundleInputs {
            cwd: temp.path(),
            workspace_roots: &roots,
            harness_source: crate::harness_source::HarnessSource::Workspace,
            provider: "test",
            model: "test-model",
            mode: &mode,
            policy: &ToolPolicy::default(),
            advertised_tools: &[],
            mcp_servers: &[],
            global_config_root: None,
            background_jobs: None,
            remote_instructions: &[],
            user_agent: None,
        })
        .expect("bundle");

        let catalog = bundle
            .workspace
            .iter()
            .find(|message| message.message.metadata.contains_key("ee_prompt"))
            .expect("memory catalog injected");
        assert_eq!(catalog.message.trust, TrustLevel::WorkspaceInstructionUntrusted);
        let text = catalog.message.text_content();
        assert!(text.contains("testing"));
        assert!(text.contains("critical"));
        assert!(text.contains("rust"));
        assert!(text.contains("load_memory"));
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
            harness_source: crate::harness_source::HarnessSource::Workspace,
            provider: "test",
            model: "test-model",
            mode: &mode,
            policy: &ToolPolicy::default(),
            advertised_tools: &[],
            mcp_servers: &[],
            global_config_root: Some(&global),
            background_jobs: None,
            remote_instructions: &[],
            user_agent: None,
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
    fn model_transcript_preserves_tool_provenance_and_drops_prompt_planes() {
        let transcript = Arc::new(Mutex::new(Vec::new()));
        let messages = vec![
            ModelMessage::text(ModelRole::System, "stable prompt"),
            ModelMessage::text(ModelRole::User, "hello")
                .with_metadata([(String::from("ee_prompt"), String::from("runtime_context"))]),
            ModelMessage::text(ModelRole::User, "fix it"),
            ModelMessage::text(ModelRole::Assistant, "checking"),
            ModelMessage::tool_result("call-1", crate::tools::ToolResult::success("file body")),
        ];

        record_model_transcript(&transcript, messages);
        let history = model_history(&transcript);

        assert_eq!(history.len(), 3, "system and rebuilt prompt planes are dropped");
        assert_eq!(history[0].text_content(), "fix it");
        assert_eq!(history[1].role, ModelRole::Assistant);
        assert_eq!(history[2].role, ModelRole::Tool, "tool provenance is retained");
    }

    #[test]
    fn model_transcript_remains_bounded() {
        let transcript = Arc::new(Mutex::new(Vec::new()));
        let messages = (0..CONVERSATION_MAX_MESSAGES + 10)
            .map(|index| ModelMessage::text(ModelRole::User, format!("message {index}")))
            .collect();

        record_model_transcript(&transcript, messages);
        let history = model_history(&transcript);

        assert_eq!(history.len(), CONVERSATION_MAX_MESSAGES);
        assert_eq!(history[0].text_content(), "message 10", "oldest messages age out first");
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
            harness_source: crate::harness_source::HarnessSource::Workspace,
            provider: "test",
            model: "test-model",
            mode: &mode,
            policy: &policy,
            advertised_tools: &[],
            mcp_servers: &[],
            global_config_root: None,
            background_jobs: None,
            remote_instructions: &[],
            user_agent: None,
        })
        .expect("bundle");
        let runtime = bundle.runtime.expect("runtime context");
        assert!(
            runtime.text.contains("current 0, maximum 0, remaining levels 0"),
            "{}",
            runtime.text
        );
    }

    #[test]
    fn background_jobs_render_as_a_hashed_runtime_section() {
        let roots: Vec<std::path::PathBuf> = Vec::new();
        let mode = SessionModeId::new("write");
        let bundle = prompt_bundle(PromptBundleInputs {
            cwd: std::path::Path::new("/workspace"),
            workspace_roots: &roots,
            harness_source: crate::harness_source::HarnessSource::Workspace,
            provider: "test",
            model: "test-model",
            mode: &mode,
            policy: &ToolPolicy::default(),
            advertised_tools: &[],
            mcp_servers: &[],
            global_config_root: None,
            background_jobs: Some("- terminal term-1 [running]: cargo build"),
            remote_instructions: &[],
            user_agent: None,
        })
        .expect("bundle");
        let runtime = bundle.runtime.expect("runtime context");
        let section = runtime
            .sections
            .iter()
            .find(|section| section.name == "BACKGROUND_JOBS")
            .expect("background jobs section");
        assert!(section.text.contains("cargo build"));
        assert!(!section.hash.is_empty());
    }
}
