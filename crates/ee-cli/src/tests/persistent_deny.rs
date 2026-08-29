//! Focused Phase 9 persistent-deny app/UI regressions.
//!
//! Tests drive approval policy directly through test-only bridge seams. No
//! agent transport or terminal process starts.

use std::fs;
use std::path::{Path, PathBuf};

use ee_agent_host::AgentError;
use tempfile::TempDir;

use crate::app::{ActionLogEntry, AgentPaneLayout, App, ApprovalChoice, Mode, ToolApprovalMode};
use crate::policy::session::SessionChoice;
use crate::policy::{
    BrowserActionClass, CommandRule, DecisionReason, FilesystemOperationKind, FilesystemRule,
    HostMatchMode, MatchMode, McpDenyRule, NetworkMethodClass, NetworkRule, NetworkScheme,
    PathPrefix, TrustCategory, TrustEffect, TrustRule, TrustRuleScope, TrustStore,
    WriteOperationKind, WriteRule,
};
use crate::tests::agent_bridge::{agents_app_in, base_script};
use crate::tests::helpers::render_screen_rows;

const SESSION: &str = "persistent-deny-session";
const AGENT: &str = "persistent-deny-agent";
const COMMAND: &str = "ee-observable-command";
const ARGS: [&str; 2] = ["ARG_SECRET_4bca", "MCP_SECRET_c71e"];
const ENV: [(&str, &str); 1] = [("TOKEN", "ENV_SECRET_92dd")];

fn app_with_store() -> (App, TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("workspace tempdir");
    let (mut app, _fake) = agents_app_in(&temp, base_script());
    let state_dir = temp.path().join("state");
    fs::create_dir_all(&state_dir).expect("state directory");
    app.agents.test_trust_store_base = Some(state_dir.clone());
    (app, temp, state_dir)
}

fn deny_scope(store: &TrustStore, agent: Option<&str>) -> TrustRuleScope {
    TrustRuleScope {
        workspace: *store.workspace(),
        agent: agent.map(str::to_string),
        expires_at: None,
        max_uses: None,
    }
}

fn seed_rule(app: &mut App, state_dir: &Path, workspace: &Path, rule: TrustRule) {
    TrustStore::at(state_dir, workspace)
        .expect("trust store")
        .add_rule(rule)
        .expect("seed deny rule");
    app.reload_workspace_trust_store().expect("reload seeded deny");
}

fn seed_command_deny(
    state_dir: &Path,
    workspace: &Path,
    id: &str,
    agent: Option<&str>,
) -> TrustRule {
    let store = TrustStore::at(state_dir, workspace).expect("trust store");
    let rule = TrustRule::Command(CommandRule {
        id: id.to_string(),
        effect: TrustEffect::Deny,
        scope: deny_scope(&store, agent),
        executable: COMMAND.to_string(),
        match_mode: MatchMode::ArgvExact,
        argv: ARGS.iter().map(|value| (*value).to_string()).collect(),
    });
    store.add_rule(rule.clone()).expect("seed deny rule");
    rule
}

fn queue_request(
    app: &mut App,
    cwd: &Path,
) -> tokio::sync::oneshot::Receiver<ee_agent_host::ClientRequestResult> {
    app.queue_terminal_approval_for_test(
        SESSION,
        Some(AGENT),
        COMMAND,
        &ARGS,
        &ENV,
        Some(cwd.to_path_buf()),
    )
}

fn assert_denied(
    receiver: &mut tokio::sync::oneshot::Receiver<ee_agent_host::ClientRequestResult>,
) -> String {
    let result = receiver.try_recv().expect("request must resolve");
    let Err(AgentError::PermissionDenied { reason }) = result else {
        panic!("request must be denied: {result:?}");
    };
    reason
}

