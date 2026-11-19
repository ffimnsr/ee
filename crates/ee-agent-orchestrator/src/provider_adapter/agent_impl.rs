//! `impl AgentProvider for OrchestratorProvider`: ACP session surface.
use super::conversation::McpTurnCleanup;
use super::conversation::default_model_labels;
use super::conversation::emit_mcp_diagnostics;
use super::conversation::initial_commands;
use super::conversation::model_history;
use super::conversation::next_session_number;
use super::conversation::normalize_workspace_roots;
use super::conversation::persist_session_snapshot;
use super::conversation::record_agent_message;
use super::conversation::record_final_response;
use super::conversation::record_model_transcript;
use super::conversation::record_user_message;
use super::conversation::replay_conversation;
use super::conversation::restore_runtime_from_checkpoint;
use super::conversation::session_compaction_history;
use super::conversation::session_snapshot;
use super::conversation::validate_mcp_servers;
use super::conversation::{RuntimePromptInputs, runtime_prompt_bundle};
use super::session_identity;
use super::*;

impl AgentProvider for OrchestratorProvider {
    fn info(&self) -> Implementation {
        self.config.implementation.clone()
    }

    fn capabilities(&self) -> AgentCapabilities {
        bootstrap::capabilities(&self.config)
    }

    fn new_session(
        &self,
        ctx: NewSessionContext,
    ) -> ProviderFuture<Result<SessionInit, ProviderError>> {
        let config = self.config.orchestrator.clone();
        let models = self.models.clone();
        let policy = self.policy.clone();
        let sessions = self.sessions.clone();
        let session_store = self.session_store.clone();
        let implementation = self.config.implementation.clone();
        let next_session = self.next_session.clone();
        let recovery_enabled = config.recovery.enabled;
        let plugins = self.plugins.clone();
        Box::pin(async move {
            // Monotonic id per process, raised past ids that survive in the
            // durable stores so a reconnected session is never shadowed by a
            // fresh one after a restart.
            let durable_session_ids = session_store
                .session_ids(&implementation.name, &ctx.cwd)
                .map_err(ProviderError::from)?;
            let number = next_session_number(&next_session, &config.recovery, durable_session_ids);
            let session_id = SessionId::new(format!("{SESSION_ID_PREFIX}-{number}"));
            let workspace_roots = normalize_workspace_roots(&ctx.cwd, &ctx.additional_directories);
            let (provider_label, model_label) = default_model_labels(&models);
            let mode = default_session_mode();
            let mcp_servers = validate_mcp_servers(&ctx.mcp_servers)?;
            let runtime = OrchestratorRuntime::with_shared_model_registry(
                config,
                models,
                plugins.gate_policy(mode_policy(&policy, &mode)?),
            )?
            .with_plugin_observer(plugins.bridge().observer(session_id.to_string()))
            .into_shared();
            session_identity::register(
                &runtime,
                &session_id,
                &ctx.cwd,
                &implementation.name,
                None,
            )?;
            plugins.session_scope(&session_id.to_string()).map_err(ProviderError::from)?;
            let entry = SessionRuntime::new(
                runtime,
                ctx.cwd.clone(),
                workspace_roots,
                (provider_label, model_label),
                mode.clone(),
                mcp_servers,
            );
            session_store
                .save(
                    &implementation.name,
                    &entry.workspace,
                    &session_id.to_string(),
                    &session_snapshot(&entry),
                )
                .map_err(ProviderError::from)?;
            sessions
                .lock()
                .expect("adapter sessions poisoned")
                .insert(session_id.to_string(), entry);
            Ok(SessionInit::new(session_id)
                .commands(initial_commands(recovery_enabled))
                .modes(session_modes(mode)))
        })
    }

