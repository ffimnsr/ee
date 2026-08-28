use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{LazyLock, RwLock};

#[cfg(test)]
use std::sync::{Mutex, MutexGuard};

#[cfg(any(test, feature = "test-grammars"))]
use super::types::GrammarHandle;

use crate::syntax::{LanguageDefinition, Languages};
use crate::tree_sitter_support::{
    BlockCommentStyle, IndentationStrategy, LanguageMetadata, LineCommentStyle, SemanticTargetKind,
};

#[cfg(any(test, feature = "test-grammars"))]
use ee_ts_test_grammars as test_grammars;

use super::errors::RuntimeLoaderError;
use super::helpers::{bundled_runtime_root_from_env, normalize_lookup_key};
use super::loader::RuntimeLoader;
use super::types::{
    DefaultRuntimeLoaderOverrides, RuntimeGrammarConfig, RuntimeGrammarCrateSource,
    RuntimeGrammarSource, RuntimeLanguageConfig, RuntimeLanguageOverrides, RuntimeQueryKind,
    RuntimeRoots, WorkspaceRuntimeOverrides,
};

// ---------------------------------------------------------------------------
// Builtin language definitions
// ---------------------------------------------------------------------------
fn builtin_language_definition(name: &str, file_types: &[&str]) -> LanguageDefinition {
    LanguageDefinition {
        name: name.into(),
        extensions: file_types.iter().map(|value| (*value).to_string()).collect(),
        first_line_match: None,
        scope: format!("source.{}", normalize_lookup_key(name)),
        default_config: None,
    }
}

