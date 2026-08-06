//! Phase 1 foundation tests for the unified host-local workspace trust
//! policy (ISSUES.md "Unified Host-Local Workspace Trust Policy").
//!
//! Covers the shared trust-domain contracts, the pure shared evaluator, the
//! host-local trust store, and the session precedence adaptation.  All store
//! tests run against temporary state directories and canonical workspace
//! roots; nothing touches real user state.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use tempfile::TempDir;

use crate::policy::evaluator::{PolicyInput, evaluate};
use crate::policy::rules::{MatchMode, TrustRule, WriteOperationKind};
use crate::policy::session::{SessionChoice, SessionPolicy};
use crate::policy::store::{TrustStore, TrustStoreDocument, TrustStoreError};
use crate::policy::{
    DecisionReason, OperationIdentity, PathPrefix, TransportKind, TrustCategory, TrustDecision,
    TrustOperation, TrustOutcome, TrustRuleScope, UsageSnapshot, WorkspaceIdentity,
};

// ── Helpers ──────────────────────────────────────────────────────────────────

fn identity(bytes: &[u8]) -> WorkspaceIdentity {
    WorkspaceIdentity::from_canonical_root_bytes(bytes)
}

fn at(text: &str) -> SystemTime {
    chrono::DateTime::parse_from_rfc3339(text)
        .expect("valid RFC3339")
        .with_timezone(&chrono::Utc)
        .into()
}

struct RuleSpec<'a> {
    id: &'a str,
    agent: Option<&'a str>,
    expires_at: Option<&'a str>,
    max_uses: Option<u64>,
}

fn spec(id: &str) -> RuleSpec<'_> {
    RuleSpec { id, agent: None, expires_at: Some("2026-08-08T12:00:00Z"), max_uses: Some(20) }
}

fn scope_for(spec: &RuleSpec<'_>, workspace: WorkspaceIdentity) -> TrustRuleScope {
    TrustRuleScope {
        workspace,
        agent: spec.agent.map(String::from),
        expires_at: spec.expires_at.map(at),
        max_uses: spec.max_uses,
    }
}

fn command_rule(
    spec: RuleSpec<'_>,
    workspace: WorkspaceIdentity,
    executable: &str,
    match_mode: MatchMode,
    argv: &[&str],
) -> TrustRule {
    TrustRule::Command(crate::policy::CommandRule {
        id: spec.id.to_string(),
        scope: scope_for(&spec, workspace),
        executable: executable.to_string(),
        match_mode,
        argv: argv.iter().map(|token| token.to_string()).collect(),
    })
}

fn mcp_rule(spec: RuleSpec<'_>, workspace: WorkspaceIdentity, arguments_json: &str) -> TrustRule {
    TrustRule::Mcp(crate::policy::McpRule {
        id: spec.id.to_string(),
        scope: scope_for(&spec, workspace),
        server: "ee".to_string(),
        transport_identity: "stdio:test".to_string(),
        tool: "ee_read_file".to_string(),
        tool_schema_version: 1,
        arguments_json: arguments_json.to_string(),
    })
}

fn read_path_rule(
    spec: RuleSpec<'_>,
    workspace: WorkspaceIdentity,
    path_prefix: &str,
    max_bytes: u64,
) -> TrustRule {
    TrustRule::ReadPath(crate::policy::ReadPathRule {
        id: spec.id.to_string(),
        scope: scope_for(&spec, workspace),
        path_prefix: PathPrefix::parse(path_prefix).expect("valid prefix"),
        max_bytes,
    })
}

fn profile_rule(spec: RuleSpec<'_>, workspace: WorkspaceIdentity, profile: &str) -> TrustRule {
    TrustRule::Profile(crate::policy::ProfileRule {
        id: spec.id.to_string(),
        scope: scope_for(&spec, workspace),
        profile: profile.to_string(),
    })
}

fn write_rule(
    spec: RuleSpec<'_>,
    workspace: WorkspaceIdentity,
    operation: WriteOperationKind,
    path_prefix: &str,
    max_files: u64,
    max_total_bytes: u64,
    max_file_bytes: u64,
) -> TrustRule {
    TrustRule::Write(crate::policy::WriteRule {
        id: spec.id.to_string(),
        scope: scope_for(&spec, workspace),
        operation,
        path_prefix: PathPrefix::parse(path_prefix).expect("valid prefix"),
        max_files,
        max_total_bytes,
        max_file_bytes,
    })
}

fn operation(
    workspace: WorkspaceIdentity,
    agent: Option<&str>,
    category: TrustCategory,
    identity: OperationIdentity,
) -> TrustOperation {
    TrustOperation {
        workspace,
        agent: agent.map(String::from),
        transport: TransportKind::Acp,
        category,
        identity,
    }
}

