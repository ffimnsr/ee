//! Phase 7 cross-transport security and compatibility matrix tests
//! (ISSUES.md "Unified Host-Local Workspace Trust Policy"): the
//! operation-category × policy-state matrix, the three-transport decision
//! matrix (direct ACP, stdio MCP proxy, ACP-native MCP-over-ACP), the
//! lifecycle matrix (cancellation, session close, store reload, repository
//! config), and compatibility regression guards (schema, discovery,
//! secret hygiene).
//!
//! Everything runs on temporary directories, fake agents, fake MCP
//! servers, injected clocks, and explicit task shutdown — no fixed sleeps
//! or retry loops, and no repository-controlled configuration ever grants
//! effective authority.

use std::time::SystemTime;

use crate::policy::evaluator::PolicyInput;
use crate::policy::rules::{MatchMode, TrustRule};
use crate::policy::session::SessionPolicy;
use crate::policy::store::TrustStore;
#[cfg(feature = "agents")]
use crate::policy::store::TrustStoreDocument;
use crate::policy::{
    CommandRule, DecisionReason, McpReadRule, McpRule, OperationIdentity, PathPrefix, ProfileRule,
    ReadPathRule, TrustCategory, TrustDecision, TrustOperation, TrustOutcome, TrustRuleScope,
    UsageSnapshot, WorkspaceIdentity, WriteOperationKind, WriteRule, evaluate,
};

fn identity(bytes: &[u8]) -> WorkspaceIdentity {
    WorkspaceIdentity::from_canonical_root_bytes(bytes)
}

fn at(text: &str) -> SystemTime {
    chrono::DateTime::parse_from_rfc3339(text)
        .expect("valid RFC3339")
        .with_timezone(&chrono::Utc)
        .into()
}

fn scope(
    workspace: WorkspaceIdentity,
    agent: Option<&str>,
    expires: Option<&str>,
    max_uses: Option<u64>,
) -> TrustRuleScope {
    TrustRuleScope {
        workspace,
        agent: agent.map(String::from),
        expires_at: expires.map(at),
        max_uses,
    }
}

/// Clone of `rule` with the agent scope overridden (matrix scope-mismatch
/// state).
fn rule_with_agent(rule: &TrustRule, agent: &str) -> TrustRule {
    let mut rule = rule.clone();
    rule.scope_mut().agent = Some(agent.into());
    rule
}

/// Clone of `rule` with the expiry overridden (matrix expired-rule state).
fn rule_with_expiry(rule: &TrustRule, expires: Option<&str>) -> TrustRule {
    let mut rule = rule.clone();
    rule.scope_mut().expires_at = expires.map(at);
    rule
}

/// Clone of `rule` with the use budget overridden (matrix exhausted-rule
/// state).
fn rule_with_max_uses(rule: &TrustRule, max_uses: Option<u64>) -> TrustRule {
    let mut rule = rule.clone();
    rule.scope_mut().max_uses = max_uses;
    rule
}

fn decide(
    op: &TrustOperation,
    rules: &[TrustRule],
    session: &SessionPolicy,
    workspace_enabled: bool,
    now: SystemTime,
    usage: &UsageSnapshot,
) -> TrustDecision {
    evaluate(&PolicyInput {
        session_id: "s1",
        fingerprint: "fp",
        operation: op,
        session,
        rules,
        now,
        usage,
        workspace_enabled,
        built_in_deny: None,
        tool_default: None,
        category_default: None,
        global_default: None,
    })
}

// ── Part 1: operation-category × policy-state matrix ─────────────────────────

/// One eligible category: a matching rule, the operation it authorizes, and
/// whether the workspace gate is required for that category.
struct CategoryFixture {
    label: &'static str,
    gate_required: bool,
    rule: TrustRule,
    operation: TrustOperation,
}

