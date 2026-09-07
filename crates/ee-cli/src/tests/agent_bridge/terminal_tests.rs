//! `terminal/*` bridge tests.
use super::*;

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
    use crate::app::{AgentTerminals, OwnedTerminalStop, TerminalOwner};
    use ee_agent_protocol::{CreateTerminalRequest, KillTerminalRequest, SessionId, TerminalId};

    let terminals = AgentTerminals::default();
    let created = terminals
        .spawn(
            &CreateTerminalRequest::new(SessionId::new("s1"), "sleep")
                .args(vec![String::from("30")]),
            Some("test-agent"),
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

    let owner =
        TerminalOwner { agent_id: String::from("test-agent"), session_id: String::from("s1") };
    assert_eq!(
        terminals.stop_owned(&owner, terminal_id.0.as_ref()).expect("owned stop succeeds"),
        OwnedTerminalStop::StopRequested
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
    let created = terminals.spawn(&request, Some("test-agent")).expect("terminal spawns");
    let terminal_id = created.terminal_id.0.to_string();
    let owner = crate::app::TerminalOwner {
        agent_id: String::from("test-agent"),
        session_id: String::from("s1"),
    };
    let foreign_agent = crate::app::TerminalOwner {
        agent_id: String::from("other-agent"),
        session_id: String::from("s1"),
    };
    assert_eq!(terminals.list_owned(&owner, 128).len(), 1);
    assert!(terminals.list_owned(&foreign_agent, 128).is_empty());

    let denied =
        terminals.output(&ee_agent_protocol::TerminalOutputRequest::new("s2", terminal_id.clone()));
    assert!(denied.is_err(), "other sessions must not observe terminal output");

    let owned = terminals.output(&ee_agent_protocol::TerminalOutputRequest::new("s1", terminal_id));
    assert!(owned.is_ok(), "owner session may observe output");
    terminals.kill_all();
}