fn builtin_runtime_components() -> (Languages, RuntimeLanguageOverrides) {
    let mut overrides = RuntimeLanguageOverrides::new();
    let definitions = vec![
        builtin_language_definition("bash", &["sh", "bash", ".bashrc", ".zshrc"]),
        builtin_language_definition("c", &["c", "h"]),
        builtin_language_definition("csharp", &["cs"]),
        builtin_language_definition("cpp", &["cc", "cpp", "cxx", "hh", "hpp", "hxx"]),
        builtin_language_definition("css", &["css", "less", "scss"]),
        builtin_language_definition("elixir", &["ex", "exs"]),
        builtin_language_definition("go", &["go"]),
        builtin_language_definition("haskell", &["hs"]),
        builtin_language_definition("html", &["htm", "html", "xhtml"]),
        builtin_language_definition("java", &["java"]),
        builtin_language_definition("javascript", &["cjs", "js", "jsx", "mjs"]),
        builtin_language_definition("json", &["json"]),
        builtin_language_definition("php", &["php", "phtml"]),
        builtin_language_definition("python", &["py", "pyw"]),
        builtin_language_definition("ruby", &["rb", "gemspec", "gemfile", "rake", "rakefile"]),
        builtin_language_definition("rust", &["rs"]),
        builtin_language_definition("scala", &["sc", "scala"]),
        builtin_language_definition("typescript", &["cts", "mts", "ts", "tsx"]),
        builtin_language_definition("yaml", &["yaml", "yml"]),
    ];
    let standard_and_ee = RuntimeQueryKind::STANDARD
        .into_iter()
        .chain(RuntimeQueryKind::EE_OWNED)
        .collect::<BTreeSet<_>>();

    macro_rules! builtin_language {
        ($name:literal, $grammar:literal, $version:literal, $symbol:literal, $aliases:expr, $metadata:expr) => {{
            overrides.insert(
                normalize_lookup_key($name),
                RuntimeLanguageConfig {
                    aliases: Some($aliases.iter().map(|value| (*value).to_string()).collect()),
                    supported_query_kinds: Some(standard_and_ee.clone()),
                    grammar: Some(RuntimeGrammarConfig {
                        library: Some($grammar.to_string()),
                        symbol: Some($symbol.to_string()),
                        source: Some(RuntimeGrammarSource::Crate(RuntimeGrammarCrateSource {
                            name: $grammar.to_string(),
                            version: $version.to_string(),
                        })),
                    }),
                    metadata: Some($metadata),
                    ..RuntimeLanguageConfig::default()
                },
            );
        }};
    }

    macro_rules! metadata {
        ($line_comment:expr, $block_comment:expr, $indentation:expr, $unsupported:expr) => {
            LanguageMetadata {
                line_comment: $line_comment,
                block_comment: $block_comment,
                indentation: $indentation,
                unsupported_semantic_targets: $unsupported,
            }
        };
    }

    builtin_language!(
        "bash",
        "tree-sitter-bash",
        "0.25.1",
        "tree_sitter_bash",
        ["bash", "shell", "shellscript", "sh"],
        metadata!(
            LineCommentStyle::Token("#"),
            BlockCommentStyle::Unsupported,
            IndentationStrategy::Unsupported,
            &[]
        )
    );
    builtin_language!(
        "c",
        "tree-sitter-c",
        "0.24.2",
        "tree_sitter_c",
        ["c"],
        metadata!(
            LineCommentStyle::Token("//"),
            BlockCommentStyle::Tokens { open: "/*", close: "*/" },
            IndentationStrategy::TreeSitter,
            &[]
        )
    );
    builtin_language!(
        "csharp",
        "tree-sitter-c-sharp",
        "0.23.5",
        "tree_sitter_c_sharp",
        ["c#", "csharp", "cs"],
        metadata!(
            LineCommentStyle::Token("//"),
            BlockCommentStyle::Tokens { open: "/*", close: "*/" },
            IndentationStrategy::TreeSitter,
            &[]
        )
    );
    builtin_language!(
        "cpp",
        "tree-sitter-cpp",
        "0.23.4",
        "tree_sitter_cpp",
        ["c++", "cpp", "cplusplus"],
        metadata!(
            LineCommentStyle::Token("//"),
            BlockCommentStyle::Tokens { open: "/*", close: "*/" },
            IndentationStrategy::TreeSitter,
            &[]
        )
    );
    builtin_language!(
        "css",
        "tree-sitter-css",
        "0.25.0",
        "tree_sitter_css",
        ["css"],
        metadata!(
            LineCommentStyle::Unsupported,
            BlockCommentStyle::Tokens { open: "/*", close: "*/" },
            IndentationStrategy::Unsupported,
            &[
                SemanticTargetKind::Function,
                SemanticTargetKind::Class,
                SemanticTargetKind::Parameter,
                SemanticTargetKind::Test,
            ]
        )
    );
    builtin_language!(
        "elixir",
        "tree-sitter-elixir",
        "0.3.5",
        "tree_sitter_elixir",
        ["elixir", "ex", "exs"],
        metadata!(
            LineCommentStyle::Token("#"),
            BlockCommentStyle::Unsupported,
            IndentationStrategy::Unsupported,
            &[]
        )
    );
    builtin_language!(
        "go",
        "tree-sitter-go",
        "0.25.0",
        "tree_sitter_go",
        ["go", "golang"],
        metadata!(
            LineCommentStyle::Token("//"),
            BlockCommentStyle::Tokens { open: "/*", close: "*/" },
            IndentationStrategy::TreeSitter,
            &[]
        )
    );
    builtin_language!(
        "haskell",
        "tree-sitter-haskell",
        "0.23.1",
        "tree_sitter_haskell",
        ["haskell", "hs"],
        metadata!(
            LineCommentStyle::Token("--"),
            BlockCommentStyle::Tokens { open: "{-", close: "-}" },
            IndentationStrategy::Unsupported,
            &[]
        )
    );
    builtin_language!(
        "html",
        "tree-sitter-html",
        "0.23.2",
        "tree_sitter_html",
        ["html"],
        metadata!(
            LineCommentStyle::Unsupported,
            BlockCommentStyle::Tokens { open: "<!--", close: "-->" },
            IndentationStrategy::Unsupported,
            &[
                SemanticTargetKind::Function,
                SemanticTargetKind::Class,
                SemanticTargetKind::Parameter,
                SemanticTargetKind::Test,
            ]
        )
    );
    builtin_language!(
        "java",
        "tree-sitter-java",
        "0.23.5",
        "tree_sitter_java",
        ["java"],
        metadata!(
            LineCommentStyle::Token("//"),
            BlockCommentStyle::Tokens { open: "/*", close: "*/" },
            IndentationStrategy::TreeSitter,
            &[]
        )
    );
    builtin_language!(
        "javascript",
        "tree-sitter-javascript",
        "0.25.0",
        "tree_sitter_javascript",
        ["javascript", "javascriptreact", "js", "jsx"],
        metadata!(
            LineCommentStyle::Token("//"),
            BlockCommentStyle::Tokens { open: "/*", close: "*/" },
            IndentationStrategy::TreeSitter,
            &[]
        )
    );
    builtin_language!(
        "json",
        "tree-sitter-json",
        "0.24.8",
        "tree_sitter_json",
        ["json"],
        metadata!(
            LineCommentStyle::Unsupported,
            BlockCommentStyle::Unsupported,
            IndentationStrategy::Unsupported,
            &[
                SemanticTargetKind::Function,
                SemanticTargetKind::Class,
                SemanticTargetKind::Parameter,
                SemanticTargetKind::Test,
            ]
        )
    );
    builtin_language!(
        "php",
        "tree-sitter-php",
        "0.24.2",
        "tree_sitter_php",
        ["php"],
        metadata!(
            LineCommentStyle::Token("//"),
            BlockCommentStyle::Tokens { open: "/*", close: "*/" },
            IndentationStrategy::TreeSitter,
            &[]
        )
    );
    builtin_language!(
        "python",
        "tree-sitter-python",
        "0.25.0",
        "tree_sitter_python",
        ["py", "python", "python3"],
        metadata!(
            LineCommentStyle::Token("#"),
            BlockCommentStyle::Unsupported,
            IndentationStrategy::TreeSitter,
            &[]
        )
    );
    builtin_language!(
        "ruby",
        "tree-sitter-ruby",
        "0.23.1",
        "tree_sitter_ruby",
        ["rb", "ruby"],
        metadata!(
            LineCommentStyle::Token("#"),
            BlockCommentStyle::Tokens { open: "=begin", close: "=end" },
            IndentationStrategy::Unsupported,
            &[]
        )
    );
    builtin_language!(
        "rust",
        "tree-sitter-rust",
        "0.24.2",
        "tree_sitter_rust",
        ["rs", "rust"],
        metadata!(
            LineCommentStyle::Token("//"),
            BlockCommentStyle::Tokens { open: "/*", close: "*/" },
            IndentationStrategy::TreeSitter,
            &[]
        )
    );
    builtin_language!(
        "scala",
        "tree-sitter-scala",
        "0.26.0",
        "tree_sitter_scala",
        ["scala"],
        metadata!(
            LineCommentStyle::Token("//"),
            BlockCommentStyle::Tokens { open: "/*", close: "*/" },
            IndentationStrategy::Unsupported,
            &[]
        )
    );

    overrides.insert(
        String::from("typescript"),
        RuntimeLanguageConfig {
            aliases: Some(vec![
                "ts".to_string(),
                "typescript".to_string(),
                "tsx".to_string(),
                "typescriptreact".to_string(),
            ]),
            supported_query_kinds: Some(standard_and_ee.clone()),
            grammar: Some(RuntimeGrammarConfig {
                library: Some("tree-sitter-typescript".to_string()),
                symbol: Some("tree_sitter_typescript".to_string()),
                source: Some(RuntimeGrammarSource::Crate(RuntimeGrammarCrateSource {
                    name: String::from("tree-sitter-typescript"),
                    version: String::from("0.23.2"),
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
    );
    builtin_language!(
        "yaml",
        "tree-sitter-yaml",
        "0.7.2",
        "tree_sitter_yaml",
        ["yaml", "yml"],
        metadata!(
            LineCommentStyle::Token("#"),
            BlockCommentStyle::Unsupported,
            IndentationStrategy::Unsupported,
            &[
                SemanticTargetKind::Function,
                SemanticTargetKind::Class,
                SemanticTargetKind::Parameter,
                SemanticTargetKind::Test,
            ]
        )
    );

    (Languages::new(&definitions), overrides)
}

// ---------------------------------------------------------------------------
// Default loader and statics
// ---------------------------------------------------------------------------
pub(crate) fn default_runtime_loader() -> RuntimeLoader {
    let roots = RuntimeRoots::from_data_dir(bundled_runtime_root_from_env(), None, None)
        .unwrap_or_else(|| {
            RuntimeRoots::new(bundled_runtime_root_from_env(), PathBuf::from(".ee"), None)
        });
    let mut loader = RuntimeLoader::new(roots.clone(), roots.parser_directories(true))
        .expect("default runtime loader should initialize");
    let (languages, builtin_overrides) = builtin_runtime_components();
    loader
        .reload_merged_languages(&languages, &builtin_overrides, None)
        .expect("builtin runtime languages should load");
    loader
}

static DEFAULT_RUNTIME_LOADER: LazyLock<RwLock<RuntimeLoader>> =
    LazyLock::new(|| RwLock::new(default_runtime_loader()));

static DEFAULT_RUNTIME_LOADER_OVERRIDES: LazyLock<RwLock<DefaultRuntimeLoaderOverrides>> =
    LazyLock::new(|| RwLock::new(DefaultRuntimeLoaderOverrides::default()));

// ---------------------------------------------------------------------------
// Default loader public API
// ---------------------------------------------------------------------------
fn default_runtime_loader_overrides() -> DefaultRuntimeLoaderOverrides {
    DEFAULT_RUNTIME_LOADER_OVERRIDES.read().expect("runtime loader overrides poisoned").clone()
}

pub fn with_default_runtime_loader<T>(f: impl FnOnce(&RuntimeLoader) -> T) -> T {
    let guard = DEFAULT_RUNTIME_LOADER.read().expect("runtime loader poisoned");
    f(&guard)
}

pub fn with_default_runtime_loader_mut<T>(f: impl FnOnce(&mut RuntimeLoader) -> T) -> T {
    let mut guard = DEFAULT_RUNTIME_LOADER.write().expect("runtime loader poisoned");
    f(&mut guard)
}

pub(crate) fn builtin_runtime_languages() -> Languages {
    let (languages, _) = builtin_runtime_components();
    languages
}

pub(crate) fn merged_runtime_languages(extra: &Languages) -> Languages {
    let mut definitions =
        builtin_runtime_languages().iter().map(|language| (**language).clone()).collect::<Vec<_>>();
    definitions.extend(extra.iter().map(|language| (**language).clone()));
    Languages::new(&definitions)
}

pub fn reload_default_runtime_loader_languages(
    languages: &Languages,
) -> Result<(), RuntimeLoaderError> {
    let (_, mut overrides) = builtin_runtime_components();
    let external = default_runtime_loader_overrides();
    super::languages::merge_runtime_language_overrides(&mut overrides, &external.user_overrides);
    with_default_runtime_loader_mut(|loader| {
        let workspace =
            (!external.workspace_overrides.is_empty()).then_some(WorkspaceRuntimeOverrides {
                trusted: external.workspace_trusted,
                overrides: &external.workspace_overrides,
            });
        loader.reload_merged_languages(languages, &overrides, workspace)?;
        loader.invalidate_all();
        Ok(())
    })
}

pub fn validate_runtime_language_overrides(
    user_overrides: &RuntimeLanguageOverrides,
    workspace_overrides: &RuntimeLanguageOverrides,
    workspace_trusted: bool,
) -> Result<(), RuntimeLoaderError> {
    let (languages, mut overrides) = builtin_runtime_components();
    super::languages::merge_runtime_language_overrides(&mut overrides, user_overrides);
    let roots = RuntimeRoots::new(
        bundled_runtime_root_from_env(),
        PathBuf::from(".ee-runtime-validation"),
        workspace_trusted.then(|| PathBuf::from(".ee-runtime-validation-workspace")),
    );
    let mut loader =
        RuntimeLoader::new(roots.clone(), roots.parser_directories(workspace_trusted))?;
    let workspace = (!workspace_overrides.is_empty()).then_some(WorkspaceRuntimeOverrides {
        trusted: workspace_trusted,
        overrides: workspace_overrides,
    });
    loader.reload_merged_languages(&languages, &overrides, workspace)
}

pub fn configure_default_runtime_loader_overrides(
    user_overrides: RuntimeLanguageOverrides,
    workspace_overrides: RuntimeLanguageOverrides,
    workspace_trusted: bool,
) -> Result<(), RuntimeLoaderError> {
    configure_default_runtime_loader_overrides_inner(
        DefaultRuntimeLoaderOverrides { user_overrides, workspace_overrides, workspace_trusted },
        false,
    )
}

pub fn configure_default_runtime_loader_overrides_if_changed(
    user_overrides: RuntimeLanguageOverrides,
    workspace_overrides: RuntimeLanguageOverrides,
    workspace_trusted: bool,
) -> Result<(), RuntimeLoaderError> {
    configure_default_runtime_loader_overrides_inner(
        DefaultRuntimeLoaderOverrides { user_overrides, workspace_overrides, workspace_trusted },
        true,
    )
}

fn configure_default_runtime_loader_overrides_inner(
    requested: DefaultRuntimeLoaderOverrides,
    skip_unchanged: bool,
) -> Result<(), RuntimeLoaderError> {
    {
        let mut guard =
            DEFAULT_RUNTIME_LOADER_OVERRIDES.write().expect("runtime loader overrides poisoned");
        if skip_unchanged && *guard == requested {
            return Ok(());
        }
        *guard = requested;
    }
    reload_default_runtime_loader_languages(&builtin_runtime_languages())
}

#[cfg(any(test, feature = "test-grammars"))]
pub(crate) fn ensure_default_runtime_loader_has_test_grammars() {
    with_default_runtime_loader_mut(|loader| {
        if loader.language_for_name("rust").is_none() || loader.language_for_name("bash").is_none()
        {
            *loader = default_runtime_loader();
        }
        preload_builtin_test_grammars(loader);
    });
}

#[cfg(any(test, feature = "test-grammars"))]
fn preload_builtin_test_grammars(loader: &mut RuntimeLoader) {
    macro_rules! preload_test_language {
        ($name:literal, $language:expr, $symbol:literal) => {
            loader.preload_language(
                $name,
                GrammarHandle::from_loaded(
                    $language,
                    PathBuf::from(format!("__test__/{}", normalize_lookup_key($name))),
                    $symbol,
                ),
            );
        };
    }

    preload_test_language!("bash", test_grammars::bash(), "tree_sitter_bash");
    preload_test_language!("c", test_grammars::c(), "tree_sitter_c");
    preload_test_language!("csharp", test_grammars::c_sharp(), "tree_sitter_c_sharp");
    preload_test_language!("cpp", test_grammars::cpp(), "tree_sitter_cpp");
    preload_test_language!("css", test_grammars::css(), "tree_sitter_css");
    preload_test_language!("elixir", test_grammars::elixir(), "tree_sitter_elixir");
    preload_test_language!("go", test_grammars::go(), "tree_sitter_go");
    preload_test_language!("haskell", test_grammars::haskell(), "tree_sitter_haskell");
    preload_test_language!("html", test_grammars::html(), "tree_sitter_html");
    preload_test_language!("java", test_grammars::java(), "tree_sitter_java");
    preload_test_language!("javascript", test_grammars::javascript(), "tree_sitter_javascript");
    preload_test_language!("json", test_grammars::json(), "tree_sitter_json");
    preload_test_language!("php", test_grammars::php(), "tree_sitter_php");
    preload_test_language!("python", test_grammars::python(), "tree_sitter_python");
    preload_test_language!("ruby", test_grammars::ruby(), "tree_sitter_ruby");
    preload_test_language!("rust", test_grammars::rust(), "tree_sitter_rust");
    preload_test_language!("scala", test_grammars::scala(), "tree_sitter_scala");
    preload_test_language!("typescript", test_grammars::typescript(), "tree_sitter_typescript");
    preload_test_language!("yaml", test_grammars::yaml(), "tree_sitter_yaml");
}

// ---------------------------------------------------------------------------
// Test guard
// ---------------------------------------------------------------------------
#[cfg(test)]
pub(crate) fn runtime_loader_test_guard() -> MutexGuard<'static, ()> {
    static RUNTIME_LOADER_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
    match RUNTIME_LOADER_TEST_LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[allow(dead_code)]
fn _loader_language_configuration_type(_: Option<&tree_sitter_loader::LanguageConfiguration<'_>>) {}
