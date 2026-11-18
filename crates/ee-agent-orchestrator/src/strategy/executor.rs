//! The strategy execution wrappers.

use super::*;

impl StrategyExecutor {
    pub(crate) fn new(
        config: OrchestratorConfig,
        model: Arc<dyn ModelAdapter>,
        tools: Arc<Mutex<ToolRegistry>>,
        budget: Arc<Mutex<BudgetTracker>>,
        policy: PolicyEngine,
        tasks: Arc<Mutex<TaskGraph>>,
        events: EventRecorder,
    ) -> Self {
        Self {
            config,
            model,
            tools,
            budget,
            policy,
            tasks,
            events,
            session_context: crate::session_context::SessionContext::default(),
        }
    }

    /// Seeds the provider-facing session affinity for every request the turn
    /// makes. A runtime without a recorded context leaves the default.
    #[must_use]
    pub(crate) fn with_session_context(
        mut self,
        context: crate::session_context::SessionContext,
    ) -> Self {
        self.session_context = context;
        self
    }

    /// Runs one strategic turn: builds the transcript, then dispatches to the
    /// strategy wrapper.  `validation` collects recorded outcomes for
    /// validate-then-review; `run` carries the task, graph, and plumbing.
    /// Loop strategies run the bounded reflection pass when enabled and
    /// return its outcome.
    pub(crate) async fn execute(
        &self,
        strategy: TurnStrategy,
        prompt: PromptContext,
        memory: Option<String>,
        context_plan: Option<&ContextPlan>,
        run: StrategyRun,
        validation: &mut ValidationRecorder,
    ) -> Result<(PromptResult, ReflectionOutcome), OrchestratorError> {
        let mut transcript = Transcript::from_prompt(&prompt);
        if let Some(plan) = context_plan {
            plan.apply_to_transcript(&mut transcript);
        }
        if let Some(facts) = &memory {
            transcript.prepend_system(format!("Memory facts:\n{facts}"));
        }
        let definitions = self.tools.lock().expect("tool registry poisoned").definitions();
        if strategy == TurnStrategy::ResearchThenEdit && has_web_context_tools(&definitions) {
            transcript.prepend_system(WEB_RESEARCH_GUIDANCE);
        }
        if matches!(
            strategy,
            TurnStrategy::ToolLoop
                | TurnStrategy::PlanThenExecute
                | TurnStrategy::ResearchThenEdit
                | TurnStrategy::ValidateThenReview
        ) && has_file_search_tools(&definitions)
        {
            transcript.prepend_system(SEARCH_GUIDANCE);
        }
        let session_id = prompt.session_id.to_string();
        match strategy {
            TurnStrategy::SimpleAnswer => self
                .run_simple_answer(transcript, session_id, run)
                .await
                .map(|result| (result, ReflectionOutcome::default())),
            TurnStrategy::ToolLoop | TurnStrategy::PlanThenExecute => {
                let (result, mut transcript) = self
                    .run_loop(
                        LoopMode::Standard,
                        false,
                        transcript,
                        session_id.clone(),
                        run.clone(),
                    )
                    .await?;
                let reflection = self
                    .run_reflection_phase(&session_id, &mut transcript, &run, validation)
                    .await?;
                Ok((result, reflection))
            }
            TurnStrategy::ResearchThenEdit => {
                let (result, mut transcript) = self
                    .run_loop(LoopMode::Standard, true, transcript, session_id.clone(), run.clone())
                    .await?;
                let reflection = self
                    .run_reflection_phase(&session_id, &mut transcript, &run, validation)
                    .await?;
                Ok((result, reflection))
            }
            TurnStrategy::ValidateThenReview => {
                self.run_validate_then_review(transcript, session_id, run, validation).await
            }
            TurnStrategy::ParallelDelegation => self
                .run_parallel_delegation(transcript, session_id, run)
                .await
                .map(|result| (result, ReflectionOutcome::default())),
        }
    }

