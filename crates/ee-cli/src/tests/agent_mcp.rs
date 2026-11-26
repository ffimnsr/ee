//! MCP config forwarding, health registry,
//! prompt/resource browsing, the ee MCP proxy (same permission broker as
//! direct ACP methods), shutdown orchestration, and approval policy.
//!
//! End-to-end through the real `ee-agent-host` and `ee-mcp` stacks: ACP runs
//! over the in-process fake agent, MCP over in-process fake servers, and the
//! proxy over a real Unix socket (the listener the editor hosts).  No
//! external binaries are spawned.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ee_agent_host::FakeTransportFactory;
use ee_agent_host::fake::{FakeAgent, FakeAgentScript, FakeAgentTransport};
use ee_mcp::fake::{FakeMcpScript, FakeMcpServer, FakeMcpTransportFactory};
use ee_plugin_api::model::{Diagnostic, DiagnosticSeverity, Range, SelectionRange};
use serde_json::{Value, json};

use crate::app::{AgentPaneLayout, App, ThreadUiState};
use crate::tests::helpers::*;

const WAIT: Duration = Duration::from_secs(30);

// ── Fake transport factories ─────────────────────────────────────────────────

/// Builds one fake ACP agent transport per connection (same as agent_pane).
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

/// Builds one fake MCP server transport per connection.
#[derive(Clone)]
struct McpScriptedFake {
    script: FakeMcpScript,
    handle: Arc<Mutex<Option<FakeMcpServer>>>,
}

impl McpScriptedFake {
    fn new(script: FakeMcpScript) -> Self {
        Self { script, handle: Arc::new(Mutex::new(None)) }
    }
}

impl FakeMcpTransportFactory for McpScriptedFake {
    fn build(&self) -> tokio::io::DuplexStream {
        let (server, transport) = FakeMcpServer::spawn(self.script.clone());
        *self.handle.lock().expect("fake handle poisoned") = Some(server);
        transport
    }
}

// ── App builders ─────────────────────────────────────────────────────────────

pub(crate) fn base_agent_script() -> FakeAgentScript {
    FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
        .wait_for("session/set_mode")
        .respond(json!({}))
        .delay(25)
}

/// App with agents enabled, optional `[mcp.servers.tools]`, optional proxy.
pub(crate) fn mcp_app(
    agent_script: FakeAgentScript,
    mcp_servers: bool,
    proxy: bool,
) -> (App, tempfile::TempDir, ScriptedFake) {
    let temp = tempfile::tempdir().unwrap();
    let (app, fake) = mcp_app_in(&temp, agent_script, mcp_servers, proxy);
    (app, temp, fake)
}

pub(crate) fn mcp_app_in(
    temp: &tempfile::TempDir,
    agent_script: FakeAgentScript,
    mcp_servers: bool,
    proxy: bool,
) -> (App, ScriptedFake) {
    // Keep fixture configuration independent from user and system layers. In
    // particular, a developer's `agents.servers.fake.env` secret reference
    // must not make these fake-agent tests depend on a platform keychain.
    let mut toml = String::from(
        "root = true\n\n[agents]\nenabled = true\ndefault_agent = \"fake\"\n\n[agents.servers.fake]\ncommand = \"unused\"\n\n[agents.workspace_memory]\nenabled = false\n",
    );
    if mcp_servers {
        toml.push_str(
            "[mcp.servers.tools]\ntransport = \"stdio\"\ncommand = \"mcp-tools\"\nargs = [\"serve\"]\n",
        );
    }
    if proxy {
        toml.push_str("[mcp.proxy]\nenabled = true\n");
    } else {
        // Proxy defaults on with agents mode; the non-proxy fixtures opt out
        // explicitly to keep their expectations deterministic.
        toml.push_str("[mcp.proxy]\nenabled = false\n");
    }
    fs::write(temp.path().join(".ee.toml"), toml).unwrap();
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let mut app = App::from_path(None).unwrap();
    drop(_cwd_restore);
    drop(_cwd_lock);
    app.agents.test_config_home = Some(temp.path().join(".ee-test-config"));
    pin_workspace_root(&mut app, temp);
    let fake = ScriptedFake::new(agent_script);
    app.agents.test_fake_transports.insert(String::from("fake"), Arc::new(fake.clone()));
    (app, fake)
}

fn install_mcp_fake(app: &mut App, script: FakeMcpScript) -> McpScriptedFake {
    let fake = McpScriptedFake::new(script);
    app.agents.mcp.test_fake_transports.insert(String::from("tools"), Arc::new(fake.clone()));
    fake
}

pub(crate) fn wait_until(app: &mut App, label: &str, mut condition: impl FnMut(&App) -> bool) {
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
        "timed out waiting for {label}; mode={:?} approvals={} permission={} elicitation={} status={:?}",
        app.mode,
        app.agents.approvals.len(),
        app.agents.permission().is_some(),
        app.agents.elicitation().is_some(),
        app.backend.status_message.as_deref()
    );
}

/// Waits until the MCP server `id` reached the Ready state.
fn wait_mcp_ready(app: &mut App, id: &str) {
    wait_until(app, &format!("mcp server {id} ready"), |app| {
        app.agents
            .mcp
            .servers
            .get(id)
            .is_some_and(|server| server.state == ee_mcp::McpServerState::Ready)
    });
}

