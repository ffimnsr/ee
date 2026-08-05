//! JSON-RPC request dispatch.
//!
//! Routes incoming requests to typed handlers, validates params *before*
//! any provider call, negotiates ACP v1 only, and shapes typed SDK
//! responses.  Unknown requests get `method-not-found`; notifications are
//! routed here too (`session/cancel`), everything else is ignored with
//! tracing debug.
//!
//! `session/prompt` starts a spawned provider task and defers its response
//! to the server's FIFO outbound channel, so updates emitted during the
//! prompt arrive before the prompt response.

use std::sync::Arc;

use ee_agent_protocol::registry::{
    INITIALIZE_METHOD_NAME, SESSION_CANCEL_NOTIFICATION, SESSION_CLOSE_METHOD_NAME,
    SESSION_LIST_METHOD_NAME, SESSION_LOAD_METHOD_NAME, SESSION_NEW_METHOD_NAME,
    SESSION_PROMPT_METHOD_NAME,
};
use ee_agent_protocol::{
    CancelNotification, CloseSessionRequest, CloseSessionResponse, Error as RpcError,
    InitializeRequest, InitializeResponse, ListSessionsRequest, ListSessionsResponse,
    LoadSessionRequest, LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest,
    ProtocolVersion, RequestId, SessionId, SessionInfo,
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::{mpsc, watch};

use crate::client::ClientBridgeFactory;
use crate::config::AcpAgentServerConfig;
use crate::error::{AcpServerError, ProviderError};
use crate::provider::{
    AgentProvider, LoadSessionContext, NewSessionContext, PromptContext, ProviderFuture,
};
use crate::server::{ActivePromptError, ActivePrompts, OutboundEvent};
use crate::session::{ServerSession, SessionStore, SessionStoreError};
use crate::updates::{UpdateSink, UpdateSinkError};
use crate::validate::{validate_absolute_paths, validate_protocol_version_v1, validate_session_id};

/// Outcome of dispatching one request.
pub(crate) enum DispatchOutcome {
    /// A response is ready to write immediately.
    Immediate(Result<Value, RpcError>),
    /// A prompt task started; its response arrives via the outbound channel.
    Deferred,
}

/// Per-request dispatcher borrowing server state.
///
/// Constructed once per incoming request; provider calls are bounded by the
/// configured `request_timeout`, and spawned prompt tasks hold `Arc` clones
/// of the state they need.
pub(crate) struct RequestDispatcher<P> {
    provider: Arc<P>,
    config: Arc<AcpAgentServerConfig>,
    sessions: Arc<SessionStore>,
    active_prompts: Arc<ActivePrompts>,
    outbound_tx: mpsc::UnboundedSender<OutboundEvent>,
    client_factory: ClientBridgeFactory,
    request_id: RequestId,
}

impl<P: AgentProvider> RequestDispatcher<P> {
    pub(crate) fn new(
        provider: Arc<P>,
        config: Arc<AcpAgentServerConfig>,
        sessions: Arc<SessionStore>,
        active_prompts: Arc<ActivePrompts>,
        outbound_tx: mpsc::UnboundedSender<OutboundEvent>,
        client_factory: ClientBridgeFactory,
        request_id: RequestId,
    ) -> Self {
        Self { provider, config, sessions, active_prompts, outbound_tx, client_factory, request_id }
    }

    /// Routes one request to its typed handler.
    pub(crate) async fn dispatch(&self, method: &str, params: Value) -> DispatchOutcome {
        match method {
            INITIALIZE_METHOD_NAME => DispatchOutcome::Immediate(self.initialize(params).await),
            SESSION_NEW_METHOD_NAME => DispatchOutcome::Immediate(self.session_new(params).await),
            SESSION_LOAD_METHOD_NAME => DispatchOutcome::Immediate(self.session_load(params).await),
            SESSION_LIST_METHOD_NAME => DispatchOutcome::Immediate(self.session_list(params).await),
            SESSION_CLOSE_METHOD_NAME => {
                DispatchOutcome::Immediate(self.session_close(params).await)
            }
            SESSION_PROMPT_METHOD_NAME => match self.session_prompt(params).await {
                Ok(outcome) => outcome,
                Err(error) => DispatchOutcome::Immediate(Err(error)),
            },
            // `session/cancel` may arrive as a request (id present) or a
            // notification; both share the CancelNotification params shape.
            SESSION_CANCEL_NOTIFICATION => {
                DispatchOutcome::Immediate(self.session_cancel(params).await.map(|()| Value::Null))
            }
            _ => {
                tracing::debug!(method, "unknown JSON-RPC method");
                DispatchOutcome::Immediate(Err(RpcError::method_not_found()))
            }
        }
    }

    /// Routes one notification; only `session/cancel` is handled, everything
    /// else is ignored with tracing debug.
    pub(crate) async fn dispatch_notification(&self, method: &str, params: Value) {
        match method {
            SESSION_CANCEL_NOTIFICATION => {
                if let Err(error) = self.session_cancel(params).await {
                    tracing::warn!(%error, "failed to handle session/cancel notification");
                }
            }
            _ => {
                tracing::debug!(method, "ignoring notification");
            }
        }
    }

    /// `initialize`: negotiates ACP v1 only and returns provider identity
    /// and capabilities, plus framework identity metadata from config.
    async fn initialize(&self, params: Value) -> Result<Value, RpcError> {
        let request: InitializeRequest = parse_params(params)?;
        validate_protocol_version_v1(request.protocol_version)
            .map_err(|error| error.into_rpc_error())?;

        let mut response = InitializeResponse::new(ProtocolVersion::V1)
            .agent_info(self.provider.info())
            .agent_capabilities(self.provider.capabilities());
        // Framework implementation metadata from config, so clients can tell
        // which framework version serves the agent.
        let framework = serde_json::to_value(&self.config.implementation)
            .map_err(|_| RpcError::internal_error())?;
        let mut meta = serde_json::Map::new();
        meta.insert("framework".to_string(), framework);
        response.meta = Some(meta);

        to_value(response)
    }

    /// `session/new`: validates absolute paths, then delegates to the
    /// provider and registers the resolved session.
    async fn session_new(&self, params: Value) -> Result<Value, RpcError> {
        let request: NewSessionRequest = parse_params(params)?;
        validate_absolute_paths(&request.cwd, &request.additional_directories)
            .map_err(|error| error.into_rpc_error())?;

        let ctx = NewSessionContext {
            cwd: request.cwd.clone(),
            additional_directories: request.additional_directories.clone(),
            mcp_servers: request.mcp_servers.clone(),
            metadata: request.meta.clone(),
        };
        let init = self.with_provider_timeout(self.provider.new_session(ctx)).await?;
        reject_invalid_provider_session_id(&init.session_id)?;
        let session_id = init.session_id.clone();

        self.register_session(ServerSession {
            session_id: init.session_id.clone(),
            cwd: Some(request.cwd.clone()),
            additional_directories: request.additional_directories.clone(),
            mcp_servers: request.mcp_servers.clone(),
            title: init.title.clone(),
            metadata: Value::Null,
        })?;
        let response = NewSessionResponse::new(session_id)
            .modes(init.modes)
            .config_options(init.config_options);
        to_value(response)
    }

    /// `session/load`: validates absolute paths, delegates to the provider,
    /// and registers the loaded session.
    async fn session_load(&self, params: Value) -> Result<Value, RpcError> {
        let request: LoadSessionRequest = parse_params(params)?;
        validate_absolute_paths(&request.cwd, &request.additional_directories)
            .map_err(|error| error.into_rpc_error())?;

        let ctx = LoadSessionContext {
            session_id: request.session_id.clone(),
            cwd: request.cwd.clone(),
            additional_directories: request.additional_directories.clone(),
            mcp_servers: request.mcp_servers.clone(),
            metadata: request.meta.clone(),
        };
        let init = self.with_provider_timeout(self.provider.load_session(ctx)).await?;
        reject_invalid_provider_session_id(&init.session_id)?;

        self.register_session(ServerSession {
            session_id: init.session_id.clone(),
            cwd: Some(request.cwd.clone()),
            additional_directories: request.additional_directories.clone(),
            mcp_servers: request.mcp_servers.clone(),
            title: init.title.clone(),
            metadata: Value::Null,
        })?;
        let response =
            LoadSessionResponse::new().modes(init.modes).config_options(init.config_options);
        to_value(response)
    }

    /// `session/list`: returns live sessions in stable order.  Cursors are
    /// treated as opaque (parsed, not interpreted); the server serves a
    /// single page, so `next_cursor` stays unset.
    async fn session_list(&self, params: Value) -> Result<Value, RpcError> {
        let _request: ListSessionsRequest = parse_params(params)?;
        let sessions = self
            .sessions
            .list()
            .into_iter()
            .filter_map(|session| {
                // `session/list` requires a cwd; sessions without one (not
                // produced by dispatch) are skipped.
                Some(
                    SessionInfo::new(session.session_id, session.cwd?)
                        .additional_directories(session.additional_directories)
                        .title(session.title),
                )
            })
            .collect();
        to_value(ListSessionsResponse::new(sessions))
    }

    /// `session/prompt`: starts a cancellable prompt task for the session.
    ///
    /// Unknown sessions and concurrent same-session prompts are rejected
    /// before any provider call.  The response is deferred: the task streams
    /// updates through the FIFO outbound channel and completes it with the
    /// prompt result, so updates arrive before the response.
    async fn session_prompt(&self, params: Value) -> Result<DispatchOutcome, RpcError> {
        let request: PromptRequest = parse_params(params)?;

        // Reject unknown sessions before registering or invoking anything.
        let sink = self.update_sink_for(&request.session_id).map_err(|error| match error {
            UpdateSinkError::UnknownSession(session_id) => {
                AcpServerError::UnknownSession(session_id).into_rpc_error()
            }
            other => {
                RpcError::internal_error().data(serde_json::json!({ "reason": other.to_string() }))
            }
        })?;

        // One active prompt per session.
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let generation = self
            .active_prompts
            .start(&request.session_id, cancel_tx.clone())
            .map_err(|error| match error {
                ActivePromptError::AlreadyActive(session_id) => {
                    concurrent_prompt_error(&session_id)
                }
            })?;
        drop(cancel_tx); // the registry entry holds its own sender

        let ctx = PromptContext {
            session_id: request.session_id.clone(),
            prompt: request.prompt.clone(),
            metadata: request.meta.clone(),
        };
        let provider = self.provider.clone();
        let active_prompts = self.active_prompts.clone();
        let outbound_tx = self.outbound_tx.clone();
        let request_id = self.request_id.clone();
        let session_id = request.session_id.clone();
        // One live bridge per prompt: agent → client requests flow through
        // the outbound path, and every request this prompt owns dies with it.
        let client = self.client_factory.bridge();
        let join = tokio::spawn(async move {
            let result = provider.prompt(ctx, sink, client, cancel_rx).await;
            let _ = outbound_tx.send(OutboundEvent::PromptCompleted { request_id, result });
            // Always clean up active-prompt state: completion, provider
            // error, and cancellation all land here.
            active_prompts.remove(&session_id);
        });
        self.active_prompts.attach_join(&request.session_id, generation, join);
        Ok(DispatchOutcome::Deferred)
    }

    /// `session/cancel` (notification or request form): cancels the active
    /// prompt for the session and invokes the provider's cancel hook.
    ///
    /// Never errors when no prompt is active.
    async fn session_cancel(&self, params: Value) -> Result<(), RpcError> {
        let notification: CancelNotification = parse_params(params)?;
        if let Some(join) = self.active_prompts.cancel(&notification.session_id) {
            // The prompt task observes the cancellation signal, returns a
            // `Cancelled` result, and removes its own state.
            drop(join);
        }
        if let Err(error) = self
            .with_provider_timeout(self.provider.cancel_session(notification.session_id.clone()))
            .await
        {
            tracing::warn!(%error, "provider cancel_session failed");
        }
        Ok(())
    }

    /// `session/close`: cancels any active prompt (awaiting its cleanup with
    /// a bounded timeout), delegates to the provider, then removes the
    /// session.
    async fn session_close(&self, params: Value) -> Result<Value, RpcError> {
        let request: CloseSessionRequest = parse_params(params)?;
        let Some(_session) = self.sessions.get(&request.session_id) else {
            return Err(AcpServerError::UnknownSession(request.session_id.clone()).into_rpc_error());
        };

        // Cancel an active prompt for this session, if one is running, and
        // wait for its cleanup with a bounded timeout.
        if let Some(join) = self.active_prompts.cancel(&request.session_id) {
            let _ = tokio::time::timeout(self.config.request_timeout, join).await;
            self.active_prompts.remove(&request.session_id);
        }

        self.with_provider_timeout(self.provider.close_session(request.session_id.clone())).await?;
        self.sessions.remove(&request.session_id);
        to_value(CloseSessionResponse::new())
    }

    /// Creates an update sink for a live session; rejects unknown sessions.
    fn update_sink_for(&self, session_id: &SessionId) -> Result<UpdateSink, UpdateSinkError> {
        if !self.sessions.contains(session_id) {
            return Err(UpdateSinkError::UnknownSession(session_id.clone()));
        }
        Ok(UpdateSink::new(session_id.clone(), self.outbound_tx.clone()))
    }

    /// Registers a provider-resolved session, rejecting duplicate ids.
    fn register_session(&self, session: ServerSession) -> Result<(), RpcError> {
        match self.sessions.insert_new(session) {
            Ok(()) => Ok(()),
            Err(SessionStoreError::DuplicateSession(session_id)) => Err(RpcError::internal_error()
                .data(serde_json::json!({
                    "reason": format!("provider returned a duplicate session id: {session_id}"),
                }))),
        }
    }

    /// Runs a provider call bounded by the configured request timeout.
    async fn with_provider_timeout<T>(
        &self,
        call: ProviderFuture<Result<T, ProviderError>>,
    ) -> Result<T, RpcError> {
        match tokio::time::timeout(self.config.request_timeout, call).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(provider_error)) => {
                Err(AcpServerError::Provider(provider_error).into_rpc_error())
            }
            Err(_) => Err(AcpServerError::RequestTimeout { request_id: self.request_id.clone() }
                .into_rpc_error()),
        }
    }
}

