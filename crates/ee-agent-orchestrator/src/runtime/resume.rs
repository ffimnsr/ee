//! `impl OrchestratorRuntime` methods: resume.
use super::*;

impl OrchestratorRuntime {
    /// Resumes an interrupted turn from its latest checkpoint: restores the
    /// stores (fresh deadline slice, cumulative counters retained), appends
    /// the new prompt to the checkpoint transcript tail, and runs the loop
    /// with the completed-tool idempotency guard.  `provider` must match the
    /// checkpoint's provider identity (crash-restore validation).
    pub async fn resume_turn(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        system_context: String,
        provider: &str,
    ) -> Result<TurnOutcome, OrchestratorError> {
        self.resume_turn_with_metadata(
            ctx,
            sink,
            client,
            cancel,
            system_context,
            provider,
            RecoveryTurnMetadata::default(),
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn resume_turn_with_metadata(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        system_context: String,
        provider: &str,
        metadata: RecoveryTurnMetadata,
    ) -> Result<TurnOutcome, OrchestratorError> {
        let session_id = ctx.session_id.to_string();
        let Some((checkpoint_id, checkpoint)) =
            self.checkpoints.load_latest(&session_id).map_err(|error| {
                OrchestratorError::Serialization(format!(
                    "failed to load pending checkpoint: {error}"
                ))
            })?
        else {
            return Err(OrchestratorError::InvalidState(format!(
                "no pending checkpoint for session {session_id}"
            )));
        };
        if checkpoint.provider != provider {
            return Err(OrchestratorError::PolicyDenied(format!(
                "checkpoint provider {:?} does not match {:?}; refusing restore",
                checkpoint.provider, provider
            )));
        }
        let resume = checkpoint.resume.clone().ok_or_else(|| {
            OrchestratorError::InvalidState("checkpoint has no resumable turn state".into())
        })?;
        if let Some(in_flight) = &resume.in_flight {
            return Err(OrchestratorError::PolicyDenied(format!(
                "pending {} operation {} ({}) has ambiguous completion; explicitly abandon with /discard before another turn",
                in_flight.tool_name, in_flight.tool_call_id, in_flight.arguments_fingerprint
            )));
        }
        if resume.transcript.is_empty() && ctx.prompt.is_empty() {
            return Err(OrchestratorError::InvalidState(
                "durable recovery omits transcript content; send a fresh prompt or explicitly abandon with /discard"
                    .into(),
            ));
        }
        if session_timeout_expired(
            &self.config,
            resume.first_started_at_millis,
            current_unix_millis(),
        ) {
            self.checkpoints.delete_session(&session_id);
            return Err(OrchestratorError::InvalidState(
                "session exceeded its cumulative timeout; checkpoint discarded".into(),
            ));
        }
        self.restore_from_checkpoint(&checkpoint)?;
        let mut resumed = resume.clone();
        resumed.resumed_count += 1;
        self.events.record(OrchestratorEvent::TurnResumed {
            session_id: session_id.clone(),
            checkpoint_id: checkpoint_id.clone(),
            resumed_count: resumed.resumed_count,
        });
        let mut transcript = Transcript::new();
        transcript.messages = resume.transcript;
        // Durable checkpoints omit transcript content. A non-empty prompt is
        // therefore required after crash recovery; `/resume` alone fails above.
        if !ctx.prompt.is_empty() {
            transcript.messages.extend(Transcript::from_prompt(&ctx).messages);
        }
        let memory = self.memory.lock().expect("memory store poisoned").compact_context();
        if let Some(facts) = memory {
            transcript.prepend_system(format!("Memory facts:\n{facts}"));
        }
        if !system_context.is_empty() {
            transcript.prepend_system(&system_context);
        }
        let handle = CheckpointHandle::new(self.checkpoints.clone(), &session_id, provider)
            .with_capture_metadata(metadata.checkpoint_context, metadata.evidence_refs);
        let model = self.models.default_adapter()?;
        let policy = self.policy();
        let engine = LoopEngine::new(
            self.config.clone(),
            model,
            self.tools.clone(),
            self.budget.clone(),
            policy,
            self.events.clone(),
            LoopOptions {
                graph: Some(self.tasks.clone()),
                available_models: self.models.advertised(),
                model_id: Some(DEFAULT_MODEL_ID.to_string()),
                memory: Some(self.memory.clone()),
                checkpoint: Some(handle),
                resume_state: Some(resumed),
                ..LoopOptions::default()
            },
        );
        let task = {
            let tasks = self.tasks.lock().expect("task graph poisoned");
            tasks.get(&crate::tasks::TaskId::new(resume.active_task_id.clone())).cloned()
        }
        .ok_or_else(|| {
            OrchestratorError::InvalidState(format!(
                "resume references unknown active task {}",
                resume.active_task_id
            ))
        })?;
        let outcome = engine
            .run_transcript(&mut transcript, session_id.clone(), sink, client, cancel, task)
            .await;
        match outcome {
            Ok(result) => {
                self.checkpoints.delete_session(&session_id);
                Ok(TurnOutcome::Completed(result))
            }
            Err(OrchestratorError::Cancellation) => {
                self.checkpoints.delete_session(&session_id);
                Err(OrchestratorError::Cancellation)
            }
            Err(OrchestratorError::DeadlineExceeded(detail)) => {
                Ok(TurnOutcome::Interrupted(self.interruption_for(
                    &session_id,
                    RecoverableFault::Deadline,
                    detail,
                    Some(resume.resumed_count + 1),
                )?))
            }
            Err(OrchestratorError::Timeout(detail)) => {
                Ok(TurnOutcome::Interrupted(self.interruption_for(
                    &session_id,
                    RecoverableFault::Deadline,
                    detail,
                    Some(resume.resumed_count + 1),
                )?))
            }
            Err(error) => Err(error),
        }
    }
    /// Builds a [`RecoverableInterruption`] from the session's latest
    /// checkpoint, recording the interruption event.
    pub(super) fn interruption_for(
        &self,
        session_id: &str,
        fault: RecoverableFault,
        detail: String,
        resumed_count_hint: Option<u32>,
    ) -> Result<RecoverableInterruption, OrchestratorError> {
        let latest = self.checkpoints.load_latest(session_id)?;
        let interruption = RecoverableInterruption::from_checkpoint(
            fault,
            detail,
            None,
            None,
            latest.as_ref().map(|(id, checkpoint)| (id.as_str(), checkpoint)),
        );
        let interruption = match resumed_count_hint {
            Some(count) => RecoverableInterruption { resumed_count: count, ..interruption },
            None => interruption,
        };
        self.events.record(OrchestratorEvent::TurnInterrupted {
            session_id: session_id.to_string(),
            fault: fault.as_str().to_string(),
            safe_resume: interruption.safe_resume,
            resumed_count: interruption.resumed_count,
        });
        Ok(interruption)
    }
    /// Same as [`OrchestratorRuntime::run_turn`] but records every loop
    /// decision into `events`; used by tests to assert stable decision
    /// sequences.
    pub(crate) async fn run_turn_recording(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        events: EventRecorder,
    ) -> Result<PromptResult, OrchestratorError> {
        self.run_turn_recording_with_system_context(ctx, sink, client, cancel, events, None).await
    }
    pub(super) async fn run_turn_recording_with_system_context(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        events: EventRecorder,
        system_context: Option<String>,
    ) -> Result<PromptResult, OrchestratorError> {
        // Agent-advertised slash commands arrive as ordinary prompt text;
        // `/compact` takes the compaction path before any task or tool work.
        let prompt_text = prompt_text(&ctx);
        if let Some(command) = parse_slash_command(&prompt_text)
            && command.name == COMPACT_COMMAND_NAME
        {
            return self
                .run_compact_turn_recording(
                    ctx,
                    sink,
                    cancel,
                    command.instructions,
                    system_context,
                    Vec::new(),
                    events,
                )
                .await;
        }
        let (title, description) = task_summary(&ctx);
        let (task, entries) = {
            let mut tasks = self.tasks.lock().expect("task graph poisoned");
            let task = tasks.create_root(&title, &description);
            let entries = tasks.plan_entries();
            (task, entries)
        };
        // Keep the client's plan view in sync with the task graph.
        sink.plan_replace(entries).map_err(|error| {
            OrchestratorError::InvalidState(format!("plan emission failed: {error}"))
        })?;
        self.run_loop_with_options(
            ctx,
            sink,
            client,
            cancel,
            task,
            system_context,
            Vec::new(),
            None,
            events,
            LoopOptions {
                graph: Some(self.tasks.clone()),
                available_models: self.models.advertised(),
                model_id: Some(DEFAULT_MODEL_ID.to_string()),
                ..LoopOptions::default()
            },
        )
        .await
    }
    /// Shared turn runner: builds the engine from `options` and runs one
    /// bounded turn over the prompt with the given system context.  The
    /// wall-clock deadline is re-anchored to this turn's slice (a runtime
    /// may idle between turns for longer than one timeout).
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_loop_with_options(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        task: TaskNode,
        system_context: Option<String>,
        history: Vec<ModelMessage>,
        task_context: Option<ContextPlan>,
        events: EventRecorder,
        options: LoopOptions,
    ) -> Result<PromptResult, OrchestratorError> {
        self.budget
            .lock()
            .expect("budget tracker poisoned")
            .reset_deadline(self.config.turn_timeout);
        let memory = self.memory.lock().expect("memory store poisoned").compact_context();
        let model = self.models.default_adapter()?;
        let policy = self.policy();
        let engine = LoopEngine::new(
            self.config.clone(),
            model,
            self.tools.clone(),
            self.budget.clone(),
            policy,
            events,
            options,
        );
        engine
            .run_with_system_context(
                ctx,
                sink,
                client,
                cancel,
                task,
                TurnSystemContext { history, memory, session: system_context, task_context },
            )
            .await
    }
}
