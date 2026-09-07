//! Runtime-loader tests: cache.
use super::*;

#[test]
fn runtime_loader_caches_use_canonical_paths_and_support_invalidation() {
    let temp_dir = TempDir::new().unwrap();
    let library_path = temp_dir.path().join(shared_library_filename("tree-sitter-rust"));
    fs::write(&library_path, b"stub").unwrap();
    let query_path = temp_dir.path().join("highlights.scm");
    fs::write(&query_path, b"(function_item)").unwrap();

    let roots = RuntimeRoots::new("/bundle", "/user/ee", None);
    let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
    let handle =
        GrammarHandle::from_loaded(test_grammars::rust(), &library_path, "tree_sitter_rust");
    loader.record_grammar_handle(handle);
    loader.record_query_artifact(
        "rust",
        RuntimeQueryKind::Highlights,
        "(function_item)".to_string(),
        vec![query_path.clone()],
        vec![(query_path.clone(), 0..15)],
    );

    assert!(loader.cached_grammar_handle(&library_path).is_some());
    assert!(loader.cached_query_artifact("rust", RuntimeQueryKind::Highlights).is_some());

    loader.invalidate_all();

    assert!(loader.cached_grammar_handle(&library_path).is_none());
    assert!(loader.cached_query_artifact("rust", RuntimeQueryKind::Highlights).is_none());
}

#[test]
fn grammar_cache_invalidates_when_library_file_changes() {
    let temp_dir = TempDir::new().unwrap();
    let bundled_root = temp_dir.path().join("bundle");
    let user_root = temp_dir.path().join("user");
    let grammar_dir = user_root.join(GRAMMARS_DIR_NAME);
    fs::create_dir_all(&grammar_dir).unwrap();
    let library_path = grammar_dir.join(shared_library_filename("tree-sitter-rust"));
    fs::write(&library_path, b"stub").unwrap();

    let roots = RuntimeRoots::new(&bundled_root, &user_root, None);
    let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
    let languages = Languages::new(&[language_definition("rust", &["rs"])]);
    let mut overrides = RuntimeLanguageOverrides::new();
    overrides.insert(
        "rust".to_string(),
        runtime_language_override("tree-sitter-rust", "tree_sitter_rust"),
    );
    loader.reload_merged_languages(&languages, &overrides, None).unwrap();

    let cached =
        GrammarHandle::from_loaded(test_grammars::rust(), &library_path, "tree_sitter_rust");
    loader.record_grammar_handle(cached.clone());

    let first = loader.load_language_for_name("rust").unwrap();
    assert_eq!(first.canonical_library_path(), cached.canonical_library_path());

    write_until_modified(&library_path, b"changed-stub".to_vec());

    assert!(loader.cached_grammar_handle(&library_path).is_none());
    let error = loader.load_language_for_name("rust").unwrap_err();
    assert!(matches!(error, RuntimeLoaderError::Loader(_)));
}

#[test]
fn query_cache_refreshes_when_query_file_changes() {
    let temp_dir = TempDir::new().unwrap();
    let bundled_root = temp_dir.path().join("bundle");
    let query_dir = bundled_root.join(QUERIES_DIR_NAME).join("rust");
    fs::create_dir_all(&query_dir).unwrap();
    let query_path = query_dir.join("highlights.scm");
    fs::write(&query_path, "((identifier) @old)").unwrap();

    let roots = RuntimeRoots::new(&bundled_root, temp_dir.path().join("user"), None);
    let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
    let languages = Languages::new(&[language_definition("rust", &["rs"])]);
    let mut overrides = RuntimeLanguageOverrides::new();
    overrides.insert(
        "rust".to_string(),
        RuntimeLanguageConfig {
            supported_query_kinds: Some(BTreeSet::from([RuntimeQueryKind::Highlights])),
            ..runtime_language_override("tree-sitter-rust", "tree_sitter_rust")
        },
    );
    loader.reload_merged_languages(&languages, &overrides, None).unwrap();
    loader.preload_language(
        "rust",
        GrammarHandle::from_loaded(test_grammars::rust(), "__builtin__/rust", "tree_sitter_rust"),
    );

    let first = loader.resolve_query_source("rust", RuntimeQueryKind::Highlights).unwrap().unwrap();
    assert!(first.source_text.contains("@old"));

    write_until_modified(&query_path, b"((identifier) @new)".to_vec());

    let refreshed =
        loader.resolve_query_source("rust", RuntimeQueryKind::Highlights).unwrap().unwrap();
    assert!(refreshed.source_text.contains("@new"));

    let compiled = loader.compile_query_kind("rust", RuntimeQueryKind::Highlights).unwrap();
    assert!(compiled.unwrap().source_text.contains("@new"));
}

