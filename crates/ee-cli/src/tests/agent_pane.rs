//! Agents pane regression tests (Phase 3, feature `agents`).
//!
//! End-to-end through the real `ee-agent-host` stack: the pane starts the
//! host lazily, the host connects over an in-process fake ACP agent, and
//! every assertion goes through `App::pump_agents` so the event pipeline is
//! exercised exactly as in the TUI loop.  No external binaries are spawned.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ee_acp_agent_server::{
    AcpAgentServer, AcpAgentServerConfig, AcpServerError, MemoryTransport, MemoryTransportHandle,
};
use ee_agent_host::fake::{FakeAgent, FakeAgentScript, FakeAgentTransport, wire};
use ee_agent_host::{
    EvidenceCheck, EvidenceRevision, FakeTransportFactory, SafeFollowUp, TurnBlocker,
    TurnObservation, TurnTerminalStatus,
};
use ee_agent_protocol::{ContentBlock, RawJsonRpcMessage};
use ee_openrouter_agent::config::Config as OpenRouterConfig;
use ee_openrouter_agent::orchestrated::{
    openrouter_orchestrated_policy, openrouter_orchestrated_provider,
    openrouter_orchestrated_provider_with_turn_timeout, openrouter_orchestrator_config,
    test_support::ScriptedOpenRouterCompletion,
};
use futures::channel::mpsc as futures_mpsc;
use futures::{StreamExt, sink, stream};
use git2::{IndexAddOption, Repository, Signature};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};
use tokio::sync::watch;
use xi_core_lib::plugin_rpc::{Diagnostic, DiagnosticSeverity, Range};

use crate::app::{
    AgentPaneLayout, App, MessageRenderKind, Mode, ThreadUiState, TranscriptItem, wrap_text,
};
use crate::registers::RegisterName;
use crate::tests::helpers::*;
use crate::ui::ui;

const WAIT: Duration = Duration::from_secs(20);

/// Test-only write-verification hooks are process-global; live fixtures must not overlap.
static PHASE_SIX_LIVE_LOCK: Mutex<()> = Mutex::new(());

fn phase_six_live_lock() -> MutexGuard<'static, ()> {
    PHASE_SIX_LIVE_LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
}

// ── Fake transport factory ───────────────────────────────────────────────────

/// Builds one fake agent transport per connection and keeps the spawned
/// [`FakeAgent`] handle for assertions.
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

/// Test-only adapter from the host's line transport to a real in-process ACP
/// server. It retains scripted OpenRouter model traffic while exercising both
/// production provider construction and `AgentManager` transport insertion.
struct LiveAcpServer {
    stop: watch::Sender<bool>,
    pump: tokio::task::JoinHandle<()>,
    server: tokio::task::JoinHandle<Result<(), AcpServerError>>,
}

#[derive(Clone)]
struct LiveOpenRouterTransport {
    config: OpenRouterConfig,
    session_state_dir: PathBuf,
    scripted: ScriptedOpenRouterCompletion,
    turn_timeout: Option<Duration>,
    auto_resume_max: Option<u32>,
    servers: Arc<Mutex<Vec<LiveAcpServer>>>,
}

impl LiveOpenRouterTransport {
    fn new(
        config: OpenRouterConfig,
        session_state_dir: PathBuf,
        scripted: ScriptedOpenRouterCompletion,
    ) -> Self {
        Self {
            config,
            session_state_dir,
            scripted,
            turn_timeout: None,
            auto_resume_max: None,
            servers: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn with_turn_timeout(mut self, turn_timeout: Duration) -> Self {
        self.turn_timeout = Some(turn_timeout);
        self
    }

    /// Disables automatic recovery only where a fixture must exercise pane `/resume`.
    fn with_auto_resume_max(mut self, auto_resume_max: u32) -> Self {
        self.auto_resume_max = Some(auto_resume_max);
        self
    }

    fn shutdown(&self) {
        for server in self.servers.lock().expect("live ACP servers poisoned").drain(..) {
            let _ = server.stop.send(true);
            server.pump.abort();
            server.server.abort();
        }
    }
}

impl FakeTransportFactory for LiveOpenRouterTransport {
    fn build(&self) -> FakeAgentTransport {
        let adapter = self.scripted.adapter(self.config.clone());
        let provider = match (self.turn_timeout, self.auto_resume_max) {
            (None, None) => openrouter_orchestrated_provider(
                &self.config,
                self.session_state_dir.clone(),
                adapter,
            ),
            (Some(turn_timeout), None) => openrouter_orchestrated_provider_with_turn_timeout(
                &self.config,
                self.session_state_dir.clone(),
                adapter,
                turn_timeout,
            ),
            (turn_timeout, auto_resume_max) => {
                let mut config =
                    openrouter_orchestrator_config(&self.config, self.session_state_dir.clone());
                if let Some(turn_timeout) = turn_timeout {
                    config.orchestrator.turn_timeout = turn_timeout;
                }
                if let Some(auto_resume_max) = auto_resume_max {
                    config.orchestrator.recovery.auto_resume_max = auto_resume_max;
                }
                ee_agent_orchestrator::OrchestratorProvider::with_policy(
                    config,
                    Arc::new(adapter),
                    openrouter_orchestrated_policy(),
                )
            }
        };
        let server = AcpAgentServer::new(provider, AcpAgentServerConfig::default());
        let (transport, handle) = MemoryTransport::new();
        let server = tokio::spawn(async move { server.run_with_transport(transport).await });
        let (bridge, transport) = memory_transport_bridge(handle);
        self.servers.lock().expect("live ACP servers poisoned").push(LiveAcpServer {
            stop: bridge.0,
            pump: bridge.1,
            server,
        });
        transport
    }
}

fn memory_transport_bridge(
    handle: MemoryTransportHandle,
) -> ((watch::Sender<bool>, tokio::task::JoinHandle<()>), FakeAgentTransport) {
    let (to_host_tx, to_host_rx) = futures_mpsc::unbounded::<io::Result<String>>();
    let (stop_tx, stop_rx) = watch::channel(false);
    let outgoing_sink = sink::unfold(handle.clone(), |handle, line: String| async move {
        let frame = serde_json::from_str::<RawJsonRpcMessage>(&line)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if !handle.send(frame) {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "ACP server closed"));
        }
        Ok::<_, io::Error>(handle)
    });
    let pump = tokio::spawn(async move {
        loop {
            if *stop_rx.borrow() {
                break;
            }
            for frame in handle.take_outbound() {
                if let Ok(line) = serde_json::to_string(&frame) {
                    let _ = to_host_tx.unbounded_send(Ok(line));
                }
            }
            tokio::task::yield_now().await;
        }
    });
    let incoming_stream =
        stream::unfold(to_host_rx, |mut rx| async move { rx.next().await.map(|item| (item, rx)) });
    let transport = FakeAgentTransport::new(Box::pin(outgoing_sink), Box::pin(incoming_stream));
    ((stop_tx, pump), transport)
}

// ── Test helpers ─────────────────────────────────────────────────────────────

/// Standard initialize + session/new happy path.
fn base_script() -> FakeAgentScript {
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

fn openrouter_fixture_config() -> OpenRouterConfig {
    OpenRouterConfig {
        model: String::from("test/model"),
        api_url: String::from("https://openrouter.invalid/api/v1"),
        api_key: Some(String::from("sk-hermetic-test-key")),
        site_url: None,
        app_title: String::from("ee-cli-live-phase-six-test"),
        timeout: Duration::from_secs(1),
        system_prompt: String::from("system"),
        reasoning_effort: None,
        orchestrated: true,
        compact_min_messages: 4,
        compact_retained_tail: 2,
        compact_max_input_bytes: 65_536,
        context_window: 128_000,
        auto_compact_threshold_percent: 80,
        max_iterations: 16,
        retry_max_attempts: 0,
        retry_base_delay: Duration::from_millis(1),
        retry_max_delay: Duration::from_millis(10),
        checkpoint_dir: None,
    }
}

fn commit_git_baseline(workspace: &std::path::Path) {
    let repository = Repository::init(workspace).expect("initialize fixture repository");
    let mut index = repository.index().expect("fixture index");
    index.add_all(["*"].iter(), IndexAddOption::DEFAULT, None).expect("stage fixture baseline");
    index.write().expect("write fixture index");
    let tree_id = index.write_tree().expect("write fixture tree");
    let tree = repository.find_tree(tree_id).expect("find fixture tree");
    let signature =
        Signature::now("EE Fixture", "fixture@example.invalid").expect("fixture signature");
    repository
        .commit(Some("HEAD"), &signature, &signature, "fixture baseline", &tree, &[])
        .expect("commit fixture baseline");
}

fn live_openrouter_app_in(workspace: &std::path::Path, factory: LiveOpenRouterTransport) -> App {
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(workspace).unwrap();
    let mut app = App::from_path(None).unwrap();
    drop(_cwd_restore);
    drop(_cwd_lock);
    app.agents.test_fake_transports.insert(String::from("fake"), Arc::new(factory));
    app
}

/// Builds an `App` in an existing workspace and installs the fake agent for
/// the `fake` server id. Reusing the directory simulates a full TUI restart.
fn fake_agents_app_in(workspace: &std::path::Path, script: FakeAgentScript) -> (App, ScriptedFake) {
    fs::write(workspace.join(".ee.toml"), AGENTS_TOML).unwrap();
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(workspace).unwrap();
    let mut app = App::from_path(None).unwrap();
    drop(_cwd_restore);
    drop(_cwd_lock);
    let fake = ScriptedFake::new(script);
    app.agents.test_fake_transports.insert(String::from("fake"), Arc::new(fake.clone()));
    (app, fake)
}

/// Builds an `App` with agents enabled in a temp workspace and installs the
/// fake agent for the `fake` server id.
fn fake_agents_app(script: FakeAgentScript) -> (App, tempfile::TempDir, ScriptedFake) {
    let temp = tempfile::tempdir().unwrap();
    let (app, fake) = fake_agents_app_in(temp.path(), script);
    (app, temp, fake)
}

/// Pumps agents + backend until `condition` holds or the timeout fires.
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

fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    app.handle_event(Event::Key(KeyEvent::new(code, modifiers)));
}

fn approve_until_turn_ready(app: &mut App, label: &str, max_approvals: usize) -> usize {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut approvals = 0;
    while Instant::now() < deadline {
        app.pump_agents();
        let _ = app.backend.drain_events();
        if !app.agents.approvals.is_empty() {
            assert!(approvals < max_approvals, "{label} exceeded {max_approvals} approvals");
            press(app, KeyCode::Enter, KeyModifiers::NONE);
            approvals += 1;
        } else if app.agents.threads[0].state == ThreadUiState::Ready {
            return approvals;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "timed out waiting for {label}; approvals={approvals}; status={:?}",
        app.backend.status_message.as_deref()
    );
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PhaseSixFixtureMetrics {
    prompt_requests: usize,
    evidence_ids: usize,
    approvals: usize,
}

fn live_write_script(
    target: &Path,
    call_id: &str,
    content: &str,
    completion: &str,
) -> ScriptedOpenRouterCompletion {
    live_write_script_with_calls(target, &[(call_id, content)], completion)
}

fn live_tool_response(call_id: &str, name: &str, arguments: Value) -> Value {
    json!({
        "choices": [{
            "message": {
                "content": null,
                "tool_calls": [{
                    "id": call_id,
                    "type": "function",
                    "function": { "name": name, "arguments": arguments.to_string() },
                }],
            },
            "finish_reason": "tool_calls",
        }],
    })
}

fn live_completion_response(content: &str) -> Value {
    json!({
        "choices": [{
            "message": { "content": content },
            "finish_reason": "stop",
        }],
    })
}

fn live_write_script_with_calls(
    target: &Path,
    calls: &[(&str, &str)],
    completion: &str,
) -> ScriptedOpenRouterCompletion {
    let requests =
        calls.iter().map(|(call_id, content)| (*call_id, target, *content)).collect::<Vec<_>>();
    live_write_script_with_requests(&requests, completion)
}

fn live_write_script_with_requests(
    requests: &[(&str, &Path, &str)],
    completion: &str,
) -> ScriptedOpenRouterCompletion {
    let tool_calls = requests
        .iter()
        .map(|(call_id, target, content)| {
            json!({
                "id": call_id,
                "type": "function",
                "function": {
                    "name": "write_file",
                    "arguments": json!({
                        "path": target.display().to_string(),
                        "content": content,
                    })
                    .to_string(),
                },
            })
        })
        .collect::<Vec<_>>();
    ScriptedOpenRouterCompletion::new(vec![
        json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": tool_calls,
                },
                "finish_reason": "tool_calls",
            }],
        }),
        json!({
            "choices": [{
                "message": { "content": completion },
                "finish_reason": "stop",
            }],
        }),
    ])
}

fn live_write_script_in_rounds(
    requests: &[(&str, &Path, &str)],
    completion: &str,
) -> ScriptedOpenRouterCompletion {
    let mut responses = requests
        .iter()
        .map(|(call_id, target, content)| {
            json!({
                "choices": [{
                    "message": {
                        "content": null,
                        "tool_calls": [{
                            "id": call_id,
                            "type": "function",
                            "function": {
                                "name": "write_file",
                                "arguments": json!({
                                    "path": target.display().to_string(),
                                    "content": content,
                                })
                                .to_string(),
                            },
                        }],
                    },
                    "finish_reason": "tool_calls",
                }],
            })
        })
        .collect::<Vec<_>>();
    responses.push(json!({
        "choices": [{
            "message": { "content": completion },
            "finish_reason": "stop",
        }],
    }));
    ScriptedOpenRouterCompletion::new(responses)
}

fn select_live_write_mode(app: &mut App) {
    type_text(app, "/mode");
    press(app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(app, "provider write mode advertised", |app| {
        app.agents
            .mode_selection
            .as_ref()
            .is_some_and(|picker| picker.options.iter().any(|mode| mode == "write"))
    });
    while app
        .agents
        .mode_selection
        .as_ref()
        .is_some_and(|picker| picker.options[picker.selected] != "write")
    {
        press(app, KeyCode::Down, KeyModifiers::NONE);
    }
    press(app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(app, "provider write mode selected", |app| {
        app.agents.threads[0]
            .host
            .snapshot()
            .current_mode
            .as_ref()
            .is_some_and(|mode| mode.0.as_ref() == "write")
    });
}

fn begin_fixture_turn(app: &mut App, fake: &ScriptedFake) -> u64 {
    type_text(app, "phase six fixture");
    press(app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(app, "fixture turn starts", |app| {
        app.agents.threads[0].state == ThreadUiState::Running
            && app.agents.threads[0].host.active_turn_key().is_some()
    });
    wait_until(app, "fixture prompt reaches fake agent", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 1
    });
    app.agents.threads[0]
        .host
        .active_turn_key()
        .expect("fixture evidence must be recorded while turn remains active")
        .turn_id()
}

// ── Disabled path ────────────────────────────────────────────────────────────

#[test]
fn agents_disabled_path_opens_disabled_message_without_host() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join(".ee.toml"), "[agents]\nenabled = false\n").unwrap();
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let mut app = App::from_path(None).unwrap();
    drop(_cwd_restore);
    drop(_cwd_lock);

    run_ex(&mut app, "agents");

    let status = app.backend.status_message.clone().unwrap_or_default();
    assert!(status.contains("agents mode disabled"), "unexpected status: {status}");
    assert!(app.agents.host.is_none(), "disabled path must not start the host");
    assert_eq!(app.agents.layout, AgentPaneLayout::Closed);
    assert_eq!(app.mode, Mode::Normal);

    // The rest of the command surface stays inert too.
    run_ex(&mut app, "agents_layout");
    assert_eq!(app.agents.layout, AgentPaneLayout::Closed);
}

