use super::*;

// ── Evaluator: persistent rule scope and matching ────────────────────────────

#[test]
fn command_exact_rule_matches_only_exact_argv() {
    let ws = identity(b"/work/root");
    let rule = command_rule(spec("cmd_1"), ws, "git", MatchMode::ArgvExact, &["status"]);
    assert_eq!(
        decide_default(&command_op(ws, "git", &["status"]), std::slice::from_ref(&rule)).outcome,
        TrustOutcome::Allow
    );
    assert_eq!(
        decide_default(&command_op(ws, "git", &["status", "--short"]), std::slice::from_ref(&rule))
            .outcome,
        TrustOutcome::Confirm
    );
    assert_eq!(
        decide_default(&command_op(ws, "git", &["stash"]), std::slice::from_ref(&rule)).outcome,
        TrustOutcome::Confirm
    );
    assert_eq!(
        decide_default(&command_op(ws, "hub", &["status"]), &[rule]).outcome,
        TrustOutcome::Confirm
    );
}

#[test]
fn command_prefix_rule_matches_only_matching_prefix() {
    let ws = identity(b"/work/root");
    let rule = command_rule(spec("cmd_1"), ws, "git", MatchMode::ArgvPrefix, &["status"]);
    assert_eq!(
        decide_default(&command_op(ws, "git", &["status"]), std::slice::from_ref(&rule)).outcome,
        TrustOutcome::Allow
    );
    assert_eq!(
        decide_default(&command_op(ws, "git", &["status", "--short"]), std::slice::from_ref(&rule))
            .outcome,
        TrustOutcome::Allow
    );
    assert_eq!(
        decide_default(&command_op(ws, "git", &["stash"]), &[rule]).outcome,
        TrustOutcome::Confirm
    );
}

#[test]
fn cross_workspace_rule_never_matches() {
    let ws = identity(b"/work/root");
    let other = identity(b"/work/other");
    let rule = command_rule(spec("cmd_1"), ws, "git", MatchMode::ArgvExact, &["status"]);
    let decision = decide_default(&command_op(other, "git", &["status"]), &[rule]);
    assert_eq!(decision.outcome, TrustOutcome::Confirm);
    assert_eq!(decision.reason, DecisionReason::NoMatchingRule);
}

#[test]
fn agent_scoping_matches_only_the_configured_agent() {
    let ws = identity(b"/work/root");
    let rule = command_rule(
        spec("cmd_1").agent(Some("openrouter")),
        ws,
        "git",
        MatchMode::ArgvExact,
        &["status"],
    );
    let op = |agent: Option<&str>| {
        operation(
            ws,
            agent,
            TrustCategory::Execute,
            OperationIdentity::Command { executable: "git".into(), argv: vec!["status".into()] },
        )
    };
    assert_eq!(
        decide_default(&op(Some("openrouter")), std::slice::from_ref(&rule)).outcome,
        TrustOutcome::Allow
    );
    assert_eq!(
        decide_default(&op(None), std::slice::from_ref(&rule)).outcome,
        TrustOutcome::Confirm
    );
    assert_eq!(decide_default(&op(Some("other")), &[rule]).outcome, TrustOutcome::Confirm);

    // A rule without an agent scopes to any agent.
    let any_agent = command_rule(spec("cmd_2"), ws, "git", MatchMode::ArgvExact, &["status"]);
    assert_eq!(
        decide_default(&op(Some("openrouter")), std::slice::from_ref(&any_agent)).outcome,
        TrustOutcome::Allow
    );
    assert_eq!(decide_default(&op(None), &[any_agent]).outcome, TrustOutcome::Allow);
}

