//! Policy-rules tests: extraction and evaluation.
use super::*;

#[test]
fn persistent_deny_command_allows_shell_wrapper_but_keeps_token_validation() {
    let raw = RawCommandRule {
        id: "deny-shell".into(),
        effect: TrustEffect::Deny,
        agent: None,
        executable: "sh".into(),
        match_mode: MatchMode::ArgvPrefix,
        argv: vec!["-c".into()],
        expires_at: None,
        max_uses: None,
    };
    let rule = TrustRule::Command(CommandRule::from_raw(raw.clone(), workspace()).unwrap());
    assert!(rule.matches(&operation(
        TrustCategory::Execute,
        OperationIdentity::Command {
            executable: "sh".into(),
            argv: vec!["-c".into(), "echo safe".into()],
        },
    )));
    assert!(
        CommandRule::from_raw(RawCommandRule { effect: TrustEffect::Allow, ..raw }, workspace())
            .is_err()
    );
}

#[test]
fn persistent_deny_read_ignores_allow_only_ceiling() {
    let rule = TrustRule::ReadPath(
        ReadPathRule::from_raw(
            RawReadPathRule {
                id: "deny-read".into(),
                effect: TrustEffect::Deny,
                agent: None,
                path_prefix: "target/cache".into(),
                max_bytes: u64::MAX,
                expires_at: None,
                max_uses: None,
            },
            workspace(),
        )
        .unwrap(),
    );
    assert!(rule.matches(&operation(
        TrustCategory::Read,
        OperationIdentity::ReadPath {
            relative_path: "target/cache/blob".into(),
            byte_count: Some(u64::MAX),
        },
    )));
}

#[test]
fn persistent_deny_mcp_category_does_not_require_arguments() {
    let rule = TrustRule::from_raw_mcp(
        RawMcpRule {
            id: "deny-mcp".into(),
            effect: TrustEffect::Deny,
            agent: None,
            server: "ee".into(),
            transport_identity: "stdio:ee".into(),
            tool: "ee_apply_patch".into(),
            tool_schema_version: 2,
            category: Some(TrustCategory::WriteModify),
            arguments_json: None,
            expires_at: None,
            max_uses: None,
        },
        workspace(),
    )
    .unwrap();
    let identity = OperationIdentity::Mcp {
        server: "ee".into(),
        transport_identity: "stdio:ee".into(),
        tool: "ee_apply_patch".into(),
        tool_schema_version: 2,
        arguments_json: "{\"path\":\"secret\"}".into(),
    };
    assert!(rule.matches(&operation(TrustCategory::WriteModify, identity.clone())));
    assert!(!rule.matches(&operation(TrustCategory::Read, identity)));
}

#[test]
fn persistent_deny_filesystem_matches_source_or_destination_boundary() {
    let rule = FilesystemRule::from_raw(
        RawFilesystemRule {
            id: "deny-fs".into(),
            effect: TrustEffect::Deny,
            agent: None,
            operations: vec![FilesystemOperationKind::Rename],
            path_prefix: "deploy/prod".into(),
            expires_at: None,
            max_uses: None,
        },
        workspace(),
    )
    .unwrap()
    .into_trust_rule();
    let identity = OperationIdentity::filesystem(
        FilesystemOperationKind::Rename,
        Some("tmp/file"),
        Some("deploy/prod/file"),
    )
    .unwrap();
    assert!(rule.matches(&operation(TrustCategory::WriteModify, identity)));
    let outside = OperationIdentity::filesystem(
        FilesystemOperationKind::Rename,
        Some("tmp/file"),
        Some("deploy/production/file"),
    )
    .unwrap();
    assert!(!rule.matches(&operation(TrustCategory::WriteModify, outside)));
}

#[test]
fn bounded_rule_extraction_rejects_broad_or_writing_network_allow_from_schema() {
    for (host_match, method, action) in [
        (HostMatchMode::Suffix, NetworkMethodClass::Read, BrowserActionClass::Fetch),
        (HostMatchMode::Exact, NetworkMethodClass::Write, BrowserActionClass::Upload),
    ] {
        let result = NetworkRule::from_raw(
            RawNetworkRule {
                id: "allow-net".into(),
                effect: TrustEffect::Allow,
                agent: None,
                scheme: NetworkScheme::Https,
                host: "api.example.com".into(),
                host_match,
                port: 443,
                method,
                browser_action: action,
                expires_at: Some("1970-01-01T01:00:00Z".into()),
                max_uses: Some(1),
            },
            workspace(),
        );
        assert!(result.is_err());
    }
}

#[test]
fn persistent_deny_network_suffix_uses_dns_label_boundary() {
    let rule = TrustRule::Network(
        NetworkRule::from_raw(
            RawNetworkRule {
                id: "deny-net".into(),
                effect: TrustEffect::Deny,
                agent: None,
                scheme: NetworkScheme::Https,
                host: "Example.COM.".into(),
                host_match: HostMatchMode::Suffix,
                port: 443,
                method: NetworkMethodClass::Write,
                browser_action: BrowserActionClass::Upload,
                expires_at: None,
                max_uses: None,
            },
            workspace(),
        )
        .unwrap(),
    );
    let matching = OperationIdentity::network(
        NetworkScheme::Https,
        "api.example.com",
        443,
        NetworkMethodClass::Write,
        BrowserActionClass::Upload,
    )
    .unwrap();
    let boundary_miss = OperationIdentity::network(
        NetworkScheme::Https,
        "notexample.com",
        443,
        NetworkMethodClass::Write,
        BrowserActionClass::Upload,
    )
    .unwrap();
    assert!(rule.matches(&operation(TrustCategory::Network, matching)));
    assert!(!rule.matches(&operation(TrustCategory::Network, boundary_miss)));
    assert!(normalize_host("127.0.0.1", HostMatchMode::Suffix).is_err());
}

#[test]
fn persistent_deny_tool_rejects_mixed_identity_and_unsafe_ids() {
    let raw = RawToolRule {
        id: "deny-tool".into(),
        effect: TrustEffect::Deny,
        agent: None,
        native_tool: Some("terminal".into()),
        server: Some("ee".into()),
        transport_identity: None,
        tool: None,
        tool_schema_version: None,
        category: None,
        expires_at: None,
        max_uses: None,
    };
    assert!(ToolRule::from_raw(raw, workspace()).is_err());
    assert!(validate_rule_id("contains space").is_err());
    assert!(validate_rule_id(&"a".repeat(81)).is_err());
    assert!(validate_rule_id("deny_tool-1").is_ok());
}
