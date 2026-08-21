//! Phase 4 bridge regression tests: ACP `fs/*` and `terminal/*` client
//! methods against real buffers, the save pipeline, and tracked terminals.
//!
//! Everything flows through the full host stack (fake agent → host handler →
//! pane approval) exactly as in the TUI loop.  No external binaries are
//! spawned except the terminal commands under test (`sh`, `sleep`).

use std::fs;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ee_agent_host::FakeTransportFactory;
use ee_agent_host::fake::{CaptureSource, FakeAgent, FakeAgentScript, FakeAgentTransport, wire};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};

use crate::app::{App, ThreadUiState};
use crate::tests::helpers::*;
use crate::ui::ui;

const WAIT: Duration = Duration::from_secs(5);

// ── Shared harness (mirrors tests/agent_pane.rs helpers) ─────────────────────

#[derive(Clone)]
pub(crate) struct ScriptedFake {
    script: FakeAgentScript,
    handle: Arc<Mutex<Option<FakeAgent>>>,
}

impl ScriptedFake {
    fn new(script: FakeAgentScript) -> Self {
        Self { script, handle: Arc::new(Mutex::new(None)) }
    }

    pub(crate) fn agent(&self) -> FakeAgent {
        self.handle
            .lock()
            .expect("fake handle poisoned")
            .clone()
            .expect("fake agent not spawned yet (open the pane first)")
    }
}

impl FakeTransportFactory for ScriptedFake {
    fn build(&self) -> FakeAgentTransport {
        let (fake, transport) = FakeAgent::spawn(self.script.clone());
        *self.handle.lock().expect("fake handle poisoned") = Some(fake);
        transport
    }
}

pub(crate) fn base_script() -> FakeAgentScript {
    FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
}

const AGENTS_TOML: &str = r#"
[agents]
enabled = true

[agents.servers.fake]
command = "unused"
"#;

pub(crate) fn agents_app_in(
    temp: &tempfile::TempDir,
    script: FakeAgentScript,
) -> (App, ScriptedFake) {
    fs::write(temp.path().join(".ee.toml"), AGENTS_TOML).unwrap();
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let mut app = App::from_path(None).unwrap();
    drop(_cwd_restore);
    drop(_cwd_lock);
    let fake = ScriptedFake::new(script);
    app.agents.test_fake_transports.insert(String::from("fake"), Arc::new(fake.clone()));
    (app, fake)
}

fn fake_agents_app(script: FakeAgentScript) -> (App, tempfile::TempDir, ScriptedFake) {
    let temp = tempfile::tempdir().unwrap();
    let (app, fake) = agents_app_in(&temp, script);
    (app, temp, fake)
}

fn wait_until(app: &mut App, label: &str, mut condition: impl FnMut(&App) -> bool) {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        app.pump_agents();
        let _ = app.backend.drain_events();
        if condition(app) {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "timed out waiting for {label}; mode={:?} approvals={} permission={} elicitation={} status={:?} active={:?} lines={:?}",
        app.mode,
        app.agents.approvals.len(),
        app.agents.permission.is_some(),
        app.agents.elicitation.is_some(),
        app.backend.status_message.as_deref(),
        app.backend.active().path,
        app.backend.lines
    );
}

fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    app.handle_event(Event::Key(KeyEvent::new(code, modifiers)));
}

fn open_pane_and_wait_ready(app: &mut App) {
    run_ex(app, "agents");
    wait_until(app, "first agent thread ready", |app| {
        app.agents.threads.len() == 1 && app.agents.threads[0].state == ThreadUiState::Ready
    });
}

fn open_buffer_and_wait(app: &mut App, path: &std::path::Path) {
    // `open_buffer` adds an inactive view; the bridge serves reads from the
    // active buffer, so switch to it explicitly.
    let id = app.backend.open_buffer(Some(path.to_path_buf())).unwrap();
    app.backend.switch_to_id(id).unwrap();
    wait_until(app, "buffer open", |app| app.backend.active().path.as_deref() == Some(path));
}

// ── Wire helpers ─────────────────────────────────────────────────────────────

pub(crate) fn write_text_file(id: i64, session_id: &str, path: &str, content: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "fs/write_text_file",
        "params": { "sessionId": session_id, "path": path, "content": content }
    })
}

