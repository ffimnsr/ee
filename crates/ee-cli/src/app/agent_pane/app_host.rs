//! `impl App`: host worker ensure/start, reconnect, focus management.

use std::sync::Arc;
use std::time::Duration;

use ee_agent_host::{AgentManager, AgentManagerConfig};
use tokio::sync::mpsc as tokio_mpsc;

use super::super::*;

use super::host::AgentHostBridge;
use super::state::{PendingFork, PendingSession};
use super::thread_ui::ThreadUiState;
impl App {
    /// Phase 7 shutdown orchestration, called before the app exits:
    /// cancels running turns, resolves pending approvals/elicitations as
    /// cancelled, kills agent-owned terminals, and stops MCP servers and
    /// agent subprocesses.  Every step is internally bounded (host request
    /// timeouts), so a hung agent or MCP server cannot delay exit.
    pub(crate) fn shutdown_agents(&mut self) {
        // Persist local transcript before host teardown clears in-memory threads.
        self.persist_agent_workspace();
        // 1. Cancel running turns (also resolves their pending permissions).
        if let Some(host) = &self.agents.host {
            for thread in &self.agents.threads {
                if thread.host.is_turn_running() {
                    let _ = host.cancel(thread.host.clone());
                }
            }
        }
        // 2. Resolve pending approvals and elicitations as cancelled:
        //    dropping the reply senders makes the host resolve them.
        self.agents.approvals.clear();
        self.agents.write_leases.clear();
        self.agents.mode_selection = None;
        self.agents.approval_mode_confirmation = None;
        self.agents.elicitations.clear();
        self.agents.permissions.clear();
        self.agents.pending_cancels.clear();
        if let Some(pending) = self.agents.pending_external_critic.take() {
            let _ = pending.cancel.send(true);
        }
        // 3. Kill agent-owned terminals.
        self.agents.terminals.kill_all();
        // 4. Stop MCP servers and the proxy listener.
        self.shutdown_mcp();
        // 5. Stop ACP agent subprocesses (worker → manager shutdown).
        if let Some(host) = self.agents.host.take() {
            drop(host);
        }
        self.agents.threads.clear();
        self.agents.archived_threads.clear();
        self.agents.web_context_service = None;
        self.agents.web_context_config_fingerprint = None;
        self.agents.pending_sessions.clear();
        self.agents.workspace_restore = None;
        self.agents.approval_policy = crate::app::agent_bridge::ApprovalPolicy::default();
        self.agents.approval_modes.clear();
        self.agents.usage_ledger = crate::policy::UsageLedger::default();
    }

    /// `:agents_threads` — open modal thread picker.
    pub(super) fn open_agents_thread_picker(&mut self) {
        if self.agents.threads.is_empty() {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        }
        let items = self
            .agents
            .threads
            .iter()
            .enumerate()
            .map(|(index, thread)| {
                let state = match thread.state {
                    ThreadUiState::Starting => "starting",
                    ThreadUiState::Ready => "ready",
                    ThreadUiState::Queued => "queued",
                    ThreadUiState::Running => "running",
                    ThreadUiState::AwaitingPermission => "awaiting permission",
                    ThreadUiState::AwaitingElicitation => "awaiting elicitation",
                    ThreadUiState::Cancelling => "cancelling",
                    ThreadUiState::PausedRecoverable => "paused (recoverable)",
                    ThreadUiState::Closed => "closed",
                    ThreadUiState::Failed => "failed",
                };
                let unread = if thread.unread > 0 {
                    format!(" · unread:{}", thread.unread)
                } else {
                    String::new()
                };
                crate::picker::PickerItem {
                    label: thread.display_name.clone(),
                    detail: Some(format!(
                        "agent:{} · {state}{unread} · session:{}",
                        thread.agent_id, thread.session_id
                    )),
                    path: None,
                    buf_id: None,
                    line: None,
                    col: None,
                    choice_index: Some(index),
                }
            })
            .collect();
        self.open_picker(crate::picker::PickerState::new_agent_threads(items));
        if let Some(picker) = self.picker.as_mut()
            && let Some(active) = self.agents.active_thread
            && let Some(selected) = picker.filtered.iter().position(|index| *index == active)
        {
            picker.selected = selected;
        }
    }

