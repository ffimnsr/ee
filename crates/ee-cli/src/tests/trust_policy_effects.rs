use std::fs;
use std::path::Path;
use std::time::SystemTime;

use tempfile::TempDir;

use crate::policy::evaluator::{PolicyInput, evaluate};
use crate::policy::rules::{CommandRule, MatchMode, TrustRule};
use crate::policy::session::{SessionChoice, SessionPolicy};
use crate::policy::store::{TrustStore, TrustStoreError};
use crate::policy::{
    DecisionReason, FallbackEffect, OperationIdentity, SafeguardCategory, SafeguardMatch,
    TransportKind, TrustCategory, TrustEffect, TrustOperation, TrustOutcome, TrustRuleScope,
    UsageSnapshot, WorkspaceIdentity,
};

fn at(text: &str) -> SystemTime {
    chrono::DateTime::parse_from_rfc3339(text)
        .expect("valid RFC3339")
        .with_timezone(&chrono::Utc)
        .into()
}

fn identity(bytes: &[u8]) -> WorkspaceIdentity {
    WorkspaceIdentity::from_canonical_root_bytes(bytes)
}

fn operation(workspace: WorkspaceIdentity) -> TrustOperation {
    TrustOperation {
        workspace,
        agent: Some("agent-a".into()),
        transport: TransportKind::Acp,
        category: TrustCategory::Execute,
        identity: OperationIdentity::Command {
            executable: "git".into(),
            argv: vec!["status".into()],
        },
    }
}

fn rule(workspace: WorkspaceIdentity, id: &str, effect: TrustEffect) -> TrustRule {
    TrustRule::Command(CommandRule {
        id: id.into(),
        effect,
        scope: TrustRuleScope {
            workspace,
            agent: Some("agent-a".into()),
            expires_at: Some(at("2026-08-08T12:00:00Z")),
            max_uses: (effect == TrustEffect::Allow).then_some(20),
        },
        executable: "git".into(),
        match_mode: MatchMode::ArgvExact,
        argv: vec!["status".into()],
    })
}

fn decide(
    operation: &TrustOperation,
    rules: &[TrustRule],
    session: &SessionPolicy,
    built_in_deny: bool,
    tool_default: Option<FallbackEffect>,
    global_default: Option<FallbackEffect>,
) -> crate::policy::TrustDecision {
    evaluate(&PolicyInput {
        session_id: "session-a",
        fingerprint: "git-status",
        operation,
        session,
        rules,
        now: at("2026-08-07T12:00:00Z"),
        usage: &UsageSnapshot::default(),
        workspace_enabled: true,
        built_in_deny: built_in_deny.then_some(SafeguardMatch {
            rule_id: "builtin.v1.test",
            category: SafeguardCategory::CatastrophicDeletion,
        }),
        tool_default,
        category_default: None,
        global_default,
    })
}

#[derive(Clone, Copy)]
enum PolicySource {
    BuiltInDeny,
    PersistentDeny,
    SessionDeny,
    Confirm,
    SessionAllow,
    PersistentAllow,
    ToolDefault,
    GlobalDefault,
}

fn decide_sources(
    operation: &TrustOperation,
    workspace: WorkspaceIdentity,
    sources: &[PolicySource],
) -> crate::policy::TrustDecision {
    let mut rules = Vec::new();
    let mut session = SessionPolicy::default();
    let mut built_in_deny = false;
    let mut tool_default = None;
    let mut global_default = None;
    for source in sources {
        match source {
            PolicySource::BuiltInDeny => built_in_deny = true,
            PolicySource::PersistentDeny => {
                rules.push(rule(workspace, "deny", TrustEffect::Deny));
            }
            PolicySource::SessionDeny => {
                session.record("session-a", "git-status", SessionChoice::Deny);
            }
            PolicySource::Confirm => {
                rules.push(rule(workspace, "confirm", TrustEffect::Confirm));
            }
            PolicySource::SessionAllow => {
                session.record("session-a", "git-status", SessionChoice::Allow);
            }
            PolicySource::PersistentAllow => {
                rules.push(rule(workspace, "allow", TrustEffect::Allow));
            }
            PolicySource::ToolDefault => tool_default = Some(FallbackEffect::Confirm),
            PolicySource::GlobalDefault => global_default = Some(FallbackEffect::Deny),
        }
    }
    decide(operation, &rules, &session, built_in_deny, tool_default, global_default)
}

