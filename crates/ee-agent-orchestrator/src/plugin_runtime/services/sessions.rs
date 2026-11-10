//! `agent.sessions@1`: session/turn scopes, cancellation, and checkpoints.
//!
//! A session scope owns child plugin scopes. Turn scopes are children of the
//! session scope; dropping a turn scope disposes its contributions immediately,
//! and closing a session disposes every remaining child in reverse order.
//!
//! Session data is owner-scoped: this service exposes lifecycle metadata and a
//! plugin scope handle, never prompt text or tool output.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ee_plugin_api::context::PluginContext;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::event::EventBus;
use ee_plugin_api::ids::ScopeId;
use ee_plugin_api::service::Service;
use serde::{Deserialize, Serialize};

use crate::plugin_runtime::events::{AgentSessionClosed, AgentSessionCreated};

/// Maximum sessions tracked at once.
pub const MAX_SESSIONS: usize = 64;
/// Maximum retained checkpoints per session.
pub const MAX_SESSION_CHECKPOINTS: usize = 8;

/// Owner-visible session state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Active,
    Cancelled,
    Closed,
}

struct SessionRecord {
    context: Option<PluginContext>,
    state: SessionState,
    cancelled: Arc<AtomicBool>,
    checkpoints: Vec<String>,
    turn_counter: AtomicU64,
}

/// One live plugin session scope.
#[derive(Clone)]
pub struct SessionHandle {
    session_id: String,
    record: Arc<Mutex<SessionRecord>>,
    sessions: Arc<Mutex<BTreeMap<String, Arc<Mutex<SessionRecord>>>>>,
    events: EventBus,
}

impl SessionHandle {
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Clone of the session's plugin scope, when the session is still live.
    #[must_use]
    pub fn context(&self) -> Option<PluginContext> {
        self.record.lock().expect("session record lock poisoned").context.clone()
    }

    #[must_use]
    pub fn state(&self) -> SessionState {
        self.record.lock().expect("session record lock poisoned").state
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.record.lock().expect("session record lock poisoned").cancelled.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn checkpoints(&self) -> Vec<String> {
        self.record.lock().expect("session record lock poisoned").checkpoints.clone()
    }

    /// Cancels the session; later turn scopes observe the cancellation flag.
    pub fn cancel(&self) {
        let mut record = self.record.lock().expect("session record lock poisoned");
        if record.state == SessionState::Active {
            record.state = SessionState::Cancelled;
            record.cancelled.store(true, Ordering::SeqCst);
        }
    }

    /// Records a checkpoint id; bounded to the most recent entries.
    pub fn checkpoint(&self, checkpoint_id: impl Into<String>) -> Result<(), PluginError> {
        let mut record = self.record.lock().expect("session record lock poisoned");
        if record.state == SessionState::Closed {
            return Err(PluginError::Runtime {
                message: format!("session `{}` is closed", self.session_id),
            });
        }
        record.checkpoints.push(checkpoint_id.into());
        while record.checkpoints.len() > MAX_SESSION_CHECKPOINTS {
            record.checkpoints.remove(0);
        }
        Ok(())
    }

    /// Next monotonic turn number for this session.
    pub fn next_turn(&self) -> u64 {
        self.record
            .lock()
            .expect("session record lock poisoned")
            .turn_counter
            .fetch_add(1, Ordering::SeqCst)
            + 1
    }

    /// Creates a child scope for one prompt turn.
    pub fn turn_scope(&self, turn: u64) -> Result<TurnScope, PluginError> {
        let record = self.record.lock().expect("session record lock poisoned");
        if record.state == SessionState::Closed {
            return Err(PluginError::Runtime {
                message: format!("session `{}` is closed", self.session_id),
            });
        }
        let parent = record.context.as_ref().ok_or_else(|| PluginError::Runtime {
            message: format!("session `{}` has no live scope", self.session_id),
        })?;
        let scope = ScopeId::new(format!("agent.turn.{}.{}", self.session_id, turn))?;
        let context = parent.child_scope(scope);
        Ok(TurnScope {
            context,
            session_id: self.session_id.clone(),
            turn,
            cancelled: Arc::clone(&record.cancelled),
        })
    }

    /// Closes the session and disposes its scopes.
    pub fn close(&self, reason: impl Into<String>) {
        let reason = reason.into();
        let removed =
            self.sessions.lock().expect("session registry lock poisoned").remove(&self.session_id);
        if let Some(record) = removed {
            let context = {
                let mut record = record.lock().expect("session record lock poisoned");
                if record.state == SessionState::Closed {
                    return;
                }
                record.state = SessionState::Closed;
                record.cancelled.store(true, Ordering::SeqCst);
                record.context.take()
            };
            // Disposing the scope revokes every child registration LIFO, even
            // while callers still hold clones of the session context.
            if let Some(context) = context {
                context.dispose();
            }
            let _ = self
                .events
                .emit(&AgentSessionClosed { session_id: self.session_id.clone(), reason });
        }
    }
}

impl std::fmt::Debug for SessionHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionHandle")
            .field("session_id", &self.session_id)
            .field("state", &self.state())
            .finish()
    }
}

