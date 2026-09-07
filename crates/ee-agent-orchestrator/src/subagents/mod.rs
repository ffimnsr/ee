//! Subagent delegation: logical in-process workers.
//!
//! Subagents are not OS processes; they are reduced `LoopEngine` runs over
//! the shared tool registry, with a scoped role (name, instructions, allowed
//! tool classes, iteration cap, optional model selection), a child task node
//! in the task graph, and a bounded structured handoff returned to the parent. The
//! `SubagentManager` enforces the configured depth and parallelism limits,
//! propagates cancellation from parent to children, and merges child memory
//! items (never sensitive ones) into the parent store — after the child
//! handoff's citations were verified against its execution evidence, and
//! only when the child completed.  Failed, cancelled, and unverified child
//! output is quarantined instead of merged.  The built-in `delegate_task`
//! tool exposes delegation to the model; the built-in role library lives in
//! [`crate::subagent_roles`].
//!
//! Model selection: the manager resolves the child adapter through the
//! shared [`ModelRegistry`] before the child task node exists — a role's
//! explicit `model` id wins, followed by configured role routing, the parent
//! loop's adapter id, then the registry default. Unknown ids are rejected with
//! a deterministic error and never create a node. The selected id is recorded
//! on the child task, in the `SubagentStarted` event, and in the child's
//! `ModelRequest` diagnostic metadata; the advertised model list is exposed
//! to the delegating model through `ModelRequest` and the `delegate_task`
//! schema.

pub(crate) use std::fmt;
pub(crate) use std::path::PathBuf;
pub(crate) use std::sync::{Arc, Mutex, RwLock};

pub(crate) use ee_acp_agent_server::{ClientBridge, UpdateSink};
pub(crate) use ee_agent_protocol::SessionId;
pub(crate) use serde::{Deserialize, Serialize};
pub(crate) use tokio::sync::{Semaphore, mpsc, watch};
pub(crate) use tokio::time::Instant;

pub(crate) use crate::budget::BudgetTracker;
pub(crate) use crate::child_registry::{
    ChildProgress, ChildRegistry, ChildState, MAX_CHILD_ROLE_CHARS,
};
pub(crate) use crate::config::OrchestratorConfig;
pub(crate) use crate::critique::CritiqueReportVerifier;
pub(crate) use crate::decision_log::DecisionLog;
pub(crate) use crate::delegation_quality::ReportEvidence;
pub(crate) use crate::error::OrchestratorError;
pub(crate) use crate::events::{EventRecorder, OrchestratorEvent};
pub(crate) use crate::loop_engine::{LoopEngine, LoopOptions};
pub(crate) use crate::memory::{MemoryItem, MemoryStore};
pub(crate) use crate::metrics::OrchestratorMetrics;
pub(crate) use crate::model::{ModelAdapter, ModelMessage, ModelRole, Transcript};
pub(crate) use crate::model_registry::{DEFAULT_MODEL_ID, ModelRegistry};
pub(crate) use crate::model_router::{ModelRouter, TaskKind};
pub(crate) use crate::policy::{PolicyEngine, ToolPolicy};
pub use crate::subagent_handoff::SubagentStatus;
pub(crate) use crate::subagent_handoff::{
    GENERIC_HANDOFF_INSTRUCTIONS, HandoffOutputFormat, SubagentHandoff,
};
pub(crate) use crate::subagent_roles::{
    BuiltinSubagentRole, RUBBER_DUCK_MAX_CONTEXT_BYTES, RUBBER_DUCK_MAX_ITERATIONS,
    RUBBER_DUCK_MAX_MODEL_CALLS, RUBBER_DUCK_MAX_OUTPUT_BYTES, RUBBER_DUCK_MAX_RECURSION_DEPTH,
    RUBBER_DUCK_MAX_TOOL_CALLS, RUBBER_DUCK_TIMEOUT, RUBBER_DUCK_TOOL_TIMEOUT,
    rubber_duck_allows_tool,
};
pub(crate) use crate::subagent_verifier::{
    SubagentCitations, SubagentEvidence, SubagentQuarantine, SubagentResultVerifier,
};
pub(crate) use crate::tasks::{TaskGraph, TaskId, TaskStatus, truncate};
pub(crate) use crate::tool_dependencies::{ToolDataClass, ToolDependency};
pub(crate) use crate::tools::{
    ServerTool, SideEffectClass, ToolCallContext, ToolDefinition, ToolErrorKind, ToolFuture,
    ToolRegistry, ToolResult,
};
pub(crate) use crate::trust::TrustLevel;
pub(crate) use crate::workspace_scope::WorkspaceScope;

