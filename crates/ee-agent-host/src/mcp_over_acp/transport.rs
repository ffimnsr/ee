//! rmcp transport plumbing between the SDK dispatch and the rmcp server loop.
use super::*;

///
/// Boxed so the small `Closed` marker does not carry the large message
/// variant's size.
pub(crate) enum PendingReply {
    Message(Box<TxServerMessage>),
    Closed,
}

/// Inner MCP lifecycle bootstrap probe.  rmcp answers this request without
/// selecting a lifecycle, so it does not end the pre-initialize window.
const MCP_LIFECYCLE_PROBE_METHOD: &str = "server/discover";

/// Cap on inner notifications held back before the lifecycle is selected.
const MCP_DEFERRED_NOTIFICATION_CAP: usize = 64;

/// Pre-lifecycle forwarding state for one logical connection.
///
/// rmcp's serve loop negotiates the inner MCP lifecycle before it dispatches
/// anything: it only leaves that window once it reads an `initialize` request
/// or the first request that is not the `server/discover` bootstrap probe.
/// Any other message read in the window is fatal, so notifications are held
/// back until the lifecycle is selected instead of tearing the connection
/// down.
#[derive(Default)]
struct LifecycleGate {
    /// Whether a lifecycle-selecting request has been forwarded.
    selected: bool,
    /// Notifications received before the lifecycle was selected, in arrival
    /// order.
    deferred: Vec<RxJsonRpcMessage<RoleServer>>,
}

/// One logical MCP-over-ACP connection (the server side the agent talks to).
pub(crate) struct LogicalConnection {
    /// Pushes inner MCP messages into the rmcp serve loop (`receive` side).
    pub(super) rx_tx: mpsc::UnboundedSender<RxJsonRpcMessage<RoleServer>>,
    /// Inner responses from rmcp keyed by inner request id.
    pub(super) pending: Arc<Mutex<HashMap<RequestId, oneshot::Sender<PendingReply>>>>,
    /// Cancels the serve loop on disconnect/close.
    pub(super) shutdown: CancellationToken,
    /// Next inner MCP request id for this connection.
    pub(super) next_inner_id: AtomicI64,
    /// Pre-lifecycle forwarding gate (see [`LifecycleGate`]).
    lifecycle: Mutex<LifecycleGate>,
}

impl LogicalConnection {
    /// Builds a fresh logical connection.
    pub(super) fn new(
        rx_tx: mpsc::UnboundedSender<RxJsonRpcMessage<RoleServer>>,
        pending: Arc<Mutex<HashMap<RequestId, oneshot::Sender<PendingReply>>>>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            rx_tx,
            pending,
            shutdown,
            next_inner_id: AtomicI64::new(0),
            lifecycle: Mutex::new(LifecycleGate::default()),
        }
    }

    /// Forwards an inner MCP request into the serve loop.
    ///
    /// Once the first lifecycle-selecting request is in, any notifications
    /// held back are flushed behind it so arrival order is preserved.  Returns
    /// `false` when the serve loop is gone.
    pub(super) fn send_request(&self, method: &str, message: RxJsonRpcMessage<RoleServer>) -> bool {
        let mut gate = self.lifecycle.lock().expect("mcp lifecycle gate poisoned");
        if self.rx_tx.send(message).is_err() {
            return false;
        }
        if !gate.selected && method != MCP_LIFECYCLE_PROBE_METHOD {
            gate.selected = true;
            for deferred in gate.deferred.drain(..) {
                if self.rx_tx.send(deferred).is_err() {
                    break;
                }
            }
        }
        true
    }

    /// Forwards an inner MCP notification into the serve loop, or holds it
    /// until the lifecycle is selected.
    pub(super) fn send_notification(&self, message: RxJsonRpcMessage<RoleServer>) {
        let mut gate = self.lifecycle.lock().expect("mcp lifecycle gate poisoned");
        if !gate.selected {
            if gate.deferred.len() < MCP_DEFERRED_NOTIFICATION_CAP {
                gate.deferred.push(message);
            } else {
                tracing::warn!("dropping mcp-over-acp notification: pre-lifecycle buffer is full");
            }
            return;
        }
        if self.rx_tx.send(message).is_err() {
            tracing::debug!("dropping mcp/message notification: connection closed");
        }
    }

    /// Resolves every pending inner request as closed and cancels the serve
    /// loop; the serve thread then exits on its own (detached).
    pub(super) fn close(&self) {
        self.shutdown.cancel();
        self.lifecycle.lock().expect("mcp lifecycle gate poisoned").deferred.clear();
        let pendings: Vec<oneshot::Sender<PendingReply>> = {
            let mut guard = self.pending.lock().expect("pending map poisoned");
            guard.drain().map(|(_, tx)| tx).collect()
        };
        for tx in pendings {
            let _ = tx.send(PendingReply::Closed);
        }
    }
}

///
/// `receive` pops inner client→server messages pushed by the `mcp/message`
/// handlers; `send` correlates rmcp responses back to the awaiting
/// `mcp/message` responder by inner request id.  rmcp never sends
/// server-initiated messages for the ee proxy (fixed tool list), so every
/// outbound item is a response to an inner request.
pub(crate) struct McpOverAcpTransport {
    pub(super) rx: mpsc::UnboundedReceiver<RxJsonRpcMessage<RoleServer>>,
    pub(super) pending: Arc<Mutex<HashMap<RequestId, oneshot::Sender<PendingReply>>>>,
}

impl Transport<RoleServer> for McpOverAcpTransport {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleServer>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        let pending = self.pending.clone();
        let item_id = match &item {
            TxServerMessage::Response(response) => Some(response.id.clone()),
            TxServerMessage::Error(error) => error.id.clone(),
            _ => None,
        };
        std::future::ready(match item_id {
            Some(id) => {
                let tx = pending.lock().expect("pending map poisoned").remove(&id);
                match tx {
                    Some(tx) => {
                        let _ = tx.send(PendingReply::Message(Box::new(item)));
                        Ok(())
                    }
                    None => {
                        // Response for a request that was cancelled/closed.
                        tracing::debug!(?id, "dropping stale mcp-over-acp response");
                        Ok(())
                    }
                }
            }
            None => Ok(()),
        })
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleServer>> {
        self.rx.recv().await
    }

    fn close(&mut self) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        std::future::ready(Ok(()))
    }
}
