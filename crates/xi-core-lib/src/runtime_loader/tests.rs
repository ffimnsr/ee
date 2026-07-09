use super::*;

use std::collections::BTreeSet;
use std::env;
use std::fs;

use ee_ts_test_grammars as test_grammars;

use crate::syntax::{LanguageDefinition, Languages};
use crate::tree_sitter_support::{
    BlockCommentStyle, IndentationStrategy, LanguageMetadata, LineCommentStyle,
};
use std::iter;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use tempfile::TempDir;

fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|error| error.into_inner())
}

fn language_definition(name: &str, extensions: &[&str]) -> LanguageDefinition {
    LanguageDefinition {
        name: name.into(),
        extensions: extensions.iter().map(|value| (*value).to_string()).collect(),
        first_line_match: None,
        scope: format!("source.{}", name.to_ascii_lowercase()),
        default_config: None,
    }
}

fn runtime_grammar_config(library: &str, symbol: &str, version: &str) -> RuntimeGrammarConfig {
    RuntimeGrammarConfig {
        library: Some(library.to_string()),
        symbol: Some(symbol.to_string()),
        source: Some(RuntimeGrammarSource::Crate(RuntimeGrammarCrateSource {
            name: library.to_string(),
            version: version.to_string(),
        })),
    }
}

fn runtime_language_override(library: &str, symbol: &str) -> RuntimeLanguageConfig {
    RuntimeLanguageConfig {
        grammar: Some(runtime_grammar_config(library, symbol, "0.0.0")),
        ..RuntimeLanguageConfig::default()
    }
}

fn write_until_modified(path: &Path, contents: impl Into<Vec<u8>>) {
    let mut contents = contents.into();
    let original = metadata_modified_time(path);
    for marker in 0u8..=32 {
        fs::write(path, &contents).unwrap();
        if metadata_modified_time(path) != original {
            return;
        }
        contents.extend(iter::once(marker));
    }
    panic!("mtime did not change for {}", path.display());
}

#[test]
fn runtime_roots_follow_directory_contract() {
    let roots = RuntimeRoots::new(
        "/opt/ee/runtime",
        RuntimeRoots::user_root_for_data_dir(Path::new("/tmp/data")),
        Some(PathBuf::from("/work/project/.ee")),
    );

    assert_eq!(roots.user_root(), Path::new("/tmp/data/ee"));
    assert_eq!(
        roots.grammar_dir_for(RuntimeConfigSource::User).as_deref(),
        Some(Path::new("/tmp/data/ee/grammars"))
    );
    assert_eq!(
        roots.query_dir_for(RuntimeConfigSource::User, "rust").as_deref(),
        Some(Path::new("/tmp/data/ee/queries/rust"))
    );
    assert_eq!(
        roots.parser_directories(true),
        vec![
            PathBuf::from("/opt/ee/runtime"),
            PathBuf::from("/tmp/data/ee"),
            PathBuf::from("/work/project/.ee")
        ]
    );
    assert_eq!(
        roots.parser_directories(false),
        vec![PathBuf::from("/opt/ee/runtime"), PathBuf::from("/tmp/data/ee")]
    );
}

#[test]
fn runtime_query_dir_names_are_terminal_friendly() {
    assert_eq!(runtime_query_dir_name("rust"), "rust");
    assert_eq!(runtime_query_dir_name("csharp"), "csharp");
    assert_eq!(runtime_query_dir_name("cpp"), "cpp");
    assert_eq!(runtime_query_dir_name("typescript"), "typescript");
}

#[test]
fn bundled_runtime_root_prefers_env_then_release_layouts() {
    let fallback = Path::new("/tmp/runtime-fallback");
    let windows_exe = Path::new("C:/Program Files/ee/ee.exe");

    assert_eq!(
        resolve_bundled_runtime_root(
            Some(Path::new("/custom/runtime")),
            Some(Path::new("/opt/ee/bin/ee")),
            fallback,
            false,
        ),
        PathBuf::from("/custom/runtime")
    );
    assert_eq!(
        resolve_bundled_runtime_root(None, Some(Path::new("/opt/ee/bin/ee")), fallback, false),
        PathBuf::from("/opt/ee/share/ee")
    );
    assert_eq!(
        resolve_bundled_runtime_root(None, Some(windows_exe), fallback, true,),
        PathBuf::from("C:/Program Files/ee/runtime")
    );
}

