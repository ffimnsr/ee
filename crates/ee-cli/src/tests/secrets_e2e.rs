//! Phase 6 end-to-end fixtures: encrypted secret → global config reference →
//! agent launch environment, entirely through fakes.
//!
//! No developer keychain, machine ID, network, or real OpenRouter endpoint is
//! ever touched: the vault uses fake keychain/host-binding layers, secrets
//! are created through the fake CLI input path, config layers live in temp
//! directories, and the agent "process" is an in-process fake transport.
//!
//! The core fixtures run under default features (store + config + resolver);
//! the launch fixtures additionally require the `agents` feature.

use std::io::Cursor;
use std::path::PathBuf;

use crate::config::{
    AgentEnvValue, AgentServerSettings, ConfigLayerKind, load_config_for_test,
    test_config_environment, write_config_layer,
};
use crate::secrets::cli::{HiddenTerminalSecretSource, run_secrets_set};
use crate::secrets::test_support::{ScriptedKeychain, StoredKeychain};
use crate::secrets::{HostBinding, SecretName, SecretStore, SecretStoreError, resolve};

const SEEDED: &str = "sk-secret-OPENROUTER-9876";
const REFERENCE: &str = "secret://openrouter-api-key";

const GLOBAL_REF_TOML: &str = r#"
[agents]
enabled = true

[agents.servers.fake]
command = "unused"
env = { OPENROUTER_API_KEY = "secret://openrouter-api-key" }
"#;

// ── Fixture: fake keychain + host binding + vault ───────────────────────────

struct E2eStore {
    _dir: tempfile::TempDir,
    keychain: StoredKeychain,
    vault_path: PathBuf,
    binding: HostBinding,
    store: SecretStore,
}

impl E2eStore {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let keychain = StoredKeychain::new();
        let binding = HostBinding::from_identifier_bytes(b"e2e-machine-id\n").expect("valid");
        let vault_path = dir.path().join("ee").join("secrets").join("v1.json");
        let store =
            SecretStore::new(Box::new(keychain.clone()), binding.clone(), vault_path.clone());
        Self { _dir: dir, keychain, vault_path, binding, store }
    }

    /// Creates a secret through the fake CLI `set` input path.
    fn create_secret(&self, name: &str, value: &str) {
        let mut out = Vec::new();
        let mut stdin = Cursor::new(format!("{value}\n").into_bytes());
        let mut terminal = HiddenTerminalSecretSource;
        run_secrets_set(
            &self.store,
            &SecretName::new(name).expect("valid name"),
            true,
            &mut stdin,
            &mut terminal,
            &mut out,
        )
        .expect("set through fake CLI input");
        assert_eq!(String::from_utf8(out).expect("utf-8"), format!("set secret {name}\n"));
    }

    fn vault_json(&self) -> String {
        std::fs::read_to_string(&self.vault_path).expect("vault file")
    }
}

fn env_value(raw: &str) -> AgentEnvValue {
    AgentEnvValue { layer: ConfigLayerKind::UserXdg, raw: raw.to_owned() }
}

/// A server definition whose env references the seeded secret.
fn referencing_server() -> AgentServerSettings {
    let mut env = std::collections::BTreeMap::new();
    env.insert(String::from("OPENROUTER_API_KEY"), env_value(REFERENCE));
    AgentServerSettings { command: String::from("unused"), args: Vec::new(), env, cwd: None }
}

// ── Positive fixture ─────────────────────────────────────────────────────────

