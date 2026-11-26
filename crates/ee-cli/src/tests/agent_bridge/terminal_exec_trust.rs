//! Workspace terminal execution trust: explicit, host-local opt-in for
//! prompt-free native ACP v1 `terminal/create` requests.
//!
//! The grant is separate from MCP `--execute` and from plain workspace trust.
//! Native requests are driven through the approval seam with the workspace
//! pinned by an active buffer; the MCP boundary test uses the real proxy.

use std::fs;

use crossterm::event::{KeyCode, KeyModifiers};
use serde_json::json;

use super::*;
use crate::policy::{CommandRule, MatchMode, TrustEffect, TrustRule, TrustRuleScope, TrustStore};
use crate::workspace_trust::{WorkspaceTrustDecision, WorkspaceTrustStore};

fn pin_workspace_root(app: &mut App, temp: &tempfile::TempDir) {
    fs::create_dir_all(temp.path().join(".git")).unwrap();
    let file = temp.path().join("workspace.txt");
    fs::write(&file, "workspace\n").unwrap();
    let id = app.backend.open_buffer(Some(file)).unwrap();
    app.backend.switch_to_id(id).unwrap();
}

fn trust_workspace(app: &mut App, temp: &tempfile::TempDir, execute: bool) {
    let state = temp.path().join("terminal-trust-state");
    fs::create_dir_all(&state).unwrap();
    app.agents.test_trust_store_base = Some(state.clone());
    let store = WorkspaceTrustStore::at(&state, temp.path()).unwrap();
    store.set_decision(WorkspaceTrustDecision::Trusted).unwrap();
    if execute {
        store.set_terminal_execution(true).unwrap();
    }
}

fn native_app(temp: &tempfile::TempDir) -> App {
    let (mut app, _fake) = agents_app_in(temp, base_script());
    open_pane_and_wait_ready(&mut app);
    pin_workspace_root(&mut app, temp);
    app
}

fn queue_terminal(
    app: &mut App,
    command: &str,
    args: &[&str],
    cwd: Option<std::path::PathBuf>,
) -> tokio::sync::oneshot::Receiver<ee_agent_host::ClientRequestResult> {
    app.queue_terminal_approval_for_test("s1", None, command, args, &[], cwd)
}

#[test]
fn granted_workspace_auto_spawns_simple_terminal() {
    let temp = tempfile::tempdir().unwrap();
    let mut app = native_app(&temp);
    trust_workspace(&mut app, &temp, true);
    app.config.agents.read_only_modes.clear();

    let mut reply = queue_terminal(&mut app, "touch", &["marker"], None);
    wait_until(&mut app, "trusted terminal spawned", |app| {
        app.agents.terminals.tracked_count() == 1
            && app.agents.approvals.is_empty()
            && temp.path().join("marker").exists()
    });
    assert!(app.agents.approvals.is_empty(), "grant skips the approval prompt");
    assert!(reply.try_recv().expect("auto reply").is_ok());
}

#[test]
fn granted_workspace_accepts_primary_workspace_subdirectory() {
    let temp = tempfile::tempdir().unwrap();
    let child = temp.path().join("src");
    fs::create_dir_all(&child).unwrap();
    let mut app = native_app(&temp);
    trust_workspace(&mut app, &temp, true);
    app.config.agents.read_only_modes.clear();

    let mut reply = queue_terminal(&mut app, "touch", &["sub-marker"], Some(child.clone()));
    wait_until(&mut app, "subdirectory terminal spawned", |app| {
        app.agents.terminals.tracked_count() == 1
            && app.agents.approvals.is_empty()
            && child.join("sub-marker").exists()
    });
    assert!(reply.try_recv().expect("auto reply").is_ok());
}

#[test]
fn granted_workspace_accepts_cd_prefixed_simple_command() {
    let temp = tempfile::tempdir().unwrap();
    let child = temp.path().join("src");
    fs::create_dir_all(&child).unwrap();
    let mut app = native_app(&temp);
    trust_workspace(&mut app, &temp, true);
    app.config.agents.read_only_modes.clear();

    let command = format!("cd {} && touch cd-marker", child.display());
    let request = ee_agent_protocol::CreateTerminalRequest::new(
        ee_agent_protocol::SessionId::new("s1"),
        command.as_str(),
    );
    assert!(
        app.terminal_trust_invocation(&request).is_some(),
        "cd prefix must normalize; primary={:?} command={command:?}",
        app.primary_workspace_root()
    );
    let mut reply = queue_terminal(&mut app, &command, &[], None);
    wait_until(&mut app, "cd-prefixed terminal spawned", |app| {
        app.agents.terminals.tracked_count() == 1
            && app.agents.approvals.is_empty()
            && child.join("cd-marker").exists()
    });
    assert!(reply.try_recv().expect("auto reply").is_ok());
}

