//! Subagent delegation: logical in-process workers.
//!
//! Subagents are not OS processes; they are reduced [`LoopEngine`] runs over
//! the shared tool registry, with a scoped role (name, instructions, allowed
//! tool classes, iteration cap, optional model selection), a child task node
//! in the task graph, and a bounded summary returned to the parent.  The
//! [`SubagentManager`] enforces the configured depth and parallelism limits,
//! propagates cancellation from parent to children, and merges child memory
//! items (never sensitive ones) into the parent store — after the child
//! summary's citations were verified against its execution evidence, and
//! only when the child completed.  Failed, cancelled, and unverified child
//! output is quarantined instead of merged.  The built-in `delegate_task`
//! tool exposes delegation to the model; the built-in role library lives in
//! [`crate::subagent_roles`].
//!
//! Model selection: the manager resolves the child adapter through the
//! shared [`ModelRegistry`] before the child task node exists — a role's
//! `model` id wins, otherwise the parent loop's adapter id (the registry
//! default at the root) is the fallback.  Unknown ids are rejected with a
//! deterministic error and never create a node.  The selected id is recorded
//! on the child task, in the `SubagentStarted` event, and in the child's
//! `ModelRequest` diagnostic metadata; the advertised model list is exposed
//! to the delegating model through `ModelRequest` and the `delegate_task`
//! schema.

use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ee_acp_agent_server::{ClientBridge, UpdateSink};
use ee_agent_protocol::SessionId;
use serde::{Deserialize, Serialize};
use tokio::sync::{Semaphore, mpsc, watch};

use crate::budget::BudgetTracker;
use crate::config::OrchestratorConfig;
use crate::error::OrchestratorError;
use crate::events::{EventRecorder, OrchestratorEvent};
use crate::loop_engine::{LoopEngine, LoopOptions};
use crate::memory::{MemoryItem, MemoryStore};
use crate::model::{ModelAdapter, ModelMessage, ModelRole, Transcript};
use crate::model_registry::{DEFAULT_MODEL_ID, ModelRegistry};
use crate::policy::{PolicyEngine, ToolPolicy};
use crate::subagent_verifier::{
    SubagentCitations, SubagentEvidence, SubagentQuarantine, SubagentResultVerifier,
};
use crate::tasks::{TaskGraph, TaskId, TaskNode, TaskStatus, truncate};
use crate::tool_dependencies::{ToolDataClass, ToolDependency};
use crate::tools::{
    ServerTool, SideEffectClass, ToolCallContext, ToolDefinition, ToolErrorKind, ToolFuture,
    ToolRegistry, ToolResult,
};
use crate::trust::TrustLevel;
use crate::workspace_scope::WorkspaceScope;

/// Default max loop iterations for a subagent role.
pub const SUBAGENT_DEFAULT_MAX_ITERATIONS: usize = 8;
/// Cap on the summary (and error summary) a subagent returns to its parent.
pub const SUBAGENT_SUMMARY_MAX_CHARS: usize = 4_000;
/// Session id namespace for subagent turns; subagent work streams no updates
/// to the client, so this only labels internal events.
const SUBAGENT_SESSION: &str = "subagent";
/// Default role name when the model omits one.
const DEFAULT_ROLE_NAME: &str = "worker";

/// Stable identifier for one subagent (1:1 with its child task).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SubagentId(String);

impl SubagentId {
    /// Creates a subagent id from its string form.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SubagentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A model request to delegate work to a subagent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SubagentIntent {
    /// What the subagent should accomplish.
    pub task_description: String,
}

impl SubagentIntent {
    /// Creates a delegation intent.
    #[must_use]
    pub fn new(task_description: impl Into<String>) -> Self {
        Self { task_description: task_description.into() }
    }
}

/// Scoped worker configuration for one subagent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SubagentRole {
    /// Human-readable role name (also the child task title).
    pub name: String,
    /// Instructions seeded as a system message in the child transcript.
    pub instructions: String,
    /// Tool classes the child may use; everything else is policy-denied.
    pub allowed_tool_classes: Vec<SideEffectClass>,
    /// Child loop iteration cap.
    pub max_iterations: usize,
    /// File globs narrowing the parent workspace scope for this child; empty
    /// inherits the parent scope unchanged.
    pub allowed_scope_globs: Vec<String>,
    /// Registry model id the child runs on; `None` falls back to the parent
    /// loop's adapter.
    #[serde(default)]
    pub model: Option<String>,
}

impl SubagentRole {
    /// Creates a read-only role with default iteration cap.
    #[must_use]
    pub fn new(name: impl Into<String>, instructions: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            instructions: instructions.into(),
            allowed_tool_classes: vec![SideEffectClass::Read],
            max_iterations: SUBAGENT_DEFAULT_MAX_ITERATIONS,
            allowed_scope_globs: Vec::new(),
            model: None,
        }
    }

    /// Sets the allowed tool classes.
    #[must_use]
    pub fn with_allowed_tool_classes(mut self, classes: Vec<SideEffectClass>) -> Self {
        self.allowed_tool_classes = classes;
        self
    }

    /// Sets the child loop iteration cap.
    #[must_use]
    pub fn with_max_iterations(mut self, iterations: usize) -> Self {
        self.max_iterations = iterations;
        self
    }

    /// Sets the file globs narrowing the parent workspace scope for the
    /// child; empty inherits the parent scope.
    #[must_use]
    pub fn with_allowed_scope_globs(mut self, globs: Vec<String>) -> Self {
        self.allowed_scope_globs = globs;
        self
    }

    /// Sets the registry model id this child runs on; `None` (the default)
    /// falls back to the parent loop's adapter.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }
}

/// Delegation request from the model, before the child task node exists.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub(crate) struct DelegationRequest {
    /// Task the subagent is delegated from.
    pub parent_task_id: TaskId,
    /// Scoped worker configuration.
    pub role: SubagentRole,
    /// The delegation prompt.
    pub scoped_prompt: String,
    /// Parent transcript snapshot the child sees as context.
    pub context_snapshot: Vec<ModelMessage>,
    /// Parent's active workspace scope; the child scope is narrowed from it.
    pub scope: Option<WorkspaceScope>,
    /// Registry id of the adapter the delegating loop runs on; the fallback
    /// when the role selects no model.
    pub model_id: Option<String>,
}

/// Full delegation request, built by the manager after the child task node
/// exists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SubagentRequest {
    /// Task the subagent is delegated from.
    pub parent_task_id: TaskId,
    /// Task node created for the subagent.
    pub child_task_id: TaskId,
    /// Scoped worker configuration.
    pub role: SubagentRole,
    /// The delegation prompt.
    pub scoped_prompt: String,
    /// Parent transcript snapshot the child sees as context.
    pub context_snapshot: Vec<ModelMessage>,
    /// The child's narrowed workspace scope (roots inherited, globs narrowed).
    pub scope: Option<WorkspaceScope>,
    /// Intended absolute write paths for this child.  When the fan-out
    /// coordinator's write-scope detector is active, overlapping scopes of
    /// concurrent children are rejected before spawn; empty means the child
    /// intends no writes.
    #[serde(default)]
    pub write_scope: Vec<PathBuf>,
    /// Registry model id the child runs on (resolved selection or parent
    /// fallback), recorded before the child task node was created.
    #[serde(default)]
    pub model_id: Option<String>,
}

impl SubagentRequest {
    /// Sets the intended absolute write paths for the child.
    #[must_use]
    pub fn with_write_scope(mut self, scope: Vec<PathBuf>) -> Self {
        self.write_scope = scope;
        self
    }
}

/// Terminal outcome of one subagent run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SubagentStatus {
    /// The child loop ended normally.
    Completed,
    /// The child loop failed.
    Failed,
    /// The child loop was cancelled (directly or by the parent).
    Cancelled,
}

/// Bounded outcome of one subagent run, returned to the parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SubagentResult {
    /// Stable subagent id.
    pub subagent_id: SubagentId,
    /// Terminal status.
    pub status: SubagentStatus,
    /// Bounded summary (newest assistant text when completed).
    pub summary: String,
    /// Memory items the child produced (never sensitive).
    pub produced_memory_items: Vec<MemoryItem>,
    /// Tool calls the child executed.
    pub tool_call_count: usize,
    /// Bounded error summary, when the child failed or was cancelled.
    pub error_summary: Option<String>,
    /// Citations the summary claims (`[file:path]` / `[tool:name]` markers in
    /// the summary text, or structured values); checked against the child's
    /// execution evidence before its memory may merge.
    #[serde(default)]
    pub citations: SubagentCitations,
}

