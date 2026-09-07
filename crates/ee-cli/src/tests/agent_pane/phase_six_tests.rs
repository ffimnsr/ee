//! `impl App` agents-pane tests: phase_six_tests domain.
use super::*;

#[test]
fn phase_six_live_openrouter_pane_write_collects_post_write_evidence() {
    let _live_lock = phase_six_live_lock();
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Metrics {
        model_requests: usize,
        approvals: usize,
        evidence_ids: usize,
        write_actions: usize,
    }

    let workspace = tempfile::tempdir().expect("fixture workspace");
    let target = workspace.path().join("live.txt");
    fs::write(workspace.path().join(".ee.toml"), AGENTS_TOML).expect("write agents config");
    fs::write(&target, "before\n").expect("write baseline file");
    commit_git_baseline(workspace.path());

    let scripted = live_write_script(&target, "write-live-file", "after\n", "write complete");
    let state = tempfile::tempdir().expect("fixture session state");
    let factory = LiveOpenRouterTransport::new(
        openrouter_fixture_config(),
        state.path().join("agent-sessions"),
        scripted.clone(),
    );
    let mut app = live_openrouter_app_in(workspace.path(), factory.clone());
    let buffer_id = app.backend.open_buffer(Some(target.clone())).expect("open target buffer");
    app.backend.switch_to_id(buffer_id).expect("focus target buffer");
    open_pane_and_wait_ready(&mut app);

    // Select production provider's write mode through the real pane picker;
    // no host evidence is injected by this fixture.
    select_live_write_mode(&mut app);

    type_text(&mut app, "make live editor write");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "live write turn started", |app| {
        app.agents.threads[0].host.active_turn_key().is_some()
    });
    let turn_id =
        app.agents.threads[0].host.active_turn_key().expect("live write turn key").turn_id();
    wait_until(&mut app, "real write approval", |app| app.agents.approvals.len() == 1);
    let approval_count = app.agents.approvals.len();
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "host post-write evidence", |app| {
        app.agents.threads[0].host.turn_evidence(turn_id).is_some_and(|evidence| {
            let records = evidence.records();
            evidence.current_revision().is_some()
                && records.iter().any(|record| {
                    matches!(
                        record.observation(),
                        TurnObservation::Write {
                            outcome: ee_agent_host::WriteEvidenceOutcome::Applied,
                            ..
                        }
                    )
                })
                && records.iter().any(|record| {
                    matches!(
                        record.observation(),
                        TurnObservation::ChangedFiles { truncated: false, .. }
                    )
                })
                && records.iter().any(|record| {
                    matches!(
                        record.observation(),
                        TurnObservation::Diagnostics { outcome: EvidenceCheck::Passed, .. }
                    )
                })
                && records.iter().any(|record| {
                    matches!(
                        record.observation(),
                        TurnObservation::DiffReview { outcome: EvidenceCheck::Passed, .. }
                    )
                })
        })
    });

    wait_until(&mut app, "real provider turn completion", |app| {
        app.agents.threads[0].state == ThreadUiState::Ready
    });

    let thread = &app.agents.threads[0];
    let summary = thread.host.turn_evidence_summary(turn_id).expect("post-write host evidence");
    assert_eq!(summary.status, TurnTerminalStatus::PartiallyVerified);
    assert_eq!(summary.blocker, Some(TurnBlocker::MissingSelectedValidation));
    assert_eq!(summary.safe_follow_up, SafeFollowUp::RunSelectedValidation);
    assert_eq!(fs::read_to_string(&target).expect("read agent write"), "after\n");
    assert_eq!(thread.state, ThreadUiState::Ready);
    assert_eq!(app.backend.active().whole_text().as_deref(), Some("after\n"));
    assert!(thread.verification_paths.iter().any(|path| path == &target));
    let write_actions = app
        .agents
        .action_log
        .iter()
        .filter(|action| format!("{action:?}").starts_with("Write {"))
        .count();
    assert_eq!(write_actions, 1, "one approved write is recorded: {:?}", app.agents.action_log);
    let metrics = Metrics {
        model_requests: scripted.request_bodies().len(),
        approvals: approval_count,
        evidence_ids: summary.evidence_ids.len(),
        write_actions,
    };
    assert_eq!(metrics.model_requests, 2);
    assert_eq!(metrics.approvals, 1);
    assert_eq!(metrics.write_actions, 1);
    assert!(
        metrics.evidence_ids >= 15,
        "one real approved write must retain changed-files, diagnostics, diff, and missing-validation evidence: {metrics:?}"
    );
    let bodies = scripted.request_bodies();
    assert!(
        bodies[1]["messages"].as_array().expect("second model request messages").iter().any(
            |message| message["role"] == "tool" && message["tool_call_id"] == "write-live-file"
        ),
        "approved bridge write result must reach concrete OpenRouter adapter"
    );

    app.shutdown_agents();
    factory.shutdown();
}

