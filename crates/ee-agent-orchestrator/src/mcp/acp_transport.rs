//! ACP-native MCP-over-ACP client transport (Phase 12).
//!
//! The agent is the MCP client; the host serves the `ee` proxy's MCP server
//! over ACP `mcp/connect` / `mcp/message` / `mcp/disconnect` requests.  This
//! module is the client-side mirror of the host's `McpOverAcpTransport`
//! (`ee-agent-host/src/mcp_over_acp.rs`): it implements rmcp's
//! [`Transport`] for [`RoleClient`] over the framework's
//! [`ClientBridge`].  No ACP or MCP wire structs are handrolled — inner
//! messages use rmcp's `JsonRpcMessage`/`ServerResult` model and the
//! official SDK request/response types.
//!
//! Correlation: the host generates the inner JSON-RPC id itself, so replies
//! cannot be matched by id; each `mcp/message` ACP request answers exactly
//! one inner request, so replies are delivered in order, each tagged with
//! the client request id that produced it.

use std::future::Future;
use std::time::Duration;

use ee_acp_agent_server::{ClientBridge, ProviderError};
use ee_agent_protocol::{
    ConnectMcpRequest, DisconnectMcpRequest, McpConnectionId, McpServerAcpId,
    MessageMcpNotification, MessageMcpRequest,
};
use rmcp::model::{JsonRpcMessage, ServerResult};
use rmcp::service::{RoleClient, RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use serde_json::Value;
use tokio::sync::mpsc;

/// Transport-level failure of the ACP MCP bridge.
#[derive(Debug)]
pub(crate) enum McpBridgeError {
    /// The ACP round trip failed (host error, timeout, or closed transport).
    Bridge(ProviderError),
    /// The inner MCP response could not be decoded.
    InvalidResponse(String),
    /// The `mcp/connect` handshake exceeded its budget.
    ConnectTimeout,
}

impl std::fmt::Display for McpBridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bridge(error) => write!(f, "ACP mcp bridge error: {error}"),
            Self::InvalidResponse(reason) => write!(f, "invalid inner MCP response: {reason}"),
            Self::ConnectTimeout => f.write_str("mcp/connect timed out"),
        }
    }
}

impl std::error::Error for McpBridgeError {}

impl From<ProviderError> for McpBridgeError {
    fn from(error: ProviderError) -> Self {
        Self::Bridge(error)
    }
}

/// The inner JSON-RPC messages an rmcp client sends to its peer.
type TxClientMessage = TxJsonRpcMessage<RoleClient>;
/// The inner JSON-RPC messages an rmcp client receives from its peer.
type RxClientMessage = RxJsonRpcMessage<RoleClient>;

/// rmcp client transport over one ACP `mcp/connect` connection.
#[derive(Debug)]
pub(crate) struct AcpBridgeTransport {
    bridge: ClientBridge,
    connection_id: McpConnectionId,
    replies: mpsc::UnboundedSender<RxClientMessage>,
    replies_rx: mpsc::UnboundedReceiver<RxClientMessage>,
}

impl AcpBridgeTransport {
    /// Runs `mcp/connect`; the returned transport is ready for
    /// `serve_with_lifecycle`.
    ///
    /// # Errors
    ///
    /// Fails when the host rejects the server id or the round trip exceeds
    /// `timeout`.
    pub(crate) async fn connect(
        bridge: &ClientBridge,
        server_id: &McpServerAcpId,
        timeout: Duration,
    ) -> Result<Self, McpBridgeError> {
        let response = tokio::time::timeout(
            timeout,
            bridge.mcp_connect(ConnectMcpRequest::new(server_id.clone())),
        )
        .await
        .map_err(|_| McpBridgeError::ConnectTimeout)??;
        let (replies, replies_rx) = mpsc::unbounded_channel();
        Ok(Self {
            bridge: bridge.clone(),
            connection_id: response.connection_id,
            replies,
            replies_rx,
        })
    }

    /// The ACP connection id, for explicit `mcp/disconnect` on failed
    /// handshakes.
    #[must_use]
    pub(crate) fn connection_id(&self) -> McpConnectionId {
        self.connection_id.clone()
    }
}

/// Extracts the inner `method` and optional `params` from a serialized
/// client request/notification payload.
fn extract_method_params(
    value: &Value,
) -> Result<(String, Option<serde_json::Map<String, Value>>), McpBridgeError> {
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| McpBridgeError::InvalidResponse("inner message has no method".into()))?
        .to_string();
    let params = value.get("params").and_then(Value::as_object).cloned();
    Ok((method, params))
}

impl Transport<RoleClient> for AcpBridgeTransport {
    type Error = McpBridgeError;