#[test]
fn secrets_e2e_global_reference_reaches_launch_config_never_user_visible() {
    // Create the secret through the fake CLI input path.
    let fixture = E2eStore::new();
    fixture.create_secret("openrouter-api-key", SEEDED);

    // Global (XDG) config references the stored secret.
    let dir = tempfile::tempdir().expect("temp dir");
    let env = test_config_environment(dir.path());
    write_config_layer(&env, ConfigLayerKind::UserXdg, GLOBAL_REF_TOML);
    let settings = load_config_for_test(&env);
    let server = settings.agents.servers.get("fake").expect("merged server");
    let value = server.env.get("OPENROUTER_API_KEY").expect("env value");
    assert_eq!(value.raw, REFERENCE, "raw reference preserved through merge");
    assert_eq!(value.layer, ConfigLayerKind::UserXdg);

    // Build the launch environment without spawning any process.
    let launch_env = resolve::resolve_agent_env(&fixture.store, server).expect("all resolved");
    assert_eq!(
        launch_env.get("OPENROUTER_API_KEY").map(String::as_str),
        Some(SEEDED),
        "seeded plaintext only inside the launch environment"
    );

    // No captured user-visible output carries the plaintext.
    assert!(!fixture.vault_json().contains(SEEDED), "vault JSON omits plaintext");
    let mut status_out = Vec::new();
    crate::secrets::cli::run_secrets_status(&fixture.store, &mut status_out).expect("status");
    assert!(
        !String::from_utf8(status_out).expect("utf-8").contains(SEEDED),
        "status output omits plaintext"
    );
    let show = toml::to_string_pretty(&crate::config::resolved_config_with_env(None, &env))
        .expect("config document");
    assert!(show.contains(REFERENCE), "config output shows the reference");
    assert!(!show.contains(SEEDED), "config output omits plaintext");
}

// ── Negative fixtures ────────────────────────────────────────────────────────

#[test]
fn secrets_e2e_missing_referenced_secret_prevents_launch_env() {
    // Vault exists but the referenced secret was never created.
    let fixture = E2eStore::new();
    let err =
        resolve::resolve_agent_env(&fixture.store, &referencing_server()).expect_err("must fail");
    assert!(matches!(err, SecretStoreError::NotFound));
    assert!(!err.to_string().contains(SEEDED));
}

#[test]
fn secrets_e2e_copied_vault_under_different_binding_prevents_launch_env() {
    let fixture = E2eStore::new();
    fixture.create_secret("openrouter-api-key", SEEDED);

    // Same vault file and keychain content, different host: fail closed.
    let other_binding =
        HostBinding::from_identifier_bytes(b"other-e2e-machine-id\n").expect("valid");
    let foreign = SecretStore::new(
        Box::new(fixture.keychain.clone()),
        other_binding,
        fixture.vault_path.clone(),
    );
    let err = resolve::resolve_agent_env(&foreign, &referencing_server()).expect_err("must fail");
    assert!(matches!(err, SecretStoreError::HostBindingMismatch { version: 1 }));
    assert!(!err.to_string().contains(SEEDED), "mismatch error hides the secret");
}

#[test]
fn secrets_e2e_unavailable_keychain_prevents_launch_env() {
    let fixture = E2eStore::new();
    fixture.create_secret("openrouter-api-key", SEEDED);

    let broken = SecretStore::new(
        Box::new(ScriptedKeychain::new(vec![Err(SecretStoreError::KeychainUnavailable)], vec![])),
        fixture.binding.clone(),
        fixture.vault_path.clone(),
    );
    let err = resolve::resolve_agent_env(&broken, &referencing_server()).expect_err("must fail");
    assert!(matches!(err, SecretStoreError::KeychainUnavailable));
    assert!(!err.to_string().contains(SEEDED));
}

#[test]
fn secrets_e2e_corrupt_vault_prevents_launch_env() {
    let fixture = E2eStore::new();
    fixture.create_secret("openrouter-api-key", SEEDED);

    // Tamper the ciphertext: valid base64, wrong bytes.
    let mut json: serde_json::Value = serde_json::from_str(&fixture.vault_json()).expect("json");
    use base64::Engine as _;
    json["records"][0]["ciphertext"] =
        serde_json::Value::String(base64::engine::general_purpose::STANDARD.encode([0u8; 16]));
    std::fs::write(&fixture.vault_path, serde_json::to_string(&json).expect("serialize"))
        .expect("tamper write");

    let err =
        resolve::resolve_agent_env(&fixture.store, &referencing_server()).expect_err("must fail");
    assert!(matches!(err, SecretStoreError::VaultCorruption));
    assert!(!err.to_string().contains(SEEDED), "corruption error hides the secret");
}