#[cfg(not(feature = "agents"))]
#[test]
fn agents_disabled_without_feature_stays_inert() {
    let mut app = App::from_path(None).unwrap();
    run_ex(&mut app, "agents");
    let status = app.backend.status_message.clone().unwrap_or_default();
    assert!(status.contains("compiled without `agents` feature"), "status: {status}");
}

// ── Enabled path + lazy start ────────────────────────────────────────────────

#[test]
fn agents_enabled_creates_pane_and_sends_lazy_session_new() {
    let (mut app, _temp, fake) = fake_agents_app(base_script());

    // Nothing starts until the user opens the pane.
    assert!(app.agents.host.is_none());
    assert_eq!(app.agents.layout, AgentPaneLayout::Closed);

    run_ex(&mut app, "agents");
    wait_until(&mut app, "first agent thread ready", |app| {
        app.agents.threads.len() == 1 && app.agents.threads[0].state == ThreadUiState::Ready
    });

    assert_eq!(app.agents.layout, AgentPaneLayout::Full);
    assert_eq!(app.mode, Mode::Agent);
    assert_eq!(app.agents.threads[0].session_id, "s1");
    assert_eq!(app.agents.threads[0].display_name, "1.fake");

    let agent = fake.agent();
    assert!(!agent.requests_by_method("initialize").is_empty(), "initialize must be sent");
    assert!(!agent.requests_by_method("session/new").is_empty(), "session/new must be sent");

    // Re-opening the pane must not create a second session.
    run_ex(&mut app, "agents");
    wait_until(&mut app, "session count stays one", |app| app.agents.threads.len() == 1);
    assert_eq!(agent.requests_by_method("session/new").len(), 1);
}

#[test]
fn agents_mode_advertises_editor_backed_optional_client_capabilities() {
    let (mut app, _temp, fake) = fake_agents_app(base_script());

    open_pane_and_wait_ready(&mut app);

    let initialize = fake
        .agent()
        .requests_by_method("initialize")
        .into_iter()
        .next()
        .expect("initialize request sent");
    let client_capabilities = &initialize["params"]["clientCapabilities"];
    assert_eq!(client_capabilities["fs"]["readTextFile"], true);
    assert_eq!(client_capabilities["fs"]["writeTextFile"], true);
    assert_eq!(client_capabilities["terminal"], true);
    assert_eq!(client_capabilities["elicitation"]["form"], json!({}));
    assert_eq!(client_capabilities["elicitation"]["url"], json!({}));
}

#[test]
fn agents_close_restores_mode_that_opened_command_line() {
    let (mut app, _temp, _fake) = fake_agents_app(base_script());

    app.mode = Mode::CommandLine;
    app.command_mode_origin = Some(Mode::Insert);
    type_text(&mut app, "agents");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "first agent thread ready", |app| {
        app.agents.threads.len() == 1 && app.agents.threads[0].state == ThreadUiState::Ready
    });
    assert_eq!(app.mode, Mode::Agent);

    run_ex(&mut app, "agents_close");

    assert_eq!(app.mode, Mode::Insert, "focus returns to command-line origin mode");
    assert_eq!(app.agents.layout, AgentPaneLayout::Closed, "explicit close hides agents pane");
}

#[test]
fn colon_in_agents_pane_stays_in_agent_draft() {
    let (mut app, _temp, _fake) = fake_agents_app(base_script());

    open_pane_and_wait_ready(&mut app);
    press(&mut app, KeyCode::Char(':'), KeyModifiers::NONE);

    assert_eq!(
        app.mode,
        Mode::Agent,
        "colon must not open ee command line while agents pane has focus"
    );
    assert_eq!(app.command_buffer, "", "editor command buffer stays untouched");
    assert_eq!(app.agents.threads[0].draft, ":", "colon is regular agent prompt input");
}

#[test]
fn pane_startup_is_inert_and_editor_modes_unchanged_while_closed() {
    let (mut app, _temp, _fake) = fake_agents_app(base_script());

    assert_eq!(app.agents.layout, AgentPaneLayout::Closed);
    assert_eq!(app.mode, Mode::Normal);

    // Normal editing works while the pane is closed.
    press(&mut app, KeyCode::Char('i'), KeyModifiers::NONE);
    type_text(&mut app, "ab");
    wait_until(&mut app, "insert text lands", |app| app.backend.lines == vec![String::from("ab")]);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::Normal);

    // Command line still opens.
    press(&mut app, KeyCode::Char(':'), KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::CommandLine);
}

// ── Prompt submission ────────────────────────────────────────────────────────

#[test]
fn enter_without_config_reports_needed_server_and_keeps_draft() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join(".ee.toml"), "[agents]\nenabled = true\n").unwrap();
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let mut app = App::from_path(None).unwrap();
    drop(_cwd_restore);
    drop(_cwd_lock);

    run_ex(&mut app, "agents");
    type_text(&mut app, "qwe");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    let status = app.backend.status_message.clone().unwrap_or_default();
    assert!(status.contains("no agent configured"), "unexpected status: {status}");
    assert_eq!(app.agents.pending_draft, "qwe");
    assert!(app.agents.threads.is_empty());
}

#[test]
fn exit_slash_commands_work_without_a_configured_agent() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join(".ee.toml"), "[agents]\nenabled = true\n").unwrap();
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let mut app = App::from_path(None).unwrap();
    drop(_cwd_restore);
    drop(_cwd_lock);

    run_ex(&mut app, "agents");
    assert_eq!(app.agents.layout, AgentPaneLayout::Full);
    assert!(app.agents.threads.is_empty());

    type_text(&mut app, "/quit");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert_eq!(app.agents.layout, AgentPaneLayout::Closed);
    assert_eq!(app.mode, Mode::Normal);
    assert!(app.agents.pending_draft.is_empty());
    assert!(app.agents.pending_session.is_none());
    assert!(app.agents.threads.is_empty());

    run_ex(&mut app, "agents");
    type_text(&mut app, "/quit_full");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert!(app.should_quit);
    assert_eq!(app.agents.layout, AgentPaneLayout::Full);
    assert_eq!(app.mode, Mode::Agent);
    assert!(app.agents.pending_draft.is_empty());
    assert!(app.agents.pending_session.is_none());
    assert!(app.agents.threads.is_empty());
}

#[test]
fn local_slash_commands_control_agent_tui_without_forwarding_prompts() {
    let script = base_script().wait_for("session/new").respond(json!({ "sessionId": "s2" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/lay");
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].draft, "/layout");
    press(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);

    type_text(&mut app, "/new_thread");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "second thread ready", |app| {
        app.agents.threads.len() == 2 && app.agents.threads[1].state == ThreadUiState::Ready
    });
    assert_eq!(app.agents.active_thread, Some(1));

    type_text(&mut app, "/prev");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.active_thread, Some(0));
    type_text(&mut app, "/next");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.active_thread, Some(1));

    type_text(&mut app, "/layout right");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.layout, AgentPaneLayout::Right);

    type_text(&mut app, "/thoughts off");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(!app.agents.show_thoughts);

    type_text(&mut app, "/config");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.backend.status_message.as_deref(), Some("no session config options advertised"));

    type_text(&mut app, "/mcp");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        app.backend
            .status_message
            .as_deref()
            .is_some_and(|message| message.contains("no MCP servers"))
    );

    type_text(&mut app, "/stop");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.backend.status_message.as_deref(), Some("no running turn to stop"));

    assert!(!app.agents.threads[1].transcript.is_empty());
    type_text(&mut app, "/clear");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.agents.threads[1].transcript.is_empty());
    assert!(
        fake.agent().requests_by_method("session/prompt").is_empty(),
        "local slash commands must not forward prompt turns"
    );
}

#[test]
fn phase_one_slash_commands_are_local_and_safe() {
    let (mut app, _temp, fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/help");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let help = app.agents.threads[0].system_notices().join("\n");
    assert!(help.contains("Local slash commands:"));
    assert!(help.contains("/approval — default|autopilot|bypass; bypass keeps validation"));
    assert!(help.contains("Provider-owned slash commands"));

    type_text(&mut app, "/status");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let status = app.backend.status_message.as_deref().expect("status output");
    assert!(status.contains("session:s1"));
    assert!(status.contains("agent:fake"));
    assert!(status.contains("approval:default"));

    app.agents.threads[0].transcript.push(TranscriptItem::Message {
        nick: String::from("fake"),
        text: String::from("safe completed response"),
        kind: MessageRenderKind::Assistant,
        message_id: Some(String::from("assistant-1")),
        response_group: Some(1),
        at: SystemTime::now(),
    });
    type_text(&mut app, "/copy");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.registers.get(&RegisterName::Clipboard), "safe completed response");
    assert_eq!(app.backend.status_message.as_deref(), Some("copied assistant response 1"));

    type_text(&mut app, "/rename   Audit\u{200B}   run   ");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].session_name.as_deref(), Some("Audit run"));
    assert_eq!(app.agents.threads[0].display_name, "1.Audit run");

    type_text(&mut app, "/diff");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        app.backend
            .status_message
            .as_deref()
            .is_some_and(|message| message.contains("workspace diff unavailable"))
    );
    assert!(fake.agent().requests_by_method("session/prompt").is_empty());
}

#[test]
fn phase_two_session_lifecycle_commands_keep_local_and_provider_state_distinct() {
    let script = base_script().wait_for("session/new").respond(json!({ "sessionId": "s2" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    app.agents.threads[0].transcript.push(TranscriptItem::System {
        text: String::from("first transcript"),
        at: SystemTime::now(),
    });
    type_text(&mut app, "/clear");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.agents.threads[0].transcript.is_empty());
    assert_eq!(app.agents.threads[0].session_id, "s1", "clear must not create/reset session");
    assert!(
        app.backend
            .status_message
            .as_deref()
            .is_some_and(|message| message.contains("provider conversation remains intact"))
    );

    type_text(&mut app, "/new");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "new session ready", |app| {
        app.agents.threads.len() == 2 && app.agents.threads[1].state == ThreadUiState::Ready
    });
    assert_eq!(app.agents.threads[1].session_id, "s2");

    app.agents.threads[1].transcript.push(TranscriptItem::System {
        text: String::from("archived transcript"),
        at: SystemTime::now(),
    });
    type_text(&mut app, "/archive");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.threads.len(), 1);
    assert_eq!(app.agents.archived_threads.len(), 1);
    assert!(
        app.agents.archived_threads[0]
            .system_notices()
            .contains(&String::from("archived transcript"))
    );

    type_text(&mut app, "/archive restore 1");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.threads.len(), 2);
    assert!(app.agents.archived_threads.is_empty());
    assert_eq!(app.agents.active_thread, Some(1));

    type_text(&mut app, "/delete");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.agents.session_deletion_confirmation.is_some());
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.agents.session_deletion_confirmation.is_none());
    assert_eq!(app.agents.threads.len(), 2);

    type_text(&mut app, "/delete");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.threads.len(), 1);
    assert_eq!(app.agents.threads[0].session_id, "s1");
    assert!(
        app.backend
            .status_message
            .as_deref()
            .is_some_and(|message| message.contains("provider session unchanged"))
    );
    assert_eq!(fake.agent().requests_by_method("session/new").len(), 2);
}

#[test]
fn fork_and_branch_create_redacted_seeded_sessions_without_mutating_parent() {
    let script = base_script()
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s2" }))
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s3" }))
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);
    app.agents.threads[0].transcript.push(TranscriptItem::Message {
        nick: String::from("you"),
        text: String::from("parent context"),
        kind: MessageRenderKind::User,
        message_id: Some(String::from("parent-1")),
        response_group: None,
        at: SystemTime::now(),
    });

    type_text(&mut app, "/fork");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "fork ready", |app| app.agents.threads.len() == 2);
    assert_eq!(app.agents.active_thread, Some(0), "fork keeps parent active");
    assert_eq!(app.agents.threads[1].fork_parent_session_id.as_deref(), Some("s1"));
    assert!(app.agents.threads[1].context_files.is_empty(), "child attachments must be isolated");
    wait_until(&mut app, "fork seed sent", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 1
    });
    let prompts = fake.agent().requests_by_method("session/prompt");
    let seed = prompts[0]["params"]["prompt"][0]["text"].as_str().expect("fork seed text");
    assert!(seed.contains("parent context"));
    assert!(seed.contains("not provider-side session cloning"));

    type_text(&mut app, "/branch");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "branch ready", |app| app.agents.threads.len() == 3);
    assert_eq!(app.agents.active_thread, Some(2), "branch switches to child");
    assert_eq!(app.agents.threads[2].fork_parent_session_id.as_deref(), Some("s1"));
    assert!(app.agents.threads[2].context_files.is_empty());
}

#[test]
fn approval_slash_command_scopes_modes_to_active_session_and_confirms_bypass() {
    let (mut app, _temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/approval");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.backend.status_message.as_deref(), Some("tool approvals: default"));

    type_text(&mut app, "/approval autopilot");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.approval_modes.get("s1").map(|mode| mode.label()), Some("autopilot"));

    type_text(&mut app, "/approval bypass");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.agents.approval_mode_confirmation.is_some(), "bypass needs confirmation");

    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rendered: Vec<String> = (0..20)
        .map(|y| {
            (0..100).map(|x| terminal.backend().buffer().cell((x, y)).unwrap().symbol()).collect()
        })
        .collect();
    assert!(
        rendered.iter().any(|row| row.contains("enable bypass tool approvals")),
        "confirmation missing: {rendered:#?}"
    );

    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.agents.approval_mode_confirmation.is_none());
    assert_eq!(
        app.agents.approval_modes.get("s1").map(|mode| mode.label()),
        Some("autopilot"),
        "cancelling bypass preserves prior mode"
    );

    type_text(&mut app, "/approval bypass");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.approval_modes.get("s1").map(|mode| mode.label()), Some("bypass"));

    type_text(&mut app, "/approval default");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(!app.agents.approval_modes.contains_key("s1"), "default removes session override");
}

#[test]
fn draft_typed_before_session_ready_carries_into_first_thread() {
    let (mut app, _temp, _fake) = fake_agents_app(base_script());

    run_ex(&mut app, "agents");
    type_text(&mut app, "hello before ready");
    assert_eq!(app.agents.pending_draft, "hello before ready");

    wait_until(&mut app, "first agent thread ready", |app| {
        app.agents.threads.len() == 1 && app.agents.threads[0].state == ThreadUiState::Ready
    });

    assert!(app.agents.pending_draft.is_empty());
    assert_eq!(app.agents.threads[0].draft, "hello before ready");
}

