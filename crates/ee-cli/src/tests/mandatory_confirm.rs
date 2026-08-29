//! Focused Phase 10 mandatory-confirm and safe-default regressions.

use std::fs;
use std::path::Path;
#[cfg(feature = "agents")]
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

#[cfg(feature = "agents")]
use tempfile::TempDir;

#[cfg(feature = "agents")]
use crate::app::{AgentPaneLayout, App, ApprovalChoice, Mode, ToolApprovalMode};
use crate::policy::evaluator::{PolicyInput, evaluate};
use crate::policy::session::{SessionChoice, SessionPolicy};
use crate::policy::templates;
use crate::policy::{
    CategoryDefaultRule, CommandRule, DecisionReason, FallbackEffect, FilesystemOperationKind,
    FilesystemRule, MatchMode, McpDenyRule, OperationIdentity, PathPrefix, ToolDefaultRule,
    ToolRule, ToolRuleIdentity, TransportKind, TrustCategory, TrustEffect, TrustOperation,
    TrustOutcome, TrustRule, TrustRuleScope, TrustStore, TrustStoreDocument, UsageSnapshot,
    WorkspaceIdentity,
};
#[cfg(feature = "agents")]
use crate::tests::agent_bridge::{agents_app_in, base_script};
#[cfg(feature = "agents")]
use crate::tests::helpers::render_screen_rows;

const SESSION: &str = "mandatory-confirm-session";
const AGENT: &str = "mandatory-confirm-agent";

fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_786_108_800)
}

fn operation(workspace: WorkspaceIdentity) -> TrustOperation {
    TrustOperation {
        workspace,
        agent: Some(AGENT.into()),
        transport: TransportKind::Acp,
        category: TrustCategory::Execute,
        identity: OperationIdentity::Command {
            executable: "git".into(),
            argv: vec!["push".into(), "origin".into(), "main".into()],
        },
    }
}

fn command_rule(workspace: WorkspaceIdentity, id: &str, effect: TrustEffect) -> TrustRule {
    TrustRule::Command(CommandRule {
        id: id.into(),
        effect,
        scope: TrustRuleScope {
            workspace,
            agent: Some(AGENT.into()),
            expires_at: (effect == TrustEffect::Allow).then_some(now() + Duration::from_secs(3600)),
            max_uses: (effect == TrustEffect::Allow).then_some(20),
        },
        executable: "git".into(),
        match_mode: MatchMode::ArgvExact,
        argv: vec!["push".into(), "origin".into(), "main".into()],
    })
}

#[allow(clippy::too_many_arguments)]
fn decide(
    operation: &TrustOperation,
    rules: &[TrustRule],
    session: &SessionPolicy,
    tool_default: Option<FallbackEffect>,
    category_default: Option<FallbackEffect>,
    global_default: Option<FallbackEffect>,
) -> crate::policy::TrustDecision {
    evaluate(&PolicyInput {
        session_id: SESSION,
        fingerprint: "git-push",
        operation,
        session,
        rules,
        now: now(),
        usage: &UsageSnapshot::default(),
        workspace_enabled: true,
        built_in_deny: None,
        tool_default,
        category_default,
        global_default,
    })
}

#[test]
fn mandatory_confirm_overrides_session_and_bounded_persistent_allow_every_time() {
    let workspace = WorkspaceIdentity::from_canonical_root_bytes(b"/phase10");
    let operation = operation(workspace);
    let confirm = command_rule(workspace, "confirm_push", TrustEffect::Confirm);
    let allow = command_rule(workspace, "allow_push", TrustEffect::Allow);
    let mut session = SessionPolicy::default();
    session.record(SESSION, "git-push", SessionChoice::Allow);

    for _ in 0..2 {
        let decision = decide(
            &operation,
            &[allow.clone(), confirm.clone()],
            &session,
            Some(FallbackEffect::Deny),
            Some(FallbackEffect::Deny),
            Some(FallbackEffect::Deny),
        );
        assert_eq!(decision.outcome, TrustOutcome::Confirm);
        assert_eq!(decision.reason, DecisionReason::MandatoryConfirm);
        assert_eq!(decision.rule_id.as_deref(), Some("confirm_push"));
    }
}

