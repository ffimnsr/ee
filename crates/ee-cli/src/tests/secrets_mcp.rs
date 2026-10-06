//! End-to-end MCP secret wiring: workspace config references, host-local
//! workspace trust, vault resolution, and redaction collection — all fakes.
//!
//! No developer keychain, machine ID, network, or real MCP server is touched:
//! vault layers are fake, config layers live in temp directories, and the
//! "server" is only ever a resolved settings value.

use crate::app::App;
use crate::secrets::test_support::StoredKeychain;
use crate::secrets::{HostBinding, SecretName, SecretStore};
use crate::tests::helpers::{CurrentDirGuard, EnvVarGuard, run_ex};
use crate::workspace_trust::{WorkspaceTrustDecision, WorkspaceTrustStore};
use zeroize::Zeroizing;

const WORKSPACE_MCP_TOML: &str = r#"
[mcp.servers.tools]
transport = "stdio"
command = "mcp-tools"
env = { GITHUB_TOKEN = "secret://mcp.github-token", LOG_LEVEL = "info" }

[mcp.servers.remote]
transport = "streamable_http"
url = "https://example.com/mcp"
headers = { Authorization = "Bearer secret://mcp.remote-token" }
"#;

const LITERAL_MCP_TOML: &str = r#"
[mcp.servers.local]
transport = "stdio"
command = "mcp-local"
env = { LOG_LEVEL = "debug", GITHUB_TOKEN = "literal-token" }
"#;

const USER_LAYER_MCP_TOML: &str = r#"
[mcp.servers.user-owned]
transport = "stdio"
command = "mcp-user"
env = { TOKEN = "secret://mcp.user-token" }
"#;

const WORKSPACE_AGENT_TOML: &str = r#"
[agents]
enabled = true
default_agent = "fake"

[agents.servers.fake]
command = "unused"
env = { TOKEN = "secret://agent.test-token" }
"#;

struct Fixture {
    keychain: StoredKeychain,
    store: SecretStore,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let keychain = StoredKeychain::new();
        let binding = HostBinding::from_identifier_bytes(b"mcp-secrets-machine\n").expect("valid");
        let store = SecretStore::new(
            Box::new(keychain.clone()),
            binding,
            dir.path().join("ee").join("secrets").join("v1.json"),
        );
        Self { keychain, store, _dir: dir }
    }

    fn seed(&self, name: &str, value: &str) {
        self.store
            .set(&SecretName::new(name).expect("valid name"), &Zeroizing::new(value.to_owned()))
            .expect("seed secret");
    }
}

#[test]
fn mcp_references_deny_until_workspace_is_trusted() {
    let fixture = Fixture::new();
    fixture.seed("mcp.github-token", "ghp-resolved-token");
    fixture.seed("mcp.remote-token", "remote-resolved-token");

    let temp = tempfile::tempdir().expect("temp dir");
    let home = tempfile::tempdir().expect("home temp dir");
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let _home_guard = EnvVarGuard::set("HOME", home.path());
    let _xdg_guard = EnvVarGuard::set("XDG_CONFIG_HOME", home.path().join("xdg"));
    std::fs::write(temp.path().join(".ee.toml"), WORKSPACE_MCP_TOML).unwrap();

    let mut app = App::from_path(None).unwrap();
    let state = temp.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    app.agents.test_trust_store_base = Some(state.clone());
    let keychain = fixture.keychain.clone();
    app.agents.test_secret_store = Some(fixture.store);

    // Undecided workspace: every referencing server fails closed before any
    // keychain access, and each failure is visible in the pane state.
    let loads_before = keychain.load_calls();
    assert!(app.resolve_mcp_servers().is_empty(), "undecided denies workspace references");
    assert_eq!(keychain.load_calls(), loads_before, "deny happens before store access");
    assert_eq!(app.agents.mcp.servers.len(), 2, "failures surface in the pane state");
    for server in app.agents.mcp.servers.values() {
        assert_eq!(server.state, ee_mcp::McpServerState::Failed);
        assert!(
            server.error.as_deref().is_some_and(|error| error.contains("workspace is not trusted")),
            "failure reason names the trust gate"
        );
    }

    // Trusted: both servers resolve, templates substitute exactly once.
    WorkspaceTrustStore::at(&state, temp.path())
        .unwrap()
        .set_decision(WorkspaceTrustDecision::Trusted)
        .unwrap();
    let resolved = app.resolve_mcp_servers();
    assert_eq!(resolved.len(), 2);
    let tools = resolved.iter().find(|server| server.id == "tools").expect("tools server");
    assert_eq!(tools.env.get("GITHUB_TOKEN").map(String::as_str), Some("ghp-resolved-token"));
    assert_eq!(tools.env.get("LOG_LEVEL").map(String::as_str), Some("info"));
    let remote = resolved.iter().find(|server| server.id == "remote").expect("remote server");
    assert_eq!(
        remote.headers.get("Authorization").map(String::as_str),
        Some("Bearer remote-resolved-token")
    );
    assert!(keychain.load_calls() > loads_before, "trusted resolution reads the store");

    // Redaction sees bare resolved values and never reference text.
    let secrets = app.agents_secret_values();
    assert!(secrets.contains(&String::from("ghp-resolved-token")));
    assert!(secrets.contains(&String::from("remote-resolved-token")));
    assert!(!secrets.iter().any(|secret| secret.contains("secret://")));

    // Forwarding builds one ACP entry per resolved server.
    assert_eq!(app.mcp_forward_entries().len(), 2);
}