#[test]
fn phase_six_live_openrouter_pane_denied_write_reports_blocked_evidence() {
    let _live_lock = phase_six_live_lock();
    let workspace = tempfile::tempdir().expect("fixture workspace");
    let target = workspace.path().join("denied.txt");
    fs::write(workspace.path().join(".ee.toml"), AGENTS_TOML).expect("write agents config");
    fs::write(&target, "before\n").expect("write baseline file");
    commit_git_baseline(workspace.path());

    let scripted = live_write_script_with_calls(
        &target,
        &[("write-live-warmup", "before\n"), ("write-live-denied", "after\n")],
        "write denied",
    );
    let state = tempfile::tempdir().expect("fixture session state");
    let factory = LiveOpenRouterTransport::new(
        openrouter_fixture_config(),
        state.path().join("agent-sessions"),
        scripted.clone(),
    );
    let mut app = live_openrouter_app_in(workspace.path(), factory.clone());
    let buffer_id = app.backend.open_buffer(Some(target.clone())).expect("open target buffer");
    app.backend.switch_to_id(buffer_id).expect("focus target buffer");
    open_pane_and_wait_ready(&mut app);
    select_live_write_mode(&mut app);

    type_text(&mut app, "reject live editor write");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "warmup write approval", |app| !app.agents.approvals.is_empty());
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "denied write approval", |app| !app.agents.approvals.is_empty());
    assert_eq!(app.agents.approvals.front().expect("write approval").selected, 0);
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.agents.approvals.front().expect("write approval").selected, 2);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "write denial resolved", |app| app.agents.approvals.is_empty());
    wait_until(&mut app, "pane denied-write evidence", |app| {
        app.agents.threads[0].system_notices().iter().any(|notice| {
            notice.contains("verification: Blocked") && notice.contains("WriteDenied")
        })
    });
    wait_until(&mut app, "denied provider turn completion", |app| {
        app.agents.threads[0].state == ThreadUiState::Ready
    });

    let thread = &app.agents.threads[0];
    assert_eq!(fs::read_to_string(&target).expect("read denied write"), "before\n");
    assert_eq!(app.backend.active().whole_text().as_deref(), Some("before\n"));
    assert_eq!(
        app.agents
            .action_log
            .iter()
            .filter(|action| format!("{action:?}").starts_with("Write {"))
            .count(),
        1,
        "only no-op warmup reaches the write bridge"
    );
    assert_eq!(scripted.request_bodies().len(), 2, "denial must reach provider tool result");
    assert!(
        scripted.request_bodies()[1]["messages"]
            .as_array()
            .expect("second model request messages")
            .iter()
            .any(|message| message["role"] == "tool"
                && message["tool_call_id"] == "write-live-denied"),
        "denied pane write result must reach concrete OpenRouter adapter"
    );
    assert!(
        thread.system_notices().iter().any(|notice| {
            notice.contains("verification: Blocked") && notice.contains("WriteDenied")
        }),
        "pane must render denied-write blocker: {:?}",
        thread.system_notices()
    );

    app.shutdown_agents();
    factory.shutdown();
}

