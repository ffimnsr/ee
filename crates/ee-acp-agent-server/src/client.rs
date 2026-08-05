//! Agent → client request bridge.
//!
//! Providers call typed [`ClientBridge`] methods (fs, terminal, elicitation)
//! during a prompt turn.  The framework owns the outbound JSON-RPC request,
//! id allocation, response correlation, timeouts, and cleanup:
//!
//! - Requests flow through the server's FIFO outbound channel, so they share
//!   the single transport writer path with updates and prompt responses.
//! - The client's response is routed back by the server run loop through
//!   [`PendingRequests::handle_response`], which resolves the matching
//!   pending oneshot by request id.
//! - Pending entries are removed on a matching response, on timeout, on
//!   write failure, when the owning prompt ends ([`OwnerCleanup`]), and when
//!   the transport closes ([`PendingRequests::fail_all`]) — a request never
//!   outlives its prompt, a timeout, or the connection.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ee_agent_protocol::registry::{
    ELICITATION_CREATE_METHOD_NAME, FS_READ_TEXT_FILE_METHOD_NAME, FS_WRITE_TEXT_FILE_METHOD_NAME,
    TERMINAL_CREATE_METHOD_NAME, TERMINAL_KILL_METHOD_NAME, TERMINAL_OUTPUT_METHOD_NAME,
    TERMINAL_RELEASE_METHOD_NAME, TERMINAL_WAIT_FOR_EXIT_METHOD_NAME,
};
use ee_agent_protocol::{
    CreateElicitationRequest, CreateElicitationResponse, CreateTerminalRequest,
    CreateTerminalResponse, Error as RpcError, KillTerminalRequest, KillTerminalResponse,
    RawJsonRpcMessage, ReadTextFileRequest, ReadTextFileResponse, ReleaseTerminalRequest,
    ReleaseTerminalResponse, RequestId, Response, TerminalOutputRequest, TerminalOutputResponse,
    WaitForTerminalExitRequest, WaitForTerminalExitResponse, WriteTextFileRequest,
    WriteTextFileResponse,
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
    /// The prompt that issued the request; entries are cleaned up when the
    /// owning prompt ends.
    owner: u64,
    /// Resolved by [`PendingRequests::handle_response`], a timeout, or
    /// [`PendingRequests::fail_all`].
    sender: oneshot::Sender<Result<Value, ProviderError>>,
}

/// Registry of in-flight agent → client requests, keyed by request id.
///
/// Shared by the server run loop (which routes inbound responses here) and
/// every [`ClientBridge`] (which inserts and awaits requests).
#[derive(Default)]
pub(crate) struct PendingRequests {
    inner: Mutex<std::collections::HashMap<RequestId, PendingRequest>>,
}

impl PendingRequests {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Registers a pending request before it is written to the transport.
    pub(crate) fn insert(
        &self,
        id: RequestId,
        owner: u64,
        sender: oneshot::Sender<Result<Value, ProviderError>>,
    ) {
        self.inner
            .lock()
            .expect("pending requests poisoned")
            .insert(id, PendingRequest { owner, sender });
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
        self.inner.lock().expect("pending requests poisoned").remove(id);
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
            let _ = entry.sender.send(Err(reason.clone()));
        }
    }

    /// Removes every pending request owned by one prompt (its bridge handle
    /// was dropped: prompt finished, was cancelled, or was aborted).
    pub(crate) fn remove_owner(&self, owner: u64) {
        self.inner
            .lock()
            .expect("pending requests poisoned")
            .retain(|_, entry| entry.owner != owner);
    }

    fn resolve(&self, id: RequestId, result: Result<Value, ProviderError>) {
        let Some(entry) = self.inner.lock().expect("pending requests poisoned").remove(&id) else {
            tracing::debug!(%id, "ignoring response for unknown request id");
            return;
        };
        let _ = entry.sender.send(result);
    }
}

/// Maps a client's JSON-RPC error onto the provider-visible error: permission
/// denials and cancellations keep their meaning; everything else is a
/// client-request failure carrying the client's message and code.
fn client_error_to_provider(error: RpcError) -> ProviderError {
    match i32::from(error.code) {
        CODE_PERMISSION_DENIED => ProviderError::PermissionDenied(error.message),
        CODE_REQUEST_CANCELLED => ProviderError::Cancellation,
        _ => ProviderError::ClientRequestFailure(format!(
            "{} (jsonrpc code {})",
            error.message,
            i32::from(error.code)
        )),
    }
}

/// State shared by every bridge of one server: the id space, the pending
/// registry, the outbound writer path, and the request timeout.
struct ClientBridgeInner {
    ids: Mutex<RequestIdGenerator>,
    pending: Arc<PendingRequests>,
    outbound_tx: mpsc::UnboundedSender<OutboundEvent>,
    request_timeout: Duration,
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
        Self { inner: self.inner.clone(), owner: self.owner, cleanup: None }
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
            }),
            owner: 1,
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
    /// [`OwnerCleanup`].
    async fn send_request(&self, method: &str, params: Value) -> Result<Value, ProviderError> {
        let id = self.inner.ids.lock().expect("request id generator poisoned").next_id();
        let (sender, receiver) = oneshot::channel();
        self.inner.pending.insert(id.clone(), self.owner, sender);

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
    ) -> Self {
        Self {
            inner: Arc::new(ClientBridgeInner { ids, pending, outbound_tx, request_timeout }),
            next_owner: Arc::new(Mutex::new(1)),
        }
    }

    /// Creates the bridge for one prompt turn.  Every prompt gets a fresh
    /// owner id so its requests die with it.
    pub(crate) fn bridge(&self) -> ClientBridge {
        let mut next = self.next_owner.lock().expect("bridge owner counter poisoned");
        let owner = *next;
        *next += 1;
        ClientBridge {
            inner: self.inner.clone(),
            owner,
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
        });
        (ClientBridge { inner, owner: 1, cleanup: None }, outbound_rx)
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
        );
        let prompt_bridge = factory.bridge();
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
}
