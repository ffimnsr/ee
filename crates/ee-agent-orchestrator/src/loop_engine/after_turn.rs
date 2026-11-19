//! Post-loop work: the iteration-cap summary, instruction injection,
//! resume reuse, and checkpoint capture.

use super::util::touches_workspace_instructions;
use super::*;

impl LoopEngine {
    ///
    /// Tools are disabled and the summary instruction is appended as an
    /// assistant message, matching the reference runner: the provider's cached
    /// tool prefix stays intact and the model can only answer in prose.
    pub(super) async fn finish_at_max_steps(
        &self,
        input: MaxStepsCall<'_>,
    ) -> Result<PromptResult, OrchestratorError> {
        let MaxStepsCall {
            transcript,
            sink,
            cancel,
            task,
            session,
            mut turn_usage,
            session_id,
            max_iterations,
        } = input;
        if *cancel.borrow() {
            return Err(OrchestratorError::Cancellation);
        }
        let tools = {
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
        transcript.push_message(ModelMessage::text(ModelRole::Assistant, MAX_STEPS_PROMPT));
        let request_messages =
            crate::compaction::prune_redundant_readonly_tool_outputs(transcript.request_messages());
        let prepared = crate::prompt_injection::prepare_request(&request_messages);
        let budget_snapshot = {
            let mut budget = self.budget.lock().expect("budget tracker poisoned");
            // The summary is the point of the cap; when the model-call budget is
            // already spent, stop with the accumulated answer rather than error.
            if budget.try_reserve_model_call().is_err() {
                return Ok(prompt_result_with_usage(StopReason::MaxTurnRequests, turn_usage));
            }
            budget.snapshot()
        };
        let request = ModelRequest::new(prepared.messages, tools, budget_snapshot, task.clone())
            .with_available_models(self.options.available_models.clone())
            .with_model_id(self.options.model_id.clone())
            .with_session(Some(session.clone()))
            .with_tool_control(crate::model_options::ToolCallControl {
                choice: crate::model_options::ToolChoice::None,
                parallel: None,
            });
        if let Err(error) = transcript.record_model_request(&request) {
            self.events.record(OrchestratorEvent::Error { error: error.to_string() });
            return Err(error);
        }
        self.events.record(OrchestratorEvent::ModelRequested { iteration: max_iterations });
        let message_id = format!("max-steps-{max_iterations}");
        let response = run_streaming_response(
            |events| self.model.complete_streaming(request, cancel.clone(), events),
            Some(sink),
            cancel,
            &message_id,
        )
        .await?;
        self.events.record(OrchestratorEvent::ModelResponded { iteration: max_iterations });
        turn_usage = turn_usage.accumulate(&response.usage);
        if let Some(log) = transcript.session_log() {
            let _ = log.record_usage(&response.usage, self.options.model_id.as_deref());
        }
        // The summary is recorded like any other round so usage reporting and
        // the output-byte accounting stay consistent; a budget breach here does
        // not turn the terminal summary into an error.
        {
            let mut budget = self.budget.lock().expect("budget tracker poisoned");
            let output_bytes =
                response.text.len() + response.reasoning.as_deref().map_or(0, str::len);
            let _ = budget.record_model_usage(
                output_bytes,
                response.usage.input_tokens,
                response.usage.output_tokens,
            );
            budget.emit(&self.events);
        }
        if let Some(used) = response.usage.context_tokens.or(response.usage.input_tokens) {
            let _ = sink.raw_update(SessionUpdate::UsageUpdate(UsageUpdate::new(
                used as u64,
                self.config.context_window_tokens,
            )));
        }
        if let Some(metrics) = &self.options.metrics {
            metrics.lock().expect("orchestrator metrics poisoned").record_usage_for_model(
                self.options.model_id.as_deref().unwrap_or("unknown"),
                &response.usage,
            );
        }
        transcript.push_assistant(&response);
        self.events.record(OrchestratorEvent::StepStarted {
            session_id: session_id.to_string(),
            task_id: task.id.as_str().to_string(),
            iteration: max_iterations,
            advertised_tool_count: 0,
        });
        Ok(prompt_result_with_usage(StopReason::MaxTurnRequests, turn_usage))
    }

    /// Emits the current ACP plan after a successful `todowrite` mutation.
    ///
    /// Emission is best-effort: a closed sink is recorded as an event instead
    /// of failing an otherwise completed tool call.
    pub(super) fn emit_plan_update(&self, sink: &UpdateSink) {
        let Some(plan) = self.options.plan.as_ref() else { return };
        let entries = plan.lock().expect("todo plan poisoned").plan_entries();
        if let Err(error) = sink.plan_replace(entries) {
            self.events.record(OrchestratorEvent::Error {
                error: format!("plan emission failed: {error}"),
            });
        }
    }

    /// Emits a queued session title after a successful `session_rename`.
    ///
    /// The title is consumed only when the update was queued, so a closed
    /// sink leaves it pending for the next turn instead of losing it.
    pub(super) fn emit_pending_title(&self, sink: &UpdateSink) {
        let Some(pending) = self.options.session_title.as_ref() else { return };
        let title = pending.lock().expect("pending title poisoned").clone();
        let Some(title) = title else { return };
        let update = SessionInfoUpdate::new().title(title.clone());
        match sink.session_info_update(update) {
            Ok(()) => {
                let mut pending = pending.lock().expect("pending title poisoned");
                if pending.as_deref() == Some(title.as_str()) {
                    *pending = None;
                }
            }
            Err(error) => {
                self.events.record(OrchestratorEvent::Error {
                    error: format!("session title emission failed: {error}"),
                });
            }
        }
    }

    /// Emits the ACP updates a successful tool call may have produced.
    pub(super) fn emit_session_updates(&self, tool_name: &str, sink: &UpdateSink) {
        if tool_name == "todowrite" {
            self.emit_plan_update(sink);
        }
        if tool_name == "session_rename" {
            self.emit_pending_title(sink);
        }
    }

    /// Injects nested workspace instructions discovered under a touched path.
    ///
    /// The tracker deduplicates by content digest and enforces the remaining
    /// 64 KiB budget, so a long turn cannot replay the same nested file or
    /// grow the prompt without bound.
    pub(super) fn inject_nested_instructions(
        &self,
        transcript: &mut Transcript,
        tool_name: &str,
        path: Option<&str>,
    ) {
        if !touches_workspace_instructions(tool_name) {
            return;
        }
        let Some(path) = path else { return };
        let Some(tracker) = &self.options.workspace_instructions else { return };
        let messages = tracker
            .lock()
            .expect("workspace instruction tracker poisoned")
            .discover_for_path(Path::new(path));
        for message in messages {
            transcript.push_message(message);
        }
    }

    /// Reuses the stored result of a completed write/execute/delegate call
    /// when the model requests the identical operation again after a resume;
    /// pushes the reused observation into the transcript and returns the
    /// reconstructed result.  Reads are never reused (they are idempotent
    /// and may re-run).
    pub(super) fn try_reuse_completed(
        &self,
        intent: &ToolIntent,
        class: Option<SideEffectClass>,
        track: &Mutex<ResumeState>,
        transcript: &mut Transcript,
    ) -> Option<ToolResult> {
        // Idempotency guard only exists on resumed runs: within a fresh run
        // the stuck detector needs repeated identical calls to execute.
        self.resume_state.as_ref()?;
        let class = class?;
        if !matches!(
            class,
            SideEffectClass::Write | SideEffectClass::Execute | SideEffectClass::Delegate
        ) {
            return None;
        }
        let fingerprint = tool_call_fingerprint(&intent.name, &intent.arguments).ok()?;
        let completed = track
            .lock()
            .expect("recovery track poisoned")
            .completed_tools
            .iter()
            .find(|completed| {
                completed.tool_name == intent.name && completed.arguments_fingerprint == fingerprint
            })
            .cloned()?;
        self.events.record(OrchestratorEvent::ToolResultReused {
            tool_call_id: intent.tool_call_id.clone(),
            tool_name: intent.name.clone(),
        });
        let result = ToolResult {
            success: completed.success,
            text_output: "previous side-effect result withheld from durable recovery state".into(),
            structured_output: None,
            error_kind: None,
        };
        transcript.push_tool_result(intent.tool_call_id.clone(), result.clone());
        Some(result)
    }

    /// Builds and persists a recovery checkpoint from the current stores and
    /// the per-turn recovery track.  Best-effort: capture failures are
    /// recorded as events and never fail the turn.
    pub(super) async fn capture_checkpoint(
        &self,
        transcript: &Transcript,
        track: &Mutex<ResumeState>,
        task: &TaskNode,
        force: bool,
    ) {
        let Some(handle) = &self.checkpoint else { return };
        let Some(memory) = &self.memory else { return };
        let Some(tasks) = self.graph.as_ref() else { return };
        let mut resume = track.lock().expect("recovery track poisoned").clone();
        resume.transcript = transcript.messages().to_vec();
        resume.active_task_id = task.id.as_str().to_string();
        let checkpoint = {
            let tasks = tasks.lock().expect("task graph poisoned").clone();
            let memory = memory.lock().expect("memory store poisoned").clone();
            let budget = self.budget.lock().expect("budget tracker poisoned").snapshot();
            let summary = TranscriptSummary::from_transcript(transcript.messages());
            OrchestratorCheckpoint::with_recovery_metadata(
                "checkpoint",
                self.config.clone(),
                handle.session_id(),
                tasks,
                memory,
                summary,
                budget,
                SubagentTreeState::new(),
                IdGeneratorState::new(),
                handle.provider(),
                current_unix_millis(),
                Some(resume),
                handle.capture_metadata(if force {
                    CheckpointCaptureOrigin::Interruption
                } else {
                    CheckpointCaptureOrigin::Milestone
                }),
            )
        };
        let checkpoint = match checkpoint {
            Ok(checkpoint) => checkpoint,
            Err(error) => {
                self.events.record(OrchestratorEvent::Error {
                    error: format!("checkpoint capture failed: {error}"),
                });
                return;
            }
        };
        let saved = if force {
            handle.save_terminal(&checkpoint).map(Some)
        } else {
            handle.save_milestone(&checkpoint)
        };
        match saved {
            Ok(Some(checkpoint_id)) => {
                if let Some(log) = transcript.session_log() {
                    let seq = checkpoint_id
                        .rsplit('-')
                        .next()
                        .and_then(|value| value.parse().ok())
                        .unwrap_or_default();
                    let _ = log.record_checkpoint(&checkpoint_id, seq);
                }
                self.events.record(OrchestratorEvent::CheckpointSaved {
                    session_id: handle.session_id().to_string(),
                    checkpoint_id,
                });
            }
            Ok(None) => {}
            Err(error) => {
                self.events.record(OrchestratorEvent::Error {
                    error: format!("checkpoint capture failed: {error}"),
                });
            }
        }
    }
}
