//! `impl SubagentManager`: spawn/run/cancel and child lifecycle.
use super::*;

impl SubagentManager {
    /// Creates a manager sharing the runtime's stores, model registry, and
    /// per-turn budget tracker.
    pub(crate) fn new(
        config: OrchestratorConfig,
        models: Arc<ModelRegistry>,
        tools: Arc<Mutex<ToolRegistry>>,
        state: SubagentState,
        observability: SubagentObservability,
    ) -> Self {
        let semaphore = Arc::new(Semaphore::new(config.max_parallel_subagents));
        let quarantine = Arc::new(Mutex::new(SubagentQuarantine::default()));
        Self {
            config,
            models,
            tools,
            tasks: state.tasks,
            memory: state.memory,
            budget: state.budget,
            observability,
            semaphore,
            quarantine,
            children: state.children,
        }
    }

    /// Snapshot of quarantined (failed, cancelled, or unverified) child
    /// output; used by tests to prove failed memory never merges.
    #[cfg(test)]
    pub(crate) fn quarantine_snapshot(&self) -> SubagentQuarantine {
        self.quarantine.lock().expect("subagent quarantine poisoned").clone()
    }

    /// Spawns one logical subagent under `parent_task_id`.
    ///
    /// Resolves the child adapter through the model registry (explicit role
    /// selection, else the parent loop's adapter) *before* the child task
    /// node exists; unknown ids fail closed.  Enforces the depth limit,
    /// bounds concurrency with the shared semaphore, runs the child with the
    /// same [`LoopEngine`] under a reduced config and role-derived policy,
    /// merges the child's non-sensitive memory items (including a summary
    /// fact) into the parent store, and records `SubagentStarted`/`SubagentFinished`
    /// events into `events`.
    pub(crate) async fn spawn(
        &self,
        request: DelegationRequest,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        events: EventRecorder,
    ) -> Result<SubagentResult, OrchestratorError> {
        let depth = self.task_depth(&request.parent_task_id);
        if depth + 1 > self.config.max_subagent_depth {
            self.observability.decisions.lock().expect("decision log poisoned").record_delegation(
                &request.role.name,
                depth,
                false,
            );
            return Err(OrchestratorError::InvalidState(format!(
                "subagent depth limit exceeded (max {})",
                self.config.max_subagent_depth
            )));
        }

        // Resolve the child adapter before the child task node is created.
        // Explicit role selection wins, then configured role routing, then
        // the parent loop's adapter. Unknown ids never create a node.
        let model = self.resolve_model(
            request.role.model.clone(),
            request.model_id,
            &request.role.name,
            &events,
        )?;

        // Reserve the per-turn subagent budget before creating the child
        // task node; budget-denied spawns never start.
        {
            let mut budget = self.budget.lock().expect("budget tracker poisoned");
            budget.try_reserve_subagent()?;
            budget.emit(&events);
        }

        // Create the child task node before spawning any work, recording the
        // resolved model id on it.
        let child = {
            let mut tasks = self.tasks.lock().expect("task graph poisoned");
            let child = tasks.create_child(
                &request.parent_task_id,
                &request.role.name,
                &truncate(&request.scoped_prompt, SUBAGENT_SUMMARY_MAX_CHARS),
            )?;
            tasks
                .set_model_id(&child.id, model.id.clone())
                .expect("child task exists for model id");
            child
        };
        let subagent_id = SubagentId::new(child.id.as_str());
        let registration = self.children.register(
            subagent_id.clone(),
            child.id.clone(),
            request.parent_task_id.clone(),
            request.role.name.clone(),
            self.config.subagent_timeout,
        );
        let heartbeat_events = events.with_observer({
            let children = self.children.clone();
            let subagent_id = subagent_id.clone();
            move |event| {
                let progress = match event {
                    OrchestratorEvent::ModelRequested { .. } => Some(ChildProgress::ModelRequested),
                    OrchestratorEvent::ModelResponded { .. } => Some(ChildProgress::ModelResponded),
                    OrchestratorEvent::ToolStarted { .. } => Some(ChildProgress::ToolStarted),
                    OrchestratorEvent::ToolFinished { .. } => Some(ChildProgress::ToolFinished),
                    _ => None,
                };
                if let Some(progress) = progress {
                    children.heartbeat(&subagent_id, progress);
                }
            }
        });
        let mut guard = ChildRunGuard::new(
            self.children.clone(),
            self.tasks.clone(),
            heartbeat_events.clone(),
            subagent_id.clone(),
            child.id.clone(),
        );
        self.observability
            .metrics
            .lock()
            .expect("orchestrator metrics poisoned")
            .record_subagent_spawn(&request.role.name);
        self.observability.decisions.lock().expect("decision log poisoned").record_delegation(
            &request.role.name,
            depth,
            true,
        );
        heartbeat_events.record(OrchestratorEvent::SubagentStarted {
            subagent_id: subagent_id.as_str().to_string(),
            role: request.role.name.clone(),
            model_id: model.id.clone(),
        });
        // Child scope: roots inherited, globs narrowed from the role.  An
        // empty role-glob list inherits the parent scope unchanged; children
        // never widen it.
        let child_scope =
            request.scope.as_ref().map(|scope| scope.narrow(&request.role.allowed_scope_globs));
        let request = SubagentRequest {
            parent_task_id: request.parent_task_id,
            child_task_id: child.id.clone(),
            role: request.role,
            scoped_prompt: request.scoped_prompt,
            context_snapshot: request.context_snapshot,
            scope: child_scope,
            write_scope: Vec::new(),
            model_id: model.id.clone(),
        };
        let deadline = registration.deadline;
        let targeted_cancel = registration.cancel;
        let permit = tokio::select! {
            biased;
            _ = cancelled(cancel.clone()) => {
                let result = self.cancelled_result(
                    &request,
                    &subagent_id,
                    "cancelled while waiting for a subagent permit",
                    heartbeat_events.clone(),
                    ChildState::Cancelled,
                ).await;
                guard.disarm();
                return Ok(result);
            },
            _ = cancelled(targeted_cancel.clone()) => {
                let result = self.cancelled_result(
                    &request,
                    &subagent_id,
                    "subagent cancelled while waiting for a permit",
                    heartbeat_events.clone(),
                    ChildState::Cancelled,
                ).await;
                guard.disarm();
                return Ok(result);
            },
            _ = tokio::time::sleep_until(deadline) => {
                let result = self.cancelled_result(
                    &request,
                    &subagent_id,
                    "subagent exceeded total timeout while waiting for a permit",
                    heartbeat_events.clone(),
                    ChildState::Failed,
                ).await;
                guard.disarm();
                return Ok(result);
            },
            permit = self.semaphore.acquire() => permit.map_err(|_| {
                OrchestratorError::InvalidState("subagent semaphore closed".into())
            })?,
        };
        let _permit = permit;
        let remaining_timeout = deadline.saturating_duration_since(Instant::now());

        {
            let mut tasks = self.tasks.lock().expect("task graph poisoned");
            tasks.transition(&child.id, TaskStatus::Running)?;
        }
        self.children.mark_running(&subagent_id);

        let child_cancel = targeted_cancel.clone();
        let child_run = self.run_child(
            request.clone(),
            model,
            client,
            child_cancel,
            heartbeat_events.clone(),
            remaining_timeout,
        );
        tokio::pin!(child_run);
        let result = tokio::select! {
            biased;
            _ = cancelled(cancel) => {
                let _ = self.children.cancel(&subagent_id);
                self.cancelled_result(
                    &request,
                    &subagent_id,
                    "subagent cancelled by parent",
                    heartbeat_events.clone(),
                    ChildState::Cancelled,
                ).await
            },
            _ = cancelled(targeted_cancel) => {
                self.cancelled_result(
                    &request,
                    &subagent_id,
                    "subagent cancelled by request",
                    heartbeat_events.clone(),
                    ChildState::Cancelled,
                ).await
            },
            _ = tokio::time::sleep_until(deadline) => {
                let _ = self.children.cancel(&subagent_id);
                self.cancelled_result(
                    &request,
                    &subagent_id,
                    "subagent exceeded total timeout",
                    heartbeat_events.clone(),
                    ChildState::Failed,
                ).await
            },
            stalled = self.children.wait_for_stall(
                &subagent_id,
                self.config.subagent_stall_timeout,
            ) => {
                if stalled {
                    let _ = self.children.cancel(&subagent_id);
                    self.cancelled_result(
                        &request,
                        &subagent_id,
                        "subagent stalled without model, tool, or task progress",
                        heartbeat_events.clone(),
                        ChildState::Stalled,
                    ).await
                } else {
                    self.cancelled_result(
                        &request,
                        &subagent_id,
                        "subagent supervision ended unexpectedly",
                        heartbeat_events.clone(),
                        ChildState::Failed,
                    ).await
                }
            },
            result = &mut child_run => result,
        };
        let registry_state = match result.handoff.status {
            SubagentStatus::Completed => ChildState::Completed,
            SubagentStatus::Failed => ChildState::Failed,
            SubagentStatus::Cancelled => ChildState::Cancelled,
        };
        self.children.finish(&subagent_id, registry_state);
        guard.disarm();
        Ok(result)
    }