    /// Focuses thread `index`, resetting its unread state.
    pub(crate) fn focus_thread(&mut self, index: usize) {
        self.agents.active_thread = Some(index);
        if let Some(thread) = self.agents.threads.get_mut(index) {
            thread.unread = 0;
            thread.activity = false;
        }
        self.persist_agent_workspace();
        self.enter_agent_focus();
    }

    /// Enters agents focus, remembering the editor mode to return to.
    ///
    /// The mode seen here is usually `CommandLine` (commands run from `:`);
    /// the mode to restore on close is the one held before the pane opened,
    /// captured on first focus and never overwritten.
    pub(crate) fn enter_agent_focus(&mut self) {
        if self.agents_focused() {
            return;
        }
        if self.agents.previous_editor_mode.is_none() {
            let previous = match self.mode {
                Mode::CommandLine => self.command_mode_origin.unwrap_or(Mode::Normal),
                mode => mode,
            };
            if previous != Mode::Agent {
                self.agents.previous_editor_mode = Some(previous);
            }
        }
        self.mode = Mode::Agent;
    }

    pub(super) fn external_rubber_duck_available(&self) -> bool {
        let settings = &self.config.agents.rubber_duck;
        if settings.mode == crate::config::RubberDuckModeSetting::Off
            || settings.internal_model_id.is_some()
        {
            return false;
        }
        let Some(agent_id) = settings.external_agent_id.as_deref() else {
            return false;
        };
        self.agents.host.as_ref().is_some_and(|host| host.manager.has_agent(agent_id))
    }

    /// The agent id used for `:agents` / `:agents_new`.
    pub(super) fn default_agent_id(&self) -> Option<String> {
        let host = self.agents.host.as_ref()?;
        host.manager.resolve_default_agent(self.config.agents.default_agent.as_deref())
    }

    /// Creates the host bridge on first use (lazy).
    ///
    /// Secret references in agent env values are resolved here, immediately
    /// before `AgentProcessConfig` creation; a server whose references fail
    /// to resolve is skipped and never spawned.
    pub(super) fn ensure_agents_host(&mut self) {
        if self.agents.host.is_some() {
            return;
        }
        let servers: Vec<(String, crate::config::AgentServerSettings)> = self
            .config
            .agents
            .servers
            .iter()
            .map(|(id, server)| (id.clone(), server.clone()))
            .collect();
        let mut config = AgentManagerConfig::default();
        let mut secret_store: Option<crate::secrets::SecretStore> = None;
        for (id, server) in servers {
            let env = if crate::secrets::resolve::agent_env_has_references(&server.env) {
                if secret_store.is_none() {
                    secret_store = self.build_agents_secret_store();
                }
                let Some(store) = &secret_store else {
                    eprintln!(
                        "ee: warning: agent `{id}` launch aborted: secrets store unavailable"
                    );
                    continue;
                };
                match crate::secrets::resolve::resolve_agent_env(store, &server) {
                    Ok(env) => env,
                    Err(err) => {
                        eprintln!("ee: warning: agent `{id}` launch aborted: {err}");
                        continue;
                    }
                }
            } else {
                server.env.iter().map(|(key, value)| (key.clone(), value.raw.clone())).collect()
            };
            // Collect secret-like final values for stderr/diagnostics
            // redaction (phase 5).
            for (name, value) in &env {
                if ee_agent_host::redact::is_secret_key(name) {
                    self.agents.resolved_secret_values.push(value.clone());
                }
            }
            config.agents.insert(
                id.clone(),
                ee_agent_host::AgentProcessConfig {
                    command: server.command.clone(),
                    args: server.args.clone(),
                    env,
                    cwd: server.cwd.clone(),
                },
            );
        }
        // Always host policy-governed editor MCP for ACP-native agents. Explicit
        // proxy configuration remains required only for stdio fallback.
        config.ee_proxy_enabled = true;
        let memory = &self.config.agents.workspace_memory;
        config.workspace_memory = ee_agent_host::WorkspaceMemoryHostConfig {
            enabled: memory.enabled,
            trusted_roots: self.canonical_workspace_roots(),
            database_path: None,
            quotas: ee_agent_host::WorkspaceMemoryQuotas {
                max_value_bytes: memory.max_value_bytes,
                max_active_facts: memory.max_active_facts,
                max_active_bytes: memory.max_active_bytes,
                max_total_facts: memory.max_total_facts,
                max_total_bytes: memory.max_total_bytes,
                max_recall_results: memory.max_recall_results,
            },
            busy_timeout: Duration::from_millis(memory.busy_timeout_ms),
            retention: ee_agent_host::MemoryRetention {
                default_expiry: (memory.default_expiry_days != 0)
                    .then(|| Duration::from_secs(memory.default_expiry_days * 86_400)),
                candidate_retention: Duration::from_secs(memory.candidate_retention_days * 86_400),
                stale_retention: Duration::from_secs(memory.stale_retention_days * 86_400),
                superseded_retention: Duration::from_secs(
                    memory.superseded_retention_days * 86_400,
                ),
            },
        };
        #[cfg(test)]
        for (id, factory) in &self.agents.test_fake_transports {
            config.fake_transports.insert(id.clone(), factory.clone());
        }
        let (events_tx, events_rx) = tokio_mpsc::unbounded_channel();
        let handler: Arc<dyn ee_agent_host::ClientRequestHandler> =
            Arc::new(crate::app::agent_bridge::BridgeUiHandler::new(
                self.agents.bridge_tx.clone(),
                self.agents.terminals.clone(),
            ));
        let options = ee_agent_host::AgentConnectionOptions {
            max_concurrent_prompts: self.config.agents.max_concurrent_prompts,
            ..ee_agent_host::AgentConnectionOptions::default()
        };
        let manager = AgentManager::with_options(config, handler, events_tx, options);
        self.agents.host = Some(AgentHostBridge::new(manager, events_rx));
    }

