//! Agents pane regression tests (feature `agents`).
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
use ee_agent_host::fake::{CaptureSource, FakeAgent, FakeAgentScript, FakeAgentTransport, wire};
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

mod flaky_regressions;

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

/// Standard initialize + session/new happy path. Mode-less agents must accept
/// the TUI's explicit safe default before the session becomes ready.
fn base_script() -> FakeAgentScript {
    FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
        .wait_for("session/set_mode")
        .respond(json!({}))
}

const AGENTS_TOML: &str = r#"
root = true

[agents]
enabled = true
default_agent = "fake"

[agents.servers.fake]
command = "unused"
"#;

const TWO_AGENTS_TOML: &str = r#"
root = true

[agents]
enabled = true

[agents.servers.alpha]
label = "Alpha Agent"
command = "unused-alpha"

[agents.servers.beta]
label = "Beta Agent"
command = "unused-beta"
"#;

fn openrouter_fixture_config() -> OpenRouterConfig {
    OpenRouterConfig {
        model: String::from("test/model"),
        model_family: Some(String::from("other:test-root")),
        rubber_duck_model: None,
        rubber_duck_model_family: None,
        rubber_duck: ee_agent_orchestrator::RubberDuckConfig::default(),
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

fn two_fake_agents_app() -> (App, tempfile::TempDir, ScriptedFake, ScriptedFake) {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join(".ee.toml"), TWO_AGENTS_TOML).unwrap();
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let mut app = App::from_path(None).unwrap();
    drop(_cwd_restore);
    drop(_cwd_lock);

    let alpha = ScriptedFake::new(
        FakeAgentScript::new()
            .wait_for("initialize")
            .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
            .wait_for("session/new")
            .respond(json!({ "sessionId": "alpha-session" }))
            .wait_for("session/set_mode")
            .respond(json!({})),
    );
    let beta = ScriptedFake::new(
        FakeAgentScript::new()
            .wait_for("initialize")
            .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
            .wait_for("session/new")
            .respond(json!({ "sessionId": "beta-session" }))
            .wait_for("session/set_mode")
            .respond(json!({})),
    );
    app.agents.test_fake_transports.insert(String::from("alpha"), Arc::new(alpha.clone()));
    app.agents.test_fake_transports.insert(String::from("beta"), Arc::new(beta.clone()));
    (app, temp, alpha, beta)
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

mod footer_tests;
mod mode_config_tests;
mod pane_tests;
mod permissions_tests;
mod phase_six_tests;
mod reconnect_tests;
mod render_tests;
mod slash_tests;
mod thread_tests;
mod transcript_tests;

fn two_session_script() -> FakeAgentScript {
    FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
        .wait_for("session/set_mode")
        .respond(json!({}))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s2" }))
        .wait_for("session/set_mode")
        .respond(json!({}))
}

fn permission_request(id: i64, session_id: &str, tool_call_id: &str, title: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/request_permission",
        "params": {
            "sessionId": session_id,
            "toolCall": { "toolCallId": tool_call_id, "title": title },
            "options": [
                { "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" },
                { "optionId": "deny", "name": "Deny", "kind": "reject_once" }
            ]
        }
    })
}

fn open_second_thread(app: &mut App) {
    type_text(app, "/new_thread");
    press(app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(app, "second session ready", |app| {
        app.agents.threads.len() == 2 && app.agents.threads[1].state == ThreadUiState::Ready
    });
}

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