#[test]
fn cd_prefix_outside_the_workspace_stays_on_the_approval_path() {
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let mut app = native_app(&temp);
    trust_workspace(&mut app, &temp, true);
    app.config.agents.read_only_modes.clear();

    let command = format!("cd {} && touch outside-cd-marker", outside.path().display());
    let mut reply = queue_terminal(&mut app, &command, &[], None);
    wait_until(&mut app, "escape cd prefix prompts", |app| !app.agents.approvals.is_empty());
    assert!(reply.try_recv().is_err());
    assert!(!outside.path().join("outside-cd-marker").exists());
    assert_eq!(app.agents.terminals.tracked_count(), 0);
    app.agents.approvals.clear();
}

#[test]
fn classifier_meta_auto_approves_only_for_builtin_orchestrator_agents() {
    use ee_agent_protocol::approval_meta::mark_classifier_auto_approved;

    let temp = tempfile::tempdir().unwrap();
    let mut app = native_app(&temp);
    trust_workspace(&mut app, &temp, true);
    app.config.agents.read_only_modes.clear();
    app.config.agents.servers.insert(
        String::from("builtin"),
        crate::config::AgentServerSettings {
            label: None,
            command: String::from("/opt/ee/ee-agent-opencode"),
            args: Vec::new(),
            env: Default::default(),
            cwd: None,
            protocol: crate::config::AgentProtocol::V1,
        },
    );

    // Shell syntax keeps classifier opt-in distinct from workspace trust.
    let mut request = ee_agent_protocol::CreateTerminalRequest::new(
        ee_agent_protocol::SessionId::new("s1"),
        "echo classifier > classifier-marker",
    );
    request.meta = Some(mark_classifier_auto_approved(None));
    let mut reply = app.queue_terminal_request_for_test(Some("builtin"), request);
    wait_until(&mut app, "classifier auto-approval spawns", |app| {
        app.agents.terminals.tracked_count() == 1
            && app.agents.approvals.is_empty()
            && temp.path().join("classifier-marker").exists()
    });
    assert!(reply.try_recv().expect("auto reply").is_ok());

    // A generic ACP agent cannot self-approve through `_meta`.
    let mut request = ee_agent_protocol::CreateTerminalRequest::new(
        ee_agent_protocol::SessionId::new("s1"),
        "echo hi > ignored-marker",
    );
    request.meta = Some(mark_classifier_auto_approved(None));
    let mut reply = app.queue_terminal_request_for_test(Some("fake"), request);
    wait_until(&mut app, "non-builtin meta prompts", |app| !app.agents.approvals.is_empty());
    assert!(reply.try_recv().is_err());
    assert!(!temp.path().join("ignored-marker").exists());
    app.agents.approvals.clear();
}

#[test]
fn plain_workspace_trust_does_not_grant_terminal_execution() {
    let temp = tempfile::tempdir().unwrap();
    let mut app = native_app(&temp);
    trust_workspace(&mut app, &temp, false);
    app.config.agents.read_only_modes.clear();

    let mut reply = queue_terminal(&mut app, "touch", &["marker"], None);
    wait_until(&mut app, "approval without execution grant", |app| {
        !app.agents.approvals.is_empty()
    });
    assert!(reply.try_recv().is_err(), "prompt stays unresolved");
    assert!(!temp.path().join("marker").exists());
    assert_eq!(app.agents.terminals.tracked_count(), 0);
    app.agents.approvals.clear();
}

#[test]
fn undecided_workspace_does_not_grant_terminal_execution() {
    let temp = tempfile::tempdir().unwrap();
    let mut app = native_app(&temp);
    app.agents.test_trust_store_base = Some(temp.path().join("undecided-state"));

    let mut reply = queue_terminal(&mut app, "touch", &["marker"], None);
    wait_until(&mut app, "approval on undecided workspace", |app| !app.agents.approvals.is_empty());
    assert!(reply.try_recv().is_err());
    assert!(!temp.path().join("marker").exists());
    app.agents.approvals.clear();
}