fn categories(ws: WorkspaceIdentity) -> Vec<CategoryFixture> {
    let command_rule = TrustRule::Command(CommandRule {
        id: "cmd_matrix".into(),
        effect: crate::policy::TrustEffect::Allow,
        scope: scope(ws, None, Some("2026-08-08T12:00:00Z"), Some(20)),
        executable: "git".into(),
        match_mode: MatchMode::ArgvExact,
        argv: vec!["status".into()],
    });
    let mcp_rule = TrustRule::Mcp(McpRule {
        id: "mcp_matrix".into(),
        effect: crate::policy::TrustEffect::Allow,
        scope: scope(ws, None, Some("2026-08-08T12:00:00Z"), Some(20)),
        server: "ee".into(),
        transport_identity: "stdio:matrix".into(),
        tool: "ee_apply_code_action".into(),
        tool_schema_version: 1,
        arguments_json: r#"{"action_id":"a1","path":"src/main.rs"}"#.into(),
    });
    let read_rule = TrustRule::ReadPath(ReadPathRule {
        id: "read_matrix".into(),
        effect: crate::policy::TrustEffect::Allow,
        scope: scope(ws, None, None, None),
        path_prefix: PathPrefix::parse("src").expect("prefix"),
        max_bytes: 1024,
    });
    let mcp_read_rule = TrustRule::McpRead(McpReadRule {
        id: "mcp_read_matrix".into(),
        effect: crate::policy::TrustEffect::Allow,
        scope: scope(ws, None, None, None),
        server: "ee".into(),
        transport_identity: "stdio:matrix".into(),
        tool: "ee_read_text_file".into(),
        tool_schema_version: 1,
        path_prefix: PathPrefix::parse("src").expect("prefix"),
        max_bytes: 1024,
    });
    let profile_rule = TrustRule::Profile(ProfileRule {
        id: "profile_matrix".into(),
        effect: crate::policy::TrustEffect::Allow,
        scope: scope(ws, None, Some("2026-08-08T12:00:00Z"), Some(20)),
        profile: "git_readonly".into(),
    });
    let write_rule = |id: &str, operation: WriteOperationKind| {
        TrustRule::Write(WriteRule {
            id: id.into(),
            effect: crate::policy::TrustEffect::Allow,
            scope: scope(ws, None, Some("2026-08-08T12:00:00Z"), Some(20)),
            operation,
            path_prefix: PathPrefix::parse("src/generated").expect("prefix"),
            max_files: 5,
            max_total_bytes: 65_536,
            max_file_bytes: 16_384,
        })
    };
    let write_op = |category: TrustCategory| TrustOperation {
        workspace: ws,
        agent: None,
        transport: crate::policy::TransportKind::Acp,
        category,
        identity: OperationIdentity::Write {
            relative_path: "src/generated/a.rs".into(),
            file_count: 1,
            total_bytes: Some(1024),
            max_file_bytes: Some(1024),
        },
    };

    vec![
        CategoryFixture {
            label: "terminal command",
            gate_required: false,
            rule: command_rule,
            operation: TrustOperation {
                workspace: ws,
                agent: None,
                transport: crate::policy::TransportKind::Acp,
                category: TrustCategory::Execute,
                identity: OperationIdentity::Command {
                    executable: "git".into(),
                    argv: vec!["status".into()],
                },
            },
        },
        CategoryFixture {
            label: "exact MCP invocation",
            gate_required: false,
            rule: mcp_rule,
            operation: TrustOperation {
                workspace: ws,
                agent: None,
                transport: crate::policy::TransportKind::McpStdio,
                category: TrustCategory::WriteModify,
                identity: OperationIdentity::Mcp {
                    server: "ee".into(),
                    transport_identity: "stdio:matrix".into(),
                    tool: "ee_apply_code_action".into(),
                    tool_schema_version: 1,
                    arguments_json: r#"{"action_id":"a1","path":"src/main.rs"}"#.into(),
                },
            },
        },
        CategoryFixture {
            label: "native read",
            gate_required: true,
            rule: read_rule,
            operation: TrustOperation {
                workspace: ws,
                agent: None,
                transport: crate::policy::TransportKind::Acp,
                category: TrustCategory::Read,
                identity: OperationIdentity::ReadPath {
                    relative_path: "src/main.rs".into(),
                    byte_count: Some(100),
                },
            },
        },
        CategoryFixture {
            label: "MCP read",
            gate_required: true,
            rule: mcp_read_rule,
            operation: TrustOperation {
                workspace: ws,
                agent: None,
                transport: crate::policy::TransportKind::McpStdio,
                category: TrustCategory::Read,
                identity: OperationIdentity::McpRead {
                    server: "ee".into(),
                    transport_identity: "stdio:matrix".into(),
                    tool: "ee_read_text_file".into(),
                    tool_schema_version: 1,
                    relative_path: "src/main.rs".into(),
                    byte_count: Some(100),
                },
            },
        },
        CategoryFixture {
            label: "curated profile",
            gate_required: true,
            rule: profile_rule,
            operation: TrustOperation {
                workspace: ws,
                agent: None,
                transport: crate::policy::TransportKind::Acp,
                category: TrustCategory::Execute,
                identity: OperationIdentity::Profile { profile: "git_readonly".into() },
            },
        },
        CategoryFixture {
            label: "create write",
            gate_required: false,
            rule: write_rule("write_create_matrix", WriteOperationKind::Create),
            operation: write_op(TrustCategory::WriteCreate),
        },
        CategoryFixture {
            label: "modify write",
            gate_required: false,
            rule: write_rule("write_modify_matrix", WriteOperationKind::Modify),
            operation: write_op(TrustCategory::WriteModify),
        },
    ]
}

