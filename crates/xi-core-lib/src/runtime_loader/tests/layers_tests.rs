//! Runtime-loader tests: layers.
use super::*;

#[test]
fn runtime_loader_merges_built_in_user_and_workspace_layers() {
    let languages = Languages::new(&[language_definition("rust", &["rs"])]);
    let roots = RuntimeRoots::new("/bundle", "/user/ee", Some(PathBuf::from("/workspace/.ee")));
    let mut loader = RuntimeLoader::new(roots, vec![PathBuf::from("/parser-dir")]).unwrap();

    let mut user_overrides = RuntimeLanguageOverrides::new();
    user_overrides.insert(
        "rust".to_string(),
        RuntimeLanguageConfig {
            aliases: Some(vec!["rscript".to_string()]),
            shebangs: Some(vec!["#!/usr/bin/env rust-script".to_string()]),
            supported_query_kinds: Some(BTreeSet::from([
                RuntimeQueryKind::Highlights,
                RuntimeQueryKind::Locals,
                RuntimeQueryKind::Indents,
            ])),
            grammar: Some(runtime_grammar_config("tree-sitter-rust", "tree_sitter_rust", "0.0.0")),
            ..RuntimeLanguageConfig::default()
        },
    );

    let mut workspace_overrides = RuntimeLanguageOverrides::new();
    workspace_overrides.insert(
        "rust".to_string(),
        RuntimeLanguageConfig {
            file_types: Some(vec!["rs.in".to_string()]),
            globs: Some(vec!["*.rs.in".to_string()]),
            match_priority: Some(20),
            ..RuntimeLanguageConfig::default()
        },
    );

    loader
        .reload_merged_languages(
            &languages,
            &user_overrides,
            Some(WorkspaceRuntimeOverrides { trusted: true, overrides: &workspace_overrides }),
        )
        .unwrap();

    let language = loader.language_for_name("rscript").unwrap();
    assert_eq!(language.canonical_id(), "rust");
    assert_eq!(language.display_name(), "rust");
    assert_eq!(language.grammar_library_name(), Some("tree-sitter-rust"));
    assert_eq!(language.asset_source(), RuntimeConfigSource::User);
    assert!(language.file_types().iter().any(|value| value == "rs.in"));
    assert!(language.globs().iter().any(|value| value == "*.rs.in"));
    assert!(language.shebangs().iter().any(|value| value == "#!/usr/bin/env rust-script"));
    assert_eq!(language.match_priority(), 20);
    assert!(language.supported_query_kinds().contains(&RuntimeQueryKind::Indents));
}

#[test]
fn runtime_loader_ignores_untrusted_workspace_overrides() {
    let languages = Languages::new(&[language_definition("rust", &["rs"])]);
    let roots = RuntimeRoots::new("/bundle", "/user/ee", Some(PathBuf::from("/workspace/.ee")));
    let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();

    let mut workspace_overrides = RuntimeLanguageOverrides::new();
    workspace_overrides.insert(
        "rust".to_string(),
        RuntimeLanguageConfig {
            file_types: Some(vec!["workspace-rs".to_string()]),
            ..RuntimeLanguageConfig::default()
        },
    );

    loader
        .reload_merged_languages(
            &languages,
            &RuntimeLanguageOverrides::new(),
            Some(WorkspaceRuntimeOverrides { trusted: false, overrides: &workspace_overrides }),
        )
        .unwrap();

    assert!(loader.language_for_path(Path::new("main.workspace-rs")).is_none());
}

#[test]
fn runtime_loader_adds_config_defined_language() {
    let roots = RuntimeRoots::new("/bundle", "/user/ee", None);
    let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
    let mut overrides = RuntimeLanguageOverrides::new();
    overrides.insert(
        String::from("gleam"),
        RuntimeLanguageConfig {
            name: Some(String::from("Gleam")),
            file_types: Some(vec![String::from(".gleam")]),
            scope: Some(String::from("source.gleam")),
            aliases: Some(vec![String::from("gleam")]),
            grammar: Some(runtime_grammar_config(
                "tree-sitter-gleam",
                "tree_sitter_gleam",
                "1.0.0",
            )),
            ..RuntimeLanguageConfig::default()
        },
    );

    loader.reload_merged_languages(&Languages::default(), &overrides, None).unwrap();

    let language = loader.language_for_name("gleam").unwrap();
    assert_eq!(language.display_name(), "Gleam");
    assert_eq!(language.grammar_library_name(), Some("tree-sitter-gleam"));
    assert_eq!(
        loader.language_for_path(Path::new("main.gleam")).map(RuntimeLanguage::display_name),
        Some("Gleam")
    );
}

#[test]
fn runtime_loader_disables_language_when_enabled_false() {
    let languages = Languages::new(&[language_definition("rust", &["rs"])]);
    let roots = RuntimeRoots::new("/bundle", "/user/ee", None);
    let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
    let mut overrides = RuntimeLanguageOverrides::new();
    overrides.insert(
        String::from("rust"),
        RuntimeLanguageConfig { enabled: Some(false), ..RuntimeLanguageConfig::default() },
    );

    loader.reload_merged_languages(&languages, &overrides, None).unwrap();

    assert!(loader.language_for_name("rust").is_none());
    assert!(loader.language_for_path(Path::new("main.rs")).is_none());
}

#[test]
fn runtime_loader_rejects_git_source_with_multiple_refs() {
    let roots = RuntimeRoots::new("/bundle", "/user/ee", None);
    let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
    let mut overrides = RuntimeLanguageOverrides::new();
    overrides.insert(
        String::from("demo"),
        RuntimeLanguageConfig {
            name: Some(String::from("Demo")),
            file_types: Some(vec![String::from("demo")]),
            grammar: Some(RuntimeGrammarConfig {
                library: Some(String::from("tree-sitter-demo")),
                symbol: Some(String::from("tree_sitter_demo")),
                source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                    url: String::from("https://example.com/tree-sitter-demo"),
                    branch: Some(String::from("main")),
                    tag: Some(String::from("v1.0.0")),
                    rev: None,
                })),
            }),
            ..RuntimeLanguageConfig::default()
        },
    );

    let error =
        loader.reload_merged_languages(&Languages::default(), &overrides, None).unwrap_err();
    assert!(matches!(error, RuntimeLoaderError::InvalidConfig { .. }));
}
