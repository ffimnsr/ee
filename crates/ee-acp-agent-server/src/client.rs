//! Agent → client request bridge.
//!
//! Providers call typed [`ClientBridge`] methods (fs, terminal, elicitation)
//! during a prompt turn.  The framework owns the outbound JSON-RPC request,
//! id allocation, response correlation, timeouts, and cleanup:
//!
//! - Requests flow through the server's FIFO outbound channel, so they share
//!   the single transport writer path with updates and prompt responses.
//! - The client's response is routed back by the server run loop through
//!   `PendingRequests::handle_response`, which resolves the matching pending
//!   oneshot by request id.
//! - Pending entries are removed on a matching response, on timeout, on
//!   write failure, when the owning prompt ends (`OwnerCleanup`), and when
//!   the transport closes (`PendingRequests::fail_all`) — a request never
//!   outlives its prompt, a timeout, or the connection.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use ee_agent_protocol::registry::{
    ELICITATION_CREATE_METHOD_NAME, FS_READ_TEXT_FILE_METHOD_NAME, FS_WRITE_TEXT_FILE_METHOD_NAME,
    MCP_CONNECT_METHOD_NAME, MCP_DISCONNECT_METHOD_NAME, MCP_MESSAGE_METHOD_NAME,
    TERMINAL_CREATE_METHOD_NAME, TERMINAL_KILL_METHOD_NAME, TERMINAL_OUTPUT_METHOD_NAME,
    TERMINAL_RELEASE_METHOD_NAME, TERMINAL_WAIT_FOR_EXIT_METHOD_NAME,
};

