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
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ee_agent_host::FakeTransportFactory;
use ee_agent_host::fake::{FakeAgent, FakeAgentScript, FakeAgentTransport};
use ee_mcp::fake::{FakeMcpScript, FakeMcpServer, FakeMcpTransportFactory};
use serde_json::{Value, json};
use xi_core_lib::plugin_rpc::{Diagnostic, DiagnosticSeverity, Range, SelectionRange};

use crate::app::{AgentPaneLayout, App, ThreadUiState};
use crate::tests::helpers::*;

const WAIT: Duration = Duration::from_secs(5);

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
        "root = true\n\n[agents]\nenabled = true\ndefault_agent = \"fake\"\n\n[agents.servers.fake]\ncommand = \"unused\"\n",
    );
    if mcp_servers {
        toml.push_str(
            "[mcp.servers.tools]\ntransport = \"stdio\"\ncommand = \"mcp-tools\"\nargs = [\"serve\"]\n",
        );
    }
    if proxy {
        toml.push_str("[mcp.proxy]\nenabled = true\n");
    }
    fs::write(temp.path().join(".ee.toml"), toml).unwrap();
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let mut app = App::from_path(None).unwrap();
    drop(_cwd_restore);
    drop(_cwd_lock);
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
        app.agents.permission.is_some(),
        app.agents.elicitation.is_some(),
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

#[test]
fn stdio_proxy_mcp_frames_cover_read_write_and_execute_classes() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);
    let info = app.agents.mcp.proxy.clone().expect("proxy listener available");
    drop(connect_proxy(&app)); // Wait for listener bind before subprocess-side socket connect.
    let target = temp.path().join("stdio-proxy-write.txt");
    let target_text = target.display().to_string();

    let (client_stream, server_stream) = tokio::io::duplex(128 * 1024);
    let server = thread::spawn(move || {
        crate::app::agents_mcp::run_proxy_stdio_with_duplex(
            info.socket_path,
            info.token,
            server_stream,
        )
        .expect("stdio proxy exits cleanly after client EOF");
    });
    let (result_tx, result_rx) = mpsc::channel();
    let client = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let result = runtime.block_on(async move {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
            let (reader, mut writer) = tokio::io::split(client_stream);
            let mut reader = tokio::io::BufReader::new(reader).lines();
            async fn request(
                writer: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
                reader: &mut tokio::io::Lines<tokio::io::BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
                id: u64,
                method: &str,
                params: Value,
            ) -> Value {
                let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
                writer.write_all(request.to_string().as_bytes()).await.unwrap();
                writer.write_all(b"\n").await.unwrap();
                writer.flush().await.unwrap();
                let line = reader.next_line().await.unwrap().expect("stdio response");
                serde_json::from_str(&line).unwrap()
            }

            let initialized = request(
                &mut writer,
                &mut reader,
                1,
                "initialize",
                json!({
                    "protocolVersion": "2026-07-28",
                    "capabilities": {},
                    "clientInfo": { "name": "stdio-test", "version": "1" }
                }),
            )
            .await;
            let list = request(&mut writer, &mut reader, 2, "tools/list", json!({})).await;
            let manifest = request(
                &mut writer,
                &mut reader,
                3,
                "tools/call",
                json!({ "name": "ee_tools_manifest", "arguments": {} }),
            )
            .await;
            let read = request(
                &mut writer,
                &mut reader,
                4,
                "tools/call",
                json!({ "name": "ee_workspace_roots", "arguments": {} }),
            )
            .await;
            let write = request(
                &mut writer,
                &mut reader,
                5,
                "tools/call",
                json!({ "name": "ee_write_text_file", "arguments": { "path": target_text, "content": "stdio" } }),
            )
            .await;
            let execute = request(
                &mut writer,
                &mut reader,
                6,
                "tools/call",
                json!({ "name": "ee_terminal_create", "arguments": { "command": "true" } }),
            )
            .await;
            (initialized, list, manifest, read, write, execute)
        });
        result_tx.send(result).unwrap();
    });

    let deadline = Instant::now() + WAIT;
    let responses = loop {
        if let Ok(responses) = result_rx.try_recv() {
            break responses;
        }
        app.pump_agents();
        let _ = app.backend.drain_events();
        if !app.agents.approvals.is_empty() {
            allow_pending_approval_once(&mut app); // Allow once for write and `true` terminal.
        }
        assert!(Instant::now() < deadline, "stdio MCP proxy did not finish");
        thread::sleep(Duration::from_millis(10));
    };
    client.join().expect("stdio client thread");
    server.join().expect("stdio server thread");

    let (initialized, list, manifest, read, write, execute) = responses;
    assert_eq!(initialized["result"]["protocolVersion"], json!("2026-07-28"));
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .expect("tool list")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(names.contains(&"ee_workspace_roots"));
    assert!(names.contains(&"ee_write_text_file"));
    assert!(names.contains(&"ee_terminal_create"));
    assert!(!names.contains(&"ee_terminal_output"), "ACP-only tool hidden on stdio");
    assert_eq!(manifest["result"]["isError"], json!(false));
    assert_eq!(read["result"]["isError"], json!(false));
    assert_eq!(write["result"]["isError"], json!(false));
    assert_eq!(execute["result"]["isError"], json!(false));
    assert_eq!(fs::read_to_string(target).unwrap().trim_end(), "stdio");
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
        reply.get("result").and_then(|r| r.get("value")).is_some(),
        "allowed write returns ok: {reply}"
    );
}

