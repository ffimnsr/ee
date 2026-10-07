//! Agent-permission alignment: a user decision on the agent's own
//! `session/request_permission` resolves the matching bridge prompt
//! (`fs/write_text_file`, `terminal/create`) without a second approval.
//!
//! Every flow runs through the full host stack (fake agent → host handler →
//! pane approval).  Mismatched payloads, mismatched content, non-matching
//! commands, and duplicate follow-up requests must still prompt.

use super::*;

use crate::policy::{CommandRule, MatchMode, TrustEffect, TrustRule, TrustRuleScope, TrustStore};

// ── Wire helpers ─────────────────────────────────────────────────────────────

fn permission_request(id: i64, session_id: &str, tool_call: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/request_permission",
        "params": {
            "sessionId": session_id,
            "toolCall": tool_call,
            "options": [
                { "optionId": "allow-once", "name": "Allow once", "kind": "allow_once" },
                { "optionId": "allow-always", "name": "Allow always", "kind": "allow_always" },
                { "optionId": "reject-once", "name": "Deny", "kind": "reject_once" }
            ]
        }
    })
}

fn write_permission_tool_call(path: &str, content: &str) -> Value {
    json!({
        "toolCallId": "call-write",
        "title": "Write file",
        "rawInput": { "file_path": path, "content": content }
    })
}

fn terminal_permission_tool_call(command: &str) -> Value {
    json!({
        "toolCallId": "call-terminal",
        "title": "Run command",
        "rawInput": { "command": command }
    })
}

// ── Writes ───────────────────────────────────────────────────────────────────

#[test]
fn agent_permission_allow_resolves_matching_write_without_second_prompt() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("aligned.txt");
    let path = file.to_string_lossy().to_string();
    let script = base_script()
        .emit(permission_request(100, "s1", write_permission_tool_call(&path, "hello\n")))
        .wait_for_response(100)
        .emit(write_text_file(103, "s1", &path, "hello\n"));
    let (mut app, fake) = agents_app_in(&temp, script);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "agent permission prompt", |app| app.agents.permission().is_some());
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // Allow once

    wait_until(&mut app, "aligned write answered", |_| {
        fake.agent().response_with_id(103).is_some()
    });
    let response = fake.agent().response_with_id(103).expect("write answered");
    assert!(response.get("result").is_some(), "aligned write must succeed: {response}");
    assert!(
        app.agents.approvals.is_empty(),
        "alignment must not queue a bridge approval: {:#?}",
        app.agents.approvals
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), "hello\n");
    assert!(
        app.agents.threads[0]
            .system_notices()
            .iter()
            .any(|notice| notice.contains("agent permission already granted")),
        "automatic alignment must stay visible in the transcript"
    );
}

#[test]
fn agent_permission_allow_requires_exact_content() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("mismatch.txt");
    let path = file.to_string_lossy().to_string();
    let script = base_script()
        .emit(permission_request(100, "s1", write_permission_tool_call(&path, "approved\n")))
        .wait_for_response(100)
        .emit(write_text_file(103, "s1", &path, "different\n"));
    let (mut app, fake) = agents_app_in(&temp, script);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "agent permission prompt", |app| app.agents.permission().is_some());
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // Allow once

    wait_until(&mut app, "mismatched write still prompts", |app| {
        app.agents.approvals.front().is_some()
    });
    assert!(!file.exists(), "nothing may be written before the bridge approval");

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // Allow once on the bridge prompt
    wait_until(&mut app, "mismatched write answered", |_| {
        fake.agent().response_with_id(103).is_some()
    });
    let response = fake.agent().response_with_id(103).expect("write answered");
    assert!(response.get("result").is_some(), "approved write must succeed: {response}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "different\n");
}

