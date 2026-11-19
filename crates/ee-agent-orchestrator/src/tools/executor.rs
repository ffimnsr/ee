//! The tool execution pipeline: lookup, policy gate, budget, lifecycle, outcome.

use std::sync::{Arc, Mutex};

use ee_agent_acp_server::{ClientBridge, UpdateSink, UpdateSinkError};
use ee_agent_protocol::{ContentBlock, TextContent, ToolCallContent, ToolKind};
use tokio::sync::watch;

use crate::budget::BudgetTracker;
use crate::config::OrchestratorConfig;
use crate::decision_log::DecisionLog;
use crate::error::OrchestratorError;
use crate::events::{EventRecorder, OrchestratorEvent};
use crate::model::ModelMessage;
use crate::policy::{PolicyContext, PolicyEngine};
use crate::tasks::TaskNode;
use crate::tool_cache::{ToolResultCache, affected_paths, cache_key, path_argument};
use crate::tools::definition::{
    SideEffectClass, TerminalWaitLimits, ToolCallContext, ToolDefinition, ToolErrorKind,
    ToolIntent, ToolResult,
};
use crate::tools::registry::ToolRegistry;

/// Executes one tool intent end to end: lookup, argument validation, policy
/// gate, budget reservation, update lifecycle, bounded run, and outcome
/// update.  Returns the normalized [`ToolResult`] for the transcript, or a
/// loop-stopping error on cancellation and budget exhaustion.
#[derive(Clone)]
pub struct ToolExecutor {
    config: OrchestratorConfig,
    tools: Arc<Mutex<ToolRegistry>>,
    budget: Arc<Mutex<BudgetTracker>>,
    policy: PolicyEngine,
    depth: usize,
    events: EventRecorder,
    cache: Option<Arc<Mutex<ToolResultCache>>>,
    model_id: Option<String>,
    background_jobs: Option<crate::background_jobs::SharedBackgroundJobs>,
    workspace_remap: Option<Arc<crate::workspace_isolation::WorkspacePathRemap>>,
    approval_classifier: Option<Arc<crate::approval_classifier::ApprovalClassifier>>,
    decisions: Option<Arc<Mutex<DecisionLog>>>,
}

impl ToolExecutor {
    /// Creates an executor sharing the runtime's stores, evaluating policy at
    /// the given subagent depth and recording tool lifecycle events.
    #[must_use]
    pub fn new(
        config: OrchestratorConfig,
        tools: Arc<Mutex<ToolRegistry>>,
        budget: Arc<Mutex<BudgetTracker>>,
        policy: PolicyEngine,
        depth: usize,
        events: EventRecorder,
    ) -> Self {
        Self {
            config,
            tools,
            budget,
            policy,
            depth,
            events,
            cache: None,
            model_id: None,
            background_jobs: None,
            workspace_remap: None,
            approval_classifier: None,
            decisions: None,
        }
    }

    /// Sets the registry model id of the adapter the loop runs on; passed to
    /// tools so delegation can fall back to the parent adapter.
    #[must_use]
    pub fn with_model_id(mut self, model_id: Option<String>) -> Self {
        self.model_id = model_id;
        self
    }

    /// Enables a turn-scoped read-only result cache; write successes
    /// invalidate overlapping path-scoped entries.
    #[must_use]
    pub fn with_cache(mut self, cache: Arc<Mutex<ToolResultCache>>) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Registers created terminals as background jobs and clears a job when
    /// the model waits on it in the foreground.
    #[must_use]
    pub fn with_background_jobs(
        mut self,
        jobs: Option<crate::background_jobs::SharedBackgroundJobs>,
    ) -> Self {
        self.background_jobs = jobs;
        self
    }

    /// Remaps primary-checkout paths into an isolated worktree before policy
    /// evaluation and execution.
    #[must_use]
    pub fn with_workspace_remap(
        mut self,
        remap: Option<Arc<crate::workspace_isolation::WorkspacePathRemap>>,
    ) -> Self {
        self.workspace_remap = remap;
        self
    }

    /// Enables the opt-in risk classifier before host approval dispatch.
    #[must_use]
    pub fn with_approval_classifier(
        mut self,
        classifier: Option<Arc<crate::approval_classifier::ApprovalClassifier>>,
    ) -> Self {
        self.approval_classifier = classifier;
        self
    }

    /// Shares the runtime decision log for classifier audit entries.
    #[must_use]
    pub fn with_decisions(mut self, decisions: Option<Arc<Mutex<DecisionLog>>>) -> Self {
        self.decisions = decisions;
        self
    }