#[test]
fn proxy_workspace_roots_and_search_text_return_structured_results() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    let target = temp.path().join("search.txt");
    fs::write(&target, "alpha\nneedle beta\n").unwrap();
    let mut stream = connect_proxy(&app);

    proxy_send(&mut stream, 1, json!({ "method": "workspace_roots" }));
    settle(&mut app);
    let roots_reply = proxy_recv(&mut stream);
    assert_eq!(
        roots_reply["result"]["value"]["roots"][0],
        json!(temp.path().display().to_string())
    );

    proxy_send(&mut stream, 2, json!({ "method": "search_text", "query": "needle" }));
    settle(&mut app);
    let search_reply = proxy_recv(&mut stream);
    let matches = search_reply["result"]["value"]["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1, "search reply: {search_reply}");
    assert_eq!(matches[0]["path"], json!(target.display().to_string()));
    assert_eq!(matches[0]["line"], json!(2));
    assert_eq!(matches[0]["context"], json!("needle beta"));
}

#[test]
fn proxy_phase1_extras_return_structured_results() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    let visible = temp.path().join("visible.rs");
    let hidden = temp.path().join(".hidden.rs");
    let ignored = temp.path().join("ignored.log");
    fs::write(temp.path().join(".ignore"), "ignored.log\n").unwrap();
    fs::write(&visible, "alpha\nregex-hit\nneedle\n").unwrap();
    fs::write(&hidden, "regex-hit hidden\n").unwrap();
    fs::write(&ignored, "needle ignored\n").unwrap();
    let mut stream = connect_proxy(&app);

    proxy_send(&mut stream, 1, json!({ "method": "list_directory_all", "path": temp.path() }));
    settle(&mut app);
    let list_reply = proxy_recv(&mut stream);
    let entries = list_reply["result"]["value"]["entries"].as_array().unwrap();
    assert!(entries.iter().any(|entry| entry["path"] == json!(hidden.display().to_string())
        && entry["hidden"] == json!(true)));
    assert!(entries.iter().any(|entry| entry["path"] == json!(ignored.display().to_string())
        && entry["ignored"] == json!(true)));

    proxy_send(&mut stream, 2, json!({ "method": "search_files_all", "pattern": "*.rs" }));
    settle(&mut app);
    let files_reply = proxy_recv(&mut stream);
    let file_matches = files_reply["result"]["value"]["matches"].as_array().unwrap();
    assert!(file_matches.iter().any(|entry| entry["path"] == json!(hidden.display().to_string())
        && entry["hidden"] == json!(true)));

    proxy_send(&mut stream, 3, json!({ "method": "search_text_regex", "pattern": "regex-hit" }));
    settle(&mut app);
    let regex_reply = proxy_recv(&mut stream);
    let regex_matches = regex_reply["result"]["value"]["matches"].as_array().unwrap();
    assert!(
        regex_matches.iter().any(|entry| entry["path"] == json!(visible.display().to_string()))
    );

    proxy_send(
        &mut stream,
        4,
        json!({ "method": "search_text_in_files", "query": "needle", "file_glob": "*.rs" }),
    );
    settle(&mut app);
    let scoped_reply = proxy_recv(&mut stream);
    let scoped_matches = scoped_reply["result"]["value"]["matches"].as_array().unwrap();
    assert_eq!(scoped_matches.len(), 1, "scoped reply: {scoped_reply}");
    assert_eq!(scoped_matches[0]["path"], json!(visible.display().to_string()));
}

