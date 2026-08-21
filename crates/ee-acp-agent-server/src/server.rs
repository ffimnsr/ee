//! Server runtime.
//!
//! [`AcpAgentServer`] owns the reader/dispatcher loop: every incoming
//! request is routed through [`crate::dispatch`], every response and
//! `session/update` notification is written through the transport writer
//! path.  Malformed frames are answered per JSON-RPC (`-32700` parse error,
//! `-32600` invalid request) and the loop keeps serving.  On transport close
//! (clean EOF or error) the writer path closes first, then every active
//! prompt is cancelled and awaited (bounded), then pending agent → client
//! requests fail so blocked providers can finish.
//!
//! Prompt execution runs in spawned tasks.  Each prompt gets one
//! [`crate::updates::UpdateSink`] bound to its session; updates and the
//! prompt completion both flow through a single FIFO outbound channel, so a
//! session's updates always arrive before its prompt response.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use ee_agent_protocol::registry::SESSION_UPDATE_NOTIFICATION;
use ee_agent_protocol::{
    Error as RpcError, PromptResponse, RawJsonRpcMessage, RawJsonRpcParams, RequestId, SessionId,
    SessionNotification, SessionUpdate, StopReason,
};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::client::{ClientBridgeFactory, PendingRequests};
use crate::config::AcpAgentServerConfig;
use crate::dispatch::{DispatchOutcome, RequestDispatcher};
use crate::error::{AcpServerError, ProviderError};
use crate::ids::RequestIdGenerator;
use crate::provider::AgentProvider;
use crate::session::SessionStore;
use crate::transport::{AcpTransport, JsonRpcFrame, StdioTransport};

/// One item on the server's FIFO outbound channel, produced by prompt tasks
/// and drained by the run loop.
///
/// Public so the `test-utils` constructors of [`UpdateSink`]
/// (crate::updates::UpdateSink) and [`ClientBridge`](crate::client::ClientBridge)
/// can expose channels that capture these events without a running server;
/// consumers never construct this enum themselves.
#[derive(Debug)]
pub enum OutboundEvent {
    /// One `session/update` notification to forward for a session.
    Update {
        /// The session the update pertains to.
        session_id: SessionId,
        /// The SDK update payload.
        update: Box<SessionUpdate>,
    },
    /// A prompt finished; the loop writes its response.
    PromptCompleted {
        /// The id of the `session/prompt` request this answers.
        request_id: RequestId,
        /// The provider's outcome; a provider cancellation is mapped to a
        /// deterministic `StopReason::Cancelled` result.
        result: Result<PromptResponse, ProviderError>,
    },
    /// A deferred non-prompt response finished (e.g. `session/load` with
    /// conversation replay); the loop writes it after every update the
    /// provider queued, so streamed updates always precede the response.
    DeferredResponse {
        /// The id of the request this answers.
        request_id: RequestId,
        /// The serialized result, or a JSON-RPC error.
        result: Result<serde_json::Value, ee_agent_protocol::Error>,
    },
    /// One agent → client request to write (from a prompt's `ClientBridge`).
    ClientRequest {
        /// The fully-built JSON-RPC request frame.
        frame: RawJsonRpcMessage,
    },
}

/// Why a new prompt could not start for a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ActivePromptError {
    /// The session already has a running prompt.
    AlreadyActive(SessionId),
}

impl fmt::Display for ActivePromptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyActive(session_id) => {
                write!(f, "a prompt is already active for session {session_id}")
            }
        }
    }
}

/// One running prompt: its cancellation signal and join handle.
struct ActivePrompt {
    cancel: watch::Sender<bool>,
    join: Option<JoinHandle<()>>,
    generation: u64,
}

/// Registry of active prompts, one per session.
///
/// `start` reserves a session atomically (rejecting concurrent prompts),
/// `attach_join` pairs the spawned task with its entry (guarded by a
/// generation so a stale attach cannot clobber a newer prompt), `cancel`
/// flips the cancellation signal, and `remove` cleans up after the task
/// finishes.
#[derive(Default)]
pub(crate) struct ActivePrompts {
    inner: Mutex<ActivePromptsInner>,
}

#[derive(Default)]
struct ActivePromptsInner {
    prompts: HashMap<SessionId, ActivePrompt>,
    next_generation: u64,
}

