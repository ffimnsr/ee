//! `fs/write_text_file` bridge and lease tests.
use super::*;

// ── fs/write_text_file ───────────────────────────────────────────────────────

#[test]
fn write_denial_leaves_buffer_and_disk_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("guarded.txt");
    fs::write(&file, "original\n").unwrap();
    let path = file.to_string_lossy().to_string();

    let script = base_script().emit(write_text_file(103, "s1", &path, "changed\n"));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_buffer_and_wait(&mut app, &file);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "write approval appears", |app| app.agents.approvals.front().is_some());

    // Esc denies without touching anything.
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.agents.approvals.is_empty(), "approval resolved");

    wait_until(&mut app, "deny answered", |_| fake.agent().response_with_id(103).is_some());
    let response = fake.agent().response_with_id(103).expect("deny response");
    assert!(response.get("error").is_some(), "denied writes must error: {response}");
    assert_eq!(response["error"]["code"], -32602);
    assert_eq!(fs::read_to_string(&file).unwrap(), "original\n", "disk unchanged");
    wait_until(&mut app, "buffer text intact", |app| {
        // The editor model keeps a trailing empty line for a newline-terminated file.
        app.backend.lines == vec![String::from("original"), String::new()]
    });
}

#[test]
fn write_approval_updates_buffer_and_saves_file() {
    // The target file lives inside the app's workspace (the temp dir the
    // fake app is created in).
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("created.txt");
    let path = file.to_string_lossy().to_string();
    let script = base_script().emit(write_text_file(103, "s1", &path, "one\ntwo\n"));
    let (mut app, fake) = agents_app_in(&temp, script);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "write approval appears", |app| app.agents.approvals.front().is_some());
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // Allow once

    wait_until(&mut app, "write answered", |_| fake.agent().response_with_id(103).is_some());
    let response = fake.agent().response_with_id(103).expect("write answered");
    if response.get("result").is_none() {
        panic!("write did not succeed: {response}\napprovals={:?}", app.agents.approvals.len());
    }
    assert_eq!(response["result"], json!({}), "fs/write_text_file must return empty ACP result");
    assert_eq!(fs::read_to_string(&file).unwrap(), "one\ntwo\n", "file saved on disk");
    wait_until(&mut app, "buffer updated", |app| {
        app.backend
            .all_bufs()
            .iter()
            .find(|buf| buf.path.as_deref() == Some(file.as_path()))
            // The editor model keeps a trailing empty line for a
            // newline-terminated file.
            .is_some_and(|buf| {
                buf.lines == vec![String::from("one"), String::from("two"), String::new()]
            })
    });

    let log = app.agents_action_log();
    assert!(
        log.iter().any(|entry| matches!(
            entry,
            crate::app::ActionLogEntry::Write {
                path: logged,
                old_fingerprint,
                new_fingerprint,
                ..
            } if logged == &file && old_fingerprint != new_fingerprint
        )),
        "write must be logged with a real old fingerprint: {log:?}"
    );
}

#[test]
fn approval_modes_auto_approve_validated_native_writes() {
    for mode in [crate::app::ToolApprovalMode::Autopilot, crate::app::ToolApprovalMode::Bypass] {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join(format!("{}/target.txt", mode.label()));
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "original\n").unwrap();
        let path = file.to_string_lossy().to_string();
        let script = base_script().emit(write_text_file(103, "s1", &path, "changed\n"));
        let (mut app, fake) = agents_app_in(&temp, script);
        app.agents.approval_modes.insert(String::from("s1"), mode);

        open_pane_and_wait_ready(&mut app);
        wait_until(&mut app, "write answered and persisted", |_| {
            fake.agent().response_with_id(103).is_some()
                && fs::read_to_string(&file).ok().as_deref() == Some("changed\n")
        });

        let response = fake.agent().response_with_id(103).expect("write response");
        assert!(response.get("result").is_some(), "mode={} response={response}", mode.label());
        assert!(app.agents.approvals.is_empty(), "mode={} must not queue approval", mode.label());
        assert_eq!(fs::read_to_string(&file).unwrap(), "changed\n");
    }
}

#[test]
fn write_in_non_active_workspace_root_fails_before_approval() {
    let temp = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let active_file = temp.path().join("active.txt");
    let other_file = other.path().join("other.txt");
    fs::write(&active_file, "active\n").unwrap();
    fs::write(&other_file, "other\n").unwrap();
    let other_path = other_file.to_string_lossy().to_string();
    let script = base_script().emit(write_text_file(103, "s1", &other_path, "blocked\n"));
    let (mut app, fake) = agents_app_in(&temp, script);
    open_buffer_and_wait(&mut app, &other_file);
    open_buffer_and_wait(&mut app, &active_file);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "cross-root write answered", |_| {
        fake.agent().response_with_id(103).is_some()
    });
    let response = fake.agent().response_with_id(103).expect("response");
    assert!(response.get("error").is_some(), "cross-root writes must fail: {response}");
    assert_eq!(response["error"]["code"], -32602);
    let reason = response["error"]["data"]["reason"].as_str().unwrap_or_default();
    assert!(reason.contains("outside allowed workspace"), "reason: {reason}");
    assert!(app.agents.approvals.is_empty(), "invalid writes must fail before approval");
}

