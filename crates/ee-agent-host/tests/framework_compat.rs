//! Host ↔ framework compatibility.
//!
//! The real host connection stack (`AgentConnection`, SDK transport,
//! reducer, permission broker) drives a real `ee-acp-agent-server` server
//! over an in-process line bridge — no external binaries, no network.  This
//! proves the two crates still interoperate after provider refactors:
//! initialize handshake, session creation, prompt with streamed update, and
//! session close.
//!
//! The framework side stays host-free; the dependency direction here is
//! test-only (host dev-dependency on the framework).

use std::io;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use ee_acp_agent_server::{
    AcpAgentServer, AcpAgentServerConfig, AcpServerError, AgentProvider, ClientBridge,
    LoadSessionContext, MemoryTransport, MemoryTransportHandle, NewSessionContext, PromptContext,
    PromptResult, ProviderError, ProviderFuture, SessionInit, UpdateSink,
};
use ee_agent_host::fake::FakeAgentTransport;
use ee_agent_host::{
    AgentConnection, AgentConnectionOptions, AgentEvent, AgentThread, DenyAllHandler,
};
use ee_agent_protocol::{
    AgentCapabilities, ContentBlock, Error as RpcError, Implementation, PromptResponse,
    RawJsonRpcMessage, RequestId, SessionCapabilities, SessionCloseCapabilities, SessionId,
    SessionUpdate, StopReason, TextContent,
};
use futures::channel::mpsc as futures_mpsc;
use futures::{StreamExt, sink, stream};
use tokio::sync::{mpsc, watch};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Minimal echo provider: generates `compat-N` session ids and echoes the
/// prompt text back as one message chunk.  Advertises session close so the
/// host's `session/close` capability gate passes.
struct CompatProvider {
    next_session: Mutex<u64>,
}

impl AgentProvider for CompatProvider {
    fn info(&self) -> Implementation {
        Implementation::new("compat-echo", env!("CARGO_PKG_VERSION")).title("Compat Echo")
    }

    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities::default()
            .session_capabilities(SessionCapabilities::new().close(SessionCloseCapabilities::new()))
    }

    fn new_session(
        &self,
        _ctx: NewSessionContext,
    ) -> ProviderFuture<Result<SessionInit, ProviderError>> {
        let mut next = self.next_session.lock().expect("compat session counter poisoned");
        let session_id = SessionId::new(format!("compat-{}", *next));
        *next += 1;
        Box::pin(async move { Ok(SessionInit::new(session_id)) })
    }

    fn load_session(
        &self,
        ctx: LoadSessionContext,
    ) -> ProviderFuture<Result<SessionInit, ProviderError>> {
        let session_id = ctx.session_id.clone();
        Box::pin(async move { Ok(SessionInit::new(session_id)) })
    }

    fn prompt(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        _client: ClientBridge,
        _cancel: watch::Receiver<bool>,
    ) -> ProviderFuture<Result<PromptResult, ProviderError>> {
        Box::pin(async move {
            let text: String = ctx
                .prompt
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ");
            sink.agent_message_chunk("compat-msg", text).map_err(|error| {
                ProviderError::BackendFailure(format!("failed to emit echo update: {error}"))
            })?;
            Ok(PromptResponse::new(StopReason::EndTurn))
        })
    }

    fn cancel_session(&self, _session_id: SessionId) -> ProviderFuture<Result<(), ProviderError>> {
        Box::pin(async { Ok(()) })
    }

    fn close_session(&self, _session_id: SessionId) -> ProviderFuture<Result<(), ProviderError>> {
        Box::pin(async { Ok(()) })
    }
}

/// Line bridge between the host connection's [`FakeAgentTransport`] and the
/// framework server's in-memory transport.
///
/// Host → agent: each JSON line the host writes is parsed into a frame and
/// injected into the framework (parse failures get a `-32700` response).
/// Agent → host: a pump task serializes the framework's outbound frames into
/// lines; it stops on the shutdown signal so no task outlives the test.
struct Bridge {
    handle: MemoryTransportHandle,
    to_host_tx: futures_mpsc::UnboundedSender<io::Result<String>>,
    stop_tx: watch::Sender<bool>,
    pump: tokio::task::JoinHandle<()>,
}

