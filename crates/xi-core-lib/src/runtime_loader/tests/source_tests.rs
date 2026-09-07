//! Runtime-loader tests: source.
use super::*;

#[test]
fn git_source_pin_redacts_url_credentials() {
    let plan = GrammarFetchPlan::Git(GrammarGitSpec {
        url: String::from("https://token:secret@example.com/org/tree-sitter-demo"),
        branch: Some(String::from("main")),
        tag: None,
        rev: None,
    });

    assert_eq!(plan.source_pin(), "git:https://example.com/org/tree-sitter-demo#branch:main");
    assert_eq!(
        plan.diagnostic_summary("demo"),
        "language `demo` git source url `https://example.com/org/tree-sitter-demo` ref branch `main`"
    );
}

fn test_runtime_language(name: &str) -> RuntimeLanguage {
    RuntimeLanguage {
        canonical_id: name.to_string(),
        display_name: name.to_string(),
        grammar_id: name.to_string(),
        grammar_library_name: Some(format!("tree-sitter-{}", normalize_lookup_key(name))),
        grammar_crate_version: Some("0.0.0".to_string()),
        grammar_symbol_name: Some(format!("tree_sitter_{}", normalize_lookup_key(name))),
        grammar_source: Some(RuntimeGrammarSource::Crate(RuntimeGrammarCrateSource {
            name: format!("tree-sitter-{}", normalize_lookup_key(name)),
            version: String::from("0.0.0"),
        })),
        query_language: name.to_string(),
        scope: None,
        content_regex: None,
        first_line_regex: None,
        injection_regex: None,
        aliases: Vec::new(),
        file_types: Vec::new(),
        globs: Vec::new(),
        shebangs: Vec::new(),
        supported_query_kinds: BTreeSet::new(),
        match_priority: 0,
        asset_source: RuntimeConfigSource::Bundled,
        has_base_definition: true,
        metadata: LanguageMetadata {
            line_comment: LineCommentStyle::Unsupported,
            block_comment: BlockCommentStyle::Unsupported,
            indentation: IndentationStrategy::Unsupported,
            unsupported_semantic_targets: &[],
        },
        standard_query_paths: RuntimeStandardQueryPaths::default(),
    }
}

#[test]
fn grammar_source_detection_accepts_tree_sitter_manifest_without_root_parser() {
    let temp_dir = TempDir::new().unwrap();
    let nested = temp_dir.path().join("tree-sitter-php-0.24.2");
    fs::create_dir_all(nested.join("php").join("src")).unwrap();
    fs::write(
        nested.join("tree-sitter.json"),
        r#"{
  "grammars": [
    {
      "name": "php",
      "path": "php"
    }
  ]
}"#,
    )
    .unwrap();
    fs::write(nested.join("php").join("src").join("parser.c"), "int parser(void) { return 0; }\n")
        .unwrap();

    assert!(looks_like_runtime_grammar_source(&nested));
    assert!(!looks_like_runtime_grammar_source(temp_dir.path().join("empty").as_path()));
}

#[test]
fn grammar_source_detection_accepts_nested_parser_directory_without_manifest() {
    let temp_dir = TempDir::new().unwrap();
    let nested = temp_dir.path().join("tree-sitter-typescript-0.23.2");
    fs::create_dir_all(nested.join("typescript").join("src")).unwrap();
    fs::write(
        nested.join("typescript").join("src").join("parser.c"),
        "int parser(void) { return 0; }\n",
    )
    .unwrap();

    assert!(looks_like_runtime_grammar_source(&nested));
}

#[test]
fn grammar_build_dir_uses_manifest_declared_subpath() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("tree-sitter-php");
    fs::create_dir_all(root.join("php").join("src")).unwrap();
    fs::write(
        root.join("tree-sitter.json"),
        r#"{
  "grammars": [
    {
      "name": "php",
      "path": "php"
    }
  ]
}"#,
    )
    .unwrap();
    fs::write(root.join("php").join("src").join("parser.c"), "int parser(void) { return 0; }\n")
        .unwrap();

    let resolved = resolve_staged_grammar_build_dir(&root, &test_runtime_language("PHP")).unwrap();
    assert_eq!(resolved, root.join("php"));
}

#[test]
fn grammar_build_dir_uses_matching_nested_parser_directory() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("tree-sitter-typescript");
    fs::create_dir_all(root.join("typescript").join("src")).unwrap();
    fs::create_dir_all(root.join("tsx").join("src")).unwrap();
    fs::write(
        root.join("typescript").join("src").join("parser.c"),
        "int parser(void) { return 0; }\n",
    )
    .unwrap();
    fs::write(root.join("tsx").join("src").join("parser.c"), "int parser(void) { return 0; }\n")
        .unwrap();

    let resolved =
        resolve_staged_grammar_build_dir(&root, &test_runtime_language("typescript")).unwrap();
    assert_eq!(resolved, root.join("typescript"));
}

#[test]
fn runtime_loader_rejects_ambiguous_file_type_without_priority() {
    let languages = Languages::new(&[
        language_definition("rust", &["rs"]),
        language_definition("Reason", &["rs"]),
    ]);
    let roots = RuntimeRoots::new("/bundle", "/user/ee", None);
    let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();

    let error = loader
        .reload_merged_languages(&languages, &RuntimeLanguageOverrides::new(), None)
        .unwrap_err();

    assert!(matches!(error, RuntimeLoaderError::AmbiguousFileType { .. }));
}

#[test]
fn runtime_loader_uses_priority_to_break_file_type_tie() {
    let languages = Languages::new(&[
        language_definition("rust", &["rs"]),
        language_definition("Reason", &["rs"]),
    ]);
    let roots = RuntimeRoots::new("/bundle", "/user/ee", None);
    let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();

    let mut user_overrides = RuntimeLanguageOverrides::new();
    user_overrides.insert(
        "reason".to_string(),
        RuntimeLanguageConfig { match_priority: Some(10), ..RuntimeLanguageConfig::default() },
    );

    loader.reload_merged_languages(&languages, &user_overrides, None).unwrap();

    assert_eq!(
        loader.language_for_path(Path::new("main.rs")).map(RuntimeLanguage::canonical_id),
        Some("Reason")
    );
}