#[test]
fn secrets_e2e_project_reference_rejected_while_project_literals_supported() {
    let dir = tempfile::tempdir().expect("temp dir");
    let env = test_config_environment(dir.path());
    write_config_layer(
        &env,
        ConfigLayerKind::Ancestor,
        r#"
[agents.servers.referencing]
command = "unused"
env = { OPENROUTER_API_KEY = "secret://openrouter-api-key" }

[agents.servers.literal]
command = "unused"
env = { OPENROUTER_API_KEY = "sk-project-literal" }
"#,
    );
    let settings = load_config_for_test(&env);

    // The workspace reference cannot create a launch configuration at all.
    assert!(
        !settings.agents.servers.contains_key("referencing"),
        "project secret reference rejected at merge"
    );

    // Project literal env stays fully supported through the existing path.
    let literal = settings.agents.servers.get("literal").expect("literal server");
    let fixture = E2eStore::new();
    let launch = resolve::resolve_agent_env(&fixture.store, literal).expect("literal launch");
    assert_eq!(launch.get("OPENROUTER_API_KEY").map(String::as_str), Some("sk-project-literal"));
    assert_eq!(fixture.keychain.load_calls(), 0, "literals never touch the store");
}

// ── Legacy behavior regressions ──────────────────────────────────────────────

#[test]
fn secrets_e2e_literal_api_key_reaches_launch_unchanged() {
    let fixture = E2eStore::new();
    let dir = tempfile::tempdir().expect("temp dir");
    let env = test_config_environment(dir.path());
    write_config_layer(
        &env,
        ConfigLayerKind::UserXdg,
        r#"
[agents.servers.fake]
command = "unused"
env = { OPENROUTER_API_KEY = "sk-literal-111" }
"#,
    );
    let settings = load_config_for_test(&env);
    let server = settings.agents.servers.get("fake").expect("merged server");
    assert_eq!(server.env.get("OPENROUTER_API_KEY").expect("env").raw, "sk-literal-111");

    let loads_before = fixture.keychain.load_calls();
    let launch = resolve::resolve_agent_env(&fixture.store, server).expect("literal launch");
    assert_eq!(
        launch.get("OPENROUTER_API_KEY").map(String::as_str),
        Some("sk-literal-111"),
        "literal reaches launch configuration unchanged"
    );
    assert_eq!(fixture.keychain.load_calls(), loads_before, "no store interaction");
}

#[test]
fn secrets_e2e_agent_without_references_launches_through_existing_path() {
    let fixture = E2eStore::new();
    let mut env = std::collections::BTreeMap::new();
    env.insert(String::from("LANG"), env_value("en_US.UTF-8"));
    let server =
        AgentServerSettings { command: String::from("unused"), args: Vec::new(), env, cwd: None };
    assert!(!resolve::agent_env_has_references(&server.env));

    // Works even though the vault has no key and the keychain is empty.
    let launch = resolve::resolve_agent_env(&fixture.store, &server).expect("literal launch");
    assert_eq!(launch.get("LANG").map(String::as_str), Some("en_US.UTF-8"));
    assert_eq!(fixture.keychain.load_calls(), 0);
}

// ── Launch fixtures (agents feature) ─────────────────────────────────────────

#[cfg(feature = "agents")]
mod launch {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use ee_agent_host::FakeTransportFactory;
    use ee_agent_host::fake::{FakeAgent, FakeAgentScript, FakeAgentTransport};
    use serde_json::json;

    use crate::app::App;
    use crate::tests::helpers::{CurrentDirGuard, EnvVarGuard, run_ex};

    const WAIT: Duration = Duration::from_secs(5);

    fn base_script() -> FakeAgentScript {
        FakeAgentScript::new()
            .wait_for("initialize")
            .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
            .wait_for("session/new")
            .respond(json!({ "sessionId": "s1" }))
    }

    #[derive(Clone)]
    struct ScriptedFake {
        script: FakeAgentScript,
        spawned: Arc<Mutex<bool>>,
    }

