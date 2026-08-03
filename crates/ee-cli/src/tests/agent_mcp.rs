//! Phase 6 + 7 integration tests: MCP config forwarding, health registry,
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
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ee_agent_host::FakeTransportFactory;
use ee_agent_host::fake::{FakeAgent, FakeAgentScript, FakeAgentTransport};
use ee_mcp::fake::{FakeMcpScript, FakeMcpServer, FakeMcpTransportFactory};
use serde_json::{Value, json};

use crate::app::{AgentPaneLayout, App, ThreadUiState};
use crate::tests::helpers::*;

const WAIT: Duration = Duration::from_secs(10);

// ── Fake transport factories ─────────────────────────────────────────────────

/// Builds one fake ACP agent transport per connection (same as agent_pane).
#[derive(Clone)]
struct ScriptedFake {
    script: FakeAgentScript,
    handle: Arc<Mutex<Option<FakeAgent>>>,
}

impl ScriptedFake {
    fn new(script: FakeAgentScript) -> Self {
        Self { script, handle: Arc::new(Mutex::new(None)) }
    }

    fn agent(&self) -> FakeAgent {
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

fn base_agent_script() -> FakeAgentScript {
    FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
}

/// App with agents enabled, optional `[mcp.servers.tools]`, optional proxy.
fn mcp_app(
    agent_script: FakeAgentScript,
    mcp_servers: bool,
    proxy: bool,
) -> (App, tempfile::TempDir, ScriptedFake) {
    let temp = tempfile::tempdir().unwrap();
    let mut toml =
        String::from("[agents]\nenabled = true\n\n[agents.servers.fake]\ncommand = \"unused\"\n");
    if mcp_servers {
        toml.push_str(
            "[mcp.servers.tools]\ntransport = \"stdio\"\ncommand = \"mcp-tools\"\nargs = [\"serve\"]\n",
        );
    }
    if proxy {
        toml.push_str("[mcp.proxy]\nenabled = true\n");
    }
    fs::write(temp.path().join(".ee.toml"), toml).unwrap();
    let _cwd_guard = crate::config::test_cwd_lock().lock().unwrap();
    let original = std::env::current_dir().unwrap();
    std::env::set_current_dir(temp.path()).unwrap();
    let mut app = App::from_path(None).unwrap();
    std::env::set_current_dir(original).unwrap();
    drop(_cwd_guard);
    let fake = ScriptedFake::new(agent_script);
    app.agents.test_fake_transports.insert(String::from("fake"), Arc::new(fake.clone()));
    (app, temp, fake)
}

fn install_mcp_fake(app: &mut App, script: FakeMcpScript) -> McpScriptedFake {
    let fake = McpScriptedFake::new(script);
    app.agents.mcp.test_fake_transports.insert(String::from("tools"), Arc::new(fake.clone()));
    fake
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
    panic!("timed out waiting for {label}; status={:?}", app.backend.status_message.as_deref());
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

fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    app.handle_event(Event::Key(KeyEvent::new(code, modifiers)));
}

fn type_text(app: &mut App, text: &str) {
    for ch in text.chars() {
        press(app, KeyCode::Char(ch), KeyModifiers::NONE);
    }
}

fn open_pane_and_wait_ready(app: &mut App) {
    run_ex(app, "agents");
    wait_until(app, "first agent thread ready", |app| {
        app.agents.threads.len() == 1 && app.agents.threads[0].state == ThreadUiState::Ready
    });
}

/// Connects a std Unix socket to the editor's proxy listener (waits for the
/// listener to bind) and performs the token handshake.
fn connect_proxy(app: &App) -> UnixStream {
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
fn proxy_send(stream: &mut UnixStream, id: u64, params: Value) {
    let frame = json!({ "id": id, "params": params });
    stream.write_all(frame.to_string().as_bytes()).unwrap();
    stream.write_all(b"\n").unwrap();
    stream.flush().unwrap();
}

/// Pumps the app for a short fixed window (lets async worker replies land
/// when no UI condition is observable).
fn settle(app: &mut App) {
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        app.pump_agents();
        let _ = app.backend.drain_events();
        thread::sleep(Duration::from_millis(10));
    }
}

/// Reads one proxy reply line with a bounded wait.
fn proxy_recv(stream: &mut UnixStream) -> Value {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    stream.set_read_timeout(Some(WAIT)).expect("read timeout settable");
    let mut line = String::new();
    reader.read_line(&mut line).expect("proxy reply within timeout");
    serde_json::from_str(line.trim_end()).unwrap()
}

// ── MCP config forwarding ────────────────────────────────────────────────────

#[test]
fn session_new_receives_mcp_config_when_configured() {
    let (mut app, _temp, fake) = mcp_app(base_agent_script(), true, false);
    open_pane_and_wait_ready(&mut app);

    let session_new = fake.agent().requests_by_method("session/new");
    let params = session_new[0].get("params").expect("params");
    let servers = params.get("mcpServers").expect("mcpServers present");
    let server = servers
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some("tools"));
    let server = server.expect("tools server forwarded");
    assert_eq!(server.get("command").and_then(Value::as_str), Some("mcp-tools"));
    assert_eq!(server.get("args").and_then(Value::as_array).unwrap(), &vec![json!("serve")]);
}

#[test]
fn session_new_omits_mcp_config_when_none_configured() {
    let (mut app, _temp, fake) = mcp_app(base_agent_script(), false, false);
    open_pane_and_wait_ready(&mut app);

    let session_new = fake.agent().requests_by_method("session/new");
    let params = session_new[0].get("params").expect("params");
    let servers = params.get("mcpServers").expect("mcpServers field present");
    assert!(servers.as_array().unwrap().is_empty(), "no servers must be forwarded");
}

#[test]
fn session_new_forwards_proxy_stdio_entry_when_proxy_enabled() {
    let (mut app, _temp, fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    let session_new = fake.agent().requests_by_method("session/new");
    let servers = session_new[0]
        .get("params")
        .and_then(|params| params.get("mcpServers"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let proxy = servers
        .iter()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some("ee"))
        .expect("ee proxy entry forwarded");
    let args = proxy.get("args").and_then(Value::as_array).unwrap();
    assert!(args.contains(&json!("--mcp-proxy")));
    let env = proxy.get("env").and_then(Value::as_array).unwrap();
    assert!(env.iter().any(|variable| {
        variable.get("name").and_then(Value::as_str) == Some("EE_MCP_PROXY_SOCKET")
    }));
}

// ── MCP health registry ──────────────────────────────────────────────────────

#[test]
fn mcp_health_shows_ready_identity_and_capabilities() {
    let script = FakeMcpScript::new().respond(
        "server/discover",
        json!({
            "resultType": "complete",
            "supportedVersions": ["2026-07-28"],
            "capabilities": { "tools": {}, "prompts": {} },
            "ttlMs": 0,
            "cacheScope": "private",
            "_meta": {
                "io.modelcontextprotocol/serverInfo": {
                    "name": "fake-mcp", "version": "9.9", "title": "Fake Tools"
                }
            },
        }),
    );
    let (mut app, _temp, _fake) = mcp_app(base_agent_script(), true, false);
    install_mcp_fake(&mut app, script);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "mcp server ready", |app| {
        app.agents
            .mcp
            .servers
            .get("tools")
            .is_some_and(|server| server.state == ee_mcp::McpServerState::Ready)
    });
    let server = &app.agents.mcp.servers["tools"];
    let identity = server.identity.as_deref().unwrap_or_default();
    assert!(identity.contains("Fake Tools"), "identity: {identity}");
    assert!(server.capabilities.contains("tools"), "capabilities: {}", server.capabilities);
    assert!(server.capabilities.contains("prompts"));

    // Health lines are deterministic and non-fatal for the ACP chat.
    let lines = app.mcp_health_lines();
    assert!(lines.iter().any(|line| line.contains("tools") && line.contains("ready")));
    assert_eq!(app.agents.threads[0].state, ThreadUiState::Ready);
}

#[test]
fn mcp_health_failure_is_non_fatal_and_shows_failed_state() {
    let script = FakeMcpScript::new().respond(
        "server/discover",
        json!({
            "resultType": "complete",
            "supportedVersions": ["2025-11-25"],
            "capabilities": {},
            "ttlMs": 0,
            "cacheScope": "private",
        }),
    );
    let (mut app, _temp, _fake) = mcp_app(base_agent_script(), true, false);
    install_mcp_fake(&mut app, script);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "mcp server failed", |app| {
        app.agents
            .mcp
            .servers
            .get("tools")
            .is_some_and(|server| server.state == ee_mcp::McpServerState::Failed)
    });
    // The ACP chat still works despite the failed MCP server.
    assert_eq!(app.agents.threads[0].state, ThreadUiState::Ready);
}

#[test]
fn mcp_servers_start_only_when_pane_opens() {
    let script = FakeMcpScript::new().discover_2026_07_28(json!({ "tools": {} }));
    let (mut app, _temp, fake) = mcp_app(base_agent_script(), true, false);
    install_mcp_fake(&mut app, script);

    // Pane closed: no MCP host, no fake server spawned.
    assert!(app.agents.mcp.host.is_none());
    assert!(app.agents.mcp.servers.is_empty());

    open_pane_and_wait_ready(&mut app);
    wait_until(&mut app, "mcp server ready", |app| {
        app.agents
            .mcp
            .servers
            .get("tools")
            .is_some_and(|server| server.state == ee_mcp::McpServerState::Ready)
    });
    let _ = fake.agent();
}

// ── Prompt / resource browsing ───────────────────────────────────────────────

/// Standard MCP script: discover + empty tools list (every real server
/// answers `tools/list`; the pane primes tool metadata on discovery).
fn mcp_standard_script(extra: FakeMcpScript) -> FakeMcpScript {
    extra.discover_2026_07_28(json!({ "tools": {}, "prompts": {}, "resources": {} })).respond(
        "tools/list",
        json!({ "tools": [], "resultType": "complete", "ttlMs": 0, "cacheScope": "private" }),
    )
}

#[test]
fn prompt_browse_inserts_prompt_content_into_draft() {
    let script = mcp_standard_script(FakeMcpScript::new())
        .respond(
            "prompts/list",
            json!({
                "prompts": [
                    { "name": "summarize", "description": "Summarize the file",
                      "arguments": [] }
                ],
                "resultType": "complete", "ttlMs": 0, "cacheScope": "private",
            }),
        )
        .respond(
            "prompts/get",
            json!({
                "messages": [
                    { "role": "user",
                      "content": { "type": "text", "text": "Please summarize the file" } }
                ],
                "resultType": "complete",
            }),
        );
    let (mut app, _temp, _fake) = mcp_app(base_agent_script(), true, false);
    install_mcp_fake(&mut app, script);
    open_pane_and_wait_ready(&mut app);
    wait_mcp_ready(&mut app, "tools");

    run_ex(&mut app, "agents_mcp prompts");
    wait_until(&mut app, "prompt list loaded", |app| {
        app.agents
            .mcp
            .browse
            .as_ref()
            .is_some_and(|browse| !browse.loading && !browse.items.is_empty())
    });
    let browse = app.agents.mcp.browse.as_ref().unwrap();
    assert_eq!(browse.items[0].label, "summarize");

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "prompt content inserted", |app| {
        app.agents.threads[0].draft.contains("Please summarize the file")
    });
    assert!(app.agents.mcp.browse.is_none(), "browse closes after insertion");
}

