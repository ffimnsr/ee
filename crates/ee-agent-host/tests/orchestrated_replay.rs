//! Hermetic OpenRouter orchestrated ACP host-transport integration coverage.
//!
//! This test uses production `openrouter_orchestrated_provider` construction
//! with a concrete `OpenRouterModelAdapter` and a scripted completion client.
//! It covers model-to-ACP-host transport and live host completion evidence; it
//! does not claim editor post-write or pane rendering coverage.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use ee_acp_agent_server::{
    AcpAgentServer, AcpAgentServerConfig, AcpServerError, MemoryTransport, MemoryTransportHandle,
};
use ee_agent_host::fake::FakeAgentTransport;
use ee_agent_host::{
    AgentConnection, AgentConnectionOptions, AgentEvent, AgentThread, DenyAllHandler, TurnBlocker,
    TurnTerminalStatus,
};
use ee_agent_protocol::{
    ContentBlock, Error as RpcError, RawJsonRpcMessage, RequestId, TextContent,
};
use ee_openrouter_agent::config::Config;
use ee_openrouter_agent::orchestrated::{
    openrouter_orchestrated_provider, test_support::ScriptedOpenRouterCompletion,
};
use futures::channel::mpsc as futures_mpsc;
use futures::{StreamExt, sink, stream};
use serde_json::json;
use tokio::sync::{mpsc, watch};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PhaseSixFixtureMetrics {
    model_requests: usize,
    evidence_ids: usize,
    terminal_status: TurnTerminalStatus,
}

/// Line bridge between host connection and in-memory ACP server.
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

    async fn shutdown(self, server: tokio::task::JoinHandle<Result<(), AcpServerError>>) {
        let _ = self.stop_tx.send(true);
        self.pump.await.expect("bridge pump joins");
        drop(self.handle);
        drop(self.to_host_tx);
        server.await.expect("server task joins").expect("server exits cleanly on EOF");
    }
}

struct TestHost {
    connection: AgentConnection,
    events: mpsc::UnboundedReceiver<AgentEvent>,
}

fn test_config() -> Config {
    Config {
        model: String::from("test/model"),
        api_url: String::from("https://openrouter.invalid/api/v1"),
        api_key: Some(String::from("sk-hermetic-test-key")),
        site_url: None,
        app_title: String::from("ee-host-integration-test"),
        timeout: Duration::from_secs(1),
        system_prompt: String::from("system"),
        reasoning_effort: None,
        orchestrated: true,
        compact_min_messages: 4,
        compact_retained_tail: 2,
        compact_max_input_bytes: 65_536,
        context_window: 128_000,
        auto_compact_threshold_percent: 80,
        max_iterations: 16,
        retry_max_attempts: 0,
        retry_base_delay: Duration::from_millis(1),
        retry_max_delay: Duration::from_millis(10),
        checkpoint_dir: None,
    }
}

async fn next_event(events: &mut mpsc::UnboundedReceiver<AgentEvent>) -> AgentEvent {
    tokio::time::timeout(TEST_TIMEOUT, events.recv())
        .await
        .expect("timed out waiting for host event")
        .expect("host event channel closed")
}

async fn wait_for_thread_created(events: &mut mpsc::UnboundedReceiver<AgentEvent>) {
    loop {
        if matches!(next_event(events).await, AgentEvent::ThreadCreated { .. }) {
            return;
        }
    }
}

async fn completed_turn_evidence(
    thread: &AgentThread,
    events: &mut mpsc::UnboundedReceiver<AgentEvent>,
) -> u64 {
    let response = thread
        .send_prompt(vec![ContentBlock::Text(TextContent::new("fixture prompt"))])
        .await
        .expect("OpenRouter orchestrated ACP prompt completes");
    assert_eq!(response.stop_reason, ee_agent_protocol::StopReason::EndTurn);

    let mut turn_id = None;
    loop {
        match next_event(events).await {
            AgentEvent::TurnStarted { turn, .. } => turn_id = Some(turn.turn_id()),
            AgentEvent::TurnEvidenceUpdated { summary, .. } => {
                assert_eq!(summary.status, TurnTerminalStatus::Unverified);
                assert_eq!(summary.blocker, Some(TurnBlocker::MissingRevision));
                return turn_id.expect("turn start arrives before terminal evidence");
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn phase_six_production_false_success_fixture_runs_through_acp_host_transport() {
    let config = test_config();
    let scripted = ScriptedOpenRouterCompletion::new(vec![json!({
        "choices": [{
            "message": { "reasoning": "plan", "content": "completed" },
            "finish_reason": "stop"
        }]
    })]);
    let session_state = tempfile::tempdir().expect("session-state directory");
    let workspace = tempfile::tempdir().expect("fixture workspace");
    let provider = openrouter_orchestrated_provider(
        &config,
        session_state.path().join("agent-sessions"),
        scripted.adapter(config.clone()),
    );
    let server = AcpAgentServer::new(provider, AcpAgentServerConfig::default());
    let (transport, handle) = MemoryTransport::new();
    let server_task = tokio::spawn(async move { server.run_with_transport(transport).await });
    let (bridge, agent_transport) = Bridge::spawn(handle);
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let connection = AgentConnection::connect_with_transport(
        "openrouter-host-transport-test".into(),
        Arc::new(DenyAllHandler),
        events_tx,
        AgentConnectionOptions {
            handshake_timeout: TEST_TIMEOUT,
            request_timeout: TEST_TIMEOUT,
            ..AgentConnectionOptions::default()
        },
        agent_transport,
    )
    .expect("host connects to production OpenRouter orchestrated provider");
    let mut host = TestHost { connection, events: events_rx };
    host.connection.wait_ready().await.expect("initialize succeeds");
    let thread = host
        .connection
        .new_session(vec![workspace.path().to_path_buf()], Vec::new(), None)
        .await
        .expect("session/new succeeds");
    wait_for_thread_created(&mut host.events).await;

    let turn_id = completed_turn_evidence(&thread, &mut host.events).await;

    let retained = thread.turn_evidence_summary(turn_id).expect("host retains terminal evidence");
    assert_eq!(retained.status, TurnTerminalStatus::Unverified);
    assert_eq!(retained.blocker, Some(TurnBlocker::MissingRevision));
    let bodies = scripted.request_bodies();
    assert_eq!(
        PhaseSixFixtureMetrics {
            model_requests: bodies.len(),
            evidence_ids: retained.evidence_ids.len(),
            terminal_status: retained.status,
        },
        PhaseSixFixtureMetrics {
            model_requests: 1,
            evidence_ids: 1,
            terminal_status: TurnTerminalStatus::Unverified,
        },
        "false-success fixture must retain only host terminal evidence"
    );
    assert_eq!(bodies.len(), 1, "scripted OpenRouter client received one model request");
    assert_eq!(bodies[0]["messages"][0]["content"], "system");
    assert!(
        bodies[0]["messages"]
            .as_array()
            .expect("OpenRouter messages array")
            .iter()
            .any(|message| message["content"] == "fixture prompt"),
        "host prompt reaches concrete OpenRouter adapter"
    );
    assert!(
        !bodies[0].to_string().contains("sk-hermetic-test-key"),
        "API key must stay out of scripted OpenRouter request bodies"
    );

    host.connection.close().await;
    bridge.shutdown(server_task).await;
}