    /// `SimpleAnswer`: one model call, no tool execution; tool intents fail
    /// closed.  Uses the engine's `SimpleAnswer` mode so budget, policy, and
    /// cancellation checks stay identical to the standard path.
    async fn run_simple_answer(
        &self,
        transcript: Transcript,
        session_id: String,
        run: StrategyRun,
    ) -> Result<PromptResult, OrchestratorError> {
        self.run_loop(LoopMode::SimpleAnswer, false, transcript, session_id, run)
            .await
            .map(|(result, _)| result)
    }

    /// Standard or read-first loop over the shared engine.
    async fn run_loop(
        &self,
        mode: LoopMode,
        read_first: bool,
        mut transcript: Transcript,
        session_id: String,
        run: StrategyRun,
    ) -> Result<(PromptResult, Transcript), OrchestratorError> {
        let options = LoopOptions {
            mode,
            read_first,
            execution_log: Some(run.execution_log.clone()),
            graph: Some(self.tasks.clone()),
            session_context: Some(self.session_context.clone()),
            ..LoopOptions::default()
        };
        let engine = LoopEngine::new(
            self.config.clone(),
            self.model.clone(),
            self.tools.clone(),
            self.budget.clone(),
            self.policy.clone(),
            self.events.clone(),
            options,
        );
        engine
            .run_transcript(&mut transcript, session_id, run.sink, run.client, run.cancel, run.task)
            .await
            .map(|result| (result, transcript))
    }

    /// `ValidateThenReview`: run the edit loop, then a bounded review phase
    /// that may only execute validation-capable tools, recording every
    /// outcome into `validation`, then the reflection pass when enabled.
    async fn run_validate_then_review(
        &self,
        transcript: Transcript,
        session_id: String,
        run: StrategyRun,
        validation: &mut ValidationRecorder,
    ) -> Result<(PromptResult, ReflectionOutcome), OrchestratorError> {
        let (result, transcript) = self
            .run_loop(LoopMode::Standard, false, transcript, session_id.clone(), run.clone())
            .await?;
        let mut transcript = self.run_review_phase(transcript, run.clone(), validation).await?;
        let reflection =
            self.run_reflection_phase(&session_id, &mut transcript, &run, validation).await?;
        Ok((result, reflection))
    }

    /// `ParallelDelegation`: re-check the selector preconditions (independent
    /// children, delegation policy) before running the loop; delegate intents
    /// flow through the shared subagent manager.
    async fn run_parallel_delegation(
        &self,
        transcript: Transcript,
        session_id: String,
        run: StrategyRun,
    ) -> Result<PromptResult, OrchestratorError> {
        if !has_independent_children(&run.task_graph) {
            return Err(OrchestratorError::InvalidState(
                "ParallelDelegation requires at least two independent pending child tasks".into(),
            ));
        }
        if !self.policy.policy().allow_delegate {
            return Err(OrchestratorError::PolicyDenied(
                "ParallelDelegation requires delegate policy allowance".into(),
            ));
        }
        self.run_loop(LoopMode::Standard, false, transcript, session_id, run)
            .await
            .map(|(result, _)| result)
    }

