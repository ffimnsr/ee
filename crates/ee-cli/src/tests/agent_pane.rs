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
use serde_json::{Value, json};

use crate::app::{AgentPaneLayout, App, Mode, ThreadUiState, wrap_text};
use crate::tests::helpers::*;

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
    let mut app = App::from_path(None).unwrap();

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

    assert_eq!(app.agents.layout, AgentPaneLayout::Right);
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
    let pairs = app.agents.threads[0].message_pairs();
    assert_eq!(
        pairs,
        vec![
            (String::from("you"), String::from("hi")),
            (String::from("fake"), String::from("hello")),
            (String::from("thought"), String::from("hmm")),
        ]
    );

    // Nick-column wrapping stays deterministic for the merged text.
    for (nick, _text) in &pairs {
        assert!(nick.chars().count() <= 10, "nick overflows the nick column: {nick:?}");
    }
    for line in wrap_text("hello", 8) {
        assert!(!line.is_empty());
    }
}

// ── Scrollback behavior ──────────────────────────────────────────────────────

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
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "elicitation/create",
        "params": {
            "mode": "form",
            "sessionId": "s1",
            "requestedSchema": schema,
            "message": "fill the form"
        }
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

    // Enter (accept) with an unsupported form declines safely.
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "unsupported elicitation declined", |_app| {
        fake.agent().response_with_id(201).is_some()
    });
    let response = fake.agent().response_with_id(201).expect("decline response");
    assert!(response.get("error").is_some(), "unsupported forms fail closed: {response}");
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
    wait_until(&mut app, "deep elicitation declined", |_app| {
        fake.agent().response_with_id(203).is_some()
    });
    assert!(fake.agent().response_with_id(203).expect("response").get("error").is_some());
    assert!(app.agents.elicitation.is_none());
}

#[test]
fn url_elicitation_shows_full_url_and_choice() {
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
    let url = app.agents.elicitation.as_ref().and_then(|p| p.url.clone()).expect("url");
    assert!(url.contains("https://example.com/authorize"), "full url shown: {url}");

    // Left/Right cycles accept/decline; Esc declines without opening.
    press(&mut app, KeyCode::Left, KeyModifiers::NONE);
    assert_eq!(app.agents.elicitation.as_ref().expect("prompt").selected_choice, 1);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    wait_until(&mut app, "url elicitation declined", |_| {
        fake.agent().response_with_id(202).is_some()
    });
    assert!(fake.agent().response_with_id(202).expect("response").get("error").is_some());
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