impl ActivePrompts {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Reserves a session for a new prompt; fails if one is already active.
    ///
    /// The caller supplies the cancellation sender; the task receives its
    /// receiver, so signals sent through the registry reach the running
    /// provider.
    pub(crate) fn start(
        &self,
        session_id: &SessionId,
        cancel: watch::Sender<bool>,
    ) -> Result<u64, ActivePromptError> {
        let mut inner = self.inner.lock().expect("active prompts poisoned");
        if inner.prompts.contains_key(session_id) {
            return Err(ActivePromptError::AlreadyActive(session_id.clone()));
        }
        let generation = inner.next_generation;
        inner.next_generation += 1;
        inner.prompts.insert(session_id.clone(), ActivePrompt { cancel, join: None, generation });
        Ok(generation)
    }

    /// Pairs a spawned prompt task with its registry entry.  No-op if the
    /// entry was already removed (task finished first) or replaced by a
    /// newer generation.
    pub(crate) fn attach_join(
        &self,
        session_id: &SessionId,
        generation: u64,
        join: JoinHandle<()>,
    ) {
        let mut inner = self.inner.lock().expect("active prompts poisoned");
        if let Some(entry) = inner.prompts.get_mut(session_id)
            && entry.generation == generation
        {
            entry.join = Some(join);
        }
    }

    /// Whether a session has an active prompt or load operation.
    pub(crate) fn contains(&self, session_id: &SessionId) -> bool {
        self.inner.lock().expect("active prompts poisoned").prompts.contains_key(session_id)
    }

    /// Flips the cancellation signal for a session's active prompt and
    /// returns its join handle, if any.  Returns `None` when no prompt is
    /// active (or its task already finished).
    pub(crate) fn cancel(&self, session_id: &SessionId) -> Option<JoinHandle<()>> {
        let mut inner = self.inner.lock().expect("active prompts poisoned");
        let entry = inner.prompts.get_mut(session_id)?;
        let _ = entry.cancel.send(true);
        entry.join.take()
    }

    /// Removes the session's active-prompt entry (idempotent).
    pub(crate) fn remove(&self, session_id: &SessionId) {
        self.inner.lock().expect("active prompts poisoned").prompts.remove(session_id);
    }

    /// Flips the cancellation signal for every active prompt and returns all
    /// join handles — used when the transport closes so no prompt outlives
    /// the connection.
    pub(crate) fn cancel_all(&self) -> Vec<JoinHandle<()>> {
        let mut inner = self.inner.lock().expect("active prompts poisoned");
        inner
            .prompts
            .values_mut()
            .filter_map(|entry| {
                let _ = entry.cancel.send(true);
                entry.join.take()
            })
            .collect()
    }
}

/// Agent-side server runtime for one ACP connection.
///
/// Generic over the provider; the framework owns dispatch, version
/// negotiation, the session store, active-prompt tracking, and typed
/// response shaping.
pub struct AcpAgentServer<P> {
    provider: std::sync::Arc<P>,
    config: std::sync::Arc<AcpAgentServerConfig>,
    sessions: std::sync::Arc<SessionStore>,
    active_prompts: std::sync::Arc<ActivePrompts>,
    outbound_tx: Option<mpsc::UnboundedSender<OutboundEvent>>,
    outbound_rx: Option<mpsc::UnboundedReceiver<OutboundEvent>>,
    /// Pending agent → client requests; the run loop routes inbound client
    /// responses here and fails everything on transport close.
    pending: std::sync::Arc<PendingRequests>,
    /// Builds per-prompt [`ClientBridge`](crate::client::ClientBridge)
    /// handles sharing this server's id space and pending registry.
    client_factory: ClientBridgeFactory,
}

impl<P: AgentProvider> AcpAgentServer<P> {
    /// Creates a server for the given provider and configuration.
    #[must_use]
    pub fn new(provider: P, config: AcpAgentServerConfig) -> Self {
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let pending = std::sync::Arc::new(PendingRequests::new());
        let client_factory = ClientBridgeFactory::new(
            Mutex::new(RequestIdGenerator::new()),
            pending.clone(),
            outbound_tx.clone(),
            config.request_timeout,
        );
        Self {
            provider: std::sync::Arc::new(provider),
            config: std::sync::Arc::new(config),
            sessions: std::sync::Arc::new(SessionStore::new()),
            active_prompts: std::sync::Arc::new(ActivePrompts::new()),
            outbound_tx: Some(outbound_tx),
            outbound_rx: Some(outbound_rx),
            pending,
            client_factory,
        }
    }