    fn send(
        &mut self,
        item: TxClientMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let bridge = self.bridge.clone();
        let connection_id = self.connection_id.clone();
        let replies = self.replies.clone();
        async move {
            match item {
                JsonRpcMessage::Request(request) => {
                    let id = request.id.clone();
                    let envelope = serde_json::to_value(&request.request).map_err(|error| {
                        McpBridgeError::InvalidResponse(format!(
                            "inner request serialization: {error}"
                        ))
                    })?;
                    let (method, params) = extract_method_params(&envelope)?;
                    let reply = bridge
                        .mcp_message(MessageMcpRequest::new(connection_id, method).params(params))
                        .await?;
                    let result =
                        serde_json::from_str::<ServerResult>(reply.0.get()).map_err(|error| {
                            McpBridgeError::InvalidResponse(format!("inner MCP result: {error}"))
                        })?;
                    let _ = replies.send(JsonRpcMessage::response(result, id));
                    Ok(())
                }
                JsonRpcMessage::Notification(notification) => {
                    let envelope =
                        serde_json::to_value(&notification.notification).map_err(|error| {
                            McpBridgeError::InvalidResponse(format!(
                                "inner notification serialization: {error}"
                            ))
                        })?;
                    let (method, params) = extract_method_params(&envelope)?;
                    bridge
                        .mcp_message_notification(
                            MessageMcpNotification::new(connection_id, method).params(params),
                        )
                        .map_err(McpBridgeError::Bridge)
                }
                other => {
                    // The ee proxy never initiates server→client requests,
                    // so a client response here is a protocol violation.
                    tracing::debug!(?other, "ignoring unexpected client transport message");
                    Ok(())
                }
            }
        }
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxClientMessage>> + Send {
        self.replies_rx.recv()
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let bridge = self.bridge.clone();
        let connection_id = self.connection_id.clone();
        async move {
            // Best-effort: the host treats unknown connection ids as an
            // error, so a failed disconnect (host already closed) is fine.
            let _ = bridge.mcp_disconnect(DisconnectMcpRequest::new(connection_id)).await;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ee_acp_agent_server::server::OutboundEvent;
    use ee_agent_protocol::{Error as RpcError, RawJsonRpcMessage, RawJsonRpcParams, Response};
    use rmcp::model::{ClientNotification, ClientRequest, RequestId, ServerResult};
    use serde_json::{Value, json};
    use tokio::sync::mpsc;

    use super::*;

    /// A scripted host answering `mcp/connect` and `mcp/message` rounds.
    struct FakeAcpHost {
        bridge: ClientBridge,
        rx: mpsc::UnboundedReceiver<OutboundEvent>,
        /// Inner `mcp/message` method → result.
        results: std::collections::HashMap<String, Value>,
        /// Requests logged as `method: params`.
        log: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl FakeAcpHost {
        fn new(bridge: ClientBridge, rx: mpsc::UnboundedReceiver<OutboundEvent>) -> Self {
            Self {
                bridge,
                rx,
                results: std::collections::HashMap::new(),
                log: Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }

        /// Clone of the shared request log.
        fn log_arc(&self) -> Arc<std::sync::Mutex<Vec<String>>> {
            self.log.clone()
        }

        fn answer(&mut self, method: &str, result: Value) {
            self.results.insert(method.to_string(), result);
        }

        async fn serve_next(&mut self) {
            let frame = self.rx.recv().await.expect("a frame was sent");
            let OutboundEvent::ClientRequest { frame } = frame else {
                panic!("expected a client request frame");
            };
            match frame {
                RawJsonRpcMessage::Request(request) => {
                    let params = match request.params {
                        None => Value::Null,
                        Some(RawJsonRpcParams::Object(map)) => Value::Object(map),
                        Some(RawJsonRpcParams::Array(array)) => Value::Array(array),
                    };
                    let method = request.method.to_string();
                    let result = match self.results.get(&method).cloned() {
                        Some(result) => Response::Result { id: request.id, result },
                        None => Response::Error {
                            id: request.id,
                            error: RpcError::method_not_found().data(
                                serde_json::json!({ "reason": format!("unhandled {method}") }),
                            ),
                        },
                    };
                    self.log.lock().expect("log poisoned").push(format!("{method}: {params}"));
                    self.bridge.handle_response(result);
                }
                RawJsonRpcMessage::Notification(notification) => {
                    let params = match notification.params {
                        None => Value::Null,
                        Some(RawJsonRpcParams::Object(map)) => Value::Object(map),
                        Some(RawJsonRpcParams::Array(array)) => Value::Array(array),
                    };
                    self.log
                        .lock()
                        .expect("log poisoned")
                        .push(format!("notification: {}: {params}", notification.method));
                }
                RawJsonRpcMessage::Response(_) => {}
            }
        }
    }

    fn test_bridge() -> (ClientBridge, mpsc::UnboundedReceiver<OutboundEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (ClientBridge::new_for_test(Duration::from_secs(60), tx), rx)
    }

    #[tokio::test]
    async fn connect_sends_mcp_connect_and_keeps_connection_id() {
        let (bridge, rx) = test_bridge();
        let mut host = FakeAcpHost::new(bridge.clone(), rx);
        host.answer("mcp/connect", json!({ "connectionId": "conn-1" }));
        let host_task = tokio::spawn(async move { host.serve_next().await });

        let transport = AcpBridgeTransport::connect(
            &bridge,
            &McpServerAcpId::new("ee-mcp-proxy:test"),
            Duration::from_secs(5),
        )
        .await
        .expect("connects");
        host_task.await.expect("host joins");
        assert_eq!(transport.connection_id().to_string(), "conn-1");
    }

    #[tokio::test]
    async fn connect_timeout_fails_closed() {
        let (bridge, _rx) = test_bridge();
        let server_id = McpServerAcpId::new("ee-mcp-proxy:test");
        let error = AcpBridgeTransport::connect(&bridge, &server_id, Duration::from_millis(10))
            .await
            .expect_err("times out");
        assert!(matches!(error, McpBridgeError::ConnectTimeout), "{error:?}");
    }

    #[tokio::test]
    async fn request_round_trips_through_mcp_message_v2() {
        let (bridge, rx) = test_bridge();
        let mut host = FakeAcpHost::new(bridge.clone(), rx);
        host.answer("mcp/connect", json!({ "connectionId": "conn-1" }));
        host.answer("mcp/message", json!({ "resultType": "complete", "tools": [] }));
        let bridge_for_spawn = bridge.clone();
        let server_id = McpServerAcpId::new("ee-mcp-proxy:test");
        let connect_task = tokio::spawn(async move {
            AcpBridgeTransport::connect(&bridge_for_spawn, &server_id, Duration::from_secs(5)).await
        });
        host.serve_next().await;
        let mut transport = connect_task.await.expect("joins").expect("connects");

        // Host answers the next inner round from a spawned task while the
        // test drives the transport.
        let host_task = tokio::spawn(async move { host.serve_next().await });

        let request: ClientRequest = serde_json::from_value(json!({
            "method": "tools/list",
            "params": { "cursor": null },
        }))
        .expect("tools/list request deserializes");
        transport
            .send(JsonRpcMessage::request(request, RequestId::Number(7)))
            .await
            .expect("send succeeds");
        host_task.await.expect("host joins");

        // The reply is delivered by `receive` tagged with the client id.
        let message = tokio::time::timeout(Duration::from_secs(5), transport.receive())
            .await
            .expect("reply arrives")
            .expect("some message");
        let JsonRpcMessage::Response(response) = message else {
            panic!("expected a response, got {message:?}");
        };
        assert_eq!(response.id, RequestId::Number(7));
        assert!(matches!(response.result, ServerResult::ListToolsResult(_)));
    }

    #[tokio::test]
    async fn notification_forwards_without_reply() {
        let (bridge, rx) = test_bridge();
        let mut host = FakeAcpHost::new(bridge.clone(), rx);
        host.answer("mcp/connect", json!({ "connectionId": "conn-1" }));
        let bridge_for_spawn = bridge.clone();
        let server_id = McpServerAcpId::new("ee-mcp-proxy:test");
        let connect_task = tokio::spawn(async move {
            AcpBridgeTransport::connect(&bridge_for_spawn, &server_id, Duration::from_secs(5)).await
        });
        host.serve_next().await;
        let mut transport = connect_task.await.expect("joins").expect("connects");

        let notification: ClientNotification = serde_json::from_value(json!({
            "method": "notifications/initialized",
        }))
        .expect("notification deserializes");
        transport
            .send(JsonRpcMessage::notification(notification))
            .await
            .expect("notification sends");

        host.serve_next().await;
        let log = host.log.lock().expect("log poisoned").clone();
        assert!(log.iter().any(|line| line.contains("notifications/initialized")), "{log:?}");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), transport.receive()).await.is_err(),
            "notifications produce no reply"
        );
    }

    #[tokio::test]
    async fn close_sends_mcp_disconnect() {
        let (bridge, rx) = test_bridge();
        let mut host = FakeAcpHost::new(bridge.clone(), rx);
        host.answer("mcp/connect", json!({ "connectionId": "conn-1" }));
        let bridge_for_spawn = bridge.clone();
        let server_id = McpServerAcpId::new("ee-mcp-proxy:test");
        let connect_task = tokio::spawn(async move {
            AcpBridgeTransport::connect(&bridge_for_spawn, &server_id, Duration::from_secs(5)).await
        });
        host.serve_next().await;
        let mut transport = connect_task.await.expect("joins").expect("connects");

        let log = host.log_arc();
        let host_task = tokio::spawn(async move { host.serve_next().await });
        transport.close().await.expect("close succeeds");
        host_task.await.expect("host joins");

        let log = log.lock().expect("log poisoned").clone();
        assert!(log.iter().any(|line| line.starts_with("mcp/disconnect:")), "{log:?}");
    }
}
