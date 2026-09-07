//! End-to-end host flows against the in-process fake ACP agent.
//!
//! These tests exercise the real connection stack (SDK transport, handshake,
//! driver, reducer, permission broker) over the scripted fake transport; no
//! external binaries are spawned.

pub(crate) use std::path::PathBuf;
pub(crate) use std::sync::{Arc, Mutex};
pub(crate) use std::time::Duration;

pub(crate) use ee_agent_host::fake::{CaptureSource, FakeAgent, FakeAgentScript, wire};
pub(crate) use ee_agent_host::reducer::MessageKind;
pub(crate) use ee_agent_host::{
    AgentConnection, AgentConnectionOptions, AgentError, AgentEvent, AgentManager,
    AgentManagerConfig, AgentProcessConfig, ClientRequest, ClientRequestHandler,
    ClientRequestResponse, ClientRequestResult, DenyAllHandler, HandlerCapabilities,
    RecordingHandler, SafeFollowUp, ThreadCloseReason, TurnBlocker,
};
pub(crate) use ee_agent_protocol::{
    AudioContent, ContentBlock, CreateElicitationResponse, CreateTerminalResponse,
    ElicitationAcceptAction, ElicitationAction, EmbeddedResource, EmbeddedResourceResource,
    ImageContent, KillTerminalResponse, ProtocolVersion, ReadTextFileResponse,
    ReleaseTerminalResponse, RequestPermissionOutcome, ResourceLink, SelectedPermissionOutcome,
    SessionConfigId, SessionConfigOptionValue, SessionId, SessionModeId, StopReason,
    TerminalExitStatus, TerminalId, TerminalOutputResponse, TextContent, TextResourceContents,
    WaitForTerminalExitResponse, WriteTextFileResponse,
};
pub(crate) use serde_json::{Value, json};
pub(crate) use tokio::sync::mpsc;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

/// A connected host plus its event stream.
struct TestHost {
    connection: AgentConnection,
    events: mpsc::UnboundedReceiver<AgentEvent>,
}

async fn spawn_host(
    script: FakeAgentScript,
    handler: Arc<dyn ee_agent_host::ClientRequestHandler>,
) -> (FakeAgent, TestHost) {
    spawn_host_with_limit(script, handler, ee_agent_host::DEFAULT_MAX_CONCURRENT_PROMPTS).await
}

async fn spawn_host_with_limit(
    script: FakeAgentScript,
    handler: Arc<dyn ee_agent_host::ClientRequestHandler>,
    max_concurrent_prompts: usize,
) -> (FakeAgent, TestHost) {
    let (fake, transport) = FakeAgent::spawn(script);
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let options = AgentConnectionOptions {
        handshake_timeout: TEST_TIMEOUT,
        request_timeout: TEST_TIMEOUT,
        max_concurrent_prompts,
        ..Default::default()
    };
    let connection = AgentConnection::connect_with_transport(
        "fake".into(),
        handler,
        events_tx,
        options,
        transport,
    )
    .expect("connect over fake transport");
    (fake, TestHost { connection, events: events_rx })
}

async fn next_event(rx: &mut mpsc::UnboundedReceiver<AgentEvent>) -> AgentEvent {
    tokio::time::timeout(TEST_TIMEOUT, rx.recv())
        .await
        .expect("timed out waiting for host event")
        .expect("event channel closed")
}

impl TestHost {
    /// Closes the connection so the fake's driver can finish (the transport
    /// stays open while the connection lives).
    async fn close(&self) {
        self.connection.close().await;
    }
}