fn source_result(source: PolicySource) -> (TrustOutcome, DecisionReason) {
    match source {
        PolicySource::BuiltInDeny => (TrustOutcome::Deny, DecisionReason::BuiltInDeny),
        PolicySource::PersistentDeny => (TrustOutcome::Deny, DecisionReason::PersistentDeny),
        PolicySource::SessionDeny => (TrustOutcome::Deny, DecisionReason::SessionDeny),
        PolicySource::Confirm => (TrustOutcome::Confirm, DecisionReason::MandatoryConfirm),
        PolicySource::SessionAllow => (TrustOutcome::Allow, DecisionReason::SessionAllow),
        PolicySource::PersistentAllow => (TrustOutcome::Allow, DecisionReason::PersistentAllow),
        PolicySource::ToolDefault => (TrustOutcome::Confirm, DecisionReason::ToolDefaultConfirm),
        PolicySource::GlobalDefault => (TrustOutcome::Deny, DecisionReason::GlobalDefaultDeny),
    }
}

#[test]
fn trust_policy_effects_precedence_is_exact_and_order_independent() {
    let workspace = identity(b"/workspace");
    let operation = operation(workspace);
    let allow = rule(workspace, "z_allow", TrustEffect::Allow);
    let confirm = rule(workspace, "m_confirm", TrustEffect::Confirm);
    let deny = rule(workspace, "a_deny", TrustEffect::Deny);
    let mut session = SessionPolicy::default();
    session.record("session-a", "git-status", SessionChoice::Allow);
    session.record("session-a", "git-status", SessionChoice::Deny);

    let decision = decide(
        &operation,
        &[allow.clone(), confirm.clone(), deny.clone()],
        &session,
        true,
        Some(FallbackEffect::Deny),
        Some(FallbackEffect::Deny),
    );
    assert_eq!(
        (decision.outcome, decision.reason),
        (TrustOutcome::Deny, DecisionReason::BuiltInDeny)
    );

    let decision = decide(
        &operation,
        &[allow.clone(), confirm.clone(), deny],
        &session,
        false,
        Some(FallbackEffect::Deny),
        Some(FallbackEffect::Deny),
    );
    assert_eq!(
        (decision.outcome, decision.reason),
        (TrustOutcome::Deny, DecisionReason::PersistentDeny)
    );

    let decision = decide(
        &operation,
        &[allow.clone(), confirm.clone()],
        &session,
        false,
        Some(FallbackEffect::Deny),
        Some(FallbackEffect::Deny),
    );
    assert_eq!(
        (decision.outcome, decision.reason),
        (TrustOutcome::Deny, DecisionReason::SessionDeny)
    );

    let mut allow_session = SessionPolicy::default();
    allow_session.record("session-a", "git-status", SessionChoice::Allow);
    let decision = decide(
        &operation,
        &[allow.clone(), confirm],
        &allow_session,
        false,
        Some(FallbackEffect::Deny),
        Some(FallbackEffect::Deny),
    );
    assert_eq!(
        (decision.outcome, decision.reason),
        (TrustOutcome::Confirm, DecisionReason::MandatoryConfirm)
    );

    let decision = decide(
        &operation,
        std::slice::from_ref(&allow),
        &allow_session,
        false,
        Some(FallbackEffect::Deny),
        Some(FallbackEffect::Deny),
    );
    assert_eq!(
        (decision.outcome, decision.reason),
        (TrustOutcome::Allow, DecisionReason::SessionAllow)
    );

    let decision = decide(
        &operation,
        &[allow],
        &SessionPolicy::default(),
        false,
        Some(FallbackEffect::Deny),
        Some(FallbackEffect::Deny),
    );
    assert_eq!(
        (decision.outcome, decision.reason),
        (TrustOutcome::Allow, DecisionReason::PersistentAllow)
    );

    let decision = decide(
        &operation,
        &[],
        &SessionPolicy::default(),
        false,
        Some(FallbackEffect::Confirm),
        Some(FallbackEffect::Deny),
    );
    assert_eq!(
        (decision.outcome, decision.reason),
        (TrustOutcome::Confirm, DecisionReason::ToolDefaultConfirm)
    );

    let decision =
        decide(&operation, &[], &SessionPolicy::default(), false, None, Some(FallbackEffect::Deny));
    assert_eq!(
        (decision.outcome, decision.reason),
        (TrustOutcome::Deny, DecisionReason::GlobalDefaultDeny)
    );

    let ordered = [
        PolicySource::BuiltInDeny,
        PolicySource::PersistentDeny,
        PolicySource::SessionDeny,
        PolicySource::Confirm,
        PolicySource::SessionAllow,
        PolicySource::PersistentAllow,
        PolicySource::ToolDefault,
        PolicySource::GlobalDefault,
    ];
    for (higher_index, higher) in ordered.iter().copied().enumerate() {
        for lower in ordered.iter().copied().skip(higher_index + 1) {
            let forward = decide_sources(&operation, workspace, &[higher, lower]);
            let reverse = decide_sources(&operation, workspace, &[lower, higher]);
            assert_eq!((forward.outcome, forward.reason), source_result(higher));
            assert_eq!(reverse, forward, "input order must not affect precedence");
        }
    }
}

