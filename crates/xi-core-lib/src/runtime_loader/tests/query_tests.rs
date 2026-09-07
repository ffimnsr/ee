//! Runtime-loader tests: query.
use super::*;

#[test]
fn builtin_yaml_runtime_language_detects_yaml_files() {
    let loader = default_runtime_loader();

    assert_eq!(
        loader.language_for_path(Path::new("tasks.yaml")).map(RuntimeLanguage::canonical_id),
        Some("yaml")
    );
    assert_eq!(
        loader.language_for_path(Path::new("tasks.yml")).map(RuntimeLanguage::canonical_id),
        Some("yaml")
    );
}

#[test]
fn bundled_yaml_highlights_query_compiles() {
    let temp_dir = TempDir::new().unwrap();
    let bundled_root = temp_dir.path().join("bundle");
    let query_dir = bundled_root.join(QUERIES_DIR_NAME).join("yaml");
    fs::create_dir_all(&query_dir).unwrap();
    fs::write(
        query_dir.join("highlights.scm"),
        include_str!("../../../../../runtime/queries/yaml/highlights.scm"),
    )
    .unwrap();

    let roots = RuntimeRoots::new(&bundled_root, temp_dir.path().join("user"), None);
    let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
    let languages = Languages::new(&[language_definition("yaml", &["yaml", "yml"])]);
    let mut overrides = RuntimeLanguageOverrides::new();
    overrides.insert(
        "yaml".to_string(),
        RuntimeLanguageConfig {
            supported_query_kinds: Some(BTreeSet::from([RuntimeQueryKind::Highlights])),
            ..runtime_language_override("tree-sitter-yaml", "tree_sitter_yaml")
        },
    );
    loader.reload_merged_languages(&languages, &overrides, None).unwrap();
    loader.preload_language(
        "yaml",
        GrammarHandle::from_loaded(test_grammars::yaml(), "__builtin__/yaml", "tree_sitter_yaml"),
    );

    let compiled =
        loader.compile_query_kind("yaml", RuntimeQueryKind::Highlights).unwrap().unwrap();
    assert!(compiled.source_text.contains("@variable.other.member"));
}

#[test]
fn bundled_yaml_injections_query_compiles() {
    let temp_dir = TempDir::new().unwrap();
    let bundled_root = temp_dir.path().join("bundle");
    let query_dir = bundled_root.join(QUERIES_DIR_NAME).join("yaml");
    fs::create_dir_all(&query_dir).unwrap();
    fs::write(
        query_dir.join("injections.scm"),
        include_str!("../../../../../runtime/queries/yaml/injections.scm"),
    )
    .unwrap();

    let roots = RuntimeRoots::new(&bundled_root, temp_dir.path().join("user"), None);
    let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
    let languages = Languages::new(&[language_definition("yaml", &["yaml", "yml"])]);
    let mut overrides = RuntimeLanguageOverrides::new();
    overrides.insert(
        "yaml".to_string(),
        RuntimeLanguageConfig {
            supported_query_kinds: Some(BTreeSet::from([RuntimeQueryKind::Injections])),
            ..runtime_language_override("tree-sitter-yaml", "tree_sitter_yaml")
        },
    );
    loader.reload_merged_languages(&languages, &overrides, None).unwrap();
    loader.preload_language(
        "yaml",
        GrammarHandle::from_loaded(test_grammars::yaml(), "__builtin__/yaml", "tree_sitter_yaml"),
    );

    let compiled =
        loader.compile_query_kind("yaml", RuntimeQueryKind::Injections).unwrap().unwrap();
    assert!(compiled.source_text.contains("injection.content"));
    assert!(compiled.source_text.contains("injection.language"));
}