#[test]
fn category_matrix_never_bypasses_approval() {
    let ws = identity(b"/work/root");
    let now = at("2026-08-07T12:00:00Z");
    let mut session_deny = SessionPolicy::default();
    session_deny.record("s1", "fp", crate::policy::SessionChoice::Deny);

    for fixture in categories(ws) {
        // Gate disabled: gate-required categories prompt, others allow.
        let gated = decide(
            &fixture.operation,
            std::slice::from_ref(&fixture.rule),
            &SessionPolicy::default(),
            false,
            now,
            &UsageSnapshot::default(),
        );
        if fixture.gate_required {
            assert_eq!(
                gated.reason,
                DecisionReason::WorkspaceDisabled,
                "{}: gate off",
                fixture.label
            );
            assert_eq!(gated.outcome, TrustOutcome::Confirm);
        } else {
            assert_eq!(
                gated.outcome,
                TrustOutcome::Allow,
                "{}: gate off still allows non-gated categories",
                fixture.label
            );
        }

        // Gate enabled without a rule: prompt.
        let bare = decide(
            &fixture.operation,
            &[],
            &SessionPolicy::default(),
            true,
            now,
            &UsageSnapshot::default(),
        );
        assert_eq!(bare.reason, DecisionReason::NoMatchingRule, "{}: no rule", fixture.label);

        // Matching rule: allow with the stable rule id.
        let allowed = decide(
            &fixture.operation,
            std::slice::from_ref(&fixture.rule),
            &SessionPolicy::default(),
            true,
            now,
            &UsageSnapshot::default(),
        );
        assert_eq!(allowed.outcome, TrustOutcome::Allow, "{}: matching rule allows", fixture.label);
        assert_eq!(
            allowed.rule_id.as_deref(),
            Some(fixture.rule.id()),
            "{}: rule id",
            fixture.label
        );

        // Session deny precedes every persistent allow.
        let denied = decide(
            &fixture.operation,
            std::slice::from_ref(&fixture.rule),
            &session_deny,
            true,
            now,
            &UsageSnapshot::default(),
        );
        assert_eq!(denied.reason, DecisionReason::SessionDeny, "{}: session deny", fixture.label);

        // Agent scope mismatch: the rule never matches another agent.
        let foreign_rule = rule_with_agent(&fixture.rule, "other_agent");
        let mismatched = decide(
            &fixture.operation,
            std::slice::from_ref(&foreign_rule),
            &SessionPolicy::default(),
            true,
            now,
            &UsageSnapshot::default(),
        );
        assert_eq!(
            mismatched.outcome,
            TrustOutcome::Confirm,
            "{}: agent scope mismatch",
            fixture.label
        );

        // Expired rule: prompt (expiry is checked before the matcher).
        let expired_rule = rule_with_expiry(&fixture.rule, Some("2026-08-06T12:00:00Z"));
        let expired = decide(
            &fixture.operation,
            std::slice::from_ref(&expired_rule),
            &SessionPolicy::default(),
            true,
            now,
            &UsageSnapshot::default(),
        );
        assert_eq!(expired.outcome, TrustOutcome::Confirm, "{}: expired rule", fixture.label);

        // Exhausted rule: prompt without mutating the usage state.  The
        // budget is keyed by this fixture's rule id.
        let exhausted_rule = rule_with_max_uses(&fixture.rule, Some(20));
        let exhausted =
            UsageSnapshot::new([(fixture.rule.id().to_string(), 20u64)].into_iter().collect());
        let spent = decide(
            &fixture.operation,
            std::slice::from_ref(&exhausted_rule),
            &SessionPolicy::default(),
            true,
            now,
            &exhausted,
        );
        assert_eq!(spent.outcome, TrustOutcome::Confirm, "{}: exhausted rule", fixture.label);
    }
}

#[test]
fn prompt_only_operations_never_authorize() {
    let ws = identity(b"/work/root");
    let now = at("2026-08-07T12:00:00Z");
    // Delete, rename, VCS mutation, package mutation, network mutation,
    // secret access, external paths, and unknown tools normalize to an
    // unknown identity: no session or persistent state can ever allow them.
    let cases: Vec<(&str, TrustCategory, OperationIdentity)> = vec![
        ("delete", TrustCategory::WriteModify, OperationIdentity::Unknown),
        ("rename", TrustCategory::WriteModify, OperationIdentity::Unknown),
        ("vcs mutation", TrustCategory::Execute, OperationIdentity::Unknown),
        ("package mutation", TrustCategory::Execute, OperationIdentity::Unknown),
        ("network mutation", TrustCategory::Execute, OperationIdentity::Unknown),
        ("secret access", TrustCategory::Read, OperationIdentity::Unknown),
        ("external path", TrustCategory::WriteCreate, OperationIdentity::Unknown),
        ("unknown tool", TrustCategory::Unknown, OperationIdentity::Unknown),
    ];
    // Unknown operations prompt in every policy state.  A recorded session
    // allow is the only in-memory resolution (never persistent authority),
    // so the default-session matrix is the no-bypass guarantee.
    for (label, category, identity) in cases {
        let op = TrustOperation {
            workspace: ws,
            agent: None,
            transport: crate::policy::TransportKind::Acp,
            category,
            identity,
        };
        let decision =
            decide(&op, &[], &SessionPolicy::default(), true, now, &UsageSnapshot::default());
        assert_eq!(decision.reason, DecisionReason::UnknownOperation, "{label} must prompt");
        assert_eq!(decision.outcome, TrustOutcome::Confirm);
    }
}

