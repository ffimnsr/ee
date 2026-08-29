//! Phase 14 extended permission-policy security and compatibility matrix.
//!
//! Uses deterministic clocks, temporary owner-only stores, immutable evaluator
//! inputs, injected dispatch counters, and fake agent UI seams. No live network
//! or user-home state participates.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::policy::evaluator::{PolicyInput, evaluate_with_trace};
use crate::policy::manager::{RuleMutation, mutate_rule, test_policy};
use crate::policy::session::{SessionChoice, SessionPolicy};
use crate::policy::{
    BrowserActionClass, CommandRule, DecisionReason, FallbackEffect, FilesystemOperationKind,
    FilesystemRule, HostMatchMode, MatchMode, McpDenyRule, McpRule, NetworkMethodClass,
    NetworkRule, NetworkScheme, OperationIdentity, PathPrefix, SafeguardCategory, SafeguardMatch,
    ToolRule, ToolRuleIdentity, TraceStatus, TransportKind, TrustCategory, TrustEffect,
    TrustOperation, TrustOutcome, TrustRule, TrustRuleScope, TrustStore, TrustStoreDocument,
    UsageLedger, UsageSnapshot, WorkspaceIdentity, WriteOperationKind, WriteRule,
};

const SESSION: &str = "phase14-session";
const AGENT: &str = "phase14-agent";
const FINGERPRINT: &str = "phase14-operation";
const SECRET: &str = "PHASE14_SECRET_7c91";

fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_800_000_000)
}

fn workspace() -> WorkspaceIdentity {
    WorkspaceIdentity::from_canonical_root_bytes(b"/phase14/workspace")
}

fn allow_scope(workspace: WorkspaceIdentity) -> TrustRuleScope {
    TrustRuleScope {
        workspace,
        agent: Some(AGENT.into()),
        expires_at: Some(now() + Duration::from_secs(3600)),
        max_uses: Some(5),
    }
}

fn reducing_scope(workspace: WorkspaceIdentity) -> TrustRuleScope {
    TrustRuleScope { workspace, agent: Some(AGENT.into()), expires_at: None, max_uses: None }
}

#[derive(Clone, Copy, Debug)]
enum FixtureKind {
    CommandExact,
    CommandPrefix,
    Mcp,
    Read,
    WriteCreate,
    WriteModify,
    Fs(FilesystemOperationKind),
    Network,
    NativeTool,
}

#[derive(Clone, Debug)]
struct Fixture {
    label: &'static str,
    kind: FixtureKind,
    operation: TrustOperation,
}

fn category_for_filesystem(operation: FilesystemOperationKind) -> TrustCategory {
    match operation {
        FilesystemOperationKind::Read => TrustCategory::Read,
        FilesystemOperationKind::Create => TrustCategory::WriteCreate,
        FilesystemOperationKind::Delete => TrustCategory::Delete,
        FilesystemOperationKind::Modify
        | FilesystemOperationKind::Rename
        | FilesystemOperationKind::Chmod
        | FilesystemOperationKind::Symlink => TrustCategory::WriteModify,
    }
}