/// Client-request methods removed in v2 (migration guide: the client file
/// system and terminal execution APIs are gone; client-side tools surface
/// through MCP servers instead).  On a v2 connection these fail closed before
/// anything reaches the transport — the receiving v2 client has no such
/// handlers.
const V2_REMOVED_CLIENT_METHODS: &[&str] = &[
    FS_READ_TEXT_FILE_METHOD_NAME,
    FS_WRITE_TEXT_FILE_METHOD_NAME,
    TERMINAL_CREATE_METHOD_NAME,
    TERMINAL_OUTPUT_METHOD_NAME,
    TERMINAL_RELEASE_METHOD_NAME,
    TERMINAL_WAIT_FOR_EXIT_METHOD_NAME,
    TERMINAL_KILL_METHOD_NAME,
];
use ee_agent_protocol::{
    ConnectMcpRequest, ConnectMcpResponse, CreateElicitationRequest, CreateElicitationResponse,
    CreateTerminalRequest, CreateTerminalResponse, DisconnectMcpRequest, DisconnectMcpResponse,
    Error as RpcError, KillTerminalRequest, KillTerminalResponse, MessageMcpNotification,
    MessageMcpRequest, MessageMcpResponse, ProtocolVersion, RawJsonRpcMessage, ReadTextFileRequest,
    ReadTextFileResponse, ReleaseTerminalRequest, ReleaseTerminalResponse, RequestId, Response,
    SessionId, TerminalOutputRequest, TerminalOutputResponse, WaitForTerminalExitRequest,
    WaitForTerminalExitResponse, WriteTextFileRequest, WriteTextFileResponse,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::error::{CODE_PERMISSION_DENIED, CODE_REQUEST_CANCELLED, ProviderError};
use crate::ids::RequestIdGenerator;
use crate::server::OutboundEvent;

/// One in-flight agent → client request.
struct PendingRequest {
    /// The session the request belongs to (v2 foreground-state reporting).
    session_id: SessionId,
    /// The prompt that issued the request; entries are cleaned up when the
    /// owning prompt ends.
    owner: u64,
    /// Resolved by [`PendingRequests::handle_response`], a timeout, or
    /// [`PendingRequests::fail_all`].
    sender: oneshot::Sender<Result<Value, ProviderError>>,
}

/// Invoked when a pending agent → client request starts (`true`) or stops
/// (`false`) for a session.  The server uses it to report v2 `state_update`
/// foreground transitions (`requires_action` while blocked, `running` when the
/// client answers) on v2 connections.
type PendingToggle = Arc<dyn Fn(&SessionId, bool) + Send + Sync>;

/// Registry of in-flight agent → client requests, keyed by request id.
///
/// Shared by the server run loop (which routes inbound responses here) and
/// every [`ClientBridge`] (which inserts and awaits requests).
#[derive(Default)]
pub(crate) struct PendingRequests {
    inner: Mutex<std::collections::HashMap<RequestId, PendingRequest>>,
    toggle: Option<PendingToggle>,
}

impl PendingRequests {
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Creates the registry with a foreground-state toggle; the callback
    /// fires for every pending request start/stop (see [`PendingToggle`]).
    pub(crate) fn with_toggle(toggle: PendingToggle) -> Self {
        Self { inner: Mutex::new(std::collections::HashMap::new()), toggle: Some(toggle) }
    }

    /// Registers a pending request before it is written to the transport.
    pub(crate) fn insert(
        &self,
        id: RequestId,
        owner: u64,
        session_id: SessionId,
        sender: oneshot::Sender<Result<Value, ProviderError>>,
    ) {
        self.notify(&session_id, true);
        self.inner
            .lock()
            .expect("pending requests poisoned")
            .insert(id, PendingRequest { session_id, owner, sender });
    }

    /// Routes one inbound JSON-RPC response envelope to its pending request.
    ///
    /// Matching requests are resolved with the result or a mapped provider
    /// error; unknown ids (late responses, unsolicited frames) are ignored
    /// with tracing debug.
    pub(crate) fn handle_response(&self, response: Response<Value>) {
        match response {
            Response::Result { id, result } => self.resolve(id, Ok(result)),
            Response::Error { id, error } => self.resolve(id, Err(client_error_to_provider(error))),
        }
    }

    /// Removes the pending request with the given id, if present.
    pub(crate) fn remove(&self, id: &RequestId) {
        let entry = self.inner.lock().expect("pending requests poisoned").remove(id);
        if let Some(entry) = entry {
            self.notify(&entry.session_id, false);
        }
    }

    /// Number of pending requests (unit tests assert cleanup).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.lock().expect("pending requests poisoned").len()
    }

    /// Resolves every pending request with the given failure — used when the
    /// transport closes so blocked providers can finish.
    pub(crate) fn fail_all(&self, reason: ProviderError) {
        let entries = std::mem::take(&mut *self.inner.lock().expect("pending requests poisoned"));
        tracing::debug!(count = entries.len(), "failing pending client requests on close");
        for (_, entry) in entries {
            self.notify(&entry.session_id, false);
            let _ = entry.sender.send(Err(reason.clone()));
        }
    }

    /// Removes every pending request owned by one prompt (its bridge handle
    /// was dropped: prompt finished, was cancelled, or was aborted).
    pub(crate) fn remove_owner(&self, owner: u64) {
        let mut entries = self.inner.lock().expect("pending requests poisoned");
        let removed = entries
            .extract_if(|_, entry| entry.owner == owner)
            .map(|(_, entry)| entry.session_id)
            .collect::<Vec<_>>();
        drop(entries);
        for session_id in removed {
            self.notify(&session_id, false);
        }
    }

    fn resolve(&self, id: RequestId, result: Result<Value, ProviderError>) {
        let Some(entry) = self.inner.lock().expect("pending requests poisoned").remove(&id) else {
            tracing::debug!(%id, "ignoring response for unknown request id");
            return;
        };
        self.notify(&entry.session_id, false);
        let _ = entry.sender.send(result);
    }

    /// Reports a foreground-state transition (v2-only on the wire; the
    /// callback decides what to emit).
    fn notify(&self, session_id: &SessionId, pending: bool) {
        if let Some(toggle) = &self.toggle {
            toggle(session_id, pending);
        }
    }
}

/// Maps a client's JSON-RPC error onto the provider-visible error: permission
/// denials and cancellations keep their meaning; everything else is a
/// client-request failure carrying the client's message, safe reason, and code.
fn client_error_to_provider(error: RpcError) -> ProviderError {
    let code = i32::from(error.code);
    let reason = error
        .data
        .as_ref()
        .and_then(|data| data.get("reason"))
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty());
    let message = match reason {
        Some(reason) if reason != error.message => format!("{}: {reason}", error.message),
        _ => error.message,
    };

    match code {
        CODE_PERMISSION_DENIED => ProviderError::PermissionDenied(message),
        CODE_REQUEST_CANCELLED => ProviderError::Cancellation,
        _ => ProviderError::ClientRequestFailure(format!("{message} (jsonrpc code {code})")),
    }
}

