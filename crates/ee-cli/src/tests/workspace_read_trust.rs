//! Phase 4 workspace-gated read trust tests (ISSUES.md "Unified Host-Local
//! Workspace Trust Policy"): workspace gate, native/MCP read rules, and
//! protected-path classification.
//!
//! The workspace gate alone never permits an operation; every read needs
//! the gate plus its own matching rule.  Protected, external, traversal,
//! and symlink-escape paths can never match a persistent read rule.

use std::fs;
use std::path::Path;
#[cfg(feature = "agents")]
use std::time::Duration;
use std::time::SystemTime;

use tempfile::TempDir;

use crate::policy::evaluator::PolicyInput;
use crate::policy::paths::is_protected_relative_path;
use crate::policy::rules::TrustRule;
use crate::policy::session::SessionPolicy;
use crate::policy::store::TrustStore;
#[cfg(feature = "agents")]
use crate::policy::store::TrustStoreDocument;
use crate::policy::{
    DecisionReason, OperationIdentity, PathPrefix, TrustCategory, TrustDecision, TrustOperation,
    TrustOutcome, TrustRuleScope, UsageSnapshot, WorkspaceIdentity, evaluate,
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
        max_uses: None,
    }
}

fn read_path_rule(
    id: &str,
    workspace: WorkspaceIdentity,
    prefix: &str,
    max_bytes: u64,
) -> TrustRule {
    TrustRule::ReadPath(crate::policy::ReadPathRule {
        id: id.to_string(),
        scope: scope(workspace),
        path_prefix: PathPrefix::parse(prefix).expect("valid prefix"),
        max_bytes,
    })
}

fn read_op(
    workspace: WorkspaceIdentity,
    relative: &str,
    byte_count: Option<u64>,
) -> TrustOperation {
    TrustOperation {
        workspace,
        agent: None,
        transport: crate::policy::TransportKind::Acp,
        category: TrustCategory::Read,
        identity: OperationIdentity::ReadPath { relative_path: relative.to_string(), byte_count },
    }
}

fn mcp_read_rule(
    id: &str,
    workspace: WorkspaceIdentity,
    prefix: &str,
    max_bytes: u64,
) -> TrustRule {
    TrustRule::McpRead(crate::policy::McpReadRule {
        id: id.to_string(),
        scope: scope(workspace),
        server: "ee".to_string(),
        transport_identity: "stdio:ee --mcp-proxy".to_string(),
        tool: "ee_read_text_file".to_string(),
        tool_schema_version: 1,
        path_prefix: PathPrefix::parse(prefix).expect("valid prefix"),
        max_bytes,
    })
}

fn mcp_read_profile_rule(
    id: &str,
    workspace: WorkspaceIdentity,
    transport_identity: &str,
) -> TrustRule {
    TrustRule::McpReadProfile(crate::policy::McpReadProfileRule {
        id: id.to_string(),
        scope: scope(workspace),
        server: "ee".to_string(),
        transport_identity: transport_identity.to_string(),
        tool_schema_version: ee_mcp::EE_TOOL_SCHEMA_VERSION,
        profile: crate::policy::EE_MCP_SAFE_READ_PROFILE.to_string(),
    })
}

fn mcp_read_op(
    workspace: WorkspaceIdentity,
    transport_identity: &str,
    tool: &str,
    relative: &str,
    byte_count: Option<u64>,
) -> TrustOperation {
    TrustOperation {
        workspace,
        agent: None,
        transport: crate::policy::TransportKind::McpStdio,
        category: TrustCategory::Read,
        identity: OperationIdentity::McpRead {
            server: "ee".to_string(),
            transport_identity: transport_identity.to_string(),
            tool: tool.to_string(),
            tool_schema_version: 1,
            relative_path: relative.to_string(),
            byte_count,
        },
    }
}

