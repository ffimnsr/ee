//! Phase 2 command trust tests (ISSUES.md "Unified Host-Local Workspace
//! Trust Policy"): exact structured terminal command trust.
//!
//! Covers shell wrapper / token / cwd eligibility, the pure command
//! matcher, the session-local usage ledger, host-local persistence, and the
//! terminal approval integration.  End-to-end tests run under the `agents`
//! feature and drive terminal requests through the MCP proxy.

use std::fs;
use std::path::Path;
#[cfg(feature = "agents")]
use std::time::Duration;
use std::time::SystemTime;

use tempfile::TempDir;

use crate::policy::rules::{MatchMode, TrustRule};
use crate::policy::session::SessionPolicy;
use crate::policy::store::TrustStore;
use crate::policy::{
    DecisionReason, OperationIdentity, SHELL_WRAPPERS, TrustCategory, TrustOperation, TrustOutcome,
    TrustRuleScope, UsageLedger, UsageSnapshot, WorkspaceIdentity, is_shell_wrapper,
    resolve_command_cwd, validate_command_tokens, validate_executable,
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

fn scope(workspace: WorkspaceIdentity) -> TrustRuleScope {
    TrustRuleScope {
        workspace,
        agent: None,
        expires_at: Some(at("2026-08-08T12:00:00Z")),
        max_uses: Some(20),
    }
}

fn command_rule(
    id: &str,
    workspace: WorkspaceIdentity,
    executable: &str,
    match_mode: MatchMode,
    argv: &[&str],
) -> TrustRule {
    TrustRule::Command(crate::policy::CommandRule {
        id: id.to_string(),
        scope: scope(workspace),
        executable: executable.to_string(),
        match_mode,
        argv: argv.iter().map(|token| token.to_string()).collect(),
    })
}

fn command_op(workspace: WorkspaceIdentity, executable: &str, argv: &[&str]) -> TrustOperation {
    TrustOperation {
        workspace,
        agent: None,
        transport: crate::policy::TransportKind::Acp,
        category: TrustCategory::Execute,
        identity: OperationIdentity::Command {
            executable: executable.to_string(),
            argv: argv.iter().map(|token| token.to_string()).collect(),
        },
    }
}

fn decide(
    op: &TrustOperation,
    rules: &[TrustRule],
    usage: &UsageSnapshot,
) -> crate::policy::TrustDecision {
    crate::policy::evaluate(&crate::policy::PolicyInput {
        session_id: "s1",
        fingerprint: "fp",
        operation: op,
        session: &SessionPolicy::default(),
        rules,
        now: at("2026-08-07T12:00:00Z"),
        usage,
        workspace_enabled: true,
    })
}

// ── Eligibility: shell wrappers, tokens, cwd ─────────────────────────────────

#[test]
fn shell_wrappers_are_ineligible_for_command_trust() {
    for wrapper in SHELL_WRAPPERS {
        assert!(is_shell_wrapper(wrapper), "{wrapper} must be a shell wrapper");
        assert!(validate_executable(wrapper).is_err(), "{wrapper} must be rejected");
    }
    // Basename matching catches path-qualified wrappers too.
    assert!(is_shell_wrapper("/bin/sh"));
    assert!(validate_executable("/usr/bin/bash").is_err());
    // Ordinary executables remain eligible.
    for executable in ["git", "git-status", "cargo", "ls"] {
        assert!(!is_shell_wrapper(executable));
        assert!(validate_executable(executable).is_ok(), "{executable} must be accepted");
    }
}

#[test]
fn control_characters_and_empty_tokens_are_rejected() {
    let tokens = |values: &[&str]| values.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(validate_executable("").is_err(), "empty executable rejected");
    assert!(validate_executable("gi\tt").is_err(), "control executable rejected");
    assert!(validate_executable("gi\u{0}t").is_err(), "NUL executable rejected");
    assert!(validate_command_tokens("git", &tokens(&["status"])).is_ok());
    assert!(validate_command_tokens("git", &tokens(&[""])).is_err(), "empty token rejected");
    assert!(
        validate_command_tokens("git", &tokens(&["sta\tus"])).is_err(),
        "control token rejected"
    );
    assert!(
        validate_command_tokens("git", &tokens(&["sta\u{0}us"])).is_err(),
        "NUL token rejected"
    );
}

#[test]
fn cwd_resolution_rejects_relative_external_and_traversal_cwds() {
    let root = TempDir::new().unwrap();
    let subdir = root.path().join("src");
    fs::create_dir_all(&subdir).unwrap();
    let roots = vec![fs::canonicalize(root.path()).unwrap()];
    let outside = TempDir::new().unwrap();

    assert_eq!(
        resolve_command_cwd(&subdir, &roots).unwrap(),
        fs::canonicalize(&subdir).unwrap(),
        "in-workspace subdirectory resolves"
    );
    assert_eq!(
        resolve_command_cwd(root.path(), &roots).unwrap(),
        roots[0],
        "workspace root resolves"
    );
    assert!(resolve_command_cwd(Path::new("src"), &roots).is_err(), "relative cwd rejected");
    assert!(resolve_command_cwd(outside.path(), &roots).is_err(), "external cwd rejected");
    assert!(
        resolve_command_cwd(&subdir.join("..").join(".."), &roots).is_err(),
        "traversal escape rejected"
    );
    assert!(resolve_command_cwd(&subdir.join("missing"), &roots).is_err(), "missing cwd rejected");
    let file = root.path().join("file.txt");
    fs::write(&file, "x").unwrap();
    assert!(resolve_command_cwd(&file, &roots).is_err(), "file as cwd rejected");
}

#[cfg(unix)]
#[test]
fn cwd_resolution_rejects_symlink_escape() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let roots = vec![fs::canonicalize(root.path()).unwrap()];

    let escape = root.path().join("escape");
    std::os::unix::fs::symlink(outside.path(), &escape).unwrap();
    assert!(
        resolve_command_cwd(&escape, &roots).is_err(),
        "symlink escaping the workspace rejected"
    );

    let inside = root.path().join("real");
    fs::create_dir_all(&inside).unwrap();
    let link = root.path().join("alias");
    std::os::unix::fs::symlink(&inside, &link).unwrap();
    assert!(
        resolve_command_cwd(&link, &roots).is_ok(),
        "symlink staying inside the workspace resolves"
    );
}