#[test]
fn mandatory_confirm_reuses_exact_mcp_filesystem_and_tool_matchers() {
    let workspace = WorkspaceIdentity::from_canonical_root_bytes(b"/phase10-matchers");
    let scope = || TrustRuleScope {
        workspace,
        agent: Some(AGENT.into()),
        expires_at: None,
        max_uses: None,
    };
    let fixtures = [
        (
            TrustRule::mcp_deny(McpDenyRule {
                id: "confirm_mcp".into(),
                effect: TrustEffect::Confirm,
                scope: scope(),
                server: "ee".into(),
                transport_identity: "stdio:ee --mcp-proxy".into(),
                tool: "ee_format_file".into(),
                tool_schema_version: 1,
                category: Some(TrustCategory::WriteModify),
            }),
            TrustOperation {
                workspace,
                agent: Some(AGENT.into()),
                transport: TransportKind::McpStdio,
                category: TrustCategory::WriteModify,
                identity: OperationIdentity::Mcp {
                    server: "ee".into(),
                    transport_identity: "stdio:ee --mcp-proxy".into(),
                    tool: "ee_format_file".into(),
                    tool_schema_version: 1,
                    arguments_json: "{}".into(),
                },
            },
        ),
        (
            TrustRule::filesystem(FilesystemRule {
                id: "confirm_delete".into(),
                effect: TrustEffect::Confirm,
                scope: scope(),
                operations: vec![FilesystemOperationKind::Delete],
                path_prefix: PathPrefix::parse("generated").expect("prefix"),
            }),
            TrustOperation {
                workspace,
                agent: Some(AGENT.into()),
                transport: TransportKind::Acp,
                category: TrustCategory::Delete,
                identity: OperationIdentity::filesystem(
                    FilesystemOperationKind::Delete,
                    Some("generated/output.txt"),
                    None,
                )
                .expect("filesystem identity"),
            },
        ),
        (
            TrustRule::tool(ToolRule {
                id: "confirm_native_tool".into(),
                effect: TrustEffect::Confirm,
                scope: scope(),
                identity: ToolRuleIdentity::Native { tool: "fs/write_text_file".into() },
                category: Some(TrustCategory::WriteModify),
            }),
            TrustOperation {
                workspace,
                agent: Some(AGENT.into()),
                transport: TransportKind::Acp,
                category: TrustCategory::WriteModify,
                identity: OperationIdentity::native_tool("fs/write_text_file")
                    .expect("native identity"),
            },
        ),
    ];

    for (rule, operation) in fixtures {
        let decision = decide(
            &operation,
            std::slice::from_ref(&rule),
            &SessionPolicy::default(),
            None,
            None,
            Some(FallbackEffect::Deny),
        );
        assert_eq!(decision.outcome, TrustOutcome::Confirm, "{}", rule.id());
        assert_eq!(decision.reason, DecisionReason::MandatoryConfirm);
    }
}

#[test]
fn mandatory_confirm_default_precedence_is_tool_then_category_then_global() {
    let workspace = WorkspaceIdentity::from_canonical_root_bytes(b"/phase10-defaults");
    let operation = operation(workspace);
    let session = SessionPolicy::default();

    let tool_deny = decide(
        &operation,
        &[],
        &session,
        Some(FallbackEffect::Deny),
        Some(FallbackEffect::Confirm),
        Some(FallbackEffect::Confirm),
    );
    assert_eq!(tool_deny.reason, DecisionReason::ToolDefaultDeny);

    let tool_confirm = decide(
        &operation,
        &[],
        &session,
        Some(FallbackEffect::Confirm),
        Some(FallbackEffect::Deny),
        Some(FallbackEffect::Deny),
    );
    assert_eq!(tool_confirm.reason, DecisionReason::ToolDefaultConfirm);

    let category_deny = decide(
        &operation,
        &[],
        &session,
        None,
        Some(FallbackEffect::Deny),
        Some(FallbackEffect::Confirm),
    );
    assert_eq!(category_deny.reason, DecisionReason::CategoryDefaultDeny);

    let global_confirm =
        decide(&operation, &[], &session, None, None, Some(FallbackEffect::Confirm));
    assert_eq!(global_confirm.reason, DecisionReason::GlobalDefaultConfirm);
}

#[test]
fn mandatory_confirm_unknown_operation_uses_deny_default_but_never_allows() {
    let workspace = WorkspaceIdentity::from_canonical_root_bytes(b"/phase10-unknown");
    let malformed = TrustOperation {
        workspace,
        agent: None,
        transport: TransportKind::Acp,
        category: TrustCategory::Unknown,
        identity: OperationIdentity::Unknown,
    };
    let session = SessionPolicy::default();

    let denied = decide(&malformed, &[], &session, None, None, Some(FallbackEffect::Deny));
    assert_eq!(denied.outcome, TrustOutcome::Deny);
    assert_eq!(denied.reason, DecisionReason::GlobalDefaultDeny);

    let confirmed = decide(&malformed, &[], &session, None, None, Some(FallbackEffect::Confirm));
    assert_eq!(confirmed.outcome, TrustOutcome::Confirm);
    assert_eq!(confirmed.reason, DecisionReason::UnknownOperation);
}

fn private_write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("trust directory");
    fs::write(path, text).expect("trust fixture");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path.parent().expect("parent"), fs::Permissions::from_mode(0o700))
            .expect("private directory");
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("private file");
    }
}