#[test]
fn phase_six_live_openrouter_pane_dirty_buffer_reports_blocked_evidence() {
    let _live_lock = phase_six_live_lock();
    let workspace = tempfile::tempdir().expect("fixture workspace");
    let target = workspace.path().join("dirty.txt");
    fs::write(workspace.path().join(".ee.toml"), AGENTS_TOML).expect("write agents config");
    fs::write(&target, "before\n").expect("write baseline file");
    commit_git_baseline(workspace.path());

    let scripted = live_write_script(&target, "write-live-dirty", "after\n", "write conflicted");
    let state = tempfile::tempdir().expect("fixture session state");
    let factory = LiveOpenRouterTransport::new(
        openrouter_fixture_config(),
        state.path().join("agent-sessions"),
        scripted.clone(),
    );
    let mut app = live_openrouter_app_in(workspace.path(), factory.clone());
    let buffer_id = app.backend.open_buffer(Some(target.clone())).expect("open target buffer");
    app.backend.switch_to_id(buffer_id).expect("focus target buffer");
    wait_until(&mut app, "target buffer loaded", |app| {
        app.backend.active().whole_text().as_deref() == Some("before\n")
    });
    app.backend
        .replace_line_range(0, 0, &[String::from("unsaved user edit")])
        .expect("make target buffer dirty");
    app.backend.flush_all_pending_edits().expect("flush user edit");
    wait_until(&mut app, "target buffer dirty", |app| {
        app.backend.active().whole_text().as_deref() == Some("unsaved user edit\n")
    });

    open_pane_and_wait_ready(&mut app);
    select_live_write_mode(&mut app);
    type_text(&mut app, "conflict with dirty editor write");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "pane conflicted-write evidence", |app| {
        app.agents.threads[0].system_notices().iter().any(|notice| {
            notice.contains("verification: Blocked") && notice.contains("WriteConflicted")
        })
    });
    wait_until(&mut app, "dirty provider turn completion", |app| {
        app.agents.threads[0].state == ThreadUiState::Ready
    });

    let thread = &app.agents.threads[0];
    assert!(app.agents.approvals.is_empty(), "dirty scope must fail before approval");
    assert_eq!(fs::read_to_string(&target).expect("read conflicted write"), "before\n");
    assert_eq!(app.backend.active().whole_text().as_deref(), Some("unsaved user edit\n"));
    assert!(
        app.agents.action_log.iter().all(|action| !format!("{action:?}").starts_with("Write {"))
    );
    assert_eq!(scripted.request_bodies().len(), 2, "conflict must reach provider tool result");
    assert!(
        scripted.request_bodies()[1]["messages"]
            .as_array()
            .expect("second model request messages")
            .iter()
            .any(|message| message["role"] == "tool"
                && message["tool_call_id"] == "write-live-dirty"),
        "conflicted pane write result must reach concrete OpenRouter adapter"
    );
    assert!(
        thread.system_notices().iter().any(|notice| {
            notice.contains("verification: Blocked") && notice.contains("WriteConflicted")
        }),
        "pane must render dirty-buffer blocker: {:?}",
        thread.system_notices()
    );

    app.shutdown_agents();
    factory.shutdown();
}