/// Default max loop iterations for a subagent role.
pub const SUBAGENT_DEFAULT_MAX_ITERATIONS: usize = 8;
/// Legacy cap used for error summaries and quarantine inspection.
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
    /// Whether successful output must cite backend-observed files or tools.
    /// Custom roles default fail-closed through [`SubagentRole::new`].
    pub requires_evidence: bool,
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
            requires_evidence: true,
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

    /// Sets evidence policy. Intended for trusted backend role definitions;
    /// model-supplied custom-role arguments cannot disable this policy.
    #[must_use]
    pub fn with_requires_evidence(mut self, requires_evidence: bool) -> Self {
        self.requires_evidence = requires_evidence;
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

/// Bounded outcome of one subagent run, returned to the parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SubagentResult {
    /// Stable subagent id.
    pub subagent_id: SubagentId,
    /// Single authoritative parent handoff, including status, summary, claims,
    /// and backend-observed evidence.
    pub handoff: SubagentHandoff,
    /// Memory items the child produced (never sensitive).
    pub produced_memory_items: Vec<MemoryItem>,
    /// Tool calls the child executed.
    pub tool_call_count: usize,
    /// Bounded error summary, when the child failed or was cancelled.
    pub error_summary: Option<String>,
}

impl SubagentResult {
    /// Validates backend identity and handoff integrity at persistence and
    /// injected fan-in boundaries.
    pub(crate) fn validate_against(
        &self,
        expected_id: &str,
        expected_role: Option<&str>,
        expected_status: Option<SubagentStatus>,
    ) -> Result<(), OrchestratorError> {
        if self.subagent_id.as_str() != expected_id {
            return Err(OrchestratorError::InvalidState(format!(
                "subagent result id {} does not match expected child {expected_id}",
                self.subagent_id
            )));
        }
        if self.handoff.subagent_id != self.subagent_id.as_str() {
            return Err(OrchestratorError::InvalidState(format!(
                "subagent handoff id {} does not match result id {}",
                self.handoff.subagent_id, self.subagent_id
            )));
        }
        if let Some(role) = expected_role
            && self.handoff.role != role
        {
            return Err(OrchestratorError::InvalidState(format!(
                "subagent handoff role {} does not match expected role {role}",
                self.handoff.role
            )));
        }
        if let Some(status) = expected_status
            && self.handoff.status != status
        {
            return Err(OrchestratorError::InvalidState(format!(
                "subagent handoff status {:?} does not match task status {status:?}",
                self.handoff.status
            )));
        }
        self.handoff.validate_integrity().map_err(OrchestratorError::InvalidState)
    }
}

/// A resolved child adapter: the adapter plus its registry id.
pub(crate) struct ResolvedModel {
    adapter: Arc<dyn ModelAdapter>,
    id: Option<String>,
}

/// Shared routing and telemetry stores used by subagent execution.
pub(crate) struct SubagentObservability {
    pub(crate) router: Arc<RwLock<Option<ModelRouter>>>,
    pub(crate) metrics: Arc<Mutex<OrchestratorMetrics>>,
    pub(crate) decisions: Arc<Mutex<DecisionLog>>,
}

pub(crate) struct SubagentState {
    pub(crate) tasks: Arc<Mutex<TaskGraph>>,
    pub(crate) memory: Arc<Mutex<MemoryStore>>,
    pub(crate) budget: Arc<Mutex<BudgetTracker>>,
    pub(crate) children: Arc<ChildRegistry>,
}

impl SubagentState {
    pub(crate) fn new(
        tasks: Arc<Mutex<TaskGraph>>,
        memory: Arc<Mutex<MemoryStore>>,
        budget: Arc<Mutex<BudgetTracker>>,
        children: Arc<ChildRegistry>,
    ) -> Self {
        Self { tasks, memory, budget, children }
    }
}