#[test]
fn prompt_submission_appends_optimistic_you_and_sends_acp_prompt() {
    let script =
        base_script().wait_for("session/prompt").respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "hello agent");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    // Optimistic `you` message appears before any agent traffic.
    let pairs = app.agents.threads[0].message_pairs();
    assert_eq!(pairs, vec![(String::from("you"), String::from("hello agent"))]);
    assert!(app.agents.threads[0].draft.is_empty(), "draft must clear after submit");

    wait_until(&mut app, "prompt sent to agent", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 1
    });
    wait_until(&mut app, "turn completed notice", |app| {
        app.agents.threads[0].system_notices().iter().any(|n| n.contains("turn completed"))
    });
    assert_eq!(app.agents.threads[0].state, ThreadUiState::Ready);
}

#[test]
fn context_slash_command_attaches_snapshots_and_controls_session_files() {
    let script =
        base_script().wait_for("session/prompt").respond(json!({ "stopReason": "end_turn" }));
    let (mut app, temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);
    fs::write(temp.path().join("context.txt"), "context snapshot\n").unwrap();

    type_text(&mut app, "/context add context.txt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].context_files.len(), 1);
    assert_eq!(app.agents.threads[0].context_files[0].relative_path, "context.txt");
    assert_eq!(app.agents.threads[0].context_files[0].content, "context snapshot\n");
    assert_eq!(
        app.backend.status_message.as_deref(),
        Some("context attached: context.txt (17 bytes)")
    );

    type_text(&mut app, "/context");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        app.backend.status_message.as_deref(),
        Some("context files: context.txt (17 bytes)")
    );

    type_text(&mut app, "use selected context");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "context prompt sent", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 1
    });
    let prompt = &fake.agent().requests_by_method("session/prompt")[0]["params"]["prompt"];
    assert_eq!(prompt[0]["text"], "use selected context");
    let context = prompt[1]["text"].as_str().expect("context block text");
    assert!(context.contains("User-selected context file: `context.txt`"), "{context}");
    assert!(context.contains("context snapshot\n"), "{context}");
    assert!(
        !app.agents.threads[0]
            .system_notices()
            .iter()
            .any(|notice| notice.contains("context snapshot")),
        "context body must not enter transcript"
    );

    wait_until(&mut app, "context turn completes", |app| {
        app.agents.threads[0].state == ThreadUiState::Ready
    });
    type_text(&mut app, "/context remove context.txt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.agents.threads[0].context_files.is_empty());
    type_text(&mut app, "/context add context.txt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    type_text(&mut app, "/context clear");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.agents.threads[0].context_files.is_empty());
}

#[test]
fn context_status_and_one_turn_mentions_stay_bounded_and_session_local() {
    let script =
        base_script().wait_for("session/prompt").respond(json!({ "stopReason": "end_turn" }));
    let (mut app, temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);
    fs::write(temp.path().join("session.txt"), "session snapshot\n").unwrap();
    fs::write(temp.path().join("mention.txt"), "mention snapshot\n").unwrap();

    type_text(&mut app, "/context add session.txt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    type_text(&mut app, "/mention mention.txt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].context_files.len(), 1);
    assert_eq!(app.agents.threads[0].next_prompt_context_files.len(), 1);

    type_text(&mut app, "/context status");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let status = app.backend.status_message.clone().unwrap_or_default();
    assert!(status.contains("scope=session-only"), "status: {status}");
    assert!(status.contains("session.txt (17 bytes)"), "status: {status}");
    assert!(status.contains("mention.txt (17 bytes)"), "status: {status}");

    type_text(&mut app, "use both snapshots");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "mention prompt sent", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 1
    });
    let prompt = &fake.agent().requests_by_method("session/prompt")[0]["params"]["prompt"];
    assert_eq!(prompt.as_array().map(Vec::len), Some(3));
    assert!(prompt[2]["text"].as_str().is_some_and(|text| text.contains("One-turn user mention")));
    assert!(app.agents.threads[0].next_prompt_context_files.is_empty());
}

#[test]
fn composer_at_path_completion_completes_only_safe_unique_workspace_file() {
    let (mut app, temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);
    fs::write(temp.path().join("mention-target.txt"), "safe\n").unwrap();
    fs::write(temp.path().join(".env"), "TOKEN=secret\n").unwrap();

    type_text(&mut app, "review @mention-t");
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].draft, "review @mention-target.txt");

    press(&mut app, KeyCode::Char(' '), KeyModifiers::NONE);
    type_text(&mut app, "@.e");
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].draft, "review @mention-target.txt @.e\t");
}

#[test]
fn add_dir_requires_advertised_capability_and_explicit_confirmation() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": { "sessionCapabilities": { "additionalDirectories": {} } }
        }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }));
    let (mut app, temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);
    let extra = temp.path().join("extra-root");
    fs::create_dir(&extra).unwrap();

    type_text(&mut app, "/add-dir extra-root");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.agents.additional_directory_confirmation.is_some());
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.agents.additional_workspace_roots.is_empty());

    type_text(&mut app, "/add-dir extra-root");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.additional_workspace_roots.len(), 1);
    assert!(
        app.backend
            .status_message
            .as_deref()
            .is_some_and(|status| status.contains("current provider session unchanged"))
    );
}

#[test]
fn ps_tasks_and_terminal_stop_stay_scoped_to_active_agent_session() {
    let (mut app, _temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);
    let request = ee_agent_protocol::CreateTerminalRequest::new(
        ee_agent_protocol::SessionId::new("s1"),
        "sleep",
    )
    .args(vec![String::from("30")]);
    let created =
        app.agents.terminals.spawn(&request, Some("fake")).expect("owned terminal spawns");
    let terminal_id = created.terminal_id.0.to_string();

    type_text(&mut app, "/ps");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        app.backend.status_message.as_deref().is_some_and(|status| status.contains(&terminal_id))
    );

    type_text(&mut app, "/tasks");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        app.backend
            .status_message
            .as_deref()
            .is_some_and(|status| status.contains("subagent tasks: unavailable"))
    );

    type_text(&mut app, &format!("/stop {terminal_id}"));
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        app.backend
            .status_message
            .as_deref()
            .is_some_and(|status| status.contains("stop requested"))
    );
    app.agents.terminals.kill_all();
}

#[test]
fn phase_four_history_queue_transcript_and_draft_controls_stay_local_and_safe() {
    let script = base_script()
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }))
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }))
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }))
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }))
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "first prompt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].state, ThreadUiState::Running);
    type_text(&mut app, "second prompt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    type_text(&mut app, "third prompt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].queued_prompts.len(), 2);

    type_text(&mut app, "/queue move 2 1");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    type_text(&mut app, "/queue edit 1 revised third prompt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "queued prompts dispatch", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 3
    });
    let prompts = fake.agent().requests_by_method("session/prompt");
    assert_eq!(prompts[0]["params"]["prompt"][0]["text"], "first prompt");
    assert_eq!(prompts[1]["params"]["prompt"][0]["text"], "revised third prompt");
    assert_eq!(prompts[2]["params"]["prompt"][0]["text"], "second prompt");
    wait_until(&mut app, "queued turns complete", |app| {
        app.agents.threads[0].state == ThreadUiState::Ready
    });

    type_text(&mut app, "duplicate prompt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "first duplicate completes", |app| {
        app.agents.threads[0].state == ThreadUiState::Ready
    });
    type_text(&mut app, "duplicate prompt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "second duplicate completes", |app| {
        app.agents.threads[0].state == ThreadUiState::Ready
    });
    assert_eq!(
        app.agents.threads[0]
            .prompt_history
            .iter()
            .filter(|prompt| prompt.as_str() == "duplicate prompt")
            .count(),
        1,
        "adjacent duplicate prompts collapse in history"
    );

    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].draft, "duplicate prompt");
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert!(app.agents.threads[0].draft.is_empty());
    type_text(&mut app, "second");
    press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert_eq!(app.agents.threads[0].draft, "second prompt");
    press(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);

    type_text(&mut app, "/details on");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.agents.threads[0].transcript_detail);
    app.key_bindings.insert(
        crate::keymap::BindingKey {
            mode: Mode::Agent,
            key: KeyCode::Char('x'),
            modifiers: KeyModifiers::CONTROL,
            prefix: None,
        },
        crate::keymap::Action::AgentToggleTranscriptRaw,
    );
    press(&mut app, KeyCode::Char('x'), KeyModifiers::CONTROL);
    assert!(app.agents.threads[0].transcript_raw, "configured Agent-mode keymap action dispatches");
    app.agents.threads[0].transcript.push(TranscriptItem::ToolCall {
        id: String::from("diff-1"),
        title: String::from("edit"),
        status: String::from("completed"),
        detail: String::from("kind: edit · content: diff: src/example.rs"),
        response_group: 77,
        at: SystemTime::now(),
    });
    assert_eq!(
        app.agents.threads[0].response_group_change_summary(77).as_deref(),
        Some("changes: src/example.rs")
    );

    type_text(&mut app, "long local draft");
    press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert!(app.agents.threads[0].draft.is_empty());
    press(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
    assert_eq!(app.agents.threads[0].draft, "long local draft");
    press(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL | KeyModifiers::SHIFT);
    let request = app.take_agent_external_editor_request().expect("external editor request queued");
    assert_eq!(request.session_id.as_deref(), Some("s1"));
    assert_eq!(request.draft, "long local draft");
    assert!(fake.agent().requests_by_method("session/prompt").len() == 5);
}

#[test]
fn context_slash_command_rejects_protected_outside_and_oversized_files() {
    let (mut app, temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);
    fs::write(temp.path().join(".env"), "TOKEN=secret\n").unwrap();
    fs::write(temp.path().join("too-large.txt"), "x".repeat(64 * 1024 + 1)).unwrap();

    type_text(&mut app, "/context add .env");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        app.backend.status_message.as_deref().is_some_and(|message| message.contains("protected"))
    );
    type_text(&mut app, "/context add /tmp/outside-context.txt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        app.backend.status_message.as_deref(),
        Some("context path must be workspace-relative")
    );
    type_text(&mut app, "/context add too-large.txt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.backend.status_message.as_deref().is_some_and(|message| message.contains("limit")));
    assert!(app.agents.threads[0].context_files.is_empty());

    fs::write(temp.path().join("first.txt"), "a".repeat(64 * 1024)).unwrap();
    fs::write(temp.path().join("second.txt"), "b".repeat(64 * 1024)).unwrap();
    fs::write(temp.path().join("third.txt"), "c").unwrap();
    type_text(&mut app, "/context add first.txt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    type_text(&mut app, "/context add second.txt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    type_text(&mut app, "/context add third.txt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.backend.status_message.as_deref().is_some_and(|message| message.contains("total")));
    assert_eq!(app.agents.threads[0].context_files.len(), 2);
}

#[cfg(unix)]
#[test]
fn context_and_mentions_reject_workspace_symlink_escapes_before_reading() {
    let (mut app, temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), "outside-secret-value\n").unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.txt"), temp.path().join("escape.txt"))
        .unwrap();

    type_text(&mut app, "/context add escape.txt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        app.backend.status_message.as_deref(),
        Some("context file outside primary workspace")
    );
    assert!(app.agents.threads[0].context_files.is_empty());

    type_text(&mut app, "/mention escape.txt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        app.backend.status_message.as_deref(),
        Some("context file outside primary workspace")
    );
    assert!(app.agents.threads[0].next_prompt_context_files.is_empty());
    assert!(
        !app.agents.threads[0]
            .system_notices()
            .iter()
            .any(|notice| notice.contains("outside-secret-value")),
        "rejected target content must never reach transcript notices"
    );
}

#[test]
fn context_slash_command_uses_unsaved_open_buffer_snapshot() {
    let script =
        base_script().wait_for("session/prompt").respond(json!({ "stopReason": "end_turn" }));
    let (mut app, temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);
    let path = temp.path().join("dirty.txt");
    fs::write(&path, "disk text\n").unwrap();
    let id = app.backend.open_buffer(Some(path.clone())).unwrap();
    app.backend.switch_to_id(id).unwrap();
    wait_until(&mut app, "context buffer open", |app| {
        app.backend.active().path.as_deref() == Some(path.as_path())
    });
    app.backend.replace_line_range(0, 0, &[String::from("unsaved text")]).unwrap();
    app.backend.flush_all_pending_edits().unwrap();
    wait_until(&mut app, "context buffer dirty", |app| {
        app.backend
            .active()
            .whole_text()
            .as_deref()
            .is_some_and(|text| text.starts_with("unsaved text"))
    });

    type_text(&mut app, "/context add dirty.txt");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.agents.threads[0].context_files[0].content.starts_with("unsaved text"));
    type_text(&mut app, "use dirty context");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "dirty context prompt sent", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 1
    });
    let prompts = fake.agent().requests_by_method("session/prompt");
    let context = prompts[0]["params"]["prompt"][1]["text"].as_str().expect("context block text");
    assert!(context.contains("unsaved text"), "{context}");
    assert!(!context.contains("disk text"), "{context}");
}

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
    wait_until(&mut app, "real write approval", |app| app.agents.approvals.len() == 1);
    let approval_count = app.agents.approvals.len();
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "pane terminal write evidence", |app| {
        app.agents.threads[0].terminal_evidence.as_ref().is_some_and(|summary| {
            summary.status == TurnTerminalStatus::PartiallyVerified
                && summary.blocker == Some(TurnBlocker::MissingSelectedValidation)
                && summary.safe_follow_up == SafeFollowUp::RunSelectedValidation
        })
    });

    wait_until(&mut app, "real provider turn completion", |app| {
        app.agents.threads[0].state == ThreadUiState::Ready
    });

    let thread = &app.agents.threads[0];
    let summary = thread.terminal_evidence.as_ref().expect("post-write pane evidence");
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
    wait_until(&mut app, "real dirty write approval", |app| app.agents.approvals.len() == 1);
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
fn phase_six_live_openrouter_pane_diagnostics_regression_reports_blocked_evidence() {
    let _live_lock = phase_six_live_lock();
    let workspace = tempfile::tempdir().expect("fixture workspace");
    let target = workspace.path().join("diagnostics.txt");
    fs::write(workspace.path().join(".ee.toml"), AGENTS_TOML).expect("write agents config");
    fs::write(&target, "before\n").expect("write baseline file");
    commit_git_baseline(workspace.path());

    let scripted =
        live_write_script(&target, "write-live-diagnostics", "after\n", "diagnostics regressed");
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
    App::set_pre_write_verification_test_hook(|app| {
        app.backend.diagnostics = vec![Diagnostic {
            range: Range { start: 0, end: 5 },
            severity: DiagnosticSeverity::Error,
            message: String::from("fresh post-write diagnostic"),
            source: Some(String::from("test-lsp")),
            code: Some(String::from("E-POST-WRITE")),
        }];
    });
    open_pane_and_wait_ready(&mut app);
    select_live_write_mode(&mut app);

    type_text(&mut app, "trigger real diagnostic regression");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "diagnostic write approval", |app| app.agents.approvals.len() == 1);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "diagnostic provider turn completion", |app| {
        app.agents.threads[0].state == ThreadUiState::Ready
    });

    let thread = &app.agents.threads[0];
    let summary = thread.terminal_evidence.as_ref().expect("diagnostics pane evidence");
    assert_eq!(summary.status, TurnTerminalStatus::Blocked);
    assert_eq!(summary.blocker, Some(TurnBlocker::DiagnosticsFailed));
    assert_eq!(summary.safe_follow_up, SafeFollowUp::RefreshEvidence);
    assert_eq!(fs::read_to_string(&target).expect("read diagnostics write"), "after\n");
    assert_eq!(app.backend.diagnostics.len(), 1, "post-write diagnostic reaches editor state");
    let bodies = scripted.request_bodies();
    assert_eq!(bodies.len(), 3, "diagnostic failure triggers one bounded repair model round");
    assert!(bodies.iter().any(|body| {
        body["messages"].as_array().is_some_and(|messages| {
            messages.iter().any(|message| {
                message["content"]
                    .as_str()
                    .is_some_and(|text| text.contains("Repair controller request."))
            })
        })
    }));

    app.shutdown_agents();
    factory.shutdown();
}

