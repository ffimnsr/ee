//! Always-allow workspace trust-gate integration tests (ISSUES.md "Unified
//! Host-Local Workspace Trust Policy"): the workspace-wide "Allow always"
//! option is offered, persisted, reused, and revoked only through the
//! host-local workspace trust decision.
//!
//! The gate resolves the workspace from the process cwd, so each test pins the
//! cwd to the fixture workspace; the rule store is injected through
//! `agents.test_trust_store_base` and no agent transport or terminal output is
//! observed beyond a successful spawn.

#[cfg(feature = "agents")]
mod gate {
    use std::fs;
    use std::path::{Path, PathBuf};

    use crate::app::{App, ApprovalChoice};
    use crate::policy::{TrustRule, TrustStore};
    use crate::tests::agent_bridge::{agents_app_in, base_script};
    use crate::tests::helpers::CurrentDirGuard;
    use crate::workspace_trust::{WorkspaceTrustDecision, WorkspaceTrustStore};

    const SESSION: &str = "always-allow-gate";

    /// Fixture app plus an injected host-local state dir with no recorded
    /// workspace decision yet.
    fn app_with_store() -> (App, tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        let (mut app, _fake) = agents_app_in(&temp, base_script());
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).expect("state directory");
        app.agents.test_trust_store_base = Some(state_dir.clone());
        (app, temp, state_dir)
    }

    fn set_decision(state_dir: &Path, workspace: &Path, decision: WorkspaceTrustDecision) {
        WorkspaceTrustStore::at(state_dir, workspace)
            .expect("workspace trust store")
            .set_decision(decision)
            .expect("workspace trust decision");
    }

    /// Queues one eligible structured terminal request for the fixture
    /// workspace through the test-only bridge seam.
    fn queue_terminal(
        app: &mut App,
        workspace: &Path,
    ) -> tokio::sync::oneshot::Receiver<ee_agent_host::ClientRequestResult> {
        // `git stash` is eligible structured command text but never matches
        // the built-in safe_read allowlist, so the prompt path is exercised.
        app.queue_terminal_approval_for_test(
            SESSION,
            None,
            "git",
            &["stash"],
            &[],
            Some(workspace.to_path_buf()),
        )
    }

    fn stored_rules(state_dir: &Path, workspace: &Path) -> Vec<TrustRule> {
        TrustStore::at(state_dir, workspace)
            .expect("trust store")
            .load()
            .expect("load trust store")
            .rules
    }

    #[test]
    fn always_allow_hidden_for_untrusted_workspace() {
        let (mut app, temp, state_dir) = app_with_store();
        let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
        let _cwd_restore = CurrentDirGuard::capture();
        std::env::set_current_dir(temp.path()).unwrap();

        // The trust base is injected but no decision is recorded: an undecided
        // workspace fails closed and never offers always-allow options.
        let mut reply = queue_terminal(&mut app, temp.path());
        assert_eq!(app.agents.approvals.len(), 1, "untrusted command must prompt");
        let prompt = app.agents.approvals.front().expect("approval");
        assert!(
            !prompt.options.iter().any(|(_, choice)| matches!(
                choice,
                ApprovalChoice::AllowAlways | ApprovalChoice::AllowAlwaysPrefix(_)
            )),
            "undecided workspace must not offer always-allow: {:?}",
            prompt.options
        );
        assert!(reply.try_recv().is_err(), "prompt stays unresolved");

        // The confirmation preview is candidate-backed: forcing the
        // always-allow confirmation must find no preview and persist nothing.
        app.confirm_bridge_approval_for_test(ApprovalChoice::AllowAlways);
        assert!(
            app.agents.approvals.front().expect("approval").allow_confirmation_preview().is_none(),
            "undecided workspace must not preview an always-allow rule"
        );
        app.cancel_rule_confirmation_for_test();
        assert!(stored_rules(&state_dir, temp.path()).is_empty(), "fail closed: no rule persisted");
    }

    #[test]
    fn always_allow_persists_and_auto_allows_under_trusted_workspace() {
        let (mut app, temp, state_dir) = app_with_store();
        let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
        let _cwd_restore = CurrentDirGuard::capture();
        std::env::set_current_dir(temp.path()).unwrap();
        set_decision(&state_dir, temp.path(), WorkspaceTrustDecision::Trusted);

        let mut first = queue_terminal(&mut app, temp.path());
        assert_eq!(app.agents.approvals.len(), 1, "unmatched command awaits approval");
        let prompt = app.agents.approvals.front().expect("approval");
        assert!(
            prompt.options.iter().any(|(label, choice)| {
                label == "Allow always" && *choice == ApprovalChoice::AllowAlways
            }),
            "trusted workspace offers the exact always-allow option: {:?}",
            prompt.options
        );

        // Explicit preview, then confirmation: exactly one workspace-wide rule
        // with no agent binding, expiry, or use budget.
        app.confirm_bridge_approval_for_test(ApprovalChoice::AllowAlways);
        assert!(
            app.agents.approvals.front().expect("approval").allow_confirmation_preview().is_some(),
            "always-allow must require an explicit preview"
        );
        app.confirm_bridge_approval_for_test(ApprovalChoice::AllowAlways);
        assert!(app.agents.approvals.is_empty(), "first prompt resolved");
        assert!(first.try_recv().expect("first reply").is_ok(), "first request dispatched");

        let rules = stored_rules(&state_dir, temp.path());
        assert_eq!(rules.len(), 1, "exactly one persisted always-allow rule");
        let TrustRule::Command(rule) = &rules[0] else { panic!("command rule expected") };
        assert_eq!(rule.executable, "git");
        assert_eq!(rule.argv, vec![String::from("stash")]);
        assert_eq!(rule.scope.agent, None, "workspace-wide, not agent-scoped");
        assert_eq!(rule.scope.expires_at, None, "always-allow rules never expire");
        assert_eq!(rule.scope.max_uses, None, "always-allow rules carry no use budget");

        // The identical command auto-allows through the persisted rule without
        // a new approval prompt.
        let mut second = queue_terminal(&mut app, temp.path());
        assert!(app.agents.approvals.is_empty(), "auto-allowed without a prompt");
        assert!(
            second.try_recv().expect("auto-allowed reply").is_ok(),
            "auto-allowed request dispatched"
        );
    }

    #[test]
    fn revoking_workspace_trust_makes_stored_always_rule_inert() {
        let (mut app, temp, state_dir) = app_with_store();
        let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
        let _cwd_restore = CurrentDirGuard::capture();
        std::env::set_current_dir(temp.path()).unwrap();
        set_decision(&state_dir, temp.path(), WorkspaceTrustDecision::Trusted);

        let mut first = queue_terminal(&mut app, temp.path());
        app.confirm_bridge_approval_for_test(ApprovalChoice::AllowAlways);
        app.confirm_bridge_approval_for_test(ApprovalChoice::AllowAlways);
        assert!(first.try_recv().expect("first reply").is_ok(), "first request dispatched");
        assert_eq!(stored_rules(&state_dir, temp.path()).len(), 1, "rule persisted while trusted");

        // Revocation filters the stored always-allow rule out of effective
        // policy: the rule stays on disk but becomes inert.
        set_decision(&state_dir, temp.path(), WorkspaceTrustDecision::Untrusted);
        app.reload_workspace_trust_store().expect("reload revoked trust");

        let mut second = queue_terminal(&mut app, temp.path());
        assert_eq!(app.agents.approvals.len(), 1, "revoked always-allow rule must not auto-apply");
        let prompt = app.agents.approvals.front().expect("approval");
        assert!(
            !prompt.options.iter().any(|(_, choice)| matches!(
                choice,
                ApprovalChoice::AllowAlways | ApprovalChoice::AllowAlwaysPrefix(_)
            )),
            "untrusted workspace must not offer always-allow again: {:?}",
            prompt.options
        );
        assert!(second.try_recv().is_err(), "prompt stays unresolved");
        assert_eq!(
            stored_rules(&state_dir, temp.path()).len(),
            1,
            "rule stays stored but inert after revocation"
        );
    }

    #[test]
    fn always_allow_reuses_identical_rule() {
        let (mut app, temp, state_dir) = app_with_store();
        let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
        let _cwd_restore = CurrentDirGuard::capture();
        std::env::set_current_dir(temp.path()).unwrap();
        set_decision(&state_dir, temp.path(), WorkspaceTrustDecision::Trusted);

        // Both prompts are queued before any rule exists, so each carries its
        // own previewed candidate for the identical request.
        let first = queue_terminal(&mut app, temp.path());
        let second = queue_terminal(&mut app, temp.path());
        assert_eq!(app.agents.approvals.len(), 2, "both requests await approval");

        // Confirm and persist the first candidate.
        app.confirm_bridge_approval_for_test(ApprovalChoice::AllowAlways);
        app.confirm_bridge_approval_for_test(ApprovalChoice::AllowAlways);
        assert_eq!(app.agents.approvals.len(), 1, "second prompt still pending");

        // Confirming the identical candidate reuses the stored rule instead of
        // appending a duplicate.
        app.confirm_bridge_approval_for_test(ApprovalChoice::AllowAlways);
        app.confirm_bridge_approval_for_test(ApprovalChoice::AllowAlways);
        assert!(app.agents.approvals.is_empty(), "both prompts resolved");
        assert_eq!(
            stored_rules(&state_dir, temp.path()).len(),
            1,
            "identical matcher reused instead of appending a duplicate"
        );
        drop(first);
        drop(second);
    }
}
