//! Built-in workspace-trusted read-only allowlist tests (ISSUES.md
//! "Unified Host-Local Workspace Trust Policy").
//!
//! `safe_read` registry matches are pure and unit-tested here; the approval
//! flow is exercised through the test-only bridge seam with the cwd pinned to
//! the fixture workspace, because the host-local workspace trust decision
//! resolves the workspace from the process cwd.

#[cfg(feature = "agents")]
mod gate {
    use std::fs;
    use std::path::{Path, PathBuf};

    use crate::app::App;
    use crate::policy::profiles::{
        PROFILE_REGISTRY_VERSION, PROFILES, ProfileArgPolicy, SAFE_READ_PROFILE,
        match_profile_candidates,
    };
    use crate::policy::{
        CommandRule, MatchMode, TrustEffect, TrustRule, TrustRuleScope, TrustStore,
    };
    use crate::tests::agent_bridge::{agents_app_in, base_script};
    use crate::tests::helpers::CurrentDirGuard;
    use crate::workspace_trust::{WorkspaceTrustDecision, WorkspaceTrustStore};

    const SESSION: &str = "safe-read-gate";

    fn tokens(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn profiles_for(executable: &str, argv: &[&str]) -> Vec<&'static str> {
        match_profile_candidates(executable, &tokens(argv))
            .into_iter()
            .map(|matched| matched.profile)
            .collect()
    }

    // ── Registry ────────────────────────────────────────────────────────────

    #[test]
    fn safe_read_registry_covers_read_only_command_families() {
        for (executable, argv) in [
            ("ls", &["-la"][..]),
            ("pwd", &[][..]),
            ("cat", &["src/main.rs"][..]),
            ("head", &["-n", "20", "src/main.rs"][..]),
            ("tail", &["--lines", "5", "Cargo.toml"][..]),
            ("wc", &["-l", "src/main.rs"][..]),
            ("stat", &["-c", "%s", "Cargo.toml"][..]),
            ("file", &["src/main.rs"][..]),
            ("du", &["-sh"][..]),
            ("find", &[".", "-name", "*.rs"][..]),
            ("fd", &["main", "src"][..]),
            ("diff", &["-u", "a.txt", "b.txt"][..]),
            ("sha256sum", &["Cargo.toml"][..]),
            ("sort", &["-n", "numbers.txt"][..]),
            ("cut", &["-d", ",", "-f", "1", "data.csv"][..]),
            ("grep", &["-n", "fn main", "src/main.rs"][..]),
            ("rg", &["-n", "fn main", "src"][..]),
            ("rg", &["fn main", "."][..]),
            ("git", &["status", "--short"][..]),
            ("git", &["diff", "--stat"][..]),
            ("git", &["log", "--oneline", "-n", "5"][..]),
            ("git", &["log", "--max-count=5"][..]),
            ("git", &["diff", "--diff-filter=AM"][..]),
            ("npm", &["list", "--depth=0"][..]),
            ("git", &["show", "HEAD"][..]),
            ("git", &["blame", "-L", "1,10", "src/main.rs"][..]),
            ("git", &["ls-files", "--cached"][..]),
            ("git", &["rev-parse", "--show-toplevel"][..]),
            ("git", &["stash", "list"][..]),
            ("npm", &["list", "--depth", "0"][..]),
            ("yarn", &["list"][..]),
            ("pnpm", &["list"][..]),
            ("node", &["--version"][..]),
            ("echo", &["hello", "world"][..]),
            ("printf", &["%s\\n", "value"][..]),
            ("date", &["-u"][..]),
            ("ps", &["aux"][..]),
        ] {
            let profiles = profiles_for(executable, argv);
            assert!(
                profiles.contains(&SAFE_READ_PROFILE),
                "{executable} {argv:?} must match safe_read: {profiles:?}"
            );
        }
    }

