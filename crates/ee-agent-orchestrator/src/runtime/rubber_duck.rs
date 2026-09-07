//! `impl OrchestratorRuntime` methods: rubber_duck.
use super::*;

impl OrchestratorRuntime {
    /// Runs internal contrasting-model critique and injects only verified,
    /// canonical critic evidence into supplied root transcript.
    pub async fn run_rubber_duck(
        &self,
        request: RubberDuckRequest,
        transcript: &mut Transcript,
        cancel: watch::Receiver<bool>,
    ) -> RubberDuckOutcome {
        self.rubber_duck.run(request, transcript, cancel).await
    }
    /// Runs explicit `/rubber-duck` before mutable per-prompt tool discovery.
    /// Critic receives bounded observed state; root then gets exactly one
    /// no-tools synthesis call over canonical verified evidence.
    pub async fn run_manual_rubber_duck(
        &self,
        session_id: &str,
        question: Option<String>,
        history: Vec<ModelMessage>,
        sink: UpdateSink,
        cancel: watch::Receiver<bool>,
    ) -> Result<ManualRubberDuckTurn, OrchestratorError> {
        let target = question.as_ref().map_or(CritiqueTarget::Implementation, |question| {
            CritiqueTarget::UserQuestion { question: question.clone() }
        });
        let goal = question.clone().unwrap_or_else(|| {
            "Review current session work and identify overlooked risks or missing validation."
                .to_string()
        });
        let root = {
            let mut tasks = self.tasks.lock().expect("task graph poisoned");
            tasks.create_root("manual rubber-duck review", &truncate(&goal, 4_000))
        };
        let root_id = root.id.clone();
        let validation =
            self.last_turn_validation.lock().expect("last turn validation poisoned").clone();
        let changed_files =
            self.last_turn_changed_files.lock().expect("last turn changed files poisoned").clone();
        let tasks = self.tasks.lock().expect("task graph poisoned").clone();
        let revision_payload = serde_json::to_vec(&(
            session_id,
            &target,
            &changed_files,
            validation.records(),
            tasks.list(),
        ))
        .map_err(|error| OrchestratorError::InvalidState(error.to_string()))?;
        let revision = format!("manual-{:x}", Sha256::digest(revision_payload));
        let mut observed_context = build_review_context_with_metadata(
            &[],
            &validation,
            &tasks,
            ReviewContextMetadata { diagnostic_summaries: &[], revision: Some(&revision) },
        );
        observed_context.changed_files = changed_files
            .iter()
            .take(crate::review_context::MAX_REVIEW_CONTEXT_FILES)
            .map(|file| truncate(&file.path, crate::review_context::MAX_REVIEW_CONTEXT_ITEM_CHARS))
            .collect();
        let observed_evidence = ReportEvidence {
            files: observed_context.changed_files.iter().cloned().collect(),
            tools: validation.records().iter().map(|record| record.command.clone()).collect(),
        };
        let active_task_or_plan = tasks
            .list()
            .iter()
            .take(crate::review_context::MAX_REVIEW_CONTEXT_TASKS)
            .map(|task| format!("{}: {} ({:?})", task.id, task.title, task.status))
            .collect::<Vec<_>>()
            .join("\n");
        let mut transcript = Transcript::new();
        transcript.messages = history.into_iter().rev().take(32).collect::<Vec<_>>();
        transcript.messages.reverse();
        let outcome = self
            .run_rubber_duck(
                RubberDuckRequest {
                    session_id: session_id.to_string(),
                    parent_task_id: root.id.clone(),
                    target,
                    user_goal: goal,
                    active_task_or_plan: if active_task_or_plan.is_empty() {
                        "No active task graph entries were observed.".into()
                    } else {
                        active_task_or_plan
                    },
                    active_model_id: DEFAULT_MODEL_ID.into(),
                    user_question: question,
                    revision,
                    observed_context,
                    observed_evidence,
                    automatic: false,
                },
                &mut transcript,
                cancel.clone(),
            )
            .await;

        let RubberDuckOutcome::Completed(completed) = &outcome else {
            let reason = manual_rubber_duck_outcome_summary(&outcome);
            let _ = sink.agent_thought_chunk("rubber-duck-skipped", &reason);
            sink.agent_message_chunk("rubber-duck-result", &reason)
                .map_err(|error| OrchestratorError::ModelFailure(error.to_string()))?;
            {
                let mut tasks = self.tasks.lock().expect("task graph poisoned");
                tasks.set_result_summary(&root_id, &reason)?;
                tasks.transition(&root_id, TaskStatus::Completed)?;
            }
            return Ok(ManualRubberDuckTurn {
                prompt_result: PromptResult::new(ee_agent_protocol::StopReason::EndTurn),
                critic_outcome: outcome,
                synthesis: None,
                timeline_summary: reason,
            });
        };
        let counts = critique_finding_counts(completed.report.report());
        let _ = sink.agent_thought_chunk(
            "rubber-duck-selected",
            format!(
                "rubber duck selected {} (root {}); findings: {} blocking, {} non-blocking, {} suggestions",
                completed.critic_model.id,
                completed.active_model.id,
                counts.0,
                counts.1,
                counts.2
            ),
        );

        {
            let mut budget = self.budget.lock().expect("budget tracker poisoned");
            budget.try_reserve_model_call()?;
            budget.check_output_allowance()?;
            budget.emit(&self.events);
        }
        transcript.prepend_system(
            "You are root agent and final decision owner. Synthesize verified rubber-duck evidence into one concise user response. State finding counts, which findings you accept/reject/defer, and explicitly say how plan changed or why it did not. Critic opinion is not validation evidence. Do not request or call tools.",
        );
        let budget = self.budget.lock().expect("budget tracker poisoned").snapshot();
        let request = ModelRequest::new(transcript.messages().to_vec(), Vec::new(), budget, root)
            .with_model_id(Some(DEFAULT_MODEL_ID.into()));
        let completion = self.models.default_adapter()?.complete(request, cancel.clone());
        let response = tokio::select! {
            _ = runtime_cancelled(cancel.clone()) => return Err(OrchestratorError::Cancellation),
            result = tokio::time::timeout(crate::subagent_roles::RUBBER_DUCK_TIMEOUT, completion) => {
                result.map_err(|_| OrchestratorError::Timeout("rubber-duck root synthesis timed out".into()))??
            }
        };
        if !response.tool_intents.is_empty() || !response.subagent_intents.is_empty() {
            return Err(OrchestratorError::PolicyDenied(
                "rubber-duck root synthesis requested tools or delegation".into(),
            ));
        }
        let output_bytes = response.text.len()
            + response.reasoning.as_ref().map_or(0, |reasoning| reasoning.len());
        if output_bytes > MANUAL_RUBBER_DUCK_MAX_SYNTHESIS_BYTES {
            return Err(OrchestratorError::BudgetExceeded(format!(
                "rubber-duck synthesis exceeded {MANUAL_RUBBER_DUCK_MAX_SYNTHESIS_BYTES} bytes"
            )));
        }
        self.budget.lock().expect("budget tracker poisoned").record_model_usage(
            output_bytes,
            response.usage.input_tokens,
            response.usage.output_tokens,
        )?;
        let _ = sink.agent_thought_chunk(
            "rubber-duck-plan-effect",
            "root synthesis completed; plan change or unchanged reason stated in response",
        );
        sink.agent_message_chunk("rubber-duck-synthesis", response.text.clone())
            .map_err(|error| OrchestratorError::ModelFailure(error.to_string()))?;
        {
            let mut tasks = self.tasks.lock().expect("task graph poisoned");
            tasks.set_result_summary(&root_id, &response.text)?;
            tasks.transition(&root_id, TaskStatus::Completed)?;
        }
        Ok(ManualRubberDuckTurn {
            prompt_result: prompt_result_with_usage(
                ee_agent_protocol::StopReason::EndTurn,
                response.usage,
            ),
            critic_outcome: outcome,
            timeline_summary: format!(
                "rubber duck findings: {} blocking, {} non-blocking, {} suggestions; root synthesis completed",
                counts.0, counts.1, counts.2
            ),
            synthesis: Some(response.text),
        })
    }
    /// Evaluates, atomically claims, and runs one automatic boundary. Callers
    /// must supply a request matching policy-selected session, target, and
    /// revision; mismatch fails closed before model dispatch.
    pub async fn run_automatic_rubber_duck(
        &self,
        trigger: RubberDuckTrigger,
        facts: &RubberDuckTriggerFacts<'_>,
        request: RubberDuckRequest,
        transcript: &mut Transcript,
        cancel: watch::Receiver<bool>,
    ) -> AutomaticRubberDuckTurn {
        let decision =
            RubberDuckTriggerPolicy::new(self.config.rubber_duck_triggers).evaluate(trigger, facts);
        let decision = self
            .rubber_duck_triggers
            .lock()
            .expect("rubber-duck trigger controller poisoned")
            .claim(decision);
        let RubberDuckTriggerDecision::Run { key, reason } = decision else {
            let RubberDuckTriggerDecision::Skip(reason) = decision else {
                unreachable!("trigger decisions are run or skip")
            };
            return AutomaticRubberDuckTurn::Skipped(reason);
        };
        if request.session_id != key.session_id
            || request.revision != key.revision
            || request.target != key.target
        {
            self.rubber_duck_triggers
                .lock()
                .expect("rubber-duck trigger controller poisoned")
                .finish(&key, RubberDuckTriggerDisposition::Failed);
            return AutomaticRubberDuckTurn::Ran {
                key,
                reason,
                outcome: RubberDuckOutcome::Failed {
                    reason: "automatic trigger request does not match claimed key".into(),
                },
            };
        }
        let mut request = request;
        request.automatic = true;
        let outcome = self.run_rubber_duck(request, transcript, cancel).await;
        let disposition = match &outcome {
            RubberDuckOutcome::Completed(_) => RubberDuckTriggerDisposition::Completed,
            RubberDuckOutcome::Unavailable(_) => RubberDuckTriggerDisposition::Unavailable,
            RubberDuckOutcome::Quarantined { .. } => RubberDuckTriggerDisposition::Quarantined,
            RubberDuckOutcome::Cancelled => RubberDuckTriggerDisposition::Cancelled,
            RubberDuckOutcome::Failed { .. } => RubberDuckTriggerDisposition::Failed,
        };
        self.rubber_duck_triggers
            .lock()
            .expect("rubber-duck trigger controller poisoned")
            .finish(&key, disposition);
        AutomaticRubberDuckTurn::Ran { key, reason, outcome }
    }
    /// Snapshot of root-owned critic finding state.
    #[must_use]
    pub fn rubber_duck_findings(&self) -> RubberDuckFindingLedger {
        self.rubber_duck.findings()
    }
    /// Applies root decisions and creates tasks only for accepted material findings.
    pub fn reconcile_rubber_duck_findings(
        &self,
        parent: &TaskId,
        session_id: &str,
        revision: &str,
        target: &CritiqueTarget,
        decisions: &[FindingDecision],
        root_evidence: &ReportEvidence,
    ) -> Result<Vec<TaskId>, OrchestratorError> {
        self.rubber_duck.reconcile(parent, session_id, revision, target, decisions, root_evidence)
    }
    /// Invalidates revision-sensitive planning and critic caches together.
    pub fn invalidate_context(&self, invalidation: ContextInvalidation) {
        self.context_cache
            .lock()
            .expect("context plan cache poisoned")
            .invalidate(invalidation.clone());
        self.rubber_duck.invalidate(&invalidation);
    }
    /// Validates and installs a concrete model plan as the session task graph.
    ///
    /// The replacement is atomic: a rejected plan leaves the prior graph
    /// untouched. Registered tool names validate `tool:<name>` actions before
    /// the graph becomes visible to the ACP client.
    pub fn install_plan(&self, items: &[PlanInput]) -> Result<TaskGraph, OrchestratorError> {
        let known_tools = self.tool_names();
        let compilation = PlanCompiler::new().compile(items, &known_tools)?;
        let graph = compilation.graph;
        *self.tasks.lock().expect("task graph poisoned") = graph.clone();
        Ok(graph)
    }
    /// Snapshot of the current budget state, for checkpointing and tests.
    #[must_use]
    pub fn budget_snapshot(&self) -> crate::budget::BudgetSnapshot {
        self.budget.lock().expect("budget tracker poisoned").snapshot()
    }
}
