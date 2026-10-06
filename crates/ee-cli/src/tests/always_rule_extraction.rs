use std::path::PathBuf;

use crate::policy::{
    AlwaysRuleCandidate, BrowserActionClass, CommandInvocation, MatchMode, McpInvocation,
    NetworkMethodClass, NetworkScheme, OperationIdentity, PathPrefix, TransportKind, TrustCategory,
    TrustEffect, TrustOperation, TrustRule, WorkspaceIdentity, WriteOperationKind,
};

fn workspace() -> WorkspaceIdentity {
    WorkspaceIdentity::from_canonical_root_bytes(b"/workspace")
}

fn command(executable: &str, argv: &[&str]) -> CommandInvocation {
    CommandInvocation {
        workspace: workspace(),
        executable: executable.to_string(),
        argv: argv.iter().map(|value| (*value).to_string()).collect(),
        canonical_cwd: PathBuf::from("/workspace/src"),
    }
}

fn mcp_invocation() -> McpInvocation {
    McpInvocation {
        workspace: workspace(),
        agent: Some("codex".into()),
        transport: TransportKind::McpStdio,
        transport_identity: "stdio:ee --mcp-proxy".into(),
        server: "ee".into(),
        tool: "ee_format_file".into(),
        tool_schema_version: 3,
        category: TrustCategory::WriteModify,
        arguments_json: r#"{"path":"src/lib.rs"}"#.into(),
    }
}

#[test]
fn always_rule_extraction_command_exact_and_prefix_use_token_boundaries() {
    let invocation = command("cargo", &["test", "--package", "ee-cli"]);
    let exact = AlwaysRuleCandidate::command_exact(&invocation).unwrap();
    let prefix = AlwaysRuleCandidate::command_prefix(&invocation, 2).unwrap();

    let TrustRule::Command(exact_rule) = exact.rule else { panic!("command rule") };
    assert_eq!(exact_rule.match_mode, MatchMode::ArgvExact);
    assert_eq!(exact_rule.argv, invocation.argv);
    let TrustRule::Command(prefix_rule) = prefix.rule else { panic!("command rule") };
    assert_eq!(prefix_rule.match_mode, MatchMode::ArgvPrefix);
    assert_eq!(prefix_rule.argv, vec!["test", "--package"]);
    assert!(AlwaysRuleCandidate::command_prefix(&invocation, 0).is_err());
    assert!(AlwaysRuleCandidate::command_prefix(&invocation, 4).is_err());

    let shell = command("bash", &["-lc", "cargo test"]);
    assert!(AlwaysRuleCandidate::command_exact(&shell).is_err());
}

#[test]
fn always_rule_extraction_path_prefix_rejects_root_traversal_glob_and_protected_paths() {
    for invalid in ["", ".", "..", "/", "src/../secrets", "src/*", ".git/hooks"] {
        assert!(PathPrefix::parse(invalid).is_err(), "accepted {invalid:?}");
    }
    let prefix = PathPrefix::parse("src/generated").unwrap();
    let candidate = AlwaysRuleCandidate::write_prefix(
        workspace(),
        WriteOperationKind::Create,
        prefix,
        2,
        128,
        96,
    )
    .unwrap();
    let TrustRule::Write(rule) = candidate.rule else { panic!("write rule") };
    assert_eq!(rule.path_prefix.display(), "src/generated");
    assert_eq!(rule.max_files, 2);
    assert_eq!(rule.max_total_bytes, 128);
    assert_eq!(rule.max_file_bytes, 96);
    assert!(
        AlwaysRuleCandidate::write_prefix(
            workspace(),
            WriteOperationKind::Create,
            PathPrefix::parse("src").unwrap(),
            0,
            128,
            96,
        )
        .is_err()
    );
}