#[test]
fn resource_browse_inserts_uri_mention() {
    let script = mcp_standard_script(FakeMcpScript::new()).respond(
        "resources/list",
        json!({
            "resources": [
                { "uri": "file:///tmp/notes.txt", "name": "notes" }
            ],
            "resultType": "complete", "ttlMs": 0, "cacheScope": "private",
        }),
    );
    let (mut app, _temp, _fake) = mcp_app(base_agent_script(), true, false);
    install_mcp_fake(&mut app, script);
    open_pane_and_wait_ready(&mut app);
    wait_mcp_ready(&mut app, "tools");

    run_ex(&mut app, "agents_mcp resources");
    wait_until(&mut app, "resource list loaded", |app| {
        app.agents
            .mcp
            .browse
            .as_ref()
            .is_some_and(|browse| !browse.loading && !browse.items.is_empty())
    });
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "uri inserted", |app| {
        app.agents.threads[0].draft.contains("file:///tmp/notes.txt")
    });
}

#[test]
fn tool_list_changed_refreshes_pane_tool_metadata() {
    let script = FakeMcpScript::new()
        .discover_2026_07_28(json!({ "tools": {} }))
        .respond_once(
            "tools/list",
            json!({
                "tools": [{ "name": "toolA", "description": "a", "inputSchema": {} }],
                "resultType": "complete", "ttlMs": 0, "cacheScope": "private",
            }),
        )
        .emit(json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }))
        .respond_once(
            "tools/list",
            json!({
                "tools": [{ "name": "toolB", "description": "b", "inputSchema": {} }],
                "resultType": "complete", "ttlMs": 0, "cacheScope": "private",
            }),
        );
    let (mut app, _temp, _fake) = mcp_app(base_agent_script(), true, false);
    install_mcp_fake(&mut app, script);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "tool metadata refreshed", |app| {
        app.agents
            .mcp
            .servers
            .get("tools")
            .is_some_and(|server| server.tools.contains(&String::from("tools/toolB")))
    });
    let server = &app.agents.mcp.servers["tools"];
    assert!(server.tools.contains(&String::from("tools/toolB")));
}