#[test]
fn phase_six_live_openrouter_pane_partial_multi_file_apply_reports_blocked_evidence() {
    let _live_lock = phase_six_live_lock();
    let workspace = tempfile::tempdir().expect("fixture workspace");
    let first = workspace.path().join("first.txt");
    let second_directory = workspace.path().join("second-directory");
    fs::write(workspace.path().join(".ee.toml"), AGENTS_TOML).expect("write agents config");
    fs::write(&first, "before\n").expect("write first baseline");
    fs::create_dir(&second_directory).expect("create failing write target");
    commit_git_baseline(workspace.path());

    let scripted = live_write_script_in_rounds(
        &[
            ("write-first", first.as_path(), "after first\n"),
            ("write-second", second_directory.as_path(), "after second\n"),
        ],
        "partial apply reported",
    );
    let state = tempfile::tempdir().expect("fixture session state");
    let factory = LiveOpenRouterTransport::new(
        openrouter_fixture_config(),
        state.path().join("agent-sessions"),
        scripted.clone(),
    );
    let mut app = live_openrouter_app_in(workspace.path(), factory.clone());
    let buffer_id = app.backend.open_buffer(Some(first.clone())).expect("open first buffer");
    app.backend.switch_to_id(buffer_id).expect("focus first buffer");
    open_pane_and_wait_ready(&mut app);
    select_live_write_mode(&mut app);

    type_text(&mut app, "partially apply real multi-file write");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "first write approval", |app| !app.agents.approvals.is_empty());
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "second write approval", |app| !app.agents.approvals.is_empty());
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "pane partial-apply evidence", |app| {
        app.agents.threads[0].system_notices().iter().any(|notice| {
            notice.contains("verification: Blocked") && notice.contains("WriteFailed")
        })
    });
    wait_until(&mut app, "partial provider turn completion", |app| {
        app.agents.threads[0].state == ThreadUiState::Ready
    });

    let thread = &app.agents.threads[0];
    let summary = thread.terminal_evidence.as_ref().expect("partial-apply pane evidence");
    assert_eq!(summary.status, TurnTerminalStatus::Blocked);
    assert_eq!(summary.blocker, Some(TurnBlocker::WriteFailed));
    assert_eq!(summary.safe_follow_up, SafeFollowUp::RefreshEvidence);
    assert_eq!(fs::read_to_string(&first).expect("read first write"), "after first\n");
    assert!(second_directory.is_dir(), "failed second write must preserve directory target");
    assert!(
        thread.system_notices().iter().any(|notice| {
            notice.contains("verification: Blocked") && notice.contains("WriteFailed")
        }),
        "pane must render partial-apply blocker and follow-up: {:?}",
        thread.system_notices()
    );
    assert_eq!(scripted.request_bodies().len(), 3);

    app.shutdown_agents();
    factory.shutdown();
}

#[test]
fn phase_six_live_openrouter_pane_unavailable_terminal_validation_is_blocked() {
    let _live_lock = phase_six_live_lock();
    let workspace = tempfile::tempdir().expect("fixture workspace");
    let target = workspace.path().join("unavailable.txt");
    let missing_cwd = workspace.path().join("missing-terminal-cwd");
    fs::write(workspace.path().join(".ee.toml"), AGENTS_TOML).expect("write agents config");
    fs::write(&target, "before\n").expect("write baseline file");
    commit_git_baseline(workspace.path());

    let scripted = ScriptedOpenRouterCompletion::new(vec![
        live_tool_response(
            "write-unavailable",
            "write_file",
            json!({ "path": target.display().to_string(), "content": "after\n" }),
        ),
        live_tool_response(
            "terminal-unavailable",
            "create_terminal",
            json!({ "command": "echo unavailable", "cwd": missing_cwd.display().to_string() }),
        ),
        live_completion_response("terminal unavailable reported"),
    ]);
    let state = tempfile::tempdir().expect("fixture session state");
    let factory = LiveOpenRouterTransport::new(
        openrouter_fixture_config(),
        state.path().join("agent-sessions"),
        scripted.clone(),
    );
    let mut app = live_openrouter_app_in(workspace.path(), factory.clone());
    let buffer_id = app.backend.open_buffer(Some(target.clone())).expect("open target buffer");
    app.backend.switch_to_id(buffer_id).expect("focus target buffer");
    open_pane_and_wait_ready(&mut app);
    select_live_write_mode(&mut app);

    type_text(&mut app, "write then run unavailable selected validation");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "write approval", |app| app.agents.approvals.len() == 1);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "terminal approval", |app| app.agents.approvals.len() == 1);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "unavailable terminal evidence", |app| {
        app.agents.threads[0].terminal_evidence.as_ref().is_some_and(|summary| {
            summary.status == TurnTerminalStatus::Blocked
                && summary.blocker == Some(TurnBlocker::ValidationUnavailable)
                && summary.safe_follow_up == SafeFollowUp::RunSelectedValidation
        })
    });
    wait_until(&mut app, "unavailable terminal provider completion", |app| {
        app.agents.threads[0].state == ThreadUiState::Ready
    });

    let summary = app.agents.threads[0].terminal_evidence.as_ref().expect("unavailable evidence");
    assert_eq!(summary.blocker, Some(TurnBlocker::ValidationUnavailable));
    assert_eq!(fs::read_to_string(&target).expect("read agent write"), "after\n");
    assert!(!missing_cwd.exists());
    assert_eq!(scripted.request_bodies().len(), 3);

    app.shutdown_agents();
    factory.shutdown();
}

