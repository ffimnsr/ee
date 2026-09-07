//! LSP plugin tests.
use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::json;

use super::{Config, LanguageMatch, LspPlugin};
use crate::types::{DisabledLanguageConfig, LanguageConfig};

fn language_config(
    command: &str,
    extensions: &[&str],
    filenames: &[&str],
    supports_single_file: bool,
    workspace_identifier: Option<&str>,
) -> LanguageConfig {
    LanguageConfig {
        language_name: String::from("Test"),
        start_command: String::from(command),
        start_arguments: Vec::new(),
        extensions: extensions.iter().map(|ext| (*ext).to_owned()).collect(),
        filenames: filenames.iter().map(|filename| (*filename).to_owned()).collect(),
        supports_single_file,
        workspace_identifier: workspace_identifier.map(str::to_owned),
        env: BTreeMap::new(),
        initialization_options: None,
    }
}

#[test]
fn changed_language_ids_only_reports_modified_servers() {
    let current = Config {
        language_config: HashMap::from([
            (
                String::from("rust"),
                language_config("rust-analyzer", &["rs"], &[], false, Some("Cargo.toml")),
            ),
            (
                String::from("json"),
                language_config("vscode-json-languageserver", &["json"], &[], true, None),
            ),
        ]),
        disabled_language_config: HashMap::new(),
        language_servers: HashMap::from([
            (String::from("rust"), vec![String::from("rust")]),
            (String::from("json"), vec![String::from("json")]),
        ]),
    };
    let next = Config {
        language_config: HashMap::from([
            (
                String::from("rust"),
                LanguageConfig {
                    env: BTreeMap::from([(String::from("RUST_LOG"), String::from("debug"))]),
                    ..language_config("rust-analyzer", &["rs"], &[], false, Some("Cargo.toml"))
                },
            ),
            (
                String::from("gleam"),
                LanguageConfig {
                    initialization_options: Some(json!({ "feature": true })),
                    ..language_config("gleam", &["gleam"], &[], true, None)
                },
            ),
        ]),
        disabled_language_config: HashMap::new(),
        language_servers: HashMap::from([
            (String::from("rust"), vec![String::from("rust")]),
            (String::from("gleam"), vec![String::from("gleam")]),
        ]),
    };

    let plugin = LspPlugin::new(current);

    assert_eq!(
        plugin.changed_language_ids(&next),
        HashSet::from([String::from("rust"), String::from("json"), String::from("gleam")])
    );
}

#[test]
fn parse_plugin_config_update_merges_partial_changes() {
    let mut plugin = LspPlugin::new(Config {
        language_config: HashMap::from([(
            String::from("rust"),
            language_config("rust-analyzer", &["rs"], &[], false, Some("Cargo.toml")),
        )]),
        disabled_language_config: HashMap::new(),
        language_servers: HashMap::from([(String::from("rust"), vec![String::from("rust")])]),
    });

    let next = plugin
        .parse_plugin_config_update(&serde_json::Map::from_iter([(
            String::from("language_servers"),
            json!({ "rust": ["rust", "clippy"] }),
        )]))
        .expect("partial config update should merge with current config");

    assert_eq!(
        next.language_config["rust"].start_command,
        plugin.config.language_config["rust"].start_command
    );
    assert_eq!(
        next.language_servers.get("rust"),
        Some(&vec![String::from("rust"), String::from("clippy")])
    );

    plugin.apply_plugin_config(next);

    assert_eq!(
        plugin.config.language_servers.get("rust"),
        Some(&vec![String::from("rust"), String::from("clippy")])
    );
}

#[test]
fn parse_plugin_config_update_ignores_non_lsp_tables() {
    let plugin = LspPlugin::new(Config::bundled());

    let next = plugin
        .parse_plugin_config_update(&serde_json::Map::from_iter([
            (String::from("tab_size"), json!(2)),
            (String::from("translate_tabs_to_spaces"), json!(true)),
        ]))
        .expect("non-lsp config table should keep current config");

    assert_eq!(next, plugin.config);
}

#[test]
fn path_matching_reports_disabled_server() {
    let plugin = LspPlugin::new(Config {
        language_config: HashMap::new(),
        disabled_language_config: HashMap::from([(
            String::from("typescript"),
            DisabledLanguageConfig { extensions: vec![String::from("ts")], filenames: Vec::new() },
        )]),
        language_servers: HashMap::new(),
    });

    assert_eq!(
        plugin.language_match_for_path(std::path::Path::new("main.ts")),
        Some(LanguageMatch::Disabled(String::from("typescript")))
    );
}

#[test]
fn path_matching_uses_exact_filename_before_extension() {
    let plugin = LspPlugin::new(Config {
        language_config: HashMap::from([
            (
                String::from("dockerfile"),
                language_config("docker-langserver", &[], &["Dockerfile"], true, None),
            ),
            (
                String::from("shell"),
                language_config("bash-language-server", &["Dockerfile"], &[], true, None),
            ),
        ]),
        disabled_language_config: HashMap::new(),
        language_servers: HashMap::new(),
    });

    assert_eq!(
        plugin.language_match_for_path(std::path::Path::new("Dockerfile")),
        Some(LanguageMatch::Enabled(String::from("dockerfile")))
    );
}

#[test]
fn path_matching_reports_disabled_filename_server() {
    let plugin = LspPlugin::new(Config {
        language_config: HashMap::new(),
        disabled_language_config: HashMap::from([(
            String::from("just"),
            DisabledLanguageConfig {
                extensions: Vec::new(),
                filenames: vec![String::from("Justfile")],
            },
        )]),
        language_servers: HashMap::new(),
    });

    assert_eq!(
        plugin.language_match_for_path(std::path::Path::new("Justfile")),
        Some(LanguageMatch::Disabled(String::from("just")))
    );
}

#[test]
fn unsupported_single_file_server_has_no_key_without_workspace_root() {
    let plugin = LspPlugin::new(Config {
        language_config: HashMap::from([(
            String::from("gleam"),
            language_config("gleam", &["gleam"], &[], false, Some("gleam.toml")),
        )]),
        disabled_language_config: HashMap::new(),
        language_servers: HashMap::new(),
    });

    assert_eq!(plugin.language_server_key("gleam", &None), None);
}

#[test]
fn spawn_failure_status_includes_install_hint_without_leaking_secrets() {
    let (key, value) = LspPlugin::spawn_failure_status("gleam", "gleam");

    assert_eq!(key, "lsp:gleam:status");
    assert_eq!(
        value,
        "lsp:gleam:spawn failed: gleam; hint: install gleam and ensure it is in PATH"
    );
    assert!(!value.contains("initialization_options"));
    assert!(!value.contains("XI_LSP_SECRET"));
}

#[test]
fn language_matches_use_explicit_language_attachments_before_extensions() {
    let plugin = LspPlugin::new(Config {
        language_config: HashMap::from([
            (String::from("eslint"), language_config("eslint", &["js"], &[], true, None)),
            (
                String::from("typescript"),
                language_config("typescript-language-server", &["ts"], &[], true, None),
            ),
        ]),
        disabled_language_config: HashMap::new(),
        language_servers: HashMap::from([(
            String::from("typescript"),
            vec![String::from("typescript"), String::from("eslint")],
        )]),
    });

    assert_eq!(
        plugin.language_matches_for_path(std::path::Path::new("main.ts"), Some("typescript"),),
        vec![
            LanguageMatch::Enabled(String::from("typescript")),
            LanguageMatch::Enabled(String::from("eslint")),
        ]
    );
}
