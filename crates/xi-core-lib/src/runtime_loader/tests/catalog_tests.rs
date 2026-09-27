//! Runtime-loader tests: generated Zed language catalog invariants.
//!
//! These tests are network-free; they validate that the generated catalog
//! (scripts/zed-catalog) stays structurally sound:
//! - every non-deferred entry has a Git-pinned grammar source,
//! - definitions and overrides agree per language id,
//! - the default loader resolves new languages and retains detection data.
use super::*;

use crate::runtime_loader::builtin_zed_generated::{
    generated_zed_language_definitions, generated_zed_language_overrides,
};
use crate::runtime_loader::{
    TreeSitterPackageGrammar, TreeSitterPackageManifest, select_manifest_grammar,
};

#[test]
fn generated_catalog_definitions_and_overrides_are_aligned() {
    let definitions = generated_zed_language_definitions();
    let overrides = generated_zed_language_overrides();

    assert!(!definitions.is_empty(), "generated catalog must not be empty");
    assert_eq!(
        definitions.len(),
        overrides.len(),
        "every generated definition must have an override entry"
    );

    let defined_ids = definitions
        .iter()
        .map(|def| normalize_lookup_key(def.name.as_ref()))
        .collect::<BTreeSet<_>>();
    let override_ids =
        overrides.iter().map(|(id, _)| normalize_lookup_key(id)).collect::<BTreeSet<_>>();
    assert_eq!(defined_ids, override_ids, "definition ids and override ids must match");

    for (id, config) in &overrides {
        let grammar = config.grammar.as_ref().unwrap_or_else(|| {
            panic!("language `{id}` must carry a grammar config");
        });
        let source = grammar.source.as_ref().unwrap_or_else(|| {
            panic!("language `{id}` must carry a grammar source");
        });
        let git = match source {
            RuntimeGrammarSource::Git(git) => git,
            RuntimeGrammarSource::Crate(_) => {
                panic!("language `{id}` must use a git source, not a crate source")
            }
        };
        assert!(!git.url.is_empty(), "language `{id}` needs a grammar repository url");
        let rev = git.rev.as_ref().unwrap_or_else(|| {
            panic!("language `{id}` must pin an exact git rev");
        });
        assert_eq!(rev.len(), 40, "language `{id}` rev must be a full sha: {rev}");
        assert!(git.branch.is_none() && git.tag.is_none(), "language `{id}` must pin rev-only");
        assert!(config.metadata.is_some(), "language `{id}` must carry comment metadata");
        let symbol = grammar.symbol.as_ref().unwrap_or_else(|| {
            panic!("language `{id}` must carry a grammar symbol");
        });
        assert!(
            symbol.starts_with("tree_sitter_"),
            "language `{id}` symbol `{symbol}` looks wrong"
        );
    }
}

#[test]
fn generated_catalog_ids_are_distinct_and_have_detection_surface() {
    let definitions = generated_zed_language_definitions();
    let mut ids = BTreeSet::new();
    for def in &definitions {
        let key = normalize_lookup_key(def.name.as_ref());
        assert!(ids.insert(key.clone()), "duplicate generated language id: {key}");
        assert!(
            !def.extensions.is_empty() || !def.filenames.is_empty() || !def.globs.is_empty(),
            "language `{key}` must register detection surface (file_types, filenames, or globs)"
        );
    }
}

#[test]
fn default_loader_resolves_generated_languages() {
    let loader = default_runtime_loader();

    for id in [
        "toml",
        "zig",
        "clojure",
        "terraform",
        "opentofu",
        "vue",
        "astro",
        "diff",
        "docker",
        "just",
    ] {
        let language = loader.language_for_name(id).unwrap_or_else(|| {
            panic!("default loader must resolve generated language `{id}`");
        });
        assert!(
            matches!(language.grammar_source(), Some(RuntimeGrammarSource::Git(_))),
            "language `{id}` must resolve to a git grammar source"
        );
    }

    // Detection data must survive the merge.
    assert_eq!(
        loader.language_for_name("terraform").unwrap().query_language(),
        "hcl",
        "terraform must use the hcl query language"
    );
    assert_eq!(
        loader.language_for_name("opentofu").unwrap().query_language(),
        "hcl",
        "opentofu must use the hcl query language"
    );
    assert!(
        loader.language_for_path(Path::new("main.tf")).is_some(),
        "terraform must detect main.tf"
    );
    assert!(
        loader.language_for_path(Path::new("App.svelte")).is_some(),
        "svelte must detect .svelte files"
    );
    assert!(
        loader.language_for_path(Path::new("change.diff")).is_some(),
        "diff must detect .diff files"
    );
    assert!(
        loader.language_for_path(Path::new("change.patch")).is_some(),
        "diff must detect .patch files"
    );
    assert!(
        loader.language_for_path(Path::new("Makefile")).is_some(),
        "makefile must detect Makefile by filename"
    );
    assert!(
        loader.language_for_path(Path::new("GNUmakefile")).is_some(),
        "makefile must detect GNUmakefile by filename"
    );
    assert!(
        loader.language_for_path(Path::new("rules.mk")).is_some(),
        "makefile must detect .mk files"
    );
    assert!(
        loader.language_for_path(Path::new("Dockerfile")).is_some(),
        "docker must detect Dockerfile by filename"
    );
    assert!(
        loader.language_for_path(Path::new("ci/images/Dockerfile.dev")).is_some(),
        "docker must detect nested Dockerfile.dev via glob"
    );
    assert!(
        loader.language_for_path(Path::new("build/Containerfile")).is_some(),
        "docker must detect nested Containerfile via glob"
    );
    assert!(
        loader.language_for_path(Path::new("Justfile")).is_some(),
        "just must detect Justfile by filename"
    );
}

