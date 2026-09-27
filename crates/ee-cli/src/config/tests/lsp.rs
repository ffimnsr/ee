use super::super::*;

use serde_json::Value;
use xi_core_lib::runtime_loader::FormatterAttachment as PluginFormatterAttachment;
// The process cwd is process-global; lock it while mutating.
#[test]
fn lsp_config_merges_system_user_and_project_layers() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    let folder = project.join("folder");
    let file = folder.join("main.rs");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::create_dir_all(env.system_config_path.parent().unwrap()).unwrap();
    std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
    std::fs::write(
            env.system_config_path.as_path(),
            "[lsp.servers.gleam]\nlanguage_name = \"Gleam\"\ncommand = \"gleam\"\nextensions = [\"gleam\"]\n",
        )
        .unwrap();
    std::fs::write(
        env.config_dir.as_ref().unwrap().join("ee").join("config.toml"),
        "[lsp.servers.gleam]\nargs = [\"lsp\"]\n",
    )
    .unwrap();
    std::fs::write(
            project.join(".ee.toml"),
            "[lsp.servers.gleam]\nsupports_single_file = false\nworkspace_identifier = \"gleam.toml\"\n",
        )
        .unwrap();

    let settings = load_config_with_env(Some(&file), &env);
    let gleam = settings.lsp.servers.get("gleam").unwrap();

    assert_eq!(gleam.language_name, "Gleam");
    assert_eq!(gleam.command, "gleam");
    assert_eq!(gleam.args, vec!["lsp"]);
    assert_eq!(gleam.extensions, vec!["gleam"]);
    assert!(!gleam.supports_single_file);
    assert_eq!(gleam.workspace_identifier.as_deref(), Some("gleam.toml"));
}
#[test]
fn lsp_config_replaces_scalars_and_arrays() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    let folder = project.join("folder");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
            project.join(".ee.toml"),
            "[lsp.servers.rust]\nlanguage_name = \"Rust\"\ncommand = \"rust-analyzer\"\nargs = [\"--stdio\"]\nextensions = [\"rs\", \"ron\"]\nworkspace_identifier = \"Cargo.toml\"\n",
        )
        .unwrap();
    std::fs::write(
            folder.join(".ee.toml"),
            "[lsp.servers.rust]\ncommand = \"rust-analyzer-nightly\"\nargs = [\"--nightly\"]\nextensions = [\"rs\"]\nworkspace_identifier = \"Rust.toml\"\n",
        )
        .unwrap();

    let settings = load_config_with_env(Some(&folder.join("main.rs")), &env);
    let rust = settings.lsp.servers.get("rust").unwrap();

    assert_eq!(rust.command, "rust-analyzer-nightly");
    assert_eq!(rust.args, vec!["--nightly"]);
    assert_eq!(rust.extensions, vec!["rs"]);
    assert_eq!(rust.workspace_identifier.as_deref(), Some("Rust.toml"));
}
#[test]
fn lsp_config_shallow_merges_env_and_replaces_initialization_options() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    let file = project.join("main.ts");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
    std::fs::write(
            env.config_dir.as_ref().unwrap().join("ee").join("config.toml"),
            "[lsp.servers.typescript]\nlanguage_name = \"Typescript\"\ncommand = \"typescript-language-server\"\nextensions = [\"ts\"]\nenv = { PATH_HINT = \"/opt/bin\", KEEP = \"yes\" }\ninitialization_options = { format = true }\n",
        )
        .unwrap();
    std::fs::write(
            project.join(".ee.toml"),
            "[lsp.servers.typescript]\nenv = { PATH_HINT = \"/custom/bin\", EXTRA = \"1\" }\ninitialization_options = { format = false, lint = true }\n",
        )
        .unwrap();

    let settings = load_config_with_env(Some(&file), &env);
    let ts = settings.lsp.servers.get("typescript").unwrap();

    assert_eq!(ts.env.get("PATH_HINT").map(String::as_str), Some("/custom/bin"));
    assert_eq!(ts.env.get("KEEP").map(String::as_str), Some("yes"));
    assert_eq!(ts.env.get("EXTRA").map(String::as_str), Some("1"));
    assert_eq!(
        ts.initialization_options
            .as_ref()
            .and_then(|value| value.get("format"))
            .and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        ts.initialization_options
            .as_ref()
            .and_then(|value| value.get("lint"))
            .and_then(Value::as_bool),
        Some(true)
    );
}
#[test]
fn lsp_config_enabled_false_removes_server() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    let file = project.join("main.ts");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join(".ee.toml"), "[lsp.servers.typescript]\nenabled = false\n")
        .unwrap();

    let settings = load_config_with_env(Some(&file), &env);

    assert!(!settings.lsp.servers.contains_key("typescript"));
    assert_eq!(
        settings.lsp.disabled_servers.get("typescript").map(|server| server.extensions.as_slice()),
        Some(
            &[String::from("ts"), String::from("tsx"), String::from("mts"), String::from("cts")][..]
        )
    );
    assert_eq!(
        settings.lsp.language_servers.get("typescript"),
        Some(&vec![String::from("typescript")])
    );
}
#[test]
fn lsp_config_normalizes_extensions_and_rejects_empty_values() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
            project.join(".ee.toml"),
            "[lsp.servers.gleam]\nlanguage_name = \"Gleam\"\ncommand = \"gleam\"\nextensions = [\".gleam\", \".\", \"\"]\n",
        )
        .unwrap();

    let settings = load_config_with_env(Some(&project.join("main.gleam")), &env);
    let gleam = settings.lsp.servers.get("gleam").unwrap();

    assert_eq!(gleam.extensions, vec!["gleam"]);
}
#[test]
fn lsp_config_duplicate_extensions_later_server_wins() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
            project.join(".ee.toml"),
            "[lsp.servers.alpha]\nlanguage_name = \"Alpha\"\ncommand = \"alpha\"\nextensions = [\"demo\", \"alpha\"]\n\n[lsp.servers.beta]\nlanguage_name = \"Beta\"\ncommand = \"beta\"\nextensions = [\"demo\", \"beta\"]\n",
        )
        .unwrap();

    let settings = load_config_with_env(Some(&project.join("main.demo")), &env);
    let alpha = settings.lsp.servers.get("alpha").unwrap();
    let beta = settings.lsp.servers.get("beta").unwrap();

    assert_eq!(alpha.extensions, vec!["alpha"]);
    assert_eq!(beta.extensions, vec!["demo", "beta"]);
}
#[test]
fn lsp_config_normalizes_filenames_and_rejects_invalid_values() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
            project.join(".ee.toml"),
            "[lsp.servers.dockerfile]\nlanguage_name = \"Dockerfile\"\ncommand = \"docker-langserver\"\nfilenames = [\" Dockerfile \", \"\", \"nested/Dockerfile\", \"nested\\\\Dockerfile\", \"Containerfile\"]\n",
        )
        .unwrap();

    let settings = load_config_with_env(Some(&project.join("Dockerfile")), &env);
    let dockerfile = settings.lsp.servers.get("dockerfile").unwrap();

    assert_eq!(dockerfile.filenames, vec!["Dockerfile", "Containerfile"]);
}
#[test]
fn lsp_config_duplicate_filenames_later_server_wins() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
            project.join(".ee.toml"),
            "[lsp.servers.alpha]\nlanguage_name = \"Alpha\"\ncommand = \"alpha\"\nfilenames = [\"Sharedfile\", \"Alphafile\"]\n\n[lsp.servers.beta]\nlanguage_name = \"Beta\"\ncommand = \"beta\"\nfilenames = [\"Sharedfile\", \"Betafile\"]\n",
        )
        .unwrap();

    let settings = load_config_with_env(Some(&project.join("Sharedfile")), &env);
    let alpha = settings.lsp.servers.get("alpha").unwrap();
    let beta = settings.lsp.servers.get("beta").unwrap();

    assert_eq!(alpha.filenames, vec!["Alphafile"]);
    assert_eq!(beta.filenames, vec!["Sharedfile", "Betafile"]);
}
#[test]
fn lsp_config_table_includes_disabled_matching_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join(".ee.toml"), "[lsp.servers.dockerfile]\nenabled = false\n")
        .unwrap();

    let settings = load_config_with_env(Some(&project.join("Dockerfile")), &env);
    let table = settings.lsp.to_config_table();

    assert_eq!(
        table
            .get("disabled_language_config")
            .and_then(Value::as_object)
            .and_then(|servers| servers.get("dockerfile"))
            .and_then(|server| server.get("filenames"))
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(2)
    );
    assert_eq!(
        table
            .get("language_servers")
            .and_then(Value::as_object)
            .and_then(|languages| languages.get("dockerfile"))
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(1)
    );
}
#[test]
fn unified_language_lsp_attachments_allow_extensionless_server_defs() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    let file = project.join("main.ts");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
            project.join(".ee.toml"),
            "[lsp.servers.tsserver]\nlanguage_name = \"TypeScript\"\ncommand = \"typescript-language-server\"\n\n[languages.typescript]\nlsp = [\"tsserver\"]\n",
        )
        .unwrap();

    let settings = load_config_with_env(Some(&file), &env);

    assert!(settings.lsp.servers.contains_key("tsserver"));
    assert_eq!(
        settings.lsp.language_servers.get("typescript"),
        Some(&vec![String::from("tsserver")])
    );
}
#[test]
fn unified_language_lsp_attachments_replace_across_layers() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    let folder = project.join("folder");
    let file = folder.join("main.ts");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
    std::fs::write(
            env.config_dir.as_ref().unwrap().join("ee").join("config.toml"),
            "[lsp.servers.typescript]\nlanguage_name = \"TypeScript\"\ncommand = \"typescript-language-server\"\nextensions = [\"ts\"]\n\n[lsp.servers.eslint]\nlanguage_name = \"ESLint\"\ncommand = \"vscode-eslint-language-server\"\n\n[languages.typescript]\nlsp = [\"typescript\", \"eslint\"]\n",
        )
        .unwrap();
    std::fs::write(project.join(".ee.toml"), "[languages.typescript]\nlsp = [\"eslint\"]\n")
        .unwrap();

    let settings = load_config_with_env(Some(&file), &env);

    assert_eq!(
        settings.lsp.language_servers.get("typescript"),
        Some(&vec![String::from("eslint")])
    );
}
#[test]
fn ee_toml_parses_lsp_servers() {
    let toml = r#"
[lsp.servers.gleam]
language_name = "Gleam"
command = "gleam"
args = ["lsp"]
extensions = ["gleam"]
supports_single_file = false
workspace_identifier = "gleam.toml"

[lsp.servers.rust]
command = "rust-analyzer"
extensions = ["rs"]

[lsp.servers.dockerfile]
command = "docker-langserver"
filenames = ["Dockerfile", "Containerfile"]
"#;
    let raw: EeToml = toml::from_str(toml).unwrap();

    let gleam = raw.lsp.as_ref().unwrap().servers.get("gleam").unwrap();
    assert_eq!(gleam.language_name.as_deref(), Some("Gleam"));
    assert_eq!(gleam.command.as_deref(), Some("gleam"));
    assert_eq!(gleam.args, Some(vec!["lsp".to_owned()]));
    assert_eq!(gleam.extensions, Some(vec!["gleam".to_owned()]));
    assert_eq!(gleam.supports_single_file, Some(false));
    assert_eq!(gleam.workspace_identifier.as_deref(), Some("gleam.toml"));
    assert_eq!(gleam.enabled, None);
    assert!(gleam.env.is_empty());
    assert_eq!(gleam.initialization_options, None);

    let rust = raw.lsp.as_ref().unwrap().servers.get("rust").unwrap();
    assert_eq!(rust.command.as_deref(), Some("rust-analyzer"));
    assert_eq!(rust.extensions, Some(vec!["rs".to_owned()]));
    assert_eq!(rust.language_name, None);

    let dockerfile = raw.lsp.as_ref().unwrap().servers.get("dockerfile").unwrap();
    assert_eq!(dockerfile.command.as_deref(), Some("docker-langserver"));
    assert_eq!(
        dockerfile.filenames,
        Some(vec!["Dockerfile".to_owned(), "Containerfile".to_owned()])
    );
}
#[test]
fn ee_toml_parses_unified_language_lsp_attachments() {
    let toml = r#"
[languages.typescript]
lsp = ["typescript", "eslint"]
"#;
    let raw: EeToml = toml::from_str(toml).unwrap();

    assert_eq!(
        raw.languages.get("typescript").and_then(|language| language.lsp.as_deref()),
        Some(&[String::from("typescript"), String::from("eslint")][..])
    );
}
#[test]
fn ee_toml_rejects_unknown_lsp_server_fields() {
    let toml = r#"
[lsp.servers.rust]
command = "rust-analyzer"
extensions = ["rs"]
bogus = true
"#;

    let err = toml::from_str::<EeToml>(toml).unwrap_err();

    assert!(err.to_string().contains("unknown field `bogus`"));
}
#[test]
fn ee_toml_parses_disabled_lsp_server() {
    let toml = r#"
[lsp.servers.typescript]
enabled = false
filenames = ["tsconfig.json"]
"#;
    let raw: EeToml = toml::from_str(toml).unwrap();

    let server = raw.lsp.as_ref().unwrap().servers.get("typescript").unwrap();
    assert_eq!(server.enabled, Some(false));
    assert_eq!(server.command, None);
    assert_eq!(server.extensions, None);
    assert_eq!(server.filenames, Some(vec!["tsconfig.json".to_owned()]));
}
#[test]
fn ee_toml_parses_lsp_env() {
    let toml = r#"
[lsp.servers.typescript]
command = "typescript-language-server"
extensions = ["ts"]
env = { NODE_NO_WARNINGS = "1", PATH_HINT = "/opt/bin" }
"#;
    let raw: EeToml = toml::from_str(toml).unwrap();

    let server = raw.lsp.as_ref().unwrap().servers.get("typescript").unwrap();
    assert_eq!(server.env.get("NODE_NO_WARNINGS").map(String::as_str), Some("1"));
    assert_eq!(server.env.get("PATH_HINT").map(String::as_str), Some("/opt/bin"));
}
#[test]
fn ee_toml_parses_lsp_initialization_options() {
    let toml = r#"
[lsp.servers.json]
command = "vscode-json-languageserver"
extensions = ["json"]
initialization_options = { provideFormatter = true, nested = { mode = "strict" } }
"#;
    let raw: EeToml = toml::from_str(toml).unwrap();

    let server = raw.lsp.as_ref().unwrap().servers.get("json").unwrap();
    let init = server.initialization_options.as_ref().unwrap();
    assert_eq!(init.get("provideFormatter").and_then(Value::as_bool), Some(true));
    assert_eq!(
        init.get("nested")
            .and_then(Value::as_object)
            .and_then(|nested| nested.get("mode"))
            .and_then(Value::as_str),
        Some("strict")
    );
}
#[test]
fn readme_documents_lsp_server_config() {
    let readme = include_str!("../../../../../README.md");

    assert!(readme.contains("[lsp.servers.<id>]"));
    assert!(readme.contains("lsp = [\"typescript\", \"eslint\"]"));
    assert!(readme.contains("Config precedence"));
    assert!(readme.contains("typescript"));
}

