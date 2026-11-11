//! Trusted-workspace native write tests (ISSUES.md "Unified Host-Local
//! Workspace Trust Policy"): an explicit host-local `trusted` workspace
//! decision auto-applies native writes whose targets canonicalize inside the
//! primary workspace root, and nothing else.
//!
//! Outside targets, symlink escapes, protected paths, the secret store,
//! additional workspace roots, requests beyond the write safety maxima, agent
//! modes configured read-only (`agents.read_only_modes`, default `ask`/`plan`),
//! and MCP proxy writes always stay on the explicit approval path.

use std::fs;
use std::path::Path;

use serde_json::{Value, json};
use tempfile::TempDir;

use crate::app::App;
use crate::tests::agent_mcp::{
    ScriptedFake, base_agent_script, connect_proxy, mcp_app, open_pane_and_wait_ready, proxy_recv,
    proxy_send, settle, wait_until,
};
use crate::tests::helpers::CurrentDirGuard;
use crate::workspace_trust::{WorkspaceTrustDecision, WorkspaceTrustStore};
use ee_agent_host::fake::FakeAgentScript;

/// Fake agent that advertises ask/plan/write modes and starts in `write`, so
/// trusted-write auto-approval is enabled; the default fixture starts in
/// `ask`, which is intentionally read-only.
pub(crate) fn write_mode_agent_script() -> FakeAgentScript {
    FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({
            "sessionId": "s1",
            "modes": {
                "currentModeId": "write",
                "availableModes": [
                    { "id": "ask", "name": "Ask" },
                    { "id": "plan", "name": "Plan" },
                    { "id": "write", "name": "Write" },
                ],
            },
        }))
        .wait_for("session/set_mode")
        .respond(json!({}))
        .delay(25)
}

fn write_frame(path: &Path, content: &str) -> Value {
    json!({
        "method": "write_text_file",
        "path": path.display().to_string(),
        "content": content,
    })
}

/// App with the agents pane open and the primary workspace explicitly
/// trusted. The caller must pin the process cwd to the workspace for the
/// duration of the test, because trust resolution reads the workspace root
/// from the current working directory.
fn trusted_app() -> (App, TempDir, ScriptedFake) {
    let (mut app, temp, fake) = mcp_app(write_mode_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);
    let state_dir = temp.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();
    app.agents.test_trust_store_base = Some(state_dir.clone());
    WorkspaceTrustStore::at(&state_dir, temp.path())
        .unwrap()
        .set_decision(WorkspaceTrustDecision::Trusted)
        .unwrap();
    (app, temp, fake)
}

/// App with the agents pane open and no workspace trust decision.
fn undecided_app() -> (App, TempDir, ScriptedFake) {
    let (mut app, temp, fake) = mcp_app(write_mode_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);
    let state_dir = temp.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();
    app.agents.test_trust_store_base = Some(state_dir);
    (app, temp, fake)
}

#[test]
fn trusted_workspace_auto_applies_in_workspace_create() {
    let (mut app, temp, _fake) = trusted_app();
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();

    let target = temp.path().join("new.rs");
    let mut stream = connect_proxy(&app);
    proxy_send(&mut stream, 1, write_frame(&target, "fn new() {}"));

    wait_until(&mut app, "trusted create dispatched", |app| {
        app.agents.approvals.is_empty() && target.exists()
    });
    let reply = proxy_recv(&mut stream);
    assert_eq!(reply["result"]["value"].as_str(), Some("ok"), "auto-allowed: {reply}");
    assert_eq!(fs::read_to_string(&target).unwrap().trim_end(), "fn new() {}");
}