/// State shared by every bridge of one server: the id space, the pending
/// registry, the outbound writer path, and the request timeout.
struct ClientBridgeInner {
    ids: Mutex<RequestIdGenerator>,
    pending: Arc<PendingRequests>,
    outbound_tx: mpsc::UnboundedSender<OutboundEvent>,
    request_timeout: Duration,
    /// The connection's negotiated protocol version (pinned after
    /// `initialize`); fs/terminal client requests fail closed on v2.
    negotiated: Arc<OnceLock<ProtocolVersion>>,
}

/// RAII cleanup: removes a prompt's pending requests when the bridge handle
/// handed to that prompt is dropped (completion, cancellation, or abort).
struct OwnerCleanup {
    pending: Arc<PendingRequests>,
    owner: u64,
}

impl Drop for OwnerCleanup {
    fn drop(&mut self) {
        self.pending.remove_owner(self.owner);
    }
}

/// Cloneable handle for agent → client requests during one prompt turn.
///
/// Clones share the owner id but only the handle handed to the prompt
/// carries the cleanup guard, so every request a prompt issued is removed
/// from the pending registry when that prompt ends — even if a provider
/// subtask outlived the prompt's future.
pub struct ClientBridge {
    inner: Arc<ClientBridgeInner>,
    owner: u64,
    /// The session this prompt's requests belong to (v2 foreground-state
    /// reporting).
    session_id: SessionId,
    cleanup: Option<OwnerCleanup>,
}

impl std::fmt::Debug for ClientBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientBridge")
            .field("owner", &self.owner)
            .field("cleanup_armed", &self.cleanup.is_some())
            .finish_non_exhaustive()
    }
}

impl Clone for ClientBridge {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            owner: self.owner,
            session_id: self.session_id.clone(),
            cleanup: None,
        }
    }
}

/// Test-only bridge: fresh id generator and pending registry backed by a
/// plain outbound channel, so downstream crates can exercise agent → client
/// requests without a running server.
#[cfg(feature = "test-utils")]
impl ClientBridge {
    /// Creates a bridge sharing no server state.
    #[must_use]
    pub fn new_for_test(
        request_timeout: Duration,
        outbound_tx: mpsc::UnboundedSender<OutboundEvent>,
    ) -> Self {
        Self {
            inner: Arc::new(ClientBridgeInner {
                ids: Mutex::new(RequestIdGenerator::new()),
                pending: Arc::new(PendingRequests::new()),
                outbound_tx,
                request_timeout,
                // Unset means the connection never negotiated: v1 behavior.
                negotiated: Arc::new(OnceLock::new()),
            }),
            owner: 1,
            session_id: SessionId::new("test-session"),
            cleanup: None,
        }
    }

    /// Forwards one client response to the pending-request manager.
    ///
    /// The server reader loop normally routes inbound responses here; tests
    /// without a running server use this to answer bridge requests.
    pub fn handle_response(&self, response: Response<Value>) {
        self.inner.pending.handle_response(response);
    }
}

impl ClientBridge {
    /// Sends one JSON-RPC request and awaits its response, bounded by the
    /// configured `request_timeout`.
    ///
    /// The pending entry is always removed: on a matching response, on
    /// timeout, on write failure, or when the owning prompt ends via
    /// `OwnerCleanup`.
    async fn send_request(&self, method: &str, params: Value) -> Result<Value, ProviderError> {
        if V2_REMOVED_CLIENT_METHODS.contains(&method)
            && self.inner.negotiated.get() == Some(&ProtocolVersion::V2)
        {
            return Err(ProviderError::InvalidRequest(format!(
                "{method} was removed in ACP v2; expose client-side tools through MCP servers instead"
            )));
        }
        let id = self.inner.ids.lock().expect("request id generator poisoned").next_id();
        let (sender, receiver) = oneshot::channel();
        self.inner.pending.insert(id.clone(), self.owner, self.session_id.clone(), sender);

        let frame = match RawJsonRpcMessage::request(method.to_string(), params, id.clone()) {
            Ok(frame) => frame,
            Err(error) => {
                self.inner.pending.remove(&id);
                return Err(ProviderError::ClientRequestFailure(format!(
                    "failed to build client request: {error}"
                )));
            }
        };
        if self.inner.outbound_tx.send(OutboundEvent::ClientRequest { frame }).is_err() {
            self.inner.pending.remove(&id);
            return Err(ProviderError::ClientRequestFailure(
                "transport closed while sending client request".into(),
            ));
        }

        match tokio::time::timeout(self.inner.request_timeout, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => {
                // The entry was already removed (owner cleanup, fail_all, or
                // a competing timeout); the request is gone.
                Err(ProviderError::ClientRequestFailure("client request abandoned".into()))
            }
            Err(_) => {
                self.inner.pending.remove(&id);
                Err(ProviderError::ClientRequestFailure(format!(
                    "client request timed out after {:?}",
                    self.inner.request_timeout
                )))
            }
        }
    }