#[test]
fn compiled_query_cache_reuses_compiled_query_until_invalidation() {
    let temp_dir = TempDir::new().unwrap();
    let bundled_root = temp_dir.path().join("bundle");
    fs::create_dir_all(bundled_root.join("queries").join("rust")).unwrap();
    fs::write(
        bundled_root.join("queries").join("rust").join("tags.scm"),
        "((function_item name: (identifier) @definition.function))",
    )
    .unwrap();

    let roots = RuntimeRoots::new(&bundled_root, temp_dir.path().join("user"), None);
    let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
    let languages = Languages::new(&[language_definition("rust", &["rs"])]);
    let mut overrides = RuntimeLanguageOverrides::new();
    overrides.insert(
        "rust".to_string(),
        RuntimeLanguageConfig {
            supported_query_kinds: Some(BTreeSet::from([RuntimeQueryKind::Tags])),
            ..runtime_language_override("tree-sitter-rust", "tree_sitter_rust")
        },
    );
    loader.reload_merged_languages(&languages, &overrides, None).unwrap();
    loader.preload_language(
        "rust",
        GrammarHandle::from_loaded(test_grammars::rust(), "__builtin__/rust", "tree_sitter_rust"),
    );

    let first = loader.compile_query_kind("rust", RuntimeQueryKind::Tags).unwrap().unwrap();
    let second = loader.compile_query_kind("rust", RuntimeQueryKind::Tags).unwrap().unwrap();
    assert!(Arc::ptr_eq(&first, &second));

    loader.invalidate_language("rust");
    let third = loader.compile_query_kind("rust", RuntimeQueryKind::Tags).unwrap().unwrap();
    assert!(!Arc::ptr_eq(&first, &third));
}

#[test]
fn runtime_loader_bootstraps_builtin_runtime_metadata() {
    let loader = default_runtime_loader();
    let rust = loader.language_for_name("rust").unwrap();
    assert_eq!(rust.display_name(), "rust");
    assert_eq!(rust.grammar_symbol_name(), Some("tree_sitter_rust"));
    assert_eq!(rust.metadata().line_comment, LineCommentStyle::Token("//"));
    assert!(!loader.preloaded_grammars.contains_key(&normalize_lookup_key("rust")));
}

#[test]
fn test_grammar_bootstrap_populates_default_loader_only_in_tests() {
    let _guard = runtime_loader_test_guard();
    ensure_default_runtime_loader_has_test_grammars();
    with_default_runtime_loader(|loader| {
        assert!(loader.preloaded_grammars.contains_key(&normalize_lookup_key("rust")));
    });
}

#[test]
fn identical_default_loader_overrides_preserve_runtime_caches() {
    let _guard = runtime_loader_test_guard();
    let mut changed_overrides = RuntimeLanguageOverrides::new();
    changed_overrides.insert(
        String::from("rust"),
        RuntimeLanguageConfig { enabled: Some(true), ..RuntimeLanguageConfig::default() },
    );
    configure_default_runtime_loader_overrides(
        changed_overrides,
        RuntimeLanguageOverrides::new(),
        false,
    )
    .unwrap();
    configure_default_runtime_loader_overrides(
        RuntimeLanguageOverrides::new(),
        RuntimeLanguageOverrides::new(),
        false,
    )
    .unwrap();

    let cache_path = PathBuf::from("__test__/repeated-config-cache");
    with_default_runtime_loader_mut(|loader| {
        loader.record_grammar_handle(GrammarHandle::from_loaded(
            test_grammars::rust(),
            &cache_path,
            "tree_sitter_rust",
        ));
    });

    configure_default_runtime_loader_overrides_if_changed(
        RuntimeLanguageOverrides::new(),
        RuntimeLanguageOverrides::new(),
        false,
    )
    .unwrap();

    with_default_runtime_loader(|loader| {
        assert!(loader.grammar_cache.contains_key(&cache_path));
    });
    ensure_default_runtime_loader_has_test_grammars();
}