#[test]
fn trusted_workspace_auto_applies_in_workspace_modify() {
    let (mut app, temp, _fake) = trusted_app();
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();

    let target = temp.path().join("existing.rs");
    fs::write(&target, "fn old() {}\n").unwrap();

    let mut stream = connect_proxy(&app);
    proxy_send(&mut stream, 1, write_frame(&target, "fn new() {}\n"));

    wait_until(&mut app, "trusted modify dispatched", |app| {
        app.agents.approvals.is_empty()
            && fs::read_to_string(&target).is_ok_and(|text| text.contains("fn new()"))
    });
    let reply = proxy_recv(&mut stream);
    assert_eq!(reply["result"]["value"].as_str(), Some("ok"), "auto-allowed: {reply}");
    assert_eq!(fs::read_to_string(&target).unwrap().trim_end(), "fn new() {}");
}

#[test]
fn read_only_mode_stays_explicit_even_when_workspace_trusted() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);
    let state_dir = temp.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();
    app.agents.test_trust_store_base = Some(state_dir.clone());
    WorkspaceTrustStore::at(&state_dir, temp.path())
        .unwrap()
        .set_decision(WorkspaceTrustDecision::Trusted)
        .unwrap();

    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();

    let target = temp.path().join("ask-mode.rs");
    let mut stream = connect_proxy(&app);
    proxy_send(&mut stream, 1, write_frame(&target, "fn ask() {}"));

    wait_until(&mut app, "read-only mode write queued", |app| !app.agents.approvals.is_empty());
    assert!(!target.exists(), "ask/plan modes never auto-apply trusted writes");
}

#[test]
fn empty_read_only_mode_set_restores_trusted_write_auto_apply() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);
    app.config.agents.read_only_modes.clear();
    let state_dir = temp.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();
    app.agents.test_trust_store_base = Some(state_dir.clone());
    WorkspaceTrustStore::at(&state_dir, temp.path())
        .unwrap()
        .set_decision(WorkspaceTrustDecision::Trusted)
        .unwrap();

    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();

    let target = temp.path().join("ask-mode.rs");
    let mut stream = connect_proxy(&app);
    proxy_send(&mut stream, 1, write_frame(&target, "fn ask() {}"));

    wait_until(&mut app, "configured write dispatched", |app| {
        app.agents.approvals.is_empty() && target.exists()
    });
    let reply = proxy_recv(&mut stream);
    assert_eq!(reply["result"]["value"].as_str(), Some("ok"), "auto-allowed: {reply}");
}

#[test]
fn undecided_workspace_still_prompts_writes() {
    let (mut app, temp, _fake) = undecided_app();
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();

    let target = temp.path().join("blocked.rs");
    let mut stream = connect_proxy(&app);
    proxy_send(&mut stream, 1, write_frame(&target, "fn blocked() {}"));

    wait_until(&mut app, "undecided write queued", |app| !app.agents.approvals.is_empty());
    assert!(!target.exists(), "trust is required to auto-apply writes");
}

#[test]
fn trusted_workspace_does_not_auto_allow_protected_paths() {
    let (mut app, temp, _fake) = trusted_app();
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();

    let target = temp.path().join(".env");
    let mut stream = connect_proxy(&app);
    proxy_send(&mut stream, 1, write_frame(&target, "TOKEN=x"));

    wait_until(&mut app, "protected write queued", |app| !app.agents.approvals.is_empty());
    assert!(!target.exists(), "protected paths always prompt");
}

#[test]
fn trusted_workspace_does_not_auto_allow_additional_workspace_roots() {
    let (mut app, temp, _fake) = trusted_app();
    let outside = TempDir::new().unwrap();
    app.agents.additional_workspace_roots.insert(outside.path().to_path_buf());

    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();

    let target = outside.path().join("added-root.rs");
    let mut stream = connect_proxy(&app);
    proxy_send(&mut stream, 1, write_frame(&target, "fn added() {}"));

    settle(&mut app);
    let reply = proxy_recv(&mut stream);
    assert!(!target.exists(), "trust covers the primary workspace root only");
    assert!(
        app.agents.approvals.len() == 1 || reply["result"]["error"].is_object(),
        "added-root write must never auto-apply; approvals={} reply={reply}",
        app.agents.approvals.len()
    );
}

