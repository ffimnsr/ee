//! Host-flow tests: misc.
use super::*;

#[tokio::test]
async fn close_kills_connection_and_resolves_pending_work() {
    let script = base_script().wait_for("session/prompt");
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let prompt_thread = thread.clone();
    let prompt = tokio::spawn(async move {
        prompt_thread.send_prompt(vec![ContentBlock::Text(TextContent::new("go"))]).await
    });
    tokio::time::timeout(TEST_TIMEOUT, async {
        while !fake.log_contains("\"method\":\"session/prompt\"") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("prompt observed");

    connection.close().await;

    let error = prompt.await.expect("prompt task").unwrap_err();
    assert!(matches!(error, AgentError::ConnectionClosed { .. } | AgentError::Cancelled));
    assert!(!thread.is_turn_running());
    assert_eq!(connection.permission_broker().pending_count(), 0);
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn session_new_without_roots_fails_closed() {
    let script = base_script();
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let error = connection.new_session(Vec::new(), Vec::new(), None).await.unwrap_err();
    assert!(matches!(error, AgentError::InvalidParams(_)));

    let error = connection
        .new_session(vec![PathBuf::from("relative")], Vec::new(), None)
        .await
        .unwrap_err();
    assert!(matches!(error, AgentError::InvalidParams(_)));
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn request_timeout_produces_typed_error() {
    // The agent never answers session/new: the request timeout fires.
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new");
    let (fake, transport) = FakeAgent::spawn(script);
    let (events_tx, _events_rx) = mpsc::unbounded_channel();
    let connection = AgentConnection::connect_with_transport(
        "fake".into(),
        Arc::new(DenyAllHandler),
        events_tx,
        AgentConnectionOptions {
            handshake_timeout: TEST_TIMEOUT,
            request_timeout: Duration::from_millis(100),
            ..Default::default()
        },
        transport,
    )
    .expect("connect");

    connection.wait_ready().await.expect("handshake ok");
    let error =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap_err();
    assert!(matches!(error, AgentError::RequestTimeout { ref method } if method == "session/new"));
    connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn manager_resolves_default_agent_and_lists_ids() {
    let mut config = AgentManagerConfig::default();
    config.agents.insert("primary".into(), AgentProcessConfig::new("unused"));
    config.agents.insert("secondary".into(), AgentProcessConfig::new("unused"));
    let (events_tx, _events_rx) = mpsc::unbounded_channel();
    let manager = AgentManager::new(config, Arc::new(DenyAllHandler), events_tx);

    assert_eq!(manager.agent_ids().len(), 2);
    assert!(manager.has_agent("primary"));
    assert!(!manager.has_agent("missing"));
    assert_eq!(manager.resolve_default_agent(Some("secondary")), Some("secondary".into()));
    assert_eq!(manager.resolve_default_agent(Some("missing")), None);
    assert_eq!(manager.resolve_default_agent(None), None); // ambiguous
    assert_eq!(manager.live_connection_count(), 0);
    let _ = ProtocolVersion::V1;
}
