use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::policy::{
    BoundedRuleCandidate, BrowserActionClass, CommandInvocation, MatchMode, McpInvocation,
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

#[test]
fn bounded_rule_extraction_command_exact_and_prefix_use_token_boundaries() {
    let now = SystemTime::UNIX_EPOCH;
    let invocation = command("cargo", &["test", "--package", "ee-cli"]);
    let exact = BoundedRuleCandidate::command_exact(&invocation, Some("codex"), now).unwrap();
    let prefix = BoundedRuleCandidate::command_prefix(&invocation, Some("codex"), 2, now).unwrap();

    let TrustRule::Command(exact_rule) = exact.rule else { panic!("command rule") };
    assert_eq!(exact_rule.match_mode, MatchMode::ArgvExact);
    assert_eq!(exact_rule.argv, invocation.argv);
    let TrustRule::Command(prefix_rule) = prefix.rule else { panic!("command rule") };
    assert_eq!(prefix_rule.match_mode, MatchMode::ArgvPrefix);
    assert_eq!(prefix_rule.argv, vec!["test", "--package"]);
    assert!(BoundedRuleCandidate::command_prefix(&invocation, None, 0, now).is_err());
    assert!(BoundedRuleCandidate::command_prefix(&invocation, None, 4, now).is_err());

    let shell = command("bash", &["-lc", "cargo test"]);
    assert!(BoundedRuleCandidate::command_exact(&shell, None, now).is_err());
}

#[test]
fn bounded_rule_extraction_path_prefix_rejects_root_traversal_glob_and_protected_paths() {
    for invalid in ["", ".", "..", "/", "src/../secrets", "src/*", ".git/hooks"] {
        assert!(PathPrefix::parse(invalid).is_err(), "accepted {invalid:?}");
    }
    let prefix = PathPrefix::parse("src/generated").unwrap();
    let candidate = BoundedRuleCandidate::write_prefix(
        workspace(),
        Some("codex"),
        WriteOperationKind::Create,
        prefix,
        2,
        128,
        96,
        SystemTime::UNIX_EPOCH,
    )
    .unwrap();
    let TrustRule::Write(rule) = candidate.rule else { panic!("write rule") };
    assert_eq!(rule.path_prefix.display(), "src/generated");
    assert_eq!(rule.max_files, 2);
    assert_eq!(rule.max_total_bytes, 128);
    assert_eq!(rule.max_file_bytes, 96);
}

#[test]
fn bounded_rule_extraction_network_is_exact_host_and_read_only() {
    let candidate = BoundedRuleCandidate::network_exact_read(
        workspace(),
        None,
        NetworkScheme::Https,
        "api.example.com".into(),
        443,
        NetworkMethodClass::Read,
        BrowserActionClass::Fetch,
        SystemTime::UNIX_EPOCH,
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
        BoundedRuleCandidate::network_exact_read(
            workspace(),
            None,
            NetworkScheme::Https,
            "api.example.com".into(),
            443,
            NetworkMethodClass::Write,
            BrowserActionClass::Upload,
            SystemTime::UNIX_EPOCH,
        )
        .is_err()
    );
    assert!(
        BoundedRuleCandidate::network_exact_read(
            workspace(),
            None,
            NetworkScheme::Https,
            "*.example.com".into(),
            443,
            NetworkMethodClass::Read,
            BrowserActionClass::Fetch,
            SystemTime::UNIX_EPOCH,
        )
        .is_err()
    );
}

#[test]
fn bounded_rule_extraction_mcp_is_exact_across_arguments_schema_and_transport() {
    let invocation = McpInvocation {
        workspace: workspace(),
        agent: Some("codex".into()),
        transport: TransportKind::McpStdio,
        transport_identity: "stdio:ee --mcp-proxy".into(),
        server: "ee".into(),
        tool: "ee_format_file".into(),
        tool_schema_version: 3,
        category: TrustCategory::WriteModify,
        arguments_json: r#"{"path":"src/lib.rs"}"#.into(),
    };
    let candidate =
        BoundedRuleCandidate::mcp_exact(&invocation, Some("codex"), SystemTime::UNIX_EPOCH)
            .unwrap();
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
fn bounded_rule_extraction_preview_snapshot_matches_rule_authority() {
    let candidate = BoundedRuleCandidate::command_prefix(
        &command("cargo", &["test", "--package", "ee-cli"]),
        Some("codex"),
        2,
        SystemTime::UNIX_EPOCH,
    )
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
            "effect: allow\n",
            "workspace: sha256:ac3fcb8dccb255c1e3526224621e22241325bf67b24ebb69887891f0be85fb5a\n",
            "agent: codex\n",
            "kind: command\n",
            "executable: cargo\n",
            "arguments: token prefix · 2 tokens\n",
            "cwd scope: any canonical directory in workspace\n",
            "expires: 1970-01-01T01:00:00Z\n",
            "maximum uses: 20\n",
            "terminal output bytes: 1048576\n",
            "excludes: shell wrappers, environment, different executable"
        )
    );
    let TrustRule::Command(rule) = &candidate.rule else { panic!("command rule") };
    assert_eq!(rule.effect, TrustEffect::Allow);
    assert_eq!(rule.scope.workspace.as_string(), candidate.preview.workspace);
    assert_eq!(rule.scope.agent.as_deref(), Some(candidate.preview.agent.as_str()));
    assert_eq!(rule.scope.expires_at, Some(candidate.preview.expires_at));
    assert_eq!(rule.scope.max_uses, Some(candidate.preview.max_uses));
    assert_eq!(rule.argv.len(), 2);
}

