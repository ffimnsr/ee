//! `impl OrchestratorRuntime` methods: lifecycle.
use super::*;

impl OrchestratorRuntime {
    /// Wraps the runtime in an `Arc` and records the weak self handle used by
    /// turn-scoped hooks such as model-callable compaction.
    pub(crate) fn into_shared(self) -> Arc<Self> {
        Arc::new_cyclic(|weak| {
            let _ = self.self_ref.set(weak.clone());
            self
        })
    }

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
        config.validate()?;
        Self::with_shared_model_registry(config, Arc::new(models), policy)
    }
    /// Creates a runtime from a process-owned shared registry. Adapters and
    /// provider clients stay in memory and are never checkpointed.
    pub fn with_shared_model_registry(
        config: OrchestratorConfig,
        models: Arc<ModelRegistry>,
        policy: PolicyEngine,
    ) -> Result<Self, OrchestratorError> {
        config.validate()?;
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
        let todo_plan = Arc::new(Mutex::new(crate::todo_plan::TodoPlan::new()));
        let tools = Arc::new(Mutex::new(ToolRegistry::new()));
        let checkpoints = Arc::new(CheckpointStore::new(&config.recovery));
        let events = EventRecorder::new();
        let model_router = Arc::new(RwLock::new(None));
        let metrics = Arc::new(Mutex::new(OrchestratorMetrics::new()));
        let decisions = Arc::new(Mutex::new(DecisionLog::default()));
        let children = Arc::new(ChildRegistry::default());
        let worktrees = Arc::new(Mutex::new(Vec::new()));
        let transcripts =
            Arc::new(Mutex::new(crate::subagent_transcripts::ChildTranscriptStore::default()));
        let background_jobs = Arc::new(BackgroundJobRegistry::new());
        let user_agents = Arc::new(RwLock::new(UserAgentRegistry::default()));
        let user_commands = Arc::new(RwLock::new(UserCommandRegistry::default()));
        let manager = Arc::new(
            SubagentManager::new(
                config.clone(),
                models.clone(),
                tools.clone(),
                SubagentState::new(tasks.clone(), memory.clone(), budget.clone(), children.clone())
                    .with_worktrees(worktrees.clone())
                    .with_transcripts(transcripts.clone()),
                SubagentObservability::new(
                    model_router.clone(),
                    metrics.clone(),
                    decisions.clone(),
                ),
                background_jobs.clone(),
            )
            .with_user_agents(user_agents.clone()),
        );
        tools
            .lock()
            .expect("tool registry poisoned")
            .register(Arc::new(DelegateTool::new(manager.clone())))
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
        let session_logs = SessionLogStore::from_config(&config.session_log);
        let snapshots = crate::workspace_snapshot::WorkspaceSnapshotStore::from_config(
            &config.snapshots,
            &config.session_log,
        );
        Self {
            config,
            models,
            tools,
            tasks,
            todo_plan,
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
            last_turn_execution_log: Mutex::new(Vec::new()),
            validation_workspace: Mutex::new(WorkspaceValidationConfig::default()),
            validation_changed_symbols: Mutex::new(Vec::new()),
            context_cache: Mutex::new(ContextPlanCache::new()),
            rubber_duck,
            rubber_duck_triggers: Mutex::new(RubberDuckTriggerController::default()),
            last_repair_stop: Mutex::new(None),
            runtime_snapshots: Mutex::new(HashMap::new()),
            session_logs,
            last_turn_transcript: Arc::new(Mutex::new(Vec::new())),
            workspace_scope: Arc::new(Mutex::new(None)),
            middleware_spec: Mutex::new(None),
            session_move_target: Arc::new(Mutex::new(None)),
            pending_title: Arc::new(Mutex::new(None)),
            read_only_tools: std::sync::atomic::AtomicBool::new(false),
            session_context: Mutex::new(crate::session_context::SessionContext::default()),
            workspace_instruction_seen: Arc::new(Mutex::new(std::collections::HashSet::new())),
            workspace_instruction_known: Arc::new(Mutex::new(std::collections::HashMap::new())),
            background_jobs,
            worktrees,
            transcripts,
            until: Mutex::new(None),
            compaction_checkpoints: Mutex::new(HashMap::new()),
            selected_model: Mutex::new(None),
            selected_reasoning_effort: Mutex::new(None),
            user_agents,
            active_user_agent: Mutex::new(None),
            user_commands,
            advertised_commands: Mutex::new(None),
            subagents: manager,
            snapshots,
            self_ref: std::sync::OnceLock::new(),
        }
    }

    /// Sets the session's model registry id (`None` restores the default).
    pub fn set_selected_model(&self, model_id: Option<String>) {
        *self.selected_model.lock().expect("selected model poisoned") = model_id;
    }

    /// The session's explicit model registry id, when one was selected.
    #[must_use]
    pub fn selected_model(&self) -> Option<String> {
        self.selected_model.lock().expect("selected model poisoned").clone()
    }

    /// Sets the runtime reasoning effort (`None` restores the provider default).
    pub fn set_reasoning_effort(&self, reasoning_effort: Option<String>) {
        *self.selected_reasoning_effort.lock().expect("selected effort poisoned") =
            reasoning_effort;
    }

    /// The runtime reasoning effort, when one was selected.
    #[must_use]
    pub fn selected_reasoning_effort(&self) -> Option<String> {
        self.selected_reasoning_effort.lock().expect("selected effort poisoned").clone()
    }

    /// Reloads user-defined agents from the workspace and personal config
    /// roots. Workspace definitions win over personal ones with the same name.
    ///
    /// A selected agent that disappears is cleared with its permission
    /// overlay, so a removed definition cannot keep stale authority.
    pub fn reload_user_agents(
        &self,
        cwd: &std::path::Path,
        workspace_roots: &[std::path::PathBuf],
        config_root: Option<&std::path::Path>,
    ) {
        let registry = UserAgentRegistry::discover(cwd, workspace_roots, config_root);
        let active = self.active_user_agent_name();
        if let Some(name) = &active
            && registry.get(name).is_none()
        {
            tracing::warn!(agent = %name, "selected user agent no longer exists; clearing selection");
            *self.active_user_agent.lock().expect("active user agent poisoned") = None;
            self.policy.write().expect("policy poisoned").set_permission_rules(None);
        }
        *self.user_agents.write().expect("user agent registry poisoned") = registry;
    }

    /// Selects a primary user-defined agent, or clears the selection with
    /// `None` / `"default"`.
    ///
    /// The agent's model is resolved against the registry before any state
    /// changes; an unknown or non-chat-capable model fails closed with the
    /// available ids. Subagent-mode definitions are rejected because they
    /// belong to `delegate_task`.
    pub fn select_user_agent(&self, name: Option<&str>) -> Result<(), String> {
        let name = name.map(str::trim).filter(|name| !name.is_empty());
        if name.is_none_or(|name| matches!(name, "default" | "none")) {
            *self.active_user_agent.lock().expect("active user agent poisoned") = None;
            self.policy.write().expect("policy poisoned").set_permission_rules(None);
            return Ok(());
        }
        let name = name.expect("checked above");
        let (agent, available) = {
            let registry = self.user_agents.read().expect("user agent registry poisoned");
            let agent = registry.get(name).cloned();
            let available = {
                let names = registry
                    .primary()
                    .filter(|agent| !agent.hidden)
                    .map(|agent| agent.name.clone())
                    .collect::<Vec<_>>();
                if names.is_empty() { "none".to_string() } else { names.join(", ") }
            };
            (agent, available)
        };
        let Some(agent) = agent else {
            return Err(format!("unknown agent {name:?}; available primary agents: {available}"));
        };
        if !agent.is_primary() {
            return Err(format!(
                "agent `{name}` is subagent-only; delegate it with delegate_task instead"
            ));
        }
        let mut selected_model = None;
        for candidate in agent.model_candidates() {
            if let Some(info) = self.models.info(&candidate)
                && info.enabled
                && info
                    .identity
                    .capabilities
                    .contains(&crate::model_registry::ModelCapability::ChatCompletion)
            {
                selected_model = Some(candidate);
                break;
            }
        }
        if !agent.model_candidates().is_empty() && selected_model.is_none() {
            let available = self
                .models
                .advertised()
                .into_iter()
                .map(|model| model.id)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "agent `{name}` model {:?} is not a registered chat-capable model; available models: {available}",
                agent.model_candidates().first().cloned().unwrap_or_default()
            ));
        }
        if let Some(model_id) = selected_model {
            self.set_selected_model(Some(model_id));
        }
        self.policy
            .write()
            .expect("policy poisoned")
            .set_permission_rules(agent.permissions.clone());
        *self.active_user_agent.lock().expect("active user agent poisoned") =
            Some(agent.name.clone());
        Ok(())
    }

    /// Restores a persisted selection without re-resolving its model; the
    /// provider applies the persisted model separately.
    pub fn restore_user_agent(&self, name: &str) -> Result<(), String> {
        let agent = self
            .user_agents
            .read()
            .expect("user agent registry poisoned")
            .get(name)
            .cloned()
            .ok_or_else(|| format!("persisted user agent {name:?} no longer exists"))?;
        if !agent.is_primary() {
            return Err(format!("persisted agent {name:?} is not a primary agent"));
        }
        self.policy
            .write()
            .expect("policy poisoned")
            .set_permission_rules(agent.permissions.clone());
        *self.active_user_agent.lock().expect("active user agent poisoned") = Some(agent.name);
        Ok(())
    }

    /// The selected primary user-defined agent, when one is active.
    #[must_use]
    pub fn active_user_agent(&self) -> Option<crate::user_agents::UserAgentDefinition> {
        let name = self.active_user_agent_name()?;
        self.user_agents.read().expect("user agent registry poisoned").get(&name).cloned()
    }

    /// The selected user-agent name, when one is active.
    #[must_use]
    pub fn active_user_agent_name(&self) -> Option<String> {
        self.active_user_agent.lock().expect("active user agent poisoned").clone()
    }

    /// Snapshot of the discovered agent registry for listings and commands.
    #[must_use]
    pub fn user_agents_snapshot(&self) -> UserAgentRegistry {
        self.user_agents.read().expect("user agent registry poisoned").clone()
    }

    /// Reloads markdown command templates from the workspace and personal
    /// config roots.
    pub fn reload_user_commands(
        &self,
        cwd: &std::path::Path,
        workspace_roots: &[std::path::PathBuf],
        config_root: Option<&std::path::Path>,
    ) {
        let registry = UserCommandRegistry::discover(cwd, workspace_roots, config_root);
        *self.user_commands.write().expect("user command registry poisoned") = registry;
    }

    /// Snapshot of the discovered command registry.
    #[must_use]
    pub fn user_commands_snapshot(&self) -> UserCommandRegistry {
        self.user_commands.read().expect("user command registry poisoned").clone()
    }

    /// Whether this session has already received a template-aware command
    /// list. `false` means only the initial built-in list was advertised.
    pub(crate) fn commands_were_advertised(&self) -> bool {
        self.advertised_commands.lock().expect("advertised commands poisoned").is_some()
    }

    /// Marks the current command-name set advertised. Returns `true` when the
    /// set changed and the client needs a fresh completion list.
    pub(crate) fn mark_commands_advertised(&self, names: &[String]) -> bool {
        let mut advertised = self.advertised_commands.lock().expect("advertised commands poisoned");
        if advertised.as_deref() == Some(names) {
            return false;
        }
        *advertised = Some(names.to_vec());
        true
    }

    /// Resolves the role for a `subtask: true` command. The default is the
    /// read-only researcher role; a named built-in or user-defined subagent
    /// replaces it. Primary-only agents are rejected.
    pub(crate) fn command_role(
        &self,
        agent: Option<&str>,
    ) -> Result<SubagentRole, OrchestratorError> {
        self.subagents.command_role(agent)
    }

    /// Runs one command-defined subtask through the shared subagent manager,
    /// so it uses the same budget, task graph, cancellation, and evidence
    /// verification as `delegate_task`.
    pub(crate) async fn run_user_command_subtask(
        &self,
        role: SubagentRole,
        prompt: String,
        model_id: Option<String>,
        context_snapshot: Vec<ModelMessage>,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
    ) -> Result<SubagentResult, OrchestratorError> {
        let parent = {
            let mut tasks = self.tasks.lock().expect("task graph poisoned");
            tasks.create_root("user command", &truncate(&prompt, 200))
        };
        let scope = self
            .workspace_scope()
            .map(|(_, roots)| crate::workspace_scope::WorkspaceScope::new(roots, Vec::new()));
        let request = DelegationRequest {
            parent_task_id: parent.id.clone(),
            role,
            scoped_prompt: prompt,
            context_snapshot,
            scope,
            model_id,
            resume_child_id: None,
            fork_context: false,
        };
        let result = self.subagents.spawn(request, client, cancel, self.events.clone()).await;
        let status = if result.is_ok() { TaskStatus::Completed } else { TaskStatus::Failed };
        let _ = self.tasks.lock().expect("task graph poisoned").transition(&parent.id, status);
        result
    }

    /// Root-turn config with an active agent's `steps` budget applied.
    #[must_use]
    pub(crate) fn config_for_turn(&self) -> OrchestratorConfig {
        let mut config = self.config.clone();
        if let Some(steps) = self.active_user_agent().and_then(|agent| agent.steps) {
            config.max_loop_iterations =
                Some(config.max_loop_iterations.map_or(steps, |cap| cap.min(steps)));
        }
        config
    }

    /// Workspace snapshot store for this session.
    #[must_use]
    pub fn workspace_snapshots(&self) -> &crate::workspace_snapshot::WorkspaceSnapshotStore {
        &self.snapshots
    }

    /// Resolves the adapter for this session's turns: the selected model when
    /// one is set, otherwise the registry default.  Fails closed when the
    /// selected id is no longer registered.
    pub(crate) fn model_for_turn(&self) -> Result<Arc<dyn ModelAdapter>, OrchestratorError> {
        let selected = self.selected_model();
        match selected {
            Some(model_id) => self.models.get(&model_id).ok_or_else(|| {
                OrchestratorError::InvalidState(format!(
                    "selected model {model_id:?} is not registered in this provider"
                ))
            }),
            None => self.models.default_adapter(),
        }
    }

    /// Installs a loop stop predicate, evaluated after each model step of every
    /// subsequent root turn.
    ///
    /// Returning `true` ends the turn with the normal final response, so a
    /// caller can end a multi-step loop deterministically instead of relying on
    /// an iteration cap. Passing `None` clears it.
    pub fn set_until_hook(&self, until: Option<UntilPredicate>) {
        *self.until.lock().expect("until hook poisoned") = until;
    }

    /// Returns the installed loop stop predicate, when one is set.
    #[must_use]
    pub fn until_hook(&self) -> Option<UntilPredicate> {
        self.until.lock().expect("until hook poisoned").clone()
    }
    /// Records the session working directory and workspace roots for mid-turn
    /// nested `AGENTS.md`/`CLAUDE.md` discovery. Called once per prompt by the
    /// provider adapter before the turn starts; a model-requested
    /// `session_move` takes precedence over the adapter's original workspace.
    pub fn set_workspace_scope(&self, cwd: std::path::PathBuf, roots: Vec<std::path::PathBuf>) {
        let cwd = self
            .session_move_target
            .lock()
            .expect("session move target poisoned")
            .clone()
            .unwrap_or(cwd);
        *self.workspace_scope.lock().expect("workspace scope poisoned") = Some((cwd, roots));
    }

    /// Working directory requested by `session_move`, when the model moved this
    /// session. Providers use it as the prompt cwd instead of the workspace
    /// captured at `session/new`.
    #[must_use]
    pub fn session_workspace_override(&self) -> Option<std::path::PathBuf> {
        self.session_move_target.lock().expect("session move target poisoned").clone()
    }

    /// Replaces the declarative middleware hook set for following turns.
    ///
    /// `None` disables middleware entirely, keeping tool results byte-identical
    /// to a session that never had a middleware file.
    pub fn set_middleware_spec(&self, spec: Option<crate::middleware::MiddlewareSpec>) {
        *self.middleware_spec.lock().expect("middleware spec poisoned") = spec;
    }

    /// Snapshot of the declarative middleware hooks for one turn.
    #[must_use]
    pub(crate) fn middleware_spec(&self) -> Option<crate::middleware::MiddlewareSpec> {
        self.middleware_spec.lock().expect("middleware spec poisoned").clone()
    }

    /// Restricts the advertised tool set to read-class tools for following
    /// turns. Plan mode sets this; execution policy still denies mutations.
    pub fn set_read_only_tools(&self, enabled: bool) {
        self.read_only_tools.store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether the advertised tool set is restricted to read-class tools.
    #[must_use]
    pub(crate) fn read_only_tools(&self) -> bool {
        self.read_only_tools.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Canonical advertised tool definitions for this session, with the
    /// read-class restriction applied when plan mode is active.
    pub(crate) fn advertised_definitions(&self) -> Vec<crate::tools::ToolDefinition> {
        let definitions = crate::tools::canonical_tool_definitions(
            self.tools.lock().expect("tool registry poisoned").definitions(),
            None,
        );
        if self.read_only_tools() {
            crate::tools::read_only_definitions(definitions)
        } else {
            definitions
        }
    }

    /// Snapshot of the session workspace scope, when one was recorded.
    #[must_use]
    pub(crate) fn workspace_scope(&self) -> Option<(std::path::PathBuf, Vec<std::path::PathBuf>)> {
        self.workspace_scope.lock().expect("workspace scope poisoned").clone()
    }

    /// Builds the opt-in approval classifier over the configured (or default)
    /// registered model. Disabled or unknown-model configs return `None`, so
    /// approval stays on the interactive host path.
    pub(crate) fn approval_classifier(
        &self,
    ) -> Option<Arc<crate::approval_classifier::ApprovalClassifier>> {
        if !self.config.approval_classifier.mode.is_enabled() {
            return None;
        }
        let model_id = self
            .config
            .approval_classifier
            .model_id
            .clone()
            .unwrap_or_else(|| crate::model_registry::DEFAULT_MODEL_ID.to_string());
        let adapter = self.models.get(&model_id)?;
        let backend =
            Arc::new(crate::approval_classifier::ModelRiskClassifier::new(adapter, model_id));
        Some(Arc::new(crate::approval_classifier::ApprovalClassifier::new(
            backend,
            self.config.approval_classifier.timeout,
        )))
    }

    /// Renders active background terminal jobs for the runtime prompt section.
    #[must_use]
    pub fn background_jobs_snapshot(&self) -> Option<String> {
        self.background_jobs.snapshot_text()
    }

    /// Kills and releases every running background terminal job and clears the
    /// registry. Used by `/discard`; session cancel/close clear state only
    /// because the host owns the terminal processes.
    pub async fn terminate_background_jobs(
        &self,
        client: &ClientBridge,
        session_id: &ee_agent_protocol::SessionId,
    ) {
        self.background_jobs.terminate_all(client, session_id).await;
    }

    /// Drops background job state without touching host terminals.
    pub fn clear_background_jobs(&self) {
        // Session cancel/close/discard must stop detached delegates before
        // their shared state disappears.
        self.background_jobs.cancel_subagents();
        self.background_jobs.clear();
    }

    /// Prunes every retained isolated worktree. Called on `/discard` and
    /// session close so speculative child patches never outlive the session.
    pub fn prune_worktrees(&self) {
        let mut worktrees = self.worktrees.lock().expect("worktrees poisoned");
        for worktree in worktrees.drain(..) {
            let _ = worktree.prune();
        }
    }

    /// Worktrees retained for host apply.
    #[must_use]
    pub fn retained_worktrees(&self) -> Vec<std::path::PathBuf> {
        self.worktrees
            .lock()
            .expect("worktrees poisoned")
            .iter()
            .map(|worktree| worktree.path().to_path_buf())
            .collect()
    }

    /// Drops retained subagent transcripts so `/discard`, cancel, and visit
    /// close cannot resume a child from a discarded session.
    pub fn clear_subagent_transcripts(&self) {
        self.transcripts.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clear();
    }

    /// Sweeps retained tool-output overflow artifacts for one closed session.
    ///
    /// The record log stays as the durable audit trail; only bodies no record
    /// references are removed, which is exactly the retained overflow that no
    /// later request re-reads. Memory-only logs own no files, so this is a
    /// no-op there.
    pub fn purge_session_artifacts(&self, session_id: &str) {
        self.snapshots.purge_session(session_id);
        let Some(store) =
            crate::session_log::SessionLogStore::from_config(&self.config.session_log)
        else {
            return;
        };
        match store.log_for(session_id).purge_artifacts() {
            Ok(0) => {}
            Ok(removed) => {
                tracing::debug!(session_id, removed, "swept retained tool-output artifacts");
            }
            Err(error) => {
                tracing::warn!(%error, session_id, "sweeping retained tool-output artifacts failed");
            }
        }
    }

    /// Installs an observe-only observer on the runtime event recorder.
    ///
    /// The recorder keeps the same event buffer, so early events are not lost.
    /// Plugin observation must never change loop decisions.
    #[must_use]
    pub fn with_plugin_observer(mut self, observer: crate::events::EventObserver) -> Self {
        self.events = self.events.with_observer(move |event| observer(event));
        self
    }

    /// Replaces the active tool policy without resetting session state.
    pub fn set_policy(&self, mut policy: PolicyEngine) {
        // A mode or plugin policy refresh must not silently drop the selected
        // user agent's deny overlay.
        if let Some(rules) = self.active_user_agent().and_then(|agent| agent.permissions) {
            policy.set_permission_rules(Some(rules));
        }
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
    /// Session log handle for one session, when durable logging is enabled.
    pub(crate) fn session_log_for(
        &self,
        session_id: &str,
    ) -> Option<Arc<crate::session_log::SessionLog>> {
        self.session_logs.as_ref().map(|store| store.log_for(session_id))
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
        let mut tools = self.tools.lock().expect("tool registry poisoned");
        tools.register_builtins(session_id)?;
        tools.register_todo_tool(self.todo_plan.clone())?;
        for tool in crate::tools::session_tools(Arc::new(crate::tools::SessionToolContext {
            models: self.models.clone(),
            config: self.config.clone(),
            workspace_scope: self.workspace_scope.clone(),
            session_move_target: self.session_move_target.clone(),
            pending_title: self.pending_title.clone(),
        })) {
            tools.register(tool)?;
        }
        if self.config.workspace_isolation.enabled {
            for tool in crate::tools::worktree_tools(
                self.worktrees.clone(),
                self.config.workspace_isolation.clone(),
            ) {
                tools.register(tool)?;
            }
        }
        if let Some(store) = &self.session_logs {
            tools.register(Arc::new(crate::usage_stats::UsageStatsTool::new(
                store.log_for(session_id.0.as_ref()),
            )))?;
        }
        Ok(())
    }
    /// ACP plan entries for the current session plan.
    ///
    /// A model-written todo list owns the user-visible plan; otherwise the
    /// execution task graph remains the plan, preserving prior behavior.
    #[must_use]
    pub(crate) fn plan_entries_for_emission(&self) -> Vec<ee_agent_protocol::PlanEntry> {
        let plan = self.todo_plan.lock().expect("todo plan poisoned");
        if plan.is_empty() {
            self.tasks.lock().expect("task graph poisoned").plan_entries()
        } else {
            plan.plan_entries()
        }
    }
    /// Bounded prompt section for the current todo plan, when non-empty.
    #[must_use]
    pub(crate) fn todo_plan_prompt(&self) -> Option<String> {
        self.todo_plan.lock().expect("todo plan poisoned").render_prompt()
    }

    /// Records the provider-facing affinity for this session.
    ///
    /// Providers key prompt caches and per-session rate limits on the affinity,
    /// so a branch or forked child shares its parent's lineage while carrying
    /// its own session id. The provider adapter seeds this once per session;
    /// the loop stamps the active session id on every request.
    pub fn set_session_context(&self, context: crate::session_context::SessionContext) {
        *self.session_context.lock().expect("session context poisoned") = context;
    }
    /// Snapshot of the provider-facing affinity for this session.
    #[must_use]
    pub fn session_context_snapshot(&self) -> crate::session_context::SessionContext {
        self.session_context.lock().expect("session context poisoned").clone()
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
    /// Normalized model-visible messages from the latest root turn, including
    /// tool calls and tool results. Empty before the first turn completes.
    #[must_use]
    pub fn last_turn_transcript(&self) -> Vec<ModelMessage> {
        self.last_turn_transcript.lock().expect("last turn transcript poisoned").clone()
    }
    /// Replays the append-only session log for `session_id`, including a branch
    /// session produced by [`Self::branch_from_checkpoint`]. Returns an empty
    /// vector when the log is disabled.
    pub fn replay_session_messages(
        &self,
        session_id: &str,
    ) -> Result<Vec<(String, ModelMessage)>, OrchestratorError> {
        let Some(store) = &self.session_logs else {
            return Ok(Vec::new());
        };
        store.log_for(session_id).replay_messages()
    }

    /// Live durable log for `session_id`, when session logging is enabled.
    ///
    /// Transfer commands must use this handle: a memory-only runtime owns the
    /// only sink, so rebuilding a store from configuration would see no
    /// records.
    #[must_use]
    pub fn session_log(&self, session_id: &str) -> Option<Arc<crate::session_log::SessionLog>> {
        self.session_logs.as_ref().map(|store| store.log_for(session_id))
    }

    /// Live session-log store, when logging is enabled.
    #[must_use]
    pub fn session_log_store(&self) -> Option<Arc<crate::session_log::SessionLogStore>> {
        self.session_logs.clone()
    }

    /// Replays the logged conversation prefix ending at `checkpoint_seq`.
    ///
    /// Returns `None` when the session log is disabled or has no record for
    /// that checkpoint, so a revert can report `conversation_reverted: false`
    /// instead of silently keeping post-checkpoint messages.
    pub fn replay_session_messages_until(
        &self,
        session_id: &str,
        checkpoint_seq: u64,
    ) -> Result<Option<Vec<(String, ModelMessage)>>, OrchestratorError> {
        let Some(store) = &self.session_logs else {
            return Ok(None);
        };
        let log = store.log_for(session_id);
        let records = log.records()?;
        let Some(position) = records.iter().position(|record| {
            matches!(
                record,
                crate::session_log::SessionLogRecord::Checkpoint { checkpoint_seq: seq, .. }
                    if *seq == checkpoint_seq
            )
        }) else {
            return Ok(None);
        };
        let message_count = records[..=position]
            .iter()
            .filter(|record| matches!(record, crate::session_log::SessionLogRecord::Message(_)))
            .count();
        let mut messages = log.replay_messages()?;
        messages.truncate(message_count);
        Ok(Some(messages))
    }
}