#[test]
fn phase_six_live_openrouter_pane_stale_revision_after_evidence_is_blocked() {
    let _live_lock = phase_six_live_lock();
    let workspace = tempfile::tempdir().expect("fixture workspace");
    let target = workspace.path().join("stale.txt");
    fs::write(workspace.path().join(".ee.toml"), AGENTS_TOML).expect("write agents config");
    fs::write(&target, "before\n").expect("write baseline file");
    commit_git_baseline(workspace.path());

    let scripted = live_write_script(&target, "write-stale", "after\n", "write complete");
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
    App::set_post_write_test_hook(|app| {
        app.backend
            .replace_line_range(0, 0, &[String::from("intervening editor mutation")])
            .expect("mutate buffer after evidence capture");
        app.backend.flush_all_pending_edits().expect("flush intervening editor mutation");
    });
    open_pane_and_wait_ready(&mut app);
    select_live_write_mode(&mut app);

    type_text(&mut app, "write then mutate editor after verification capture");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "stale write approval", |app| app.agents.approvals.len() == 1);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "stale revision evidence", |app| {
        app.agents.threads[0].terminal_evidence.as_ref().is_some_and(|summary| {
            summary.status == TurnTerminalStatus::Blocked
                && summary.blocker == Some(TurnBlocker::StaleRevision)
                && summary.safe_follow_up == SafeFollowUp::RefreshEvidence
        })
    });
    wait_until(&mut app, "stale provider turn completion", |app| {
        app.agents.threads[0].state == ThreadUiState::Ready
    });

    let thread = &app.agents.threads[0];
    assert_eq!(fs::read_to_string(&target).expect("read saved agent write"), "after\n");
    assert_eq!(app.backend.active().whole_text().as_deref(), Some("intervening editor mutation\n"));
    assert_eq!(
        thread.terminal_evidence.as_ref().expect("stale evidence").blocker,
        Some(TurnBlocker::StaleRevision)
    );
    assert_eq!(scripted.request_bodies().len(), 2);

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
fn phase_six_live_openrouter_pane_repeated_selected_validation_stops_repair() {
    let _live_lock = phase_six_live_lock();
    let workspace = tempfile::tempdir().expect("fixture workspace");
    let target = workspace.path().join("lib.rs");
    fs::write(workspace.path().join(".ee.toml"), AGENTS_TOML).expect("write agents config");
    fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"phase-six-validation\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\npath = \"lib.rs\"\n",
    )
    .expect("write cargo manifest");
    fs::write(&target, "pub fn phase_six() {}\n").expect("write baseline Rust file");
    commit_git_baseline(workspace.path());

    let scripted = ScriptedOpenRouterCompletion::new(vec![
        live_tool_response(
            "write-invalid-initial",
            "write_file",
            json!({ "path": target.display().to_string(), "content": "pub fn phase_six() {\n" }),
        ),
        live_completion_response("initial implementation complete"),
        live_tool_response(
            "write-invalid-repair",
            "write_file",
            json!({ "path": target.display().to_string(), "content": "pub fn phase_six_repaired() {\n" }),
        ),
        live_completion_response("repair attempted"),
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

    type_text(&mut app, "write invalid Rust and exercise bounded repair");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let approval_count =
        approve_until_turn_ready(&mut app, "repeated validation provider completion", 8);
    assert!(approval_count >= 4, "write, validation, repair, and revalidation need approval");
    wait_until(&mut app, "repeated validation pane evidence", |app| {
        app.agents.threads[0].terminal_evidence.as_ref().is_some_and(|summary| {
            summary.status == TurnTerminalStatus::Blocked
                && summary.blocker == Some(TurnBlocker::ValidationFailed)
        })
    });

    let thread = &app.agents.threads[0];
    assert_eq!(
        app.agents
            .action_log
            .iter()
            .filter(|action| format!("{action:?}").starts_with("Write {"))
            .count(),
        2,
        "initial write plus one repair write must execute"
    );
    assert_eq!(
        scripted.request_bodies().len(),
        4,
        "repair controller must stop before another model loop"
    );
    assert!(
        scripted.request_bodies().iter().any(|body| {
            body["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["content"]
                        .as_str()
                        .is_some_and(|text| text.contains("Repair controller request."))
                })
            })
        }),
        "repair request must use fresh production repair context"
    );
    assert_eq!(
        thread.terminal_evidence.as_ref().expect("failed validation evidence").blocker,
        Some(TurnBlocker::ValidationFailed)
    );

    app.shutdown_agents();
    factory.shutdown();
}

#[test]
fn phase_six_live_openrouter_pane_resume_reuses_completed_write() {
    let _live_lock = phase_six_live_lock();
    let workspace = tempfile::tempdir().expect("fixture workspace");
    let target = workspace.path().join("resume.txt");
    fs::write(workspace.path().join(".ee.toml"), AGENTS_TOML).expect("write agents config");
    fs::write(&target, "before\n").expect("write baseline file");
    commit_git_baseline(workspace.path());

    let write_arguments = json!({ "path": target.display().to_string(), "content": "after\n" });
    let scripted = ScriptedOpenRouterCompletion::pause_then(
        vec![live_tool_response("write-before-pause", "write_file", write_arguments.clone())],
        vec![
            live_tool_response("write-before-pause", "write_file", write_arguments),
            live_completion_response("resumed without repeating write"),
        ],
    );
    let state = tempfile::tempdir().expect("fixture session state");
    let factory = LiveOpenRouterTransport::new(
        openrouter_fixture_config(),
        state.path().join("agent-sessions"),
        scripted.clone(),
    )
    .with_turn_timeout(Duration::from_secs(1))
    .with_auto_resume_max(0);
    let mut app = live_openrouter_app_in(workspace.path(), factory.clone());
    let buffer_id = app.backend.open_buffer(Some(target.clone())).expect("open target buffer");
    app.backend.switch_to_id(buffer_id).expect("focus target buffer");
    open_pane_and_wait_ready(&mut app);
    select_live_write_mode(&mut app);

    type_text(&mut app, "write then interrupt and resume");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "write approval before pause", |app| app.agents.approvals.len() == 1);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "provider checkpointed pause", |app| {
        app.agents.threads[0].state == ThreadUiState::PausedRecoverable
    });
    let evidence_before_resume = app.agents.threads[0]
        .terminal_evidence
        .as_ref()
        .expect("post-write evidence retained before resume")
        .evidence_ids
        .clone();
    assert_eq!(fs::read_to_string(&target).expect("read paused write"), "after\n");
    assert_eq!(
        scripted.request_bodies().len(),
        2,
        "timeout interrupts after checkpointed write and before manual resume"
    );

    type_text(&mut app, "/resume");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "resumed provider completion", |app| {
        app.agents.threads[0].state == ThreadUiState::Ready && scripted.request_bodies().len() == 4
    });

    let thread = &app.agents.threads[0];
    assert!(thread.pending_recovery.is_none());
    assert_eq!(fs::read_to_string(&target).expect("read resumed write"), "after\n");
    assert_eq!(
        app.agents
            .action_log
            .iter()
            .filter(|action| format!("{action:?}").starts_with("Write {"))
            .count(),
        1,
        "resumed duplicate write must reuse completed side effect"
    );
    assert!(
        evidence_before_resume.iter().all(|id| thread
            .terminal_evidence
            .as_ref()
            .is_some_and(|summary| summary.evidence_ids.contains(id))),
        "resume must preserve pre-interruption host evidence"
    );
    let bodies = scripted.request_bodies();
    assert_eq!(bodies.len(), 4, "resume performs only two bounded model rounds");
    assert!(
        bodies[3]["messages"]
            .as_array()
            .expect("final resumed request messages")
            .iter()
            .any(|message| message["role"] == "tool"
                && message["tool_call_id"] == "write-before-pause"),
        "resumed model receives reused completed tool result instead of a duplicate bridge write"
    );

    app.shutdown_agents();
    factory.shutdown();
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
    assert_eq!(
        PhaseSixFixtureMetrics {
            prompt_requests: fake.agent().requests_by_method("session/prompt").len(),
            evidence_ids: app.agents.threads[0]
                .terminal_evidence
                .as_ref()
                .map_or(0, |summary| summary.evidence_ids.len()),
            approvals: app.agents.approvals.len(),
        },
        PhaseSixFixtureMetrics { prompt_requests: 2, evidence_ids: 1, approvals: 0 },
        "resume sends only original and resumed ACP prompt; completed tool replay stays provider-owned"
    );
}

#[test]
fn recoverable_pause_discard_clears_state() {
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
                    "completed_tool_calls": 0,
                    "resumed_count": 0,
                }
            }),
        )
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "hello agent");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "turn paused notice", |app| {
        app.agents.threads[0].state == ThreadUiState::PausedRecoverable
    });

    // `/discard` tells the agent to drop the checkpoint.
    type_text(&mut app, "/discard");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "discard prompt sent", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 2
    });
    wait_until(&mut app, "turn completed after discard", |app| {
        app.agents.threads[0].state == ThreadUiState::Ready
    });
    assert!(app.agents.threads[0].pending_recovery.is_none(), "discard clears the pause");
}

#[test]
fn agents_reconnect_loads_persisted_session_and_replays_conversation() {
    let state_dir = tempfile::tempdir().unwrap();
    let script = FakeAgentScript::new()
        // The restarted agent advertises load (replay) and resume.
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "loadSession": true,
                "sessionCapabilities": { "resume": {} }
            }
        }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
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
                    "completed_tool_calls": 0,
                    "resumed_count": 0,
                }
            }),
        )
        // The simulated restart answers `session/load` by replaying the
        // conversation, then responds (the SDK parses an empty object).
        .wait_for("session/load")
        .emit(wire::session_update("s1", wire::user_message_chunk("hello agent")))
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "first answer")))
        .respond(json!({}));
    let (mut app, _temp, fake) = fake_agents_app(script);
    app.agents.test_session_state_base = Some(state_dir.path().to_path_buf());
    open_pane_and_wait_ready(&mut app);

    // Submit a prompt that pauses: the persisted record now holds the
    // session id and the prompt text (client-persisted for the resend path).
    type_text(&mut app, "hello agent");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "turn paused notice", |app| {
        app.agents.threads[0].state == ThreadUiState::PausedRecoverable
    });

    // Reconnect: `session/load` (preferred over `session/resume`) restores
    // the session and replays the conversation; the existing thread is
    // rebound to the fresh connection.
    type_text(&mut app, "/reconnect");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let deadline = Instant::now() + WAIT;
    loop {
        app.pump_agents();
        let _ = app.backend.drain_events();
        if app.agents.threads.len() == 1
            && app.agents.threads[0].state == ThreadUiState::Ready
            && app.agents.threads[0].transcript.iter().any(|item| {
                matches!(
                    item,
                    TranscriptItem::Message {
                        kind: MessageRenderKind::User,
                        text,
                        ..
                    } if text == "hello agent"
                )
            })
            && app.agents.threads[0].transcript.iter().any(|item| {
                matches!(
                    item,
                    TranscriptItem::Message {
                        kind: MessageRenderKind::Assistant,
                        text,
                        ..
                    } if text == "first answer"
                )
            })
        {
            break;
        }
        if Instant::now() > deadline {
            panic!(
                "reconnect replay never applied; threads={} pending_session={:?} pending_replay={:?} fake_log={:?}",
                app.agents.threads.len(),
                app.agents.pending_session.is_some(),
                app.agents.pending_replay.keys().collect::<Vec<_>>(),
                fake.agent().log(),
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !fake.agent().requests_by_method("session/load").is_empty(),
        "reconnect sends session/load"
    );
    assert!(
        fake.agent().requests_by_method("session/resume").is_empty(),
        "load is preferred over resume"
    );
    // The persisted last prompt is restored for the resend path.
    let restored = app.agents.threads[0].last_prompt.as_ref().and_then(|blocks| {
        blocks.iter().find_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
    });
    assert_eq!(restored.as_deref(), Some("hello agent"));
}

#[test]
fn workspace_restart_restores_all_agent_threads_on_pane_open() {
    let workspace = tempfile::tempdir().unwrap();
    let state_dir = tempfile::tempdir().unwrap();
    let first_script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s2" }));
    let (mut first_app, _first_fake) = fake_agents_app_in(workspace.path(), first_script);
    first_app.agents.test_session_state_base = Some(state_dir.path().to_path_buf());
    open_pane_and_wait_ready(&mut first_app);
    type_text(&mut first_app, "/new_thread");
    press(&mut first_app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut first_app, "second workspace thread ready", |app| {
        app.agents.threads.len() == 2 && app.agents.threads[1].state == ThreadUiState::Ready
    });
    assert_eq!(first_app.agents.active_thread, Some(1));
    first_app.shutdown_agents();
    drop(first_app);

    let restarted_script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": { "loadSession": true }
        }))
        .wait_for("session/load")
        .emit(wire::session_update("s1", wire::agent_message_chunk("s1-message", "first replay")))
        .respond(json!({}))
        .wait_for("session/load")
        .emit(wire::session_update("s2", wire::agent_message_chunk("s2-message", "second replay")))
        .respond(json!({}));
    let (mut restarted_app, restarted_fake) =
        fake_agents_app_in(workspace.path(), restarted_script);
    restarted_app.agents.test_session_state_base = Some(state_dir.path().to_path_buf());

    // Opening a fresh TUI pane restores workspace threads. It must not need
    // `/reconnect`, and it must not create a replacement `session/new`.
    run_ex(&mut restarted_app, "agents");
    wait_until(&mut restarted_app, "workspace threads restored", |app| {
        app.agents.threads.len() == 2
            && app.agents.threads.iter().all(|thread| thread.state == ThreadUiState::Ready)
            && app.agents.threads[0].transcript.iter().any(|item| {
                matches!(item, TranscriptItem::Message { text, .. } if text == "first replay")
            })
            && app.agents.threads[1].transcript.iter().any(|item| {
                matches!(item, TranscriptItem::Message { text, .. } if text == "second replay")
            })
    });

    assert_eq!(
        restarted_app
            .agents
            .threads
            .iter()
            .map(|thread| thread.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["s1", "s2"]
    );
    assert_eq!(restarted_app.agents.active_thread, Some(1));
    let loads = restarted_fake.agent().requests_by_method("session/load");
    assert_eq!(loads.len(), 2);
    assert_eq!(loads[0]["params"]["sessionId"], "s1");
    assert_eq!(loads[1]["params"]["sessionId"], "s2");
    assert!(restarted_fake.agent().requests_by_method("session/new").is_empty());
    assert!(restarted_fake.agent().requests_by_method("session/resume").is_empty());
}