#[test]
fn proxy_phase1_path_tools_reject_outside_workspace() {
    let (mut app, _temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);
    let outside = tempfile::tempdir().unwrap();
    let mut stream = connect_proxy(&app);

    proxy_send(
        &mut stream,
        1,
        json!({ "method": "list_directory", "path": outside.path().display().to_string() }),
    );
    settle(&mut app);
    let reply = proxy_recv(&mut stream);
    let error = &reply["result"]["error"]["message"];
    assert!(
        error.as_str().unwrap_or_default().contains("outside allowed workspace"),
        "reply: {reply}"
    );

    proxy_send(
        &mut stream,
        2,
        json!({ "method": "list_directory_all", "path": outside.path().display().to_string() }),
    );
    settle(&mut app);
    let reply = proxy_recv(&mut stream);
    let error = &reply["result"]["error"]["message"];
    assert!(
        error.as_str().unwrap_or_default().contains("outside allowed workspace"),
        "reply: {reply}"
    );
}

#[test]
fn proxy_phase1_caps_truncate_results() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    for index in 0..560 {
        fs::write(temp.path().join(format!("cap-{index:03}.rs")), "cap-hit\n").unwrap();
    }
    let mut stream = connect_proxy(&app);

    proxy_send(&mut stream, 1, json!({ "method": "list_directory_all", "path": temp.path() }));
    settle(&mut app);
    let list_reply = proxy_recv(&mut stream);
    let list_value = &list_reply["result"]["value"];
    assert_eq!(list_value["truncated"], json!(true), "reply: {list_reply}");
    assert!(list_value["entries"].as_array().unwrap().len() < 560, "reply: {list_reply}");

    proxy_send(&mut stream, 2, json!({ "method": "search_files_all", "pattern": "*.rs" }));
    settle(&mut app);
    let files_reply = proxy_recv(&mut stream);
    let files_value = &files_reply["result"]["value"];
    assert_eq!(files_value["truncated"], json!(true), "reply: {files_reply}");
    assert!(files_value["matches"].as_array().unwrap().len() < 560, "reply: {files_reply}");

    proxy_send(&mut stream, 3, json!({ "method": "search_text", "query": "cap-hit" }));
    settle(&mut app);
    let text_reply = proxy_recv(&mut stream);
    let text_value = &text_reply["result"]["value"];
    assert_eq!(text_value["truncated"], json!(true), "reply: {text_reply}");
    assert!(text_value["matches"].as_array().unwrap().len() < 560, "reply: {text_reply}");
}

#[test]
fn proxy_phase2_open_buffers_and_read_buffer_reflect_unsaved_state() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    let target = temp.path().join("dirty.rs");
    fs::write(&target, "alpha\nbeta\n").unwrap();
    let id = app.backend.open_buffer(Some(target.clone())).unwrap();
    app.backend.switch_to_id(id).unwrap();
    wait_until(&mut app, "buffer active", |app| {
        app.backend.active().path.as_deref() == Some(target.as_path())
    });
    app.backend.replace_line_range(0, 0, &[String::from("dirty-alpha")]).unwrap();
    app.backend.flush_all_pending_edits().unwrap();
    app.backend.set_selections(&[SelectionRange { start: 0, end: 5 }]).unwrap();
    wait_until(&mut app, "buffer dirty", |app| !app.backend.active().pristine);

    let mut stream = connect_proxy(&app);
    proxy_send(&mut stream, 1, json!({ "method": "open_buffers" }));
    settle(&mut app);
    let buffers_reply = proxy_recv(&mut stream);
    let buffers = buffers_reply["result"]["value"]["buffers"].as_array().unwrap();
    let entry =
        buffers.iter().find(|entry| entry["path"] == json!(target.display().to_string())).unwrap();
    assert_eq!(entry["dirty"], json!(true), "reply: {buffers_reply}");
    assert_eq!(entry["languageId"], json!("rust"), "reply: {buffers_reply}");

    proxy_send(
        &mut stream,
        2,
        json!({ "method": "read_buffer", "path": target.display().to_string() }),
    );
    settle(&mut app);
    let read_reply = proxy_recv(&mut stream);
    assert_eq!(read_reply["result"]["value"], json!("dirty-alpha\nbeta"));

    proxy_send(
        &mut stream,
        3,
        json!({ "method": "read_buffer_lines", "path": target.display().to_string(), "line": 1, "limit": 1 }),
    );
    settle(&mut app);
    let lines_reply = proxy_recv(&mut stream);
    assert_eq!(lines_reply["result"]["value"], json!("dirty-alpha"));
}