#[test]
fn expired_rule_prompts_and_only_injected_time_decides() {
    let ws = identity(b"/work/root");
    let rule = command_rule(spec("cmd_1"), ws, "git", MatchMode::ArgvExact, &["status"]);
    let op = command_op(ws, "git", &["status"]);
    let before = decide(
        &op,
        std::slice::from_ref(&rule),
        &SessionPolicy::default(),
        "s1",
        "2026-08-08T11:59:59Z",
        &UsageSnapshot::default(),
        true,
    );
    assert_eq!(before.outcome, TrustOutcome::Allow);
    let at_expiry = decide(
        &op,
        std::slice::from_ref(&rule),
        &SessionPolicy::default(),
        "s1",
        "2026-08-08T12:00:00Z",
        &UsageSnapshot::default(),
        true,
    );
    assert_eq!(at_expiry.outcome, TrustOutcome::Confirm, "expiry boundary is exclusive");
    let after = decide(
        &op,
        &[rule],
        &SessionPolicy::default(),
        "s1",
        "2026-08-08T12:00:01Z",
        &UsageSnapshot::default(),
        true,
    );
    assert_eq!(after.outcome, TrustOutcome::Confirm);
    assert_eq!(after.reason, DecisionReason::NoMatchingRule);
}

#[test]
fn exhausted_rule_prompts_without_mutating_usage() {
    let ws = identity(b"/work/root");
    let rule =
        command_rule(spec("cmd_1").max_uses(Some(2)), ws, "git", MatchMode::ArgvExact, &["status"]);
    let op = command_op(ws, "git", &["status"]);
    let usage = UsageSnapshot::new(BTreeMap::from([("cmd_1".to_string(), 1)]));
    let allowed = decide(
        &op,
        std::slice::from_ref(&rule),
        &SessionPolicy::default(),
        "s1",
        "2026-08-07T12:00:00Z",
        &usage,
        true,
    );
    assert_eq!(allowed.outcome, TrustOutcome::Allow);
    let exhausted = UsageSnapshot::new(BTreeMap::from([("cmd_1".to_string(), 2)]));
    let decision = decide(
        &op,
        &[rule],
        &SessionPolicy::default(),
        "s1",
        "2026-08-07T12:00:00Z",
        &exhausted,
        true,
    );
    assert_eq!(decision.outcome, TrustOutcome::Confirm);
    assert_eq!(decision.reason, DecisionReason::NoMatchingRule);
    assert_eq!(exhausted.used("cmd_1"), 2, "usage snapshot must be unchanged");
}

#[test]
fn read_rules_require_the_workspace_gate() {
    let ws = identity(b"/work/root");
    let rule = read_path_rule(spec("read_1"), ws, "src", 262_144);
    let op = read_path_op(ws, "src/main.rs", Some(1024));
    let gated = decide(
        &op,
        std::slice::from_ref(&rule),
        &SessionPolicy::default(),
        "s1",
        "2026-08-07T12:00:00Z",
        &UsageSnapshot::default(),
        false,
    );
    assert_eq!(gated.outcome, TrustOutcome::Confirm);
    assert_eq!(gated.reason, DecisionReason::WorkspaceDisabled);
    let open = decide(
        &op,
        std::slice::from_ref(&rule),
        &SessionPolicy::default(),
        "s1",
        "2026-08-07T12:00:00Z",
        &UsageSnapshot::default(),
        true,
    );
    assert_eq!(open.outcome, TrustOutcome::Allow);
    assert_eq!(open.rule_id.as_deref(), Some("read_1"));
    // The gate alone never authorizes anything.
    let no_rule = decide(
        &op,
        &[],
        &SessionPolicy::default(),
        "s1",
        "2026-08-07T12:00:00Z",
        &UsageSnapshot::default(),
        true,
    );
    assert_eq!(no_rule.outcome, TrustOutcome::Confirm);
}