fn command_op(workspace: WorkspaceIdentity, executable: &str, argv: &[&str]) -> TrustOperation {
    operation(
        workspace,
        None,
        TrustCategory::Execute,
        OperationIdentity::Command {
            executable: executable.to_string(),
            argv: argv.iter().map(|token| token.to_string()).collect(),
        },
    )
}

fn read_path_op(
    workspace: WorkspaceIdentity,
    relative: &str,
    bytes: Option<u64>,
) -> TrustOperation {
    operation(
        workspace,
        None,
        TrustCategory::Read,
        OperationIdentity::ReadPath { relative_path: relative.to_string(), byte_count: bytes },
    )
}

fn profile_op(workspace: WorkspaceIdentity, profile: &str) -> TrustOperation {
    operation(
        workspace,
        None,
        TrustCategory::Execute,
        OperationIdentity::Profile { profile: profile.to_string() },
    )
}

fn decide(
    op: &TrustOperation,
    rules: &[TrustRule],
    session: &SessionPolicy,
    session_id: &str,
    now: &str,
    usage: &UsageSnapshot,
    workspace_enabled: bool,
) -> TrustDecision {
    evaluate(&PolicyInput {
        session_id,
        fingerprint: "fp",
        operation: op,
        session,
        rules,
        now: at(now),
        usage,
        workspace_enabled,
    })
}

fn decide_default(op: &TrustOperation, rules: &[TrustRule]) -> TrustDecision {
    decide(
        op,
        rules,
        &SessionPolicy::default(),
        "s1",
        "2026-08-07T12:00:00Z",
        &UsageSnapshot::default(),
        true,
    )
}

fn store_setup() -> (TempDir, TempDir, TrustStore) {
    let base = TempDir::new().expect("state dir");
    let workspace = TempDir::new().expect("workspace root");
    let store = TrustStore::at(base.path(), workspace.path()).expect("store");
    (base, workspace, store)
}

fn write_store_text(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
        set_owner_only_dir(parent);
    }
    fs::write(path, text).unwrap();
    set_owner_only(path);
}

#[cfg(unix)]
fn set_owner_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

#[cfg(unix)]
fn set_owner_only_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path) {}

fn dir_entries(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> =
        fs::read_dir(dir).unwrap().map(|entry| entry.unwrap().path()).collect();
    entries.sort();
    entries
}

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
        assert_eq!(decision.outcome, TrustOutcome::Prompt, "unknown op must prompt");
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
    assert_eq!(decision.outcome, TrustOutcome::Prompt);
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
    assert_eq!(denied.outcome, TrustOutcome::Prompt);
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
    assert_eq!(decision.outcome, TrustOutcome::Prompt);
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
        TrustOutcome::Prompt
    );
    assert_eq!(
        decide_default(&command_op(ws, "git", &["stash"]), std::slice::from_ref(&rule)).outcome,
        TrustOutcome::Prompt
    );
    assert_eq!(
        decide_default(&command_op(ws, "hub", &["status"]), &[rule]).outcome,
        TrustOutcome::Prompt
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
        TrustOutcome::Prompt
    );
}

#[test]
fn cross_workspace_rule_never_matches() {
    let ws = identity(b"/work/root");
    let other = identity(b"/work/other");
    let rule = command_rule(spec("cmd_1"), ws, "git", MatchMode::ArgvExact, &["status"]);
    let decision = decide_default(&command_op(other, "git", &["status"]), &[rule]);
    assert_eq!(decision.outcome, TrustOutcome::Prompt);
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
        TrustOutcome::Prompt
    );
    assert_eq!(decide_default(&op(Some("other")), &[rule]).outcome, TrustOutcome::Prompt);

    // A rule without an agent scopes to any agent.
    let any_agent = command_rule(spec("cmd_2"), ws, "git", MatchMode::ArgvExact, &["status"]);
    assert_eq!(
        decide_default(&op(Some("openrouter")), std::slice::from_ref(&any_agent)).outcome,
        TrustOutcome::Allow
    );
    assert_eq!(decide_default(&op(None), &[any_agent]).outcome, TrustOutcome::Allow);
}