#[test]
fn manifest_grammar_selection_prefers_exact_then_dialect_suffix() {
    let helm_manifest = TreeSitterPackageManifest {
        grammars: vec![
            TreeSitterPackageGrammar {
                name: String::from("go-template"),
                path: Some(String::from(".")),
                highlights: None,
                injections: None,
                locals: None,
                tags: None,
            },
            TreeSitterPackageGrammar {
                name: String::from("go-template-helm"),
                path: Some(String::from("dialects/helm")),
                highlights: None,
                injections: None,
                locals: None,
                tags: None,
            },
        ],
    };
    let helm = crate::runtime_loader::RuntimeLanguage::from_definition(
        &super::language_definition("helm", &["tpl"]),
    );
    let selected = select_manifest_grammar(&helm_manifest, &helm).unwrap();
    assert_eq!(selected.name, "go-template-helm");
    assert_eq!(selected.path.as_deref(), Some("dialects/helm"));

    let md_manifest = TreeSitterPackageManifest {
        grammars: vec![
            TreeSitterPackageGrammar {
                name: String::from("markdown"),
                path: Some(String::from(".")),
                highlights: None,
                injections: None,
                locals: None,
                tags: None,
            },
            TreeSitterPackageGrammar {
                name: String::from("markdown-inline"),
                path: Some(String::from("inline")),
                highlights: None,
                injections: None,
                locals: None,
                tags: None,
            },
        ],
    };
    let markdown = crate::runtime_loader::RuntimeLanguage::from_definition(
        &super::language_definition("markdown", &["md"]),
    );
    let selected = select_manifest_grammar(&md_manifest, &markdown).unwrap();
    assert_eq!(selected.name, "markdown");

    // No `-helm` variant: fall back to the first grammar instead of failing.
    let plain_manifest = TreeSitterPackageManifest {
        grammars: vec![TreeSitterPackageGrammar {
            name: String::from("go-template"),
            path: Some(String::from(".")),
            highlights: None,
            injections: None,
            locals: None,
            tags: None,
        }],
    };
    let selected = select_manifest_grammar(&plain_manifest, &helm).unwrap();
    assert_eq!(selected.name, "go-template");
}

#[test]
fn staged_build_dir_prefers_manifest_dialect_over_root_parser() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("tree-sitter-go-template");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("dialects").join("helm").join("src")).unwrap();
    // Root has its own parser: previously this short-circuited manifest lookup.
    fs::write(root.join("src").join("parser.c"), "int tree_sitter_gotmpl(void) { return 1; }\n")
        .unwrap();
    fs::write(
        root.join("dialects").join("helm").join("src").join("parser.c"),
        "int tree_sitter_helm(void) { return 1; }\n",
    )
    .unwrap();
    fs::write(
        root.join("tree-sitter.json"),
        r#"{
  "grammars": [
    { "name": "go-template", "path": "." },
    { "name": "go-template-helm", "path": "dialects/helm" }
  ]
}"#,
    )
    .unwrap();

    let loader = default_runtime_loader();
    let helm = loader.language_for_name("helm").unwrap();
    let resolved = resolve_staged_grammar_build_dir(&root, helm).unwrap();
    assert_eq!(resolved, root.join("dialects").join("helm"));

    // Manifest-less repos keep the root parser short-circuit.
    let plain = temp.path().join("plain-grammar");
    fs::create_dir_all(plain.join("src")).unwrap();
    fs::write(plain.join("src").join("parser.c"), "int tree_sitter_plain(void) { return 1; }\n")
        .unwrap();
    let toml = loader.language_for_name("toml").unwrap();
    let resolved = resolve_staged_grammar_build_dir(&plain, toml).unwrap();
    assert_eq!(resolved, plain);
}
