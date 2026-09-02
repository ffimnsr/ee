//! Phase 6 trust lifecycle tests (ISSUES.md "Unified Host-Local Workspace
//! Trust Policy"): injected-clock expiry, finite-use budgets, workspace-
//! scoped usage ledger, invalid persisted scope, and redacted audit
//! metadata.  Every time-dependent assertion runs against a deterministic
//! fake clock — no test depends on wall-clock sleeps.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tempfile::TempDir;

use crate::policy::clock::PolicyClock;
use crate::policy::evaluator::PolicyInput;
use crate::policy::rules::{MAX_RULE_DURATION, MAX_RULE_MAX_USES, MatchMode};
use crate::policy::session::SessionPolicy;
use crate::policy::store::TrustStore;
use crate::policy::{
    CommandRule, DecisionReason, OperationIdentity, TrustCategory, TrustDecision, TrustOperation,
    TrustOutcome, TrustRule, TrustRuleScope, UsageSnapshot, WorkspaceIdentity, evaluate,
};

/// Deterministic virtual now for lifecycle assertions (no wall clock).
fn virtual_now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

fn command_rule(
    id: &str,
    workspace: WorkspaceIdentity,
    expires_at: Option<SystemTime>,
    max_uses: Option<u64>,
) -> TrustRule {
    TrustRule::Command(CommandRule {
        id: id.to_string(),
        effect: crate::policy::TrustEffect::Allow,
        scope: TrustRuleScope { workspace, agent: None, expires_at, max_uses },
        executable: "git".to_string(),
        match_mode: MatchMode::ArgvExact,
        argv: vec!["status".to_string()],
    })
}

fn command_op(workspace: WorkspaceIdentity) -> TrustOperation {
    TrustOperation {
        workspace,
        agent: None,
        transport: crate::policy::TransportKind::Acp,
        category: TrustCategory::Execute,
        identity: OperationIdentity::Command {
            executable: "git".to_string(),
            argv: vec!["status".to_string()],
        },
    }
}

fn write_op(workspace: WorkspaceIdentity, category: TrustCategory) -> TrustOperation {
    TrustOperation {
        workspace,
        agent: None,
        transport: crate::policy::TransportKind::Acp,
        category,
        identity: OperationIdentity::Write {
            relative_path: "src/generated/a.rs".to_string(),
            file_count: 1,
            total_bytes: Some(1024),
            max_file_bytes: Some(1024),
        },
    }
}

fn decide(
    op: &TrustOperation,
    rules: &[TrustRule],
    now: SystemTime,
    usage: &UsageSnapshot,
) -> TrustDecision {
    evaluate(&PolicyInput {
        session_id: "s1",
        fingerprint: "fp",
        operation: op,
        session: &SessionPolicy::default(),
        rules,
        now,
        usage,
        workspace_enabled: true,
        built_in_deny: None,
        tool_default: None,
        category_default: None,
        global_default: None,
    })
}

fn used(count: u64) -> UsageSnapshot {
    UsageSnapshot::new([("lifecycle_rule".to_string(), count)].into_iter().collect())
}

// ── Injected clock and expiry ────────────────────────────────────────────────

#[test]
fn virtual_clock_permits_before_expiry_and_prompts_after() {
    let ws = WorkspaceIdentity::from_canonical_root_bytes(b"/work/root");
    let before = virtual_now();
    let rule =
        command_rule("lifecycle_rule", ws, Some(before + Duration::from_secs(3600)), Some(20));

    let early =
        decide(&command_op(ws), std::slice::from_ref(&rule), before, &UsageSnapshot::default());
    assert_eq!(early.outcome, TrustOutcome::Allow, "permitted before expiry");

    let late = decide(
        &command_op(ws),
        std::slice::from_ref(&rule),
        before + Duration::from_secs(7200),
        &UsageSnapshot::default(),
    );
    assert_eq!(late.outcome, TrustOutcome::Confirm, "expired rule prompts");
    assert_eq!(late.reason, DecisionReason::NoMatchingRule);

    // At exactly the expiry instant the rule is expired (expires_at is
    // exclusive).
    let exact = decide(
        &command_op(ws),
        std::slice::from_ref(&rule),
        before + Duration::from_secs(3600),
        &UsageSnapshot::default(),
    );
    assert_eq!(exact.outcome, TrustOutcome::Confirm, "expiry instant is expired");
}