    /// Runs the server over stdin/stdout with the configured frame cap.
    ///
    /// Returns `Ok(())` on clean EOF and `Err` on transport failure; either
    /// way all pending state is dropped.
    pub async fn run_stdio(self) -> Result<(), AcpServerError> {
        let transport = StdioTransport::new(self.config.max_frame_bytes);
        self.run_with_transport(transport).await
    }

    /// Runs the server over an arbitrary transport (in-memory transports
    /// for tests, stdio for real servers).
    ///
    /// Reads frames until clean EOF (`Ok(())`) or a transport error
    /// (propagated).  Requests are dispatched; prompt tasks stream updates
    /// and completion through the FIFO outbound channel, so per-session
    /// update order and update-before-response order hold.  Inbound client
    /// responses are routed to the pending agent → client requests.
    ///
    /// Malformed frames are answered per JSON-RPC and the loop keeps serving:
    /// unparseable JSON gets a `-32700` parse-error response and valid JSON
    /// that is not a JSON-RPC message (including oversized frames) gets a
    /// `-32600` invalid-request response, both with a `null` id.  I/O errors
    /// stop the server.
    ///
    /// On close — clean EOF or error — the writer path is closed first, then
    /// every active prompt is cancelled and awaited (bounded), and finally
    /// every pending agent → client request is failed so blocked providers
    /// can finish; all session state is dropped with the server.
    pub async fn run_with_transport<T: AcpTransport>(
        mut self,
        mut transport: T,
    ) -> Result<(), AcpServerError> {
        let result = async {
            // The outbound receiver lives inside the run loop: when the loop
            // exits (clean EOF or error) it drops, which closes the writer
            // path — every later send by a prompt task fails with `Closed`.
            let mut outbound_rx = self.outbound_rx.take().expect("server runs once");
            loop {
                tokio::select! {
                    read = transport.read_message() => {
                        let frame = match read {
                            Ok(Some(frame)) => frame,
                            Ok(None) => break, // clean EOF: shut down
                            Err(error) => match error {
                                // JSON-RPC: answer parse errors and keep
                                // serving the rest of the stream.
                                AcpServerError::JsonParse { .. } => {
                                    tracing::warn!(%error, "responding to malformed JSON frame");
                                    self.write_result(
                                        &mut transport,
                                        RequestId::Null,
                                        Err(error.into_rpc_error()),
                                    ).await?;
                                    continue;
                                }
                                // Valid JSON that is not a JSON-RPC message
                                // (wrong shape, batch, or oversized frame).
                                AcpServerError::Protocol(_) => {
                                    tracing::warn!(%error, "responding to invalid JSON-RPC frame");
                                    self.write_result(
                                        &mut transport,
                                        RequestId::Null,
                                        Err(error.into_rpc_error()),
                                    ).await?;
                                    continue;
                                }
                                other => return Err(other),
                            },
                        };
                        match frame {
                            JsonRpcFrame::Request(request) => {
                                let id = request.id.clone();
                                let method = request.method.to_string();
                                let params = raw_params_to_value(request.params);
                                let dispatcher = self.dispatcher(id.clone());
                                match dispatcher.dispatch(&method, params).await {
                                    DispatchOutcome::Immediate(result) => {
                                        self.write_result(&mut transport, id, result).await?;
                                    }
                                    DispatchOutcome::Deferred => {
                                        // Response arrives via the outbound channel.
                                    }
                                }
                            }
                            JsonRpcFrame::Notification(notification) => {
                                let method = notification.method.to_string();
                                let params = raw_params_to_value(notification.params);
                                let dispatcher = self.dispatcher(RequestId::Null);
                                dispatcher.dispatch_notification(&method, params).await;
                            }
                            JsonRpcFrame::Response(response) => {
                                // The client answered one of our agent →
                                // client requests; resolve its pending entry.
                                self.pending.handle_response(response);
                            }
                        }
                    }
                    event = outbound_rx.recv() => {
                        match event {
                            Some(OutboundEvent::Update { session_id, update }) => {
                                // Never emit updates for sessions the store no
                                // longer knows (closed mid-prompt).
                                if !self.sessions.contains(&session_id) {
                                    tracing::warn!(%session_id, "dropping update for unknown session");
                                    continue;
                                }
                                let params = serde_json::to_value(SessionNotification::new(
                                    session_id,
                                    *update,
                                ))
                                    .map_err(|_| AcpServerError::Protocol(
                                        "failed to serialize session update".into(),
                                    ))?;
                                let frame = RawJsonRpcMessage::notification(
                                    SESSION_UPDATE_NOTIFICATION.to_string(),
                                    params,
                                ).map_err(|_| AcpServerError::Protocol(
                                    "failed to build session update notification".into(),
                                ))?;
                                transport.write_message(frame).await?;
                            }
                            Some(OutboundEvent::PromptCompleted { request_id, result }) => {
                                let response = match result {
                                    Ok(prompt_response) => RawJsonRpcMessage::response(
                                        request_id,
                                        Ok(serde_json::to_value(prompt_response).map_err(|_| {
                                            AcpServerError::Protocol(
                                                "failed to serialize prompt response".into(),
                                            )
                                        })?),
                                    ),
                                    Err(ProviderError::Cancellation) => RawJsonRpcMessage::response(
                                        request_id,
                                        // Deterministic result: ACP defines a
                                        // `cancelled` stop reason.
                                        Ok(serde_json::to_value(
                                            PromptResponse::new(StopReason::Cancelled),
                                        ).map_err(|_| AcpServerError::Protocol(
                                            "failed to serialize cancelled prompt response".into(),
                                        ))?),
                                    ),
                                    Err(provider_error) => RawJsonRpcMessage::response(
                                        request_id,
                                        Err(AcpServerError::Provider(provider_error)
                                            .into_rpc_error()),
                                    ),
                                };
                                transport.write_message(response).await?;
                            }
                            Some(OutboundEvent::ClientRequest { frame }) => {
                                transport.write_message(frame).await?;
                            }
                            Some(OutboundEvent::DeferredResponse { request_id, result }) => {
                                let response = match result {
                                    Ok(value) => RawJsonRpcMessage::response(request_id, Ok(value)),
                                    Err(error) => {
                                        RawJsonRpcMessage::response(request_id, Err(error))
                                    }
                                };
                                transport.write_message(response).await?;
                            }
                            None => {
                                // Unreachable while the server holds its own
                                // sender; guard anyway.
                                tracing::warn!("prompt outbound channel closed");
                                break;
                            }
                        }
                    }
                }
            }
            Ok(())
        }
        .await;
        // The reader is gone (clean EOF or error).  Tear down in order:
        // 1. Close the outbound writer path from the server side (the
        //    receiver already dropped with the run loop).
        drop(self.outbound_tx.take());
        // 2. Signal every active prompt, then fail every pending agent →
        //    client request.  Prompts blocked on a bridge call can only
        //    finish once their pending request is resolved, so both signals
        //    go out before any join is awaited.
        let joins = self.active_prompts.cancel_all();
        self.pending.fail_all(ProviderError::ClientRequestFailure("transport closed".into()));
        // 3. Await prompt cleanup (bounded) so no prompt task outlives the
        //    connection.
        for join in joins {
            let _ = tokio::time::timeout(self.config.request_timeout, join).await;
        }
        result
    }