    /// Reads a text file through the client (`fs/read_text_file`).
    ///
    /// Relative paths are rejected before anything is written to the
    /// transport.
    pub async fn read_text_file(
        &self,
        request: ReadTextFileRequest,
    ) -> Result<ReadTextFileResponse, ProviderError> {
        validate_absolute_path(&request.path, "path")?;
        self.send_typed(FS_READ_TEXT_FILE_METHOD_NAME, &request).await
    }

    /// Writes a text file through the client (`fs/write_text_file`).
    ///
    /// Relative paths are rejected before anything is written to the
    /// transport.
    pub async fn write_text_file(
        &self,
        request: WriteTextFileRequest,
    ) -> Result<WriteTextFileResponse, ProviderError> {
        validate_absolute_path(&request.path, "path")?;
        self.send_typed(FS_WRITE_TEXT_FILE_METHOD_NAME, &request).await
    }

    /// Creates a terminal through the client (`terminal/create`).
    ///
    /// A relative working directory is rejected before anything is written
    /// to the transport.
    pub async fn create_terminal(
        &self,
        request: CreateTerminalRequest,
    ) -> Result<CreateTerminalResponse, ProviderError> {
        if let Some(cwd) = &request.cwd {
            validate_absolute_path(cwd, "cwd")?;
        }
        self.send_typed(TERMINAL_CREATE_METHOD_NAME, &request).await
    }

    /// Fetches the current output and status of a terminal
    /// (`terminal/output`).
    pub async fn terminal_output(
        &self,
        request: TerminalOutputRequest,
    ) -> Result<TerminalOutputResponse, ProviderError> {
        self.send_typed(TERMINAL_OUTPUT_METHOD_NAME, &request).await
    }

    /// Waits for a terminal command to exit (`terminal/wait_for_exit`).
    pub async fn wait_for_terminal_exit(
        &self,
        request: WaitForTerminalExitRequest,
    ) -> Result<WaitForTerminalExitResponse, ProviderError> {
        self.send_typed(TERMINAL_WAIT_FOR_EXIT_METHOD_NAME, &request).await
    }

    /// Kills a terminal without releasing it (`terminal/kill`).
    pub async fn kill_terminal(
        &self,
        request: KillTerminalRequest,
    ) -> Result<KillTerminalResponse, ProviderError> {
        self.send_typed(TERMINAL_KILL_METHOD_NAME, &request).await
    }

    /// Releases a terminal and its resources (`terminal/release`).
    pub async fn release_terminal(
        &self,
        request: ReleaseTerminalRequest,
    ) -> Result<ReleaseTerminalResponse, ProviderError> {
        self.send_typed(TERMINAL_RELEASE_METHOD_NAME, &request).await
    }

    /// Creates an elicitation through the client (`elicitation/create`).
    pub async fn create_elicitation(
        &self,
        request: CreateElicitationRequest,
    ) -> Result<CreateElicitationResponse, ProviderError> {
        self.send_typed(ELICITATION_CREATE_METHOD_NAME, &request).await
    }

    /// Connects to a host-advertised MCP server over ACP (`mcp/connect`).
    ///
    /// The returned [`ConnectMcpResponse`] carries the connection id every
    /// subsequent [`ClientBridge::mcp_message`] / [`ClientBridge::mcp_disconnect`]
    /// call must use.  Hosts that did not advertise the server fail closed
    /// with a permission/invalid-params error.
    pub async fn mcp_connect(
        &self,
        request: ConnectMcpRequest,
    ) -> Result<ConnectMcpResponse, ProviderError> {
        self.send_typed(MCP_CONNECT_METHOD_NAME, &request).await
    }