    /// Resolves the child adapter: the role's explicit `model` selection when
    /// present (unknown ids fail closed), else the parent loop's adapter id,
    /// else the registry default.  Returns the adapter and the resolved id.
    pub(super) fn resolve_model(
        &self,
        selected: Option<String>,
        parent_id: Option<String>,
        role: &str,
        events: &EventRecorder,
    ) -> Result<ResolvedModel, OrchestratorError> {
        let id = match selected {
            Some(id) => {
                if !self.models.contains(&id) {
                    return Err(OrchestratorError::InvalidState(format!("unknown model id: {id}")));
                }
                id
            }
            None => {
                let routed = self
                    .observability
                    .router
                    .read()
                    .expect("model router poisoned")
                    .as_ref()
                    .map(|router| {
                        router
                            .select(TaskKind::Delegation, Some(role), events)
                            .map(|route| route.adapter_id.clone())
                    })
                    .transpose()?;
                routed.or(parent_id).unwrap_or_else(|| DEFAULT_MODEL_ID.to_string())
            }
        };
        let adapter = self.models.get(&id).ok_or_else(|| {
            OrchestratorError::InvalidState(format!("model registry has no adapter {id}"))
        })?;
        Ok(ResolvedModel { adapter, id: Some(id) })
    }

    /// Runs the child loop and finalizes the child task node, memory, and
    /// events.
    pub(super) async fn run_child(
        &self,
        request: SubagentRequest,
        model: ResolvedModel,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        events: EventRecorder,
        remaining_timeout: std::time::Duration,
    ) -> SubagentResult {
        let child = self
            .tasks
            .lock()
            .expect("task graph poisoned")
            .get(&request.child_task_id)
            .cloned()
            .expect("child task exists while running");
        let depth = self.task_depth(&child.id);
        let subagent_id = SubagentId::new(child.id.as_str());
        let is_rubber_duck = BuiltinSubagentRole::by_name(&request.role.name)
            == Some(BuiltinSubagentRole::RubberDuck);
        // Reduced config: generic children inherit bounded root settings;
        // rubber ducks receive dedicated limits strictly below root defaults.
        let mut child_config = OrchestratorConfig {
            max_loop_iterations: request.role.max_iterations,
            turn_timeout: remaining_timeout,
            ..self.config.clone()
        };
        if is_rubber_duck {
            child_config.max_loop_iterations = RUBBER_DUCK_MAX_ITERATIONS;
            child_config.max_model_calls = RUBBER_DUCK_MAX_MODEL_CALLS;
            child_config.max_tool_calls_per_turn = RUBBER_DUCK_MAX_TOOL_CALLS;
            child_config.memory_limit_bytes = RUBBER_DUCK_MAX_CONTEXT_BYTES;
            child_config.max_output_bytes = RUBBER_DUCK_MAX_OUTPUT_BYTES;
            child_config.turn_timeout = RUBBER_DUCK_TIMEOUT.min(remaining_timeout);
            child_config.tool_timeout = RUBBER_DUCK_TOOL_TIMEOUT.min(self.config.tool_timeout);
            child_config.max_subagent_depth = RUBBER_DUCK_MAX_RECURSION_DEPTH;
            child_config.max_subagents = 0;
            child_config.max_parallel_subagents = 0;
            child_config.max_parallel_tools = child_config.max_parallel_tools.min(2);
        }
        let child_budget = Arc::new(Mutex::new(BudgetTracker::new(&child_config)));
        let child_policy = ToolPolicy {
            allow_read: request.role.allowed_tool_classes.contains(&SideEffectClass::Read),
            allow_write: request.role.allowed_tool_classes.contains(&SideEffectClass::Write),
            allow_execute: request.role.allowed_tool_classes.contains(&SideEffectClass::Execute),
            allow_delegate: request.role.allowed_tool_classes.contains(&SideEffectClass::Delegate),
            allow_host_approved_side_effects: !is_rubber_duck,
            max_delegate_depth: if is_rubber_duck {
                RUBBER_DUCK_MAX_RECURSION_DEPTH
            } else {
                self.config.max_subagent_depth
            },
            max_parallel_delegates: if is_rubber_duck {
                0
            } else {
                self.config.max_parallel_subagents
            },
            // Destructive subclasses default to denied for children; scope
            // narrows from the parent's active scope.
            allowed_side_effect_subclasses: Default::default(),
            owned_terminal_ids: Default::default(),
            scope: request.scope.clone(),
        };
        // Discovery and dispatch use same exact immutable critic tool set.
        let visible_tool_names = is_rubber_duck.then(|| {
            self.tools
                .lock()
                .expect("tool registry poisoned")
                .definitions()
                .into_iter()
                .filter(rubber_duck_allows_tool)
                .map(|tool| tool.name)
                .collect::<std::collections::HashSet<_>>()
        });
        let mut policy = PolicyEngine::new(child_policy);
        if let Some(names) = &visible_tool_names {
            policy = policy.with_allowed_tool_names(names.iter().cloned());
        }
        // The child's execution log becomes verification evidence before any
        // memory merge or parent-visible success.
        let execution_log = Arc::new(Mutex::new(Vec::new()));
        let engine = LoopEngine::new(
            child_config,
            model.adapter,
            self.tools.clone(),
            child_budget.clone(),
            policy,
            events.clone(),
            LoopOptions {
                depth,
                graph: Some(self.tasks.clone()),
                execution_log: Some(execution_log.clone()),
                available_models: self.models.advertised(),
                model_id: model.id,
                visible_tool_names,
                ..LoopOptions::default()
            },
        );

        // Scoped transcript: role instructions, parent context snapshot, and
        // the delegation prompt as the newest user message.
        let mut transcript = Transcript::new();
        if !request.role.instructions.is_empty() {
            transcript.prepend_system(request.role.instructions.clone());
        }
        if !is_rubber_duck {
            transcript.prepend_system(GENERIC_HANDOFF_INSTRUCTIONS);
        }
        transcript.messages.extend(request.context_snapshot.clone());
        transcript
            .messages
            .push(ModelMessage::text(ModelRole::User, request.scoped_prompt.clone()));

        // Subagents stream no updates to the client; their output is the
        // bounded summary carried back in the delegate tool result.
        let (sink_tx, _sink_rx) = mpsc::unbounded_channel();
        let sink = UpdateSink::new(SessionId::new(SUBAGENT_SESSION), sink_tx);
        let outcome = engine
            .run_transcript(
                &mut transcript,
                SUBAGENT_SESSION.to_string(),
                sink,
                client,
                cancel,
                child.clone(),
            )
            .await;
        let tool_call_count =
            child_budget.lock().expect("budget tracker poisoned").snapshot().tool_calls_used;

        let (status, raw_output, error_summary) = match outcome {
            Ok(_) => (
                SubagentStatus::Completed,
                transcript.last_assistant_text().unwrap_or_default(),
                None,
            ),
            Err(OrchestratorError::Cancellation) => {
                (SubagentStatus::Cancelled, String::new(), Some("subagent cancelled".into()))
            }
            Err(error) => (SubagentStatus::Failed, String::new(), Some(error.to_string())),
        };
        let error_summary = error_summary.map(|text| truncate(&text, SUBAGENT_SUMMARY_MAX_CHARS));
        let evidence = SubagentEvidence::from_execution_log(
            &execution_log.lock().expect("execution log poisoned"),
        );
        let handoff = if is_rubber_duck {
            let mut handoff =
                SubagentHandoff::terminal(&request.role.name, subagent_id.as_str(), status);
            handoff.summary = raw_output;
            handoff.claimed_citations = SubagentCitations::extract(&handoff.summary);
            handoff.observed_evidence = evidence.clone();
            handoff
        } else if status == SubagentStatus::Completed {
            SubagentHandoff::from_completed_output(
                &request.role.name,
                subagent_id.as_str(),
                &raw_output,
                evidence.clone(),
            )
        } else {
            SubagentHandoff::terminal(&request.role.name, subagent_id.as_str(), status)
        };
        let mut result = SubagentResult {
            subagent_id,
            handoff,
            produced_memory_items: Vec::new(),
            tool_call_count,
            error_summary,
        };
        if result.handoff.output_format == HandoffOutputFormat::RejectedMalformed {
            result.error_summary =
                Some("subagent handoff rejected: malformed or unsupported JSON".into());
        }

        // Rubber-duck output becomes parent-visible only after strict JSON,
        // bounds, and observed-evidence verification. Invalid raw output stays
        // in quarantine and returns failure, never a successful tool result.
        if is_rubber_duck && result.handoff.status == SubagentStatus::Completed {
            let observed = ReportEvidence::from_subagent_evidence(&evidence);
            match CritiqueReportVerifier.parse_and_accept(&result.handoff.summary, &observed) {
                Ok(verified) => match verified.to_json() {
                    Ok(summary) => result.handoff.summary = summary,
                    Err(error) => {
                        result.handoff.status = SubagentStatus::Failed;
                        result.error_summary = Some(truncate(
                            &format!("verified critique serialization failed: {error}"),
                            SUBAGENT_SUMMARY_MAX_CHARS,
                        ));
                    }
                },
                Err(error) => {
                    result.handoff.status = SubagentStatus::Failed;
                    result.error_summary = Some(truncate(
                        &format!("rubber-duck critique rejected: {error}"),
                        SUBAGENT_SUMMARY_MAX_CHARS,
                    ));
                }
            }
        }

        let summary_item = MemoryItem::from_task(
            format!("subagent:{}", result.subagent_id),
            result.handoff.summary.clone(),
            request.child_task_id.clone(),
        )
        .with_trust(TrustLevel::SubagentSummaryUntrusted);
        result.produced_memory_items.push(summary_item);

        {
            let mut memory = self.memory.lock().expect("memory store poisoned");
            let mut quarantine = self.quarantine.lock().expect("subagent quarantine poisoned");
            match result.handoff.status {
                SubagentStatus::Completed if is_rubber_duck => {
                    merge_memory_items(&mut memory, &result.produced_memory_items);
                }
                SubagentStatus::Completed => {
                    let verification =
                        SubagentResultVerifier::new().verify(&request.role, &result, &evidence);
                    if verification.verified {
                        merge_memory_items(&mut memory, &result.produced_memory_items);
                    } else {
                        let reason = verification
                            .rejected_reason
                            .unwrap_or_else(|| "unverified subagent summary".into());
                        result.handoff.status = SubagentStatus::Failed;
                        result.error_summary = Some(truncate(&reason, SUBAGENT_SUMMARY_MAX_CHARS));
                        quarantine.quarantine(&result, reason);
                    }
                }
                SubagentStatus::Failed => {
                    let reason =
                        result.error_summary.clone().unwrap_or_else(|| "subagent failed".into());
                    quarantine.quarantine(&result, reason);
                }
                SubagentStatus::Cancelled => {
                    quarantine.quarantine(&result, "subagent cancelled");
                }
            }
        }
        self.finish_child(&request, &result, events).await;
        result
    }

