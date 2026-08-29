//! Focused Phase 13 trust manager, tester, and explainability regressions.

use std::fs;
use std::time::{Duration, SystemTime};

use tempfile::TempDir;

use crate::policy::evaluator::{PolicyInput, TraceStatus, evaluate};
use crate::policy::manager::{
    RuleMutation, inspect_rule, mutate_rule, summarize_rules, test_policy,
};
use crate::policy::session::{SessionChoice, SessionPolicy};
use crate::policy::{
    CommandRule, DecisionReason, FallbackEffect, MatchMode, OperationIdentity, TransportKind,
    TrustCategory, TrustEffect, TrustOperation, TrustOutcome, TrustRule, TrustRuleScope,
    TrustStore, UsageSnapshot,
};

const SESSION: &str = "manager-session";
const FINGERPRINT: &str = "manager-operation";

fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_788_019_200)
}

fn setup() -> (TempDir, TempDir, TrustStore) {
    let workspace = tempfile::tempdir().expect("workspace");
    let state = tempfile::tempdir().expect("state");
    let store = TrustStore::at(state.path(), workspace.path()).expect("store");
    (workspace, state, store)
}

fn operation(store: &TrustStore) -> TrustOperation {
    TrustOperation {
        workspace: *store.workspace(),
        agent: Some("agent-a".into()),
        transport: TransportKind::Acp,
        category: TrustCategory::Execute,
        identity: OperationIdentity::Command {
            executable: "git".into(),
            argv: vec!["status".into(), "--short".into()],
        },
    }
}

fn rule(
    store: &TrustStore,
    id: &str,
    effect: TrustEffect,
    expires_at: Option<SystemTime>,
) -> TrustRule {
    TrustRule::Command(CommandRule {
        id: id.into(),
        effect,
        scope: TrustRuleScope {
            workspace: *store.workspace(),
            agent: Some("agent-a".into()),
            expires_at,
            max_uses: (effect == TrustEffect::Allow).then_some(5),
        },
        executable: "git".into(),
        match_mode: MatchMode::ArgvExact,
        argv: vec!["status".into(), "--short".into()],
    })
}

fn input<'a>(
    operation: &'a TrustOperation,
    rules: &'a [TrustRule],
    session: &'a SessionPolicy,
    usage: &'a UsageSnapshot,
) -> PolicyInput<'a> {
    PolicyInput {
        session_id: SESSION,
        fingerprint: FINGERPRINT,
        operation,
        session,
        rules,
        now: now(),
        usage,
        workspace_enabled: true,
        built_in_deny: None,
        tool_default: None,
        category_default: Some(FallbackEffect::Confirm),
        global_default: Some(FallbackEffect::Confirm),
    }
}

#[test]
fn trust_rule_manager_disable_enable_and_revoke_take_effect_after_durable_write() {
    let (_workspace, _state, store) = setup();
    store.add_rule(rule(&store, "manager_deny", TrustEffect::Deny, None)).expect("seed");
    let operation = operation(&store);
    let session = SessionPolicy::default();
    let usage = UsageSnapshot::default();
    assert_eq!(
        evaluate(&input(&operation, &store.effective_at(now()).rules, &session, &usage)).outcome,
        TrustOutcome::Deny
    );

    mutate_rule(&store, "manager_deny", RuleMutation::Disable, now()).expect("disable");
    assert_eq!(
        evaluate(&input(&operation, &store.effective_at(now()).rules, &session, &usage)).outcome,
        TrustOutcome::Confirm
    );
    let managed = store.load_for_management_at(now()).expect("managed load");
    assert!(!managed.state("manager_deny").expect("state").enabled);

    mutate_rule(&store, "manager_deny", RuleMutation::Enable, now()).expect("enable");
    assert_eq!(
        evaluate(&input(&operation, &store.effective_at(now()).rules, &session, &usage)).outcome,
        TrustOutcome::Deny
    );

    mutate_rule(&store, "manager_deny", RuleMutation::Revoke, now()).expect("revoke");
    assert!(store.load_for_management_at(now()).expect("managed").document.rules.is_empty());
    assert_eq!(
        evaluate(&input(&operation, &store.effective_at(now()).rules, &session, &usage)).outcome,
        TrustOutcome::Confirm
    );
}