fn read_text_file_with_range(
    id: i64,
    session_id: &str,
    path: &str,
    line: u32,
    limit: u32,
) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "fs/read_text_file",
        "params": { "sessionId": session_id, "path": path, "line": line, "limit": limit }
    })
}

fn terminal_create(id: i64, session_id: &str, command: &str, args: Value, extra: Value) -> Value {
    let mut params = json!({ "sessionId": session_id, "command": command, "args": args });
    if let Some(obj) = extra.as_object() {
        for (key, value) in obj {
            params[key] = value.clone();
        }
    }
    json!({ "jsonrpc": "2.0", "id": id, "method": "terminal/create", "params": params })
}

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
        wait_until(&mut app, "write answered", |_| fake.agent().response_with_id(103).is_some());

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
fn concurrent_user_edit_merges_into_agent_write() {
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

    // The user edits line 3 while the agent write is queued.
    app.backend.replace_line_range(2, 2, &[String::from("threeX")]).unwrap();
    app.backend.flush_all_pending_edits().unwrap();
    wait_until(&mut app, "user edit lands", |app| {
        // Newline-terminated files keep a trailing empty line in the model.
        app.backend.lines
            == vec![String::from("one"), String::from("two"), String::from("threeX"), String::new()]
    });

    open_pane_and_wait_ready(&mut app);
    wait_until(&mut app, "write approval appears", |app| app.agents.approvals.front().is_some());
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // Allow once

    wait_until(&mut app, "merged write lands", |_| {
        fake.agent().response_with_id(103).is_some_and(|response| response.get("result").is_some())
    });
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "one\nTWO\nthreeX\n",
        "user edit survives the agent write"
    );
}

// ── terminal bridge ──────────────────────────────────────────────────────────

#[test]
fn bypass_mode_keeps_invalid_terminal_requests_on_approval_path() {
    let script =
        base_script().emit(terminal_create(102, "s1", "sh", json!(["-c", "echo hi"]), json!({})));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    app.agents.approval_modes.insert(String::from("s1"), crate::app::ToolApprovalMode::Bypass);

    open_pane_and_wait_ready(&mut app);
    wait_until(&mut app, "invalid terminal approval appears", |app| {
        app.agents.approvals.front().is_some()
    });
    assert_eq!(app.agents.terminals.tracked_count(), 0);
}

#[test]
fn terminal_denial_does_not_spawn_process() {
    let script =
        base_script().emit(terminal_create(102, "s1", "sh", json!(["-c", "echo hi"]), json!({})));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "terminal approval appears", |app| app.agents.approvals.front().is_some());
    // The approval detail shows the command but no secret values.
    let approval = app.agents.approvals.front().expect("approval queued");
    assert_eq!(approval.title, "terminal/create");
    assert!(approval.detail.contains("echo hi"), "detail: {}", approval.detail);

    press(&mut app, KeyCode::Esc, KeyModifiers::NONE); // deny
    wait_until(&mut app, "deny answered", |_| fake.agent().response_with_id(102).is_some());
    let response = fake.agent().response_with_id(102).expect("deny response");
    assert!(response.get("error").is_some(), "denied terminals must error: {response}");
    assert!(
        response["result"].as_object().and_then(|result| result.get("terminalId")).is_none(),
        "no terminal id on denial"
    );
}

#[test]
fn terminal_approval_renders_only_in_composer_and_highlights_each_choice() {
    let script = base_script().emit(terminal_create(
        102,
        "s1",
        "git",
        json!(["--no-pager", "log", "--oneline", "--decorate", "--all", "--branches", "--remotes"]),
        json!({}),
    ));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);
    wait_until(&mut app, "terminal approval appears", |app| app.agents.approvals.front().is_some());

    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.agents.approvals.front().expect("approval").selected, 1);

    let backend = TestBackend::new(72, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let rendered: Vec<String> =
        (0..20).map(|y| (0..72).map(|x| rows.cell((x, y)).unwrap().symbol()).collect()).collect();

    for fragment in ["git", "--no-pager", "--decorate", "--remotes"] {
        assert!(
            rendered.iter().any(|row| row.contains(fragment)),
            "approval detail must wrap instead of clipping {fragment:?}: {rendered:#?}"
        );
    }
    assert!(
        !rendered.iter().any(|row| row.contains("approval: terminal/create")),
        "approval must not duplicate into chat transcript: {rendered:#?}"
    );
    let approval_start = rendered
        .iter()
        .position(|row| row.contains("approval required [2/"))
        .expect("expanded approval header");
    let approval_rows = &rendered[approval_start..];
    let allow_once_row =
        approval_rows.iter().position(|row| row.contains("Allow once")).expect("allow once row");
    let allow_session_row = approval_rows
        .iter()
        .position(|row| row.contains("> Allow session"))
        .expect("selected allow session row");
    assert_ne!(allow_once_row, allow_session_row, "choices need individual rows: {rendered:#?}");
    assert!(
        approval_rows.iter().any(|row| row.contains("↑/↓ select")),
        "approval controls must advertise vertical navigation: {rendered:#?}"
    );
}