#[test]
fn revoked_workspace_denies_mcp_references_after_grant() {
    let fixture = Fixture::new();
    fixture.seed("mcp.github-token", "ghp-resolved-token");
    fixture.seed("mcp.remote-token", "remote-resolved-token");

    let temp = tempfile::tempdir().expect("temp dir");
    let home = tempfile::tempdir().expect("home temp dir");
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let _home_guard = EnvVarGuard::set("HOME", home.path());
    let _xdg_guard = EnvVarGuard::set("XDG_CONFIG_HOME", home.path().join("xdg"));
    std::fs::write(temp.path().join(".ee.toml"), WORKSPACE_MCP_TOML).unwrap();

    let mut app = App::from_path(None).unwrap();
    let state = temp.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    app.agents.test_trust_store_base = Some(state.clone());
    app.agents.test_secret_store = Some(fixture.store);

    let trust = WorkspaceTrustStore::at(&state, temp.path()).unwrap();
    trust.set_decision(WorkspaceTrustDecision::Trusted).unwrap();
    trust.set_decision(WorkspaceTrustDecision::Untrusted).unwrap();

    assert!(app.resolve_mcp_servers().is_empty(), "revoked workspace denies references");
}

#[test]
fn trust_does_not_bleed_between_repositories() {
    let fixture = Fixture::new();
    fixture.seed("mcp.github-token", "ghp-resolved-token");
    fixture.seed("mcp.remote-token", "remote-resolved-token");

    let root = tempfile::tempdir().expect("root temp dir");
    let repo_a = root.path().join("repo_a");
    let repo_b = root.path().join("repo_b");
    for repo in [&repo_a, &repo_b] {
        std::fs::create_dir_all(repo.join(".git")).unwrap();
    }
    std::fs::create_dir_all(repo_b.join("src")).unwrap();
    std::fs::write(repo_b.join(".ee.toml"), WORKSPACE_MCP_TOML).unwrap();
    let file_b = repo_b.join("src").join("main.rs");
    std::fs::write(&file_b, "fn main() {}\n").unwrap();

    let home = tempfile::tempdir().expect("home temp dir");
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    // Open ee with cwd in repo A while the opened file belongs to repo B.
    std::env::set_current_dir(&repo_a).unwrap();
    let _home_guard = EnvVarGuard::set("HOME", home.path());
    let _xdg_guard = EnvVarGuard::set("XDG_CONFIG_HOME", home.path().join("xdg"));

    let mut app = App::from_path(Some(file_b.clone())).unwrap();
    assert!(
        app.current_workspace_root().ends_with("repo_b"),
        "trust root follows the opened file's workspace"
    );
    let state = root.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    app.agents.test_trust_store_base = Some(state.clone());
    app.agents.test_secret_store = Some(fixture.store);

    // Repo A is trusted, but repo B is undecided: B's references stay blocked.
    WorkspaceTrustStore::at(&state, &repo_a)
        .unwrap()
        .set_decision(WorkspaceTrustDecision::Trusted)
        .unwrap();
    assert!(
        app.resolve_mcp_servers().is_empty(),
        "trust for one repository never unlocks another repository's references"
    );

    // Trusting repo B itself unlocks exactly its servers.
    WorkspaceTrustStore::at(&state, &repo_b)
        .unwrap()
        .set_decision(WorkspaceTrustDecision::Trusted)
        .unwrap();
    assert_eq!(app.resolve_mcp_servers().len(), 2);
}

