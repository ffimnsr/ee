//! Host-flow tests: elicitation.
use super::*;

#[derive(Debug, Clone)]
struct DelayedUrlHandler {
    seen: Arc<Mutex<Vec<ClientRequest>>>,
    delay: Duration,
}

#[derive(Debug, Clone)]
struct DelayedReadHandler {
    seen: Arc<Mutex<Vec<ClientRequest>>>,
    delay: Duration,
}

impl DelayedUrlHandler {
    fn new(delay: Duration) -> Self {
        Self { seen: Arc::new(Mutex::new(Vec::new())), delay }
    }
}

impl DelayedReadHandler {
    fn new(delay: Duration) -> Self {
        Self { seen: Arc::new(Mutex::new(Vec::new())), delay }
    }
}

impl ClientRequestHandler for DelayedUrlHandler {
    fn capabilities(&self) -> HandlerCapabilities {
        HandlerCapabilities { elicitation_url: true, ..HandlerCapabilities::none() }
    }

    fn handle(
        &self,
        request: ClientRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ClientRequestResult> + Send + '_>> {
        Box::pin(async move {
            self.seen.lock().expect("delayed url handler poisoned").push(request.clone());
            match request {
                ClientRequest::CreateElicitation(_) => {
                    tokio::time::sleep(self.delay).await;
                    Ok(ClientRequestResponse::CreateElicitation(CreateElicitationResponse::new(
                        ElicitationAction::Accept(ElicitationAcceptAction::new()),
                    )))
                }
                other => {
                    Err(AgentError::HandlerError(format!("unhandled request {}", other.method())))
                }
            }
        })
    }
}

impl ClientRequestHandler for DelayedReadHandler {
    fn capabilities(&self) -> HandlerCapabilities {
        HandlerCapabilities { fs_read: true, ..HandlerCapabilities::none() }
    }

    fn handle(
        &self,
        request: ClientRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ClientRequestResult> + Send + '_>> {
        Box::pin(async move {
            self.seen.lock().expect("delayed read handler poisoned").push(request.clone());
            match request {
                ClientRequest::ReadTextFile(_) => {
                    tokio::time::sleep(self.delay).await;
                    Ok(ClientRequestResponse::ReadTextFile(ReadTextFileResponse::new("late")))
                }
                other => {
                    Err(AgentError::HandlerError(format!("unhandled request {}", other.method())))
                }
            }
        })
    }
}

#[tokio::test]
async fn incoming_cancel_request_aborts_long_running_client_request() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::read_text_file("s1", "/work/Cargo.toml"))
        .delay(50)
        .emit(cancel_request(101))
        .respond(json!({ "stopReason": "end_turn" }));

    let handler = Arc::new(DelayedReadHandler::new(Duration::from_secs(1)));
    let (fake, host) = spawn_host(script, handler).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    thread.send_prompt(vec![ContentBlock::Text(TextContent::new("go"))]).await.unwrap();

    let response = await_response(&fake, 101).await;
    assert_eq!(response["error"]["code"], -32800, "response: {response}");
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn url_elicitation_completion_is_connection_scoped_and_idempotent() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::elicitation_url("s1", "el-1", "https://example.com/authorize", "authorize"))
        .emit(wire::elicitation_complete("el-1"))
        .emit(wire::elicitation_complete("el-1"))
        .emit(wire::elicitation_complete("el-stale"))
        .respond(json!({ "stopReason": "end_turn" }));

    let handler = Arc::new(DelayedUrlHandler::new(Duration::from_millis(100)));
    let (fake, mut host) = spawn_host(script, handler).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    thread
        .send_prompt(vec![ContentBlock::Text(TextContent::new("go"))])
        .await
        .expect("turn completes");

    let mut completions = Vec::new();
    let mut saw_turn_completed = false;
    while !saw_turn_completed {
        match next_event(&mut host.events).await {
            AgentEvent::ElicitationCompleted { agent_id, session_id, elicitation_id } => {
                assert_eq!(agent_id, "fake");
                assert_eq!(session_id.as_ref().map(|id| id.0.as_ref()), Some("s1"));
                completions.push(elicitation_id.0.to_string());
            }
            AgentEvent::TurnCompleted { .. } => {
                saw_turn_completed = true;
            }
            AgentEvent::TurnStarted { .. }
            | AgentEvent::ThreadCreated { .. }
            | AgentEvent::ConnectionStateChanged { .. }
            | AgentEvent::ClientRequestDispatched { .. }
            | AgentEvent::SessionUpdate { .. } => {}
            _ => {}
        }
    }
    assert_eq!(completions, vec![String::from("el-1")]);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}
