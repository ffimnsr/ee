use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::super::*;
use super::common::*;
// The process cwd is process-global; lock it while mutating.
#[test]
fn agents_settings_disabled_by_default() {
    let settings = EditorSettings::default();

    assert!(!settings.agents.enabled);
    assert!(settings.agents.default_agent.is_none());
    assert_eq!(settings.agents.max_concurrent_prompts, 4);
    assert!(settings.agents.servers.is_empty());
    assert!(settings.mcp.servers.is_empty());
}
#[test]
fn ee_toml_parses_agents_settings() {
    let toml = r#"
[agents]
enabled = true
default_agent = "helper"
max_concurrent_prompts = 2

[agents.servers.helper]
label = "Local Helper"
command = "ee-helper"
args = ["serve"]
env = { EE_AGENT_MODE = "1" }
cwd = "/tmp/agent"

[agents.servers.other]
command = "other-agent"
"#;
    let raw: EeToml = toml::from_str(toml).unwrap();
    assert_eq!(raw.agents.as_ref().unwrap().enabled, Some(true));

    let mut settings = EditorSettings::default();
    settings.merge_toml(&raw, ConfigLayerKind::UserXdg);

    assert!(settings.agents.enabled);
    assert_eq!(settings.agents.default_agent.as_deref(), Some("helper"));
    assert_eq!(settings.agents.max_concurrent_prompts, 2);
    let helper = settings.agents.servers.get("helper").unwrap();
    assert_eq!(helper.label.as_deref(), Some("Local Helper"));
    assert_eq!(helper.command, "ee-helper");
    assert_eq!(helper.args, vec!["serve"]);
    assert_eq!(helper.env.get("EE_AGENT_MODE").map(|v| v.raw.as_str()), Some("1"));
    assert_eq!(helper.cwd.as_deref(), Some(Path::new("/tmp/agent")));
    assert_eq!(settings.agents.servers.len(), 2);
}
#[test]
fn agent_env_secret_reference_from_xdg_layer_is_preserved() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(env.cwd.as_path()).unwrap();
    write_config_layer(&env, ConfigLayerKind::UserXdg, AGENT_REF_TOML);

    let settings = load_for(&env);
    let server = settings.agents.servers.get("gh").expect("server merged");
    let key = server.env.get("OPENROUTER_API_KEY").expect("env value");
    assert_eq!(key.raw, "secret://openrouter-api-key", "raw reference preserved");
    assert_eq!(key.layer, ConfigLayerKind::UserXdg);
    let literal = server.env.get("LANG").expect("literal");
    assert_eq!(literal.raw, "en_US.UTF-8");
    assert_eq!(literal.layer, ConfigLayerKind::UserXdg);
}
#[test]
fn agent_env_secret_reference_from_legacy_user_layer_is_preserved() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(env.cwd.as_path()).unwrap();
    write_config_layer(&env, ConfigLayerKind::UserLegacy, AGENT_REF_TOML);

    let settings = load_for(&env);
    let key = settings
        .agents
        .servers
        .get("gh")
        .expect("server merged")
        .env
        .get("OPENROUTER_API_KEY")
        .expect("env value");
    assert_eq!(key.raw, "secret://openrouter-api-key");
    assert_eq!(key.layer, ConfigLayerKind::UserLegacy);
}
#[test]
fn agent_env_secret_reference_from_system_layer_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(env.cwd.as_path()).unwrap();
    write_config_layer(&env, ConfigLayerKind::System, AGENT_REF_TOML);

    let settings = load_for(&env);
    assert!(
        !settings.agents.servers.contains_key("gh"),
        "system-layer secret reference must not merge"
    );
}
#[test]
fn agent_env_secret_reference_from_ancestor_layer_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(env.cwd.as_path()).unwrap();
    write_config_layer(&env, ConfigLayerKind::Ancestor, AGENT_REF_TOML);

    let settings = load_for(&env);
    assert!(
        !settings.agents.servers.contains_key("gh"),
        "ancestor-layer secret reference must not merge"
    );
}
#[test]
fn agent_env_project_literal_override_wins_over_global_literal() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(env.cwd.as_path()).unwrap();
    write_config_layer(
        &env,
        ConfigLayerKind::UserXdg,
        r#"
[agents.servers.gh]
command = "agent-bin"
env = { OPENROUTER_API_KEY = "global-literal" }
"#,
    );
    write_config_layer(
        &env,
        ConfigLayerKind::Ancestor,
        r#"
[agents.servers.gh]
command = "agent-bin"
env = { OPENROUTER_API_KEY = "project-literal" }
"#,
    );

    let settings = load_for(&env);
    let key = settings
        .agents
        .servers
        .get("gh")
        .expect("server merged")
        .env
        .get("OPENROUTER_API_KEY")
        .expect("env value");
    assert_eq!(key.raw, "project-literal", "higher-priority literal replaces global");
    assert_eq!(key.layer, ConfigLayerKind::Ancestor);
}
#[test]
fn rejected_ancestor_reference_keeps_lower_layer_server() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(env.cwd.as_path()).unwrap();
    write_config_layer(
        &env,
        ConfigLayerKind::UserXdg,
        r#"
[agents.servers.gh]
command = "agent-bin"
env = { OPENROUTER_API_KEY = "global-literal" }
"#,
    );
    // The ancestor cannot override with, or cause launch of, a reference.
    write_config_layer(&env, ConfigLayerKind::Ancestor, AGENT_REF_TOML);

    let settings = load_for(&env);
    let key = settings
        .agents
        .servers
        .get("gh")
        .expect("lower-layer server survives")
        .env
        .get("OPENROUTER_API_KEY")
        .expect("env value");
    assert_eq!(key.raw, "global-literal");
}
#[test]
fn validate_agent_server_rejects_malformed_secret_reference_with_field_path() {
    let server = AgentServerToml {
        label: None,
        command: Some(String::from("agent-bin")),
        args: None,
        env: BTreeMap::from([(
            String::from("OPENROUTER_API_KEY"),
            String::from("secret://bad name"),
        )]),
        cwd: None,
    };
    let err = validate_agent_server("gh", &server).expect_err("rejected");
    assert!(err.contains("agents.servers.gh.env.OPENROUTER_API_KEY"), "field path: {err}");
    assert!(!err.contains("bad name"), "no raw value echo: {err}");
}
#[test]
fn agent_env_secret_reference_substring_stays_literal() {
    let server = AgentServerToml {
        label: None,
        command: Some(String::from("agent-bin")),
        args: None,
        env: BTreeMap::from([
            (
                String::from("ENDPOINT"),
                String::from("https://api.example.com/secret://openrouter-api-key"),
            ),
            (String::from("NOTE"), String::from("see secret://docs")),
        ]),
        cwd: None,
    };
    let resolved =
        merge_agent_server("gh", &server, None, ConfigLayerKind::Ancestor).expect("literals");
    assert_eq!(
        resolved.env.get("ENDPOINT").expect("literal").raw,
        "https://api.example.com/secret://openrouter-api-key"
    );
    assert_eq!(resolved.env.get("NOTE").expect("literal").raw, "see secret://docs");
    // Even from an ancestor layer, substrings are never treated as refs.
    assert_eq!(resolved.env.get("ENDPOINT").expect("literal").layer, ConfigLayerKind::Ancestor);
}
#[test]
fn config_show_preserves_secret_reference_text() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(env.cwd.as_path()).unwrap();
    write_config_layer(&env, ConfigLayerKind::UserXdg, AGENT_REF_TOML);

    let document = toml::to_string_pretty(&resolved_config_with_env(None, &env)).unwrap();
    assert!(
        document.contains("secret://openrouter-api-key"),
        "config show exposes the reference, never the plaintext: {document}"
    );
    assert!(!document.contains("sk-live"), "no resolved plaintext in show output");
}
#[test]
fn project_config_enables_agents_while_defaults_stay_disabled() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    let file = project.join("main.rs");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();

    // Built-in defaults keep agents disabled even with no config layers.
    let defaults = load_config_with_env(Some(&file), &env);
    assert!(!defaults.agents.enabled);
    assert!(defaults.agents.servers.is_empty());

    // User layer defines an agent server but never enables agents mode.
    std::fs::write(
        env.config_dir.as_ref().unwrap().join("ee").join("config.toml"),
        "[agents.servers.user-agent]\ncommand = \"user-agent\"\n",
    )
    .unwrap();

    // Project-local `.ee.toml` enables agents and refines the server.
    std::fs::write(
            project.join(".ee.toml"),
            "[agents]\nenabled = true\ndefault_agent = \"user-agent\"\n\n[agents.servers.user-agent]\ncommand = \"user-agent\"\nargs = [\"--stdio\"]\n",
        )
        .unwrap();

    let settings = load_config_with_env(Some(&file), &env);
    assert!(settings.agents.enabled);
    assert_eq!(settings.agents.default_agent.as_deref(), Some("user-agent"));
    let agent = settings.agents.servers.get("user-agent").unwrap();
    assert_eq!(agent.command, "user-agent");
    assert_eq!(agent.args, vec!["--stdio"]);
}
#[test]
fn validate_config_file_rejects_empty_agent_command() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(".ee.toml");
    std::fs::write(&path, "[agents.servers.broken]\ncommand = \"\"\n").unwrap();

    let error = validate_config_file(&path).unwrap_err();

    assert!(error.contains("Config validation error"));
    assert!(error.contains("agents server `broken`"));
    assert!(error.contains("command must not be empty"));
}
// Agent `env` documentation exposes the `secret://<name>` reference
// syntax and its user-global-only resolution boundary (phase 5)
// without changing the config shape.
// Serialize back through the full document shape: the resolved
// document carries the proxy flag.
#[test]
fn merged_config_document_keeps_agents_disabled_without_config() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(env.cwd.as_path()).unwrap();

    let text = toml::to_string_pretty(&resolved_config_with_env(None, &env)).unwrap();

    assert!(text.contains("enabled = false"));
    assert!(!text.contains("[mcp]"));
}
#[test]
fn rubber_duck_config_rejects_ambiguous_backend_and_roundtrips_limits() {
    let ambiguous = RubberDuckToml {
        internal_model_id: Some("critic-model".into()),
        external_agent_id: Some("critic-agent".into()),
        ..RubberDuckToml::default()
    };
    assert!(validate_rubber_duck_toml(&ambiguous).unwrap_err().contains("mutually exclusive"));

    let patch = RubberDuckToml {
        mode: Some("automatic".into()),
        external_agent_id: Some("critic-agent".into()),
        max_calls: Some(3),
        max_context_bytes: Some(4096),
        max_output_bytes: Some(2048),
        timeout_ms: Some(5000),
        ..RubberDuckToml::default()
    };
    let settings = merge_rubber_duck(&RubberDuckSettings::default(), &patch).unwrap();
    assert_eq!(settings.mode, RubberDuckModeSetting::Automatic);
    assert_eq!(settings.external_agent_id.as_deref(), Some("critic-agent"));
    assert_eq!(settings.max_calls, 3);
}
#[test]
fn rubber_duck_resolution_degrades_unknown_optional_agent_only() {
    let settings = RubberDuckSettings {
        external_agent_id: Some("missing".into()),
        ..RubberDuckSettings::default()
    };
    let resolved = settings
        .resolve_backend_policy(&BTreeSet::new(), &BTreeSet::from(["root".into()]))
        .unwrap();
    assert!(resolved.unavailable.is_some());
    assert!(!resolved.critic_available());
}
#[test]
fn agent_setup_writes_complete_server_to_global_config_only() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let config_dir = env.config_dir.as_ref().expect("test config directory");
    std::fs::create_dir_all(config_dir.join("ee")).unwrap();
    std::fs::write(
        config_dir.join("ee").join("config.toml"),
        "wrap_lines = true\n[agents.servers.existing]\ncommand = \"existing-agent\"\n",
    )
    .unwrap();
    let values = BTreeMap::from([
        (
            String::from("OPENROUTER_API_KEY"),
            String::from("secret://agent.openrouter.OPENROUTER_API_KEY"),
        ),
        (String::from("OPENROUTER_MODEL"), String::from("example/model")),
        (String::from("OPENROUTER_MAX_ITERATIONS"), String::from("32")),
    ]);

    let path = configure_global_agent_server_with_env(
        "openrouter",
        Path::new("/home/example/.local/bin/ee-openrouter-agent"),
        &[String::from("--stdio")],
        &values,
        &env,
    )
    .expect("configure global agent");
    let document: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();

    assert_eq!(path, config_dir.join("ee").join("config.toml"));
    assert!(!env.cwd.join(".ee.toml").exists());
    assert_eq!(document["wrap_lines"].as_bool(), Some(true));
    assert_eq!(document["agents"]["enabled"].as_bool(), Some(true));
    assert_eq!(document["agents"]["default_agent"].as_str(), Some("openrouter"));
    assert_eq!(
        document["agents"]["servers"]["openrouter"]["command"].as_str(),
        Some("/home/example/.local/bin/ee-openrouter-agent")
    );
    assert_eq!(
        document["agents"]["servers"]["openrouter"]["args"],
        toml::Value::Array(vec![toml::Value::String(String::from("--stdio"))])
    );
    assert_eq!(
        document["agents"]["servers"]["openrouter"]["env"]["OPENROUTER_API_KEY"].as_str(),
        Some("secret://agent.openrouter.OPENROUTER_API_KEY")
    );
    assert_eq!(
        document["agents"]["servers"]["openrouter"]["env"]["OPENROUTER_MAX_ITERATIONS"].as_str(),
        Some("32")
    );
    assert_eq!(
        document["agents"]["servers"]["existing"]["command"].as_str(),
        Some("existing-agent")
    );
}