/// Parses typed request params, rejecting malformed shapes before any
/// provider call.
fn parse_params<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|source| {
        RpcError::invalid_params().data(serde_json::json!({ "reason": source.to_string() }))
    })
}

/// Rejects provider-returned session ids that fail framework validation
/// (empty ids) as an internal error: the client did nothing wrong — the
/// provider backend returned malformed output.
fn reject_invalid_provider_session_id(session_id: &SessionId) -> Result<(), RpcError> {
    validate_session_id(session_id).map_err(|error| {
        RpcError::internal_error()
            .data(serde_json::json!({ "reason": format!("provider returned an invalid session id: {error}") }))
    })
}

fn concurrent_prompt_error(session_id: &SessionId) -> RpcError {
    RpcError::invalid_params().data(serde_json::json!({
        "reason": format!("a prompt is already active for session {session_id}"),
    }))
}

/// Serializes a typed response; only fails on impossible SDK serialization
/// bugs.
fn to_value<T: serde::Serialize>(value: T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|_| RpcError::internal_error())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ClientBridge;
    use crate::ids::RequestIdGenerator;
    use crate::provider::{PromptResult, ProviderFuture, SessionInit};
    use crate::updates::UpdateSinkError;

    struct StubProvider;

    impl AgentProvider for StubProvider {
        fn info(&self) -> ee_agent_protocol::Implementation {
            ee_agent_protocol::Implementation::new("stub", "0")
        }

        fn capabilities(&self) -> ee_agent_protocol::AgentCapabilities {
            ee_agent_protocol::AgentCapabilities::default()
        }

        fn new_session(
            &self,
            _ctx: NewSessionContext,
        ) -> ProviderFuture<Result<SessionInit, ProviderError>> {
            Box::pin(async { unimplemented!("stub provider") })
        }

        fn load_session(
            &self,
            _ctx: LoadSessionContext,
        ) -> ProviderFuture<Result<SessionInit, ProviderError>> {
            Box::pin(async { unimplemented!("stub provider") })
        }

        fn prompt(
            &self,
            _ctx: PromptContext,
            _sink: crate::updates::UpdateSink,
            _client: ClientBridge,
            _cancel: watch::Receiver<bool>,
        ) -> ProviderFuture<Result<PromptResult, ProviderError>> {
            Box::pin(async { unimplemented!("stub provider") })
        }

        fn cancel_session(
            &self,
            _session_id: SessionId,
        ) -> ProviderFuture<Result<(), ProviderError>> {
            Box::pin(async { unimplemented!("stub provider") })
        }

        fn close_session(
            &self,
            _session_id: SessionId,
        ) -> ProviderFuture<Result<(), ProviderError>> {
            Box::pin(async { unimplemented!("stub provider") })
        }
    }

    fn dispatcher(sessions: Arc<SessionStore>) -> RequestDispatcher<StubProvider> {
        let (outbound_tx, _outbound_rx) = mpsc::unbounded_channel();
        let config = AcpAgentServerConfig::default();
        let client_factory = ClientBridgeFactory::new(
            std::sync::Mutex::new(RequestIdGenerator::new()),
            Arc::new(crate::client::PendingRequests::new()),
            outbound_tx.clone(),
            config.request_timeout,
        );
        RequestDispatcher::new(
            Arc::new(StubProvider),
            Arc::new(config),
            sessions,
            Arc::new(ActivePrompts::new()),
            outbound_tx,
            client_factory,
            RequestId::Number(1),
        )
    }

    fn known_session_store() -> Arc<SessionStore> {
        let sessions = Arc::new(SessionStore::new());
        sessions
            .insert_new(ServerSession {
                session_id: SessionId::new("known"),
                cwd: None,
                additional_directories: Vec::new(),
                mcp_servers: Vec::new(),
                title: None,
                metadata: Value::Null,
            })
            .expect("seeds store");
        sessions
    }

    #[test]
    fn update_sink_for_rejects_unknown_session() {
        let dispatcher = dispatcher(Arc::new(SessionStore::new()));
        match dispatcher.update_sink_for(&SessionId::new("ghost")) {
            Err(UpdateSinkError::UnknownSession(session_id)) => {
                assert_eq!(session_id, SessionId::new("ghost"));
            }
            Ok(_) => panic!("expected unknown-session rejection, got a sink"),
            Err(other) => panic!("expected unknown-session rejection, got {other:?}"),
        }
    }

    #[test]
    fn update_sink_for_accepts_known_session() {
        let dispatcher = dispatcher(known_session_store());
        let sink = dispatcher.update_sink_for(&SessionId::new("known")).expect("sink");
        assert_eq!(sink.session_id(), &SessionId::new("known"));
    }

    #[test]
    fn update_sink_error_variants_are_identifiable() {
        assert_eq!(
            UpdateSinkError::UnknownSession(SessionId::new("ghost")).to_string(),
            "unknown session: ghost"
        );
        assert_eq!(
            UpdateSinkError::EmptyId("message_id").to_string(),
            "message_id must not be empty"
        );
        assert_eq!(UpdateSinkError::Closed.to_string(), "update sink is closed");
    }
}