#[test]
fn proxy_phase2_replace_text_and_create_overwrite_file_work() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    let target = temp.path().join("replace.txt");
    fs::write(&target, "alpha\nbeta\n").unwrap();
    let mut stream = connect_proxy(&app);

    proxy_send(
        &mut stream,
        1,
        json!({
            "method": "replace_text",
            "path": target.display().to_string(),
            "old_text": "alpha",
            "new_text": "omega"
        }),
    );
    wait_until(&mut app, "replace approval queued", |app| !app.agents.approvals.is_empty());
    run_ex(&mut app, "agents");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "replace approval resolved", |app| app.agents.approvals.is_empty());
    let replace_reply = proxy_recv(&mut stream);
    assert_eq!(replace_reply["result"]["value"]["editCount"], json!(1));
    assert_eq!(replace_reply["result"]["value"]["saved"], json!(true));
    assert_eq!(fs::read_to_string(&target).unwrap(), "omega\nbeta\n");

    let created = temp.path().join("created.txt");
    proxy_send(
        &mut stream,
        2,
        json!({
            "method": "create_text_file",
            "path": created.display().to_string(),
            "content": "fresh\n"
        }),
    );
    wait_until(&mut app, "create approval queued", |app| !app.agents.approvals.is_empty());
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "create approval resolved", |app| app.agents.approvals.is_empty());
    let create_reply = proxy_recv(&mut stream);
    assert_eq!(
        create_reply["result"]["value"]["changedFile"],
        json!(created.display().to_string())
    );
    assert_eq!(fs::read_to_string(&created).unwrap(), "fresh\n");

    proxy_send(
        &mut stream,
        3,
        json!({
            "method": "overwrite_text_file",
            "path": created.display().to_string(),
            "content": "overwritten\n"
        }),
    );
    wait_until(&mut app, "overwrite approval queued", |app| !app.agents.approvals.is_empty());
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "overwrite approval resolved", |app| app.agents.approvals.is_empty());
    let overwrite_reply = proxy_recv(&mut stream);
    assert_eq!(overwrite_reply["result"]["value"]["saved"], json!(true));
    assert_eq!(fs::read_to_string(&created).unwrap(), "overwritten\n");
}

