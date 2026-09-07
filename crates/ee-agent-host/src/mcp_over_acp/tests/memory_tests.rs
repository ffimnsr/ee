//! MCP-over-ACP tests: workspace memory approval and verified facts.
use super::*;

fn memory_backend() -> (HostProxyBackend, mpsc::UnboundedReceiver<ProxyJob>, tempfile::TempDir) {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("workspace");
    std::fs::create_dir(&root).expect("workspace root");
    let workspace_memory =
        WorkspaceMemoryHost::new(&crate::workspace_memory::WorkspaceMemoryHostConfig {
            enabled: true,
            trusted_roots: vec![root],
            database_path: Some(temp.path().join("memory.sqlite3")),
            ..Default::default()
        });
    let (jobs, received) = mpsc::unbounded_channel();
    (
        HostProxyBackend {
            jobs,
            process: Arc::new(Mutex::new(None)),
            threads: Arc::new(Mutex::new(HashMap::new())),
            agent_id: "agent-secret-name".to_string(),
            scope: "connection-secret-scope".to_string(),
            supported_tools: None,
            workspace_memory,
            shutdown: CancellationToken::new(),
        },
        received,
        temp,
    )
}

#[test]
fn workspace_memory_mutations_require_typed_approval_without_value_disclosure() {
    let (backend, mut jobs, _temp) = memory_backend();
    let worker = std::thread::spawn(move || {
        for expected in
            [WorkspaceMemoryMutationOperation::Remember, WorkspaceMemoryMutationOperation::Forget]
        {
            let job = jobs.blocking_recv().expect("approval request");
            match job.request {
                ClientRequest::ApproveWorkspaceMemoryMutation { operation, key } => {
                    assert_eq!(operation, expected);
                    assert_eq!(key, "architecture.parser");
                }
                request => panic!("unexpected request: {request:?}"),
            }
            job.reply
                .send(Ok(ClientRequestResponse::WorkspaceMemoryApproval { approved: true }))
                .expect("approval response");
        }
    });

    let remembered = backend
        .remember_workspace_fact(
            "architecture.parser".to_string(),
            "Tree-sitter remains backend-owned".to_string(),
        )
        .expect("approved remember");
    let fact = remembered.fact.expect("remembered fact");
    assert_eq!(fact.authority, "user_asserted");
    assert_eq!(fact.state, "active");
    assert_eq!(fact.provenance.source_kind, "mcp_user_approved");
    assert!(fact.provenance.source_id.starts_with("mcp:"));
    assert!(!fact.provenance.source_id.contains("agent-secret-name"));
    assert!(!fact.provenance.source_id.contains("connection-secret-scope"));

    // Reads bypass handler approval after opt-in.
    assert_eq!(
        backend.read_workspace_fact("architecture.parser".to_string()).expect("direct read").value,
        "Tree-sitter remains backend-owned"
    );
    assert_eq!(
        backend.recall_workspace_facts("parser".to_string()).expect("direct recall").facts.len(),
        1
    );
    assert_eq!(
        backend
            .forget_workspace_fact("architecture.parser".to_string())
            .expect("approved forget")
            .affected,
        1
    );
    worker.join().expect("approval worker");
}