    #[test]
    fn safe_read_rejects_mutation_network_interpreters_and_unknown_flags() {
        for (executable, argv) in [
            // Mutation and VCS writes.
            ("git", &["push"][..]),
            ("git", &["commit", "-m", "x"][..]),
            ("git", &["checkout", "main"][..]),
            ("git", &["clean", "-fd"][..]),
            ("rm", &["-rf", "target"][..]),
            ("mv", &["a", "b"][..]),
            ("cp", &["a", "b"][..]),
            ("mkdir", &["new"][..]),
            ("touch", &["file"][..]),
            ("truncate", &["-s", "0", "file"][..]),
            ("tee", &["file"][..]),
            // In-place / output-redirecting flags.
            ("git", &["diff", "--output=patch.txt"][..]),
            ("git", &["diff", "--ext-diff"][..]),
            ("git", &["diff", "--no-index", "a", "b"][..]),
            ("ls", &["-R"][..]),
            ("find", &[".", "-exec", "rm", "{}", ";"][..]),
            ("find", &[".", "-delete"][..]),
            ("find", &[".", "-fprint", "out"][..]),
            ("find", &[".", "-newer", "/etc/shadow"][..]),
            ("rg", &["--files-with-matches", "x"][..]),
            ("grep", &["-rn", "x", "src"][..]),
            ("grep", &["-R", "x", "src"][..]),
            ("rg", &["--hidden", "x"][..]),
            ("rg", &["--no-ignore", "x"][..]),
            ("rg", &["-uu", "x"][..]),
            ("fd", &["-H", "x"][..]),
            ("fd", &["-L", "x"][..]),
            ("sha256sum", &["--check", "sums.txt"][..]),
            ("git", &["cat-file", "-p", "HEAD"][..]),
            // Interpreters and shell wrappers.
            ("python", &["-c", "print(1)"][..]),
            ("node", &["-e", "1"][..]),
            ("perl", &["-e", "1"][..]),
            ("awk", &["{print}"][..]),
            ("sed", &["-i", "s/a/b/", "file"][..]),
            ("xargs", &["rm"][..]),
            ("sh", &["-c", "ls"][..]),
            ("bash", &["-lc", "ls"][..]),
            // Secret dumps.
            ("env", &[][..]),
            ("printenv", &[][..]),
            // Network and package mutation.
            ("curl", &["https://example.com"][..]),
            ("wget", &["https://example.com"][..]),
            ("npm", &["install"][..]),
            ("npm", &["outdated"][..]),
            ("npm", &["view", "left-pad"][..]),
            ("cargo", &["check"][..]),
            ("cargo", &["install", "cargo-nextest"][..]),
            ("git", &["ls-remote", "origin"][..]),
            ("git", &["fetch"][..]),
        ] {
            assert!(
                profiles_for(executable, argv).is_empty(),
                "{executable} {argv:?} must never match a curated profile"
            );
        }
    }

    #[test]
    fn safe_read_registry_hygiene() {
        assert_eq!(PROFILE_REGISTRY_VERSION, 3);
        let forbidden_executables = [
            "rm", "mv", "cp", "mkdir", "touch", "chmod", "chown", "ln", "dd", "tee", "truncate",
            "sh", "bash", "zsh", "fish", "python", "python3", "perl", "ruby", "awk", "sed",
            "xargs", "env", "printenv", "curl", "wget", "ssh", "scp", "rsync", "docker", "kubectl",
            "make", "just", "npx",
        ];
        let forbidden_tokens =
            ["--output", "--ext-diff", "--no-index", "-exec", "-delete", "-fprint", "-fls", "-ok"];
        for profile in PROFILES {
            for entry in profile.entries {
                assert!(
                    !forbidden_executables.contains(&entry.executable),
                    "{}: {executable} must never appear in the registry",
                    profile.id,
                    executable = entry.executable
                );
                for token in entry.args.flags.iter().chain(entry.args.value_flags.iter()) {
                    assert!(
                        !forbidden_tokens.contains(token),
                        "{}: forbidden token {token}",
                        profile.id
                    );
                    assert!(token.starts_with('-'), "{}: flag {token} must be a flag", profile.id);
                }
                if entry.args.allow_paths || entry.args.allow_pattern {
                    assert!(
                        entry.args != ProfileArgPolicy::EXACT,
                        "{}: permissive entries carry an explicit policy",
                        profile.id
                    );
                }
            }
        }
        let safe = PROFILES.iter().find(|profile| profile.id == SAFE_READ_PROFILE).unwrap();
        assert!(safe.entries.len() >= 40, "safe_read must stay a broad read-only list");
        // `node` is registered for the version probe only; entries must stay
        // exact so `-e`/script execution can never match.
        for entry in safe.entries.iter().filter(|entry| entry.executable == "node") {
            assert_eq!(entry.argv, &["--version"]);
            assert_eq!(entry.args, ProfileArgPolicy::EXACT);
        }
        // Traversal widening stays out: recursive grep ignores ignore rules,
        // and rg/fd hidden/ignored/symlink flags lower the bar further.
        for entry in PROFILES.iter().flat_map(|profile| profile.entries.iter()) {
            match entry.executable {
                "grep" | "egrep" | "fgrep" => {
                    assert!(!entry.args.flags.contains(&"-r"), "grep recursion");
                    assert!(!entry.args.flags.contains(&"-R"), "grep symlink recursion");
                }
                "rg" => {
                    for flag in
                        ["--hidden", "--no-ignore", "--no-ignore-dot", "--no-ignore-vcs", "-uu"]
                    {
                        assert!(!entry.args.flags.contains(&flag), "rg widening flag {flag}");
                    }
                }
                "fd" => {
                    for flag in ["-H", "--hidden", "-I", "--no-ignore", "-L", "--follow"] {
                        assert!(!entry.args.flags.contains(&flag), "fd widening flag {flag}");
                    }
                }
                "sha256sum" | "sha1sum" | "md5sum" => {
                    assert!(!entry.args.flags.contains(&"--check"), "checksum file targets");
                }
                _ => {}
            }
        }
    }

    // ── Approval flow ───────────────────────────────────────────────────────

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