#[test]
fn trusted_workspace_write_outside_workspace_is_rejected() {
    let (mut app, temp, _fake) = trusted_app();
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();

    let outside = TempDir::new().unwrap();
    let target = outside.path().join("escape.rs");
    let mut stream = connect_proxy(&app);
    proxy_send(&mut stream, 1, write_frame(&target, "fn escape() {}"));

    settle(&mut app);
    let reply = proxy_recv(&mut stream);
    assert!(reply["result"]["error"].is_object(), "external write rejected: {reply}");
    assert!(!target.exists());
}

#[cfg(unix)]
#[test]
fn trusted_workspace_does_not_follow_symlink_escapes() {
    let (mut app, temp, _fake) = trusted_app();
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();

    let outside = TempDir::new().unwrap();
    let link = temp.path().join("escape-dir");
    std::os::unix::fs::symlink(outside.path(), &link).unwrap();
    let target = link.join("escaped.rs");

    let mut stream = connect_proxy(&app);
    proxy_send(&mut stream, 1, write_frame(&target, "fn escaped() {}"));

    settle(&mut app);
    let reply = proxy_recv(&mut stream);
    assert!(reply["result"]["error"].is_object(), "symlink escape rejected: {reply}");
    assert!(!outside.path().join("escaped.rs").exists());
}

// ── Evaluator precedence ─────────────────────────────────────────────────────

mod precedence {
    use std::time::SystemTime;

    use crate::policy::evaluator::PolicyInput;
    use crate::policy::session::{SessionChoice, SessionPolicy};
    use crate::policy::{
        DecisionReason, OperationIdentity, TransportKind, TrustCategory, TrustDecision,
        TrustOperation, TrustOutcome, UsageSnapshot, WorkspaceIdentity, evaluate,
    };

    fn write_operation() -> TrustOperation {
        TrustOperation {
            workspace: WorkspaceIdentity::from_canonical_root_bytes(b"/work"),
            agent: None,
            transport: TransportKind::Acp,
            category: TrustCategory::WriteCreate,
            identity: OperationIdentity::Write {
                relative_path: String::from("src/a.rs"),
                file_count: 1,
                total_bytes: Some(8),
                max_file_bytes: Some(8),
            },
        }
    }

    fn decide(
        operation: &TrustOperation,
        session: &SessionPolicy,
        trusted_write_enabled: bool,
    ) -> TrustDecision {
        evaluate(&PolicyInput {
            session_id: "s1",
            fingerprint: "fp",
            operation,
            session,
            rules: &[],
            now: SystemTime::UNIX_EPOCH,
            usage: &UsageSnapshot::default(),
            workspace_enabled: false,
            safe_read_enabled: false,
            trusted_write_enabled,
            built_in_deny: None,
            tool_default: None,
            category_default: None,
            global_default: None,
        })
    }

    #[test]
    fn trusted_workspace_allows_validated_native_writes() {
        let decision = decide(&write_operation(), &SessionPolicy::default(), true);
        assert_eq!(decision.outcome, TrustOutcome::Allow);
        assert_eq!(decision.reason, DecisionReason::BuiltInAllow);
    }

    #[test]
    fn trusted_write_allow_requires_the_trust_flag() {
        let decision = decide(&write_operation(), &SessionPolicy::default(), false);
        assert_eq!(decision.outcome, TrustOutcome::Confirm);
    }

    #[test]
    fn trusted_write_allow_skips_unanchored_write_identities() {
        let mut operation = write_operation();
        operation.identity =
            OperationIdentity::native_tool("fs/write_text_file").expect("native tool identity");
        let decision = decide(&operation, &SessionPolicy::default(), true);
        assert_eq!(decision.outcome, TrustOutcome::Confirm);
    }

    #[test]
    fn session_deny_beats_the_trusted_write_allow() {
        let mut session = SessionPolicy::default();
        session.record("s1", "fp", SessionChoice::Deny);
        let decision = decide(&write_operation(), &session, true);
        assert_eq!(decision.outcome, TrustOutcome::Deny);
        assert_eq!(decision.reason, DecisionReason::SessionDeny);
    }
}