#[test]
fn agents_reconnect_without_persisted_session_reports_error() {
    // A fresh state directory and no session ever created: there is no
    // persisted record to reconnect.
    let state_dir = tempfile::tempdir().unwrap();
    let (mut app, _temp, _fake) = fake_agents_app(base_script());
    app.agents.test_session_state_base = Some(state_dir.path().to_path_buf());

    run_ex(&mut app, "agents_reconnect");
    assert!(
        app.agents
            .error
            .as_deref()
            .is_some_and(|error| error.contains("no persisted agent session"))
    );
}

#[test]
fn no_session_footer_uses_agent_status_background_and_ask_mode() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join(".ee.toml"), "[agents]\nenabled = true\n").unwrap();
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let mut app = App::from_path(None).unwrap();
    drop(_cwd_restore);
    drop(_cwd_lock);

    run_ex(&mut app, "agents");
    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let rendered: Vec<String> = (0..24)
        .map(|y| (0..120).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();
    let footer_y = rendered
        .iter()
        .position(|row| row.contains("agents [no session] | mode:ask"))
        .expect("no-session footer row");

    let footer = &rendered[footer_y];
    assert!(
        (0..120).all(|x| {
            rows.cell((x, footer_y as u16)).unwrap().bg == crate::theme::ui::BG_AGENT_STATUS
        }),
        "no-session footer must use agent-status background: {rendered:#?}"
    );
    assert!(
        !footer.contains('/'),
        "no-session footer must not advertise slash commands: {rendered:#?}"
    );
    assert!(
        !footer.contains("Enter") && !footer.contains("Esc"),
        "no-session footer must not advertise keyboard hints: {rendered:#?}"
    );
}

#[test]
fn footer_defaults_unadvertised_agent_mode_to_ask() {
    let (mut app, _temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);

    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let rendered: Vec<String> = (0..24)
        .map(|y| (0..120).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();
    let composer_row =
        rendered.iter().position(|row| row.contains("prompt>")).expect("composer row");

    assert!(
        rendered[composer_row - 1].contains("mode:ask"),
        "footer must default to ask mode: {rendered:#?}"
    );
}

#[test]
fn footer_renders_current_agent_mode() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({
            "sessionId": "s1",
            "modes": {
                "currentModeId": "plan",
                "availableModes": [
                    { "id": "ask", "name": "Ask" },
                    { "id": "plan", "name": "Plan" }
                ]
            }
        }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let rendered: Vec<String> = (0..24)
        .map(|y| (0..120).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();
    let composer_row =
        rendered.iter().position(|row| row.contains("prompt>")).expect("composer row");
    let footer_row = &rendered[composer_row - 1];

    assert!(
        footer_row.contains("mode:plan"),
        "footer must render current agent mode: {rendered:#?}"
    );
    assert!(
        !footer_row.contains("Ctrl-"),
        "footer must not render keyboard shortcuts: {rendered:#?}"
    );
}

#[test]
fn turn_completed_records_metrics_and_renders_tokens() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "s1",
            json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": "hmm" } }),
        ))
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "hello back")))
        .respond(json!({
            "stopReason": "end_turn",
            "usage": {
                "totalTokens": 8431,
                "inputTokens": 6120,
                "outputTokens": 2311,
            }
        }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "hello agent");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "turn metrics recorded", |app| {
        app.agents.threads[0].last_turn_metrics.is_some()
    });

    let thread = &app.agents.threads[0];
    let metrics = thread.last_turn_metrics.as_ref().expect("metrics recorded");
    let tokens = metrics.tokens.as_ref().expect("reported tokens attached");
    assert_eq!(tokens.total_tokens, 8431);
    assert_eq!(tokens.input_tokens, 6120);
    assert_eq!(tokens.output_tokens, 2311);
    assert!(!thread.turn_metrics.is_empty(), "metrics keyed by response group");
    assert!(thread.turn_started_at.is_none(), "start marker cleared");

    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let rendered: Vec<String> = (0..40)
        .map(|y| (0..120).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();
    let joined = rendered.join("\n");
    assert!(
        joined.contains("8,431 tokens (6,120 in / 2,311 out)"),
        "response header must render turn metrics: {rendered:#?}"
    );
    assert!(
        joined.contains("last:0.0s · 8,431 tokens"),
        "footer must render latest turn metrics: {rendered:#?}"
    );
}

#[test]
fn turn_without_usage_renders_elapsed_only() {
    let script =
        base_script().wait_for("session/prompt").respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "hello agent");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "turn metrics recorded", |app| {
        app.agents.threads[0].last_turn_metrics.is_some()
    });

    let metrics = app.agents.threads[0].last_turn_metrics.as_ref().expect("metrics recorded");
    assert_eq!(metrics.tokens, None, "unknown usage stays unknown, never zero");
    assert_eq!(crate::app::turn_metrics_label(metrics), "0.0s");
}

#[test]
fn usage_update_renders_context_window_right_aligned_above_composer() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "s1",
            json!({ "sessionUpdate": "usage_update", "used": 10_000, "size": 100_000 }),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "hello agent");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "usage update recorded", |app| app.agents.threads[0].usage.is_some());
    assert_eq!(app.agents.threads[0].usage.as_deref(), Some("10k/100k tokens"));

    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    assert!(
        (0..24).all(|y| rows.cell((0, y)).unwrap().symbol() != "│")
            && (0..24).all(|y| rows.cell((119, y)).unwrap().symbol() != "│"),
        "agents pane must not render an outer vertical border"
    );
    let rendered: Vec<String> = (0..24)
        .map(|y| (0..120).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();

    let composer_row =
        rendered.iter().position(|row| row.contains("prompt>")).expect("composer row");
    let footer_row = &rendered[composer_row - 1];
    assert!(
        footer_row.contains("10k/100k tokens"),
        "footer row above the composer must carry the context usage: {rendered:#?}"
    );
    assert!(
        (0..120).all(|x| {
            rows.cell((x, (composer_row - 1) as u16)).unwrap().bg
                == crate::theme::ui::BG_AGENT_STATUS
        }),
        "usage footer must paint its full row background: {rendered:#?}"
    );
    let label = "10k/100k tokens";
    // Byte offsets shift for multi-byte glyphs; compare in character columns.
    let label_end = footer_row
        .match_indices(label)
        .last()
        .map(|(byte_start, _)| footer_row[..byte_start].chars().count() + label.chars().count());
    assert_eq!(
        label_end,
        Some(120),
        "context usage must sit right-aligned at the row end: {rendered:#?}"
    );

    // Narrow panes: the left footer overflows, but the usage label must stay
    // pinned to the rightmost edge (not truncated away).
    let backend = TestBackend::new(60, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let rendered: Vec<String> = (0..24)
        .map(|y| (0..60).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();
    let composer_row =
        rendered.iter().position(|row| row.contains("prompt>")).expect("composer row");
    let footer_row = &rendered[composer_row - 1];
    assert!(
        footer_row.contains("10k/100k tokens"),
        "usage must survive narrow panes: {rendered:#?}"
    );
    let label_end = footer_row
        .match_indices(label)
        .last()
        .map(|(byte_start, _)| footer_row[..byte_start].chars().count() + label.chars().count());
    assert_eq!(
        label_end,
        Some(60),
        "usage must stay right-aligned at the edge in narrow panes: {rendered:#?}"
    );
}

#[test]
fn composer_uses_only_terminal_cursor() {
    let (mut app, _temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);
    type_text(&mut app, "hello");

    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();

    let cursor = terminal.get_cursor_position().unwrap();
    let rows = terminal.backend().buffer();
    let composer_row: String =
        (0..120).map(|x| rows.cell((x, cursor.y)).unwrap().symbol()).collect();
    let composer_start = composer_row.find("prompt> hello").expect("composer contents");
    let composer_start_col = composer_row[..composer_start].chars().count();

    assert!(
        !composer_row.contains('█'),
        "composer must not render a second cursor: {composer_row:?}"
    );
    assert_eq!(
        usize::from(cursor.x),
        composer_start_col + "prompt> hello".len(),
        "terminal cursor must sit directly after composer text"
    );
}

#[test]
fn turn_metrics_label_formats_duration_and_tokens() {
    use ee_agent_host::TurnMetrics;
    use ee_agent_protocol::Usage;
    use std::time::Duration;

    let plain = TurnMetrics { elapsed: Duration::from_millis(12_400), tokens: None };
    assert_eq!(crate::app::turn_metrics_label(&plain), "12.4s");

    let with_tokens = TurnMetrics {
        elapsed: Duration::from_secs(192),
        tokens: Some(Usage::new(8_431, 6_120, 2_311)),
    };
    assert_eq!(
        crate::app::turn_metrics_label(&with_tokens),
        "3m 12s · 8,431 tokens (6,120 in / 2,311 out)"
    );
    assert_eq!(crate::app::format_duration(Duration::from_millis(500)), "0.5s");
}

#[test]
fn separate_user_turns_do_not_concatenate_without_message_ids() {
    let script = base_script()
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }))
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }))
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    for (index, prompt) in ["one", "two", "three"].into_iter().enumerate() {
        type_text(&mut app, prompt);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        wait_until(&mut app, "turn completed", |app| {
            app.agents.threads[0]
                .system_notices()
                .iter()
                .filter(|notice| notice.contains("turn completed"))
                .count()
                > index
        });
    }

    let pairs = app.agents.threads[0].message_pairs();
    assert_eq!(
        pairs,
        vec![
            (String::from("you"), String::from("one")),
            (String::from("you"), String::from("two")),
            (String::from("you"), String::from("three")),
        ]
    );
}

#[test]
fn assistant_chunks_with_reused_message_ids_stay_in_their_turn() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "first reply")))
        .respond(json!({ "stopReason": "end_turn" }))
        .wait_for("session/prompt")
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "second reply")))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "first question");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "first reply", |app| app.agents.threads[0].message_pairs().len() == 2);

    type_text(&mut app, "second question");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "second reply", |app| app.agents.threads[0].message_pairs().len() == 4);

    assert_eq!(
        app.agents.threads[0].message_pairs(),
        vec![
            (String::from("you"), String::from("first question")),
            (String::from("fake"), String::from("first reply")),
            (String::from("you"), String::from("second question")),
            (String::from("fake"), String::from("second reply")),
        ]
    );
}

#[test]
fn streamed_assistant_chunks_render_in_order_with_thoughts() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "hel")))
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "lo")))
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "agent_thought_chunk",
                "content": { "type": "text", "text": "hmm" }
            }),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "hi");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "chunks merged in order", |app| {
        app.agents.threads[0].message_pairs().len() == 3
    });
    assert!(app.agents.show_thoughts, "thought streaming must be visible by default");
    let pairs = app.agents.threads[0].message_pairs();
    assert_eq!(
        pairs,
        vec![
            (String::from("you"), String::from("hi")),
            (String::from("fake"), String::from("hello")),
            (String::from("think"), String::from("hmm")),
        ]
    );

    // Nick-column wrapping stays deterministic for the merged text.
    for (nick, _text) in &pairs {
        assert!(nick.chars().count() <= 10, "nick overflows the nick column: {nick:?}");
    }
    for line in wrap_text("hello", 8) {
        assert!(!line.is_empty());
    }

    let thread = &app.agents.threads[0];
    assert_eq!(thread.response_group_ids(), vec![1]);
    assert_eq!(thread.response_group_counts(1), (1, 0));
    assert_eq!(thread.selected_response_group, Some(1));
    assert!(thread.collapsed_response_groups.contains(&1), "completed turns collapse");

    press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert!(!app.agents.threads[0].collapsed_response_groups.contains(&1));
    press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert!(app.agents.threads[0].collapsed_response_groups.contains(&1));
}

#[test]
fn blank_agent_chunks_do_not_create_transcript_gaps() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update("s1", wire::agent_message_chunk("empty", "")))
        .emit(wire::session_update("s1", wire::agent_message_chunk("visible", "reply")));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "question");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "non-empty reply", |app| app.agents.threads[0].message_pairs().len() == 2);

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let rendered: Vec<String> = (0..20)
        .map(|y| (0..80).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();

    assert!(rendered.iter().any(|row| row.contains("reply")), "reply missing: {rendered:#?}");
    assert!(
        rendered.iter().all(|row| {
            !row.contains("assistant")
                || !row.split_once("assistant").is_some_and(|(_, text)| text.trim().is_empty())
        }),
        "blank assistant chunk rendered as a transcript row: {rendered:#?}"
    );
}

#[test]
fn agents_thoughts_command_toggles_visibility_without_dropping_transcript() {
    let (mut app, _temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);
    assert!(app.agents.show_thoughts);

    type_text(&mut app, "/thoughts off");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(!app.agents.show_thoughts);
    assert_eq!(app.backend.status_message.as_deref(), Some("agent thoughts hidden"));

    app.agents.threads[0].transcript.push(TranscriptItem::Message {
        nick: String::from("think"),
        text: String::from("private summary"),
        kind: crate::app::MessageRenderKind::Thought,
        message_id: Some(String::from("th-1")),
        response_group: Some(1),
        at: std::time::SystemTime::UNIX_EPOCH,
    });
    assert_eq!(
        app.agents.threads[0].message_pairs().len(),
        1,
        "toggle must not drop stored thought transcript"
    );

    type_text(&mut app, "/thoughts toggle");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.agents.show_thoughts);
    assert_eq!(app.backend.status_message.as_deref(), Some("agent thoughts visible"));

    type_text(&mut app, "/thoughts maybe");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.backend.status_message.as_deref(), Some("usage: /thoughts on|off|toggle"));
    assert!(app.agents.show_thoughts, "invalid input must not change visibility");
}