// ── ee MCP proxy: same permission broker as direct ACP methods ───────────────

#[test]
fn proxy_write_denial_leaves_buffer_unchanged() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    let target = temp.path().join("proxy.txt");
    fs::write(&target, "original").unwrap();
    let mut stream = connect_proxy(&app);

    proxy_send(
        &mut stream,
        1,
        json!({
            "method": "write_text_file",
            "path": target.display().to_string(),
            "content": "agent-wrote-this",
        }),
    );
    // The approval prompt is queued in the pane (same path as ACP writes).
    wait_until(&mut app, "approval queued", |app| !app.agents.approvals.is_empty());
    let prompt = app.agents.approvals.front().unwrap();
    assert!(prompt.title.contains("fs/write_text_file"));
    assert!(prompt.detail.contains("proxy.txt"));

    // Deny through the same approval UI used for direct ACP methods.
    run_ex(&mut app, "agents");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    wait_until(&mut app, "approval resolved", |app| app.agents.approvals.is_empty());
    let reply = proxy_recv(&mut stream);

    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        "original",
        "denied write must not touch disk"
    );
    let reply = reply.get("result").unwrap();
    assert!(reply.get("error").is_some(), "proxy must report the denial: {reply}");
}

#[test]
fn proxy_write_allow_writes_through_buffer() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    let target = temp.path().join("proxy-allow.txt");
    fs::write(&target, "original").unwrap();
    let mut stream = connect_proxy(&app);

    proxy_send(
        &mut stream,
        1,
        json!({
            "method": "write_text_file",
            "path": target.display().to_string(),
            "content": "agent-wrote-this",
        }),
    );
    wait_until(&mut app, "approval queued", |app| !app.agents.approvals.is_empty());
    run_ex(&mut app, "agents");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // Allow once

    wait_until(&mut app, "file written", |_app| {
        fs::read_to_string(&target).map(|text| text == "agent-wrote-this").unwrap_or(false)
    });
    let reply = proxy_recv(&mut stream);
    assert!(
        reply.get("result").and_then(|r| r.get("text")).is_some(),
        "allowed write returns ok: {reply}"
    );
}

