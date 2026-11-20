//! Transport-agnostic JSON-RPC connection state.
//!
//! stdio and WebSocket transports share this handler: handshake gating,
//! typed dispatch with per-request deadlines, overload responses, and
//! approval-response routing. Transports own framing and outbound writes.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;

use crate::dispatch::CallerIdentity;
use crate::error::RemoteError;
use crate::host::RemoteHost;
use crate::protocol::jsonrpc::{
    self, Incoming, parse_incoming, response_error, response_error_null_id, response_ok,
};
use crate::protocol::{InitializeParams, Method, MethodKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    New,
    Handshaken,
    Ready,
}

/// One client connection's protocol state.
pub(crate) struct ConnectionHandler {
    host: RemoteHost,
    writer: mpsc::Sender<Value>,
    overloaded: Arc<AtomicBool>,
    phase: Phase,
    caller: CallerIdentity,
    request_timeout: Duration,
}

impl ConnectionHandler {
    pub(crate) fn new(
        host: RemoteHost,
        writer: mpsc::Sender<Value>,
        overloaded: Arc<AtomicBool>,
    ) -> Self {
        let request_timeout = host.config().request_timeout;
        Self {
            host,
            writer,
            overloaded,
            phase: Phase::New,
            caller: CallerIdentity::anonymous(),
            request_timeout,
        }
    }

    /// Handles one text frame. Responses and errors are written to the
    /// outbound queue; nothing is returned to the transport.
    pub(crate) async fn handle_text(&mut self, text: &str) {
        let incoming = match parse_incoming(text) {
            Ok(incoming) => incoming,
            Err(error) => {
                self.send(response_error_null_id(&error));
                return;
            }
        };
        match incoming {
            Incoming::Request(request) => {
                let id = request.id.clone();
                let Some(method) = Method::from_name(&request.method) else {
                    self.send(response_error(&id, &RemoteError::method_not_found(&request.method)));
                    return;
                };
                if method.kind() != MethodKind::Request {
                    self.send(response_error(
                        &id,
                        &RemoteError::invalid_request(format!(
                            "{} is not a client request method",
                            method.name()
                        )),
                    ));
                    return;
                }
                if self.phase == Phase::New && method != Method::Initialize {
                    self.send(response_error(&id, &RemoteError::not_initialized()));
                    return;
                }
                if method == Method::Initialize && self.phase != Phase::New {
                    self.send(response_error(
                        &id,
                        &RemoteError::invalid_request("connection is already initialized"),
                    ));
                    return;
                }
                if self.overloaded.load(Ordering::Acquire) {
                    self.send(response_error(&id, &RemoteError::overload()));
                    return;
                }
                if method == Method::Initialize {
                    self.caller = caller_from(parse_initialize(&request.params));
                }
                let dispatched = tokio::time::timeout(
                    self.request_timeout,
                    self.host.dispatch_as(&self.caller, method, request.params),
                )
                .await;
                match dispatched {
                    Ok(Ok(result)) => {
                        if method == Method::Initialize {
                            self.phase = Phase::Handshaken;
                        }
                        self.send(response_ok(&id, result));
                    }
                    Ok(Err(error)) => self.send(response_error(&id, &error)),
                    Err(_) => self.send(response_error(
                        &id,
                        &RemoteError::timeout(
                            method.name(),
                            self.request_timeout.as_millis() as u64,
                        ),
                    )),
                }
            }
            Incoming::Notification(notification) => {
                if notification.method == crate::protocol::INITIALIZED
                    && self.phase == Phase::Handshaken
                {
                    self.phase = Phase::Ready;
                }
            }
            Incoming::Response(response) => {
                let server_id = match &response.id {
                    jsonrpc::RequestId::Number(number) => u64::try_from(*number).ok(),
                    jsonrpc::RequestId::String(_) => None,
                };
                let Some(server_id) = server_id else {
                    tracing::warn!("ignoring response with a non-numeric id");
                    return;
                };
                if let Err(error) = self.host.resolve_approval_response(
                    server_id,
                    response.result.as_ref(),
                    &self.caller,
                ) {
                    tracing::warn!(%error, "approval response was not applied");
                }
            }
        }
    }
    fn send(&self, frame: Value) {
        send_frame(&self.writer, self.host.redactor(), frame);
    }
}

fn parse_initialize(params: &Option<Value>) -> Option<InitializeParams> {
    serde_json::from_value(params.clone()?).ok()
}

fn caller_from(params: Option<InitializeParams>) -> CallerIdentity {
    let Some(params) = params else {
        return CallerIdentity::anonymous();
    };
    let client_id = params.client_id.or_else(|| params.client_info.map(|info| info.name));
    CallerIdentity::new(client_id.unwrap_or_else(|| String::from("anonymous")))
}

fn send_frame(writer: &mpsc::Sender<Value>, redactor: &crate::redact::Redactor, frame: Value) {
    let frame = redactor.value(&frame);
    if let Err(error) = writer.try_send(frame) {
        tracing::warn!(%error, "dropping outbound frame: client queue unavailable");
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::config::RemoteHostConfig;

    fn handler(overloaded: bool) -> (ConnectionHandler, mpsc::Receiver<Value>) {
        let host = RemoteHost::deny_all(RemoteHostConfig::default());
        let (tx, rx) = mpsc::channel(16);
        let mut handler = ConnectionHandler::new(host, tx, Arc::new(AtomicBool::new(overloaded)));
        handler.request_timeout = Duration::from_secs(1);
        (handler, rx)
    }

    #[tokio::test]
    async fn overload_returns_retry_guidance_instead_of_dispatching() {
        let (mut handler, mut rx) = handler(true);
        handler
            .handle_text(
                &json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": { "protocolVersion": crate::REMOTE_PROTOCOL_VERSION }
                })
                .to_string(),
            )
            .await;
        let response = rx.recv().await.expect("response");
        assert_eq!(response["error"]["code"], -32001);
        assert_eq!(response["error"]["data"]["retryable"], true);
    }

    #[tokio::test]
    async fn handshake_gating_is_enforced() {
        let (mut handler, mut rx) = handler(false);
        handler
            .handle_text(
                &json!({ "jsonrpc": "2.0", "id": 1, "method": "session/list" }).to_string(),
            )
            .await;
        let response = rx.recv().await.expect("response");
        assert_eq!(response["error"]["code"], -32002);
    }

    #[tokio::test]
    async fn malformed_frames_answer_with_parse_errors() {
        let (mut handler, mut rx) = handler(false);
        handler.handle_text("{").await;
        let response = rx.recv().await.expect("response");
        assert_eq!(response["error"]["code"], -32700);
    }
}