// ── Matcher and rule validation ──────────────────────────────────────────────

#[test]
fn git_status_rules_match_only_intended_structured_argv() {
    let ws = identity(b"/work/root");
    let exact = command_rule("cmd_1", ws, "git", MatchMode::ArgvExact, &["status"]);
    let prefix = command_rule("cmd_2", ws, "git", MatchMode::ArgvPrefix, &["status"]);

    // Exact: only the exact structured argv matches.
    let status = command_op(ws, "git", &["status"]);
    assert_eq!(
        decide(&status, std::slice::from_ref(&exact), &UsageSnapshot::default()).outcome,
        TrustOutcome::Allow,
        "exact rule matches git status"
    );
    // Prefix: matching prefix plus extra flags.
    for argv in [&["status"][..], &["status", "--short"][..]] {
        let op = command_op(ws, "git", argv);
        assert_eq!(
            decide(&op, std::slice::from_ref(&prefix), &UsageSnapshot::default()).outcome,
            TrustOutcome::Allow,
            "prefix rule matches {argv:?}"
        );
    }
    let short = command_op(ws, "git", &["status", "--short"]);
    assert_eq!(
        decide(&short, std::slice::from_ref(&exact), &UsageSnapshot::default()).outcome,
        TrustOutcome::Prompt,
        "exact rule rejects extra flags"
    );
    for argv in [
        &["commit", "-m", "x"][..],
        &["reset", "--hard"][..],
        &["clean", "-fd"][..],
        &["diff"][..],
        &["stash"][..],
    ] {
        let op = command_op(ws, "git", argv);
        let decision = decide(&op, std::slice::from_ref(&exact), &UsageSnapshot::default());
        assert_eq!(decision.outcome, TrustOutcome::Prompt, "{argv:?} must not match exact");
        let decision = decide(&op, std::slice::from_ref(&prefix), &UsageSnapshot::default());
        assert_eq!(decision.outcome, TrustOutcome::Prompt, "{argv:?} must not match prefix");
    }
    // The executable token is part of the identity.
    let other = command_op(ws, "hub", &["status"]);
    assert_eq!(decide(&other, &[exact], &UsageSnapshot::default()).outcome, TrustOutcome::Prompt);
}