#[test]
fn runtime_loading_disabled_reason_tracks_supported_targets() {
    assert_eq!(runtime_loading_disabled_reason_for(true), None);
    assert_eq!(
        runtime_loading_disabled_reason_for(false),
        Some("shared-library runtime grammars are only supported on Linux, macOS, and Windows")
    );
}

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

#[test]
fn runtime_loader_fetches_grammar_source_from_cargo_registry() {
    let _guard = env_lock();
    let loader = default_runtime_loader();
    let temp_dir = TempDir::new().unwrap();

    let fetched = loader
        .fetch_grammar_sources(&[String::from("rust")], false, temp_dir.path(), true)
        .unwrap();

    assert_eq!(fetched.len(), 1);
    assert!(fetched[0].source_pin.starts_with("crate:"));
    assert_eq!(fetched[0].resolved_rev, None);
    assert!(fetched[0].source_dir.join("tree-sitter.json").exists());
    assert!(fetched[0].source_dir.join("src").join("parser.c").exists());
}

#[test]
fn runtime_loader_fetches_versioned_grammar_without_workspace_dependency_edit() {
    let _guard = env_lock();
    let temp_dir = TempDir::new().unwrap();
    let cargo_home = temp_dir.path().join("cargo-home");
    let registry_source =
        cargo_home.join("registry").join("src").join("test-index").join("tree-sitter-demo-1.2.3");
    fs::create_dir_all(registry_source.join("src")).unwrap();

    let cargo_script = temp_dir.path().join("fake-cargo.sh");
    fs::write(
        &cargo_script,
        format!(
            "#!/bin/sh\nset -eu\nmanifest=\"\"\nwhile [ \"$#\" -gt 0 ]; do\n  case \"$1\" in\n    --manifest-path) manifest=\"$2\"; shift 2 ;;\n    *) shift ;;\n  esac\ndone\n[ -n \"$manifest\" ]\ngrep -q 'tree-sitter-demo = \"=1.2.3\"' \"$manifest\"\nmkdir -p \"{}\"\nprintf '{{\"grammars\":[{{\"name\":\"Demo\",\"scope\":\"source.demo\",\"file-types\":[\"demo\"],\"path\":\".\"}}],\"metadata\":{{\"version\":\"1.2.3\"}}}}' > \"{}/tree-sitter.json\"\nprintf 'int tree_sitter_demo(void) {{ return 0; }}\n' > \"{}/src/parser.c\"\n",
            registry_source.display(),
            registry_source.display(),
            registry_source.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&cargo_script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&cargo_script, permissions).unwrap();
    }

    let languages = Languages::new(&[language_definition("Demo", &["demo"])]);
    let roots =
        RuntimeRoots::new(temp_dir.path().join("bundle"), temp_dir.path().join("user"), None);
    let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
    let mut overrides = RuntimeLanguageOverrides::new();
    overrides.insert(
        "Demo".to_string(),
        RuntimeLanguageConfig {
            grammar: Some(runtime_grammar_config("tree-sitter-demo", "tree_sitter_demo", "1.2.3")),
            ..RuntimeLanguageConfig::default()
        },
    );
    loader.reload_merged_languages(&languages, &overrides, None).unwrap();

    let original_cargo_home = env::var_os("CARGO_HOME");
    let original_cargo = env::var_os("CARGO");
    unsafe {
        env::set_var("CARGO_HOME", &cargo_home);
        env::set_var("CARGO", &cargo_script);
    }

    let fetched = loader
        .fetch_grammar_sources(
            &[String::from("Demo")],
            false,
            temp_dir.path().join("sources").as_path(),
            true,
        )
        .unwrap();

    unsafe {
        if let Some(value) = original_cargo_home {
            env::set_var("CARGO_HOME", value);
        } else {
            env::remove_var("CARGO_HOME");
        }
        if let Some(value) = original_cargo {
            env::set_var("CARGO", value);
        } else {
            env::remove_var("CARGO");
        }
    }

    assert_eq!(fetched[0].crate_name, "tree-sitter-demo");
    assert_eq!(fetched[0].resolved_rev, None);
    assert!(fetched[0].source_dir.join("tree-sitter.json").exists());
    assert!(fetched[0].source_dir.join("src").join("parser.c").exists());
}