#[test]
fn agent_permission_allow_cannot_cover_a_different_write_path() {
    // The same content at a different path is a different validated operation:
    // approving the agent's permission prompt for one path must never resolve
    // a bridge write to another.
    let temp = tempfile::tempdir().unwrap();
    let approved_file = temp.path().join("approved.txt");
    let actual_file = temp.path().join("actual.txt");
    let approved_path = approved_file.to_string_lossy().to_string();
    let actual_path = actual_file.to_string_lossy().to_string();
    let script = base_script()
        .emit(permission_request(100, "s1", write_permission_tool_call(&approved_path, "same\n")))
        .wait_for_response(100)
        .emit(write_text_file(103, "s1", &actual_path, "same\n"));
    let (mut app, fake) = agents_app_in(&temp, script);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "agent permission prompt", |app| app.agents.permission().is_some());
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // Allow once

    wait_until(&mut app, "different-path write still prompts", |app| {
        app.agents.approvals.front().is_some()
    });
    assert!(!actual_file.exists(), "nothing may be written before the bridge approval");

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // Allow once on the bridge prompt
    wait_until(&mut app, "different-path write answered", |_| {
        fake.agent().response_with_id(103).is_some()
    });
    assert_eq!(fs::read_to_string(&actual_file).unwrap(), "same\n");
    assert!(!approved_file.exists(), "the approved path is never touched");
}

#[test]
fn agent_permission_reject_denies_matching_write_without_prompt() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("denied.txt");
    let path = file.to_string_lossy().to_string();
    let script = base_script()
        .emit(permission_request(100, "s1", write_permission_tool_call(&path, "nope\n")))
        .wait_for_response(100)
        .emit(write_text_file(103, "s1", &path, "nope\n"));
    let (mut app, fake) = agents_app_in(&temp, script);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "agent permission prompt", |app| app.agents.permission().is_some());
    press(&mut app, KeyCode::Right, KeyModifiers::NONE);
    press(&mut app, KeyCode::Right, KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // Deny (reject_once)

    wait_until(&mut app, "denied write answered", |_| fake.agent().response_with_id(103).is_some());
    let response = fake.agent().response_with_id(103).expect("write answered");
    assert!(response.get("error").is_some(), "denied write must error: {response}");
    assert!(!file.exists(), "denied write must not touch disk");
    assert!(app.agents.approvals.is_empty(), "rejection must not queue a bridge approval");
}

#[test]
fn agent_permission_allow_once_is_single_use_across_bridge_requests() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("single-use.txt");
    let path = file.to_string_lossy().to_string();
    let script = base_script()
        .emit(permission_request(100, "s1", write_permission_tool_call(&path, "once\n")))
        .wait_for_response(100)
        .emit(write_text_file(103, "s1", &path, "once\n"))
        .emit(write_text_file(104, "s1", &path, "once\n"));
    let (mut app, fake) = agents_app_in(&temp, script);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "agent permission prompt", |app| app.agents.permission().is_some());
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // Allow once

    wait_until(&mut app, "first write answered", |_| fake.agent().response_with_id(103).is_some());
    assert!(fake.agent().response_with_id(103).unwrap().get("result").is_some());

    // The grant was consumed; the identical follow-up prompts again.
    wait_until(&mut app, "second write prompts", |app| app.agents.approvals.front().is_some());
    assert!(fake.agent().response_with_id(104).is_none(), "second write awaits approval");

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // Allow once on the bridge prompt
    wait_until(&mut app, "second write answered", |_| fake.agent().response_with_id(104).is_some());
    assert!(fake.agent().response_with_id(104).unwrap().get("result").is_some());
}

#[test]
fn agent_permission_allow_always_covers_repeated_bridge_writes() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("always.txt");
    let path = file.to_string_lossy().to_string();
    let content = "always\n";
    // The permission payload must match the first write exactly; the second
    // write reuses the session-wide grant even though a fresh prompt would
    // otherwise appear.
    let script = base_script()
        .emit(permission_request(100, "s1", write_permission_tool_call(&path, content)))
        .wait_for_response(100)
        .emit(write_text_file(103, "s1", &path, content))
        .emit(write_text_file(104, "s1", &path, content));
    let (mut app, fake) = agents_app_in(&temp, script);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "agent permission prompt", |app| app.agents.permission().is_some());
    press(&mut app, KeyCode::Right, KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // Allow always

    wait_until(&mut app, "both writes answered", |_| {
        fake.agent().response_with_id(103).is_some() && fake.agent().response_with_id(104).is_some()
    });
    assert!(fake.agent().response_with_id(103).unwrap().get("result").is_some());
    assert!(fake.agent().response_with_id(104).unwrap().get("result").is_some());
    assert!(app.agents.approvals.is_empty(), "session grant must suppress both prompts");
    assert_eq!(fs::read_to_string(&file).unwrap(), content);
}

// ── Terminals ────────────────────────────────────────────────────────────────

