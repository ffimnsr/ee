use super::super::*;
use std::collections::BTreeMap;

use serde_json::Value;
// The process cwd is process-global; lock it while mutating.
// The ancestor cannot override with, or cause launch of, a reference.
// Even from an ancestor layer, substrings are never treated as refs.
#[test]
fn ee_toml_parses_mcp_servers() {
    let toml = r#"
[mcp.servers.filesystem]
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem"]

[mcp.servers.remote]
transport = "streamable_http"
url = "https://example.com/mcp"
headers = { Authorization = "Bearer token" }
"#;
    let raw: EeToml = toml::from_str(toml).unwrap();

    let mut settings = EditorSettings::default();
    settings.merge_toml(&raw, ConfigLayerKind::UserXdg);

    let servers = &settings.mcp.servers;
    match servers.get("filesystem").unwrap() {
        McpServerSettings::Stdio { command, args, env, cwd } => {
            assert_eq!(command, "npx");
            assert_eq!(
                args,
                &vec!["-y".to_owned(), "@modelcontextprotocol/server-filesystem".to_owned()]
            );
            assert!(env.is_empty());
            assert!(cwd.is_none());
        }
        other => panic!("expected stdio transport, got {other:?}"),
    }
    match servers.get("remote").unwrap() {
        McpServerSettings::StreamableHttp { url, headers, timeout_ms } => {
            assert_eq!(url, "https://example.com/mcp");
            assert_eq!(
                headers.get("Authorization").map(|value| value.raw.as_str()),
                Some("Bearer token")
            );
            assert_eq!(*timeout_ms, DEFAULT_MCP_HTTP_TIMEOUT_MS);
        }
        other => panic!("expected streamable_http transport, got {other:?}"),
    }
}
#[test]
fn ee_toml_rejects_unknown_agents_and_mcp_fields() {
    let err = toml::from_str::<EeToml>("[agents]\nbogus = true\n").unwrap_err();
    assert!(err.to_string().contains("unknown field `bogus`"));

    let err = toml::from_str::<EeToml>("[mcp.servers.foo]\ntransport = \"stdio\"\nbogus = true\n")
        .unwrap_err();
    assert!(err.to_string().contains("unknown field `bogus`"));
}
#[test]
fn ee_toml_mcp_server_requires_transport() {
    let err = toml::from_str::<EeToml>("[mcp.servers.foo]\ncommand = \"x\"\n").unwrap_err();
    assert!(err.to_string().contains("transport"));
}
// Built-in defaults keep agents disabled even with no config layers.
// User layer defines an agent server but never enables agents mode.
// Project-local `.ee.toml` enables agents and refines the server.
#[test]
fn validate_config_file_rejects_invalid_mcp_url() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(".ee.toml");

    std::fs::write(
        &path,
        "[mcp.servers.remote]\ntransport = \"streamable_http\"\nurl = \"ftp://example.com/mcp\"\n",
    )
    .unwrap();
    let error = validate_config_file(&path).unwrap_err();
    assert!(error.contains("mcp server `remote`"));
    assert!(error.contains("scheme must be http or https"));

    std::fs::write(
        &path,
        "[mcp.servers.remote]\ntransport = \"streamable_http\"\nurl = \"not a url\"\n",
    )
    .unwrap();
    let error = validate_config_file(&path).unwrap_err();
    assert!(error.contains("invalid mcp url"));
}
#[test]
fn validate_config_file_rejects_empty_server_ids() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(".ee.toml");

    std::fs::write(&path, "[agents.servers.\"\"]\ncommand = \"x\"\n").unwrap();
    let error = validate_config_file(&path).unwrap_err();
    assert!(error.contains("agent server id must not be empty"));

    std::fs::write(&path, "[mcp.servers.\"\"]\ntransport = \"stdio\"\ncommand = \"x\"\n").unwrap();
    let error = validate_config_file(&path).unwrap_err();
    assert!(error.contains("mcp server id must not be empty"));
}
#[test]
fn validate_config_file_rejects_duplicate_effective_server_ids() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(".ee.toml");
    std::fs::write(
            &path,
            "[agents.servers.dup]\ncommand = \"agent\"\n\n[mcp.servers.dup]\ntransport = \"stdio\"\ncommand = \"mcp-server\"\n",
        )
        .unwrap();

    let error = validate_config_file(&path).unwrap_err();

    assert!(error.contains("Config validation error"));
    assert!(error.contains("duplicate effective server id `dup`"));
}
#[test]
fn config_schema_includes_agents_and_mcp_fields() {
    let schema: Value = serde_json::from_str(&config_schema_json().unwrap()).unwrap();
    let properties = schema.get("properties").and_then(Value::as_object).unwrap();
    assert!(properties.contains_key("agents"));
    assert!(properties.contains_key("mcp"));

    let defs = schema.get("$defs").and_then(Value::as_object).unwrap();
    let agents = defs.get("AgentsToml").unwrap().get("properties").unwrap();
    assert!(agents.get("enabled").is_some());
    assert!(agents.get("default_agent").is_some());
    assert!(agents.get("max_concurrent_prompts").is_some());
    assert!(agents.get("workspace_memory").is_some());
    assert!(agents.get("servers").is_some());
    let workspace_memory = defs.get("WorkspaceMemoryToml").unwrap().get("properties").unwrap();
    for field in [
        "enabled",
        "max_value_bytes",
        "max_active_facts",
        "max_active_bytes",
        "max_recall_results",
        "busy_timeout_ms",
        "default_expiry_days",
        "candidate_retention_days",
        "stale_retention_days",
        "superseded_retention_days",
    ] {
        assert!(workspace_memory.get(field).is_some(), "missing workspace-memory field {field}");
    }

    let mcp = defs.get("McpToml").unwrap().get("properties").unwrap();
    assert!(mcp.get("servers").is_some());
    assert!(mcp.get("proxy").is_some());
    let proxy = defs.get("McpProxyToml").unwrap().get("properties").unwrap();
    assert!(proxy.get("enabled").is_some());

    // Agent `env` documentation exposes the `secret://<name>` reference
    // syntax and its workspace trust resolution boundary without changing
    // the config shape.
    let agent_server = defs.get("AgentServerToml").unwrap();
    let env_schema =
        agent_server.get("properties").and_then(|p| p.get("env")).expect("env property");
    let description = env_schema.get("description").and_then(Value::as_str).expect("description");
    assert!(description.contains("secret://<name>"), "documents reference syntax");
    assert!(
        description.contains("workspace trust decision"),
        "documents the workspace trust boundary"
    );
    assert!(
        !env_schema.as_object().unwrap().contains_key("pattern"),
        "reference syntax does not narrow the config shape"
    );

    let web_context = defs.get("AgentWebContextToml").unwrap();
    let web_properties = web_context.get("properties").and_then(Value::as_object).unwrap();
    for field in ["backend", "provider_secret_reference", "exa", "brave_llm_context", "tavily"] {
        assert!(web_properties.contains_key(field), "missing web-context schema field {field}");
    }
    assert!(!web_properties.contains_key("endpoint_override"));
    let backends = defs
        .get("WebContextBackendToml")
        .and_then(|value| value.get("enum"))
        .and_then(Value::as_array)
        .unwrap();
    assert_eq!(
        backends,
        &[
            Value::String(String::from("searxng")),
            Value::String(String::from("exa")),
            Value::String(String::from("brave_llm_context")),
            Value::String(String::from("tavily")),
        ]
    );
}
#[test]
fn mcp_proxy_disabled_by_default_and_parsed_from_toml() {
    assert!(!EditorSettings::default().mcp.proxy.enabled);

    let toml = r#"
[mcp.proxy]
enabled = true
"#;
    let raw: EeToml = toml::from_str(toml).unwrap();
    let mut settings = EditorSettings::default();
    settings.merge_toml(&raw, ConfigLayerKind::UserXdg);
    assert!(settings.mcp.proxy.enabled);

    // Serialize back through the full document shape: the resolved
    // document carries the proxy flag.
    let document = EeToml {
        mcp: Some(McpToml {
            proxy: Some(McpProxyToml { enabled: Some(true) }),
            servers: BTreeMap::new(),
        }),
        ..Default::default()
    };
    let text = toml::to_string(&document).unwrap();
    let roundtrip: EeToml = toml::from_str(&text).unwrap();
    let mut restored = EditorSettings::default();
    restored.merge_toml(&roundtrip, ConfigLayerKind::UserXdg);
    assert!(restored.mcp.proxy.enabled);
}
#[test]
fn mcp_proxy_defaults_on_when_agents_enabled() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(&env.cwd).unwrap();
    write_config_layer(&env, ConfigLayerKind::Ancestor, "[agents]\nenabled = true\n");
    let settings = load_config_with_env(None, &env);
    assert!(settings.mcp.proxy.enabled, "proxy defaults on with agents mode");
}
#[test]
fn mcp_proxy_explicit_opt_out_wins_over_default() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(&env.cwd).unwrap();
    write_config_layer(
        &env,
        ConfigLayerKind::Ancestor,
        "[agents]\nenabled = true\n\n[mcp.proxy]\nenabled = false\n",
    );
    let settings = load_config_with_env(None, &env);
    assert!(!settings.mcp.proxy.enabled);
}
#[test]
fn mcp_proxy_stays_off_without_agents() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(&env.cwd).unwrap();
    let settings = load_config_with_env(None, &env);
    assert!(!settings.mcp.proxy.enabled);
}
#[test]
fn mcp_settings_to_toml_includes_proxy_when_enabled() {
    let settings = EditorSettings::default();
    assert!(mcp_settings_to_toml(&settings.mcp).is_none());

    let mut enabled = EditorSettings::default();
    enabled.mcp.proxy.enabled = true;
    let toml = mcp_settings_to_toml(&enabled.mcp).expect("proxy present");
    assert_eq!(toml.proxy.as_ref().and_then(|p| p.enabled), Some(true));
}
#[test]
fn merged_config_document_includes_agents_and_mcp() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    let file = project.join("main.rs");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
            project.join(".ee.toml"),
            "[agents]\nenabled = true\ndefault_agent = \"helper\"\n\n[agents.servers.helper]\ncommand = \"helper-agent\"\n\n[mcp.servers.tools]\ntransport = \"stdio\"\ncommand = \"mcp-tools\"\n",
        )
        .unwrap();

    let text = toml::to_string_pretty(&resolved_config_with_env(Some(&file), &env)).unwrap();

    assert!(text.contains("enabled = true"));
    assert!(text.contains("default_agent = \"helper\""));
    assert!(text.contains("command = \"helper-agent\""));
    assert!(text.contains("transport = \"stdio\""));
    assert!(text.contains("command = \"mcp-tools\""));
}
#[test]
fn mcp_server_field_merge_completes_split_layer_secret_entry() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(&env.cwd).unwrap();
    std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
    // User layer: split-layer patch with only the secret env.
    write_config_layer(
        &env,
        ConfigLayerKind::UserXdg,
        "[mcp.servers.github]\ntransport = \"stdio\"\nenv = { GITHUB_TOKEN = \"ghp_secret\" }\n",
    );
    // Workspace layer: operational server definition.
    write_config_layer(
        &env,
        ConfigLayerKind::Ancestor,
        "[mcp.servers.github]\ntransport = \"stdio\"\ncommand = \"npx\"\nargs = [\"-y\", \"@modelcontextprotocol/server-github\"]\n",
    );

    let settings = load_config_with_env(None, &env);
    match settings.mcp.servers.get("github").expect("merged server") {
        McpServerSettings::Stdio { command, args, env, .. } => {
            assert_eq!(command, "npx");
            assert_eq!(
                args,
                &vec!["-y".to_owned(), "@modelcontextprotocol/server-github".to_owned()]
            );
            assert_eq!(env.get("GITHUB_TOKEN").map(|value| value.raw.as_str()), Some("ghp_secret"));
        }
        other => panic!("expected stdio transport, got {other:?}"),
    }
}
#[test]
fn mcp_partial_entry_without_completion_stays_inert() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(&env.cwd).unwrap();
    std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
    write_config_layer(
        &env,
        ConfigLayerKind::UserXdg,
        "[mcp.servers.orphan]\ntransport = \"stdio\"\nenv = { TOKEN = \"x\" }\n",
    );

    let settings = load_config_with_env(None, &env);
    assert!(settings.mcp.servers.is_empty(), "uncompleted partial stays inert");
    assert_eq!(settings.mcp.partial.len(), 1, "partial parked for later layers");
}
#[test]
fn mcp_http_headers_field_merge_and_transport_mismatch_replace() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(&env.cwd).unwrap();
    std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
    // User layer: partial streamable_http entry with secret headers.
    write_config_layer(
        &env,
        ConfigLayerKind::UserXdg,
        "[mcp.servers.remote]\ntransport = \"streamable_http\"\nheaders = { Authorization = \"Bearer abc\" }\n",
    );
    // Workspace layer: complete http entry, different id overrides nothing.
    write_config_layer(
        &env,
        ConfigLayerKind::Ancestor,
        "[mcp.servers.remote]\ntransport = \"streamable_http\"\nurl = \"https://example.com/mcp\"\n[mcp.servers.other]\ntransport = \"stdio\"\ncommand = \"x\"\n",
    );

    let settings = load_config_with_env(None, &env);
    match settings.mcp.servers.get("remote").expect("merged http server") {
        McpServerSettings::StreamableHttp { url, headers, timeout_ms } => {
            assert_eq!(url, "https://example.com/mcp");
            assert_eq!(
                headers.get("Authorization").map(|value| value.raw.as_str()),
                Some("Bearer abc")
            );
            assert_eq!(*timeout_ms, DEFAULT_MCP_HTTP_TIMEOUT_MS);
        }
        other => panic!("expected streamable_http transport, got {other:?}"),
    }
    assert!(settings.mcp.servers.contains_key("other"));
}
#[test]
fn mcp_transport_mismatch_between_layers_replaces_whole_entry() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(&env.cwd).unwrap();
    std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
    write_config_layer(
        &env,
        ConfigLayerKind::UserXdg,
        "[mcp.servers.mix]\ntransport = \"stdio\"\ncommand = \"old\"\nenv = { OLD = \"1\" }\n",
    );
    write_config_layer(
        &env,
        ConfigLayerKind::Ancestor,
        "[mcp.servers.mix]\ntransport = \"streamable_http\"\nurl = \"https://example.com/mcp\"\n",
    );

    let settings = load_config_with_env(None, &env);
    assert!(matches!(
        settings.mcp.servers.get("mix"),
        Some(McpServerSettings::StreamableHttp { url, .. }) if url == "https://example.com/mcp"
    ));
}
#[test]
fn higher_layer_transport_switch_without_completion_retires_lower_entry() {
    // A higher-priority layer that explicitly declares a different transport
    // redefines the server. Even when its own entry is an incomplete patch
    // (headers but no URL), the lower layer's resolved server must not stay
    // effective under the old transport.
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(&env.cwd).unwrap();
    std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
    write_config_layer(
        &env,
        ConfigLayerKind::UserXdg,
        "[mcp.servers.mix]\ntransport = \"stdio\"\ncommand = \"old\"\n",
    );
    write_config_layer(
        &env,
        ConfigLayerKind::Ancestor,
        "[mcp.servers.mix]\ntransport = \"streamable_http\"\nheaders = { Authorization = \"Bearer x\" }\n",
    );

    let settings = load_config_with_env(None, &env);
    assert!(
        !settings.mcp.servers.contains_key("mix"),
        "the explicit transport switch retires the lower-layer server"
    );
    assert_eq!(settings.mcp.partial.len(), 1, "incomplete patch stays parked");
}
#[test]
fn validate_config_file_accepts_partial_mcp_server_patch() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(".ee.toml");
    std::fs::write(
        &path,
        "[mcp.servers.partial]\ntransport = \"stdio\"\nenv = { TOKEN = \"x\" }\n",
    )
    .unwrap();

    validate_config_file(&path).expect("split-layer patch is valid file shape");
}
#[test]
fn mcp_setup_writes_references_into_workspace_config() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(&env.cwd).unwrap();
    std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
    let server = McpServerToml {
        transport: McpTransportToml::Stdio,
        command: Some(String::from("npx")),
        args: Some(vec![String::from("-y"), String::from("@modelcontextprotocol/server-github")]),
        env: BTreeMap::from([(
            String::from("GITHUB_TOKEN"),
            String::from("secret://mcp.github.env.GITHUB_TOKEN"),
        )]),
        cwd: None,
        url: None,
        headers: BTreeMap::new(),
        timeout_ms: None,
    };

    let path = write_mcp_server_with_env(ConfigScope::Local, "github", &server, &env)
        .expect("write workspace server");

    let workspace: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(workspace["mcp"]["servers"]["github"]["command"].as_str(), Some("npx"));
    assert_eq!(
        workspace["mcp"]["servers"]["github"]["env"]["GITHUB_TOKEN"].as_str(),
        Some("secret://mcp.github.env.GITHUB_TOKEN"),
        "workspace config carries the reference; the value stays in the vault"
    );
    assert!(
        !env.config_dir.as_ref().unwrap().join("ee").join("config.toml").exists(),
        "workspace-scope setup no longer writes a user-layer patch"
    );

    // Merged values keep workspace provenance so the trust gate applies.
    let settings = load_config_with_env(None, &env);
    match settings.mcp.servers.get("github").expect("merged server") {
        McpServerSettings::Stdio { env, .. } => {
            let token = env.get("GITHUB_TOKEN").expect("env value");
            assert_eq!(token.raw, "secret://mcp.github.env.GITHUB_TOKEN");
            assert_eq!(token.layer, ConfigLayerKind::Ancestor);
        }
        other => panic!("expected stdio transport, got {other:?}"),
    }

    remove_mcp_server_with_env(ConfigScope::Local, "github", &env).expect("remove local");
    assert!(list_mcp_servers_with_env(ConfigScope::Local, &env).expect("list").is_empty());
}
