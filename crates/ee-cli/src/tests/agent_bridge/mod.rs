//! Regression tests: ACP `fs/*` and `terminal/*` client
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

const WAIT: Duration = Duration::from_secs(15);

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
        .wait_for("session/set_mode")
        .respond(json!({}))
        .delay(25)
}

const AGENTS_TOML: &str = r#"
root = true

[agents]
enabled = true
default_agent = "fake"

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
        app.agents.permission().is_some(),
        app.agents.elicitation().is_some(),
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

mod read_tests;
mod terminal_tests;
mod write_tests;