    /// Executes one tool intent, emitting the pending → in-progress →
    /// completed/failed update lifecycle.
    pub async fn execute(
        &self,
        intent: &ToolIntent,
        sink: &UpdateSink,
        client: &ClientBridge,
        cancel: watch::Receiver<bool>,
        task: &TaskNode,
        transcript: &[ModelMessage],
    ) -> Result<ToolResult, OrchestratorError> {
        // Isolated children see primary-checkout paths in prompts and scope
        // rules but must operate on their worktree; remap before validation,
        // policy, cache keys, and execution so every layer agrees.
        let mut remapped;
        let intent = match &self.workspace_remap {
            Some(remap) => {
                remapped = intent.clone();
                remap.remap_arguments(&mut remapped.arguments);
                &remapped
            }
            None => intent,
        };
        let tool = self.tools.lock().expect("tool registry poisoned").get(&intent.name);
        let Some(tool) = tool else {
            let result = ToolResult::failure(
                ToolErrorKind::Backend,
                format!("unknown tool: {}", intent.name),
            );
            emit_failed_tool(sink, intent, &result)?;
            return Ok(result);
        };
        let definition = tool.definition();

        if let Some(reason) = &intent.argument_error {
            // The model's own malformed JSON is a replayable tool error: return
            // the parse failure so it can correct the call on the next step.
            let result = ToolResult::failure(ToolErrorKind::InvalidArguments, reason.clone());
            emit_failed_tool(sink, intent, &result)?;
            return Ok(result);
        }

        if let Err(reason) = definition.validate_arguments(&intent.arguments) {
            let result = ToolResult::failure(ToolErrorKind::InvalidArguments, reason);
            emit_failed_tool(sink, intent, &result)?;
            return Ok(result);
        }

        let decision = self.policy.check_with_arguments(
            &definition,
            PolicyContext { subagent_depth: self.depth, active_delegates: 0 },
            &intent.arguments,
        );
        if !decision.allow {
            let reason = decision.reason.unwrap_or_else(|| "tool denied by policy".to_string());
            let result = ToolResult::failure(ToolErrorKind::PermissionDenied, reason);
            emit_failed_tool(sink, intent, &result)?;
            return Ok(result);
        }
        if definition.host_approval {
            if let Some(classifier) = &self.approval_classifier {
                let candidate = crate::approval_classifier::ApprovalCandidate::new(
                    &definition.name,
                    definition.side_effect_class,
                    definition.side_effect_subclass,
                    &intent.arguments,
                );
                let outcome = classifier.classify(&candidate).await;
                match &self.decisions {
                    Some(decisions) => classifier.record(
                        &candidate,
                        &outcome,
                        &mut decisions.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
                        &self.events,
                    ),
                    None => {
                        let mut decisions = DecisionLog::default();
                        classifier.record(&candidate, &outcome, &mut decisions, &self.events);
                    }
                }
                match &outcome {
                    crate::approval_classifier::ApprovalClassification::Deny { reason } => {
                        let result =
                            ToolResult::failure(ToolErrorKind::PermissionDenied, reason.clone());
                        emit_failed_tool(sink, intent, &result)?;
                        return Ok(result);
                    }
                    // A non-low classification keeps the interactive host prompt,
                    // so the approval waterfall stage must still observe it.
                    crate::approval_classifier::ApprovalClassification::NeedsApproval {
                        ..
                    } => {
                        self.events.record(OrchestratorEvent::ApprovalRequested {
                            tool_call_id: intent.tool_call_id.clone(),
                            tool_name: definition.name.clone(),
                        });
                    }
                    crate::approval_classifier::ApprovalClassification::AutoApprove { .. } => {}
                }
            } else {
                self.events.record(OrchestratorEvent::ApprovalRequested {
                    tool_call_id: intent.tool_call_id.clone(),
                    tool_name: definition.name.clone(),
                });
            }
        }
        {
            let mut budget = self.budget.lock().expect("budget tracker poisoned");
            budget.try_reserve_tool_call()?;
            budget.emit(&self.events);
        }

        // Read-only result cache: on a hit the tool is not executed, but the
        // update lifecycle is still streamed from the cached result and the
        // tool call still counts against the per-turn budget.
        let session_id = sink.session_id().to_string();
        // Plugin tool gates run after mandatory policy, budget reservation, and
        // the write-transaction/ownership checks above. A plugin may deny an
        // otherwise allowed call; it can never allow a mandatory denial.
        if let Some(gate) = self.policy.plugin_gate() {
            let decision = gate.check_tool(
                &definition,
                PolicyContext { subagent_depth: self.depth, active_delegates: 0 },
                &intent.arguments,
            );
            if !decision.allow {
                let reason =
                    decision.reason.unwrap_or_else(|| "tool denied by plugin gate".to_string());
                let result = ToolResult::failure(ToolErrorKind::PermissionDenied, reason);
                emit_failed_tool(sink, intent, &result)?;
                return Ok(result);
            }
        }
        if let Some(cache) = &self.cache {
            let key = cache_key(
                &definition.name,
                &intent.arguments,
                &session_id,
                path_argument(&intent.arguments),
            );
            if let Some(cached) = cache.lock().expect("tool cache poisoned").get(&key) {
                sink.tool_call_pending(
                    &intent.tool_call_id,
                    &definition.name,
                    tool_kind(&definition),
                )
                .map_err(map_update_error)?;
                sink.tool_call_in_progress(&intent.tool_call_id, &definition.name, Vec::new())
                    .map_err(map_update_error)?;
                emit_tool_outcome(sink, intent, &cached)?;
                return Ok(cached);
            }
        }

        sink.tool_call_pending(&intent.tool_call_id, &definition.name, tool_kind(&definition))
            .map_err(map_update_error)?;
        sink.tool_call_in_progress(&intent.tool_call_id, &definition.name, Vec::new())
            .map_err(map_update_error)?;

        let arguments = intent.arguments.clone();
        let terminal_wait = TerminalWaitLimits {
            default: self.config.tool_timeouts.terminal_default,
            max: self.config.tool_timeouts.terminal_max,
        };
        let context = ToolCallContext {
            task: task.clone(),
            transcript: transcript.to_vec(),
            events: self.events.clone(),
            scope: self.policy.policy().scope.clone(),
            model_id: self.model_id.clone(),
            terminal_wait,
        };
        let requested_ms = intent.arguments.get("timeout_ms").and_then(serde_json::Value::as_u64);
        let tool_timeout = self.config.tool_timeouts.effective_timeout(
            &definition.name,
            definition.side_effect_class,
            requested_ms,
        );
        let result = tokio::time::timeout(
            tool_timeout,
            tool.execute(arguments, client.clone(), cancel.clone(), context),
        )
        .await
        .unwrap_or_else(|_| {
            ToolResult::failure(
                ToolErrorKind::Timeout,
                format!("tool {} timed out after {tool_timeout:?}", definition.name),
            )
        });

        if *cancel.borrow() {
            return Err(OrchestratorError::Cancellation);
        }
        self.track_background_job(&definition, intent, &result);
        if let Some(cache) = &self.cache {
            if result.success && definition.side_effect_class == SideEffectClass::Read {
                let key = cache_key(
                    &definition.name,
                    &intent.arguments,
                    &session_id,
                    path_argument(&intent.arguments),
                );
                cache.lock().expect("tool cache poisoned").insert(key, result.clone());
            }
            if result.success && definition.side_effect_class == SideEffectClass::Write {
                let mut cache = cache.lock().expect("tool cache poisoned");
                for path in affected_paths(&definition, &intent.arguments) {
                    cache.invalidate_path(&session_id, &path);
                }
            }
        }
        emit_tool_outcome(sink, intent, &result)?;
        Ok(result)
    }

