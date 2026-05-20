use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, LazyLock, RwLock};
use std::time::SystemTime;

#[cfg(any(test, feature = "test-grammars"))]
use ee_ts_test_grammars as test_grammars;

use globset::Glob;
use regex::Regex;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use semver::Version;
use serde::{Deserialize, Serialize};
use tree_sitter::{Language, Query, QueryError};
use tree_sitter_loader::{
    Config as LoaderConfig, LanguageConfiguration as LoaderLanguageConfiguration, Loader,
    LoaderError,
};

use crate::syntax::{LanguageDefinition, Languages};
use crate::tree_sitter_support::{
    BlockCommentStyle, IndentationStrategy, LanguageMetadata, LineCommentStyle, SemanticTargetKind,
};

pub const RUNTIME_DIR_NAME: &str = "ee";
pub const GRAMMARS_DIR_NAME: &str = "grammars";
pub const QUERIES_DIR_NAME: &str = "queries";
pub const SOURCES_DIR_NAME: &str = "sources";

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeQueryKind {
    Highlights,
    Injections,
    Locals,
    Tags,
    Textobjects,
    Indents,
    Folds,
    Rainbows,
}

impl RuntimeQueryKind {
    pub const STANDARD: [Self; 4] = [Self::Highlights, Self::Injections, Self::Locals, Self::Tags];

    pub const EE_OWNED: [Self; 4] = [Self::Textobjects, Self::Indents, Self::Folds, Self::Rainbows];