#[test]
fn runtime_loader_builds_runtime_assets_from_fetched_sources() {
    let _guard = env_lock();
    let loader = default_runtime_loader();
    let temp_dir = TempDir::new().unwrap();
    let source_root = temp_dir.path().join("sources");
    let output_root = temp_dir.path().join("runtime");
    let original_host = env::var_os("HOST");
    let original_target = env::var_os("TARGET");

    unsafe {
        env::remove_var("HOST");
        env::remove_var("TARGET");
    }

    let built = loader.build_runtime_assets(
        &[String::from("rust")],
        false,
        &source_root,
        &output_root,
        true,
        false,
    );

    unsafe {
        if let Some(value) = original_host {
            env::set_var("HOST", value);
        } else {
            env::remove_var("HOST");
        }
        if let Some(value) = original_target {
            env::set_var("TARGET", value);
        } else {
            env::remove_var("TARGET");
        }
    }

    let built = built.unwrap();

    assert_eq!(built.len(), 1);
    assert!(built[0].source_pin.starts_with("crate:"));
    assert_eq!(built[0].resolved_rev, None);
    assert!(built[0].grammar_path.exists());
    assert!(
        built[0]
            .query_paths
            .iter()
            .any(|path| path.ends_with(Path::new("rust").join("highlights.scm")))
    );
    assert!(
        built[0]
            .query_paths
            .iter()
            .any(|path| path.ends_with(Path::new("rust").join("indents.scm")))
    );
    assert!(output_root.join("queries").join("rust").join("indents.scm").exists());
}

#[test]
fn runtime_loader_builds_runtime_assets_without_host_load_validation() {
    let _guard = env_lock();
    let loader = default_runtime_loader();
    let temp_dir = TempDir::new().unwrap();
    let source_root = temp_dir.path().join("sources");
    let output_root = temp_dir.path().join("runtime");

    let built = loader
        .build_runtime_assets(
            &[String::from("rust")],
            false,
            &source_root,
            &output_root,
            true,
            true,
        )
        .unwrap();

    assert_eq!(built.len(), 1);
    assert!(built[0].grammar_path.exists());
}

fn run_git_fixture(repo: &Path, args: &[&str]) {
    let status = Command::new("git").arg("-C").arg(repo).args(args).status().unwrap();
    assert!(status.success(), "git {:?} failed in {}", args, repo.display());
}

fn git_output(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git").arg("-C").arg(repo).args(args).output().unwrap();
    assert!(output.status.success(), "git {:?} failed in {}", args, repo.display());
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn create_demo_git_repo(temp_dir: &TempDir) -> (PathBuf, String, String, String) {
    let repo = temp_dir.path().join("demo-repo");
    run_git_fixture(temp_dir.path(), &["init", "demo-repo"]);
    run_git_fixture(&repo, &["config", "user.name", "EE Tests"]);
    run_git_fixture(&repo, &["config", "user.email", "ee-tests@example.com"]);
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::create_dir_all(repo.join("queries")).unwrap();

    fs::write(
        repo.join("tree-sitter.json"),
        r#"{
  "grammars": [
    {
      "name": "Demo",
      "scope": "source.demo",
      "file-types": ["demo"],
      "path": ".",
      "highlights": "queries/highlights.scm",
      "locals": "queries/locals.scm",
      "tags": "queries/tags.scm"
    }
  ]
}"#,
    )
    .unwrap();
    fs::write(repo.join("src").join("parser.c"), "int tree_sitter_demo(void) { return 1; }\n")
        .unwrap();
    fs::write(repo.join("queries").join("highlights.scm"), "((identifier) @variable.first)")
        .unwrap();
    fs::write(repo.join("queries").join("locals.scm"), "((identifier) @local.reference)").unwrap();
    fs::write(repo.join("queries").join("tags.scm"), "((identifier) @definition.function)")
        .unwrap();
    run_git_fixture(&repo, &["add", "."]);
    run_git_fixture(&repo, &["commit", "-m", "initial"]);
    let first_rev = git_output(&repo, &["rev-parse", "HEAD"]);
    run_git_fixture(&repo, &["tag", "-a", "v1.0.0", "-m", "v1.0.0"]);

    fs::write(repo.join("queries").join("highlights.scm"), "((identifier) @variable.second)")
        .unwrap();
    run_git_fixture(&repo, &["add", "."]);
    run_git_fixture(&repo, &["commit", "-m", "branch update"]);
    let branch_name = git_output(&repo, &["branch", "--show-current"]);
    let second_rev = git_output(&repo, &["rev-parse", "HEAD"]);

    (repo, first_rev, branch_name, second_rev)
}