#[test]
fn argv_exact_empty_rule_matches_only_a_no_argument_request() {
    let ws = identity(b"/work/root");
    let rule = command_rule("cmd_1", ws, "true", MatchMode::ArgvExact, &[]);
    let no_args = command_op(ws, "true", &[]);
    assert_eq!(
        decide(&no_args, std::slice::from_ref(&rule), &UsageSnapshot::default()).outcome,
        TrustOutcome::Allow
    );
    let with_args = command_op(ws, "true", &["--flag"]);
    assert_eq!(
        decide(&with_args, &[rule], &UsageSnapshot::default()).outcome,
        TrustOutcome::Prompt
    );
}

#[test]
fn command_only_trust_is_prohibited_at_load() {
    let (_base, _workspace_dir, store) = store_setup();
    let text = format!(
        r#"
schema_version = 1

[workspace]
identity = "{identity}"

[policy]
workspace_enabled = false

[[command_allow]]
id = "cmd_bare"
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
        identity = store.workspace().as_string()
    );
    write_store_text(store.path(), &text);
    let document = store.load().expect("load");
    let ids: Vec<&str> = document.rules.iter().map(TrustRule::id).collect();
    assert_eq!(ids, vec!["cmd_ok"], "command-only git rule rejected, bounded rule loads");
}

#[test]
fn shell_wrapper_rules_are_rejected_at_load() {
    let (_base, _workspace_dir, store) = store_setup();
    let text = format!(
        r#"
schema_version = 1

[workspace]
identity = "{identity}"

[policy]
workspace_enabled = false

[[command_allow]]
id = "cmd_sh"
executable = "sh"
match = "argv_exact"
argv = ["-c", "echo hi"]
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20

[[command_allow]]
id = "cmd_pwsh"
executable = "powershell"
match = "argv_exact"
argv = []
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20

[[command_allow]]
id = "cmd_git"
executable = "git"
match = "argv_exact"
argv = ["status"]
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20
"#,
        identity = store.workspace().as_string()
    );
    write_store_text(store.path(), &text);
    let document = store.load().expect("load");
    let ids: Vec<&str> = document.rules.iter().map(TrustRule::id).collect();
    assert_eq!(ids, vec!["cmd_git"], "shell wrapper rules rejected at load");
}

// ── Usage ledger ─────────────────────────────────────────────────────────────

#[test]
fn usage_ledger_snapshots_records_and_invalidates_per_session() {
    let ws = identity(b"/work/root");
    let mut ledger = UsageLedger::default();
    ledger.record_use(ws, "s1", "cmd_1");
    ledger.record_use(ws, "s1", "cmd_1");
    ledger.record_use(ws, "s1", "cmd_2");
    ledger.record_use(ws, "s2", "cmd_1");

    let s1 = ledger.snapshot(ws, "s1");
    assert_eq!(s1.used("cmd_1"), 2);
    assert_eq!(s1.used("cmd_2"), 1);
    assert_eq!(s1.used("cmd_3"), 0);
    let s2 = ledger.snapshot(ws, "s2");
    assert_eq!(s2.used("cmd_1"), 1);
    assert_eq!(s2.used("cmd_2"), 0, "rows are session-scoped");

    ledger.invalidate_session("s1");
    assert!(
        ledger.snapshot(ws, "s1").used("cmd_1") == 0
            && ledger.snapshot(ws, "s1").used("cmd_2") == 0
    );
    assert_eq!(ledger.snapshot(ws, "s2").used("cmd_1"), 1, "other session survives");
    ledger.invalidate_session("s2");
    assert!(ledger.is_empty(), "ledger dies with the sessions");
}

#[test]
fn exhausted_persistent_grant_prompts_without_mutating_usage() {
    let ws = identity(b"/work/root");
    let rule = command_rule("cmd_1", ws, "git", MatchMode::ArgvExact, &["status"]);
    let rule = match rule {
        TrustRule::Command(mut inner) => {
            inner.scope.max_uses = Some(2);
            TrustRule::Command(inner)
        }
        _ => unreachable!(),
    };
    let op = command_op(ws, "git", &["status"]);

    let usage = UsageSnapshot::new(std::collections::BTreeMap::from([("cmd_1".to_string(), 1)]));
    assert_eq!(decide(&op, std::slice::from_ref(&rule), &usage).outcome, TrustOutcome::Allow);
    let exhausted =
        UsageSnapshot::new(std::collections::BTreeMap::from([("cmd_1".to_string(), 2)]));
    let decision = decide(&op, &[rule], &exhausted);
    assert_eq!(decision.outcome, TrustOutcome::Prompt);
    assert_eq!(decision.reason, DecisionReason::NoMatchingRule);
    assert_eq!(exhausted.used("cmd_1"), 2, "usage snapshot unchanged");
}