#[test]
fn proxy_phase2_ambiguous_and_stale_edits_fail_closed() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    let ambiguous = temp.path().join("ambiguous.txt");
    fs::write(&ambiguous, "dup\ndup\n").unwrap();
    let mut stream = connect_proxy(&app);
    proxy_send(
        &mut stream,
        1,
        json!({
            "method": "replace_text",
            "path": ambiguous.display().to_string(),
            "old_text": "dup",
            "new_text": "once"
        }),
    );
    settle(&mut app);
    let ambiguous_reply = proxy_recv(&mut stream);
    assert!(
        ambiguous_reply["result"]["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("expected exactly one match"),
        "reply: {ambiguous_reply}"
    );
    assert!(app.agents.approvals.is_empty(), "ambiguous edit must fail before approval");

    let stale = temp.path().join("stale.txt");
    fs::write(&stale, "alpha\nbeta\n").unwrap();
    let id = app.backend.open_buffer(Some(stale.clone())).unwrap();
    app.backend.switch_to_id(id).unwrap();
    wait_until(&mut app, "stale buffer active", |app| {
        app.backend.active().path.as_deref() == Some(stale.as_path())
    });
    wait_until(&mut app, "stale buffer text loaded", |app| {
        app.backend.active().whole_text().as_deref() == Some("alpha\nbeta\n")
    });

    proxy_send(
        &mut stream,
        2,
        json!({
            "method": "replace_text",
            "path": stale.display().to_string(),
            "old_text": "alpha",
            "new_text": "omega"
        }),
    );
    wait_until(&mut app, "stale approval queued", |app| !app.agents.approvals.is_empty());
    app.backend.replace_line_range(1, 1, &[String::from("gamma")]).unwrap();
    app.backend.flush_all_pending_edits().unwrap();
    wait_until(&mut app, "user edit lands", |app| {
        app.backend.active().whole_text().as_deref() == Some("alpha\ngamma\n")
    });
    run_ex(&mut app, "agents");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let stale_reply = proxy_recv(&mut stream);
    assert!(
        stale_reply["result"]["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("re-read and retry"),
        "reply: {stale_reply}"
    );
    assert_eq!(app.backend.active().whole_text().as_deref(), Some("alpha\ngamma\n"));
}

#[test]
fn proxy_phase3_diagnostics_return_bounded_editor_state() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    let target = temp.path().join("diag.rs");
    fs::write(&target, "alpha\nbeta\n").unwrap();
    let id = app.backend.open_buffer(Some(target.clone())).unwrap();
    app.backend.switch_to_id(id).unwrap();
    wait_until(&mut app, "diagnostic buffer active", |app| {
        app.backend.active().path.as_deref() == Some(target.as_path())
    });
    app.backend.diagnostics = vec![Diagnostic {
        range: Range { start: 0, end: 5 },
        severity: DiagnosticSeverity::Error,
        message: String::from("broken alpha"),
        source: Some(String::from("fake-lsp")),
        code: Some(String::from("E-DEMO")),
    }];

    let mut stream = connect_proxy(&app);
    proxy_send(&mut stream, 1, json!({ "method": "get_diagnostics" }));
    settle(&mut app);
    let workspace_reply = proxy_recv(&mut stream);
    let diagnostics = workspace_reply["result"]["value"]["diagnostics"].as_array().unwrap();
    assert_eq!(diagnostics.len(), 1, "reply: {workspace_reply}");
    assert_eq!(diagnostics[0]["path"], json!(target.display().to_string()));
    assert_eq!(diagnostics[0]["severity"], json!("error"));
    assert_eq!(diagnostics[0]["message"], json!("broken alpha"));
    assert_eq!(diagnostics[0]["source"], json!("fake-lsp"));
    assert_eq!(diagnostics[0]["code"], json!("E-DEMO"));
    assert_eq!(diagnostics[0]["range"]["startLine"], json!(1));

    proxy_send(
        &mut stream,
        2,
        json!({ "method": "get_file_diagnostics", "path": target.display().to_string() }),
    );
    settle(&mut app);
    let file_reply = proxy_recv(&mut stream);
    assert_eq!(file_reply["result"]["value"]["total"], json!(1), "reply: {file_reply}");
}

#[test]
fn proxy_phase3_symbols_references_and_code_actions_use_agent_payloads() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    let target = temp.path().join("symbols.rs");
    fs::write(&target, "fn main() {\n    thing();\n}\n").unwrap();
    let id = app.backend.open_buffer(Some(target.clone())).unwrap();
    app.backend.switch_to_id(id).unwrap();
    wait_until(&mut app, "symbol buffer active", |app| {
        app.backend.active().path.as_deref() == Some(target.as_path())
    });
    let view_id = app.backend.active().view_id.clone();
    app.backend.pending_agent_tool_results.push((
        view_id.clone(),
        String::from("document_symbols"),
        json!({
            "symbols": [{
                "name": "main",
                "kind": "function",
                "range": { "startLine": 1, "startCharacter": 1, "endLine": 3, "endCharacter": 2 },
                "selectionRange": { "startLine": 1, "startCharacter": 4, "endLine": 1, "endCharacter": 8 },
                "containerPath": target.display().to_string()
            }]
        }),
    ));
    app.backend.pending_agent_tool_results.push((
        view_id.clone(),
        String::from("references"),
        json!({
            "references": [{
                "path": target.display().to_string(),
                "range": { "startLine": 2, "startCharacter": 5, "endLine": 2, "endCharacter": 10 }
            }]
        }),
    ));
    app.backend.pending_agent_tool_results.push((
        view_id,
        String::from("list_code_actions"),
        json!({
            "actions": [{
                "title": "Replace thing",
                "kind": "quickfix",
                "hasCommand": false,
                "edits": [{
                    "range": { "startLine": 2, "startCharacter": 5, "endLine": 2, "endCharacter": 10 },
                    "newText": "other"
                }]
            }]
        }),
    ));

    let mut stream = connect_proxy(&app);
    proxy_send(
        &mut stream,
        1,
        json!({ "method": "document_symbols", "path": target.display().to_string() }),
    );
    settle(&mut app);
    let symbols_reply = proxy_recv(&mut stream);
    assert_eq!(symbols_reply["result"]["value"]["symbols"][0]["name"], json!("main"));

    proxy_send(
        &mut stream,
        2,
        json!({ "method": "references", "path": target.display().to_string(), "line": 2, "character": 5 }),
    );
    settle(&mut app);
    let references_reply = proxy_recv(&mut stream);
    assert_eq!(
        references_reply["result"]["value"]["references"][0]["range"]["startLine"],
        json!(2)
    );

    proxy_send(
        &mut stream,
        3,
        json!({ "method": "list_code_actions", "path": target.display().to_string(), "line": 2, "character": 5 }),
    );
    settle(&mut app);
    let actions_reply = proxy_recv(&mut stream);
    let action_id =
        actions_reply["result"]["value"]["actions"][0]["actionId"].as_str().unwrap().to_string();
    assert_eq!(actions_reply["result"]["value"]["actions"][0]["title"], json!("Replace thing"));

    proxy_send(
        &mut stream,
        4,
        json!({
            "method": "apply_code_action",
            "path": target.display().to_string(),
            "action_id": action_id
        }),
    );
    wait_until(&mut app, "code action approval queued", |app| !app.agents.approvals.is_empty());
    run_ex(&mut app, "agents");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "code action approval resolved", |app| app.agents.approvals.is_empty());
    let apply_reply = proxy_recv(&mut stream);
    assert_eq!(apply_reply["result"]["value"]["editCount"], json!(1));
    assert!(fs::read_to_string(&target).unwrap().contains("other();"));
}

