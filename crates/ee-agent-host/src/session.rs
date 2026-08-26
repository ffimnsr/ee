//! Session threads: one handle per ACP session, owning the prompt turn
//! lifecycle, mode switching, and the reduced session state.
//!
//! A thread is the unit the UI talks to: submit prompts, cancel turns,
//! switch modes, answer permission requests, and read deterministic
//! [`SessionState`] snapshots.  All wire traffic happens on the owning
//! [`AgentConnection`] driver; threads never touch JSON-RPC directly.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ee_agent_protocol::{
    ContentBlock, PromptResponse, RequestPermissionOutcome, SessionConfigId, SessionConfigKind,
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigOptionValue,
    SessionConfigSelectOptions, SessionConfigValueId, SessionId, SessionModeId, SessionModeState,
    SessionUpdate,
};
use tokio::sync::{mpsc, watch};

use crate::connection::AgentConnection;
use crate::error::AgentError;
use crate::events::{
    AgentEvent, ConnectionCloseReason, PermissionRequestId, RecoverableInfo, ThreadCloseReason,
    TurnMetrics,
};
use crate::mcp_over_acp::EeProxyMode;
use crate::reducer::{MessageKind, ReducedMessage, SessionState, apply_update};
use crate::turn_evidence::{
    PromptTerminalOutcome, TurnEvidence, TurnEvidenceError, TurnEvidenceStore, TurnEvidenceSummary,
    TurnKey, TurnObservation,
};

/// Shared per-session state; the connection routes `session/update`
/// notifications here and the thread exposes snapshots to the UI.
pub(crate) struct ThreadShared {
    pub agent_id: String,
    pub session_id: SessionId,
    pub state: Mutex<SessionState>,
    pub order: Mutex<ee_agent_protocol::SessionUpdateOrder>,
    /// Explicit reservation for the one in-flight ACP prompt. Cancellation
    /// changes its state but keeps this reservation until that exact
    /// `send_prompt` future resolves, so a replacement prompt cannot race it.
    pub turn: Mutex<Option<RunningTurn>>,
    /// Current host turn key while an ACP prompt is active. Editor-owned
    /// observations may attach only to this key; stale or proxy-only calls
    /// cannot fabricate completion evidence.
    pub active_turn: Mutex<Option<TurnKey>>,
    /// Host-owned evidence turn retained only after an agent-reported
    /// recoverable interruption, for an explicit resume request.
    pub paused_turn: Mutex<Option<TurnKey>>,
    /// When the running turn started; cleared when the turn finishes.
    pub turn_started: Mutex<Option<Instant>>,
    /// Append-only, host-owned evidence keyed by monotonic ACP turn id.
    pub evidence: Mutex<TurnEvidenceStore>,
    /// Set after the first host turn starts. Used only to gate discovery of
    /// the read-only evidence tool; summary retrieval still validates each
    /// requested session and turn.
    pub(crate) evidence_available: AtomicBool,
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

    /// Reserves this thread for a prompt and reduces its optimistic user
    /// message only after the reservation succeeds. This keeps rejected
    /// concurrent prompts out of the transcript.
    fn start_turn(
        &self,
        prompt: Vec<ContentBlock>,
        resume: bool,
    ) -> Result<StartedTurn, AgentError> {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let turn = {
            let mut active = self.turn.lock().expect("turn state poisoned");
            if active.is_some() {
                return Err(AgentError::TurnAlreadyRunning);
            }
            let turn =
                if resume {
                    self.paused_turn.lock().expect("paused turn poisoned").take().ok_or_else(
                        || AgentError::invalid_params("no recoverable turn to resume"),
                    )?
                } else {
                    self.paused_turn.lock().expect("paused turn poisoned").take();
                    self.evidence
                        .lock()
                        .expect("turn evidence poisoned")
                        .start_turn(self.agent_id.clone(), self.session_id.0.to_string())
                };
            *active = Some(RunningTurn::new(turn.clone(), cancel_tx));
            turn
        };
        if !resume {
            self.state.lock().expect("session state poisoned").messages.push(ReducedMessage {
                kind: MessageKind::User,
                message_id: None,
                blocks: prompt,
            });
        }
        *self.active_turn.lock().expect("active turn poisoned") = Some(turn.clone());
        self.evidence_available.store(true, Ordering::Release);
        Ok(StartedTurn { cancel_rx, turn })
    }