pub(crate) fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    app.handle_event(Event::Key(KeyEvent::new(code, modifiers)));
}

fn type_text(app: &mut App, text: &str) {
    for ch in text.chars() {
        press(app, KeyCode::Char(ch), KeyModifiers::NONE);
    }
}

pub(crate) fn open_pane_and_wait_ready(app: &mut App) {
    run_ex(app, "agents");
    wait_until(app, "first agent thread ready", |app| {
        app.agents.threads.len() == 1 && app.agents.threads[0].state == ThreadUiState::Ready
    });
}

/// Connects a std Unix socket to the editor's proxy listener (waits for the
/// listener to bind) and performs the token handshake.
pub(crate) fn connect_proxy(app: &App) -> UnixStream {
    let info = app.agents.mcp.proxy.as_ref().expect("proxy info present");
    let deadline = Instant::now() + WAIT;
    let mut stream = loop {
        match UnixStream::connect(&info.socket_path) {
            Ok(stream) => break stream,
            Err(_) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("cannot connect to proxy socket: {error}"),
        }
    };
    stream.write_all(format!("{}\n", info.token).as_bytes()).unwrap();
    stream.flush().unwrap();
    stream
}

/// Sends one proxy call frame (non-blocking; the test pumps the UI between
/// send and recv because approvals are resolved on the UI thread).
pub(crate) fn proxy_send(stream: &mut UnixStream, id: u64, params: Value) {
    let frame = json!({ "id": id, "params": params });
    stream.write_all(frame.to_string().as_bytes()).unwrap();
    stream.write_all(b"\n").unwrap();
    stream.flush().unwrap();
}

/// Pumps the app for a short fixed window (lets async worker replies land
/// when no UI condition is observable).
pub(crate) fn settle(app: &mut App) {
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        app.pump_agents();
        let _ = app.backend.drain_events();
        thread::sleep(Duration::from_millis(10));
    }
}

fn allow_pending_approval_once(app: &mut App) {
    run_ex(app, "agents");
    press(app, KeyCode::Enter, KeyModifiers::NONE);
}

/// Reads one proxy reply line with a bounded wait.
pub(crate) fn proxy_recv(stream: &mut UnixStream) -> Value {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    stream.set_read_timeout(Some(WAIT)).expect("read timeout settable");
    let mut line = String::new();
    reader.read_line(&mut line).expect("proxy reply within timeout");
    serde_json::from_str(line.trim_end()).unwrap()
}

/// Standard MCP script: discover + empty tools list (every real server
/// answers `tools/list`; the pane primes tool metadata on discovery).
fn mcp_standard_script(extra: FakeMcpScript) -> FakeMcpScript {
    extra.discover_2026_07_28(json!({ "tools": {}, "prompts": {}, "resources": {} })).respond(
        "tools/list",
        json!({ "tools": [], "resultType": "complete", "ttlMs": 0, "cacheScope": "private" }),
    )
}

/// Fake agent that advertises `mcp_capabilities.acp` and captures the
/// host-generated `serverId` from `session/new`.
pub(crate) fn acp_agent_script() -> FakeAgentScript {
    FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": { "mcpCapabilities": { "acp": true } }
        }))
        .capture(
            ee_agent_host::fake::CaptureSource::Request { method: "session/new".into() },
            "params.mcpServers[0].serverId",
            "server_id",
        )
        .respond(json!({ "sessionId": "s1" }))
        .wait_for("session/set_mode")
        .respond(json!({}))
        .delay(25)
}

/// Script tail: `mcp/connect` (200), capture the connection id, run MCP
/// `server/discover` (201), then one `tools/call` (202).
pub(crate) fn acp_connect_script(mut tool_call: Value) -> FakeAgentScript {
    let request_meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": { "name": "fake-agent", "version": "0" },
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    tool_call
        .as_object_mut()
        .expect("tool call params object")
        .insert(String::from("_meta"), request_meta.clone());

    acp_agent_script()
        .emit(json!({
            "jsonrpc": "2.0",
            "id": 200,
            "method": "mcp/connect",
            "params": { "serverId": { "$capture": "server_id" } }
        }))
        .capture(
            ee_agent_host::fake::CaptureSource::Response { id: 200 },
            "result.connectionId",
            "conn_id",
        )
        .emit(json!({
            "jsonrpc": "2.0",
            "id": 201,
            "method": "mcp/message",
            "params": {
                "connectionId": { "$capture": "conn_id" },
                "method": "server/discover",
                "params": { "_meta": request_meta }
            }
        }))
        .wait_for_response(201)
        .emit(json!({
            "jsonrpc": "2.0",
            "id": 202,
            "method": "mcp/message",
            "params": {
                "connectionId": { "$capture": "conn_id" },
                "method": "tools/call",
                "params": tool_call
            }
        }))
}

/// Polls the fake agent's log for the host's response to the request with
/// `id` (the ACP response to the fake's `mcp/*` request).
pub(crate) fn fake_response(fake: &FakeAgent, id: i64) -> Value {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(response) = fake.response_with_id(id) {
            return response;
        }
        assert!(Instant::now() < deadline, "timed out waiting for fake response {id}");
        thread::sleep(Duration::from_millis(10));
    }
}

mod acp_and_policy;
mod mcp_basics;
mod proxy_core;
mod proxy_phase23;