#[test]
fn terminal_output_is_capped_and_preserves_final_visible_output() {
    let script = base_script()
        .emit(terminal_create(
            102,
            "s1",
            "sh",
            json!(["-c", "printf aaaaabbbbb"]),
            json!({ "outputByteLimit": 8 }),
        ))
        .capture(CaptureSource::Response { id: 102 }, "result.terminalId", "term_id")
        .delay(400)
        .emit(json!({
            "jsonrpc": "2.0",
            "id": 104,
            "method": "terminal/output",
            "params": { "sessionId": "s1", "terminalId": { "$capture": "term_id" } }
        }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "terminal approval appears", |app| app.agents.approvals.front().is_some());
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // Allow once

    wait_until(&mut app, "terminal create answered", |_| {
        fake.agent().response_with_id(102).is_some()
    });
    let created = fake.agent().response_with_id(102).expect("create response");
    assert!(created.get("result").is_some(), "terminal create must succeed: {created}");

    wait_until(&mut app, "terminal output answered", |_| {
        fake.agent().response_with_id(104).is_some()
    });
    let output = fake.agent().response_with_id(104).expect("output response");
    let text = output["result"]["output"].as_str().expect("output text");
    assert_eq!(text.len(), 8, "output capped at the request limit: {text:?}");
    assert!(text.ends_with("bbbbb"), "final visible output preserved: {text:?}");
    assert_eq!(output["result"]["truncated"], true);
}

#[test]
fn terminal_kill_resolves_wait_for_exit() {
    let script = base_script()
        .emit(terminal_create(102, "s1", "sleep", json!(["30"]), json!({})))
        .capture(CaptureSource::Response { id: 102 }, "result.terminalId", "term_id")
        .emit(json!({
            "jsonrpc": "2.0",
            "id": 104,
            "method": "terminal/output",
            "params": { "sessionId": "s1", "terminalId": { "$capture": "term_id" } }
        }))
        .emit(json!({
            "jsonrpc": "2.0",
            "id": 105,
            "method": "terminal/wait_for_exit",
            "params": { "sessionId": "s1", "terminalId": { "$capture": "term_id" } }
        }))
        .emit(json!({
            "jsonrpc": "2.0",
            "id": 106,
            "method": "terminal/kill",
            "params": { "sessionId": "s1", "terminalId": { "$capture": "term_id" } }
        }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "terminal approval appears", |app| app.agents.approvals.front().is_some());
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // Allow once

    wait_until(&mut app, "terminal output answered", |_| {
        fake.agent().response_with_id(104).is_some()
    });
    // ACP v1 `terminal/output` carries output, truncation, and an optional
    // exit status — no running flag.  Liveness is the absent exit status.
    let output = fake.agent().response_with_id(104).expect("output response");
    assert_eq!(output["result"]["output"], "", "sleep writes nothing: {output}");
    assert_eq!(output["result"]["truncated"], false);
    assert_eq!(
        output["result"]["exitStatus"],
        Value::Null,
        "sleep must still be active (no exit status yet): {output}"
    );

    wait_until(&mut app, "kill answered", |_| fake.agent().response_with_id(106).is_some());
    wait_until(&mut app, "wait resolved after kill", |_| {
        fake.agent().response_with_id(105).is_some()
    });
    let waited = fake.agent().response_with_id(105).expect("wait response");
    // ACP v1 `terminal/wait_for_exit` result carries the exit status directly:
    // `{ "signal": "9" }` when the terminal was SIGKILLed.
    assert_eq!(
        waited["result"]["signal"], "9",
        "wait_for_exit must report the kill signal: {waited}"
    );
    assert_eq!(fake.agent().response_with_id(106).expect("kill response").get("error"), None);
}

#[test]
fn terminal_snapshot_reports_running_and_monotonic_elapsed_ms() {
    use crate::app::AgentTerminals;
    use ee_agent_protocol::{CreateTerminalRequest, KillTerminalRequest, SessionId, TerminalId};

    let terminals = AgentTerminals::default();
    let created = terminals
        .spawn(
            &CreateTerminalRequest::new(SessionId::new("s1"), "sleep")
                .args(vec![String::from("30")]),
        )
        .expect("terminal spawns");
    let terminal_id = TerminalId::new(created.terminal_id.0.to_string());

    let request = |terminal_id: TerminalId| {
        ee_agent_protocol::TerminalOutputRequest::new(SessionId::new("s1"), terminal_id)
    };
    let first = terminals.output_snapshot(&request(terminal_id.clone())).expect("live snapshot");
    assert!(first.running, "sleep must still be active in the snapshot");

    thread::sleep(Duration::from_millis(5));
    let second = terminals.output_snapshot(&request(terminal_id.clone())).expect("later snapshot");
    assert!(
        second.elapsed_ms >= first.elapsed_ms,
        "elapsedMs must be monotonic: {} then {}",
        first.elapsed_ms,
        second.elapsed_ms
    );

    terminals
        .kill(&KillTerminalRequest::new(SessionId::new("s1"), terminal_id.clone()))
        .expect("kill succeeds");
    let after = terminals.output_snapshot(&request(terminal_id)).expect("snapshot after kill");
    assert!(!after.running, "killed terminal must report not running");
}

#[test]
fn terminal_release_invalidates_acp_id_but_keeps_output_displayable() {
    let script = base_script()
        .emit(terminal_create(102, "s1", "sh", json!(["-c", "printf hello"]), json!({})))
        .capture(CaptureSource::Response { id: 102 }, "result.terminalId", "term_id")
        .emit(json!({
            "jsonrpc": "2.0",
            "id": 105,
            "method": "terminal/wait_for_exit",
            "params": { "sessionId": "s1", "terminalId": { "$capture": "term_id" } }
        }))
        .wait_for_response(105)
        .emit(json!({
            "jsonrpc": "2.0",
            "id": 104,
            "method": "terminal/output",
            "params": { "sessionId": "s1", "terminalId": { "$capture": "term_id" } }
        }))
        .wait_for_response(104)
        .emit(json!({
            "jsonrpc": "2.0",
            "id": 107,
            "method": "terminal/release",
            "params": { "sessionId": "s1", "terminalId": { "$capture": "term_id" } }
        }))
        .emit(json!({
            "jsonrpc": "2.0",
            "id": 108,
            "method": "terminal/output",
            "params": { "sessionId": "s1", "terminalId": { "$capture": "term_id" } }
        }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "terminal approval appears", |app| app.agents.approvals.front().is_some());
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "release answered", |_| fake.agent().response_with_id(107).is_some());
    wait_until(&mut app, "output rejected after release", |_| {
        fake.agent().response_with_id(108).is_some()
    });
    let created = fake.agent().response_with_id(102).expect("create response");
    let terminal_id = created["result"]["terminalId"].as_str().expect("terminal id");
    let display =
        app.agents.terminals.display_output(terminal_id).expect("released display snapshot");
    assert_eq!(display.output, "hello");
    let response = fake.agent().response_with_id(108).expect("output response");
    assert_eq!(response["error"]["code"], -32602, "released ids must be invalid: {response}");
}

#[test]
fn terminal_ids_are_session_owned() {
    let terminals = crate::app::AgentTerminals::default();
    let request = ee_agent_protocol::CreateTerminalRequest::new("s1", "sh")
        .args(vec![String::from("-c"), String::from("printf ok")]);
    let created = terminals.spawn(&request).expect("terminal spawns");
    let terminal_id = created.terminal_id.0.to_string();

    let denied =
        terminals.output(&ee_agent_protocol::TerminalOutputRequest::new("s2", terminal_id.clone()));
    assert!(denied.is_err(), "other sessions must not observe terminal output");

    let owned = terminals.output(&ee_agent_protocol::TerminalOutputRequest::new("s1", terminal_id));
    assert!(owned.is_ok(), "owner session may observe output");
    terminals.kill_all();
}