impl SubagentObservability {
    pub(crate) fn new(
        router: Arc<RwLock<Option<ModelRouter>>>,
        metrics: Arc<Mutex<OrchestratorMetrics>>,
        decisions: Arc<Mutex<DecisionLog>>,
    ) -> Self {
        Self { router, metrics, decisions }
    }
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
    observability: SubagentObservability,
    semaphore: Arc<Semaphore>,
    quarantine: Arc<Mutex<SubagentQuarantine>>,
    children: Arc<ChildRegistry>,
}

/// Synchronous fail-safe for a dropped or panicking child future. Normal
/// completion disarms it after registry and task state reach a terminal state.
struct ChildRunGuard {
    children: Arc<ChildRegistry>,
    tasks: Arc<Mutex<TaskGraph>>,
    events: EventRecorder,
    subagent_id: SubagentId,
    task_id: TaskId,
    armed: bool,
}

impl ChildRunGuard {
    fn new(
        children: Arc<ChildRegistry>,
        tasks: Arc<Mutex<TaskGraph>>,
        events: EventRecorder,
        subagent_id: SubagentId,
        task_id: TaskId,
    ) -> Self {
        Self { children, tasks, events, subagent_id, task_id, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ChildRunGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let _ = self.children.cancel(&self.subagent_id);
        let mut tasks = self.tasks.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let active = tasks
            .get(&self.task_id)
            .is_some_and(|task| matches!(task.status, TaskStatus::Pending | TaskStatus::Running));
        if active {
            let _ = tasks.transition(&self.task_id, TaskStatus::Cancelled);
        }
        drop(tasks);
        if self.children.finish(&self.subagent_id, ChildState::Cancelled) && active {
            self.events.record(OrchestratorEvent::SubagentFinished {
                subagent_id: self.subagent_id.as_str().to_string(),
                success: false,
            });
        }
    }
}
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
        let name = match map.get("role_name") {
            None => DEFAULT_ROLE_NAME.to_string(),
            Some(serde_json::Value::String(name)) => {
                let name = name.trim();
                let mut chars = name.chars();
                let valid = name.chars().count() <= MAX_CHILD_ROLE_CHARS
                    && chars.next().is_some_and(|character| character.is_ascii_alphanumeric())
                    && chars.all(|character| {
                        character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
                    });
                if !valid {
                    return Err(format!(
                        "role_name must be a 1-{MAX_CHILD_ROLE_CHARS} character ASCII identifier using letters, digits, '_' or '-', starting with a letter or digit"
                    ));
                }
                name.to_string()
            }
            Some(_) => return Err("role_name must be a string".into()),
        };
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

        // Built-in role security contracts are immutable at the tool boundary.
        // Model-supplied instructions, tool classes, and iteration limits are
        // ignored; only scope narrowing and model selection may vary.
        if let Some(builtin) = BuiltinSubagentRole::by_name(&name) {
            let mut role = builtin.role();
            role.allowed_scope_globs = allowed_scope_globs;
            role.model = model;
            return Ok((role, prompt));
        }

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
        Ok((
            SubagentRole {
                name,
                instructions,
                allowed_tool_classes: allowed,
                max_iterations,
                allowed_scope_globs,
                model,
                requires_evidence: true,
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
                "Delegates a bounded task to a logical subagent that runs in-process with scoped instructions, tools, and memory, and returns bounded structured handoff JSON.{described}"
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string" },
                    "role_name": {
                        "type": "string",
                        "maxLength": MAX_CHILD_ROLE_CHARS,
                        "pattern": "^[A-Za-z0-9][A-Za-z0-9_-]*$",
                        "description": format!(
                            "Built-in role ({}) or custom role identifier",
                            BuiltinSubagentRole::ALL
                                .iter()
                                .map(|role| role.name())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    },
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
                Ok(result) => match result.handoff.status {
                    SubagentStatus::Completed
                        if BuiltinSubagentRole::by_name(&result.handoff.role)
                            == Some(BuiltinSubagentRole::RubberDuck) =>
                    {
                        ToolResult::success(result.handoff.summary)
                    }
                    SubagentStatus::Completed => match result.handoff.to_json() {
                        Ok(handoff) => ToolResult::success(handoff),
                        Err(error) => ToolResult::failure(
                            ToolErrorKind::Backend,
                            format!("subagent handoff serialization failed: {error}"),
                        ),
                    },
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

mod manager;

#[cfg(test)]
mod tests;