    pub(super) async fn cancelled_result(
        &self,
        request: &SubagentRequest,
        subagent_id: &SubagentId,
        reason: &str,
        events: EventRecorder,
        registry_state: ChildState,
    ) -> SubagentResult {
        let status = if registry_state == ChildState::Failed {
            SubagentStatus::Failed
        } else {
            SubagentStatus::Cancelled
        };
        let result = SubagentResult {
            subagent_id: subagent_id.clone(),
            handoff: SubagentHandoff::terminal(&request.role.name, subagent_id.as_str(), status),
            produced_memory_items: Vec::new(),
            tool_call_count: 0,
            error_summary: Some(truncate(reason, SUBAGENT_SUMMARY_MAX_CHARS)),
        };
        self.quarantine
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .quarantine(&result, reason);
        self.finish_child(request, &result, events).await;
        self.children.finish(subagent_id, registry_state);
        result
    }

    /// Applies the result to the child task node and records the terminal
    /// subagent event. Repeated finalization is harmless so cancellation and
    /// natural completion may race without panicking.
    pub(super) async fn finish_child(
        &self,
        request: &SubagentRequest,
        result: &SubagentResult,
        events: EventRecorder,
    ) {
        let final_status = match result.handoff.status {
            SubagentStatus::Completed => TaskStatus::Completed,
            SubagentStatus::Failed => TaskStatus::Failed,
            SubagentStatus::Cancelled => TaskStatus::Cancelled,
        };
        let mut tasks = self.tasks.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let should_emit = tasks
            .get(&request.child_task_id)
            .is_some_and(|task| matches!(task.status, TaskStatus::Pending | TaskStatus::Running));
        if should_emit {
            let _ = tasks.transition(&request.child_task_id, final_status);
        }
        drop(tasks);
        if should_emit {
            events.record(OrchestratorEvent::SubagentFinished {
                subagent_id: result.subagent_id.as_str().to_string(),
                success: result.handoff.status == SubagentStatus::Completed,
            });
        }
    }

    /// Nesting depth of `task_id` in the graph (root is 0).
    pub(super) fn task_depth(&self, task_id: &TaskId) -> usize {
        let tasks = self.tasks.lock().expect("task graph poisoned");
        let mut depth = 0usize;
        let mut current = Some(task_id.clone());
        while let Some(id) = current {
            let Some(node) = tasks.get(&id) else { break };
            match &node.parent {
                Some(parent) => {
                    depth += 1;
                    current = Some(parent.clone());
                }
                None => break,
            }
        }
        depth
    }
}