#[test]
fn bounded_rule_extraction_every_authority_grant_has_expiry_and_budget() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(7);
    let invocation = command("cargo", &["check"]);
    let command = BoundedRuleCandidate::command_exact(&invocation, None, now).unwrap();
    let command_short = BoundedRuleCandidate::command_exact_short(&invocation, None, now).unwrap();
    assert_eq!(command_short.rule.scope().max_uses, Some(5));
    assert_eq!(command_short.rule.scope().expires_at, Some(now + Duration::from_secs(10 * 60)));
    let network = BoundedRuleCandidate::network_exact_read(
        workspace(),
        None,
        NetworkScheme::Https,
        "example.com".into(),
        443,
        NetworkMethodClass::Read,
        BrowserActionClass::Navigate,
        now,
    )
    .unwrap();
    let write = BoundedRuleCandidate::write_prefix(
        workspace(),
        None,
        WriteOperationKind::Modify,
        PathPrefix::parse("src").unwrap(),
        1,
        32,
        32,
        now,
    )
    .unwrap();
    for candidate in [command, command_short, network, write] {
        assert_eq!(candidate.rule.effect(), TrustEffect::Allow);
        assert!(candidate.rule.scope().expires_at.is_some());
        assert!(candidate.rule.scope().max_uses.is_some());
    }
}

#[cfg(feature = "agents")]
#[test]
fn bounded_rule_extraction_ui_requires_preview_then_explicit_confirmation() {
    use crate::app::{App, ApprovalChoice};
    use crate::policy::TrustStore;
    use crate::tests::agent_mcp::{base_agent_script, mcp_app};

    let (mut app, temp, _fake): (App, _, _) = mcp_app(base_agent_script(), false, true);
    let state = temp.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    app.agents.test_trust_store_base = Some(state.clone());
    let receiver = app.queue_terminal_approval_for_test(
        "bounded-preview",
        Some("codex"),
        "printf",
        &["%s", "ok"],
        &[],
        Some(temp.path().to_path_buf()),
    );

    app.confirm_bridge_approval_for_test(ApprovalChoice::AllowPersistent);
    let prompt = app.agents.approvals.front().expect("preview remains pending");
    let preview = prompt.allow_confirmation_preview().expect("bounded preview");
    assert_eq!(preview.max_uses, 20);
    assert!(preview.expires_at > app.trust_clock.now());
    assert!(TrustStore::at(&state, temp.path()).unwrap().load().unwrap().rules.is_empty());

    app.cancel_rule_confirmation_for_test();
    assert!(app.agents.approvals.front().unwrap().allow_confirmation_preview().is_none());
    assert!(TrustStore::at(&state, temp.path()).unwrap().load().unwrap().rules.is_empty());

    app.confirm_bridge_approval_for_test(ApprovalChoice::AllowPersistent);
    app.confirm_bridge_approval_for_test(ApprovalChoice::AllowPersistent);
    let document = TrustStore::at(&state, temp.path()).unwrap().load().unwrap();
    assert_eq!(document.rules.len(), 1);
    let rule = &document.rules[0];
    assert!(rule.scope().expires_at.is_some());
    assert_eq!(rule.scope().max_uses, Some(20));
    drop(receiver);
}