    /// One model call after the edit loop; validation tool intents execute
    /// and record outcomes.  Cancellation and budget checks mirror the main
    /// loop; non-validation intents fail closed.  Returns the transcript so
    /// the reflection pass can continue from it.
    async fn run_review_phase(
        &self,
        mut transcript: Transcript,
        run: StrategyRun,
        validation: &mut ValidationRecorder,
    ) -> Result<Transcript, OrchestratorError> {
        if *run.cancel.borrow() {
            return Err(OrchestratorError::Cancellation);
        }
        let budget_snapshot = {
            let mut budget = self.budget.lock().expect("budget tracker poisoned");
            budget.try_reserve_iteration()?;
            budget.try_reserve_model_call()?;
            budget.check_output_allowance()?;
            let snapshot = budget.snapshot();
            budget.emit(&self.events);
            snapshot
        };
        let tools = self.tools.lock().expect("tool registry poisoned").definitions();
        let prepared = crate::prompt_injection::prepare_request(&transcript.request_messages());
        for detection in &prepared.detections {
            self.events.record(OrchestratorEvent::SuspiciousContentDetected {
                trust: detection.trust,
                pattern: detection.pattern.clone(),
                excerpt: detection.excerpt.clone(),
            });
        }
        let request =
            ModelRequest::new(prepared.messages, tools, budget_snapshot, run.task.clone());
        self.events.record(OrchestratorEvent::ModelRequested {
            iteration: budget_snapshot.iterations_used,
        });
        let response = match self.model.complete(request, run.cancel.clone()).await {
            Ok(response) => response,
            Err(error) => {
                self.events.record(OrchestratorEvent::Error { error: error.to_string() });
                return Err(error.into());
            }
        };
        self.events.record(OrchestratorEvent::ModelResponded {
            iteration: budget_snapshot.iterations_used,
        });
        if *run.cancel.borrow() {
            return Err(OrchestratorError::Cancellation);
        }
        {
            let mut budget = self.budget.lock().expect("budget tracker poisoned");
            let output_bytes =
                response.text.len() + response.reasoning.as_deref().map_or(0, str::len);
            budget.record_model_usage(
                output_bytes,
                response.usage.input_tokens,
                response.usage.output_tokens,
            )?;
            budget.emit(&self.events);
        }
        let executor = ToolExecutor::new(
            self.config.clone(),
            self.tools.clone(),
            self.budget.clone(),
            self.policy.clone(),
            0,
            self.events.clone(),
        );
        for intent in response.tool_intents {
            if !is_validation_tool(&intent, &self.tools) {
                return Err(OrchestratorError::ModelFailure(format!(
                    "review phase may only execute validation tools, got {}",
                    intent.name
                )));
            }
            self.events.record(OrchestratorEvent::ToolStarted {
                tool_call_id: intent.tool_call_id.clone(),
                tool_name: intent.name.clone(),
            });
            let executed = executor
                .execute(
                    &intent,
                    &run.sink,
                    &run.client,
                    run.cancel.clone(),
                    &run.task,
                    transcript.messages(),
                )
                .await;
            let success = executed.as_ref().is_ok_and(|result| result.success);
            self.events.record(OrchestratorEvent::ToolFinished {
                tool_call_id: intent.tool_call_id.clone(),
                tool_name: intent.name.clone(),
                success,
            });
            let command = intent.name.clone();
            match executed {
                Ok(result) => {
                    validation.record(
                        command.clone(),
                        if result.success {
                            ValidationOutcome::Passed
                        } else {
                            ValidationOutcome::Failed
                        },
                        Some(result.summary_text()),
                        Some(run.task.id.clone()),
                    );
                    log_execution(
                        &run.execution_log,
                        &intent,
                        Some(result.success),
                        &result.summary_text(),
                    );
                    transcript.push_tool_result(intent.tool_call_id.clone(), result);
                }
                Err(error)
                    if error.is_cancellation()
                        || matches!(
                            error,
                            OrchestratorError::BudgetExceeded(_) | OrchestratorError::Timeout(_)
                        ) =>
                {
                    return Err(error);
                }
                Err(error) => {
                    validation.record(
                        command,
                        ValidationOutcome::Failed,
                        Some(error.to_string()),
                        Some(run.task.id.clone()),
                    );
                    log_execution(&run.execution_log, &intent, None, &error.to_string());
                }
            }
            if *run.cancel.borrow() {
                return Err(OrchestratorError::Cancellation);
            }
        }
        Ok(transcript)
    }