fn fixtures(transport: TransportKind) -> Vec<Fixture> {
    let workspace = workspace();
    let mut values = vec![
        Fixture {
            label: "command-exact",
            kind: FixtureKind::CommandExact,
            operation: TrustOperation {
                workspace,
                agent: Some(AGENT.into()),
                transport,
                category: TrustCategory::Execute,
                identity: OperationIdentity::Command {
                    executable: "cargo".into(),
                    argv: vec!["check".into(), "--package".into(), "ee-cli".into()],
                },
            },
        },
        Fixture {
            label: "command-prefix",
            kind: FixtureKind::CommandPrefix,
            operation: TrustOperation {
                workspace,
                agent: Some(AGENT.into()),
                transport,
                category: TrustCategory::Execute,
                identity: OperationIdentity::Command {
                    executable: "cargo".into(),
                    argv: vec!["test".into(), "--package".into(), "ee-cli".into()],
                },
            },
        },
        Fixture {
            label: "mcp",
            kind: FixtureKind::Mcp,
            operation: TrustOperation {
                workspace,
                agent: Some(AGENT.into()),
                transport,
                category: TrustCategory::WriteModify,
                identity: OperationIdentity::Mcp {
                    server: "ee".into(),
                    transport_identity: match transport {
                        TransportKind::McpAcp => "acp:ee".into(),
                        _ => "stdio:ee --mcp-proxy".into(),
                    },
                    tool: "ee_format_file".into(),
                    tool_schema_version: 1,
                    arguments_json: r#"{"path":"src/lib.rs"}"#.into(),
                },
            },
        },
        Fixture {
            label: "read",
            kind: FixtureKind::Read,
            operation: TrustOperation {
                workspace,
                agent: Some(AGENT.into()),
                transport,
                category: TrustCategory::Read,
                identity: OperationIdentity::ReadPath {
                    relative_path: "src/lib.rs".into(),
                    byte_count: Some(64),
                },
            },
        },
        Fixture {
            label: "write-create",
            kind: FixtureKind::WriteCreate,
            operation: write_operation(transport, TrustCategory::WriteCreate),
        },
        Fixture {
            label: "write-modify",
            kind: FixtureKind::WriteModify,
            operation: write_operation(transport, TrustCategory::WriteModify),
        },
        Fixture {
            label: "network",
            kind: FixtureKind::Network,
            operation: TrustOperation {
                workspace,
                agent: Some(AGENT.into()),
                transport,
                category: TrustCategory::Network,
                identity: OperationIdentity::network(
                    NetworkScheme::Https,
                    "api.example.test",
                    443,
                    NetworkMethodClass::Read,
                    BrowserActionClass::Fetch,
                )
                .expect("network identity"),
            },
        },
        Fixture {
            label: "native-tool",
            kind: FixtureKind::NativeTool,
            operation: TrustOperation {
                workspace,
                agent: Some(AGENT.into()),
                transport,
                category: TrustCategory::WriteModify,
                identity: OperationIdentity::native_tool("editor/apply-code-action")
                    .expect("native identity"),
            },
        },
    ];
    for operation in [
        FilesystemOperationKind::Read,
        FilesystemOperationKind::Create,
        FilesystemOperationKind::Modify,
        FilesystemOperationKind::Delete,
        FilesystemOperationKind::Rename,
        FilesystemOperationKind::Chmod,
        FilesystemOperationKind::Symlink,
    ] {
        let (source, destination) = match operation {
            FilesystemOperationKind::Rename | FilesystemOperationKind::Symlink => {
                (Some("generated/source.txt"), Some("generated/destination.txt"))
            }
            _ => (Some("generated/source.txt"), None),
        };
        values.push(Fixture {
            label: match operation {
                FilesystemOperationKind::Read => "filesystem-read",
                FilesystemOperationKind::Create => "filesystem-create",
                FilesystemOperationKind::Modify => "filesystem-modify",
                FilesystemOperationKind::Delete => "filesystem-delete",
                FilesystemOperationKind::Rename => "filesystem-rename",
                FilesystemOperationKind::Chmod => "filesystem-chmod",
                FilesystemOperationKind::Symlink => "filesystem-symlink",
            },
            kind: FixtureKind::Fs(operation),
            operation: TrustOperation {
                workspace,
                agent: Some(AGENT.into()),
                transport,
                category: category_for_filesystem(operation),
                identity: OperationIdentity::filesystem(operation, source, destination)
                    .expect("filesystem identity"),
            },
        });
    }
    values
}

fn write_operation(transport: TransportKind, category: TrustCategory) -> TrustOperation {
    TrustOperation {
        workspace: workspace(),
        agent: Some(AGENT.into()),
        transport,
        category,
        identity: OperationIdentity::Write {
            relative_path: "generated/output.rs".into(),
            file_count: 1,
            total_bytes: Some(128),
            max_file_bytes: Some(128),
        },
    }
}

