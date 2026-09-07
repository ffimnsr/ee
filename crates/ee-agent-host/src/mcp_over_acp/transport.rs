//! rmcp transport plumbing between the SDK dispatch and the rmcp server loop.
use super::*;

///
/// Boxed so the small `Closed` marker does not carry the large message
/// variant's size.
pub(crate) enum PendingReply {
    Message(Box<TxServerMessage>),
    Closed,
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
}

impl LogicalConnection {
    /// Resolves every pending inner request as closed and cancels the serve
    /// loop; the serve thread then exits on its own (detached).
    pub(super) fn close(&self) {
        self.shutdown.cancel();
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