#[test]
fn ee_toml_parses_shared_formatter_definitions_and_attachments() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join(".ee.toml"),
        "[formatters.shfmt]\ncommand = \"shfmt\"\nargs = [\"-\"]\ntimeout_ms = 3000\nmax_output_bytes = 4194304\n\n[languages.shell]\nformatter = \"shfmt\"\n[languages.fish]\nformatter = \"shfmt\"\n[languages.markdown]\nformatter = false\n",
    )
    .unwrap();

    let settings = load_config_with_env(Some(&project.join("x.sh")), &env);

    // One shared definition...
    let formatter = settings.lsp.formatters.get("shfmt").unwrap();
    assert_eq!(formatter.command, "shfmt");
    assert_eq!(formatter.args, vec!["-"]);
    assert_eq!(formatter.timeout_ms, 3000);
    assert_eq!(formatter.max_output_bytes, 4194304);

    // ...attached to multiple languages through the languages table.
    assert_eq!(
        settings.lsp.language_formatters.get("shell"),
        Some(&PluginFormatterAttachment::Id(String::from("shfmt")))
    );
    assert_eq!(
        settings.lsp.language_formatters.get("fish"),
        Some(&PluginFormatterAttachment::Id(String::from("shfmt")))
    );
    assert_eq!(
        settings.lsp.language_formatters.get("markdown"),
        Some(&PluginFormatterAttachment::Disabled(false))
    );
}

