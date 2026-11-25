use super::super::*;

fn write_workspace_mcp_json(env: &ConfigEnvironment, contents: &str) {
    let path = env.cwd.join(".agents").join("mcp.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn write_user_mcp_json(env: &ConfigEnvironment, contents: &str) {
    let path = env.config_dir.as_ref().unwrap().join("ee").join("mcp.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

#[test]
fn workspace_agents_mcp_json_is_imported() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(&env.cwd).unwrap();
    write_workspace_mcp_json(
        &env,
        r#"{
            "mcpServers": {
                "filesystem": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-filesystem"]
                },
                "remote": {
                    "url": "https://example.com/mcp",
                    "headers": { "Authorization": "Bearer token" }
                }
            }
        }"#,
    );

    let settings = load_config_with_env(None, &env);

    assert!(matches!(
        settings.mcp.servers.get("filesystem"),
        Some(McpServerSettings::Stdio { command, args, .. })
            if command == "npx"
                && args == &vec![
                    String::from("-y"),
                    String::from("@modelcontextprotocol/server-filesystem")
                ]
    ));
    assert!(matches!(
        settings.mcp.servers.get("remote"),
        Some(McpServerSettings::StreamableHttp { url, headers, .. })
            if url == "https://example.com/mcp"
                && headers.get("Authorization").map(|value| value.raw.as_str())
                    == Some("Bearer token")
    ));
}

#[test]
fn user_ee_mcp_json_is_imported() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(&env.cwd).unwrap();
    write_user_mcp_json(
        &env,
        r#"{ "mcpServers": { "user-tools": { "command": "user-server" } } }"#,
    );

    let settings = load_config_with_env(None, &env);

    assert!(matches!(
        settings.mcp.servers.get("user-tools"),
        Some(McpServerSettings::Stdio { command, .. }) if command == "user-server"
    ));
}

#[test]
fn workspace_standard_json_overrides_user_standard_json() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(&env.cwd).unwrap();
    write_user_mcp_json(&env, r#"{ "mcpServers": { "shared": { "command": "user-server" } } }"#);
    write_workspace_mcp_json(
        &env,
        r#"{ "mcpServers": { "shared": { "command": "workspace-server" } } }"#,
    );

    let settings = load_config_with_env(None, &env);

    assert!(matches!(
        settings.mcp.servers.get("shared"),
        Some(McpServerSettings::Stdio { command, .. }) if command == "workspace-server"
    ));
}

#[test]
fn ee_toml_overrides_standard_json_in_same_directory_and_merges_env() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(&env.cwd).unwrap();
    write_workspace_mcp_json(
        &env,
        r#"{ "mcpServers": { "shared": { "command": "json-server", "env": { "FROM_JSON": "1" } } } }"#,
    );
    write_config_layer(
        &env,
        ConfigLayerKind::Ancestor,
        "[mcp.servers.shared]\ntransport = \"stdio\"\ncommand = \"toml-server\"\nenv = { FROM_TOML = \"2\" }\n",
    );

    let settings = load_config_with_env(None, &env);
    match settings.mcp.servers.get("shared").expect("merged server") {
        McpServerSettings::Stdio { command, env, .. } => {
            assert_eq!(command, "toml-server", "same-directory TOML wins");
            assert_eq!(env.get("FROM_JSON").map(|value| value.raw.as_str()), Some("1"));
            assert_eq!(env.get("FROM_TOML").map(|value| value.raw.as_str()), Some("2"));
        }
        other => panic!("expected stdio transport, got {other:?}"),
    }
}