    fn load_session(
        &self,
        ctx: LoadSessionContext,
    ) -> ProviderFuture<Result<SessionInit, ProviderError>> {
        let config = self.config.orchestrator.clone();
        let implementation = self.config.implementation.clone();
        let models = self.models.clone();
        let policy = self.policy.clone();
        let sessions = self.sessions.clone();
        let persisted = self.persisted.clone();
        let session_store = self.session_store.clone();
        let session_id = ctx.session_id.clone();
        let recovery_enabled = config.recovery.enabled;
        let plugins = self.plugins.clone();
        Box::pin(async move {
            let workspace_roots = normalize_workspace_roots(&ctx.cwd, &ctx.additional_directories);
            let (mut provider_label, mut model_label) = default_model_labels(&models);
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
                return Ok(SessionInit::new(session_id)
                    .commands(initial_commands(recovery_enabled))
                    .modes(session_modes(entry.mode.clone())));
            }
            let state = persisted
                .lock()
                .expect("adapter persisted poisoned")
                .remove(&session_id.to_string());
            let state = match state {
                Some(state) => Some(state),
                None => session_store
                    .load(&implementation.name, &ctx.cwd, &session_id.to_string())
                    .map_err(ProviderError::from)?,
            };
            let durable_load = state.is_some();
            let model_override = state.as_ref().and_then(|state| state.model_id.clone());
            let agent_override = state.as_ref().and_then(|state| state.agent.clone());
            let (runtime, conversation, mode) = match state {
                Some(state) => {
                    let mode = state.mode;
                    (
                        OrchestratorRuntime::with_shared_model_registry(
                            config,
                            models.clone(),
                            plugins.gate_policy(mode_policy(&policy, &mode)?),
                        )?
                        .with_plugin_observer(plugins.bridge().observer(session_id.to_string()))
                        .into_shared(),
                        Vec::new(),
                        mode,
                    )
                }
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
                        let detail = if config.recovery.enabled && !config.recovery.is_durable() {
                            "recovery is memory-only; provider restart cannot be resumed without EE_CHECKPOINT_DIR"
                        } else {
                            "no persisted orchestrator state for this session"
                        };
                        return Err(ProviderError::BackendFailure(format!(
                            "{detail}: {session_id}"
                        )));
                    };
                    // The pending checkpoint stays until the resumed turn
                    // completes or the client discards it.
                    let mode = default_session_mode();
                    let runtime = restore_runtime_from_checkpoint(
                        &checkpoint,
                        &implementation.name,
                        models.clone(),
                        plugins.gate_policy(mode_policy(&policy, &mode)?),
                        Some(plugins.bridge().observer(session_id.to_string())),
                    )?;
                    // Durable checkpoints contain no transcript content, so
                    // crash restore never replays user or model text.
                    (runtime, Vec::new(), mode)
                }
            };
            if let Some(model_id) = model_override {
                // A persisted switch survives reconnect only while the target
                // is still registered; a removed model falls back to default
                // instead of making the session unloadable.
                if models.contains(&model_id) {
                    if let Some(info) = models.info(&model_id) {
                        provider_label = info.identity.provider_id.clone();
                        model_label = model_id.clone();
                    }
                    runtime.set_selected_model(Some(model_id));
                } else {
                    tracing::warn!(
                        %session_id,
                        model_id,
                        "persisted session model is no longer registered; using the default model"
                    );
                }
            }
            if let Some(agent) = agent_override {
                let config_root = crate::prompt::skills::user_config_root_from_env();
                runtime.reload_user_agents(&ctx.cwd, &workspace_roots, config_root.as_deref());
                if let Err(reason) = runtime.restore_user_agent(&agent) {
                    tracing::warn!(
                        %session_id,
                        agent,
                        %reason,
                        "persisted user agent could not be restored; continuing without it"
                    );
                }
            }
            session_identity::register(
                &runtime,
                &session_id,
                &ctx.cwd,
                &implementation.name,
                None,
            )?;
            replay_conversation(ctx.replay_sink.as_ref(), &conversation)?;
            plugins.session_scope(&session_id.to_string()).map_err(ProviderError::from)?;
            // The checkpoint stays pending: the next prompt resumes it.
            let entry = SessionRuntime::new(
                runtime,
                ctx.cwd.clone(),
                workspace_roots,
                (provider_label, model_label),
                mode.clone(),
                mcp_servers,
            );
            if durable_load {
                super::conversation::restore_planes_from_log(
                    &entry.runtime,
                    &session_id.to_string(),
                    &entry.conversation,
                    &entry.model_transcript,
                );
                replay_conversation(
                    ctx.replay_sink.as_ref(),
                    &entry.conversation.lock().expect("conversation poisoned"),
                )?;
            }
            sessions
                .lock()
                .expect("adapter sessions poisoned")
                .insert(session_id.to_string(), entry);
            Ok(SessionInit::new(session_id)
                .commands(initial_commands(recovery_enabled))
                .modes(session_modes(mode)))
        })
    }

    fn resume_session(
        &self,
        ctx: LoadSessionContext,
    ) -> ProviderFuture<Result<SessionInit, ProviderError>> {
        let config = self.config.orchestrator.clone();
        let implementation = self.config.implementation.clone();
        let models = self.models.clone();
        let policy = self.policy.clone();
        let sessions = self.sessions.clone();
        let session_id = ctx.session_id.clone();
        let recovery_enabled = config.recovery.enabled;
        let recovery_durable = config.recovery.is_durable();
        let plugins = self.plugins.clone();
        Box::pin(async move {
            if !recovery_durable {
                return Err(ProviderError::BackendFailure(
                    "recovery is memory-only; provider restart cannot be resumed without EE_CHECKPOINT_DIR"
                        .to_string(),
                ));
            }
            // `session/resume` restores context with NO replay (ACP v1): the
            // interrupted turn is continued by the next `session/prompt` via
            // the pending-checkpoint detection.  A live session (same
            // process) is reused as-is.
            let workspace_roots = normalize_workspace_roots(&ctx.cwd, &ctx.additional_directories);
            let (provider_label, model_label) = default_model_labels(&models);
            let mcp_servers = validate_mcp_servers(&ctx.mcp_servers)?;
            let live_mode = {
                let mut sessions = sessions.lock().expect("adapter sessions poisoned");
                if let Some(entry) = sessions.get_mut(&session_id.to_string()) {
                    entry.mcp_servers = mcp_servers.clone();
                    Some(entry.mode.clone())
                } else {
                    None
                }
            };
            let mode = if let Some(mode) = live_mode {
                mode
            } else {
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
                let mode = default_session_mode();
                let runtime = restore_runtime_from_checkpoint(
                    &checkpoint,
                    &implementation.name,
                    models.clone(),
                    plugins.gate_policy(mode_policy(&policy, &mode)?),
                    Some(plugins.bridge().observer(session_id.to_string())),
                )?;
                session_identity::register(
                    &runtime,
                    &session_id,
                    &ctx.cwd,
                    &implementation.name,
                    None,
                )?;
                plugins.session_scope(&session_id.to_string()).map_err(ProviderError::from)?;
                // The checkpoint stays pending: the next prompt resumes it.
                sessions.lock().expect("adapter sessions poisoned").insert(
                    session_id.to_string(),
                    SessionRuntime::new(
                        runtime,
                        ctx.cwd.clone(),
                        workspace_roots,
                        (provider_label, model_label),
                        mode.clone(),
                        mcp_servers,
                    ),
                );
                mode
            };
            Ok(SessionInit::new(session_id)
                .commands(initial_commands(recovery_enabled))
                .modes(session_modes(mode)))
        })
    }

    fn prompt(
        &self,
        mut ctx: PromptContext,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
    ) -> ProviderFuture<Result<PromptResult, ProviderError>> {
        let session_id = ctx.session_id.clone();
        let sessions = self.sessions.clone();
        let session_store = self.session_store.clone();
        let mcp_policy = self.config.mcp.clone();
        let implementation_name = self.config.implementation.name.clone();
        let next_final_response = self.next_final_response.clone();
        let validation_workspace = self.config.validation_workspace.clone();
        let telemetry = self.telemetry.clone();
        let telemetry_attribution = self.config.telemetry_attribution.clone();
        let next_telemetry_turn = self.next_telemetry_turn.clone();
        let plugins = self.plugins.clone();
        let models = self.models.clone();
        let (instruction_sources, instruction_fetcher) =
            bootstrap::remote_instruction_inputs(&self.config);
        let base_policy = self.policy.clone();
        Box::pin(async move {
            let session = {
                let sessions = sessions.lock().expect("adapter sessions poisoned");
                sessions.get(&session_id.to_string()).map(|session| {
                    (
                        session.runtime.clone(),
                        session.workspace.clone(),
                        session.workspace_roots.clone(),
                        session.provider_label.clone(),
                        session.model_label.clone(),
                        session.mode.clone(),
                        session.mcp_servers.clone(),
                        session.conversation.clone(),
                        session.model_transcript.clone(),
                        session.remote_instructions.clone(),
                    )
                })
            };
            let Some((
                runtime,
                workspace,
                workspace_roots,
                provider_label,
                model_label,
                mode,
                mcp_servers,
                conversation,
                model_transcript,
                remote_cache,
            )) = session
            else {
                return Err(ProviderError::BackendFailure(format!(
                    "no orchestrator state for session {session_id}"
                )));
            };
            let telemetry_turn =
                format!("turn-{}", next_telemetry_turn.fetch_add(1, Ordering::Relaxed));
            // Mid-turn nested instruction discovery needs the session scope;
            // the adapter owns it, so record it before the turn starts. A
            // model-requested `session_move` outlives the turn, so it also
            // drives this prompt's cwd and workspace discovery.
            let workspace = runtime.session_workspace_override().unwrap_or(workspace);
            runtime.set_workspace_scope(workspace.clone(), workspace_roots.clone());
            // Plan mode advertises read-class tools only; the mode policy
            // already denies writes, so the model never sees a tool it cannot
            // call.
            runtime.set_read_only_tools(mode.to_string() == PLAN_MODE_ID);
            let telemetry_started_at = Instant::now();
            let telemetry_event_start = runtime.event_snapshot().len();
            let _ = telemetry
                .lock()
                .expect("telemetry recorder poisoned")
                .start_turn(&telemetry_turn, telemetry_attribution);
            let mcp_server_names: Vec<String> =
                mcp_servers.iter().map(|descriptor| descriptor.name.clone()).collect();
            let background_jobs = runtime.background_jobs_snapshot();
            let global_config_root = crate::prompt::skills::user_config_root_from_env();
            runtime.reload_user_agents(&workspace, &workspace_roots, global_config_root.as_deref());
            runtime.reload_user_commands(
                &workspace,
                &workspace_roots,
                global_config_root.as_deref(),
            );
            let prepared_prompt = crate::prompt_attachments::prepare_prompt(
                &mut ctx,
                &workspace,
                &workspace_roots,
                global_config_root.as_deref(),
                &runtime.config().prompt_attachments,
            )
            .map_err(ProviderError::InvalidRequest)?;
            let user_commands = runtime.user_commands_snapshot();
            // Refresh completion when the template set changed; builtins are
            // always included so the list replaces the previous session state.
            let mut command_names = initial_commands(runtime.config().recovery.enabled)
                .into_iter()
                .map(|command| command.name)
                .collect::<Vec<_>>();
            command_names.extend(user_commands.iter().map(|command| command.name.clone()));
            if runtime.commands_were_advertised() || !user_commands.is_empty() {
                let mut commands = initial_commands(runtime.config().recovery.enabled);
                commands.extend(user_commands.available_commands());
                if runtime.mark_commands_advertised(&command_names) {
                    sink.available_commands_replace(commands).map_err(|error| {
                        ProviderError::BackendFailure(format!(
                            "failed to advertise session commands: {error}"
                        ))
                    })?;
                }
            }
            let remote = bootstrap::remote_instructions(
                &instruction_sources,
                instruction_fetcher.as_ref(),
                &remote_cache,
            )
            .await;
            let mut bundle = runtime_prompt_bundle(
                &runtime,
                &models,
                RuntimePromptInputs {
                    cwd: &workspace,
                    workspace_roots: &workspace_roots,
                    mode: &mode,
                    mcp_servers: &mcp_server_names,
                    global_config_root: global_config_root.as_deref(),
                    background_jobs: background_jobs.as_deref(),
                    remote_instructions: &remote,
                },
            )?;
            bundle.workspace.extend(prepared_prompt.instructions.iter().cloned());
            // `/compact` is detected here, before any MCP bridging or tool
            // registration, so compaction turns never connect servers or
            // expose tools; the runtime still routes the turn to its
            // dedicated compaction path (no model–tool loop).
            let mut prompt_text = ctx
                .prompt
                .iter()
                .filter_map(|block| match block {
                    ee_agent_protocol::ContentBlock::Text(text) => Some(text.text.clone()),
                    ee_agent_protocol::ContentBlock::Resource(resource) => {
                        match &resource.resource {
                            ee_agent_protocol::EmbeddedResourceResource::TextResourceContents(
                                text,
                            ) => Some(text.text.clone()),
                            _ => None,
                        }
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ");
            let compact_command = parse_slash_command(&prompt_text)
                .filter(|command| command.name == COMPACT_COMMAND_NAME);
            if let Some(command) = compact_command {
                let history = session_compaction_history(
                    &runtime,
                    &session_id.to_string(),
                    &model_transcript,
                );
                let result = runtime
                    .run_compact_turn_with_history(
                        ctx,
                        sink,
                        cancel,
                        command.instructions,
                        Some(bundle.clone()),
                        history,
                    )
                    .await
                    .map_err(ProviderError::from);
                let events = runtime.event_snapshot();
                finish_provider_telemetry(
                    &telemetry,
                    ProviderTelemetryTurn {
                        turn_id: &telemetry_turn,
                        started_at: telemetry_started_at,
                        events: &events[telemetry_event_start..],
                        outcome: if result.is_ok() {
                            TelemetryTurnOutcome::Succeeded
                        } else {
                            TelemetryTurnOutcome::Failed
                        },
                        terminal_state: None,
                        evidence_ids: Vec::new(),
                        usage: result.as_ref().ok().and_then(|result| result.usage.as_ref()),
                    },
                );
                let result = result?;
                persist_session_snapshot(
                    &session_store,
                    &implementation_name,
                    &session_id,
                    &sessions,
                )?;
                return Ok(result);
            }
            // `/primary-agent` selects a user-defined primary agent. It runs
            // before MCP discovery and tool registration because the selection
            // changes the prompt and model for the next turn, not this one.
            if let Some(command) = parse_slash_command(&prompt_text)
                && command.name == PRIMARY_AGENT_COMMAND_NAME
            {
                return super::agent_command::handle_agent_command(
                    super::agent_command::AgentCommand {
                        runtime: &runtime,
                        sessions: &sessions,
                        session_store: &session_store,
                        implementation_name: &implementation_name,
                        models: &models,
                        sink: &sink,
                        session_id: &session_id,
                        instructions: command.instructions.as_deref(),
                        telemetry: &telemetry,
                        telemetry_turn: &telemetry_turn,
                        telemetry_started_at,
                        telemetry_event_start,
                    },
                );
            }
            // Manual critique is handled before MCP discovery and mutable tool
            // registration. Existing built-ins remain registered but critic
            // discovery/policy exposes read-only definitions only.
            if let Some(command) = parse_slash_command(&prompt_text)
                && command.name == RUBBER_DUCK_COMMAND_NAME
            {
                let history = model_history(&model_transcript);
                record_user_message(&conversation, prompt_text.clone());
                let turn = runtime
                    .run_manual_rubber_duck(
                        &session_id.to_string(),
                        command.instructions,
                        history,
                        sink,
                        cancel,
                    )
                    .await
                    .map_err(ProviderError::from)?;
                if let Some(synthesis) = &turn.synthesis {
                    record_agent_message(&conversation, synthesis);
                } else {
                    record_agent_message(&conversation, &turn.timeline_summary);
                }
                let events = runtime.event_snapshot();
                finish_provider_telemetry(
                    &telemetry,
                    ProviderTelemetryTurn {
                        turn_id: &telemetry_turn,
                        started_at: telemetry_started_at,
                        events: &events[telemetry_event_start..],
                        outcome: TelemetryTurnOutcome::Succeeded,
                        terminal_state: None,
                        evidence_ids: Vec::new(),
                        usage: None,
                    },
                );
                persist_session_snapshot(
                    &session_store,
                    &implementation_name,
                    &session_id,
                    &sessions,
                )?;
                return Ok(turn.prompt_result);
            }
            // `/diff` reports the files and patches one turn changed, using
            // the start/end workspace snapshots captured around it.
            if let Some(command) = parse_slash_command(&prompt_text)
                && command.name == DIFF_COMMAND_NAME
            {
                return super::snapshot_command::handle_diff_command(
                    super::snapshot_command::DiffCommand {
                        runtime: &runtime,
                        sink: &sink,
                        session_id: &session_id,
                        instructions: command.instructions.as_deref(),
                        workspace: &workspace,
                        telemetry: &telemetry,
                        telemetry_turn: &telemetry_turn,
                        telemetry_started_at,
                        telemetry_event_start,
                    },
                );
            }
            // `/export-session` and `/import-session` move a session between
            // machines. Export is redacted unless `--include-content`; import
            // seeds only an empty session so an existing log is never
            // overwritten.
            if let Some(command) = parse_slash_command(&prompt_text)
                && command.name == EXPORT_SESSION_COMMAND_NAME
            {
                return super::session_transfer_command::handle_export_command(
                    super::session_transfer_command::ExportCommand {
                        runtime: &runtime,
                        sink: &sink,
                        session_id: &session_id,
                        instructions: command.instructions.as_deref(),
                        info: crate::session_transfer::SessionTransferInfo {
                            provider: Some(provider_label.clone()),
                            model_id: Some(model_label.clone()),
                            mode: Some(mode.to_string()),
                            agent: runtime.active_user_agent_name(),
                            title: None,
                        },
                        telemetry: &telemetry,
                        telemetry_turn: &telemetry_turn,
                        telemetry_started_at,
                        telemetry_event_start,
                    },
                );
            }
            if let Some(command) = parse_slash_command(&prompt_text)
                && command.name == IMPORT_SESSION_COMMAND_NAME
            {
                let result = super::session_transfer_command::handle_import_command(
                    super::session_transfer_command::ImportCommand {
                        runtime: &runtime,
                        sink: &sink,
                        session_id: &session_id,
                        instructions: command.instructions.as_deref(),
                        conversation: &conversation,
                        model_transcript: &model_transcript,
                        telemetry: &telemetry,
                        telemetry_turn: &telemetry_turn,
                        telemetry_started_at,
                        telemetry_event_start,
                    },
                )?;
                persist_session_snapshot(
                    &session_store,
                    &implementation_name,
                    &session_id,
                    &sessions,
                )?;
                return Ok(result);
            }
            // `/branch` forks the current session from a stored checkpoint.
            // It runs before discard/resume handling so a paused turn can be
            // branched instead of continued. Missing checkpoints fail closed
            // without touching the active session.
            if let Some(command) = parse_slash_command(&prompt_text)
                && matches!(command.name.as_str(), BRANCH_COMMAND_NAME | REVERT_COMMAND_NAME)
            {
                if command.name == REVERT_COMMAND_NAME {
                    return super::checkpoint_command::handle_revert_command(
                        super::checkpoint_command::RevertCommand {
                            runtime: &runtime,
                            sessions: &sessions,
                            sink: &sink,
                            session_id: &session_id,
                            instructions: command.instructions.as_deref(),
                            workspace: &workspace,
                            workspace_roots: &workspace_roots,
                            telemetry: &telemetry,
                            telemetry_turn: &telemetry_turn,
                            telemetry_started_at,
                            telemetry_event_start,
                        },
                    );
                }
                return super::branch_command::handle_branch_command(
                    super::branch_command::BranchCommand {
                        runtime: &runtime,
                        plugins: &plugins,
                        models: &models,
                        sessions: &sessions,
                        session_store: &session_store,
                        sink: &sink,
                        session_id: &session_id,
                        instructions: command.instructions.as_deref(),
                        base_policy: &base_policy,
                        mode: &mode,
                        workspace: &workspace,
                        workspace_roots: &workspace_roots,
                        provider_label: &provider_label,
                        model_label: &model_label,
                        selected_model: runtime.selected_model(),
                        mcp_servers: &mcp_servers,
                        implementation_name: &implementation_name,
                        telemetry: &telemetry,
                        telemetry_turn: &telemetry_turn,
                        telemetry_started_at,
                        telemetry_event_start,
                    },
                );
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
                runtime.terminate_background_jobs(&client, &session_id).await;
                runtime.prune_worktrees();
                runtime.clear_subagent_transcripts();
                let _ = sink
                    .agent_message_chunk("discard", "paused turn discarded; checkpoint cleared");
                finish_provider_telemetry(
                    &telemetry,
                    ProviderTelemetryTurn {
                        turn_id: &telemetry_turn,
                        started_at: telemetry_started_at,
                        events: &runtime.event_snapshot()[telemetry_event_start..],
                        outcome: TelemetryTurnOutcome::Succeeded,
                        terminal_state: None,
                        evidence_ids: Vec::new(),
                        usage: None,
                    },
                );
                return Ok(PromptResult::new(StopReason::EndTurn));
            }
            // User-defined templates run after built-in commands so a template
            // can never shadow `/compact`, `/branch`, or another reserved name.
            if let Some(parsed) = parse_slash_command(&prompt_text)
                && let Some(user_command) = user_commands.get(&parsed.name)
            {
                let outcome = super::command_template::handle_command_template(
                    super::command_template::CommandTemplate {
                        runtime: &runtime,
                        sessions: &sessions,
                        models: &models,
                        sink: &sink,
                        client: &client,
                        cancel: &cancel,
                        ctx: &mut ctx,
                        command: user_command,
                        instructions: parsed.instructions.as_deref(),
                        transcript: &model_transcript,
                        telemetry: &telemetry,
                        telemetry_turn: &telemetry_turn,
                        telemetry_started_at,
                        telemetry_event_start,
                    },
                )
                .await?;
                match outcome {
                    super::command_template::CommandTemplateOutcome::Prompt(rendered) => {
                        prompt_text = rendered;
                        // Selection may have changed the model route or active
                        // agent; rebuild the prompt planes for this same turn.
                        bundle = runtime_prompt_bundle(
                            &runtime,
                            &models,
                            RuntimePromptInputs {
                                cwd: &workspace,
                                workspace_roots: &workspace_roots,
                                mode: &mode,
                                mcp_servers: &mcp_server_names,
                                global_config_root: global_config_root.as_deref(),
                                background_jobs: background_jobs.as_deref(),
                                remote_instructions: &remote,
                            },
                        )?;
                        bundle.workspace.extend(prepared_prompt.instructions.iter().cloned());
                    }
                    super::command_template::CommandTemplateOutcome::Completed(result) => {
                        persist_session_snapshot(
                            &session_store,
                            &implementation_name,
                            &session_id,
                            &sessions,
                        )?;
                        return Ok(result);
                    }
                }
            }
            let has_pending = store.has_pending(&session_id.to_string());
            // `/resume` continues a paused turn without the original prompt
            // (client-crash continuation).  Without a pending checkpoint it
            // is an ordinary prompt whose text reaches the model.
            let resume_command = recovery.enabled && is_resume_command(&prompt_text);
            // Automatic compaction runs before the new user message is
            // recorded, so the prompt is never summarized away. The summary
            // lives in orchestrator memory; the provider retains only a recent
            // verbatim tail.
            if !has_pending && !resume_command {
                let history_for_compaction = session_compaction_history(
                    &runtime,
                    &session_id.to_string(),
                    &model_transcript,
                );
                let estimated = super::auto_compact::estimated_tokens(&history_for_compaction);
                let transcript_at_capacity =
                    model_transcript.lock().expect("model transcript poisoned").len()
                        >= CONVERSATION_MAX_MESSAGES;
                if transcript_at_capacity
                    || super::auto_compact::should_compact(
                        estimated,
                        runtime.config().context_window_tokens,
                        runtime.config().compaction.auto_threshold_percent,
                    )
                {
                    let compaction = runtime
                        .run_compact_turn_with_history(
                            ctx.clone(),
                            sink.clone(),
                            cancel.clone(),
                            None,
                            Some(bundle.clone()),
                            history_for_compaction,
                        )
                        .await;
                    if let Err(error) = compaction {
                        tracing::warn!(%error, "automatic context compaction failed; continuing with full history");
                    } else {
                        let retained = runtime.config().compaction.auto_retained_messages;
                        super::auto_compact::trim_conversation(&conversation, retained);
                        super::auto_compact::trim_model_transcript(&model_transcript, retained);
                    }
                }
            }
            let history = if has_pending { Vec::new() } else { model_history(&model_transcript) };
            if !has_pending {
                record_user_message(&conversation, prompt_text);
                // Persist prompt receipt before model work so an abrupt host
                // shutdown never loses the user's last message.
                persist_session_snapshot(
                    &session_store,
                    &implementation_name,
                    &session_id,
                    &sessions,
                )?;
            }
            // Record the client-visible conversation for `session/load`
            // replay: user prompts here, agent text chunks through a sink
            // observer.
            let recording_conversation = conversation.clone();
            let plan_output = Arc::new(Mutex::new(String::new()));
            let recorded_plan_output = plan_output.clone();
            let sink = sink.with_observer(Arc::new(move |update| {
                if let SessionUpdate::AgentMessageChunk(chunk) = update {
                    let text = match &chunk.content {
                        ee_agent_protocol::ContentBlock::Text(text) => text.text.clone(),
                        _ => String::new(),
                    };
                    if !text.is_empty() {
                        let is_final_response =
                            chunk.message_id.as_ref().is_some_and(|message_id| {
                                message_id.0.starts_with("ee-final-response-")
                            });
                        if is_final_response {
                            record_final_response(&recording_conversation, text);
                        } else {
                            record_agent_message(&recording_conversation, &text);
                            recorded_plan_output
                                .lock()
                                .expect("plan output poisoned")
                                .push_str(&text);
                        }
                    }
                }
            }));
            let plan_sink = sink.clone();
            let final_sink = sink.clone();
            // Phase 12: bridge the session's MCP servers into the tool
            // registry for this prompt.  The manager is per prompt (the
            // `ClientBridge` is per prompt), while the validated descriptors
            // stay per session.
            let has_mcp_servers = !mcp_servers.is_empty();
            let mut manager =
                McpSessionManager::new(mcp_servers, client.clone(), cancel.clone(), mcp_policy);
            let mut diagnostics = manager.discover_all().await;
            let mcp_instructions = manager.server_instructions();
            let manager = Arc::new(manager);
            let mut registered = Vec::new();
            // Mirror prompt-scoped MCP tools through `agent.tools` so scope
            // ownership and disposal are observable by plugins. The runtime
            // registry remains the execution path.
            let plugin_turn =
                plugins.sessions().session(&session_id.to_string()).and_then(|session| {
                    let turn = session.next_turn();
                    session.turn_scope(turn).ok()
                });
            let mut host_tool_disposers = Vec::new();
            let mcp_plugin = ee_plugin_api::ids::PluginId::new("ee.mcp").ok();
            let definitions = manager.tool_definitions();
            let host_turn =
                plugin_turn.as_ref().map(|turn| crate::plugin_runtime::services::ToolScope::Turn {
                    session_id: session_id.to_string(),
                    turn: turn.turn(),
                });
            for definition in definitions {
                let tool: Arc<dyn crate::tools::ServerTool> =
                    Arc::new(McpBackedTool::new(definition.clone(), manager.clone()));
                if let (Some(plugin), Some(scope)) = (mcp_plugin.as_ref(), host_turn.clone())
                    && let Ok(disposer) = plugins.tools().register_for_session(
                        plugin,
                        scope,
                        Some(&session_id.to_string()),
                        Arc::clone(&tool),
                    )
                {
                    host_tool_disposers.push(disposer);
                }
                match runtime.register_tool(tool) {
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
            drop(host_turn);
            if has_mcp_servers {
                // Resource and bounded-event tools share the prompt's MCP
                // connections; they are read-class and never mutate state.
                for tool in crate::mcp::resource_tools(manager.clone()) {
                    let name = tool.definition().name;
                    match runtime.register_tool(tool) {
                        Ok(()) => registered.push(name),
                        Err(error) => diagnostics.push(McpDiscoveryDiagnostic {
                            server_id: "mcp".to_string(),
                            message: format!(
                                "failed to register MCP resource tool {name:?}: {error}"
                            ),
                        }),
                    }
                }
            }
            emit_mcp_diagnostics(&sink, &diagnostics, &runtime.policy(), &manager);
            // Rebuild the stable prompt now that prompt-scoped MCP tools are
            // registered: guidance must name only tools the model can call.
            let background_jobs = runtime.background_jobs_snapshot();
            let mut bundle = runtime_prompt_bundle(
                &runtime,
                &models,
                RuntimePromptInputs {
                    cwd: &workspace,
                    workspace_roots: &workspace_roots,
                    mode: &mode,
                    mcp_servers: &mcp_server_names,
                    global_config_root: global_config_root.as_deref(),
                    background_jobs: background_jobs.as_deref(),
                    remote_instructions: &remote,
                },
            )?;
            bundle.mcp = crate::prompt::mcp::instruction_messages(&mcp_instructions);
            let cleanup =
                McpTurnCleanup { runtime: runtime.clone(), manager: Some(manager), registered };
            // The framework's cancellation watch flips on `session/cancel`
            // and `session/close`; run_turn observes it and stops promptly.
            let provider_name = implementation_name.clone();
            // Auto-resume needs the prompt inputs after the first run consumed
            // them; clone once up front.
            let resume_ctx = ctx.clone();
            let resume_sink = sink.clone();
            let resume_client = client.clone();
            let resume_cancel = cancel.clone();
            // Host evidence is not carried by the current ACP prompt surface, so
            // the runtime derives completion evidence from the tool execution
            // log. A host that can supply typed evidence still wins here; this
            // seam remains optional rather than forcing every turn unverified.
            let strategic_input =
                StrategicInput { validation_workspace, ..StrategicInput::default() };
            let recovery_context = StrategicRecoveryContext::new(
                strategic_input.clone(),
                bundle.clone(),
                provider_name.clone(),
            );
            let resume_recovery_context =
                StrategicRecoveryContext::new(strategic_input, bundle, provider_name);
            // Capture the pre-turn workspace before the first model call.  The
            // capture is bounded and ignore-aware; a failure never blocks the
            // turn, it only disables file diff/revert for this turn.
            let snapshot_roots: Vec<std::path::PathBuf> = {
                let mut roots = Vec::with_capacity(workspace_roots.len() + 1);
                roots.push(workspace.clone());
                roots.extend(workspace_roots.iter().filter(|root| **root != workspace).cloned());
                roots
            };
            let snapshot_turn = match runtime
                .workspace_snapshots()
                .begin_turn(&session_id.to_string(), &snapshot_roots)
            {
                Ok(record) => Some(record.turn),
                Err(error) => {
                    tracing::warn!(%error, "workspace snapshot capture failed; turn diff disabled");
                    None
                }
            };
            let result = if has_pending {
                // A paused turn awaits: the same prompt resumes it from its
                // checkpoint (manual resume). `/resume` carries no new prompt
                // text, so the checkpoint transcript remains authoritative.
                let resume_ctx = if resume_command {
                    PromptContext::new(ctx.session_id.clone(), Vec::new())
                } else {
                    ctx
                };
                runtime
                    .resume_turn_strategic(resume_ctx, sink, client, cancel, recovery_context)
                    .await
            } else {
                runtime
                    .run_turn_strategic_recoverable_with_history(
                        ctx,
                        sink,
                        client,
                        cancel,
                        recovery_context,
                        history,
                    )
                    .await
            };
            let result: Result<Box<StrategicRecoveryTurn>, ProviderError> = match result {
                Ok(StrategicTurnOutcome::Completed(turn)) => Ok(turn),
                Ok(StrategicTurnOutcome::Interrupted(interruption)) => {
                    // Safe single auto-resume: transient/deadline faults with
                    // a durable checkpoint and no ambiguous in-flight tool
                    // resume once automatically, capped by the config.
                    let auto_resume = recovery.auto_resume_max > 0
                        && interruption.safe_resume
                        && interruption.resumed_count < recovery.auto_resume_max;
                    if auto_resume {
                        match runtime
                            .resume_turn_strategic(
                                resume_ctx,
                                resume_sink,
                                resume_client,
                                resume_cancel,
                                resume_recovery_context,
                            )
                            .await
                        {
                            Ok(StrategicTurnOutcome::Completed(turn)) => Ok(turn),
                            Ok(StrategicTurnOutcome::Interrupted(again)) => {
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
            let snapshot_record = snapshot_turn.and_then(|turn| {
                match runtime.workspace_snapshots().finish_turn(
                    &session_id.to_string(),
                    turn,
                    &snapshot_roots,
                ) {
                    Ok(record) => Some(record),
                    Err(error) => {
                        tracing::warn!(%error, "workspace snapshot end capture failed");
                        None
                    }
                }
            });
            if result.is_ok() {
                record_model_transcript(&model_transcript, runtime.last_turn_transcript());
                if let Some(record) = &snapshot_record {
                    annotate_snapshot_ids(&model_transcript, record);
                }
            }
            let telemetry_completion = result.as_ref().ok().map(|turn| {
                (
                    turn.final_response.completion.state,
                    turn.final_response.completion.evidence_ids.clone(),
                )
            });
            let result = if mode.to_string() == PLAN_MODE_ID {
                match result {
                    Ok(turn) => {
                        let plan_output = plan_output.lock().expect("plan output poisoned").clone();
                        let plan_result = parse_plan_items(&plan_output).and_then(|items| {
                            let entries = runtime
                                .install_plan(&items)
                                .map_err(|error| {
                                    ProviderError::BackendFailure(format!(
                                        "plan mode rejected: {error}"
                                    ))
                                })?
                                .plan_entries();
                            plan_sink.plan_replace(entries).map_err(|error| {
                                ProviderError::BackendFailure(format!(
                                    "plan emission failed: {error}"
                                ))
                            })
                        });
                        plan_result.and_then(|()| {
                            emit_final_response(
                                &final_sink,
                                &turn.final_response,
                                &next_final_response,
                            )?;
                            Ok(turn.prompt_result)
                        })
                    }
                    Err(error) => Err(error),
                }
            } else {
                result.and_then(|turn| {
                    emit_final_response(&final_sink, &turn.final_response, &next_final_response)?;
                    Ok(turn.prompt_result)
                })
            };
            cleanup.finish().await;
            let events = runtime.event_snapshot();
            let telemetry_outcome = if result.is_ok() {
                TelemetryTurnOutcome::Succeeded
            } else if result
                .as_ref()
                .err()
                .is_some_and(|error| error.to_string().to_ascii_lowercase().contains("cancel"))
            {
                TelemetryTurnOutcome::Cancelled
            } else {
                TelemetryTurnOutcome::Failed
            };
            let (terminal_state, evidence_ids) =
                telemetry_completion.map_or((None, Vec::new()), |(state, ids)| (Some(state), ids));
            let turn_usage = result.as_ref().ok().and_then(|result| result.usage.clone());
            finish_provider_telemetry(
                &telemetry,
                ProviderTelemetryTurn {
                    turn_id: &telemetry_turn,
                    started_at: telemetry_started_at,
                    events: &events[telemetry_event_start..],
                    outcome: telemetry_outcome,
                    terminal_state,
                    evidence_ids,
                    usage: turn_usage.as_ref(),
                },
            );
            match result {
                Ok(prompt_result) => {
                    persist_session_snapshot(
                        &session_store,
                        &implementation_name,
                        &session_id,
                        &sessions,
                    )?;
                    Ok(prompt_result)
                }
                Err(error) => {
                    // Preserve conversation/task state best-effort while
                    // retaining the original turn failure for the client.
                    let _ = persist_session_snapshot(
                        &session_store,
                        &implementation_name,
                        &session_id,
                        &sessions,
                    );
                    Err(error)
                }
            }
        })
    }

    fn set_mode(&self, ctx: SetModeContext) -> ProviderFuture<Result<(), ProviderError>> {
        let sessions = self.sessions.clone();
        let session_store = self.session_store.clone();
        let implementation_name = self.config.implementation.name.clone();
        let base_policy = self.policy.clone();
        let plugins = self.plugins.clone();
        Box::pin(async move {
            let policy = plugins.gate_policy(mode_policy(&base_policy, &ctx.mode_id)?);
            {
                let mut sessions = sessions.lock().expect("adapter sessions poisoned");
                let Some(session) = sessions.get_mut(&ctx.session_id.to_string()) else {
                    return Err(ProviderError::InvalidRequest(format!(
                        "unknown orchestrator session: {}",
                        ctx.session_id
                    )));
                };
                session.runtime.set_policy(policy);
                session.mode = ctx.mode_id.clone();
            }
            persist_session_snapshot(
                &session_store,
                &implementation_name,
                &ctx.session_id,
                &sessions,
            )
        })
    }

    fn set_model(&self, ctx: SetModelContext) -> ProviderFuture<Result<(), ProviderError>> {
        let sessions = self.sessions.clone();
        let models = self.models.clone();
        let session_store = self.session_store.clone();
        let implementation_name = self.config.implementation.name.clone();
        Box::pin(async move {
            let info = models.info(&ctx.model_id).ok_or_else(|| {
                let available = models
                    .advertised()
                    .into_iter()
                    .map(|model| model.id)
                    .collect::<Vec<_>>()
                    .join(", ");
                ProviderError::InvalidRequest(format!(
                    "unknown model modelId={:?}; available models: {available}",
                    ctx.model_id
                ))
            })?;
            if !info.enabled {
                return Err(ProviderError::InvalidRequest(format!(
                    "model modelId={:?} is disabled; enable it in provider config before switching",
                    ctx.model_id
                )));
            }
            if !info
                .identity
                .capabilities
                .contains(&crate::model_registry::ModelCapability::ChatCompletion)
            {
                return Err(ProviderError::InvalidRequest(format!(
                    "model modelId={:?} does not advertise chat-completion support",
                    ctx.model_id
                )));
            }
            {
                let mut sessions = sessions.lock().expect("adapter sessions poisoned");
                let Some(session) = sessions.get_mut(&ctx.session_id.to_string()) else {
                    return Err(ProviderError::InvalidRequest(format!(
                        "unknown orchestrator session: {}",
                        ctx.session_id
                    )));
                };
                // The transcript is provider-agnostic; provider-native state
                // stays producer-gated by each codec's `replayable_for`, so a
                // cross-provider switch simply stops replaying old state.
                session.runtime.set_selected_model(Some(ctx.model_id.clone()));
                session.provider_label = info.identity.provider_id.clone();
                session.model_label = ctx.model_id.clone();
            }
            persist_session_snapshot(
                &session_store,
                &implementation_name,
                &ctx.session_id,
                &sessions,
            )
        })
    }

    fn cancel_session(&self, session_id: SessionId) -> ProviderFuture<Result<(), ProviderError>> {
        let sessions = self.sessions.clone();
        let plugins = self.plugins.clone();
        Box::pin(async move {
            // Framework cancellation reaches the parent watch; explicit child
            // cancellation also covers queued children and targeted registry
            // handles without relying on parent-future polling.
            if let Some(session) =
                sessions.lock().expect("adapter sessions poisoned").get(&session_id.to_string())
            {
                session.runtime.cancel_all_children();
                session.runtime.clear_background_jobs();
                session.runtime.prune_worktrees();
                session.runtime.clear_subagent_transcripts();
            }
            if let Some(session) = plugins.sessions().session(&session_id.to_string()) {
                session.cancel();
            }
            Ok(())
        })
    }

    fn close_session(&self, session_id: SessionId) -> ProviderFuture<Result<(), ProviderError>> {
        let sessions = self.sessions.clone();
        let persisted = self.persisted.clone();
        let session_store = self.session_store.clone();
        let implementation_name = self.config.implementation.name.clone();
        let plugins = self.plugins.clone();
        Box::pin(async move {
            // The framework awaits the active prompt's cleanup (bounded)
            // before invoking this hook, so serializing the stores is safe.
            if let Some(session) = plugins.sessions().session(&session_id.to_string()) {
                session.close("session closed");
            }
            let Some(runtime) =
                sessions.lock().expect("adapter sessions poisoned").remove(&session_id.to_string())
            else {
                // Idempotent: the session was never created here.
                return Ok(());
            };
            runtime.runtime.cancel_all_children();
            runtime.runtime.clear_background_jobs();
            runtime.runtime.prune_worktrees();
            runtime.runtime.clear_subagent_transcripts();
            // Retained tool-output overflow bodies are large and are never
            // re-read after the session ends; the record log stays.
            runtime.runtime.purge_session_artifacts(&session_id.to_string());
            // Explicit close finalizes the session: pending recovery
            // checkpoints are deleted (the interrupted work is abandoned
            // unless the host loads the persisted state below).
            runtime.runtime.checkpoint_store().delete_session(&session_id.to_string());
            let state = session_snapshot(&runtime);
            // Keep the in-process copy for existing callers, then flush the
            // same snapshot so closing the editor cannot erase this thread.
            persisted
                .lock()
                .expect("adapter persisted poisoned")
                .insert(session_id.to_string(), state.clone());
            session_store
                .save(&implementation_name, &runtime.workspace, &session_id.to_string(), &state)
                .map_err(ProviderError::from)
        })
    }
}

/// Tags the turn's user and final assistant messages with the snapshot ids
/// captured around it.  Metadata is diagnostic only; providers ignore it.
fn annotate_snapshot_ids(
    transcript: &Arc<Mutex<Vec<ModelMessage>>>,
    record: &crate::workspace_snapshot::TurnSnapshotRecord,
) {
    let mut messages = transcript.lock().expect("model transcript poisoned");
    if let Some(user) = messages.iter_mut().rev().find(|message| message.role == ModelRole::User) {
        user.metadata.insert("ee_snapshot_start".to_string(), record.start.clone());
    }
    if let Some(end) = &record.end
        && let Some(assistant) =
            messages.iter_mut().rev().find(|message| message.role == ModelRole::Assistant)
    {
        assistant.metadata.insert("ee_snapshot_end".to_string(), end.clone());
    }
}