#[test]
fn formatter_definitions_and_attachments_reach_plugin_config() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join(".ee.toml"),
        "[formatters.shfmt]\ncommand = \"shfmt\"\n\n[languages.shell]\nformatter = \"shfmt\"\n[languages.markdown]\nformatter = false\n",
    )
    .unwrap();

    let settings = load_config_with_env(Some(&project.join("x.sh")), &env);
    let table = settings.lsp.to_config_table();

    let formatters = table.get("formatters").and_then(Value::as_object).unwrap();
    let shfmt = formatters.get("shfmt").and_then(Value::as_object).unwrap();
    assert_eq!(shfmt.get("command").and_then(Value::as_str), Some("shfmt"));

    let attachments = table.get("language_formatters").and_then(Value::as_object).unwrap();
    assert_eq!(attachments.get("shell").and_then(Value::as_str), Some("shfmt"));
    assert_eq!(attachments.get("markdown").and_then(Value::as_bool), Some(false));
}

#[test]
fn formatter_layers_override_scalar_fields() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
    std::fs::write(
        env.config_dir.as_ref().unwrap().join("ee").join("config.toml"),
        "[formatters.shfmt]\ncommand = \"shfmt\"\nargs = [\"-\"]\n",
    )
    .unwrap();
    std::fs::write(project.join(".ee.toml"), "[formatters.shfmt]\nargs = [\"-i\", \"2\"]\n")
        .unwrap();

    let settings = load_config_with_env(Some(&project.join("x.sh")), &env);
    let formatter = settings.lsp.formatters.get("shfmt").unwrap();
    assert_eq!(formatter.command, "shfmt");
    assert_eq!(formatter.args, vec!["-i", "2"]);
}

