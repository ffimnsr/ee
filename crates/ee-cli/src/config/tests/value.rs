use super::super::*;

// The process cwd is process-global; lock it while mutating.
#[test]
fn set_config_value_creates_global_file_and_get_reads_it() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(env.cwd.as_path()).unwrap();

    let written =
        set_config_value_with_env(ConfigScope::Global, "wrap_lines", "true", &env).unwrap();
    let value =
        get_config_value_with_env(ConfigScope::Global, "wrap_lines", &env).unwrap().unwrap();

    assert_eq!(written, temp.path().join("xdg").join("ee").join("config.toml"));
    assert_eq!(value, "true");
}
#[test]
fn set_config_value_writes_local_nested_keys() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(env.cwd.as_path()).unwrap();

    let written = set_config_value_with_env(
        ConfigScope::Local,
        "lsp.servers.rust.command",
        "rust-analyzer",
        &env,
    )
    .unwrap();
    let contents = std::fs::read_to_string(&written).unwrap();

    assert_eq!(written, env.cwd.join(".ee.toml"));
    assert!(contents.contains("[lsp.servers.rust]"));
    assert!(contents.contains("command = \"rust-analyzer\""));
}
