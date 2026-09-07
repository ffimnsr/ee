//! `fs/read_text_file` bridge tests.
use super::*;

// ── fs/read_text_file ────────────────────────────────────────────────────────

#[test]
fn read_open_buffer_returns_unsaved_in_memory_text() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("notes.txt");
    fs::write(&file, "seed\n").unwrap();
    let path = file.to_string_lossy().to_string();
    let script = base_script().emit(wire::read_text_file("s1", &path));
    let (mut app, fake) = agents_app_in(&temp, script);

    // Open the file in the editor and type unsaved text.
    open_buffer_and_wait(&mut app, &file);
    wait_until(&mut app, "buffer content loaded", |app| {
        app.backend.lines == vec![String::from("seed"), String::new()]
    });
    press(&mut app, KeyCode::Char('i'), KeyModifiers::NONE);
    for ch in "unsaved".chars() {
        press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    wait_until(&mut app, "typed text lands", |app| {
        app.backend.lines.first().is_some_and(|line| line.starts_with("unsaved"))
    });

    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "read answered", |_| fake.agent().response_with_id(101).is_some());
    let response = fake.agent().response_with_id(101).expect("read response");
    let content = response["result"]["content"].as_str().expect("content");
    assert!(content.contains("unsaved"), "in-memory text must win: {content:?}");
    // Typing at column 0 prepends to the seeded line, so the response is the
    // in-memory line, never the stale disk text.
    assert!(!content.starts_with("seed"), "stale disk text must not be returned");

    // The read is recorded in the action log.
    let log = app.agents_action_log();
    assert!(
        log.iter().any(|entry| matches!(
            entry,
            crate::app::ActionLogEntry::Read { path: logged, .. } if logged == &file
        )),
        "read must be logged: {log:?}"
    );
}

#[test]
fn line_limited_read_uses_one_based_acp_ranges() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("lines.txt");
    fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();
    let path = file.to_string_lossy().to_string();

    let script = base_script()
        .emit(wire::read_text_file("s1", &path))
        .emit(read_text_file_with_range(104, "s1", &path, 2, 1));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_buffer_and_wait(&mut app, &file);
    // Wait for the content update to land so reads cannot race it under
    // parallel test load.
    wait_until(&mut app, "buffer content loaded", |app| {
        app.backend.lines
            == vec![
                String::from("alpha"),
                String::from("beta"),
                String::from("gamma"),
                String::new(),
            ]
    });
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "both reads answered", |_| {
        fake.agent().response_with_id(101).is_some() && fake.agent().response_with_id(104).is_some()
    });
    // 1-based semantics: line 2 limit 1 → "beta".
    let ranged = fake.agent().response_with_id(104).expect("range response");
    assert_eq!(ranged["result"]["content"], "beta");
    // The unbounded read returns the whole buffer.
    let whole = fake.agent().response_with_id(101).expect("whole response");
    assert_eq!(whole["result"]["content"], "alpha\nbeta\ngamma");
}

#[test]
fn read_rejects_zero_based_line_numbers() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("lines.txt");
    fs::write(&file, "alpha\nbeta\n").unwrap();
    let path = file.to_string_lossy().to_string();
    let script = base_script().emit(read_text_file_with_range(104, "s1", &path, 0, 1));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "invalid read answered", |_| fake.agent().response_with_id(104).is_some());
    let response = fake.agent().response_with_id(104).expect("response");
    assert!(response.get("error").is_some(), "zero-based reads must fail: {response}");
    assert_eq!(response["error"]["code"], -32602);
    assert_eq!(response["error"]["data"]["reason"], "line must be 1-based");
}

#[test]
fn read_in_non_active_workspace_root_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let active_file = temp.path().join("active.txt");
    let other_file = other.path().join("other.txt");
    fs::write(&active_file, "active\n").unwrap();
    fs::write(&other_file, "other\n").unwrap();
    let other_path = other_file.to_string_lossy().to_string();
    let script = base_script().emit(wire::read_text_file("s1", &other_path));
    let (mut app, fake) = agents_app_in(&temp, script);
    open_buffer_and_wait(&mut app, &other_file);
    open_buffer_and_wait(&mut app, &active_file);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "cross-root read answered", |_| {
        fake.agent().response_with_id(101).is_some()
    });
    let response = fake.agent().response_with_id(101).expect("response");
    assert!(response.get("error").is_some(), "cross-root reads must fail: {response}");
    assert_eq!(response["error"]["code"], -32602);
    let reason = response["error"]["data"]["reason"].as_str().unwrap_or_default();
    assert!(reason.contains("outside allowed workspace"), "reason: {reason}");
}

#[test]
fn read_outside_workspace_fails_closed() {
    let path = "/etc/hostname".to_string();
    let script = base_script().emit(wire::read_text_file("s1", &path));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "read answered", |_| fake.agent().response_with_id(101).is_some());
    let response = fake.agent().response_with_id(101).expect("response");
    assert!(response.get("error").is_some(), "outside-workspace reads must fail: {response}");
    assert_eq!(response["error"]["code"], -32602);
    let reason = response["error"]["data"]["reason"].as_str().unwrap_or_default();
    assert!(reason.contains("workspace"), "reason: {reason}");
}

#[test]
fn vlf_unbounded_read_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("huge.bin");
    fs::write(&file, "a\nb\nc\n").unwrap();
    let path = file.to_string_lossy().to_string();
    let script = base_script().emit(wire::read_text_file("s1", &path));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_buffer_and_wait(&mut app, &file);
    // Drain the xi-core `document_mode` notification and content update so
    // `is_vlf` is not reset before the agent request is processed.
    wait_until(&mut app, "buffer content loaded", |app| {
        // Newline-terminated files keep a trailing empty line in the model.
        app.backend.lines
            == vec![String::from("a"), String::from("b"), String::from("c"), String::new()]
    });
    // Simulate a very-large-file buffer with a small cached viewport.
    app.backend.is_vlf = true;
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "vlf read answered", |_| fake.agent().response_with_id(101).is_some());
    let response = fake.agent().response_with_id(101).expect("response");
    assert!(response.get("error").is_some(), "unbounded VLF reads must be rejected: {response}");
    let reason = response["error"]["data"]["reason"].as_str().unwrap_or_default();
    assert!(reason.contains("very large"), "reason: {reason}");
}
