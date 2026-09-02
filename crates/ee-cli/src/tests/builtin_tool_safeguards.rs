//! Phase 11 non-overridable safeguard integration regressions.

use std::fs;
use std::path::{Path, PathBuf};

use ee_agent_host::AgentError;
use tempfile::TempDir;

use crate::app::{ActionLogEntry, App, ApprovalChoice};
use crate::policy::{
    CATASTROPHIC_DELETE_RULE_ID, DecisionReason, SafeguardCategory, TrustCategory, TrustStore,
};
use crate::tests::agent_bridge::{agents_app_in, base_script};

const SESSION: &str = "builtin-safeguard-session";

fn app_with_store() -> (App, TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("workspace tempdir");
    let (mut app, _fake) = agents_app_in(&temp, base_script());
    let state_dir = temp.path().join("host-state");
    fs::create_dir_all(&state_dir).expect("state directory");
    app.agents.test_trust_store_base = Some(state_dir.clone());
    (app, temp, state_dir)
}

fn queue_terminal(
    app: &mut App,
    cwd: &Path,
    command: &str,
) -> tokio::sync::oneshot::Receiver<ee_agent_host::ClientRequestResult> {
    app.queue_terminal_approval_for_test(SESSION, None, command, &[], &[], Some(cwd.to_path_buf()))
}

fn assert_safeguard_denied(
    receiver: &mut tokio::sync::oneshot::Receiver<ee_agent_host::ClientRequestResult>,
    expected_rule: &str,
    expected_category: SafeguardCategory,
) {
    let result = receiver.try_recv().expect("safeguard resolves synchronously");
    let Err(AgentError::NonOverridableDenied { rule_id, category }) = result else {
        panic!("expected typed non-overridable denial: {result:?}");
    };
    assert_eq!(rule_id, expected_rule);
    assert_eq!(category, expected_category.as_str());
}

#[test]
fn builtin_tool_safeguards_deny_catastrophic_chains_before_approval_or_dispatch() {
    let (mut app, temp, _state_dir) = app_with_store();
    let secret = "ARG_SECRET_must_not_appear";
    let mut reply =
        queue_terminal(&mut app, temp.path(), &format!("printf '%s' '{secret}' && ( rm -fR / )"));

    assert!(app.agents.approvals.is_empty());
    assert_eq!(app.agents.terminals.tracked_count(), 0);
    assert_safeguard_denied(
        &mut reply,
        CATASTROPHIC_DELETE_RULE_ID,
        SafeguardCategory::CatastrophicDeletion,
    );

    let audit = app
        .agents_action_log()
        .iter()
        .rev()
        .find(|entry| {
            matches!(
                entry,
                ActionLogEntry::TrustDecision {
                    rule_id: Some(rule_id),
                    category: TrustCategory::Execute,
                    reason: DecisionReason::BuiltInDeny,
                    ..
                } if rule_id == CATASTROPHIC_DELETE_RULE_ID
            )
        })
        .expect("redacted safeguard audit");
    assert!(!format!("{audit:?}").contains(secret));
}

#[test]
fn builtin_tool_safeguards_deny_workspace_root_and_guarded_parse_ambiguity() {
    let (mut app, temp, _state_dir) = app_with_store();
    let workspace = fs::canonicalize(temp.path()).expect("canonical workspace");
    for command in [
        format!("rm --recursive --force '{}'", workspace.display()),
        "sh -c 'echo ok; rm -rf /'".into(),
        "rm -rf \"$TARGET\"".into(),
    ] {
        let mut reply = queue_terminal(&mut app, &workspace, &command);
        assert!(app.agents.approvals.is_empty(), "command: {command}");
        let result = reply.try_recv().expect("safeguard resolves");
        assert!(matches!(result, Err(AgentError::NonOverridableDenied { .. })));
    }
    assert_eq!(app.agents.terminals.tracked_count(), 0);
}

#[test]
fn builtin_tool_safeguards_do_not_match_similar_names_or_quoted_text() {
    let (mut app, temp, _state_dir) = app_with_store();
    for command in ["echo 'rm -rf /'", "remove-all /", "rm -r /workspace-copy"] {
        let mut reply = queue_terminal(&mut app, temp.path(), command);
        assert_eq!(app.agents.approvals.len(), 1, "command should prompt: {command}");
        assert!(
            !app.agents
                .approvals
                .front()
                .expect("approval")
                .options
                .iter()
                .any(|(_, choice)| *choice == ApprovalChoice::AllowPersistent),
            "shell text must never offer persistent allow"
        );
        assert!(reply.try_recv().is_err());
        app.confirm_bridge_approval_for_test(ApprovalChoice::DenyOnce);
        assert!(matches!(
            reply.try_recv().expect("user denial resolves"),
            Err(AgentError::PermissionDenied { .. })
        ));
    }
}

#[test]
fn builtin_tool_safeguards_protect_trust_store_and_workspace_root_delete() {
    let (mut app, temp, state_dir) = app_with_store();
    let store = TrustStore::at(&state_dir, temp.path()).expect("trust store");
    let mut write_reply = app.queue_write_approval_for_test(store.path().to_path_buf(), "tamper");
    assert!(app.agents.approvals.is_empty());
    assert_safeguard_denied(
        &mut write_reply,
        "builtin.v1.protected-state-mutation",
        SafeguardCategory::ProtectedStateMutation,
    );
    assert!(!store.path().exists());

    let root = fs::canonicalize(temp.path()).expect("canonical root");
    let mut delete_reply = app.queue_filesystem_delete_approval_for_test(root.clone());
    assert!(app.agents.approvals.is_empty());
    assert_safeguard_denied(
        &mut delete_reply,
        CATASTROPHIC_DELETE_RULE_ID,
        SafeguardCategory::CatastrophicDeletion,
    );
    assert!(root.exists());
}

#[cfg(unix)]
#[test]
fn builtin_tool_safeguards_deny_special_files_and_path_escapes() {
    use std::os::unix::net::UnixListener;

    let (mut app, _temp, _state_dir) = app_with_store();
    let socket_dir = tempfile::Builder::new()
        .prefix("ee-sock-")
        .tempdir_in("/tmp")
        .expect("short socket tempdir");
    let socket = socket_dir.path().join("agent.sock");
    let _listener = UnixListener::bind(&socket).expect("bind Unix socket");

    let mut socket_reply = app.queue_write_approval_for_test(socket.clone(), "data");
    assert!(app.agents.approvals.is_empty());
    assert_safeguard_denied(
        &mut socket_reply,
        "builtin.v1.special-file-mutation",
        SafeguardCategory::SpecialFileMutation,
    );

    let outside = tempfile::tempdir().expect("outside tempdir");
    let target = outside.path().join("created");
    let mut escape_reply = app.queue_filesystem_create_approval_for_test(target.clone());
    assert!(app.agents.approvals.is_empty());
    assert_safeguard_denied(
        &mut escape_reply,
        "builtin.v1.canonical-path-escape",
        SafeguardCategory::CanonicalPathEscape,
    );
    assert!(!target.exists());
}
