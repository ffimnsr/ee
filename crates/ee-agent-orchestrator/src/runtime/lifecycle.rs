//! `impl OrchestratorRuntime` methods: lifecycle.
use super::*;

impl OrchestratorRuntime {
    /// Creates a runtime with the given config and injected model adapter
    /// and empty stores.  Uses a conservative fail-closed policy: reads are
    /// allowed, writes, executes, and delegation are denied.
    #[must_use]
    pub fn new(config: OrchestratorConfig, model: Arc<dyn ModelAdapter>) -> Self {
        Self::with_policy(config, model, PolicyEngine::default())
    }
    /// Creates a runtime with the given config, injected model adapter, and
    /// policy.  Registers the built-in `delegate_task` tool backed by an
    /// internal subagent manager; delegation requires `allow_delegate` in the
    /// policy, and depth/parallelism limits still apply.
    #[must_use]
    pub fn with_policy(
        config: OrchestratorConfig,
        model: Arc<dyn ModelAdapter>,
        policy: PolicyEngine,
    ) -> Self {
        let memory = MemoryStore::new(config.memory_limit_bytes);
        Self::from_stores(
            config.clone(),
            Arc::new(ModelRegistry::single(model)),
            policy,
            Arc::new(Mutex::new(TaskGraph::new())),
            Arc::new(Mutex::new(memory)),
            BudgetTracker::new(&config),
        )
    }
    /// Creates a runtime with a shared model registry, so delegation can
    /// select which adapter a subagent runs on.  Requires a registered
    /// [`DEFAULT_MODEL_ID`] entry (the parent/default adapter); fails closed
    /// otherwise.
    pub fn with_model_registry(
        config: OrchestratorConfig,
        models: ModelRegistry,
        policy: PolicyEngine,
    ) -> Result<Self, OrchestratorError> {
        Self::with_shared_model_registry(config, Arc::new(models), policy)
    }
    /// Creates a runtime from a process-owned shared registry. Adapters and
    /// provider clients stay in memory and are never checkpointed.
    pub fn with_shared_model_registry(
        config: OrchestratorConfig,
        models: Arc<ModelRegistry>,
        policy: PolicyEngine,
    ) -> Result<Self, OrchestratorError> {
        models.default_adapter()?;
        let memory = MemoryStore::new(config.memory_limit_bytes);
        Ok(Self::from_stores(
            config.clone(),
            models,
            policy,
            Arc::new(Mutex::new(TaskGraph::new())),
            Arc::new(Mutex::new(memory)),
            BudgetTracker::new(&config),
        ))
    }
    /// Creates a runtime from previously persisted task and memory state
    /// (used by `session/load`-style restore flows).  Fresh stores are created
    /// for everything else, so restored state is isolated per session.
    #[must_use]
    pub fn with_state(
        config: OrchestratorConfig,
        model: Arc<dyn ModelAdapter>,
        policy: PolicyEngine,
        tasks: TaskGraph,
        memory: MemoryStore,
    ) -> Self {
        Self::from_stores(
            config.clone(),
            Arc::new(ModelRegistry::single(model)),
            policy,
            Arc::new(Mutex::new(tasks)),
            Arc::new(Mutex::new(memory)),
            BudgetTracker::new(&config),
        )
    }
    /// Restores a runtime from a validated checkpoint (see
    /// [`OrchestratorCheckpoint::validate`](crate::checkpoint::OrchestratorCheckpoint::validate)).
    /// The task graph, memory store, and budget counters are rebuilt from the
    /// checkpoint; the wall-clock deadline restarts from the checkpoint's
    /// config.
    pub fn from_checkpoint(
        checkpoint: &crate::checkpoint::OrchestratorCheckpoint,
        model: Arc<dyn ModelAdapter>,
        policy: PolicyEngine,
    ) -> Result<Self, OrchestratorError> {
        Self::from_checkpoint_with_model_registry(
            checkpoint,
            Arc::new(ModelRegistry::single(model)),
            policy,
        )
    }
    /// Restores state against process-owned adapters. Registry metadata,
    /// credentials, HTTP clients, and adapters never come from the checkpoint.
    pub fn from_checkpoint_with_model_registry(
        checkpoint: &crate::checkpoint::OrchestratorCheckpoint,
        models: Arc<ModelRegistry>,
        policy: PolicyEngine,
    ) -> Result<Self, OrchestratorError> {
        checkpoint.validate()?;
        models.default_adapter()?;
        Self::from_validated_checkpoint(checkpoint, models, policy)
    }
    /// Shared restore path; `checkpoint` and registry must already be validated.
    pub(crate) fn from_validated_checkpoint(
        checkpoint: &crate::checkpoint::OrchestratorCheckpoint,
        models: Arc<ModelRegistry>,
        policy: PolicyEngine,
    ) -> Result<Self, OrchestratorError> {
        let config = checkpoint.config.clone();
        let mut budget = BudgetTracker::new(&config);
        budget.restore_used(&checkpoint.budget)?;
        Ok(Self::from_stores(
            config,
            models,
            policy,
            Arc::new(Mutex::new(checkpoint.tasks.clone())),
            Arc::new(Mutex::new(checkpoint.memory.clone())),
            budget,
        ))
    }
    /// Shared constructor: wires the stores, budget, and delegate tool.
    pub(super) fn from_stores(
        config: OrchestratorConfig,
        models: Arc<ModelRegistry>,
        policy: PolicyEngine,
        tasks: Arc<Mutex<TaskGraph>>,
        memory: Arc<Mutex<MemoryStore>>,
        budget: BudgetTracker,
    ) -> Self {
        let budget = Arc::new(Mutex::new(budget));
        let tools = Arc::new(Mutex::new(ToolRegistry::new()));
        let checkpoints = Arc::new(CheckpointStore::new(&config.recovery));
        let events = EventRecorder::new();
        let model_router = Arc::new(RwLock::new(None));
        let metrics = Arc::new(Mutex::new(OrchestratorMetrics::new()));
        let decisions = Arc::new(Mutex::new(DecisionLog::default()));
        let children = Arc::new(ChildRegistry::default());
        let manager = Arc::new(SubagentManager::new(
            config.clone(),
            models.clone(),
            tools.clone(),
            SubagentState::new(tasks.clone(), memory.clone(), budget.clone(), children.clone()),
            SubagentObservability::new(model_router.clone(), metrics.clone(), decisions.clone()),
        ));
        tools
            .lock()
            .expect("tool registry poisoned")
            .register(Arc::new(DelegateTool::new(manager)))
            .expect("registers delegate_task");
        let rubber_duck = Arc::new(
            RubberDuckRunner::with_config(
                models.clone(),
                tools.clone(),
                tasks.clone(),
                budget.clone(),
                events.clone(),
                config.rubber_duck.clone(),
            )
            .expect(
                "orchestrator rubber-duck config must be validated before runtime construction",
            ),
        );
        Self {
            config,
            models,
            tools,
            tasks,
            memory,
            budget,
            policy: Arc::new(RwLock::new(policy)),
            checkpoints,
            events,
            model_router,
            metrics,
            decisions,
            children,
            last_turn_changed_files: Mutex::new(Vec::new()),
            last_turn_validation: Mutex::new(ValidationRecorder::new()),
            validation_workspace: Mutex::new(WorkspaceValidationConfig::default()),
            validation_changed_symbols: Mutex::new(Vec::new()),
            context_cache: Mutex::new(ContextPlanCache::new()),
            rubber_duck,
            rubber_duck_triggers: Mutex::new(RubberDuckTriggerController::default()),
            last_repair_stop: Mutex::new(None),
        }
    }
    /// Replaces the active tool policy without resetting session state.
    pub fn set_policy(&self, policy: PolicyEngine) {
        *self.policy.write().expect("runtime policy poisoned") = policy;
    }
    /// Snapshot of the active tool policy.
    #[must_use]
    pub fn policy(&self) -> PolicyEngine {
        self.policy.read().expect("runtime policy poisoned").clone()
    }
    /// Snapshot of the runtime's recovery/loop events (diagnostics and
    /// traceability; the loop's own recorder is separate).
    #[must_use]
    pub fn event_snapshot(&self) -> Vec<OrchestratorEvent> {
        self.events.events()
    }
    /// Bounded metadata-only snapshot of recent and active children.
    #[must_use]
    pub fn child_snapshot(&self, limit: usize) -> ChildSnapshot {
        self.children.snapshot(limit)
    }
    /// Requests cancellation of one child without affecting its parent or siblings.
    pub fn cancel_child(&self, id: &crate::subagents::SubagentId) -> ChildCancelResult {
        self.children.cancel(id)
    }
    /// Requests cancellation of every active child owned by this runtime.
    pub fn cancel_all_children(&self) -> usize {
        self.children.cancel_all()
    }
    /// Installs deterministic model routing for future subagent spawns.
    /// Every route must reference a model already present in the registry.
    pub fn set_model_router(&self, router: ModelRouter) -> Result<(), OrchestratorError> {
        for route in router.routes() {
            if !self.models.contains(&route.adapter_id) {
                return Err(OrchestratorError::InvalidState(format!(
                    "model route {} references unknown adapter {}",
                    route.route_id, route.adapter_id
                )));
            }
        }
        *self.model_router.write().expect("model router poisoned") = Some(router);
        Ok(())
    }
    /// Removes configured role routing; children fall back to explicit or
    /// parent model selection.
    pub fn clear_model_router(&self) {
        *self.model_router.write().expect("model router poisoned") = None;
    }
    /// Counter-only runtime metrics snapshot.
    #[must_use]
    pub fn metrics_snapshot(&self) -> OrchestratorMetrics {
        self.metrics.lock().expect("orchestrator metrics poisoned").clone()
    }
    /// Bounded delegation decision-log snapshot.
    #[must_use]
    pub fn decision_log_snapshot(&self) -> DecisionLog {
        self.decisions.lock().expect("decision log poisoned").clone()
    }
    /// The checkpoint store backing this runtime (same directory as every
    /// other runtime sharing the recovery config).
    #[must_use]
    pub(crate) fn checkpoint_store(&self) -> Arc<CheckpointStore> {
        self.checkpoints.clone()
    }
    /// Replaces the task/memory/budget stores from a validated checkpoint
    /// and restarts the wall-clock deadline slice.  Cumulative budget
    /// counters are retained; only the deadline is fresh.
    pub fn restore_from_checkpoint(
        &self,
        checkpoint: &OrchestratorCheckpoint,
    ) -> Result<(), OrchestratorError> {
        checkpoint.validate()?;
        *self.tasks.lock().expect("task graph poisoned") = checkpoint.tasks.clone();
        *self.memory.lock().expect("memory store poisoned") = checkpoint.memory.clone();
        let mut budget = BudgetTracker::new(&checkpoint.config);
        budget.restore_used(&checkpoint.budget)?;
        *self.budget.lock().expect("budget tracker poisoned") = budget;
        Ok(())
    }
    /// The active configuration.
    #[must_use]
    pub fn config(&self) -> &OrchestratorConfig {
        &self.config
    }
    /// Registers a server-side tool for the loop engine.
    pub fn register_tool(&self, tool: Arc<dyn ServerTool>) -> Result<(), OrchestratorError> {
        self.tools.lock().expect("tool registry poisoned").register(tool)
    }
    /// Registers the built-in client-bridge tools (`read_file`, `write_file`,
    /// terminal lifecycle, `ask_user`) bound to `session_id`.  Called once per
    /// session by the ACP adapter; the registry rejects duplicate names.
    pub fn register_builtins(
        &self,
        session_id: &ee_agent_protocol::SessionId,
    ) -> Result<(), OrchestratorError> {
        self.tools.lock().expect("tool registry poisoned").register_builtins(session_id)
    }
    /// Removes a previously registered tool (per-prompt MCP tools are
    /// deregistered at prompt end so they never leak across turns).
    pub fn remove_tool(&self, name: &str) {
        self.tools.lock().expect("tool registry poisoned").remove(name);
    }
    /// Names of the currently registered tools (tests and diagnostics).
    #[must_use]
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.lock().expect("tool registry poisoned").names()
    }
    /// Snapshot of the current memory store state.
    #[must_use]
    pub fn memory(&self) -> MemoryStore {
        self.memory.lock().expect("memory store poisoned").clone()
    }
    /// Snapshot of the current task graph state.
    #[must_use]
    pub fn tasks(&self) -> TaskGraph {
        self.tasks.lock().expect("task graph poisoned").clone()
    }
}