#[test]
fn agent_permission_allow_resolves_matching_terminal_without_second_prompt() {
    let script = base_script()
        .emit(permission_request(100, "s1", terminal_permission_tool_call("echo aligned")))
        .wait_for_response(100)
        .emit(terminal_create(102, "s1", "echo", json!(["aligned"]), json!({})));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "agent permission prompt", |app| app.agents.permission().is_some());
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // Allow once

    wait_until(&mut app, "aligned terminal answered", |_| {
        fake.agent().response_with_id(102).is_some()
    });
    let response = fake.agent().response_with_id(102).expect("terminal answered");
    assert!(
        response["result"]["terminalId"].is_string(),
        "aligned terminal must start: {response}"
    );
    assert!(app.agents.approvals.is_empty(), "alignment must not queue a terminal approval");
}

#[test]
fn agent_permission_terminal_requires_exact_argv() {
    let script = base_script()
        .emit(permission_request(100, "s1", terminal_permission_tool_call("echo approved")))
        .wait_for_response(100)
        .emit(terminal_create(102, "s1", "echo", json!(["different"]), json!({})));
    let (mut app, _temp, fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "agent permission prompt", |app| app.agents.permission().is_some());
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // Allow once

    wait_until(&mut app, "mismatched terminal still prompts", |app| {
        app.agents.approvals.front().is_some()
    });
    assert_eq!(app.agents.terminals.tracked_count(), 0, "nothing spawns before approval");

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // Allow once on the bridge prompt
    wait_until(&mut app, "mismatched terminal answered", |_| {
        fake.agent().response_with_id(102).is_some()
    });
    assert!(
        fake.agent().response_with_id(102).unwrap()["result"]["terminalId"].is_string(),
        "approved terminal must start"
    );
}

#[test]
fn mandatory_confirm_rule_keeps_the_bridge_prompt_despite_alignment() {
    // A persistent mandatory-confirm rule must outrank the alignment ledger:
    // even a byte-exact grant from the agent's own permission prompt cannot
    // resolve the bridge request without the explicit UI confirmation.
    let temp = tempfile::tempdir().unwrap();
    let state_dir = temp.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();
    let script = base_script()
        .emit(permission_request(100, "s1", terminal_permission_tool_call("ee-mc-test x")))
        .wait_for_response(100)
        .emit(terminal_create(102, "s1", "ee-mc-test", json!(["x"]), json!({})));
    let (mut app, fake) = agents_app_in(&temp, script);
    app.agents.test_trust_store_base = Some(state_dir.clone());
    let store = TrustStore::at(&state_dir, temp.path()).expect("trust store");
    store
        .add_rule(TrustRule::Command(CommandRule {
            id: String::from("confirm_mc_test"),
            effect: TrustEffect::Confirm,
            scope: TrustRuleScope {
                workspace: *store.workspace(),
                agent: None,
                expires_at: None,
                max_uses: None,
            },
            executable: String::from("ee-mc-test"),
            match_mode: MatchMode::ArgvExact,
            argv: vec![String::from("x")],
        }))
        .expect("seed mandatory-confirm rule");
    app.reload_workspace_trust_store().expect("reload seeded confirm");
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "agent permission prompt", |app| app.agents.permission().is_some());
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // Allow once
    assert_eq!(app.agents.agent_permissions.len(), 1, "grant recorded from the agent prompt");

    wait_until(&mut app, "mandatory confirm still prompts", |app| {
        app.agents.approvals.front().is_some()
    });
    assert!(
        fake.agent().response_with_id(102).is_none(),
        "nothing resolves before the explicit confirmation"
    );
    assert_eq!(
        app.agents.agent_permissions.len(),
        1,
        "alignment must not consume the grant under mandatory confirm"
    );
}

// ── Prompt previews (Phase 2) ───────────────────────────────────────────────

/// Renders the whole UI and returns the visible rows.
fn render_rows(app: &App) -> Vec<String> {
    const WIDTH: u16 = 120;
    const HEIGHT: u16 = 30;
    let backend = TestBackend::new(WIDTH, HEIGHT);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, app)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..HEIGHT)
        .map(|y| (0..WIDTH).map(|x| buffer.cell((x, y)).unwrap().symbol()).collect())
        .collect()
}