#[test]
fn inner_standard_json_overrides_outer_standard_json() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    let file = project.join("main.rs");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(&file, "fn main() {}\n").unwrap();
    write_workspace_mcp_json(&env, r#"{ "mcpServers": { "shared": { "command": "outer" } } }"#);
    let inner = project.join(".agents").join("mcp.json");
    std::fs::create_dir_all(inner.parent().unwrap()).unwrap();
    std::fs::write(inner, r#"{ "mcpServers": { "shared": { "command": "inner" } } }"#).unwrap();

    let settings = load_config_with_env(Some(&file), &env);

    assert!(matches!(
        settings.mcp.servers.get("shared"),
        Some(McpServerSettings::Stdio { command, .. }) if command == "inner"
    ));
}

#[test]
fn root_true_stops_standard_json_discovery_above_the_boundary() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    let folder = project.join("folder");
    let file = folder.join("main.rs");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(&file, "fn main() {}\n").unwrap();

    let outer = project.join(".agents").join("mcp.json");
    std::fs::create_dir_all(outer.parent().unwrap()).unwrap();
    std::fs::write(outer, r#"{ "mcpServers": { "outer": { "command": "outer" } } }"#).unwrap();
    let inner = folder.join(".agents").join("mcp.json");
    std::fs::create_dir_all(inner.parent().unwrap()).unwrap();
    std::fs::write(inner, r#"{ "mcpServers": { "inner": { "command": "inner" } } }"#).unwrap();
    std::fs::write(folder.join(".ee.toml"), "root = true\n").unwrap();

    let settings = load_config_with_env(Some(&file), &env);

    assert!(settings.mcp.servers.contains_key("inner"));
    assert!(!settings.mcp.servers.contains_key("outer"), "root=true is a hard boundary");
}

#[test]
fn malformed_standard_json_does_not_block_other_layers() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(&env.cwd).unwrap();
    write_workspace_mcp_json(&env, "{ not json");
    write_config_layer(
        &env,
        ConfigLayerKind::Ancestor,
        "[mcp.servers.from-toml]\ntransport = \"stdio\"\ncommand = \"toml-server\"\n",
    );

    let settings = load_config_with_env(None, &env);

    assert!(settings.mcp.servers.contains_key("from-toml"));
}

#[test]
fn disabled_standard_server_is_not_loaded() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(&env.cwd).unwrap();
    write_workspace_mcp_json(
        &env,
        r#"{ "mcpServers": { "off": { "command": "server", "disabled": true } } }"#,
    );

    let settings = load_config_with_env(None, &env);

    assert!(settings.mcp.servers.is_empty());
}

#[test]
fn workspace_standard_env_keeps_ancestor_provenance() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(&env.cwd).unwrap();
    write_workspace_mcp_json(
        &env,
        r#"{ "mcpServers": { "srv": { "command": "server", "env": { "TOKEN": "secret://mcp-token" } } } }"#,
    );

    let settings = load_config_with_env(None, &env);
    match settings.mcp.servers.get("srv").expect("server") {
        McpServerSettings::Stdio { env, .. } => {
            let token = env.get("TOKEN").expect("env value");
            assert_eq!(token.raw, "secret://mcp-token");
            assert_eq!(token.layer, ConfigLayerKind::Ancestor, "workspace refs need trust");
        }
        other => panic!("expected stdio transport, got {other:?}"),
    }
}

#[test]
fn relative_cwd_in_workspace_json_resolves_against_workspace_directory() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(&env.cwd).unwrap();
    write_workspace_mcp_json(
        &env,
        r#"{ "mcpServers": { "srv": { "command": "server", "cwd": "tools" } } }"#,
    );

    let settings = load_config_with_env(None, &env);
    match settings.mcp.servers.get("srv").expect("server") {
        McpServerSettings::Stdio { cwd, .. } => {
            assert_eq!(cwd.as_deref(), Some(env.cwd.join("tools").as_path()));
        }
        other => panic!("expected stdio transport, got {other:?}"),
    }
}

#[test]
fn workspace_standard_json_overrides_user_toml() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(&env.cwd).unwrap();
    write_config_layer(
        &env,
        ConfigLayerKind::UserXdg,
        "[mcp.servers.shared]\ntransport = \"stdio\"\ncommand = \"user-toml-server\"\n",
    );
    write_workspace_mcp_json(
        &env,
        r#"{ "mcpServers": { "shared": { "command": "workspace-json-server" } } }"#,
    );

    let settings = load_config_with_env(None, &env);

    assert!(
        matches!(
            settings.mcp.servers.get("shared"),
            Some(McpServerSettings::Stdio { command, .. }) if command == "workspace-json-server"
        ),
        "a workspace layer outranks every user layer"
    );
}

#[test]
fn invalid_secret_reference_in_standard_json_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(&env.cwd).unwrap();
    write_workspace_mcp_json(
        &env,
        r#"{ "mcpServers": { "srv": { "command": "server", "env": { "TOKEN": "Bearer secret://mcp-token" } } } }"#,
    );

    let settings = load_config_with_env(None, &env);

    assert!(
        !settings.mcp.servers.contains_key("srv"),
        "stdio env accepts exact references only; an embedded template must skip the server"
    );
}

#[test]
fn config_search_report_marks_standard_mcp_files() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(&env.cwd).unwrap();
    write_workspace_mcp_json(&env, r#"{ "mcpServers": { "srv": { "command": "server" } } }"#);
    write_user_mcp_json(&env, r#"{ "mcpServers": { "user-srv": { "command": "server" } } }"#);

    let report = config_search_report_with_env(&env, None);
    let standard = report
        .layers
        .iter()
        .filter(|layer| layer.note.as_deref() == Some("standard MCP JSON"))
        .collect::<Vec<_>>();

    assert_eq!(standard.len(), 2);
    assert!(standard.iter().all(|layer| layer.loaded && layer.exists));
    assert!(standard.iter().any(|layer| layer.path.ends_with(".agents/mcp.json")));
    assert!(standard.iter().any(|layer| layer.path.ends_with("ee/mcp.json")));
}