impl RuleSpec<'_> {
    fn agent(mut self, agent: Option<&'static str>) -> Self {
        self.agent = agent;
        self
    }

    fn max_uses(mut self, max_uses: Option<u64>) -> Self {
        self.max_uses = max_uses;
        self
    }
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
    assert_eq!(at_expiry.outcome, TrustOutcome::Prompt, "expiry boundary is exclusive");
    let after = decide(
        &op,
        &[rule],
        &SessionPolicy::default(),
        "s1",
        "2026-08-08T12:00:01Z",
        &UsageSnapshot::default(),
        true,
    );
    assert_eq!(after.outcome, TrustOutcome::Prompt);
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
    assert_eq!(decision.outcome, TrustOutcome::Prompt);
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
    assert_eq!(gated.outcome, TrustOutcome::Prompt);
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
    assert_eq!(no_rule.outcome, TrustOutcome::Prompt);
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
        TrustOutcome::Prompt,
        "over byte cap"
    );
    assert_eq!(
        decide_default(&read_path_op(ws, "lib/main.rs", Some(100)), &[rule]).outcome,
        TrustOutcome::Prompt,
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
        TrustOutcome::Prompt,
        "changed argument"
    );
    assert_eq!(
        decide_default(&op(r#"{"path":"src/main.rs","line":1}"#), std::slice::from_ref(&rule))
            .outcome,
        TrustOutcome::Prompt,
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
    assert_eq!(decide_default(&other_tool, &[rule]).outcome, TrustOutcome::Prompt, "changed tool");
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
        TrustOutcome::Prompt,
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
        TrustOutcome::Prompt,
        "outside prefix"
    );
    assert_eq!(
        decide_default(
            &op("src/generated/a.rs", 6, Some(1024), Some(1024)),
            std::slice::from_ref(&rule)
        )
        .outcome,
        TrustOutcome::Prompt,
        "over file count"
    );
    assert_eq!(
        decide_default(
            &op("src/generated/a.rs", 1, Some(70_000), Some(1024)),
            std::slice::from_ref(&rule)
        )
        .outcome,
        TrustOutcome::Prompt,
        "over total bytes"
    );
    assert_eq!(
        decide_default(&op("src/generated/a.rs", 1, Some(1024), Some(20_000)), &[rule]).outcome,
        TrustOutcome::Prompt,
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
    assert_eq!(late.outcome, TrustOutcome::Prompt);
}

// ── Store: schema compatibility ──────────────────────────────────────────────

fn sample_document(store: &TrustStore) -> TrustStoreDocument {
    let ws = *store.workspace();
    TrustStoreDocument {
        workspace: ws,
        workspace_enabled: false,
        rules: vec![
            command_rule(
                spec("cmd_1").agent(Some("openrouter")),
                ws,
                "git",
                MatchMode::ArgvPrefix,
                &["status"],
            ),
            mcp_rule(spec("mcp_1").agent(Some("openrouter")), ws, r#"{"path":"src/main.rs"}"#),
            read_path_rule(spec("read_1").agent(Some("openrouter")), ws, "src", 262_144),
            TrustRule::McpRead(crate::policy::McpReadRule {
                id: "mcp_read_1".to_string(),
                scope: scope_for(&spec("mcp_read_1").agent(Some("openrouter")), ws),
                server: "ee".to_string(),
                transport_identity: "stdio:test".to_string(),
                tool: "ee_read_file".to_string(),
                tool_schema_version: 1,
                path_prefix: PathPrefix::parse("src").expect("valid prefix"),
                max_bytes: 262_144,
            }),
            profile_rule(spec("profile_1").agent(Some("openrouter")), ws, "git_readonly"),
            write_rule(
                spec("write_1").agent(Some("openrouter")),
                ws,
                WriteOperationKind::Create,
                "src/generated",
                5,
                65_536,
                16_384,
            ),
        ],
    }
}

#[test]
fn every_documented_rule_array_round_trips_through_store() {
    let (_base, workspace_dir, store) = store_setup();
    let document = sample_document(&store);
    store.write(&document).expect("write");

    let reloaded =
        TrustStore::at(store.path().parent().unwrap().parent().unwrap(), workspace_dir.path())
            .expect("store")
            .load()
            .expect("load");
    assert_eq!(reloaded, document, "parse must reproduce the written document");

    let second = store.load().expect("reload after read");
    assert_eq!(second, document);
}

#[test]
fn serialized_document_uses_canonical_schema_shape() {
    let (_base, _workspace_dir, store) = store_setup();
    store.write(&sample_document(&store)).expect("write");
    let text = fs::read_to_string(store.path()).unwrap();

    assert!(text.contains("schema_version = 1"), "schema version first");
    assert!(text.contains(&format!("identity = \"{}\"", store.workspace().as_string())));
    assert!(text.contains("workspace_enabled = false"));
    for array in [
        "[[command_allow]]",
        "[[mcp_allow]]",
        "[[read_path_allow]]",
        "[[mcp_read_allow]]",
        "[[profile_allow]]",
        "[[write_allow]]",
    ] {
        assert!(text.contains(array), "missing {array}");
    }
    // Canonical field order inside the command table (id, agent,
    // executable, match, argv, expires_at, max_uses).
    let index = |needle: &str| text.find(needle).unwrap_or_else(|| panic!("missing {needle}"));
    let id = index("id = \"cmd_1\"");
    let agent = index("agent = \"openrouter\"");
    let executable = index("executable = \"git\"");
    let argv = index("argv =");
    let expires = index("expires_at =");
    let uses = index("max_uses =");
    assert!(
        id < agent && agent < executable && executable < argv && argv < expires && expires < uses
    );
    // No raw workspace path or identity prefix leaks into the document.
    assert!(!text.contains("sha256:sha256:"));
}

#[test]
fn unsupported_schema_version_loads_no_effective_rules() {
    let (_base, _workspace_dir, store) = store_setup();
    store.write(&sample_document(&store)).expect("write");
    let text = fs::read_to_string(store.path())
        .unwrap()
        .replace("schema_version = 1", "schema_version = 2");
    write_store_text(store.path(), &text);

    match store.load() {
        Err(TrustStoreError::UnsupportedSchemaVersion(2)) => {}
        other => panic!("expected UnsupportedSchemaVersion(2), got {other:?}"),
    }
    let effective = store.effective();
    assert!(effective.rules.is_empty(), "unsupported version yields no effective rules");
    assert!(!effective.workspace_enabled);
    let op = command_op(*store.workspace(), "git", &["status"]);
    assert_eq!(decide_default(&op, &effective.rules).outcome, TrustOutcome::Prompt);
}

#[test]
fn missing_workspace_identity_rejects_the_store() {
    let (_base, _workspace_dir, store) = store_setup();
    let text = r#"
schema_version = 1

[policy]
workspace_enabled = false
"#;
    write_store_text(store.path(), text);
    assert!(matches!(store.load(), Err(TrustStoreError::ValidationFailure(_))));
    assert!(store.effective().rules.is_empty());
}

#[test]
fn cross_workspace_document_fails_identity_validation() {
    let (base, _workspace_a, store_a) = store_setup();
    let workspace_b = TempDir::new().unwrap();
    let store_b = TrustStore::at(base.path(), workspace_b.path()).expect("store b");

    store_a.write(&sample_document(&store_a)).expect("write a");
    // Copy the store file itself: identity inside the document is bound to
    // workspace A, so store B must fail closed.
    fs::copy(store_a.path(), store_b.path()).unwrap();
    set_owner_only(store_b.path());

    match store_b.load() {
        Err(TrustStoreError::IdentityMismatch) => {}
        other => panic!("expected IdentityMismatch, got {other:?}"),
    }
    let effective = store_b.effective();
    assert!(effective.rules.is_empty(), "copied store file grants nothing");
    let op = command_op(*store_b.workspace(), "git", &["status"]);
    assert_eq!(decide_default(&op, &effective.rules).outcome, TrustOutcome::Prompt);
}

#[test]
fn writing_a_document_for_another_workspace_is_rejected() {
    let (base, _workspace_a, store_a) = store_setup();
    let workspace_b = TempDir::new().unwrap();
    let store_b = TrustStore::at(base.path(), workspace_b.path()).expect("store b");
    // A document bound to workspace A (identity and rule scopes) cannot be
    // written into workspace B's store.
    match store_b.write(&sample_document(&store_a)) {
        Err(TrustStoreError::IdentityMismatch) => {}
        other => panic!("expected IdentityMismatch, got {other:?}"),
    }
    assert!(!store_b.path().exists(), "rejected write must not create the store");
    // Rules created for workspace B persist under B's store.
    store_b.write(&sample_document(&store_b)).expect("correct identity writes");
    assert_eq!(store_b.load().expect("load").rules.len(), 6);
}

#[test]
fn duplicate_rule_ids_invalidate_only_conflicting_entries() {
    let (_base, _workspace_dir, store) = store_setup();
    let ws = *store.workspace();
    let text = format!(
        r#"
schema_version = 1

[workspace]
identity = "{identity}"

[policy]
workspace_enabled = false

[[command_allow]]
id = "cmd_1"
executable = "git"
match = "argv_exact"
argv = ["status"]
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20

[[command_allow]]
id = "cmd_1"
executable = "git"
match = "argv_exact"
argv = ["stash"]
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20

[[mcp_allow]]
id = "cmd_1"
server = "ee"
transport_identity = "stdio:test"
tool = "ee_read_file"
tool_schema_version = 1
arguments_json = "{{\"path\":\"src/main.rs\"}}"

[[command_allow]]
id = "cmd_2"
executable = "git"
match = "argv_exact"
argv = ["diff"]
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20
"#,
        identity = ws.as_string()
    );
    write_store_text(store.path(), &text);
    let document = store.load().expect("load");
    assert_eq!(document.rules.len(), 1, "conflicting duplicates dropped, unique entry loads");
    assert_eq!(document.rules[0].id(), "cmd_2");
    // Cross-array duplicates are conflicts too.
    assert_eq!(document.rules[0].id(), "cmd_2");
}

#[test]
fn cross_kind_and_unknown_fields_drop_only_that_entry() {
    let (_base, _workspace_dir, store) = store_setup();
    let ws = *store.workspace();
    let text = format!(
        r#"
schema_version = 1

[workspace]
identity = "{identity}"

[policy]
workspace_enabled = false

[[read_path_allow]]
id = "bad_1"
path_prefix = "src"
max_bytes = 100
executable = "git"

[[read_path_allow]]
id = "bad_2"
path_prefix = "src"
max_bytes = 100
bogus_field = 1

[[read_path_allow]]
id = "read_1"
path_prefix = "src"
max_bytes = 262144

[[command_allow]]
id = "cmd_1"
executable = "git"
match = "argv_exact"
argv = ["status"]
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20
"#,
        identity = ws.as_string()
    );
    write_store_text(store.path(), &text);
    let document = store.load().expect("load");
    let ids: Vec<&str> = document.rules.iter().map(TrustRule::id).collect();
    assert_eq!(ids, vec!["cmd_1", "read_1"], "invalid entries dropped, valid entries load");
}

#[test]
fn invalid_match_mode_and_write_operation_entries_are_rejected() {
    let (_base, _workspace_dir, store) = store_setup();
    let ws = *store.workspace();
    let text = format!(
        r#"
schema_version = 1

[workspace]
identity = "{identity}"

[policy]
workspace_enabled = false

[[command_allow]]
id = "cmd_bad"
executable = "git"
match = "contains"
argv = ["status"]
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20

[[write_allow]]
id = "write_bad"
operation = "overwrite"
path_prefix = "src/generated"
max_files = 5
max_total_bytes = 65536
max_file_bytes = 16384
expires_at = "2026-08-08T12:00:00Z"
max_uses = 5

[[command_allow]]
id = "cmd_1"
executable = "git"
match = "argv_exact"
argv = ["status"]
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20
"#,
        identity = ws.as_string()
    );
    write_store_text(store.path(), &text);
    let document = store.load().expect("load");
    assert_eq!(document.rules.len(), 1);
    assert_eq!(document.rules[0].id(), "cmd_1");
}

#[test]
fn argv_exact_empty_is_accepted_and_argv_prefix_empty_is_rejected() {
    let (_base, _workspace_dir, store) = store_setup();
    let ws = *store.workspace();
    let text = format!(
        r#"
schema_version = 1

[workspace]
identity = "{identity}"

[policy]
workspace_enabled = false

[[command_allow]]
id = "cmd_exact"
executable = "git"
match = "argv_exact"
argv = []
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20

[[command_allow]]
id = "cmd_prefix"
executable = "git"
match = "argv_prefix"
argv = []
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20

[[command_allow]]
id = "cmd_ok"
executable = "git"
match = "argv_prefix"
argv = ["status"]
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20
"#,
        identity = ws.as_string()
    );
    write_store_text(store.path(), &text);
    let document = store.load().expect("load");
    let ids: Vec<&str> = document.rules.iter().map(TrustRule::id).collect();
    assert_eq!(ids, vec!["cmd_exact", "cmd_ok"], "exact [] accepted, prefix [] rejected");
    let exact = document.rules.iter().find(|rule| rule.id() == "cmd_exact").unwrap();
    assert_eq!(
        decide_default(&command_op(ws, "git", &[]), std::slice::from_ref(exact)).outcome,
        TrustOutcome::Allow,
        "no-argument invocation matches argv_exact = []"
    );
    assert_eq!(
        decide_default(&command_op(ws, "git", &["status"]), std::slice::from_ref(exact)).outcome,
        TrustOutcome::Prompt
    );
}

#[test]
fn runtime_usage_counters_never_appear_in_serialized_document() {
    let (_base, _workspace_dir, store) = store_setup();
    store.write(&sample_document(&store)).expect("write");
    let text = fs::read_to_string(store.path()).unwrap();
    for needle in ["usage", "used", "counter"] {
        assert!(!text.contains(needle), "usage state must never serialize: {needle}");
    }
}

#[test]
fn unknown_top_level_field_rejects_the_whole_store() {
    let (_base, _workspace_dir, store) = store_setup();
    let text = format!(
        r#"
schema_version = 1

[workspace]
identity = "{identity}"

[policy]
workspace_enabled = false

authority = {{ source = "repository" }}
"#,
        identity = store.workspace().as_string()
    );
    write_store_text(store.path(), &text);
    assert!(matches!(store.load(), Err(TrustStoreError::ValidationFailure(_))));
    assert!(store.effective().rules.is_empty());
}

#[test]
fn malformed_store_toml_yields_empty_effective_rules() {
    let (_base, _workspace_dir, store) = store_setup();
    write_store_text(store.path(), "schema_version = 1\n[[command_allow\n");
    assert!(matches!(store.load(), Err(TrustStoreError::ParseFailure(_))));
    assert!(store.effective().rules.is_empty());
    let op = command_op(*store.workspace(), "git", &["status"]);
    assert_eq!(decide_default(&op, &store.effective().rules).outcome, TrustOutcome::Prompt);
}

#[test]
fn invalid_path_prefixes_are_rejected() {
    let (_base, _workspace_dir, store) = store_setup();
    let ws = *store.workspace();
    let bad_prefixes = [
        "",
        ".",
        "..",
        "/abs",
        "src/../etc",
        "src//x",
        "src/./x",
        "a/**/b",
        "a/?.rs",
        "src/{a,b}",
        ".env",
        "src/.git",
        "secrets",
        "C:src",
    ];
    let mut entries = String::new();
    for (index, prefix) in bad_prefixes.iter().enumerate() {
        entries.push_str(&format!(
            r#"
[[read_path_allow]]
id = "bad_{index}"
path_prefix = "{prefix}"
max_bytes = 1024
"#
        ));
    }
    // Backslash and control forms cannot even be expressed as TOML basic
    // strings; the parser must reject them directly.
    assert!(PathPrefix::parse("src\\x").is_err(), "backslash prefix rejected");
    assert!(PathPrefix::parse("src/\u{0}").is_err(), "control prefix rejected");
    entries.push_str(
        r#"
[[read_path_allow]]
id = "read_1"
path_prefix = "src/generated"
max_bytes = 1024
"#,
    );
    let text = format!(
        r#"
schema_version = 1

[workspace]
identity = "{identity}"

[policy]
workspace_enabled = false
{entries}
"#,
        identity = ws.as_string()
    );
    write_store_text(store.path(), &text);
    let document = store.load().expect("load");
    let ids: Vec<&str> = document.rules.iter().map(TrustRule::id).collect();
    assert_eq!(ids, vec!["read_1"], "only the valid prefix loads");
}

#[test]
fn invalid_arguments_json_entries_are_rejected() {
    let (_base, _workspace_dir, store) = store_setup();
    let ws = *store.workspace();
    let oversized = "x".repeat(5000);
    let bad = [
        r#"{"a":1,"a":2}"#,
        "[1,2,3]",
        r#"{"token":"abc"}"#,
        r#"{"nested":{"api_key":"x"}}"#,
        r#"{"a":"\u0000"}"#,
        &oversized,
        "not json",
    ];
    let mut entries = String::new();
    for (index, args) in bad.iter().enumerate() {
        // JSON object payloads are persisted as TOML basic strings, so the
        // embedded quotes must be escaped exactly like the documented
        // `arguments_json = "{\"path\":\"src/main.rs\"}"` form.
        let escaped = args.replace('\\', "\\\\").replace('\"', "\\\"");
        entries.push_str(&format!(
            r#"
[[mcp_allow]]
id = "bad_mcp_{index}"
server = "ee"
transport_identity = "stdio:test"
tool = "ee_read_file"
tool_schema_version = 1
arguments_json = "{escaped}"
"#
        ));
    }
    entries.push_str(
        r#"
[[mcp_allow]]
id = "mcp_1"
server = "ee"
transport_identity = "stdio:test"
tool = "ee_read_file"
tool_schema_version = 1
arguments_json = "{ \"path\": \"src/main.rs\", \"b\": { \"x\": 1 } }"
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20
"#,
    );
    let text = format!(
        r#"
schema_version = 1

[workspace]
identity = "{identity}"

[policy]
workspace_enabled = false
{entries}
"#,
        identity = ws.as_string()
    );
    write_store_text(store.path(), &text);
    let document = store.load().expect("load");
    assert_eq!(document.rules.len(), 1, "only the valid canonical argument object loads");
    let TrustRule::Mcp(rule) = &document.rules[0] else {
        panic!("expected mcp rule");
    };
    assert_eq!(rule.arguments_json, r#"{"b":{"x":1},"path":"src/main.rs"}"#);
}

#[test]
fn missing_optional_read_scope_is_preserved_and_mandatory_scope_is_enforced() {
    let (_base, _workspace_dir, store) = store_setup();
    let ws = *store.workspace();
    let text = format!(
        r#"
schema_version = 1

[workspace]
identity = "{identity}"

[policy]
workspace_enabled = false

[[read_path_allow]]
id = "read_unlimited"
path_prefix = "src"
max_bytes = 1024

[[command_allow]]
id = "cmd_no_expiry"
executable = "git"
match = "argv_exact"
argv = ["status"]
max_uses = 20

[[command_allow]]
id = "cmd_no_uses"
executable = "git"
match = "argv_exact"
argv = ["status"]
expires_at = "2026-08-08T12:00:00Z"

[[write_allow]]
id = "write_no_uses"
operation = "create"
path_prefix = "src/generated"
max_files = 5
max_total_bytes = 65536
max_file_bytes = 16384
expires_at = "2026-08-08T12:00:00Z"
"#,
        identity = ws.as_string()
    );
    write_store_text(store.path(), &text);
    let document = store.load().expect("load");
    let ids: Vec<&str> = document.rules.iter().map(TrustRule::id).collect();
    assert_eq!(ids, vec!["read_unlimited"], "read may omit scope; execute/write cannot");
    let TrustRule::ReadPath(rule) = &document.rules[0] else {
        panic!("expected read rule");
    };
    assert_eq!(rule.scope.expires_at, None);
    assert_eq!(rule.scope.max_uses, None);
}

// ── Store: file behavior and atomicity ───────────────────────────────────────

#[test]
fn missing_store_loads_an_empty_document() {
    let (_base, _workspace_dir, store) = store_setup();
    let document = store.load().expect("missing store is an empty document");
    assert_eq!(document.workspace, *store.workspace());
    assert!(!document.workspace_enabled);
    assert!(document.rules.is_empty());
    assert!(store.effective().rules.is_empty());
    let op = command_op(*store.workspace(), "git", &["status"]);
    assert_eq!(decide_default(&op, &store.effective().rules).outcome, TrustOutcome::Prompt);
}

#[test]
fn store_filename_derives_from_digest_and_never_contains_raw_path() {
    let (base, workspace_dir, store) = store_setup();
    let filename = store.path().file_name().unwrap().to_string_lossy().to_string();
    assert_eq!(filename, format!("{}.toml", store.workspace().hex()));
    assert!(filename.ends_with(&format!("{}.toml", store.workspace().hex())));
    let raw_name = workspace_dir.path().file_name().unwrap().to_string_lossy().to_string();
    assert!(!filename.contains(&raw_name), "raw workspace path must never appear");
    assert!(!filename.contains("sha256:sha256:"));
    assert!(store.path().starts_with(base.path().join("trust")));
}

#[test]
fn store_round_trip_keeps_serialization_canonical() {
    let (_base, _workspace_dir, store) = store_setup();
    store.write(&sample_document(&store)).expect("write");
    let first = fs::read_to_string(store.path()).unwrap();
    store.write(&store.load().expect("reload")).expect("rewrite");
    let second = fs::read_to_string(store.path()).unwrap();
    assert_eq!(first, second, "load → write must be byte-stable");
}

#[test]
fn add_rule_appends_or_reuses_by_stable_rule_id() {
    let (_base, _workspace_dir, store) = store_setup();
    let ws = *store.workspace();
    let rule = command_rule(spec("cmd_1"), ws, "git", MatchMode::ArgvExact, &["status"]);

    let document = store.add_rule(rule.clone()).expect("first grant");
    assert_eq!(document.rules.len(), 1);
    let bytes_after_first = fs::read(store.path()).unwrap();

    let reused = store.add_rule(rule.clone()).expect("reuse");
    assert_eq!(reused.rules.len(), 1, "same id must not duplicate the grant");
    assert_eq!(fs::read(store.path()).unwrap(), bytes_after_first, "reuse must not rewrite");

    let second = command_rule(spec("cmd_2"), ws, "git", MatchMode::ArgvExact, &["diff"]);
    let appended = store.add_rule(second).expect("append");
    assert_eq!(appended.rules.len(), 2);
    assert_ne!(fs::read(store.path()).unwrap(), bytes_after_first, "new id appends");
}

#[test]
fn add_rule_rejects_rules_bound_to_another_workspace() {
    let (_base, _workspace_dir, store) = store_setup();
    let foreign = command_rule(
        spec("cmd_1"),
        identity(b"/work/other"),
        "git",
        MatchMode::ArgvExact,
        &["status"],
    );
    match store.add_rule(foreign) {
        Err(TrustStoreError::IdentityMismatch) => {}
        other => panic!("expected IdentityMismatch, got {other:?}"),
    }
    assert!(!store.path().exists(), "failed grant must not create the store");
}

#[cfg(unix)]
#[test]
fn symlinked_store_is_rejected() {
    let (_base, _workspace_dir, store) = store_setup();
    store.write(&sample_document(&store)).expect("write");
    let real = store.path().with_extension("real.toml");
    fs::rename(store.path(), &real).unwrap();
    std::os::unix::fs::symlink(&real, store.path()).unwrap();
    assert!(matches!(store.load(), Err(TrustStoreError::PermissionFailure(_))));
    assert!(store.effective().rules.is_empty());
}

#[cfg(unix)]
#[test]
fn insecure_store_permissions_are_rejected() {
    use std::os::unix::fs::PermissionsExt;
    let (_base, _workspace_dir, store) = store_setup();
    store.write(&sample_document(&store)).expect("write");

    fs::set_permissions(store.path(), fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        matches!(store.load(), Err(TrustStoreError::PermissionFailure(_))),
        "file 0644 must fail closed"
    );
    fs::set_permissions(store.path(), fs::Permissions::from_mode(0o600)).unwrap();
    assert!(store.load().is_ok(), "file 0600 loads");

    let trust_dir = store.path().parent().unwrap();
    fs::set_permissions(trust_dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        matches!(store.load(), Err(TrustStoreError::PermissionFailure(_))),
        "dir 0755 must fail closed"
    );
    fs::set_permissions(trust_dir, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(store.load().is_ok(), "dir 0700 loads");
}

#[test]
fn directory_at_store_path_is_rejected() {
    let (_base, _workspace_dir, store) = store_setup();
    store.write(&sample_document(&store)).expect("write");
    fs::remove_file(store.path()).unwrap();
    fs::create_dir(store.path()).unwrap();
    assert!(matches!(store.load(), Err(TrustStoreError::PermissionFailure(_))));
    assert!(store.effective().rules.is_empty());
}

#[cfg(unix)]
#[test]
fn failed_write_preserves_prior_bytes_and_removes_temporary_file() {
    use std::os::unix::fs::PermissionsExt;
    let (_base, _workspace_dir, store) = store_setup();
    store.write(&sample_document(&store)).expect("write");
    let prior = fs::read(store.path()).unwrap();

    let trust_dir = store.path().parent().unwrap();
    fs::set_permissions(trust_dir, fs::Permissions::from_mode(0o500)).unwrap();
    let result = store.write(&sample_document(&store));
    fs::set_permissions(trust_dir, fs::Permissions::from_mode(0o700)).unwrap();

    assert!(result.is_err(), "write into read-only trust dir must fail");
    assert_eq!(fs::read(store.path()).unwrap(), prior, "failed write preserves prior store bytes");
    assert_eq!(
        dir_entries(trust_dir),
        vec![store.path().to_path_buf()],
        "no temp file left behind"
    );
}

#[test]
fn write_rejects_duplicate_or_empty_rule_ids() {
    let (_base, _workspace_dir, store) = store_setup();
    let ws = *store.workspace();
    let mut document = sample_document(&store);
    let duplicate = command_rule(spec("cmd_1"), ws, "git", MatchMode::ArgvExact, &["stash"]);
    document.rules.push(duplicate);
    assert!(matches!(store.write(&document), Err(TrustStoreError::ValidationFailure(_))));
    assert!(!store.path().exists());
}

#[test]
fn store_rules_feed_the_evaluator_end_to_end() {
    let (_base, _workspace_dir, store) = store_setup();
    let ws = *store.workspace();
    store
        .add_rule(command_rule(spec("cmd_1"), ws, "git", MatchMode::ArgvExact, &["status"]))
        .unwrap();
    let effective = store.effective();

    let matching = command_op(ws, "git", &["status"]);
    let decision = decide_default(&matching, &effective.rules);
    assert_eq!(decision.outcome, TrustOutcome::Allow);
    assert_eq!(decision.rule_id.as_deref(), Some("cmd_1"));

    // Session deny still wins over the persisted grant.
    let mut session = SessionPolicy::default();
    session.record("s1", "fp", SessionChoice::Deny);
    let denied = decide(
        &matching,
        &effective.rules,
        &session,
        "s1",
        "2026-08-07T12:00:00Z",
        &UsageSnapshot::default(),
        true,
    );
    assert_eq!(denied.outcome, TrustOutcome::Prompt);
    assert_eq!(denied.reason, DecisionReason::SessionDeny);
}