    /// Sends one inner MCP request over an ACP MCP connection
    /// (`mcp/message`) and awaits the inner MCP response.
    ///
    /// The inner response is returned verbatim; MCP protocol failures inside
    /// the response surface as a failed round trip (fail closed), never as a
    /// transport crash.
    pub async fn mcp_message(
        &self,
        request: MessageMcpRequest,
    ) -> Result<MessageMcpResponse, ProviderError> {
        self.send_typed(MCP_MESSAGE_METHOD_NAME, &request).await
    }

    /// Sends one inner MCP notification over an ACP MCP connection
    /// (`mcp/message` notification).  Notifications carry no response.
    ///
    /// # Errors
    ///
    /// Fails when the frame cannot be built or the outbound path is gone.
    pub fn mcp_message_notification(
        &self,
        notification: MessageMcpNotification,
    ) -> Result<(), ProviderError> {
        let params = serde_json::to_value(&notification).map_err(|source| {
            ProviderError::ClientRequestFailure(format!(
                "failed to serialize mcp/message notification: {source}"
            ))
        })?;
        let frame = RawJsonRpcMessage::notification(MCP_MESSAGE_METHOD_NAME.to_string(), params)
            .map_err(|error| {
                ProviderError::ClientRequestFailure(format!(
                    "failed to build mcp/message notification: {error}"
                ))
            })?;
        if self.inner.outbound_tx.send(OutboundEvent::ClientRequest { frame }).is_err() {
            return Err(ProviderError::ClientRequestFailure(
                "transport closed while sending mcp/message notification".into(),
            ));
        }
        Ok(())
    }

    /// Closes an ACP MCP connection (`mcp/disconnect`).  Idempotent from the
    /// client's perspective: unknown connection ids fail closed on the host.
    pub async fn mcp_disconnect(
        &self,
        request: DisconnectMcpRequest,
    ) -> Result<DisconnectMcpResponse, ProviderError> {
        self.send_typed(MCP_DISCONNECT_METHOD_NAME, &request).await
    }

    /// Sends a typed request and decodes the typed response.
    async fn send_typed<T: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        request: &T,
    ) -> Result<R, ProviderError> {
        let params = serde_json::to_value(request).map_err(|source| {
            ProviderError::ClientRequestFailure(format!(
                "failed to serialize client request: {source}"
            ))
        })?;
        let response = self.send_request(method, params).await?;
        serde_json::from_value(response).map_err(|source| {
            ProviderError::ClientRequestFailure(format!("invalid client response: {source}"))
        })
    }
}

/// Creates per-prompt bridges sharing one server's id space, pending
/// registry, outbound path, and request timeout.
#[derive(Clone)]
pub(crate) struct ClientBridgeFactory {
    inner: Arc<ClientBridgeInner>,
    next_owner: Arc<Mutex<u64>>,
}

impl ClientBridgeFactory {
    pub(crate) fn new(
        ids: Mutex<RequestIdGenerator>,
        pending: Arc<PendingRequests>,
        outbound_tx: mpsc::UnboundedSender<OutboundEvent>,
        request_timeout: Duration,
        negotiated: Arc<OnceLock<ProtocolVersion>>,
    ) -> Self {
        Self {
            inner: Arc::new(ClientBridgeInner {
                ids,
                pending,
                outbound_tx,
                request_timeout,
                negotiated,
            }),
            next_owner: Arc::new(Mutex::new(1)),
        }
    }

    /// Creates the bridge for one prompt turn serving `session_id`.  Every
    /// prompt gets a fresh owner id so its requests die with it.
    pub(crate) fn bridge_for(&self, session_id: &SessionId) -> ClientBridge {
        let mut next = self.next_owner.lock().expect("bridge owner counter poisoned");
        let owner = *next;
        *next += 1;
        ClientBridge {
            inner: self.inner.clone(),
            owner,
            session_id: session_id.clone(),
            cleanup: Some(OwnerCleanup { pending: self.inner.pending.clone(), owner }),
        }
    }
}

