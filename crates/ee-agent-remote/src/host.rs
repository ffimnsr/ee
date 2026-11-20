//! Headless host wrapper around [`AgentManager`].
//!
//! The host owns the single agent manager instance, the session registry, the
//! notification bus, and the agent-event pump. Construction never spawns an
//! agent subprocess; connections stay lazy exactly like the editor path.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ee_agent_host::{
    AgentConnectionOptions, AgentConnectionState, AgentEvent, AgentManager, AgentThread,
    ClientRequestHandler, DenyAllHandler, ThreadCloseReason, TurnKey,
};
use ee_agent_protocol::{SessionId, StopReason};
use serde_json::json;
use tokio::sync::mpsc;

use crate::config::RemoteHostConfig;
use crate::error::RemoteError;
use crate::events::EventBus;
use crate::headless::HeadlessHandler;
use crate::history::{SessionLog, entry_for_turn};
use crate::permissions::RemoteApprovals;
use crate::permissions::audit::AuditLog;
use crate::protocol::{
    RemoteFailure, SessionUpdateNotification, StatusChangedNotification, StatusState,
    TurnCompletedNotification, TurnStatus,
};
use crate::redact::Redactor;

/// One loaded session and its remote turn reservation.
pub struct RemoteSession {
    pub(crate) agent_id: String,
    pub(crate) thread: AgentThread,
    pub(crate) cwd: PathBuf,
    turn_seq: AtomicU64,
    active_turn: Mutex<Option<u64>>,
}

impl std::fmt::Debug for RemoteSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteSession")
            .field("agent_id", &self.agent_id)
            .field("session_id", &self.thread.session_id().0)
            .field("cwd", &self.cwd)
            .field("turn_running", &self.thread.is_turn_running())
            .finish_non_exhaustive()
    }
}

impl RemoteSession {
    fn new(agent_id: String, thread: AgentThread, cwd: PathBuf) -> Self {
        Self { agent_id, thread, cwd, turn_seq: AtomicU64::new(0), active_turn: Mutex::new(None) }
    }

    /// The ACP session id.
    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        self.thread.session_id()
    }

    /// The owning agent id.
    #[must_use]
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// The session working directory.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Whether a turn is currently running.
    #[must_use]
    pub fn turn_running(&self) -> bool {
        self.thread.is_turn_running()
    }

    /// Reserves one remote turn id. Returns `None` when a turn is already
    /// reserved or the ACP thread reports a running turn.
    pub(crate) fn reserve_turn(&self) -> Option<u64> {
        if self.thread.is_turn_running() {
            return None;
        }
        let mut active = self.active_turn.lock().expect("remote turn poisoned");
        if active.is_some() {
            return None;
        }
        let id = self.turn_seq.fetch_add(1, Ordering::AcqRel) + 1;
        *active = Some(id);
        Some(id)
    }

    /// Takes the reserved turn id, if any.
    pub(crate) fn take_active_turn(&self) -> Option<u64> {
        self.active_turn.lock().expect("remote turn poisoned").take()
    }

    /// The reserved turn id, if any.
    pub(crate) fn active_turn(&self) -> Option<u64> {
        *self.active_turn.lock().expect("remote turn poisoned")
    }

    fn fallback_turn_id(&self) -> u64 {
        self.turn_seq.fetch_add(1, Ordering::AcqRel) + 1
    }
}

pub(crate) struct RemoteHostInner {
    pub(crate) config: RemoteHostConfig,
    pub(crate) manager: AgentManager,
    pub(crate) events: Mutex<Option<mpsc::UnboundedReceiver<AgentEvent>>>,
    pub(crate) sessions: Mutex<BTreeMap<String, Arc<RemoteSession>>>,
    pub(crate) bus: EventBus,
    pub(crate) approvals: Arc<RemoteApprovals>,
    pub(crate) redactor: Redactor,
    pub(crate) history: SessionLog,
    started: AtomicBool,
}