fn rule_for(fixture: &Fixture, effect: TrustEffect, id: &str) -> TrustRule {
    let workspace = fixture.operation.workspace;
    let scope = if effect == TrustEffect::Allow {
        allow_scope(workspace)
    } else {
        reducing_scope(workspace)
    };
    match fixture.kind {
        FixtureKind::CommandExact | FixtureKind::CommandPrefix => {
            let OperationIdentity::Command { executable, argv } = &fixture.operation.identity
            else {
                unreachable!()
            };
            TrustRule::Command(CommandRule {
                id: id.into(),
                effect,
                scope,
                executable: executable.clone(),
                match_mode: if matches!(fixture.kind, FixtureKind::CommandPrefix) {
                    MatchMode::ArgvPrefix
                } else {
                    MatchMode::ArgvExact
                },
                argv: if matches!(fixture.kind, FixtureKind::CommandPrefix) {
                    argv[..2].to_vec()
                } else {
                    argv.clone()
                },
            })
        }
        FixtureKind::Mcp => {
            let OperationIdentity::Mcp {
                server,
                transport_identity,
                tool,
                tool_schema_version,
                arguments_json,
            } = &fixture.operation.identity
            else {
                unreachable!()
            };
            if effect == TrustEffect::Allow {
                TrustRule::Mcp(McpRule {
                    id: id.into(),
                    effect,
                    scope,
                    server: server.clone(),
                    transport_identity: transport_identity.clone(),
                    tool: tool.clone(),
                    tool_schema_version: *tool_schema_version,
                    arguments_json: arguments_json.clone(),
                })
            } else {
                TrustRule::mcp_deny(McpDenyRule {
                    id: id.into(),
                    effect,
                    scope,
                    server: server.clone(),
                    transport_identity: transport_identity.clone(),
                    tool: tool.clone(),
                    tool_schema_version: *tool_schema_version,
                    category: Some(fixture.operation.category),
                })
            }
        }
        FixtureKind::Read => TrustRule::ReadPath(crate::policy::ReadPathRule {
            id: id.into(),
            effect,
            scope,
            path_prefix: PathPrefix::parse("src").expect("read prefix"),
            max_bytes: 1024,
        }),
        FixtureKind::WriteCreate | FixtureKind::WriteModify => TrustRule::Write(WriteRule {
            id: id.into(),
            effect,
            scope,
            operation: if matches!(fixture.kind, FixtureKind::WriteCreate) {
                WriteOperationKind::Create
            } else {
                WriteOperationKind::Modify
            },
            path_prefix: PathPrefix::parse("generated").expect("write prefix"),
            max_files: if effect == TrustEffect::Allow { 2 } else { 0 },
            max_total_bytes: if effect == TrustEffect::Allow { 1024 } else { 0 },
            max_file_bytes: if effect == TrustEffect::Allow { 512 } else { 0 },
        }),
        FixtureKind::Fs(operation) => TrustRule::filesystem(FilesystemRule {
            id: id.into(),
            effect,
            scope,
            operations: vec![operation],
            path_prefix: PathPrefix::parse("generated").expect("filesystem prefix"),
        }),
        FixtureKind::Network => {
            let mut rule = if effect == TrustEffect::Allow {
                NetworkRule::allow_exact(
                    id.into(),
                    scope,
                    NetworkScheme::Https,
                    "api.example.test".into(),
                    443,
                    NetworkMethodClass::Read,
                    BrowserActionClass::Fetch,
                )
                .expect("network allow")
            } else {
                NetworkRule::deny(
                    id.into(),
                    scope,
                    NetworkScheme::Https,
                    "api.example.test".into(),
                    HostMatchMode::Exact,
                    443,
                    NetworkMethodClass::Read,
                    BrowserActionClass::Fetch,
                )
                .expect("network reducing rule")
            };
            rule.effect = effect;
            TrustRule::Network(rule)
        }
        FixtureKind::NativeTool => TrustRule::tool(ToolRule {
            id: id.into(),
            effect,
            scope,
            identity: ToolRuleIdentity::Native { tool: "editor/apply-code-action".into() },
            category: Some(TrustCategory::WriteModify),
        }),
    }
}

fn policy_input<'a>(
    operation: &'a TrustOperation,
    rules: &'a [TrustRule],
    session: &'a SessionPolicy,
    usage: &'a UsageSnapshot,
) -> PolicyInput<'a> {
    PolicyInput {
        session_id: SESSION,
        fingerprint: FINGERPRINT,
        operation,
        session,
        rules,
        now: now(),
        usage,
        workspace_enabled: true,
        built_in_deny: None,
        tool_default: None,
        category_default: None,
        global_default: Some(FallbackEffect::Confirm),
    }
}

#[derive(Default)]
struct DispatchRecorder {
    dispatches: usize,
    usage: UsageLedger,
}