#[test]
fn execute_and_write_grants_allow_exactly_configured_uses_then_prompt() {
    let ws = WorkspaceIdentity::from_canonical_root_bytes(b"/work/root");
    let now = virtual_now();
    let command =
        command_rule("lifecycle_rule", ws, Some(now + Duration::from_secs(3600)), Some(3));
    let write = TrustRule::Write(crate::policy::WriteRule {
        id: "lifecycle_rule".to_string(),
        effect: crate::policy::TrustEffect::Allow,
        scope: TrustRuleScope {
            workspace: ws,
            agent: None,
            expires_at: Some(now + Duration::from_secs(3600)),
            max_uses: Some(3),
        },
        operation: crate::policy::WriteOperationKind::Modify,
        path_prefix: crate::policy::PathPrefix::parse("src/generated").expect("prefix"),
        max_files: 1,
        max_total_bytes: 65_536,
        max_file_bytes: 16_384,
    });

    for (rule, op) in [(command, command_op(ws)), (write, write_op(ws, TrustCategory::WriteModify))]
    {
        assert_eq!(
            decide(&op, std::slice::from_ref(&rule), now, &used(2)).outcome,
            TrustOutcome::Allow,
            "one use left"
        );
        assert_eq!(
            decide(&op, std::slice::from_ref(&rule), now, &used(3)).outcome,
            TrustOutcome::Confirm,
            "budget exhausted allows exactly max_uses"
        );
        assert_eq!(
            decide(&op, std::slice::from_ref(&rule), now, &used(99)).outcome,
            TrustOutcome::Confirm
        );
    }
}

#[test]
fn scope_constants_reject_oversized_and_overlong_grants() {
    assert_eq!(MAX_RULE_MAX_USES, 10_000);
    assert_eq!(MAX_RULE_DURATION, Duration::from_secs(30 * 24 * 60 * 60));
    // Evaluator-level defense: a code-constructed rule with an overlong
    // window or oversized budget still only matches while scope checks pass.
    let ws = WorkspaceIdentity::from_canonical_root_bytes(b"/work/root");
    let now = virtual_now();
    let overlong = command_rule("r1", ws, Some(now + MAX_RULE_DURATION * 2), Some(20));
    // (The evaluator scope check only rejects expiry in the past; the
    // duration ceiling is a loader rule, asserted in the store tests.)
    assert_eq!(
        decide(&command_op(ws), std::slice::from_ref(&overlong), now, &UsageSnapshot::default())
            .outcome,
        TrustOutcome::Allow
    );
}

// ── Workspace-scoped usage ledger ────────────────────────────────────────────

#[test]
fn usage_ledger_is_keyed_by_workspace_session_and_rule() {
    let ws1 = WorkspaceIdentity::from_canonical_root_bytes(b"/work/one");
    let ws2 = WorkspaceIdentity::from_canonical_root_bytes(b"/work/two");
    let mut ledger = crate::policy::UsageLedger::default();

    ledger.record_use(ws1, "s1", "rule_a");
    ledger.record_use(ws1, "s1", "rule_a");
    ledger.record_use(ws1, "s2", "rule_a");
    ledger.record_use(ws2, "s1", "rule_a");

    assert_eq!(ledger.used(ws1, "s1", "rule_a"), 2);
    assert_eq!(ledger.used(ws1, "s2", "rule_a"), 1);
    assert_eq!(ledger.used(ws2, "s1", "rule_a"), 1, "other workspace is isolated");
    assert_eq!(ledger.used(ws2, "s1", "rule_b"), 0);

    let snapshot = ledger.snapshot(ws1, "s1");
    assert_eq!(snapshot.used("rule_a"), 2);
    assert_eq!(ledger.snapshot(ws2, "s1").used("rule_a"), 1);
    assert_eq!(ledger.snapshot(ws1, "s3").used("rule_a"), 0);

    // Failed, canceled, and denied requests never record; only a successful
    // dispatch calls record_use.  Session close drops every row.
    assert_eq!(ledger.used(ws1, "s1", "never_dispatched"), 0);
    ledger.invalidate_session("s1");
    assert_eq!(ledger.used(ws1, "s1", "rule_a"), 0, "rows die with the session");
    assert_eq!(ledger.used(ws2, "s1", "rule_a"), 0);
    assert_eq!(ledger.used(ws1, "s2", "rule_a"), 1, "other sessions survive");
}