/// A headless remote-control host.
///
/// Cloning shares the same host state; every surface (stdio, WebSocket, MCP)
/// must go through one instance so approvals and cursors stay coherent.
#[derive(Clone)]
pub struct RemoteHost {
    pub(crate) inner: Arc<RemoteHostInner>,
}

impl std::fmt::Debug for RemoteHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteHost")
            .field("config", &self.inner.config)
            .field("live_connections", &self.inner.manager.live_connection_count())
            .field("sessions", &self.session_count())
            .field("subscribers", &self.inner.bus.subscriber_count())
            .finish_non_exhaustive()
    }
}

impl RemoteHost {
    /// Creates a host over the resolved config.
    ///
    /// No subprocess is started here. The event receiver is retained for the
    /// notification pump so no event is lost between construction and serve.
    #[must_use]
    pub fn new(config: RemoteHostConfig, handler: Arc<dyn ClientRequestHandler>) -> Self {
        Self::with_options(config, handler, AgentConnectionOptions::default())
    }

    /// Creates a host with explicit connection tuning (tests).
    #[must_use]
    pub fn with_options(
        config: RemoteHostConfig,
        handler: Arc<dyn ClientRequestHandler>,
        options: AgentConnectionOptions,
    ) -> Self {
        let (redactor, bus, approvals) = Self::services(&config);
        Self::assemble(config, handler, options, bus, approvals, redactor)
    }

    /// Builds a host whose inbound file/terminal requests go through the
    /// headless handler, sharing the same approval registry as the remote
    /// surface.
    ///
    /// # Errors
    ///
    /// Returns the config validation error when no agent is configured.
    pub fn headless(config: RemoteHostConfig) -> Result<(Self, Arc<HeadlessHandler>), RemoteError> {
        Self::headless_with_options(config, AgentConnectionOptions::default())
    }

    /// Headless variant with explicit connection tuning (tests).
    ///
    /// # Errors
    ///
    /// Returns the config validation error when no agent is configured.
    pub fn headless_with_options(
        config: RemoteHostConfig,
        options: AgentConnectionOptions,
    ) -> Result<(Self, Arc<HeadlessHandler>), RemoteError> {
        config.validate()?;
        let (redactor, bus, approvals) = Self::services(&config);
        let handler =
            Arc::new(HeadlessHandler::new(config.allowed_roots.clone(), approvals.clone()));
        Ok((Self::assemble(config, handler.clone(), options, bus, approvals, redactor), handler))
    }

    /// Builds the shared services once so the bus, approvals, and every
    /// surface use the same redactor seeds.
    fn services(config: &RemoteHostConfig) -> (Redactor, EventBus, Arc<RemoteApprovals>) {
        let redactor = Redactor::new(config.secret_values.clone());
        let bus = EventBus::with_redactor(
            config.ring_capacity,
            config.max_outbound_queue,
            redactor.clone(),
        );
        let audit = AuditLog::new(config.audit_path.clone(), config.audit_max_bytes);
        let approvals = Arc::new(RemoteApprovals::with_redactor(
            config.permission_mode,
            config.approval_timeout,
            audit,
            bus.clone(),
            redactor.clone(),
        ));
        (redactor, bus, approvals)
    }

    fn assemble(
        config: RemoteHostConfig,
        handler: Arc<dyn ClientRequestHandler>,
        options: AgentConnectionOptions,
        bus: EventBus,
        approvals: Arc<RemoteApprovals>,
        redactor: Redactor,
    ) -> Self {
        let history = config
            .session_log_dir
            .clone()
            .map(SessionLog::new)
            .unwrap_or_else(SessionLog::disabled);
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let manager =
            AgentManager::with_options(config.manager_config(), handler, events_tx, options);
        Self {
            inner: Arc::new(RemoteHostInner {
                config,
                manager,
                events: Mutex::new(Some(events_rx)),
                sessions: Mutex::new(BTreeMap::new()),
                bus,
                approvals,
                redactor,
                history,
                started: AtomicBool::new(false),
            }),
        }
    }

