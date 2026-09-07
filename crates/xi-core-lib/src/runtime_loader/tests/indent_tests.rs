//! Runtime-loader tests: indent.
use super::*;

#[test]
fn indent_query_capture_contract_round_trips_names() {
    assert_eq!(IndentQueryCapture::from_capture_name("indent"), Some(IndentQueryCapture::Indent));
    assert_eq!(IndentQueryCapture::from_capture_name("dedent"), Some(IndentQueryCapture::Dedent));
    assert_eq!(IndentQueryCapture::from_capture_name("branch"), None);
    assert_eq!(IndentQueryCapture::allowed_names(), vec!["indent", "dedent"]);
}

#[test]
fn compile_indent_query_uses_shared_runtime_loader_path() {
    let temp_dir = TempDir::new().unwrap();
    let bundled_root = temp_dir.path().join("bundle");
    let query_dir = bundled_root.join("queries").join("rust");
    fs::create_dir_all(&query_dir).unwrap();
    let indents_path = query_dir.join("indents.scm");
    fs::write(&indents_path, "((block) @indent)").unwrap();

    let roots = RuntimeRoots::new(&bundled_root, temp_dir.path().join("user"), None);
    let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
    let languages = Languages::new(&[language_definition("rust", &["rs"])]);
    let mut overrides = RuntimeLanguageOverrides::new();
    overrides.insert(
        "rust".to_string(),
        RuntimeLanguageConfig {
            supported_query_kinds: Some(BTreeSet::from([RuntimeQueryKind::Indents])),
            ..runtime_language_override("tree-sitter-rust", "tree_sitter_rust")
        },
    );
    loader.reload_merged_languages(&languages, &overrides, None).unwrap();
    loader.preload_language(
        "rust",
        GrammarHandle::from_loaded(test_grammars::rust(), "__builtin__/rust", "tree_sitter_rust"),
    );

    let artifact = loader.resolve_indent_query_source("rust").unwrap().unwrap();
    assert_eq!(artifact.source_paths, vec![indents_path.clone()]);
    let compiled = loader.compile_indent_query("rust").unwrap().unwrap();
    assert_eq!(compiled.kind, RuntimeQueryKind::Indents);
    assert_eq!(compiled.source_paths, vec![indents_path]);
}

#[test]
fn invalid_indent_query_capture_reports_clear_error() {
    let temp_dir = TempDir::new().unwrap();
    let bundled_root = temp_dir.path().join("bundle");
    let query_dir = bundled_root.join("queries").join("rust");
    fs::create_dir_all(&query_dir).unwrap();
    let indents_path = query_dir.join("indents.scm");
    fs::write(&indents_path, "((block) @branch)").unwrap();

    let roots = RuntimeRoots::new(&bundled_root, temp_dir.path().join("user"), None);
    let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
    let languages = Languages::new(&[language_definition("rust", &["rs"])]);
    let mut overrides = RuntimeLanguageOverrides::new();
    overrides.insert(
        "rust".to_string(),
        RuntimeLanguageConfig {
            supported_query_kinds: Some(BTreeSet::from([RuntimeQueryKind::Indents])),
            ..runtime_language_override("tree-sitter-rust", "tree_sitter_rust")
        },
    );
    loader.reload_merged_languages(&languages, &overrides, None).unwrap();
    loader.preload_language(
        "rust",
        GrammarHandle::from_loaded(test_grammars::rust(), "__builtin__/rust", "tree_sitter_rust"),
    );

    let error = loader.compile_indent_query("rust").unwrap_err();
    match error {
        RuntimeLoaderError::InvalidQueryCapture { kind, file, capture, allowed } => {
            assert_eq!(kind, RuntimeQueryKind::Indents);
            assert_eq!(file.as_deref(), Some(indents_path.as_path()));
            assert_eq!(capture, "branch");
            assert_eq!(allowed, vec!["indent", "dedent"]);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn bundled_runtime_indent_queries_compile_for_rust_json_and_python() {
    let temp_dir = TempDir::new().unwrap();
    let bundled_root = temp_dir.path().join("bundle");
    let repo_runtime =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("runtime");

    for (language, file_types, grammar, symbol) in [
        ("rust", vec!["rs"], test_grammars::rust(), "tree_sitter_rust"),
        ("json", vec!["json"], test_grammars::json(), "tree_sitter_json"),
        ("python", vec!["py"], test_grammars::python(), "tree_sitter_python"),
    ] {
        let source = repo_runtime.join("queries").join(language).join("indents.scm");
        assert!(source.exists(), "missing bundled indent query {}", source.display());
        let query_dir = bundled_root.join("queries").join(language);
        fs::create_dir_all(&query_dir).unwrap();
        fs::copy(&source, query_dir.join("indents.scm")).unwrap();

        let roots = RuntimeRoots::new(&bundled_root, temp_dir.path().join("user"), None);
        let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
        let languages = Languages::new(&[language_definition(language, &file_types)]);
        let mut overrides = RuntimeLanguageOverrides::new();
        overrides.insert(
            language.to_string(),
            RuntimeLanguageConfig {
                supported_query_kinds: Some(BTreeSet::from([RuntimeQueryKind::Indents])),
                ..runtime_language_override(&format!("tree-sitter-{language}"), symbol)
            },
        );
        loader.reload_merged_languages(&languages, &overrides, None).unwrap();
        loader.preload_language(
            language,
            GrammarHandle::from_loaded(grammar, format!("__builtin__/{language}"), symbol),
        );

        let compiled = loader.compile_indent_query(language).unwrap().unwrap();
        assert_eq!(compiled.kind, RuntimeQueryKind::Indents);
        assert!(!compiled.source_text.trim().is_empty(), "compiled query empty for {language}");
    }
}

#[test]
fn standard_queries_fall_back_to_upstream_loader_metadata_when_overlay_absent() {
    let temp_dir = TempDir::new().unwrap();
    let parser_package = temp_dir.path().join("bundle").join("tree-sitter-rust");
    fs::create_dir_all(parser_package.join("queries")).unwrap();
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
    fs::write(
        parser_package.join("queries").join("highlights.scm"),
        "((function_item name: (identifier) @function))",
    )
    .unwrap();
    fs::write(parser_package.join("queries").join("locals.scm"), "((identifier) @local.reference)")
        .unwrap();
    fs::write(
        parser_package.join("queries").join("tags.scm"),
        "((function_item name: (identifier) @definition.function))",
    )
    .unwrap();

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

    let syntax = loader.compile_syntax_queries("rust").unwrap();
    assert!(syntax.combined_source.contains("@function"));
    assert!(syntax.combined_source.contains("@local.reference"));

    let tags = loader.compile_query_kind("rust", RuntimeQueryKind::Tags).unwrap().unwrap();
    assert!(tags.source_text.contains("@definition.function"));
    assert!(
        tags.source_paths.iter().any(|path| path.ends_with(Path::new("queries").join("tags.scm")))
    );
}