#[test]
fn workspace_memory_management_tools_use_bounded_metadata_without_values() {
    let (backend, mut jobs, _temp) = memory_backend();
    let secret_value = "Tree-sitter remains backend-owned";
    let worker = std::thread::spawn(move || {
        for (expected, metadata_prefix) in [
            (WorkspaceMemoryMutationOperation::Remember, "architecture.parser"),
            (WorkspaceMemoryMutationOperation::Remember, "export:include_values=true"),
            (WorkspaceMemoryMutationOperation::Forget, "retract:architecture.parser"),
            (WorkspaceMemoryMutationOperation::Remember, "import:schema="),
            (WorkspaceMemoryMutationOperation::Forget, "clear:workspace"),
        ] {
            let job = jobs.blocking_recv().expect("approval request");
            match job.request {
                ClientRequest::ApproveWorkspaceMemoryMutation { operation, key } => {
                    assert_eq!(operation, expected);
                    assert!(key.starts_with(metadata_prefix), "{key}");
                    assert!(!key.contains(secret_value));
                }
                request => panic!("unexpected request: {request:?}"),
            }
            job.reply
                .send(Ok(ClientRequestResponse::WorkspaceMemoryApproval { approved: true }))
                .expect("approval response");
        }
    });

    backend
        .remember_workspace_fact("architecture.parser".to_string(), secret_value.to_string())
        .expect("remember");
    let listed = backend.list_workspace_facts(1).expect("list");
    assert_eq!(listed.facts.len(), 1);
    assert_eq!(listed.facts[0].value, secret_value);

    let export = backend.export_workspace_memory(true).expect("export");
    assert_eq!(export["redacted"], serde_json::json!(false));
    let export_json = serde_json::to_string(&export).expect("serialize export");
    backend.retract_workspace_fact("architecture.parser".to_string()).expect("retract");
    assert!(backend.read_workspace_fact("architecture.parser".to_string()).is_err());
    assert_eq!(
        backend.import_workspace_memory(export_json).expect("import")["affected"],
        serde_json::json!(1)
    );
    assert_eq!(backend.clear_workspace_memory().expect("clear")["affected"], json!(2));
    worker.join().expect("approval worker");
}

fn install_verified_turn(backend: &HostProxyBackend) -> String {
    let session = SessionId::new("verified-session");
    let revision = EvidenceRevision::new("revision-1");
    let mut evidence = TurnEvidenceStore::default();
    let turn = evidence.start_turn(backend.agent_id.clone(), session.0.to_string());
    for observation in [
        TurnObservation::Revision { revision: revision.clone() },
        TurnObservation::Write {
            revision: revision.clone(),
            outcome: WriteEvidenceOutcome::Applied,
        },
        TurnObservation::ChangedFiles {
            revision: revision.clone(),
            files: vec!["src/lib.rs".to_string()],
            truncated: false,
        },
        TurnObservation::Diagnostics { revision: revision.clone(), outcome: EvidenceCheck::Passed },
        TurnObservation::DiffReview { revision: revision.clone(), outcome: EvidenceCheck::Passed },
        TurnObservation::ValidationRecord {
            revision,
            selected: true,
            record: HostValidationRecord {
                run_id: "validation-run".to_string(),
                command_id: "cargo-test".to_string(),
                command: "cargo test --quiet".to_string(),
                tool: Some("terminal".to_string()),
                selector: Some("cargo-test".to_string()),
                outcome: EvidenceCheck::Passed,
                exit_status: Some(0),
                elapsed_ms: Some(10),
                affected_tests: vec!["host".to_string()],
                diagnostics_delta: 0,
                output_truncated: false,
                skip_or_denial: None,
            },
        },
        TurnObservation::PromptTerminal { outcome: PromptTerminalOutcome::Completed },
    ] {
        evidence.observe(turn.turn_id(), observation).expect("verified observation");
    }
    let snapshot = evidence.snapshot(turn.turn_id()).expect("verified evidence snapshot");
    let key = derive_workspace_verified_fact_candidates(&snapshot)
        .expect("verified candidate")
        .remove(0)
        .key;
    let (events, _) = mpsc::unbounded_channel();
    backend.threads.lock().expect("threads poisoned").insert(
        session.clone(),
        Arc::new(ThreadShared {
            agent_id: backend.agent_id.clone(),
            session_id: session,
            state: Mutex::new(SessionState::default()),
            order: Mutex::new(ee_agent_protocol::SessionUpdateOrder::new()),
            turn: Mutex::new(None),
            active_turn: Mutex::new(None),
            paused_turn: Mutex::new(None),
            turn_started: Mutex::new(None),
            evidence: Mutex::new(evidence),
            evidence_available: std::sync::atomic::AtomicBool::new(true),
            modes: Mutex::new(None),
            events,
        }),
    );
    key
}

