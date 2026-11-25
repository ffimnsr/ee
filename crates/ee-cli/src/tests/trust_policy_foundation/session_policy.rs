use super::*;

// ── Session policy contract (shared precedence) ─────────────────────────────

#[test]
fn session_policy_deny_takes_precedence_over_allow() {
    let mut session = SessionPolicy::default();
    session.record("s1", "write:/work/a.txt", SessionChoice::Allow);
    session.record("s1", "write:/work/a.txt", SessionChoice::Deny);
    assert_eq!(
        session.lookup("s1", "write:/work/a.txt"),
        Some(SessionChoice::Deny),
        "deny must win over allow for the same key"
    );
}

#[test]
fn session_policy_is_scoped_by_session_and_fingerprint() {
    let mut session = SessionPolicy::default();
    session.record("s1", "write:/work/a.txt", SessionChoice::Allow);
    assert_eq!(session.lookup("s1", "write:/work/a.txt"), Some(SessionChoice::Allow));
    assert!(session.lookup("s2", "write:/work/a.txt").is_none(), "other session unaffected");
    assert!(session.lookup("s1", "write:/work/b.txt").is_none(), "other fingerprint unaffected");
}

#[test]
fn session_policy_invalidate_clears_session_rows() {
    let mut session = SessionPolicy::default();
    session.record("s1", "a", SessionChoice::Allow);
    session.record("s1", "b", SessionChoice::Deny);
    session.record("s2", "a", SessionChoice::Allow);
    session.invalidate_session("s1");
    assert!(!session.is_empty());
    assert_eq!(session.lookup("s2", "a"), Some(SessionChoice::Allow));
    session.invalidate_session("s2");
    assert!(session.is_empty(), "session state dies with the session");
}

// ── Evaluator: unknown operations and session precedence ─────────────────────

#[test]
fn every_unknown_operation_prompts() {
    let ws = identity(b"/work/root");
    for (category, identity) in [
        (
            TrustCategory::Unknown,
            OperationIdentity::Command { executable: "git".into(), argv: vec![] },
        ),
        (TrustCategory::Execute, OperationIdentity::Unknown),
        (TrustCategory::Unknown, OperationIdentity::Unknown),
    ] {
        let op = operation(ws, None, category, identity);
        let decision = decide_default(&op, &[]);
        assert_eq!(decision.outcome, TrustOutcome::Confirm, "unknown op must prompt");
        assert_eq!(decision.reason, DecisionReason::UnknownOperation);
        assert_eq!(decision.reason.as_str(), "unknown_operation");
    }
}

#[test]
fn unknown_operations_never_match_a_persistent_rule() {
    let ws = identity(b"/work/root");
    let rule = command_rule(spec("cmd_1"), ws, "git", MatchMode::ArgvExact, &["status"]);
    let op = operation(
        ws,
        None,
        TrustCategory::Unknown,
        OperationIdentity::Command { executable: "git".into(), argv: vec!["status".into()] },
    );
    let decision = decide_default(&op, &[rule]);
    assert_eq!(decision.outcome, TrustOutcome::Confirm);
    assert_eq!(decision.reason, DecisionReason::UnknownOperation);
}

#[test]
fn session_deny_overrides_matching_persistent_allow() {
    let ws = identity(b"/work/root");
    let rule = command_rule(spec("cmd_1"), ws, "git", MatchMode::ArgvExact, &["status"]);
    let op = command_op(ws, "git", &["status"]);
    let mut session = SessionPolicy::default();
    session.record("s1", "fp", SessionChoice::Deny);

    let denied = decide(
        &op,
        std::slice::from_ref(&rule),
        &session,
        "s1",
        "2026-08-07T12:00:00Z",
        &UsageSnapshot::default(),
        true,
    );
    assert_eq!(denied.outcome, TrustOutcome::Deny);
    assert_eq!(denied.reason, DecisionReason::SessionDeny);

    // Without the session deny the same persistent rule allows.
    let allowed = decide_default(&op, &[rule]);
    assert_eq!(allowed.outcome, TrustOutcome::Allow);
    assert_eq!(allowed.reason, DecisionReason::PersistentAllow);
    assert_eq!(allowed.rule_id.as_deref(), Some("cmd_1"));
}

#[test]
fn session_deny_precedes_session_allow_and_is_scoped() {
    let ws = identity(b"/work/root");
    let op = command_op(ws, "git", &["status"]);
    let mut session = SessionPolicy::default();
    session.record("s1", "fp", SessionChoice::Deny);
    session.record("s1", "fp", SessionChoice::Allow);
    // Deny recorded after allow still wins.
    let decision =
        decide(&op, &[], &session, "s1", "2026-08-07T12:00:00Z", &UsageSnapshot::default(), true);
    assert_eq!(decision.outcome, TrustOutcome::Deny);
    assert_eq!(decision.reason, DecisionReason::SessionDeny);
    // The deny is per-session.
    let other =
        decide(&op, &[], &session, "s2", "2026-08-07T12:00:00Z", &UsageSnapshot::default(), true);
    assert_eq!(other.reason, DecisionReason::NoMatchingRule);
}

#[test]
fn session_allow_permits_without_any_persistent_rule() {
    let ws = identity(b"/work/root");
    let op = command_op(ws, "git", &["status"]);
    let mut session = SessionPolicy::default();
    session.record("s1", "fp", SessionChoice::Allow);
    let decision =
        decide(&op, &[], &session, "s1", "2026-08-07T12:00:00Z", &UsageSnapshot::default(), true);
    assert_eq!(decision.outcome, TrustOutcome::Allow);
    assert_eq!(decision.reason, DecisionReason::SessionAllow);
    assert_eq!(decision.rule_id, None);
}
