//! Agents pane regression tests (Phase 3, feature `agents`).
//!
//! End-to-end through the real `ee-agent-host` stack: the pane starts the
//! host lazily, the host connects over an in-process fake ACP agent, and
//! every assertion goes through `App::pump_agents` so the event pipeline is
//! exercised exactly as in the TUI loop.  No external binaries are spawned.

use std::fs;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ee_agent_host::FakeTransportFactory;
use ee_agent_host::fake::{FakeAgent, FakeAgentScript, FakeAgentTransport, wire};
use ee_agent_protocol::ContentBlock;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};

use crate::app::{
    AgentPaneLayout, App, MessageRenderKind, Mode, ThreadUiState, TranscriptItem, wrap_text,
};
use crate::tests::helpers::*;
use crate::ui::ui;

const WAIT: Duration = Duration::from_secs(5);

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

/// Builds an `App` with agents enabled in a temp workspace and installs the
/// fake agent for the `fake` server id.
fn fake_agents_app(script: FakeAgentScript) -> (App, tempfile::TempDir, ScriptedFake) {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join(".ee.toml"), AGENTS_TOML).unwrap();
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let mut app = App::from_path(None).unwrap();
    drop(_cwd_restore);
    drop(_cwd_lock);
    let fake = ScriptedFake::new(script);
    app.agents.test_fake_transports.insert(String::from("fake"), Arc::new(fake.clone()));
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
fn recoverable_pause_offers_resume_and_resume_resends_prompt() {
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
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

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
    // Typing a new prompt while paused is rejected.
    type_text(&mut app, "new question");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].state, ThreadUiState::PausedRecoverable);
    assert!(app.agents.error.as_deref().is_some_and(|error| error.contains("resume")));

    // Resume re-sends the original prompt and the turn completes.
    run_ex(&mut app, "agents_resume");
    wait_until(&mut app, "resumed prompt sent", |_| {
        fake.agent().requests_by_method("session/prompt").len() == 2
    });
    wait_until(&mut app, "turn completed after resume", |app| {
        app.agents.threads[0].state == ThreadUiState::Ready
            && app.agents.threads[0].system_notices().iter().any(|n| n.contains("turn completed"))
    });
    assert!(app.agents.threads[0].pending_recovery.is_none(), "resume clears the pause");
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

    // Discard tells the agent to drop the checkpoint (`/discard` prompt).
    run_ex(&mut app, "agents_discard");
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
    run_ex(&mut app, "agents_reconnect");
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
    let label = "10k/100k tokens";
    // Byte offsets shift for multi-byte glyphs (←/→ borders); compare in
    // character columns.
    let label_end = footer_row
        .match_indices(label)
        .last()
        .map(|(byte_start, _)| footer_row[..byte_start].chars().count() + label.chars().count());
    assert_eq!(
        label_end,
        Some(119),
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
        Some(59),
        "usage must stay right-aligned at the edge in narrow panes: {rendered:#?}"
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
fn agents_thoughts_command_toggles_visibility_without_dropping_transcript() {
    let (mut app, _temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);
    assert!(app.agents.show_thoughts);

    run_ex(&mut app, "agents_thoughts off");
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

    run_ex(&mut app, "agents_thoughts toggle");
    assert!(app.agents.show_thoughts);
    assert_eq!(app.backend.status_message.as_deref(), Some("agent thoughts visible"));

    run_ex(&mut app, "agents_thoughts maybe");
    assert_eq!(
        app.backend.status_message.as_deref(),
        Some("usage: :agents_thoughts on|off|toggle")
    );
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

    run_ex(&mut app, "agents_config");
    let listed = app.backend.status_message.clone().unwrap_or_default();
    assert!(listed.contains("mode=ask"), "status: {listed}");
    assert!(listed.contains("confirmEdits=off"), "status: {listed}");

    run_ex(&mut app, "agents_config_toggle confirmEdits");
    wait_until(&mut app, "boolean config sent", |_| {
        fake.agent().requests_by_method("session/set_config_option").len() == 1
    });
    let toggle = &fake.agent().requests_by_method("session/set_config_option")[0];
    assert_eq!(toggle["params"]["configId"], "confirmEdits");
    assert_eq!(toggle["params"]["type"], "boolean");
    assert_eq!(toggle["params"]["value"], true);

    run_ex(&mut app, "agents_config_set mode plan");
    wait_until(&mut app, "select config sent", |_| {
        fake.agent().requests_by_method("session/set_config_option").len() == 2
    });
    let set_mode = &fake.agent().requests_by_method("session/set_config_option")[1];
    assert_eq!(set_mode["params"]["configId"], "mode");
    assert_eq!(set_mode["params"]["value"], "plan");
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
fn tool_call_updates_render_kind_content_locations_and_secret_conscious_diagnostics() {
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
        .respond(json!({ "stopReason": "end_turn" }));
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
    assert!(thread.collapsed_response_groups.contains(&1), "completed turns collapse");
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
    assert_eq!(app.backend.status_message.as_deref(), Some("agents scrollback cleared"));

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
