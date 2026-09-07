//! `impl OrchestratorRuntime` methods: strategic.
use super::*;

impl OrchestratorRuntime {
    /// Runs the production recovery path and builds an evidence-gated final
    /// response only after the recovery engine reaches terminal completion.
    ///
    /// This deliberately delegates all loop/checkpoint work to
    /// [`OrchestratorRuntime::run_turn_recoverable`]. The strategic wrapper
    /// therefore preserves timeout, cancellation, durable checkpoint,
    /// completed-tool reuse, and interruption semantics without creating a
    /// second root task or replaying side effects.
    pub async fn run_turn_strategic_recoverable(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        recovery: StrategicRecoveryContext,
    ) -> Result<StrategicTurnOutcome, OrchestratorError> {
        self.run_turn_strategic_recoverable_with_history(
            ctx,
            sink,
            client,
            cancel,
            recovery,
            Vec::new(),
        )
        .await
    }
    pub(crate) async fn run_turn_strategic_recoverable_with_history(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        recovery: StrategicRecoveryContext,
        history: Vec<ModelMessage>,
    ) -> Result<StrategicTurnOutcome, OrchestratorError> {
        self.set_validation_planning_input(&recovery.input);
        let mut metadata = recovery_turn_metadata(&recovery.input);
        metadata.history = history;
        match self
            .run_turn_recoverable_with_metadata(
                ctx,
                sink,
                client,
                cancel,
                recovery.system_context,
                &recovery.provider,
                metadata,
            )
            .await?
        {
            TurnOutcome::Completed(prompt_result) => {
                Ok(StrategicTurnOutcome::Completed(Box::new(StrategicRecoveryTurn {
                    prompt_result,
                    final_response: self.build_recovery_final_response(&recovery.input),
                })))
            }
            TurnOutcome::Interrupted(interruption) => {
                Ok(StrategicTurnOutcome::Interrupted(interruption))
            }
        }
    }
    /// Resumes an interrupted production turn and assembles final evidence only
    /// once the restored recovery path has actually completed.
    ///
    /// Completed tool calls continue to be reused by [`OrchestratorRuntime::resume_turn`].
    pub async fn resume_turn_strategic(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        recovery: StrategicRecoveryContext,
    ) -> Result<StrategicTurnOutcome, OrchestratorError> {
        self.set_validation_planning_input(&recovery.input);
        let metadata = recovery_turn_metadata(&recovery.input);
        match self
            .resume_turn_with_metadata(
                ctx,
                sink,
                client,
                cancel,
                recovery.system_context,
                &recovery.provider,
                metadata,
            )
            .await?
        {
            TurnOutcome::Completed(prompt_result) => {
                Ok(StrategicTurnOutcome::Completed(Box::new(StrategicRecoveryTurn {
                    prompt_result,
                    final_response: self.build_recovery_final_response(&recovery.input),
                })))
            }
            TurnOutcome::Interrupted(interruption) => {
                Ok(StrategicTurnOutcome::Interrupted(interruption))
            }
        }
    }
    /// Builds completion from host-provided evidence only. The recovery loop
    /// does not fabricate editor facts or copy model prose into evidence, so
    /// missing host evidence remains explicitly unverified.
    pub(super) fn build_recovery_final_response(&self, input: &StrategicInput) -> FinalResponse {
        let validation =
            self.last_turn_validation.lock().expect("last turn validation poisoned").clone();
        let changed_files =
            self.last_turn_changed_files.lock().expect("last turn changed files poisoned").clone();
        let tasks = self.tasks.lock().expect("task graph poisoned").clone();
        let memory = self.memory.lock().expect("memory store poisoned").clone();
        let mut final_response = FinalResponseBuilder {
            changed_files,
            validation: &validation,
            completion_evidence: input.completion_evidence.as_ref(),
            unresolved_risks: Vec::new(),
            follow_up_suggestions: Vec::new(),
            task_graph: &tasks,
            memory: &memory,
            progress: None,
        }
        .build();
        if let Some(transaction) = &input.write_transaction {
            transaction.constrain_completion(&mut final_response.completion);
            final_response.can_finish = final_response.completion.is_verified();
        }
        if let Some(revision) =
            input.completion_evidence.as_ref().map(|evidence| evidence.revision.as_str())
        {
            self.rubber_duck.constrain_completion(revision, &mut final_response.completion);
            final_response.can_finish = final_response.completion.is_verified();
            if !final_response.can_finish
                && let Some(blocker) = final_response.completion.blocker.clone()
                && blocker.starts_with("unresolved current-revision rubber-duck finding")
            {
                final_response.unresolved_risks.push(blocker);
            }
        }
        if let Some(reason) = *self.last_repair_stop.lock().expect("last repair stop poisoned") {
            final_response.completion.state = crate::completion::CompletionState::Blocked;
            final_response.completion.blocker =
                Some(format!("automatic repair stopped: {}", repair_stop_label(reason)));
            final_response.completion.safe_follow_up =
                Some(repair_safe_follow_up(reason).to_string());
            final_response
                .unresolved_risks
                .push(format!("automatic repair stopped: {}", repair_stop_label(reason)));
            final_response.follow_up_suggestions.push(repair_safe_follow_up(reason).to_string());
            final_response.can_finish = false;
        }
        final_response
    }
    /// Sets trusted, bounded inputs used only to select registered validation tools.
    pub(super) fn set_validation_planning_input(&self, input: &StrategicInput) {
        *self.validation_workspace.lock().expect("validation workspace poisoned") =
            input.validation_workspace.clone();
        *self.validation_changed_symbols.lock().expect("validation symbols poisoned") =
            input.changed_symbols.iter().take(64).cloned().collect();
    }
    /// Plans and executes focused validation after a completed write sequence.
    /// Every command still goes through the shared executor, so policy,
    /// approval, cancellation, timeout, output cap, and terminal ownership
    /// stay enforced by their existing implementations.
    pub(super) async fn run_post_write_validation(
        &self,
        execution_log: Vec<ToolExecutionLogEntry>,
        task: &TaskNode,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
    ) -> Result<PostWriteValidation, OrchestratorError> {
        let changed_files = changed_files_from_log(&execution_log, &task.id);
        let definitions = self.tools.lock().expect("tool registry poisoned").definitions();
        let workspace =
            self.validation_workspace.lock().expect("validation workspace poisoned").clone();
        let changed_symbols =
            self.validation_changed_symbols.lock().expect("validation symbols poisoned").clone();
        let planner = ValidationPlanner::new();
        let plan = planner.plan_with_context(
            ValidationPlanningContext {
                changed_files: &changed_files,
                changed_symbols: &changed_symbols,
                workspace: &workspace,
            },
            &definitions,
        );
        let mut validation = ValidationRecorder::new();
        let mut results = Vec::new();
        if !plan.is_empty() {
            let task_ids = {
                let mut graph = self.tasks.lock().expect("task graph poisoned");
                planner.create_tasks(&mut graph, &plan, &task.id)?
            };
            let executor = ToolExecutor::new(
                self.config.clone(),
                self.tools.clone(),
                self.budget.clone(),
                self.policy(),
                0,
                self.events.clone(),
            )
            .with_model_id(Some(DEFAULT_MODEL_ID.to_string()));
            let mut runner = ValidationRunner::new(executor, self.events.clone());
            results = runner
                .run_plan(&plan, &task_ids, &sink, &client, cancel, task, &[], &mut validation)
                .await?;
            let mut graph = self.tasks.lock().expect("task graph poisoned");
            finalize_validation_tasks(&mut graph, &task_ids, &results)?;
        }
        *self.last_turn_changed_files.lock().expect("last turn changed files poisoned") =
            changed_files;
        *self.last_turn_validation.lock().expect("last turn validation poisoned") = validation;
        Ok(PostWriteValidation { results })
    }
    /// Runs at most the configured number of repair loops after a current
    /// validation, diagnostic, or conflict-diff failure. Each attempt refreshes
    /// read-only host facts first, reuses this turn's root task and budgets, and
    /// never converts server observations into host completion evidence.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_bounded_repair(
        &self,
        ctx: &PromptContext,
        task: &TaskNode,
        execution_log: Arc<Mutex<Vec<ToolExecutionLogEntry>>>,
        mut validation: PostWriteValidation,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        system_context: Option<String>,
        session_id: &str,
        provider: &str,
    ) -> Result<(), OrchestratorError> {
        let mut controller = RepairController::new(self.config.repair);
        let mut previous_write_count =
            successful_write_count(&execution_log.lock().expect("execution log poisoned"));
        if previous_write_count == 0 {
            return Ok(());
        }

        loop {
            self.invalidate_repair_context(session_id, true);
            let snapshot = match self
                .collect_repair_context(task, &sink, &client, cancel.clone(), session_id)
                .await
            {
                Ok(snapshot) => snapshot,
                Err(reason) => {
                    self.finish_repair(reason);
                    return Ok(());
                }
            };
            let recorded_validation =
                self.last_turn_validation.lock().expect("last turn validation poisoned").clone();
            let Some((reason, failure, evidence_ids)) =
                repair_request(&snapshot, &validation, &recorded_validation)
            else {
                return Ok(());
            };
            let write_count =
                successful_write_count(&execution_log.lock().expect("execution log poisoned"));
            let progress = RepairProgress {
                context_revision: snapshot.revision.clone(),
                diff_fingerprint: snapshot.diff_fingerprint.clone(),
                validation_fingerprint: repair_validation_fingerprint(&validation),
                made_progress: controller.attempts().is_empty()
                    || write_count > previous_write_count,
                ..RepairProgress::default()
            };
            let attempt = match controller.starts(reason, failure, evidence_ids, progress) {
                RepairDecision::Attempt(attempt) => attempt,
                RepairDecision::Stop(reason) => {
                    self.finish_repair(reason);
                    return Ok(());
                }
            };
            self.events.record(OrchestratorEvent::RepairStarted {
                attempt_number: attempt.attempt_number,
                reason: repair_reason_label(attempt.reason).to_string(),
            });
            let plan = self.plan_repair_context(&task.id, &snapshot.input);
            let repair_context = repair_system_context(&snapshot, &attempt);
            let repair_log_start = execution_log.lock().expect("execution log poisoned").len();
            let outcome = self
                .run_repair_loop(
                    ctx.clone(),
                    task.clone(),
                    sink.clone(),
                    client.clone(),
                    cancel.clone(),
                    system_context.clone(),
                    repair_context,
                    plan,
                    execution_log.clone(),
                    session_id,
                    provider,
                )
                .await;
            match outcome {
                Ok(()) => {}
                Err(OrchestratorError::Cancellation) => {
                    return Err(OrchestratorError::Cancellation);
                }
                Err(OrchestratorError::DeadlineExceeded(_))
                | Err(OrchestratorError::Timeout(_)) => {
                    self.finish_repair(RepairStopReason::Timeout);
                    return Ok(());
                }
                Err(OrchestratorError::BudgetExceeded(_)) => {
                    self.finish_repair(RepairStopReason::BudgetExhaustion);
                    return Ok(());
                }
                Err(OrchestratorError::PolicyDenied(_)) => {
                    self.finish_repair(RepairStopReason::PolicyDenial);
                    return Ok(());
                }
                Err(_) => {
                    self.finish_repair(RepairStopReason::UnavailableEnvironment);
                    return Ok(());
                }
            }
            let repair_log = execution_log.lock().expect("execution log poisoned").clone();
            let repaired_write_count = successful_write_count(&repair_log);
            let observed_progress = RepairProgress {
                context_revision: snapshot.revision,
                diff_fingerprint: snapshot.diff_fingerprint,
                validation_fingerprint: repair_validation_fingerprint(&validation),
                tool_call_fingerprints: repair_log[repair_log_start..]
                    .iter()
                    .map(|entry| format!("tool:{}", entry.tool_name))
                    .collect(),
                made_progress: repaired_write_count > write_count,
                ..RepairProgress::default()
            };
            if let Some(reason) = controller.record_progress(observed_progress) {
                self.finish_repair(reason);
                return Ok(());
            }

            self.invalidate_repair_context(session_id, false);
            previous_write_count = write_count;
            let post_repair_log = execution_log.lock().expect("execution log poisoned").clone();
            validation = self
                .run_post_write_validation(
                    post_repair_log,
                    task,
                    sink.clone(),
                    client.clone(),
                    cancel.clone(),
                )
                .await?;
        }
    }
    /// Collects a new snapshot using only existing registered read-only editor
    /// MCP tools. No command, approval, or write tool is invoked here.
    pub(super) async fn collect_repair_context(
        &self,
        task: &TaskNode,
        sink: &UpdateSink,
        client: &ClientBridge,
        cancel: watch::Receiver<bool>,
        session_id: &str,
    ) -> Result<RepairContextSnapshot, RepairStopReason> {
        let executor = ToolExecutor::new(
            self.config.clone(),
            self.tools.clone(),
            self.budget.clone(),
            self.policy(),
            0,
            self.events.clone(),
        )
        .with_model_id(Some(DEFAULT_MODEL_ID.to_string()));
        let mut observations = Vec::new();
        for (index, tool_name) in REPAIR_CONTEXT_TOOLS.iter().enumerate() {
            let intent = crate::tools::ToolIntent::new(
                format!("{REPAIR_CONTEXT_TOOL_CALL_PREFIX}-{index}"),
                *tool_name,
                serde_json::json!({}),
            );
            let result = match executor
                .execute(&intent, sink, client, cancel.clone(), task, &[])
                .await
            {
                Ok(result) => result,
                Err(OrchestratorError::Cancellation) => return Err(RepairStopReason::Cancellation),
                Err(OrchestratorError::BudgetExceeded(_)) => {
                    return Err(RepairStopReason::BudgetExhaustion);
                }
                Err(OrchestratorError::DeadlineExceeded(_))
                | Err(OrchestratorError::Timeout(_)) => {
                    return Err(RepairStopReason::Timeout);
                }
                Err(OrchestratorError::PolicyDenied(_)) => {
                    return Err(RepairStopReason::PolicyDenial);
                }
                Err(_) => return Err(RepairStopReason::UnavailableEnvironment),
            };
            observations
                .push(RepairContextObservation { tool_name: (*tool_name).to_string(), result });
        }
        build_repair_context(session_id, &observations)
    }
    pub(super) fn plan_repair_context(
        &self,
        task_id: &TaskId,
        input: &crate::context_planner::ContextPlanningInput,
    ) -> ContextPlan {
        let config = ContextPlannerConfig::default();
        let mut cache = self.context_cache.lock().expect("context plan cache poisoned");
        if let Some(plan) = cache.get(task_id.as_str(), input, &config) {
            return plan;
        }
        let plan = ContextPlanner.plan(input, &config);
        cache.insert(task_id.as_str(), input, &config, plan.clone());
        plan
    }
    /// Conservatively invalidate every stale source class before a fresh host
    /// snapshot. Cache invalidation is cheap and stronger than selectively
    /// trusting state that may have changed outside this process.
    pub(super) fn invalidate_repair_context(&self, session_id: &str, after_write: bool) {
        if after_write {
            self.invalidate_context(ContextInvalidation::Write {
                session_id: session_id.to_string(),
            });
        }
        for invalidation in [
            ContextInvalidation::BufferRevision { session_id: session_id.to_string() },
            ContextInvalidation::DiagnosticsRevision { session_id: session_id.to_string() },
            ContextInvalidation::WorktreeRevision { session_id: session_id.to_string() },
            ContextInvalidation::CheckoutRevision { session_id: session_id.to_string() },
            ContextInvalidation::ValidationResult { session_id: session_id.to_string() },
            ContextInvalidation::GraphRevision { session_id: session_id.to_string() },
        ] {
            self.invalidate_context(invalidation);
        }
    }
    /// Runs a repair loop against the existing root task and shared counters.
    /// The primary turn deadline remains in force; only the remaining duration
    /// is given to this loop's outer timeout.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_repair_loop(
        &self,
        ctx: PromptContext,
        task: TaskNode,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        system_context: Option<String>,
        repair_context: String,
        context_plan: ContextPlan,
        execution_log: Arc<Mutex<Vec<ToolExecutionLogEntry>>>,
        session_id: &str,
        provider: &str,
    ) -> Result<(), OrchestratorError> {
        let remaining = self
            .budget
            .lock()
            .expect("budget tracker poisoned")
            .deadline_remaining()
            .unwrap_or(self.config.turn_timeout);
        if remaining.is_zero() {
            return Err(OrchestratorError::DeadlineExceeded(
                "repair budget deadline elapsed".into(),
            ));
        }
        let mut config = self.config.clone();
        config.turn_timeout = remaining;
        let handle = CheckpointHandle::new(self.checkpoints.clone(), session_id, provider);
        let model = self.models.default_adapter()?;
        let engine = LoopEngine::new(
            config,
            model,
            self.tools.clone(),
            self.budget.clone(),
            self.policy(),
            self.events.clone(),
            LoopOptions {
                graph: Some(self.tasks.clone()),
                available_models: self.models.advertised(),
                model_id: Some(DEFAULT_MODEL_ID.to_string()),
                memory: Some(self.memory.clone()),
                checkpoint: Some(handle),
                execution_log: Some(execution_log),
                ..LoopOptions::default()
            },
        );
        let memory = self.memory.lock().expect("memory store poisoned").compact_context();
        let mut session = system_context.unwrap_or_default();
        if !session.is_empty() {
            session.push_str("\n\n");
        }
        session.push_str(&repair_context);
        engine
            .run_with_system_context(
                ctx,
                sink,
                client,
                cancel,
                task,
                TurnSystemContext {
                    history: Vec::new(),
                    memory,
                    session: Some(session),
                    task_context: Some(context_plan),
                },
            )
            .await
            .map(|_| ())
    }
    pub(super) fn finish_repair(&self, reason: RepairStopReason) {
        *self.last_repair_stop.lock().expect("last repair stop poisoned") = Some(reason);
        self.events.record(OrchestratorEvent::RepairStopped {
            reason: repair_stop_label(reason).to_string(),
        });
    }
}