#[test]
fn always_rule_extraction_network_is_exact_host_and_read_only() {
    let candidate = AlwaysRuleCandidate::network_exact_read(
        workspace(),
        NetworkScheme::Https,
        "api.example.com".into(),
        443,
        NetworkMethodClass::Read,
        BrowserActionClass::Fetch,
    )
    .unwrap();
    let matching = TrustOperation {
        workspace: workspace(),
        agent: None,
        transport: TransportKind::McpStdio,
        category: TrustCategory::Network,
        identity: OperationIdentity::network(
            NetworkScheme::Https,
            "api.example.com",
            443,
            NetworkMethodClass::Read,
            BrowserActionClass::Fetch,
        )
        .unwrap(),
    };
    let boundary_miss = TrustOperation {
        identity: OperationIdentity::network(
            NetworkScheme::Https,
            "notapi.example.com",
            443,
            NetworkMethodClass::Read,
            BrowserActionClass::Fetch,
        )
        .unwrap(),
        ..matching.clone()
    };
    assert!(candidate.rule.matches(&matching));
    assert!(!candidate.rule.matches(&boundary_miss));
    assert!(
        AlwaysRuleCandidate::network_exact_read(
            workspace(),
            NetworkScheme::Https,
            "api.example.com".into(),
            443,
            NetworkMethodClass::Write,
            BrowserActionClass::Upload,
        )
        .is_err()
    );
    assert!(
        AlwaysRuleCandidate::network_exact_read(
            workspace(),
            NetworkScheme::Https,
            "*.example.com".into(),
            443,
            NetworkMethodClass::Read,
            BrowserActionClass::Fetch,
        )
        .is_err()
    );
}

#[test]
fn always_rule_extraction_mcp_is_exact_across_arguments_schema_and_transport() {
    let invocation = mcp_invocation();
    let candidate = AlwaysRuleCandidate::mcp_exact(&invocation).unwrap();
    assert!(candidate.rule.matches(&invocation.to_operation()));

    let mut changed_arguments = invocation.clone();
    changed_arguments.arguments_json = r#"{"path":"src/main.rs"}"#.into();
    assert!(!candidate.rule.matches(&changed_arguments.to_operation()));
    let mut changed_schema = invocation.clone();
    changed_schema.tool_schema_version = 4;
    assert!(!candidate.rule.matches(&changed_schema.to_operation()));
    let mut changed_transport = invocation;
    changed_transport.transport_identity = "acp:ee".into();
    assert!(!candidate.rule.matches(&changed_transport.to_operation()));
}

