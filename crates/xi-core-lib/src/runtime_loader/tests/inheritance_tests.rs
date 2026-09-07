//! Runtime-loader tests: inheritance.
use super::*;

#[test]
fn query_inheritance_merges_parent_before_child() {
    let temp_dir = TempDir::new().unwrap();
    let bundled_root = temp_dir.path().join("bundle");
    fs::create_dir_all(bundled_root.join("queries").join("rust")).unwrap();
    let base_query_dir = runtime_query_dir_name("Base");
    fs::create_dir_all(bundled_root.join("queries").join(&base_query_dir)).unwrap();
    fs::write(
        bundled_root.join("queries").join(&base_query_dir).join("textobjects.scm"),
        "((identifier) @base)",
    )
    .unwrap();
    fs::write(
        bundled_root.join("queries").join("rust").join("textobjects.scm"),
        "; inherits: Base\n((function_item) @function.outer)",
    )
    .unwrap();

    let roots = RuntimeRoots::new(&bundled_root, temp_dir.path().join("user"), None);
    let mut loader = RuntimeLoader::new(roots.clone(), Vec::new()).unwrap();
    let languages = Languages::new(&[
        language_definition("Base", &["base"]),
        language_definition("rust", &["rs"]),
    ]);
    let mut overrides = RuntimeLanguageOverrides::new();
    overrides.insert(
        "rust".to_string(),
        RuntimeLanguageConfig {
            supported_query_kinds: Some(BTreeSet::from([RuntimeQueryKind::Textobjects])),
            ..runtime_language_override("tree-sitter-rust", "tree_sitter_rust")
        },
    );
    overrides.insert(
        "Base".to_string(),
        RuntimeLanguageConfig {
            supported_query_kinds: Some(BTreeSet::from([RuntimeQueryKind::Textobjects])),
            ..runtime_language_override("tree-sitter-rust", "tree_sitter_rust")
        },
    );
    loader.reload_merged_languages(&languages, &overrides, None).unwrap();
    loader.preload_language(
        "rust",
        GrammarHandle::from_loaded(test_grammars::rust(), "__builtin__/rust", "tree_sitter_rust"),
    );
    loader.preload_language(
        "Base",
        GrammarHandle::from_loaded(test_grammars::rust(), "__builtin__/base", "tree_sitter_rust"),
    );

    let artifact =
        loader.compile_query_kind("rust", RuntimeQueryKind::Textobjects).unwrap().unwrap();
    assert!(artifact.source_text.contains("@base"));
    assert!(artifact.source_text.contains("@function.outer"));
    assert!(
        artifact.source_text.find("@base").unwrap()
            < artifact.source_text.find("@function.outer").unwrap()
    );
}

#[test]
fn query_inheritance_cycle_reports_error() {
    let temp_dir = TempDir::new().unwrap();
    let bundled_root = temp_dir.path().join("bundle");
    fs::create_dir_all(bundled_root.join("queries").join("rust")).unwrap();
    let base_query_dir = runtime_query_dir_name("Base");
    fs::create_dir_all(bundled_root.join("queries").join(&base_query_dir)).unwrap();
    fs::write(
        bundled_root.join("queries").join("rust").join("indents.scm"),
        "; inherits: Base\n((block) @indent)",
    )
    .unwrap();
    fs::write(
        bundled_root.join("queries").join(&base_query_dir).join("indents.scm"),
        "; inherits: Rust\n((source_file) @indent)",
    )
    .unwrap();

    let roots = RuntimeRoots::new(&bundled_root, temp_dir.path().join("user"), None);
    let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
    let languages = Languages::new(&[
        language_definition("Base", &["base"]),
        language_definition("rust", &["rs"]),
    ]);
    loader.reload_merged_languages(&languages, &RuntimeLanguageOverrides::new(), None).unwrap();

    let error = loader.resolve_query_source("rust", RuntimeQueryKind::Indents).unwrap_err();
    assert!(matches!(error, RuntimeLoaderError::QueryInheritanceCycle { .. }));
}

#[test]
fn syntax_queries_compile_standard_groups_together() {
    let temp_dir = TempDir::new().unwrap();
    let bundled_root = temp_dir.path().join("bundle");
    fs::create_dir_all(bundled_root.join("queries").join("rust")).unwrap();
    fs::write(
        bundled_root.join("queries").join("rust").join("highlights.scm"),
        "((function_item name: (identifier) @function))",
    )
    .unwrap();
    fs::write(
        bundled_root.join("queries").join("rust").join("locals.scm"),
        "((identifier) @local.reference)",
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
                RuntimeQueryKind::Locals,
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
    assert!(syntax.combined_query.is_some());
    assert!(syntax.combined_source.contains("@function"));
    assert!(syntax.combined_source.contains("@local.reference"));
}

#[test]
fn missing_optional_queries_do_not_disable_loaded_syntax_queries() {
    let temp_dir = TempDir::new().unwrap();
    let bundled_root = temp_dir.path().join("bundle");
    fs::create_dir_all(bundled_root.join(QUERIES_DIR_NAME).join("rust")).unwrap();
    fs::write(
        bundled_root.join(QUERIES_DIR_NAME).join("rust").join("highlights.scm"),
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
                RuntimeQueryKind::Textobjects,
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

    let syntax = loader.compile_syntax_queries("rust").unwrap();
    assert!(syntax.combined_query.is_some());
    assert!(syntax.highlights.is_some());

    let semantic = loader.compile_semantic_queries("rust").unwrap();
    assert!(semantic.textobjects.is_none());
    assert!(semantic.tags.is_none());
    assert!(loader.compile_query_kind("rust", RuntimeQueryKind::Indents).unwrap().is_none());
}