#[test]
fn agent_launch_failures_surface_the_trust_gate_without_store_access() {
    let fixture = Fixture::new();
    fixture.seed("agent.test-token", "agent-resolved-token");

    let temp = tempfile::tempdir().expect("temp dir");
    let home = tempfile::tempdir().expect("home temp dir");
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let _home_guard = EnvVarGuard::set("HOME", home.path());
    let _xdg_guard = EnvVarGuard::set("XDG_CONFIG_HOME", home.path().join("xdg"));
    std::fs::write(temp.path().join(".ee.toml"), WORKSPACE_AGENT_TOML).unwrap();

    let mut app = App::from_path(None).unwrap();
    let state = temp.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    app.agents.test_trust_store_base = Some(state.clone());
    app.agents.test_session_state_base = Some(state.join("sessions"));
    let keychain = fixture.keychain.clone();
    app.agents.test_secret_store = Some(fixture.store);

    // Undecided workspace: the agent is skipped before any store access, and
    // the reason stays visible after the stderr warning scrolls away.
    let loads_before = keychain.load_calls();
    run_ex(&mut app, "agents");

    let message = app
        .agents
        .launch_failures
        .get("fake")
        .cloned()
        .expect("skipped agent is recorded as a launch failure");
    assert!(message.contains("workspace is not trusted"), "message: {message}");
    assert_eq!(keychain.load_calls(), loads_before, "deny happens before store access");

    // Starting the skipped server reports the recorded reason in the status line.
    app.start_selected_agent_session(String::from("fake"));
    let status = app.backend.status_message.clone().unwrap_or_default();
    assert!(
        status.contains("unavailable") && status.contains("workspace is not trusted"),
        "status: {status}"
    );
}

#[test]
fn literal_mcp_servers_never_touch_the_store() {
    let fixture = Fixture::new();
    let keychain = fixture.keychain.clone();

    let temp = tempfile::tempdir().expect("temp dir");
    let home = tempfile::tempdir().expect("home temp dir");
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let _home_guard = EnvVarGuard::set("HOME", home.path());
    let _xdg_guard = EnvVarGuard::set("XDG_CONFIG_HOME", home.path().join("xdg"));
    std::fs::write(temp.path().join(".ee.toml"), LITERAL_MCP_TOML).unwrap();

    let mut app = App::from_path(None).unwrap();
    let state = temp.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    app.agents.test_trust_store_base = Some(state);
    app.agents.test_secret_store = Some(fixture.store);

    let resolved = app.resolve_mcp_servers();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].env.get("LOG_LEVEL").map(String::as_str), Some("debug"));
    assert_eq!(resolved[0].env.get("GITHUB_TOKEN").map(String::as_str), Some("literal-token"));
    assert_eq!(keychain.load_calls(), 0, "literal-only MCP config never reads the store");

    // Literal secret-like values stay in the redaction set.
    assert!(app.agents_secret_values().contains(&String::from("literal-token")));
}

#[test]
fn user_layer_mcp_references_resolve_without_workspace_trust() {
    let fixture = Fixture::new();
    fixture.seed("mcp.user-token", "user-resolved-token");

    let temp = tempfile::tempdir().expect("temp dir");
    let xdg = tempfile::tempdir().expect("xdg temp dir");
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let _home_guard = EnvVarGuard::set("HOME", temp.path());
    let _xdg_guard = EnvVarGuard::set("XDG_CONFIG_HOME", xdg.path());
    std::fs::create_dir_all(xdg.path().join("ee")).unwrap();
    std::fs::write(xdg.path().join("ee").join("config.toml"), USER_LAYER_MCP_TOML).unwrap();

    let mut app = App::from_path(None).unwrap();
    app.agents.test_trust_store_base = Some(temp.path().join("state"));
    app.agents.test_secret_store = Some(fixture.store);

    let resolved = app.resolve_mcp_servers();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].env.get("TOKEN").map(String::as_str), Some("user-resolved-token"));
    let secrets = app.agents_secret_values();
    assert!(secrets.contains(&String::from("user-resolved-token")));
    assert!(!secrets.iter().any(|secret| secret.contains("secret://")));
}

#[test]
fn missing_referenced_mcp_secret_skips_only_that_server() {
    let fixture = Fixture::new();
    fixture.seed("mcp.github-token", "ghp-resolved-token");
    // `mcp.remote-token` is never seeded.

    let temp = tempfile::tempdir().expect("temp dir");
    let home = tempfile::tempdir().expect("home temp dir");
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let _home_guard = EnvVarGuard::set("HOME", home.path());
    let _xdg_guard = EnvVarGuard::set("XDG_CONFIG_HOME", home.path().join("xdg"));
    std::fs::write(temp.path().join(".ee.toml"), WORKSPACE_MCP_TOML).unwrap();

    let mut app = App::from_path(None).unwrap();
    let state = temp.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    app.agents.test_trust_store_base = Some(state.clone());
    app.agents.test_secret_store = Some(fixture.store);
    WorkspaceTrustStore::at(&state, temp.path())
        .unwrap()
        .set_decision(WorkspaceTrustDecision::Trusted)
        .unwrap();

    let resolved = app.resolve_mcp_servers();
    assert_eq!(resolved.len(), 1, "missing secret skips only the referencing server");
    assert_eq!(resolved[0].id, "tools");
    let secrets = app.agents_secret_values();
    assert!(secrets.contains(&String::from("ghp-resolved-token")));
    assert!(!secrets.iter().any(|secret| secret.contains("secret://")));
}