impl Bridge {
    fn spawn(handle: MemoryTransportHandle) -> (Self, FakeAgentTransport) {
        let (to_host_tx, to_host_rx) = futures_mpsc::unbounded::<io::Result<String>>();
        let (stop_tx, stop_rx) = watch::channel(false);

        // Host → agent: every line the host writes is parsed into a frame
        // and injected into the framework (parse failures get a `-32700`
        // response so the host never hangs).
        let outgoing_sink = sink::unfold(handle.clone(), |handle, line: String| async move {
            match serde_json::from_str::<RawJsonRpcMessage>(&line) {
                Ok(frame) => {
                    let _ = handle.send(frame);
                }
                Err(error) => {
                    let response = RawJsonRpcMessage::response(
                        RequestId::Null,
                        Err(RpcError::new(-32700, format!("parse error: {error}"))),
                    );
                    let _ = handle.send(response);
                }
            }
            Ok::<_, io::Error>(handle)
        });

        let pump = {
            let handle = handle.clone();
            let to_host_tx = to_host_tx.clone();
            let stop_rx = stop_rx.clone();
            tokio::spawn(async move {
                loop {
                    if *stop_rx.borrow() {
                        break;
                    }
                    for frame in handle.take_outbound() {
                        if let Ok(line) = serde_json::to_string(&frame) {
                            let _ = to_host_tx.unbounded_send(Ok(line));
                        }
                    }
                    tokio::task::yield_now().await;
                }
            })
        };

        let incoming_stream =
            stream::unfold(
                to_host_rx,
                |mut rx| async move { rx.next().await.map(|item| (item, rx)) },
            );
        let transport = FakeAgentTransport::new(Box::pin(outgoing_sink), Box::pin(incoming_stream));
        (Self { handle, to_host_tx, stop_tx, pump }, transport)
    }

    /// Stops the pump, closes the framework transport (EOF), and asserts the
    /// server exits cleanly.
    async fn shutdown(self, server: tokio::task::JoinHandle<Result<(), AcpServerError>>) {
        let _ = self.stop_tx.send(true);
        self.pump.await.expect("bridge pump joins");
        drop(self.handle);
        drop(self.to_host_tx);
        server.await.expect("server task joins").expect("server exits cleanly on EOF");
    }
}

/// A connected host plus its event stream.
struct TestHost {
    connection: AgentConnection,
    events: mpsc::UnboundedReceiver<AgentEvent>,
}

async fn next_event(rx: &mut mpsc::UnboundedReceiver<AgentEvent>) -> AgentEvent {
    tokio::time::timeout(TEST_TIMEOUT, rx.recv())
        .await
        .expect("timed out waiting for host event")
        .expect("event channel closed")
}

#[tokio::test]
async fn host_drives_framework_initialize_session_prompt_and_close() {
    // Framework side: echo provider over the in-memory transport.
    let provider = CompatProvider { next_session: Mutex::new(1) };
    let server = AcpAgentServer::new(provider, AcpAgentServerConfig::default());
    let (transport, handle) = MemoryTransport::new();
    let server_task = tokio::spawn(async move { server.run_with_transport(transport).await });

    // Bridge + host side.
    let (bridge, agent_transport) = Bridge::spawn(handle);
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let options = AgentConnectionOptions {
        handshake_timeout: TEST_TIMEOUT,
        request_timeout: TEST_TIMEOUT,
        ..Default::default()
    };
    let connection = AgentConnection::connect_with_transport(
        "compat".into(),
        std::sync::Arc::new(DenyAllHandler),
        events_tx,
        options,
        agent_transport,
    )
    .expect("host connects over the bridge");
    let mut host = TestHost { connection, events: events_rx };

    // Initialize: the host handshakes with the framework.
    host.connection.wait_ready().await.expect("initialize handshake succeeds");
    assert!(matches!(
        next_event(&mut host.events).await,
        AgentEvent::ConnectionStateChanged {
            state: ee_agent_host::AgentConnectionState::Ready { .. },
            ..
        }
    ));

    // Session creation.
    let thread: AgentThread = host
        .connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), None)
        .await
        .expect("session/new succeeds");
    assert_eq!(thread.session_id().to_string(), "compat-1");
    assert!(matches!(next_event(&mut host.events).await, AgentEvent::ThreadCreated { .. }));

    // Prompt: the framework streams the echo update, then completes.
    let response = thread
        .send_prompt(vec![ContentBlock::Text(TextContent::new("hello framework"))])
        .await
        .expect("prompt completes");
    assert_eq!(response.stop_reason, StopReason::EndTurn);

    assert!(matches!(next_event(&mut host.events).await, AgentEvent::TurnStarted { .. }));
    match next_event(&mut host.events).await {
        AgentEvent::SessionUpdate { update, .. } => match *update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                let ContentBlock::Text(text) = chunk.content else {
                    panic!("expected text chunk");
                };
                assert_eq!(text.text, "hello framework");
            }
            other => panic!("expected an agent message chunk update, got {other:?}"),
        },
        other => panic!("expected a session update event, got {other:?}"),
    }
    assert!(matches!(
        next_event(&mut host.events).await,
        AgentEvent::TurnCompleted { stop_reason: StopReason::EndTurn, .. }
    ));

    // Session close.
    host.connection
        .close_session(thread.session_id().clone())
        .await
        .expect("session/close succeeds");
    assert!(matches!(next_event(&mut host.events).await, AgentEvent::ThreadClosed { .. }));

    // Tear down: host first, then the bridge and the framework server.
    host.connection.close().await;
    bridge.shutdown(server_task).await;
}