#[test]
fn bundled_builtin_highlights_queries_compile() {
    let _guard = runtime_loader_test_guard();
    ensure_default_runtime_loader_has_test_grammars();

    for language in [
        "bash", "c", "csharp", "css", "elixir", "go", "haskell", "html", "java", "json", "php",
        "python", "ruby", "rust", "scala", "yaml",
    ] {
        with_default_runtime_loader_mut(|loader| {
            loader.invalidate_language(language);
            let compiled = loader
                .compile_query_kind(language, RuntimeQueryKind::Highlights)
                .unwrap_or_else(|error| panic!("failed compiling {language} highlights: {error}"))
                .unwrap_or_else(|| panic!("missing bundled highlights query for {language}"));
            assert!(
                !compiled.source_text.trim().is_empty(),
                "expected non-empty bundled highlights for {language}",
            );
        });
    }
}

#[test]
fn runtime_loader_detects_shebang_glob_then_file_type() {
    let languages = Languages::new(&[
        language_definition("rust", &["rs"]),
        language_definition("Shell", &["sh"]),
    ]);
    let roots = RuntimeRoots::new("/bundle", "/user/ee", None);
    let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();

    let mut overrides = RuntimeLanguageOverrides::new();
    overrides.insert(
        "rust".to_string(),
        RuntimeLanguageConfig {
            globs: Some(vec!["*.rs.in".to_string()]),
            content_regex: Some(String::from("\\bfn\\s+main\\b")),
            match_priority: Some(20),
            ..RuntimeLanguageConfig::default()
        },
    );
    overrides.insert(
        "shell".to_string(),
        RuntimeLanguageConfig {
            shebangs: Some(vec!["#!/usr/bin/env bash".to_string()]),
            ..RuntimeLanguageConfig::default()
        },
    );
    loader.reload_merged_languages(&languages, &overrides, None).unwrap();

    let shebang = loader
        .detect_language(
            Some(Path::new("script.unknown")),
            Some("#!/usr/bin/env bash"),
            Some("#!/usr/bin/env bash\necho hi\n"),
        )
        .unwrap();
    assert_eq!(shebang.canonical_id, "Shell");
    assert_eq!(shebang.detection_source, RuntimeLanguageDetectionSource::Shebang);

    let glob = loader.detect_language(Some(Path::new("main.rs.in")), None, None).unwrap();
    assert_eq!(glob.canonical_id, "rust");
    assert_eq!(glob.detection_source, RuntimeLanguageDetectionSource::Glob);

    let file_type = loader.detect_language(Some(Path::new("main.rs")), None, None).unwrap();
    assert_eq!(file_type.canonical_id, "rust");
    assert_eq!(file_type.detection_source, RuntimeLanguageDetectionSource::FileType);

    let content =
        loader.detect_language(None, None, Some("fn main() { println!(\"hi\"); }")).unwrap();
    assert_eq!(content.canonical_id, "rust");
    assert_eq!(content.detection_source, RuntimeLanguageDetectionSource::ContentRegex);
}

#[test]
fn runtime_loader_matches_injection_language_by_regex_and_priority() {
    let languages = Languages::new(&[
        language_definition("javascript", &["js"]),
        language_definition("typescript", &["ts"]),
    ]);
    let roots = RuntimeRoots::new("/bundle", "/user/ee", None);
    let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();

    let mut overrides = RuntimeLanguageOverrides::new();
    overrides.insert(
        "javascript".to_string(),
        RuntimeLanguageConfig {
            injection_regex: Some(String::from("^(js|javascript)$")),
            match_priority: Some(5),
            ..RuntimeLanguageConfig::default()
        },
    );
    overrides.insert(
        "typescript".to_string(),
        RuntimeLanguageConfig {
            injection_regex: Some(String::from("^(ts|tsx|javascript)$")),
            match_priority: Some(10),
            ..RuntimeLanguageConfig::default()
        },
    );

    loader.reload_merged_languages(&languages, &overrides, None).unwrap();

    let tsx = loader.match_injection_language("tsx").unwrap();
    assert_eq!(tsx.canonical_id, "typescript");

    let javascript = loader.match_injection_language("javascript").unwrap();
    assert_eq!(javascript.canonical_id, "typescript");

    assert!(loader.match_injection_language("sql").is_none());
}