    /// Bounded reflection pass after a tool/edit loop: review model calls
    /// (evidence from the execution log, validation records, and task state)
    /// convert findings into task-graph items, and at most the configured
    /// number of fix loops runs.  Every review call costs a model-call budget
    /// slot and every fix loop a loop-iteration budget slot, so the pass can
    /// never exceed its configured limits.
    async fn run_reflection_phase(
        &self,
        session_id: &str,
        transcript: &mut Transcript,
        run: &StrategyRun,
        validation: &ValidationRecorder,
    ) -> Result<ReflectionOutcome, OrchestratorError> {
        let config = self.config.reflection;
        let mut outcome = ReflectionOutcome::default();
        if !config.enabled {
            return Ok(outcome);
        }
        let log = run.execution_log.lock().expect("execution log poisoned").clone();
        if changed_files_from_log(&log, &run.task.id).is_empty() && validation.records().is_empty()
        {
            // Nothing observed to review: stay silent instead of spending a
            // model call on empty evidence.
            return Ok(outcome);
        }
        let task_graph = self.tasks.lock().expect("task graph poisoned").clone();
        let context = build_review_context(&log, validation, &task_graph);
        loop {
            if outcome.review_calls >= config.max_review_iterations {
                break;
            }
            if *run.cancel.borrow() {
                return Err(OrchestratorError::Cancellation);
            }
            let snapshot = {
                let mut budget = self.budget.lock().expect("budget tracker poisoned");
                budget.try_reserve_iteration()?;
                budget.try_reserve_model_call()?;
                budget.check_output_allowance()?;
                let snapshot = budget.snapshot();
                budget.emit(&self.events);
                snapshot
            };
            let tools = self.tools.lock().expect("tool registry poisoned").definitions();
            let request =
                build_review_request(transcript, &context, tools, snapshot, run.task.clone());
            self.events
                .record(OrchestratorEvent::ModelRequested { iteration: snapshot.iterations_used });
            let response = match self.model.complete(request, run.cancel.clone()).await {
                Ok(response) => response,
                Err(error) => {
                    self.events.record(OrchestratorEvent::Error { error: error.to_string() });
                    return Err(error.into());
                }
            };
            self.events
                .record(OrchestratorEvent::ModelResponded { iteration: snapshot.iterations_used });
            if *run.cancel.borrow() {
                return Err(OrchestratorError::Cancellation);
            }
            {
                let mut budget = self.budget.lock().expect("budget tracker poisoned");
                let output_bytes =
                    response.text.len() + response.reasoning.as_deref().map_or(0, str::len);
                budget.record_model_usage(
                    output_bytes,
                    response.usage.input_tokens,
                    response.usage.output_tokens,
                )?;
                budget.emit(&self.events);
            }
            outcome.review_calls += 1;

            // The review response becomes assistant transcript content so a
            // subsequent fix loop sees the findings.
            transcript.push_assistant(&response);
            let findings = findings_from_response(&response);
            if findings.is_empty() {
                break;
            }
            let ids = {
                let mut tasks = self.tasks.lock().expect("task graph poisoned");
                create_finding_tasks(&mut tasks, &run.task.id, &findings)?
            };
            let mut new_findings: Vec<ReviewFinding> = findings
                .into_iter()
                .map(|detail| ReviewFinding { detail, task_id: None })
                .collect();
            for (finding, id) in new_findings.iter_mut().zip(ids.iter()) {
                finding.task_id = Some(id.clone());
            }
            outcome.findings.extend(new_findings);
            outcome.finding_task_ids.extend(ids);

            if outcome.fix_loops >= config.max_fix_iterations {
                break;
            }
            outcome.fix_loops += 1;
            let (result, updated) = self
                .run_loop(
                    LoopMode::Standard,
                    false,
                    transcript.clone(),
                    session_id.to_string(),
                    run.clone(),
                )
                .await?;
            let _ = result;
            *transcript = updated;
            let mut tasks = self.tasks.lock().expect("task graph poisoned");
            mark_finding_tasks(&mut tasks, &outcome.finding_task_ids, "addressed in fix loop")?;
        }
        Ok(outcome)
    }
}

/// Whether the intent targets a registered validation-capable tool.
fn is_validation_tool(intent: &ToolIntent, tools: &Mutex<ToolRegistry>) -> bool {
    tools
        .lock()
        .expect("tool registry poisoned")
        .get(&intent.name)
        .is_some_and(|tool| is_validation_tool_name(&tool.definition().name))
}

/// Records one executed intent into the shared execution log.
fn log_execution(
    log: &Mutex<Vec<ToolExecutionLogEntry>>,
    intent: &ToolIntent,
    success: Option<bool>,
    summary: &str,
) {
    log.lock().expect("execution log poisoned").push(ToolExecutionLogEntry {
        tool_call_id: intent.tool_call_id.clone(),
        tool_name: intent.name.clone(),
        side_effect_class: None,
        arguments: intent.arguments.clone(),
        success: success.unwrap_or(false),
        summary: summary.to_string(),
    });
}