// ── Store scope validation with injected time ────────────────────────────────

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

#[cfg(not(unix))]
fn set_owner_only_dir(_path: &Path) {}

fn document_with(store: &TrustStore, entries: &str) -> String {
    format!(
        r#"
schema_version = 1

[workspace]
identity = "{identity}"

[policy]
workspace_enabled = false
{entries}
"#,
        identity = store.workspace().as_string()
    )
}

#[test]
fn reload_preserves_valid_expiry_metadata() {
    let (_base, _workspace_dir, store) = store_setup();
    let ws = *store.workspace();
    let now = virtual_now();
    let rule = command_rule("cmd_1", ws, Some(now + Duration::from_secs(3600)), Some(20));
    store
        .write(&crate::policy::TrustStoreDocument {
            workspace: ws,
            workspace_enabled: false,
            tool_defaults: Vec::new(),
            category_defaults: Vec::new(),
            global_default: crate::policy::FallbackEffect::Confirm,
            rules: vec![rule],
        })
        .expect("write");

    let reloaded = store.load_at(now).expect("load at injected time");
    assert_eq!(reloaded.rules.len(), 1);
    assert_eq!(
        reloaded.rules[0].scope().expires_at,
        Some(now + Duration::from_secs(3600)),
        "valid expiry survives reload"
    );
    assert_eq!(reloaded.rules[0].scope().max_uses, Some(20));
    assert_eq!(reloaded.rules[0].id(), "cmd_1", "stable id retained");
}

#[test]
fn invalid_persisted_scope_never_loads() {
    let (_base, _workspace_dir, store) = store_setup();
    let now = virtual_now();
    let fmt = |time: SystemTime| {
        let datetime: chrono::DateTime<chrono::Utc> = time.into();
        datetime.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    };
    let past = fmt(now - Duration::from_secs(86_400));
    let overlong = fmt(now + MAX_RULE_DURATION * 2);
    let within = fmt(now + Duration::from_secs(3600));
    let entries = format!(
        r#"
[[command_allow]]
id = "past_expiry"
executable = "git"
match = "argv_exact"
argv = ["status"]
expires_at = "{past}"
max_uses = 20

[[command_allow]]
id = "overlong"
executable = "git"
match = "argv_exact"
argv = ["status"]
expires_at = "{overlong}"
max_uses = 20

[[command_allow]]
id = "oversized_budget"
executable = "git"
match = "argv_exact"
argv = ["status"]
expires_at = "{within}"
max_uses = 50000


[[command_allow]]
id = "valid"
executable = "git"
match = "argv_exact"
argv = ["diff"]
expires_at = "{within}"
max_uses = 20
"#
    );
    write_store_text(store.path(), &document_with(&store, &entries));
    let document = store.load_at(now).expect("load at injected time");
    let ids: Vec<&str> = document.rules.iter().map(TrustRule::id).collect();
    assert_eq!(ids, vec!["valid"], "past expiry, overlong window, and oversized budget never load");
    // The invalid entries remain stored on disk; they just never become
    // effective authority.
    let text = fs::read_to_string(store.path()).unwrap();
    assert!(text.contains("past_expiry"), "expired rule stays stored");
}