    pub fn file_name(self) -> &'static str {
        match self {
            Self::Highlights => "highlights.scm",
            Self::Injections => "injections.scm",
            Self::Locals => "locals.scm",
            Self::Tags => "tags.scm",
            Self::Textobjects => "textobjects.scm",
            Self::Indents => "indents.scm",
            Self::Folds => "folds.scm",
            Self::Rainbows => "rainbows.scm",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RuntimeGrammarCrateSource {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeGrammarGitSource {
    pub url: String,
    pub branch: Option<String>,
    pub tag: Option<String>,
    pub rev: Option<String>,
}

impl JsonSchema for RuntimeGrammarGitSource {
    fn schema_name() -> Cow<'static, str> {
        "RuntimeGrammarGitSource".into()
    }

    fn schema_id() -> Cow<'static, str> {
        concat!(module_path!(), "::RuntimeGrammarGitSource").into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "object",
            "properties": {
                "url": { "type": "string" },
                "branch": { "type": ["string", "null"] },
                "tag": { "type": ["string", "null"] },
                "rev": { "type": ["string", "null"] }
            },
            "required": ["url"],
            "additionalProperties": false,
            "oneOf": [
                {
                    "required": ["branch"],
                    "properties": { "branch": { "type": "string" } },
                    "not": {
                        "anyOf": [
                            { "required": ["tag"] },
                            { "required": ["rev"] }
                        ]
                    }
                },
                {
                    "required": ["tag"],
                    "properties": { "tag": { "type": "string" } },
                    "not": {
                        "anyOf": [
                            { "required": ["branch"] },
                            { "required": ["rev"] }
                        ]
                    }
                },
                {
                    "required": ["rev"],
                    "properties": { "rev": { "type": "string" } },
                    "not": {
                        "anyOf": [
                            { "required": ["branch"] },
                            { "required": ["tag"] }
                        ]
                    }
                }
            ]
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum RuntimeGrammarSource {
    #[serde(rename = "crate")]
    Crate(RuntimeGrammarCrateSource),
    #[serde(rename = "git")]
    Git(RuntimeGrammarGitSource),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeGrammarConfig {
    pub library: Option<String>,
    pub symbol: Option<String>,
    pub source: Option<RuntimeGrammarSource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeConfigSource {
    Bundled,
    User,
    Workspace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeRoots {
    bundled_root: PathBuf,
    user_root: PathBuf,
    workspace_root: Option<PathBuf>,
}

impl RuntimeRoots {
    pub fn new(
        bundled_root: impl Into<PathBuf>,
        user_root: impl Into<PathBuf>,
        workspace_root: Option<PathBuf>,
    ) -> Self {
        Self { bundled_root: bundled_root.into(), user_root: user_root.into(), workspace_root }
    }

    pub fn user_root_for_data_dir(data_dir: &Path) -> PathBuf {
        data_dir.join(RUNTIME_DIR_NAME)
    }

    pub fn from_data_dir(
        bundled_root: impl Into<PathBuf>,
        data_dir: Option<PathBuf>,
        workspace_root: Option<PathBuf>,
    ) -> Option<Self> {
        let data_dir = match data_dir {
            Some(data_dir) => data_dir,
            None => dirs::data_dir()?,
        };
        Some(Self::new(bundled_root, Self::user_root_for_data_dir(&data_dir), workspace_root))
    }

    pub fn bundled_root(&self) -> &Path {
        &self.bundled_root
    }

    pub fn user_root(&self) -> &Path {
        &self.user_root
    }

    pub fn workspace_root(&self) -> Option<&Path> {
        self.workspace_root.as_deref()
    }

    pub fn root_for(&self, source: RuntimeConfigSource) -> Option<&Path> {
        match source {
            RuntimeConfigSource::Bundled => Some(self.bundled_root()),
            RuntimeConfigSource::User => Some(self.user_root()),
            RuntimeConfigSource::Workspace => self.workspace_root(),
        }
    }

    pub fn grammar_dir_for(&self, source: RuntimeConfigSource) -> Option<PathBuf> {
        self.root_for(source).map(|root| root.join(GRAMMARS_DIR_NAME))
    }

    pub fn query_dir_for(&self, source: RuntimeConfigSource, language_id: &str) -> Option<PathBuf> {
        self.root_for(source).map(|root| root.join(QUERIES_DIR_NAME).join(language_id))
    }

    pub fn source_dir_for(&self, source: RuntimeConfigSource) -> Option<PathBuf> {
        self.root_for(source).map(|root| root.join(SOURCES_DIR_NAME))
    }

    pub fn parser_directories(&self, include_workspace: bool) -> Vec<PathBuf> {
        let mut roots = vec![self.bundled_root.clone(), self.user_root.clone()];
        if include_workspace {
            if let Some(workspace_root) = &self.workspace_root {
                roots.push(workspace_root.clone());
            }
        }
        roots
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct RuntimeLanguageConfig {
    pub enabled: Option<bool>,
    pub lsp: Option<Vec<String>>,
    pub name: Option<String>,
    pub query_language: Option<String>,
    pub scope: Option<String>,
    pub content_regex: Option<String>,
    pub first_line_regex: Option<String>,
    pub injection_regex: Option<String>,
    pub aliases: Option<Vec<String>>,
    pub file_types: Option<Vec<String>>,
    pub globs: Option<Vec<String>>,
    pub shebangs: Option<Vec<String>>,
    pub supported_query_kinds: Option<BTreeSet<RuntimeQueryKind>>,
    pub match_priority: Option<i32>,
    pub grammar: Option<RuntimeGrammarConfig>,
    #[serde(skip)]
    pub(crate) metadata: Option<LanguageMetadata>,
    #[serde(skip)]
    pub(crate) standard_query_paths: Option<RuntimeStandardQueryPaths>,
}

pub type RuntimeLanguageOverrides = BTreeMap<String, RuntimeLanguageConfig>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RuntimeStandardQueryPaths {
    highlights: Option<Vec<PathBuf>>,
    injections: Option<Vec<PathBuf>>,
    locals: Option<Vec<PathBuf>>,
    tags: Option<Vec<PathBuf>>,
}

impl RuntimeStandardQueryPaths {
    fn from_loader_configuration(configuration: &LoaderLanguageConfiguration<'_>) -> Self {
        let resolve = |paths: &Option<Vec<PathBuf>>| {
            paths.as_ref().map(|paths| {
                paths.iter().map(|path| configuration.root_path.join(path)).collect::<Vec<_>>()
            })
        };

        Self {
            highlights: resolve(&configuration.highlights_filenames),
            injections: resolve(&configuration.injections_filenames),
            locals: resolve(&configuration.locals_filenames),
            tags: resolve(&configuration.tags_filenames),
        }
    }

    fn is_empty(&self) -> bool {
        self.highlights.is_none()
            && self.injections.is_none()
            && self.locals.is_none()
            && self.tags.is_none()
    }

    fn for_kind(&self, kind: RuntimeQueryKind) -> Option<&[PathBuf]> {
        match kind {
            RuntimeQueryKind::Highlights => self.highlights.as_deref(),
            RuntimeQueryKind::Injections => self.injections.as_deref(),
            RuntimeQueryKind::Locals => self.locals.as_deref(),
            RuntimeQueryKind::Tags => self.tags.as_deref(),
            RuntimeQueryKind::Textobjects
            | RuntimeQueryKind::Indents
            | RuntimeQueryKind::Folds
            | RuntimeQueryKind::Rainbows => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WorkspaceRuntimeOverrides<'a> {
    pub trusted: bool,
    pub overrides: &'a RuntimeLanguageOverrides,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeLanguage {
    canonical_id: String,
    display_name: String,
    grammar_id: String,
    grammar_library_name: Option<String>,
    grammar_crate_version: Option<String>,
    grammar_symbol_name: Option<String>,
    grammar_source: Option<RuntimeGrammarSource>,
    query_language: String,
    scope: Option<String>,
    content_regex: Option<String>,
    first_line_regex: Option<String>,
    injection_regex: Option<String>,
    aliases: Vec<String>,
    file_types: Vec<String>,
    globs: Vec<String>,
    shebangs: Vec<String>,
    supported_query_kinds: BTreeSet<RuntimeQueryKind>,
    match_priority: i32,
    asset_source: RuntimeConfigSource,
    has_base_definition: bool,
    metadata: LanguageMetadata,
    standard_query_paths: RuntimeStandardQueryPaths,
}

impl RuntimeLanguage {
    fn from_definition(definition: &LanguageDefinition) -> Self {
        let display_name = definition.name.as_ref().to_string();
        Self {
            canonical_id: display_name.clone(),
            display_name,
            grammar_id: definition.name.as_ref().to_string(),
            grammar_library_name: None,
            grammar_crate_version: None,
            grammar_symbol_name: None,
            grammar_source: None,
            query_language: definition.name.as_ref().to_string(),
            scope: Some(definition.scope.clone()),
            content_regex: None,
            first_line_regex: definition.first_line_match.clone(),
            injection_regex: None,
            aliases: Vec::new(),
            file_types: definition.extensions.clone(),
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

    fn new_config_only(language_id: &str) -> Self {
        Self {
            canonical_id: language_id.to_string(),
            display_name: language_id.to_string(),
            grammar_id: language_id.to_string(),
            grammar_library_name: None,
            grammar_crate_version: None,
            grammar_symbol_name: None,
            grammar_source: None,
            query_language: language_id.to_string(),
            scope: Some(format!("source.{language_id}")),
            content_regex: None,
            first_line_regex: None,
            injection_regex: None,
            aliases: Vec::new(),
            file_types: Vec::new(),
            globs: Vec::new(),
            shebangs: Vec::new(),
            supported_query_kinds: RuntimeQueryKind::STANDARD
                .into_iter()
                .chain(RuntimeQueryKind::EE_OWNED)
                .collect(),
            match_priority: 0,
            asset_source: RuntimeConfigSource::User,
            has_base_definition: false,
            metadata: LanguageMetadata {
                line_comment: LineCommentStyle::Unsupported,
                block_comment: BlockCommentStyle::Unsupported,
                indentation: IndentationStrategy::Unsupported,
                unsupported_semantic_targets: &[],
            },
            standard_query_paths: RuntimeStandardQueryPaths::default(),
        }
    }

    fn apply_config(
        &mut self,
        language_id: &str,
        config: &RuntimeLanguageConfig,
        source: RuntimeConfigSource,
    ) {
        if let Some(name) = &config.name {
            self.display_name = name.clone();
        }
        if !self.has_base_definition {
            self.canonical_id = language_id.to_string();
            self.grammar_id = language_id.to_string();
        }
        if let Some(grammar) = &config.grammar {
            if let Some(library) = &grammar.library {
                self.grammar_library_name = Some(library.clone());
                self.asset_source = source;
            }
            if let Some(symbol) = &grammar.symbol {
                self.grammar_symbol_name = Some(symbol.clone());
                self.asset_source = source;
            }
            if let Some(source_config) = &grammar.source {
                self.grammar_source = Some(source_config.clone());
                self.grammar_crate_version = match source_config {
                    RuntimeGrammarSource::Crate(source) => Some(source.version.clone()),
                    RuntimeGrammarSource::Git(_) => None,
                };
                self.asset_source = source;
            }
        }
        if let Some(query_language) = &config.query_language {
            self.query_language = query_language.clone();
            self.asset_source = source;
        }
        if let Some(scope) = &config.scope {
            self.scope = Some(scope.clone());
        }
        if let Some(content_regex) = &config.content_regex {
            self.content_regex = Some(content_regex.clone());
        }
        if let Some(first_line_regex) = &config.first_line_regex {
            self.first_line_regex = Some(first_line_regex.clone());
        }
        if let Some(injection_regex) = &config.injection_regex {
            self.injection_regex = Some(injection_regex.clone());
        }
        if let Some(aliases) = &config.aliases {
            self.aliases = aliases.clone();
        }
        if let Some(file_types) = &config.file_types {
            self.file_types = file_types
                .iter()
                .map(|file_type| file_type.trim().trim_start_matches('.').to_string())
                .filter(|file_type| !file_type.is_empty())
                .collect();
        }
        if let Some(globs) = &config.globs {
            self.globs = globs.clone();
        }
        if let Some(shebangs) = &config.shebangs {
            self.shebangs = shebangs.clone();
        }
        if let Some(supported_query_kinds) = &config.supported_query_kinds {
            self.supported_query_kinds = supported_query_kinds.clone();
        }
        if let Some(match_priority) = config.match_priority {
            self.match_priority = match_priority;
        }
        if let Some(metadata) = config.metadata {
            self.metadata = metadata;
        }
        if let Some(standard_query_paths) = &config.standard_query_paths {
            self.standard_query_paths = standard_query_paths.clone();
        }
    }

    fn validate_configured(&self) -> Result<(), RuntimeLoaderError> {
        if self.canonical_id.trim().is_empty() {
            return Err(RuntimeLoaderError::InvalidConfig {
                message: String::from("runtime language id must not be empty"),
            });
        }
        if self.display_name.trim().is_empty() {
            return Err(RuntimeLoaderError::InvalidConfig {
                message: format!("runtime language `{}` has empty name", self.canonical_id),
            });
        }
        if self.file_types.is_empty() {
            return Err(RuntimeLoaderError::InvalidConfig {
                message: format!(
                    "runtime language `{}` is missing non-empty file_types",
                    self.canonical_id
                ),
            });
        }
        let has_any_grammar = self.grammar_library_name.is_some()
            || self.grammar_symbol_name.is_some()
            || self.grammar_source.is_some();
        if has_any_grammar || !self.has_base_definition {
            if self.grammar_library_name.as_deref().is_none_or(str::is_empty) {
                return Err(RuntimeLoaderError::InvalidConfig {
                    message: format!(
                        "runtime language `{}` is missing grammar.library",
                        self.canonical_id
                    ),
                });
            }
            if self.grammar_symbol_name.as_deref().is_none_or(str::is_empty) {
                return Err(RuntimeLoaderError::InvalidConfig {
                    message: format!(
                        "runtime language `{}` is missing grammar.symbol",
                        self.canonical_id
                    ),
                });
            }
            let Some(source) = &self.grammar_source else {
                return Err(RuntimeLoaderError::InvalidConfig {
                    message: format!(
                        "runtime language `{}` is missing grammar.source",
                        self.canonical_id
                    ),
                });
            };
            validate_runtime_grammar_source(&self.canonical_id, source)
                .map_err(|message| RuntimeLoaderError::InvalidConfig { message })?;
        }
        if self.file_types.iter().any(|file_type| file_type.trim().is_empty()) {
            return Err(RuntimeLoaderError::InvalidConfig {
                message: format!(
                    "runtime language `{}` has empty file_types entry",
                    self.canonical_id
                ),
            });
        }
        Ok(())
    }

    pub fn canonical_id(&self) -> &str {
        &self.canonical_id
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub fn grammar_id(&self) -> &str {
        &self.grammar_id
    }

    pub fn grammar_library_name(&self) -> Option<&str> {
        self.grammar_library_name.as_deref()
    }

    pub fn grammar_crate_version(&self) -> Option<&str> {
        self.grammar_crate_version.as_deref()
    }

    pub fn grammar_symbol_name(&self) -> Option<&str> {
        self.grammar_symbol_name.as_deref()
    }

    pub fn grammar_source(&self) -> Option<&RuntimeGrammarSource> {
        self.grammar_source.as_ref()
    }

    pub fn query_language(&self) -> &str {
        &self.query_language
    }

    pub fn scope(&self) -> Option<&str> {
        self.scope.as_deref()
    }

    pub fn aliases(&self) -> &[String] {
        &self.aliases
    }

    pub fn file_types(&self) -> &[String] {
        &self.file_types
    }

    pub fn globs(&self) -> &[String] {
        &self.globs
    }

    pub fn shebangs(&self) -> &[String] {
        &self.shebangs
    }

    pub fn injection_regex(&self) -> Option<&str> {
        self.injection_regex.as_deref()
    }

    pub fn supported_query_kinds(&self) -> &BTreeSet<RuntimeQueryKind> {
        &self.supported_query_kinds
    }

    pub fn match_priority(&self) -> i32 {
        self.match_priority
    }

    pub fn asset_source(&self) -> RuntimeConfigSource {
        self.asset_source
    }

    pub(crate) fn metadata(&self) -> LanguageMetadata {
        self.metadata
    }

    fn standard_query_paths(&self, kind: RuntimeQueryKind) -> Option<&[PathBuf]> {
        self.standard_query_paths.for_kind(kind)
    }

    pub fn grammar_library_path(&self, roots: &RuntimeRoots) -> Option<PathBuf> {
        let library_name = self.grammar_library_name.as_deref()?;
        let dir = roots.grammar_dir_for(self.asset_source)?;
        Some(dir.join(shared_library_filename(library_name)))
    }

    pub fn query_dir(&self, roots: &RuntimeRoots) -> Option<PathBuf> {
        roots.query_dir_for(self.asset_source, &self.canonical_id)
    }
}

#[derive(Debug, Clone)]
pub struct GrammarHandle {
    language: Language,
    canonical_library_path: PathBuf,
    modified_time: Option<SystemTime>,
    symbol_name: String,
}

impl GrammarHandle {
    pub fn from_loaded(
        language: Language,
        library_path: impl Into<PathBuf>,
        symbol_name: impl Into<String>,
    ) -> Self {
        let canonical_library_path = canonicalize_or_original(library_path.into());
        let modified_time = metadata_modified_time(&canonical_library_path);
        Self { language, canonical_library_path, modified_time, symbol_name: symbol_name.into() }
    }

    pub fn language(&self) -> Language {
        self.language.clone()
    }

    pub fn canonical_library_path(&self) -> &Path {
        &self.canonical_library_path
    }

    pub fn modified_time(&self) -> Option<SystemTime> {
        self.modified_time
    }

    pub fn symbol_name(&self) -> &str {
        &self.symbol_name
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryArtifactCacheEntry {
    pub language_id: String,
    pub kind: RuntimeQueryKind,
    pub source_text: String,
    pub source_paths: Vec<PathBuf>,
    pub source_mtimes: Vec<Option<SystemTime>>,
    pub path_ranges: Vec<(PathBuf, std::ops::Range<usize>)>,
    pub newest_mtime: Option<SystemTime>,
}

type ResolvedQuerySource = (String, Vec<PathBuf>, Vec<(PathBuf, std::ops::Range<usize>)>);

#[derive(Debug)]
pub struct CompiledQueryArtifact {
    pub kind: RuntimeQueryKind,
    pub source_text: String,
    pub source_paths: Vec<PathBuf>,
    pub source_mtimes: Vec<Option<SystemTime>>,
    pub newest_mtime: Option<SystemTime>,
    pub query: Arc<Query>,
}

#[derive(Debug)]
pub struct SyntaxQuerySet {
    pub combined_source: String,
    pub combined_paths: Vec<PathBuf>,
    pub combined_query: Option<Arc<Query>>,
    pub highlights: Option<Arc<CompiledQueryArtifact>>,
    pub injections: Option<Arc<CompiledQueryArtifact>>,
    pub locals: Option<Arc<CompiledQueryArtifact>>,
}

#[derive(Debug)]
pub struct SemanticQuerySet {
    pub textobjects: Option<Arc<CompiledQueryArtifact>>,
    pub tags: Option<Arc<CompiledQueryArtifact>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeLanguageDetectionSource {
    Explicit,
    Shebang,
    Glob,
    FileType,
    FirstLineRegex,
    ContentRegex,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeLanguageMatch {
    pub canonical_id: String,
    pub display_name: String,
    pub detection_source: RuntimeLanguageDetectionSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeInjectionMatch {
    pub canonical_id: String,
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeGrammarHealth {
    Unresolved,
    Loaded,
    Missing,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeQueryHealth {
    Unsupported,
    Missing,
    Loaded,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeQueryHealthReport {
    pub kind: RuntimeQueryKind,
    pub status: RuntimeQueryHealth,
    pub source_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeHealthReport {
    pub requested_language: Option<String>,
    pub requested_injection_language: Option<String>,
    pub file_path: Option<PathBuf>,
    pub detection_source: Option<RuntimeLanguageDetectionSource>,
    pub language_id: Option<String>,
    pub display_name: Option<String>,
    pub injection_match: Option<RuntimeInjectionMatch>,
    pub asset_source: Option<RuntimeConfigSource>,
    pub effective_runtime_root: Option<PathBuf>,
    pub grammar_path: Option<PathBuf>,
    pub grammar_status: RuntimeGrammarHealth,
    pub query_reports: Vec<RuntimeQueryHealthReport>,
    pub runtime_roots: RuntimeRoots,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeOperationErrorKind {
    ConfigMerge,
    GrammarSource,
    RuntimeAsset,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeOperationError {
    kind: RuntimeOperationErrorKind,
    message: String,
}

impl RuntimeOperationError {
    fn new(kind: RuntimeOperationErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }

    fn config_merge(message: impl Into<String>) -> Self {
        Self::new(RuntimeOperationErrorKind::ConfigMerge, message)
    }

    fn grammar_source(message: impl Into<String>) -> Self {
        Self::new(RuntimeOperationErrorKind::GrammarSource, message)
    }

    fn runtime_asset(message: impl Into<String>) -> Self {
        Self::new(RuntimeOperationErrorKind::RuntimeAsset, message)
    }

    pub fn kind(&self) -> RuntimeOperationErrorKind {
        self.kind
    }
}

impl fmt::Display for RuntimeOperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl Error for RuntimeOperationError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeFetchedGrammar {
    pub language_id: String,
    pub crate_name: String,
    pub source_pin: String,
    pub resolved_rev: Option<String>,
    pub source_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeBuiltGrammar {
    pub language_id: String,
    pub source_pin: String,
    pub resolved_rev: Option<String>,
    pub grammar_path: PathBuf,
    pub query_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct GrammarCrateSpec {
    crate_name: String,
    version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GrammarGitSpec {
    url: String,
    branch: Option<String>,
    tag: Option<String>,
    rev: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GrammarFetchPlan {
    Crate(GrammarCrateSpec),
    Git(GrammarGitSpec),
}

impl GrammarFetchPlan {
    fn crate_name(&self, language: &RuntimeLanguage) -> String {
        match self {
            Self::Crate(spec) => spec.crate_name.clone(),
            Self::Git(_) => language
                .grammar_library_name()
                .map(str::to_string)
                .unwrap_or_else(|| language.canonical_id().to_string()),
        }
    }

    fn source_pin(&self) -> String {
        match self {
            Self::Crate(spec) => {
                format!("crate:{}@{}", spec.crate_name, spec.version.as_deref().unwrap_or("*"))
            }
            Self::Git(spec) => match (&spec.branch, &spec.tag, &spec.rev) {
                (Some(branch), None, None) => {
                    format!("git:{}#branch:{}", redact_git_url_credentials(&spec.url), branch)
                }
                (None, Some(tag), None) => {
                    format!("git:{}#tag:{}", redact_git_url_credentials(&spec.url), tag)
                }
                (None, None, Some(rev)) => {
                    format!("git:{}#rev:{}", redact_git_url_credentials(&spec.url), rev)
                }
                _ => String::from("git:invalid"),
            },
        }
    }

    fn source_type(&self) -> &'static str {
        match self {
            Self::Crate(_) => "crate",
            Self::Git(_) => "git",
        }
    }

    fn reference_summary(&self) -> String {
        match self {
            Self::Crate(spec) => format!(
                "crate `{}` version `{}`",
                spec.crate_name,
                spec.version.as_deref().unwrap_or("*")
            ),
            Self::Git(spec) => {
                let url = redact_git_url_credentials(&spec.url);
                match (&spec.branch, &spec.tag, &spec.rev) {
                    (Some(branch), None, None) => {
                        format!("url `{url}` ref branch `{branch}`")
                    }
                    (None, Some(tag), None) => format!("url `{url}` ref tag `{tag}`"),
                    (None, None, Some(rev)) => format!("url `{url}` ref rev `{rev}`"),
                    _ => format!("url `{url}` ref invalid"),
                }
            }
        }
    }

    fn diagnostic_summary(&self, language_id: &str) -> String {
        format!(
            "language `{language_id}` {} source {}",
            self.source_type(),
            self.reference_summary()
        )
    }

    fn stage_dir_name(&self, language: &RuntimeLanguage) -> String {
        let language_id = sanitize_path_component(language.canonical_id());
        match self {
            Self::Crate(spec) => format!(
                "{language_id}-crate-{}-{}",
                sanitize_path_component(&spec.crate_name),
                sanitize_path_component(spec.version.as_deref().unwrap_or("unlocked"))
            ),
            Self::Git(spec) => {
                let (ref_kind, ref_value) = match (&spec.branch, &spec.tag, &spec.rev) {
                    (Some(branch), None, None) => ("branch", branch.as_str()),
                    (None, Some(tag), None) => ("tag", tag.as_str()),
                    (None, None, Some(rev)) => ("rev", rev.as_str()),
                    _ => ("invalid", "invalid"),
                };
                format!(
                    "{language_id}-git-{ref_kind}-{}-{}",
                    sanitize_path_component(ref_value),
                    stable_hash_hex(&spec.url)
                )
            }
        }
    }
}

fn grammar_fetch_plan_for_language(
    language: &RuntimeLanguage,
) -> Result<GrammarFetchPlan, RuntimeOperationError> {
    match language.grammar_source() {
        Some(RuntimeGrammarSource::Crate(source)) => {
            Ok(GrammarFetchPlan::Crate(GrammarCrateSpec {
                crate_name: source.name.clone(),
                version: Some(source.version.clone()),
            }))
        }
        Some(RuntimeGrammarSource::Git(source)) => Ok(GrammarFetchPlan::Git(GrammarGitSpec {
            url: source.url.clone(),
            branch: source.branch.clone(),
            tag: source.tag.clone(),
            rev: source.rev.clone(),
        })),
        None => {
            let crate_name = language.grammar_library_name().ok_or_else(|| {
                RuntimeOperationError::config_merge(format!(
                    "language `{}` has no configured grammar package",
                    language.canonical_id()
                ))
            })?;
            Ok(GrammarFetchPlan::Crate(GrammarCrateSpec {
                crate_name: crate_name.to_string(),
                version: language.grammar_crate_version().map(str::to_string),
            }))
        }
    }
}

#[derive(Debug)]
pub enum RuntimeLoaderError {
    Loader(LoaderError),
    RuntimeDisabled { reason: &'static str },
    InvalidConfig { message: String },
    AmbiguousAlias { alias: String, first_language: String, second_language: String },
    AmbiguousFileType { file_type: String, first_language: String, second_language: String },
    UnknownLanguage { requested: String },
    MissingGrammar { language_id: String, path: Option<PathBuf> },
    GrammarOutsideRuntimeRoot { path: PathBuf, allowed_roots: Vec<PathBuf> },
    QueryIo { kind: RuntimeQueryKind, path: PathBuf, error: io::Error },
    QueryCompile { kind: RuntimeQueryKind, file: Option<PathBuf>, error: QueryError },
    QueryInheritanceCycle { kind: RuntimeQueryKind, chain: Vec<String> },
    UnknownInheritedLanguage { kind: RuntimeQueryKind, language: String },
}

impl fmt::Display for RuntimeLoaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Loader(error) => write!(f, "tree-sitter loader error: {error}"),
            Self::RuntimeDisabled { reason } => write!(f, "runtime tree-sitter disabled: {reason}"),
            Self::InvalidConfig { message } => write!(f, "invalid runtime config: {message}"),
            Self::AmbiguousAlias { alias, first_language, second_language } => write!(
                f,
                "alias `{alias}` is claimed by both `{first_language}` and `{second_language}`"
            ),
            Self::AmbiguousFileType { file_type, first_language, second_language } => write!(
                f,
                "file type `{file_type}` is claimed by both `{first_language}` and `{second_language}` without explicit precedence"
            ),
            Self::UnknownLanguage { requested } => {
                write!(f, "unknown runtime language `{requested}`")
            }
            Self::MissingGrammar { language_id, path } => match path {
                Some(path) => {
                    write!(f, "missing runtime grammar for `{language_id}` at {}", path.display())
                }
                None => write!(f, "missing runtime grammar for `{language_id}`"),
            },
            Self::GrammarOutsideRuntimeRoot { path, allowed_roots } => write!(
                f,
                "grammar path {} is outside known runtime roots {:?}",
                path.display(),
                allowed_roots
            ),
            Self::QueryIo { kind, path, error } => {
                write!(f, "failed reading {} for {:?}: {error}", path.display(), kind)
            }
            Self::QueryCompile { kind, file, error } => match file {
                Some(file) => {
                    write!(f, "failed compiling {:?} query {}: {error}", kind, file.display())
                }
                None => write!(f, "failed compiling {:?} query: {error}", kind),
            },
            Self::QueryInheritanceCycle { kind, chain } => {
                write!(f, "query inheritance cycle for {:?}: {}", kind, chain.join(" -> "))
            }
            Self::UnknownInheritedLanguage { kind, language } => {
                write!(f, "unknown inherited language `{language}` for {:?}", kind)
            }
        }
    }
}

impl Error for RuntimeLoaderError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Loader(error) => Some(error),
            Self::QueryIo { error, .. } => Some(error),
            Self::QueryCompile { error, .. } => Some(error),
            Self::InvalidConfig { .. }
            | Self::RuntimeDisabled { .. }
            | Self::AmbiguousAlias { .. }
            | Self::AmbiguousFileType { .. }
            | Self::UnknownLanguage { .. }
            | Self::MissingGrammar { .. }
            | Self::GrammarOutsideRuntimeRoot { .. }
            | Self::QueryInheritanceCycle { .. }
            | Self::UnknownInheritedLanguage { .. } => None,
        }
    }
}

impl From<LoaderError> for RuntimeLoaderError {
    fn from(value: LoaderError) -> Self {
        Self::Loader(value)
    }
}

#[derive(Debug, Clone)]
struct FileTypeOwner {
    canonical_id: String,
    priority: i32,
}

#[derive(Debug, Clone, Default)]
struct DefaultRuntimeLoaderOverrides {
    user_overrides: RuntimeLanguageOverrides,
    workspace_overrides: RuntimeLanguageOverrides,
    workspace_trusted: bool,
}

pub struct RuntimeLoader {
    runtime_roots: RuntimeRoots,
    loader_config: LoaderConfig,
    loader: Loader,
    workspace_runtime_trusted: bool,
    languages: BTreeMap<String, RuntimeLanguage>,
    alias_index: HashMap<String, String>,
    file_type_index: HashMap<String, FileTypeOwner>,
    preloaded_grammars: HashMap<String, GrammarHandle>,
    grammar_cache: HashMap<PathBuf, GrammarHandle>,
    query_cache: HashMap<(String, RuntimeQueryKind), QueryArtifactCacheEntry>,
    compiled_query_cache: HashMap<(String, RuntimeQueryKind), Arc<CompiledQueryArtifact>>,
}

impl RuntimeLoader {
    pub fn new(
        runtime_roots: RuntimeRoots,
        parser_directories: Vec<PathBuf>,
    ) -> Result<Self, RuntimeLoaderError> {
        let parser_lib_path = runtime_roots
            .grammar_dir_for(RuntimeConfigSource::User)
            .unwrap_or_else(|| runtime_roots.user_root().join(GRAMMARS_DIR_NAME));
        let loader_config = LoaderConfig { parser_directories };
        Ok(Self {
            runtime_roots,
            loader_config,
            loader: Loader::with_parser_lib_path(parser_lib_path),
            workspace_runtime_trusted: false,
            languages: BTreeMap::new(),
            alias_index: HashMap::new(),
            file_type_index: HashMap::new(),
            preloaded_grammars: HashMap::new(),
            grammar_cache: HashMap::new(),
            query_cache: HashMap::new(),
            compiled_query_cache: HashMap::new(),
        })
    }

    pub fn runtime_roots(&self) -> &RuntimeRoots {
        &self.runtime_roots
    }

    pub fn default_user_source_root(&self) -> PathBuf {
        self.runtime_roots
            .source_dir_for(RuntimeConfigSource::User)
            .unwrap_or_else(|| self.runtime_roots.user_root().join(SOURCES_DIR_NAME))
    }

    pub fn loader_config(&self) -> &LoaderConfig {
        &self.loader_config
    }

    pub fn upstream_loader(&self) -> &Loader {
        &self.loader
    }

    pub fn upstream_loader_mut(&mut self) -> &mut Loader {
        &mut self.loader
    }

    pub fn languages(&self) -> impl Iterator<Item = &RuntimeLanguage> {
        self.languages.values()
    }

    pub fn language_for_name(&self, name: &str) -> Option<&RuntimeLanguage> {
        let key = normalize_lookup_key(name);
        self.alias_index.get(&key).and_then(|canonical_id| self.languages.get(canonical_id))
    }

    pub fn language_for_path(&self, path: &Path) -> Option<&RuntimeLanguage> {
        let file_type = path
            .extension()
            .or_else(|| path.file_name())
            .and_then(|segment| segment.to_str())?
            .to_ascii_lowercase();
        self.file_type_index
            .get(&file_type)
            .and_then(|owner| self.languages.get(&owner.canonical_id))
    }

    pub fn detect_language(
        &self,
        file_path: Option<&Path>,
        first_line: Option<&str>,
        content: Option<&str>,
    ) -> Option<RuntimeLanguageMatch> {
        if let Some((language, source)) =
            self.detect_language_with_source(file_path, first_line, content)
        {
            return Some(RuntimeLanguageMatch {
                canonical_id: language.canonical_id().to_string(),
                display_name: language.display_name().to_string(),
                detection_source: source,
            });
        }
        None
    }

    pub fn match_injection_language(
        &self,
        injection_language: &str,
    ) -> Option<RuntimeInjectionMatch> {
        self.ordered_languages().into_iter().find_map(|language| {
            language
                .injection_regex()
                .filter(|pattern| regex_matches(pattern, injection_language))
                .map(|_| RuntimeInjectionMatch {
                    canonical_id: language.canonical_id().to_string(),
                    display_name: language.display_name().to_string(),
                })
        })
    }

    pub fn canonical_language_name(&self, requested: &str) -> Option<String> {
        self.language_for_name(requested).map(|language| language.canonical_id().to_string())
    }

    pub fn supports_query_kind(&self, language_name: &str, kind: RuntimeQueryKind) -> bool {
        self.language_for_name(language_name)
            .is_some_and(|language| language.supported_query_kinds().contains(&kind))
    }

    pub fn supports_any_query_kind(&self, language_name: &str, kinds: &[RuntimeQueryKind]) -> bool {
        kinds.iter().copied().any(|kind| self.supports_query_kind(language_name, kind))
    }

    pub fn preload_language(&mut self, language_id: &str, handle: GrammarHandle) {
        self.preloaded_grammars.insert(normalize_lookup_key(language_id), handle);
    }

    pub fn load_language_for_name(
        &mut self,
        language_name: &str,
    ) -> Result<GrammarHandle, RuntimeLoaderError> {
        let canonical_id = self
            .language_for_name(language_name)
            .map(|language| language.canonical_id().to_string())
            .ok_or_else(|| RuntimeLoaderError::UnknownLanguage {
                requested: language_name.to_string(),
            })?;
        self.load_language_for_canonical_id(&canonical_id)
    }

    pub fn load_language_for_path(
        &mut self,
        path: &Path,
    ) -> Result<GrammarHandle, RuntimeLoaderError> {
        let canonical_id = self
            .language_for_path(path)
            .map(|language| language.canonical_id().to_string())
            .ok_or_else(|| RuntimeLoaderError::UnknownLanguage {
                requested: path.display().to_string(),
            })?;
        self.load_language_for_canonical_id(&canonical_id)
    }

    pub fn reload_merged_languages(
        &mut self,
        languages: &Languages,
        user_overrides: &RuntimeLanguageOverrides,
        workspace_overrides: Option<WorkspaceRuntimeOverrides<'_>>,
    ) -> Result<(), RuntimeLoaderError> {
        self.workspace_runtime_trusted =
            workspace_overrides.is_some_and(|workspace| workspace.trusted);
        let upstream_standard_query_paths = self.discover_upstream_standard_query_paths();
        let mut merged = BTreeMap::new();
        let mut alias_index = HashMap::new();
        let mut file_type_index = HashMap::new();

        let mut configured_ids = languages
            .iter()
            .map(|definition| normalize_lookup_key(definition.name.as_ref()))
            .collect::<BTreeSet<_>>();
        configured_ids.extend(user_overrides.keys().map(|id| normalize_lookup_key(id)));
        if let Some(workspace) = workspace_overrides.filter(|workspace| workspace.trusted) {
            configured_ids.extend(workspace.overrides.keys().map(|id| normalize_lookup_key(id)));
        }

        for language_id in configured_ids {
            let definition = languages
                .iter()
                .find(|definition| normalize_lookup_key(definition.name.as_ref()) == language_id);
            let mut language =
                definition.map(|definition| RuntimeLanguage::from_definition(definition));

            if let Some(user_config) = lookup_runtime_language_config(user_overrides, &language_id)
            {
                language = apply_runtime_language_config(
                    language,
                    &language_id,
                    user_config,
                    RuntimeConfigSource::User,
                );
            }

            if let Some(workspace) = workspace_overrides.filter(|workspace| workspace.trusted)
                && let Some(workspace_config) =
                    lookup_runtime_language_config(workspace.overrides, &language_id)
            {
                language = apply_runtime_language_config(
                    language,
                    &language_id,
                    workspace_config,
                    RuntimeConfigSource::Workspace,
                );
            }

            let Some(mut language) = language else {
                continue;
            };

            if let Some(standard_query_paths) =
                upstream_standard_query_paths.get(&normalize_lookup_key(language.query_language()))
            {
                language.standard_query_paths = standard_query_paths.clone();
            }

            language.validate_configured()?;
            self.index_language_aliases(&language, &mut alias_index)?;
            self.index_language_file_types(&language, &mut file_type_index)?;
            merged.insert(language.canonical_id.clone(), language);
        }

        self.languages = merged;
        self.alias_index = alias_index;
        self.file_type_index = file_type_index;
        Ok(())
    }

    pub fn record_grammar_handle(&mut self, handle: GrammarHandle) {
        self.grammar_cache.insert(handle.canonical_library_path.clone(), handle);
    }

    pub fn cached_grammar_handle(&self, library_path: &Path) -> Option<&GrammarHandle> {
        self.grammar_cache
            .get(&canonicalize_or_original(library_path.to_path_buf()))
            .filter(|handle| grammar_handle_is_fresh(handle))
    }

    pub fn record_query_artifact(
        &mut self,
        language_id: impl Into<String>,
        kind: RuntimeQueryKind,
        source_text: String,
        source_paths: Vec<PathBuf>,
        path_ranges: Vec<(PathBuf, std::ops::Range<usize>)>,
    ) {
        let language_id = language_id.into();
        let source_mtimes = current_source_mtimes(&source_paths);
        let newest_mtime = source_mtimes.iter().flatten().copied().max();
        self.query_cache.insert(
            (language_id.clone(), kind),
            QueryArtifactCacheEntry {
                language_id,
                kind,
                source_text,
                source_paths,
                source_mtimes,
                path_ranges,
                newest_mtime,
            },
        );
    }

    pub fn cached_query_artifact(
        &self,
        language_id: &str,
        kind: RuntimeQueryKind,
    ) -> Option<&QueryArtifactCacheEntry> {
        self.query_cache
            .get(&(language_id.to_string(), kind))
            .filter(|entry| query_artifact_is_fresh(entry))
    }

    pub fn invalidate_all(&mut self) {
        self.grammar_cache.clear();
        self.query_cache.clear();
        self.compiled_query_cache.clear();
    }

    pub fn invalidate_language(&mut self, language_id: &str) {
        let Some((canonical_id, library_path)) =
            self.language_for_name(language_id).map(|language| {
                (
                    language.canonical_id().to_string(),
                    language.grammar_library_path(&self.runtime_roots),
                )
            })
        else {
            return;
        };
        if let Some(library_path) = library_path {
            self.grammar_cache.remove(&canonicalize_or_original(library_path));
        }
        self.compiled_query_cache.retain(|(cached_language_id, _), _| {
            !cached_language_id.eq_ignore_ascii_case(&canonical_id)
        });
        self.query_cache.retain(|(cached_language_id, _), _| {
            !cached_language_id.eq_ignore_ascii_case(&canonical_id)
        });
    }

    pub fn resolve_query_source(
        &mut self,
        language_name: &str,
        kind: RuntimeQueryKind,
    ) -> Result<Option<&QueryArtifactCacheEntry>, RuntimeLoaderError> {
        let canonical_id = self
            .language_for_name(language_name)
            .map(|language| language.canonical_id().to_string())
            .ok_or_else(|| RuntimeLoaderError::UnknownLanguage {
                requested: language_name.to_string(),
            })?;
        let cache_key = (canonical_id.clone(), kind);
        let needs_refresh =
            self.query_cache.get(&cache_key).is_none_or(|entry| !query_artifact_is_fresh(entry));
        if needs_refresh {
            self.query_cache.remove(&cache_key);
            let resolved =
                self.resolve_query_source_uncached(&canonical_id, kind, &mut Vec::new())?;
            if let Some(resolved) = resolved {
                self.record_query_artifact(
                    canonical_id.clone(),
                    kind,
                    resolved.0,
                    resolved.1,
                    resolved.2,
                );
            }
        }
        Ok(self.query_cache.get(&cache_key).filter(|entry| query_artifact_is_fresh(entry)))
    }

    pub fn compile_query_kind(
        &mut self,
        language_name: &str,
        kind: RuntimeQueryKind,
    ) -> Result<Option<Arc<CompiledQueryArtifact>>, RuntimeLoaderError> {
        let artifact = self.resolve_query_source(language_name, kind)?.cloned();
        let Some(artifact) = artifact else {
            return Ok(None);
        };
        let cache_key = (artifact.language_id.clone(), kind);
        if let Some(cached) = self.compiled_query_cache.get(&cache_key) {
            if cached.newest_mtime == artifact.newest_mtime
                && cached.source_mtimes == artifact.source_mtimes
                && cached.source_paths == artifact.source_paths
                && cached.source_text == artifact.source_text
            {
                return Ok(Some(Arc::clone(cached)));
            }
        }
        let compiled = self.compile_query_artifact(language_name, kind, artifact)?;
        self.compiled_query_cache.insert(cache_key, Arc::clone(&compiled));
        Ok(Some(compiled))
    }

    pub fn compile_query_kind_transient(
        &mut self,
        language_name: &str,
        kind: RuntimeQueryKind,
    ) -> Result<Option<Arc<CompiledQueryArtifact>>, RuntimeLoaderError> {
        let canonical_id = self
            .language_for_name(language_name)
            .map(|language| language.canonical_id().to_string())
            .ok_or_else(|| RuntimeLoaderError::UnknownLanguage {
                requested: language_name.to_string(),
            })?;
        let artifact = if let Some(cached) = self.cached_query_artifact(&canonical_id, kind) {
            Some(cached.clone())
        } else {
            self.resolve_query_source_uncached(&canonical_id, kind, &mut Vec::new())?.map(
                |(source_text, source_paths, path_ranges)| {
                    let source_mtimes = current_source_mtimes(&source_paths);
                    let newest_mtime = source_mtimes.iter().flatten().copied().max();
                    QueryArtifactCacheEntry {
                        language_id: canonical_id.clone(),
                        kind,
                        source_text,
                        source_paths,
                        source_mtimes,
                        path_ranges,
                        newest_mtime,
                    }
                },
            )
        };
        artifact
            .map(|artifact| self.compile_query_artifact(language_name, kind, artifact))
            .transpose()
    }

    fn compile_query_artifact(
        &mut self,
        language_name: &str,
        kind: RuntimeQueryKind,
        artifact: QueryArtifactCacheEntry,
    ) -> Result<Arc<CompiledQueryArtifact>, RuntimeLoaderError> {
        let handle = self.load_language_for_name(language_name)?;
        let query = Query::new(&handle.language(), &artifact.source_text)
            .map_err(|error| map_query_error(kind, error, &artifact.path_ranges))?;
        Ok(Arc::new(CompiledQueryArtifact {
            kind,
            source_text: artifact.source_text,
            source_paths: artifact.source_paths,
            source_mtimes: artifact.source_mtimes,
            newest_mtime: artifact.newest_mtime,
            query: Arc::new(query),
        }))
    }

    pub fn compile_syntax_queries(
        &mut self,
        language_name: &str,
    ) -> Result<SyntaxQuerySet, RuntimeLoaderError> {
        let highlights = self.compile_query_kind(language_name, RuntimeQueryKind::Highlights)?;
        let injections = self.compile_query_kind(language_name, RuntimeQueryKind::Injections)?;
        let locals = self.compile_query_kind(language_name, RuntimeQueryKind::Locals)?;

        let mut combined_source = String::new();
        let mut combined_paths = Vec::new();
        let mut combined_ranges = Vec::new();
        for artifact in [&highlights, &injections, &locals].into_iter().flatten() {
            let start = combined_source.len();
            combined_source.push_str(&artifact.source_text);
            if !artifact.source_text.ends_with('\n') {
                combined_source.push('\n');
            }
            let end = combined_source.len();
            if let Some(path) = artifact.source_paths.first() {
                combined_paths.extend(artifact.source_paths.iter().cloned());
                combined_ranges.push((path.clone(), start..end));
            }
        }

        let combined_query = if combined_source.trim().is_empty() {
            None
        } else {
            let handle = self.load_language_for_name(language_name)?;
            Some(Arc::new(Query::new(&handle.language(), &combined_source).map_err(|error| {
                map_query_error(RuntimeQueryKind::Highlights, error, &combined_ranges)
            })?))
        };

        Ok(SyntaxQuerySet {
            combined_source,
            combined_paths,
            combined_query,
            highlights,
            injections,
            locals,
        })
    }

    pub fn compile_syntax_queries_transient(
        &mut self,
        language_name: &str,
    ) -> Result<SyntaxQuerySet, RuntimeLoaderError> {
        let highlights =
            self.compile_query_kind_transient(language_name, RuntimeQueryKind::Highlights)?;
        let injections =
            self.compile_query_kind_transient(language_name, RuntimeQueryKind::Injections)?;
        let locals = self.compile_query_kind_transient(language_name, RuntimeQueryKind::Locals)?;

        let mut combined_source = String::new();
        let mut combined_paths = Vec::new();
        let mut combined_ranges = Vec::new();
        for artifact in [&highlights, &injections, &locals].into_iter().flatten() {
            let start = combined_source.len();
            combined_source.push_str(&artifact.source_text);
            if !artifact.source_text.ends_with('\n') {
                combined_source.push('\n');
            }
            let end = combined_source.len();
            if let Some(path) = artifact.source_paths.first() {
                combined_paths.extend(artifact.source_paths.iter().cloned());
                combined_ranges.push((path.clone(), start..end));
            }
        }

        let combined_query = if combined_source.trim().is_empty() {
            None
        } else {
            let handle = self.load_language_for_name(language_name)?;
            Some(Arc::new(Query::new(&handle.language(), &combined_source).map_err(|error| {
                map_query_error(RuntimeQueryKind::Highlights, error, &combined_ranges)
            })?))
        };

        Ok(SyntaxQuerySet {
            combined_source,
            combined_paths,
            combined_query,
            highlights,
            injections,
            locals,
        })
    }

    pub fn compile_semantic_queries(
        &mut self,
        language_name: &str,
    ) -> Result<SemanticQuerySet, RuntimeLoaderError> {
        Ok(SemanticQuerySet {
            textobjects: self.compile_query_kind(language_name, RuntimeQueryKind::Textobjects)?,
            tags: self.compile_query_kind(language_name, RuntimeQueryKind::Tags)?,
        })
    }

    pub fn runtime_health_report(
        &mut self,
        explicit_language: Option<&str>,
        file_path: Option<&Path>,
        first_line: Option<&str>,
        content: Option<&str>,
        injection_language: Option<&str>,
    ) -> RuntimeHealthReport {
        let resolved = explicit_language
            .and_then(|language_name| {
                self.language_for_name(language_name).map(|language| RuntimeLanguageMatch {
                    canonical_id: language.canonical_id().to_string(),
                    display_name: language.display_name().to_string(),
                    detection_source: RuntimeLanguageDetectionSource::Explicit,
                })
            })
            .or_else(|| self.detect_language(file_path, first_line, content));

        let mut report = RuntimeHealthReport {
            requested_language: explicit_language.map(str::to_string),
            requested_injection_language: injection_language.map(str::to_string),
            file_path: file_path.map(Path::to_path_buf),
            detection_source: resolved.as_ref().map(|language| language.detection_source),
            language_id: resolved.as_ref().map(|language| language.canonical_id.clone()),
            display_name: resolved.as_ref().map(|language| language.display_name.clone()),
            injection_match: injection_language
                .and_then(|value| self.match_injection_language(value)),
            asset_source: None,
            effective_runtime_root: None,
            grammar_path: None,
            grammar_status: RuntimeGrammarHealth::Unresolved,
            query_reports: Vec::new(),
            runtime_roots: self.runtime_roots.clone(),
        };

        let Some(language_name) = report.language_id.clone() else {
            return report;
        };
        let Some(language) = self.language_for_name(&language_name).cloned() else {
            report.grammar_status = RuntimeGrammarHealth::Error(format!(
                "resolved runtime language `{language_name}` disappeared from loader state"
            ));
            return report;
        };

        report.asset_source = Some(language.asset_source());
        report.effective_runtime_root =
            self.runtime_roots.root_for(language.asset_source()).map(Path::to_path_buf);
        report.grammar_path = language.grammar_library_path(&self.runtime_roots);
        report.grammar_status = match self.load_language_for_name(&language_name) {
            Ok(_) => RuntimeGrammarHealth::Loaded,
            Err(RuntimeLoaderError::MissingGrammar { .. }) => RuntimeGrammarHealth::Missing,
            Err(error) => RuntimeGrammarHealth::Error(error.to_string()),
        };

        for kind in RuntimeQueryKind::STANDARD.into_iter().chain(RuntimeQueryKind::EE_OWNED) {
            if !language.supported_query_kinds().contains(&kind) {
                report.query_reports.push(RuntimeQueryHealthReport {
                    kind,
                    status: RuntimeQueryHealth::Unsupported,
                    source_paths: Vec::new(),
                });
                continue;
            }

            match self.resolve_query_source(&language_name, kind).map(|artifact| artifact.cloned())
            {
                Ok(Some(artifact)) => {
                    let status = match self.compile_query_kind(&language_name, kind) {
                        Ok(Some(_)) => RuntimeQueryHealth::Loaded,
                        Ok(None) => RuntimeQueryHealth::Missing,
                        Err(error) => RuntimeQueryHealth::Error(error.to_string()),
                    };
                    report.query_reports.push(RuntimeQueryHealthReport {
                        kind,
                        status,
                        source_paths: artifact.source_paths.clone(),
                    });
                }
                Ok(None) => {
                    report.query_reports.push(RuntimeQueryHealthReport {
                        kind,
                        status: RuntimeQueryHealth::Missing,
                        source_paths: self.query_source_paths(&language, kind),
                    });
                }
                Err(error) => {
                    report.query_reports.push(RuntimeQueryHealthReport {
                        kind,
                        status: RuntimeQueryHealth::Error(error.to_string()),
                        source_paths: Vec::new(),
                    });
                }
            }
        }

        report
    }

    pub fn fetch_grammar_sources(
        &self,
        requested_languages: &[String],
        include_all: bool,
        source_root: &Path,
        force: bool,
    ) -> Result<Vec<RuntimeFetchedGrammar>, RuntimeOperationError> {
        let selected_languages =
            self.resolve_languages_for_operation(requested_languages, include_all)?;
        fs::create_dir_all(source_root).map_err(|error| {
            RuntimeOperationError::grammar_source(format!(
                "failed creating grammar source root {}: {error}",
                source_root.display()
            ))
        })?;

        let fetch_plans = selected_languages
            .iter()
            .map(grammar_fetch_plan_for_language)
            .collect::<Result<Vec<_>, _>>()?;

        let crate_specs = fetch_plans
            .iter()
            .filter_map(|plan| match plan {
                GrammarFetchPlan::Crate(spec) => Some(spec.clone()),
                GrammarFetchPlan::Git(_) => None,
            })
            .collect::<Vec<_>>();

        let mut source_dirs = HashMap::new();
        let mut missing = Vec::new();
        for spec in dedupe_grammar_crate_specs(crate_specs.iter())? {
            match locate_grammar_crate_source(&spec.crate_name, spec.version.as_deref()) {
                Ok(path) => {
                    source_dirs.insert(spec.crate_name.clone(), path);
                }
                Err(_) => missing.push(spec),
            }
        }
        if !missing.is_empty() {
            let (versioned, unversioned): (Vec<_>, Vec<_>) =
                missing.into_iter().partition(|spec| spec.version.is_some());
            if !versioned.is_empty() {
                cargo_fetch_runtime_crates(&versioned)?;
                for spec in versioned {
                    let path =
                        locate_grammar_crate_source(&spec.crate_name, spec.version.as_deref())?;
                    source_dirs.insert(spec.crate_name, path);
                }
            }
            if !unversioned.is_empty() {
                cargo_fetch_locked()?;
                for spec in unversioned {
                    let path = locate_grammar_crate_source(&spec.crate_name, None)?;
                    source_dirs.insert(spec.crate_name, path);
                }
            }
        }

        let mut results = Vec::new();
        for (language, plan) in selected_languages.into_iter().zip(fetch_plans) {
            let crate_name = plan.crate_name(&language);
            let source_pin = plan.source_pin();
            let source_dir = source_root.join(plan.stage_dir_name(&language));
            if force && source_dir.exists() {
                fs::remove_dir_all(&source_dir).map_err(|error| {
                    RuntimeOperationError::grammar_source(format!(
                        "failed clearing grammar source {}: {error}",
                        source_dir.display()
                    ))
                })?;
            }
            let resolved_rev = match &plan {
                GrammarFetchPlan::Crate(spec) => {
                    if !source_dir.exists() {
                        let registry_source =
                            source_dirs.get(&spec.crate_name).ok_or_else(|| {
                                RuntimeOperationError::grammar_source(format!(
                                    "grammar crate source for `{}` not found in cargo registry",
                                    spec.crate_name
                                ))
                            })?;
                        copy_dir_recursive(registry_source, &source_dir).map_err(|error| {
                            RuntimeOperationError::grammar_source(format!(
                                "failed copying grammar source from {} to {}: {error}",
                                registry_source.display(),
                                source_dir.display()
                            ))
                        })?;
                    }
                    None
                }
                GrammarFetchPlan::Git(spec) => {
                    Some(fetch_git_grammar_source(language.canonical_id(), spec, &source_dir)?)
                }
            };
            results.push(RuntimeFetchedGrammar {
                language_id: language.canonical_id().to_string(),
                crate_name,
                source_pin,
                resolved_rev,
                source_dir,
            });
        }

        Ok(results)
    }

    pub fn build_runtime_assets(
        &self,
        requested_languages: &[String],
        include_all: bool,
        source_root: &Path,
        output_root: &Path,
        force: bool,
        skip_load: bool,
    ) -> Result<Vec<RuntimeBuiltGrammar>, RuntimeOperationError> {
        let selected_languages =
            self.resolve_languages_for_operation(requested_languages, include_all)?;
        let fetched =
            self.fetch_grammar_sources(requested_languages, include_all, source_root, false)?;
        let fetched_by_language = fetched
            .into_iter()
            .map(|grammar| (grammar.language_id.clone(), grammar))
            .collect::<HashMap<_, _>>();

        let grammar_dir = output_root.join(GRAMMARS_DIR_NAME);
        fs::create_dir_all(&grammar_dir).map_err(|error| {
            RuntimeOperationError::runtime_asset(format!(
                "failed creating grammar output dir {}: {error}",
                grammar_dir.display()
            ))
        })?;
        let builder = Loader::with_parser_lib_path(grammar_dir.clone());

        let mut built = Vec::new();
        for language in selected_languages {
            let crate_name = language.grammar_library_name().ok_or_else(|| {
                RuntimeOperationError::config_merge(format!(
                    "language `{}` has no configured grammar package",
                    language.canonical_id()
                ))
            })?;
            let fetched = fetched_by_language.get(language.canonical_id()).ok_or_else(|| {
                RuntimeOperationError::grammar_source(format!(
                    "no fetched grammar source staged for `{}`",
                    language.canonical_id()
                ))
            })?;
            let grammar_path = grammar_dir.join(shared_library_filename(crate_name));
            if force && grammar_path.exists() {
                fs::remove_file(&grammar_path).map_err(|error| {
                    RuntimeOperationError::runtime_asset(format!(
                        "failed clearing grammar asset {}: {error}",
                        grammar_path.display()
                    ))
                })?;
            }
            let build_source_dir =
                resolve_staged_grammar_build_dir(&fetched.source_dir, &language)?;
            compile_runtime_grammar(
                &builder,
                &build_source_dir,
                &grammar_path,
                skip_load,
                language.canonical_id(),
            )?;
            if !skip_load {
                validate_built_grammar_symbol(&grammar_path, &language)?;
            }
            let query_paths =
                copy_standard_queries_to_runtime(&fetched.source_dir, output_root, &language)
                    .map_err(|error| {
                        RuntimeOperationError::runtime_asset(format!(
                            "failed copying queries for `{}`: {error}",
                            language.canonical_id()
                        ))
                    })?;
            built.push(RuntimeBuiltGrammar {
                language_id: language.canonical_id().to_string(),
                source_pin: fetched.source_pin.clone(),
                resolved_rev: fetched.resolved_rev.clone(),
                grammar_path,
                query_paths,
            });
        }

        Ok(built)
    }

    fn load_language_for_canonical_id(
        &mut self,
        canonical_id: &str,
    ) -> Result<GrammarHandle, RuntimeLoaderError> {
        let normalized = normalize_lookup_key(canonical_id);
        let Some(language) = self.languages.get(canonical_id) else {
            return Err(RuntimeLoaderError::UnknownLanguage {
                requested: canonical_id.to_string(),
            });
        };

        if let Some(reason) = runtime_loading_disabled_reason() {
            return self
                .preloaded_grammars
                .get(&normalized)
                .cloned()
                .ok_or(RuntimeLoaderError::RuntimeDisabled { reason });
        }

        if let Some(library_path) = language.grammar_library_path(&self.runtime_roots) {
            if library_path.exists() {
                let canonical_library_path = canonicalize_or_original(library_path.clone());
                self.ensure_library_within_runtime_roots(&canonical_library_path)?;
                if let Some(handle) = self
                    .grammar_cache
                    .get(&canonical_library_path)
                    .filter(|handle| grammar_handle_is_fresh(handle))
                {
                    return Ok(handle.clone());
                }
                self.grammar_cache.remove(&canonical_library_path);
                let symbol_name = language
                    .grammar_symbol_name()
                    .map(str::to_owned)
                    .unwrap_or_else(|| default_symbol_name(language.grammar_id()));
                let loaded = Loader::load_language(&canonical_library_path, &symbol_name)?;
                let handle =
                    GrammarHandle::from_loaded(loaded, canonical_library_path.clone(), symbol_name);
                self.grammar_cache.insert(canonical_library_path, handle.clone());
                return Ok(handle);
            }
        }

        self.preloaded_grammars.get(&normalized).cloned().ok_or_else(|| {
            RuntimeLoaderError::MissingGrammar {
                language_id: canonical_id.to_string(),
                path: language.grammar_library_path(&self.runtime_roots),
            }
        })
    }

    fn ensure_library_within_runtime_roots(&self, path: &Path) -> Result<(), RuntimeLoaderError> {
        let allowed_roots = [
            self.runtime_roots.grammar_dir_for(RuntimeConfigSource::Bundled),
            self.runtime_roots.grammar_dir_for(RuntimeConfigSource::User),
            self.workspace_runtime_root().map(|root| root.join(GRAMMARS_DIR_NAME)),
        ]
        .into_iter()
        .flatten()
        .map(canonicalize_or_original)
        .collect::<Vec<_>>();

        if allowed_roots.iter().any(|root| path.starts_with(root)) {
            Ok(())
        } else {
            Err(RuntimeLoaderError::GrammarOutsideRuntimeRoot {
                path: path.to_path_buf(),
                allowed_roots,
            })
        }
    }

    fn discover_upstream_standard_query_paths(&self) -> HashMap<String, RuntimeStandardQueryPaths> {
        let parser_lib_path = self
            .runtime_roots
            .grammar_dir_for(RuntimeConfigSource::User)
            .unwrap_or_else(|| self.runtime_roots.user_root().join(GRAMMARS_DIR_NAME));
        let mut loader = Loader::with_parser_lib_path(parser_lib_path);
        let _ = loader.find_all_languages(&self.loader_config);

        let mut query_paths = HashMap::new();
        for (configuration, _) in loader.get_all_language_configurations() {
            let paths = RuntimeStandardQueryPaths::from_loader_configuration(configuration);
            if paths.is_empty() {
                continue;
            }
            query_paths.insert(normalize_lookup_key(&configuration.language_name), paths);
        }
        query_paths
    }

    fn resolve_query_source_uncached(
        &self,
        canonical_id: &str,
        kind: RuntimeQueryKind,
        stack: &mut Vec<String>,
    ) -> Result<Option<ResolvedQuerySource>, RuntimeLoaderError> {
        let Some(language) = self.languages.get(canonical_id) else {
            return Err(RuntimeLoaderError::UnknownLanguage {
                requested: canonical_id.to_string(),
            });
        };

        let visit_key = format!("{}:{:?}", language.canonical_id(), kind);
        if stack.iter().any(|entry| entry == &visit_key) {
            let mut chain = stack.clone();
            chain.push(visit_key);
            return Err(RuntimeLoaderError::QueryInheritanceCycle { kind, chain });
        }
        stack.push(visit_key);

        let mut source = String::new();
        let mut paths = Vec::new();
        let mut ranges = Vec::new();

        for path in self.query_source_paths(language, kind) {
            let content = fs::read_to_string(&path)
                .map_err(|error| RuntimeLoaderError::QueryIo { kind, path: path.clone(), error })?;
            let inherited = inherited_languages(&content);
            for inherited_language in inherited {
                let parent_canonical = self
                    .language_for_name(&inherited_language)
                    .map(|language| language.canonical_id().to_string())
                    .ok_or_else(|| RuntimeLoaderError::UnknownInheritedLanguage {
                        kind,
                        language: inherited_language.clone(),
                    })?;
                if let Some((parent_source, parent_paths, parent_ranges)) =
                    self.resolve_query_source_uncached(&parent_canonical, kind, stack)?
                {
                    let offset = source.len();
                    source.push_str(&parent_source);
                    paths.extend(parent_paths);
                    ranges.extend(
                        parent_ranges.into_iter().map(|(path, range)| {
                            (path, (range.start + offset)..(range.end + offset))
                        }),
                    );
                }
            }
            let start = source.len();
            source.push_str(&content);
            if !content.ends_with('\n') {
                source.push('\n');
            }
            let end = source.len();
            paths.push(path.clone());
            ranges.push((path, start..end));
        }

        stack.pop();

        if paths.is_empty() { Ok(None) } else { Ok(Some((source, paths, ranges))) }
    }

    fn query_overlay_paths(
        &self,
        language: &RuntimeLanguage,
        kind: RuntimeQueryKind,
    ) -> Vec<PathBuf> {
        [
            self.runtime_roots
                .query_dir_for(RuntimeConfigSource::Bundled, language.query_language()),
            self.runtime_roots.query_dir_for(RuntimeConfigSource::User, language.query_language()),
            self.workspace_runtime_root()
                .map(|root| root.join(QUERIES_DIR_NAME).join(language.query_language())),
        ]
        .into_iter()
        .flatten()
        .map(|dir| dir.join(kind.file_name()))
        .filter(|path| path.exists())
        .collect()
    }

    fn query_source_paths(
        &self,
        language: &RuntimeLanguage,
        kind: RuntimeQueryKind,
    ) -> Vec<PathBuf> {
        let overlay_paths = self.query_overlay_paths(language, kind);
        if !overlay_paths.is_empty() {
            return overlay_paths;
        }

        language
            .standard_query_paths(kind)
            .into_iter()
            .flatten()
            .filter(|path| path.exists())
            .cloned()
            .collect()
    }

    fn detect_language_with_source(
        &self,
        file_path: Option<&Path>,
        first_line: Option<&str>,
        content: Option<&str>,
    ) -> Option<(&RuntimeLanguage, RuntimeLanguageDetectionSource)> {
        let ordered_languages = self.ordered_languages();

        if let Some(first_line) = first_line {
            if let Some(language) = ordered_languages.iter().copied().find(|language| {
                language.shebangs().iter().any(|marker| shebang_matches(marker, first_line))
            }) {
                return Some((language, RuntimeLanguageDetectionSource::Shebang));
            }
        }

        if let Some(path) = file_path {
            if let Some(language) = ordered_languages
                .iter()
                .copied()
                .find(|language| language.globs().iter().any(|glob| path_matches_glob(path, glob)))
            {
                return Some((language, RuntimeLanguageDetectionSource::Glob));
            }
        }

        if let Some(path) = file_path.and_then(|path| self.language_for_path(path)) {
            return Some((path, RuntimeLanguageDetectionSource::FileType));
        }

        if let Some(first_line) = first_line {
            if let Some(language) = ordered_languages.iter().copied().find(|language| {
                language
                    .first_line_regex
                    .as_deref()
                    .is_some_and(|pattern| regex_matches(pattern, first_line))
            }) {
                return Some((language, RuntimeLanguageDetectionSource::FirstLineRegex));
            }
        }

        if let Some(content) = content {
            if let Some(language) = ordered_languages.iter().copied().find(|language| {
                language
                    .content_regex
                    .as_deref()
                    .is_some_and(|pattern| regex_matches(pattern, content))
            }) {
                return Some((language, RuntimeLanguageDetectionSource::ContentRegex));
            }
        }

        None
    }

    fn ordered_languages(&self) -> Vec<&RuntimeLanguage> {
        let mut languages = self.languages.values().collect::<Vec<_>>();
        languages.sort_by(|left, right| {
            right
                .match_priority()
                .cmp(&left.match_priority())
                .then_with(|| left.canonical_id().cmp(right.canonical_id()))
        });
        languages
    }

    fn resolve_languages_for_operation(
        &self,
        requested_languages: &[String],
        include_all: bool,
    ) -> Result<Vec<RuntimeLanguage>, RuntimeOperationError> {
        if include_all {
            let mut languages = self
                .languages()
                .filter(|language| language.grammar_library_name().is_some())
                .filter(|language| self.language_allowed_for_operation(language).is_ok())
                .cloned()
                .collect::<Vec<_>>();
            languages.sort_by(|left, right| left.canonical_id().cmp(right.canonical_id()));
            return Ok(languages);
        }

        if requested_languages.is_empty() {
            return Err(RuntimeOperationError::config_merge(
                "pass --all or at least one --language for runtime operations",
            ));
        }

        let mut resolved = BTreeMap::new();
        for requested in requested_languages {
            let language = self.language_for_name(requested).ok_or_else(|| {
                RuntimeOperationError::config_merge(format!(
                    "unknown runtime language `{requested}`"
                ))
            })?;
            self.language_allowed_for_operation(language)?;
            resolved.insert(language.canonical_id().to_string(), language.clone());
        }
        Ok(resolved.into_values().collect())
    }

    fn workspace_runtime_root(&self) -> Option<&Path> {
        self.workspace_runtime_trusted.then_some(()).and(self.runtime_roots.workspace_root())
    }

    fn language_allowed_for_operation(
        &self,
        language: &RuntimeLanguage,
    ) -> Result<(), RuntimeOperationError> {
        if language.asset_source() == RuntimeConfigSource::Workspace
            && self.workspace_runtime_root().is_none()
        {
            let plan = grammar_fetch_plan_for_language(language)?;
            return Err(RuntimeOperationError::grammar_source(format!(
                "{} requires trusted workspace runtime config",
                plan.diagnostic_summary(language.canonical_id())
            )));
        }
        Ok(())
    }

    fn index_language_aliases(
        &self,
        language: &RuntimeLanguage,
        alias_index: &mut HashMap<String, String>,
    ) -> Result<(), RuntimeLoaderError> {
        let mut keys = vec![language.display_name.clone(), language.canonical_id.clone()];
        keys.extend(language.aliases.iter().cloned());
        for key in keys {
            let normalized = normalize_lookup_key(&key);
            match alias_index.get(&normalized) {
                Some(existing) if existing != language.canonical_id() => {
                    return Err(RuntimeLoaderError::AmbiguousAlias {
                        alias: key,
                        first_language: existing.clone(),
                        second_language: language.canonical_id.clone(),
                    });
                }
                Some(_) => {}
                None => {
                    alias_index.insert(normalized, language.canonical_id.clone());
                }
            }
        }
        Ok(())
    }

    fn index_language_file_types(
        &self,
        language: &RuntimeLanguage,
        file_type_index: &mut HashMap<String, FileTypeOwner>,
    ) -> Result<(), RuntimeLoaderError> {
        for file_type in &language.file_types {
            let normalized = file_type.to_ascii_lowercase();
            match file_type_index.get(&normalized) {
                Some(existing)
                    if existing.canonical_id != language.canonical_id
                        && existing.priority == language.match_priority =>
                {
                    return Err(RuntimeLoaderError::AmbiguousFileType {
                        file_type: normalized,
                        first_language: existing.canonical_id.clone(),
                        second_language: language.canonical_id.clone(),
                    });
                }
                Some(existing) if existing.priority > language.match_priority => {}
                _ => {
                    file_type_index.insert(
                        normalized,
                        FileTypeOwner {
                            canonical_id: language.canonical_id.clone(),
                            priority: language.match_priority,
                        },
                    );
                }
            }
        }
        Ok(())
    }
}

fn lookup_runtime_language_config<'a>(
    overrides: &'a RuntimeLanguageOverrides,
    language_id: &str,
) -> Option<&'a RuntimeLanguageConfig> {
    overrides.get(language_id).or_else(|| {
        overrides.iter().find_map(|(candidate, config)| {
            (normalize_lookup_key(candidate) == language_id).then_some(config)
        })
    })
}

fn apply_runtime_language_config(
    language: Option<RuntimeLanguage>,
    language_id: &str,
    config: &RuntimeLanguageConfig,
    source: RuntimeConfigSource,
) -> Option<RuntimeLanguage> {
    if config.enabled == Some(false) {
        return None;
    }

    let mut language = language.unwrap_or_else(|| RuntimeLanguage::new_config_only(language_id));
    language.apply_config(language_id, config, source);
    Some(language)
}

fn validate_runtime_grammar_source(
    language_id: &str,
    source: &RuntimeGrammarSource,
) -> Result<(), String> {
    match source {
        RuntimeGrammarSource::Crate(source) => {
            if source.name.trim().is_empty() {
                return Err(format!(
                    "runtime language `{language_id}` has empty grammar.source.crate.name"
                ));
            }
            if source.version.trim().is_empty() {
                return Err(format!(
                    "runtime language `{language_id}` has empty grammar.source.crate.version"
                ));
            }
        }
        RuntimeGrammarSource::Git(source) => {
            if source.url.trim().is_empty() {
                return Err(format!(
                    "runtime language `{language_id}` has empty grammar.source.git.url"
                ));
            }
            let ref_count = usize::from(source.branch.is_some())
                + usize::from(source.tag.is_some())
                + usize::from(source.rev.is_some());
            if ref_count != 1 {
                return Err(format!(
                    "runtime language `{language_id}` must set exactly one of grammar.source.git.branch, tag, or rev"
                ));
            }
        }
    }
    Ok(())
}

fn normalize_lookup_key(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|ch| !matches!(ch, ' ' | '_' | '-'))
        .flat_map(char::to_lowercase)
        .collect()
}

fn metadata_modified_time(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|metadata| metadata.modified()).ok()
}

fn current_source_mtimes(paths: &[PathBuf]) -> Vec<Option<SystemTime>> {
    paths.iter().map(|path| metadata_modified_time(path)).collect()
}

fn query_artifact_is_fresh(entry: &QueryArtifactCacheEntry) -> bool {
    current_source_mtimes(&entry.source_paths) == entry.source_mtimes
}

fn grammar_handle_is_fresh(handle: &GrammarHandle) -> bool {
    metadata_modified_time(handle.canonical_library_path()) == handle.modified_time()
}

fn canonicalize_or_original(path: PathBuf) -> PathBuf {
    path.canonicalize().unwrap_or(path)
}

fn shared_library_filename(stem: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{stem}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{stem}.dylib")
    } else {
        format!("lib{stem}.so")
    }
}

fn compile_runtime_grammar(
    builder: &Loader,
    build_source_dir: &Path,
    grammar_path: &Path,
    skip_load: bool,
    canonical_id: &str,
) -> Result<(), RuntimeOperationError> {
    if skip_load {
        compile_parser_shared_library(build_source_dir, grammar_path).map_err(|error| {
            RuntimeOperationError::grammar_source(format!(
                "failed building grammar `{canonical_id}` from {}: {error}",
                build_source_dir.display()
            ))
        })
    } else {
        builder.compile_parser_at_path(build_source_dir, grammar_path.to_path_buf(), &[]).map_err(
            |error| {
                RuntimeOperationError::grammar_source(format!(
                    "failed building grammar `{canonical_id}` from {}: {error}",
                    build_source_dir.display()
                ))
            },
        )
    }
}

fn validate_built_grammar_symbol(
    grammar_path: &Path,
    language: &RuntimeLanguage,
) -> Result<(), RuntimeOperationError> {
    let symbol_name = language
        .grammar_symbol_name()
        .map(str::to_owned)
        .unwrap_or_else(|| default_symbol_name(language.grammar_id()));
    Loader::load_language(grammar_path, &symbol_name).map_err(|error| {
        RuntimeOperationError::runtime_asset(format!(
            "failed validating built grammar `{}` at {} with symbol `{symbol_name}`: {error}",
            language.canonical_id(),
            grammar_path.display()
        ))
    })?;
    Ok(())
}

fn compile_parser_shared_library(
    grammar_path: &Path,
    output_path: &Path,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let src_path = grammar_path.join("src");
    let parser_path = src_path.join("parser.c");
    if !parser_path.exists() {
        return Err(format!("missing parser source {}", parser_path.display()).into());
    }

    let mut cc_config = cc::Build::new();
    let host_triple = effective_host_triple()?;
    let target_triple = effective_target_triple(&host_triple)?;
    cc_config
        .cargo_metadata(false)
        .cargo_warnings(false)
        .debug(false)
        .opt_level(2)
        .extra_warnings(false)
        .host(&host_triple)
        .target(&target_triple)
        .file(&parser_path)
        .include(&src_path)
        .std("c11");

    let scanner_path = src_path.join("scanner.c");
    if scanner_path.exists() {
        cc_config.file(&scanner_path);
    }

    let compiler = cc_config.get_compiler();
    let mut command = Command::new(compiler.path());
    command.args(compiler.args());
    for (key, value) in compiler.env() {
        command.env(key, value);
    }

    if compiler.is_like_msvc() {
        command.arg(if cfg!(debug_assertions) { "-LDd" } else { "-LD" });
        command.arg("-utf-8");
    } else {
        command.arg("-Werror=implicit-function-declaration");
        if cfg!(any(target_os = "macos", target_os = "ios")) {
            command.arg("-dynamiclib");
            command.arg("-UTREE_SITTER_REUSE_ALLOCATOR");
        } else {
            command.arg("-shared");
            command.arg("-Wl,--no-undefined");
            #[cfg(target_os = "openbsd")]
            command.arg("-lc");
        }
    }

    command.args(cc_config.get_files());
    command.arg("-o").arg(output_path);

    let output = command.output().map_err(|error| {
        format!(
            "failed starting compiler `{}` for {}: {error}",
            compiler.path().display(),
            output_path.display()
        )
    })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "compiler exited with status {} while building {}:\nstdout:\n{}\nstderr:\n{}",
            output.status,
            output_path.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .into())
    }
}

fn effective_host_triple() -> Result<String, Box<dyn Error + Send + Sync>> {
    env::var("HOST")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(Ok)
        .unwrap_or_else(detect_rustc_host_triple)
}

fn effective_target_triple(host_triple: &str) -> Result<String, Box<dyn Error + Send + Sync>> {
    Ok(env::var("TARGET")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| host_triple.to_string()))
}

fn detect_rustc_host_triple() -> Result<String, Box<dyn Error + Send + Sync>> {
    let rustc = env::var("RUSTC").unwrap_or_else(|_| String::from("rustc"));
    let output = Command::new(&rustc)
        .arg("-vV")
        .output()
        .map_err(|error| format!("failed starting `{rustc} -vV` to detect host target: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "`{rustc} -vV` exited with status {} while detecting host target:\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }

    let stdout = String::from_utf8(output.stdout)
        .map_err(|error| format!("`{rustc} -vV` emitted non-utf8 output: {error}"))?;
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("`{rustc} -vV` did not report host target").into())
}

fn regex_matches(pattern: &str, text: &str) -> bool {
    Regex::new(pattern).ok().is_some_and(|regex| regex.is_match(text))
}

fn path_matches_glob(path: &Path, glob: &str) -> bool {
    let Ok(glob) = Glob::new(glob) else {
        return false;
    };
    let matcher = glob.compile_matcher();
    matcher.is_match(path)
        || path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| matcher.is_match(name))
}

fn shebang_matches(marker: &str, first_line: &str) -> bool {
    if marker.starts_with("#!") {
        first_line.starts_with(marker)
    } else {
        first_line.contains(marker)
    }
}

fn default_symbol_name(grammar_id: &str) -> String {
    format!("tree_sitter_{}", grammar_id.replace('-', "_"))
}

fn inherited_languages(query: &str) -> Vec<String> {
    let mut inherited = Vec::new();
    for line in query.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("; inherits:") {
            inherited.extend(
                rest.split([',', ' '])
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
            );
            continue;
        }
        if !trimmed.starts_with(';') {
            break;
        }
    }
    inherited
}

fn cargo_fetch_locked() -> Result<(), RuntimeOperationError> {
    let cargo = env::var("CARGO").unwrap_or_else(|_| String::from("cargo"));
    let mut command = Command::new(cargo);
    command.arg("fetch").arg("--locked");
    if let Some(root) = workspace_root_from_current_dir() {
        command.current_dir(root);
    }
    let status = command.status().map_err(|error| {
        RuntimeOperationError::grammar_source(format!(
            "failed starting `cargo fetch --locked`: {error}"
        ))
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(RuntimeOperationError::grammar_source(format!(
            "`cargo fetch --locked` exited with status {status}"
        )))
    }
}

fn workspace_root_from_current_dir() -> Option<PathBuf> {
    let mut current = env::current_dir().ok()?;
    loop {
        if current.join("Cargo.toml").exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn dedupe_grammar_crate_specs<'a>(
    specs: impl IntoIterator<Item = &'a GrammarCrateSpec>,
) -> Result<Vec<GrammarCrateSpec>, RuntimeOperationError> {
    let mut deduped = BTreeMap::<String, Option<String>>::new();
    for spec in specs {
        match deduped.entry(spec.crate_name.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(spec.version.clone());
            }
            std::collections::btree_map::Entry::Occupied(entry) if entry.get() == &spec.version => {
            }
            std::collections::btree_map::Entry::Occupied(entry) => {
                return Err(RuntimeOperationError::config_merge(format!(
                    "grammar crate `{}` requested with conflicting versions `{}` and `{}`",
                    spec.crate_name,
                    entry.get().as_deref().unwrap_or("<unspecified>"),
                    spec.version.as_deref().unwrap_or("<unspecified>")
                )));
            }
        }
    }
    Ok(deduped
        .into_iter()
        .map(|(crate_name, version)| GrammarCrateSpec { crate_name, version })
        .collect())
}

fn locate_grammar_crate_source(
    crate_name: &str,
    version: Option<&str>,
) -> Result<PathBuf, RuntimeOperationError> {
    let registry_root = cargo_registry_src_root()?;
    let prefix = format!("{crate_name}-");
    let mut best_match: Option<(Version, PathBuf)> = None;
    for registry in fs::read_dir(&registry_root).map_err(|error| {
        RuntimeOperationError::grammar_source(format!(
            "failed reading cargo registry source root {}: {error}",
            registry_root.display()
        ))
    })? {
        let registry = registry.map_err(|error| {
            RuntimeOperationError::grammar_source(format!(
                "failed reading cargo registry entry under {}: {error}",
                registry_root.display()
            ))
        })?;
        if !registry.path().is_dir() {
            continue;
        }
        for entry in fs::read_dir(registry.path()).map_err(|error| {
            RuntimeOperationError::grammar_source(format!(
                "failed listing cargo registry directory {}: {error}",
                registry.path().display()
            ))
        })? {
            let entry = entry.map_err(|error| {
                RuntimeOperationError::grammar_source(format!(
                    "failed reading cargo registry crate entry under {}: {error}",
                    registry.path().display()
                ))
            })?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(version_str) = name.strip_prefix(&prefix) else {
                continue;
            };
            if version.is_some_and(|requested| requested != version_str) {
                continue;
            }
            let Ok(version) = Version::parse(version_str) else {
                continue;
            };
            let candidate = path.clone();
            if !looks_like_runtime_grammar_source(&candidate) {
                continue;
            }
            match &best_match {
                Some((best_version, _)) if best_version >= &version => {}
                _ => best_match = Some((version, candidate)),
            }
        }
    }

    best_match.map(|(_, path)| path).ok_or_else(|| match version {
        Some(version) => RuntimeOperationError::grammar_source(format!(
            "cargo registry source for grammar crate `{crate_name}` version `{version}` not found"
        )),
        None => RuntimeOperationError::grammar_source(format!(
            "cargo registry source for grammar crate `{crate_name}` not found"
        )),
    })
}

fn cargo_fetch_runtime_crates(specs: &[GrammarCrateSpec]) -> Result<(), RuntimeOperationError> {
    let cargo = env::var("CARGO").unwrap_or_else(|_| String::from("cargo"));
    let manifest_dir = cargo_registry_src_root()?.join("..").join("cache").join("ee-runtime-fetch");
    fs::create_dir_all(&manifest_dir).map_err(|error| {
        RuntimeOperationError::grammar_source(format!(
            "failed creating runtime fetch manifest directory {}: {error}",
            manifest_dir.display()
        ))
    })?;
    fs::create_dir_all(manifest_dir.join("src")).map_err(|error| {
        RuntimeOperationError::grammar_source(format!(
            "failed creating runtime fetch source directory {}: {error}",
            manifest_dir.join("src").display()
        ))
    })?;
    let manifest_path = manifest_dir.join("Cargo.toml");
    let manifest = render_runtime_fetch_manifest(specs);
    fs::write(&manifest_path, manifest).map_err(|error| {
        RuntimeOperationError::grammar_source(format!(
            "failed writing runtime fetch manifest {}: {error}",
            manifest_path.display()
        ))
    })?;
    fs::write(manifest_dir.join("src").join("lib.rs"), "pub fn _ee_runtime_fetch() {}\n").map_err(
        |error| {
            RuntimeOperationError::grammar_source(format!(
                "failed writing runtime fetch stub source under {}: {error}",
                manifest_dir.display()
            ))
        },
    )?;

    let mut command = Command::new(cargo);
    command.arg("fetch").arg("--manifest-path").arg(&manifest_path);
    if let Some(root) = workspace_root_from_current_dir() {
        command.current_dir(root);
    }
    let status = command.status().map_err(|error| {
        RuntimeOperationError::grammar_source(format!(
            "failed starting `cargo fetch --manifest-path {}`: {error}",
            manifest_path.display()
        ))
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(RuntimeOperationError::grammar_source(format!(
            "`cargo fetch --manifest-path {}` exited with status {status}",
            manifest_path.display()
        )))
    }
}

fn render_runtime_fetch_manifest(specs: &[GrammarCrateSpec]) -> String {
    let mut manifest = String::from(
        "[package]\nname = \"ee-runtime-fetch\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[dependencies]\n",
    );
    for spec in specs {
        if let Some(version) = &spec.version {
            manifest.push_str(&format!("{} = \"={}\"\n", spec.crate_name, version));
        } else {
            manifest.push_str(&format!("{} = \"*\"\n", spec.crate_name));
        }
    }
    manifest
}

fn cargo_registry_src_root() -> Result<PathBuf, RuntimeOperationError> {
    let cargo_home = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".cargo")))
        .ok_or_else(|| {
            RuntimeOperationError::grammar_source(
                "unable to determine cargo home for runtime grammar sources",
            )
        })?;
    Ok(cargo_home.join("registry").join("src"))
}

fn sanitize_path_component(value: &str) -> String {
    let mut sanitized = String::new();
    let mut last_was_dash = false;
    for ch in value.trim().chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            sanitized.push(ch);
            last_was_dash = false;
        } else if !last_was_dash {
            sanitized.push('-');
            last_was_dash = true;
        }
    }
    sanitized.trim_matches('-').to_string()
}

fn redact_git_url_credentials(url: &str) -> String {
    let Some(scheme_index) = url.find("://") else {
        return url.to_string();
    };
    let authority_start = scheme_index + 3;
    let authority_end = url[authority_start..]
        .find(['/', '?', '#'])
        .map(|index| authority_start + index)
        .unwrap_or(url.len());
    let authority = &url[authority_start..authority_end];
    let Some(user_info_end) = authority.rfind('@') else {
        return url.to_string();
    };

    format!(
        "{}{}{}",
        &url[..authority_start],
        &authority[user_info_end + 1..],
        &url[authority_end..]
    )
}

fn redact_git_command_args(args: &[String]) -> String {
    args.iter().map(|arg| redact_git_url_credentials(arg)).collect::<Vec<_>>().join(" ")
}

fn stable_hash_hex(value: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn fetch_git_grammar_source(
    language_id: &str,
    spec: &GrammarGitSpec,
    source_dir: &Path,
) -> Result<String, RuntimeOperationError> {
    let source_label = GrammarFetchPlan::Git(spec.clone()).diagnostic_summary(language_id);
    if source_dir.exists() {
        if !source_dir.join(".git").exists() {
            return Err(RuntimeOperationError::grammar_source(format!(
                "{source_label}: staged checkout {} exists but is not a git repository",
                source_dir.display()
            )));
        }
    } else {
        run_git(None, ["clone", "--no-checkout", &spec.url, &source_dir.display().to_string()])
            .map_err(|error| {
                RuntimeOperationError::grammar_source(format!("{source_label}: {error}"))
            })?;
    }

    run_git(Some(source_dir), ["remote", "set-url", "origin", &spec.url]).map_err(|error| {
        RuntimeOperationError::grammar_source(format!("{source_label}: {error}"))
    })?;
    run_git(
        Some(source_dir),
        ["fetch", "--tags", "--force", "origin", "+refs/heads/*:refs/remotes/origin/*"],
    )
    .map_err(|error| RuntimeOperationError::grammar_source(format!("{source_label}: {error}")))?;

    let resolved_rev = match (&spec.branch, &spec.tag, &spec.rev) {
        (Some(branch), None, None) => git_rev_parse(
            source_dir,
            &format!("refs/remotes/origin/{branch}^{{commit}}"),
            &format!("branch `{branch}`"),
            &source_label,
        )?,
        (None, Some(tag), None) => git_rev_parse(
            source_dir,
            &format!("refs/tags/{tag}^{{commit}}"),
            &format!("tag `{tag}`"),
            &source_label,
        )?,
        (None, None, Some(rev)) => git_rev_parse(
            source_dir,
            &format!("{rev}^{{commit}}"),
            &format!("rev `{rev}`"),
            &source_label,
        )?,
        _ => {
            return Err(RuntimeOperationError::grammar_source(format!(
                "{source_label}: must set exactly one git ref"
            )));
        }
    };

    run_git(Some(source_dir), ["checkout", "--force", &resolved_rev]).map_err(|error| {
        RuntimeOperationError::grammar_source(format!("{source_label}: {error}"))
    })?;
    Ok(resolved_rev)
}

fn git_rev_parse(
    source_dir: &Path,
    revision: &str,
    display_ref: &str,
    source_label: &str,
) -> Result<String, RuntimeOperationError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(source_dir)
        .arg("rev-parse")
        .arg("--verify")
        .arg(revision)
        .output()
        .map_err(|error| {
            RuntimeOperationError::grammar_source(format!(
                "{source_label}: failed starting `git rev-parse` in {}: {error}",
                source_dir.display(),
            ))
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(RuntimeOperationError::grammar_source(format!(
            "{source_label}: missing {display_ref} in {}{}",
            source_dir.display(),
            if stderr.is_empty() { String::new() } else { format!(": {stderr}") }
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn run_git<I, S>(current_dir: Option<&Path>, args: I) -> Result<(), RuntimeOperationError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args = args.into_iter().map(|value| value.as_ref().to_string()).collect::<Vec<_>>();
    let display_args = redact_git_command_args(&args);
    let mut command = Command::new("git");
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }
    command.args(&args);
    let output = command.output().map_err(|error| {
        RuntimeOperationError::grammar_source(format!(
            "failed starting `git {}`: {error}",
            display_args
        ))
    })?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(RuntimeOperationError::grammar_source(format!(
        "`git {}` failed{}",
        display_args,
        if stderr.is_empty() { String::new() } else { format!(": {stderr}") }
    )))
}

fn copy_dir_recursive(source: &Path, destination: &Path) -> io::Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_recursive(&source_path, &destination_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &destination_path)?;
        }
    }
    Ok(())
}

fn copy_standard_queries_to_runtime(
    source_dir: &Path,
    output_root: &Path,
    language: &RuntimeLanguage,
) -> Result<Vec<PathBuf>, RuntimeOperationError> {
    let manifest_query_paths = resolve_manifest_standard_query_paths(source_dir, language)?;
    let source_query_dir = source_dir.join("queries");
    let destination_query_dir = output_root.join(QUERIES_DIR_NAME).join(language.query_language());
    fs::create_dir_all(&destination_query_dir).map_err(|error| {
        RuntimeOperationError::runtime_asset(format!(
            "failed creating query output dir {}: {error}",
            destination_query_dir.display()
        ))
    })?;

    let mut copied = Vec::new();
    for kind in RuntimeQueryKind::STANDARD {
        let source_path = manifest_query_paths
            .for_kind(kind)
            .and_then(|paths| paths.first().cloned())
            .unwrap_or_else(|| source_query_dir.join(kind.file_name()));
        if !source_path.exists() {
            continue;
        }
        let destination_path = destination_query_dir.join(kind.file_name());
        fs::copy(&source_path, &destination_path).map_err(|error| {
            RuntimeOperationError::runtime_asset(format!(
                "failed copying query {} to {}: {error}",
                source_path.display(),
                destination_path.display()
            ))
        })?;
        copied.push(destination_path);
    }
    Ok(copied)
}

fn looks_like_runtime_grammar_source(path: &Path) -> bool {
    if path.join("tree-sitter.json").exists() || path.join("src").join("parser.c").exists() {
        return true;
    }

    fs::read_dir(path)
        .ok()
        .into_iter()
        .flat_map(|entries| entries.filter_map(Result::ok))
        .map(|entry| entry.path())
        .filter(|entry_path| entry_path.is_dir())
        .any(|entry_path| entry_path.join("src").join("parser.c").exists())
}

#[derive(Debug, Deserialize)]
struct TreeSitterPackageManifest {
    #[serde(default)]
    grammars: Vec<TreeSitterPackageGrammar>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum TreeSitterQueryPathSpec {
    Single(String),
    Multiple(Vec<String>),
}

impl TreeSitterQueryPathSpec {
    fn into_paths(self, root: &Path) -> Vec<PathBuf> {
        match self {
            Self::Single(path) => vec![root.join(path)],
            Self::Multiple(paths) => paths.into_iter().map(|path| root.join(path)).collect(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct TreeSitterPackageGrammar {
    name: String,
    path: Option<String>,
    highlights: Option<TreeSitterQueryPathSpec>,
    injections: Option<TreeSitterQueryPathSpec>,
    locals: Option<TreeSitterQueryPathSpec>,
    tags: Option<TreeSitterQueryPathSpec>,
}

fn parse_tree_sitter_manifest(
    source_dir: &Path,
) -> Result<Option<TreeSitterPackageManifest>, RuntimeOperationError> {
    let manifest_path = source_dir.join("tree-sitter.json");
    if !manifest_path.exists() {
        return Ok(None);
    }

    let manifest_text = fs::read_to_string(&manifest_path).map_err(|error| {
        RuntimeOperationError::grammar_source(format!(
            "failed reading tree-sitter manifest {}: {error}",
            manifest_path.display()
        ))
    })?;
    let manifest = serde_json::from_str(&manifest_text).map_err(|error| {
        RuntimeOperationError::grammar_source(format!(
            "failed parsing tree-sitter manifest {}: {error}",
            manifest_path.display()
        ))
    })?;
    Ok(Some(manifest))
}

fn select_manifest_grammar<'a>(
    manifest: &'a TreeSitterPackageManifest,
    language: &RuntimeLanguage,
) -> Option<&'a TreeSitterPackageGrammar> {
    if manifest.grammars.is_empty() {
        return None;
    }

    let target_names = [language.grammar_id(), language.canonical_id(), language.query_language()]
        .into_iter()
        .map(normalize_lookup_key)
        .collect::<Vec<_>>();
    manifest
        .grammars
        .iter()
        .find(|grammar| {
            target_names.iter().any(|target| *target == normalize_lookup_key(&grammar.name))
        })
        .or_else(|| manifest.grammars.first())
}

fn resolve_manifest_standard_query_paths(
    source_dir: &Path,
    language: &RuntimeLanguage,
) -> Result<RuntimeStandardQueryPaths, RuntimeOperationError> {
    let Some(manifest) = parse_tree_sitter_manifest(source_dir)? else {
        return Ok(RuntimeStandardQueryPaths::default());
    };
    let Some(grammar) = select_manifest_grammar(&manifest, language) else {
        return Ok(RuntimeStandardQueryPaths::default());
    };

    let resolve = |path: &Option<TreeSitterQueryPathSpec>| {
        path.clone()
            .map(|path| {
                path.into_paths(source_dir)
                    .into_iter()
                    .filter(|path| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| !name.trim().is_empty())
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|paths| !paths.is_empty())
    };

    Ok(RuntimeStandardQueryPaths {
        highlights: resolve(&grammar.highlights),
        injections: resolve(&grammar.injections),
        locals: resolve(&grammar.locals),
        tags: resolve(&grammar.tags),
    })
}

fn resolve_staged_grammar_build_dir(
    source_dir: &Path,
    language: &RuntimeLanguage,
) -> Result<PathBuf, RuntimeOperationError> {
    if source_dir.join("src").join("parser.c").exists() {
        return Ok(source_dir.to_path_buf());
    }

    if let Some(path) = resolve_manifest_grammar_subdir(source_dir, language)? {
        return Ok(path);
    }

    if let Some(path) = resolve_nested_grammar_subdir(source_dir, language)? {
        return Ok(path);
    }

    Err(RuntimeOperationError::grammar_source(format!(
        "failed resolving grammar source directory for `{}` under {}",
        language.canonical_id(),
        source_dir.display()
    )))
}

fn resolve_manifest_grammar_subdir(
    source_dir: &Path,
    language: &RuntimeLanguage,
) -> Result<Option<PathBuf>, RuntimeOperationError> {
    let Some(manifest) = parse_tree_sitter_manifest(source_dir)? else {
        return Ok(None);
    };
    if manifest.grammars.is_empty() {
        return Ok(Some(source_dir.to_path_buf()));
    }

    let grammar = select_manifest_grammar(&manifest, language).expect("checked non-empty grammars");

    Ok(Some(
        grammar
            .path
            .as_deref()
            .filter(|path| !path.is_empty() && *path != ".")
            .map(|path| source_dir.join(path))
            .unwrap_or_else(|| source_dir.to_path_buf()),
    ))
}

fn resolve_nested_grammar_subdir(
    source_dir: &Path,
    language: &RuntimeLanguage,
) -> Result<Option<PathBuf>, RuntimeOperationError> {
    let target_names = [language.grammar_id(), language.canonical_id(), language.query_language()]
        .into_iter()
        .map(normalize_lookup_key)
        .collect::<Vec<_>>();
    let candidates = fs::read_dir(source_dir)
        .map_err(|error| {
            RuntimeOperationError::grammar_source(format!(
                "failed listing grammar source root {}: {error}",
                source_dir.display()
            ))
        })?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && path.join("src").join("parser.c").exists())
        .collect::<Vec<_>>();

    if let Some(path) = candidates.iter().find(|path| {
        path.file_name().and_then(|name| name.to_str()).is_some_and(|name| {
            target_names.iter().any(|target| *target == normalize_lookup_key(name))
        })
    }) {
        return Ok(Some(path.clone()));
    }

    if candidates.len() == 1 {
        return Ok(candidates.into_iter().next());
    }

    Ok(None)
}

fn map_query_error(
    kind: RuntimeQueryKind,
    error: QueryError,
    ranges: &[(PathBuf, std::ops::Range<usize>)],
) -> RuntimeLoaderError {
    let file = ranges
        .iter()
        .find(|(_, range)| range.contains(&error.offset))
        .map(|(path, _)| path.clone())
        .or_else(|| ranges.last().map(|(path, _)| path.clone()));
    RuntimeLoaderError::QueryCompile { kind, file, error }
}

fn resolve_bundled_runtime_root(
    env_override: Option<&Path>,
    exe_path: Option<&Path>,
    fallback_dir: &Path,
    windows_layout: bool,
) -> PathBuf {
    if let Some(path) = env_override {
        return path.to_path_buf();
    }
    if let Some(exe) = exe_path {
        if windows_layout {
            if let Some(parent) = exe.parent() {
                return parent.join("runtime");
            }
        } else if let Some(bin_dir) = exe.parent() {
            if let Some(prefix) = bin_dir.parent() {
                return prefix.join("share").join(RUNTIME_DIR_NAME);
            }
        }
    }
    fallback_dir.join("runtime")
}

fn runtime_loading_disabled_reason() -> Option<&'static str> {
    runtime_loading_disabled_reason_for(cfg!(any(
        target_os = "linux",
        target_os = "macos",
        windows
    )))
}

fn runtime_loading_disabled_reason_for(runtime_supported: bool) -> Option<&'static str> {
    (!runtime_supported).then_some(
        "shared-library runtime grammars are only supported on Linux, macOS, and Windows",
    )
}

fn bundled_runtime_root_from_env() -> PathBuf {
    let env_override = env::var_os("EE_RUNTIME_DIR").map(PathBuf::from);
    let exe_path = env::current_exe().ok();
    let fallback_dir = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    resolve_bundled_runtime_root(
        env_override.as_deref(),
        exe_path.as_deref(),
        &fallback_dir,
        cfg!(windows),
    )
}

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
        builtin_language_definition("Bash", &["sh", "bash", ".bashrc", ".zshrc"]),
        builtin_language_definition("C", &["c", "h"]),
        builtin_language_definition("C#", &["cs"]),
        builtin_language_definition("C++", &["cc", "cpp", "cxx", "hh", "hpp", "hxx"]),
        builtin_language_definition("CSS", &["css", "less", "scss"]),
        builtin_language_definition("Elixir", &["ex", "exs"]),
        builtin_language_definition("Go", &["go"]),
        builtin_language_definition("Haskell", &["hs"]),
        builtin_language_definition("HTML", &["htm", "html", "xhtml"]),
        builtin_language_definition("Java", &["java"]),
        builtin_language_definition("JavaScript", &["cjs", "js", "jsx", "mjs"]),
        builtin_language_definition("JSON", &["json"]),
        builtin_language_definition("PHP", &["php", "phtml"]),
        builtin_language_definition("Python", &["py", "pyw"]),
        builtin_language_definition("Ruby", &["rb", "gemspec", "gemfile", "rake", "rakefile"]),
        builtin_language_definition("Rust", &["rs"]),
        builtin_language_definition("Scala", &["sc", "scala"]),
        builtin_language_definition("TypeScript", &["cts", "mts", "ts", "tsx"]),
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
        "Bash",
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
        "C",
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
        "C#",
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
        "C++",
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
        "CSS",
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
        "Elixir",
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
        "Go",
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
        "Haskell",
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
        "HTML",
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
        "Java",
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
        "JavaScript",
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
        "JSON",
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
        "PHP",
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
        "Python",
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
        "Ruby",
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
        "Rust",
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
        "Scala",
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
            supported_query_kinds: Some(standard_and_ee),
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

    (Languages::new(&definitions), overrides)
}

fn default_runtime_loader() -> RuntimeLoader {
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
    merge_runtime_language_overrides(&mut overrides, &external.user_overrides);
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

pub fn configure_default_runtime_loader_overrides(
    user_overrides: RuntimeLanguageOverrides,
    workspace_overrides: RuntimeLanguageOverrides,
    workspace_trusted: bool,
) -> Result<(), RuntimeLoaderError> {
    {
        let mut guard =
            DEFAULT_RUNTIME_LOADER_OVERRIDES.write().expect("runtime loader overrides poisoned");
        *guard = DefaultRuntimeLoaderOverrides {
            user_overrides,
            workspace_overrides,
            workspace_trusted,
        };
    }
    reload_default_runtime_loader_languages(&builtin_runtime_languages())
}

#[cfg(any(test, feature = "test-grammars"))]
pub(crate) fn ensure_default_runtime_loader_has_test_grammars() {
    with_default_runtime_loader_mut(|loader| {
        if loader.language_for_name("Rust").is_none() || loader.language_for_name("Bash").is_none()
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

    preload_test_language!("Bash", test_grammars::bash(), "tree_sitter_bash");
    preload_test_language!("C", test_grammars::c(), "tree_sitter_c");
    preload_test_language!("C#", test_grammars::c_sharp(), "tree_sitter_c_sharp");
    preload_test_language!("C++", test_grammars::cpp(), "tree_sitter_cpp");
    preload_test_language!("CSS", test_grammars::css(), "tree_sitter_css");
    preload_test_language!("Elixir", test_grammars::elixir(), "tree_sitter_elixir");
    preload_test_language!("Go", test_grammars::go(), "tree_sitter_go");
    preload_test_language!("Haskell", test_grammars::haskell(), "tree_sitter_haskell");
    preload_test_language!("HTML", test_grammars::html(), "tree_sitter_html");
    preload_test_language!("Java", test_grammars::java(), "tree_sitter_java");
    preload_test_language!("JavaScript", test_grammars::javascript(), "tree_sitter_javascript");
    preload_test_language!("JSON", test_grammars::json(), "tree_sitter_json");
    preload_test_language!("PHP", test_grammars::php(), "tree_sitter_php");
    preload_test_language!("Python", test_grammars::python(), "tree_sitter_python");
    preload_test_language!("Ruby", test_grammars::ruby(), "tree_sitter_ruby");
    preload_test_language!("Rust", test_grammars::rust(), "tree_sitter_rust");
    preload_test_language!("Scala", test_grammars::scala(), "tree_sitter_scala");
    preload_test_language!("TypeScript", test_grammars::typescript(), "tree_sitter_typescript");
}

static DEFAULT_RUNTIME_LOADER: LazyLock<RwLock<RuntimeLoader>> =
    LazyLock::new(|| RwLock::new(default_runtime_loader()));

static DEFAULT_RUNTIME_LOADER_OVERRIDES: LazyLock<RwLock<DefaultRuntimeLoaderOverrides>> =
    LazyLock::new(|| RwLock::new(DefaultRuntimeLoaderOverrides::default()));

fn default_runtime_loader_overrides() -> DefaultRuntimeLoaderOverrides {
    DEFAULT_RUNTIME_LOADER_OVERRIDES.read().expect("runtime loader overrides poisoned").clone()
}

fn merge_runtime_language_overrides(
    target: &mut RuntimeLanguageOverrides,
    updates: &RuntimeLanguageOverrides,
) {
    for (language_id, update) in updates {
        merge_runtime_language_config(target.entry(language_id.clone()).or_default(), update);
    }
}

fn merge_runtime_language_config(
    target: &mut RuntimeLanguageConfig,
    update: &RuntimeLanguageConfig,
) {
    if let Some(enabled) = update.enabled {
        target.enabled = Some(enabled);
    }
    if let Some(name) = &update.name {
        target.name = Some(name.clone());
    }
    if let Some(query_language) = &update.query_language {
        target.query_language = Some(query_language.clone());
    }
    if let Some(scope) = &update.scope {
        target.scope = Some(scope.clone());
    }
    if let Some(content_regex) = &update.content_regex {
        target.content_regex = Some(content_regex.clone());
    }
    if let Some(first_line_regex) = &update.first_line_regex {
        target.first_line_regex = Some(first_line_regex.clone());
    }
    if let Some(injection_regex) = &update.injection_regex {
        target.injection_regex = Some(injection_regex.clone());
    }
    if let Some(aliases) = &update.aliases {
        target.aliases = Some(aliases.clone());
    }
    if let Some(file_types) = &update.file_types {
        target.file_types = Some(file_types.clone());
    }
    if let Some(globs) = &update.globs {
        target.globs = Some(globs.clone());
    }
    if let Some(shebangs) = &update.shebangs {
        target.shebangs = Some(shebangs.clone());
    }
    if let Some(supported_query_kinds) = &update.supported_query_kinds {
        target.supported_query_kinds = Some(supported_query_kinds.clone());
    }
    if let Some(match_priority) = update.match_priority {
        target.match_priority = Some(match_priority);
    }
    if let Some(grammar_update) = &update.grammar {
        let grammar = target.grammar.get_or_insert_with(RuntimeGrammarConfig::default);
        if let Some(library) = &grammar_update.library {
            grammar.library = Some(library.clone());
        }
        if let Some(symbol) = &grammar_update.symbol {
            grammar.symbol = Some(symbol.clone());
        }
        if let Some(source) = &grammar_update.source {
            grammar.source = Some(source.clone());
        }
    }
    if let Some(metadata) = update.metadata {
        target.metadata = Some(metadata);
    }
    if let Some(standard_query_paths) = &update.standard_query_paths {
        target.standard_query_paths = Some(standard_query_paths.clone());
    }
}

pub fn with_default_runtime_loader<T>(f: impl FnOnce(&RuntimeLoader) -> T) -> T {
    let guard = DEFAULT_RUNTIME_LOADER.read().expect("runtime loader poisoned");
    f(&guard)
}

pub fn with_default_runtime_loader_mut<T>(f: impl FnOnce(&mut RuntimeLoader) -> T) -> T {
    let mut guard = DEFAULT_RUNTIME_LOADER.write().expect("runtime loader poisoned");
    f(&mut guard)
}

#[allow(dead_code)]
fn _loader_language_configuration_type(_: Option<&LoaderLanguageConfiguration<'_>>) {}

#[cfg(test)]
mod tests {
    use super::*;

    use std::iter;
    use std::sync::{Mutex, MutexGuard, OnceLock};

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
        let languages = Languages::new(&[language_definition("Rust", &["rs"])]);
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
                grammar: Some(runtime_grammar_config(
                    "tree-sitter-rust",
                    "tree_sitter_rust",
                    "0.0.0",
                )),
                ..RuntimeLanguageConfig::default()
            },
        );

        let mut workspace_overrides = RuntimeLanguageOverrides::new();
        workspace_overrides.insert(
            "Rust".to_string(),
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
        assert_eq!(language.canonical_id(), "Rust");
        assert_eq!(language.display_name(), "Rust");
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
        let languages = Languages::new(&[language_definition("Rust", &["rs"])]);
        let roots = RuntimeRoots::new("/bundle", "/user/ee", Some(PathBuf::from("/workspace/.ee")));
        let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();

        let mut workspace_overrides = RuntimeLanguageOverrides::new();
        workspace_overrides.insert(
            "Rust".to_string(),
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
        let languages = Languages::new(&[language_definition("Rust", &["rs"])]);
        let roots = RuntimeRoots::new("/bundle", "/user/ee", None);
        let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
        let mut overrides = RuntimeLanguageOverrides::new();
        overrides.insert(
            String::from("rust"),
            RuntimeLanguageConfig { enabled: Some(false), ..RuntimeLanguageConfig::default() },
        );

        loader.reload_merged_languages(&languages, &overrides, None).unwrap();

        assert!(loader.language_for_name("Rust").is_none());
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
        let languages = Languages::new(&[language_definition("Rust", &["rs"])]);
        let roots = RuntimeRoots::new(
            "/bundle/ee",
            "/user/ee",
            Some(PathBuf::from("/workspace/project/.ee")),
        );
        let mut loader = RuntimeLoader::new(roots.clone(), Vec::new()).unwrap();

        let mut user_overrides = RuntimeLanguageOverrides::new();
        user_overrides.insert(
            "Rust".to_string(),
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
            "Rust".to_string(),
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

        let language = loader.language_for_name("Rust").unwrap();
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
            Some(Path::new("/workspace/project/.ee/queries/Rust"))
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
            let query_dir = root.join("queries").join("Rust");
            fs::create_dir_all(&query_dir).unwrap();
            fs::write(query_dir.join("indents.scm"), text).unwrap();
        }

        let roots = RuntimeRoots::new(&bundled_root, &user_root, Some(workspace_root));
        let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
        let languages = Languages::new(&[language_definition("Rust", &["rs"])]);
        let mut overrides = RuntimeLanguageOverrides::new();
        overrides.insert(
            "Rust".to_string(),
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

        let artifact =
            loader.resolve_query_source("Rust", RuntimeQueryKind::Indents).unwrap().unwrap();
        assert_eq!(
            artifact.source_paths,
            vec![
                bundled_root.join("queries").join("Rust").join("indents.scm"),
                user_root.join("queries").join("Rust").join("indents.scm"),
                temp_dir
                    .path()
                    .join("workspace")
                    .join(".ee")
                    .join("queries")
                    .join("Rust")
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
            let query_dir = root.join("queries").join("Rust");
            fs::create_dir_all(&query_dir).unwrap();
            fs::write(query_dir.join("indents.scm"), text).unwrap();
        }

        let roots = RuntimeRoots::new(&bundled_root, &user_root, Some(workspace_root));
        let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
        let languages = Languages::new(&[language_definition("Rust", &["rs"])]);
        let mut overrides = RuntimeLanguageOverrides::new();
        overrides.insert(
            "Rust".to_string(),
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

        let artifact =
            loader.resolve_query_source("Rust", RuntimeQueryKind::Indents).unwrap().unwrap();
        assert_eq!(
            artifact.source_paths,
            vec![
                bundled_root.join("queries").join("Rust").join("indents.scm"),
                user_root.join("queries").join("Rust").join("indents.scm"),
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
                grammar: Some(runtime_grammar_config(
                    "tree-sitter-demo",
                    "tree_sitter_demo",
                    "1.2.3",
                )),
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

        let error =
            loader.resolve_languages_for_operation(&[String::from("demo")], false).unwrap_err();
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
                grammar: Some(runtime_grammar_config(
                    "tree-sitter-demo",
                    "tree_sitter_demo",
                    "1.2.3",
                )),
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

        let resolved =
            loader.resolve_languages_for_operation(&[String::from("demo")], false).unwrap();
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
        fs::write(
            nested.join("php").join("src").join("parser.c"),
            "int parser(void) { return 0; }\n",
        )
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
        fs::write(
            root.join("php").join("src").join("parser.c"),
            "int parser(void) { return 0; }\n",
        )
        .unwrap();

        let resolved =
            resolve_staged_grammar_build_dir(&root, &test_runtime_language("PHP")).unwrap();
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
        fs::write(
            root.join("tsx").join("src").join("parser.c"),
            "int parser(void) { return 0; }\n",
        )
        .unwrap();

        let resolved =
            resolve_staged_grammar_build_dir(&root, &test_runtime_language("TypeScript")).unwrap();
        assert_eq!(resolved, root.join("typescript"));
    }

    #[test]
    fn runtime_loader_rejects_ambiguous_file_type_without_priority() {
        let languages = Languages::new(&[
            language_definition("Rust", &["rs"]),
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
            language_definition("Rust", &["rs"]),
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
            language_definition("Rust", &["rs"]),
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
        assert_eq!(glob.canonical_id, "Rust");
        assert_eq!(glob.detection_source, RuntimeLanguageDetectionSource::Glob);

        let file_type = loader.detect_language(Some(Path::new("main.rs")), None, None).unwrap();
        assert_eq!(file_type.canonical_id, "Rust");
        assert_eq!(file_type.detection_source, RuntimeLanguageDetectionSource::FileType);

        let content =
            loader.detect_language(None, None, Some("fn main() { println!(\"hi\"); }")).unwrap();
        assert_eq!(content.canonical_id, "Rust");
        assert_eq!(content.detection_source, RuntimeLanguageDetectionSource::ContentRegex);
    }

    #[test]
    fn runtime_loader_matches_injection_language_by_regex_and_priority() {
        let languages = Languages::new(&[
            language_definition("JavaScript", &["js"]),
            language_definition("TypeScript", &["ts"]),
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
        assert_eq!(tsx.canonical_id, "TypeScript");

        let javascript = loader.match_injection_language("javascript").unwrap();
        assert_eq!(javascript.canonical_id, "TypeScript");

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
        let languages = Languages::new(&[language_definition("Rust", &["rs"])]);
        let mut overrides = RuntimeLanguageOverrides::new();
        overrides.insert(
            "Rust".to_string(),
            runtime_language_override("tree-sitter-rust", "tree_sitter_rust"),
        );
        loader.reload_merged_languages(&languages, &overrides, None).unwrap();

        let cached =
            GrammarHandle::from_loaded(test_grammars::rust(), &library_path, "tree_sitter_rust");
        loader.record_grammar_handle(cached.clone());

        let first = loader.load_language_for_name("Rust").unwrap();
        assert_eq!(first.canonical_library_path(), cached.canonical_library_path());

        write_until_modified(&library_path, b"changed-stub".to_vec());

        assert!(loader.cached_grammar_handle(&library_path).is_none());
        let error = loader.load_language_for_name("Rust").unwrap_err();
        assert!(matches!(error, RuntimeLoaderError::Loader(_)));
    }

    #[test]
    fn query_cache_refreshes_when_query_file_changes() {
        let temp_dir = TempDir::new().unwrap();
        let bundled_root = temp_dir.path().join("bundle");
        let query_dir = bundled_root.join(QUERIES_DIR_NAME).join("Rust");
        fs::create_dir_all(&query_dir).unwrap();
        let query_path = query_dir.join("highlights.scm");
        fs::write(&query_path, "((identifier) @old)").unwrap();

        let roots = RuntimeRoots::new(&bundled_root, temp_dir.path().join("user"), None);
        let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
        let languages = Languages::new(&[language_definition("Rust", &["rs"])]);
        let mut overrides = RuntimeLanguageOverrides::new();
        overrides.insert(
            "Rust".to_string(),
            RuntimeLanguageConfig {
                supported_query_kinds: Some(BTreeSet::from([RuntimeQueryKind::Highlights])),
                ..runtime_language_override("tree-sitter-rust", "tree_sitter_rust")
            },
        );
        loader.reload_merged_languages(&languages, &overrides, None).unwrap();
        loader.preload_language(
            "Rust",
            GrammarHandle::from_loaded(
                test_grammars::rust(),
                "__builtin__/rust",
                "tree_sitter_rust",
            ),
        );

        let first =
            loader.resolve_query_source("Rust", RuntimeQueryKind::Highlights).unwrap().unwrap();
        assert!(first.source_text.contains("@old"));

        write_until_modified(&query_path, b"((identifier) @new)".to_vec());

        let refreshed =
            loader.resolve_query_source("Rust", RuntimeQueryKind::Highlights).unwrap().unwrap();
        assert!(refreshed.source_text.contains("@new"));

        let compiled = loader.compile_query_kind("Rust", RuntimeQueryKind::Highlights).unwrap();
        assert!(compiled.unwrap().source_text.contains("@new"));
    }

    #[test]
    fn compiled_query_cache_reuses_compiled_query_until_invalidation() {
        let temp_dir = TempDir::new().unwrap();
        let bundled_root = temp_dir.path().join("bundle");
        fs::create_dir_all(bundled_root.join("queries").join("Rust")).unwrap();
        fs::write(
            bundled_root.join("queries").join("Rust").join("tags.scm"),
            "((function_item name: (identifier) @definition.function))",
        )
        .unwrap();

        let roots = RuntimeRoots::new(&bundled_root, temp_dir.path().join("user"), None);
        let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
        let languages = Languages::new(&[language_definition("Rust", &["rs"])]);
        let mut overrides = RuntimeLanguageOverrides::new();
        overrides.insert(
            "Rust".to_string(),
            RuntimeLanguageConfig {
                supported_query_kinds: Some(BTreeSet::from([RuntimeQueryKind::Tags])),
                ..runtime_language_override("tree-sitter-rust", "tree_sitter_rust")
            },
        );
        loader.reload_merged_languages(&languages, &overrides, None).unwrap();
        loader.preload_language(
            "Rust",
            GrammarHandle::from_loaded(
                test_grammars::rust(),
                "__builtin__/rust",
                "tree_sitter_rust",
            ),
        );

        let first = loader.compile_query_kind("Rust", RuntimeQueryKind::Tags).unwrap().unwrap();
        let second = loader.compile_query_kind("Rust", RuntimeQueryKind::Tags).unwrap().unwrap();
        assert!(Arc::ptr_eq(&first, &second));

        loader.invalidate_language("Rust");
        let third = loader.compile_query_kind("Rust", RuntimeQueryKind::Tags).unwrap().unwrap();
        assert!(!Arc::ptr_eq(&first, &third));
    }

    #[test]
    fn runtime_loader_bootstraps_builtin_runtime_metadata() {
        let loader = default_runtime_loader();
        let rust = loader.language_for_name("rust").unwrap();
        assert_eq!(rust.display_name(), "Rust");
        assert_eq!(rust.grammar_symbol_name(), Some("tree_sitter_rust"));
        assert_eq!(rust.metadata().line_comment, LineCommentStyle::Token("//"));
        assert!(!loader.preloaded_grammars.contains_key(&normalize_lookup_key("Rust")));
    }

    #[test]
    fn test_grammar_bootstrap_populates_default_loader_only_in_tests() {
        ensure_default_runtime_loader_has_test_grammars();
        with_default_runtime_loader(|loader| {
            assert!(loader.preloaded_grammars.contains_key(&normalize_lookup_key("Rust")));
        });
    }

    #[test]
    fn query_inheritance_merges_parent_before_child() {
        let temp_dir = TempDir::new().unwrap();
        let bundled_root = temp_dir.path().join("bundle");
        fs::create_dir_all(bundled_root.join("queries").join("Rust")).unwrap();
        fs::create_dir_all(bundled_root.join("queries").join("Base")).unwrap();
        fs::write(
            bundled_root.join("queries").join("Base").join("textobjects.scm"),
            "((identifier) @base)",
        )
        .unwrap();
        fs::write(
            bundled_root.join("queries").join("Rust").join("textobjects.scm"),
            "; inherits: Base\n((function_item) @function.outer)",
        )
        .unwrap();

        let roots = RuntimeRoots::new(&bundled_root, temp_dir.path().join("user"), None);
        let mut loader = RuntimeLoader::new(roots.clone(), Vec::new()).unwrap();
        let languages = Languages::new(&[
            language_definition("Base", &["base"]),
            language_definition("Rust", &["rs"]),
        ]);
        let mut overrides = RuntimeLanguageOverrides::new();
        overrides.insert(
            "Rust".to_string(),
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
            "Rust",
            GrammarHandle::from_loaded(
                test_grammars::rust(),
                "__builtin__/rust",
                "tree_sitter_rust",
            ),
        );
        loader.preload_language(
            "Base",
            GrammarHandle::from_loaded(
                test_grammars::rust(),
                "__builtin__/base",
                "tree_sitter_rust",
            ),
        );

        let artifact =
            loader.compile_query_kind("Rust", RuntimeQueryKind::Textobjects).unwrap().unwrap();
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
        fs::create_dir_all(bundled_root.join("queries").join("Rust")).unwrap();
        fs::create_dir_all(bundled_root.join("queries").join("Base")).unwrap();
        fs::write(
            bundled_root.join("queries").join("Rust").join("indents.scm"),
            "; inherits: Base\n((block) @indent)",
        )
        .unwrap();
        fs::write(
            bundled_root.join("queries").join("Base").join("indents.scm"),
            "; inherits: Rust\n((source_file) @indent)",
        )
        .unwrap();

        let roots = RuntimeRoots::new(&bundled_root, temp_dir.path().join("user"), None);
        let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
        let languages = Languages::new(&[
            language_definition("Base", &["base"]),
            language_definition("Rust", &["rs"]),
        ]);
        loader.reload_merged_languages(&languages, &RuntimeLanguageOverrides::new(), None).unwrap();

        let error = loader.resolve_query_source("Rust", RuntimeQueryKind::Indents).unwrap_err();
        assert!(matches!(error, RuntimeLoaderError::QueryInheritanceCycle { .. }));
    }

    #[test]
    fn syntax_queries_compile_standard_groups_together() {
        let temp_dir = TempDir::new().unwrap();
        let bundled_root = temp_dir.path().join("bundle");
        fs::create_dir_all(bundled_root.join("queries").join("Rust")).unwrap();
        fs::write(
            bundled_root.join("queries").join("Rust").join("highlights.scm"),
            "((function_item name: (identifier) @function))",
        )
        .unwrap();
        fs::write(
            bundled_root.join("queries").join("Rust").join("locals.scm"),
            "((identifier) @local.reference)",
        )
        .unwrap();

        let roots = RuntimeRoots::new(&bundled_root, temp_dir.path().join("user"), None);
        let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
        let languages = Languages::new(&[language_definition("Rust", &["rs"])]);
        let mut overrides = RuntimeLanguageOverrides::new();
        overrides.insert(
            "Rust".to_string(),
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
            "Rust",
            GrammarHandle::from_loaded(
                test_grammars::rust(),
                "__builtin__/rust",
                "tree_sitter_rust",
            ),
        );

        let syntax = loader.compile_syntax_queries("Rust").unwrap();
        assert!(syntax.combined_query.is_some());
        assert!(syntax.combined_source.contains("@function"));
        assert!(syntax.combined_source.contains("@local.reference"));
    }

    #[test]
    fn missing_optional_queries_do_not_disable_loaded_syntax_queries() {
        let temp_dir = TempDir::new().unwrap();
        let bundled_root = temp_dir.path().join("bundle");
        fs::create_dir_all(bundled_root.join(QUERIES_DIR_NAME).join("Rust")).unwrap();
        fs::write(
            bundled_root.join(QUERIES_DIR_NAME).join("Rust").join("highlights.scm"),
            "((function_item name: (identifier) @function))",
        )
        .unwrap();

        let roots = RuntimeRoots::new(&bundled_root, temp_dir.path().join("user"), None);
        let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
        let languages = Languages::new(&[language_definition("Rust", &["rs"])]);
        let mut overrides = RuntimeLanguageOverrides::new();
        overrides.insert(
            "Rust".to_string(),
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
            "Rust",
            GrammarHandle::from_loaded(
                test_grammars::rust(),
                "__builtin__/rust",
                "tree_sitter_rust",
            ),
        );

        let syntax = loader.compile_syntax_queries("Rust").unwrap();
        assert!(syntax.combined_query.is_some());
        assert!(syntax.highlights.is_some());

        let semantic = loader.compile_semantic_queries("Rust").unwrap();
        assert!(semantic.textobjects.is_none());
        assert!(semantic.tags.is_none());
        assert!(loader.compile_query_kind("Rust", RuntimeQueryKind::Indents).unwrap().is_none());
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
      "name": "Rust",
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
        fs::write(
            parser_package.join("queries").join("locals.scm"),
            "((identifier) @local.reference)",
        )
        .unwrap();
        fs::write(
            parser_package.join("queries").join("tags.scm"),
            "((function_item name: (identifier) @definition.function))",
        )
        .unwrap();

        let roots = RuntimeRoots::new(
            temp_dir.path().join("bundle-root"),
            temp_dir.path().join("user"),
            None,
        );
        let mut loader = RuntimeLoader::new(roots, vec![temp_dir.path().join("bundle")]).unwrap();
        let languages = Languages::new(&[language_definition("Rust", &["rs"])]);
        let mut overrides = RuntimeLanguageOverrides::new();
        overrides.insert(
            "Rust".to_string(),
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
            "Rust",
            GrammarHandle::from_loaded(
                test_grammars::rust(),
                "__builtin__/rust",
                "tree_sitter_rust",
            ),
        );

        let syntax = loader.compile_syntax_queries("Rust").unwrap();
        assert!(syntax.combined_source.contains("@function"));
        assert!(syntax.combined_source.contains("@local.reference"));

        let tags = loader.compile_query_kind("Rust", RuntimeQueryKind::Tags).unwrap().unwrap();
        assert!(tags.source_text.contains("@definition.function"));
        assert!(
            tags.source_paths
                .iter()
                .any(|path| path.ends_with(Path::new("queries").join("tags.scm")))
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
      "name": "Rust",
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
        fs::write(
            parser_package.join("queries").join("locals.scm"),
            "((identifier) @local.reference)",
        )
        .unwrap();
        fs::write(
            parser_package.join("queries").join("tags.scm"),
            "((function_item name: (identifier) @definition.function))",
        )
        .unwrap();
        fs::write(&highlights_path, "((function_item").unwrap();

        let roots = RuntimeRoots::new(
            temp_dir.path().join("bundle-root"),
            temp_dir.path().join("user"),
            None,
        );
        let mut loader = RuntimeLoader::new(roots, vec![temp_dir.path().join("bundle")]).unwrap();
        let languages = Languages::new(&[language_definition("Rust", &["rs"])]);
        let mut overrides = RuntimeLanguageOverrides::new();
        overrides.insert(
            "Rust".to_string(),
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
            "Rust",
            GrammarHandle::from_loaded(
                test_grammars::rust(),
                "__builtin__/rust",
                "tree_sitter_rust",
            ),
        );

        let error = loader.compile_syntax_queries("Rust").unwrap_err();
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
            language_definition("Rust", &["rs"]),
            language_definition("JSON", &["json"]),
        ]);
        let mut overrides = RuntimeLanguageOverrides::new();
        overrides.insert(
            "Rust".to_string(),
            runtime_language_override("tree-sitter-rust", "tree_sitter_rust"),
        );
        loader.reload_merged_languages(&languages, &overrides, None).unwrap();
        loader.preload_language(
            "JSON",
            GrammarHandle::from_loaded(
                test_grammars::json(),
                "__builtin__/json",
                "tree_sitter_json",
            ),
        );

        let report = loader.runtime_health_report(
            Some("Rust"),
            Some(Path::new("main.rs")),
            None,
            None,
            None,
        );
        match report.grammar_status {
            RuntimeGrammarHealth::Error(message) => {
                assert!(message.contains("tree-sitter-rust") || message.contains("rust"));
            }
            other => panic!("unexpected grammar status: {other:?}"),
        }

        assert!(matches!(
            loader.load_language_for_name("Rust"),
            Err(RuntimeLoaderError::Loader(_))
        ));
        assert!(loader.load_language_for_name("JSON").is_ok());
    }

    #[test]
    fn runtime_health_report_distinguishes_loaded_missing_and_unsupported_queries() {
        let temp_dir = TempDir::new().unwrap();
        let bundled_root = temp_dir.path().join("bundle");
        fs::create_dir_all(bundled_root.join("queries").join("Rust")).unwrap();
        fs::write(
            bundled_root.join("queries").join("Rust").join("highlights.scm"),
            "((function_item name: (identifier) @function))",
        )
        .unwrap();

        let roots = RuntimeRoots::new(&bundled_root, temp_dir.path().join("user"), None);
        let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
        let languages = Languages::new(&[language_definition("Rust", &["rs"])]);
        let mut overrides = RuntimeLanguageOverrides::new();
        overrides.insert(
            "Rust".to_string(),
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
            "Rust",
            GrammarHandle::from_loaded(
                test_grammars::rust(),
                "__builtin__/rust",
                "tree_sitter_rust",
            ),
        );

        let report = loader.runtime_health_report(
            Some("Rust"),
            Some(Path::new("main.rs")),
            None,
            None,
            None,
        );
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
            .fetch_grammar_sources(&[String::from("Rust")], false, temp_dir.path(), true)
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
        let registry_source = cargo_home
            .join("registry")
            .join("src")
            .join("test-index")
            .join("tree-sitter-demo-1.2.3");
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
                grammar: Some(runtime_grammar_config(
                    "tree-sitter-demo",
                    "tree_sitter_demo",
                    "1.2.3",
                )),
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
            &[String::from("Rust")],
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
                .any(|path| path.ends_with(Path::new("Rust").join("highlights.scm")))
        );
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
                &[String::from("Rust")],
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

    fn create_demo_git_repo(temp_dir: &TempDir) -> (PathBuf, String, String) {
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
        fs::write(repo.join("queries").join("locals.scm"), "((identifier) @local.reference)")
            .unwrap();
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
        let second_rev = git_output(&repo, &["rev-parse", "HEAD"]);

        (repo, first_rev, second_rev)
    }

    fn demo_git_loader(
        repo: &Path,
        ref_kind: &str,
        ref_value: &str,
        symbol: &str,
    ) -> RuntimeLoader {
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
        let (repo, _tag_rev, branch_rev) = create_demo_git_repo(&temp_dir);
        let loader = demo_git_loader(&repo, "branch", "master", "tree_sitter_demo");
        let source_root = temp_dir.path().join("sources");

        let fetched = loader
            .fetch_grammar_sources(&[String::from("Demo")], false, &source_root, false)
            .unwrap();
        assert_eq!(fetched[0].resolved_rev.as_deref(), Some(branch_rev.as_str()));
        assert!(fetched[0].source_pin.contains("branch:master"));

        fs::write(fetched[0].source_dir.join("cache-marker"), "keep\n").unwrap();
        let fetched_again = loader
            .fetch_grammar_sources(&[String::from("Demo")], false, &source_root, false)
            .unwrap();
        assert_eq!(fetched_again[0].resolved_rev.as_deref(), Some(branch_rev.as_str()));
        assert!(fetched_again[0].source_dir.join("cache-marker").exists());
    }

    #[test]
    fn runtime_loader_fetches_git_tag_source_with_resolved_commit() {
        let _guard = env_lock();
        let temp_dir = TempDir::new().unwrap();
        let (repo, tag_rev, _branch_rev) = create_demo_git_repo(&temp_dir);
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
        let (repo, tag_rev, _branch_rev) = create_demo_git_repo(&temp_dir);
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
        let (repo, _tag_rev, _branch_rev) = create_demo_git_repo(&temp_dir);
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
        let (repo, _tag_rev, branch_rev) = create_demo_git_repo(&temp_dir);
        let loader = demo_git_loader(&repo, "branch", "master", "tree_sitter_demo");

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
        assert!(
            built[0]
                .query_paths
                .iter()
                .any(|path| path.ends_with(Path::new("Demo").join("highlights.scm")))
        );
        assert!(
            temp_dir.path().join("runtime").join("queries").join("Demo").join("tags.scm").exists()
        );
    }

    #[test]
    fn runtime_loader_build_fails_when_git_source_missing_parser() {
        let _guard = env_lock();
        let temp_dir = TempDir::new().unwrap();
        let (repo, _tag_rev, _branch_rev) = create_demo_git_repo(&temp_dir);
        fs::remove_file(repo.join("src").join("parser.c")).unwrap();
        run_git_fixture(&repo, &["add", "-u"]);
        run_git_fixture(&repo, &["commit", "-m", "remove parser"]);
        let loader = demo_git_loader(&repo, "branch", "master", "tree_sitter_demo");

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
        let (repo, _tag_rev, _branch_rev) = create_demo_git_repo(&temp_dir);
        fs::write(repo.join("tree-sitter.json"), "{not json\n").unwrap();
        run_git_fixture(&repo, &["add", "tree-sitter.json"]);
        run_git_fixture(&repo, &["commit", "-m", "break manifest"]);
        let loader = demo_git_loader(&repo, "branch", "master", "tree_sitter_demo");

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
        let (repo, _tag_rev, _branch_rev) = create_demo_git_repo(&temp_dir);
        let loader = demo_git_loader(&repo, "branch", "master", "tree_sitter_not_demo");
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
}