    impl FakeTransportFactory for ScriptedFake {
        fn build(&self) -> FakeAgentTransport {
            *self.spawned.lock().expect("spawned lock") = true;
            let (_fake, transport) = FakeAgent::spawn(self.script.clone());
            transport
        }
    }

    fn wait_until(app: &mut App, label: &str, mut condition: impl FnMut(&App) -> bool) {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            app.pump_agents();
            let _ = app.backend.drain_events();
            if condition(app) {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "timed out waiting for {label}; threads={} error={:?} status={:?}",
            app.agents.threads.len(),
            app.agents.error,
            app.backend.status_message.as_deref()
        );
    }

    #[test]
    fn secrets_e2e_agent_launch_resolves_reference_and_redacts_stderr() {
        let fixture = E2eStore::new();
        fixture.create_secret("openrouter-api-key", SEEDED);

        let temp = tempfile::tempdir().unwrap();
        let xdg = temp.path().join("xdg");
        let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
        let _cwd_restore = CurrentDirGuard::capture();
        std::env::set_current_dir(temp.path()).unwrap();
        let _xdg_guard = EnvVarGuard::set("XDG_CONFIG_HOME", xdg.clone());
        std::fs::create_dir_all(xdg.join("ee")).unwrap();
        std::fs::write(xdg.join("ee").join("config.toml"), GLOBAL_REF_TOML).unwrap();
        let mut app = App::from_path(None).unwrap();
        app.agents.test_secret_store = Some(fixture.store);
        let fake = ScriptedFake { script: base_script(), spawned: Arc::new(Mutex::new(false)) };
        app.agents.test_fake_transports.insert(String::from("fake"), Arc::new(fake.clone()));

        run_ex(&mut app, "agents");
        wait_until(&mut app, "fake agent session ready", |app| {
            app.agents.threads.len() == 1
                && app.agents.threads[0].state == crate::app::ThreadUiState::Ready
        });

        // The fake transport (stand-in for the child process) was created.
        assert!(*fake.spawned.lock().unwrap(), "agent launch happened");

        // The resolved secret is collected and redacts stderr/diagnostics.
        let secrets = app.agents_secret_values();
        assert!(secrets.contains(&String::from(SEEDED)), "resolved value collected");
        let redacted = ee_agent_host::redact::redact_secret_values(
            &format!("stderr: using {SEEDED} now"),
            &secrets,
        );
        assert_eq!(redacted, "stderr: using *** now");
        assert!(!redacted.contains(SEEDED));
        drop(_xdg_guard);
        drop(_cwd_restore);
        drop(_cwd_lock);
    }

    #[test]
    fn secrets_e2e_missing_referenced_secret_never_spawns_fake_process() {
        // No secret was created: the vault does not exist.
        let fixture = E2eStore::new();

        let temp = tempfile::tempdir().unwrap();
        let xdg = temp.path().join("xdg");
        let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
        let _cwd_restore = CurrentDirGuard::capture();
        std::env::set_current_dir(temp.path()).unwrap();
        let _xdg_guard = EnvVarGuard::set("XDG_CONFIG_HOME", xdg.clone());
        std::fs::create_dir_all(xdg.join("ee")).unwrap();
        std::fs::write(xdg.join("ee").join("config.toml"), GLOBAL_REF_TOML).unwrap();
        let mut app = App::from_path(None).unwrap();
        app.agents.test_secret_store = Some(fixture.store);
        let fake = ScriptedFake { script: base_script(), spawned: Arc::new(Mutex::new(false)) };
        app.agents.test_fake_transports.insert(String::from("fake"), Arc::new(fake.clone()));

        run_ex(&mut app, "agents");
        // Give the pane a bounded chance to start a session; it must not.
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            app.pump_agents();
            let _ = app.backend.drain_events();
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !*fake.spawned.lock().unwrap(),
            "missing referenced secret must prevent fake process creation"
        );
        assert!(app.agents.threads.is_empty(), "no session thread for a failed launch");
        assert!(!app.agents_secret_values().contains(&String::from(SEEDED)));
        drop(_xdg_guard);
        drop(_cwd_restore);
        drop(_cwd_lock);
    }
}
