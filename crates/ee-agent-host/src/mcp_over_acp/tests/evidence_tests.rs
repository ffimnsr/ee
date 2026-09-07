//! MCP-over-ACP tests: turn evidence summaries.
use super::*;

#[test]
fn turn_evidence_summary_returns_only_owned_redacted_host_summary() {
    let session = SessionId::new("session-1");
    let (events, _) = mpsc::unbounded_channel();
    let mut evidence = TurnEvidenceStore::default();
    let turn = evidence.start_turn(String::from("agent-1"), session.0.to_string());
    evidence
        .observe(
            turn.turn_id(),
            TurnObservation::ChangedFiles {
                revision: EvidenceRevision::new("revision-1"),
                files: vec![String::from("/private/workspace/secret.rs")],
                truncated: false,
            },
        )
        .expect("host observation records");
    let shared = Arc::new(ThreadShared {
        agent_id: String::from("agent-1"),
        session_id: session.clone(),
        state: Mutex::new(SessionState::default()),
        order: Mutex::new(ee_agent_protocol::SessionUpdateOrder::new()),
        turn: Mutex::new(None),
        active_turn: Mutex::new(Some(turn.clone())),
        paused_turn: Mutex::new(None),
        turn_started: Mutex::new(None),
        evidence: Mutex::new(evidence),
        evidence_available: std::sync::atomic::AtomicBool::new(true),
        modes: Mutex::new(None),
        events,
    });
    let threads = Arc::new(Mutex::new(HashMap::from([(session.clone(), shared)])));
    let (jobs, _) = mpsc::unbounded_channel();
    let backend = HostProxyBackend {
        jobs,
        process: Arc::new(Mutex::new(None)),
        threads,
        agent_id: String::from("agent-1"),
        scope: String::from("test"),
        supported_tools: None,
        workspace_memory: WorkspaceMemoryHost::disabled(),
        shutdown: CancellationToken::new(),
    };

    assert!(backend.exposes_turn_evidence_summary());
    let current = backend.turn_evidence_summary(None, None).expect("current summary");
    assert!(current["key"]["agent_id"].as_str().is_some_and(|value| value.starts_with("sha256:")));
    assert!(
        current["key"]["session_id"].as_str().is_some_and(|value| value.starts_with("sha256:"))
    );
    assert_eq!(current["key"]["turn_id"], 1);
    let serialized = current.to_string();
    assert!(!serialized.contains("agent-1"));
    assert!(!serialized.contains("session-1"));
    assert!(!serialized.contains("/private/workspace/secret.rs"));
    assert!(!serialized.contains("terminal output"));
    assert!(!serialized.contains("prompt"));

    assert!(backend.turn_evidence_summary(Some(String::from("session-1")), Some(1)).is_ok());
    for (session_id, turn_id) in [("foreign", 1), ("session-1", 2)] {
        let error = backend
            .turn_evidence_summary(Some(String::from(session_id)), Some(turn_id))
            .expect_err("foreign or stale evidence must fail closed");
        assert!(error.message.starts_with("evidence_unavailable:"));
    }
}
