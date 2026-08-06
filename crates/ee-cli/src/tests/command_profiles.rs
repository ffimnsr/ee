//! Phase 4 curated command profile tests (ISSUES.md "Unified Host-Local
//! Workspace Trust Policy"): the application-owned profile registry and the
//! terminal approval integration.
//!
//! Profiles cover only fixed validation commands (`git_readonly`,
//! `rust_validate`); VCS mutation, package install, publish, network, and
//! shell commands never match.  The workspace gate plus a matching profile
//! rule is required before any profile command auto-allows.

#[cfg(feature = "agents")]
use std::fs;
#[cfg(feature = "agents")]
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::policy::profiles::{
    PROFILE_REGISTRY_VERSION, PROFILES, is_known_profile, match_profile_entry,
};
use crate::policy::rules::TrustRule;
use crate::policy::session::SessionPolicy;
use crate::policy::{
    DecisionReason, OperationIdentity, TrustCategory, TrustOperation, TrustOutcome, TrustRuleScope,
    UsageSnapshot, WorkspaceIdentity,
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

fn profile_rule(workspace: WorkspaceIdentity, profile: &str) -> TrustRule {
    TrustRule::Profile(crate::policy::ProfileRule {
        id: format!("profile_{profile}"),
        scope: TrustRuleScope {
            workspace,
            agent: None,
            expires_at: Some(at("2026-08-08T12:00:00Z")),
            max_uses: Some(20),
        },
        profile: profile.to_string(),
    })
}

fn profile_op(workspace: WorkspaceIdentity, profile: &str) -> TrustOperation {
    TrustOperation {
        workspace,
        agent: None,
        transport: crate::policy::TransportKind::Acp,
        category: TrustCategory::Execute,
        identity: OperationIdentity::Profile { profile: profile.to_string() },
    }
}

fn decide(
    op: &TrustOperation,
    rules: &[TrustRule],
    workspace_enabled: bool,
) -> crate::policy::TrustDecision {
    crate::policy::evaluate(&crate::policy::evaluator::PolicyInput {
        session_id: "s1",
        fingerprint: "fp",
        operation: op,
        session: &SessionPolicy::default(),
        rules,
        now: at("2026-08-07T12:00:00Z"),
        usage: &UsageSnapshot::default(),
        workspace_enabled,
    })
}

// ── Registry ─────────────────────────────────────────────────────────────────

#[test]
fn profile_registry_is_versioned_and_application_owned() {
    assert_eq!(PROFILE_REGISTRY_VERSION, 1);
    let ids: Vec<&str> = PROFILES.iter().map(|profile| profile.id).collect();
    assert_eq!(ids, vec!["git_readonly", "rust_validate"]);
    assert!(is_known_profile("git_readonly"));
    assert!(is_known_profile("rust_validate"));
    assert!(!is_known_profile("mystery_profile"));
    assert!(!is_known_profile(""));
}

#[test]
fn profile_entries_cover_only_curated_validation_commands() {
    let tokens = |values: &[&str]| values.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    for (executable, argv, profile) in [
        ("git", &["status"][..], "git_readonly"),
        ("git", &["diff"][..], "git_readonly"),
        ("git", &["log"][..], "git_readonly"),
        ("git", &["show"][..], "git_readonly"),
        ("git", &["branch", "--show-current"][..], "git_readonly"),
        ("cargo", &["fmt", "--check"][..], "rust_validate"),
        ("cargo", &["test", "--quiet"][..], "rust_validate"),
        ("cargo", &["clippy"][..], "rust_validate"),
    ] {
        let matched = match_profile_entry(executable, &tokens(argv));
        assert_eq!(matched.map(|(id, _)| id), Some(profile), "{executable} {argv:?}");
    }
}

#[test]
fn profile_entries_exclude_mutation_install_publish_and_shell() {
    let tokens = |values: &[&str]| values.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    for (executable, argv) in [
        // VCS mutation.
        ("git", &["commit"][..]),
        ("git", &["commit", "-m", "x"][..]),
        ("git", &["reset", "--hard"][..]),
        ("git", &["clean", "-fd"][..]),
        ("git", &["push"][..]),
        ("git", &["pull"][..]),
        ("git", &["merge"][..]),
        ("git", &["rebase"][..]),
        ("git", &["stash"][..]),
        ("git", &["checkout"][..]),
        // Package install / build / publish.
        ("cargo", &["install"][..]),
        ("cargo", &["build"][..]),
        ("cargo", &["run"][..]),
        ("cargo", &["publish"][..]),
        ("cargo", &["add"][..]),
        ("cargo", &["test"][..]),
        ("cargo", &["fmt"][..]),
        // Shell interpretation.
        ("sh", &["-c", "git status"][..]),
        ("bash", &["-c", "cargo test"][..]),
        // Partial-argument variants never match fixed entries.
        ("git", &["status", "--short"][..]),
        ("git", &["branch"][..]),
    ] {
        assert!(
            match_profile_entry(executable, &tokens(argv)).is_none(),
            "{executable} {argv:?} must not match any profile"
        );
    }
}

#[test]
fn profile_entries_carry_bounded_caps_and_fixed_flags() {
    for profile in PROFILES {
        for entry in profile.entries {
            assert!(entry.timeout_cap > Duration::ZERO, "{}: timeout cap", profile.id);
            assert!(entry.output_cap > 0, "{}: output cap", profile.id);
            assert!(
                !entry.argv.iter().any(|token| token.contains(';') || token.contains('&')),
                "{}: fixed flags only",
                profile.id
            );
        }
    }
    let git = PROFILES.iter().find(|profile| profile.id == "git_readonly").unwrap();
    assert_eq!(git.entries.len(), 5);
    let rust = PROFILES.iter().find(|profile| profile.id == "rust_validate").unwrap();
    assert_eq!(rust.entries.len(), 3);
}

// ── Profile rules in the shared evaluator ────────────────────────────────────

#[test]
fn profile_rules_require_gate_and_exact_profile_id() {
    let ws = identity(b"/work/root");
    let rule = profile_rule(ws, "git_readonly");
    let op = profile_op(ws, "git_readonly");

    let gated = decide(&op, std::slice::from_ref(&rule), false);
    assert_eq!(gated.outcome, TrustOutcome::Prompt);
    assert_eq!(gated.reason, DecisionReason::WorkspaceDisabled, "gate required");

    let allowed = decide(&op, std::slice::from_ref(&rule), true);
    assert_eq!(allowed.outcome, TrustOutcome::Allow);
    assert_eq!(allowed.rule_id.as_deref(), Some("profile_git_readonly"));

    // Another profile id never matches this rule.
    let other = decide(&profile_op(ws, "rust_validate"), std::slice::from_ref(&rule), true);
    assert_eq!(other.outcome, TrustOutcome::Prompt);

    // The gate alone never authorizes a profile command.
    let bare = decide(&op, &[], true);
    assert_eq!(bare.reason, DecisionReason::NoMatchingRule);
}

// ── End-to-end terminal integration (agents feature) ─────────────────────────

#[cfg(feature = "agents")]
mod e2e {
    use super::*;
    use crate::policy::store::{TrustStore, TrustStoreDocument};
    use crate::tests::agent_mcp::{
        base_agent_script, connect_proxy, mcp_app, open_pane_and_wait_ready, press, proxy_recv,
        proxy_send, settle, wait_until,
    };
    use crate::tests::helpers::run_ex;
    use crossterm::event::{KeyCode, KeyModifiers};
    use serde_json::json;

    fn seed_profile_store(state_dir: &Path, workspace: &Path, enabled: bool, profile: &str) {
        let store = TrustStore::at(state_dir, workspace).unwrap();
        let rule = TrustRule::Profile(crate::policy::ProfileRule {
            id: format!("profile_{profile}"),
            scope: TrustRuleScope {
                workspace: *store.workspace(),
                agent: None,
                expires_at: Some(SystemTime::now() + Duration::from_secs(3600)),
                max_uses: Some(20),
            },
            profile: profile.to_string(),
        });
        let document = TrustStoreDocument {
            workspace: *store.workspace(),
            workspace_enabled: enabled,
            rules: vec![rule],
        };
        store.write(&document).expect("seed store");
    }

    fn terminal_frame(command: &str, args: serde_json::Value) -> serde_json::Value {
        json!({ "method": "terminal_create", "command": command, "args": args })
    }

    #[test]
    fn profile_grant_auto_allows_only_curated_commands() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        seed_profile_store(&state_dir, temp.path(), true, "git_readonly");

        let mut stream = connect_proxy(&app);
        // Every git_readonly entry auto-allows with the gate open.
        for (id, args) in [(1u64, json!(["status"])), (2, json!(["diff"])), (3, json!(["log"]))] {
            proxy_send(&mut stream, id, terminal_frame("git", args));
            wait_until(&mut app, "profile terminal spawned", |app| {
                app.agents.terminals.tracked_count() >= id as usize
                    && app.agents.approvals.is_empty()
            });
            let reply = proxy_recv(&mut stream);
            assert!(
                reply["result"]["value"].as_str().is_some(),
                "profile command must auto-allow: {reply}"
            );
        }

        // A non-profile command prompts even with the gate open.
        proxy_send(&mut stream, 4, terminal_frame("git", json!(["commit", "-m", "x"])));
        wait_until(&mut app, "non-profile command prompts", |app| !app.agents.approvals.is_empty());
        run_ex(&mut app, "agents");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE); // Deny
        let reply = proxy_recv(&mut stream);
        assert!(reply["result"]["error"].is_object(), "denied: {reply}");
        assert_eq!(app.agents.terminals.tracked_count(), 3);
    }

    #[test]
    fn profile_grant_never_applies_without_the_workspace_gate() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        seed_profile_store(&state_dir, temp.path(), false, "git_readonly");

        let mut stream = connect_proxy(&app);
        proxy_send(&mut stream, 1, terminal_frame("git", json!(["status"])));
        wait_until(&mut app, "gated profile command prompts", |app| {
            !app.agents.approvals.is_empty()
        });
        assert_eq!(app.agents.terminals.tracked_count(), 0, "no auto-allow without the gate");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE); // Deny
        let reply = proxy_recv(&mut stream);
        assert!(reply["result"]["error"].is_object(), "denied: {reply}");
    }

    #[test]
    fn profile_requires_the_workspace_root_cwd() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let subdir = temp.path().join("sub");
        fs::create_dir_all(&subdir).unwrap();
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        seed_profile_store(&state_dir, temp.path(), true, "git_readonly");

        let mut stream = connect_proxy(&app);
        proxy_send(
            &mut stream,
            1,
            json!({
                "method": "terminal_create",
                "command": "git",
                "args": ["status"],
                "cwd": subdir.display().to_string(),
            }),
        );
        wait_until(&mut app, "subdir profile command prompts", |app| {
            !app.agents.approvals.is_empty()
        });
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE); // Deny
        let reply = proxy_recv(&mut stream);
        assert!(reply["result"]["error"].is_object(), "denied: {reply}");
    }

    #[test]
    fn profile_terminal_keeps_ownership_and_pipeline_behavior() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        seed_profile_store(&state_dir, temp.path(), true, "git_readonly");

        let mut stream = connect_proxy(&app);
        proxy_send(&mut stream, 1, terminal_frame("git", json!(["status"])));
        wait_until(&mut app, "profile terminal spawned", |app| {
            app.agents.terminals.tracked_count() == 1
        });
        let reply = proxy_recv(&mut stream);
        let terminal_id = reply["result"]["value"].as_str().expect("terminal id").to_string();

        // Ownership and cancellation go through the unchanged pipeline.
        let denied = app.agents.terminals.output(&ee_agent_protocol::TerminalOutputRequest::new(
            ee_agent_protocol::SessionId::new("s2"),
            terminal_id.clone(),
        ));
        assert!(denied.is_err(), "other sessions must not observe profile terminal output");
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

    #[test]
    fn session_deny_overrides_profile_allow() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());

        let mut stream = connect_proxy(&app);
        // Round 1: no persistent rule yet, so the call prompts; the user
        // records a session-scoped denial for the exact fingerprint.
        proxy_send(&mut stream, 1, terminal_frame("git", json!(["status"])));
        wait_until(&mut app, "approval queued", |app| !app.agents.approvals.is_empty());
        run_ex(&mut app, "agents");
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        press(&mut app, KeyCode::Right, KeyModifiers::NONE); // Deny session
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        wait_until(&mut app, "round resolved", |app| app.agents.approvals.is_empty());
        let reply = proxy_recv(&mut stream);
        assert!(reply["result"]["error"].is_object(), "denied: {reply}");

        // A matching profile grant is seeded afterwards; the recorded
        // session deny still wins over the persistent profile allow.
        seed_profile_store(&state_dir, temp.path(), true, "git_readonly");
        proxy_send(&mut stream, 2, terminal_frame("git", json!(["status"])));
        settle(&mut app);
        let reply = proxy_recv(&mut stream);
        assert!(reply["result"]["error"].is_object(), "denied: {reply}");
        assert!(app.agents.approvals.is_empty(), "session deny must not queue a prompt");
        assert_eq!(app.agents.terminals.tracked_count(), 0);
    }
}