    /// Creates a host that fails every inbound agent request closed.
    #[must_use]
    pub fn deny_all(config: RemoteHostConfig) -> Self {
        Self::new(config, Arc::new(DenyAllHandler))
    }

    /// The resolved config.
    #[must_use]
    pub fn config(&self) -> &RemoteHostConfig {
        &self.inner.config
    }

    /// The underlying manager (sessions start lazily through it).
    #[must_use]
    pub fn manager(&self) -> &AgentManager {
        &self.inner.manager
    }

    /// The notification bus.
    #[must_use]
    pub fn bus(&self) -> &EventBus {
        &self.inner.bus
    }

    /// The redactor seeded with configured secret values.
    #[must_use]
    pub(crate) fn redactor(&self) -> &Redactor {
        &self.inner.redactor
    }

    /// The shared approval registry.
    #[must_use]
    pub fn approvals(&self) -> &RemoteApprovals {
        &self.inner.approvals
    }

    /// Durable session history.
    #[must_use]
    pub fn history(&self) -> &SessionLog {
        &self.inner.history
    }

    /// Takes the agent event receiver exactly once.
    ///
    /// The notification pump owns it; calling this twice returns `None`.
    #[must_use]
    pub fn take_events(&self) -> Option<mpsc::UnboundedReceiver<AgentEvent>> {
        self.inner.events.lock().expect("remote event receiver poisoned").take()
    }

    /// Starts the event pump. Idempotent.
    ///
    /// # Errors
    ///
    /// Returns `internal` when called outside a Tokio runtime.
    pub fn start(&self) -> Result<(), RemoteError> {
        if self.inner.started.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let Some(receiver) = self.take_events() else {
            return Ok(());
        };
        let host = self.clone();
        tokio::runtime::Handle::try_current()
            .map_err(|error| {
                RemoteError::internal(format!(
                    "remote host start requires a Tokio runtime: {error}"
                ))
            })?
            .spawn(async move { host.pump_events(receiver).await });
        Ok(())
    }

    /// Loaded sessions.
    #[must_use]
    pub fn sessions_snapshot(&self) -> Vec<Arc<RemoteSession>> {
        self.inner.sessions.lock().expect("remote sessions poisoned").values().cloned().collect()
    }

    /// Number of loaded sessions.
    #[must_use]
    pub fn session_count(&self) -> usize {
        self.inner.sessions.lock().expect("remote sessions poisoned").len()
    }

    /// Looks up one loaded session.
    ///
    /// # Errors
    ///
    /// `not_found` for unknown session ids.
    pub fn session(&self, session_id: &str) -> Result<Arc<RemoteSession>, RemoteError> {
        self.inner
            .sessions
            .lock()
            .expect("remote sessions poisoned")
            .get(session_id)
            .cloned()
            .ok_or_else(|| RemoteError::not_found(format!("session {session_id:?}")))
    }

    pub(crate) fn register_session(
        &self,
        agent_id: String,
        thread: AgentThread,
        cwd: PathBuf,
    ) -> Arc<RemoteSession> {
        let key = thread.session_id().0.to_string();
        self.inner.bus.ensure_stream(&key);
        let session = Arc::new(RemoteSession::new(agent_id, thread, cwd));
        self.inner
            .sessions
            .lock()
            .expect("remote sessions poisoned")
            .insert(key, Arc::clone(&session));
        session
    }

    pub(crate) fn remove_session(&self, session_id: &str) -> Option<Arc<RemoteSession>> {
        self.inner.sessions.lock().expect("remote sessions poisoned").remove(session_id)
    }