#[test]
fn malformed_and_cross_workspace_stores_yield_no_effective_authority() {
    let base = tempfile::TempDir::new().expect("state dir");
    let workspace = tempfile::TempDir::new().expect("workspace root");
    let other = tempfile::TempDir::new().expect("other workspace");
    let store = TrustStore::at(base.path(), workspace.path()).expect("store");
    let ws = *store.workspace();
    let other_ws =
        WorkspaceIdentity::from_canonical_root_bytes(other.path().as_os_str().as_encoded_bytes());
    let now = at("2026-08-07T12:00:00Z");
    let op = TrustOperation {
        workspace: ws,
        agent: None,
        transport: crate::policy::TransportKind::Acp,
        category: TrustCategory::Execute,
        identity: OperationIdentity::Command {
            executable: "git".into(),
            argv: vec!["status".into()],
        },
    };

    // Malformed store document: effective() is empty and everything prompts.
    write_store_text(store.path(), "not a trust document [[[");
    assert!(store.load_at(now).is_err(), "malformed store must fail strict load");
    assert!(store.effective_at(now).rules.is_empty(), "fail closed on malformed store");
    assert_eq!(
        decide(
            &op,
            &store.effective_at(now).rules,
            &SessionPolicy::default(),
            true,
            now,
            &UsageSnapshot::default()
        )
        .outcome,
        TrustOutcome::Confirm
    );

    // Identity mismatch: a document claiming another workspace loads nothing.
    write_store_text(store.path(), &document_for(other_ws));
    assert!(matches!(
        store.load_at(now),
        Err(crate::policy::store::TrustStoreError::IdentityMismatch)
    ));
    assert!(store.effective_at(now).rules.is_empty());

    // Cross-workspace operation: rules bound to ws never match operations
    // for another workspace, even when both documents are valid.
    let valid = store_document(&ws, command_rule_for(ws, "cmd_ws", 20));
    write_store_text(store.path(), &valid);
    let cross = decide(
        &TrustOperation { workspace: other_ws, ..op.clone() },
        &store.effective_at(now).rules,
        &SessionPolicy::default(),
        true,
        now,
        &UsageSnapshot::default(),
    );
    assert_eq!(cross.outcome, TrustOutcome::Confirm, "cross-workspace rule must not match");
}

fn command_rule_for(ws: WorkspaceIdentity, id: &str, max_uses: u64) -> TrustRule {
    TrustRule::Command(CommandRule {
        id: id.to_string(),
        effect: crate::policy::TrustEffect::Allow,
        scope: TrustRuleScope {
            workspace: ws,
            agent: None,
            expires_at: Some(at("2026-08-08T12:00:00Z")),
            max_uses: Some(max_uses),
        },
        executable: "git".into(),
        match_mode: MatchMode::ArgvExact,
        argv: vec!["status".into()],
    })
}

fn store_document(ws: &WorkspaceIdentity, rule: TrustRule) -> String {
    format!(
        r#"
schema_version = 1

[workspace]
identity = "{identity}"

[policy]
workspace_enabled = true

[[command_allow]]
id = "{id}"
agent = {agent}
executable = "git"
match = "argv_exact"
argv = ["status"]
expires_at = "2026-08-08T12:00:00Z"
max_uses = {max_uses}
"#,
        identity = ws.as_string(),
        id = rule.id(),
        agent = rule
            .scope()
            .agent
            .as_deref()
            .map(|a| format!("\"{a}\""))
            .unwrap_or_else(|| "null".into()),
        max_uses = rule.scope().max_uses.unwrap_or(20),
    )
}

fn document_for(ws: WorkspaceIdentity) -> String {
    format!(
        r#"
schema_version = 1

[workspace]
identity = "{identity}"

[policy]
workspace_enabled = true
"#,
        identity = ws.as_string()
    )
}

fn write_store_text(path: &std::path::Path, text: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
        set_owner_only_dir(parent);
    }
    std::fs::write(path, text).unwrap();
    set_owner_only(path);
}