#[test]
fn permission_prompt_renders_ee_verified_write_facts() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("verified.txt");
    let path = file.to_string_lossy().to_string();
    let script = base_script().emit(permission_request(
        100,
        "s1",
        write_permission_tool_call(&path, "facts\n"),
    ));
    let (mut app, _fake) = agents_app_in(&temp, script);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "agent permission prompt", |app| app.agents.permission().is_some());
    let rendered = render_rows(&app);
    assert!(
        rendered.iter().any(|row| row.contains("permission: Write file")),
        "agent question must stay visible: {rendered:#?}"
    );
    assert!(
        rendered.iter().any(|row| row.contains("ee verified: write")),
        "verified write line must render: {rendered:#?}"
    );
    assert!(
        rendered.iter().any(|row| row.contains("verified.txt")),
        "verified path must render: {rendered:#?}"
    );
    assert!(
        rendered.iter().any(|row| row.contains("sha256:")),
        "content digest must render: {rendered:#?}"
    );
    assert!(
        rendered.iter().any(|row| row.contains("· ee verified")),
        "composer must mark the pending choice as ee-verified: {rendered:#?}"
    );
}

#[test]
fn permission_prompt_renders_ee_verified_terminal_facts() {
    let script = base_script().emit(permission_request(
        100,
        "s1",
        terminal_permission_tool_call("cargo test"),
    ));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "agent permission prompt", |app| app.agents.permission().is_some());
    let rendered = render_rows(&app);
    assert!(
        rendered.iter().any(|row| row.contains("ee verified: terminal cargo test")),
        "validated argv must render: {rendered:#?}"
    );
}

#[test]
fn permission_prompt_marks_unverifiable_payload() {
    // Title-only requests carry no ee-validated identity and never align.
    let script = base_script().emit(permission_request(
        100,
        "s1",
        json!({ "toolCallId": "call-1", "title": "Do something" }),
    ));
    let (mut app, _temp, _fake) = fake_agents_app(script);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "agent permission prompt", |app| app.agents.permission().is_some());
    let rendered = render_rows(&app);
    assert!(
        rendered.iter().any(|row| row.contains("unverifiable payload")),
        "unverifiable payloads must say so: {rendered:#?}"
    );
    assert!(
        !rendered.iter().any(|row| row.contains("ee verified:")),
        "unverifiable payloads must not claim verification: {rendered:#?}"
    );
}

// ── Heuristic alignment and counters (Phase 3) ──────────────────────────────

/// An opaque permission request: no verifiable payload, optional tool kind.
fn opaque_permission_tool_call(kind: Option<&str>) -> Value {
    let mut tool_call = json!({ "toolCallId": "call-opaque", "title": "Do the thing" });
    if let Some(kind) = kind {
        tool_call["kind"] = json!(kind);
    }
    tool_call
}

fn set_alignment(app: &mut App, mode: crate::config::AgentAlignmentMode) {
    app.config.agents.approval.alignment = mode;
}

#[test]
fn heuristic_mode_covers_next_opaque_operation_and_says_so() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("heuristic.txt");
    let path = file.to_string_lossy().to_string();
    let script = base_script()
        .emit(permission_request(100, "s1", opaque_permission_tool_call(None)))
        .wait_for_response(100)
        .emit(write_text_file(103, "s1", &path, "heuristic\n"));
    let (mut app, fake) = agents_app_in(&temp, script);
    set_alignment(&mut app, crate::config::AgentAlignmentMode::Heuristic);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "agent permission prompt", |app| app.agents.permission().is_some());
    let rendered = render_rows(&app);
    assert!(
        rendered.iter().any(|row| row.contains("heuristic alignment covers the next matching")),
        "heuristic mode must disclose itself before confirmation: {rendered:#?}"
    );
    assert!(
        rendered.iter().any(|row| row.contains("· ee heuristic")),
        "composer must mark the pending choice: {rendered:#?}"
    );
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // Allow once

    wait_until(&mut app, "heuristic write answered", |_| {
        fake.agent().response_with_id(103).is_some()
    });
    assert!(fake.agent().response_with_id(103).unwrap().get("result").is_some());
    assert!(app.agents.approvals.is_empty(), "heuristic grant must suppress the bridge prompt");
    assert_eq!(fs::read_to_string(&file).unwrap(), "heuristic\n");
    assert_eq!(app.agents.alignment_stats.heuristic_resolved, 1);
    assert_eq!(app.agents.alignment_stats.exact_resolved, 0);
}