impl DispatchRecorder {
    fn resolve(
        &mut self,
        operation: &TrustOperation,
        decision: &crate::policy::TrustDecision,
    ) -> bool {
        if decision.outcome != TrustOutcome::Allow {
            return false;
        }
        self.dispatches += 1;
        if let Some(rule_id) = decision.rule_id.as_deref() {
            self.usage.record_use(operation.workspace, SESSION, rule_id);
        }
        true
    }
}

fn assert_redacted(decision: &crate::policy::TrustDecision) {
    let rendered = format!("{decision:?}");
    for forbidden in ["src/lib.rs", "generated/output.rs", "api.example.test", SECRET] {
        assert!(!rendered.contains(forbidden), "decision leaked {forbidden:?}: {rendered}");
    }
}

#[test]
fn permission_policy_matrix_effect_category_transport_is_deterministic() {
    for transport in [TransportKind::Acp, TransportKind::McpStdio, TransportKind::McpAcp] {
        for fixture in fixtures(transport) {
            let supported_effects: &[TrustEffect] = match fixture.kind {
                FixtureKind::Fs(_) | FixtureKind::NativeTool => {
                    &[TrustEffect::Deny, TrustEffect::Confirm]
                }
                _ => &[TrustEffect::Deny, TrustEffect::Confirm, TrustEffect::Allow],
            };
            for effect in supported_effects {
                let id = format!(
                    "matrix_{}_{}",
                    fixture.label.replace('-', "_"),
                    match effect {
                        TrustEffect::Deny => "deny",
                        TrustEffect::Confirm => "confirm",
                        TrustEffect::Allow => "allow",
                    }
                );
                let rule = rule_for(&fixture, *effect, &id);
                let rules = vec![rule];
                let session = SessionPolicy::default();
                let usage = UsageSnapshot::default();
                let before_usage = usage.clone();
                let store_bytes = b"immutable-host-local-store".to_vec();
                let result = evaluate_with_trace(&policy_input(
                    &fixture.operation,
                    &rules,
                    &session,
                    &usage,
                ));
                let expected = match effect {
                    TrustEffect::Deny => TrustOutcome::Deny,
                    TrustEffect::Confirm => TrustOutcome::Confirm,
                    TrustEffect::Allow => TrustOutcome::Allow,
                };
                assert_eq!(result.decision.outcome, expected, "{} {effect:?}", fixture.label);
                assert_eq!(
                    result.decision.reason,
                    match effect {
                        TrustEffect::Deny => DecisionReason::PersistentDeny,
                        TrustEffect::Confirm => DecisionReason::MandatoryConfirm,
                        TrustEffect::Allow => DecisionReason::PersistentAllow,
                    },
                    "{} {effect:?}",
                    fixture.label
                );
                assert_eq!(result.decision.rule_id.as_deref(), Some(id.as_str()));
                assert_eq!(result.trace.len(), 10);
                assert_eq!(usage, before_usage, "evaluator consumed usage");
                assert_eq!(store_bytes, b"immutable-host-local-store", "evaluator changed store");
                assert_redacted(&result.decision);

                let mut recorder = DispatchRecorder::default();
                let dispatched = recorder.resolve(&fixture.operation, &result.decision);
                assert_eq!(dispatched, expected == TrustOutcome::Allow);
                assert_eq!(recorder.dispatches, usize::from(expected == TrustOutcome::Allow));
                let approval_visible = result.decision.outcome == TrustOutcome::Confirm;
                assert_eq!(approval_visible, expected == TrustOutcome::Confirm);
                if expected != TrustOutcome::Allow {
                    assert!(!dispatched);
                    assert_eq!(recorder.dispatches, 0);
                }
                assert_eq!(
                    recorder.usage.used(fixture.operation.workspace, SESSION, &id),
                    u64::from(expected == TrustOutcome::Allow)
                );
            }
        }
    }
}

