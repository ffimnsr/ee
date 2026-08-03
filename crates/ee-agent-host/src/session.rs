//! Session threads: one handle per ACP session, owning the prompt turn
//! lifecycle, mode switching, and the reduced session state.
//!
//! A thread is the unit the UI talks to: submit prompts, cancel turns,
//! switch modes, answer permission requests, and read deterministic
//! [`SessionState`] snapshots.  All wire traffic happens on the owning
//! [`AgentConnection`] driver; threads never touch JSON-RPC directly.

use std::sync::{Arc, Mutex};

use ee_agent_protocol::{
    ContentBlock, PromptResponse, RequestPermissionOutcome, SessionId, SessionModeId,
    SessionModeState, SessionUpdate,
};
use tokio::sync::{mpsc, watch};

use crate::connection::AgentConnection;
use crate::error::AgentError;
use crate::events::{AgentEvent, ConnectionCloseReason, PermissionRequestId, ThreadCloseReason};
use crate::reducer::{MessageKind, ReducedMessage, SessionState, apply_update};

/// Shared per-session state; the connection routes `session/update`
/// notifications here and the thread exposes snapshots to the UI.
pub(crate) struct ThreadShared {
    pub agent_id: String,
    pub session_id: SessionId,
    pub state: Mutex<SessionState>,
    pub order: Mutex<ee_agent_protocol::SessionUpdateOrder>,
    pub turn: Mutex<Option<watch::Sender<bool>>>,
    pub modes: Mutex<Option<SessionModeState>>,
    pub events: mpsc::UnboundedSender<AgentEvent>,
}

impl ThreadShared {
    /// Applies one `session/update` notification: ordering check, reduction,
    /// and a deterministic [`AgentEvent::SessionUpdate`].
    pub fn apply_update(&self, update: SessionUpdate) {
        let mut state = self.state.lock().expect("session state poisoned");
        let mut order = self.order.lock().expect("session order poisoned");
        let result = apply_update(&mut state, &mut order, &update);
        drop(state);
        let event = match result {
            Ok(()) => AgentEvent::SessionUpdate {
                session_id: self.session_id.clone(),
                update: Box::new(update),
            },
            Err(error) => {
                tracing::warn!(
                    session_id = %self.session_id.0,
                    ?error,
                    "invalid session update ignored"
                );
                return;
            }
        };
        let _ = self.events.send(event);
    }

    /// Called by the connection when it goes away: clears the running turn
    /// (the driver resolves the prompt with a connection-closed error) and
    /// reports the thread as closed.
    pub fn notify_connection_lost(&self, reason: ConnectionCloseReason) {
        // The driver's prompt select resolves the in-flight turn with
        // `ConnectionClosed` (terminate arm); `send_prompt` owns the
        // TurnFailed/TurnCancelled event, so only clear the local marker.
        *self.turn.lock().expect("turn state poisoned") = None;
        let _ = self.events.send(AgentEvent::ThreadClosed {
            agent_id: self.agent_id.clone(),
            session_id: self.session_id.clone(),
            reason: if matches!(reason, ConnectionCloseReason::Closed) {
                ThreadCloseReason::HostClosed
            } else {
                ThreadCloseReason::ConnectionLost
            },
        });
    }
}

/// A handle to one ACP session thread.
#[derive(Clone)]
pub struct AgentThread {
    pub(crate) agent_id: String,
    pub(crate) session_id: SessionId,
    pub(crate) connection: AgentConnection,
    pub(crate) shared: Arc<ThreadShared>,
}