#[test]
fn always_rule_extraction_preview_snapshot_matches_rule_authority() {
    let candidate =
        AlwaysRuleCandidate::command_prefix(&command("cargo", &["test", "--package", "ee-cli"]), 2)
            .unwrap();
    let snapshot = candidate
        .preview
        .authority_fields()
        .into_iter()
        .map(|(label, value)| format!("{label}: {value}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        snapshot,
        concat!(
            "effect: allow always\n",
            "workspace: sha256:ac3fcb8dccb255c1e3526224621e22241325bf67b24ebb69887891f0be85fb5a\n",
            "agent: all agents\n",
            "kind: command\n",
            "executable: cargo\n",
            "arguments: token prefix · 2 tokens\n",
            "cwd scope: any canonical directory in workspace\n",
            "terminal output bytes: 1048576\n",
            "excludes: shell wrappers, environment, different executable"
        )
    );
    assert!(!snapshot.contains("expires"), "always-allow preview must not show expiry");
    assert!(!snapshot.contains("maximum uses"), "always-allow preview must not show a use budget");
    let TrustRule::Command(rule) = &candidate.rule else { panic!("command rule") };
    assert_eq!(rule.effect, TrustEffect::Allow);
    assert_eq!(rule.scope.workspace.as_string(), candidate.preview.workspace);
    assert_eq!(rule.scope.agent, None);
    assert_eq!(rule.scope.expires_at, None);
    assert_eq!(rule.scope.max_uses, None);
    assert_eq!(rule.argv.len(), 2);
}

#[test]
fn always_rule_extraction_every_candidate_is_unbounded_and_agent_agnostic() {
    let invocation = command("cargo", &["check"]);
    let command_exact = AlwaysRuleCandidate::command_exact(&invocation).unwrap();
    let command_prefix = AlwaysRuleCandidate::command_prefix(&invocation, 1).unwrap();
    let network = AlwaysRuleCandidate::network_exact_read(
        workspace(),
        NetworkScheme::Https,
        "example.com".into(),
        443,
        NetworkMethodClass::Read,
        BrowserActionClass::Navigate,
    )
    .unwrap();
    let write = AlwaysRuleCandidate::write_prefix(
        workspace(),
        WriteOperationKind::Modify,
        PathPrefix::parse("src").unwrap(),
        1,
        32,
        32,
    )
    .unwrap();
    let mcp = AlwaysRuleCandidate::mcp_exact(&mcp_invocation()).unwrap();
    for candidate in [command_exact, command_prefix, network, write, mcp] {
        assert_eq!(candidate.rule.effect(), TrustEffect::Allow);
        assert_eq!(candidate.rule.scope().agent, None, "{:?}", candidate.preview);
        assert_eq!(candidate.rule.scope().expires_at, None, "{:?}", candidate.preview);
        assert_eq!(candidate.rule.scope().max_uses, None, "{:?}", candidate.preview);
    }
}

#[cfg(feature = "agents")]
#[test]
fn always_rule_extraction_ui_requires_preview_then_explicit_confirmation() {
    use crate::app::{App, ApprovalChoice};
    use crate::policy::TrustStore;
    use crate::tests::agent_mcp::{base_agent_script, mcp_app};
    use crate::tests::helpers::CurrentDirGuard;
    use crate::workspace_trust::{WorkspaceTrustDecision, WorkspaceTrustStore};

    let (mut app, temp, _fake): (App, _, _) = mcp_app(base_agent_script(), false, true);
    // The trust gate resolves the workspace from the process cwd; hold the
    // workspace steady for the duration of the test.
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();

    let state = temp.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    app.agents.test_trust_store_base = Some(state.clone());
    // Always-allow is offered and persisted only while the host-local
    // workspace decision is `trusted`.
    WorkspaceTrustStore::at(&state, temp.path())
        .unwrap()
        .set_decision(WorkspaceTrustDecision::Trusted)
        .unwrap();

    let first = app.queue_terminal_approval_for_test(
        "always-preview",
        Some("codex"),
        "printf",
        &["%s", "ok"],
        &[],
        Some(temp.path().to_path_buf()),
    );
    // Queue the identical request before the rule exists so its prompt stays
    // pending and the reuse path is reachable after the first rule persists.
    let second = app.queue_terminal_approval_for_test(
        "always-reuse",
        Some("codex"),
        "printf",
        &["%s", "ok"],
        &[],
        Some(temp.path().to_path_buf()),
    );
    assert_eq!(app.agents.approvals.len(), 2, "both requests await approval");

    // First confirmation opens the preview; nothing is persisted yet.
    app.confirm_bridge_approval_for_test(ApprovalChoice::AllowAlways);
    let preview = app
        .agents
        .approvals
        .front()
        .and_then(|prompt| prompt.allow_confirmation_preview())
        .expect("always-allow preview");
    assert_eq!(preview.agent, "all agents");
    let authority = preview.authority_fields();
    assert!(authority.iter().any(|(label, value)| label == "effect" && value == "allow always"));
    assert!(!authority.iter().any(|(label, _)| label == "expires" || label == "maximum uses"));
    assert!(TrustStore::at(&state, temp.path()).unwrap().load().unwrap().rules.is_empty());

    // Cancel discards the candidate without persisting anything.
    app.cancel_rule_confirmation_for_test();
    assert!(app.agents.approvals.front().unwrap().allow_confirmation_preview().is_none());
    assert!(TrustStore::at(&state, temp.path()).unwrap().load().unwrap().rules.is_empty());

    // Explicit confirmation persists exactly one workspace-wide unbounded rule.
    app.confirm_bridge_approval_for_test(ApprovalChoice::AllowAlways);
    app.confirm_bridge_approval_for_test(ApprovalChoice::AllowAlways);
    let document = TrustStore::at(&state, temp.path()).unwrap().load().unwrap();
    assert_eq!(document.rules.len(), 1);
    let rule = &document.rules[0];
    assert_eq!(rule.effect(), TrustEffect::Allow);
    assert_eq!(rule.scope().agent, None);
    assert_eq!(rule.scope().expires_at, None);
    assert_eq!(rule.scope().max_uses, None);

    // Confirming the identical candidate reuses the persisted rule instead of
    // appending a duplicate (`TrustRule::same_matcher`).
    assert_eq!(app.agents.approvals.len(), 1, "second prompt still pending");
    app.confirm_bridge_approval_for_test(ApprovalChoice::AllowAlways);
    app.confirm_bridge_approval_for_test(ApprovalChoice::AllowAlways);
    assert_eq!(TrustStore::at(&state, temp.path()).unwrap().load().unwrap().rules.len(), 1);
    drop(first);
    drop(second);
}