#[test]
fn verify_workspace_fact_promotes_only_exact_connection_owned_evidence() {
    let (backend, mut jobs, _temp) = memory_backend();
    let key = install_verified_turn(&backend);
    let approval_key = key.clone();
    let worker = std::thread::spawn(move || {
        let job = jobs.blocking_recv().expect("verify approval request");
        match job.request {
            ClientRequest::ApproveWorkspaceMemoryMutation { operation, key } => {
                assert_eq!(operation, WorkspaceMemoryMutationOperation::Verify);
                assert_eq!(key, approval_key);
            }
            request => panic!("unexpected request: {request:?}"),
        }
        job.reply
            .send(Ok(ClientRequestResponse::WorkspaceMemoryApproval { approved: true }))
            .expect("approval response");
    });

    let result = backend
        .verify_workspace_fact("verified-session".to_string(), 1, key.clone())
        .expect("verified fact promotion");
    let fact = result.fact.expect("promoted fact");
    assert_eq!(fact.key, key);
    assert_eq!(fact.authority, "host_verified");
    assert_eq!(fact.freshness, "revision_bound");
    assert_eq!(fact.state, "active");
    assert_eq!(fact.provenance.source_kind, "turn_evidence_validation");
    worker.join().expect("approval worker");
}

#[test]
fn verify_workspace_fact_rejects_wrong_or_foreign_evidence_before_approval() {
    let (backend, mut jobs, _temp) = memory_backend();
    install_verified_turn(&backend);

    for (session, key) in
        [("verified-session", "validation.wrong-key"), ("foreign-session", "validation.wrong-key")]
    {
        let error = backend
            .verify_workspace_fact(session.to_string(), 1, key.to_string())
            .expect_err("unowned or underived fact must fail closed");
        assert!(error.message.starts_with("evidence_unavailable:"));
    }
    assert!(matches!(jobs.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Empty)));
}

#[test]
fn workspace_memory_denial_prevents_mutation() {
    let (backend, mut jobs, _temp) = memory_backend();
    let worker = std::thread::spawn(move || {
        let job = jobs.blocking_recv().expect("approval request");
        job.reply
            .send(Ok(ClientRequestResponse::WorkspaceMemoryApproval { approved: false }))
            .expect("denial response");
    });
    let error = backend
        .remember_workspace_fact("denied.key".to_string(), "safe value".to_string())
        .expect_err("denial must fail");
    assert!(error.is_permission_denied);
    assert!(backend.read_workspace_fact("denied.key".to_string()).is_err());
    worker.join().expect("denial worker");
}

#[test]
fn workspace_memory_approval_timeout_and_connection_cancel_abort_wait() {
    let (backend, mut jobs, _temp) = memory_backend();
    let timeout_worker = std::thread::spawn(move || {
        let job = jobs.blocking_recv().expect("timed request");
        while !job.cancel.is_cancelled() {
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    assert!(
        backend
            .call_with_timeout(
                ClientRequest::ApproveWorkspaceMemoryMutation {
                    operation: WorkspaceMemoryMutationOperation::Remember,
                    key: "timeout.key".to_string(),
                },
                Duration::from_millis(20),
            )
            .expect("timeout is typed")
            .is_none()
    );
    timeout_worker.join().expect("timeout worker");

    let (cancel_backend, _jobs, _temp) = memory_backend();
    let shutdown = cancel_backend.shutdown.clone();
    let cancel_thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        shutdown.cancel();
    });
    let error = cancel_backend
        .call_with_timeout(
            ClientRequest::ApproveWorkspaceMemoryMutation {
                operation: WorkspaceMemoryMutationOperation::Forget,
                key: "cancel.key".to_string(),
            },
            Duration::from_secs(1),
        )
        .expect_err("shutdown cancels approval");
    assert!(error.message.starts_with("workspace_memory_approval_cancelled:"));
    cancel_thread.join().expect("cancel thread");
}