/// Child scope for one prompt turn.
pub struct TurnScope {
    context: PluginContext,
    session_id: String,
    turn: u64,
    cancelled: Arc<AtomicBool>,
}

impl Drop for TurnScope {
    fn drop(&mut self) {
        self.context.dispose();
    }
}

impl TurnScope {
    #[must_use]
    pub fn context(&self) -> &PluginContext {
        &self.context
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    #[must_use]
    pub fn turn(&self) -> u64 {
        self.turn
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Ends the turn, disposing every turn-scoped registration.
    pub fn finish(self) {
        drop(self);
    }
}

impl std::fmt::Debug for TurnScope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TurnScope")
            .field("session_id", &self.session_id)
            .field("turn", &self.turn)
            .finish()
    }
}

/// Session registry service.
pub struct SessionService {
    sessions: Arc<Mutex<BTreeMap<String, Arc<Mutex<SessionRecord>>>>>,
    events: EventBus,
    max_sessions: usize,
}

impl Service for SessionService {
    const ID: &'static str = "agent.sessions@1";
}

impl Default for SessionService {
    fn default() -> Self {
        Self::new(EventBus::new())
    }
}

impl SessionService {
    #[must_use]
    pub fn new(events: EventBus) -> Self {
        Self { sessions: Arc::new(Mutex::new(BTreeMap::new())), events, max_sessions: MAX_SESSIONS }
    }

    /// Creates a session scope under `parent`.
    pub fn create(
        &self,
        parent: &PluginContext,
        session_id: impl Into<String>,
    ) -> Result<SessionHandle, PluginError> {
        let session_id = session_id.into();
        if session_id.is_empty() {
            return Err(PluginError::Runtime {
                message: "session id must not be empty".to_owned(),
            });
        }
        let mut sessions = self.sessions.lock().expect("session registry lock poisoned");
        if sessions.contains_key(&session_id) {
            return Err(PluginError::Runtime {
                message: format!("duplicate agent plugin session `{session_id}`"),
            });
        }
        if sessions.len() >= self.max_sessions {
            return Err(PluginError::Resource {
                message: format!("session limit of {} reached", self.max_sessions),
            });
        }
        let scope = ScopeId::new(format!("agent.session.{session_id}"))?;
        let context = parent.child_scope(scope);
        let record = Arc::new(Mutex::new(SessionRecord {
            context: Some(context),
            state: SessionState::Active,
            cancelled: Arc::new(AtomicBool::new(false)),
            checkpoints: Vec::new(),
            turn_counter: AtomicU64::new(1),
        }));
        sessions.insert(session_id.clone(), Arc::clone(&record));
        drop(sessions);
        let _ = self.events.emit(&AgentSessionCreated { session_id: session_id.clone() });
        Ok(SessionHandle {
            session_id,
            record,
            sessions: Arc::clone(&self.sessions),
            events: self.events.clone(),
        })
    }