    /// Resolves the agent id for a request.
    ///
    /// # Errors
    ///
    /// `not_found` when an explicit id is unknown, `invalid_params` when no
    /// default can be resolved.
    pub(crate) fn resolve_agent(&self, requested: Option<&str>) -> Result<String, RemoteError> {
        if let Some(agent_id) = requested {
            if self.inner.manager.has_agent(agent_id) {
                return Ok(agent_id.to_string());
            }
            return Err(RemoteError::not_found(format!("agent {agent_id:?}")));
        }
        self.inner
            .manager
            .resolve_default_agent(self.inner.config.default_agent.as_deref())
            .ok_or_else(|| {
                RemoteError::invalid_params(
                    "no agent id given and no unambiguous default agent is configured",
                )
            })
    }

    /// Resolves a requested working directory or the first allowed root.
    ///
    /// # Errors
    ///
    /// `invalid_params` for relative/missing paths, `internal` when no cwd can
    /// be resolved at all.
    pub(crate) fn resolve_cwd(&self, requested: Option<&str>) -> Result<PathBuf, RemoteError> {
        let path = match requested {
            Some(path) => PathBuf::from(path),
            None => match self.inner.config.allowed_roots.first() {
                Some(root) => root.clone(),
                None => std::env::current_dir()
                    .map_err(|error| RemoteError::internal(format!("no cwd: {error}")))?,
            },
        };
        if !path.is_absolute() {
            return Err(RemoteError::invalid_params(format!(
                "cwd must be absolute, got {}",
                path.display()
            )));
        }
        let canonical = std::fs::canonicalize(&path).map_err(|error| {
            RemoteError::invalid_params(format!("cwd {}: {error}", path.display()))
        })?;
        if !canonical.is_dir() {
            return Err(RemoteError::invalid_params(format!(
                "cwd {} is not a directory",
                canonical.display()
            )));
        }
        Ok(canonical)
    }

    /// Cancels running turns, denies pending approvals, and closes every agent
    /// connection. Idempotent.
    pub async fn shutdown(&self) {
        self.inner.approvals.cancel_all();
        let sessions = self.sessions_snapshot();
        for session in sessions {
            if session.thread.is_turn_running() {
                let _ = session.thread.cancel().await;
            }
        }
        self.inner.manager.shutdown().await;
    }

    async fn pump_events(&self, mut receiver: mpsc::UnboundedReceiver<AgentEvent>) {
        while let Some(event) = receiver.recv().await {
            self.handle_agent_event(event);
        }
    }

    fn handle_agent_event(&self, event: AgentEvent) {
        match event {
            AgentEvent::SessionUpdate { session_id, update } => {
                self.publish_session_update(&session_id, *update);
            }
            AgentEvent::TurnStarted { session_id, turn } => {
                self.record_host_turn(&session_id, &turn);
            }
            AgentEvent::TurnCompleted { session_id, stop_reason, .. } => {
                self.complete_turn(&session_id, TurnStatus::Completed, Some(stop_reason), None);
            }
            AgentEvent::TurnCancelled { session_id, .. } => {
                self.complete_turn(&session_id, TurnStatus::Cancelled, None, None);
            }
            AgentEvent::TurnFailed { session_id, error, .. } => {
                let failure = RemoteError::from(error);
                self.complete_turn(
                    &session_id,
                    TurnStatus::Failed,
                    None,
                    Some(RemoteFailure {
                        class: failure.code().as_str().to_string(),
                        message: failure.message().to_string(),
                    }),
                );
            }
            AgentEvent::TurnPausedRecoverable { session_id, recoverable, .. } => {
                self.complete_turn(
                    &session_id,
                    TurnStatus::PausedRecoverable,
                    None,
                    Some(RemoteFailure {
                        class: String::from("recoverable"),
                        message: format!("{}: {}", recoverable.fault, recoverable.detail),
                    }),
                );
            }
            AgentEvent::ThreadCreated { agent_id, session_id } => {
                self.inner.bus.ensure_stream(&session_id.0);
                self.publish_status(StatusChangedNotification {
                    cursor: 0,
                    agent_id: Some(agent_id),
                    session_id: Some(session_id.0.to_string()),
                    state: StatusState::Ready,
                    detail: None,
                });
            }
            AgentEvent::ThreadClosed { agent_id, session_id, reason } => {
                self.publish_status(StatusChangedNotification {
                    cursor: 0,
                    agent_id: Some(agent_id),
                    session_id: Some(session_id.0.to_string()),
                    state: StatusState::Closed,
                    detail: Some(close_reason(&reason).to_string()),
                });
            }
            AgentEvent::ConnectionStateChanged { agent_id, state } => {
                let (state, detail) = connection_state(&state);
                self.publish_status(StatusChangedNotification {
                    cursor: 0,
                    agent_id: Some(agent_id),
                    session_id: None,
                    state,
                    detail,
                });
            }
            AgentEvent::PermissionRequested { session_id, request } => {
                if let Ok(session) = self.session(&session_id.0) {
                    self.inner.approvals.register_acp(session.thread.clone(), *request);
                }
            }
            AgentEvent::PermissionResolved { request_id, outcome, .. } => {
                self.inner.approvals.settle_acp(request_id, &outcome);
            }
            AgentEvent::PermissionAutoResolved { session_id, tool_call, outcome } => {
                self.inner.approvals.record_auto(&session_id.0, &tool_call, &outcome);
            }
            // Queueing diagnostics, evidence, stderr, plugin status, and
            // elicitation events have no v1 remote notification.
            AgentEvent::TurnQueued { .. }
            | AgentEvent::TurnDispatched { .. }
            | AgentEvent::TurnEvidenceUpdated { .. }
            | AgentEvent::ElicitationCompleted { .. }
            | AgentEvent::ClientRequestDispatched { .. }
            | AgentEvent::StderrLine { .. }
            | AgentEvent::PluginStatusChanged { .. } => {}
        }
    }