#[test]
fn phase_six_live_openrouter_pane_resume_reuses_completed_write() {
    flaky_regressions::phase_six_live_openrouter_pane_resume_reuses_completed_write();
}

#[test]
fn phase_six_fixture_matrix_reduces_live_host_evidence_before_completion() {
    struct Fixture {
        name: &'static str,
        observations: Vec<TurnObservation>,
        status: TurnTerminalStatus,
        blocker: TurnBlocker,
        follow_up: SafeFollowUp,
        evidence_ids: usize,
    }

    let revision = |value| EvidenceRevision::new(value);
    let current = revision("phase-six-current");
    let fixtures = vec![
        Fixture {
            name: "stale_context",
            observations: vec![
                TurnObservation::Revision { revision: revision("phase-six-captured") },
                TurnObservation::ChangedFiles {
                    revision: revision("phase-six-captured"),
                    files: vec![String::from("src/lib.rs")],
                    truncated: false,
                },
                TurnObservation::Diagnostics {
                    revision: revision("phase-six-captured"),
                    outcome: EvidenceCheck::Passed,
                },
                TurnObservation::DiffReview {
                    revision: revision("phase-six-captured"),
                    outcome: EvidenceCheck::Passed,
                },
                TurnObservation::Validation {
                    revision: revision("phase-six-captured"),
                    selected: true,
                    outcome: EvidenceCheck::Passed,
                },
                TurnObservation::Revision { revision: current.clone() },
            ],
            status: TurnTerminalStatus::Blocked,
            blocker: TurnBlocker::StaleRevision,
            follow_up: SafeFollowUp::RefreshEvidence,
            evidence_ids: 6,
        },
        Fixture {
            name: "repeated_validation_failure",
            observations: vec![
                TurnObservation::Revision { revision: current.clone() },
                TurnObservation::ChangedFiles {
                    revision: current.clone(),
                    files: vec![String::from("src/lib.rs")],
                    truncated: false,
                },
                TurnObservation::Diagnostics {
                    revision: current.clone(),
                    outcome: EvidenceCheck::Passed,
                },
                TurnObservation::DiffReview {
                    revision: current.clone(),
                    outcome: EvidenceCheck::Passed,
                },
                TurnObservation::Validation {
                    revision: current.clone(),
                    selected: true,
                    outcome: EvidenceCheck::Failed,
                },
                TurnObservation::Validation {
                    revision: current.clone(),
                    selected: true,
                    outcome: EvidenceCheck::Failed,
                },
            ],
            status: TurnTerminalStatus::Blocked,
            blocker: TurnBlocker::ValidationFailed,
            follow_up: SafeFollowUp::RefreshEvidence,
            evidence_ids: 6,
        },
        Fixture {
            name: "validation_skipped",
            observations: vec![
                TurnObservation::Revision { revision: current.clone() },
                TurnObservation::ChangedFiles {
                    revision: current.clone(),
                    files: vec![String::from("src/lib.rs")],
                    truncated: false,
                },
                TurnObservation::Diagnostics {
                    revision: current.clone(),
                    outcome: EvidenceCheck::Passed,
                },
                TurnObservation::DiffReview {
                    revision: current.clone(),
                    outcome: EvidenceCheck::Passed,
                },
                TurnObservation::Validation {
                    revision: current.clone(),
                    selected: true,
                    outcome: EvidenceCheck::Skipped,
                },
            ],
            status: TurnTerminalStatus::Blocked,
            blocker: TurnBlocker::ValidationSkipped,
            follow_up: SafeFollowUp::RunSelectedValidation,
            evidence_ids: 5,
        },
        Fixture {
            name: "validation_unavailable",
            observations: vec![
                TurnObservation::Revision { revision: current.clone() },
                TurnObservation::ChangedFiles {
                    revision: current.clone(),
                    files: vec![String::from("src/lib.rs")],
                    truncated: false,
                },
                TurnObservation::Diagnostics {
                    revision: current.clone(),
                    outcome: EvidenceCheck::Passed,
                },
                TurnObservation::DiffReview {
                    revision: current.clone(),
                    outcome: EvidenceCheck::Passed,
                },
                TurnObservation::Validation {
                    revision: current.clone(),
                    selected: true,
                    outcome: EvidenceCheck::Unavailable,
                },
            ],
            status: TurnTerminalStatus::Blocked,
            blocker: TurnBlocker::ValidationUnavailable,
            follow_up: SafeFollowUp::RunSelectedValidation,
            evidence_ids: 5,
        },
    ];

    for fixture in fixtures {
        let script = base_script().wait_for("session/prompt");
        let (mut app, _temp, fake) = fake_agents_app(script);
        open_pane_and_wait_ready(&mut app);
        let turn_id = begin_fixture_turn(&mut app, &fake);

        // Fixture facts must reach the live host while the ACP request remains
        // active. Never manufacture evidence after transport completion.
        for observation in fixture.observations {
            app.agents.threads[0]
                .host
                .observe_turn_evidence(turn_id, observation)
                .expect("live fixture turn accepts host evidence");
        }
        wait_until(&mut app, fixture.name, |app| {
            app.agents.threads[0].terminal_evidence.as_ref().is_some_and(|summary| {
                summary.status == fixture.status && summary.blocker == Some(fixture.blocker)
            })
        });

        let summary = app.agents.threads[0].terminal_evidence.as_ref().expect("pane evidence");
        let metrics = PhaseSixFixtureMetrics {
            prompt_requests: fake.agent().requests_by_method("session/prompt").len(),
            evidence_ids: summary.evidence_ids.len(),
            approvals: app.agents.approvals.len(),
        };
        assert_eq!(summary.safe_follow_up, fixture.follow_up, "fixture={}", fixture.name);
        assert_eq!(
            metrics,
            PhaseSixFixtureMetrics {
                prompt_requests: 1,
                evidence_ids: fixture.evidence_ids,
                approvals: 0,
            },
            "fixture={} metrics",
            fixture.name
        );
        assert_eq!(app.agents.threads[0].state, ThreadUiState::Running, "fixture={}", fixture.name);
        assert!(
            app.agents.threads[0].host.active_turn_key().is_some(),
            "fixture={} evidence must precede completion",
            fixture.name
        );
        assert!(
            app.agents.threads[0]
                .system_notices()
                .iter()
                .any(|notice| notice.contains("verification: Blocked")
                    && notice.contains(&format!("{:?}", fixture.blocker))),
            "fixture={} pane must reduce host blocker",
            fixture.name
        );
        app.shutdown_agents();
    }
}

