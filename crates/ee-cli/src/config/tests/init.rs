use schemars::schema_for;
use serde_json::Value;
use std::env;
use std::path::Path;

use super::super::*;
#[test]
fn checked_in_config_schema_is_current() {
    let schema_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/ee-config.schema.json");
    check_config_schema(&schema_path).unwrap();
}
#[test]
fn runtime_git_source_schema_requires_exactly_one_pin() {
    let schema = serde_json::to_value(schema_for!(EeToml)).unwrap();
    let git_source = schema
        .get("$defs")
        .and_then(|defs| defs.get("RuntimeGrammarGitSource"))
        .and_then(Value::as_object)
        .unwrap();

    assert_eq!(
        git_source.get("required").and_then(Value::as_array).cloned().unwrap_or_default(),
        vec![Value::String(String::from("url"))]
    );

    let one_of = git_source.get("oneOf").and_then(Value::as_array).unwrap();
    assert_eq!(one_of.len(), 3);
    for pin in ["branch", "tag", "rev"] {
        assert!(one_of.iter().any(|branch| {
            branch
                .get("required")
                .and_then(Value::as_array)
                .is_some_and(|required| required == &[Value::String(pin.to_string())])
        }));
    }
}
// The process cwd is process-global; lock it while mutating.
#[test]
fn merged_config_document_shows_effective_merged_values() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
    std::fs::write(
        env.config_dir.as_ref().unwrap().join("ee").join("config.toml"),
        "wrap_lines = true\n",
    )
    .unwrap();
    std::fs::write(project.join(".ee.toml"), "indent_size = 2\n").unwrap();
    let file = project.join("main.rs");

    let text = toml::to_string_pretty(&resolved_config_with_env(Some(&file), &env)).unwrap();

    assert!(text.contains("wrap_lines = true"));
    assert!(text.contains("indent_size = 2"));
    assert!(text.contains("auto_indent = true"));
    assert!(text.contains("smart_indent = true"));
    assert!(text.contains("statusline_format = \"default\""));
}
#[test]
fn init_config_writes_fully_commented_local_template_without_overwriting() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(env.cwd.as_path()).unwrap();

    let path = init_config_with_env(ConfigScope::Local, &env).unwrap();
    let contents = std::fs::read_to_string(&path).unwrap();

    assert_eq!(path, env.cwd.join(".ee.toml"));
    assert!(contents.lines().all(|line| line.is_empty() || line.starts_with('#')));
    assert!(contents.contains("# [lsp.servers.example]"));
    assert!(contents.contains("# [languages.example.grammar.source.crate]"));
    assert!(contents.contains("# [agents.servers.assistant]"));
    assert!(
        contents.contains("backend = \"searxng\" # or \"exa\", \"brave_llm_context\", \"tavily\"")
    );
    assert!(contents.contains("provider_secret_reference = \"secret://web-search-api-key\""));
    assert!(contents.contains("# [mcp.servers.example]"));

    let error = init_config_with_env(ConfigScope::Local, &env).unwrap_err();
    assert_eq!(error, format!("Config already exists: {}", path.display()));
    assert_eq!(std::fs::read_to_string(path).unwrap(), contents);
}
#[test]
fn init_config_creates_global_template_in_xdg_directory() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());

    let path = init_config_with_env(ConfigScope::Global, &env).unwrap();

    assert_eq!(path, temp.path().join("xdg").join("ee").join("config.toml"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), CONFIG_TEMPLATE);
}
