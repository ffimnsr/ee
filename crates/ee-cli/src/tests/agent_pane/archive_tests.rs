//! Durable transcript archive and archived-session persistence tests.
use super::*;

fn restart_script(load: bool, fresh: bool) -> FakeAgentScript {
    let mut script = FakeAgentScript::new().wait_for("initialize").respond(json!({
        "protocolVersion": 1,
        "agentCapabilities": { "loadSession": load }
    }));
    if fresh {
        script = script
            .wait_for("session/new")
            .respond(json!({ "sessionId": "s2" }))
            .wait_for("session/set_mode")
            .respond(json!({}));
    }
    if load {
        script = script
            .wait_for("session/load")
            .respond(json!({}))
            .emit(wire::session_update("s1", wire::agent_message_chunk("replay-1", "replay view")))
            .wait_for("session/set_mode")
            .respond(json!({}));
    }
    script
}

fn transcript_dirs(root: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.join("meta.json").is_file() {
                out.push(path);
            } else {
                transcript_dirs(&path, out);
            }
        }
    }
}

#[test]
fn live_transcript_is_not_trimmed_and_restores_from_the_journal() {
    let workspace = tempfile::tempdir().unwrap();
    let state_dir = tempfile::tempdir().unwrap();
    let (mut first_app, _fake) = fake_agents_app_in(workspace.path(), base_script());
    first_app.agents.test_session_state_base = Some(state_dir.path().to_path_buf());
    open_pane_and_wait_ready(&mut first_app);

    let thread = &mut first_app.agents.threads[0];
    for index in 0..1_600 {
        thread
            .transcript
            .push(TranscriptItem::System { text: format!("line {index}"), at: SystemTime::now() });
    }
    thread
        .transcript
        .push(TranscriptItem::System { text: String::from("last line"), at: SystemTime::now() });
    assert!(first_app.agents.threads[0].transcript.len() >= 1_601);
    first_app.shutdown_agents();
    drop(first_app);

    let (mut restarted, restarted_fake) =
        fake_agents_app_in(workspace.path(), restart_script(true, false));
    restarted.agents.test_session_state_base = Some(state_dir.path().to_path_buf());
    crate::tests::agent_pane::run_ex(&mut restarted, "agents");
    wait_until(&mut restarted, "journal transcript restored", |app| {
        app.agents.threads.first().is_some_and(|thread| {
            thread.state == ThreadUiState::Ready && thread.transcript.len() >= 1_602
        })
    });
    let transcript = &restarted.agents.threads[0].transcript;
    assert!(
        transcript.iter().any(|item| {
            matches!(item, TranscriptItem::System { text, .. } if text == "line 0")
        })
    );
    assert!(transcript.iter().any(|item| {
        matches!(item, TranscriptItem::System { text, .. } if text == "last line")
    }));
    assert!(
        !transcript.iter().any(|item| {
            matches!(item, TranscriptItem::Message { text, .. } if text == "replay view")
        }),
        "bounded provider replay must not replace the durable journal transcript"
    );
    assert_eq!(
        restarted_fake.agent().requests_by_method("session/load").len(),
        1,
        "restore uses session/load"
    );
}