#[test]
fn shell_wrappers_stay_on_the_approval_path() {
    let temp = tempfile::tempdir().unwrap();
    let mut app = native_app(&temp);
    trust_workspace(&mut app, &temp, true);
    app.config.agents.read_only_modes.clear();

    let mut reply = queue_terminal(&mut app, "sh", &["-c", "touch marker"], None);
    wait_until(&mut app, "shell wrapper prompts", |app| !app.agents.approvals.is_empty());
    assert!(reply.try_recv().is_err());
    assert!(!temp.path().join("marker").exists());
    app.agents.approvals.clear();
}

#[test]
fn external_operand_stays_on_the_approval_path() {
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let mut app = native_app(&temp);
    trust_workspace(&mut app, &temp, true);
    app.config.agents.read_only_modes.clear();

    let marker = outside.path().join("outside-operand-marker");
    let mut reply = queue_terminal(&mut app, "touch", &[marker.to_str().unwrap()], None);
    wait_until(&mut app, "external operand prompts", |app| !app.agents.approvals.is_empty());
    assert!(reply.try_recv().is_err());
    assert!(!marker.exists());
    app.agents.approvals.clear();
}

#[test]
fn traversal_operand_stays_on_the_approval_path() {
    let temp = tempfile::tempdir().unwrap();
    let mut app = native_app(&temp);
    trust_workspace(&mut app, &temp, true);
    app.config.agents.read_only_modes.clear();

    let mut reply = queue_terminal(&mut app, "touch", &[".."], None);
    wait_until(&mut app, "traversal operand prompts", |app| !app.agents.approvals.is_empty());
    assert!(reply.try_recv().is_err());
    app.agents.approvals.clear();
}

#[test]
fn absolute_executable_stays_on_the_approval_path() {
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let mut app = native_app(&temp);
    trust_workspace(&mut app, &temp, true);
    app.config.agents.read_only_modes.clear();

    let tool = outside.path().join("outside-tool");
    fs::write(&tool, b"#!/bin/sh\n").unwrap();
    let mut reply = queue_terminal(&mut app, tool.to_str().unwrap(), &[], None);
    wait_until(&mut app, "absolute executable prompts", |app| !app.agents.approvals.is_empty());
    assert!(reply.try_recv().is_err());
    app.agents.approvals.clear();
}

#[test]
fn option_embedded_path_stays_on_the_approval_path() {
    let temp = tempfile::tempdir().unwrap();
    let mut app = native_app(&temp);
    trust_workspace(&mut app, &temp, true);
    app.config.agents.read_only_modes.clear();

    let mut reply = queue_terminal(&mut app, "touch", &["--out=/tmp/ee-outside-marker"], None);
    wait_until(&mut app, "option path prompts", |app| !app.agents.approvals.is_empty());
    assert!(reply.try_recv().is_err());
    app.agents.approvals.clear();
}

#[test]
fn shell_wrapper_operand_stays_on_the_approval_path() {
    let temp = tempfile::tempdir().unwrap();
    let mut app = native_app(&temp);
    trust_workspace(&mut app, &temp, true);
    app.config.agents.read_only_modes.clear();

    let mut reply = queue_terminal(&mut app, "env", &["bash", "-c", "touch marker"], None);
    wait_until(&mut app, "wrapper operand prompts", |app| !app.agents.approvals.is_empty());
    assert!(reply.try_recv().is_err());
    assert!(!temp.path().join("marker").exists());
    app.agents.approvals.clear();
}

#[test]
fn in_workspace_absolute_operand_auto_spawns() {
    let temp = tempfile::tempdir().unwrap();
    let mut app = native_app(&temp);
    trust_workspace(&mut app, &temp, true);
    app.config.agents.read_only_modes.clear();

    let marker = std::fs::canonicalize(temp.path()).unwrap().join("absolute-marker");
    let mut reply = queue_terminal(&mut app, "touch", &[marker.to_str().unwrap()], None);
    wait_until(&mut app, "in-workspace absolute operand spawned", |app| {
        app.agents.terminals.tracked_count() == 1
            && app.agents.approvals.is_empty()
            && marker.exists()
    });
    assert!(reply.try_recv().expect("auto reply").is_ok());
}

#[test]
fn external_cwd_stays_on_the_approval_path() {
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let mut app = native_app(&temp);
    trust_workspace(&mut app, &temp, true);
    app.config.agents.read_only_modes.clear();

    let mut reply =
        queue_terminal(&mut app, "touch", &["outside-marker"], Some(outside.path().to_path_buf()));
    wait_until(&mut app, "external cwd prompts", |app| !app.agents.approvals.is_empty());
    assert!(reply.try_recv().is_err());
    assert!(!outside.path().join("outside-marker").exists());
    app.agents.approvals.clear();
}

