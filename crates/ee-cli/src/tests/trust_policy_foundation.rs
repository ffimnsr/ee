//! Foundation tests for the unified host-local workspace trust
//! policy (ISSUES.md "Unified Host-Local Workspace Trust Policy").
//!
//! Covers the shared trust-domain contracts, the pure shared evaluator, the
//! host-local trust store, and the session precedence adaptation.  All store
//! tests run against temporary state directories and canonical workspace
//! roots; nothing touches real user state.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use tempfile::TempDir;

use crate::policy::evaluator::{PolicyInput, evaluate};
use crate::policy::rules::{MatchMode, TrustRule, WriteOperationKind};
use crate::policy::session::{SessionChoice, SessionPolicy};
use crate::policy::store::{TrustStore, TrustStoreDocument, TrustStoreError};
use crate::policy::{
    DecisionReason, OperationIdentity, PathPrefix, TransportKind, TrustCategory, TrustDecision,
    TrustOperation, TrustOutcome, TrustRuleScope, UsageSnapshot, WorkspaceIdentity,
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

struct RuleSpec<'a> {
    id: &'a str,
    agent: Option<&'a str>,
    expires_at: Option<&'a str>,
    max_uses: Option<u64>,
}

fn spec(id: &str) -> RuleSpec<'_> {
    RuleSpec { id, agent: None, expires_at: Some("2026-08-08T12:00:00Z"), max_uses: Some(20) }
}

fn scope_for(spec: &RuleSpec<'_>, workspace: WorkspaceIdentity) -> TrustRuleScope {
    TrustRuleScope {
        workspace,
        agent: spec.agent.map(String::from),
        expires_at: spec.expires_at.map(at),
        max_uses: spec.max_uses,
    }
}

fn command_rule(
    spec: RuleSpec<'_>,
    workspace: WorkspaceIdentity,
    executable: &str,
    match_mode: MatchMode,
    argv: &[&str],
) -> TrustRule {
    TrustRule::Command(crate::policy::CommandRule {
        id: spec.id.to_string(),
        effect: crate::policy::TrustEffect::Allow,
        scope: scope_for(&spec, workspace),
        executable: executable.to_string(),
        match_mode,
        argv: argv.iter().map(|token| token.to_string()).collect(),
    })
}

fn mcp_rule(spec: RuleSpec<'_>, workspace: WorkspaceIdentity, arguments_json: &str) -> TrustRule {
    TrustRule::Mcp(crate::policy::McpRule {
        id: spec.id.to_string(),
        effect: crate::policy::TrustEffect::Allow,
        scope: scope_for(&spec, workspace),
        server: "ee".to_string(),
        transport_identity: "stdio:test".to_string(),
        tool: "ee_read_file".to_string(),
        tool_schema_version: 1,
        arguments_json: arguments_json.to_string(),
    })
}

fn read_path_rule(
    spec: RuleSpec<'_>,
    workspace: WorkspaceIdentity,
    path_prefix: &str,
    max_bytes: u64,
) -> TrustRule {
    TrustRule::ReadPath(crate::policy::ReadPathRule {
        id: spec.id.to_string(),
        effect: crate::policy::TrustEffect::Allow,
        scope: scope_for(&spec, workspace),
        path_prefix: PathPrefix::parse(path_prefix).expect("valid prefix"),
        max_bytes,
    })
}

fn profile_rule(spec: RuleSpec<'_>, workspace: WorkspaceIdentity, profile: &str) -> TrustRule {
    TrustRule::Profile(crate::policy::ProfileRule {
        id: spec.id.to_string(),
        effect: crate::policy::TrustEffect::Allow,
        scope: scope_for(&spec, workspace),
        profile: profile.to_string(),
    })
}

fn write_rule(
    spec: RuleSpec<'_>,
    workspace: WorkspaceIdentity,
    operation: WriteOperationKind,
    path_prefix: &str,
    max_files: u64,
    max_total_bytes: u64,
    max_file_bytes: u64,
) -> TrustRule {
    TrustRule::Write(crate::policy::WriteRule {
        id: spec.id.to_string(),
        effect: crate::policy::TrustEffect::Allow,
        scope: scope_for(&spec, workspace),
        operation,
        path_prefix: PathPrefix::parse(path_prefix).expect("valid prefix"),
        max_files,
        max_total_bytes,
        max_file_bytes,
    })
}

