//! `impl AgentProvider for OrchestratorProvider`: ACP session surface.
use super::conversation::McpTurnCleanup;
use super::conversation::emit_mcp_diagnostics;
use super::conversation::initial_commands;
use super::conversation::model_history;
use super::conversation::next_session_number;
use super::conversation::persist_session_snapshot;
use super::conversation::record_agent_message;
use super::conversation::record_final_response;
use super::conversation::record_user_message;
use super::conversation::replay_conversation;
use super::conversation::restore_runtime_from_checkpoint;
use super::conversation::session_snapshot;
use super::conversation::validate_mcp_servers;
use super::conversation::workspace_system_context;
use super::*;

impl AgentProvider for OrchestratorProvider {
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
        if self.config.orchestrator.recovery.is_durable() {
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
        let models = self.models.clone();
        let policy = self.policy.clone();
        let sessions = self.sessions.clone();
        let session_store = self.session_store.clone();
        let implementation = self.config.implementation.clone();
        let next_session = self.next_session.clone();
        let recovery_enabled = config.recovery.enabled;
        Box::pin(async move {
            // Monotonic id per process, raised past ids that survive in the
            // durable stores so a reconnected session is never shadowed by a
            // fresh one after a restart.
            let durable_session_ids = session_store
                .session_ids(&implementation.name, &ctx.cwd)
                .map_err(ProviderError::from)?;
            let number = next_session_number(&next_session, &config.recovery, durable_session_ids);
            let session_id = SessionId::new(format!("{SESSION_ID_PREFIX}-{number}"));
            let system_context = workspace_system_context(&ctx.cwd, &ctx.additional_directories);
            let mode = default_session_mode();
            let mcp_servers = validate_mcp_servers(&ctx.mcp_servers)?;
            let runtime = Arc::new(OrchestratorRuntime::with_shared_model_registry(
                config,
                models,
                mode_policy(&policy, &mode)?,
            )?);
            runtime.register_builtins(&session_id).map_err(|error| {
                ProviderError::BackendFailure(format!("failed to register built-in tools: {error}"))
            })?;
            let entry = SessionRuntime {
                runtime,
                system_context,
                workspace: ctx.cwd.clone(),
                mode: mode.clone(),
                mcp_servers,
                conversation: Arc::new(Mutex::new(Vec::new())),
            };
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
            let (runtime, conversation, mode) = match state {
                Some(state) => {
                    let mode = state.mode;
                    (
                        Arc::new(OrchestratorRuntime::with_shared_model_registry(
                            config,
                            models.clone(),
                            mode_policy(&policy, &mode)?,
                        )?),
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
                        mode_policy(&policy, &mode)?,
                    )?;
                    // Durable checkpoints contain no transcript content, so
                    // crash restore never replays user or model text.
                    (runtime, Vec::new(), mode)
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
                    workspace: ctx.cwd.clone(),
                    mode: mode.clone(),
                    mcp_servers,
                    conversation: Arc::new(Mutex::new(conversation)),
                },
            );
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
                    mode_policy(&policy, &mode)?,
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
                        workspace: ctx.cwd.clone(),
                        mode: mode.clone(),
                        mcp_servers,
                        conversation: Arc::new(Mutex::new(Vec::new())),
                    },
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
        ctx: PromptContext,
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
        Box::pin(async move {
            let session = {
                let sessions = sessions.lock().expect("adapter sessions poisoned");
                sessions.get(&session_id.to_string()).map(|session| {
                    (
                        session.runtime.clone(),
                        session.system_context.clone(),
                        session.mode.clone(),
                        session.mcp_servers.clone(),
                        session.conversation.clone(),
                    )
                })
            };
            let Some((runtime, system_context, mode, mcp_servers, conversation)) = session else {
                return Err(ProviderError::BackendFailure(format!(
                    "no orchestrator state for session {session_id}"
                )));
            };
            let telemetry_turn =
                format!("turn-{}", next_telemetry_turn.fetch_add(1, Ordering::Relaxed));
            let telemetry_started_at = Instant::now();
            let telemetry_event_start = runtime.event_snapshot().len();
            let _ = telemetry
                .lock()
                .expect("telemetry recorder poisoned")
                .start_turn(&telemetry_turn, telemetry_attribution);
            let system_context = mode_system_context(system_context, &mode);
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
            let compact_command = parse_slash_command(&prompt_text)
                .filter(|command| command.name == COMPACT_COMMAND_NAME);
            if let Some(command) = compact_command {
                let history = model_history(&conversation);
                let result = runtime
                    .run_compact_turn_with_history(
                        ctx,
                        sink,
                        cancel,
                        command.instructions,
                        Some(system_context),
                        history,
                    )
                    .await
                    .map_err(ProviderError::from);
                let events = runtime.event_snapshot();
                finish_provider_telemetry(
                    &telemetry,
                    &telemetry_turn,
                    telemetry_started_at,
                    &events[telemetry_event_start..],
                    if result.is_ok() {
                        TelemetryTurnOutcome::Succeeded
                    } else {
                        TelemetryTurnOutcome::Failed
                    },
                    None,
                    Vec::new(),
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
            // Manual critique is handled before MCP discovery and mutable tool
            // registration. Existing built-ins remain registered but critic
            // discovery/policy exposes read-only definitions only.
            if let Some(command) = parse_slash_command(&prompt_text)
                && command.name == RUBBER_DUCK_COMMAND_NAME
            {
                let history = model_history(&conversation);
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
                    &telemetry_turn,
                    telemetry_started_at,
                    &events[telemetry_event_start..],
                    TelemetryTurnOutcome::Succeeded,
                    None,
                    Vec::new(),
                );
                persist_session_snapshot(
                    &session_store,
                    &implementation_name,
                    &session_id,
                    &sessions,
                )?;
                return Ok(turn.prompt_result);
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
                finish_provider_telemetry(
                    &telemetry,
                    &telemetry_turn,
                    telemetry_started_at,
                    &runtime.event_snapshot()[telemetry_event_start..],
                    TelemetryTurnOutcome::Succeeded,
                    None,
                    Vec::new(),
                );
                return Ok(PromptResult::new(StopReason::EndTurn));
            }
            let has_pending = store.has_pending(&session_id.to_string());
            // `/resume` continues a paused turn without the original prompt
            // (client-crash continuation).  Without a pending checkpoint it
            // is an ordinary prompt whose text reaches the model.
            let resume_command = recovery.enabled && is_resume_command(&prompt_text);
            let history = if has_pending { Vec::new() } else { model_history(&conversation) };
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
            emit_mcp_diagnostics(&sink, &diagnostics, &runtime.policy(), &manager);
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
            let resume_system = system_context.clone();
            // Host evidence is unavailable on current `PromptContext` / `ClientBridge`.
            // Required host seam: bounded, redacted completion evidence IDs plus
            // revisions supplied with this prompt and mapped into `StrategicInput`.
            // Until that existing-provider input is added, responses stay unverified.
            let strategic_input =
                StrategicInput { validation_workspace, ..StrategicInput::default() };
            let recovery_context = StrategicRecoveryContext::new(
                strategic_input.clone(),
                system_context,
                provider_name.clone(),
            );
            let resume_recovery_context =
                StrategicRecoveryContext::new(strategic_input, resume_system, provider_name);
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
            finish_provider_telemetry(
                &telemetry,
                &telemetry_turn,
                telemetry_started_at,
                &events[telemetry_event_start..],
                telemetry_outcome,
                terminal_state,
                evidence_ids,
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
        Box::pin(async move {
            let policy = mode_policy(&base_policy, &ctx.mode_id)?;
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

    fn cancel_session(&self, session_id: SessionId) -> ProviderFuture<Result<(), ProviderError>> {
        let sessions = self.sessions.clone();
        Box::pin(async move {
            // Framework cancellation reaches the parent watch; explicit child
            // cancellation also covers queued children and targeted registry
            // handles without relying on parent-future polling.
            if let Some(session) =
                sessions.lock().expect("adapter sessions poisoned").get(&session_id.to_string())
            {
                session.runtime.cancel_all_children();
            }
            Ok(())
        })
    }

    fn close_session(&self, session_id: SessionId) -> ProviderFuture<Result<(), ProviderError>> {
        let sessions = self.sessions.clone();
        let persisted = self.persisted.clone();
        let session_store = self.session_store.clone();
        let implementation_name = self.config.implementation.name.clone();
        Box::pin(async move {
            // The framework awaits the active prompt's cleanup (bounded)
            // before invoking this hook, so serializing the stores is safe.
            let Some(runtime) =
                sessions.lock().expect("adapter sessions poisoned").remove(&session_id.to_string())
            else {
                // Idempotent: the session was never created here.
                return Ok(());
            };
            runtime.runtime.cancel_all_children();
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