    fn queue(
        app: &mut App,
        workspace: &Path,
        command: &str,
        args: &[&str],
    ) -> tokio::sync::oneshot::Receiver<ee_agent_host::ClientRequestResult> {
        app.queue_terminal_approval_for_test(
            SESSION,
            None,
            command,
            args,
            &[],
            Some(workspace.to_path_buf()),
        )
    }

    #[test]
    fn safe_read_auto_allows_read_only_commands_on_trusted_workspace() {
        let (mut app, temp, state_dir) = app_with_store();
        let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
        let _cwd_restore = CurrentDirGuard::capture();
        std::env::set_current_dir(temp.path()).unwrap();
        fs::create_dir_all(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        set_decision(&state_dir, temp.path(), WorkspaceTrustDecision::Trusted);

        for (command, args) in [
            ("ls", &["-la"][..]),
            ("ls", &["."][..]),
            ("head", &["-n", "5", "src/main.rs"][..]),
            ("cat", &["src/main.rs"][..]),
            ("diff", &["-u", "src/main.rs", "src/main.rs"][..]),
            ("sha256sum", &["src/main.rs"][..]),
            ("echo", &["hello", "world"][..]),
        ] {
            let mut reply = queue(&mut app, temp.path(), command, args);
            assert!(
                app.agents.approvals.is_empty(),
                "{command} {args:?} must auto-allow on a trusted workspace"
            );
            assert!(
                reply.try_recv().expect("auto-allowed reply").is_ok(),
                "{command} {args:?} must dispatch"
            );
        }
    }

    #[test]
    fn safe_read_prompts_on_untrusted_workspace() {
        let (mut app, temp, _state_dir) = app_with_store();
        let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
        let _cwd_restore = CurrentDirGuard::capture();
        std::env::set_current_dir(temp.path()).unwrap();

        let mut reply = queue(&mut app, temp.path(), "ls", &["-la"]);
        assert_eq!(app.agents.approvals.len(), 1, "undecided workspace must prompt");
        assert!(reply.try_recv().is_err(), "prompt stays unresolved");
        app.agents.approvals.clear();
    }

    #[test]
    fn safe_read_blocks_path_escapes_unknown_flags_and_mutators() {
        let (mut app, temp, state_dir) = app_with_store();
        let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
        let _cwd_restore = CurrentDirGuard::capture();
        std::env::set_current_dir(temp.path()).unwrap();
        fs::create_dir_all(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        set_decision(&state_dir, temp.path(), WorkspaceTrustDecision::Trusted);

        for (command, args) in [
            ("cat", &["../outside.txt"][..]),
            ("cat", &["/etc/passwd"][..]),
            ("ls", &["/"][..]),
            ("ls", &[".."][..]),
            ("head", &["-n", "5", "/etc/hosts"][..]),
            ("git", &["diff", "--output=patch.txt"][..]),
            ("git", &["diff", "--no-index", "a", "b"][..]),
            ("git", &["show", "HEAD:.env"][..]),
            ("find", &[".", "-exec", "rm", "{}", ";"][..]),
            ("rg", &["--files-with-matches", "x"][..]),
            ("grep", &["-rn", "x", "src"][..]),
            ("npm", &["outdated"][..]),
            ("cargo", &["check"][..]),
            ("rm", &["-rf", "target"][..]),
        ] {
            let mut reply = queue(&mut app, temp.path(), command, args);
            assert_eq!(
                app.agents.approvals.len(),
                1,
                "{command} {args:?} must require explicit approval"
            );
            assert!(reply.try_recv().is_err(), "{command} {args:?} must not dispatch");
            app.agents.approvals.clear();
        }
    }

    #[test]
    fn safe_read_respects_persistent_deny_before_built_in_allow() {
        let (mut app, temp, state_dir) = app_with_store();
        let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
        let _cwd_restore = CurrentDirGuard::capture();
        std::env::set_current_dir(temp.path()).unwrap();
        set_decision(&state_dir, temp.path(), WorkspaceTrustDecision::Trusted);

        let store = TrustStore::at(&state_dir, temp.path()).expect("trust store");
        store
            .add_rule(TrustRule::Command(CommandRule {
                id: String::from("deny_git_status"),
                effect: TrustEffect::Deny,
                scope: TrustRuleScope {
                    workspace: app.primary_workspace_identity(),
                    agent: None,
                    expires_at: None,
                    max_uses: None,
                },
                executable: String::from("git"),
                match_mode: MatchMode::ArgvExact,
                argv: vec![String::from("status")],
            }))
            .expect("deny rule persisted");
        app.reload_workspace_trust_store().expect("reload deny rule");

        let mut reply = queue(&mut app, temp.path(), "git", &["status"]);
        assert!(app.agents.approvals.is_empty(), "persistent deny resolves without a prompt");
        let error = reply.try_recv().expect("denied reply").expect_err("denied");
        let message = format!("{error:?}");
        assert!(message.contains("deny_git_status"), "deny rule id surfaced: {message}");
    }
}