fn validate_absolute_path(path: &std::path::Path, what: &str) -> Result<(), ProviderError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(ProviderError::InvalidRequest(format!(
            "{what} must be an absolute path: {}",
            path.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ee_agent_protocol::{McpConnectionId, RawJsonRpcParams};
    use serde_json::json;

    fn test_bridge(
        request_timeout: Duration,
    ) -> (ClientBridge, mpsc::UnboundedReceiver<OutboundEvent>) {
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let pending = Arc::new(PendingRequests::new());
        let inner = Arc::new(ClientBridgeInner {
            ids: Mutex::new(RequestIdGenerator::new()),
            pending,
            outbound_tx,
            request_timeout,
            negotiated: Arc::new(OnceLock::new()),
        });
        (
            ClientBridge {
                inner,
                owner: 1,
                session_id: SessionId::new("test-session"),
                cleanup: None,
            },
            outbound_rx,
        )
    }

    async fn wait_until(check: impl Fn() -> bool, what: &str) {
        for _ in 0..5_000 {
            if check() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("condition not met within budget: {what}");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn timeout_removes_pending_entry() {
        let (bridge, _outbound_rx) = test_bridge(Duration::from_millis(50));
        let task = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.send_request("fs/read_text_file", json!({ "sessionId": "s" })).await }
        });

        tokio::time::advance(Duration::from_millis(100)).await;
        let error = task.await.expect("task joins").expect_err("must time out");
        assert!(
            matches!(error, ProviderError::ClientRequestFailure(ref reason) if reason.contains("timed out")),
            "{error:?}"
        );
        assert_eq!(bridge.inner.pending.len(), 0, "no pending entry may remain after timeout");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fail_all_resolves_pending_entries() {
        let (bridge, _outbound_rx) = test_bridge(Duration::from_secs(60));
        let task = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.send_request("fs/read_text_file", json!({})).await }
        });
        wait_until(|| bridge.inner.pending.len() == 1, "pending entry inserted").await;

        bridge
            .inner
            .pending
            .fail_all(ProviderError::ClientRequestFailure("transport closed".into()));
        let error = task.await.expect("task joins").expect_err("must fail");
        assert!(
            matches!(error, ProviderError::ClientRequestFailure(ref reason) if reason == "transport closed"),
            "{error:?}"
        );
        assert_eq!(bridge.inner.pending.len(), 0, "fail_all must clear the registry");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_prompt_bridge_removes_pending_entries() {
        let (outbound_tx, _outbound_rx) = mpsc::unbounded_channel();
        let pending = Arc::new(PendingRequests::new());
        let factory = ClientBridgeFactory::new(
            Mutex::new(RequestIdGenerator::new()),
            pending.clone(),
            outbound_tx,
            Duration::from_secs(60),
            Arc::new(OnceLock::new()),
        );
        let prompt_bridge = factory.bridge_for(&SessionId::new("session-a"));
        let task = tokio::spawn({
            let clone = prompt_bridge.clone();
            async move { clone.send_request("fs/read_text_file", json!({})).await }
        });
        wait_until(|| pending.len() == 1, "pending entry inserted").await;

        // The prompt finished (or was cancelled): its bridge handle drops
        // and every request it owned is cleaned up.
        drop(prompt_bridge);
        assert_eq!(pending.len(), 0, "prompt-scoped requests must not outlive the prompt");

        // The blocked call observes the abandonment and resolves.
        let error = task.await.expect("task joins").expect_err("must fail");
        assert!(
            matches!(error, ProviderError::ClientRequestFailure(ref reason) if reason.contains("abandoned")),
            "{error:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_response_resolves_matching_pending_request() {
        let (bridge, _outbound_rx) = test_bridge(Duration::from_secs(60));
        let task = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.send_request("fs/read_text_file", json!({})).await }
        });
        wait_until(|| bridge.inner.pending.len() == 1, "pending entry inserted").await;

        // The client answers request id 1; the pending request resolves and
        // is removed.
        bridge.inner.pending.handle_response(Response::Result {
            id: RequestId::Number(1),
            result: json!({ "content": "hello" }),
        });
        let result = task.await.expect("task joins").expect("resolves");
        assert_eq!(result["content"], "hello");
        assert_eq!(bridge.inner.pending.len(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_response_ignores_unknown_request_ids() {
        let (bridge, _outbound_rx) = test_bridge(Duration::from_secs(60));
        bridge.inner.pending.handle_response(Response::Result {
            id: RequestId::Number(4242),
            result: json!({ "content": "late" }),
        });
        assert_eq!(bridge.inner.pending.len(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn relative_read_path_fails_before_request_is_sent() {
        let (bridge, mut outbound_rx) = test_bridge(Duration::from_secs(60));
        let request = ReadTextFileRequest::new("session-a", std::path::PathBuf::from("rel/file"));
        let error = bridge.read_text_file(request).await.expect_err("must reject");
        assert!(
            matches!(error, ProviderError::InvalidRequest(ref reason) if reason.contains("path must be an absolute path")),
            "{error:?}"
        );
        assert!(outbound_rx.try_recv().is_err(), "no request may be queued for a relative path");
    }

    /// Pins the negotiated version on a test bridge (the server does this via
    /// `initialize`).
    fn pin_negotiated(bridge: &ClientBridge, version: ProtocolVersion) {
        let _ = bridge.inner.negotiated.set(version);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn removed_client_methods_fail_closed_on_v2_before_sending() {
        let (bridge, mut outbound_rx) = test_bridge(Duration::from_secs(60));
        pin_negotiated(&bridge, ProtocolVersion::V2);

        // The entire fs/terminal client surface is removed in v2 (guide).
        for method in
            ["fs/read_text_file", "fs/write_text_file", "terminal/create", "terminal/kill"]
        {
            let error = bridge.send_request(method, json!({ "sessionId": "s" })).await.unwrap_err();
            assert!(
                matches!(error, ProviderError::InvalidRequest(ref reason) if reason.contains("removed in ACP v2")),
                "{method} must fail closed on v2: {error:?}"
            );
        }
        assert!(
            outbound_rx.try_recv().is_err(),
            "no removed-surface request may reach the transport on v2"
        );

        // v2-native methods still flow.
        let task = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.send_request("mcp/connect", json!({ "connectionId": "c" })).await }
        });
        let _ = outbound_rx.recv().await.expect("v2-native method still sends");
        drop(task);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn removed_client_methods_still_flow_before_negotiation() {
        // An unset negotiation is the v1 behavior: fs requests keep flowing
        // (v1 clients that advertised the surface still serve them).
        let (bridge, mut outbound_rx) = test_bridge(Duration::from_secs(60));
        let task = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.send_request("fs/read_text_file", json!({})).await }
        });
        let frame = outbound_rx.recv().await.expect("v1 fs request still sends");
        let OutboundEvent::ClientRequest { frame } = frame else {
            panic!("expected a client request frame");
        };
        let RawJsonRpcMessage::Request(request) = frame else {
            panic!("expected a request frame");
        };
        assert_eq!(request.method.as_ref(), "fs/read_text_file");
        drop(task);
    }

    /// The JSON params of an outbound request/notification frame.
    fn raw_params(params: &Option<RawJsonRpcParams>) -> Option<serde_json::Value> {
        match params {
            None => None,
            Some(RawJsonRpcParams::Object(map)) => Some(serde_json::Value::Object(map.clone())),
            Some(RawJsonRpcParams::Array(array)) => Some(serde_json::Value::Array(array.clone())),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mcp_connect_round_trips_through_the_bridge() {
        let (bridge, mut outbound_rx) = test_bridge(Duration::from_secs(60));
        let request = ConnectMcpRequest::new("ee-mcp-proxy:test");
        let task = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.mcp_connect(request).await }
        });

        let frame = outbound_rx.recv().await.expect("mcp/connect was sent");
        let OutboundEvent::ClientRequest { frame } = frame else {
            panic!("expected a client request frame");
        };
        let RawJsonRpcMessage::Request(request) = frame else {
            panic!("expected a request frame, got {frame:?}");
        };
        assert_eq!(request.method.as_ref(), ee_agent_protocol::registry::MCP_CONNECT_METHOD_NAME);
        let params = raw_params(&request.params).expect("params present");
        assert_eq!(params["serverId"], "ee-mcp-proxy:test");
        bridge.handle_response(Response::Result {
            id: request.id,
            result: json!({ "connectionId": "conn-1" }),
        });

        let response = task.await.expect("task joins").expect("connect resolves");
        assert_eq!(response.connection_id, McpConnectionId::new("conn-1"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mcp_message_round_trips_inner_result_verbatim() {
        let (bridge, mut outbound_rx) = test_bridge(Duration::from_secs(60));
        let request = MessageMcpRequest::new("conn-1", "tools/list")
            .params(serde_json::Map::from_iter([("_meta".to_string(), json!({}))]));
        let task = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.mcp_message(request).await }
        });

        let frame = outbound_rx.recv().await.expect("mcp/message was sent");
        let OutboundEvent::ClientRequest { frame } = frame else {
            panic!("expected a client request frame");
        };
        let RawJsonRpcMessage::Request(request) = frame else {
            panic!("expected a request frame, got {frame:?}");
        };
        assert_eq!(request.method.as_ref(), ee_agent_protocol::registry::MCP_MESSAGE_METHOD_NAME);
        let params = raw_params(&request.params).expect("params present");
        assert_eq!(params["connectionId"], "conn-1");
        assert_eq!(params["method"], "tools/list");
        bridge.handle_response(Response::Result {
            id: request.id,
            result: json!({ "resultType": "complete", "tools": [] }),
        });

        let response = task.await.expect("task joins").expect("message resolves");
        let inner: serde_json::Value =
            serde_json::from_str(response.0.get()).expect("inner result parses");
        assert_eq!(inner["resultType"], "complete");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mcp_message_notification_is_a_fire_and_forget_notification() {
        let (bridge, mut outbound_rx) = test_bridge(Duration::from_secs(60));
        let notification = MessageMcpNotification::new("conn-1", "notifications/initialized");

        bridge.mcp_message_notification(notification).expect("queues");

        let frame = outbound_rx.recv().await.expect("notification was sent");
        let OutboundEvent::ClientRequest { frame } = frame else {
            panic!("expected a client request frame");
        };
        let RawJsonRpcMessage::Notification(notification) = frame else {
            panic!("expected a notification frame, got {frame:?}");
        };
        assert_eq!(
            notification.method.as_ref(),
            ee_agent_protocol::registry::MCP_MESSAGE_METHOD_NAME
        );
        let params = raw_params(&notification.params).expect("params present");
        assert_eq!(params["connectionId"], "conn-1");
        assert_eq!(params["method"], "notifications/initialized");
        assert!(outbound_rx.try_recv().is_err(), "notifications are not answered");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mcp_disconnect_round_trips_through_the_bridge() {
        let (bridge, mut outbound_rx) = test_bridge(Duration::from_secs(60));
        let request = DisconnectMcpRequest::new("conn-1");
        let task = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.mcp_disconnect(request).await }
        });

        let frame = outbound_rx.recv().await.expect("mcp/disconnect was sent");
        let OutboundEvent::ClientRequest { frame } = frame else {
            panic!("expected a client request frame");
        };
        let RawJsonRpcMessage::Request(request) = frame else {
            panic!("expected a request frame, got {frame:?}");
        };
        assert_eq!(
            request.method.as_ref(),
            ee_agent_protocol::registry::MCP_DISCONNECT_METHOD_NAME
        );
        bridge.handle_response(Response::Result { id: request.id, result: json!({}) });

        task.await.expect("task joins").expect("disconnect resolves");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mcp_request_client_error_maps_to_provider_failure() {
        let (bridge, mut outbound_rx) = test_bridge(Duration::from_secs(60));
        let task = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.mcp_connect(ConnectMcpRequest::new("unknown")).await }
        });

        let frame = outbound_rx.recv().await.expect("mcp/connect was sent");
        let OutboundEvent::ClientRequest { frame } = frame else {
            panic!("expected a client request frame");
        };
        let RawJsonRpcMessage::Request(request) = frame else {
            panic!("expected a request frame");
        };
        bridge.handle_response(Response::Error {
            id: request.id,
            error: RpcError::invalid_params()
                .data(serde_json::json!({ "reason": "unknown MCP server id" })),
        });

        let error = task.await.expect("task joins").expect_err("fails closed");
        assert!(
            matches!(error, ProviderError::ClientRequestFailure(ref reason) if reason.contains("Invalid params: unknown MCP server id")),
            "{error:?}"
        );
    }
}