    /// Builds the secrets store used by lazy agent-launch and web-search reference resolution.
    /// Tests inject a fake store; production uses the real default.
    pub(crate) fn build_agents_secret_store(&mut self) -> Option<crate::secrets::SecretStore> {
        #[cfg(test)]
        {
            self.agents.test_secret_store.take()
        }
        #[cfg(not(test))]
        {
            match crate::secrets::SecretStore::default() {
                Ok(store) => Some(store),
                Err(err) => {
                    eprintln!("ee: warning: secrets store unavailable: {err}");
                    None
                }
            }
        }
    }

    /// Requests a new session for `agent_id` (async; reply pumped later).
    pub(super) fn start_session(&mut self, agent_id: String) {
        self.start_session_with_fork(agent_id, None);
    }

    pub(super) fn start_session_with_fork(&mut self, agent_id: String, fork: Option<PendingFork>) {
        let Some(host) = &self.agents.host else {
            return;
        };
        let roots = self.agents_workspace_roots();
        let mcp_servers = crate::app::agents_mcp::mcp_forward_entries(&self.config.mcp);
        let ee_proxy_stdio_fallback =
            self.agents.mcp.proxy.as_ref().map(crate::app::agents_mcp::proxy_stdio_fallback_entry);
        let reply =
            host.request_new_session(agent_id.clone(), roots, mcp_servers, ee_proxy_stdio_fallback);
        let key = self.agents.next_lifecycle_key(agent_id, None);
        self.agents.pending_sessions.insert(key, PendingSession { reply, fork });
    }

    /// Reconnects selected session from this workspace's persisted thread list.
    pub(super) fn agents_reconnect(&mut self) {
        let Some(workspace) = self.load_persisted_agent_workspace() else {
            self.agents.error =
                Some(String::from("no persisted agent session for this workspace to reconnect"));
            return;
        };
        let selected_id = self
            .agents
            .active_thread_index()
            .and_then(|index| self.agents.threads.get(index))
            .map(|thread| thread.session_id.as_str())
            .or(workspace.active_session_id.as_deref());
        let Some(record) = selected_id
            .and_then(|session_id| {
                workspace.sessions.iter().find(|record| record.session_id == session_id)
            })
            .or_else(|| workspace.sessions.first())
            .cloned()
        else {
            self.agents.error =
                Some(String::from("no persisted agent session for this workspace to reconnect"));
            return;
        };
        self.request_persisted_agent_reconnect(record);
    }
}
