use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize};
use tree_sitter::{Language, Query};
use tree_sitter_loader::LanguageConfiguration as LoaderLanguageConfiguration;

use crate::tree_sitter_support::LanguageMetadata;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------
pub const RUNTIME_DIR_NAME: &str = "ee";
pub const GRAMMARS_DIR_NAME: &str = "grammars";
pub const QUERIES_DIR_NAME: &str = "queries";
pub const SOURCES_DIR_NAME: &str = "sources";

// ---------------------------------------------------------------------------
// RuntimeQueryKind
// ---------------------------------------------------------------------------
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

// ---------------------------------------------------------------------------
// IndentQueryCapture
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IndentQueryCapture {
    Indent,
    Dedent,
}

impl IndentQueryCapture {
    pub const ALL: [Self; 2] = [Self::Indent, Self::Dedent];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Indent => "indent",
            Self::Dedent => "dedent",
        }
    }

    pub fn from_capture_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|capture| capture.as_str() == name)
    }

    pub fn allowed_names() -> Vec<&'static str> {
        Self::ALL.into_iter().map(Self::as_str).collect()
    }
}

// ---------------------------------------------------------------------------
// Grammar source types
// ---------------------------------------------------------------------------
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
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("RuntimeGrammarGitSource")
    }

    fn schema_id() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("https://example.com/schemas/runtime-grammar-git-source.json")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        // Build schema from a JSON value to avoid depending on schemars internal types
        serde_json::from_value(serde_json::json!({
            "type": "object",
            "required": ["url"],
            "properties": {
                "url": { "type": "string" },
                "branch": { "type": "string" },
                "tag": { "type": "string" },
                "rev": { "type": "string" }
            },
            "oneOf": [
                { "required": ["branch"] },
                { "required": ["tag"] },
                { "required": ["rev"] }
            ]
        }))
        .expect("static schema is valid JSON")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeGrammarSource {
    Crate(RuntimeGrammarCrateSource),
    Git(RuntimeGrammarGitSource),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeGrammarConfig {
    pub library: Option<String>,
    pub symbol: Option<String>,
    pub source: Option<RuntimeGrammarSource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeConfigSource {
    Bundled,
    User,
    Workspace,
}

// ---------------------------------------------------------------------------
// RuntimeRoots
// ---------------------------------------------------------------------------
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

    pub fn user_root_for_data_dir(data_dir: &std::path::Path) -> PathBuf {
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

    pub fn bundled_root(&self) -> &std::path::Path {
        &self.bundled_root
    }

    pub fn user_root(&self) -> &std::path::Path {
        &self.user_root
    }

    pub fn workspace_root(&self) -> Option<&std::path::Path> {
        self.workspace_root.as_deref()
    }

    pub fn root_for(&self, source: RuntimeConfigSource) -> Option<&std::path::Path> {
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
        self.root_for(source).map(|root| {
            root.join(QUERIES_DIR_NAME).join(super::helpers::runtime_query_dir_name(language_id))
        })
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

// ---------------------------------------------------------------------------
// RuntimeLanguageConfig
// ---------------------------------------------------------------------------
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

// ---------------------------------------------------------------------------
// RuntimeStandardQueryPaths
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RuntimeStandardQueryPaths {
    pub highlights: Option<Vec<PathBuf>>,
    pub injections: Option<Vec<PathBuf>>,
    pub locals: Option<Vec<PathBuf>>,
    pub tags: Option<Vec<PathBuf>>,
}

impl RuntimeStandardQueryPaths {
    pub fn from_loader_configuration(configuration: &LoaderLanguageConfiguration<'_>) -> Self {
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

    pub fn is_empty(&self) -> bool {
        self.highlights.is_none()
            && self.injections.is_none()
            && self.locals.is_none()
            && self.tags.is_none()
    }

    pub fn for_kind(&self, kind: RuntimeQueryKind) -> Option<&[PathBuf]> {
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

// ---------------------------------------------------------------------------
// WorkspaceRuntimeOverrides
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Copy)]
pub struct WorkspaceRuntimeOverrides<'a> {
    pub trusted: bool,
    pub overrides: &'a RuntimeLanguageOverrides,
}

// ---------------------------------------------------------------------------
// RuntimeLanguage (struct only; impl in languages.rs)
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeLanguage {
    pub(crate) canonical_id: String,
    pub(crate) display_name: String,
    pub(crate) grammar_id: String,
    pub(crate) grammar_library_name: Option<String>,
    pub(crate) grammar_crate_version: Option<String>,
    pub(crate) grammar_symbol_name: Option<String>,
    pub(crate) grammar_source: Option<RuntimeGrammarSource>,
    pub(crate) query_language: String,
    pub(crate) scope: Option<String>,
    pub(crate) content_regex: Option<String>,
    pub(crate) first_line_regex: Option<String>,
    pub(crate) injection_regex: Option<String>,
    pub(crate) aliases: Vec<String>,
    pub(crate) file_types: Vec<String>,
    pub(crate) globs: Vec<String>,
    pub(crate) shebangs: Vec<String>,
    pub(crate) supported_query_kinds: BTreeSet<RuntimeQueryKind>,
    pub(crate) match_priority: i32,
    pub(crate) asset_source: RuntimeConfigSource,
    pub(crate) has_base_definition: bool,
    pub(crate) metadata: LanguageMetadata,
    pub(crate) standard_query_paths: RuntimeStandardQueryPaths,
}

// ---------------------------------------------------------------------------
// GrammarHandle
// ---------------------------------------------------------------------------
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
        let canonical_library_path = super::helpers::canonicalize_or_original(library_path.into());
        let modified_time = super::helpers::metadata_modified_time(&canonical_library_path);
        Self { language, canonical_library_path, modified_time, symbol_name: symbol_name.into() }
    }

    pub fn language(&self) -> Language {
        self.language.clone()
    }

    pub fn canonical_library_path(&self) -> &std::path::Path {
        &self.canonical_library_path
    }

    pub fn modified_time(&self) -> Option<SystemTime> {
        self.modified_time
    }

    pub fn symbol_name(&self) -> &str {
        &self.symbol_name
    }
}

// ---------------------------------------------------------------------------
// Query artifact types
// ---------------------------------------------------------------------------
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

pub(crate) type ResolvedQuerySource =
    (String, Vec<PathBuf>, Vec<(PathBuf, std::ops::Range<usize>)>);

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

// ---------------------------------------------------------------------------
// Detection & health types
// ---------------------------------------------------------------------------
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeLanguageQuerySummary {
    pub language_name: String,
    pub display_name: String,
    pub grammar_status: RuntimeGrammarHealth,
    pub query_reports: Vec<RuntimeQueryHealthReport>,
}

// ---------------------------------------------------------------------------
// RuntimeOperationError
// ---------------------------------------------------------------------------
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
    pub fn new(kind: RuntimeOperationErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }

    pub fn config_merge(message: impl Into<String>) -> Self {
        Self::new(RuntimeOperationErrorKind::ConfigMerge, message)
    }

    pub fn grammar_source(message: impl Into<String>) -> Self {
        Self::new(RuntimeOperationErrorKind::GrammarSource, message)
    }

    pub fn runtime_asset(message: impl Into<String>) -> Self {
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

// ---------------------------------------------------------------------------
// Fetched / Built grammar result types
// ---------------------------------------------------------------------------
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

// ---------------------------------------------------------------------------
// Grammar crate/git spec & fetch plan (struct only; impl in grammar.rs)
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GrammarCrateSpec {
    pub crate_name: String,
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrammarGitSpec {
    pub url: String,
    pub branch: Option<String>,
    pub tag: Option<String>,
    pub rev: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrammarFetchPlan {
    Crate(GrammarCrateSpec),
    Git(GrammarGitSpec),
}

// ---------------------------------------------------------------------------
// Internal helper types
// ---------------------------------------------------------------------------
#[derive(Debug, Clone)]
pub(crate) struct FileTypeOwner {
    pub canonical_id: String,
    pub priority: i32,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DefaultRuntimeLoaderOverrides {
    pub user_overrides: RuntimeLanguageOverrides,
    pub workspace_overrides: RuntimeLanguageOverrides,
    pub workspace_trusted: bool,
}