#[test]
fn phase_six_resume_interruption_preserves_prompt_without_duplicate_acp_request() {
    // The agent answers the first prompt with a recoverable interruption
    // (deadline, durable checkpoint), then completes the resumed prompt.
    let script = base_script()
        .wait_for("session/prompt")
        .respond_error_with_data(
            -32603,
            "recoverable turn interruption: paused after 300s",
            json!({
                "recoverable": {
                    "fault": "deadline",
                    "detail": "paused after 300s",
                    "cause": null,
                    "safe_resume": true,
                    "retry_after": null,
                    "checkpoint_id": "s1-0000000001",
                    "completed_tool_calls": 4,
                    "resumed_count": 0,
                }
            }),
        )
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);
    fs::write(temp.path().join("resume-context.txt"), "original snapshot\n").unwrap();
    type_text(&mut app, "/context add resume-context.txt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    fs::write(temp.path().join("resume-context.txt"), "changed after attachment\n").unwrap();

    type_text(&mut app, "hello agent");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "turn paused notice", |app| {
        app.agents.threads[0].system_notices().iter().any(|n| n.contains("turn paused"))
    });
    assert_eq!(app.agents.threads[0].state, ThreadUiState::PausedRecoverable);
    let pending = app.agents.threads[0].pending_recovery.clone().expect("pending recovery kept");
    assert!(pending.info.safe_resume);
    assert_eq!(pending.info.checkpoint_id.as_deref(), Some("s1-0000000001"));
    assert_eq!(pending.info.completed_tool_calls, 4);
    // The original prompt is retained for Resume.
    let text = pending.prompt.iter().find_map(|block| match block {
        ContentBlock::Text(text) => Some(text.text.clone()),
        _ => None,
    });
    assert_eq!(text.as_deref(), Some("hello agent"));
    assert_eq!(pending.prompt.len(), 2, "context snapshot stays with paused turn");
    let context = match &pending.prompt[1] {
        ContentBlock::Text(text) => &text.text,
        _ => panic!("context block must be text"),
    };
    assert!(context.contains("original snapshot"), "{context}");
    assert!(!context.contains("changed after attachment"), "{context}");
    // Typing a new prompt while paused is rejected.
    type_text(&mut app, "new question");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].state, ThreadUiState::PausedRecoverable);
    assert!(app.agents.error.as_deref().is_some_and(|error| error.contains("resume")));

    // `/resume` re-sends the original prompt and the turn completes.
    press(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
    type_text(&mut app, "/resume");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "resumed prompt sent", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 2
    });
    let prompts = fake.agent().requests_by_method("session/prompt");
    let resumed = &prompts[1]["params"]["prompt"][1]["text"];
    assert!(resumed.as_str().is_some_and(|text| text.contains("original snapshot")), "{resumed}");
    wait_until(&mut app, "turn completed after resume", |app| {
        app.agents.threads[0].state == ThreadUiState::Ready
            && app.agents.threads[0].system_notices().iter().any(|n| n.contains("turn completed"))
    });
    assert!(app.agents.threads[0].pending_recovery.is_none(), "resume clears the pause");
    let summary = app.agents.threads[0].terminal_evidence.as_ref().expect("resumed evidence");
    assert_eq!(summary.status, TurnTerminalStatus::Unverified);
    assert_eq!(summary.blocker, Some(TurnBlocker::MissingRevision));
    assert_eq!(summary.safe_follow_up, SafeFollowUp::CollectCurrentRevision);
    assert_eq!(
        PhaseSixFixtureMetrics {
            prompt_requests: fake.agent().requests_by_method("session/prompt").len(),
            evidence_ids: summary.evidence_ids.len(),
            approvals: app.agents.approvals.len(),
        },
        PhaseSixFixtureMetrics { prompt_requests: 2, evidence_ids: 2, approvals: 0 },
        "resume retains pause and completion evidence while sending only original and resumed ACP prompts"
    );
}