#[test]
fn load_at_uses_injected_time_deterministically() {
    let (_base, _workspace_dir, store) = store_setup();
    let ws = *store.workspace();
    let now = virtual_now();
    let rule = command_rule("cmd_1", ws, Some(now + Duration::from_secs(3600)), Some(20));
    store
        .write(&crate::policy::TrustStoreDocument {
            workspace: ws,
            workspace_enabled: false,
            tool_defaults: Vec::new(),
            category_defaults: Vec::new(),
            global_default: crate::policy::FallbackEffect::Confirm,
            rules: vec![rule],
        })
        .expect("write");

    let before = store.load_at(now).expect("valid at now");
    assert_eq!(before.rules.len(), 1);
    let after = store.load_at(now + Duration::from_secs(7200)).expect("expired later");
    assert!(after.rules.is_empty(), "the same document expires purely by injected time");
}

#[test]
fn fake_clock_advances_only_on_demand() {
    let start = virtual_now();
    let clock = PolicyClock::fake_at(start);
    assert_eq!(clock.now(), start);
    clock.advance(Duration::from_secs(3600));
    assert_eq!(clock.now(), start + Duration::from_secs(3600));
    // The system clock is unaffected by advance (no-op).
    let system = PolicyClock::System;
    system.advance(Duration::from_secs(3600));
    let _ = system.now();
}

// ── End-to-end lifecycle and audit (agents feature) ──────────────────────────

#[cfg(feature = "agents")]
mod e2e {
    use super::*;
    use crate::app::ActionLogEntry;
    use crate::policy::store::TrustStoreDocument;
    use crate::tests::agent_mcp::{
        base_agent_script, connect_proxy, mcp_app, open_pane_and_wait_ready, press, proxy_recv,
        proxy_send, wait_until,
    };
    use crate::tests::helpers::run_ex;
    use crossterm::event::{KeyCode, KeyModifiers};
    use serde_json::json;

    fn terminal_frame(command: &str, args: serde_json::Value) -> serde_json::Value {
        json!({ "method": "terminal_create", "command": command, "args": args })
    }

    fn seed_command_rule(
        state_dir: &Path,
        workspace: &Path,
        id: &str,
        expires_at: SystemTime,
        max_uses: u64,
    ) {
        let store = TrustStore::at(state_dir, workspace).unwrap();
        let rule = TrustRule::Command(CommandRule {
            id: id.to_string(),
            effect: crate::policy::TrustEffect::Allow,
            scope: TrustRuleScope {
                workspace: *store.workspace(),
                agent: None,
                expires_at: Some(expires_at),
                max_uses: Some(max_uses),
            },
            executable: "git".to_string(),
            match_mode: MatchMode::ArgvExact,
            argv: vec!["status".to_string()],
        });
        store
            .write(&TrustStoreDocument {
                workspace: *store.workspace(),
                workspace_enabled: true,
                tool_defaults: Vec::new(),
                category_defaults: Vec::new(),
                global_default: crate::policy::FallbackEffect::Confirm,
                rules: vec![rule],
            })
            .expect("seed store");
    }

    #[test]
    fn virtual_clock_expires_rules_without_real_time_or_sleeps() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        // Deterministic fake clock: the grant expires purely by injected
        // time; the wall clock is never consulted.
        app.trust_clock = PolicyClock::fake_at(virtual_now());
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        seed_command_rule(
            &state_dir,
            temp.path(),
            "cmd_lifecycle",
            app.trust_clock.now() + Duration::from_secs(3600),
            20,
        );

        let mut stream = connect_proxy(&app);
        proxy_send(&mut stream, 1, terminal_frame("git", json!(["status"])));
        wait_until(&mut app, "granted terminal spawned", |app| {
            app.agents.terminals.tracked_count() == 1 && app.agents.approvals.is_empty()
        });
        let reply = proxy_recv(&mut stream);
        assert!(reply["result"]["value"].is_string(), "auto-allowed: {reply}");