// ── Store helpers shared with the e2e section ────────────────────────────────

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

// ── End-to-end: terminal approval integration (agents feature) ───────────────

#[cfg(feature = "agents")]
mod e2e {
    use super::*;
    use crate::app::App;
    use crate::app::{ApprovalChoice, PERSISTENT_TERMINAL_MAX_USES};
    use crate::tests::agent_mcp::{
        base_agent_script, connect_proxy, mcp_app, open_pane_and_wait_ready, press, proxy_recv,
        proxy_send, wait_until,
    };
    use crate::tests::helpers::run_ex;
    use crossterm::event::{KeyCode, KeyModifiers};
    use serde_json::{Value, json};

    /// Seeds one exact command rule into the host-local store for the app
    /// workspace and returns its stable id.
    fn seed_rule(
        state_dir: &Path,
        workspace: &Path,
        executable: &str,
        argv: &[&str],
        max_uses: u64,
    ) -> String {
        let store = TrustStore::at(state_dir, workspace).unwrap();
        let ws = *store.workspace();
        let id = format!("cmd_seed_{executable}_{}", argv.join("_"));
        let rule = TrustRule::Command(crate::policy::CommandRule {
            id: id.clone(),
            scope: TrustRuleScope {
                workspace: ws,
                agent: None,
                expires_at: Some(SystemTime::now() + Duration::from_secs(3600)),
                max_uses: Some(max_uses),
            },
            executable: executable.to_string(),
            match_mode: MatchMode::ArgvExact,
            argv: argv.iter().map(|token| token.to_string()).collect(),
        });
        store.add_rule(rule).expect("seed rule");
        id
    }