    fn observe_evidence(
        &self,
        turn_id: u64,
        observation: TurnObservation,
    ) -> Result<TurnEvidenceSummary, TurnEvidenceError> {
        let summary =
            self.evidence.lock().expect("turn evidence poisoned").observe(turn_id, observation)?;
        let _ = self.events.send(AgentEvent::TurnEvidenceUpdated {
            session_id: self.session_id.clone(),
            summary: Box::new(summary.clone()),
        });
        Ok(summary)
    }

    fn evidence_snapshot(&self, turn_id: u64) -> Option<TurnEvidence> {
        self.evidence.lock().expect("turn evidence poisoned").snapshot(turn_id)
    }

    pub(crate) fn evidence_summary(&self, turn_id: u64) -> Option<TurnEvidenceSummary> {
        self.evidence.lock().expect("turn evidence poisoned").summary(turn_id)
    }

    pub(crate) fn has_turn_evidence(&self) -> bool {
        self.evidence_available.load(Ordering::Acquire)
    }

    /// Called by the connection when it goes away: clears the running turn
    /// (the driver resolves the prompt with a connection-closed error) and
    /// reports the thread as closed.
    pub fn notify_connection_lost(&self, reason: ConnectionCloseReason) {
        // The driver's prompt select resolves the in-flight turn with
        // `ConnectionClosed` (terminate arm); `send_prompt` owns the
        // TurnFailed/TurnCancelled event, so only clear the local marker.
        *self.turn.lock().expect("turn state poisoned") = None;
        *self.active_turn.lock().expect("active turn poisoned") = None;
        *self.paused_turn.lock().expect("paused turn poisoned") = None;
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

struct StartedTurn {
    cancel_rx: watch::Receiver<bool>,
    turn: TurnKey,
}

/// Reservation state for one exact host turn.
pub(crate) struct RunningTurn {
    key: TurnKey,
    cancel: watch::Sender<bool>,
    cancelling: bool,
}

impl RunningTurn {
    fn new(key: TurnKey, cancel: watch::Sender<bool>) -> Self {
        Self { key, cancel, cancelling: false }
    }
}

impl RunningTurn {
    fn key(&self) -> &TurnKey {
        &self.key
    }

    pub(crate) fn cancel(self) -> watch::Sender<bool> {
        self.cancel
    }
}

/// A handle to one ACP session thread.
#[derive(Clone)]
pub struct AgentThread {
    pub(crate) agent_id: String,
    pub(crate) session_id: SessionId,
    pub(crate) connection: AgentConnection,
    pub(crate) shared: Arc<ThreadShared>,
    /// How the ee MCP proxy was advertised to this session (Phase 6b).
    proxy_mode: EeProxyMode,
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
        config_options: Option<Vec<SessionConfigOption>>,
        connection: AgentConnection,
        proxy_mode: EeProxyMode,
    ) -> Self {
        let mut state = SessionState::default();
        if let Some(config_options) = config_options {
            state.set_config_options(config_options);
        }
        if state.current_mode.is_none()
            && let Some(modes) = modes.as_ref()
        {
            state.current_mode = Some(modes.current_mode_id.clone());
        }
        let shared = Arc::new(ThreadShared {
            agent_id: agent_id.clone(),
            session_id: session_id.clone(),
            state: Mutex::new(state),
            order: Mutex::new(ee_agent_protocol::SessionUpdateOrder::new()),
            turn: Mutex::new(None),
            active_turn: Mutex::new(None),
            paused_turn: Mutex::new(None),
            turn_started: Mutex::new(None),
            evidence: Mutex::new(TurnEvidenceStore::default()),
            evidence_available: AtomicBool::new(false),
            modes: Mutex::new(modes),
            events: connection.inner.events.clone(),
        });
        Self { agent_id, session_id, connection, shared, proxy_mode }
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

    /// Current active host turn key, if the ACP prompt has not reached a
    /// terminal lifecycle state. Editor integrations must use this instead of
    /// guessing a turn number from transcript order.
    #[must_use]
    pub fn active_turn_key(&self) -> Option<TurnKey> {
        self.shared.active_turn.lock().expect("active turn poisoned").clone()
    }

    /// Immutable snapshot of evidence for one monotonic host turn.
    #[must_use]
    pub fn turn_evidence(&self, turn_id: u64) -> Option<TurnEvidence> {
        self.shared.evidence_snapshot(turn_id)
    }

    /// Current transport-safe terminal evidence summary for one host turn.
    #[must_use]
    pub fn turn_evidence_summary(&self, turn_id: u64) -> Option<TurnEvidenceSummary> {
        self.shared.evidence_summary(turn_id)
    }

    /// Appends one bounded editor observation to a turn's evidence ledger.
    ///
    /// Callers provide only structured revision/check data. The host sanitizes
    /// identifiers, generates evidence ids, and emits `TurnEvidenceUpdated`.
    /// This does not alter ACP transport lifecycle state.
    ///
    /// # Errors
    ///
    /// Fails when `turn_id` is not known for this thread or its bounded
    /// observation limit is exhausted.
    pub fn observe_turn_evidence(
        &self,
        turn_id: u64,
        observation: TurnObservation,
    ) -> Result<TurnEvidenceSummary, TurnEvidenceError> {
        self.shared.observe_evidence(turn_id, observation)
    }

    /// Whether this session's agent advertises additional-directory support.
    #[must_use]
    pub fn supports_additional_directories(&self) -> bool {
        self.connection.supports_additional_directories()
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

    /// The current session config options in agent-provided order.
    #[must_use]
    pub fn config_options(&self) -> Vec<SessionConfigOption> {
        self.snapshot().config_options
    }

    /// Replaces initial config options after `session/load` replay pre-registration.
    pub(crate) fn set_initial_config_options(
        &self,
        config_options: Option<Vec<SessionConfigOption>>,
    ) {
        if let Some(config_options) = config_options {
            self.shared
                .state
                .lock()
                .expect("session state poisoned")
                .set_config_options(config_options);
        }
    }

    /// How the ee MCP proxy was advertised to this session: ACP-native,
    /// stdio fallback, or disabled (Phase 6b diagnostics).
    #[must_use]
    pub fn proxy_mode(&self) -> EeProxyMode {
        self.proxy_mode
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
        self.send_prompt_inner(prompt, false).await
    }

    /// Re-sends the original prompt for an agent-reported recoverable pause.
    /// The host reuses the paused turn's evidence ledger rather than creating
    /// editor facts in the caller or a new evidence turn.
    pub async fn resume_prompt(
        &self,
        prompt: Vec<ContentBlock>,
    ) -> Result<PromptResponse, AgentError> {
        self.send_prompt_inner(prompt, true).await
    }

    async fn send_prompt_inner(
        &self,
        prompt: Vec<ContentBlock>,
        resume: bool,
    ) -> Result<PromptResponse, AgentError> {
        validate_prompt_blocks(&self.connection, &prompt)?;
        let started_turn = self.shared.start_turn(prompt.clone(), resume)?;
        *self.shared.turn_started.lock().expect("turn state poisoned") = Some(Instant::now());
        let _ = self.shared.events.send(AgentEvent::TurnStarted {
            session_id: self.session_id.clone(),
            turn: started_turn.turn.clone(),
        });

        let result = self
            .connection
            .send_prompt(self.session_id.clone(), prompt, started_turn.cancel_rx)
            .await;

        match &result {
            Ok(response) => {
                let metrics = self.take_turn_metrics(response.usage.clone());
                let _ = self.shared.events.send(AgentEvent::TurnCompleted {
                    session_id: self.session_id.clone(),
                    stop_reason: response.stop_reason,
                    metrics,
                });
                self.record_turn_terminal(&started_turn.turn, PromptTerminalOutcome::Completed);
            }
            Err(AgentError::Cancelled) => {
                let metrics = self.take_turn_metrics(None);
                let _ = self.shared.events.send(AgentEvent::TurnCancelled {
                    session_id: self.session_id.clone(),
                    metrics,
                });
                self.record_turn_terminal(&started_turn.turn, PromptTerminalOutcome::Cancelled);
            }
            Err(error) => {
                let metrics = self.take_turn_metrics(None);
                // A recoverable interruption carries structured data in the
                // JSON-RPC error; surface it as a pause, not a failure, so
                // the UI can offer Resume/Discard.
                if let AgentError::Rpc(rpc_error) = error
                    && let Some(data) = &rpc_error.data
                    && let Some(recoverable) = data.get("recoverable")
                    && let Ok(info) = serde_json::from_value::<RecoverableInfo>(recoverable.clone())
                {
                    *self.shared.paused_turn.lock().expect("paused turn poisoned") =
                        Some(started_turn.turn.clone());
                    self.record_turn_terminal(
                        &started_turn.turn,
                        PromptTerminalOutcome::PausedRecoverable,
                    );
                    // The pane may resume as soon as it receives this event.
                    // Release this attempt's reservation first so resume can
                    // reattach the same host-owned evidence turn.
                    self.finish_turn(&started_turn.turn);
                    let _ = self.shared.events.send(AgentEvent::TurnPausedRecoverable {
                        session_id: self.session_id.clone(),
                        metrics,
                        recoverable: Box::new(info),
                    });
                    return result;
                } else {
                    let _ = self.shared.events.send(AgentEvent::TurnFailed {
                        session_id: self.session_id.clone(),
                        error: error.clone(),
                        metrics,
                    });
                    self.record_turn_terminal(&started_turn.turn, PromptTerminalOutcome::Failed);
                }
            }
        }
        self.finish_turn(&started_turn.turn);
        result
    }

    /// Cancels the running turn: sends `session/cancel`, cancels the
    /// in-flight prompt request, and resolves pending permissions as
    /// cancelled. The reservation remains occupied until the running
    /// `send_prompt` future resolves, preventing a replacement prompt race.
    ///
    /// # Errors
    ///
    /// Fails with [`AgentError::NoRunningTurn`] when nothing is running.
    pub async fn cancel(&self) -> Result<(), AgentError> {
        let cancel = {
            let mut turn = self.shared.turn.lock().expect("turn state poisoned");
            let turn = turn.as_mut().ok_or(AgentError::NoRunningTurn)?;
            if turn.cancelling {
                None
            } else {
                turn.cancelling = true;
                Some(turn.cancel.clone())
            }
        };
        if let Some(cancel) = cancel {
            let _ = cancel.send(true);
            self.connection.send_session_cancel(self.session_id.clone());
            let resolved = self.connection.cancel_session_permissions(&self.session_id);
            tracing::debug!(session_id = %self.session_id.0, resolved, "cancelled pending permissions");
        }
        Ok(())
    }

    /// Switches the session mode; allowed only when the agent advertised
    /// `availableModes` at session creation.
    ///
    /// # Errors
    ///
    /// Fails when the agent advertised no modes or rejected the request.
    pub async fn set_mode(&self, mode_id: SessionModeId) -> Result<(), AgentError> {
        if let Some(mode_config) = self.mode_config_option() {
            return self
                .set_config_option(
                    mode_config.id.clone(),
                    SessionConfigOptionValue::value_id(SessionConfigValueId::new(
                        mode_id.0.as_ref(),
                    )),
                )
                .await;
        }
        let modes = self.shared.modes.lock().expect("modes poisoned").clone();
        let Some(modes) = modes else {
            return Err(AgentError::CapabilityUnsupported { method: "session/set_mode".into() });
        };
        if !modes.available_modes.iter().any(|mode| mode.id == mode_id) {
            return Err(AgentError::invalid_params(format!(
                "mode {mode_id:?} was not advertised by the agent"
            )));
        }
        self.connection.set_mode(self.session_id.clone(), mode_id.clone()).await?;
        self.shared.state.lock().expect("session state poisoned").current_mode =
            Some(mode_id.clone());
        if let Some(modes) = self.shared.modes.lock().expect("modes poisoned").as_mut() {
            modes.current_mode_id = mode_id;
        }
        Ok(())
    }

    /// Sets one session config option through `session/set_config_option`.
    pub async fn set_config_option(
        &self,
        config_id: SessionConfigId,
        value: SessionConfigOptionValue,
    ) -> Result<(), AgentError> {
        let config =
            self.config_options().into_iter().find(|config| config.id == config_id).ok_or_else(
                || {
                    AgentError::invalid_params(format!(
                        "config option {config_id:?} was not advertised by the agent"
                    ))
                },
            )?;
        validate_config_option_value(&self.connection, &config, &value)?;
        let response =
            self.connection.set_config_option(self.session_id.clone(), config_id, value).await?;
        self.shared
            .state
            .lock()
            .expect("session state poisoned")
            .set_config_options(response.config_options);
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

    /// Records host-observed prompt completion separately from ACP lifecycle.
    fn record_turn_terminal(&self, turn: &TurnKey, outcome: PromptTerminalOutcome) {
        if !matches!(outcome, PromptTerminalOutcome::Completed)
            && let Some(evidence) = self.shared.evidence_snapshot(turn.turn_id())
            && evidence.records().iter().any(|record| {
                matches!(
                    record.observation(),
                    TurnObservation::Write { .. } | TurnObservation::WriteTransaction { .. }
                )
            })
            && let Some(revision) = evidence.current_revision().cloned()
        {
            let _ = self.shared.observe_evidence(
                turn.turn_id(),
                TurnObservation::WriteTransaction {
                    revision,
                    stage: crate::turn_evidence::WriteTransactionStage::Interrupted,
                    outcome: crate::turn_evidence::EvidenceCheck::Failed,
                },
            );
        }
        let _ = self
            .shared
            .observe_evidence(turn.turn_id(), TurnObservation::PromptTerminal { outcome });
    }

    /// Clears state only when `turn` still owns this reservation. A late
    /// completion from an older prompt must never clear a newer turn.
    fn finish_turn(&self, turn: &TurnKey) {
        let mut active = self.shared.turn.lock().expect("turn state poisoned");
        if active.as_ref().is_some_and(|running| running.key() == turn) {
            *active = None;
            let mut active_turn = self.shared.active_turn.lock().expect("active turn poisoned");
            if active_turn.as_ref().is_some_and(|current| current == turn) {
                *active_turn = None;
            }
        }
    }

    /// Builds the metrics for the just-finished turn and clears the start
    /// marker.  Token usage is `None` for cancelled/failed turns.
    fn take_turn_metrics(&self, tokens: Option<ee_agent_protocol::Usage>) -> TurnMetrics {
        let started = self.shared.turn_started.lock().expect("turn state poisoned").take();
        let elapsed = started.map_or(Duration::ZERO, |started| started.elapsed());
        TurnMetrics { elapsed, tokens }
    }

    fn mode_config_option(&self) -> Option<SessionConfigOption> {
        self.config_options().into_iter().find(is_mode_config_option)
    }
}

fn is_mode_config_option(option: &SessionConfigOption) -> bool {
    matches!(option.category, Some(SessionConfigOptionCategory::Mode))
}

fn validate_prompt_blocks(
    connection: &AgentConnection,
    prompt: &[ContentBlock],
) -> Result<(), AgentError> {
    for block in prompt {
        match block {
            ContentBlock::Text(_) | ContentBlock::ResourceLink(_) => {}
            ContentBlock::Image(_) if connection.supports_prompt_images() => {}
            ContentBlock::Audio(_) if connection.supports_prompt_audio() => {}
            ContentBlock::Resource(_) if connection.supports_prompt_embedded_context() => {}
            ContentBlock::Image(_) => {
                return Err(AgentError::invalid_params(
                    "prompt content type image requires agentCapabilities.promptCapabilities.image",
                ));
            }
            ContentBlock::Audio(_) => {
                return Err(AgentError::invalid_params(
                    "prompt content type audio requires agentCapabilities.promptCapabilities.audio",
                ));
            }
            ContentBlock::Resource(_) => {
                return Err(AgentError::invalid_params(
                    "prompt content type resource requires agentCapabilities.promptCapabilities.embeddedContext",
                ));
            }
            _ => {
                return Err(AgentError::invalid_params(
                    "prompt contains unsupported content type for session/prompt",
                ));
            }
        }
    }
    Ok(())
}

fn validate_config_option_value(
    connection: &AgentConnection,
    option: &SessionConfigOption,
    value: &SessionConfigOptionValue,
) -> Result<(), AgentError> {
    match (&option.kind, value) {
        (SessionConfigKind::Select(select), SessionConfigOptionValue::ValueId { value }) => {
            if select_option_exists(&select.options, value) {
                Ok(())
            } else {
                Err(AgentError::invalid_params(format!(
                    "config option {:?} does not advertise value {:?}",
                    option.id, value
                )))
            }
        }
        (SessionConfigKind::Boolean(_), SessionConfigOptionValue::Boolean { .. }) => {
            if connection.supports_boolean_session_config_options() {
                Ok(())
            } else {
                Err(AgentError::CapabilityUnsupported {
                    method: "session/set_config_option".into(),
                })
            }
        }
        (SessionConfigKind::Select(_), _) => Err(AgentError::invalid_params(format!(
            "config option {:?} expects a select value id",
            option.id
        ))),
        (SessionConfigKind::Boolean(_), _) => Err(AgentError::invalid_params(format!(
            "config option {:?} expects a boolean value",
            option.id
        ))),
        _ => Err(AgentError::invalid_params(format!(
            "config option {:?} has unsupported kind for local validation",
            option.id
        ))),
    }
}

fn select_option_exists(
    options: &SessionConfigSelectOptions,
    value: &SessionConfigValueId,
) -> bool {
    match options {
        SessionConfigSelectOptions::Ungrouped(options) => {
            options.iter().any(|option| &option.value == value)
        }
        SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter())
            .any(|option| &option.value == value),
        _ => false,
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

    #[test]
    fn concurrent_prompt_reservation_does_not_duplicate_optimistic_message() {
        let (events, _events_rx) = mpsc::unbounded_channel();
        let shared = ThreadShared {
            agent_id: String::from("agent"),
            session_id: SessionId::new("session"),
            state: Mutex::new(SessionState::default()),
            order: Mutex::new(ee_agent_protocol::SessionUpdateOrder::new()),
            turn: Mutex::new(None),
            active_turn: Mutex::new(None),
            paused_turn: Mutex::new(None),
            turn_started: Mutex::new(None),
            evidence: Mutex::new(TurnEvidenceStore::default()),
            evidence_available: AtomicBool::new(false),
            modes: Mutex::new(None),
            events,
        };
        let prompt = vec![ContentBlock::Text(TextContent::new("fix it"))];

        shared.start_turn(prompt.clone(), false).expect("first prompt reserves turn");
        assert!(shared.has_turn_evidence());
        assert!(matches!(shared.start_turn(prompt, false), Err(AgentError::TurnAlreadyRunning)));

        let state = shared.state.lock().expect("session state poisoned");
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].blocks.len(), 1);
    }

    #[test]
    fn resume_reuses_paused_host_evidence_turn() {
        let (events, _events_rx) = mpsc::unbounded_channel();
        let shared = ThreadShared {
            agent_id: String::from("agent"),
            session_id: SessionId::new("session"),
            state: Mutex::new(SessionState::default()),
            order: Mutex::new(ee_agent_protocol::SessionUpdateOrder::new()),
            turn: Mutex::new(None),
            active_turn: Mutex::new(None),
            paused_turn: Mutex::new(None),
            turn_started: Mutex::new(None),
            evidence: Mutex::new(TurnEvidenceStore::default()),
            evidence_available: AtomicBool::new(false),
            modes: Mutex::new(None),
            events,
        };
        let prompt = vec![ContentBlock::Text(TextContent::new("fix it"))];
        let initial = shared.start_turn(prompt.clone(), false).expect("initial turn starts");
        let initial_key = initial.turn.clone();
        let before = shared
            .observe_evidence(
                initial_key.turn_id(),
                TurnObservation::PromptTerminal {
                    outcome: PromptTerminalOutcome::PausedRecoverable,
                },
            )
            .expect("paused prompt evidence")
            .evidence_ids;
        *shared.paused_turn.lock().expect("paused turn poisoned") = Some(initial_key.clone());
        *shared.turn.lock().expect("turn state poisoned") = None;
        *shared.active_turn.lock().expect("active turn poisoned") = None;

        let resumed = shared.start_turn(prompt, true).expect("paused turn resumes");
        assert_eq!(resumed.turn, initial_key);
        let after = shared
            .observe_evidence(
                resumed.turn.turn_id(),
                TurnObservation::PromptTerminal { outcome: PromptTerminalOutcome::Completed },
            )
            .expect("completed prompt evidence");
        assert!(before.iter().all(|id| after.evidence_ids.contains(id)));
        assert_eq!(shared.state.lock().expect("session state poisoned").messages.len(), 1);
    }
}
