//! MCP terminal-execution trust tests: a separate explicit opt-in for
//! `ee_terminal_create`, never folded into general server trust.

use std::fs;

use crossterm::event::{KeyCode, KeyModifiers};
use serde_json::{Value, json};

use crate::app::App;
use crate::app::ApprovalChoice;
use crate::mcp_trust::McpTrustStore;
use crate::tests::agent_mcp::{
    base_agent_script, connect_proxy, mcp_app, open_pane_and_wait_ready, press, proxy_recv,
    proxy_send, wait_until,
};
use crate::tests::helpers::{CurrentDirGuard, run_ex};
use crate::workspace_trust::{WorkspaceTrustDecision, WorkspaceTrustStore};

fn terminal_frame(command: &str, args: Value) -> Value {
    json!({ "method": "terminal_create", "command": command, "args": args })
}

fn select_option(app: &mut App, index: usize) {
    run_ex(app, "agents");
    for _ in 0..index {
        press(app, KeyCode::Right, KeyModifiers::NONE);
    }
    press(app, KeyCode::Enter, KeyModifiers::NONE);
    if app
        .agents
        .approvals
        .front()
        .is_some_and(|prompt| prompt.allow_confirmation_preview().is_some())
    {
        press(app, KeyCode::Enter, KeyModifiers::NONE);
    }
}

#[test]
fn terminal_execution_opt_in_is_separate_and_persists() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let state_dir = temp.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();
    app.agents.test_trust_store_base = Some(state_dir.clone());
    WorkspaceTrustStore::at(&state_dir, temp.path())
        .unwrap()
        .set_decision(WorkspaceTrustDecision::Trusted)
        .unwrap();

    let mut stream = connect_proxy(&app);
    proxy_send(&mut stream, 1, terminal_frame("git", json!(["stash"])));
    wait_until(&mut app, "terminal approval queued", |app| !app.agents.approvals.is_empty());
    let execution_index = {
        let options = &app.agents.approvals.front().unwrap().options;
        assert!(
            options.iter().all(|(_, choice)| *choice != ApprovalChoice::AllowServer),
            "general server trust must never cover terminal execution"
        );
        options
            .iter()
            .position(|(_, choice)| *choice == ApprovalChoice::AllowServerExecution)
            .expect("execution opt-in offered for validated commands")
    };
    select_option(&mut app, execution_index);

    wait_until(&mut app, "terminal spawned and execution grant persisted", |app| {
        app.agents.terminals.tracked_count() == 1
            && McpTrustStore::at(&state_dir, temp.path())
                .ok()
                .and_then(|store| store.load().ok())
                .is_some_and(|trust| trust.grants().iter().any(|grant| grant.includes_execution()))
    });
    let _ = proxy_recv(&mut stream);

    // A different command and argument set dispatches without a prompt.
    proxy_send(&mut stream, 2, terminal_frame("git", json!(["stash", "pop"])));
    wait_until(&mut app, "second terminal auto-dispatched", |app| {
        app.agents.terminals.tracked_count() == 2 && app.agents.approvals.is_empty()
    });
    let _ = proxy_recv(&mut stream);

    // Shell wrappers stay outside the execution opt-in even after the grant.
    proxy_send(&mut stream, 3, terminal_frame("sh", json!(["-c", "echo hi"])));
    wait_until(&mut app, "shell wrapper still prompts", |app| !app.agents.approvals.is_empty());
    let prompt = app.agents.approvals.front().unwrap();
    assert!(
        prompt.options.iter().all(|(_, choice)| !matches!(
            choice,
            ApprovalChoice::AllowServer | ApprovalChoice::AllowServerExecution
        )),
        "shell wrappers never inherit terminal execution trust"
    );
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    wait_until(&mut app, "shell wrapper denied", |app| app.agents.approvals.is_empty());
    let _ = proxy_recv(&mut stream);
}

#[test]
fn unrestricted_mcp_execution_auto_runs_shell_command_text() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let state_dir = temp.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();
    app.agents.test_trust_store_base = Some(state_dir.clone());
    app.config.agents.read_only_modes.clear();
    WorkspaceTrustStore::at(&state_dir, temp.path())
        .unwrap()
        .set_decision(WorkspaceTrustDecision::Trusted)
        .unwrap();

    let store = McpTrustStore::at(&state_dir, temp.path()).unwrap();
    let mut tools = crate::policy::pinned_server_tools(ee_mcp::ToolTransport::Stdio);
    tools.extend(crate::policy::pinned_unrestricted_execution_tools(ee_mcp::ToolTransport::Stdio));
    let grant = crate::policy::McpServerGrant::new(
        String::from("mcp_server_yolo"),
        *store.workspace(),
        String::from("ee"),
        String::from("stdio:ee --mcp-proxy"),
        crate::policy::mcp_server_trust::granted_at_now(),
        tools,
    )
    .unwrap();
    store.add_grant(grant).unwrap();
    app.reload_mcp_server_trust();

    let mut stream = connect_proxy(&app);
    proxy_send(&mut stream, 1, terminal_frame("printf yolo > mcp-yolo-marker", json!([])));
    wait_until(&mut app, "unrestricted MCP terminal auto-spawned", |app| {
        app.agents.terminals.tracked_count() == 1
            && app.agents.approvals.is_empty()
            && temp.path().join("mcp-yolo-marker").exists()
    });
    let _ = proxy_recv(&mut stream);
}

#[test]
fn general_server_trust_does_not_auto_allow_terminal_execution() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let state_dir = temp.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();
    app.agents.test_trust_store_base = Some(state_dir.clone());
    WorkspaceTrustStore::at(&state_dir, temp.path())
        .unwrap()
        .set_decision(WorkspaceTrustDecision::Trusted)
        .unwrap();

    // Seed only the general server grant (no execute pin).
    let store = McpTrustStore::at(&state_dir, temp.path()).unwrap();
    let grant = crate::policy::McpServerGrant::new(
        String::from("mcp_server_general"),
        *store.workspace(),
        String::from("ee"),
        String::from("stdio:ee --mcp-proxy"),
        crate::policy::mcp_server_trust::granted_at_now(),
        crate::policy::pinned_server_tools(ee_mcp::ToolTransport::Stdio),
    )
    .unwrap();
    store.add_grant(grant).unwrap();
    app.reload_mcp_server_trust();

    let mut stream = connect_proxy(&app);
    proxy_send(&mut stream, 1, terminal_frame("git", json!(["stash"])));
    wait_until(&mut app, "terminal still prompts under general trust", |app| {
        !app.agents.approvals.is_empty()
    });
    let prompt = app.agents.approvals.front().unwrap();
    assert!(
        prompt.options.iter().all(|(_, choice)| *choice != ApprovalChoice::AllowServer),
        "general server trust never applies to terminal execution"
    );
    assert!(
        prompt.options.iter().any(|(_, choice)| *choice == ApprovalChoice::AllowServerExecution),
        "terminal execution remains a separate explicit opt-in"
    );
    assert!(
        !McpTrustStore::at(&state_dir, temp.path())
            .unwrap()
            .load()
            .unwrap()
            .grants()
            .iter()
            .any(|grant| grant.includes_execution()),
        "general grant carries no execute pin"
    );
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    let _ = proxy_recv(&mut stream);
}