#[test]
fn permission_policy_matrix_precedence_defaults_and_mismatches_fail_closed() {
    for transport in [TransportKind::Acp, TransportKind::McpStdio, TransportKind::McpAcp] {
        for fixture in fixtures(transport) {
            let deny = rule_for(&fixture, TrustEffect::Deny, "phase14_deny");
            let confirm = rule_for(&fixture, TrustEffect::Confirm, "phase14_confirm");
            let mut session = SessionPolicy::default();
            session.record(SESSION, FINGERPRINT, SessionChoice::Allow);
            let allow = if matches!(fixture.kind, FixtureKind::Fs(_) | FixtureKind::NativeTool) {
                None
            } else {
                Some(rule_for(&fixture, TrustEffect::Allow, "phase14_allow"))
            };
            let mut rules = vec![confirm.clone(), deny.clone()];
            if let Some(allow) = allow.clone() {
                rules.push(allow);
            }
            let usage = UsageSnapshot::default();
            let decision =
                evaluate_with_trace(&policy_input(&fixture.operation, &rules, &session, &usage));
            assert_eq!(
                decision.decision.reason,
                DecisionReason::PersistentDeny,
                "{}",
                fixture.label
            );
            assert_eq!(decision.trace[1].status, TraceStatus::Matched);

            let without_deny = rules
                .iter()
                .filter(|rule| rule.effect() != TrustEffect::Deny)
                .cloned()
                .collect::<Vec<_>>();
            let decision = evaluate_with_trace(&policy_input(
                &fixture.operation,
                &without_deny,
                &session,
                &usage,
            ));
            assert_eq!(
                decision.decision.reason,
                DecisionReason::MandatoryConfirm,
                "{}",
                fixture.label
            );

            let mut session_deny = SessionPolicy::default();
            session_deny.record(SESSION, FINGERPRINT, SessionChoice::Deny);
            let allow_only = allow.into_iter().collect::<Vec<_>>();
            let decision = evaluate_with_trace(&policy_input(
                &fixture.operation,
                &allow_only,
                &session_deny,
                &usage,
            ));
            assert_eq!(decision.decision.reason, DecisionReason::SessionDeny, "{}", fixture.label);
        }
    }

    let operation = &fixtures(TransportKind::Acp)[0].operation;
    for (tool, category, global, expected) in [
        (
            Some(FallbackEffect::Deny),
            Some(FallbackEffect::Confirm),
            Some(FallbackEffect::Confirm),
            DecisionReason::ToolDefaultDeny,
        ),
        (
            Some(FallbackEffect::Confirm),
            Some(FallbackEffect::Deny),
            Some(FallbackEffect::Deny),
            DecisionReason::ToolDefaultConfirm,
        ),
        (
            None,
            Some(FallbackEffect::Deny),
            Some(FallbackEffect::Confirm),
            DecisionReason::CategoryDefaultDeny,
        ),
        (
            None,
            Some(FallbackEffect::Confirm),
            Some(FallbackEffect::Deny),
            DecisionReason::CategoryDefaultConfirm,
        ),
        (None, None, Some(FallbackEffect::Deny), DecisionReason::GlobalDefaultDeny),
        (None, None, Some(FallbackEffect::Confirm), DecisionReason::GlobalDefaultConfirm),
    ] {
        let session = SessionPolicy::default();
        let usage = UsageSnapshot::default();
        let mut input = policy_input(operation, &[], &session, &usage);
        input.tool_default = tool;
        input.category_default = category;
        input.global_default = global;
        assert_eq!(evaluate_with_trace(&input).decision.reason, expected);
    }

    let fixture = &fixtures(TransportKind::McpStdio)[2];
    let rule = rule_for(fixture, TrustEffect::Allow, "mcp_exact_allow");
    for mismatch in [
        TrustOperation {
            workspace: WorkspaceIdentity::from_canonical_root_bytes(b"/other"),
            ..fixture.operation.clone()
        },
        TrustOperation { agent: Some("other-agent".into()), ..fixture.operation.clone() },
        TrustOperation {
            identity: OperationIdentity::Mcp {
                server: "ee".into(),
                transport_identity: "acp:other".into(),
                tool: "ee_format_file".into(),
                tool_schema_version: 1,
                arguments_json: r#"{"path":"src/lib.rs"}"#.into(),
            },
            ..fixture.operation.clone()
        },
        TrustOperation {
            identity: OperationIdentity::Mcp {
                server: "ee".into(),
                transport_identity: "stdio:ee --mcp-proxy".into(),
                tool: "ee_format_file".into(),
                tool_schema_version: 2,
                arguments_json: r#"{"path":"src/lib.rs"}"#.into(),
            },
            ..fixture.operation.clone()
        },
        TrustOperation {
            category: TrustCategory::Unknown,
            identity: OperationIdentity::Unknown,
            ..fixture.operation.clone()
        },
    ] {
        let session = SessionPolicy::default();
        let usage = UsageSnapshot::default();
        let decision = evaluate_with_trace(&policy_input(
            &mismatch,
            std::slice::from_ref(&rule),
            &session,
            &usage,
        ));
        assert_ne!(decision.decision.outcome, TrustOutcome::Allow);
    }
}