#[test]
fn proxy_terminal_denial_does_not_spawn_terminal() {
    let (mut app, _temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    let mut stream = connect_proxy(&app);
    proxy_send(
        &mut stream,
        1,
        json!({
            "method": "terminal_create",
            "command": "sleep",
            "args": ["30"],
        }),
    );
    wait_until(&mut app, "approval queued", |app| !app.agents.approvals.is_empty());
    let prompt = app.agents.approvals.front().unwrap();
    assert!(prompt.title.contains("terminal/create"));
    assert!(prompt.detail.contains("sleep 30"));

    run_ex(&mut app, "agents");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    wait_until(&mut app, "approval resolved", |app| app.agents.approvals.is_empty());
    let reply = proxy_recv(&mut stream);

    assert_eq!(app.agents.terminals.tracked_count(), 0, "denied terminal must not spawn");
    let reply = reply.get("result").unwrap();
    assert!(reply.get("error").is_some(), "proxy must report the denial: {reply}");
}

// ── Approval policy (Phase 7) ────────────────────────────────────────────────

#[test]
fn approval_policy_allow_session_auto_allows_identical_writes() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);
    let target = temp.path().join("policy.txt");
    fs::write(&target, "v0").unwrap();

    let mut stream = connect_proxy(&app);
    for round in 1..=3 {
        proxy_send(
            &mut stream,
            round,
            json!({
                "method": "write_text_file",
                "path": target.display().to_string(),
                "content": format!("agent-v{round}"),
            }),
        );
        wait_until(&mut app, "round approval or auto-resolve", |app| {
            !app.agents.approvals.is_empty()
                || fs::read_to_string(&target).map(|t| t.contains("agent-v")).unwrap_or(false)
        });
        if !app.agents.approvals.is_empty() {
            run_ex(&mut app, "agents");
            press(&mut app, KeyCode::Right, KeyModifiers::NONE); // Allow session
            press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        }
        wait_until(&mut app, "round applied", |_app| {
            fs::read_to_string(&target)
                .map(|t| t.contains(&format!("agent-v{round}")))
                .unwrap_or(false)
        });
        let _ = proxy_recv(&mut stream);
    }
    // Rounds 2 and 3 were auto-allowed by the session policy (no prompts).
    assert_eq!(app.agents.approvals.len(), 0);
    assert!(fs::read_to_string(&target).unwrap().contains("agent-v3"));
}