    /// Looks up a live session.
    #[must_use]
    pub fn session(&self, session_id: &str) -> Option<SessionHandle> {
        let sessions = self.sessions.lock().expect("session registry lock poisoned");
        let record = sessions.get(session_id)?;
        Some(SessionHandle {
            session_id: session_id.to_owned(),
            record: Arc::clone(record),
            sessions: Arc::clone(&self.sessions),
            events: self.events.clone(),
        })
    }

    /// Sorted live session ids.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.sessions.lock().expect("session registry lock poisoned").keys().cloned().collect()
    }

    /// Closes every session, disposing all scopes.
    pub fn close_all(&self, reason: &str) {
        let ids = self.ids();
        for id in ids {
            if let Some(session) = self.session(&id) {
                session.close(reason);
            }
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.sessions.lock().expect("session registry lock poisoned").len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for SessionService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionService")
            .field("sessions", &self.ids())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    #![allow(unused_must_use)]
    use super::*;
    use ee_plugin_api::service::ServiceKey;

    struct ScopedService;

    impl ee_plugin_api::service::Service for ScopedService {
        const ID: &'static str = "test.session.scoped@1";
    }

    fn parent() -> PluginContext {
        PluginContext::new("agent-host")
    }

    #[test]
    fn creates_and_closes_sessions() {
        let service = SessionService::default();
        let handle = service.create(&parent(), "session-1").expect("creates");
        assert_eq!(handle.state(), SessionState::Active);
        assert_eq!(service.ids(), vec!["session-1".to_owned()]);
        handle.close("done");
        assert_eq!(handle.state(), SessionState::Closed);
        assert!(service.is_empty());
    }

    #[test]
    fn cancel_sets_the_flag_and_turn_observes_it() {
        let service = SessionService::default();
        let handle = service.create(&parent(), "session-1").expect("creates");
        let turn = handle.turn_scope(1).expect("turn");
        assert!(!turn.is_cancelled());
        handle.cancel();
        assert!(handle.is_cancelled());
        assert!(turn.is_cancelled());
    }

    #[test]
    fn checkpoints_are_bounded() {
        let service = SessionService::default();
        let handle = service.create(&parent(), "session-1").expect("creates");
        for index in 0..(MAX_SESSION_CHECKPOINTS + 2) {
            handle.checkpoint(format!("cp-{index}")).expect("checkpoint");
        }
        let checkpoints = handle.checkpoints();
        assert_eq!(checkpoints.len(), MAX_SESSION_CHECKPOINTS);
        assert_eq!(checkpoints.first().map(String::as_str), Some("cp-2"));
    }

    #[test]
    fn session_scope_disposes_registered_services() {
        let service = SessionService::default();
        let root = parent();
        let handle = service.create(&root, "session-1").expect("creates");
        let context = handle.context().expect("active scope");
        let scoped = Arc::new(ScopedService);
        context.register_service(Arc::clone(&scoped)).expect("registers");
        assert!(root.service::<ScopedService>().is_some());
        handle.close("done");
        assert!(root.service::<ScopedService>().is_none());
    }

    #[test]
    fn turn_scope_disposes_on_finish() {
        let service = SessionService::default();
        let root = parent();
        let handle = service.create(&root, "session-1").expect("creates");
        let turn = handle.turn_scope(1).expect("turn");
        let scoped = Arc::new(ScopedService);
        turn.context().register_service(Arc::clone(&scoped)).expect("registers");
        assert!(root.service::<ScopedService>().is_some());
        turn.finish();
        assert!(root.service::<ScopedService>().is_none());
    }

    #[test]
    fn duplicate_and_closed_sessions_are_rejected() {
        let service = SessionService::default();
        let root = parent();
        let handle = service.create(&root, "session-1").expect("creates");
        assert!(service.create(&root, "session-1").is_err());
        handle.close("done");
        assert!(handle.turn_scope(1).is_err());
        handle.checkpoint("cp").expect_err("closed session rejects checkpoint");
    }

    #[test]
    fn key_is_versioned() {
        let key = ServiceKey::<SessionService>::new();
        assert_eq!(key.id().as_str(), SessionService::ID);
        assert_eq!(key.id().as_str(), "agent.sessions@1");
    }
}