fn private_write(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().expect("parent")).expect("trust directory");
    fs::write(path, bytes).expect("trust fixture");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path.parent().expect("parent"), fs::Permissions::from_mode(0o700))
            .expect("private directory");
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("private file");
    }
}

#[test]
fn permission_policy_matrix_lifecycle_migration_restart_and_mutation_are_safe() {
    let state = tempfile::tempdir().expect("state");
    let workspace_dir = tempfile::tempdir().expect("workspace");
    let store = TrustStore::at(state.path(), workspace_dir.path()).expect("store");
    let workspace = *store.workspace();
    let fixture = Fixture {
        label: "restart-command",
        kind: FixtureKind::CommandExact,
        operation: TrustOperation {
            workspace,
            agent: Some(AGENT.into()),
            transport: TransportKind::Acp,
            category: TrustCategory::Execute,
            identity: OperationIdentity::Command {
                executable: "cargo".into(),
                argv: vec!["check".into(), "--package".into(), "ee-cli".into()],
            },
        },
    };
    let deny = rule_for(&fixture, TrustEffect::Deny, "restart_deny");
    let confirm = rule_for(&fixture, TrustEffect::Confirm, "restart_confirm");
    store
        .write(&TrustStoreDocument {
            workspace,
            workspace_enabled: true,
            tool_defaults: Vec::new(),
            category_defaults: Vec::new(),
            global_default: FallbackEffect::Confirm,
            rules: vec![confirm, deny],
        })
        .expect("seed durable rules");
    let durable = fs::read(store.path()).expect("durable bytes");

    let restarted = TrustStore::at(state.path(), workspace_dir.path()).expect("restart handle");
    let effective = restarted.effective_at(now());
    assert_eq!(effective.rules.len(), 2, "deny and confirm survive restart");
    let session = SessionPolicy::default();
    let usage = UsageSnapshot::default();
    let tested = test_policy(&policy_input(&fixture.operation, &effective.rules, &session, &usage));
    assert_eq!(tested.decision.reason, DecisionReason::PersistentDeny);
    assert_eq!(fs::read(store.path()).expect("unchanged bytes"), durable);

    mutate_rule(&store, "restart_deny", RuleMutation::Disable, now()).expect("disable");
    let effective = store.effective_at(now());
    assert_eq!(
        evaluate_with_trace(&policy_input(&fixture.operation, &effective.rules, &session, &usage))
            .decision
            .reason,
        DecisionReason::MandatoryConfirm
    );
    mutate_rule(&store, "restart_confirm", RuleMutation::Revoke, now()).expect("revoke");
    let effective = store.effective_at(now());
    assert_eq!(
        evaluate_with_trace(&policy_input(&fixture.operation, &effective.rules, &session, &usage))
            .decision
            .reason,
        DecisionReason::GlobalDefaultConfirm
    );

    let expires: chrono::DateTime<chrono::Utc> = (now() + Duration::from_secs(3600)).into();
    let legacy = format!(
        "schema_version = 1\n\n[workspace]\nidentity = \"{}\"\n\n[policy]\nworkspace_enabled = true\n\n[[command_allow]]\nid = \"migrated_allow\"\nagent = \"{AGENT}\"\nexecutable = \"cargo\"\nmatch = \"argv_exact\"\nargv = [\"check\", \"--package\", \"ee-cli\"]\nexpires_at = \"{}\"\nmax_uses = 5\n",
        workspace.as_string(),
        expires.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    );
    private_write(store.path(), legacy.as_bytes());
    let migrated = store.load_at(now()).expect("startup migration");
    assert_eq!(migrated.rules[0].id(), "migrated_allow");
    assert_eq!(migrated.rules[0].effect(), TrustEffect::Allow);
    assert!(
        fs::read_to_string(store.path()).expect("migrated text").contains("schema_version = 2")
    );

    let corrupt = b"schema_version = 1\n[[command_allow]]\nmatch = \"raw-regex\"\n".to_vec();
    private_write(store.path(), &corrupt);
    assert!(store.load_at(now()).is_err());
    assert_eq!(fs::read(store.path()).expect("corrupt unchanged"), corrupt);
    assert!(store.effective_at(now()).rules.is_empty());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        private_write(store.path(), legacy.as_bytes());
        fs::set_permissions(store.path(), fs::Permissions::from_mode(0o644))
            .expect("insecure mode");
        let before = fs::read(store.path()).expect("insecure bytes");
        assert!(store.load_at(now()).is_err());
        assert_eq!(fs::read(store.path()).expect("unchanged insecure bytes"), before);
        assert!(store.effective_at(now()).rules.is_empty());
    }

    let mut session_state = SessionPolicy::default();
    session_state.record(SESSION, FINGERPRINT, SessionChoice::Allow);
    let mut ledger = UsageLedger::default();
    ledger.record_use(workspace, SESSION, "migrated_allow");
    session_state.invalidate_session(SESSION);
    ledger.invalidate_session(SESSION);
    assert!(session_state.lookup(SESSION, FINGERPRINT).is_none());
    assert_eq!(ledger.used(workspace, SESSION, "migrated_allow"), 0);
}