    /// Records or clears background terminal jobs from terminal lifecycle
    /// outcomes. Only successful calls change registry state.
    pub(crate) fn track_background_job(
        &self,
        definition: &ToolDefinition,
        intent: &ToolIntent,
        result: &ToolResult,
    ) {
        let Some(jobs) = &self.background_jobs else {
            return;
        };
        if !result.success {
            return;
        }
        match definition.name.as_str() {
            "create_terminal" => {
                let background = intent
                    .arguments
                    .get("background")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true);
                if !background {
                    return;
                }
                let Some(terminal_id) = result
                    .structured_output
                    .as_ref()
                    .and_then(|value| value.get("terminal_id"))
                    .and_then(serde_json::Value::as_str)
                else {
                    return;
                };
                let command = intent
                    .arguments
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("terminal command");
                jobs.register(terminal_id.to_string(), command.to_string());
            }
            "wait_for_terminal_exit" => {
                if let Some(terminal_id) =
                    intent.arguments.get("terminal_id").and_then(serde_json::Value::as_str)
                {
                    jobs.note_foreground_wait(terminal_id);
                }
            }
            _ => {}
        }
    }
}

/// Maps an SDK-side-effect class onto the ACP `ToolKind` update vocabulary.
fn tool_kind(definition: &ToolDefinition) -> ToolKind {
    match definition.side_effect_class {
        SideEffectClass::Read => ToolKind::Read,
        SideEffectClass::Write => ToolKind::Edit,
        SideEffectClass::Execute => ToolKind::Execute,
        SideEffectClass::Delegate => ToolKind::Other,
    }
}

/// Emits the completed/failed tool-call update for a finished tool.
pub(crate) fn emit_tool_outcome(
    sink: &UpdateSink,
    intent: &ToolIntent,
    result: &ToolResult,
) -> Result<(), OrchestratorError> {
    if result.success {
        let content = vec![ToolCallContent::from(ContentBlock::Text(TextContent::new(
            result.summary_text(),
        )))];
        sink.tool_call_completed(&intent.tool_call_id, &intent.name, content)
            .map_err(map_update_error)
    } else {
        emit_failed_tool(sink, intent, result)
    }
}

/// Emits a failed tool-call update carrying the error summary as content.
fn emit_failed_tool(
    sink: &UpdateSink,
    intent: &ToolIntent,
    result: &ToolResult,
) -> Result<(), OrchestratorError> {
    sink.tool_call_failed(&intent.tool_call_id, &intent.name, result.summary_text())
        .map_err(map_update_error)
}

fn map_update_error(error: UpdateSinkError) -> OrchestratorError {
    OrchestratorError::InvalidState(format!("update emission failed: {error}"))
}