#[cfg(unix)]
fn set_owner_only(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[cfg(unix)]
fn set_owner_only_dir(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(not(unix))]
fn set_owner_only(_path: &std::path::Path) {}

#[cfg(not(unix))]
fn set_owner_only_dir(_path: &std::path::Path) {}

// ── Part 4 (unit): repository config and schema compatibility ────────────────

#[test]
fn ee_toml_rejects_trust_looking_fields() {
    for toml in [
        "[policy]\nworkspace_enabled = true\n",
        "[[command_allow]]\nid = \"cmd_1\"\nexecutable = \"git\"\n",
        "[trust]\nread = true\n",
        "[[write_allow]]\nid = \"w\"\noperation = \"create\"\n",
    ] {
        let err = toml::from_str::<crate::config::EeToml>(toml).unwrap_err();
        assert!(
            err.to_string().contains("unknown field"),
            "trust-looking ee.toml field must be rejected: {toml:?} → {err}"
        );
    }
}

#[test]
fn generated_config_schema_has_no_authority_granting_fields() {
    let schema: serde_json::Value =
        serde_json::from_str(&crate::config::config_schema_json().unwrap()).unwrap();
    let mut keys = Vec::new();
    collect_keys(&schema, &mut keys);
    for forbidden in [
        "command_allow",
        "mcp_allow",
        "read_path_allow",
        "mcp_read_allow",
        "mcp_read_profile_allow",
        "profile_allow",
        "write_allow",
        "workspace_enabled",
        "trust",
        "expires_at",
        "max_uses",
    ] {
        assert!(
            !keys.iter().any(|key| key == forbidden),
            "config schema must not contain {forbidden:?}: {keys:?}"
        );
    }
}

fn collect_keys(value: &serde_json::Value, keys: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                keys.push(key.clone());
                collect_keys(value, keys);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_keys(item, keys);
            }
        }
        _ => {}
    }
}

#[test]
fn host_local_trust_store_is_not_repository_config() {
    let base = tempfile::TempDir::new().expect("state dir");
    let workspace = tempfile::TempDir::new().expect("workspace root");
    let store = TrustStore::at(base.path(), workspace.path()).expect("store");
    let path = store.path();
    assert!(
        path.starts_with(base.path().join("trust")),
        "store lives under the state directory: {}",
        path.display()
    );
    assert!(
        !path.starts_with(workspace.path()),
        "store must never live inside the repository: {}",
        path.display()
    );
    // The checked-in project schema covers every ee.toml field; the store
    // document schema is a separate versioned contract with its own
    // `schema_version`, never part of repository configuration.
    assert_eq!(crate::policy::store::TRUST_SCHEMA_VERSION, 2);
    let text = std::fs::read_to_string(path).unwrap_or_default();
    assert!(text.is_empty() || text.contains("schema_version"), "store is versioned TOML");
}

// ── End-to-end matrix (agents feature) ───────────────────────────────────────

#[cfg(feature = "agents")]
mod e2e {
    use super::*;
    use crate::app::App;
    use crate::tests::agent_bridge::base_script;
    use crate::tests::agent_mcp::{
        acp_connect_script, base_agent_script, connect_proxy, fake_response, mcp_app, mcp_app_in,
        open_pane_and_wait_ready, press, proxy_recv, proxy_send, settle, wait_until,
    };
    use crossterm::event::{KeyCode, KeyModifiers};
    use serde_json::json;
    use std::fs;
    use std::path::Path;