#[test]
fn archived_session_survives_restart_and_can_be_restored() {
    let workspace = tempfile::tempdir().unwrap();
    let state_dir = tempfile::tempdir().unwrap();
    let (mut first_app, _fake) = fake_agents_app_in(workspace.path(), base_script());
    first_app.agents.test_session_state_base = Some(state_dir.path().to_path_buf());
    open_pane_and_wait_ready(&mut first_app);
    first_app.agents.threads[0].transcript.push(TranscriptItem::System {
        text: String::from("archived marker"),
        at: SystemTime::now(),
    });
    type_text(&mut first_app, "/archive");
    press(&mut first_app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(first_app.agents.threads.is_empty());
    assert_eq!(first_app.agents.archived_persisted.len(), 1);
    first_app.shutdown_agents();
    drop(first_app);

    let (mut restarted, restarted_fake) =
        fake_agents_app_in(workspace.path(), restart_script(true, true));
    restarted.agents.test_session_state_base = Some(state_dir.path().to_path_buf());
    crate::tests::agent_pane::run_ex(&mut restarted, "agents");
    wait_until(&mut restarted, "fresh session ready", |app| {
        app.agents.threads.first().is_some_and(|thread| thread.state == ThreadUiState::Ready)
    });
    assert_eq!(
        restarted.agents.archived_persisted.len(),
        1,
        "archived metadata reloads from the registry"
    );

    type_text(&mut restarted, "/archive list");
    press(&mut restarted, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        restarted
            .backend
            .status_message
            .as_deref()
            .is_some_and(|message| message.contains("archived sessions:") && message.contains("s1")),
        "archive list should name the persisted session: {:?}",
        restarted.backend.status_message
    );

    type_text(&mut restarted, "/archive restore 1");
    press(&mut restarted, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut restarted, "archived session reconnected", |app| {
        app.agents.threads.len() == 2
            && app.agents.threads[1].state == ThreadUiState::Ready
            && app.agents.threads[1].transcript.iter().any(|item| {
                matches!(item, TranscriptItem::System { text, .. } if text == "archived marker")
            })
    });
    assert!(restarted.agents.archived_persisted.is_empty());
    assert_eq!(restarted_fake.agent().requests_by_method("session/load").len(), 1);
}

#[test]
fn delete_removes_the_session_journal() {
    let workspace = tempfile::tempdir().unwrap();
    let state_dir = tempfile::tempdir().unwrap();
    let (mut app, _fake) = fake_agents_app_in(workspace.path(), base_script());
    app.agents.test_session_state_base = Some(state_dir.path().to_path_buf());
    open_pane_and_wait_ready(&mut app);
    let mut journals = Vec::new();
    transcript_dirs(&state_dir.path().join("agent-sessions").join("transcripts"), &mut journals);
    assert_eq!(journals.len(), 1, "journal created for the live session");

    type_text(&mut app, "/delete");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.agents.threads.is_empty());
    let mut journals = Vec::new();
    transcript_dirs(&state_dir.path().join("agent-sessions").join("transcripts"), &mut journals);
    assert!(journals.is_empty(), "delete removes the journal: {journals:?}");
}

#[test]
fn legacy_registry_transcript_migrates_into_the_journal() {
    let workspace = tempfile::tempdir().unwrap();
    let state_dir = tempfile::tempdir().unwrap();
    let canonical = fs::canonicalize(workspace.path()).unwrap();
    let identity = crate::policy::WorkspaceIdentity::from_canonical_root_bytes(
        canonical.as_os_str().as_encoded_bytes(),
    );
    let at = json!({"secs_since_epoch": 0u64, "nanos_since_epoch": 0u32});
    let document = json!({
        identity.as_string(): {
            "version": 3,
            "active_session_id": "s1",
            "sessions": [{
                "agent_id": "fake",
                "session_id": "s1",
                "last_prompt": null,
                "session_name": null,
                "transcript": [{
                    "System": { "text": "legacy marker", "at": at }
                }]
            }],
            "archived_sessions": []
        }
    });
    fs::write(state_dir.path().join("agent-sessions.json"), serde_json::to_vec(&document).unwrap())
        .unwrap();

    let (mut restarted, _fake) = fake_agents_app_in(workspace.path(), restart_script(true, false));
    restarted.agents.test_session_state_base = Some(state_dir.path().to_path_buf());
    crate::tests::agent_pane::run_ex(&mut restarted, "agents");
    wait_until(&mut restarted, "legacy transcript restored", |app| {
        app.agents.threads.first().is_some_and(|thread| {
            thread.state == ThreadUiState::Ready
                && thread.transcript.iter().any(|item| {
                    matches!(item, TranscriptItem::System { text, .. } if text == "legacy marker")
                })
        })
    });
    let mut journals = Vec::new();
    transcript_dirs(&state_dir.path().join("agent-sessions").join("transcripts"), &mut journals);
    assert_eq!(journals.len(), 1, "legacy transcript was migrated into the journal");
    let journal = fs::read_to_string(journals[0].join("transcript.jsonl")).unwrap();
    assert!(journal.contains("legacy marker"));
}

