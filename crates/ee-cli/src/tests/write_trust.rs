//! Phase 5 bounded create/modify write trust tests (ISSUES.md "Unified
//! Host-Local Workspace Trust Policy"): the write matcher, safety maxima,
//! protected-path rejection, and the terminal-approval integration with
//! host-local persistence.
//!
//! Only regular UTF-8 text create/modify operations within strict path and
//! size budgets qualify; destructive, binary, sensitive, and external
//! filesystem operations never bypass approval, and only successfully
//! dispatched trusted writes consume authority.

#[cfg(feature = "agents")]
use std::path::Path;
use std::time::SystemTime;

use tempfile::TempDir;

use crate::policy::evaluator::PolicyInput;
use crate::policy::rules::{
    MAX_WRITE_FILE_BYTES, MAX_WRITE_FILES, MAX_WRITE_TOTAL_BYTES, RawWriteRule,
};
use crate::policy::session::SessionPolicy;
use crate::policy::store::{TrustStore, TrustStoreDocument};
use crate::policy::{
    DecisionReason, OperationIdentity, PathPrefix, TrustCategory, TrustDecision, TrustOperation,
    TrustOutcome, TrustRule, TrustRuleScope, UsageSnapshot, WorkspaceIdentity, WriteOperationKind,
    WriteRule, evaluate,
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

fn scope(workspace: WorkspaceIdentity, max_uses: Option<u64>) -> TrustRuleScope {
    TrustRuleScope {
        workspace,
        agent: None,
        expires_at: Some(at("2026-08-08T12:00:00Z")),
        max_uses,
    }
}

fn write_rule(
    id: &str,
    workspace: WorkspaceIdentity,
    operation: WriteOperationKind,
    prefix: &str,
    max_files: u64,
    max_total_bytes: u64,
    max_file_bytes: u64,
) -> TrustRule {
    TrustRule::Write(WriteRule {
        id: id.to_string(),
        scope: scope(workspace, Some(20)),
        operation,
        path_prefix: PathPrefix::parse(prefix).expect("valid prefix"),
        max_files,
        max_total_bytes,
        max_file_bytes,
    })
}

fn write_op(
    workspace: WorkspaceIdentity,
    category: TrustCategory,
    relative: &str,
    files: u64,
    total: Option<u64>,
    largest: Option<u64>,
) -> TrustOperation {
    TrustOperation {
        workspace,
        agent: None,
        transport: crate::policy::TransportKind::Acp,
        category,
        identity: OperationIdentity::Write {
            relative_path: relative.to_string(),
            file_count: files,
            total_bytes: total,
            max_file_bytes: largest,
        },
    }
}

fn decide(
    op: &TrustOperation,
    rules: &[TrustRule],
    session: &SessionPolicy,
    workspace_enabled: bool,
    usage: &UsageSnapshot,
) -> TrustDecision {
    evaluate(&PolicyInput {
        session_id: "s1",
        fingerprint: "fp",
        operation: op,
        session,
        rules,
        now: at("2026-08-07T12:00:00Z"),
        usage,
        workspace_enabled,
    })
}

/// `finite` bundles the mandatory expiry and use budget; invalid-variant
/// cases clone the base record and mutate one field.
#[allow(clippy::too_many_arguments)]
fn raw_write_rule(
    id: &str,
    operation: WriteOperationKind,
    prefix: &str,
    max_files: u64,
    max_total_bytes: u64,
    max_file_bytes: u64,
    finite: Option<(&str, u64)>,
) -> RawWriteRule {
    RawWriteRule {
        id: id.to_string(),
        agent: None,
        operation,
        path_prefix: prefix.to_string(),
        max_files,
        max_total_bytes,
        max_file_bytes,
        expires_at: finite.map(|(expires, _)| expires.to_string()),
        max_uses: finite.map(|(_, uses)| uses),
    }
}

// ── Matcher and gate semantics ───────────────────────────────────────────────

#[test]
fn write_rules_do_not_require_the_workspace_gate() {
    let ws = identity(b"/work/root");
    let rule =
        write_rule("write_1", ws, WriteOperationKind::Create, "src/generated", 5, 65_536, 16_384);
    let op =
        write_op(ws, TrustCategory::WriteCreate, "src/generated/a.rs", 1, Some(1024), Some(1024));
    let allowed = decide(
        &op,
        std::slice::from_ref(&rule),
        &SessionPolicy::default(),
        false,
        &UsageSnapshot::default(),
    );
    assert_eq!(allowed.outcome, TrustOutcome::Allow, "writes are gate-independent like commands");
    // The gate alone never authorizes anything.
    let bare = decide(&op, &[], &SessionPolicy::default(), true, &UsageSnapshot::default());
    assert_eq!(bare.reason, DecisionReason::NoMatchingRule);
}

#[test]
fn write_rule_requires_finite_expiry_and_use_budget() {
    let ws = identity(b"/work/root");
    let base = raw_write_rule(
        "write_1",
        WriteOperationKind::Create,
        "src/generated",
        5,
        65_536,
        16_384,
        Some(("2026-08-08T12:00:00Z", 5)),
    );
    assert!(WriteRule::from_raw(base.clone(), ws).is_ok());

    let mut no_expiry = base.clone();
    no_expiry.id = "write_2".into();
    no_expiry.expires_at = None;
    assert!(WriteRule::from_raw(no_expiry, ws).is_err(), "expiry is mandatory");

    let mut no_uses = base.clone();
    no_uses.id = "write_3".into();
    no_uses.max_uses = None;
    assert!(WriteRule::from_raw(no_uses, ws).is_err(), "max_uses is mandatory");

    let mut zero_uses = base.clone();
    zero_uses.id = "write_4".into();
    zero_uses.max_uses = Some(0);
    assert!(WriteRule::from_raw(zero_uses, ws).is_err(), "zero budget is invalid");
}

#[test]
fn write_caps_above_safety_maxima_are_rejected_at_load() {
    let ws = identity(b"/work/root");
    let at_max = raw_write_rule(
        "write_5",
        WriteOperationKind::Create,
        "src/generated",
        MAX_WRITE_FILES,
        MAX_WRITE_TOTAL_BYTES,
        MAX_WRITE_FILE_BYTES,
        Some(("2026-08-08T12:00:00Z", 5)),
    );
    assert!(WriteRule::from_raw(at_max.clone(), ws).is_ok(), "caps at the maximum are valid");

    let mut over_files = at_max.clone();
    over_files.id = "write_1".into();
    over_files.max_files = MAX_WRITE_FILES + 1;
    assert!(WriteRule::from_raw(over_files, ws).is_err(), "file cap above the safety maximum");

    let mut over_total = at_max.clone();
    over_total.id = "write_2".into();
    over_total.max_total_bytes = MAX_WRITE_TOTAL_BYTES + 1;
    assert!(WriteRule::from_raw(over_total, ws).is_err(), "aggregate cap above the safety maximum");

    let mut over_file = at_max.clone();
    over_file.id = "write_3".into();
    over_file.max_file_bytes = MAX_WRITE_FILE_BYTES + 1;
    assert!(WriteRule::from_raw(over_file, ws).is_err(), "per-file cap above the safety maximum");

    // A per-file cap larger than the aggregate is nonsense and rejected.
    let mut inverted = at_max.clone();
    inverted.id = "write_4".into();
    inverted.max_total_bytes = 65_536;
    inverted.max_file_bytes = 262_144;
    assert!(WriteRule::from_raw(inverted, ws).is_err(), "per-file cap exceeds the aggregate");
}

#[test]
fn protected_and_root_wide_write_prefixes_are_rejected() {
    for prefix in [
        "",
        ".",
        "..",
        "src/../generated",
        "/abs",
        "src/*",
        "src/?.rs",
        "src/[ab]",
        "src/.env",
        ".env",
        "secrets",
        "credentials",
        "vault",
        "keys/id_rsa",
        "certs/server.pem",
        "secret",
        "config/secret.json",
        "src/.git/config",
    ] {
        assert!(PathPrefix::parse(prefix).is_err(), "{prefix:?} must be an invalid write prefix");
    }
    assert_eq!(
        PathPrefix::parse("src/generated").expect("valid prefix").display(),
        "src/generated"
    );
}

#[test]
fn exhausted_write_rule_prompts_without_mutating_anything() {
    let ws = identity(b"/work/root");
    let rule =
        write_rule("write_1", ws, WriteOperationKind::Create, "src/generated", 5, 65_536, 16_384);
    let op =
        write_op(ws, TrustCategory::WriteCreate, "src/generated/a.rs", 1, Some(1024), Some(1024));
    let exhausted = UsageSnapshot::new([("write_1".to_string(), 20u64)].into_iter().collect());
    let decision =
        decide(&op, std::slice::from_ref(&rule), &SessionPolicy::default(), true, &exhausted);
    assert_eq!(decision.outcome, TrustOutcome::Prompt, "exhausted budget never allows");
    assert_eq!(decision.reason, DecisionReason::NoMatchingRule);
}

#[test]
fn session_deny_overrides_matching_write_allow() {
    let ws = identity(b"/work/root");
    let rule =
        write_rule("write_1", ws, WriteOperationKind::Create, "src/generated", 5, 65_536, 16_384);
    let op =
        write_op(ws, TrustCategory::WriteCreate, "src/generated/a.rs", 1, Some(1024), Some(1024));
    let mut session = SessionPolicy::default();
    session.record("s1", "fp", crate::policy::SessionChoice::Deny);
    let decision =
        decide(&op, std::slice::from_ref(&rule), &session, true, &UsageSnapshot::default());
    assert_eq!(decision.reason, DecisionReason::SessionDeny);
    assert_eq!(decision.outcome, TrustOutcome::Prompt);
}

#[test]
fn write_allow_round_trips_through_the_store_without_usage_counters() {
    let base = TempDir::new().expect("state dir");
    let workspace = TempDir::new().expect("workspace root");
    let store = TrustStore::at(base.path(), workspace.path()).expect("store");
    let rule = write_rule(
        "write_seed",
        *store.workspace(),
        WriteOperationKind::Modify,
        "src/generated",
        3,
        65_536,
        16_384,
    );
    let document = TrustStoreDocument {
        workspace: *store.workspace(),
        workspace_enabled: false,
        rules: vec![rule],
    };
    store.write(&document).expect("write seed");

    let reloaded = store.load().expect("reload");
    assert_eq!(reloaded.rules.len(), 1);
    let TrustRule::Write(write) = &reloaded.rules[0] else {
        panic!("write rule expected");
    };
    assert_eq!(write.id, "write_seed");
    assert_eq!(write.operation, WriteOperationKind::Modify);
    assert_eq!(write.path_prefix.display(), "src/generated");
    assert_eq!(write.max_files, 3);

    // Runtime usage counters are session-local and never serialized.
    let text = std::fs::read_to_string(store.path()).expect("store text");
    assert!(!text.contains("used"), "usage counters must not appear in the document: {text}");

    // The reloaded rule still authorizes its bounded modify.
    let op = write_op(
        *store.workspace(),
        TrustCategory::WriteModify,
        "src/generated/main.rs",
        1,
        Some(4096),
        Some(4096),
    );
    let decision =
        decide(&op, &reloaded.rules, &SessionPolicy::default(), true, &UsageSnapshot::default());
    assert_eq!(decision.outcome, TrustOutcome::Allow);
    assert_eq!(decision.rule_id.as_deref(), Some("write_seed"));
}

// ── End-to-end terminal-approval integration (agents feature) ────────────────

#[cfg(feature = "agents")]
mod e2e {
    use super::*;
    use crate::app::{App, PreparedWrite, WriteExpectation, WriteReplyKind};
    use crate::tests::agent_mcp::{
        base_agent_script, connect_proxy, mcp_app, open_pane_and_wait_ready, press, proxy_recv,
        proxy_send, settle, wait_until,
    };
    use crate::tests::helpers::run_ex;
    use crossterm::event::{KeyCode, KeyModifiers};
    use serde_json::{Value, json};
    use std::fs;

    fn seed_write_store(
        state_dir: &Path,
        workspace: &Path,
        operation: WriteOperationKind,
        prefix: &str,
        max_files: u64,
        max_total_bytes: u64,
        max_file_bytes: u64,
    ) {
        let store = TrustStore::at(state_dir, workspace).unwrap();
        let rule = write_rule(
            "write_seed",
            *store.workspace(),
            operation,
            prefix,
            max_files,
            max_total_bytes,
            max_file_bytes,
        );
        let document = TrustStoreDocument {
            workspace: *store.workspace(),
            workspace_enabled: true,
            rules: vec![rule],
        };
        store.write(&document).expect("seed store");
    }

    fn write_frame(path: &Path, content: &str) -> Value {
        json!({
            "method": "write_text_file",
            "path": path.display().to_string(),
            "content": content,
        })
    }

    fn ledger_workspace(state_dir: &Path, workspace: &Path) -> WorkspaceIdentity {
        *TrustStore::at(state_dir, workspace).unwrap().workspace()
    }

    fn open_pane_and_select(app: &mut App, index: usize) {
        run_ex(app, "agents");
        for _ in 0..index {
            press(app, KeyCode::Right, KeyModifiers::NONE);
        }
        press(app, KeyCode::Enter, KeyModifiers::NONE);
    }

    #[test]
    fn create_rule_auto_allows_bounded_create_and_records_use() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        fs::create_dir_all(temp.path().join("src/generated")).unwrap();
        seed_write_store(
            &state_dir,
            temp.path(),
            WriteOperationKind::Create,
            "src/generated",
            5,
            65_536,
            16_384,
        );

        let target = temp.path().join("src/generated/new.rs");
        let mut stream = connect_proxy(&app);
        proxy_send(&mut stream, 1, write_frame(&target, "fn new() {}"));
        wait_until(&mut app, "trusted create dispatched", |app| {
            app.agents.approvals.is_empty() && target.exists()
        });
        let reply = proxy_recv(&mut stream);
        assert!(reply["result"]["value"].as_str() == Some("ok"), "auto-allowed: {reply}");
        assert_eq!(fs::read_to_string(&target).unwrap().trim_end(), "fn new() {}");
        assert_eq!(
            app.agents.usage_ledger.used(
                ledger_workspace(&state_dir, temp.path()),
                "proxy",
                "write_seed"
            ),
            1,
            "one use consumed"
        );
    }

    #[test]
    fn modify_rule_auto_allows_bounded_edit_within_budget() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        fs::create_dir_all(temp.path().join("src/generated")).unwrap();
        let target = temp.path().join("src/generated/main.rs");
        fs::write(&target, "v0").unwrap();
        seed_write_store(
            &state_dir,
            temp.path(),
            WriteOperationKind::Modify,
            "src/generated",
            5,
            65_536,
            16_384,
        );

        let mut stream = connect_proxy(&app);
        proxy_send(&mut stream, 1, write_frame(&target, "v1"));
        wait_until(&mut app, "trusted modify dispatched", |app| {
            app.agents.approvals.is_empty()
                && fs::read_to_string(&target).map(|t| t == "v1").unwrap_or(false)
        });
        let reply = proxy_recv(&mut stream);
        assert!(reply["result"]["value"].as_str() == Some("ok"), "auto-allowed: {reply}");
        assert_eq!(
            app.agents.usage_ledger.used(
                ledger_workspace(&state_dir, temp.path()),
                "proxy",
                "write_seed"
            ),
            1
        );
    }

    #[test]
    fn operation_kind_never_crosses_in_the_approval_path() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        fs::create_dir_all(temp.path().join("src/generated")).unwrap();
        let target = temp.path().join("src/generated/existing.rs");
        fs::write(&target, "v0").unwrap();
        // A create rule is seeded; the same-dir modify request must prompt.
        seed_write_store(
            &state_dir,
            temp.path(),
            WriteOperationKind::Create,
            "src/generated",
            5,
            65_536,
            16_384,
        );

        let mut stream = connect_proxy(&app);
        proxy_send(&mut stream, 1, write_frame(&target, "v1"));
        wait_until(&mut app, "modify against create rule prompts", |app| {
            !app.agents.approvals.is_empty()
        });
        assert_eq!(fs::read_to_string(&target).unwrap(), "v0", "disk unchanged");
        assert!(app.agents.usage_ledger.is_empty(), "no budget consumed");
        open_pane_and_select(&mut app, 0); // Allow once
        let reply = proxy_recv(&mut stream);
        assert!(reply["result"]["value"].as_str() == Some("ok"), "once-approved: {reply}");
        assert_eq!(fs::read_to_string(&target).unwrap(), "v1");
    }

    #[test]
    fn over_budget_and_uncovered_writes_prompt() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        fs::create_dir_all(temp.path().join("src/generated")).unwrap();
        seed_write_store(
            &state_dir,
            temp.path(),
            WriteOperationKind::Create,
            "src/generated",
            5,
            64,
            64,
        );

        let mut stream = connect_proxy(&app);
        // Over-budget: the content exceeds the per-file cap, so no match.
        let oversized = temp.path().join("src/generated/big.rs");
        proxy_send(&mut stream, 1, write_frame(&oversized, &"x".repeat(1024)));
        wait_until(&mut app, "over-budget write prompts", |app| !app.agents.approvals.is_empty());
        assert!(!oversized.exists(), "over-budget write must not dispatch");
        open_pane_and_select(&mut app, 0); // Allow once
        let _ = proxy_recv(&mut stream);

        // Uncovered directory: no rule matches, so it prompts.
        fs::create_dir_all(temp.path().join("other")).unwrap();
        let uncovered = temp.path().join("other/y.rs");
        proxy_send(&mut stream, 2, write_frame(&uncovered, "fn y() {}"));
        wait_until(&mut app, "uncovered write prompts", |app| !app.agents.approvals.is_empty());
        assert!(!uncovered.exists(), "uncovered write must not dispatch");
        open_pane_and_select(&mut app, 0); // Allow once
        let _ = proxy_recv(&mut stream);
    }

    #[test]
    fn protected_external_and_escape_writes_never_auto_allow() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        fs::create_dir_all(temp.path().join("src/generated")).unwrap();
        seed_write_store(
            &state_dir,
            temp.path(),
            WriteOperationKind::Create,
            "src/generated",
            5,
            65_536,
            16_384,
        );

        let mut stream = connect_proxy(&app);
        // Protected path inside the covered directory still prompts.
        let env = temp.path().join("src/generated/.env");
        proxy_send(&mut stream, 1, write_frame(&env, "TOKEN=x"));
        wait_until(&mut app, "protected write prompts", |app| !app.agents.approvals.is_empty());
        assert!(!env.exists());
        open_pane_and_select(&mut app, 0); // Allow once
        let _ = proxy_recv(&mut stream);

        // External and symlink-escape targets are rejected before approval.
        let outside = TempDir::new().unwrap();
        let external = outside.path().join("x.txt");
        proxy_send(&mut stream, 2, write_frame(&external, "x"));
        settle(&mut app);
        let reply = proxy_recv(&mut stream);
        assert!(reply["result"]["error"].is_object(), "external write rejected: {reply}");
        assert!(!external.exists());

        #[cfg(unix)]
        {
            let link = temp.path().join("src/generated/escape");
            std::os::unix::fs::symlink(outside.path(), &link).unwrap();
            let escaped = link.join("y.txt");
            proxy_send(&mut stream, 3, write_frame(&escaped, "y"));
            settle(&mut app);
            let reply = proxy_recv(&mut stream);
            assert!(reply["result"]["error"].is_object(), "symlink escape rejected: {reply}");
            assert!(!outside.path().join("y.txt").exists());
        }
        assert!(app.agents.usage_ledger.is_empty(), "no budget consumed by rejected writes");
    }

    #[test]
    fn persistent_option_appears_only_for_eligible_writes() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        fs::create_dir_all(temp.path().join("src/generated")).unwrap();

        let mut stream = connect_proxy(&app);
        // Eligible narrow create: the persistent write option is offered.
        let eligible = temp.path().join("src/generated/a.rs");
        proxy_send(&mut stream, 1, write_frame(&eligible, "fn a() {}"));
        wait_until(&mut app, "eligible write queued", |app| !app.agents.approvals.is_empty());
        let labels: Vec<&str> = app
            .agents
            .approvals
            .front()
            .unwrap()
            .options
            .iter()
            .map(|(label, _)| label.as_str())
            .collect();
        assert!(
            labels.contains(&"Allow for 1 hour / 5 uses"),
            "eligible write must offer the bounded persistent option: {labels:?}"
        );
        open_pane_and_select(&mut app, 0); // Allow once
        let _ = proxy_recv(&mut stream);

        // Over-budget write: no persistent option.
        let oversized = temp.path().join("src/generated/big.rs");
        let huge = "x".repeat((crate::policy::MAX_WRITE_FILE_BYTES + 1) as usize);
        proxy_send(&mut stream, 2, write_frame(&oversized, &huge));
        wait_until(&mut app, "over-budget write queued", |app| !app.agents.approvals.is_empty());
        let labels: Vec<&str> = app
            .agents
            .approvals
            .front()
            .unwrap()
            .options
            .iter()
            .map(|(label, _)| label.as_str())
            .collect();
        assert!(
            !labels.iter().any(|label| label.contains("1 hour")),
            "over-budget write must not offer persistence: {labels:?}"
        );
        open_pane_and_select(&mut app, 0); // Allow once
        let _ = proxy_recv(&mut stream);

        // Protected write: no persistent option either.
        let env = temp.path().join("src/generated/.env");
        proxy_send(&mut stream, 3, write_frame(&env, "TOKEN=x"));
        wait_until(&mut app, "protected write queued", |app| !app.agents.approvals.is_empty());
        let labels: Vec<&str> = app
            .agents
            .approvals
            .front()
            .unwrap()
            .options
            .iter()
            .map(|(label, _)| label.as_str())
            .collect();
        assert!(
            !labels.iter().any(|label| label.contains("1 hour")),
            "protected write must not offer persistence: {labels:?}"
        );
        open_pane_and_select(&mut app, 0); // Allow once
        let _ = proxy_recv(&mut stream);
    }

    #[test]
    fn persist_grant_derives_narrow_rule_and_auto_allows_next_write() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        fs::create_dir_all(temp.path().join("src/generated")).unwrap();

        let mut stream = connect_proxy(&app);
        let first = temp.path().join("src/generated/first.rs");
        proxy_send(&mut stream, 1, write_frame(&first, "fn first() {}"));
        wait_until(&mut app, "first write queued", |app| !app.agents.approvals.is_empty());
        open_pane_and_select(&mut app, 4); // Allow for 1 hour / 5 uses
        wait_until(&mut app, "first write dispatched", |_| first.exists());
        let reply = proxy_recv(&mut stream);
        assert!(reply["result"]["value"].as_str() == Some("ok"), "persisted allow: {reply}");

        // The derived rule is narrower than the safety maxima and covers the
        // canonical directory prefix of the approved request.
        let store = TrustStore::at(&state_dir, temp.path()).unwrap();
        let document = store.load().unwrap();
        assert_eq!(document.rules.len(), 1, "one derived write rule");
        let TrustRule::Write(write) = &document.rules[0] else {
            panic!("write rule expected");
        };
        assert_eq!(write.operation, WriteOperationKind::Create);
        assert_eq!(write.path_prefix.display(), "src/generated");
        assert_eq!(write.max_files, 1);
        assert_eq!(write.max_total_bytes, "fn first() {}".len() as u64);
        assert_eq!(write.max_file_bytes, "fn first() {}".len() as u64);
        assert_eq!(write.scope.max_uses, Some(crate::app::PERSISTENT_WRITE_MAX_USES));

        // The identical operation and any other in-prefix create within the
        // derived byte budget auto-allow through the persisted rule.
        let second = temp.path().join("src/generated/second.rs");
        proxy_send(&mut stream, 2, write_frame(&second, "fn a() {}"));
        wait_until(&mut app, "second write auto-allowed", |app| {
            app.agents.approvals.is_empty() && second.exists()
        });
        let reply = proxy_recv(&mut stream);
        assert!(reply["result"]["value"].as_str() == Some("ok"), "auto-allowed: {reply}");
        assert_eq!(fs::read_to_string(&second).unwrap().trim_end(), "fn a() {}");
        assert_eq!(
            app.agents.usage_ledger.used(
                ledger_workspace(&state_dir, temp.path()),
                "proxy",
                &write.id
            ),
            2,
            "two uses consumed"
        );
    }

    #[test]
    fn persistence_failure_denies_without_dispatch_and_keeps_budget() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        fs::create_dir_all(temp.path().join("src/generated")).unwrap();
        // Create the store (and its 0700 trust directory) first.
        seed_write_store(
            &state_dir,
            temp.path(),
            WriteOperationKind::Create,
            "other",
            1,
            4096,
            4096,
        );

        let mut stream = connect_proxy(&app);
        let target = temp.path().join("src/generated/blocked.rs");
        proxy_send(&mut stream, 1, write_frame(&target, "fn blocked() {}"));
        wait_until(&mut app, "write queued", |app| !app.agents.approvals.is_empty());
        assert!(app.agents.usage_ledger.is_empty());

        // Make the trust directory read-only so the new grant cannot persist.
        let trust_dir =
            TrustStore::at(&state_dir, temp.path()).unwrap().path().parent().unwrap().to_path_buf();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&trust_dir, fs::Permissions::from_mode(0o500)).unwrap();
        }
        open_pane_and_select(&mut app, 4); // Allow for 1 hour / 5 uses
        wait_until(&mut app, "denied reply", |app| app.agents.approvals.is_empty());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&trust_dir, fs::Permissions::from_mode(0o700)).unwrap();
        }

        let reply = proxy_recv(&mut stream);
        let error = reply["result"]["error"].as_object().expect("denied: {reply}");
        assert!(
            error.get("denied").and_then(Value::as_bool).unwrap_or(false),
            "denied flag: {reply}"
        );
        assert!(!target.exists(), "write must stay undispatched on persistence failure");
        assert!(
            app.agents.usage_ledger.is_empty(),
            "usage budget unchanged after persistence failure"
        );
        let store = TrustStore::at(&state_dir, temp.path()).unwrap();
        assert_eq!(store.load().unwrap().rules.len(), 1, "only the seeded rule persists");
    }

    /// A read-only parent directory makes the buffer save fail after the
    /// rule matched, so no use may be consumed (Unix only: mode bits are the
    /// only deterministic write-failure lever available in tests).
    #[cfg(unix)]
    #[test]
    fn failed_write_consumes_no_use() {
        use std::os::unix::fs::PermissionsExt;
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        let generated = temp.path().join("src/generated");
        fs::create_dir_all(&generated).unwrap();
        let target = generated.join("ro.rs");
        fs::write(&target, "locked").unwrap();
        seed_write_store(
            &state_dir,
            temp.path(),
            WriteOperationKind::Modify,
            "src/generated",
            5,
            65_536,
            16_384,
        );

        let mut stream = connect_proxy(&app);
        proxy_send(&mut stream, 1, write_frame(&target, "overwritten"));
        // A read-only parent makes the save fail after the rule matches.
        fs::set_permissions(&generated, fs::Permissions::from_mode(0o500)).unwrap();
        settle(&mut app);
        let reply = proxy_recv(&mut stream);
        fs::set_permissions(&generated, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(reply["result"]["error"].is_object(), "write failed: {reply}");
        assert!(
            app.agents.usage_ledger.is_empty(),
            "failed write must not consume the rule budget"
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "locked", "disk unchanged");
    }

    #[test]
    fn session_deny_overrides_write_allow() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        fs::create_dir_all(temp.path().join("src/generated")).unwrap();

        let mut stream = connect_proxy(&app);
        // Round 1: no rule yet, so the write prompts; record a session deny.
        let target = temp.path().join("src/generated/sd.rs");
        proxy_send(&mut stream, 1, write_frame(&target, "v1"));
        wait_until(&mut app, "approval queued", |app| !app.agents.approvals.is_empty());
        run_ex(&mut app, "agents");
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        press(&mut app, KeyCode::Right, KeyModifiers::NONE); // Deny session
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        wait_until(&mut app, "round resolved", |app| app.agents.approvals.is_empty());
        let reply = proxy_recv(&mut stream);
        assert!(reply["result"]["error"].is_object(), "denied: {reply}");

        // A matching persistent grant is seeded afterwards; the recorded
        // session deny still wins over the persistent write allow.
        seed_write_store(
            &state_dir,
            temp.path(),
            WriteOperationKind::Create,
            "src/generated",
            5,
            65_536,
            16_384,
        );
        proxy_send(&mut stream, 2, write_frame(&target, "v2"));
        settle(&mut app);
        let reply = proxy_recv(&mut stream);
        assert!(reply["result"]["error"].is_object(), "denied: {reply}");
        assert!(app.agents.approvals.is_empty(), "session deny must not queue a prompt");
        assert!(!target.exists(), "session-deny write must not dispatch");
        assert!(app.agents.usage_ledger.is_empty());
    }

    #[test]
    fn teardown_clears_write_usage_ledger() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        fs::create_dir_all(temp.path().join("src/generated")).unwrap();
        seed_write_store(
            &state_dir,
            temp.path(),
            WriteOperationKind::Create,
            "src/generated",
            5,
            65_536,
            16_384,
        );

        let target = temp.path().join("src/generated/t.rs");
        let mut stream = connect_proxy(&app);
        proxy_send(&mut stream, 1, write_frame(&target, "fn t() {}"));
        wait_until(&mut app, "trusted write dispatched", |_| target.exists());
        assert!(!app.agents.usage_ledger.is_empty(), "use recorded");
        let _ = proxy_recv(&mut stream);

        app.shutdown_agents();
        assert!(app.agents.usage_ledger.is_empty(), "usage rows die with the session teardown");
    }

    #[test]
    fn batch_shape_requires_one_directory_and_uniform_kind() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let workspace = temp.path();
        fs::create_dir_all(workspace.join("src/generated")).unwrap();
        fs::create_dir_all(workspace.join("src/other")).unwrap();
        let prepared = |path: &Path, content: &str| PreparedWrite {
            path: path.to_path_buf(),
            content: content.to_string(),
            tool_call_id: None,
            expectation: WriteExpectation::Blind,
            reply_kind: WriteReplyKind::ProxyStructured,
            proxy_edit_count: 1,
        };

        let ws = *TrustStore::at(&temp.path().join("state"), workspace).unwrap().workspace();
        // Two creates in one directory normalize to one bounded create.
        let same_dir = vec![
            prepared(&workspace.join("src/generated/a.rs"), "fn a() {}"),
            prepared(&workspace.join("src/generated/b.rs"), "fn b() {}"),
        ];
        let (category, identity) = app.native_write_batch_operation(&same_dir).expect("shape");
        assert_eq!(category, TrustCategory::WriteCreate);
        let OperationIdentity::Write { relative_path, file_count, total_bytes, max_file_bytes } =
            identity
        else {
            panic!("write identity");
        };
        assert_eq!(relative_path, "src/generated");
        assert_eq!(file_count, 2);
        assert_eq!(
            total_bytes,
            Some(("fn a() {}" as &str).len() as u64 + "fn b() {}".len() as u64)
        );
        assert_eq!(max_file_bytes, Some("fn b() {}".len() as u64));

        // A mixed create/modify batch is unknown (no single operation kind).
        let mixed = vec![
            prepared(&workspace.join("src/generated/a.rs"), "fn a() {}"),
            prepared(&workspace.join("src/generated/existing.rs"), "edit"),
        ];
        fs::write(workspace.join("src/generated/existing.rs"), "old").unwrap();
        assert!(app.native_write_batch_operation(&mixed).is_none(), "mixed kinds are unknown");

        // Targets in different directories never normalize as one grant.
        let split = vec![
            prepared(&workspace.join("src/generated/a.rs"), "fn a() {}"),
            prepared(&workspace.join("src/other/b.rs"), "fn b() {}"),
        ];
        assert!(
            app.native_write_batch_operation(&split).is_none(),
            "split directories are unknown"
        );

        // A protected target poisons the whole batch.
        let protected = vec![
            prepared(&workspace.join("src/generated/a.rs"), "fn a() {}"),
            prepared(&workspace.join("src/generated/.env"), "TOKEN=x"),
        ];
        assert!(
            app.native_write_batch_operation(&protected).is_none(),
            "protected batch is unknown"
        );

        // The derivable rule shape stays within the safety maxima.
        let shape = app.native_batch_write_rule_shape(&same_dir).expect("rule shape");
        assert_eq!(shape.0, WriteOperationKind::Create);
        assert_eq!(shape.1.display(), "src/generated");
        assert_eq!(shape.2, 2);
        assert!(shape.3 <= crate::policy::MAX_WRITE_TOTAL_BYTES);
        assert!(shape.4 <= crate::policy::MAX_WRITE_FILE_BYTES);
        assert_eq!(ws, *TrustStore::at(&temp.path().join("state"), workspace).unwrap().workspace());
    }
}