fn decide(op: &TrustOperation, rules: &[TrustRule], workspace_enabled: bool) -> TrustDecision {
    evaluate(&PolicyInput {
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

// ── Gate and read rules ──────────────────────────────────────────────────────

#[test]
fn gate_alone_never_permits_a_read() {
    let ws = identity(b"/work/root");
    let op = read_op(ws, "src/main.rs", Some(1024));
    let decision = decide(&op, &[], false);
    assert_eq!(decision.outcome, TrustOutcome::Prompt);
    assert_eq!(decision.reason, DecisionReason::WorkspaceDisabled);
    let open = decide(&op, &[], true);
    assert_eq!(open.outcome, TrustOutcome::Prompt);
    assert_eq!(open.reason, DecisionReason::NoMatchingRule, "gate alone grants nothing");
}

#[test]
fn matching_read_rule_requires_gate_and_rule() {
    let ws = identity(b"/work/root");
    let rule = read_path_rule("read_1", ws, "src", 262_144);
    let op = read_op(ws, "src/main.rs", Some(1024));

    let gated = decide(&op, std::slice::from_ref(&rule), false);
    assert_eq!(gated.outcome, TrustOutcome::Prompt);
    assert_eq!(gated.reason, DecisionReason::WorkspaceDisabled);

    let allowed = decide(&op, std::slice::from_ref(&rule), true);
    assert_eq!(allowed.outcome, TrustOutcome::Allow);
    assert_eq!(allowed.reason, DecisionReason::PersistentAllow);
    assert_eq!(allowed.rule_id.as_deref(), Some("read_1"));
}

#[test]
fn read_rules_enforce_prefix_and_bounded_bytes() {
    let ws = identity(b"/work/root");
    let rule = read_path_rule("read_1", ws, "src", 1024);
    let in_prefix =
        decide(&read_op(ws, "src/main.rs", Some(100)), std::slice::from_ref(&rule), true);
    assert_eq!(in_prefix.outcome, TrustOutcome::Allow);
    let over_bytes =
        decide(&read_op(ws, "src/main.rs", Some(2048)), std::slice::from_ref(&rule), true);
    assert_eq!(over_bytes.outcome, TrustOutcome::Prompt, "over bounded byte limit");
    let outside = decide(&read_op(ws, "lib/main.rs", Some(100)), std::slice::from_ref(&rule), true);
    assert_eq!(outside.outcome, TrustOutcome::Prompt, "outside prefix");
}

#[test]
fn mcp_read_rule_requires_gate_and_exact_tool_identity() {
    let ws = identity(b"/work/root");
    let rule = mcp_read_rule("mcp_read_1", ws, "src", 262_144);
    let op =
        mcp_read_op(ws, "stdio:ee --mcp-proxy", "ee_read_text_file", "src/main.rs", Some(1024));

    let gated = decide(&op, std::slice::from_ref(&rule), false);
    assert_eq!(gated.reason, DecisionReason::WorkspaceDisabled);

    let allowed = decide(&op, std::slice::from_ref(&rule), true);
    assert_eq!(allowed.outcome, TrustOutcome::Allow);
    assert_eq!(allowed.rule_id.as_deref(), Some("mcp_read_1"));

    for (label, candidate) in [
        (
            "changed transport",
            mcp_read_op(ws, "acp:ee", "ee_read_text_file", "src/main.rs", Some(1024)),
        ),
        (
            "changed tool",
            mcp_read_op(ws, "stdio:ee --mcp-proxy", "ee_read_buffer", "src/main.rs", Some(1024)),
        ),
        (
            "changed server",
            TrustOperation {
                identity: OperationIdentity::McpRead {
                    server: "other".to_string(),
                    transport_identity: "stdio:ee --mcp-proxy".to_string(),
                    tool: "ee_read_text_file".to_string(),
                    tool_schema_version: 1,
                    relative_path: "src/main.rs".to_string(),
                    byte_count: Some(1024),
                },
                ..op.clone()
            },
        ),
        (
            "changed schema version",
            TrustOperation {
                identity: OperationIdentity::McpRead {
                    server: "ee".to_string(),
                    transport_identity: "stdio:ee --mcp-proxy".to_string(),
                    tool: "ee_read_text_file".to_string(),
                    tool_schema_version: 2,
                    relative_path: "src/main.rs".to_string(),
                    byte_count: Some(1024),
                },
                ..op.clone()
            },
        ),
        (
            "changed prefix",
            mcp_read_op(ws, "stdio:ee --mcp-proxy", "ee_read_text_file", "lib/main.rs", Some(1024)),
        ),
        (
            "over byte limit",
            mcp_read_op(
                ws,
                "stdio:ee --mcp-proxy",
                "ee_read_text_file",
                "src/main.rs",
                Some(524_288),
            ),
        ),
    ] {
        let decision = decide(&candidate, std::slice::from_ref(&rule), true);
        assert_eq!(decision.outcome, TrustOutcome::Prompt, "{label} must not match");
    }
}

#[test]
fn mcp_safe_read_profile_is_exactly_scoped_and_never_matches_write_or_unknown_tools() {
    let ws = identity(b"/work/root");
    let rule = mcp_read_profile_rule("mcp_profile_stdio", ws, "stdio:ee --mcp-proxy");
    let read =
        mcp_read_op(ws, "stdio:ee --mcp-proxy", "ee_read_text_file", "src/main.rs", Some(42));
    assert_eq!(decide(&read, std::slice::from_ref(&rule), true).outcome, TrustOutcome::Allow);

    let other_safe_read = TrustOperation {
        workspace: ws,
        agent: None,
        transport: crate::policy::TransportKind::McpStdio,
        category: TrustCategory::Read,
        identity: OperationIdentity::Mcp {
            server: "ee".to_string(),
            transport_identity: "stdio:ee --mcp-proxy".to_string(),
            tool: "ee_git_status".to_string(),
            tool_schema_version: ee_mcp::EE_TOOL_SCHEMA_VERSION,
            arguments_json: "{}".to_string(),
        },
    };
    assert_eq!(
        decide(&other_safe_read, std::slice::from_ref(&rule), true).outcome,
        TrustOutcome::Allow
    );

    for (label, candidate) in [
        ("ACP route", mcp_read_op(ws, "acp:ee", "ee_read_text_file", "src/main.rs", Some(42))),
        (
            "unknown tool",
            mcp_read_op(ws, "stdio:ee --mcp-proxy", "ee_unknown", "src/main.rs", Some(42)),
        ),
        (
            "wrong schema",
            TrustOperation {
                identity: OperationIdentity::McpRead {
                    server: "ee".to_string(),
                    transport_identity: "stdio:ee --mcp-proxy".to_string(),
                    tool: "ee_read_text_file".to_string(),
                    tool_schema_version: ee_mcp::EE_TOOL_SCHEMA_VERSION + 1,
                    relative_path: "src/main.rs".to_string(),
                    byte_count: Some(42),
                },
                ..read.clone()
            },
        ),
        ("write category", TrustOperation { category: TrustCategory::WriteModify, ..read.clone() }),
        ("execute category", TrustOperation { category: TrustCategory::Execute, ..read.clone() }),
    ] {
        assert_eq!(
            decide(&candidate, std::slice::from_ref(&rule), true).outcome,
            TrustOutcome::Prompt,
            "{label} must fail closed"
        );
    }
}

#[test]
fn mcp_safe_read_profile_covers_every_pinned_manifest_read_tool() {
    let ws = identity(b"/work/root");
    let rule = mcp_read_profile_rule("mcp_profile_stdio", ws, "stdio:ee --mcp-proxy");

    for tool in [
        "ee_workspace_roots",
        "ee_list_directory",
        "ee_list_directory_all",
        "ee_search_files",
        "ee_search_files_all",
        "ee_search_text",
        "ee_search_text_regex",
        "ee_search_text_in_files",
        "ee_read_buffer",
        "ee_read_buffer_lines",
        "ee_open_buffers",
        "ee_get_diagnostics",
        "ee_get_file_diagnostics",
        "ee_document_symbols",
        "ee_references",
        "ee_list_code_actions",
        "ee_preview_rename_symbol",
        "ee_read_text_file",
        "ee_terminal_output",
        "ee_terminal_output_since",
        "ee_terminal_wait",
        "ee_terminal_wait_long",
        "ee_git_status",
        "ee_git_diff",
        "ee_git_diff_file",
        "ee_changed_files",
        "ee_review_context",
        "ee_tools_manifest",
        "ee_project_instructions",
        "ee_read_notes",
        "ee_read_note",
        "ee_file_dependency_map",
        "ee_diagnostics",
    ] {
        assert_eq!(
            ee_mcp::classify::side_effect_class(tool),
            ee_mcp::SideEffectClass::Read,
            "{tool}"
        );
        let operation = mcp_read_op(ws, "stdio:ee --mcp-proxy", tool, "src/main.rs", Some(42));
        assert_eq!(
            decide(&operation, std::slice::from_ref(&rule), true).outcome,
            TrustOutcome::Allow,
            "{tool} must match safe-read profile"
        );
    }

    for tool in ["ee_apply_patch", "ee_terminal_create", "ee_unknown"] {
        let operation = mcp_read_op(ws, "stdio:ee --mcp-proxy", tool, "src/main.rs", Some(42));
        assert_eq!(
            decide(&operation, std::slice::from_ref(&rule), true).outcome,
            TrustOutcome::Prompt,
            "{tool} must not match safe-read profile"
        );
    }
}

// ── Protected-path classification ────────────────────────────────────────────

#[test]
fn protected_path_classes_are_ineligible() {
    for path in [
        ".env",
        ".env.local",
        "src/.env",
        ".git/config",
        ".ssh/id_rsa",
        "credentials/token",
        "credentials/api.json",
        "secrets/db.json",
        "vault/keys",
        "keys/id_ed25519",
        "keys/id_rsa",
        "certs/server.pem",
        "keys/client.key",
        "certs/chain.p12",
        "certs/identity.pfx",
        "keys/private.p8",
        "keys/data.der",
        "config/secret.json",
    ] {
        assert!(is_protected_relative_path(path), "{path} must be protected");
    }
}

#[test]
fn ordinary_source_paths_stay_eligible() {
    for path in [
        "src/main.rs",
        "Cargo.toml",
        "docs/readme.md",
        "src/lib/mod.rs",
        "tests/helper.rs",
        "public/id_ed25519.pub",
        "certs/trusted.pub",
    ] {
        assert!(!is_protected_relative_path(path), "{path} must stay eligible");
    }
}

#[test]
fn unknown_profile_rule_ids_are_rejected_at_load() {
    let (_base, _workspace_dir, store) = store_setup();
    let text = format!(
        r#"
schema_version = 1

[workspace]
identity = "{identity}"

[policy]
workspace_enabled = false

[[mcp_read_profile_allow]]
id = "mcp_profile_bad"
server = "ee"
transport_identity = "stdio:ee --mcp-proxy"
tool_schema_version = 1
profile = "mystery_profile"

[[mcp_read_profile_allow]]
id = "mcp_profile_ok"
server = "ee"
transport_identity = "stdio:ee --mcp-proxy"
tool_schema_version = 1
profile = "ee_mcp_safe_read"

[[profile_allow]]
id = "profile_bad"
profile = "mystery_profile"
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20

[[profile_allow]]
id = "profile_ok"
profile = "git_readonly"
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20
"#,
        identity = store.workspace().as_string()
    );
    write_store_text(store.path(), &text);
    let document = store.load().expect("load");
    let ids: Vec<&str> = document.rules.iter().map(TrustRule::id).collect();
    assert_eq!(ids, vec!["mcp_profile_ok", "profile_ok"], "unknown profile ids rejected at load");
}

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

// ── App normalization (agents feature) ───────────────────────────────────────

#[cfg(feature = "agents")]
mod e2e {
    use super::*;
    use crate::tests::agent_mcp::{base_agent_script, mcp_app};
    use ee_agent_protocol::{ReadTextFileRequest, SessionId};

    /// Seeds a store document with the given rules and workspace gate.
    fn seed_store(state_dir: &Path, workspace: &Path, enabled: bool, rules: Vec<TrustRule>) {
        let store = TrustStore::at(state_dir, workspace).unwrap();
        let document =
            TrustStoreDocument { workspace: *store.workspace(), workspace_enabled: enabled, rules };
        store.write(&document).expect("seed store");
    }

    fn read_rule_with(workspace: WorkspaceIdentity, prefix: &str, max_bytes: u64) -> TrustRule {
        let mut rule = read_path_rule("read_src", workspace, prefix, max_bytes);
        match &mut rule {
            TrustRule::ReadPath(inner) => {
                inner.scope.expires_at = Some(SystemTime::now() + Duration::from_secs(3600));
            }
            _ => unreachable!(),
        }
        rule
    }

    #[test]
    fn native_read_decision_requires_gate_and_matching_rule() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, false);
        let workspace = temp.path();
        let source = workspace.join("src/main.rs");
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(&source, "fn main() {}").unwrap();
        let state_dir = workspace.join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        let ws = *TrustStore::at(&state_dir, workspace).unwrap().workspace();
        seed_store(&state_dir, workspace, true, vec![read_rule_with(ws, "src", 262_144)]);

        let allowed = app.native_read_decision(&source, Some(100));
        assert_eq!(allowed.outcome, TrustOutcome::Allow, "{allowed:?}");
        assert_eq!(allowed.rule_id.as_deref(), Some("read_src"));

        // Gate disabled: the same read prompts (no persistent authority).
        seed_store(&state_dir, workspace, false, vec![read_rule_with(ws, "src", 262_144)]);
        let gated = app.native_read_decision(&source, Some(100));
        assert_eq!(gated.outcome, TrustOutcome::Prompt);
        assert_eq!(gated.reason, DecisionReason::WorkspaceDisabled);

        // No rule at all: prompt even with the gate open.
        seed_store(&state_dir, workspace, true, Vec::new());
        let uncovered = app.native_read_decision(&source, Some(100));
        assert_eq!(uncovered.reason, DecisionReason::NoMatchingRule);
    }

    #[test]
    fn protected_external_traversal_and_symlink_escape_reads_never_match() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, false);
        let workspace = temp.path();
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(workspace.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(workspace.join(".env"), "API_TOKEN=secret").unwrap();
        let state_dir = workspace.join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        let ws = *TrustStore::at(&state_dir, workspace).unwrap().workspace();
        seed_store(&state_dir, workspace, true, vec![read_rule_with(ws, "src", 262_144)]);

        // Hidden/secret-class paths inside the matching prefix never match.
        let env = app.native_read_decision(&workspace.join(".env"), Some(100));
        assert_eq!(env.reason, DecisionReason::UnknownOperation, "{env:?}");
        let hidden = app.native_read_decision(&workspace.join("src/.env"), Some(100));
        assert_eq!(hidden.reason, DecisionReason::UnknownOperation, "{hidden:?}");
        let key = app.native_read_decision(&workspace.join("src/keys/id_rsa"), Some(100));
        assert_eq!(key.reason, DecisionReason::UnknownOperation, "{key:?}");

        // External and traversal paths are outside the workspace.
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("x.txt"), "x").unwrap();
        let external = app.native_read_decision(&outside.path().join("x.txt"), Some(100));
        assert_eq!(external.reason, DecisionReason::UnknownOperation, "{external:?}");

        // Symlink escape resolves outside the workspace.
        #[cfg(unix)]
        {
            let link = workspace.join("src/escape");
            std::os::unix::fs::symlink(outside.path(), &link).unwrap();
            let escaped = app.native_read_decision(&link, Some(100));
            assert_eq!(escaped.reason, DecisionReason::UnknownOperation, "{escaped:?}");
        }
    }

    #[test]
    fn mcp_read_decision_matches_only_its_transport_and_tool() {
        let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, false);
        let workspace = temp.path();
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(workspace.join("src/main.rs"), "fn main() {}").unwrap();
        let state_dir = workspace.join("state");
        fs::create_dir_all(&state_dir).unwrap();
        app.agents.test_trust_store_base = Some(state_dir.clone());
        let ws = *TrustStore::at(&state_dir, workspace).unwrap().workspace();
        seed_store(
            &state_dir,
            workspace,
            true,
            vec![mcp_read_rule("mcp_read_src", ws, "src", 262_144)],
        );

        let request = |path: &Path, limit: Option<u32>| {
            let mut request = ReadTextFileRequest::new(SessionId::new("s1"), path.to_path_buf());
            request.limit = limit;
            request
        };
        let source = workspace.join("src/main.rs");
        let allowed = app.mcp_read_decision(
            &request(&source, Some(100)),
            crate::app::agents_mcp::ProxyRoute::Stdio,
        );
        assert_eq!(allowed.outcome, TrustOutcome::Allow, "{allowed:?}");
        assert_eq!(allowed.rule_id.as_deref(), Some("mcp_read_src"));

        // The ACP-native transport never matches a stdio-route grant.
        let cross_transport = app.mcp_read_decision(
            &request(&source, Some(100)),
            crate::app::agents_mcp::ProxyRoute::AcpNative,
        );
        assert_eq!(cross_transport.outcome, TrustOutcome::Prompt, "{cross_transport:?}");

        // Protected paths and over-limit reads never match.
        let hidden = app.mcp_read_decision(
            &request(&workspace.join("src/.env"), Some(100)),
            crate::app::agents_mcp::ProxyRoute::Stdio,
        );
        assert_eq!(hidden.reason, DecisionReason::UnknownOperation);
        let oversized = app.mcp_read_decision(
            &request(&source, Some(524_288)),
            crate::app::agents_mcp::ProxyRoute::Stdio,
        );
        assert_eq!(oversized.outcome, TrustOutcome::Prompt, "over bounded byte limit");
    }
}