        // Two hours of virtual time later the identical request prompts.
        app.trust_clock.advance(Duration::from_secs(7200));
        proxy_send(&mut stream, 2, terminal_frame("git", json!(["status"])));
        wait_until(&mut app, "expired rule prompts", |app| !app.agents.approvals.is_empty());
        assert_eq!(app.agents.terminals.tracked_count(), 1, "no second terminal");
        run_ex(&mut app, "agents");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE); // Deny
        let reply = proxy_recv(&mut stream);
        assert!(reply["result"]["error"].is_object(), "denied: {reply}");
    }

    #[test]
    fn auto_allow_emits_redacted_audit_and_status_metadata() {
        // Bridge path (native session with a thread): the durable status
        // surface is the thread transcript, which async save alerts cannot
        // clobber.
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("src/generated")).unwrap();
        let target = temp.path().join("src/generated/audit.rs");
        fs::write(&target, "v0").unwrap();
        let secret_content = "API_TOKEN=super-secret-value";
        let script = crate::tests::agent_bridge::base_script()
            .wait_for("session/prompt")
            .emit(crate::tests::agent_bridge::write_text_file(
                103,
                "s1",
                &target.display().to_string(),
                secret_content,
            ))
            .respond(json!({ "stopReason": "end_turn" }));
        let (mut app, fake) = crate::tests::agent_bridge::agents_app_in(&temp, script);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        let store = TrustStore::at(&state_dir, temp.path()).unwrap();
        let rule = TrustRule::Write(crate::policy::WriteRule {
            id: "write_audit".to_string(),
            effect: crate::policy::TrustEffect::Allow,
            scope: TrustRuleScope {
                workspace: *store.workspace(),
                agent: None,
                expires_at: Some(SystemTime::now() + Duration::from_secs(3600)),
                max_uses: Some(5),
            },
            operation: crate::policy::WriteOperationKind::Modify,
            path_prefix: crate::policy::PathPrefix::parse("src/generated").expect("prefix"),
            max_files: 1,
            max_total_bytes: 65_536,
            max_file_bytes: 16_384,
        });
        store
            .write(&TrustStoreDocument {
                workspace: *store.workspace(),
                workspace_enabled: true,
                tool_defaults: Vec::new(),
                category_defaults: Vec::new(),
                global_default: crate::policy::FallbackEffect::Confirm,
                rules: vec![rule],
            })
            .expect("seed store");
        open_pane_and_wait_ready(&mut app);
        for ch in "perform audited write".chars() {
            press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

        wait_until(&mut app, "audited write and trust notice recorded", |app| {
            fake.agent().response_with_id(103).is_some()
                && app.agents.threads.iter().any(|thread| {
                    thread
                        .system_notices()
                        .iter()
                        .any(|notice| notice.contains("trusted by write_audit"))
                })
        });
        let response = fake.agent().response_with_id(103).expect("write answered");
        if response.get("result").is_none() {
            panic!("write did not auto-allow: {response}");
        }
        assert_eq!(
            fs::read_to_string(&target).unwrap().trim_end(),
            secret_content,
            "trusted write landed on disk"
        );

        // The status surface (thread transcript) shows redacted rule +
        // remaining-use metadata; remaining is post-dispatch (one use gone).
        let thread = app
            .agents
            .threads
            .iter()
            .find(|thread| thread.session_id == "s1")
            .expect("session thread");
        let notices = thread.system_notices();
        let trust_notice = notices
            .iter()
            .find(|notice| notice.contains("trusted by write_audit"))
            .expect("trust notice in transcript");
        assert!(trust_notice.contains("4 uses left"), "notice: {trust_notice}");
        assert!(trust_notice.contains("expires "), "notice shows expiry: {trust_notice}");
        assert!(!trust_notice.contains("API_TOKEN"), "secret value leaked: {trust_notice}");
        assert!(
            !trust_notice.contains("super-secret-value"),
            "secret value leaked: {trust_notice}"
        );
        assert!(!trust_notice.contains("audit.rs"), "raw path leaked: {trust_notice}");

        // The audit entry carries rule id, category, reason, and remaining
        // use only; the secret content and raw path never appear.
        let log = app.agents_action_log();
        let trust_entries: Vec<&ActionLogEntry> = log
            .iter()
            .filter(|entry| matches!(entry, ActionLogEntry::TrustDecision { .. }))
            .collect();
        assert!(!trust_entries.is_empty(), "audit entries recorded");
        let rendered = format!("{trust_entries:?}");
        assert!(!rendered.contains("API_TOKEN"), "secret value leaked: {rendered}");
        assert!(!rendered.contains("super-secret-value"), "secret value leaked: {rendered}");
        assert!(!rendered.contains("src/generated"), "raw path leaked: {rendered}");
        assert!(rendered.contains("write_audit"), "rule id present: {rendered}");
        assert!(rendered.contains("PersistentAllow"), "reason present: {rendered}");
        let entry = trust_entries.last().unwrap();
        match entry {
            ActionLogEntry::TrustDecision {
                rule_id,
                category,
                reason,
                remaining_uses,
                session_id,
            } => {
                assert_eq!(rule_id.as_deref(), Some("write_audit"));
                assert_eq!(*category, TrustCategory::WriteModify);
                assert_eq!(*reason, DecisionReason::PersistentAllow);
                assert_eq!(*remaining_uses, Some(5), "decision-time remaining before dispatch");
                assert_eq!(session_id, "s1");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn prompt_fallback_records_redacted_audit_and_deny_consumes_nothing() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());

        let mut stream = connect_proxy(&app);
        proxy_send(&mut stream, 1, terminal_frame("git", json!(["commit", "-m", "x"])));
        wait_until(&mut app, "uncovered command prompts", |app| !app.agents.approvals.is_empty());
        run_ex(&mut app, "agents");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE); // Deny
        let reply = proxy_recv(&mut stream);
        assert!(reply["result"]["error"].is_object(), "denied: {reply}");

        assert!(app.agents.usage_ledger.is_empty(), "denied request must not consume any use");
        let log = app.agents_action_log();
        let entry = log
            .iter()
            .rev()
            .find_map(|entry| match entry {
                ActionLogEntry::TrustDecision {
                    rule_id,
                    category,
                    reason,
                    remaining_uses,
                    session_id,
                } => Some((rule_id, category, reason, remaining_uses, session_id)),
                _ => None,
            })
            .expect("prompt fallback audit recorded");
        assert_eq!(entry.0, &None, "no rule matched");
        assert_eq!(*entry.1, TrustCategory::Execute);
        assert_eq!(*entry.2, DecisionReason::GlobalDefaultConfirm);
        assert_eq!(entry.3, &None, "no remaining use without a matched rule");
        assert_eq!(entry.4, "proxy");
        let rendered = format!("{log:?}");
        assert!(!rendered.contains("-m"), "command argv leaked into audit: {rendered}");
    }

    #[test]
    fn disconnect_clears_workspace_scoped_usage_rows() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        seed_command_rule(
            &state_dir,
            temp.path(),
            "cmd_lifecycle",
            SystemTime::now() + Duration::from_secs(3600),
            20,
        );

        let mut stream = connect_proxy(&app);
        proxy_send(&mut stream, 1, terminal_frame("git", json!(["status"])));
        wait_until(&mut app, "granted terminal spawned", |app| {
            app.agents.terminals.tracked_count() == 1
        });
        assert!(!app.agents.usage_ledger.is_empty(), "use recorded before teardown");
        let _ = proxy_recv(&mut stream);

        app.shutdown_agents();
        assert!(app.agents.usage_ledger.is_empty(), "usage rows die with the session close");
    }
}