    fn record_host_turn(&self, session_id: &SessionId, _turn: &TurnKey) {
        // The remote turn id was reserved synchronously by `turn/start`; the
        // host turn key is not part of the remote contract. Keep the stream
        // alive for sessions created before the pump observed them.
        self.inner.bus.ensure_stream(&session_id.0);
    }

    fn publish_session_update(
        &self,
        session_id: &SessionId,
        update: ee_agent_protocol::SessionUpdate,
    ) {
        let key = session_id.0.to_string();
        let params_key = key.clone();
        self.inner.bus.publish_with(&key, crate::protocol::SESSION_UPDATE, move |cursor| {
            serde_json::to_value(SessionUpdateNotification {
                session_id: params_key.clone(),
                cursor,
                update,
            })
            .unwrap_or(serde_json::Value::Null)
        });
    }

    fn complete_turn(
        &self,
        session_id: &SessionId,
        status: TurnStatus,
        stop_reason: Option<StopReason>,
        error: Option<RemoteFailure>,
    ) {
        let Ok(session) = self.session(&session_id.0) else {
            return;
        };
        let turn_id = session.take_active_turn().unwrap_or_else(|| session.fallback_turn_id());
        let key = session_id.0.to_string();
        let entry = entry_for_turn(
            &key,
            turn_id,
            turn_status_label(status),
            stop_reason.map(stop_reason_label),
            Some(&session.thread.snapshot()),
            self.redactor(),
        );
        if let Err(error) = self.inner.history.record(&entry) {
            tracing::warn!(%error, session_id = %key, "session history append failed");
        }
        let params_key = key.clone();
        self.inner.bus.publish_with(&key, crate::protocol::TURN_COMPLETED, move |cursor| {
            json!(TurnCompletedNotification {
                session_id: params_key.clone(),
                turn_id,
                cursor,
                status,
                stop_reason,
                error,
            })
        });
    }

    fn publish_status(&self, status: StatusChangedNotification) {
        let _ = self.inner.bus.publish_status(status);
    }
}

fn stop_reason_label(reason: StopReason) -> String {
    serde_json::to_value(reason)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| String::from("unknown"))
}

fn turn_status_label(status: TurnStatus) -> &'static str {
    match status {
        TurnStatus::Completed => "completed",
        TurnStatus::Cancelled => "cancelled",
        TurnStatus::Failed => "failed",
        TurnStatus::PausedRecoverable => "paused_recoverable",
    }
}