    fn dispatcher(&self, request_id: RequestId) -> RequestDispatcher<P> {
        RequestDispatcher::new(
            self.provider.clone(),
            self.config.clone(),
            self.sessions.clone(),
            self.active_prompts.clone(),
            self.outbound_tx.as_ref().expect("server is running").clone(),
            self.client_factory.clone(),
            request_id,
        )
    }

    async fn write_result<T: AcpTransport>(
        &self,
        transport: &mut T,
        id: RequestId,
        result: Result<serde_json::Value, RpcError>,
    ) -> Result<(), AcpServerError> {
        let frame = match result {
            Ok(value) => RawJsonRpcMessage::response(id, Ok(value)),
            Err(rpc_error) => RawJsonRpcMessage::response(id, Err(rpc_error)),
        };
        transport.write_message(frame).await
    }
}

/// Converts raw JSON-RPC params into a plain JSON value for typed parsing.
fn raw_params_to_value(params: Option<RawJsonRpcParams>) -> serde_json::Value {
    match params {
        None => serde_json::Value::Null,
        Some(RawJsonRpcParams::Object(map)) => serde_json::Value::Object(map),
        Some(RawJsonRpcParams::Array(array)) => serde_json::Value::Array(array),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_rejects_concurrent_prompt_but_allows_other_sessions() {
        let registry = ActivePrompts::new();
        let first = SessionId::new("s-1");
        let (cancel, _receiver) = watch::channel(false);
        let generation = registry.start(&first, cancel).expect("first prompt starts");
        assert_eq!(generation, 0);

        let (cancel, _receiver) = watch::channel(false);
        assert_eq!(
            registry.start(&first, cancel),
            Err(ActivePromptError::AlreadyActive(SessionId::new("s-1")))
        );

        // A different session may run a prompt in parallel.
        let second = SessionId::new("s-2");
        let (cancel, _receiver) = watch::channel(false);
        assert!(registry.start(&second, cancel).is_ok());
    }

    #[tokio::test]
    async fn cancel_flips_signal_and_returns_join() {
        let registry = ActivePrompts::new();
        let session = SessionId::new("s-1");
        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        let generation = registry.start(&session, cancel_tx).expect("starts");
        let join = tokio::spawn(async move {
            let _ = cancel_rx.changed().await;
        });
        registry.attach_join(&session, generation, join);

        // The task is still running until the signal flips.
        let join = registry.cancel(&session).expect("join handle");
        join.await.expect("task finishes after cancel");
    }

    #[tokio::test]
    async fn stale_attach_join_does_not_clobber_newer_prompt() {
        let registry = ActivePrompts::new();
        let session = SessionId::new("s-1");
        let (cancel, _receiver) = watch::channel(false);
        let stale_generation = registry.start(&session, cancel).expect("first starts");

        // Simulate the first task finishing and a second prompt starting.
        registry.remove(&session);
        let (cancel, _receiver) = watch::channel(false);
        let current_generation = registry.start(&session, cancel).expect("second starts");

        // A late attach from the first prompt must not attach to the second.
        let stale_join = tokio::spawn(async {});
        registry.attach_join(&session, stale_generation, stale_join);
        let current_join = tokio::spawn(async {});
        registry.attach_join(&session, current_generation, current_join);

        // Cancelling returns the second prompt's join, not the stale one.
        assert!(registry.cancel(&session).is_some());
    }

    #[test]
    fn remove_cleans_state_for_new_prompt() {
        let registry = ActivePrompts::new();
        let session = SessionId::new("s-1");
        let (cancel, _receiver) = watch::channel(false);
        let _generation = registry.start(&session, cancel).expect("starts");
        registry.remove(&session);

        let (cancel, _receiver) = watch::channel(false);
        assert!(registry.start(&session, cancel).is_ok(), "removed state frees the session");
    }

    #[test]
    fn cancel_without_active_prompt_returns_none() {
        let registry = ActivePrompts::new();
        assert!(registry.cancel(&SessionId::new("ghost")).is_none());
        // Removing a missing session is a no-op.
        registry.remove(&SessionId::new("ghost"));
    }

    #[tokio::test]
    async fn cancel_all_flips_every_prompt_signal_and_returns_all_joins() {
        let registry = ActivePrompts::new();
        let mut receivers = Vec::new();
        for session in ["s-1", "s-2", "s-3"] {
            let session = SessionId::new(session);
            let (cancel_tx, mut cancel_rx) = watch::channel(false);
            receivers.push(cancel_rx.clone());
            let generation = registry.start(&session, cancel_tx).expect("starts");
            let join = tokio::spawn(async move {
                let _ = cancel_rx.changed().await;
            });
            registry.attach_join(&session, generation, join);
        }

        let joins = registry.cancel_all();
        assert_eq!(joins.len(), 3, "one join per active prompt");
        for join in joins {
            join.await.expect("every cancelled prompt finishes");
        }
        // Signals were flipped for every prompt: all receivers observed a
        // change.
        for receiver in &mut receivers {
            assert!(receiver.has_changed().expect("receiver live"), "signal flipped");
        }
    }

    #[test]
    fn cancel_all_without_active_prompts_returns_empty() {
        let registry = ActivePrompts::new();
        assert!(registry.cancel_all().is_empty());
    }
}