#[test]
fn dirty_user_edit_blocks_blind_agent_write() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("merge.txt");
    fs::write(&file, "one\ntwo\nthree\n").unwrap();
    let path = file.to_string_lossy().to_string();

    let script = base_script().emit(write_text_file(103, "s1", &path, "one\nTWO\nthreeX\n"));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_buffer_and_wait(&mut app, &file);
    wait_until(&mut app, "buffer content loaded", |app| {
        app.backend.lines
            == vec![String::from("one"), String::from("two"), String::from("three"), String::new()]
    });

    app.backend.replace_line_range(2, 2, &[String::from("threeX")]).unwrap();
    app.backend.flush_all_pending_edits().unwrap();
    wait_until(&mut app, "user edit lands", |app| {
        app.backend.lines
            == vec![String::from("one"), String::from("two"), String::from("threeX"), String::new()]
    });

    open_pane_and_wait_ready(&mut app);
    wait_until(&mut app, "dirty write rejected", |_| fake.agent().response_with_id(103).is_some());
    let response = fake.agent().response_with_id(103).expect("write response");
    assert!(response.get("error").is_some(), "dirty write must fail: {response}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "one\ntwo\nthree\n");
    assert_eq!(
        app.backend.lines,
        vec![String::from("one"), String::from("two"), String::from("threeX"), String::new()],
        "agent must not overwrite unsaved user edits"
    );
    assert!(app.agents.approvals.is_empty(), "dirty conflict must fail before approval");
}

#[test]
fn top_level_write_leases_reject_overlap_and_allow_disjoint_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let first_path = temp.path().join("first.txt");
    let second_path = temp.path().join("second.txt");
    fs::write(&first_path, "one\n").unwrap();
    fs::write(&second_path, "two\n").unwrap();
    let (mut app, _fake) = agents_app_in(&temp, base_script());

    let _first =
        app.queue_session_write_approval_for_test("fake", "s1", first_path.clone(), "agent one\n");
    assert_eq!(app.agents.approvals.len(), 1);
    assert_eq!(app.agents.write_leases.len(), 1);

    let mut overlapping =
        app.queue_session_write_approval_for_test("fake", "s2", first_path, "agent two\n");
    let error = overlapping
        .try_recv()
        .expect("overlap resolves immediately")
        .expect_err("overlap rejected");
    assert!(error.to_string().contains("session s1"), "error: {error}");
    assert_eq!(app.agents.approvals.len(), 1);
    assert_eq!(app.agents.write_leases.len(), 1);

    let _disjoint =
        app.queue_session_write_approval_for_test("fake", "s2", second_path, "agent two\n");
    assert_eq!(app.agents.approvals.len(), 2);
    assert_eq!(app.agents.write_leases.len(), 2);

    app.confirm_bridge_approval_for_test(crate::app::ApprovalChoice::DenyOnce);
    assert_eq!(app.agents.write_leases.len(), 1);
    app.confirm_bridge_approval_for_test(crate::app::ApprovalChoice::DenyOnce);
    assert_eq!(app.agents.write_leases.len(), 0);
}

#[test]
fn cancelled_write_request_releases_only_its_lease() {
    let temp = tempfile::tempdir().unwrap();
    let first_path = temp.path().join("cancelled.txt");
    let second_path = temp.path().join("held.txt");
    fs::write(&first_path, "one\n").unwrap();
    fs::write(&second_path, "two\n").unwrap();
    let (mut app, _fake) = agents_app_in(&temp, base_script());

    let cancelled =
        app.queue_session_write_approval_for_test("fake", "s1", first_path, "agent one\n");
    let _held = app.queue_session_write_approval_for_test("fake", "s2", second_path, "agent two\n");
    assert_eq!(app.agents.write_leases.len(), 2);
    drop(cancelled);

    app.prune_cancelled_bridge_approvals();
    assert_eq!(app.agents.approvals.len(), 1);
    assert_eq!(app.agents.approvals.front().expect("s2 approval").session_id, "s2");
    assert_eq!(app.agents.write_leases.len(), 1);
}

#[test]
fn write_lease_rechecks_revision_at_apply_time() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("stale.txt");
    fs::write(&path, "before\n").unwrap();
    let (mut app, _fake) = agents_app_in(&temp, base_script());

    let mut response =
        app.queue_session_write_approval_for_test("fake", "s1", path.clone(), "agent\n");
    fs::write(&path, "user\n").unwrap();
    app.confirm_bridge_approval_for_test(crate::app::ApprovalChoice::AllowOnce);

    let error =
        response.try_recv().expect("stale write resolves").expect_err("stale revision rejected");
    assert!(error.to_string().contains("changed after lease acquisition"), "error: {error}");
    assert_eq!(fs::read_to_string(path).unwrap(), "user\n");
    assert_eq!(app.agents.write_leases.len(), 0);
}
