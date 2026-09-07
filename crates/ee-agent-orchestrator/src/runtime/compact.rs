//! `impl OrchestratorRuntime` methods: compact.
use super::*;

impl OrchestratorRuntime {
    /// Runs one strategic turn: selects a `TurnStrategy` from observed
    /// context, records the decision as an event, executes the strategy
    /// wrapper, and returns the ACP result together with the typed
    /// `FinalResponse`.  Strategy execution never bypasses the configured
    /// budget, policy, or cancellation gates.
    pub async fn run_turn_strategic(
        &self,
        ctx: PromptContext,
        input: StrategicInput,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
    ) -> Result<TurnResult, OrchestratorError> {
        self.run_turn_strategic_recording(ctx, input, sink, client, cancel, EventRecorder::new())
            .await
    }
    /// Same as [`OrchestratorRuntime::run_turn_strategic`] but records every
    /// loop decision (including `StrategySelected`) into `events`.
    pub(crate) async fn run_turn_strategic_recording(
        &self,
        ctx: PromptContext,
        input: StrategicInput,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        events: EventRecorder,
    ) -> Result<TurnResult, OrchestratorError> {
        let (title, description) = task_summary(&ctx);
        let (task, entries, task_graph) = {
            let mut tasks = self.tasks.lock().expect("task graph poisoned");
            let task = tasks.create_root(&title, &description);
            let entries = tasks.plan_entries();
            let graph = tasks.clone();
            (task, entries, graph)
        };
        // Plan emission precedes any tool work: the client sees the task
        // graph before the first execution.
        sink.plan_replace(entries).map_err(|error| {
            OrchestratorError::InvalidState(format!("plan emission failed: {error}"))
        })?;
        // Fresh slice: strategic turns re-anchor the deadline too.
        self.budget
            .lock()
            .expect("budget tracker poisoned")
            .reset_deadline(self.config.turn_timeout);
        let prompt_text = ctx
            .prompt
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        let context_plan: Option<ContextPlan> = input
            .context
            .as_ref()
            .map(|context| ContextPlanner.plan(context, &ContextPlannerConfig::default()));
        // Transaction evidence is host-observed and immutable within this
        // strategic turn. Keep it separate from model/tool prose, then use it
        // to prevent unsupported verified completion claims below.
        let write_transaction = input.write_transaction.clone();
        let strategy_ctx = StrategyContext {
            prompt_text,
            has_code_changes: input.has_code_changes,
            validation_tools_available: input.validation_tools_available,
            delegation_allowed: self.policy().policy().allow_delegate,
            task_graph: task_graph.clone(),
            tool_definitions: self.tools.lock().expect("tool registry poisoned").definitions(),
        };
        let decision = StrategySelector.select(&strategy_ctx);
        events.record(OrchestratorEvent::StrategySelected {
            strategy: decision.strategy,
            reason: decision.reason,
        });
        let memory = self.memory.lock().expect("memory store poisoned").compact_context();
        let execution_log: Arc<Mutex<Vec<ToolExecutionLogEntry>>> =
            Arc::new(Mutex::new(Vec::new()));
        let run = StrategyRun {
            task: task.clone(),
            task_graph,
            sink,
            client,
            cancel,
            execution_log: execution_log.clone(),
        };
        let mut validation = ValidationRecorder::new();
        let model = self.models.default_adapter()?;
        let policy = self.policy();
        let executor = StrategyExecutor::new(
            self.config.clone(),
            model,
            self.tools.clone(),
            self.budget.clone(),
            policy,
            self.tasks.clone(),
            events.clone(),
        );
        let (prompt_result, reflection) = executor
            .execute(decision.strategy, ctx, memory, context_plan.as_ref(), run, &mut validation)
            .await?;
        let log = execution_log.lock().expect("execution log poisoned").clone();
        let changed_files = changed_files_from_log(&log, &task.id);
        let progress = ProgressTracker::from_execution_log(
            &log,
            &validation,
            (reflection.review_calls > 0).then_some(reflection.findings.len()),
        )
        .score(&self.tasks.lock().expect("task graph poisoned").clone());
        let mut final_response = FinalResponseBuilder {
            changed_files,
            validation: &validation,
            // Host-provided evidence is optional; absence deliberately keeps
            // the final response unverified rather than fabricating facts.
            completion_evidence: input.completion_evidence.as_ref(),
            unresolved_risks: Vec::new(),
            follow_up_suggestions: Vec::new(),
            task_graph: &self.tasks.lock().expect("task graph poisoned").clone(),
            memory: &self.memory.lock().expect("memory store poisoned").clone(),
            progress: Some(&progress),
        }
        .build();
        if let Some(transaction) = &write_transaction {
            transaction.constrain_completion(&mut final_response.completion);
            final_response.can_finish = final_response.completion.is_verified();
        }
        Ok(TurnResult {
            context_plan,
            prompt_result,
            strategy: decision,
            final_response,
            write_transaction,
            reflection,
        })
    }
    /// Runs one `/compact` turn without provider-owned conversation history.
    pub async fn run_compact_turn(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        cancel: watch::Receiver<bool>,
        instructions: Option<String>,
        system_context: Option<String>,
    ) -> Result<PromptResult, OrchestratorError> {
        self.run_compact_turn_with_history(
            ctx,
            sink,
            cancel,
            instructions,
            system_context,
            Vec::new(),
        )
        .await
    }
    /// Runs one `/compact` turn with bounded provider-owned conversation
    /// history included in compaction context. Memory changes commit only
    /// after model response, usage validation, summary validation, and staged
    /// summary insertion all succeed.
    pub async fn run_compact_turn_with_history(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        cancel: watch::Receiver<bool>,
        instructions: Option<String>,
        system_context: Option<String>,
        recent_messages: Vec<ModelMessage>,
    ) -> Result<PromptResult, OrchestratorError> {
        self.run_compact_turn_recording(
            ctx,
            sink,
            cancel,
            instructions,
            system_context,
            recent_messages,
            self.events.clone(),
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_compact_turn_recording(
        &self,
        _ctx: PromptContext,
        sink: UpdateSink,
        cancel: watch::Receiver<bool>,
        instructions: Option<String>,
        system_context: Option<String>,
        recent_messages: Vec<ModelMessage>,
        events: EventRecorder,
    ) -> Result<PromptResult, OrchestratorError> {
        if *cancel.borrow() {
            return Err(OrchestratorError::Cancellation);
        }
        // Stage every memory mutation. Failed compaction leaves live memory
        // byte-for-byte unchanged.
        let original_memory = self.memory.lock().expect("memory store poisoned").clone();
        let mut staged_memory = original_memory.clone();
        let deterministic = compact_memory(&mut staged_memory, &self.config.compaction.memory);
        let tasks = self.tasks.lock().expect("task graph poisoned").clone();
        let budget_snapshot = self.budget.lock().expect("budget tracker poisoned").snapshot();
        let recent_events = events.events();
        let context = build_compaction_context(
            &tasks,
            &staged_memory,
            &recent_messages,
            &recent_events,
            &budget_snapshot,
            self.config.compaction.max_input_bytes,
        );
        // 3. One model call, no tools, bounded by the per-turn timeout and
        //    observing cancellation before and after.
        let mut system = build_compaction_prompt(instructions.as_deref());
        if !context.is_empty() {
            system.push_str("\n\nSession context:\n");
            system.push_str(&context);
        }
        if let Some(session) = system_context {
            system.push_str("\n\n");
            system.push_str(&session);
        }
        let user_prompt =
            instructions.as_deref().filter(|text| !text.trim().is_empty()).map_or_else(
                || "Compress the session into a continuation summary.".to_string(),
                str::to_string,
            );
        let transcript = vec![
            ModelMessage::text(ModelRole::System, system),
            ModelMessage::text(ModelRole::User, user_prompt),
        ];
        let task = TaskNode::new(
            TaskId::new("compact-session"),
            "compact session",
            "LLM session compaction summary",
        );
        let model = self.models.default_adapter()?;
        // Fresh slice: the compaction model call gets the full per-turn
        // timeout, regardless of how long the session idled.
        self.budget
            .lock()
            .expect("budget tracker poisoned")
            .reset_deadline(self.config.turn_timeout);
        // The compaction model call consumes budget like any other call;
        // budget exhaustion or token caps fail closed.
        {
            let mut budget = self.budget.lock().expect("budget tracker poisoned");
            budget.try_reserve_model_call()?;
            budget.emit(&events);
        }
        events.record(OrchestratorEvent::ModelRequested { iteration: 1 });
        let request = ModelRequest::new(transcript, Vec::new(), budget_snapshot, task);
        let response = match tokio::time::timeout(
            self.config.turn_timeout,
            model.complete(request, cancel.clone()),
        )
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => return Err(error.into()),
            Err(_) => {
                return Err(OrchestratorError::Timeout(
                    "compaction model call exceeded the turn timeout".into(),
                ));
            }
        };
        events.record(OrchestratorEvent::ModelResponded { iteration: 1 });
        {
            let mut budget = self.budget.lock().expect("budget tracker poisoned");
            budget.record_model_usage(
                response.text.len(),
                response.usage.input_tokens,
                response.usage.output_tokens,
            )?;
            budget.emit(&events);
        }
        // Context-window usage after the compaction model call; unknown
        // usage emits nothing.
        if let Some(input_tokens) = response.usage.input_tokens {
            let _ = sink.raw_update(SessionUpdate::UsageUpdate(UsageUpdate::new(
                input_tokens as u64,
                self.config.context_window_tokens,
            )));
        }
        if *cancel.borrow() {
            return Err(OrchestratorError::Cancellation);
        }
        let summary = response.text.trim();
        if summary.is_empty() {
            return Err(OrchestratorError::InvalidState(
                "compaction summary was empty; memory unchanged".into(),
            ));
        }
        // 4. Store the redacted summary as model-derived session memory;
        //    insertion is additive and never touches protected keys.
        let guard = SensitiveDataGuard::new();
        let item = MemoryItem::new(SESSION_SUMMARY_KEY, guard.redact(summary));
        let summary_bytes = item.byte_size();
        staged_memory.insert(item).map_err(|error| {
            OrchestratorError::InvalidState(format!("failed to store compaction summary: {error}"))
        })?;
        {
            let mut memory = self.memory.lock().expect("memory store poisoned");
            if *memory != original_memory {
                return Err(OrchestratorError::InvalidState(
                    "memory changed during compaction; retry to avoid overwriting newer facts"
                        .into(),
                ));
            }
            *memory = staged_memory;
        }
        let retained_context_bytes = context.len();
        let report = CompactTurnReport {
            merged_duplicates: deterministic.merged_duplicates,
            decayed_observations: deterministic.decayed_observations,
            preserved_protected: deterministic.preserved_protected,
            summary_bytes,
            retained_context_bytes,
        };
        let _ = sink.agent_message_chunk("compact-report", report.to_status_text());
        Ok(prompt_result_with_usage(ee_agent_protocol::StopReason::EndTurn, response.usage))
    }
}