/// A resolved child adapter: the adapter plus its registry id.
pub(crate) struct ResolvedModel {
    adapter: Arc<dyn ModelAdapter>,
    id: Option<String>,
}

/// In-process subagent manager enforcing depth, parallelism, and scoped
/// memory rules.  Owned by the runtime and driven by [`DelegateTool`].
pub(crate) struct SubagentManager {
    config: OrchestratorConfig,
    models: Arc<ModelRegistry>,
    tools: Arc<Mutex<ToolRegistry>>,
    tasks: Arc<Mutex<TaskGraph>>,
    memory: Arc<Mutex<MemoryStore>>,
    budget: Arc<Mutex<BudgetTracker>>,
    semaphore: Arc<Semaphore>,
    quarantine: Arc<Mutex<SubagentQuarantine>>,
}

impl SubagentManager {
    /// Creates a manager sharing the runtime's stores, model registry, and
    /// per-turn budget tracker.
    pub(crate) fn new(
        config: OrchestratorConfig,
        models: Arc<ModelRegistry>,
        tools: Arc<Mutex<ToolRegistry>>,
        tasks: Arc<Mutex<TaskGraph>>,
        memory: Arc<Mutex<MemoryStore>>,
        budget: Arc<Mutex<BudgetTracker>>,
    ) -> Self {
        let semaphore = Arc::new(Semaphore::new(config.max_parallel_subagents));
        let quarantine = Arc::new(Mutex::new(SubagentQuarantine::default()));
        Self { config, models, tools, tasks, memory, budget, semaphore, quarantine }
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
            return Err(OrchestratorError::InvalidState(format!(
                "subagent depth limit exceeded (max {})",
                self.config.max_subagent_depth
            )));
        }