fn demo_git_loader(repo: &Path, ref_kind: &str, ref_value: &str, symbol: &str) -> RuntimeLoader {
    let languages = Languages::new(&[language_definition("Demo", &["demo"])]);
    let roots = RuntimeRoots::new(repo.join("bundle"), repo.join("user"), None);
    let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
    let mut overrides = RuntimeLanguageOverrides::new();
    let source = match ref_kind {
        "branch" => RuntimeGrammarGitSource {
            url: repo.display().to_string(),
            branch: Some(ref_value.to_string()),
            tag: None,
            rev: None,
        },
        "tag" => RuntimeGrammarGitSource {
            url: repo.display().to_string(),
            branch: None,
            tag: Some(ref_value.to_string()),
            rev: None,
        },
        "rev" => RuntimeGrammarGitSource {
            url: repo.display().to_string(),
            branch: None,
            tag: None,
            rev: Some(ref_value.to_string()),
        },
        other => panic!("unsupported ref kind {other}"),
    };
    overrides.insert(
        "Demo".to_string(),
        RuntimeLanguageConfig {
            grammar: Some(RuntimeGrammarConfig {
                library: Some(String::from("tree-sitter-demo")),
                symbol: Some(symbol.to_string()),
                source: Some(RuntimeGrammarSource::Git(source)),
            }),
            supported_query_kinds: Some(BTreeSet::from([
                RuntimeQueryKind::Highlights,
                RuntimeQueryKind::Locals,
                RuntimeQueryKind::Tags,
            ])),
            ..RuntimeLanguageConfig::default()
        },
    );
    loader.reload_merged_languages(&languages, &overrides, None).unwrap();
    loader
}

#[test]
fn runtime_loader_fetches_git_branch_source_and_reuses_checkout() {
    let _guard = env_lock();
    let temp_dir = TempDir::new().unwrap();
    let (repo, _tag_rev, branch_name, branch_rev) = create_demo_git_repo(&temp_dir);
    let loader = demo_git_loader(&repo, "branch", &branch_name, "tree_sitter_demo");
    let source_root = temp_dir.path().join("sources");

    let fetched =
        loader.fetch_grammar_sources(&[String::from("Demo")], false, &source_root, false).unwrap();
    assert_eq!(fetched[0].resolved_rev.as_deref(), Some(branch_rev.as_str()));
    assert!(fetched[0].source_pin.contains(&format!("branch:{branch_name}")));

    fs::write(fetched[0].source_dir.join("cache-marker"), "keep\n").unwrap();
    let fetched_again =
        loader.fetch_grammar_sources(&[String::from("Demo")], false, &source_root, false).unwrap();
    assert_eq!(fetched_again[0].resolved_rev.as_deref(), Some(branch_rev.as_str()));
    assert!(fetched_again[0].source_dir.join("cache-marker").exists());
}

#[test]
fn runtime_loader_fetches_git_tag_source_with_resolved_commit() {
    let _guard = env_lock();
    let temp_dir = TempDir::new().unwrap();
    let (repo, tag_rev, _branch_name, _branch_rev) = create_demo_git_repo(&temp_dir);
    let loader = demo_git_loader(&repo, "tag", "v1.0.0", "tree_sitter_demo");

    let fetched = loader
        .fetch_grammar_sources(
            &[String::from("Demo")],
            false,
            &temp_dir.path().join("sources"),
            false,
        )
        .unwrap();

    assert_eq!(fetched[0].resolved_rev.as_deref(), Some(tag_rev.as_str()));
    assert!(fetched[0].source_pin.contains("tag:v1.0.0"));
}

#[test]
fn runtime_loader_fetches_git_rev_source_with_exact_commit() {
    let _guard = env_lock();
    let temp_dir = TempDir::new().unwrap();
    let (repo, tag_rev, _branch_name, _branch_rev) = create_demo_git_repo(&temp_dir);
    let loader = demo_git_loader(&repo, "rev", &tag_rev, "tree_sitter_demo");

    let fetched = loader
        .fetch_grammar_sources(
            &[String::from("Demo")],
            false,
            &temp_dir.path().join("sources"),
            false,
        )
        .unwrap();

    assert_eq!(fetched[0].resolved_rev.as_deref(), Some(tag_rev.as_str()));
}

#[test]
fn runtime_loader_rejects_missing_git_ref() {
    let _guard = env_lock();
    let temp_dir = TempDir::new().unwrap();
    let (repo, _tag_rev, _branch_name, _branch_rev) = create_demo_git_repo(&temp_dir);
    let loader = demo_git_loader(&repo, "tag", "missing-tag", "tree_sitter_demo");

    let error = loader
        .fetch_grammar_sources(
            &[String::from("Demo")],
            false,
            &temp_dir.path().join("sources"),
            false,
        )
        .unwrap_err();

    assert!(error.to_string().contains("missing tag `missing-tag`"));
}