#[test]
fn plan_updates_stay_hidden_until_toggled_and_replace_wholesale_without_scrollback_append() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "plan",
                "entries": [
                    { "content": "first", "priority": "high", "status": "pending" },
                    { "content": "second", "priority": "low", "status": "in_progress" }
                ]
            }),
        ))
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "plan",
                "entries": [
                    { "content": "replacement", "priority": "medium", "status": "completed" }
                ]
            }),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "plan replacement lands", |app| {
        app.agents.threads[0].plan_entries() == vec![(String::from("[medium] replacement"), 'x')]
    });
    assert!(
        !app.agents.threads[0].plan_modal_open,
        "plan updates remain hidden until the user toggles visibility"
    );

    press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    assert!(app.agents.threads[0].plan_modal_open, "Ctrl-G opens plan modal");
    let transcript_debug = format!("{:?}", app.agents.threads[0].transcript);
    assert!(
        !transcript_debug.contains("replacement"),
        "plan content must not append into chat scrollback: {transcript_debug}"
    );

    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(!app.agents.threads[0].plan_modal_open, "Esc closes plan modal");
    assert_eq!(
        app.agents.threads[0].plan_entries(),
        vec![(String::from("[medium] replacement"), 'x')],
        "closing modal keeps latest plan snapshot"
    );
}

#[test]
fn slash_commands_are_discoverable_and_tab_inserts_prompt_text() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "available_commands_update",
                "availableCommands": [
                    { "name": "plan", "description": "Create plan", "input": { "hint": "goal" } },
                    { "name": "edit", "description": "Edit code" }
                ]
            }),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "commands arrive", |app| app.agents.threads[0].command_names().len() == 2);
    assert_eq!(
        app.agents.threads[0].command_names(),
        vec![String::from("plan"), String::from("edit")]
    );
    assert!(
        app.agents.threads[0]
            .system_notices()
            .iter()
            .any(|notice| notice.contains("commands: /plan — Create plan, /edit — Edit code"))
    );

    // Slash-prefixed drafts autocomplete by prefix; once completed, Tab and
    // Shift-Tab cycle through advertised commands.
    type_text(&mut app, "/e");
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].draft, "/edit");
    press(&mut app, KeyCode::BackTab, KeyModifiers::SHIFT);
    assert_eq!(app.agents.threads[0].draft, "/plan");
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].draft, "/edit");
    type_text(&mut app, " file");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "slash prompt sent", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 2
    });
    let prompt = &fake.agent().requests_by_method("session/prompt")[1];
    assert_eq!(prompt["params"]["prompt"][0]["text"], "/edit file");
}

#[test]
fn mode_slash_command_explains_when_agent_advertises_no_modes() {
    let (mut app, _temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/mode");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        app.agents.mode_selection.as_ref().is_some_and(|picker| picker.options.is_empty()),
        "an unsupported mode command must open an explanatory composer"
    );

    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let rendered: Vec<String> = (0..24)
        .map(|y| (0..120).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();
    assert!(
        rendered.iter().any(|row| row.contains("mode unavailable")),
        "unavailable mode heading missing: {rendered:#?}"
    );
    assert!(
        rendered.iter().any(|row| row.contains("did not advertise selectable ACP modes")),
        "unavailable mode reason missing: {rendered:#?}"
    );

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.agents.mode_selection.is_some(), "Enter cannot dismiss unavailable mode state");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.agents.mode_selection.is_none(), "Esc closes unavailable mode state");
}

#[test]
fn mode_slash_command_opens_expanded_composer_picker() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({
            "sessionId": "s1",
            "modes": {
                "currentModeId": "ask",
                "availableModes": [
                    { "id": "ask", "name": "Ask" },
                    { "id": "plan", "name": "Plan" }
                ]
            }
        }))
        .wait_for("session/set_mode")
        .respond(json!({}));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/mode");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let picker = app.agents.mode_selection.as_ref().expect("mode picker opens");
    assert_eq!(picker.options, vec![String::from("ask"), String::from("plan")]);
    assert_eq!(picker.selected, 0, "current mode starts selected");

    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let rendered: Vec<String> = (0..24)
        .map(|y| (0..120).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();
    assert!(
        rendered.iter().any(|row| row.contains("select mode")),
        "mode picker heading missing: {rendered:#?}"
    );
    assert!(
        rendered.iter().any(|row| row.contains("> ask")),
        "current mode must be selected: {rendered:#?}"
    );
    assert!(
        rendered.iter().any(|row| row.contains("  plan")),
        "other advertised modes must be visible: {rendered:#?}"
    );

    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.agents.mode_selection.as_ref().expect("picker remains open").selected, 1);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "mode change accepted", |app| {
        fake.agent().requests_by_method("session/set_mode").len() == 1
            && app.agents.threads[0]
                .host
                .snapshot()
                .current_mode
                .as_ref()
                .is_some_and(|mode| mode.0.as_ref() == "plan")
    });
    assert!(app.agents.mode_selection.is_none(), "picker closes after confirmation");

    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let rendered: Vec<String> = (0..24)
        .map(|y| (0..120).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();
    let composer_row =
        rendered.iter().position(|row| row.contains("prompt>")).expect("composer row");
    assert!(
        rendered[composer_row - 1].contains("mode:plan"),
        "footer must update after /mode plan: {rendered:#?}"
    );
}

#[test]
fn session_info_updates_display_name() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "session_info_update",
                "title": "Audit run",
                "updatedAt": "2026-08-04T12:00:00Z"
            }),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "thread title updates", |app| {
        app.agents.threads[0].display_name == "1.Audit run"
    });
    assert_eq!(app.agents.threads[0].session_title.as_deref(), Some("Audit run"));
    assert_eq!(app.agents.threads[0].session_updated_at.as_deref(), Some("2026-08-04T12:00:00Z"));
}

#[test]
fn agents_config_commands_list_and_mutate_advertised_options() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({
            "sessionId": "s1",
            "configOptions": [
                {
                    "id": "mode",
                    "name": "Mode",
                    "category": "mode",
                    "type": "select",
                    "currentValue": "ask",
                    "options": [
                        { "value": "ask", "name": "Ask" },
                        { "value": "plan", "name": "Plan" }
                    ]
                },
                {
                    "id": "confirmEdits",
                    "name": "Confirm edits",
                    "type": "boolean",
                    "currentValue": false
                }
            ]
        }))
        .wait_for("session/set_config_option")
        .respond(json!({
            "configOptions": [
                {
                    "id": "mode",
                    "name": "Mode",
                    "category": "mode",
                    "type": "select",
                    "currentValue": "ask",
                    "options": [
                        { "value": "ask", "name": "Ask" },
                        { "value": "plan", "name": "Plan" }
                    ]
                },
                {
                    "id": "confirmEdits",
                    "name": "Confirm edits",
                    "type": "boolean",
                    "currentValue": true
                }
            ]
        }))
        .wait_for("session/set_config_option")
        .respond(json!({
            "configOptions": [
                {
                    "id": "mode",
                    "name": "Mode",
                    "category": "mode",
                    "type": "select",
                    "currentValue": "plan",
                    "options": [
                        { "value": "ask", "name": "Ask" },
                        { "value": "plan", "name": "Plan" }
                    ]
                },
                {
                    "id": "confirmEdits",
                    "name": "Confirm edits",
                    "type": "boolean",
                    "currentValue": true
                }
            ]
        }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/config");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let listed = app.backend.status_message.clone().unwrap_or_default();
    assert!(listed.contains("mode=ask"), "status: {listed}");
    assert!(listed.contains("confirmEdits=off"), "status: {listed}");

    type_text(&mut app, "/config toggle confirmEdits");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "boolean config sent", |_| {
        fake.agent().requests_by_method("session/set_config_option").len() == 1
    });
    let toggle = &fake.agent().requests_by_method("session/set_config_option")[0];
    assert_eq!(toggle["params"]["configId"], "confirmEdits");
    assert_eq!(toggle["params"]["type"], "boolean");
    assert_eq!(toggle["params"]["value"], true);

    type_text(&mut app, "/config set mode plan");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "select config sent", |_| {
        fake.agent().requests_by_method("session/set_config_option").len() == 2
    });
    let set_mode = &fake.agent().requests_by_method("session/set_config_option")[1];
    assert_eq!(set_mode["params"]["configId"], "mode");
    assert_eq!(set_mode["params"]["value"], "plan");
}

#[test]
fn provider_features_require_live_advertisement_or_advertised_config() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({
            "sessionId": "s1",
            "configOptions": [
                {
                    "id": "model",
                    "name": "Model",
                    "type": "select",
                    "currentValue": "small",
                    "options": [
                        { "value": "small", "name": "Small" },
                        { "value": "large", "name": "Large" }
                    ]
                },
                {
                    "id": "fast",
                    "name": "Fast mode",
                    "type": "boolean",
                    "currentValue": false
                }
            ]
        }))
        .wait_for("session/set_config_option")
        .respond(json!({}))
        .wait_for("session/set_config_option")
        .respond(json!({}))
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "available_commands_update",
                "availableCommands": [{ "name": "compact", "description": "Provider compaction" }]
            }),
        ))
        .respond(json!({ "stopReason": "end_turn" }))
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/model large");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "advertised model config sent", |_| {
        fake.agent().requests_by_method("session/set_config_option").len() == 1
    });
    let model = &fake.agent().requests_by_method("session/set_config_option")[0];
    assert_eq!(model["params"]["configId"], "model");
    assert_eq!(model["params"]["value"], "large");

    type_text(&mut app, "/fast");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "advertised fast config sent", |_| {
        fake.agent().requests_by_method("session/set_config_option").len() == 2
    });
    let fast = &fake.agent().requests_by_method("session/set_config_option")[1];
    assert_eq!(fast["params"]["configId"], "fast");
    assert_eq!(fast["params"]["value"], true);

    type_text(&mut app, "/effort high");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        app.backend
            .status_message
            .as_deref()
            .is_some_and(|status| status.contains("did not advertise config option effort")),
        "unadvertised config must remain local: {:?}",
        app.backend.status_message
    );
    assert_eq!(fake.agent().requests_by_method("session/set_config_option").len(), 2);

    type_text(&mut app, "/compact focus");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        app.backend
            .status_message
            .as_deref()
            .is_some_and(|status| status.contains("did not advertise it")),
        "unadvertised provider workflow must fail closed: {:?}",
        app.backend.status_message
    );
    assert!(fake.agent().requests_by_method("session/prompt").is_empty());

    type_text(&mut app, "sync commands");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "provider command advertisement", |app| {
        app.agents.threads[0].command_names() == vec![String::from("compact")]
    });

    type_text(&mut app, "/compact focus");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "advertised provider command forwarded", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 2
    });
    let compact = &fake.agent().requests_by_method("session/prompt")[1];
    assert_eq!(compact["params"]["prompt"][0]["text"], "/compact focus");
}

// ── Scrollback behavior ──────────────────────────────────────────────────────

#[test]
fn agents_transcript_bottom_aligns_short_chat() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "answer")))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "question");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "chat lands", |app| app.agents.threads[0].message_pairs().len() >= 2);

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let rendered: Vec<String> = (0..20)
        .map(|y| (0..80).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();

    let first_transcript_row = rendered
        .iter()
        .position(|row| row.contains("session started"))
        .expect("first transcript row");
    let user_row = rendered.iter().position(|row| row.contains("question")).expect("user row");
    let agent_row = rendered.iter().position(|row| row.contains("answer")).expect("agent row");
    assert!(first_transcript_row >= 10, "short chat should sit near composer, rows={rendered:#?}");
    assert!(user_row < agent_row, "messages remain chronological");
}

#[test]
fn agents_transcript_preserves_agent_markdown_newlines() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "s1",
            wire::agent_message_chunk("m1", "first line\nsecond line"),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "question");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "multiline response lands", |app| {
        app.agents.threads[0]
            .message_pairs()
            .iter()
            .any(|(_, text)| text == "first line\nsecond line")
    });

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let rendered: Vec<String> = (0..20)
        .map(|y| (0..80).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();

    let first_row = rendered.iter().position(|row| row.contains("first line")).expect("first row");
    let second_row =
        rendered.iter().position(|row| row.contains("second line")).expect("second row");
    assert!(first_row < second_row, "newlines must render as separate rows: {rendered:#?}");
    assert!(
        rendered.iter().all(|row| !(row.contains("first line") && row.contains("second line"))),
        "newline collapsed onto one row: {rendered:#?}"
    );
    let first_col = rendered[first_row].find("first line").expect("first line column");
    let second_col = rendered[second_row].find("second line").expect("second line column");
    assert_eq!(first_col, second_col, "continuation rows must align: {rendered:#?}");
}

#[test]
fn agent_transcript_discards_blank_rendered_lines() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "s1",
            wire::agent_message_chunk("m1", "first line\n\nsecond line\n\n"),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "question");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "multiline response lands", |app| {
        app.agents.threads[0]
            .message_pairs()
            .iter()
            .any(|(_, text)| text == "first line\n\nsecond line\n\n")
    });

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let rendered: Vec<String> = (0..20)
        .map(|y| (0..80).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();
    let first_row = rendered.iter().position(|row| row.contains("first line")).expect("first row");
    let second_row =
        rendered.iter().position(|row| row.contains("second line")).expect("second row");

    assert_eq!(second_row, first_row + 1, "blank transcript rows must be trimmed: {rendered:#?}");
}

#[test]
fn long_agent_responses_scroll_by_visual_rows() {
    let response = "wrapped response ".repeat(200);
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", &response)))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);
    run_ex(&mut app, "agents_layout full");

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "long assistant response lands", |app| {
        app.agents.threads[0]
            .message_pairs()
            .iter()
            .any(|(nick, text)| nick == "fake" && text == &response)
    });

    let pane = crate::ui::agents_pane_rect_for(
        ratatui::layout::Rect { x: 0, y: 0, width: 40, height: 12 },
        &app,
    )
    .expect("full agents pane");
    let max_scroll = crate::ui::agents_transcript_scroll_max(&app, pane);
    let transcript_item_max = app.agents.threads[0].transcript.len().saturating_sub(1);
    assert!(
        max_scroll > transcript_item_max,
        "long wrapped response needs more visual scroll rows than transcript items"
    );

    let thread = &mut app.agents.threads[0];
    thread.scroll_by(1, max_scroll);
    assert_eq!(thread.scroll, 1, "one scroll step advances one rendered row");
    assert!(!thread.stick_to_bottom, "scrolling up unpins the view");
    thread.scroll_to(usize::MAX, max_scroll);
    assert_eq!(thread.scroll, max_scroll, "scroll stays within visual-row bounds");
    assert!(thread.stick_to_bottom, "last rendered row re-pins the view");
}

#[test]
fn scrollback_pins_to_bottom_until_user_scrolls_up() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "one")))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "assistant message lands", |app| {
        app.agents.threads[0].message_pairs().len() >= 2
    });

    assert!(app.agents.threads[0].stick_to_bottom, "new content must pin to bottom");

    press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
    assert!(!app.agents.threads[0].stick_to_bottom, "scrolling up unpins the view");
    let max_scroll = app.agents.threads[0].transcript.len().saturating_sub(1);
    assert!(
        app.agents.threads[0].scroll <= max_scroll,
        "scroll offset must stay within the transcript"
    );

    press(&mut app, KeyCode::End, KeyModifiers::NONE);
    assert!(app.agents.threads[0].stick_to_bottom, "End re-pins the view");

    press(&mut app, KeyCode::Home, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].scroll, 0);

    press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
    assert!(app.agents.threads[0].stick_to_bottom, "scrolling to the end re-pins");
}

