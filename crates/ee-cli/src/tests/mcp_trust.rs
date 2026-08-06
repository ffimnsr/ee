//! Phase 3 MCP trust tests (ISSUES.md "Unified Host-Local Workspace Trust
//! Policy"): exact generic MCP invocation trust.
//!
//! Covers the exact-invocation matcher, canonical argument handling, the
//! ee-pinned manifest classification boundary, and the stdio proxy and
//! ACP-native approval integration.  End-to-end tests run under the `agents`
//! feature and drive `ee_apply_code_action` invocations through both routes.

#[cfg(feature = "agents")]
use std::fs;
#[cfg(feature = "agents")]
use std::path::Path;
#[cfg(feature = "agents")]
use std::time::Duration;
use std::time::SystemTime;

use crate::policy::rules::TrustRule;
use crate::policy::session::SessionPolicy;
#[cfg(feature = "agents")]
use crate::policy::store::TrustStore;
use crate::policy::{
    McpInvocation, OperationIdentity, TrustCategory, TrustOperation, TrustOutcome, TrustRuleScope,
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

fn mcp_op(
    workspace: WorkspaceIdentity,
    agent: Option<&str>,
    transport_identity: &str,
    tool: &str,
    arguments_json: &str,
) -> TrustOperation {
    TrustOperation {
        workspace,
        agent: agent.map(String::from),
        transport: crate::policy::TransportKind::McpStdio,
        category: TrustCategory::WriteModify,
        identity: OperationIdentity::Mcp {
            server: "ee".to_string(),
            transport_identity: transport_identity.to_string(),
            tool: tool.to_string(),
            tool_schema_version: 1,
            arguments_json: arguments_json.to_string(),
        },
    }
}

fn mcp_rule(
    id: &str,
    workspace: WorkspaceIdentity,
    transport_identity: &str,
    tool: &str,
    arguments_json: &str,
) -> TrustRule {
    TrustRule::Mcp(crate::policy::McpRule {
        id: id.to_string(),
        scope: TrustRuleScope {
            workspace,
            agent: None,
            expires_at: Some(at("2026-08-08T12:00:00Z")),
            max_uses: Some(20),
        },
        server: "ee".to_string(),
        transport_identity: transport_identity.to_string(),
        tool: tool.to_string(),
        tool_schema_version: 1,
        arguments_json: arguments_json.to_string(),
    })
}

fn decide(op: &TrustOperation, rules: &[TrustRule]) -> crate::policy::TrustDecision {
    crate::policy::evaluate(&crate::policy::PolicyInput {
        session_id: "s1",
        fingerprint: "fp",
        operation: op,
        session: &SessionPolicy::default(),
        rules,
        now: at("2026-08-07T12:00:00Z"),
        usage: &UsageSnapshot::default(),
        workspace_enabled: true,
    })
}

// ── Exact-invocation matcher ─────────────────────────────────────────────────

#[test]
fn mcp_rule_matches_every_identity_field_exactly() {
    let ws = identity(b"/work/root");
    let args = r#"{"action_id":"act_1","path":"/work/root/src/main.rs"}"#;
    let rule = mcp_rule("mcp_1", ws, "stdio:ee --mcp-proxy", "ee_apply_code_action", args);

    let base = mcp_op(ws, None, "stdio:ee --mcp-proxy", "ee_apply_code_action", args);
    assert_eq!(decide(&base, std::slice::from_ref(&rule)).outcome, TrustOutcome::Allow);

    // Changed nested value.
    let changed_value = mcp_op(
        ws,
        None,
        "stdio:ee --mcp-proxy",
        "ee_apply_code_action",
        r#"{"action_id":"act_2","path":"/work/root/src/main.rs"}"#,
    );
    assert_eq!(
        decide(&changed_value, std::slice::from_ref(&rule)).outcome,
        TrustOutcome::Prompt,
        "changed argument value"
    );

    // Changed array order (arrays keep order in the canonical identity).
    let array_rule =
        mcp_rule("mcp_2", ws, "stdio:ee --mcp-proxy", "ee_apply_code_action", r#"{"items":[1,2]}"#);
    let array_a =
        mcp_op(ws, None, "stdio:ee --mcp-proxy", "ee_apply_code_action", r#"{"items":[1,2]}"#);
    let array_b =
        mcp_op(ws, None, "stdio:ee --mcp-proxy", "ee_apply_code_action", r#"{"items":[2,1]}"#);
    assert_eq!(decide(&array_a, std::slice::from_ref(&array_rule)).outcome, TrustOutcome::Allow);
    assert_eq!(
        decide(&array_b, std::slice::from_ref(&array_rule)).outcome,
        TrustOutcome::Prompt,
        "changed array order"
    );

    // Changed transport identity.
    let other_transport = mcp_op(ws, None, "acp:ee", "ee_apply_code_action", args);
    assert_eq!(
        decide(&other_transport, std::slice::from_ref(&rule)).outcome,
        TrustOutcome::Prompt,
        "cross-transport"
    );

    // Changed server, tool, or schema version.
    let other_server = TrustOperation {
        identity: OperationIdentity::Mcp {
            server: "other".to_string(),
            transport_identity: "stdio:ee --mcp-proxy".to_string(),
            tool: "ee_apply_code_action".to_string(),
            tool_schema_version: 1,
            arguments_json: args.to_string(),
        },
        ..base.clone()
    };
    assert_eq!(
        decide(&other_server, std::slice::from_ref(&rule)).outcome,
        TrustOutcome::Prompt,
        "changed server"
    );
    let other_tool = mcp_op(
        ws,
        None,
        "stdio:ee --mcp-proxy",
        "ee_format_file",
        r#"{"path":"/work/root/src/main.rs"}"#,
    );
    assert_eq!(
        decide(&other_tool, std::slice::from_ref(&rule)).outcome,
        TrustOutcome::Prompt,
        "changed tool"
    );
    let other_schema = TrustOperation {
        identity: OperationIdentity::Mcp {
            server: "ee".to_string(),
            transport_identity: "stdio:ee --mcp-proxy".to_string(),
            tool: "ee_apply_code_action".to_string(),
            tool_schema_version: 2,
            arguments_json: args.to_string(),
        },
        ..base.clone()
    };
    assert_eq!(
        decide(&other_schema, std::slice::from_ref(&rule)).outcome,
        TrustOutcome::Prompt,
        "changed schema version"
    );

    // Changed workspace or agent scope.
    let other_workspace = mcp_op(
        identity(b"/work/other"),
        None,
        "stdio:ee --mcp-proxy",
        "ee_apply_code_action",
        args,
    );
    assert_eq!(
        decide(&other_workspace, std::slice::from_ref(&rule)).outcome,
        TrustOutcome::Prompt,
        "cross-workspace"
    );
    let scoped = mcp_rule("mcp_3", ws, "stdio:ee --mcp-proxy", "ee_apply_code_action", args);
    let scoped = match scoped {
        TrustRule::Mcp(mut inner) => {
            inner.scope.agent = Some("openrouter".to_string());
            TrustRule::Mcp(inner)
        }
        _ => unreachable!(),
    };
    assert_eq!(
        decide(&base, std::slice::from_ref(&scoped)).outcome,
        TrustOutcome::Prompt,
        "agent-scoped rule never matches an unscoped operation"
    );
}

#[test]
fn mcp_invocation_normalizes_to_the_shared_operation() {
    let invocation = McpInvocation {
        workspace: identity(b"/work/root"),
        agent: Some("openrouter".to_string()),
        transport: crate::policy::TransportKind::McpStdio,
        transport_identity: "stdio:ee --mcp-proxy".to_string(),
        server: "ee".to_string(),
        tool: "ee_apply_code_action".to_string(),
        tool_schema_version: 1,
        category: TrustCategory::WriteModify,
        arguments_json: r#"{"action_id":"act_1","path":"/work/root/a.rs"}"#.to_string(),
    };
    let operation = invocation.to_operation();
    assert_eq!(operation.workspace, identity(b"/work/root"));
    assert_eq!(operation.agent.as_deref(), Some("openrouter"));
    assert_eq!(operation.category, TrustCategory::WriteModify);
    let OperationIdentity::Mcp {
        server,
        transport_identity,
        tool,
        tool_schema_version,
        arguments_json,
    } = operation.identity
    else {
        panic!("expected Mcp identity");
    };
    assert_eq!(server, "ee");
    assert_eq!(transport_identity, "stdio:ee --mcp-proxy");
    assert_eq!(tool, "ee_apply_code_action");
    assert_eq!(tool_schema_version, 1);
    assert_eq!(arguments_json, r#"{"action_id":"act_1","path":"/work/root/a.rs"}"#);
}

#[test]
fn canonical_arguments_sort_keys_and_keep_array_order() {
    let canonical = crate::policy::rules::canonicalize_arguments_json(
        r#"{ "b": [2, 1], "a": { "z": 1, "y": 2 } }"#,
    )
    .expect("canonical form");
    assert_eq!(canonical, r#"{"a":{"y":2,"z":1},"b":[2,1]}"#);
}

// ── End-to-end: stdio proxy and ACP-native routes (agents feature) ───────────

#[cfg(feature = "agents")]
mod e2e {
    use super::*;
    use crate::app::App;
    use crate::app::agents_mcp::CachedProxyCodeAction;
    use crate::app::{ApprovalChoice, PERSISTENT_TERMINAL_MAX_USES};
    use crate::tests::agent_mcp::{
        acp_connect_script, base_agent_script, connect_proxy, fake_response, mcp_app, mcp_app_in,
        open_pane_and_wait_ready, press, proxy_recv, proxy_send, wait_until,
    };
    use crate::tests::helpers::run_ex;
    use crossterm::event::{KeyCode, KeyModifiers};
    use serde_json::{Value, json};

    /// Seeds one exact MCP rule into the host-local store for the app
    /// workspace and returns its stable id.
    fn seed_mcp_rule(
        state_dir: &Path,
        workspace: &Path,
        transport_identity: &str,
        tool: &str,
        arguments_json: &str,
        max_uses: u64,
    ) -> String {
        let store = TrustStore::at(state_dir, workspace).unwrap();
        let ws = *store.workspace();
        let id = format!("mcp_seed_{tool}_{transport_identity}");
        let rule = TrustRule::Mcp(crate::policy::McpRule {
            id: id.clone(),
            scope: TrustRuleScope {
                workspace: ws,
                agent: None,
                expires_at: Some(SystemTime::now() + Duration::from_secs(3600)),
                max_uses: Some(max_uses),
            },
            server: "ee".to_string(),
            transport_identity: transport_identity.to_string(),
            tool: tool.to_string(),
            tool_schema_version: ee_mcp::EE_TOOL_SCHEMA_VERSION,
            arguments_json: arguments_json.to_string(),
        });
        store.add_rule(rule).expect("seed rule");
        id
    }

    /// Seeds a cached code action so `ee_apply_code_action` can be prepared
    /// without an LSP round trip.
    fn seed_code_action(app: &mut App, action_id: &str, path: &str, new_text: &str, line: u32) {
        app.agents.mcp.proxy_code_actions.insert(
            action_id.to_string(),
            CachedProxyCodeAction {
                path: path.to_string(),
                has_command: false,
                edits: vec![ee_mcp::PlannedTextEdit {
                    range: ee_mcp::TextRange {
                        start_line: line,
                        start_character: 1,
                        end_line: line,
                        end_character: 20,
                    },
                    new_text: new_text.to_string(),
                }],
            },
        );
    }

    fn action_frame(path: &str, action_id: &str) -> Value {
        json!({
            "method": "apply_code_action",
            "path": path,
            "action_id": action_id,
        })
    }

    fn open_pane_and_select(app: &mut App, index: usize) {
        run_ex(app, "agents");
        for _ in 0..index {
            press(app, KeyCode::Right, KeyModifiers::NONE);
        }
        press(app, KeyCode::Enter, KeyModifiers::NONE);
    }

    fn ledger_workspace(state_dir: &Path, workspace: &Path) -> WorkspaceIdentity {
        *TrustStore::at(state_dir, workspace).unwrap().workspace()
    }

    #[test]
    fn stdio_grant_persists_exact_invocation_and_auto_allows_identical_call() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let target = temp.path().join("code-action.txt");
        fs::write(&target, "alpha\nbeta\n").unwrap();
        let target_text = target.display().to_string();
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        seed_code_action(&mut app, "act_1", &target_text, "alpha-edited", 1);

        let mut stream = connect_proxy(&app);
        proxy_send(&mut stream, 1, action_frame(&target_text, "act_1"));
        wait_until(&mut app, "approval queued", |app| !app.agents.approvals.is_empty());
        {
            let prompt = app.agents.approvals.front().unwrap();
            assert_eq!(prompt.title, "ee_apply_code_action");
            assert!(prompt.detail.contains("server: ee"), "detail: {}", prompt.detail);
            assert!(
                prompt.detail.contains("tool: ee_apply_code_action"),
                "detail: {}",
                prompt.detail
            );
            assert!(prompt.detail.contains("class: write"), "detail: {}", prompt.detail);
            assert!(prompt.detail.contains("args:"), "detail: {}", prompt.detail);
            assert_eq!(prompt.options.len(), 5, "persistent option offered");
            assert_eq!(prompt.options[4].1, ApprovalChoice::AllowPersistent);
        }
        open_pane_and_select(&mut app, 4); // Allow for 1 hour / 20 uses

        wait_until(&mut app, "rule persisted and write applied", |_app| {
            fs::read_to_string(&target).map(|t| t.contains("alpha-edited")).unwrap_or(false)
        });
        let reply = proxy_recv(&mut stream);
        assert!(reply["result"]["value"].is_object(), "structured success: {reply}");

        let store = TrustStore::at(&state_dir, temp.path()).unwrap();
        let document = store.load().unwrap();
        assert_eq!(document.rules.len(), 1);
        let TrustRule::Mcp(rule) = &document.rules[0] else {
            panic!("expected mcp rule");
        };
        assert!(rule.id.starts_with("mcp_"));
        assert_eq!(rule.server, "ee");
        assert_eq!(rule.transport_identity, "stdio:ee --mcp-proxy");
        assert_eq!(rule.tool, "ee_apply_code_action");
        assert_eq!(rule.tool_schema_version, ee_mcp::EE_TOOL_SCHEMA_VERSION);
        assert_eq!(
            rule.arguments_json,
            format!(r#"{{"action_id":"act_1","path":"{target_text}"}}"#)
        );
        assert_eq!(rule.scope.max_uses, Some(PERSISTENT_TERMINAL_MAX_USES));
        let expiry = rule.scope.expires_at.expect("expiry");
        let now = SystemTime::now();
        assert!(expiry > now + Duration::from_secs(59 * 60), "expiry ~1h ahead");
        assert!(expiry < now + Duration::from_secs(61 * 60), "expiry ~1h ahead");
        assert_eq!(
            app.agents.usage_ledger.used(
                ledger_workspace(&state_dir, temp.path()),
                "proxy",
                &rule.id
            ),
            1
        );

        // Identical invocation (same path + action id) auto-allows; the
        // cached action is re-seeded so the planned edit still applies.
        seed_code_action(&mut app, "act_1", &target_text, "beta-edited", 2);
        proxy_send(&mut stream, 2, action_frame(&target_text, "act_1"));
        wait_until(&mut app, "second write applied without prompt", |_app| {
            fs::read_to_string(&target).map(|t| t.contains("beta-edited")).unwrap_or(false)
        });
        assert!(app.agents.approvals.is_empty(), "no approval for the identical invocation");
        assert_eq!(
            app.agents.usage_ledger.used(
                ledger_workspace(&state_dir, temp.path()),
                "proxy",
                &rule.id
            ),
            2
        );
        let _ = proxy_recv(&mut stream);

        // A different argument set never matches the exact rule.
        seed_code_action(&mut app, "act_2", &target_text, "gamma-edited", 1);
        proxy_send(&mut stream, 3, action_frame(&target_text, "act_2"));
        wait_until(&mut app, "changed arguments prompt again", |app| {
            !app.agents.approvals.is_empty()
        });
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE); // Deny
        let reply = proxy_recv(&mut stream);
        assert!(reply["result"]["error"].is_object(), "denied: {reply}");
    }

    #[test]
    fn acp_native_route_matches_only_its_own_transport_identity() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("acp-action.txt");
        fs::write(&target, "alpha\nbeta\n").unwrap();
        let target_text = target.display().to_string();
        let script = acp_connect_script(json!({
            "name": "ee_apply_code_action",
            "arguments": { "path": target_text, "action_id": "act_1" }
        }));
        let (mut app, fake) = mcp_app_in(&temp, script, false, true);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        seed_code_action(&mut app, "act_1", &target_text, "alpha-edited", 1);
        // Grant through the ACP-native route only.
        seed_mcp_rule(
            &state_dir,
            temp.path(),
            "acp:ee",
            "ee_apply_code_action",
            &format!(r#"{{"action_id":"act_1","path":"{target_text}"}}"#),
            20,
        );
        open_pane_and_wait_ready(&mut app);

        // The ACP-native invocation bypasses the approval prompt entirely.
        wait_until(&mut app, "acp-native write applied", |_app| {
            fs::read_to_string(&target).map(|t| t.contains("alpha-edited")).unwrap_or(false)
        });
        assert!(app.agents.approvals.is_empty(), "no prompt for the matched invocation");
        let reply = fake_response(&fake.agent(), 202);
        let result = reply.get("result").expect("tool result payload");
        assert_ne!(result.get("isError"), Some(&json!(true)), "success: {reply}");
    }

    #[test]
    fn grants_never_cross_transports() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("acp-cross.txt");
        fs::write(&target, "alpha\nbeta\n").unwrap();
        let target_text = target.display().to_string();
        let script = acp_connect_script(json!({
            "name": "ee_apply_code_action",
            "arguments": { "path": target_text, "action_id": "act_1" }
        }));
        let (mut app, fake) = mcp_app_in(&temp, script, false, true);
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        seed_code_action(&mut app, "act_1", &target_text, "alpha-edited", 1);
        // The grant was created through the stdio route: it must never
        // authorize the ACP-native route.
        seed_mcp_rule(
            &state_dir,
            temp.path(),
            "stdio:ee --mcp-proxy",
            "ee_apply_code_action",
            &format!(r#"{{"action_id":"act_1","path":"{target_text}"}}"#),
            20,
        );
        open_pane_and_wait_ready(&mut app);

        wait_until(&mut app, "cross-transport call prompts", |app| {
            !app.agents.approvals.is_empty()
        });
        run_ex(&mut app, "agents");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE); // Deny
        wait_until(&mut app, "approval resolved", |app| app.agents.approvals.is_empty());
        let reply = fake_response(&fake.agent(), 202);
        let result = reply.get("result").expect("tool result payload");
        assert_eq!(result.get("isError"), Some(&json!(true)), "cross-transport must fail: {reply}");
        assert!(!fs::read_to_string(&target).unwrap().contains("alpha-edited"));
    }

    #[test]
    fn content_bearing_and_terminal_tools_never_offer_persistent() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        app.agents.test_trust_store_base = Some(temp.path().join("state"));

        let mut stream = connect_proxy(&app);
        let target = temp.path().join("write.txt");
        fs::write(&target, "v0").unwrap();
        // ee_write_text_file carries file contents: never persistable.
        proxy_send(
            &mut stream,
            1,
            json!({
                "method": "write_text_file",
                "path": target.display().to_string(),
                "content": "agent-v1",
            }),
        );
        wait_until(&mut app, "write approval queued", |app| !app.agents.approvals.is_empty());
        {
            let prompt = app.agents.approvals.front().unwrap();
            assert_eq!(prompt.options.len(), 4, "content-bearing write never offers persistent");
            assert!(prompt.options.iter().all(|(label, _)| !label.contains("1 hour")));
        }
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE); // Deny
        let _ = proxy_recv(&mut stream);

        // Terminal creation uses command trust only: the persistent option
        // is the phase 2 command grant, and the persisted rule is a command
        // rule — generic MCP trust never applies to terminal-create.
        proxy_send(
            &mut stream,
            2,
            json!({ "method": "terminal_create", "command": "git", "args": ["status"] }),
        );
        wait_until(&mut app, "terminal approval queued", |app| !app.agents.approvals.is_empty());
        {
            let prompt = app.agents.approvals.front().unwrap();
            assert_eq!(prompt.options.len(), 5, "command trust offers its persistent option");
        }
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        open_pane_and_select(&mut app, 4); // Allow for 1 hour / 20 uses (command trust)
        wait_until(&mut app, "terminal granted", |_app| {
            TrustStore::at(&state_dir, temp.path())
                .map(|store| store.load().map(|doc| !doc.rules.is_empty()).unwrap_or(false))
                .unwrap_or(false)
        });
        let store = TrustStore::at(&state_dir, temp.path()).unwrap();
        let document = store.load().unwrap();
        assert_eq!(document.rules.len(), 1);
        assert!(
            matches!(document.rules[0], TrustRule::Command(_)),
            "terminal-create persists a command rule, never an MCP rule"
        );
        let _ = proxy_recv(&mut stream);
    }

    #[test]
    fn persistence_failure_denies_without_dispatch_and_keeps_budget() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let target = temp.path().join("fail.txt");
        fs::write(&target, "alpha\nbeta\n").unwrap();
        let target_text = target.display().to_string();
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        seed_code_action(&mut app, "act_1", &target_text, "alpha-edited", 1);
        // Create the store (and its 0700 trust directory) first.
        seed_mcp_rule(
            &state_dir,
            temp.path(),
            "stdio:ee --mcp-proxy",
            "ee_apply_code_action",
            r#"{"action_id":"act_seed","path":"/nowhere"}"#,
            20,
        );

        let mut stream = connect_proxy(&app);
        proxy_send(&mut stream, 1, action_frame(&target_text, "act_1"));
        wait_until(&mut app, "approval queued", |app| !app.agents.approvals.is_empty());

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
        assert!(!fs::read_to_string(&target).unwrap().contains("alpha-edited"), "no dispatch");
        assert!(app.agents.usage_ledger.is_empty(), "usage budget unchanged");
        let store = TrustStore::at(&state_dir, temp.path()).unwrap();
        assert_eq!(store.load().unwrap().rules.len(), 1, "only the seeded rule persists");
    }

    #[test]
    fn exhausted_mcp_rule_prompts_again_after_budget_is_consumed() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let target = temp.path().join("exhaust.txt");
        fs::write(&target, "alpha\nbeta\n").unwrap();
        let target_text = target.display().to_string();
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        seed_code_action(&mut app, "act_1", &target_text, "alpha-edited", 1);
        let rule_id = seed_mcp_rule(
            &state_dir,
            temp.path(),
            "stdio:ee --mcp-proxy",
            "ee_apply_code_action",
            &format!(r#"{{"action_id":"act_1","path":"{target_text}"}}"#),
            1,
        );

        let mut stream = connect_proxy(&app);
        proxy_send(&mut stream, 1, action_frame(&target_text, "act_1"));
        wait_until(&mut app, "first use applied", |_app| {
            fs::read_to_string(&target).map(|t| t.contains("alpha-edited")).unwrap_or(false)
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

        // Budget exhausted: the identical invocation prompts again.
        seed_code_action(&mut app, "act_1", &target_text, "beta-edited", 2);
        proxy_send(&mut stream, 2, action_frame(&target_text, "act_1"));
        wait_until(&mut app, "re-prompt after exhaustion", |app| !app.agents.approvals.is_empty());
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE); // Deny
        let reply = proxy_recv(&mut stream);
        assert!(reply["result"]["error"].is_object(), "denied: {reply}");
    }

    #[test]
    fn teardown_clears_mcp_usage_ledger() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
        open_pane_and_wait_ready(&mut app);
        let target = temp.path().join("teardown.txt");
        fs::write(&target, "alpha\nbeta\n").unwrap();
        let target_text = target.display().to_string();
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        seed_code_action(&mut app, "act_1", &target_text, "alpha-edited", 1);
        seed_mcp_rule(
            &state_dir,
            temp.path(),
            "stdio:ee --mcp-proxy",
            "ee_apply_code_action",
            &format!(r#"{{"action_id":"act_1","path":"{target_text}"}}"#),
            20,
        );

        let mut stream = connect_proxy(&app);
        proxy_send(&mut stream, 1, action_frame(&target_text, "act_1"));
        wait_until(&mut app, "write applied", |_app| {
            fs::read_to_string(&target).map(|t| t.contains("alpha-edited")).unwrap_or(false)
        });
        assert!(!app.agents.usage_ledger.is_empty());
        let _ = proxy_recv(&mut stream);

        app.shutdown_agents();
        assert!(app.agents.usage_ledger.is_empty(), "usage rows die with the sessions");
    }
}