#[test]
fn persistent_deny_preview_persists_before_reply_and_overrides_session_allow_and_bypass() {
    let (mut app, temp, state_dir) = app_with_store();
    let secret_cwd = temp.path().join("PATH_SECRET_6f53");
    fs::create_dir(&secret_cwd).expect("secret-named cwd");

    let mut first_reply = queue_request(&mut app, &secret_cwd);
    let prompt = app.agents.approvals.front().expect("approval queued");
    assert!(prompt.options.iter().any(|(label, choice)| {
        label == "Deny for this workspace" && *choice == ApprovalChoice::DenyPersistent
    }));
    assert!(first_reply.try_recv().is_err(), "prompt must remain unresolved");

    app.confirm_bridge_approval_for_test(ApprovalChoice::DenyPersistent);
    assert_eq!(app.agents.approvals.len(), 1, "preview keeps approval queued");
    assert!(first_reply.try_recv().is_err(), "preview must not resolve reply");

    let preview = app
        .agents
        .approvals
        .front()
        .and_then(|prompt| prompt.deny_confirmation_preview())
        .expect("deny preview");
    assert_eq!(
        preview.workspace,
        TrustStore::at(&state_dir, temp.path()).expect("trust store").workspace().as_string()
    );
    assert_eq!(preview.agent, AGENT);
    assert_eq!(preview.expires, "never");
    assert!(preview.matcher_fields.contains(&("kind".into(), "command".into())));
    assert!(preview.matcher_fields.contains(&("executable".into(), COMMAND.into())));
    assert!(preview.matcher_fields.contains(&("arguments".into(), "exact · 2 tokens".into())));

    app.agents.layout = AgentPaneLayout::Right;
    app.mode = Mode::Agent;
    let screen = render_screen_rows(&app, 160, 40).join("\n");
    for expected in [
        "effect: deny",
        "workspace:",
        &format!("agent: {AGENT}"),
        "kind: command",
        &format!("executable: {COMMAND}"),
        "arguments: exact · 2 tokens",
        "expires: never",
    ] {
        assert!(screen.contains(expected), "missing {expected:?} from preview:\n{screen}");
    }

    app.confirm_bridge_approval_for_test(ApprovalChoice::DenyPersistent);
    assert!(app.agents.approvals.is_empty());

    let store = TrustStore::at(&state_dir, temp.path()).expect("trust store");
    let document = store.load().expect("persisted store must load before denied reply is observed");
    assert_eq!(document.rules.len(), 1);
    let TrustRule::Command(rule) = &document.rules[0] else {
        panic!("expected command deny: {:?}", document.rules);
    };
    assert_eq!(rule.effect, TrustEffect::Deny);
    assert_eq!(rule.scope.agent.as_deref(), Some(AGENT));
    assert_eq!(rule.scope.expires_at, None);
    assert_eq!(rule.scope.max_uses, None);
    let rule_id = rule.id.clone();
    let reason = assert_denied(&mut first_reply);
    assert!(reason.contains(&rule_id), "reply must identify saved rule: {reason}");

    let fingerprint = format!("terminal:{COMMAND}\u{1f}{}", ARGS.join("\u{1f}"));
    app.agents.approval_policy.record(SESSION, &fingerprint, SessionChoice::Allow);
    app.agents.approval_modes.insert(SESSION.to_string(), ToolApprovalMode::Bypass);

    let mut repeated_reply = queue_request(&mut app, &secret_cwd);
    assert!(app.agents.approvals.is_empty(), "matching deny must not queue approval");
    let repeated_reason = assert_denied(&mut repeated_reply);
    assert!(repeated_reason.contains(&rule_id));
    assert_eq!(app.agents.terminals.tracked_count(), 0, "deny must not spawn command");

    let matched_audit = app
        .agents_action_log()
        .iter()
        .rev()
        .find(|entry| {
            matches!(
                entry,
                ActionLogEntry::TrustDecision {
                    rule_id: Some(id),
                    category: TrustCategory::Execute,
                    reason: DecisionReason::PersistentDeny,
                    ..
                } if id == &rule_id
            )
        })
        .expect("matched persistent deny audit");
    let audit = format!("{matched_audit:?}");
    assert!(audit.contains(&rule_id));
    assert!(audit.contains("PersistentDeny"));
    assert!(audit.contains("Execute"));
    for secret in [ARGS[0], ARGS[1], ENV[0].1, "PATH_SECRET_6f53"] {
        assert!(!audit.contains(secret), "audit leaked {secret:?}: {audit}");
    }
}

#[test]
fn persistent_deny_persistence_failure_still_denies_and_reports_not_saved() {
    let (mut app, temp, state_dir) = app_with_store();
    fs::remove_dir(&state_dir).expect("remove state directory");
    fs::write(&state_dir, "not a directory").expect("blocking state file");

    let mut reply = queue_request(&mut app, temp.path());
    app.confirm_bridge_approval_for_test(ApprovalChoice::DenyPersistent);
    assert!(reply.try_recv().is_err(), "preview must not resolve reply");
    app.confirm_bridge_approval_for_test(ApprovalChoice::DenyPersistent);

    let reason = assert_denied(&mut reply);
    assert!(reason.contains("workspace deny rule was not saved"));
    assert_eq!(
        app.backend.status_message.as_deref(),
        Some("operation denied; workspace deny rule was not saved")
    );
    assert!(app.agents.approvals.is_empty());
    assert_eq!(app.agents.terminals.tracked_count(), 0);
}

#[test]
fn persistent_deny_external_store_change_requires_explicit_reload() {
    let (mut app, temp, state_dir) = app_with_store();
    app.reload_workspace_trust_store().expect("cache empty initial store");
    let rule =
        seed_command_deny(&state_dir, temp.path(), "cmd_external_persistent_deny", Some(AGENT));

    let mut before_reload = queue_request(&mut app, temp.path());
    assert_eq!(app.agents.approvals.len(), 1, "disk change must stay inactive");
    app.confirm_bridge_approval_for_test(ApprovalChoice::DenyOnce);
    assert_denied(&mut before_reload);

    app.reload_workspace_trust_store().expect("explicit reload");
    let mut after_reload = queue_request(&mut app, temp.path());
    assert!(app.agents.approvals.is_empty(), "reloaded deny must resolve without UI");
    let reason = assert_denied(&mut after_reload);
    assert!(reason.contains(rule.id()));
    assert_eq!(app.agents.terminals.tracked_count(), 0);
}