#[test]
fn heuristic_mode_respects_the_structured_tool_kind() {
    // `execute` covers terminals; a following write must still prompt.
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("class.txt");
    let path = file.to_string_lossy().to_string();
    let script = base_script()
        .emit(permission_request(100, "s1", opaque_permission_tool_call(Some("execute"))))
        .wait_for_response(100)
        .emit(write_text_file(103, "s1", &path, "class\n"));
    let (mut app, fake) = agents_app_in(&temp, script);
    set_alignment(&mut app, crate::config::AgentAlignmentMode::Heuristic);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "agent permission prompt", |app| app.agents.permission().is_some());
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // Allow once

    wait_until(&mut app, "wrong-class write still prompts", |app| {
        app.agents.approvals.front().is_some()
    });
    assert!(fake.agent().response_with_id(103).is_none(), "write awaits approval");
    assert_eq!(app.agents.alignment_stats.heuristic_resolved, 0);
    assert!(!file.exists(), "nothing may be written before the bridge approval");
}

#[test]
fn heuristic_mode_propagates_opaque_rejection() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("denied-opaque.txt");
    let path = file.to_string_lossy().to_string();
    let script = base_script()
        .emit(permission_request(100, "s1", opaque_permission_tool_call(None)))
        .wait_for_response(100)
        .emit(write_text_file(103, "s1", &path, "nope\n"));
    let (mut app, fake) = agents_app_in(&temp, script);
    set_alignment(&mut app, crate::config::AgentAlignmentMode::Heuristic);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "agent permission prompt", |app| app.agents.permission().is_some());
    press(&mut app, KeyCode::Right, KeyModifiers::NONE);
    press(&mut app, KeyCode::Right, KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // Deny (reject_once)

    wait_until(&mut app, "denied write answered", |_| fake.agent().response_with_id(103).is_some());
    let response = fake.agent().response_with_id(103).expect("write answered");
    assert!(response.get("error").is_some(), "rejected operation must stay denied: {response}");
    assert!(!file.exists(), "denied write must not touch disk");
    assert!(app.agents.approvals.is_empty(), "rejection must not queue a bridge approval");
    assert_eq!(app.agents.alignment_stats.heuristic_resolved, 1);
}

#[test]
fn exact_mode_counts_would_resolve_candidates_without_resolving() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("candidate.txt");
    let path = file.to_string_lossy().to_string();
    let script = base_script()
        .emit(permission_request(100, "s1", opaque_permission_tool_call(None)))
        .wait_for_response(100)
        .emit(write_text_file(103, "s1", &path, "candidate\n"));
    let (mut app, fake) = agents_app_in(&temp, script);
    // Default mode is exact.
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "agent permission prompt", |app| app.agents.permission().is_some());
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // Allow once
    assert_eq!(app.agents.alignment_stats.heuristic_candidates, 1);

    wait_until(&mut app, "opaque write still prompts", |app| {
        app.agents.approvals.front().is_some()
    });
    assert!(fake.agent().response_with_id(103).is_none(), "exact mode never resolves a candidate");
    assert_eq!(app.agents.alignment_stats.heuristic_would_resolve, 1);
    assert_eq!(app.agents.alignment_stats.heuristic_resolved, 0);
    assert_eq!(app.agents.alignment_stats.prompts_queued, 1);
}

#[test]
fn off_mode_disables_alignment_entirely() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("off.txt");
    let path = file.to_string_lossy().to_string();
    let script = base_script()
        .emit(permission_request(100, "s1", write_permission_tool_call(&path, "off\n")))
        .wait_for_response(100)
        .emit(write_text_file(103, "s1", &path, "off\n"));
    let (mut app, fake) = agents_app_in(&temp, script);
    set_alignment(&mut app, crate::config::AgentAlignmentMode::Off);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "agent permission prompt", |app| app.agents.permission().is_some());
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // Allow once

    wait_until(&mut app, "verified write still prompts when off", |app| {
        app.agents.approvals.front().is_some()
    });
    assert!(fake.agent().response_with_id(103).is_none(), "off mode records nothing");
    assert_eq!(app.agents.alignment_stats.exact_resolved, 0);
    assert_eq!(app.agents.alignment_stats.heuristic_resolved, 0);
    assert_eq!(app.agents.alignment_stats.heuristic_candidates, 0);
    assert_eq!(app.agents.alignment_stats.heuristic_would_resolve, 0);
    // The queued prompt is still counted; only alignment is disabled.
    assert_eq!(app.agents.alignment_stats.prompts_queued, 1);
}