fn operation(
    workspace: WorkspaceIdentity,
    agent: Option<&str>,
    category: TrustCategory,
    identity: OperationIdentity,
) -> TrustOperation {
    TrustOperation {
        workspace,
        agent: agent.map(String::from),
        transport: TransportKind::Acp,
        category,
        identity,
    }
}

fn command_op(workspace: WorkspaceIdentity, executable: &str, argv: &[&str]) -> TrustOperation {
    operation(
        workspace,
        None,
        TrustCategory::Execute,
        OperationIdentity::Command {
            executable: executable.to_string(),
            argv: argv.iter().map(|token| token.to_string()).collect(),
        },
    )
}

fn read_path_op(
    workspace: WorkspaceIdentity,
    relative: &str,
    bytes: Option<u64>,
) -> TrustOperation {
    operation(
        workspace,
        None,
        TrustCategory::Read,
        OperationIdentity::ReadPath { relative_path: relative.to_string(), byte_count: bytes },
    )
}

fn profile_op(workspace: WorkspaceIdentity, profile: &str) -> TrustOperation {
    operation(
        workspace,
        None,
        TrustCategory::Execute,
        OperationIdentity::Profile { profile: profile.to_string() },
    )
}

fn decide(
    op: &TrustOperation,
    rules: &[TrustRule],
    session: &SessionPolicy,
    session_id: &str,
    now: &str,
    usage: &UsageSnapshot,
    workspace_enabled: bool,
) -> TrustDecision {
    evaluate(&PolicyInput {
        session_id,
        fingerprint: "fp",
        operation: op,
        session,
        rules,
        now: at(now),
        usage,
        workspace_enabled,
        safe_read_enabled: false,
        trusted_write_enabled: false,
        workspace_execute_enabled: false,
        workspace_execute_unrestricted: false,
        mcp_server_trust: None,
        built_in_deny: None,
        tool_default: None,
        category_default: None,
        global_default: None,
    })
}

fn decide_default(op: &TrustOperation, rules: &[TrustRule]) -> TrustDecision {
    decide(
        op,
        rules,
        &SessionPolicy::default(),
        "s1",
        "2026-08-07T12:00:00Z",
        &UsageSnapshot::default(),
        true,
    )
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

fn dir_entries(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> =
        fs::read_dir(dir).unwrap().map(|entry| entry.unwrap().path()).collect();
    entries.sort();
    entries
}

impl RuleSpec<'_> {
    fn agent(mut self, agent: Option<&'static str>) -> Self {
        self.agent = agent;
        self
    }

    fn max_uses(mut self, max_uses: Option<u64>) -> Self {
        self.max_uses = max_uses;
        self
    }
}

fn sample_document(store: &TrustStore) -> TrustStoreDocument {
    let ws = *store.workspace();
    TrustStoreDocument {
        workspace: ws,
        workspace_enabled: false,
        tool_defaults: Vec::new(),
        category_defaults: Vec::new(),
        global_default: crate::policy::FallbackEffect::Confirm,
        rules: vec![
            command_rule(
                spec("cmd_1").agent(Some("openrouter")),
                ws,
                "git",
                MatchMode::ArgvPrefix,
                &["status"],
            ),
            mcp_rule(spec("mcp_1").agent(Some("openrouter")), ws, r#"{"path":"src/main.rs"}"#),
            read_path_rule(spec("read_1").agent(Some("openrouter")), ws, "src", 262_144),
            TrustRule::McpRead(crate::policy::McpReadRule {
                id: "mcp_read_1".to_string(),
                effect: crate::policy::TrustEffect::Allow,
                scope: scope_for(&spec("mcp_read_1").agent(Some("openrouter")), ws),
                server: "ee".to_string(),
                transport_identity: "stdio:test".to_string(),
                tool: "ee_read_file".to_string(),
                tool_schema_version: 1,
                path_prefix: PathPrefix::parse("src").expect("valid prefix"),
                max_bytes: 262_144,
            }),
            profile_rule(spec("profile_1").agent(Some("openrouter")), ws, "git_readonly"),
            write_rule(
                spec("write_1").agent(Some("openrouter")),
                ws,
                WriteOperationKind::Create,
                "src/generated",
                5,
                65_536,
                16_384,
            ),
        ],
    }
}

mod evaluator;
mod session_policy;
mod store_files;
mod store_schema;