impl std::fmt::Debug for AgentThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentThread")
            .field("agent_id", &self.agent_id)
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl AgentThread {
    /// Creates a thread for a fresh or loaded session (host-internal).
    #[must_use]
    pub(crate) fn new(
        agent_id: String,
        session_id: SessionId,
        modes: Option<SessionModeState>,
        connection: AgentConnection,
    ) -> Self {
        let shared = Arc::new(ThreadShared {
            agent_id: agent_id.clone(),
            session_id: session_id.clone(),
            state: Mutex::new(SessionState::default()),
            order: Mutex::new(ee_agent_protocol::SessionUpdateOrder::new()),
            turn: Mutex::new(None),
            modes: Mutex::new(modes),
            events: connection.inner.events.clone(),
        });
        Self { agent_id, session_id, connection, shared }
    }

    /// The owning agent id.
    #[must_use]
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// The ACP session id.
    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Whether a prompt turn is currently running.
    #[must_use]
    pub fn is_turn_running(&self) -> bool {
        self.shared.turn.lock().expect("turn state poisoned").is_some()
    }

    /// A deterministic snapshot of the reduced session state.
    #[must_use]
    pub fn snapshot(&self) -> SessionState {
        self.shared.state.lock().expect("session state poisoned").clone()
    }

    /// The modes the agent advertised at session creation, if any.
    #[must_use]
    pub fn advertised_modes(&self) -> Option<SessionModeState> {
        self.shared.modes.lock().expect("modes poisoned").clone()
    }

    /// Sends `session/prompt` and streams the turn to completion.
    ///
    /// Appends the optimistic user message first, then dispatches the
    /// request; `session/update` notifications are reduced as they arrive.
    /// The future resolves with [`PromptResponse`] or a typed error.
    ///
    /// # Errors
    ///
    /// Fails when another turn is running, the connection is gone, the turn
    /// is cancelled, or the agent rejects the prompt.
    pub async fn send_prompt(
        &self,
        prompt: Vec<ContentBlock>,
    ) -> Result<PromptResponse, AgentError> {
        {
            let mut state = self.shared.state.lock().expect("session state poisoned");
            state.messages.push(ReducedMessage {
                kind: MessageKind::User,
                message_id: None,
                blocks: prompt.clone(),
            });
        }
        let _ = self
            .shared
            .events
            .send(AgentEvent::TurnStarted { session_id: self.session_id.clone() });

        let (cancel_tx, cancel_rx) = watch::channel(false);
        {
            let mut turn = self.shared.turn.lock().expect("turn state poisoned");
            if turn.is_some() {
                return Err(AgentError::TurnAlreadyRunning);
            }
            *turn = Some(cancel_tx);
        }

        let result = self.connection.send_prompt(self.session_id.clone(), prompt, cancel_rx).await;
        self.finish_turn();

        match &result {
            Ok(response) => {
                let _ = self.shared.events.send(AgentEvent::TurnCompleted {
                    session_id: self.session_id.clone(),
                    stop_reason: response.stop_reason,
                });
            }
            Err(AgentError::Cancelled) => {
                let _ = self
                    .shared
                    .events
                    .send(AgentEvent::TurnCancelled { session_id: self.session_id.clone() });
            }
            Err(error) => {
                let _ = self.shared.events.send(AgentEvent::TurnFailed {
                    session_id: self.session_id.clone(),
                    error: error.clone(),
                });
            }
        }
        result
    }

    /// Cancels the running turn: sends `session/cancel`, cancels the
    /// in-flight prompt request, and resolves pending permissions as
    /// cancelled.  The running `send_prompt` future resolves with
    /// [`AgentError::Cancelled`] and emits the terminal turn event.
    ///
    /// # Errors
    ///
    /// Fails with [`AgentError::NoRunningTurn`] when nothing is running.
    pub async fn cancel(&self) -> Result<(), AgentError> {
        let cancel = {
            let mut turn = self.shared.turn.lock().expect("turn state poisoned");
            turn.take().ok_or(AgentError::NoRunningTurn)?
        };
        let _ = cancel.send(true);
        self.connection.send_session_cancel(self.session_id.clone());
        let resolved = self.connection.cancel_session_permissions(&self.session_id);
        tracing::debug!(session_id = %self.session_id.0, resolved, "cancelled pending permissions");
        Ok(())
    }

    /// Switches the session mode; allowed only when the agent advertised
    /// `availableModes` at session creation.
    ///
    /// # Errors
    ///
    /// Fails when the agent advertised no modes or rejected the request.
    pub async fn set_mode(&self, mode_id: SessionModeId) -> Result<(), AgentError> {
        let modes = self.shared.modes.lock().expect("modes poisoned").clone();
        let Some(modes) = modes else {
            return Err(AgentError::CapabilityUnsupported { method: "session/set_mode".into() });
        };
        if !modes.available_modes.iter().any(|mode| mode.id == mode_id) {
            return Err(AgentError::invalid_params(format!(
                "mode {mode_id:?} was not advertised by the agent"
            )));
        }
        self.connection.set_mode(self.session_id.clone(), mode_id).await?;
        Ok(())
    }

    /// Resolves one pending permission request.
    ///
    /// Returns `false` when the request id is unknown or already resolved
    /// (duplicate responses are ignored).
    pub fn respond_permission(
        &self,
        request_id: PermissionRequestId,
        outcome: RequestPermissionOutcome,
    ) -> bool {
        self.connection.respond_permission(request_id, outcome)
    }

    /// Closes this thread: deregisters it from the connection and emits a
    /// host-closed event.  Running turns keep running until cancelled or the
    /// connection closes (matching `:agents_close` semantics).
    pub fn close(&self) {
        self.connection.deregister_thread(&self.session_id);
        let _ = self.shared.events.send(AgentEvent::ThreadClosed {
            agent_id: self.agent_id.clone(),
            session_id: self.session_id.clone(),
            reason: ThreadCloseReason::HostClosed,
        });
    }

    /// Clears the running turn state if this thread still has one.
    fn finish_turn(&self) {
        let mut turn = self.shared.turn.lock().expect("turn state poisoned");
        *turn = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ee_agent_protocol::TextContent;

    #[test]
    fn reduced_messages_carry_blocks() {
        let mut state = SessionState::default();
        state.messages.push(ReducedMessage {
            kind: MessageKind::Assistant,
            message_id: Some("m1".into()),
            blocks: vec![ContentBlock::Text(TextContent::new("hi"))],
        });
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].blocks.len(), 1);
    }
}