#[test]
fn persistent_deny_matching_write_dispatches_nothing_and_queues_no_approval() {
    let (mut app, temp, state_dir) = app_with_store();
    let directory = temp.path().join("write-targets");
    fs::create_dir(&directory).expect("write target directory");
    let target = directory.join("blocked.txt");
    fs::write(&target, "original\n").expect("write original file");
    let store = TrustStore::at(&state_dir, temp.path()).expect("trust store");
    let id = "write_dispatch_persistent_deny";
    seed_rule(
        &mut app,
        &state_dir,
        temp.path(),
        TrustRule::Write(WriteRule {
            id: id.into(),
            effect: TrustEffect::Deny,
            scope: deny_scope(&store, None),
            operation: WriteOperationKind::Modify,
            path_prefix: PathPrefix::parse("write-targets").expect("path prefix"),
            max_files: 0,
            max_total_bytes: 0,
            max_file_bytes: 0,
        }),
    );

    let mut reply = app.queue_write_approval_for_test(target.clone(), "changed\n");

    assert!(app.agents.approvals.is_empty(), "matching deny must not queue approval");
    assert!(assert_denied(&mut reply).contains(id));
    assert_eq!(fs::read_to_string(target).expect("read target"), "original\n");
}

#[test]
fn persistent_deny_matching_filesystem_dispatches_nothing_and_queues_no_approval() {
    let (mut app, temp, state_dir) = app_with_store();
    let target = temp.path().join("blocked-directory");
    let store = TrustStore::at(&state_dir, temp.path()).expect("trust store");
    let id = "filesystem_dispatch_persistent_deny";
    seed_rule(
        &mut app,
        &state_dir,
        temp.path(),
        TrustRule::filesystem(FilesystemRule {
            id: id.into(),
            effect: TrustEffect::Deny,
            scope: deny_scope(&store, None),
            operations: vec![FilesystemOperationKind::Create],
            path_prefix: PathPrefix::parse("blocked-directory").expect("path prefix"),
        }),
    );

    let mut reply = app.queue_filesystem_create_approval_for_test(target.clone());

    assert!(app.agents.approvals.is_empty(), "matching deny must not queue approval");
    assert!(assert_denied(&mut reply).contains(id));
    assert!(!target.exists(), "filesystem executor must not run");
}

#[test]
fn persistent_deny_matching_network_dispatches_nothing_and_queues_no_approval() {
    let (mut app, temp, state_dir) = app_with_store();
    let store = TrustStore::at(&state_dir, temp.path()).expect("trust store");
    let id = "network_dispatch_persistent_deny";
    seed_rule(
        &mut app,
        &state_dir,
        temp.path(),
        TrustRule::Network(
            NetworkRule::deny(
                id.into(),
                deny_scope(&store, None),
                NetworkScheme::Https,
                "deny.example".into(),
                HostMatchMode::Exact,
                443,
                NetworkMethodClass::Read,
                BrowserActionClass::Fetch,
            )
            .expect("network deny"),
        ),
    );
    App::reset_web_dispatch_count_for_test();

    let mut reply = app.queue_network_fetch_approval_for_test("deny.example");

    assert!(app.agents.approvals.is_empty(), "matching deny must not queue approval");
    assert!(assert_denied(&mut reply).contains(id));
    assert_eq!(App::web_dispatch_count_for_test(), 0, "network transport must not dispatch");
}

#[test]
fn persistent_deny_matching_generic_mcp_dispatches_nothing_and_queues_no_approval() {
    let (mut app, temp, state_dir) = app_with_store();
    let directory = temp.path().join("mcp-targets");
    fs::create_dir(&directory).expect("MCP target directory");
    let target = directory.join("blocked.rs");
    fs::write(&target, "fn original() {}\n").expect("write original file");
    let store = TrustStore::at(&state_dir, temp.path()).expect("trust store");
    let id = "mcp_dispatch_persistent_deny";
    seed_rule(
        &mut app,
        &state_dir,
        temp.path(),
        TrustRule::mcp_deny(McpDenyRule {
            id: id.into(),
            effect: TrustEffect::Deny,
            scope: deny_scope(&store, None),
            server: "ee".into(),
            transport_identity: "stdio:ee --mcp-proxy".into(),
            tool: "ee_format_file".into(),
            tool_schema_version: ee_mcp::EE_TOOL_SCHEMA_VERSION,
            category: Some(TrustCategory::WriteModify),
        }),
    );

    let mut reply =
        app.queue_generic_mcp_write_approval_for_test(target.clone(), "fn dispatched() {}\n");

    assert!(app.agents.approvals.is_empty(), "matching deny must not queue approval");
    assert!(assert_denied(&mut reply).contains(id));
    assert_eq!(
        fs::read_to_string(target).expect("read MCP target"),
        "fn original() {}\n",
        "generic MCP write dispatcher must not run"
    );
}