fn close_reason(reason: &ThreadCloseReason) -> &'static str {
    match reason {
        ThreadCloseReason::HostClosed => "host closed the session",
        ThreadCloseReason::ConnectionLost => "agent connection lost",
    }
}

fn connection_state(state: &AgentConnectionState) -> (StatusState, Option<String>) {
    match state {
        AgentConnectionState::Starting | AgentConnectionState::Initializing => {
            (StatusState::Starting, None)
        }
        AgentConnectionState::Ready { .. } => (StatusState::Ready, None),
        AgentConnectionState::Failed(error) => (StatusState::Failed, Some(error.to_string())),
        AgentConnectionState::Closed(_) => (StatusState::Closed, None),
    }
}

/// Whether `path` is an absolute existing directory (helper for dispatch).
pub(crate) fn canonical_directory(path: &Path) -> Result<PathBuf, RemoteError> {
    if !path.is_absolute() {
        return Err(RemoteError::invalid_params(format!(
            "path must be absolute, got {}",
            path.display()
        )));
    }
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| RemoteError::invalid_params(format!("{}: {error}", path.display())))?;
    if !canonical.is_dir() {
        return Err(RemoteError::invalid_params(format!(
            "{} is not a directory",
            canonical.display()
        )));
    }
    Ok(canonical)
}

#[cfg(all(test, feature = "test-utils"))]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ee_agent_host::fake::{FakeAgent, FakeAgentScript, FakeAgentTransport};
    use ee_agent_host::{AgentProcessConfig, FakeTransportFactory};

    use super::*;

    #[derive(Debug)]
    struct CountingFactory {
        script: FakeAgentScript,
        built: AtomicUsize,
    }

    impl FakeTransportFactory for CountingFactory {
        fn build(&self) -> FakeAgentTransport {
            self.built.fetch_add(1, Ordering::AcqRel);
            let (_agent, transport) = FakeAgent::spawn(self.script.clone());
            transport
        }
    }

    fn config_with_factory(script: FakeAgentScript) -> (RemoteHostConfig, Arc<CountingFactory>) {
        let factory = Arc::new(CountingFactory { script, built: AtomicUsize::new(0) });
        let mut config = RemoteHostConfig::default();
        config.agents.insert("fake".into(), AgentProcessConfig::new("unused"));
        config.fake_transports.insert("fake".into(), factory.clone());
        (config, factory)
    }

    #[test]
    fn smoke_test_builds_a_manager_config_with_a_fake_transport() {
        let (config, factory) = config_with_factory(FakeAgentScript::new());
        let host = RemoteHost::deny_all(config);

        assert_eq!(host.manager().agent_ids(), vec!["fake".to_string()]);
        assert!(host.manager().has_agent("fake"));
        assert_eq!(host.manager().live_connection_count(), 0);
        assert_eq!(factory.built.load(Ordering::Acquire), 0, "connections stay lazy");
        assert!(host.take_events().is_some());
        assert!(host.take_events().is_none(), "the event receiver is taken once");
    }

    #[test]
    fn unknown_sessions_are_typed_not_found() {
        let host = RemoteHost::deny_all(RemoteHostConfig::default());
        let error = host.session("missing").expect_err("missing");
        assert_eq!(error.code(), crate::error::RemoteErrorCode::NotFound);
    }

    #[test]
    fn resolve_agent_prefers_explicit_then_singleton() {
        let mut config = RemoteHostConfig::default();
        config.agents.insert("a".into(), AgentProcessConfig::new("a"));
        config.agents.insert("b".into(), AgentProcessConfig::new("b"));
        let host = RemoteHost::deny_all(config);
        assert_eq!(host.resolve_agent(Some("b")).expect("explicit"), "b");
        assert!(host.resolve_agent(None).is_err(), "two agents are ambiguous");
        assert!(host.resolve_agent(Some("c")).is_err());
    }
}