// ── Permission flow ──────────────────────────────────────────────────────────

#[test]
fn permission_prompt_selection_resolves_host_request() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::request_permission(
            "s1",
            "call_1",
            "Run tests",
            json!([
                { "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" },
                { "optionId": "deny", "name": "Deny", "kind": "reject_once" }
            ]),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/approval bypass");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    type_text(&mut app, "run");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "permission prompt appears", |app| app.agents.permission.is_some());
    let permission = app.agents.permission.as_ref().expect("permission present");
    assert_eq!(permission.options.len(), 2);
    assert_eq!(permission.selected, 0);
    assert_eq!(
        app.agents.threads[0].system_notices().iter().find(|n| n.starts_with("approval")),
        None,
        "no approval notice before confirmation"
    );

    press(&mut app, KeyCode::Right, KeyModifiers::NONE);
    assert_eq!(app.agents.permission.as_ref().expect("prompt").selected, 1);

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.agents.permission.is_none(), "prompt clears after confirm");

    wait_until(&mut app, "host answered permission", |_| {
        fake.agent().response_with_id(100).is_some()
    });
    let response = fake.agent().response_with_id(100).expect("permission response");
    assert_eq!(response["result"]["outcome"]["outcome"], "selected");
    assert_eq!(response["result"]["outcome"]["optionId"], "deny");
    wait_until(&mut app, "approval notice lands", |app| {
        app.agents.threads[0]
            .system_notices()
            .iter()
            .any(|notice| notice.contains("approval: Deny (sent)"))
    });
}

// ── Elicitation flow ─────────────────────────────────────────────────────────

fn form_elicitation(id: i64, schema: Value) -> Value {
    form_elicitation_with_message(id, schema, "fill the form")
}

fn form_elicitation_with_message(id: i64, schema: Value, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "elicitation/create",
        "params": {
            "mode": "form",
            "sessionId": "s1",
            "requestedSchema": schema,
            "message": message
        }
    })
}

fn elicitation_complete(elicitation_id: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "elicitation/complete",
        "params": { "elicitationId": elicitation_id }
    })
}

#[test]
fn elicitation_widgets_resolve_form_requests() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(form_elicitation(
            200,
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "title": "Name" },
                    "debug": { "type": "boolean", "title": "Debug" },
                    "level": { "type": "string", "enum": ["low", "high"] }
                },
                "required": ["name"]
            }),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "elicitation prompt appears", |app| app.agents.elicitation.is_some());
    let elicitation = app.agents.elicitation.as_ref().expect("prompt present");
    assert_eq!(elicitation.agent_label, "1.fake");
    assert_eq!(elicitation.message, "fill the form");
    assert_eq!(elicitation.fields.len(), 3);
    assert!(elicitation.unsupported_reason.is_none());
    // Schema properties arrive in sorted order (BTreeMap), so locate fields
    // by name instead of assuming insertion order.
    let field_index = |name: &str| {
        elicitation
            .fields
            .iter()
            .position(|field| field.name == name)
            .unwrap_or_else(|| panic!("missing form field {name:?}"))
    };
    let name_index = field_index("name");
    let debug_index = field_index("debug");
    let level_index = field_index("level");
    assert_eq!(elicitation.fields[name_index].label(), "Name");
    assert_eq!(elicitation.fields[debug_index].label(), "Debug");
    assert_eq!(elicitation.fields[level_index].label(), "level");

    // Drive the form: navigate to each field by Tab (fields cycle), fill it,
    // and accept.
    let mut steps = 0;
    while app.agents.elicitation.as_ref().expect("prompt").selected_field != debug_index {
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        steps += 1;
        assert!(steps < 10, "Tab never reached the boolean field");
    }
    press(&mut app, KeyCode::Right, KeyModifiers::NONE); // toggle debug=true
    while app.agents.elicitation.as_ref().expect("prompt").selected_field != level_index {
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        steps += 1;
        assert!(steps < 10, "Tab never reached the enum field");
    }
    press(&mut app, KeyCode::Right, KeyModifiers::NONE); // low → high
    while app.agents.elicitation.as_ref().expect("prompt").selected_field != name_index {
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        steps += 1;
        assert!(steps < 10, "Tab never reached the name field");
    }
    type_text(&mut app, "ed");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // accept

    wait_until(&mut app, "elicitation answered on the wire", |_| {
        fake.agent().response_with_id(200).is_some()
    });
    let response = fake.agent().response_with_id(200).expect("elicitation response");
    assert_eq!(response["result"]["action"], "accept");
    assert_eq!(response["result"]["content"]["name"], "ed");
    assert_eq!(response["result"]["content"]["debug"], true);
    assert_eq!(response["result"]["content"]["level"], "high");
    assert!(app.agents.elicitation.is_none());
}

#[test]
fn elicitation_rejects_unsupported_schema_visibly_and_declines() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(form_elicitation(
            201,
            json!({
                "type": "object",
                "properties": {
                    "tags": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["a", "b"] }
                    }
                },
                "required": ["tags"]
            }),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "unsupported elicitation appears", |app| app.agents.elicitation.is_some());
    let reason = app
        .agents
        .elicitation
        .as_ref()
        .and_then(|prompt| prompt.unsupported_reason.clone())
        .expect("unsupported reason must be visible");
    assert!(reason.contains("unsupported"), "reason: {reason}");

    // Enter (accept) fails locally and keeps prompt open until user declines/cancels.
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "unsupported elicitation stays local", |app| {
        app.agents.elicitation.is_some()
            && app
                .backend
                .status_message
                .as_deref()
                .is_some_and(|status| status.contains("elicitation blocked locally"))
    });
    assert!(fake.agent().response_with_id(201).is_none());

    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    wait_until(&mut app, "unsupported elicitation declined", |_app| {
        fake.agent().response_with_id(201).is_some()
    });
    let response = fake.agent().response_with_id(201).expect("decline response");
    assert_eq!(response["result"]["action"], "decline");
    assert!(app.agents.elicitation.is_none());
}

#[test]
fn elicitation_rejects_deep_schema_visibly_and_declines() {
    let mut nested = json!("leaf");
    for _ in 0..20 {
        nested = json!({ "child": nested });
    }
    let script = base_script()
        .wait_for("session/prompt")
        .emit(form_elicitation(
            203,
            json!({
                "type": "object",
                "properties": {
                    "deep": {
                        "type": "_future_widget",
                        "payload": nested
                    }
                }
            }),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "deep elicitation appears", |app| app.agents.elicitation.is_some());
    let reason = app
        .agents
        .elicitation
        .as_ref()
        .and_then(|prompt| prompt.unsupported_reason.clone())
        .expect("unsupported reason must be visible");
    assert!(reason.contains("schema depth exceeds"), "reason: {reason}");

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "deep elicitation stays local", |app| {
        app.agents.elicitation.is_some()
            && app
                .backend
                .status_message
                .as_deref()
                .is_some_and(|status| status.contains("elicitation blocked locally"))
    });
    assert!(fake.agent().response_with_id(203).is_none());

    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    wait_until(&mut app, "deep elicitation cancelled", |_app| {
        fake.agent().response_with_id(203).is_some()
    });
    assert_eq!(fake.agent().response_with_id(203).expect("response")["result"]["action"], "cancel");
    assert!(app.agents.elicitation.is_none());
}

#[test]
fn url_elicitation_shows_full_url_host_and_choice() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(json!({
            "jsonrpc": "2.0",
            "id": 202,
            "method": "elicitation/create",
            "params": {
                "mode": "url",
                "sessionId": "s1",
                "elicitationId": "el-1",
                "url": "https://example.com/authorize?client=ee",
                "message": "authorize the agent"
            }
        }))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "url elicitation appears", |app| {
        app.agents.elicitation.as_ref().is_some_and(|prompt| prompt.url.is_some())
    });
    let prompt = app.agents.elicitation.as_ref().expect("prompt");
    let url = prompt.url.clone().expect("url");
    assert_eq!(prompt.url_host.as_deref(), Some("example.com"));
    assert!(url.contains("https://example.com/authorize"), "full url shown: {url}");
    assert!(app.agents.threads[0].transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::Elicitation {
            agent,
            message,
            url: Some(url),
            url_host: Some(host),
            ..
        } if agent == "1.fake"
            && message == "authorize the agent"
            && host == "example.com"
            && url.contains("https://example.com/authorize?client=ee")
    )));

    // Left/Right cycles accept/decline/cancel; Ctrl-D declines without opening.
    press(&mut app, KeyCode::Left, KeyModifiers::NONE);
    assert_eq!(app.agents.elicitation.as_ref().expect("prompt").selected_choice, 2);
    press(&mut app, KeyCode::Right, KeyModifiers::NONE);
    assert_eq!(app.agents.elicitation.as_ref().expect("prompt").selected_choice, 0);
    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    wait_until(&mut app, "url elicitation declined", |_| {
        fake.agent().response_with_id(202).is_some()
    });
    assert_eq!(
        fake.agent().response_with_id(202).expect("response")["result"]["action"],
        "decline"
    );
}

#[test]
fn url_elicitation_completion_clears_prompt_and_marks_complete() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(json!({
            "jsonrpc": "2.0",
            "id": 202,
            "method": "elicitation/create",
            "params": {
                "mode": "url",
                "sessionId": "s1",
                "elicitationId": "el-1",
                "url": "https://example.com/authorize?client=ee",
                "message": "authorize the agent"
            }
        }))
        .delay(50)
        .emit(elicitation_complete("el-1"))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "url elicitation completion handled", |app| {
        app.agents.elicitation.is_none() && fake.agent().response_with_id(202).is_some()
    });
    let response = fake.agent().response_with_id(202).expect("completion response");
    assert_eq!(response["result"]["action"], "accept");
    wait_until(&mut app, "completion notice lands", |app| {
        app.agents.threads[0]
            .system_notices()
            .iter()
            .any(|notice| notice.contains("elicitation completed: el-1"))
    });
}

#[test]
fn stale_url_elicitation_completion_is_ignored_without_clearing_prompt() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(json!({
            "jsonrpc": "2.0",
            "id": 202,
            "method": "elicitation/create",
            "params": {
                "mode": "url",
                "sessionId": "s1",
                "elicitationId": "el-1",
                "url": "https://example.com/authorize?client=ee",
                "message": "authorize the agent"
            }
        }))
        .delay(50)
        .emit(elicitation_complete("el-stale"))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "url elicitation remains open", |app| {
        app.agents.elicitation.as_ref().is_some_and(|prompt| prompt.url.is_some())
    });
    std::thread::sleep(Duration::from_millis(100));
    app.pump_agents();
    assert!(
        fake.agent().response_with_id(202).is_none(),
        "stale completion must stay diagnostics-only and not answer request"
    );

    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    wait_until(&mut app, "url elicitation declined after stale completion", |_| {
        fake.agent().response_with_id(202).is_some()
    });
    assert_eq!(fake.agent().response_with_id(202).expect("response")["result"]["action"], "cancel");
}

#[test]
fn secret_like_elicitation_requests_are_blocked_locally() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(form_elicitation_with_message(
            204,
            json!({
                "type": "object",
                "properties": {
                    "api_key": { "type": "string", "title": "API key" }
                },
                "required": ["api_key"]
            }),
            "enter your password",
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "secretive elicitation appears", |app| app.agents.elicitation.is_some());
    let reason = app
        .agents
        .elicitation
        .as_ref()
        .and_then(|prompt| prompt.unsupported_reason.clone())
        .expect("blocked reason visible");
    assert!(reason.contains("secret-like elicitation requests are blocked"), "reason: {reason}");

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "blocked elicitation remains local", |app| {
        app.agents.elicitation.is_some()
            && app
                .backend
                .status_message
                .as_deref()
                .is_some_and(|status| status.contains("elicitation blocked locally"))
    });
    assert!(fake.agent().response_with_id(204).is_none());

    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    wait_until(&mut app, "blocked elicitation declined", |_| {
        fake.agent().response_with_id(204).is_some()
    });
    assert_eq!(
        fake.agent().response_with_id(204).expect("response")["result"]["action"],
        "decline"
    );
}

#[test]
fn elicitation_validation_failure_stays_local_until_user_resolves_it() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(form_elicitation(
            205,
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "title": "Name" }
                },
                "required": ["name"]
            }),
        ))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "required-field elicitation appears", |app| {
        app.agents.elicitation.is_some()
    });
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "validation failure stays local", |app| {
        app.agents.elicitation.is_some()
            && app
                .backend
                .status_message
                .as_deref()
                .is_some_and(|status| status.contains("required field missing"))
    });
    assert!(fake.agent().response_with_id(205).is_none());

    type_text(&mut app, "ed");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "validation resolved", |_| fake.agent().response_with_id(205).is_some());
    assert_eq!(fake.agent().response_with_id(205).expect("response")["result"]["action"], "accept");
    assert_eq!(
        fake.agent().response_with_id(205).expect("response")["result"]["content"]["name"],
        "ed"
    );
}

