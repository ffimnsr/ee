//! `impl OrchestratorRuntime` methods: turn.
use super::*;

impl OrchestratorRuntime {
    /// Runs one turn: builds the root task from the prompt, then runs the
    /// bounded model → tool loop, streaming updates through `sink` and
    /// making agent → client calls through `client`.
    ///
    /// `cancel` flips when the session is closed or the prompt is cancelled;
    /// the loop stops promptly instead of starting new work.
    pub async fn run_turn(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
    ) -> Result<PromptResult, OrchestratorError> {
        self.run_turn_recording(ctx, sink, client, cancel, EventRecorder::new()).await
    }
    /// Runs one turn with recovery enabled: milestone checkpoints are
    /// persisted and deadline/timeout stops become
    /// [`TurnOutcome::Interrupted`] carrying a durable checkpoint instead of
    /// a fatal error.  Completed turns and cancellations clear the session's
    /// pending checkpoints.  `provider` stamps the checkpoint identity used
    /// by crash restore.
    pub async fn run_turn_recoverable(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        prompt: PromptBundle,
        provider: &str,
    ) -> Result<TurnOutcome, OrchestratorError> {
        self.run_turn_recoverable_with_metadata(
            ctx,
            sink,
            client,
            cancel,
            prompt,
            provider,
            RecoveryTurnMetadata::default(),
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_turn_recoverable_with_metadata(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        prompt: PromptBundle,
        provider: &str,
        metadata: RecoveryTurnMetadata,
    ) -> Result<TurnOutcome, OrchestratorError> {
        *self.last_turn_changed_files.lock().expect("last turn changed files poisoned") =
            Vec::new();
        *self.last_turn_validation.lock().expect("last turn validation poisoned") =
            ValidationRecorder::new();
        *self.last_turn_execution_log.lock().expect("last turn execution log poisoned") =
            Vec::new();
        *self.last_repair_stop.lock().expect("last repair stop poisoned") = None;
        if !self.config.recovery.enabled {
            let result = self
                .run_turn_guided_without_recovery(ctx, sink, client, cancel, prompt, metadata)
                .await?;
            return Ok(TurnOutcome::Completed(result));
        }
        // Agent-advertised slash commands arrive as ordinary prompt text;
        // `/compact` takes the compaction path before any task or tool work.
        let prompt_text = prompt_text(&ctx);
        if let Some(command) = parse_slash_command(&prompt_text)
            && command.name == COMPACT_COMMAND_NAME
        {
            let result = self
                .run_compact_turn_recording(
                    ctx,
                    sink,
                    cancel,
                    command.instructions,
                    Some(prompt),
                    metadata.history,
                    self.events.clone(),
                )
                .await?;
            return Ok(TurnOutcome::Completed(result));
        }
        let (title, description) = task_summary(&ctx);
        let (task, entries) = {
            let mut tasks = self.tasks.lock().expect("task graph poisoned");
            let task = tasks.create_root(&title, &description);
            let entries = tasks.plan_entries();
            (task, entries)
        };
        sink.plan_replace(entries).map_err(|error| {
            OrchestratorError::InvalidState(format!("plan emission failed: {error}"))
        })?;
        let session_id = ctx.session_id.to_string();
        let definitions = self.tools.lock().expect("tool registry poisoned").definitions();
        let strategy_context = StrategyContext {
            prompt_text: prompt_text.clone(),
            has_code_changes: false,
            validation_tools_available: definitions
                .iter()
                .any(|definition| crate::strategy::is_validation_tool_name(&definition.name)),
            delegation_allowed: self.policy().policy().allow_delegate,
            task_graph: self.tasks.lock().expect("task graph poisoned").clone(),
            tool_definitions: definitions,
        };
        let guidance = capability_aware_guidance(&strategy_context, &self.policy());
        self.events.record(OrchestratorEvent::StrategySelected {
            strategy: guidance.decision.strategy,
            reason: guidance.decision.reason,
        });
        let mut prompt = prompt;
        let budget = self.budget.lock().expect("budget tracker poisoned").snapshot();
        append_turn_guidance(&mut prompt, &guidance.text, Some(&budget));
        let handle = CheckpointHandle::new(self.checkpoints.clone(), &session_id, provider)
            .with_capture_metadata(
                metadata.checkpoint_context.clone(),
                metadata.evidence_refs.clone(),
            );
        let execution_log = Arc::new(Mutex::new(Vec::new()));
        let validation_sink = sink.clone();
        let validation_client = client.clone();
        let validation_cancel = cancel.clone();
        let validation_task = task.clone();
        let outcome = self
            .run_loop_with_options(
                ctx.clone(),
                sink,
                client,
                cancel,
                task,
                Some(prompt.clone()),
                metadata.history.clone(),
                metadata.context_plan.clone(),
                self.events.clone(),
                LoopOptions {
                    read_first: guidance.decision.strategy
                        == crate::strategy::TurnStrategy::ResearchThenEdit,
                    graph: Some(self.tasks.clone()),
                    available_models: self.models.advertised(),
                    model_id: Some(DEFAULT_MODEL_ID.to_string()),
                    memory: Some(self.memory.clone()),
                    checkpoint: Some(handle),
                    execution_log: Some(execution_log.clone()),
                    ..LoopOptions::default()
                },
            )
            .await;
        match outcome {
            Ok(result) => {
                let post_validation_log =
                    execution_log.lock().expect("execution log poisoned").clone();
                let validation = self
                    .run_post_write_validation(
                        post_validation_log,
                        &validation_task,
                        validation_sink.clone(),
                        validation_client.clone(),
                        validation_cancel.clone(),
                    )
                    .await?;
                self.run_bounded_repair(
                    &ctx,
                    &validation_task,
                    execution_log,
                    validation,
                    validation_sink,
                    validation_client,
                    validation_cancel,
                    Some(prompt),
                    &session_id,
                    provider,
                )
                .await?;
                self.checkpoints.delete_session(&session_id);
                Ok(TurnOutcome::Completed(result))
            }
            Err(OrchestratorError::Cancellation) => {
                self.checkpoints.delete_session(&session_id);
                Err(OrchestratorError::Cancellation)
            }
            Err(OrchestratorError::DeadlineExceeded(detail)) => Ok(TurnOutcome::Interrupted(
                self.interruption_for(&session_id, RecoverableFault::Deadline, detail, None)?,
            )),
            Err(OrchestratorError::Timeout(detail)) => Ok(TurnOutcome::Interrupted(
                self.interruption_for(&session_id, RecoverableFault::Deadline, detail, None)?,
            )),
            Err(error) => Err(error),
        }
    }
    /// Runs the default production loop when recovery is disabled while still
    /// applying bounded capability-aware strategy guidance and host-provided
    /// untrusted task context. It deliberately reuses `LoopEngine`; guidance
    /// never bypasses tool policy, approval, or budget gates.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_turn_guided_without_recovery(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        prompt: PromptBundle,
        metadata: RecoveryTurnMetadata,
    ) -> Result<PromptResult, OrchestratorError> {
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
                    Some(prompt),
                    metadata.history,
                    self.events.clone(),
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
        sink.plan_replace(entries).map_err(|error| {
            OrchestratorError::InvalidState(format!("plan emission failed: {error}"))
        })?;
        let definitions = self.tools.lock().expect("tool registry poisoned").definitions();
        let strategy_context = StrategyContext {
            prompt_text,
            has_code_changes: false,
            validation_tools_available: definitions
                .iter()
                .any(|definition| crate::strategy::is_validation_tool_name(&definition.name)),
            delegation_allowed: self.policy().policy().allow_delegate,
            task_graph: self.tasks.lock().expect("task graph poisoned").clone(),
            tool_definitions: definitions,
        };
        let guidance = capability_aware_guidance(&strategy_context, &self.policy());
        self.events.record(OrchestratorEvent::StrategySelected {
            strategy: guidance.decision.strategy,
            reason: guidance.decision.reason,
        });
        let mut prompt = prompt;
        let budget = self.budget.lock().expect("budget tracker poisoned").snapshot();
        append_turn_guidance(&mut prompt, &guidance.text, Some(&budget));
        self.run_loop_with_options(
            ctx,
            sink,
            client,
            cancel,
            task,
            Some(prompt),
            metadata.history,
            metadata.context_plan,
            self.events.clone(),
            LoopOptions {
                read_first: guidance.decision.strategy
                    == crate::strategy::TurnStrategy::ResearchThenEdit,
                graph: Some(self.tasks.clone()),
                available_models: self.models.advertised(),
                model_id: Some(DEFAULT_MODEL_ID.to_string()),
                ..LoopOptions::default()
            },
        )
        .await
    }
}

/// Appends strategy guidance and finite-budget pressure to the runtime plane.
fn append_turn_guidance(
    prompt: &mut PromptBundle,
    guidance: &str,
    budget: Option<&crate::budget::BudgetSnapshot>,
) {
    if let Some(runtime) = prompt.runtime.as_mut() {
        crate::prompt::runtime_context::append_turn_guidance(runtime, Some(guidance), budget);
        return;
    }
    let mut runtime = crate::prompt::context::RuntimeSnapshot::default();
    crate::prompt::runtime_context::append_turn_guidance(&mut runtime, Some(guidance), budget);
    if !runtime.text.trim().is_empty() {
        prompt.runtime = Some(runtime);
    }
}