#[test]
fn trust_rule_manager_displays_expired_exhausted_and_redacted_metadata() {
    let (_workspace, _state, store) = setup();
    store
        .add_rule(rule(
            &store,
            "manager_expired",
            TrustEffect::Allow,
            Some(now() - Duration::from_secs(1)),
        ))
        .expect("seed expired");
    let managed = store.load_for_management_at(now()).expect("unfiltered manager load");
    assert_eq!(managed.document.rules.len(), 1);
    assert!(store.effective_at(now()).rules.is_empty());
    let usage = UsageSnapshot::new([("manager_expired".to_string(), 5)].into_iter().collect());
    let summary = inspect_rule(&managed, &usage, "manager_expired").expect("summary");
    assert_eq!(summary.remaining_uses, "0");
    assert_eq!(summary.matcher, "command-structured");
    let display = summary.display();
    assert!(!display.contains("git"));
    assert!(!display.contains("--short"));
    assert!(display.contains("last-safe-usage:successful-dispatch-count:5"));
    assert_eq!(summarize_rules(&managed, &usage), vec![summary]);
}

#[test]
fn trust_rule_manager_rejects_stale_ids_and_preserves_concurrent_additions() {
    let (_workspace, _state, store_a) = setup();
    let store_b = TrustStore::at(
        store_a.path().parent().expect("trust dir").parent().expect("state dir"),
        _workspace.path(),
    )
    .expect("second handle");
    let stale = mutate_rule(&store_a, "missing_rule", RuleMutation::Revoke, now())
        .expect_err("stale id rejected");
    assert!(stale.to_string().contains("stale rule id"));

    store_a.add_rule(rule(&store_a, "concurrent_a", TrustEffect::Deny, None)).expect("first add");
    store_b
        .add_rule(rule(&store_b, "concurrent_b", TrustEffect::Confirm, None))
        .expect("second add reloads latest");
    let managed = store_a.load_for_management_at(now()).expect("reload");
    let ids = managed.document.rules.iter().map(TrustRule::id).collect::<Vec<_>>();
    assert_eq!(ids, vec!["concurrent_a", "concurrent_b"]);
}

#[cfg(unix)]
#[test]
fn trust_rule_manager_atomic_failure_keeps_original_bytes_and_effective_policy() {
    use std::os::unix::fs::PermissionsExt;

    let (_workspace, _state, store) = setup();
    store.add_rule(rule(&store, "atomic_deny", TrustEffect::Deny, None)).expect("seed");
    let original = fs::read(store.path()).expect("source bytes");
    let trust_dir = store.path().parent().expect("trust dir");
    fs::set_permissions(trust_dir, fs::Permissions::from_mode(0o500)).expect("make unsafe");
    let result = mutate_rule(&store, "atomic_deny", RuleMutation::Revoke, now());
    fs::set_permissions(trust_dir, fs::Permissions::from_mode(0o700)).expect("restore");
    assert!(result.is_err());
    assert_eq!(fs::read(store.path()).expect("unchanged bytes"), original);
    assert_eq!(store.effective_at(now()).rules[0].id(), "atomic_deny");
}

#[test]
fn trust_rule_manager_tester_matches_evaluator_without_mutating_inputs() {
    let (_workspace, _state, store) = setup();
    let deny = rule(&store, "trace_deny", TrustEffect::Deny, None);
    let allow =
        rule(&store, "trace_allow", TrustEffect::Allow, Some(now() + Duration::from_secs(3600)));
    let operation = operation(&store);
    let mut session = SessionPolicy::default();
    session.record(SESSION, FINGERPRINT, SessionChoice::Allow);
    let usage = UsageSnapshot::new([("trace_allow".to_string(), 2)].into_iter().collect());
    let rules = vec![allow, deny];

    let before_lookup = session.lookup(SESSION, FINGERPRINT);
    let before_used = usage.used("trace_allow");
    let tester = test_policy(&input(&operation, &rules, &session, &usage));
    let real = evaluate(&input(&operation, &rules, &session, &usage));

    assert_eq!(tester.decision, real);
    assert_eq!(tester.decision.reason, DecisionReason::PersistentDeny);
    assert_eq!(session.lookup(SESSION, FINGERPRINT), before_lookup);
    assert_eq!(usage.used("trace_allow"), before_used);
    assert_eq!(tester.trace.len(), 10);
    assert_eq!(
        tester
            .trace
            .iter()
            .map(|step| (step.layer, step.status, step.rule_id.as_deref()))
            .collect::<Vec<_>>(),
        vec![
            ("built_in_deny", TraceStatus::NoMatch, None),
            ("persistent_deny", TraceStatus::Matched, Some("trace_deny")),
            ("session_deny", TraceStatus::NotReached, None),
            ("mandatory_confirm", TraceStatus::NotReached, None),
            ("workspace_gate", TraceStatus::NotReached, None),
            ("session_allow", TraceStatus::NotReached, None),
            ("bounded_persistent_allow", TraceStatus::NotReached, None),
            ("tool_default", TraceStatus::NotReached, None),
            ("category_default", TraceStatus::NotReached, None),
            ("global_default", TraceStatus::NotReached, None),
        ]
    );
}