#[test]
fn proxy_phase3_format_and_rename_apply_buffer_edits() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    let format_target = temp.path().join("format.rs");
    fs::write(&format_target, "fn main(){\n}\n").unwrap();
    let format_id = app.backend.open_buffer(Some(format_target.clone())).unwrap();
    app.backend.switch_to_id(format_id).unwrap();
    wait_until(&mut app, "format buffer active", |app| {
        app.backend.active().path.as_deref() == Some(format_target.as_path())
    });
    let format_view_id = app.backend.active().view_id.clone();
    app.backend.pending_agent_tool_results.push((
        format_view_id,
        String::from("format_preview"),
        json!({
            "edits": [{
                "range": { "startLine": 1, "startCharacter": 10, "endLine": 1, "endCharacter": 10 },
                "newText": " "
            }]
        }),
    ));

    let rename_target = temp.path().join("rename.rs");
    fs::write(&rename_target, "fn old_name() {}\nfn call() { old_name(); }\n").unwrap();
    let rename_id = app.backend.open_buffer(Some(rename_target.clone())).unwrap();
    app.backend.switch_to_id(rename_id).unwrap();
    wait_until(&mut app, "rename buffer active", |app| {
        app.backend.active().path.as_deref() == Some(rename_target.as_path())
    });
    let rename_view_id = app.backend.active().view_id.clone();
    let rename_payload = json!({
        "files": [{
            "path": rename_target.display().to_string(),
            "edits": [
                {
                    "range": { "startLine": 1, "startCharacter": 4, "endLine": 1, "endCharacter": 12 },
                    "newText": "new_name"
                },
                {
                    "range": { "startLine": 2, "startCharacter": 13, "endLine": 2, "endCharacter": 21 },
                    "newText": "new_name"
                }
            ]
        }]
    });
    app.backend.pending_agent_tool_results.push((
        rename_view_id.clone(),
        String::from("preview_rename"),
        rename_payload.clone(),
    ));
    app.backend.pending_agent_tool_results.push((
        rename_view_id,
        String::from("preview_rename"),
        rename_payload,
    ));

    let mut stream = connect_proxy(&app);
    proxy_send(
        &mut stream,
        1,
        json!({ "method": "format_file", "path": format_target.display().to_string() }),
    );
    wait_until(&mut app, "format approval queued", |app| !app.agents.approvals.is_empty());
    run_ex(&mut app, "agents");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "format approval resolved", |app| app.agents.approvals.is_empty());
    let format_reply = proxy_recv(&mut stream);
    assert_eq!(format_reply["result"]["value"]["editCount"], json!(1));
    assert_eq!(fs::read_to_string(&format_target).unwrap(), "fn main() {\n}\n");

    proxy_send(
        &mut stream,
        2,
        json!({
            "method": "preview_rename_symbol",
            "path": rename_target.display().to_string(),
            "line": 1,
            "character": 4,
            "new_name": "new_name"
        }),
    );
    settle(&mut app);
    let preview_reply = proxy_recv(&mut stream);
    assert_eq!(preview_reply["result"]["value"]["totalEdits"], json!(2));

    proxy_send(
        &mut stream,
        3,
        json!({
            "method": "rename_symbol",
            "path": rename_target.display().to_string(),
            "line": 1,
            "character": 4,
            "new_name": "new_name"
        }),
    );
    wait_until(&mut app, "rename approval queued", |app| !app.agents.approvals.is_empty());
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "rename approval resolved", |app| app.agents.approvals.is_empty());
    let rename_reply = proxy_recv(&mut stream);
    assert_eq!(rename_reply["result"]["value"]["editCount"], json!(2));
    assert!(fs::read_to_string(&rename_target).unwrap().contains("new_name();"));
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