#[test]
fn profile_rules_require_the_workspace_gate() {
    let ws = identity(b"/work/root");
    let rule = profile_rule(spec("profile_1"), ws, "git_readonly");
    let op = profile_op(ws, "git_readonly");
    let gated = decide(
        &op,
        std::slice::from_ref(&rule),
        &SessionPolicy::default(),
        "s1",
        "2026-08-07T12:00:00Z",
        &UsageSnapshot::default(),
        false,
    );
    assert_eq!(gated.reason, DecisionReason::WorkspaceDisabled);
    let open = decide(
        &op,
        &[rule],
        &SessionPolicy::default(),
        "s1",
        "2026-08-07T12:00:00Z",
        &UsageSnapshot::default(),
        true,
    );
    assert_eq!(open.outcome, TrustOutcome::Allow);
}

#[test]
fn read_path_rule_enforces_prefix_and_byte_cap() {
    let ws = identity(b"/work/root");
    let rule = read_path_rule(spec("read_1"), ws, "src", 1024);
    assert_eq!(
        decide_default(&read_path_op(ws, "src/main.rs", Some(100)), std::slice::from_ref(&rule))
            .outcome,
        TrustOutcome::Allow
    );
    assert_eq!(
        decide_default(&read_path_op(ws, "src", Some(100)), std::slice::from_ref(&rule)).outcome,
        TrustOutcome::Allow,
        "directory itself"
    );
    assert_eq!(
        decide_default(&read_path_op(ws, "src/main.rs", None), std::slice::from_ref(&rule)).outcome,
        TrustOutcome::Allow,
        "unknown size"
    );
    assert_eq!(
        decide_default(&read_path_op(ws, "src/main.rs", Some(2048)), std::slice::from_ref(&rule))
            .outcome,
        TrustOutcome::Confirm,
        "over byte cap"
    );
    assert_eq!(
        decide_default(&read_path_op(ws, "lib/main.rs", Some(100)), &[rule]).outcome,
        TrustOutcome::Confirm,
        "outside prefix"
    );
}