#[test]
fn approval_policy_deny_session_auto_denies_identical_writes() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);
    let target = temp.path().join("policy-deny.txt");
    fs::write(&target, "v0").unwrap();

    let mut stream = connect_proxy(&app);
    for round in 1..=2 {
        proxy_send(
            &mut stream,
            round,
            json!({
                "method": "write_text_file",
                "path": target.display().to_string(),
                "content": format!("agent-v{round}"),
            }),
        );
        if round == 1 {
            // First round prompts; the user records a session-scoped denial.
            wait_until(&mut app, "approval queued", |app| !app.agents.approvals.is_empty());
            run_ex(&mut app, "agents");
            press(&mut app, KeyCode::Right, KeyModifiers::NONE);
            press(&mut app, KeyCode::Right, KeyModifiers::NONE);
            press(&mut app, KeyCode::Right, KeyModifiers::NONE); // Deny session
            press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
            wait_until(&mut app, "round resolved", |app| app.agents.approvals.is_empty());
        } else {
            // Round 2 is auto-denied by the session policy: no approval is
            // queued; the reply lands on its own.
            settle(&mut app);
        }
        let reply = proxy_recv(&mut stream);
        assert!(
            reply.get("result").and_then(|r| r.get("error")).is_some(),
            "round {round} must be denied: {reply}"
        );
    }
    assert_eq!(fs::read_to_string(&target).unwrap(), "v0", "disk unchanged");
}

// ── Shutdown orchestration (Phase 7) ─────────────────────────────────────────