// ── ee MCP proxy: ACP-native MCP-over-ACP (Phase 6b) ─────────────────────────

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

/// Script tail: `mcp/connect` (200), capture the connection id, run the
/// inner MCP `initialize` (201), then one `tools/call` (202).
pub(crate) fn acp_connect_script(tool_call: Value) -> FakeAgentScript {
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
                "method": "initialize",
                "params": {
                    "protocolVersion": "2026-07-28",
                    "capabilities": {},
                    "clientInfo": { "name": "fake-agent", "version": "0" }
                }
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

#[test]
fn session_new_advertises_acp_native_proxy_when_agent_supports_acp() {
    let (mut app, _temp, fake) = mcp_app(acp_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    let session_new = fake.agent().requests_by_method("session/new");
    let servers = session_new[0]
        .get("params")
        .and_then(|params| params.get("mcpServers"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let acp = servers
        .iter()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some("ee"))
        .expect("ee proxy entry forwarded");
    assert_eq!(acp.get("type").and_then(Value::as_str), Some("acp"));
    assert!(acp.get("serverId").is_some(), "acp entry carries the server id: {acp}");
    // Mutually exclusive with the stdio fallback (no `--mcp-proxy` args).
    assert!(!servers.iter().any(|entry| {
        entry
            .get("args")
            .and_then(Value::as_array)
            .is_some_and(|args| args.contains(&json!("--mcp-proxy")))
    }));
    // User-visible diagnostics: ACP-native mode.
    assert!(app.mcp_health_lines().iter().any(|line| line == "mcp proxy ee: acp-native"));
}

#[test]
fn session_new_falls_back_to_stdio_proxy_without_acp_capability() {
    let (mut app, _temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    // The wire shape is covered by `session_new_forwards_proxy_stdio_entry_...`;
    // here the diagnostics must name the fallback mode.
    assert!(app.mcp_health_lines().iter().any(|line| line == "mcp proxy ee: stdio fallback"));
}

#[test]
fn session_new_omits_acp_native_proxy_when_proxy_disabled() {
    let (mut app, _temp, fake) = mcp_app(base_agent_script(), false, false);
    open_pane_and_wait_ready(&mut app);

    let session_new = fake.agent().requests_by_method("session/new");
    let servers = session_new[0]
        .get("params")
        .and_then(|params| params.get("mcpServers"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(
        servers.iter().all(|entry| entry.get("name").and_then(Value::as_str) != Some("ee")),
        "no ee proxy entry when disabled: {servers:?}"
    );
    assert!(app.mcp_health_lines().iter().any(|line| line == "mcp: no servers started"));
}

#[test]
fn mcp_over_acp_write_denial_leaves_buffer_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("acp-proxy.txt");
    fs::write(&target, "original").unwrap();
    let target_text = target.display().to_string();
    let script = acp_connect_script(json!({
        "name": "ee_write_text_file",
        "arguments": { "path": target_text, "content": "agent-wrote-this" }
    }));
    let (mut app, fake) = mcp_app_in(&temp, script, false, true);
    open_pane_and_wait_ready(&mut app);

    // The write reaches the same approval prompt as direct ACP methods.
    wait_until(&mut app, "approval queued", |app| !app.agents.approvals.is_empty());
    let prompt = app.agents.approvals.front().unwrap();
    assert!(prompt.title.contains("fs/write_text_file"));
    assert!(prompt.detail.contains("acp-proxy.txt"));
    run_ex(&mut app, "agents");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE); // Deny
    wait_until(&mut app, "approval resolved", |app| app.agents.approvals.is_empty());

    // The denial surfaces back through `mcp/message` as an isError tool
    // result; buffer and disk stay unchanged.
    let reply = fake_response(&fake.agent(), 202);
    let result = reply.get("result").expect("tool result payload");
    assert_eq!(result.get("isError"), Some(&json!(true)));
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|blocks| blocks.first())
        .and_then(|block| block.get("text"))
        .and_then(Value::as_str)
        .expect("denial text");
    assert!(text.contains("denied"), "denial surfaced to the agent: {text}");
    assert_eq!(fs::read_to_string(&target).unwrap(), "original");
}

#[test]
fn mcp_over_acp_terminal_denial_does_not_spawn_terminal() {
    let script = acp_connect_script(json!({
        "name": "ee_terminal_create",
        "arguments": { "command": "sleep", "args": ["30"] }
    }));
    let (mut app, _temp, fake) = mcp_app(script, false, true);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "approval queued", |app| !app.agents.approvals.is_empty());
    let prompt = app.agents.approvals.front().unwrap();
    assert!(prompt.title.contains("terminal/create"));
    assert!(prompt.detail.contains("sleep 30"));
    run_ex(&mut app, "agents");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE); // Deny
    wait_until(&mut app, "approval resolved", |app| app.agents.approvals.is_empty());

    let reply = fake_response(&fake.agent(), 202);
    let result = reply.get("result").expect("tool result payload");
    assert_eq!(result.get("isError"), Some(&json!(true)));
    assert_eq!(app.agents.terminals.tracked_count(), 0, "denied terminal must not spawn");
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
    let spawned = app.agents.terminals.spawn(&request, Some("fake")).expect("terminal spawns");
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
    fs::write(temp.path().join(".ee.toml"), "[agents]\nenabled = false\n").unwrap();
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let mut app = App::from_path(None).unwrap();
    drop(_cwd_restore);
    drop(_cwd_lock);

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
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let app = App::from_path(None).unwrap();
    drop(_cwd_restore);
    drop(_cwd_lock);

    let secrets = app.agents_secret_values();
    assert!(secrets.contains(&String::from("super-secret-value")));

    let text = "trace: using super-secret-value now";
    let redacted = ee_agent_host::redact::redact_secret_values(text, &secrets);
    assert_eq!(redacted, "trace: using *** now");
    assert!(!redacted.contains("super-secret-value"));
}

#[test]
fn resolved_secret_values_are_collected_for_redaction_only_after_launch() {
    let temp = tempfile::tempdir().unwrap();
    // The reference must come from a user config layer (XDG), not the
    // ancestor workspace layer, or the merge rejects it.
    let xdg = temp.path().join("xdg");
    fs::create_dir_all(xdg.join("ee")).unwrap();
    fs::write(
        xdg.join("ee").join("config.toml"),
        "[agents]\nenabled = true\n\n[agents.servers.fake]\ncommand = \"unused\"\nenv = { OPENROUTER_API_KEY = \"secret://openrouter-api-key\" }\n",
    )
    .unwrap();
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let _xdg_guard = EnvVarGuard::set("XDG_CONFIG_HOME", xdg);
    let mut app = App::from_path(None).unwrap();

    // Inject a fake secrets store holding the referenced key; the reference
    // must not resolve before the launch configuration exists.
    let keychain = crate::secrets::test_support::StoredKeychain::new();
    let binding = crate::secrets::HostBinding::from_identifier_bytes(b"test-machine-id\n").unwrap();
    let store = crate::secrets::SecretStore::new(
        Box::new(keychain),
        binding,
        temp.path().join("ee/secrets/v1.json"),
    );
    store
        .set(
            &crate::secrets::SecretName::new("openrouter-api-key").unwrap(),
            &zeroize::Zeroizing::new(String::from("sk-resolved-42")),
        )
        .unwrap();
    app.agents.test_secret_store = Some(store);

    let before = app.agents_secret_values();
    assert!(
        !before.iter().any(|v| v == "sk-resolved-42"),
        "secret not resolved before agent launch"
    );

    // Opening the agents pane builds the launch configuration, which resolves
    // the reference and records the value for redaction.
    run_ex(&mut app, "agents");
    app.pump_agents();
    drop(_cwd_restore);
    drop(_cwd_lock);

    let secrets = app.agents_secret_values();
    assert!(
        secrets.contains(&String::from("sk-resolved-42")),
        "resolved value collected at launch"
    );
    assert!(!secrets.contains(&String::from("secret://openrouter-api-key")));

    let text = "stderr: token sk-resolved-42 leaked";
    let redacted = ee_agent_host::redact::redact_secret_values(text, &secrets);
    assert_eq!(redacted, "stderr: token *** leaked");
    assert!(!redacted.contains("sk-resolved-42"));
}