/// Polls the fake's log for the host's response to the request with `id`
/// (the responder task writes asynchronously).
async fn await_response(fake: &FakeAgent, id: i64) -> Value {
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if let Some(response) = fake.response_with_id(id) {
                break response;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed out waiting for response")
}

async fn await_request_count(fake: &FakeAgent, method: &str, count: usize) {
    tokio::time::timeout(TEST_TIMEOUT, async {
        while fake.requests_by_method(method).len() < count {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {count} {method} requests"));
}

#[derive(Debug, Clone)]
struct ScriptedHandler {
    capabilities: HandlerCapabilities,
    seen: Arc<Mutex<Vec<ClientRequest>>>,
}

impl ScriptedHandler {
    fn new(capabilities: HandlerCapabilities) -> Self {
        Self { capabilities, seen: Arc::new(Mutex::new(Vec::new())) }
    }

    fn seen(&self) -> Vec<ClientRequest> {
        self.seen.lock().expect("scripted handler poisoned").clone()
    }
}

impl ClientRequestHandler for ScriptedHandler {
    fn capabilities(&self) -> HandlerCapabilities {
        self.capabilities
    }

    fn handle(
        &self,
        request: ClientRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ClientRequestResult> + Send + '_>> {
        Box::pin(async move {
            self.seen.lock().expect("scripted handler poisoned").push(request.clone());
            match request {
                ClientRequest::ReadTextFile(_) => Ok(ClientRequestResponse::ReadTextFile(
                    ReadTextFileResponse::new("file contents"),
                )),
                ClientRequest::WriteTextFile(_) => {
                    Ok(ClientRequestResponse::WriteTextFile(WriteTextFileResponse::new()))
                }
                ClientRequest::CreateTerminal(_) => Ok(ClientRequestResponse::CreateTerminal(
                    CreateTerminalResponse::new(TerminalId::new("term-scripted")),
                )),
                ClientRequest::TerminalOutput(_) => Ok(ClientRequestResponse::TerminalOutput(
                    TerminalOutputResponse::new("stdout", false),
                )),
                ClientRequest::WaitForTerminalExit(_) => Ok(
                    ClientRequestResponse::WaitForTerminalExit(WaitForTerminalExitResponse::new(
                        TerminalExitStatus::new().exit_code(Some(0)),
                    )),
                ),
                ClientRequest::KillTerminal(_) => {
                    Ok(ClientRequestResponse::KillTerminal(KillTerminalResponse::new()))
                }
                ClientRequest::ReleaseTerminal(_) => {
                    Ok(ClientRequestResponse::ReleaseTerminal(ReleaseTerminalResponse::new()))
                }
                ClientRequest::CreateElicitation(request) => {
                    let action = match request.mode {
                        ee_agent_protocol::ElicitationMode::Form(_) => {
                            ElicitationAction::Accept(ElicitationAcceptAction::new().content(
                                std::collections::BTreeMap::from([(
                                    String::from("name"),
                                    ee_agent_protocol::ElicitationContentValue::String(
                                        String::from("ed"),
                                    ),
                                )]),
                            ))
                        }
                        ee_agent_protocol::ElicitationMode::Url(_) => {
                            ElicitationAction::Accept(ElicitationAcceptAction::new())
                        }
                        _ => {
                            return Err(AgentError::invalid_params("unsupported elicitation mode"));
                        }
                    };
                    Ok(ClientRequestResponse::CreateElicitation(CreateElicitationResponse::new(
                        action,
                    )))
                }
                other => {
                    Err(AgentError::HandlerError(format!("unhandled request {}", other.method())))
                }
            }
        })
    }
}

fn assert_method_not_found(response: &Value) {
    assert_eq!(response["error"]["code"], -32601, "response: {response}");
}

fn assert_invalid_params(response: &Value) {
    assert_eq!(response["error"]["code"], -32602, "response: {response}");
}

fn cancel_request(id: i64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "$/cancel_request",
        "params": { "requestId": id }
    })
}

/// initialize + session/new happy-path responses.
fn base_script() -> FakeAgentScript {
    FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
}

async fn ready_connection(fake: &FakeAgent, host: &TestHost) -> AgentConnection {
    let connection = host.connection.clone();
    connection.wait_ready().await.expect("handshake succeeds");
    assert!(fake.log_contains("\"method\":\"initialize\""));
    connection
}

async fn initialize_request_for_capabilities(capabilities: HandlerCapabilities) -> Value {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }));
    let handler = Arc::new(ScriptedHandler::new(capabilities));
    let (fake, host) = spawn_host(script, handler).await;
    host.connection.wait_ready().await.expect("handshake succeeds");
    let initialize = fake.requests_by_method("initialize").pop().expect("initialize sent");
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
    initialize
}

mod auth_tests;
mod cancel_tests;
mod concurrency_tests;
mod elicitation_tests;
mod lifecycle_tests;
mod misc_tests;
mod mode_tests;
mod prompt_tests;
mod request_tests;
mod session_tests;
mod snapshot_tests;
