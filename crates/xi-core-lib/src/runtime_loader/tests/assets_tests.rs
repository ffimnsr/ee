//! Runtime-loader tests: assets.
use super::*;

#[test]
fn runtime_loader_prefers_workspace_runtime_root_for_grammar_assets() {
    let languages = Languages::new(&[language_definition("rust", &["rs"])]);
    let roots =
        RuntimeRoots::new("/bundle/ee", "/user/ee", Some(PathBuf::from("/workspace/project/.ee")));
    let mut loader = RuntimeLoader::new(roots.clone(), Vec::new()).unwrap();

    let mut user_overrides = RuntimeLanguageOverrides::new();
    user_overrides.insert(
        "rust".to_string(),
        RuntimeLanguageConfig {
            grammar: Some(runtime_grammar_config(
                "tree-sitter-rust-user",
                "tree_sitter_rust",
                "0.0.0",
            )),
            ..RuntimeLanguageConfig::default()
        },
    );
    let mut workspace_overrides = RuntimeLanguageOverrides::new();
    workspace_overrides.insert(
        "rust".to_string(),
        RuntimeLanguageConfig {
            query_language: Some("rust-workspace".to_string()),
            grammar: Some(runtime_grammar_config(
                "tree-sitter-rust-workspace",
                "tree_sitter_rust",
                "0.0.0",
            )),
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

    let language = loader.language_for_name("rust").unwrap();
    assert_eq!(language.asset_source(), RuntimeConfigSource::Workspace);
    assert_eq!(
        language.grammar_library_path(&roots).as_deref(),
        Some(
            Path::new("/workspace/project/.ee/grammars")
                .join(shared_library_filename("tree-sitter-rust-workspace"))
                .as_path()
        )
    );
    assert_eq!(
        language.query_dir(&roots).as_deref(),
        Some(Path::new("/workspace/project/.ee/queries/rust"))
    );
}

#[test]
fn query_overlay_order_is_bundled_then_user_then_workspace() {
    let temp_dir = TempDir::new().unwrap();
    let bundled_root = temp_dir.path().join("bundle");
    let user_root = temp_dir.path().join("user");
    let workspace_root = temp_dir.path().join("workspace").join(".ee");
    for (root, text) in [
        (&bundled_root, "((identifier) @base)\n"),
        (&user_root, "((identifier) @user)\n"),
        (&workspace_root, "((identifier) @workspace)\n"),
    ] {
        let query_dir = root.join("queries").join("rust");
        fs::create_dir_all(&query_dir).unwrap();
        fs::write(query_dir.join("indents.scm"), text).unwrap();
    }

    let roots = RuntimeRoots::new(&bundled_root, &user_root, Some(workspace_root));
    let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
    let languages = Languages::new(&[language_definition("rust", &["rs"])]);
    let mut overrides = RuntimeLanguageOverrides::new();
    overrides.insert(
        "rust".to_string(),
        RuntimeLanguageConfig {
            supported_query_kinds: Some(BTreeSet::from([RuntimeQueryKind::Indents])),
            ..RuntimeLanguageConfig::default()
        },
    );
    loader
        .reload_merged_languages(
            &languages,
            &overrides,
            Some(WorkspaceRuntimeOverrides {
                trusted: true,
                overrides: &RuntimeLanguageOverrides::new(),
            }),
        )
        .unwrap();

    let artifact = loader.resolve_query_source("rust", RuntimeQueryKind::Indents).unwrap().unwrap();
    assert_eq!(
        artifact.source_paths,
        vec![
            bundled_root.join("queries").join("rust").join("indents.scm"),
            user_root.join("queries").join("rust").join("indents.scm"),
            temp_dir
                .path()
                .join("workspace")
                .join(".ee")
                .join("queries")
                .join("rust")
                .join("indents.scm"),
        ]
    );
    assert!(artifact.source_text.contains("@base"));
    assert!(artifact.source_text.contains("@user"));
    assert!(artifact.source_text.contains("@workspace"));
}

#[test]
fn query_overlay_ignores_workspace_runtime_root_when_untrusted() {
    let temp_dir = TempDir::new().unwrap();
    let bundled_root = temp_dir.path().join("bundle");
    let user_root = temp_dir.path().join("user");
    let workspace_root = temp_dir.path().join("workspace").join(".ee");
    for (root, text) in [
        (&bundled_root, "((identifier) @base)\n"),
        (&user_root, "((identifier) @user)\n"),
        (&workspace_root, "((identifier) @workspace)\n"),
    ] {
        let query_dir = root.join("queries").join("rust");
        fs::create_dir_all(&query_dir).unwrap();
        fs::write(query_dir.join("indents.scm"), text).unwrap();
    }

    let roots = RuntimeRoots::new(&bundled_root, &user_root, Some(workspace_root));
    let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
    let languages = Languages::new(&[language_definition("rust", &["rs"])]);
    let mut overrides = RuntimeLanguageOverrides::new();
    overrides.insert(
        "rust".to_string(),
        RuntimeLanguageConfig {
            supported_query_kinds: Some(BTreeSet::from([RuntimeQueryKind::Indents])),
            ..RuntimeLanguageConfig::default()
        },
    );
    loader
        .reload_merged_languages(
            &languages,
            &overrides,
            Some(WorkspaceRuntimeOverrides {
                trusted: false,
                overrides: &RuntimeLanguageOverrides::new(),
            }),
        )
        .unwrap();

    let artifact = loader.resolve_query_source("rust", RuntimeQueryKind::Indents).unwrap().unwrap();
    assert_eq!(
        artifact.source_paths,
        vec![
            bundled_root.join("queries").join("rust").join("indents.scm"),
            user_root.join("queries").join("rust").join("indents.scm"),
        ]
    );
    assert!(artifact.source_text.contains("@base"));
    assert!(artifact.source_text.contains("@user"));
    assert!(!artifact.source_text.contains("@workspace"));
}

#[test]
fn runtime_loader_operations_ignore_untrusted_workspace_language() {
    let roots = RuntimeRoots::new("/bundle", "/user/ee", Some(PathBuf::from("/workspace/.ee")));
    let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
    let mut workspace_overrides = RuntimeLanguageOverrides::new();
    workspace_overrides.insert(
        String::from("demo"),
        RuntimeLanguageConfig {
            name: Some(String::from("Demo")),
            file_types: Some(vec![String::from("demo")]),
            grammar: Some(runtime_grammar_config("tree-sitter-demo", "tree_sitter_demo", "1.2.3")),
            ..RuntimeLanguageConfig::default()
        },
    );

    loader
        .reload_merged_languages(
            &Languages::default(),
            &RuntimeLanguageOverrides::new(),
            Some(WorkspaceRuntimeOverrides { trusted: false, overrides: &workspace_overrides }),
        )
        .unwrap();

    let error = loader.resolve_languages_for_operation(&[String::from("demo")], false).unwrap_err();
    assert_eq!(error.kind(), RuntimeOperationErrorKind::ConfigMerge);
    assert_eq!(error.to_string(), "unknown runtime language `demo`");
}

#[test]
fn runtime_loader_operations_apply_trusted_workspace_language() {
    let roots = RuntimeRoots::new("/bundle", "/user/ee", Some(PathBuf::from("/workspace/.ee")));
    let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
    let mut workspace_overrides = RuntimeLanguageOverrides::new();
    workspace_overrides.insert(
        String::from("demo"),
        RuntimeLanguageConfig {
            name: Some(String::from("Demo")),
            file_types: Some(vec![String::from("demo")]),
            grammar: Some(runtime_grammar_config("tree-sitter-demo", "tree_sitter_demo", "1.2.3")),
            ..RuntimeLanguageConfig::default()
        },
    );

    loader
        .reload_merged_languages(
            &Languages::default(),
            &RuntimeLanguageOverrides::new(),
            Some(WorkspaceRuntimeOverrides { trusted: true, overrides: &workspace_overrides }),
        )
        .unwrap();

    let resolved = loader.resolve_languages_for_operation(&[String::from("demo")], false).unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].canonical_id(), "demo");
    assert_eq!(resolved[0].asset_source(), RuntimeConfigSource::Workspace);
}