#[test]
fn formatter_defaults_apply_when_fields_omitted() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join(".ee.toml"), "[formatters.black]\ncommand = \"black\"\n").unwrap();

    let settings = load_config_with_env(Some(&project.join("x.py")), &env);
    let formatter = settings.lsp.formatters.get("black").unwrap();
    assert_eq!(formatter.command, "black");
    assert!(formatter.args.is_empty());
    assert_eq!(formatter.timeout_ms, 5000);
    assert_eq!(formatter.max_output_bytes, 8 * 1024 * 1024);
}

#[test]
fn formatter_attachment_to_unknown_definition_is_dropped_with_warning() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join(".ee.toml"), "[languages.shell]\nformatter = \"shfmt\"\n").unwrap();

    let settings = load_config_with_env(Some(&project.join("x.sh")), &env);
    assert!(settings.lsp.language_formatters.is_empty());
}

#[test]
fn formatter_definition_without_command_is_dropped_with_warning() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join(".ee.toml"), "[formatters.shfmt]\n").unwrap();

    let settings = load_config_with_env(Some(&project.join("x.sh")), &env);
    assert!(settings.lsp.formatters.is_empty());
}

#[test]
fn formatters_roundtrip_through_merged_config_document() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join(".ee.toml"), "[formatters.shfmt]\ncommand = \"shfmt\"\n").unwrap();

    let file = project.join("x.sh");
    let merged = merged_config_document(Some(&file)).unwrap();
    assert!(merged.contains("[formatters.shfmt]"));
    assert!(merged.contains("command = \"shfmt\""));
}

#[test]
fn per_language_format_on_save_and_code_actions_are_merged() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join(".ee.toml"),
        "[languages.shell]\nformat_on_save = true\ncode_actions_on_save = [\"source.fixAll\"]\n[languages.markdown]\nformat_on_save = false\n",
    )
    .unwrap();

    let settings = load_config_with_env(Some(&project.join("x.sh")), &env);
    assert_eq!(settings.lsp.language_format_on_save.get("shell"), Some(&true));
    assert_eq!(
        settings.lsp.language_code_actions_on_save.get("shell"),
        Some(&vec![String::from("source.fixAll")])
    );
    assert_eq!(settings.lsp.language_format_on_save.get("markdown"), Some(&false));
    assert!(!settings.format_on_save, "global format_on_save defaults to false");

    let table = settings.lsp.to_config_table();
    assert_eq!(table.get("language_format_on_save"), None); // frontend-only, not plugin config
}
