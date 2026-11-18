//! The per-iteration model/tool loop.

use super::util::compact_repeated_web_fetch;
use super::*;

impl LoopEngine {
    /// Runs the loop, recording the terminal stop event on every path.  The
    /// transcript and per-turn recovery track are borrowed so both survive
    /// errors.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_loop(
        &self,
        transcript: &mut Transcript,
        track: Arc<Mutex<ResumeState>>,
        session_id: String,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        task: TaskNode,
    ) -> Result<PromptResult, OrchestratorError> {
        self.events.record(OrchestratorEvent::TurnStarted {
            session_id: session_id.clone(),
            task_id: task.id.as_str().to_string(),
        });
        if let Some(log) = transcript.session_log() {
            log.begin_turn();
            let _ = log.record_turn_started(task.id.as_str());
        }
        let outcome = self
            .run_loop_inner(transcript, &track, &sink, &client, cancel, &task, &session_id)
            .await;
        let stop_reason = match &outcome {
            Ok(result) => {
                if result.stop_reason == StopReason::MaxTurnRequests {
                    "max_iterations"
                } else {
                    "end_turn"
                }
            }
            Err(OrchestratorError::Cancellation) => "cancelled",
            Err(OrchestratorError::BudgetExceeded(_)) => "budget_exceeded",
            Err(OrchestratorError::DeadlineExceeded(_)) => "deadline_exceeded",
            Err(OrchestratorError::Timeout(_)) => "timeout",
            Err(OrchestratorError::Stuck(_)) => "stuck",
            Err(error) => {
                self.events.record(OrchestratorEvent::Error { error: error.to_string() });
                "error"
            }
        };
        if let Some(log) = transcript.session_log() {
            let _ = log.record_turn_stopped(stop_reason);
        }
        self.events.record(OrchestratorEvent::TurnStopped { stop_reason: stop_reason.into() });
        outcome
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_loop_inner(
        &self,
        transcript: &mut Transcript,
        track: &Mutex<ResumeState>,
        sink: &UpdateSink,
        client: &ClientBridge,
        cancel: watch::Receiver<bool>,
        task: &TaskNode,
        session_id: &str,
    ) -> Result<PromptResult, OrchestratorError> {
        let mut empty_responses = 0usize;
        let mut message_seq = 0usize;
        let mut repairs_used = 0usize;
        let mut output_continuations = 0usize;
        let mut measured_prompt_tokens: Option<u64> = None;
        let mut detector = StuckDetector::new(self.config.stuck);
        let mut repeat_guard = RepeatGuard::new(RepeatGuardConfig::default());
        let graph_handle = self.graph.clone();
        // Per-turn token usage aggregated across every model call; rounds
        // with unknown usage are skipped, never counted as zero.
        let mut turn_usage = ModelUsage::new();
        let mut observed_web_sources = HashSet::new();
        // Steps completed this turn, for the optional stop predicate.
        let mut step_responses: Vec<ModelResponse> = Vec::new();
        // First step's tool list; a mid-turn change is refused on routes that do
        // not advertise `tools.updates`.
        let mut first_tool_names: Option<Vec<String>> = None;
        // Provider cache routing and per-session limits key on affinity; the
        // runtime seeds provider/project extras and the loop stamps the session.
        let session = self
            .options
            .session_context
            .clone()
            .map(|mut context| {
                context.session_id = session_id.to_string();
                context
            })
            .unwrap_or_else(|| SessionContext::new(session_id.to_string()));
        // Provider-side continuation anchor for stateful Responses routes. A
        // route that does not advertise store support ignores it in the codec.
        let mut continuation: Option<crate::model::Continuation> = None;

        loop {
            if *cancel.borrow() {
                return Err(OrchestratorError::Cancellation);
            }
            // The iteration cap is a budget stop, but the reference agent turns
            // it into one final text-only call so the user still gets a summary
            // of completed and remaining work instead of an error.
            let iteration_cap = {
                let budget = self.budget.lock().expect("budget tracker poisoned");
                budget.iteration_cap_reached()
            };
            if let Some(max) = iteration_cap {
                return self
                    .finish_at_max_steps(MaxStepsCall {
                        transcript,
                        sink,
                        cancel: &cancel,
                        task,
                        session: &session,
                        turn_usage,
                        session_id,
                        max_iterations: max,
                    })
                    .await;
            }
            // Surface exited background terminal jobs before the next model
            // call so their status and output tail arrive as a bounded notice
            // without the model polling or blocking.
            if let Some(jobs) = &self.options.background_jobs {
                let notices = jobs.poll(client, &SessionId::new(session_id)).await;
                for notice in notices {
                    transcript.push_system_notice(notice);
                }
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
            let tools: Vec<ToolDefinition> = {
                let definitions = crate::tools::canonical_tool_definitions(
                    self.tools.lock().expect("tool registry poisoned").definitions(),
                    self.options.visible_tool_names.as_ref(),
                );
                if self.options.read_only_tools {
                    crate::tools::read_only_definitions(definitions)
                } else {
                    definitions
                }
            };
            let evidence_capabilities = crate::stop_gate::evidence_capabilities_for(&tools);
            crate::model_options::validate_tool_names(&tools)
                .map_err(OrchestratorError::ModelFailure)?;
            let tool_names: Vec<String> = tools.iter().map(|tool| tool.name.clone()).collect();
            if let Some(first) = &first_tool_names {
                if let Some(changed) = crate::model_options::tool_list_change(first, &tool_names)
                    && !self.config.tool_list_updates
                {
                    return Err(OrchestratorError::ModelFailure(format!(
                        "the advertised tool list changed mid-conversation ({changed}); the routed \
                         provider does not accept tool-list updates, so freeze the registry for the \
                         turn or advertise `tools.updates`"
                    )));
                }
            } else {
                first_tool_names = Some(tool_names);
            }

            self.events.record(OrchestratorEvent::StepStarted {
                session_id: session_id.to_string(),
                task_id: task.id.as_str().to_string(),
                iteration: budget_snapshot.iterations_used,
                advertised_tool_count: tools.len(),
            });
            if let Some(log) = transcript.session_log() {
                log.set_iteration(budget_snapshot.iterations_used);
            }

            // Keep the transcript inside the memory budget; oldest messages
            // are dropped first, the newest context always survives.
            transcript.enforce_memory_limit(self.config.memory_limit_bytes);
            // Losslessly elide duplicate/superseded read-only tool results in
            // the request projection before prompt-injection scanning and the
            // model call. The durable transcript keeps the full outputs.
            let request_messages = crate::compaction::prune_redundant_readonly_tool_outputs(
                transcript.request_messages(),
            );
            // Untrusted tool output and subagent summaries are labeled,
            // wrapped, and scanned for prompt-injection phrases before the
            // model sees them; suspicious text never changes policy.
            let prepared = crate::prompt_injection::prepare_request(&request_messages);
            for detection in &prepared.detections {
                self.events.record(OrchestratorEvent::SuspiciousContentDetected {
                    trust: detection.trust,
                    pattern: detection.pattern.clone(),
                    excerpt: detection.excerpt.clone(),
                });
            }
            let mut request =
                ModelRequest::new(prepared.messages, tools.clone(), budget_snapshot, task.clone())
                    .with_available_models(self.options.available_models.clone())
                    .with_model_id(self.options.model_id.clone())
                    .with_prompt_tokens(measured_prompt_tokens)
                    .with_response_format(self.options.response_format.clone())
                    .with_session(Some(session.clone()))
                    .with_continuation(continuation.clone());
            // A continuation only carries valid indexes when projection did not
            // drop messages; otherwise the codec would send the wrong delta.
            if request.continuation.as_ref().is_some_and(|anchor| {
                request.transcript.len() != transcript.messages().len()
                    || request.transcript.len() < anchor.input_from
            }) {
                request.continuation = None;
            }
            let mut sent_messages = request.transcript.len();
            // Structured output is protocol-independent: a hidden tool carries
            // the schema and the forced choice makes the model call it.
            crate::structured_output::prepare(&mut request)
                .map_err(OrchestratorError::ModelFailure)?;
            // Fail closed before the model call: every model-visible message
            // and the effective request must be durable first.
            if let Err(error) = transcript.record_model_request(&request) {
                self.events.record(OrchestratorEvent::Error { error: error.to_string() });
                return Err(error);
            }
            self.events.record(OrchestratorEvent::ModelRequested {
                iteration: budget_snapshot.iterations_used,
            });
            message_seq += 1;
            let message_id = format!("msg-{message_seq}");
            // A provider that rejects the prompt as too large gets one bounded
            // recovery: drop the oldest half of the conversation and retry the
            // request, instead of ending the turn with an unrecoverable error.
            let mut overflow_retried = false;
            let response = loop {
                match run_streaming_response(
                    |events| self.model.complete_streaming(request.clone(), cancel.clone(), events),
                    Some(sink),
                    &cancel,
                    &message_id,
                )
                .await
                {
                    Ok(response) => break response,
                    Err(OrchestratorError::ContextOverflow(detail)) if !overflow_retried => {
                        // Deterministic in-turn compaction: prune superseded
                        // read-only results, offload stale binary payloads, then
                        // drop the oldest half. A second overflow ends the turn
                        // instead of looping on an unchanged prompt.
                        let keep_multimodal = self.config.compaction.keep_recent_multimodal_turns;
                        let Some(compaction) = crate::compaction::compact_transcript_for_overflow(
                            transcript,
                            keep_multimodal,
                        ) else {
                            self.events.record(OrchestratorEvent::Error {
                                error: format!("context window exceeded: {detail}"),
                            });
                            return Err(OrchestratorError::ContextOverflow(detail));
                        };
                        overflow_retried = true;
                        self.events.record(OrchestratorEvent::Error {
                            error: format!(
                                "context window exceeded; compacted {} message(s) ({} dropped, {} \
                                 multimodal block(s) offloaded) and retrying: {detail}",
                                compaction.retained_messages,
                                compaction.dropped_messages,
                                compaction.offloaded_multimodal
                            ),
                        });
                        let projected = crate::prompt_injection::prepare_request(
                            &crate::compaction::prune_redundant_readonly_tool_outputs(
                                transcript.request_messages(),
                            ),
                        );
                        request = ModelRequest::new(
                            projected.messages,
                            tools.clone(),
                            budget_snapshot,
                            task.clone(),
                        )
                        .with_available_models(self.options.available_models.clone())
                        .with_model_id(self.options.model_id.clone())
                        .with_prompt_tokens(None)
                        .with_response_format(self.options.response_format.clone())
                        .with_session(Some(session.clone()))
                        .with_continuation(None);
                        crate::structured_output::prepare(&mut request)
                            .map_err(OrchestratorError::ModelFailure)?;
                        sent_messages = request.transcript.len();
                    }
                    Err(error) => {
                        self.events.record(OrchestratorEvent::Error { error: error.to_string() });
                        return Err(error);
                    }
                }
            };
            self.events.record(OrchestratorEvent::ModelResponded {
                iteration: budget_snapshot.iterations_used,
            });
            // A structured-output step decodes the hidden tool's arguments and
            // removes the call so the loop never executes it.
            let mut response = response;
            crate::structured_output::take_object(&request, &mut response)
                .map_err(OrchestratorError::ModelFailure)?;
            // An explicit stop reason the loop cannot interpret is a provider
            // contract violation, never a successful completion.
            validate_finish_reason(
                self.options.model_id.as_deref(),
                response.finish_reason.as_deref(),
            )?;
            if let Some(reason) = detector.observe_model_response(&response) {
                return Err(OrchestratorError::Stuck(reason.to_string()));
            }

            // Record the response against the output/token budgets before
            // streaming anything; a violating response stops the turn.
            if *cancel.borrow() {
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
            turn_usage = turn_usage.accumulate(&response.usage);
            if let Some(metrics) = &self.options.metrics {
                metrics.lock().expect("orchestrator metrics poisoned").record_usage_for_model(
                    self.options.model_id.as_deref().unwrap_or("unknown"),
                    &response.usage,
                );
            }
            // Report the context-window usage. The provider's own context size
            // (input plus cache read/write on Anthropic-style payloads) is the
            // truest measure; otherwise the round's input tokens stand in. The
            // window size comes from the configuration.
            if let Some(used) = response.usage.context_tokens.or(response.usage.input_tokens) {
                let _ = sink.raw_update(SessionUpdate::UsageUpdate(UsageUpdate::new(
                    used as u64,
                    self.config.context_window_tokens,
                )));
            }

            transcript.push_assistant(&response);
            // The response may be resumable: keep its id plus the first message
            // the provider has not seen. A route that does not store responses
            // never emits an id, which clears the anchor and falls back to a
            // full send on the next step.
            let next_input_from = if transcript.messages().len() > sent_messages {
                sent_messages + 1
            } else {
                sent_messages
            };
            continuation = response
                .response_id
                .clone()
                .map(|id| crate::model::Continuation::new(id, next_input_from));
            // Model response before tools: persist the assistant turn so an
            // interruption after tools leaves a durable transcript tail.
            self.capture_checkpoint(transcript, track, task, false).await;

            // A caller-supplied predicate can end the loop here, after the step
            // is durable and before any tool runs.
            step_responses.push(response.clone());
            if self.options.until.as_ref().is_some_and(|until| until.is_met(&step_responses)) {
                return Ok(prompt_result_with_usage(StopReason::EndTurn, turn_usage));
            }

            // Feed the measured prompt size into the next round so the adapter
            // can fit its output bound to the room left in the window.
            measured_prompt_tokens =
                response.usage.input_tokens.and_then(|tokens| u64::try_from(tokens).ok());

            // A reply cut by the output bound is continued with a bounded
            // notice, matching the reference agent's length handling. After the
            // per-turn cap the partial answer is returned instead of looping.
            if response.finish_reason.as_deref() == Some("length")
                && response.tool_intents.is_empty()
            {
                if self.options.mode == LoopMode::Standard
                    && output_continuations < self.config.max_output_continuations
                {
                    output_continuations += 1;
                    transcript.push_message(ModelMessage::text(
                        ModelRole::User,
                        CONTINUE_AFTER_OUTPUT_LIMIT,
                    ));
                    self.capture_checkpoint(transcript, track, task, false).await;
                    continue;
                }
                return Ok(prompt_result_with_usage(StopReason::EndTurn, turn_usage));
            }

            if self.options.mode == LoopMode::SimpleAnswer && !response.tool_intents.is_empty() {
                return Err(OrchestratorError::ModelFailure(
                    "strategy SimpleAnswer does not execute tools".into(),
                ));
            }
            if response.completed {
                if let Some(config) = self.options.stop_gate.map(StopGateConfig::bounded) {
                    let logged = self
                        .options
                        .execution_log
                        .as_ref()
                        .map(|log| log.lock().expect("execution log poisoned").clone())
                        .unwrap_or_default();
                    let decision = StopGate.evaluate_with_capabilities(
                        config,
                        &task.id,
                        &logged,
                        repairs_used,
                        self.options.stop_veto.as_ref(),
                        evidence_capabilities,
                    );
                    self.events.record(OrchestratorEvent::StopGateEvaluated {
                        session_id: session_id.to_string(),
                        task_id: task.id.as_str().to_string(),
                        completion_state: decision.report.state.as_str().to_string(),
                        changed_file_count: decision.changed_file_count,
                        evidence_ids: decision.report.evidence_ids.clone(),
                    });
                    match decision.outcome {
                        StopGateOutcome::Allow => {
                            return Ok(prompt_result_with_usage(StopReason::EndTurn, turn_usage));
                        }
                        StopGateOutcome::Repair => {
                            repairs_used += 1;
                            if let Some(instruction) = decision.repair_instruction {
                                transcript.push_system_notice(instruction);
                            }
                            self.capture_checkpoint(transcript, track, task, false).await;
                            continue;
                        }
                        StopGateOutcome::Blocked => {
                            return Ok(prompt_result_with_usage(StopReason::Refusal, turn_usage));
                        }
                    }
                }
                return Ok(prompt_result_with_usage(StopReason::EndTurn, turn_usage));
            }
            if self.options.mode == LoopMode::SimpleAnswer {
                return Ok(prompt_result_with_usage(StopReason::EndTurn, turn_usage));
            }
            if response.is_empty() {
                empty_responses += 1;
                if empty_responses >= 2 {
                    return Ok(prompt_result_with_usage(StopReason::EndTurn, turn_usage));
                }
            } else {
                empty_responses = 0;
            }

            // ResearchThenEdit runs read-class intents before the rest; the
            // ordering is a stable partition, never a semantic reorder.
            let intents = if self.options.read_first {
                crate::parallel_tools::read_first_order(response.tool_intents, &self.tools)
            } else {
                response.tool_intents
            };
            // Provider-executed tools (hosted search, code interpreter, MCP) are
            // recorded once from the provider's own result and never reach the
            // local executor.
            let (provider_intents, local_intents): (Vec<ToolIntent>, Vec<ToolIntent>) =
                intents.into_iter().partition(|intent| intent.provider_executed);
            for intent in provider_intents {
                let result = match intent.provider_result.clone() {
                    Some(value) => {
                        let text = serde_json::to_string(&value).unwrap_or_default();
                        ToolResult::success_structured(text, value)
                    }
                    None => ToolResult::success("provider executed this tool"),
                };
                self.events.record(OrchestratorEvent::ToolStarted {
                    tool_call_id: intent.tool_call_id.clone(),
                    tool_name: intent.name.clone(),
                });
                self.events.record(OrchestratorEvent::ToolFinished {
                    tool_call_id: intent.tool_call_id.clone(),
                    tool_name: intent.name.clone(),
                    success: result.success,
                });
                transcript.push_provider_tool_result(&intent, result);
                self.capture_checkpoint(transcript, track, task, false).await;
            }
            let intents = local_intents;
            // Read-only batches are independent regardless of which strategy
            // selected the turn; write/execute waves still serialize inside the
            // runner. `read_first` now only controls ordering.
            let parallel_read_wave =
                intents.len() > 1 && crate::parallel_tools::all_read_only(&intents, &self.tools);
            if parallel_read_wave {
                // The runner retains model order in its output while scheduling only
                // independent read waves concurrently. Host web-service limits remain
                // the final gate for approved remote requests.
                let runner = ParallelToolRunner::new(
                    self.executor.clone(),
                    self.tools.clone(),
                    self.config.max_parallel_tools,
                    self.events.clone(),
                );
                let outcomes = runner
                    .run_batch(&intents, sink, client, cancel.clone(), task, transcript.messages())
                    .await;
                for (intent, executed) in intents.into_iter().zip(outcomes) {
                    let class = Some(SideEffectClass::Read);
                    if let Some(log) = &self.options.execution_log {
                        let (success, summary) = match &executed {
                            Ok(result) => (result.success, result.summary_text()),
                            Err(_) => (false, String::new()),
                        };
                        log.lock().expect("execution log poisoned").push(ToolExecutionLogEntry {
                            tool_call_id: intent.tool_call_id.clone(),
                            tool_name: intent.name.clone(),
                            side_effect_class: class,
                            arguments: intent.arguments.clone(),
                            success,
                            summary,
                        });
                    }
                    let executed = match executed {
                        Ok(result) => result,
                        Err(error) => {
                            transcript.push_aborted_results_for_unanswered();
                            return Err(error);
                        }
                    };
                    let result =
                        compact_repeated_web_fetch(&intent, executed, &mut observed_web_sources);
                    let succeeded = result.success;
                    track.lock().expect("recovery track poisoned").completed_tools.push(
                        CompletedToolCall {
                            tool_call_id: intent.tool_call_id.clone(),
                            tool_name: intent.name.clone(),
                            arguments: intent.arguments.clone(),
                            arguments_fingerprint: tool_call_fingerprint(
                                &intent.name,
                                &intent.arguments,
                            )?,
                            success: result.success,
                            summary: result.summary_text(),
                            side_effect_class: SideEffectClass::Read,
                        },
                    );
                    if let Some(reason) = detector.observe_tool_call(&intent, class, &result) {
                        return Err(OrchestratorError::Stuck(reason.to_string()));
                    }
                    let path = crate::tool_cache::path_argument(&intent.arguments);
                    transcript.push_tool_result_classified(
                        intent.tool_call_id.clone(),
                        result,
                        &intent.name,
                        SideEffectClass::Read.as_str(),
                        path.as_deref(),
                    );
                    if succeeded {
                        self.inject_nested_instructions(transcript, &intent.name, path.as_deref());
                        self.emit_session_updates(&intent.name, sink);
                    }
                    repeat_guard.observe_and_push(transcript, &intent.name, &intent.arguments);
                    self.capture_checkpoint(transcript, track, task, false).await;
                }
            } else {
                for intent in intents {
                    self.events.record(OrchestratorEvent::ToolStarted {
                        tool_call_id: intent.tool_call_id.clone(),
                        tool_name: intent.name.clone(),
                    });
                    let class = self
                        .tools
                        .lock()
                        .expect("tool registry poisoned")
                        .get(&intent.name)
                        .map(|tool| tool.definition().side_effect_class);
                    // Resumed turns never replay a completed write/execute/delegate
                    // call with identical arguments: the stored summary is reused
                    // and the operation is not re-run (idempotency guard).
                    let reuse = self.try_reuse_completed(&intent, class, track, transcript);
                    let reused = reuse.is_some();
                    let executed = match reuse {
                        Some(result) => Ok(result),
                        None => {
                            track.lock().expect("recovery track poisoned").in_flight =
                                Some(InFlightOperation {
                                    tool_call_id: intent.tool_call_id.clone(),
                                    tool_name: intent.name.clone(),
                                    arguments_fingerprint: tool_call_fingerprint(
                                        &intent.name,
                                        &intent.arguments,
                                    )?,
                                    started_at_millis: current_unix_millis(),
                                });
                            let executed = self
                                .executor
                                .execute(
                                    &intent,
                                    sink,
                                    client,
                                    cancel.clone(),
                                    task,
                                    transcript.messages(),
                                )
                                .await;
                            track.lock().expect("recovery track poisoned").in_flight = None;
                            executed
                        }
                    };
                    let success = executed.as_ref().is_ok_and(|result| result.success);
                    self.events.record(OrchestratorEvent::ToolFinished {
                        tool_call_id: intent.tool_call_id.clone(),
                        tool_name: intent.name.clone(),
                        success,
                    });
                    if !reused && let Some(log) = &self.options.execution_log {
                        let (success, summary) = match &executed {
                            Ok(result) => (result.success, result.summary_text()),
                            Err(_) => (false, String::new()),
                        };
                        log.lock().expect("execution log poisoned").push(ToolExecutionLogEntry {
                            tool_call_id: intent.tool_call_id.clone(),
                            tool_name: intent.name.clone(),
                            side_effect_class: class,
                            arguments: intent.arguments.clone(),
                            success,
                            summary,
                        });
                    }
                    let result = match executed {
                        Ok(executed) => {
                            compact_repeated_web_fetch(&intent, executed, &mut observed_web_sources)
                        }
                        Err(error) => {
                            transcript.push_aborted_results_for_unanswered();
                            return Err(error);
                        }
                    };
                    let succeeded = result.success;
                    if !reused {
                        track.lock().expect("recovery track poisoned").completed_tools.push(
                            CompletedToolCall {
                                tool_call_id: intent.tool_call_id.clone(),
                                tool_name: intent.name.clone(),
                                arguments: intent.arguments.clone(),
                                arguments_fingerprint: tool_call_fingerprint(
                                    &intent.name,
                                    &intent.arguments,
                                )?,
                                success: result.success,
                                summary: result.summary_text(),
                                side_effect_class: class.unwrap_or(SideEffectClass::Read),
                            },
                        );
                    }
                    if let Some(reason) = detector.observe_tool_call(&intent, class, &result) {
                        return Err(OrchestratorError::Stuck(reason.to_string()));
                    }
                    let path = crate::tool_cache::path_argument(&intent.arguments);
                    match class {
                        Some(class) => {
                            transcript.push_tool_result_classified(
                                intent.tool_call_id.clone(),
                                result,
                                &intent.name,
                                class.as_str(),
                                path.as_deref(),
                            );
                        }
                        None => transcript.push_tool_result(intent.tool_call_id.clone(), result),
                    }
                    if succeeded {
                        self.inject_nested_instructions(transcript, &intent.name, path.as_deref());
                        self.emit_session_updates(&intent.name, sink);
                    }
                    repeat_guard.observe_and_push(transcript, &intent.name, &intent.arguments);
                    self.capture_checkpoint(transcript, track, task, false).await;
                }
            }
            let graph = graph_handle
                .as_ref()
                .map(|handle| handle.lock().expect("task graph poisoned").clone());
            if let Some(reason) = detector.observe_iteration(graph.as_ref()) {
                return Err(OrchestratorError::Stuck(reason.to_string()));
            }
        }
    }
}
