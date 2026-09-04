use super::super::*;

// The process cwd is process-global; lock it while mutating.
#[test]
fn runtime_language_toml_parses_crate_source() {
    let raw: EeToml = toml::from_str(
        r#"
[languages.gleam]
name = "Gleam"
file_types = ["gleam"]

[languages.gleam.grammar]
library = "tree-sitter-gleam"
symbol = "tree_sitter_gleam"

[languages.gleam.grammar.source.crate]
name = "tree-sitter-gleam"
version = "1.2.3"
"#,
    )
    .unwrap();

    let gleam = raw.languages.get("gleam").unwrap();
    assert_eq!(gleam.name.as_deref(), Some("Gleam"));
    assert_eq!(gleam.file_types.as_deref(), Some(&[String::from("gleam")][..]));
    assert!(matches!(
        gleam.grammar.as_ref().and_then(|grammar| grammar.source.as_ref()),
        Some(xi_core_lib::runtime_loader::RuntimeGrammarSource::Crate(source))
            if source.name == "tree-sitter-gleam" && source.version == "1.2.3"
    ));
}
#[test]
fn runtime_language_toml_parses_git_branch_tag_and_rev_sources() {
    for (label, source_table) in
        [("branch", "branch = \"main\""), ("tag", "tag = \"v1.0.0\""), ("rev", "rev = \"abc123\"")]
    {
        let text = format!(
            "[languages.demo]\nname = \"Demo\"\nfile_types = [\"demo\"]\n\n[languages.demo.grammar]\nlibrary = \"tree-sitter-demo\"\nsymbol = \"tree_sitter_demo\"\n\n[languages.demo.grammar.source.git]\nurl = \"https://example.com/tree-sitter-demo\"\n{source_table}\n"
        );
        let raw: EeToml = toml::from_str(&text).unwrap();
        let demo = raw.languages.get("demo").unwrap();
        match demo.grammar.as_ref().and_then(|grammar| grammar.source.as_ref()) {
            Some(xi_core_lib::runtime_loader::RuntimeGrammarSource::Git(source)) => {
                assert_eq!(source.url, "https://example.com/tree-sitter-demo");
                match label {
                    "branch" => assert_eq!(source.branch.as_deref(), Some("main")),
                    "tag" => assert_eq!(source.tag.as_deref(), Some("v1.0.0")),
                    "rev" => assert_eq!(source.rev.as_deref(), Some("abc123")),
                    _ => unreachable!(),
                }
            }
            other => panic!("unexpected source for {label}: {other:?}"),
        }
    }
}
#[test]
fn runtime_language_toml_rejects_mixed_source_kinds() {
    let error = toml::from_str::<EeToml>(
        r#"
[languages.demo]
name = "Demo"
file_types = ["demo"]

[languages.demo.grammar]
library = "tree-sitter-demo"
symbol = "tree_sitter_demo"

[languages.demo.grammar.source.crate]
name = "tree-sitter-demo"
version = "1.2.3"

[languages.demo.grammar.source.git]
url = "https://example.com/tree-sitter-demo"
rev = "abc123"
"#,
    )
    .unwrap_err();

    assert!(error.to_string().contains("source"));
}
#[test]
fn validate_config_file_rejects_incomplete_runtime_language_definition() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(".ee.toml");
    std::fs::write(
        &path,
        r#"
[languages.newlang]
lsp = ["newlang"]
"#,
    )
    .unwrap();

    let error = validate_config_file(&path).unwrap_err();

    assert!(error.contains("Config validation error"));
    assert!(error.contains("runtime language `newlang` is missing non-empty file_types"));
}
#[test]
fn validate_config_file_allows_partial_builtin_runtime_override() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(".ee.toml");
    std::fs::write(
        &path,
        r#"
[languages.rust]
lsp = ["rust"]
"#,
    )
    .unwrap();

    validate_config_file(&path).unwrap();
}
#[test]
fn runtime_language_config_uses_same_layer_precedence_as_lsp() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    let folder = project.join("folder");
    let file = folder.join("main.gleam");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::create_dir_all(env.system_config_path.parent().unwrap()).unwrap();
    std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
    std::fs::write(
            env.system_config_path.as_path(),
            "[languages.gleam]\nname = \"Gleam\"\nfile_types = [\".gleam\"]\n\n[languages.gleam.grammar]\nlibrary = \"tree-sitter-gleam\"\nsymbol = \"tree_sitter_gleam\"\n\n[languages.gleam.grammar.source.crate]\nname = \"tree-sitter-gleam\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
    std::fs::write(
            env.config_dir.as_ref().unwrap().join("ee").join("config.toml"),
            "[languages.gleam.grammar]\nlibrary = \"tree-sitter-gleam-user\"\n\n[languages.gleam.grammar.source.crate]\nname = \"tree-sitter-gleam-user\"\nversion = \"1.1.0\"\n",
        )
        .unwrap();
    std::fs::write(project.join(".ee.toml"), "[languages.gleam]\nenabled = false\n").unwrap();

    let runtime = runtime_languages_with_env(Some(&file), &env);
    let user = runtime.user_overrides.get("gleam").unwrap();
    let workspace = runtime.workspace_overrides.get("gleam").unwrap();

    assert_eq!(user.file_types.as_deref(), Some(&[String::from("gleam")][..]));
    assert_eq!(
        user.grammar.as_ref().and_then(|grammar| grammar.library.as_deref()),
        Some("tree-sitter-gleam-user")
    );
    assert_eq!(workspace.enabled, Some(false));
}