    fn seed_command_rule(state_dir: &Path, workspace: &Path, id: &str) {
        let store = TrustStore::at(state_dir, workspace).unwrap();
        let rule = command_rule_for(*store.workspace(), id, 20);
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

    fn ledger_workspace(state_dir: &Path, workspace: &Path) -> WorkspaceIdentity {
        *TrustStore::at(state_dir, workspace).unwrap().workspace()
    }

    // ── Part 2: transport matrix ─────────────────────────────────────────────

    #[test]
    fn terminal_command_decision_is_identical_across_transports() {
        // Direct ACP: the agent asks through the native session bridge.
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let frame = json!({
            "jsonrpc": "2.0",
            "id": 102,
            "method": "terminal/create",
            "params": { "sessionId": "s1", "command": "git", "args": ["status"] }
        });
        let script = base_script().emit(frame);
        let (mut acp_app, _fake) = crate::tests::agent_bridge::agents_app_in(&temp, script);
        acp_app.agents.test_trust_store_base = Some(state_dir.clone());
        seed_command_rule(&state_dir, temp.path(), "cmd_transport");
        open_pane_and_wait_ready(&mut acp_app);
        wait_until(&mut acp_app, "direct ACP terminal spawned", |app| {
            app.agents.terminals.tracked_count() == 1 && app.agents.approvals.is_empty()
        });
        assert_eq!(
            acp_app.agents.usage_ledger.used(
                ledger_workspace(&state_dir, temp.path()),
                "s1",
                "cmd_transport"
            ),
            1,
            "direct ACP consumes the same rule"
        );

        // stdio MCP proxy: the same command through the proxy listener.
        let (mut proxy_app, proxy_temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut proxy_app);
        let proxy_state = proxy_temp.path().join("state");
        fs::create_dir_all(&proxy_state).unwrap();
        proxy_app.agents.test_trust_store_base = Some(proxy_state.clone());
        seed_command_rule(&proxy_state, proxy_temp.path(), "cmd_transport");
        let mut stream = connect_proxy(&proxy_app);
        proxy_send(
            &mut stream,
            1,
            json!({ "method": "terminal_create", "command": "git", "args": ["status"] }),
        );
        wait_until(&mut proxy_app, "stdio proxy terminal spawned", |app| {
            app.agents.terminals.tracked_count() == 1 && app.agents.approvals.is_empty()
        });
        assert_eq!(
            proxy_app.agents.usage_ledger.used(
                ledger_workspace(&proxy_state, proxy_temp.path()),
                "proxy",
                "cmd_transport"
            ),
            1,
            "stdio proxy consumes the same rule"
        );
        let _ = proxy_recv(&mut stream);

        // ACP-native MCP-over-ACP: the same command as an MCP tool call.
        let acp_temp = tempfile::tempdir().unwrap();
        let acp_state = acp_temp.path().join("state");
        fs::create_dir_all(&acp_state).unwrap();
        let script = acp_connect_script(json!({
            "name": "ee_terminal_create",
            "arguments": { "command": "git", "args": ["status"] }
        }));
        let (mut native_app, fake) = mcp_app_in(&acp_temp, script, false, true);
        native_app.agents.test_trust_store_base = Some(acp_state.clone());
        seed_command_rule(&acp_state, acp_temp.path(), "cmd_transport");
        open_pane_and_wait_ready(&mut native_app);
        wait_until(&mut native_app, "ACP-native terminal spawned", |app| {
            app.agents.terminals.tracked_count() == 1 && app.agents.approvals.is_empty()
        });
        let reply = fake_response(&fake.agent(), 202);
        assert_eq!(
            reply["result"]["isError"],
            json!(false),
            "ACP-native tool call auto-allowed: {reply}"
        );
        assert_eq!(
            native_app.agents.usage_ledger.used(
                ledger_workspace(&acp_state, acp_temp.path()),
                "proxy",
                "cmd_transport"
            ),
            1,
            "ACP-native consumes the same rule"
        );
        native_app.shutdown_agents();
        acp_app.shutdown_agents();
        proxy_app.shutdown_agents();
    }

    // ── Part 3: lifecycle matrix ─────────────────────────────────────────────

    #[test]
    fn connection_close_before_approval_dispatches_nothing() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        // A seed rule for `git status` proves the store is untouched by the
        // abandoned request for `git commit`.
        seed_command_rule(&state_dir, temp.path(), "cmd_seed");

        let mut stream = connect_proxy(&app);
        proxy_send(
            &mut stream,
            1,
            json!({ "method": "terminal_create", "command": "git", "args": ["commit", "-m", "x"] }),
        );
        wait_until(&mut app, "approval queued", |app| !app.agents.approvals.is_empty());
        // Connection closes before any resolution.
        drop(stream);
        settle(&mut app);

        assert_eq!(app.agents.terminals.tracked_count(), 0, "no dispatch on connection close");
        assert!(app.agents.usage_ledger.is_empty(), "no usage consumed by the abandoned request");
        let store = TrustStore::at(&state_dir, temp.path()).unwrap();
        let document = store.load().unwrap();
        assert_eq!(document.rules.len(), 1, "no rule written by the abandoned request");
        assert_eq!(document.rules[0].id(), "cmd_seed");
    }

    #[test]
    fn session_close_clears_session_state_but_persistent_rules_remain() {
        // A persistent rule grants a write through the bridge session; a
        // session-scoped allow and a usage row are then recorded directly,
        // and session close must drop both while the host-local rule stays
        // stored and scope-checked.
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("src/generated")).unwrap();
        let covered = temp.path().join("src/generated/covered.txt");
        fs::write(&covered, "v0").unwrap();
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let store = TrustStore::at(&state_dir, temp.path()).unwrap();
        let ws = *store.workspace();
        let rule = TrustRule::Write(WriteRule {
            id: "write_persist".into(),
            effect: crate::policy::TrustEffect::Allow,
            scope: TrustRuleScope {
                workspace: ws,
                agent: None,
                expires_at: Some(at("2026-08-08T12:00:00Z")),
                max_uses: Some(5),
            },
            operation: WriteOperationKind::Modify,
            path_prefix: PathPrefix::parse("src/generated").expect("prefix"),
            max_files: 1,
            max_total_bytes: 65_536,
            max_file_bytes: 16_384,
        });
        store
            .write(&TrustStoreDocument {
                workspace: ws,
                workspace_enabled: true,
                tool_defaults: Vec::new(),
                category_defaults: Vec::new(),
                global_default: crate::policy::FallbackEffect::Confirm,
                rules: vec![rule],
            })
            .expect("seed store");

        let script = base_script().emit(crate::tests::agent_bridge::write_text_file(
            103,
            "s1",
            &covered.display().to_string(),
            "persisted",
        ));
        let (mut app, fake) = crate::tests::agent_bridge::agents_app_in(&temp, script);
        app.agents.test_trust_store_base = Some(state_dir.clone());
        open_pane_and_wait_ready(&mut app);
        wait_until(&mut app, "covered write auto-allowed and usage recorded", |app| {
            fake.agent().response_with_id(103).is_some() && !app.agents.usage_ledger.is_empty()
        });