#[test]
fn mandatory_confirm_template_and_defaults_round_trip_with_explicit_matcher() {
    let temp = tempfile::tempdir().expect("workspace");
    let state = temp.path().join("state");
    let store = TrustStore::at(&state, temp.path()).expect("store");
    let workspace = *store.workspace();
    let rule = TrustRule::with_template(
        templates::VCS_PUSH.into(),
        command_rule(workspace, "confirm_push_template", TrustEffect::Confirm),
    )
    .expect("valid application template");
    store
        .write(&TrustStoreDocument {
            workspace,
            workspace_enabled: true,
            rules: vec![rule],
            tool_defaults: vec![ToolDefaultRule {
                tool: "terminal".into(),
                effect: FallbackEffect::Confirm,
            }],
            category_defaults: vec![CategoryDefaultRule {
                category: TrustCategory::Execute,
                effect: FallbackEffect::Deny,
            }],
            global_default: FallbackEffect::Confirm,
        })
        .expect("write policy");

    let text = fs::read_to_string(store.path()).expect("serialized policy");
    assert!(text.contains("template_id = \"vcs-push-v1\""));
    assert!(text.contains("executable = \"git\""));
    assert!(text.contains("argv = [\"push\", \"origin\", \"main\"]"));

    let loaded = store.load_at(now()).expect("load policy");
    assert_eq!(loaded.rules[0].template_id(), Some(templates::VCS_PUSH));
    assert_eq!(loaded.tool_defaults[0].tool, "terminal");
    assert_eq!(loaded.category_defaults[0].category, TrustCategory::Execute);
}

#[test]
fn mandatory_confirm_schema_rejects_allow_and_unknown_defaults_and_bad_templates() {
    let temp = tempfile::tempdir().expect("workspace");
    let state = temp.path().join("state");
    let store = TrustStore::at(&state, temp.path()).expect("store");
    let identity = store.workspace().as_string();

    for extra in [
        "[[tool_defaults]]\ntool = \"terminal\"\neffect = \"allow\"\n",
        "[[tool_defaults]]\ntool = \"made-up-native-tool\"\neffect = \"confirm\"\n",
        "[[category_defaults]]\ncategory = \"unknown\"\neffect = \"deny\"\n",
    ] {
        private_write(
            store.path(),
            &format!(
                "schema_version = 2\n\n[workspace]\nidentity = \"{identity}\"\n\n[policy]\nworkspace_enabled = true\nglobal_default = \"confirm\"\n\n{extra}"
            ),
        );
        assert!(store.load_at(now()).is_err(), "unsafe default must fail: {extra}");
    }

    let bad = TrustRule::with_template(
        templates::PACKAGE_PUBLISH.into(),
        command_rule(*store.workspace(), "bad_template", TrustEffect::Confirm),
    );
    assert!(bad.is_err(), "template must match explicit resolved fields");
}

#[cfg(feature = "agents")]
fn app_with_store() -> (App, TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("workspace tempdir");
    let (mut app, _fake) = agents_app_in(&temp, base_script());
    let state = temp.path().join("state");
    fs::create_dir_all(&state).expect("state directory");
    app.agents.test_trust_store_base = Some(state.clone());
    (app, temp, state)
}

#[cfg(feature = "agents")]
#[test]
fn mandatory_confirm_prompt_hides_reusable_allows_and_bypass_cannot_skip_it() {
    let (mut app, temp, state) = app_with_store();
    let store = TrustStore::at(&state, temp.path()).expect("store");
    let rule = TrustRule::with_template(
        templates::VCS_PUSH.into(),
        command_rule(*store.workspace(), "confirm_ui_push", TrustEffect::Confirm),
    )
    .expect("template");
    store.add_rule(rule).expect("seed confirm");
    app.reload_workspace_trust_store().expect("reload confirm");

    let fingerprint = "terminal:git\u{1f}push\u{1f}origin\u{1f}main";
    app.agents.approval_policy.record(SESSION, fingerprint, SessionChoice::Allow);
    app.agents.approval_modes.insert(SESSION.into(), ToolApprovalMode::Bypass);

    for _ in 0..2 {
        let mut reply = app.queue_terminal_approval_for_test(
            SESSION,
            Some(AGENT),
            "git",
            &["push", "origin", "main"],
            &[],
            Some(temp.path().to_path_buf()),
        );
        let prompt = app.agents.approvals.front().expect("mandatory prompt");
        assert_eq!(prompt.mandatory_confirmation().expect("context").rule_id, "confirm_ui_push");
        assert!(prompt.options.iter().any(|(_, choice)| *choice == ApprovalChoice::AllowOnce));
        assert!(!prompt.options.iter().any(|(_, choice)| {
            matches!(
                choice,
                ApprovalChoice::AllowSession
                    | ApprovalChoice::AllowPersistent
                    | ApprovalChoice::AllowPersistentShort
                    | ApprovalChoice::AllowPersistentPrefix(_)
                    | ApprovalChoice::AllowPersistentPrefixShort(_)
            )
        }));
        assert!(reply.try_recv().is_err(), "bypass must not resolve mandatory prompt");

        app.agents.layout = AgentPaneLayout::Right;
        app.mode = Mode::Agent;
        let screen = render_screen_rows(&app, 160, 40).join("\n");
        assert!(screen.contains("mandatory confirmation: template vcs-push-v1"));
        assert!(screen.contains("prior session"));

        app.confirm_bridge_approval_for_test(ApprovalChoice::DenyOnce);
        assert!(reply.try_recv().expect("denied reply").is_err());
    }
    assert_eq!(app.agents.terminals.tracked_count(), 0);
}