#[test]
fn tool_call_details_stay_collapsed_until_toggled_during_active_turn() {
    let script = base_script()
        .wait_for("session/prompt")
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call_1",
                "title": "Run tests",
                "kind": "execute",
                "status": "pending",
                "rawInput": { "token": "super-secret" }
            }),
        ))
        .emit(wire::session_update(
            "s1",
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_1",
                "status": "completed",
                "content": [
                    { "type": "content", "content": { "type": "text", "text": "cargo test --quiet" } },
                    { "type": "diff", "path": "/tmp/src/lib.rs", "newText": "fn main() {}" },
                    { "type": "terminal", "terminalId": "term-1" }
                ],
                "locations": [
                    { "path": "/tmp/src/lib.rs", "line": 7 },
                    { "path": "/tmp/tests/lib.rs" }
                ],
                "rawOutput": { "password": "nope" }
            }),
        ))
        .wait_for("session/cancel");
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "go");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "tool call rendered", |app| {
        app.agents.threads[0].transcript.iter().any(|item| {
            matches!(
                item,
                TranscriptItem::ToolCall { status, detail, .. }
                    if status == "completed"
                        && detail.contains("kind: execute")
                        && detail.contains("content: cargo test --quiet")
                        && detail.contains("diff: new file /tmp/src/lib.rs")
                        && detail.contains("terminal: term-1")
                        && detail.contains("locations: /tmp/src/lib.rs:7, /tmp/tests/lib.rs")
                        && detail.contains("diagnostics: raw input/output captured")
                        && !detail.contains("super-secret")
                        && !detail.contains("nope")
            )
        })
    });
    let thread = &app.agents.threads[0];
    assert_eq!(thread.response_group_ids(), vec![1]);
    assert_eq!(thread.response_group_counts(1), (0, 1));
    assert_eq!(thread.selected_response_group, Some(1));
    assert_eq!(thread.state, ThreadUiState::Running);
    assert!(!thread.expanded_tool_details.contains(&1));

    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let collapsed: Vec<String> = (0..24)
        .map(|y| (0..120).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();
    assert!(collapsed.iter().any(|row| row.contains("Run tests [completed]")));
    assert!(
        !collapsed.iter().any(|row| row.contains("content: cargo test --quiet")),
        "tool detail must stay hidden while the turn is active: {collapsed:#?}"
    );

    press(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL);
    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let expanded: Vec<String> = (0..24)
        .map(|y| (0..120).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();
    assert!(
        expanded.iter().any(|row| row.contains("content: cargo test --quiet")),
        "expanded tool detail must render: {expanded:#?}"
    );

    press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let rows = terminal.backend().buffer();
    let response_collapsed: Vec<String> = (0..24)
        .map(|y| (0..120).map(|x| rows.cell((x, y)).unwrap().symbol()).collect::<String>())
        .collect();
    assert!(
        !response_collapsed.iter().any(|row| row.contains("Run tests [completed]")),
        "collapsed response must hide nested tool rows: {response_collapsed:#?}"
    );

    let export_base = tempfile::tempdir().unwrap();
    app.agents.test_export_base = Some(export_base.path().to_path_buf());
    type_text(&mut app, "/export");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    let export_dir = export_base.path().join("agent-exports");
    let export_path = fs::read_dir(&export_dir)
        .expect("export directory")
        .next()
        .expect("export file")
        .unwrap()
        .path();
    let exported = fs::read_to_string(&export_path).expect("exported transcript");
    assert!(exported.contains("# Agent session transcript"));
    assert!(exported.contains("Tool: Run tests"));
    assert!(exported.contains("#### Input"));
    assert!(exported.contains("#### Output"));
    assert!(exported.contains("\"token\": \"***\""));
    assert!(exported.contains("\"password\": \"***\""));
    assert!(!exported.contains("super-secret"));
    assert!(!exported.contains("nope"));
    assert!(exported.find("User (you)") < exported.find("Tool: Run tests"));
    assert!(app.agents.threads[0].draft.is_empty());
    assert!(
        app.backend.status_message.as_deref().is_some_and(|status| status.contains("exported"))
    );
}

// ── Stop, close, clear ───────────────────────────────────────────────────────

#[test]
fn agents_stop_cancels_running_turn_and_updates_status() {
    let script = base_script().wait_for("session/prompt").wait_for("session/cancel");
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "long task");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "turn running", |app| {
        app.agents.threads[0].state == ThreadUiState::Running
    });

    run_ex(&mut app, "agents_stop");
    assert_eq!(app.backend.status_message.as_deref(), Some("cancelling turn…"));

    wait_until(&mut app, "cancel reply lands", |app| {
        app.backend.status_message.as_deref() == Some("turn cancelled")
    });
    wait_until(&mut app, "turn cancelled notice", |app| {
        app.agents.threads[0].system_notices().iter().any(|notice| notice == "turn cancelled")
    });
    assert_eq!(app.agents.threads[0].state, ThreadUiState::Ready);
}

#[test]
fn steer_prioritizes_message_and_queue_runs_follow_up_after_turn_finishes() {
    let script = base_script()
        .wait_for("session/prompt")
        .wait_for("session/cancel")
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }))
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "original task");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "turn running", |app| {
        app.agents.threads[0].state == ThreadUiState::Running
    });
    wait_until(&mut app, "original prompt sent", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 1
    });

    type_text(&mut app, "/queue follow up after steer");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    type_text(&mut app, "/steer use additional user context");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].queued_prompts.len(), 2);
    assert_eq!(app.agents.threads[0].queued_prompts[0].text, "use additional user context");
    assert_eq!(app.agents.threads[0].queued_prompts[1].text, "follow up after steer");

    wait_until(&mut app, "steered and queued prompts complete", |app| {
        app.agents.threads[0].state == ThreadUiState::Ready
            && fake.agent().requests_by_method("session/prompt").len() == 3
    });
    let prompts = fake.agent().requests_by_method("session/prompt");
    assert_eq!(prompts[0]["params"]["prompt"][0]["text"], "original task");
    assert_eq!(prompts[1]["params"]["prompt"][0]["text"], "use additional user context");
    assert_eq!(prompts[2]["params"]["prompt"][0]["text"], "follow up after steer");
}

#[test]
fn closing_pane_preserves_thread_state_and_session() {
    let (mut app, _temp, fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);
    let session_count_before_close = app.agents.threads.len();

    run_ex(&mut app, "agents_close");
    assert_eq!(app.agents.layout, AgentPaneLayout::Closed);
    assert_eq!(app.mode, Mode::Normal, "focus returns to the editor");
    assert_eq!(app.agents.threads.len(), session_count_before_close, "thread survives close");

    // Reopening reuses the running session: no second session/new.
    run_ex(&mut app, "agents");
    wait_until(&mut app, "pane reopened", |app| app.agents_focused());
    assert_eq!(fake.agent().requests_by_method("session/new").len(), 1);
    assert_eq!(app.agents.threads.len(), 1);
}

#[test]
fn esc_and_ctrl_c_do_not_close_or_blur_agents_pane() {
    let (mut app, _temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);

    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.agents.layout, AgentPaneLayout::Full);
    assert_eq!(app.mode, Mode::Agent);

    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(app.agents.layout, AgentPaneLayout::Full);
    assert_eq!(app.mode, Mode::Agent);
}

#[test]
fn quit_slash_command_closes_agents_pane_locally() {
    let (mut app, _temp, fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/quit");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert_eq!(app.agents.layout, AgentPaneLayout::Closed);
    assert_eq!(app.mode, Mode::Normal);
    assert!(app.agents.threads[0].draft.is_empty());
    assert!(fake.agent().requests_by_method("session/prompt").is_empty());
}

#[test]
fn quit_full_slash_command_exits_editor_locally() {
    let (mut app, _temp, fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/quit_full");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert!(app.should_quit);
    assert_eq!(app.agents.layout, AgentPaneLayout::Full);
    assert_eq!(app.mode, Mode::Agent);
    assert!(app.agents.threads[0].draft.is_empty());
    assert!(fake.agent().requests_by_method("session/prompt").is_empty());
}

#[test]
fn new_thread_slash_command_starts_and_focuses_thread_locally() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s2" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/new_thread");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "new-thread slash-command thread ready", |app| {
        app.agents.threads.len() == 2 && app.agents.threads[1].state == ThreadUiState::Ready
    });
    assert_eq!(app.agents.active_thread, Some(1));
    assert!(app.agents.threads[0].draft.is_empty());
    assert!(app.agents.threads[1].draft.is_empty());
    assert_eq!(fake.agent().requests_by_method("session/new").len(), 2);
    assert!(fake.agent().requests_by_method("session/prompt").is_empty());
}

#[test]
fn new_slash_command_with_arguments_is_sent_to_agent() {
    let script =
        base_script().wait_for("session/prompt").respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/new project context");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    wait_until(&mut app, "slash prompt sent", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 1
    });
    let prompt = &fake.agent().requests_by_method("session/prompt")[0];
    assert_eq!(prompt["params"]["prompt"][0]["text"], "/new project context");
    assert_eq!(fake.agent().requests_by_method("session/new").len(), 1);
}

#[test]
fn new_thread_slash_command_rejects_second_request_while_session_starts() {
    let script = base_script().wait_for("session/new");
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/new_thread");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "new-thread session request pending", |_| {
        fake.agent().requests_by_method("session/new").len() == 2
    });

    type_text(&mut app, "/new_thread");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert_eq!(app.backend.status_message.as_deref(), Some("agent session is already starting"));
    assert_eq!(fake.agent().requests_by_method("session/new").len(), 2);
}

#[test]
fn agents_clear_wipes_scrollback_only_when_idle() {
    let script = base_script().wait_for("session/prompt");
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    // No turn running: clearing works.
    run_ex(&mut app, "agents_clear");
    assert!(app.agents.threads[0].transcript.is_empty());
    assert_eq!(
        app.backend.status_message.as_deref(),
        Some("visible scrollback cleared; provider conversation remains intact")
    );

    // While a turn is running the clear is refused.
    type_text(&mut app, "keep");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "turn running", |app| {
        app.agents.threads[0].state == ThreadUiState::Running
    });
    run_ex(&mut app, "agents_clear");
    assert_eq!(
        app.backend.status_message.as_deref(),
        Some("cannot clear scrollback while a turn is running")
    );
    assert!(!app.agents.threads[0].transcript.is_empty());
}

// ── Thread switching ─────────────────────────────────────────────────────────

#[test]
fn agents_threads_opens_picker_and_focuses_selected_session() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s2" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);
    run_ex(&mut app, "agents_new");
    wait_until(&mut app, "second thread ready", |app| {
        app.agents.threads.len() == 2 && app.agents.threads[1].state == ThreadUiState::Ready
    });
    assert_eq!(app.agents.active_thread, Some(1));

    run_ex(&mut app, "agents_threads");
    let picker = app.picker.as_ref().expect("agent thread picker should open");
    assert_eq!(picker.kind, crate::picker::PickerKind::AgentThreads);
    assert_eq!(picker.title, "Agent Sessions");
    assert_eq!(picker.visible_count(), 2);
    assert_eq!(picker.selected, 1, "active thread preselected");

    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.active_thread, Some(0));
    assert_eq!(app.mode, Mode::Agent);
    assert!(app.picker.is_none(), "picker closes after confirm");
}

#[test]
fn sessions_slash_command_opens_agent_thread_picker_locally() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s2" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);
    run_ex(&mut app, "agents_new");
    wait_until(&mut app, "second thread ready", |app| {
        app.agents.threads.len() == 2 && app.agents.threads[1].state == ThreadUiState::Ready
    });

    type_text(&mut app, "/sessions");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    let picker = app.picker.as_ref().expect("/sessions should open agent thread picker");
    assert_eq!(picker.kind, crate::picker::PickerKind::AgentThreads);
    assert_eq!(picker.title, "Agent Sessions");
    assert_eq!(picker.selected, 1, "active thread preselected");
    assert!(fake.agent().requests_by_method("session/prompt").is_empty());
}

#[test]
fn ctrl_t_opens_agent_thread_picker() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s2" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);
    run_ex(&mut app, "agents_new");
    wait_until(&mut app, "second thread ready", |app| {
        app.agents.threads.len() == 2 && app.agents.threads[1].state == ThreadUiState::Ready
    });

    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);

    let picker = app.picker.as_ref().expect("ctrl-t should open agent thread picker");
    assert_eq!(picker.kind, crate::picker::PickerKind::AgentThreads);
    assert_eq!(picker.selected, 1);
}

#[test]
fn thread_switching_preserves_drafts_scroll_unread_and_activity() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s2" }))
        // After s2 is fully registered, a prompt on s2 lets the fake emit
        // content for s1 while s2 is the focused thread (deterministic
        // unread bump).
        .wait_for("session/prompt")
        .emit(wire::session_update("s1", wire::agent_message_chunk("m1", "ping")))
        .respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "first draft");
    assert_eq!(app.agents.threads[0].draft, "first draft");

    run_ex(&mut app, "agents_new");
    wait_until(&mut app, "second thread ready", |app| {
        app.agents.threads.len() == 2 && app.agents.threads[1].state == ThreadUiState::Ready
    });
    assert_eq!(app.agents.active_thread, Some(1));
    assert_eq!(app.agents.threads[1].draft, "", "new thread starts with an empty draft");

    type_text(&mut app, "second draft");
    assert_eq!(app.agents.threads[1].draft, "second draft");

    // Submitting on s2 lets the fake stream s1 content while s2 is focused.
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    // s1 had unread content from the emit while s2 was focused.
    wait_until(&mut app, "unread bumped on inactive thread", |app| {
        app.agents.threads[0].unread > 0 && app.agents.threads[0].activity
    });
    let unread_before_focus = app.agents.threads[0].unread;
    assert!(unread_before_focus > 0);

    // Switch back to s1: draft preserved, unread reset by focus.
    run_ex(&mut app, "agents_prev");
    assert_eq!(app.agents.active_thread, Some(0));
    assert_eq!(app.agents.threads[0].draft, "first draft");
    assert_eq!(app.agents.threads[0].unread, 0, "focusing resets unread");

    // Scroll offset is per-thread and survives switching.
    press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
    let s1_scroll = app.agents.threads[0].scroll;
    assert!(!app.agents.threads[0].stick_to_bottom);
    run_ex(&mut app, "agents_next");
    run_ex(&mut app, "agents_prev");
    assert_eq!(app.agents.threads[0].scroll, s1_scroll);
    assert!(!app.agents.threads[0].stick_to_bottom);

    // Next wraps around to the first thread.
    run_ex(&mut app, "agents_next");
    assert_eq!(app.agents.active_thread, Some(1));
}

// ── Layout command ───────────────────────────────────────────────────────────

#[test]
fn agents_layout_changes_split_and_opens_pane() {
    let (mut app, _temp, _fake) = fake_agents_app(base_script());

    run_ex(&mut app, "agents_layout bottom");
    assert_eq!(app.agents.layout, AgentPaneLayout::Bottom);
    assert_eq!(app.mode, Mode::Agent);
    wait_until(&mut app, "thread ready after layout open", |app| {
        app.agents.threads.len() == 1 && app.agents.threads[0].state == ThreadUiState::Ready
    });

    run_ex(&mut app, "agents_layout full");
    assert_eq!(app.agents.layout, AgentPaneLayout::Full);

    run_ex(&mut app, "agents_layout left");
    assert_eq!(
        app.backend.status_message.as_deref(),
        Some("usage: :agents_layout right|bottom|full")
    );
    assert_eq!(app.agents.layout, AgentPaneLayout::Full, "invalid argument leaves layout");
}

#[test]
fn phase_five_init_and_doctor_stay_local_owned_and_safe() {
    let script =
        base_script().wait_for("session/prompt").respond(json!({ "stopReason": "end_turn" }));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    type_text(&mut app, "/doctor");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let doctor = app.agents.threads[0].system_notices().join("\n");
    assert!(doctor.contains("Agents TUI doctor (read-only)"));
    assert!(doctor.contains("feature: agents mode enabled"));
    assert!(doctor.contains("configured agent command: fake: unused"));
    assert!(doctor.contains("redaction:"));
    assert!(fake.agent().requests_by_method("session/prompt").is_empty());

    type_text(&mut app, "/init");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "init workflow prompt sent", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 1
    });
    let prompt = &fake.agent().requests_by_method("session/prompt")[0]["params"]["prompt"];
    let text = prompt[0]["text"].as_str().expect("init workflow text");
    assert!(text.contains("ee_project_instructions"));
    assert!(text.contains("ee_create_text_file"));
    assert!(text.contains("do not overwrite"));
    assert!(text.contains("normal file-write approval"));
    let transcript = app.agents.threads[0].message_pairs();
    assert!(transcript.iter().any(|(_, text)| text.contains("EE local /init request sent")));
    assert!(!transcript.iter().any(|(_, text)| text.contains("ee_create_text_file")));
}