        // Session-scoped state: an allow-session decision and an extra usage
        // row for the same session.
        let fingerprint = format!("write:{}", covered.display());
        app.agents.approval_policy.record("s1", &fingerprint, crate::policy::SessionChoice::Allow);
        assert!(app.agents.approval_policy.is_allowed("s1", &fingerprint));
        app.agents.usage_ledger.record_use(
            ledger_workspace(&state_dir, temp.path()),
            "s1",
            "write_persist",
        );

        app.shutdown_agents();
        assert!(app.agents.approval_policy.is_empty(), "session decisions die with the session");
        assert!(app.agents.usage_ledger.is_empty(), "runtime budgets die with the session");

        // The persistent rule remains stored and scope-checked.
        let reloaded = TrustStore::at(&state_dir, temp.path()).unwrap().load().unwrap();
        assert_eq!(reloaded.rules.len(), 1, "persistent rule survives the session");
        assert_eq!(reloaded.rules[0].scope().max_uses, Some(5));
        let now = at("2026-08-07T12:00:00Z");
        assert!(
            reloaded.rules[0].scope().expires_at.is_some_and(|expires| expires > now),
            "rule still within its window"
        );
        let op = TrustOperation {
            workspace: ws,
            agent: None,
            transport: crate::policy::TransportKind::Acp,
            category: TrustCategory::WriteModify,
            identity: OperationIdentity::Write {
                relative_path: "src/generated/covered.txt".into(),
                file_count: 1,
                total_bytes: Some(1024),
                max_file_bytes: Some(1024),
            },
        };
        let decision = decide(
            &op,
            &reloaded.rules,
            &SessionPolicy::default(),
            true,
            now,
            &UsageSnapshot::default(),
        );
        assert_eq!(
            decision.outcome,
            TrustOutcome::Allow,
            "persistent rule stays scope-checked and effective"
        );
    }

    #[test]
    fn store_reload_applies_valid_changes_and_fails_closed_on_corruption() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        seed_command_rule(&state_dir, temp.path(), "cmd_status");
        app.reload_workspace_trust_store().expect("reload seeded command rule");

        let mut stream = connect_proxy(&app);
        proxy_send(
            &mut stream,
            1,
            json!({ "method": "terminal_create", "command": "git", "args": ["status"] }),
        );
        wait_until(&mut app, "seeded rule auto-allows", |app| {
            app.agents.terminals.tracked_count() == 1 && app.agents.approvals.is_empty()
        });
        let _ = proxy_recv(&mut stream);

        // External host-local changes remain inactive until explicit reload.
        let store = TrustStore::at(&state_dir, temp.path()).unwrap();
        let mut document = store.load().unwrap();
        document.rules.push(TrustRule::Command(CommandRule {
            id: "cmd_diff".into(),
            effect: crate::policy::TrustEffect::Allow,
            scope: TrustRuleScope {
                workspace: *store.workspace(),
                agent: None,
                expires_at: Some(at("2026-08-08T12:00:00Z")),
                max_uses: Some(20),
            },
            executable: "git".into(),
            match_mode: MatchMode::ArgvExact,
            argv: vec!["diff".into()],
        }));
        store.write(&document).expect("apply valid host-local change");
        proxy_send(
            &mut stream,
            2,
            json!({ "method": "terminal_create", "command": "git", "args": ["diff"] }),
        );
        wait_until(&mut app, "external rule remains inactive", |app| {
            !app.agents.approvals.is_empty()
        });
        assert_eq!(app.agents.terminals.tracked_count(), 1, "no dispatch before reload");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE); // Deny
        let reply = proxy_recv(&mut stream);
        assert!(reply["result"]["error"].is_object(), "denied: {reply}");

        app.reload_workspace_trust_store().expect("reload valid external change");
        proxy_send(
            &mut stream,
            3,
            json!({ "method": "terminal_create", "command": "git", "args": ["diff"] }),
        );
        wait_until(&mut app, "reloaded rule auto-allows", |app| {
            app.agents.terminals.tracked_count() == 2 && app.agents.approvals.is_empty()
        });
        let _ = proxy_recv(&mut stream);

        // Corrupt reload fails closed without replacing known-good policy.
        write_store_text(store.path(), "broken [[[[ not toml");
        assert!(app.reload_workspace_trust_store().is_err(), "corrupt reload must fail");
        proxy_send(
            &mut stream,
            4,
            json!({ "method": "terminal_create", "command": "git", "args": ["status"] }),
        );
        wait_until(&mut app, "known-good policy survives corrupt reload", |app| {
            app.agents.terminals.tracked_count() == 3 && app.agents.approvals.is_empty()
        });
        let _ = proxy_recv(&mut stream);

        // Restoring and explicitly reloading keeps valid authority active.
        store.write(&document).expect("restore store");
        app.reload_workspace_trust_store().expect("reload restored store");
        proxy_send(
            &mut stream,
            5,
            json!({ "method": "terminal_create", "command": "git", "args": ["diff"] }),
        );
        wait_until(&mut app, "restored store auto-allows", |app| {
            app.agents.terminals.tracked_count() == 4 && app.agents.approvals.is_empty()
        });
        let _ = proxy_recv(&mut stream);
    }

    #[test]
    fn repository_config_trust_fields_never_grant_authority() {
        // A repository ee.toml carrying trust-looking fields is rejected by
        // the strict config parser; agents mode never activates and the
        // host-local trust store stays empty.
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join(".ee.toml"),
            "[agents]\nenabled = true\n\n[agents.servers.fake]\ncommand = \"unused\"\n\n[policy]\nworkspace_enabled = true\n\n[[command_allow]]\nid = \"cmd_repo\"\nexecutable = \"git\"\nmatch = \"argv_exact\"\nargv = [\"status\"]\nexpires_at = \"2099-01-01T00:00:00Z\"\nmax_uses = 20\n",
        )
        .unwrap();
        let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
        let _cwd_restore = crate::tests::helpers::CurrentDirGuard::capture();
        std::env::set_current_dir(temp.path()).unwrap();
        let mut app = App::from_path(None).unwrap();
        drop(_cwd_restore);
        drop(_cwd_lock);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());

        // The pane reports agents unavailable (config rejected), so no
        // operation can even reach the policy engine.
        assert!(!app.config.agents.enabled, "config with trust fields must not enable agents");
        assert_eq!(
            TrustStore::at(&state_dir, temp.path()).unwrap().load().unwrap().rules.len(),
            0,
            "repository config never writes host-local grants"
        );
    }

    #[test]
    fn fixture_secrets_never_reach_store_or_diagnostics() {
        // A trusted write carries a secret-like fixture value; the store
        // document, the generated schema, and the audit entries must never
        // contain it.
        let secret = "FIXTURE_API_TOKEN=super-secret-7";
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("src/generated")).unwrap();
        // The fixture filename must not itself be secret-like; the secret
        // lives in the content.
        let target = temp.path().join("src/generated/fixture.rs");
        fs::write(&target, "v0").unwrap();
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let store = TrustStore::at(&state_dir, temp.path()).unwrap();
        let ws = *store.workspace();
        let rule = TrustRule::Write(WriteRule {
            id: "write_secret".into(),
            effect: crate::policy::TrustEffect::Allow,
            scope: TrustRuleScope {
                workspace: ws,
                agent: None,
                expires_at: Some(at("2026-08-08T12:00:00Z")),
                max_uses: Some(5),
            },
            operation: WriteOperationKind::Modify,
            path_prefix: PathPrefix::parse("src/generated").expect("prefix"),
            max_files: 1,
            max_total_bytes: 65_536,
            max_file_bytes: 16_384,
        });
        store
            .write(&TrustStoreDocument {
                workspace: ws,
                workspace_enabled: true,
                tool_defaults: Vec::new(),
                category_defaults: Vec::new(),
                global_default: crate::policy::FallbackEffect::Confirm,
                rules: vec![rule],
            })
            .expect("seed store");

        let script = base_script().emit(crate::tests::agent_bridge::write_text_file(
            103,
            "s1",
            &target.display().to_string(),
            secret,
        ));
        let (mut app, fake) = crate::tests::agent_bridge::agents_app_in(&temp, script);
        app.agents.test_trust_store_base = Some(state_dir.clone());
        open_pane_and_wait_ready(&mut app);
        wait_until(&mut app, "secret-bearing write completed and audited", |app| {
            fake.agent().response_with_id(103).is_some()
                && app
                    .agents_action_log()
                    .iter()
                    .any(|entry| matches!(entry, crate::app::ActionLogEntry::TrustDecision { .. }))
        });
        assert!(fs::read_to_string(&target).unwrap().contains("FIXTURE_API_TOKEN"));

        let store_text = fs::read_to_string(store.path()).unwrap();
        assert!(!store_text.contains("super-secret-7"), "secret leaked into the store");
        let schema = crate::config::config_schema_json().unwrap();
        assert!(!schema.contains("super-secret-7"), "secret leaked into the schema");
        let rendered = format!("{:?}", app.agents_action_log());
        assert!(!rendered.contains("super-secret-7"), "secret leaked into audit entries");
        // The thread transcript (status surface) shows the trust notice and
        // the pre-existing write notice, but never the value.
        let thread = app.agents.threads.iter().find(|thread| thread.session_id == "s1").unwrap();
        assert!(
            !format!("{:?}", thread.system_notices()).contains("super-secret-7"),
            "secret leaked into the transcript"
        );
    }
}