#[test]
fn runtime_loader_builds_runtime_assets_from_git_sources_and_manifest_queries() {
    let _guard = env_lock();
    let temp_dir = TempDir::new().unwrap();
    let (repo, _tag_rev, branch_name, branch_rev) = create_demo_git_repo(&temp_dir);
    let loader = demo_git_loader(&repo, "branch", &branch_name, "tree_sitter_demo");

    let built = loader
        .build_runtime_assets(
            &[String::from("Demo")],
            false,
            &temp_dir.path().join("sources"),
            &temp_dir.path().join("runtime"),
            true,
            true,
        )
        .unwrap();

    assert_eq!(built[0].resolved_rev.as_deref(), Some(branch_rev.as_str()));
    assert!(built[0].grammar_path.exists());
    assert!(built[0].query_paths.iter().any(|path| {
        path.ends_with(Path::new(&runtime_query_dir_name("Demo")).join("highlights.scm"))
    }));
    assert!(
        temp_dir
            .path()
            .join("runtime")
            .join("queries")
            .join(runtime_query_dir_name("Demo"))
            .join("tags.scm")
            .exists()
    );
}

#[test]
fn runtime_loader_build_fails_when_git_source_missing_parser() {
    let _guard = env_lock();
    let temp_dir = TempDir::new().unwrap();
    let (repo, _tag_rev, branch_name, _branch_rev) = create_demo_git_repo(&temp_dir);
    fs::remove_file(repo.join("src").join("parser.c")).unwrap();
    run_git_fixture(&repo, &["add", "-u"]);
    run_git_fixture(&repo, &["commit", "-m", "remove parser"]);
    let loader = demo_git_loader(&repo, "branch", &branch_name, "tree_sitter_demo");

    let error = loader
        .build_runtime_assets(
            &[String::from("Demo")],
            false,
            &temp_dir.path().join("sources"),
            &temp_dir.path().join("runtime"),
            true,
            true,
        )
        .unwrap_err();

    assert_eq!(error.kind(), RuntimeOperationErrorKind::GrammarSource);
    assert!(error.to_string().contains("missing parser source"));
}

#[test]
fn runtime_loader_build_fails_for_bad_git_tree_sitter_manifest() {
    let _guard = env_lock();
    let temp_dir = TempDir::new().unwrap();
    let (repo, _tag_rev, branch_name, _branch_rev) = create_demo_git_repo(&temp_dir);
    fs::write(repo.join("tree-sitter.json"), "{not json\n").unwrap();
    run_git_fixture(&repo, &["add", "tree-sitter.json"]);
    run_git_fixture(&repo, &["commit", "-m", "break manifest"]);
    let loader = demo_git_loader(&repo, "branch", &branch_name, "tree_sitter_demo");

    let error = loader
        .build_runtime_assets(
            &[String::from("Demo")],
            false,
            &temp_dir.path().join("sources"),
            &temp_dir.path().join("runtime"),
            true,
            true,
        )
        .unwrap_err();

    assert!(error.to_string().contains("failed parsing tree-sitter manifest"));
}

#[test]
fn runtime_loader_build_fails_for_grammar_symbol_mismatch() {
    let _guard = env_lock();
    let temp_dir = TempDir::new().unwrap();
    let (repo, _tag_rev, branch_name, _branch_rev) = create_demo_git_repo(&temp_dir);
    let loader = demo_git_loader(&repo, "branch", &branch_name, "tree_sitter_not_demo");
    let original_host = env::var_os("HOST");
    let original_target = env::var_os("TARGET");

    unsafe {
        env::remove_var("HOST");
        env::remove_var("TARGET");
    }

    let error = loader
        .build_runtime_assets(
            &[String::from("Demo")],
            false,
            &temp_dir.path().join("sources"),
            &temp_dir.path().join("runtime"),
            true,
            false,
        )
        .unwrap_err();

    unsafe {
        if let Some(value) = original_host {
            env::set_var("HOST", value);
        } else {
            env::remove_var("HOST");
        }
        if let Some(value) = original_target {
            env::set_var("TARGET", value);
        } else {
            env::remove_var("TARGET");
        }
    }

    assert!(matches!(
        error.kind(),
        RuntimeOperationErrorKind::GrammarSource | RuntimeOperationErrorKind::RuntimeAsset
    ));
    assert!(!error.to_string().trim().is_empty());
}
