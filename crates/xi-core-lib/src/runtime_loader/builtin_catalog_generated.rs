// GENERATED FILE - do not edit by hand.
// Regenerate with scripts/language-catalog/sync.sh.

use std::collections::BTreeSet;

use crate::syntax::LanguageDefinition;
use crate::tree_sitter_support::{
    BlockCommentStyle, IndentationStrategy, LanguageMetadata, LineCommentStyle,
};

use super::types::{
    RuntimeGrammarConfig, RuntimeGrammarGitSource, RuntimeGrammarSource, RuntimeLanguageConfig,
    RuntimeQueryKind,
};

/// Builtin language definitions for Zed-documented languages.
pub(crate) fn generated_catalog_language_definitions() -> Vec<LanguageDefinition> {
    vec![
        LanguageDefinition {
            name: "ansible".into(),
            extensions: vec!["ansible".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.ansible".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "asciidoc".into(),
            extensions: vec!["adoc".to_string(), "asciidoc".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.asciidoc".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "astro".into(),
            extensions: vec!["astro".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.astro".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "clojure".into(),
            extensions: vec![
                "clj".to_string(),
                "cljs".to_string(),
                "cljc".to_string(),
                "edn".to_string(),
            ],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.clojure".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "dart".into(),
            extensions: vec!["dart".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.dart".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "diff".into(),
            extensions: vec!["diff".to_string(), "patch".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.diff".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "docker".into(),
            extensions: vec![],
            filenames: vec!["Dockerfile".to_string(), "Containerfile".to_string()],
            globs: vec!["**/Dockerfile*".to_string(), "**/Containerfile*".to_string()],
            first_line_match: None,
            scope: "source.docker".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "elm".into(),
            extensions: vec!["elm".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.elm".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "erlang".into(),
            extensions: vec!["erl".to_string(), "hrl".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.erlang".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "fish".into(),
            extensions: vec!["fish".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.fish".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "gleam".into(),
            extensions: vec!["gleam".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.gleam".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "helm".into(),
            extensions: vec!["tpl".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.helm".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "jsonnet".into(),
            extensions: vec!["jsonnet".to_string(), "libsonnet".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.jsonnet".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "julia".into(),
            extensions: vec!["jl".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.julia".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "just".into(),
            extensions: vec!["just".to_string()],
            filenames: vec!["Justfile".to_string(), "justfile".to_string()],
            globs: vec![],
            first_line_match: None,
            scope: "source.just".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "kotlin".into(),
            extensions: vec!["kt".to_string(), "kts".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.kotlin".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "lua".into(),
            extensions: vec!["lua".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.lua".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "luau".into(),
            extensions: vec!["luau".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.luau".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "makefile".into(),
            extensions: vec!["mk".to_string(), "mak".to_string()],
            filenames: vec![
                "Makefile".to_string(),
                "makefile".to_string(),
                "GNUmakefile".to_string(),
                "gnumakefile".to_string(),
                "BSDmakefile".to_string(),
                "bsdmakefile".to_string(),
            ],
            globs: vec![],
            first_line_match: None,
            scope: "source.makefile".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "nim".into(),
            extensions: vec!["nim".to_string(), "nimble".to_string(), "nims".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.nim".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "ocaml".into(),
            extensions: vec!["ml".to_string(), "mli".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.ocaml".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "opentofu".into(),
            extensions: vec!["tofu".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.opentofu".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "prisma".into(),
            extensions: vec!["prisma".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.prisma".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "proto".into(),
            extensions: vec!["proto".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.proto".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "purescript".into(),
            extensions: vec!["purs".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.purescript".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "racket".into(),
            extensions: vec!["rkt".to_string(), "scrbl".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.racket".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "rego".into(),
            extensions: vec!["rego".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.rego".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "rst".into(),
            extensions: vec!["rst".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.rst".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "scheme".into(),
            extensions: vec!["scm".to_string(), "ss".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.scheme".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "sml".into(),
            extensions: vec!["sml".to_string(), "sig".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.sml".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "sql".into(),
            extensions: vec!["sql".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.sql".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "svelte".into(),
            extensions: vec!["svelte".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.svelte".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "terraform".into(),
            extensions: vec!["tf".to_string(), "tfvars".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.terraform".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "toml".into(),
            extensions: vec!["toml".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.toml".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "vue".into(),
            extensions: vec!["vue".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.vue".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "xml".into(),
            extensions: vec!["xml".to_string(), "xsd".to_string(), "svg".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.xml".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "yara".into(),
            extensions: vec!["yar".to_string(), "yara".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.yara".into(),
            default_config: None,
        },
        LanguageDefinition {
            name: "zig".into(),
            extensions: vec!["zig".to_string()],
            filenames: vec![],
            globs: vec![],
            first_line_match: None,
            scope: "source.zig".into(),
            default_config: None,
        },
    ]
}

/// Grammar source overrides for Zed-documented languages (git-pinned).
pub(crate) fn generated_catalog_language_overrides() -> Vec<(String, RuntimeLanguageConfig)> {
    let standard_and_ee = RuntimeQueryKind::STANDARD
        .into_iter()
        .chain(RuntimeQueryKind::EE_OWNED)
        .collect::<BTreeSet<_>>();

    vec![
        (
            "ansible".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["ansible".to_string()]),
                query_language: Some("yaml".to_string()),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-yaml".to_string()),
                    symbol: Some("tree_sitter_yaml".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/zed-industries/tree-sitter-yaml".to_string(),
                        rev: Some("baff0b51c64ef6a1fb1f8390f3ad6015b83ec13a".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Unsupported,
                    block_comment: BlockCommentStyle::Unsupported,
                    indentation: IndentationStrategy::Unsupported,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "asciidoc".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["asciidoc".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-asciidoc".to_string()),
                    symbol: Some("tree_sitter_asciidoc".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/cathaysia/tree-sitter-asciidoc".to_string(),
                        rev: Some("ade998931aeac0a10ca592b421cdb1f2f088f6c9".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Token("//"),
                    block_comment: BlockCommentStyle::Unsupported,
                    indentation: IndentationStrategy::Unsupported,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "astro".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["astro".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-astro".to_string()),
                    symbol: Some("tree_sitter_astro".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/virchau13/tree-sitter-astro".to_string(),
                        rev: Some("213f6e6973d9b456c6e50e86f19f66877e7ef0ee".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Token("//"),
                    block_comment: BlockCommentStyle::Tokens { open: "<!--", close: "-->" },
                    indentation: IndentationStrategy::TreeSitter,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "clojure".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["clojure".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-clojure".to_string()),
                    symbol: Some("tree_sitter_clojure".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/prcastro/tree-sitter-clojure".to_string(),
                        rev: Some("e43eff80d17cf34852dcd92ca5e6986d23a7040f".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Token(";"),
                    block_comment: BlockCommentStyle::Unsupported,
                    indentation: IndentationStrategy::TreeSitter,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "dart".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["dart".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-dart".to_string()),
                    symbol: Some("tree_sitter_dart".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/UserNobody14/tree-sitter-dart".to_string(),
                        rev: Some("be07cf7118d3dba06236a3f19541685a68209934".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Token("//"),
                    block_comment: BlockCommentStyle::Tokens { open: "/*", close: "*/" },
                    indentation: IndentationStrategy::TreeSitter,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "diff".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["diff".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-diff".to_string()),
                    symbol: Some("tree_sitter_diff".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/the-mikedavis/tree-sitter-diff".to_string(),
                        rev: Some("ada384ac7bfc1307f32de474620120add29998fb".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Unsupported,
                    block_comment: BlockCommentStyle::Unsupported,
                    indentation: IndentationStrategy::Unsupported,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "docker".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["docker".to_string()]),
                query_language: Some("dockerfile".to_string()),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-dockerfile".to_string()),
                    symbol: Some("tree_sitter_dockerfile".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/camdencheek/tree-sitter-dockerfile".to_string(),
                        rev: Some("971acdd908568b4531b0ba28a445bf0bb720aba5".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Unsupported,
                    block_comment: BlockCommentStyle::Unsupported,
                    indentation: IndentationStrategy::Unsupported,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "elm".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["elm".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-elm".to_string()),
                    symbol: Some("tree_sitter_elm".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/elm-tooling/tree-sitter-elm".to_string(),
                        rev: Some("e1e8fea161a1e66f3997855d316be2a43e4e956f".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Token("--"),
                    block_comment: BlockCommentStyle::Tokens { open: "{-", close: "-}" },
                    indentation: IndentationStrategy::TreeSitter,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "erlang".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["erlang".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-erlang".to_string()),
                    symbol: Some("tree_sitter_erlang".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/WhatsApp/tree-sitter-erlang".to_string(),
                        rev: Some("6ba4c762eb3065495e3db85697ffeecdf364ce35".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Token("%"),
                    block_comment: BlockCommentStyle::Unsupported,
                    indentation: IndentationStrategy::Unsupported,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "fish".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["fish".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-fish".to_string()),
                    symbol: Some("tree_sitter_fish".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/ram02z/tree-sitter-fish".to_string(),
                        rev: Some("b7f1d682941e0c62dfcd6bf9ef481638351bab16".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Token("#"),
                    block_comment: BlockCommentStyle::Unsupported,
                    indentation: IndentationStrategy::Unsupported,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "gleam".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["gleam".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-gleam".to_string()),
                    symbol: Some("tree_sitter_gleam".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/gleam-lang/tree-sitter-gleam".to_string(),
                        rev: Some("fa6d0d94804f1b342e175865a3dbeb316ee5115e".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Token("//"),
                    block_comment: BlockCommentStyle::Unsupported,
                    indentation: IndentationStrategy::TreeSitter,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "helm".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["helm".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-go-template".to_string()),
                    symbol: Some("tree_sitter_helm".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/ngalaiko/tree-sitter-go-template".to_string(),
                        rev: Some("aa71f63de226c5592dfbfc1f29949522d7c95fac".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Unsupported,
                    block_comment: BlockCommentStyle::Unsupported,
                    indentation: IndentationStrategy::Unsupported,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "jsonnet".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["jsonnet".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-jsonnet".to_string()),
                    symbol: Some("tree_sitter_jsonnet".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/sourcegraph/tree-sitter-jsonnet".to_string(),
                        rev: Some("ddd075f1939aed8147b7aa67f042eda3fce22790".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Token("//"),
                    block_comment: BlockCommentStyle::Tokens { open: "/*", close: "*/" },
                    indentation: IndentationStrategy::TreeSitter,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "julia".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["julia".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-julia".to_string()),
                    symbol: Some("tree_sitter_julia".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/tree-sitter/tree-sitter-julia".to_string(),
                        rev: Some("e0f9dcd180fdcfcfa8d79a3531e11d99e79321d3".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Token("#"),
                    block_comment: BlockCommentStyle::Unsupported,
                    indentation: IndentationStrategy::Unsupported,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "just".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["just".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-just".to_string()),
                    symbol: Some("tree_sitter_just".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/IndianBoy42/tree-sitter-just".to_string(),
                        rev: Some("5685543a6e64f66335e25518c9ae8ffa1dae3d01".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Token("#"),
                    block_comment: BlockCommentStyle::Unsupported,
                    indentation: IndentationStrategy::TreeSitter,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "kotlin".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["kotlin".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-kotlin".to_string()),
                    symbol: Some("tree_sitter_kotlin".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/fwcd/tree-sitter-kotlin".to_string(),
                        rev: Some("1852ea17b7f60fb3f9d84e0b1555d56b46b39fb1".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Token("//"),
                    block_comment: BlockCommentStyle::Unsupported,
                    indentation: IndentationStrategy::TreeSitter,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "lua".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["lua".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-lua".to_string()),
                    symbol: Some("tree_sitter_lua".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/tree-sitter-grammars/tree-sitter-lua".to_string(),
                        rev: Some("10fe0054734eec83049514ea2e718b2a56acd0c9".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Token("--"),
                    block_comment: BlockCommentStyle::Unsupported,
                    indentation: IndentationStrategy::TreeSitter,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "luau".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["luau".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-luau".to_string()),
                    symbol: Some("tree_sitter_luau".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/4teapo/tree-sitter-luau".to_string(),
                        rev: Some("5c708649dd8d26735f8d6cca022195325ea27086".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Token("--"),
                    block_comment: BlockCommentStyle::Unsupported,
                    indentation: IndentationStrategy::TreeSitter,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "makefile".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["makefile".to_string()]),
                query_language: Some("make".to_string()),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-make".to_string()),
                    symbol: Some("tree_sitter_make".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/caius/tree-sitter-make".to_string(),
                        rev: Some("3a295ac70ae1f738c5f96a316ac523a5a213d658".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Token("#"),
                    block_comment: BlockCommentStyle::Unsupported,
                    indentation: IndentationStrategy::Unsupported,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "nim".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["nim".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-nim".to_string()),
                    symbol: Some("tree_sitter_nim".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/alaviss/tree-sitter-nim".to_string(),
                        rev: Some("ac72ba30d16edf0be021588a9301ede4accd6cf4".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Token("#"),
                    block_comment: BlockCommentStyle::Unsupported,
                    indentation: IndentationStrategy::TreeSitter,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "ocaml".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["ocaml".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-ocaml".to_string()),
                    symbol: Some("tree_sitter_ocaml".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/tree-sitter/tree-sitter-ocaml".to_string(),
                        rev: Some("3b2e14e0697d405c9aa0beddfa09b71f45abc504".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Token("//"),
                    block_comment: BlockCommentStyle::Unsupported,
                    indentation: IndentationStrategy::TreeSitter,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "opentofu".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["opentofu".to_string()]),
                query_language: Some("hcl".to_string()),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-hcl".to_string()),
                    symbol: Some("tree_sitter_hcl".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/tree-sitter-grammars/tree-sitter-hcl".to_string(),
                        rev: Some("64ad62785d442eb4d45df3a1764962dafd5bc98b".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Token("//"),
                    block_comment: BlockCommentStyle::Tokens { open: "/*", close: "*/" },
                    indentation: IndentationStrategy::TreeSitter,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "prisma".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["prisma".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-prisma".to_string()),
                    symbol: Some("tree_sitter_prisma".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/victorhqc/tree-sitter-prisma".to_string(),
                        rev: Some("3556b2c1f20ec9ac91e92d32c43d9d2a0ca3cc49".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Token("//"),
                    block_comment: BlockCommentStyle::Unsupported,
                    indentation: IndentationStrategy::Unsupported,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "proto".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["proto".to_string()]),
                query_language: Some("protobuf".to_string()),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-proto".to_string()),
                    symbol: Some("tree_sitter_proto".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/coder3101/tree-sitter-proto".to_string(),
                        rev: Some("be5691cf82ca284f83e68b9c8cdc4cac5d7fa208".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Unsupported,
                    block_comment: BlockCommentStyle::Unsupported,
                    indentation: IndentationStrategy::Unsupported,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "purescript".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["purescript".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-purescript".to_string()),
                    symbol: Some("tree_sitter_purescript".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/postsolar/tree-sitter-purescript".to_string(),
                        rev: Some("f541f95ffd6852fbbe88636317c613285bc105af".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Token("--"),
                    block_comment: BlockCommentStyle::Tokens { open: "{-", close: "-}" },
                    indentation: IndentationStrategy::TreeSitter,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "racket".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["racket".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-racket".to_string()),
                    symbol: Some("tree_sitter_racket".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/zed-industries/tree-sitter-racket".to_string(),
                        rev: Some("d9858a0f607578814f2d34662ad4bc21aa37a455".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Token(";"),
                    block_comment: BlockCommentStyle::Tokens { open: "#|", close: "|#" },
                    indentation: IndentationStrategy::Unsupported,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "rego".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["rego".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-rego".to_string()),
                    symbol: Some("tree_sitter_rego".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/FallenAngel97/tree-sitter-rego".to_string(),
                        rev: Some("da2a1f63cd877efb05d56de61fe516e90012b9a7".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Token("#"),
                    block_comment: BlockCommentStyle::Unsupported,
                    indentation: IndentationStrategy::Unsupported,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "rst".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["rst".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-rst".to_string()),
                    symbol: Some("tree_sitter_rst".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/stsewd/tree-sitter-rst".to_string(),
                        rev: Some("a60f1070b824cb8bb8409b4b6d7da0d07997c30e".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Unsupported,
                    block_comment: BlockCommentStyle::Unsupported,
                    indentation: IndentationStrategy::Unsupported,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "scheme".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["scheme".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-scheme".to_string()),
                    symbol: Some("tree_sitter_scheme".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/6cdh/tree-sitter-scheme".to_string(),
                        rev: Some("1b112d9571e4f62fb3d095d52a51f1da7756fb94".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Token(";"),
                    block_comment: BlockCommentStyle::Unsupported,
                    indentation: IndentationStrategy::Unsupported,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "sml".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["sml".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-sml".to_string()),
                    symbol: Some("tree_sitter_sml".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/MatthewFluet/tree-sitter-sml".to_string(),
                        rev: Some("fd4b4955bb998262840ab8119885b3edf20ea75a".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Unsupported,
                    block_comment: BlockCommentStyle::Tokens { open: "(*", close: "*)" },
                    indentation: IndentationStrategy::Unsupported,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "sql".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["sql".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-sql".to_string()),
                    symbol: Some("tree_sitter_sql".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/nervenes/tree-sitter-sql".to_string(),
                        rev: Some("6dfca8b6dcb196d943c10e9cabab25e60232d332".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Unsupported,
                    block_comment: BlockCommentStyle::Unsupported,
                    indentation: IndentationStrategy::Unsupported,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "svelte".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["svelte".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-svelte".to_string()),
                    symbol: Some("tree_sitter_svelte".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/tree-sitter-grammars/tree-sitter-svelte"
                            .to_string(),
                        rev: Some("ae5199db47757f785e43a14b332118a5474de1a2".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Unsupported,
                    block_comment: BlockCommentStyle::Tokens { open: "<!--", close: "-->" },
                    indentation: IndentationStrategy::TreeSitter,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "terraform".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["terraform".to_string()]),
                query_language: Some("hcl".to_string()),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-hcl".to_string()),
                    symbol: Some("tree_sitter_hcl".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/tree-sitter-grammars/tree-sitter-hcl".to_string(),
                        rev: Some("64ad62785d442eb4d45df3a1764962dafd5bc98b".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Token("//"),
                    block_comment: BlockCommentStyle::Tokens { open: "/*", close: "*/" },
                    indentation: IndentationStrategy::TreeSitter,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "toml".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["toml".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-toml".to_string()),
                    symbol: Some("tree_sitter_toml".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/tree-sitter/tree-sitter-toml".to_string(),
                        rev: Some("342d9be207c2dba869b9967124c679b5e6fd0ebe".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Token("#"),
                    block_comment: BlockCommentStyle::Unsupported,
                    indentation: IndentationStrategy::Unsupported,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "vue".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["vue".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-vue".to_string()),
                    symbol: Some("tree_sitter_vue".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/tree-sitter-grammars/tree-sitter-vue".to_string(),
                        rev: Some("ce8011a414fdf8091f4e4071752efc376f4afb08".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Unsupported,
                    block_comment: BlockCommentStyle::Tokens { open: "<!--", close: "-->" },
                    indentation: IndentationStrategy::TreeSitter,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "xml".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["xml".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-xml".to_string()),
                    symbol: Some("tree_sitter_xml".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/tree-sitter-grammars/tree-sitter-xml".to_string(),
                        rev: Some("5000ae8f22d11fbe93939b05c1e37cf21117162d".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Unsupported,
                    block_comment: BlockCommentStyle::Tokens { open: "<!--", close: "-->" },
                    indentation: IndentationStrategy::Unsupported,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "yara".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["yara".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-yara".to_string()),
                    symbol: Some("tree_sitter_yara".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/egibs/tree-sitter-yara".to_string(),
                        rev: Some("eb3ede203275c38000177f72ec0f9965312806ef".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Token("//"),
                    block_comment: BlockCommentStyle::Unsupported,
                    indentation: IndentationStrategy::Unsupported,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
        (
            "zig".to_string(),
            RuntimeLanguageConfig {
                aliases: Some(vec!["zig".to_string()]),
                supported_query_kinds: Some(standard_and_ee.clone()),
                grammar: Some(RuntimeGrammarConfig {
                    library: Some("tree-sitter-zig".to_string()),
                    symbol: Some("tree_sitter_zig".to_string()),
                    source: Some(RuntimeGrammarSource::Git(RuntimeGrammarGitSource {
                        url: "https://github.com/tree-sitter-grammars/tree-sitter-zig".to_string(),
                        rev: Some("6479aa13f32f701c383083d8b28360ebd682fb7d".to_string()),
                        branch: None,
                        tag: None,
                    })),
                }),
                metadata: Some(LanguageMetadata {
                    line_comment: LineCommentStyle::Token("//"),
                    block_comment: BlockCommentStyle::Unsupported,
                    indentation: IndentationStrategy::TreeSitter,
                    unsupported_semantic_targets: &[],
                }),
                ..RuntimeLanguageConfig::default()
            },
        ),
    ]
}