#[test]
fn trust_policy_effects_deny_and_confirm_ignore_allow_usage_budget() {
    let workspace = identity(b"/workspace");
    let operation = operation(workspace);
    for effect in [TrustEffect::Deny, TrustEffect::Confirm] {
        let mut candidate = rule(workspace, "rule", effect);
        match &mut candidate {
            TrustRule::Command(rule) => rule.scope.max_uses = None,
            _ => unreachable!(),
        }
        let decision =
            decide(&operation, &[candidate], &SessionPolicy::default(), false, None, None);
        assert_eq!(
            decision.outcome,
            if effect == TrustEffect::Deny { TrustOutcome::Deny } else { TrustOutcome::Confirm }
        );
    }
}

fn store_setup() -> (TempDir, TempDir, TrustStore) {
    let state = TempDir::new().expect("state");
    let workspace = TempDir::new().expect("workspace");
    let store = TrustStore::at(state.path(), workspace.path()).expect("store");
    (state, workspace, store)
}

fn write_private(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("directory");
    fs::write(path, text).expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
}

#[test]
fn trust_policy_effects_v1_migration_preserves_authority_fields() {
    let (_state, _workspace, store) = store_setup();
    let legacy = format!(
        r#"schema_version = 1

[workspace]
identity = "{}"

[policy]
workspace_enabled = true

[[command_allow]]
id = "stable-id"
agent = "agent-a"
executable = "git"
match = "argv_exact"
argv = ["status"]
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20
"#,
        store.workspace().as_string()
    );
    write_private(store.path(), &legacy);

    let document = store.load_at(at("2026-08-07T12:00:00Z")).expect("migrate");
    assert!(document.workspace_enabled);
    assert_eq!(document.global_default, FallbackEffect::Confirm);
    assert_eq!(document.rules.len(), 1);
    let TrustRule::Command(migrated) = &document.rules[0] else { panic!("command") };
    assert_eq!(migrated.id, "stable-id");
    assert_eq!(migrated.effect, TrustEffect::Allow);
    assert_eq!(migrated.scope.agent.as_deref(), Some("agent-a"));
    assert_eq!(migrated.scope.expires_at, Some(at("2026-08-08T12:00:00Z")));
    assert_eq!(migrated.scope.max_uses, Some(20));
    assert_eq!(migrated.executable, "git");
    assert_eq!(migrated.argv, ["status"]);

    let migrated_text = fs::read_to_string(store.path()).unwrap();
    assert!(migrated_text.contains("schema_version = 2"));
    assert!(migrated_text.contains("[[command_rules]]"));
    assert!(migrated_text.contains("effect = \"allow\""));
    assert!(!migrated_text.contains("command_allow"));
}

#[test]
fn trust_policy_effects_failed_migration_preserves_original_bytes() {
    let (_state, _workspace, store) = store_setup();
    let corrupt = format!(
        r#"schema_version = 1

[workspace]
identity = "{}"

[policy]
workspace_enabled = true

[[command_allow]]
id = "bad"
executable = "git"
match = "contains"
argv = ["status"]
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20
"#,
        store.workspace().as_string()
    );
    write_private(store.path(), &corrupt);
    let before = fs::read(store.path()).unwrap();

    assert!(matches!(
        store.load_at(at("2026-08-07T12:00:00Z")),
        Err(TrustStoreError::ValidationFailure(_))
    ));
    assert_eq!(fs::read(store.path()).unwrap(), before);
    let effective = store.effective_at(at("2026-08-07T12:00:00Z"));
    assert!(effective.rules.is_empty());
    assert_eq!(effective.global_default, FallbackEffect::Confirm);
}

#[test]
fn trust_policy_effects_schema_rejects_allow_defaults_and_incompatible_fields() {
    let (_state, _workspace, store) = store_setup();
    let invalid_default = format!(
        r#"schema_version = 2

[workspace]
identity = "{}"

[policy]
workspace_enabled = true
global_default = "allow"
"#,
        store.workspace().as_string()
    );
    write_private(store.path(), &invalid_default);
    assert!(matches!(store.load(), Err(TrustStoreError::ValidationFailure(_))));

    let incompatible = format!(
        r#"schema_version = 2

[workspace]
identity = "{}"

[policy]
workspace_enabled = true
global_default = "confirm"

[[command_rules]]
id = "deny"
effect = "deny"
executable = "git"
match = "argv_exact"
argv = ["status"]
max_uses = 20
"#,
        store.workspace().as_string()
    );
    write_private(store.path(), &incompatible);
    assert!(matches!(store.load(), Err(TrustStoreError::ValidationFailure(_))));
}