    fn terminal_frame(command: &str, args: Value) -> Value {
        json!({ "method": "terminal_create", "command": command, "args": args })
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
    fn persisted_rule_auto_allows_exact_terminal_and_prompts_for_others() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        seed_rule(&state_dir, temp.path(), "git", &["status"], 20);

        let mut stream = connect_proxy(&app);
        // Exact match: auto-allowed without any approval prompt.
        proxy_send(&mut stream, 1, terminal_frame("git", json!(["status"])));
        wait_until(&mut app, "trusted terminal spawned", |app| {
            app.agents.terminals.tracked_count() == 1 && app.agents.approvals.is_empty()
        });
        let reply = proxy_recv(&mut stream);
        let terminal_id = reply["result"]["value"].as_str().expect("terminal id").to_string();
        assert!(terminal_id.starts_with("term-"), "reply: {reply}");

        // Different argv: prompts (never auto-allowed).
        proxy_send(&mut stream, 2, terminal_frame("git", json!(["diff"])));
        wait_until(&mut app, "diff approval queued", |app| !app.agents.approvals.is_empty());
        assert_eq!(app.agents.terminals.tracked_count(), 1, "no terminal spawned");
        run_ex(&mut app, "agents");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE); // Deny
        let reply = proxy_recv(&mut stream);
        assert!(reply["result"]["error"].is_object(), "diff must be denied: {reply}");
        assert_eq!(app.agents.terminals.tracked_count(), 1);
    }

    #[test]
    fn persistent_option_creates_and_activates_host_local_rule() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());

        let mut stream = connect_proxy(&app);
        proxy_send(&mut stream, 1, terminal_frame("git", json!(["status"])));
        wait_until(&mut app, "approval queued", |app| !app.agents.approvals.is_empty());
        {
            let prompt = app.agents.approvals.front().unwrap();
            assert_eq!(prompt.options.len(), 5, "persistent option offered");
            assert_eq!(prompt.options[4].0, "Allow for 1 hour / 20 uses");
            assert_eq!(prompt.options[4].1, ApprovalChoice::AllowPersistent);
        }
        open_pane_and_select(&mut app, 4); // Allow for 1 hour / 20 uses

        wait_until(&mut app, "rule persisted and terminal spawned", |app| {
            app.agents.terminals.tracked_count() == 1
                && TrustStore::at(&state_dir, temp.path())
                    .unwrap()
                    .load()
                    .map(|doc| !doc.rules.is_empty())
                    .unwrap_or(false)
        });
        let store = TrustStore::at(&state_dir, temp.path()).unwrap();
        let document = store.load().unwrap();
        assert_eq!(document.rules.len(), 1);
        let TrustRule::Command(rule) = &document.rules[0] else {
            panic!("expected command rule");
        };
        assert!(rule.id.starts_with("cmd_"));
        assert_eq!(rule.executable, "git");
        assert_eq!(rule.argv, vec!["status".to_string()]);
        assert_eq!(rule.match_mode, MatchMode::ArgvExact);
        assert_eq!(rule.scope.max_uses, Some(PERSISTENT_TERMINAL_MAX_USES));
        assert_eq!(rule.scope.agent, None, "proxy session is not agent-scoped");
        let expiry = rule.scope.expires_at.expect("expiry");
        let now = app.trust_clock.now();
        assert!(expiry > now + Duration::from_secs(59 * 60), "expiry ~1h ahead");
        assert!(expiry < now + Duration::from_secs(61 * 60), "expiry ~1h ahead");
        let used = app.agents.usage_ledger.used(
            ledger_workspace(&state_dir, temp.path()),
            "proxy",
            &rule.id,
        );
        assert_eq!(used, 1, "the creating dispatch consumes one use");
        let _ = proxy_recv(&mut stream);

        // The persisted rule activates immediately: identical request
        // auto-allows with no prompt and consumes the second use.
        proxy_send(&mut stream, 2, terminal_frame("git", json!(["status"])));
        wait_until(&mut app, "second trusted terminal spawned", |app| {
            app.agents.terminals.tracked_count() == 2 && app.agents.approvals.is_empty()
        });
        assert_eq!(
            app.agents.usage_ledger.used(
                ledger_workspace(&state_dir, temp.path()),
                "proxy",
                &rule.id
            ),
            2
        );
        let _ = proxy_recv(&mut stream);
    }

    #[test]
    fn shell_wrapper_and_external_cwd_requests_never_offer_persistent() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        app.agents.test_trust_store_base = Some(temp.path().join("state"));

        let mut stream = connect_proxy(&app);
        proxy_send(&mut stream, 1, terminal_frame("sh", json!(["-c", "echo hi"])));
        wait_until(&mut app, "shell approval queued", |app| !app.agents.approvals.is_empty());
        {
            let prompt = app.agents.approvals.front().unwrap();
            assert_eq!(prompt.options.len(), 4, "shell wrapper never offers persistent");
            assert!(prompt.options.iter().all(|(label, _)| !label.contains("1 hour")));
        }
        run_ex(&mut app, "agents");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE); // Deny
        let _ = proxy_recv(&mut stream);

        // External cwd: prompt-only, no persistent option.
        let outside = TempDir::new().unwrap();
        proxy_send(
            &mut stream,
            2,
            json!({
                "method": "terminal_create",
                "command": "git",
                "args": ["status"],
                "cwd": outside.path().display().to_string(),
            }),
        );
        wait_until(&mut app, "external-cwd approval queued", |app| {
            !app.agents.approvals.is_empty()
        });
        {
            let prompt = app.agents.approvals.front().unwrap();
            assert_eq!(prompt.options.len(), 4, "external cwd never offers persistent");
        }
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE); // Deny
        let _ = proxy_recv(&mut stream);
        assert_eq!(app.agents.terminals.tracked_count(), 0);
    }

    #[test]
    fn persistence_failure_denies_without_spawn_and_keeps_budget() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        // Create the store (and its 0700 trust directory) first.
        seed_rule(&state_dir, temp.path(), "git", &["diff"], 20);

        let mut stream = connect_proxy(&app);
        proxy_send(&mut stream, 1, terminal_frame("git", json!(["status"])));
        wait_until(&mut app, "approval queued", |app| !app.agents.approvals.is_empty());
        assert_eq!(
            app.agents.usage_ledger.used(
                ledger_workspace(&state_dir, temp.path()),
                "proxy",
                "cmd_seed_git_diff"
            ),
            0
        );

        // Make the trust directory read-only so the new grant cannot persist.
        let trust_dir =
            TrustStore::at(&state_dir, temp.path()).unwrap().path().parent().unwrap().to_path_buf();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&trust_dir, fs::Permissions::from_mode(0o500)).unwrap();
        }
        open_pane_and_select(&mut app, 4); // Allow for 1 hour / 20 uses
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
        assert_eq!(app.agents.terminals.tracked_count(), 0, "terminal must stay unspawned");
        assert!(
            app.agents.usage_ledger.is_empty(),
            "usage budget unchanged after persistence failure"
        );
        let store = TrustStore::at(&state_dir, temp.path()).unwrap();
        assert_eq!(store.load().unwrap().rules.len(), 1, "only the seeded rule persists");
    }

    #[test]
    fn exhausted_rule_prompts_again_after_budget_is_consumed() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        let rule_id = seed_rule(&state_dir, temp.path(), "git", &["status"], 1);

        let mut stream = connect_proxy(&app);
        proxy_send(&mut stream, 1, terminal_frame("git", json!(["status"])));
        wait_until(&mut app, "first use spawned", |app| {
            app.agents.terminals.tracked_count() == 1 && app.agents.approvals.is_empty()
        });
        assert_eq!(
            app.agents.usage_ledger.used(
                ledger_workspace(&state_dir, temp.path()),
                "proxy",
                &rule_id
            ),
            1
        );
        let _ = proxy_recv(&mut stream);

        // Budget exhausted: the identical request prompts again.
        proxy_send(&mut stream, 2, terminal_frame("git", json!(["status"])));
        wait_until(&mut app, "re-prompt after exhaustion", |app| !app.agents.approvals.is_empty());
        assert_eq!(app.agents.terminals.tracked_count(), 1, "no second terminal");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE); // Deny
        let reply = proxy_recv(&mut stream);
        assert!(reply["result"]["error"].is_object(), "denied: {reply}");
    }

    #[test]
    fn teardown_clears_usage_ledger_and_session_state() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        seed_rule(&state_dir, temp.path(), "git", &["status"], 20);

        let mut stream = connect_proxy(&app);
        proxy_send(&mut stream, 1, terminal_frame("git", json!(["status"])));
        wait_until(&mut app, "trusted terminal spawned", |app| {
            app.agents.terminals.tracked_count() == 1
        });
        assert!(!app.agents.usage_ledger.is_empty());
        let _ = proxy_recv(&mut stream);

        app.shutdown_agents();
        assert!(app.agents.usage_ledger.is_empty(), "usage rows die with the sessions");
        assert!(app.agents.approval_policy.is_empty());
    }

    #[test]
    fn trusted_terminal_keeps_ownership_and_pipeline_behavior() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        seed_rule(&state_dir, temp.path(), "git", &["status"], 20);

        let mut stream = connect_proxy(&app);
        proxy_send(&mut stream, 1, terminal_frame("git", json!(["status"])));
        wait_until(&mut app, "trusted terminal spawned", |app| {
            app.agents.terminals.tracked_count() == 1
        });
        let reply = proxy_recv(&mut stream);
        let terminal_id = reply["result"]["value"].as_str().expect("terminal id").to_string();

        // The trusted path registers through the same session-owned pipeline:
        // the proxy session can query, other sessions cannot.
        let output = crate::app::AgentTerminals::default();
        let _ = output;
        let denied = app.agents.terminals.output(&ee_agent_protocol::TerminalOutputRequest::new(
            ee_agent_protocol::SessionId::new("s2"),
            terminal_id.clone(),
        ));
        assert!(denied.is_err(), "other sessions must not observe trusted terminal output");
        let owned = app.agents.terminals.output(&ee_agent_protocol::TerminalOutputRequest::new(
            ee_agent_protocol::SessionId::new("proxy"),
            terminal_id.clone(),
        ));
        assert!(owned.is_ok(), "owner session observes output through the same pipeline");
        let killed = app.agents.terminals.kill(&ee_agent_protocol::KillTerminalRequest::new(
            ee_agent_protocol::SessionId::new("proxy"),
            ee_agent_protocol::TerminalId::new(terminal_id),
        ));
        assert!(killed.is_ok(), "cancellation works through the trusted path");
        app.shutdown_agents();
    }
}