#[test]
fn live_turn_after_restore_is_not_mistaken_for_load_replay() {
    let workspace = tempfile::tempdir().unwrap();
    let state_dir = tempfile::tempdir().unwrap();
    let (mut first_app, _fake) = fake_agents_app_in(workspace.path(), base_script());
    first_app.agents.test_session_state_base = Some(state_dir.path().to_path_buf());
    open_pane_and_wait_ready(&mut first_app);
    first_app.agents.threads[0].transcript.push(TranscriptItem::System {
        text: String::from("durable marker"),
        at: SystemTime::now(),
    });
    first_app.shutdown_agents();
    drop(first_app);

    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": { "loadSession": true }
        }))
        .wait_for("session/load")
        .respond(json!({}))
        .emit(wire::session_update("s1", wire::agent_message_chunk("replay", "bounded replay")))
        .wait_for("session/set_mode")
        .respond(json!({}))
        .wait_for("session/prompt")
        .emit(wire::session_update("s1", wire::agent_message_chunk("live", "live answer")))
        .respond(json!({}));
    let (mut restarted, _fake) = fake_agents_app_in(workspace.path(), script);
    restarted.agents.test_session_state_base = Some(state_dir.path().to_path_buf());
    crate::tests::agent_pane::run_ex(&mut restarted, "agents");
    wait_until(&mut restarted, "journal transcript restored", |app| {
        app.agents.threads.first().is_some_and(|thread| {
            thread.state == ThreadUiState::Ready
                && thread.transcript.iter().any(|item| {
                    matches!(item, TranscriptItem::System { text, .. } if text == "durable marker")
                })
        })
    });
    assert!(restarted.agents.threads[0].transcript_archive_loaded);

    type_text(&mut restarted, "next turn");
    press(&mut restarted, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut restarted, "live turn streamed after restore", |app| {
        app.agents.threads[0].transcript.iter().any(
            |item| matches!(item, TranscriptItem::Message { text, .. } if text == "live answer"),
        )
    });
    assert!(!restarted.agents.threads[0].transcript_archive_loaded);
    assert!(
        !restarted.agents.threads[0].transcript.iter().any(|item| {
            matches!(item, TranscriptItem::Message { text, .. } if text == "bounded replay")
        }),
        "the restore replay must stay ignored while the new turn is live"
    );
}

#[test]
fn failed_archived_restore_keeps_the_archive_record() {
    let workspace = tempfile::tempdir().unwrap();
    let state_dir = tempfile::tempdir().unwrap();
    let (mut first_app, _fake) = fake_agents_app_in(workspace.path(), base_script());
    first_app.agents.test_session_state_base = Some(state_dir.path().to_path_buf());
    open_pane_and_wait_ready(&mut first_app);
    type_text(&mut first_app, "/archive");
    press(&mut first_app, KeyCode::Enter, KeyModifiers::NONE);
    first_app.shutdown_agents();
    drop(first_app);

    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": { "loadSession": true }
        }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s2" }))
        .wait_for("session/set_mode")
        .respond(json!({}))
        .wait_for("session/load")
        .respond_error(-32603, "load exploded");
    let (mut restarted, _fake) = fake_agents_app_in(workspace.path(), script);
    restarted.agents.test_session_state_base = Some(state_dir.path().to_path_buf());
    crate::tests::agent_pane::run_ex(&mut restarted, "agents");
    wait_until(&mut restarted, "fresh session ready", |app| {
        app.agents.threads.first().is_some_and(|thread| thread.state == ThreadUiState::Ready)
    });
    type_text(&mut restarted, "/archive restore 1");
    press(&mut restarted, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut restarted, "archived restore failure", |app| {
        app.agents.error.as_deref().is_some_and(|error| error.contains("session/load"))
            || app.agents.error.as_deref().is_some_and(|error| error.contains("lifecycle failed"))
    });
    assert_eq!(restarted.agents.archived_persisted.len(), 1);
}