#[test]
fn execution_affecting_env_overrides_stay_on_the_approval_path() {
    let temp = tempfile::tempdir().unwrap();
    let mut app = native_app(&temp);
    trust_workspace(&mut app, &temp, true);
    app.config.agents.read_only_modes.clear();

    let mut reply = app.queue_terminal_approval_for_test(
        "s1",
        None,
        "touch",
        &["marker"],
        &[("PATH", "/tmp")],
        None,
    );
    wait_until(&mut app, "env override prompts", |app| !app.agents.approvals.is_empty());
    assert!(reply.try_recv().is_err());
    assert!(!temp.path().join("marker").exists());
    app.agents.approvals.clear();
}

#[test]
fn read_only_agent_mode_stays_on_the_approval_path() {
    let temp = tempfile::tempdir().unwrap();
    let mut app = native_app(&temp);
    trust_workspace(&mut app, &temp, true);
    let mode = app
        .agents
        .threads
        .first()
        .and_then(|thread| thread.host.snapshot().current_mode)
        .map(|mode| mode.0.to_string())
        .expect("fake agent advertises a current mode");
    app.config.agents.read_only_modes.insert(mode);

    let mut reply = queue_terminal(&mut app, "touch", &["marker"], None);
    wait_until(&mut app, "read-only mode prompts", |app| !app.agents.approvals.is_empty());
    assert!(reply.try_recv().is_err());
    assert!(!temp.path().join("marker").exists());
    app.agents.approvals.clear();
}

#[test]
fn persistent_deny_wins_over_the_execution_grant() {
    let temp = tempfile::tempdir().unwrap();
    let mut app = native_app(&temp);
    trust_workspace(&mut app, &temp, true);
    app.config.agents.read_only_modes.clear();
    let state = temp.path().join("terminal-trust-state");
    let store = TrustStore::at(&state, temp.path()).unwrap();
    store
        .add_rule(TrustRule::Command(CommandRule {
            id: String::from("deny_touch_marker"),
            effect: TrustEffect::Deny,
            scope: TrustRuleScope {
                workspace: *store.workspace(),
                agent: None,
                expires_at: None,
                max_uses: None,
            },
            executable: String::from("touch"),
            match_mode: MatchMode::ArgvExact,
            argv: vec![String::from("marker")],
        }))
        .unwrap();
    app.reload_workspace_trust_store().unwrap();

    let mut reply = queue_terminal(&mut app, "touch", &["marker"], None);
    let result = reply.try_recv().expect("deny resolved immediately");
    assert!(result.is_err(), "deny wins over the execution grant");
    assert!(app.agents.approvals.is_empty());
    assert!(!temp.path().join("marker").exists());
}

#[test]
fn simple_shell_text_still_matches_safe_read() {
    let temp = tempfile::tempdir().unwrap();
    let mut app = native_app(&temp);
    trust_workspace(&mut app, &temp, false);

    let mut reply = queue_terminal(&mut app, "git status", &[], None);
    wait_until(&mut app, "safe-read shell text spawned", |app| {
        app.agents.terminals.tracked_count() == 1 && app.agents.approvals.is_empty()
    });
    assert!(reply.try_recv().expect("auto reply").is_ok());
}

#[test]
fn workspace_execution_grant_does_not_cross_into_mcp_terminal() {
    use crate::tests::agent_mcp::{
        connect_proxy, mcp_app, open_pane_and_wait_ready as open_mcp_pane, proxy_recv, proxy_send,
        wait_until as wait_mcp,
    };

    let (mut app, temp, _fake) = mcp_app(crate::tests::agent_mcp::base_agent_script(), false, true);
    open_mcp_pane(&mut app);
    pin_workspace_root(&mut app, &temp);
    trust_workspace(&mut app, &temp, true);
    app.config.agents.read_only_modes.clear();

    let mut stream = connect_proxy(&app);
    proxy_send(
        &mut stream,
        1,
        json!({ "method": "terminal_create", "command": "touch", "args": ["mcp-marker"] }),
    );
    wait_mcp(&mut app, "MCP terminal still prompts", |app| !app.agents.approvals.is_empty());
    assert_eq!(app.agents.terminals.tracked_count(), 0);
    assert!(!temp.path().join("mcp-marker").exists());
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    let _ = proxy_recv(&mut stream);
}