#[test]
fn shutdown_cancels_hung_turn_kills_terminals_and_stops_hosts() {
    let script = base_agent_script().wait_for("session/prompt"); // never answers
    let (mut app, _temp, _fake) = mcp_app(script, false, false);
    open_pane_and_wait_ready(&mut app);

    // Start a turn that can never complete.
    type_text(&mut app, "hello");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "turn running", |app| {
        app.agents.threads[0].state == ThreadUiState::Running
    });

    // Spawn a long-running agent terminal through the approval path.
    let mut request = ee_agent_protocol::CreateTerminalRequest::new(
        ee_agent_protocol::SessionId::new("s1"),
        "sleep",
    );
    request.args = vec![String::from("30")];
    let spawned = app.agents.terminals.spawn(&request).expect("terminal spawns");
    assert_eq!(app.agents.terminals.tracked_count(), 1);

    // Shutdown must complete within a bounded window despite the hung agent.
    let start = Instant::now();
    app.shutdown_agents();
    assert!(start.elapsed() < Duration::from_secs(5), "shutdown must not hang");

    assert!(app.agents.host.is_none(), "agent host must stop");
    assert!(app.agents.mcp.host.is_none());
    assert_eq!(app.agents.terminals.tracked_count(), 0, "terminals must be killed");
    assert!(app.agents.threads.is_empty());
    assert!(app.agents.approvals.is_empty());
    let _ = spawned;
}

// ── Disabled-mode regression (Phase 7) ───────────────────────────────────────

#[test]
fn runtime_disabled_agents_do_not_start_mcp_or_agent_hosts() {
    // agents.enabled is false but MCP servers exist: `:agents` must not
    // start either host.
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join(".ee.toml"),
        "[agents.servers.fake]\ncommand = \"unused\"\n\n[mcp.servers.tools]\ntransport = \"stdio\"\ncommand = \"mcp-tools\"\n",
    )
    .unwrap();
    let _cwd_guard = crate::config::test_cwd_lock().lock().unwrap();
    let original = std::env::current_dir().unwrap();
    std::env::set_current_dir(temp.path()).unwrap();
    let mut app = App::from_path(None).unwrap();
    std::env::set_current_dir(original).unwrap();
    drop(_cwd_guard);

    run_ex(&mut app, "agents");
    assert!(app.agents.host.is_none(), "agent host must not start");
    assert!(app.agents.mcp.host.is_none(), "mcp host must not start");
    assert_eq!(app.agents.layout, AgentPaneLayout::Closed);
}

#[test]
fn agents_mcp_command_disabled_path_reports_message() {
    let (mut app, _temp, _fake) = mcp_app(base_agent_script(), true, false);
    app.config.agents.enabled = false;
    run_ex(&mut app, "agents_mcp prompts");
    let status = app.backend.status_message.clone().unwrap_or_default();
    assert!(status.contains("agents mode disabled"), "status: {status}");
    assert!(app.agents.mcp.host.is_none());
}

// ── Secret redaction wiring (Phase 7) ────────────────────────────────────────

#[test]
fn configured_secret_values_are_collected_for_redaction() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join(".ee.toml"),
        "[agents]\nenabled = true\n\n[agents.servers.fake]\ncommand = \"unused\"\nenv = { API_TOKEN = \"super-secret-value\" }\n",
    )
    .unwrap();
    let _cwd_guard = crate::config::test_cwd_lock().lock().unwrap();
    let original = std::env::current_dir().unwrap();
    std::env::set_current_dir(temp.path()).unwrap();
    let app = App::from_path(None).unwrap();
    std::env::set_current_dir(original).unwrap();
    drop(_cwd_guard);

    let secrets = app.agents_secret_values();
    assert!(secrets.contains(&String::from("super-secret-value")));

    let text = "trace: using super-secret-value now";
    let redacted = ee_agent_host::redact::redact_secret_values(text, &secrets);
    assert_eq!(redacted, "trace: using *** now");
    assert!(!redacted.contains("super-secret-value"));
}