        // Resolve the child adapter before the child task node is created:
        // an explicit role selection must exist in the registry, otherwise
        // the parent loop's adapter is the fallback.  Unknown ids never
        // create a node.
        let model = self.resolve_model(request.role.model.clone(), request.model_id)?;

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
        events.record(OrchestratorEvent::SubagentStarted {
            subagent_id: subagent_id.as_str().to_string(),
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

        let permit = tokio::select! {
            permit = self.semaphore.acquire() => permit.expect("subagent semaphore is never closed"),
            _ = cancelled(cancel.clone()) => {
                let result = SubagentResult {
                    subagent_id: subagent_id.clone(),
                    status: SubagentStatus::Cancelled,
                    summary: String::new(),
                    produced_memory_items: Vec::new(),
                    tool_call_count: 0,
                    error_summary: Some("cancelled while waiting for a subagent permit".into()),
                    citations: SubagentCitations::default(),
                };
                self.finish_child(&request, &result, events).await;
                return Ok(result);
            }
        };
        let _permit = permit;

        {
            let mut tasks = self.tasks.lock().expect("task graph poisoned");
            tasks.transition(&child.id, TaskStatus::Running).expect("pending -> running");
        }
        let result = self.run_child(request, child, model, client, cancel, events).await;
        Ok(result)
    }

    /// Resolves the child adapter: the role's explicit `model` selection when
    /// present (unknown ids fail closed), else the parent loop's adapter id,
    /// else the registry default.  Returns the adapter and the resolved id.
    fn resolve_model(
        &self,
        selected: Option<String>,
        parent_id: Option<String>,
    ) -> Result<ResolvedModel, OrchestratorError> {
        let id = match selected {
            Some(id) => {
                if !self.models.contains(&id) {
                    return Err(OrchestratorError::InvalidState(format!("unknown model id: {id}")));
                }
                id
            }
            None => parent_id.unwrap_or_else(|| DEFAULT_MODEL_ID.to_string()),
        };
        let adapter = self.models.get(&id).ok_or_else(|| {
            OrchestratorError::InvalidState(format!("model registry has no adapter {id}"))
        })?;
        Ok(ResolvedModel { adapter, id: Some(id) })
    }

    /// Runs the child loop and finalizes the child task node, memory, and
    /// events.
    async fn run_child(
        &self,
        request: SubagentRequest,
        child: TaskNode,
        model: ResolvedModel,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        events: EventRecorder,
    ) -> SubagentResult {
        let depth = self.task_depth(&child.id);
        let subagent_id = SubagentId::new(child.id.as_str());
        // Reduced config: the child gets the role's iteration cap and the
        // configured subagent timeout instead of the turn timeout.
        let child_config = OrchestratorConfig {
            max_loop_iterations: request.role.max_iterations,
            turn_timeout: self.config.subagent_timeout,
            ..self.config.clone()
        };
        let child_budget = Arc::new(Mutex::new(BudgetTracker::new(&child_config)));
        let child_policy = ToolPolicy {
            allow_read: request.role.allowed_tool_classes.contains(&SideEffectClass::Read),
            allow_write: request.role.allowed_tool_classes.contains(&SideEffectClass::Write),
            allow_execute: request.role.allowed_tool_classes.contains(&SideEffectClass::Execute),
            allow_delegate: request.role.allowed_tool_classes.contains(&SideEffectClass::Delegate),
            max_delegate_depth: self.config.max_subagent_depth,
            max_parallel_delegates: self.config.max_parallel_subagents,
            // Destructive subclasses default to denied for children; scope
            // narrows from the parent's active scope.
            allowed_side_effect_subclasses: Default::default(),
            owned_terminal_ids: Default::default(),
            scope: request.scope.clone(),
        };
        // The child's execution log becomes the verification evidence for its
        // summary citations before any memory merge.
        let execution_log = Arc::new(Mutex::new(Vec::new()));
        let engine = LoopEngine::new(
            child_config,
            model.adapter,
            self.tools.clone(),
            child_budget.clone(),
            PolicyEngine::new(child_policy),
            events.clone(),
            LoopOptions {
                depth,
                graph: Some(self.tasks.clone()),
                execution_log: Some(execution_log.clone()),
                available_models: self.models.advertised(),
                model_id: model.id,
                ..LoopOptions::default()
            },
        );

        // Scoped transcript: role instructions, parent context snapshot, and
        // the delegation prompt as the newest user message.
        let mut transcript = Transcript::new();
        if !request.role.instructions.is_empty() {
            transcript.prepend_system(request.role.instructions.clone());
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

        let (status, summary, error_summary) = match outcome {
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
        let summary = truncate(&summary, SUBAGENT_SUMMARY_MAX_CHARS);
        let error_summary = error_summary.map(|text| truncate(&text, SUBAGENT_SUMMARY_MAX_CHARS));
        let citations = SubagentCitations::extract(&summary);

        // Every subagent produces a summary fact; child store items (always
        // non-sensitive) merge too when the parent verified the summary.  The
        // summary is untrusted subagent content.  Failed, cancelled, and
        // unverified output is quarantined instead of merged.
        let summary_item = MemoryItem::from_task(
            format!("subagent:{subagent_id}"),
            summary.clone(),
            request.child_task_id.clone(),
        )
        .with_trust(TrustLevel::SubagentSummaryUntrusted);
        let produced_memory_items = vec![summary_item];
        let result = SubagentResult {
            subagent_id,
            status,
            summary,
            produced_memory_items,
            tool_call_count,
            error_summary,
            citations,
        };
        let evidence = SubagentEvidence::from_execution_log(
            &execution_log.lock().expect("execution log poisoned"),
        );
        {
            let mut memory = self.memory.lock().expect("memory store poisoned");
            let mut quarantine = self.quarantine.lock().expect("subagent quarantine poisoned");
            match result.status {
                SubagentStatus::Completed => {
                    let verification =
                        SubagentResultVerifier::new().verify(&request.role, &result, &evidence);
                    if verification.verified {
                        merge_memory_items(&mut memory, &result.produced_memory_items);
                    } else {
                        quarantine.quarantine(
                            &result,
                            verification
                                .rejected_reason
                                .clone()
                                .unwrap_or_else(|| "unverified subagent summary".into()),
                        );
                    }
                }
                SubagentStatus::Failed => {
                    quarantine.quarantine(&result, "subagent failed");
                }
                SubagentStatus::Cancelled => {
                    quarantine.quarantine(&result, "subagent cancelled");
                }
            }
        }
        self.finish_child(&request, &result, events).await;
        result
    }

    /// Applies the result to the child task node and records the terminal
    /// subagent event.
    async fn finish_child(
        &self,
        request: &SubagentRequest,
        result: &SubagentResult,
        events: EventRecorder,
    ) {
        let final_status = match result.status {
            SubagentStatus::Completed => TaskStatus::Completed,
            SubagentStatus::Failed => TaskStatus::Failed,
            SubagentStatus::Cancelled => TaskStatus::Cancelled,
        };
        let mut tasks = self.tasks.lock().expect("task graph poisoned");
        tasks.transition(&request.child_task_id, final_status).expect("child task transitions");
        drop(tasks);
        events.record(OrchestratorEvent::SubagentFinished {
            subagent_id: result.subagent_id.as_str().to_string(),
            success: result.status == SubagentStatus::Completed,
        });
    }

    /// Nesting depth of `task_id` in the graph (root is 0).
    fn task_depth(&self, task_id: &TaskId) -> usize {
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

/// Merges child memory items into the parent store, skipping sensitive items
/// and items the store rejects (e.g. over the byte limit).  Returns the
/// number of merged items.
pub(crate) fn merge_memory_items(store: &mut MemoryStore, items: &[MemoryItem]) -> usize {
    let mut merged = 0usize;
    for item in items {
        if item.sensitive {
            continue;
        }
        if store.insert(item.clone()).is_ok() {
            merged += 1;
        }
    }
    merged
}

/// Resolves when the cancel signal fires.
async fn cancelled(mut cancel: watch::Receiver<bool>) {
    if *cancel.borrow() {
        return;
    }
    let _ = cancel.changed().await;
}

/// Built-in `delegate_task` tool; the model supplies the prompt and role
/// shape, the manager owns the lifecycle.
pub(crate) struct DelegateTool {
    manager: Arc<SubagentManager>,
}

impl DelegateTool {
    /// Creates the tool backed by the given manager.
    pub(crate) fn new(manager: Arc<SubagentManager>) -> Self {
        Self { manager }
    }

    /// Parses and validates delegation arguments, returning `(role, prompt)`.
    fn role_from_arguments(
        arguments: &serde_json::Value,
    ) -> Result<(SubagentRole, String), String> {
        let map = arguments.as_object().ok_or("delegate arguments must be a JSON object")?;
        let prompt = map
            .get("prompt")
            .and_then(serde_json::Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .ok_or("missing required argument: prompt")?
            .to_string();
        let name = map
            .get("role_name")
            .and_then(serde_json::Value::as_str)
            .filter(|text| !text.is_empty())
            .unwrap_or(DEFAULT_ROLE_NAME)
            .to_string();
        let instructions =
            map.get("instructions").and_then(serde_json::Value::as_str).unwrap_or("").to_string();
        let mut allowed = Vec::new();
        if let Some(classes) = map.get("allowed_tool_classes") {
            let classes = classes.as_array().ok_or("allowed_tool_classes must be an array")?;
            for class in classes {
                let name = class.as_str().ok_or("allowed_tool_classes entries must be strings")?;
                let parsed = match name {
                    "read" => SideEffectClass::Read,
                    "write" => SideEffectClass::Write,
                    "execute" => SideEffectClass::Execute,
                    "delegate" => SideEffectClass::Delegate,
                    _ => return Err(format!("unknown tool class: {name}")),
                };
                allowed.push(parsed);
            }
        }
        if allowed.is_empty() {
            allowed.push(SideEffectClass::Read);
        }
        let max_iterations = map
            .get("max_iterations")
            .and_then(serde_json::Value::as_u64)
            .map(|value| value as usize)
            .filter(|value| *value > 0)
            .unwrap_or(SUBAGENT_DEFAULT_MAX_ITERATIONS);
        let allowed_scope_globs = match map.get("allowed_scope_globs") {
            Some(globs) => {
                let globs = globs.as_array().ok_or("allowed_scope_globs must be an array")?;
                globs
                    .iter()
                    .map(|glob| {
                        glob.as_str()
                            .map(str::to_string)
                            .ok_or("allowed_scope_globs entries must be strings")
                    })
                    .collect::<Result<Vec<_>, _>>()?
            }
            None => Vec::new(),
        };
        // Optional registry model id for the child; unknown ids are rejected
        // by the manager before the child task node exists.
        let model = map
            .get("model")
            .and_then(serde_json::Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string);
        Ok((
            SubagentRole {
                name,
                instructions,
                allowed_tool_classes: allowed,
                max_iterations,
                allowed_scope_globs,
                model,
            },
            prompt,
        ))
    }
}

impl ServerTool for DelegateTool {
    fn definition(&self) -> ToolDefinition {
        // Advertise the registered models so the delegating model can pick;
        // ids only — never provider secrets or credentials.
        let models = self.manager.models.advertised();
        let ids: Vec<String> = models.iter().map(|model| model.id.clone()).collect();
        let described = if ids.is_empty() {
            String::new()
        } else {
            format!(
                " Available models: {} — set `model` to one of these ids, or omit it to use the parent model.",
                ids.iter().map(String::as_str).collect::<Vec<_>>().join(", ")
            )
        };
        ToolDefinition {
            name: "delegate_task".into(),
            description: format!(
                "Delegates a bounded task to a logical subagent that runs in-process with scoped instructions, tools, and memory, and returns a bounded summary.{described}"
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string" },
                    "role_name": { "type": "string" },
                    "instructions": { "type": "string" },
                    "allowed_tool_classes": { "type": "array" },
                    "allowed_scope_globs": { "type": "array" },
                    "max_iterations": { "type": "integer" },
                    "model": {
                        "type": "string",
                        "description": "Registry model id the subagent runs on; omit to use the parent model",
                        "enum": ids,
                    },
                },
                "required": ["prompt"]
            }),
            side_effect_class: SideEffectClass::Delegate,
            side_effect_subclass: None,
            host_approval: false,
            required_capabilities: Vec::new(),
            dependency: ToolDependency::new().produces(vec![ToolDataClass::SubagentSummary]),
        }
    }

    fn execute(
        &self,
        arguments: serde_json::Value,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
        context: ToolCallContext,
    ) -> ToolFuture<ToolResult> {
        let manager = self.manager.clone();
        Box::pin(async move {
            let (role, prompt) = match Self::role_from_arguments(&arguments) {
                Ok(pair) => pair,
                Err(reason) => return ToolResult::failure(ToolErrorKind::InvalidArguments, reason),
            };
            let request = DelegationRequest {
                parent_task_id: context.task.id.clone(),
                role,
                scoped_prompt: prompt,
                context_snapshot: context.transcript,
                scope: context.scope.clone(),
                model_id: context.model_id.clone(),
            };
            match manager.spawn(request, client, cancel, context.events).await {
                Ok(result) => match result.status {
                    SubagentStatus::Completed => ToolResult::success(result.summary),
                    SubagentStatus::Failed => ToolResult::failure(
                        ToolErrorKind::Backend,
                        result.error_summary.unwrap_or_else(|| "subagent failed".into()),
                    ),
                    SubagentStatus::Cancelled => ToolResult::failure(
                        ToolErrorKind::Cancelled,
                        result.error_summary.unwrap_or_else(|| "subagent cancelled".into()),
                    ),
                },
                Err(error) => ToolResult::failure(ToolErrorKind::Backend, error.to_string()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use ee_acp_agent_server::server::OutboundEvent;
    use ee_acp_agent_server::{ClientBridge, PromptContext, UpdateSink};
    use ee_agent_protocol::{ContentBlock, SessionId, SessionUpdate, StopReason, TextContent};
    use serde_json::json;
    use tokio::sync::{mpsc, watch};

    use super::*;
    use crate::config::OrchestratorConfig;
    use crate::model::{
        ModelContent, ModelError, ModelFuture, ModelRequest, ModelResponse, ModelRole,
    };
    use crate::policy::{PolicyEngine, ToolPolicy};
    use crate::runtime::OrchestratorRuntime;
    use crate::tasks::TaskStatus;
    use crate::test_support::{FakeModel, FakeTool};
    use crate::tools::{ToolDefinition, ToolIntent, ToolResult};

    /// A runtime whose policy allows delegation (reads allowed, writes and
    /// executes still denied).
    fn delegating_runtime(
        config: OrchestratorConfig,
        model: Arc<dyn ModelAdapter>,
    ) -> OrchestratorRuntime {
        let policy = PolicyEngine::new(ToolPolicy {
            allow_read: true,
            allow_write: false,
            allow_execute: false,
            allow_delegate: true,
            max_delegate_depth: config.max_subagent_depth,
            max_parallel_delegates: config.max_parallel_subagents,
            ..ToolPolicy::default()
        });
        OrchestratorRuntime::with_policy(config, model, policy)
    }

    fn prompt(text: &str) -> PromptContext {
        PromptContext::new(SessionId::new("s-1"), vec![ContentBlock::Text(TextContent::new(text))])
    }

    fn plumbing() -> (UpdateSink, ClientBridge, mpsc::UnboundedReceiver<OutboundEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            UpdateSink::new_for_test(SessionId::new("s-1"), tx.clone()),
            ClientBridge::new_for_test(Duration::from_secs(5), tx),
            rx,
        )
    }

    async fn next_update(rx: &mut mpsc::UnboundedReceiver<OutboundEvent>) -> SessionUpdate {
        match rx.recv().await.expect("outbound event queued") {
            OutboundEvent::Update { update, .. } => *update,
            other => panic!("expected update event, got {other:?}"),
        }
    }

    fn delegate_intent(arguments: serde_json::Value) -> ToolIntent {
        ToolIntent::new("tc-1", "delegate_task", arguments)
    }

    fn bridge() -> ClientBridge {
        let (tx, _rx) = mpsc::unbounded_channel();
        ClientBridge::new_for_test(Duration::from_secs(5), tx)
    }

    /// Test harness wiring a manager over fresh stores plus the shared
    /// per-turn budget tracker.
    struct ManagerHarness {
        manager: Arc<SubagentManager>,
        tasks: Arc<Mutex<TaskGraph>>,
        _memory: Arc<Mutex<MemoryStore>>,
        budget: Arc<Mutex<BudgetTracker>>,
    }

    fn manager_harness(config: OrchestratorConfig, model: Arc<dyn ModelAdapter>) -> ManagerHarness {
        let tools = Arc::new(Mutex::new(ToolRegistry::new()));
        let tasks = Arc::new(Mutex::new(TaskGraph::new()));
        let memory = Arc::new(Mutex::new(MemoryStore::new(config.memory_limit_bytes)));
        let budget = Arc::new(Mutex::new(BudgetTracker::new(&config)));
        let manager = Arc::new(SubagentManager::new(
            config,
            Arc::new(ModelRegistry::single(model)),
            tools,
            tasks.clone(),
            memory.clone(),
            budget.clone(),
        ));
        ManagerHarness { manager, tasks, _memory: memory, budget }
    }

    // ── Delegate tool integration ────────────────────────────────────────

    #[tokio::test]
    async fn delegate_task_spawns_logical_subagent() {
        let model = FakeModel::new(vec![
            ModelResponse::new().tool_intents(vec![delegate_intent(json!({
                "prompt": "do the thing",
                "role_name": "researcher",
            }))]),
            ModelResponse::new().text("child answer").completed(),
            ModelResponse::new().text("parent done").completed(),
        ]);
        let (sink, client, mut rx) = plumbing();
        let runtime = delegating_runtime(OrchestratorConfig::default(), Arc::new(model.clone()));

        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let result = runtime
            .run_turn(prompt("hello world"), sink, client, cancel_rx)
            .await
            .expect("turn succeeds");
        assert_eq!(result.stop_reason, StopReason::EndTurn);

        // Parent (1) + child (1) + parent (1) model calls.
        assert_eq!(model.call_count(), 3);
        let calls = model.requests();

        // The child saw the parent transcript as context and the delegation
        // prompt as its own user message.
        let child_request = &calls[1];
        assert_eq!(child_request.transcript.len(), 2);
        assert_eq!(child_request.transcript[0].role, ModelRole::User);
        assert_eq!(
            child_request.transcript[0].content[0],
            ModelContent::Text("hello world".into())
        );
        assert_eq!(
            child_request.transcript[1].content[0],
            ModelContent::Text("do the thing".into())
        );

        // The parent saw the bounded child summary through the tool result.
        let parent_tool = calls[2]
            .transcript
            .iter()
            .find(|message| message.role == ModelRole::Tool)
            .expect("tool observation in parent");
        let ModelContent::ToolResult { result, .. } = &parent_tool.content[0] else {
            panic!("expected tool result content");
        };
        assert!(result.success);
        assert!(result.text_output.contains("child answer"));

        // Researcher summaries must cite evidence; "child answer" cites
        // nothing, so the child memory is quarantined, not merged.
        assert!(
            !runtime.memory().items().iter().any(|item| item.key.starts_with("subagent:")),
            "unverified researcher summary is quarantined, not merged"
        );

        // Update stream: plan, delegate tool lifecycle, final message.  The
        // child streams nothing to the client.
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::Plan(_)));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCall(_)));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCallUpdate(_)));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCallUpdate(_)));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::AgentMessageChunk(_)));
        assert!(rx.try_recv().is_err(), "no further outbound events");
    }

    #[tokio::test]
    async fn delegate_task_verified_child_merges_memory() {
        // A researcher child that cites the file and tool it actually used
        // passes verification, so its summary fact merges into parent memory.
        let tool = Arc::new(FakeTool::new(
            ToolDefinition::new("read_file", "reads a file")
                .side_effect_class(SideEffectClass::Read),
            ToolResult::success("content"),
        ));
        let model = FakeModel::new(vec![
            ModelResponse::new().tool_intents(vec![delegate_intent(json!({
                "prompt": "inspect",
                "role_name": "researcher",
            }))]),
            ModelResponse::new().tool_intents(vec![ToolIntent::new(
                "tc-2",
                "read_file",
                json!({ "path": "/work/a.rs" }),
            )]),
            ModelResponse::new().text("found it [file:/work/a.rs] [tool:read_file]").completed(),
            ModelResponse::new().text("parent done").completed(),
        ]);
        let (sink, client, _rx) = plumbing();
        let runtime = delegating_runtime(OrchestratorConfig::default(), Arc::new(model.clone()));
        runtime.register_tool(tool.clone()).expect("registers read_file");

        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let result = runtime
            .run_turn(prompt("hello world"), sink, client, cancel_rx)
            .await
            .expect("turn succeeds");
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        assert_eq!(tool.call_count(), 1, "child read the cited file");
        assert!(
            runtime.memory().items().iter().any(|item| item.key.starts_with("subagent:")),
            "verified child summary merges into parent memory"
        );
    }

    #[tokio::test]
    async fn delegate_task_child_uses_allowed_tools() {
        let tool = Arc::new(FakeTool::new(
            ToolDefinition::new("echo", "echoes").side_effect_class(SideEffectClass::Read),
            ToolResult::success("echoed"),
        ));
        let model = FakeModel::new(vec![
            ModelResponse::new().tool_intents(vec![delegate_intent(json!({
                "prompt": "summarize",
                "allowed_tool_classes": ["read"],
            }))]),
            ModelResponse::new().tool_intents(vec![ToolIntent::new("tc-2", "echo", json!({}))]),
            ModelResponse::new().text("child done").completed(),
            ModelResponse::new().text("parent done").completed(),
        ]);
        let (sink, client, _rx) = plumbing();
        let runtime = delegating_runtime(OrchestratorConfig::default(), Arc::new(model.clone()));
        runtime.register_tool(tool.clone()).expect("registers echo");

        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let result = runtime
            .run_turn(prompt("hello world"), sink, client, cancel_rx)
            .await
            .expect("turn succeeds");
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        assert_eq!(tool.call_count(), 1, "child executed its allowed read tool");
        assert_eq!(model.call_count(), 4, "parent, child, child, parent");
    }

    #[tokio::test]
    async fn delegate_task_child_scope_is_narrowed_to_role_globs() {
        let tool = Arc::new(FakeTool::new(
            ToolDefinition::new("read_file", "reads a file")
                .side_effect_class(SideEffectClass::Read),
            ToolResult::success("content"),
        ));
        let model = FakeModel::new(vec![
            ModelResponse::new().tool_intents(vec![delegate_intent(json!({
                "prompt": "inspect",
                "allowed_tool_classes": ["read"],
                "allowed_scope_globs": ["sub/**"],
            }))]),
            ModelResponse::new().tool_intents(vec![ToolIntent::new(
                "tc-2",
                "read_file",
                json!({ "path": "/work/out.txt" }),
            )]),
            ModelResponse::new().text("child done").completed(),
            ModelResponse::new().text("parent done").completed(),
        ]);
        let (sink, client, _rx) = plumbing();
        // Root scope allows everything under /work; the child role narrows
        // the globs to `sub/**`, so /work/out.txt is outside the child scope.
        let policy = PolicyEngine::new(ToolPolicy {
            allow_read: true,
            allow_delegate: true,
            scope: Some(crate::workspace_scope::WorkspaceScope::new(
                vec![std::path::PathBuf::from("/work")],
                Vec::new(),
            )),
            ..ToolPolicy::default()
        });
        let runtime = OrchestratorRuntime::with_policy(
            OrchestratorConfig::default(),
            Arc::new(model.clone()),
            policy,
        );
        runtime.register_tool(tool.clone()).expect("registers read_file");

        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let result = runtime
            .run_turn(prompt("hello world"), sink, client, cancel_rx)
            .await
            .expect("turn succeeds");
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        assert_eq!(tool.call_count(), 0, "scope-denied read never executes");

        let calls = model.requests();
        // The child's second request carries the denied read observation.
        let child_denied = calls[2]
            .transcript
            .iter()
            .find(|message| message.role == ModelRole::Tool)
            .expect("denied tool observation in child");
        let ModelContent::ToolResult { result, .. } = &child_denied.content[0] else {
            panic!("expected tool result content");
        };
        assert!(!result.success);
        assert_eq!(result.error_kind, Some(crate::tools::ToolErrorKind::PermissionDenied));
        assert!(result.text_output.contains("outside the active workspace scope"));
    }

    #[tokio::test]
    async fn delegate_task_nested_spawns_stop_at_depth_limit() {
        // The role must explicitly allow delegation; the depth limit then
        // denies the grandchild's own delegate before any spawn.
        let delegate =
            || delegate_intent(json!({ "prompt": "deeper", "allowed_tool_classes": ["delegate"] }));
        let model = FakeModel::new(vec![
            ModelResponse::new().tool_intents(vec![delegate()]),
            ModelResponse::new().tool_intents(vec![delegate()]),
            ModelResponse::new().tool_intents(vec![delegate()]),
            ModelResponse::new().text("done").completed(),
            ModelResponse::new().text("done").completed(),
            ModelResponse::new().text("done").completed(),
        ]);
        let (sink, client, _rx) = plumbing();
        let runtime = delegating_runtime(OrchestratorConfig::default(), Arc::new(model.clone()));

        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let result = runtime
            .run_turn(prompt("hello world"), sink, client, cancel_rx)
            .await
            .expect("turn succeeds");
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        // root → child (1) → grandchild (2); the grandchild's own delegate
        // intent is policy-denied before any spawn.
        assert_eq!(model.call_count(), 6, "no fourth-level spawn");
        let calls = model.requests();
        let denied = calls[3]
            .transcript
            .iter()
            .find(|message| message.role == ModelRole::Tool)
            .expect("denied tool observation");
        let ModelContent::ToolResult { result, .. } = &denied.content[0] else {
            panic!("expected tool result content");
        };
        assert!(!result.success);
        assert_eq!(result.error_kind, Some(crate::tools::ToolErrorKind::PermissionDenied));
    }

    #[tokio::test]
    async fn delegate_task_child_failure_returns_error_summary() {
        let model = FakeModel::new(vec![
            ModelResponse::new().tool_intents(vec![delegate_intent(json!({ "prompt": "deep" }))]),
            // The child returns a subagent intent, which the loop rejects as
            // unsupported — a provider error that fails the child.
            ModelResponse::new().subagent_intents(vec![SubagentIntent::new("break")]),
            ModelResponse::new().text("parent done").completed(),
        ]);
        let (sink, client, _rx) = plumbing();
        let runtime = delegating_runtime(OrchestratorConfig::default(), Arc::new(model.clone()));

        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let result = runtime
            .run_turn(prompt("hello world"), sink, client, cancel_rx)
            .await
            .expect("turn succeeds");
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        assert_eq!(model.call_count(), 3);

        let calls = model.requests();
        let parent_tool = calls[2]
            .transcript
            .iter()
            .find(|message| message.role == ModelRole::Tool)
            .expect("tool observation in parent");
        let ModelContent::ToolResult { result, .. } = &parent_tool.content[0] else {
            panic!("expected tool result content");
        };
        assert!(!result.success);
        assert_eq!(result.error_kind, Some(crate::tools::ToolErrorKind::Backend));
        assert!(result.text_output.contains("subagent intents"));
    }

    #[tokio::test]
    async fn delegate_task_bounds_child_summary() {
        let long = "x".repeat(SUBAGENT_SUMMARY_MAX_CHARS + 100);
        let model = FakeModel::new(vec![
            ModelResponse::new().tool_intents(vec![delegate_intent(json!({ "prompt": "deep" }))]),
            ModelResponse::new().text(long.clone()).completed(),
            ModelResponse::new().text("parent done").completed(),
        ]);
        let (sink, client, _rx) = plumbing();
        let runtime = delegating_runtime(OrchestratorConfig::default(), Arc::new(model.clone()));

        let (_cancel_tx, cancel_rx) = watch::channel(false);
        runtime
            .run_turn(prompt("hello world"), sink, client, cancel_rx)
            .await
            .expect("turn succeeds");

        let calls = model.requests();
        let parent_tool = calls[2]
            .transcript
            .iter()
            .find(|message| message.role == ModelRole::Tool)
            .expect("tool observation in parent");
        let ModelContent::ToolResult { result, .. } = &parent_tool.content[0] else {
            panic!("expected tool result content");
        };
        assert!(result.success);
        assert_eq!(
            result.text_output.chars().count(),
            SUBAGENT_SUMMARY_MAX_CHARS + 1,
            "summary truncated to the cap plus ellipsis"
        );
        assert!(result.text_output.ends_with('…'));
        assert!(!result.text_output.contains(&long), "truncated, not the raw text");
    }

    #[tokio::test]
    async fn delegate_task_rejects_missing_prompt_without_spawn() {
        let model = FakeModel::new(vec![
            ModelResponse::new().tool_intents(vec![delegate_intent(json!({}))]),
            ModelResponse::new().text("parent done").completed(),
        ]);
        let (sink, client, _rx) = plumbing();
        let runtime = delegating_runtime(OrchestratorConfig::default(), Arc::new(model.clone()));

        let (_cancel_tx, cancel_rx) = watch::channel(false);
        runtime
            .run_turn(prompt("hello world"), sink, client, cancel_rx)
            .await
            .expect("turn succeeds");

        let calls = model.requests();
        let parent_tool = calls[1]
            .transcript
            .iter()
            .find(|message| message.role == ModelRole::Tool)
            .expect("tool observation in parent");
        let ModelContent::ToolResult { result, .. } = &parent_tool.content[0] else {
            panic!("expected tool result content");
        };
        assert!(!result.success);
        assert_eq!(result.error_kind, Some(crate::tools::ToolErrorKind::InvalidArguments));
        assert!(runtime.memory().is_empty(), "no subagent spawned, no memory merged");
    }

    // ── Manager limits and lifecycle ─────────────────────────────────────

    #[tokio::test]
    async fn failed_child_output_is_quarantined_not_merged() {
        // The child loop rejects subagent intents — a provider error that
        // fails the child.  Failed output must be quarantined by default and
        // never reach parent memory.
        let model = FakeModel::new(vec![
            ModelResponse::new().subagent_intents(vec![SubagentIntent::new("break")]),
        ]);
        let harness = manager_harness(OrchestratorConfig::default(), Arc::new(model.clone()));
        let root = harness.tasks.lock().expect("task graph poisoned").create_root("parent", "p");
        let request = DelegationRequest {
            parent_task_id: root.id.clone(),
            role: SubagentRole::new("researcher", "research"),
            scoped_prompt: "do it".into(),
            context_snapshot: Vec::new(),
            scope: None,
            model_id: None,
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let client = ClientBridge::new_for_test(Duration::from_secs(5), tx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let result = harness
            .manager
            .spawn(request, client, cancel_rx, EventRecorder::new())
            .await
            .expect("spawn succeeds");
        assert_eq!(result.status, SubagentStatus::Failed);
        assert!(
            harness._memory.lock().expect("memory store poisoned").is_empty(),
            "failed child memory never merges"
        );
        assert!(
            harness.manager.quarantine_snapshot().is_quarantined(&result.subagent_id),
            "failed child output is quarantined by default"
        );
    }

    #[tokio::test]
    async fn subagent_depth_limit_is_enforced_before_spawn() {
        let model = FakeModel::new(Vec::new());
        let harness = manager_harness(OrchestratorConfig::default(), Arc::new(model.clone()));
        let (_root, _child, grandchild) = {
            let mut graph = harness.tasks.lock().expect("task graph poisoned");
            let root = graph.create_root("root", "r");
            let child = graph.create_child(&root.id, "c1", "d").expect("child");
            let grandchild = graph.create_child(&child.id, "c2", "d").expect("child");
            (root, child, grandchild)
        };

        let error = harness
            .manager
            .spawn(
                DelegationRequest {
                    parent_task_id: grandchild.id.clone(),
                    role: SubagentRole::new("worker", "instructions"),
                    scoped_prompt: "prompt".into(),
                    context_snapshot: Vec::new(),
                    scope: None,
                    model_id: None,
                },
                bridge(),
                watch::channel(false).1,
                EventRecorder::new(),
            )
            .await
            .expect_err("depth limit");
        assert!(
            matches!(error, OrchestratorError::InvalidState(ref message) if message.contains("depth limit"))
        );
        assert_eq!(model.call_count(), 0, "no child may run");
        assert_eq!(
            harness.tasks.lock().expect("task graph poisoned").len(),
            3,
            "no new node created"
        );
    }

    #[tokio::test]
    async fn subagent_parallel_limit_bounds_concurrency() {
        let probe_state: Arc<Mutex<(usize, usize)>> = Arc::new(Mutex::new((0, 0)));
        let model = Arc::new(ConcurrencyProbe { active: probe_state.clone() });
        let config =
            OrchestratorConfig { max_parallel_subagents: 2, ..OrchestratorConfig::default() };
        let harness = manager_harness(config, model);
        let root = harness.tasks.lock().expect("task graph poisoned").create_root("root", "r");
        let events = EventRecorder::new();
        let manager = harness.manager;

        let mut handles = Vec::new();
        for index in 0..4 {
            let manager = manager.clone();
            let root_id = root.id.clone();
            let role = SubagentRole::new("worker", "instructions");
            let events = events.clone();
            handles.push(tokio::spawn(async move {
                manager
                    .spawn(
                        DelegationRequest {
                            parent_task_id: root_id,
                            role,
                            scoped_prompt: format!("task {index}"),
                            context_snapshot: Vec::new(),
                            scope: None,
                            model_id: None,
                        },
                        bridge(),
                        watch::channel(false).1,
                        events,
                    )
                    .await
                    .expect("spawn succeeds")
            }));
        }
        let results: Vec<SubagentResult> = futures::future::join_all(handles)
            .await
            .into_iter()
            .collect::<Result<_, _>>()
            .expect("spawn tasks complete");
        assert_eq!(results.len(), 4);
        assert!(results.iter().all(|result| result.status == SubagentStatus::Completed));

        let (current, max) = *probe_state.lock().expect("probe state poisoned");
        assert_eq!(current, 0, "all probes finished");
        assert_eq!(max, 2, "at most max_parallel_subagents children ran concurrently");
        assert_eq!(
            harness.tasks.lock().expect("task graph poisoned").len(),
            5,
            "root plus four children"
        );
    }

    #[tokio::test]
    async fn parent_cancellation_cancels_children() {
        let model = FakeModel::new(Vec::new());
        let harness = manager_harness(OrchestratorConfig::default(), Arc::new(model.clone()));
        let manager = harness.manager;
        let tasks = harness.tasks;
        let root = tasks.lock().expect("task graph poisoned").create_root("root", "r");
        let events = EventRecorder::new();

        let (cancel_tx, cancel_rx) = watch::channel(false);
        let handle = tokio::spawn({
            let manager = manager.clone();
            let root_id = root.id.clone();
            let events = events.clone();
            async move {
                manager
                    .spawn(
                        DelegationRequest {
                            parent_task_id: root_id,
                            role: SubagentRole::new("worker", "instructions"),
                            scoped_prompt: "prompt".into(),
                            context_snapshot: Vec::new(),
                            scope: None,
                            model_id: None,
                        },
                        bridge(),
                        cancel_rx,
                        events,
                    )
                    .await
                    .expect("spawn succeeds")
            }
        });
        cancel_tx.send(true).expect("cancel receiver alive");

        let result = handle.await.expect("spawn task completes");
        assert_eq!(result.status, SubagentStatus::Cancelled);
        assert_eq!(model.call_count(), 0, "child never ran");

        let graph = tasks.lock().expect("task graph poisoned");
        let child = graph
            .list()
            .into_iter()
            .find(|node| node.parent == Some(root.id.clone()))
            .expect("child task node exists");
        assert_eq!(child.status, TaskStatus::Cancelled);
        drop(graph);

        let recorded = events.events();
        assert!(
            recorded.iter().any(|event| matches!(event, OrchestratorEvent::SubagentStarted { .. }))
        );
        assert!(recorded.iter().any(|event| {
            matches!(event, OrchestratorEvent::SubagentFinished { success: false, .. })
        }));
    }

    #[tokio::test]
    async fn subagent_budget_denies_spawn_beyond_limit() {
        let model = FakeModel::new(Vec::new());
        let config = OrchestratorConfig { max_subagents: 1, ..OrchestratorConfig::default() };
        let harness = manager_harness(config, Arc::new(model.clone()));
        let manager = harness.manager;
        let tasks = harness.tasks;
        let budget = harness.budget;
        let root = tasks.lock().expect("task graph poisoned").create_root("root", "r");
        let events = EventRecorder::new();

        let first = manager
            .spawn(
                DelegationRequest {
                    parent_task_id: root.id.clone(),
                    role: SubagentRole::new("worker", "instructions"),
                    scoped_prompt: "one".into(),
                    context_snapshot: Vec::new(),
                    scope: None,
                    model_id: None,
                },
                bridge(),
                watch::channel(false).1,
                events.clone(),
            )
            .await
            .expect("first spawn is within budget");
        assert_eq!(first.status, SubagentStatus::Completed);

        let second = manager
            .spawn(
                DelegationRequest {
                    parent_task_id: root.id,
                    role: SubagentRole::new("worker", "instructions"),
                    scoped_prompt: "two".into(),
                    context_snapshot: Vec::new(),
                    scope: None,
                    model_id: None,
                },
                bridge(),
                watch::channel(false).1,
                events.clone(),
            )
            .await;
        assert!(
            matches!(second, Err(OrchestratorError::BudgetExceeded(ref r)) if r.contains("max subagents")),
            "second spawn denied before starting: {second:?}"
        );
        assert_eq!(
            budget.lock().expect("budget tracker poisoned").snapshot().subagents_used,
            1,
            "only the allowed spawn consumed budget"
        );
        assert_eq!(tasks.lock().expect("task graph poisoned").len(), 2, "root plus one child");
        let recorded = events.events();
        assert_eq!(
            recorded
                .iter()
                .filter(|event| matches!(event, OrchestratorEvent::SubagentStarted { .. }))
                .count(),
            1,
            "denied spawn was never started"
        );
        assert!(recorded.iter().any(|event| matches!(
            event,
            OrchestratorEvent::BudgetUpdated { subagents_used: 1, .. }
        )));
    }

    #[tokio::test]
    async fn cancellation_during_subagent_run_cancels_child_task() {
        let calls = Arc::new(Mutex::new(0usize));
        let model = Arc::new(CancelAwaitingModel { calls: calls.clone() });
        let harness = manager_harness(OrchestratorConfig::default(), model);
        let manager = harness.manager;
        let tasks = harness.tasks;
        let root = tasks.lock().expect("task graph poisoned").create_root("root", "r");
        let events = EventRecorder::new();
        let (cancel_tx, cancel_rx) = watch::channel(false);

        let handle = tokio::spawn({
            let manager = manager.clone();
            let root_id = root.id.clone();
            let events = events.clone();
            async move {
                manager
                    .spawn(
                        DelegationRequest {
                            parent_task_id: root_id,
                            role: SubagentRole::new("worker", "instructions"),
                            scoped_prompt: "prompt".into(),
                            context_snapshot: Vec::new(),
                            scope: None,
                            model_id: None,
                        },
                        bridge(),
                        cancel_rx,
                        events,
                    )
                    .await
                    .expect("spawn succeeds")
            }
        });

        // Wait until the child's model call is in flight, then cancel the
        // parent turn; the child must observe the token and stop.
        wait_until(|| *calls.lock().expect("calls poisoned") == 1).await;
        cancel_tx.send(true).expect("cancel receiver alive");

        let result = handle.await.expect("spawn task completes");
        assert_eq!(result.status, SubagentStatus::Cancelled);
        assert_eq!(*calls.lock().expect("calls poisoned"), 1, "child never called again");

        let graph = tasks.lock().expect("task graph poisoned");
        let child = graph
            .list()
            .into_iter()
            .find(|node| node.parent.as_ref() == Some(&root.id))
            .expect("child task node exists");
        assert_eq!(child.status, TaskStatus::Cancelled, "child task cleaned up");
        drop(graph);
    }

    /// Bounded wait that pumps the runtime until `condition` holds.
    async fn wait_until(condition: impl Fn() -> bool) {
        for _ in 0..10_000 {
            if condition() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("condition never satisfied");
    }

    /// Model that blocks until the cancellation watch flips, then reports
    /// cancellation; proves parent cancellation reaches a running child.
    struct CancelAwaitingModel {
        calls: Arc<Mutex<usize>>,
    }

    impl ModelAdapter for CancelAwaitingModel {
        fn complete(
            &self,
            _request: ModelRequest,
            mut cancel: watch::Receiver<bool>,
        ) -> ModelFuture<Result<ModelResponse, ModelError>> {
            let calls = self.calls.clone();
            Box::pin(async move {
                *calls.lock().expect("calls poisoned") += 1;
                if *cancel.borrow() {
                    return Err(ModelError::Cancelled);
                }
                let _ = cancel.changed().await;
                Err(ModelError::Cancelled)
            })
        }
    }

    #[test]
    fn merge_memory_items_skips_sensitive_and_rejected() {
        let mut store = MemoryStore::new(1024);
        let normal = MemoryItem::new("fact", "value");
        let sensitive = MemoryItem::new("token", "secret").as_sensitive();
        let merged = merge_memory_items(&mut store, &[normal.clone(), sensitive]);
        assert_eq!(merged, 1);
        assert_eq!(store.query("fact"), Some(normal));
        assert_eq!(store.query("token"), None, "sensitive item never merged");
    }

    #[test]
    fn subagent_role_defaults_are_read_only() {
        let role = SubagentRole::new("worker", "do good work");
        assert_eq!(role.name, "worker");
        assert_eq!(role.allowed_tool_classes, vec![SideEffectClass::Read]);
        assert_eq!(role.max_iterations, SUBAGENT_DEFAULT_MAX_ITERATIONS);
        let role =
            role.with_allowed_tool_classes(vec![SideEffectClass::Execute]).with_max_iterations(4);
        assert_eq!(role.allowed_tool_classes, vec![SideEffectClass::Execute]);
        assert_eq!(role.max_iterations, 4);
    }

    #[test]
    fn subagent_types_roundtrip_through_json() {
        let result = SubagentResult {
            subagent_id: SubagentId::new("task-3"),
            status: SubagentStatus::Completed,
            summary: "done".into(),
            produced_memory_items: vec![MemoryItem::new("fact", "value")],
            tool_call_count: 2,
            error_summary: None,
            citations: SubagentCitations {
                files: vec!["/work/a.rs".into()],
                tools: vec!["read_file".into()],
            },
        };
        let json = serde_json::to_string(&result).expect("serializes");
        let restored: SubagentResult = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, result);
        assert_eq!(restored.citations, result.citations);

        let intent = SubagentIntent::new("summarize the findings");
        let json = serde_json::to_string(&intent).expect("serializes");
        let restored: SubagentIntent = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, intent);
    }

    /// Model probe counting concurrently active completions.
    struct ConcurrencyProbe {
        active: Arc<Mutex<(usize, usize)>>,
    }

    impl ModelAdapter for ConcurrencyProbe {
        fn complete(
            &self,
            _request: ModelRequest,
            _cancel: watch::Receiver<bool>,
        ) -> ModelFuture<Result<ModelResponse, ModelError>> {
            let active = self.active.clone();
            Box::pin(async move {
                {
                    let mut state = active.lock().expect("probe poisoned");
                    state.0 += 1;
                    state.1 = state.1.max(state.0);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
                active.lock().expect("probe poisoned").0 -= 1;
                Ok(ModelResponse::new().text("done").completed())
            })
        }
    }

    // ── Subagent model selection (phase 11) ───────────────────────────────

    /// A delegating runtime sharing a registry of two fake adapters:
    /// `default` (parent) and `strong` (selectable by delegation).
    fn model_registry_runtime(
        config: OrchestratorConfig,
        parent: Arc<dyn ModelAdapter>,
        strong: Arc<dyn ModelAdapter>,
    ) -> OrchestratorRuntime {
        let mut registry = ModelRegistry::single(parent);
        registry
            .register_with_hints(
                "strong",
                strong,
                Some("Strong Model".to_string()),
                vec!["tools".to_string()],
            )
            .expect("registers strong model");
        let policy = PolicyEngine::new(ToolPolicy {
            allow_read: true,
            allow_write: false,
            allow_execute: false,
            allow_delegate: true,
            max_delegate_depth: config.max_subagent_depth,
            max_parallel_delegates: config.max_parallel_subagents,
            ..ToolPolicy::default()
        });
        OrchestratorRuntime::with_model_registry(config, registry, policy).expect("runtime")
    }

    #[tokio::test]
    async fn delegate_model_explicit_selection_runs_child_on_selected_adapter() {
        let parent = FakeModel::new(vec![
            ModelResponse::new().tool_intents(vec![delegate_intent(json!({
                "prompt": "do the thing",
                "role_name": "researcher",
                "model": "strong",
            }))]),
            ModelResponse::new().text("parent done").completed(),
        ]);
        let strong = FakeModel::new(vec![ModelResponse::new().text("child answer").completed()]);
        let (sink, client, _rx) = plumbing();
        let runtime = model_registry_runtime(
            OrchestratorConfig::default(),
            Arc::new(parent.clone()),
            Arc::new(strong.clone()),
        );
        let events = EventRecorder::new();
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        runtime
            .run_turn_recording(prompt("delegate"), sink, client, cancel_rx, events.clone())
            .await
            .expect("turn succeeds");

        // The parent ran twice on the default adapter; the child ran once on
        // the selected strong adapter.
        assert_eq!(parent.requests().len(), 2, "parent adapter serves the parent loop");
        assert_eq!(strong.requests().len(), 1, "strong adapter serves the child loop");
        let parent_request = &parent.requests()[0];
        assert_eq!(parent_request.model_id.as_deref(), Some("default"));
        let child_request = &strong.requests()[0];
        assert_eq!(child_request.model_id.as_deref(), Some("strong"));
        assert_eq!(
            child_request.task.id,
            TaskId::new("task-2"),
            "child request carries the child task node"
        );

        // Both requests advertise the registry list without secrets.
        for request in [parent_request, child_request] {
            let ids: Vec<&str> =
                request.available_models.iter().map(|info| info.id.as_str()).collect();
            assert_eq!(ids, vec!["default", "strong"]);
            let strong = request
                .available_models
                .iter()
                .find(|info| info.id == "strong")
                .expect("strong advertised");
            assert_eq!(strong.display_name.as_deref(), Some("Strong Model"));
            assert_eq!(strong.capabilities, vec!["tools"]);
            assert!(
                request.available_models.iter().all(|info| info.id != "secret-provider-token"),
                "advertised list never leaks secrets"
            );
        }

        // The delegation event records the selected model.
        let started = events
            .events()
            .iter()
            .find_map(|event| match event {
                OrchestratorEvent::SubagentStarted { subagent_id, model_id } => {
                    Some((subagent_id.clone(), model_id.clone()))
                }
                _ => None,
            })
            .expect("subagent started");
        assert_eq!(started, ("task-2".to_string(), Some("strong".to_string())));

        // The child task node records the selected model.
        let tasks = runtime.tasks();
        let child = tasks.get(&TaskId::new("task-2")).expect("child task exists");
        assert_eq!(child.model_id.as_deref(), Some("strong"));
        assert_eq!(tasks.get(&TaskId::new("task-1")).expect("root").model_id, None);
    }

    #[tokio::test]
    async fn delegate_model_unset_selection_falls_back_to_parent_adapter() {
        let parent = FakeModel::new(vec![
            ModelResponse::new().tool_intents(vec![delegate_intent(json!({
                "prompt": "do the thing",
            }))]),
            ModelResponse::new().text("parent answer").completed(),
        ]);
        let strong = FakeModel::new(Vec::new());
        let (sink, client, _rx) = plumbing();
        let runtime = model_registry_runtime(
            OrchestratorConfig::default(),
            Arc::new(parent.clone()),
            Arc::new(strong.clone()),
        );
        let events = EventRecorder::new();
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        runtime
            .run_turn_recording(prompt("delegate"), sink, client, cancel_rx, events.clone())
            .await
            .expect("turn succeeds");

        // The child consumed the next parent script response on the parent
        // adapter; the strong adapter never ran.
        assert!(strong.requests().is_empty(), "unselected adapter never runs");
        let parent_requests = parent.requests();
        let child_call = parent_requests
            .iter()
            .find(|request| request.task.id == TaskId::new("task-2"))
            .expect("child request went to the parent adapter");
        assert_eq!(child_call.model_id.as_deref(), Some("default"), "fallback resolved");

        // The delegation event and child node record the fallback selection.
        let started = events
            .events()
            .iter()
            .find_map(|event| match event {
                OrchestratorEvent::SubagentStarted { subagent_id, model_id } => {
                    Some((subagent_id.clone(), model_id.clone()))
                }
                _ => None,
            })
            .expect("subagent started");
        assert_eq!(started, ("task-2".to_string(), Some("default".to_string())));
        let tasks = runtime.tasks();
        assert_eq!(
            tasks.get(&TaskId::new("task-2")).expect("child task").model_id.as_deref(),
            Some("default")
        );
    }

    #[tokio::test]
    async fn delegate_model_unknown_selection_never_creates_child_task() {
        let parent = FakeModel::new(vec![
            ModelResponse::new().tool_intents(vec![delegate_intent(json!({
                "prompt": "do the thing",
                "model": "nope",
            }))]),
            ModelResponse::new().text("parent done").completed(),
        ]);
        let strong = FakeModel::new(Vec::new());
        let (sink, client, _rx) = plumbing();
        let runtime = model_registry_runtime(
            OrchestratorConfig::default(),
            Arc::new(parent.clone()),
            Arc::new(strong.clone()),
        );
        let events = EventRecorder::new();
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        runtime
            .run_turn_recording(prompt("delegate"), sink, client, cancel_rx, events.clone())
            .await
            .expect("turn succeeds with the failure fed back");

        // No child task node was created and no subagent started.
        let tasks = runtime.tasks();
        assert_eq!(tasks.len(), 1, "only the root task exists");
        assert_eq!(tasks.get(&TaskId::new("task-1")).expect("root").model_id, None);
        assert!(
            !events
                .events()
                .iter()
                .any(|event| matches!(event, OrchestratorEvent::SubagentStarted { .. })),
            "unknown model never starts a subagent"
        );
        assert!(strong.requests().is_empty());

        // The delegate tool failed with the deterministic rejection, and the
        // parent loop recovered.
        assert!(events.events().iter().any(|event| matches!(
            event,
            OrchestratorEvent::ToolFinished {
                tool_name,
                success: false,
                ..
            } if tool_name == "delegate_task"
        )));
        assert_eq!(parent.requests().len(), 2, "parent recovered after the failure");
    }

    #[tokio::test]
    async fn delegate_model_nested_child_falls_back_to_parent_adapter() {
        // Root delegates to a child on `strong`; that child delegates again
        // without a selection, so the grandchild must fall back to `strong`
        // (its parent adapter), not the registry default.
        let parent = FakeModel::new(vec![
            ModelResponse::new().tool_intents(vec![delegate_intent(json!({
                "prompt": "level one",
                "model": "strong",
                "allowed_tool_classes": ["delegate"],
            }))]),
            ModelResponse::new().text("root done").completed(),
        ]);
        let strong = FakeModel::new(vec![
            ModelResponse::new().tool_intents(vec![delegate_intent(json!({
                "prompt": "level two",
                "allowed_tool_classes": ["delegate"],
            }))]),
            ModelResponse::new().text("child done").completed(),
        ]);
        let (sink, client, _rx) = plumbing();
        let runtime = model_registry_runtime(
            OrchestratorConfig { max_subagent_depth: 2, ..OrchestratorConfig::default() },
            Arc::new(parent.clone()),
            Arc::new(strong.clone()),
        );
        let events = EventRecorder::new();
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        runtime
            .run_turn_recording(prompt("delegate"), sink, client, cancel_rx, events.clone())
            .await
            .expect("turn succeeds");

        // Both children ran on `strong`.
        let started: Vec<(String, Option<String>)> = events
            .events()
            .into_iter()
            .filter_map(|event| match event {
                OrchestratorEvent::SubagentStarted { subagent_id, model_id } => {
                    Some((subagent_id.clone(), model_id.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(started.len(), 2);
        assert_eq!(started[0], ("task-2".to_string(), Some("strong".to_string())));
        assert_eq!(started[1], ("task-3".to_string(), Some("strong".to_string())));

        // The grandchild request carried `strong` as its diagnostic model id
        // and the strong adapter served the child and grandchild loops.
        let strong_requests = strong.requests();
        assert_eq!(strong_requests.len(), 4, "child loop (3 calls) + grandchild loop (1 call)");
        assert!(
            strong_requests.iter().all(|request| request.model_id.as_deref() == Some("strong"))
        );
        let tasks = runtime.tasks();
        assert_eq!(
            tasks.get(&TaskId::new("task-3")).expect("grandchild").model_id.as_deref(),
            Some("strong")
        );
    }

    #[tokio::test]
    async fn delegate_model_schema_advertises_registry_ids() {
        let parent = FakeModel::new(vec![ModelResponse::new().text("done").completed()]);
        let strong = FakeModel::new(Vec::new());
        let (sink, client, _rx) = plumbing();
        let runtime = model_registry_runtime(
            OrchestratorConfig::default(),
            Arc::new(parent.clone()),
            Arc::new(strong.clone()),
        );
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        runtime.run_turn(prompt("delegate"), sink, client, cancel_rx).await.expect("turn");

        let parent_requests = parent.requests();
        let delegate = parent_requests[0]
            .tools
            .iter()
            .find(|tool| tool.name == "delegate_task")
            .expect("delegate_task advertised");
        assert!(
            delegate.description.contains("Available models: default, strong"),
            "description advertises ids: {}",
            delegate.description
        );
        let model_schema = &delegate.input_schema["properties"]["model"];
        assert_eq!(model_schema["type"], json!("string"));
        assert_eq!(model_schema["enum"], json!(["default", "strong"]));
    }

    #[test]
    fn delegate_model_role_builder_and_roundtrip() {
        let role = SubagentRole::new("researcher", "instructions").with_model("strong");
        assert_eq!(role.model.as_deref(), Some("strong"));
        let json = serde_json::to_string(&role).expect("serializes");
        let restored: SubagentRole = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, role);
        assert_eq!(SubagentRole::new("worker", "x").model, None);
    }

    #[test]
    fn delegate_model_registry_requires_default_adapter() {
        let mut registry = ModelRegistry::new();
        registry.register("strong", Arc::new(FakeModel::new(Vec::new()))).expect("registers");
        let error = match OrchestratorRuntime::with_model_registry(
            OrchestratorConfig::default(),
            registry,
            PolicyEngine::default(),
        ) {
            Ok(_) => panic!("registry without a default adapter must fail closed"),
            Err(error) => error,
        };
        assert!(
            matches!(error, OrchestratorError::InvalidState(reason) if reason.contains("no default adapter"))
        );
    }
}