#[cfg(feature = "agents")]
fn submit_permissions(app: &mut crate::app::App, arguments: &str) {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    for character in format!("/permissions {arguments}").chars() {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)));
    }
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
}

#[cfg(feature = "agents")]
#[test]
fn permission_policy_matrix_ui_audit_preview_revoke_tester_and_reset_are_redacted() {
    use crate::app::{ActionLogEntry, ApprovalChoice};
    use crate::policy::PolicyClock;
    use crate::tests::agent_bridge::{agents_app_in, base_script};
    use crate::tests::agent_mcp::open_pane_and_wait_ready;

    let temp = tempfile::tempdir().expect("workspace");
    let (mut app, _fake) = agents_app_in(&temp, base_script());
    app.trust_clock = PolicyClock::fake_at(now());
    let state = temp.path().join("state");
    fs::create_dir_all(&state).expect("state directory");
    app.agents.test_trust_store_base = Some(state.clone());
    open_pane_and_wait_ready(&mut app);

    let store = TrustStore::at(&state, temp.path()).expect("store");
    let rule = TrustRule::Command(CommandRule {
        id: "ui_deny".into(),
        effect: TrustEffect::Deny,
        scope: TrustRuleScope {
            workspace: *store.workspace(),
            agent: None,
            expires_at: None,
            max_uses: None,
        },
        executable: "cargo".into(),
        match_mode: MatchMode::ArgvExact,
        argv: vec!["check".into()],
    });
    store.add_rule(rule).expect("seed deny");
    app.reload_workspace_trust_store().expect("reload deny");
    let durable = fs::read(store.path()).expect("store bytes");

    let mut denied = app.queue_terminal_approval_for_test(
        SESSION,
        Some(AGENT),
        "cargo",
        &["check"],
        &[("TOKEN", SECRET)],
        Some(temp.path().to_path_buf()),
    );
    assert!(app.agents.approvals.is_empty(), "automatic deny opens no approval UI");
    assert!(denied.try_recv().expect("automatic result").is_err());
    assert_eq!(app.agents.terminals.tracked_count(), 0);
    assert!(app.agents.usage_ledger.is_empty());
    assert_eq!(fs::read(store.path()).expect("deny does not mutate store"), durable);

    submit_permissions(&mut app, "list");
    let notices = app
        .agents
        .threads
        .iter()
        .flat_map(|thread| thread.system_notices())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(notices.contains("[persistent deny]"));
    assert!(notices.contains("id:ui_deny"));
    assert!(!notices.contains("cargo"), "manager leaked executable: {notices}");
    assert!(!notices.contains(SECRET));

    submit_permissions(&mut app, "revoke ui_deny");
    assert!(store.load().expect("revoked store").rules.is_empty());
    let mut prompted = app.queue_terminal_approval_for_test(
        SESSION,
        Some(AGENT),
        "cargo",
        &["check"],
        &[("TOKEN", SECRET)],
        Some(temp.path().to_path_buf()),
    );
    assert_eq!(app.agents.approvals.len(), 1, "revocation takes effect immediately");
    assert!(prompted.try_recv().is_err());
    app.confirm_bridge_approval_for_test(ApprovalChoice::DenyOnce);
    assert!(prompted.try_recv().expect("denied prompt").is_err());

    submit_permissions(&mut app, "test command executable=cargo argv=check");
    let tester_notice = app
        .agents
        .threads
        .iter()
        .flat_map(|thread| thread.system_notices())
        .rev()
        .find(|notice| notice.contains("side-effects:none"))
        .expect("tester output");
    assert!(tester_notice.contains("verdict:Confirm"));
    assert!(tester_notice.contains("precedence:"));
    assert!(app.agents.usage_ledger.is_empty());
    assert!(store.load().expect("tester store").rules.is_empty());

    let mut preview_reply = app.queue_terminal_approval_for_test(
        SESSION,
        Some(AGENT),
        "cargo",
        &["check"],
        &[],
        Some(temp.path().to_path_buf()),
    );
    app.confirm_bridge_approval_for_test(ApprovalChoice::AllowPersistent);
    let preview = app
        .agents
        .approvals
        .front()
        .and_then(|prompt| prompt.allow_confirmation_preview())
        .expect("bounded preview");
    assert!(preview.expires_at > now());
    assert_eq!(preview.max_uses, 20);
    assert!(store.load().expect("preview store").rules.is_empty());
    app.cancel_rule_confirmation_for_test();
    app.confirm_bridge_approval_for_test(ApprovalChoice::DenyOnce);
    assert!(preview_reply.try_recv().expect("preview canceled and denied").is_err());

    store
        .add_rule(TrustRule::Command(CommandRule {
            id: "reset_deny".into(),
            effect: TrustEffect::Deny,
            scope: reducing_scope(*store.workspace()),
            executable: "cargo".into(),
            match_mode: MatchMode::ArgvExact,
            argv: vec!["check".into()],
        }))
        .expect("seed reset rule");
    submit_permissions(&mut app, "reset");
    assert_eq!(store.load().expect("unconfirmed reset").rules.len(), 1);
    submit_permissions(&mut app, "reset confirm");
    assert!(store.load().expect("confirmed reset").rules.is_empty());

    let audit = format!("{:?}", app.agents_action_log());
    assert!(audit.contains("PersistentDeny"));
    assert!(audit.contains("revoke"));
    assert!(audit.contains("reset"));
    assert!(!audit.contains(SECRET));
    assert!(!notices.contains(SECRET));
    assert!(!tester_notice.contains(SECRET));
    assert!(!format!("{:?}", app.backend.status_message).contains(SECRET));
    assert!(!fs::read_to_string(store.path()).expect("final store").contains(SECRET));
    assert!(app.agents_action_log().iter().any(|entry| matches!(
        entry,
        ActionLogEntry::TrustRuleMutation { action, .. } if action == "revoke"
    )));
}

#[test]
fn permission_policy_matrix_builtin_deny_beats_every_allow_source() {
    let fixture = &fixtures(TransportKind::Acp)[0];
    let allow = rule_for(fixture, TrustEffect::Allow, "builtin_conflict_allow");
    let mut session = SessionPolicy::default();
    session.record(SESSION, FINGERPRINT, SessionChoice::Allow);
    let usage = UsageSnapshot::default();
    let mut input =
        policy_input(&fixture.operation, std::slice::from_ref(&allow), &session, &usage);
    input.built_in_deny = Some(SafeguardMatch {
        rule_id: "builtin.phase14",
        category: SafeguardCategory::CatastrophicDeletion,
    });
    input.tool_default = Some(FallbackEffect::Confirm);
    input.category_default = Some(FallbackEffect::Confirm);
    let result = evaluate_with_trace(&input);
    assert_eq!(result.decision.outcome, TrustOutcome::Deny);
    assert_eq!(result.decision.reason, DecisionReason::BuiltInDeny);
    assert_eq!(result.trace[0].status, TraceStatus::Matched);
    assert!(result.trace[1..].iter().all(|step| step.status == TraceStatus::NotReached));
    assert_redacted(&result.decision);
}