#[test]
fn mcp_rule_matches_exact_invocation_only() {
    let ws = identity(b"/work/root");
    let rule = mcp_rule(spec("mcp_1"), ws, r#"{"path":"src/main.rs"}"#);
    let op = |args: &str| {
        operation(
            ws,
            None,
            TrustCategory::Read,
            OperationIdentity::Mcp {
                server: "ee".into(),
                transport_identity: "stdio:test".into(),
                tool: "ee_read_file".into(),
                tool_schema_version: 1,
                arguments_json: args.to_string(),
            },
        )
    };
    assert_eq!(
        decide_default(&op(r#"{"path":"src/main.rs"}"#), std::slice::from_ref(&rule)).outcome,
        TrustOutcome::Allow
    );
    assert_eq!(
        decide_default(&op(r#"{"path":"src/other.rs"}"#), std::slice::from_ref(&rule)).outcome,
        TrustOutcome::Confirm,
        "changed argument"
    );
    assert_eq!(
        decide_default(&op(r#"{"path":"src/main.rs","line":1}"#), std::slice::from_ref(&rule))
            .outcome,
        TrustOutcome::Confirm,
        "extra argument"
    );
    let other_tool = operation(
        ws,
        None,
        TrustCategory::Read,
        OperationIdentity::Mcp {
            server: "ee".into(),
            transport_identity: "stdio:test".into(),
            tool: "ee_search_files".into(),
            tool_schema_version: 1,
            arguments_json: r#"{"path":"src/main.rs"}"#.into(),
        },
    );
    assert_eq!(decide_default(&other_tool, &[rule]).outcome, TrustOutcome::Confirm, "changed tool");
}

#[test]
fn write_rule_authorizes_only_its_operation_kind() {
    let ws = identity(b"/work/root");
    let create = write_rule(
        spec("write_1"),
        ws,
        WriteOperationKind::Create,
        "src/generated",
        5,
        65_536,
        16_384,
    );
    let op = |category: TrustCategory| {
        operation(
            ws,
            None,
            category,
            OperationIdentity::Write {
                relative_path: "src/generated/a.rs".into(),
                file_count: 1,
                total_bytes: Some(1024),
                max_file_bytes: Some(1024),
            },
        )
    };
    assert_eq!(
        decide_default(&op(TrustCategory::WriteCreate), std::slice::from_ref(&create)).outcome,
        TrustOutcome::Allow
    );
    assert_eq!(
        decide_default(&op(TrustCategory::WriteModify), &[create]).outcome,
        TrustOutcome::Confirm,
        "create never authorizes modify"
    );

    let modify = write_rule(
        spec("write_2"),
        ws,
        WriteOperationKind::Modify,
        "src/generated",
        5,
        65_536,
        16_384,
    );
    assert_eq!(
        decide_default(&op(TrustCategory::WriteModify), &[modify]).outcome,
        TrustOutcome::Allow
    );
}

#[test]
fn write_rule_enforces_path_and_budget_bounds() {
    let ws = identity(b"/work/root");
    let rule = write_rule(
        spec("write_1"),
        ws,
        WriteOperationKind::Create,
        "src/generated",
        5,
        65_536,
        16_384,
    );
    let op = |relative: &str, files: u64, total: Option<u64>, largest: Option<u64>| {
        operation(
            ws,
            None,
            TrustCategory::WriteCreate,
            OperationIdentity::Write {
                relative_path: relative.into(),
                file_count: files,
                total_bytes: total,
                max_file_bytes: largest,
            },
        )
    };
    assert_eq!(
        decide_default(
            &op("src/generated/a.rs", 1, Some(1024), Some(1024)),
            std::slice::from_ref(&rule)
        )
        .outcome,
        TrustOutcome::Allow
    );
    assert_eq!(
        decide_default(
            &op("src/not-generated/a.rs", 1, Some(1024), Some(1024)),
            std::slice::from_ref(&rule)
        )
        .outcome,
        TrustOutcome::Confirm,
        "outside prefix"
    );
    assert_eq!(
        decide_default(
            &op("src/generated/a.rs", 6, Some(1024), Some(1024)),
            std::slice::from_ref(&rule)
        )
        .outcome,
        TrustOutcome::Confirm,
        "over file count"
    );
    assert_eq!(
        decide_default(
            &op("src/generated/a.rs", 1, Some(70_000), Some(1024)),
            std::slice::from_ref(&rule)
        )
        .outcome,
        TrustOutcome::Confirm,
        "over total bytes"
    );
    assert_eq!(
        decide_default(&op("src/generated/a.rs", 1, Some(1024), Some(20_000)), &[rule]).outcome,
        TrustOutcome::Confirm,
        "over per-file bytes"
    );
}

#[test]
fn evaluator_performs_no_filesystem_clock_or_counter_mutation() {
    let ws = identity(b"/work/root");
    let rule = command_rule(spec("cmd_1"), ws, "git", MatchMode::ArgvExact, &["status"]);
    let op = command_op(ws, "git", &["status"]);
    let usage = UsageSnapshot::new(BTreeMap::from([("cmd_1".to_string(), 1)]));
    let before_usage = usage.clone();

    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("marker.txt"), "before").unwrap();
    let before_entries = dir_entries(dir.path());

    for _ in 0..2 {
        let decision = decide(
            &op,
            std::slice::from_ref(&rule),
            &SessionPolicy::default(),
            "s1",
            "2026-08-07T12:00:00Z",
            &usage,
            true,
        );
        assert_eq!(decision.outcome, TrustOutcome::Allow);
        assert_eq!(usage, before_usage, "usage snapshot must be immutable");
        assert_eq!(dir_entries(dir.path()), before_entries, "no filesystem mutation");
        assert_eq!(fs::read_to_string(dir.path().join("marker.txt")).unwrap(), "before");
    }

    // The decision depends only on the injected clock.
    let late = decide(
        &op,
        std::slice::from_ref(&rule),
        &SessionPolicy::default(),
        "s1",
        "2099-01-01T00:00:00Z",
        &usage,
        true,
    );
    assert_eq!(late.outcome, TrustOutcome::Confirm);
}
