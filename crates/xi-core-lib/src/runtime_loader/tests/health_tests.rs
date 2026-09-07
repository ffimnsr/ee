//! Runtime-loader tests: health.
use super::*;

#[test]
fn malformed_standard_query_reports_upstream_file_path() {
    let temp_dir = TempDir::new().unwrap();
    let parser_package = temp_dir.path().join("bundle").join("tree-sitter-rust");
    fs::create_dir_all(parser_package.join("src")).unwrap();
    fs::create_dir_all(parser_package.join("queries")).unwrap();
    let highlights_path = parser_package.join("queries").join("highlights.scm");
    fs::write(
        parser_package.join("tree-sitter.json"),
        r#"{
  "grammars": [
    {
      "name": "rust",
      "scope": "source.rust",
      "file-types": ["rs"],
            "highlights": "queries/highlights.scm",
            "locals": "queries/locals.scm",
            "tags": "queries/tags.scm"
    }
    ],
    "metadata": {
        "version": "0.1.0"
    }
}"#,
    )
    .unwrap();
    fs::write(parser_package.join("src").join("parser.c"), "int parser(void) { return 0; }\n")
        .unwrap();
    fs::write(parser_package.join("queries").join("locals.scm"), "((identifier) @local.reference)")
        .unwrap();
    fs::write(
        parser_package.join("queries").join("tags.scm"),
        "((function_item name: (identifier) @definition.function))",
    )
    .unwrap();
    fs::write(&highlights_path, "((function_item").unwrap();

    let roots =
        RuntimeRoots::new(temp_dir.path().join("bundle-root"), temp_dir.path().join("user"), None);
    let mut loader = RuntimeLoader::new(roots, vec![temp_dir.path().join("bundle")]).unwrap();
    let languages = Languages::new(&[language_definition("rust", &["rs"])]);
    let mut overrides = RuntimeLanguageOverrides::new();
    overrides.insert(
        "rust".to_string(),
        RuntimeLanguageConfig {
            supported_query_kinds: Some(BTreeSet::from([
                RuntimeQueryKind::Highlights,
                RuntimeQueryKind::Locals,
                RuntimeQueryKind::Tags,
            ])),
            ..runtime_language_override("tree-sitter-rust", "tree_sitter_rust")
        },
    );
    loader.reload_merged_languages(&languages, &overrides, None).unwrap();
    loader.preload_language(
        "rust",
        GrammarHandle::from_loaded(test_grammars::rust(), "__builtin__/rust", "tree_sitter_rust"),
    );

    let error = loader.compile_syntax_queries("rust").unwrap_err();
    match error {
        RuntimeLoaderError::QueryCompile { kind, file, .. } => {
            assert_eq!(kind, RuntimeQueryKind::Highlights);
            assert_eq!(file.as_deref(), Some(highlights_path.as_path()));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn broken_shared_library_reports_error_without_poisoning_other_languages() {
    let temp_dir = TempDir::new().unwrap();
    let bundled_root = temp_dir.path().join("bundle");
    let user_root = temp_dir.path().join("user");
    let grammar_dir = user_root.join(GRAMMARS_DIR_NAME);
    fs::create_dir_all(&grammar_dir).unwrap();
    let rust_library = grammar_dir.join(shared_library_filename("tree-sitter-rust"));
    fs::write(&rust_library, b"not-a-shared-library").unwrap();

    let roots = RuntimeRoots::new(&bundled_root, &user_root, None);
    let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
    let languages = Languages::new(&[
        language_definition("rust", &["rs"]),
        language_definition("JSON", &["json"]),
    ]);
    let mut overrides = RuntimeLanguageOverrides::new();
    overrides.insert(
        "rust".to_string(),
        runtime_language_override("tree-sitter-rust", "tree_sitter_rust"),
    );
    loader.reload_merged_languages(&languages, &overrides, None).unwrap();
    loader.preload_language(
        "JSON",
        GrammarHandle::from_loaded(test_grammars::json(), "__builtin__/json", "tree_sitter_json"),
    );

    let report =
        loader.runtime_health_report(Some("rust"), Some(Path::new("main.rs")), None, None, None);
    match report.grammar_status {
        RuntimeGrammarHealth::Error(message) => {
            assert!(message.contains("tree-sitter-rust") || message.contains("rust"));
        }
        other => panic!("unexpected grammar status: {other:?}"),
    }

    assert!(matches!(loader.load_language_for_name("rust"), Err(RuntimeLoaderError::Loader(_))));
    assert!(loader.load_language_for_name("JSON").is_ok());
}

#[test]
fn runtime_health_report_distinguishes_loaded_missing_and_unsupported_queries() {
    let temp_dir = TempDir::new().unwrap();
    let bundled_root = temp_dir.path().join("bundle");
    fs::create_dir_all(bundled_root.join("queries").join("rust")).unwrap();
    fs::write(
        bundled_root.join("queries").join("rust").join("highlights.scm"),
        "((function_item name: (identifier) @function))",
    )
    .unwrap();

    let roots = RuntimeRoots::new(&bundled_root, temp_dir.path().join("user"), None);
    let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
    let languages = Languages::new(&[language_definition("rust", &["rs"])]);
    let mut overrides = RuntimeLanguageOverrides::new();
    overrides.insert(
        "rust".to_string(),
        RuntimeLanguageConfig {
            supported_query_kinds: Some(BTreeSet::from([
                RuntimeQueryKind::Highlights,
                RuntimeQueryKind::Indents,
            ])),
            ..runtime_language_override("tree-sitter-rust", "tree_sitter_rust")
        },
    );
    loader.reload_merged_languages(&languages, &overrides, None).unwrap();
    loader.preload_language(
        "rust",
        GrammarHandle::from_loaded(test_grammars::rust(), "__builtin__/rust", "tree_sitter_rust"),
    );

    let report =
        loader.runtime_health_report(Some("rust"), Some(Path::new("main.rs")), None, None, None);
    assert_eq!(report.grammar_status, RuntimeGrammarHealth::Loaded);
    assert_eq!(report.detection_source, Some(RuntimeLanguageDetectionSource::Explicit));
    assert!(report.query_reports.iter().any(|query| {
        query.kind == RuntimeQueryKind::Highlights && query.status == RuntimeQueryHealth::Loaded
    }));
    assert!(report.query_reports.iter().any(|query| {
        query.kind == RuntimeQueryKind::Indents && query.status == RuntimeQueryHealth::Missing
    }));
    assert!(report.query_reports.iter().any(|query| {
        query.kind == RuntimeQueryKind::Tags && query.status == RuntimeQueryHealth::Unsupported
    }));
}
