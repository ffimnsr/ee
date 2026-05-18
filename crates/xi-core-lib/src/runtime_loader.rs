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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeLanguageLayer {
    pub canonical_id: Option<String>,
    pub grammar_id: Option<String>,
    pub grammar_library_name: Option<String>,
    pub grammar_crate_version: Option<String>,
    pub grammar_symbol_name: Option<String>,
    pub query_language: Option<String>,
    pub scope: Option<String>,
    pub content_regex: Option<String>,
    pub first_line_regex: Option<String>,
    pub injection_regex: Option<String>,
    pub aliases: Vec<String>,
    pub file_types: Vec<String>,
    pub globs: Vec<String>,
    pub shebangs: Vec<String>,
    pub supported_query_kinds: BTreeSet<RuntimeQueryKind>,
    pub match_priority: Option<i32>,
    #[serde(skip)]
    pub(crate) metadata: Option<LanguageMetadata>,
    #[serde(skip)]
    pub(crate) standard_query_paths: Option<RuntimeStandardQueryPaths>,
}

pub type RuntimeLanguageOverrides = BTreeMap<String, RuntimeLanguageLayer>;

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
            metadata: LanguageMetadata {
                line_comment: LineCommentStyle::Unsupported,
                block_comment: BlockCommentStyle::Unsupported,
                indentation: IndentationStrategy::Unsupported,
                unsupported_semantic_targets: &[],
            },
            standard_query_paths: RuntimeStandardQueryPaths::default(),
        }
    }

    fn apply_layer(&mut self, layer: &RuntimeLanguageLayer, source: RuntimeConfigSource) {
        if let Some(canonical_id) = &layer.canonical_id {
            self.canonical_id = canonical_id.clone();
        }
        if let Some(grammar_id) = &layer.grammar_id {
            self.grammar_id = grammar_id.clone();
            self.asset_source = source;
        }
        if let Some(grammar_library_name) = &layer.grammar_library_name {
            self.grammar_library_name = Some(grammar_library_name.clone());
            self.asset_source = source;
        }
        if let Some(grammar_crate_version) = &layer.grammar_crate_version {
            self.grammar_crate_version = Some(grammar_crate_version.clone());
        }
        if let Some(grammar_symbol_name) = &layer.grammar_symbol_name {
            self.grammar_symbol_name = Some(grammar_symbol_name.clone());
            self.asset_source = source;
        }
        if let Some(query_language) = &layer.query_language {
            self.query_language = query_language.clone();
            self.asset_source = source;
        }
        if let Some(scope) = &layer.scope {
            self.scope = Some(scope.clone());
        }
        if let Some(content_regex) = &layer.content_regex {
            self.content_regex = Some(content_regex.clone());
        }
        if let Some(first_line_regex) = &layer.first_line_regex {
            self.first_line_regex = Some(first_line_regex.clone());
        }
        if let Some(injection_regex) = &layer.injection_regex {
            self.injection_regex = Some(injection_regex.clone());
        }
        if !layer.aliases.is_empty() {
            append_unique(&mut self.aliases, &layer.aliases);
        }
        if !layer.file_types.is_empty() {
            append_unique(&mut self.file_types, &layer.file_types);
        }
        if !layer.globs.is_empty() {
            append_unique(&mut self.globs, &layer.globs);
        }
        if !layer.shebangs.is_empty() {
            append_unique(&mut self.shebangs, &layer.shebangs);
        }
        if !layer.supported_query_kinds.is_empty() {
            self.supported_query_kinds = layer.supported_query_kinds.clone();
        }
        if let Some(match_priority) = layer.match_priority {
            self.match_priority = match_priority;
        }
        if let Some(metadata) = layer.metadata {
            self.metadata = metadata;
        }
        if let Some(standard_query_paths) = &layer.standard_query_paths {
            self.standard_query_paths = standard_query_paths.clone();
        }
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
    pub file_path: Option<PathBuf>,
    pub detection_source: Option<RuntimeLanguageDetectionSource>,
    pub language_id: Option<String>,
    pub display_name: Option<String>,
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
    pub source_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeBuiltGrammar {
    pub language_id: String,
    pub grammar_path: PathBuf,
    pub query_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct GrammarCrateSpec {
    crate_name: String,
    version: Option<String>,
}

#[derive(Debug)]
pub enum RuntimeLoaderError {
    Loader(LoaderError),
    RuntimeDisabled { reason: &'static str },
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
            Self::RuntimeDisabled { .. }
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

pub struct RuntimeLoader {
    runtime_roots: RuntimeRoots,
    loader_config: LoaderConfig,
    loader: Loader,
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
        let upstream_standard_query_paths = self.discover_upstream_standard_query_paths();
        let mut merged = BTreeMap::new();
        let mut alias_index = HashMap::new();
        let mut file_type_index = HashMap::new();

        for definition in languages.iter() {
            let mut language = RuntimeLanguage::from_definition(definition);
            if let Some(user_layer) = user_overrides.get(definition.name.as_ref()) {
                language.apply_layer(user_layer, RuntimeConfigSource::User);
            }
            if let Some(workspace) = workspace_overrides {
                if workspace.trusted {
                    if let Some(workspace_layer) = workspace.overrides.get(definition.name.as_ref())
                    {
                        language.apply_layer(workspace_layer, RuntimeConfigSource::Workspace);
                    }
                }
            }
            if let Some(standard_query_paths) =
                upstream_standard_query_paths.get(&normalize_lookup_key(language.query_language()))
            {
                language.standard_query_paths = standard_query_paths.clone();
            }

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
        let handle = self.load_language_for_name(language_name)?;
        let query = Query::new(&handle.language(), &artifact.source_text)
            .map_err(|error| map_query_error(kind, error, &artifact.path_ranges))?;
        let compiled = Arc::new(CompiledQueryArtifact {
            kind,
            source_text: artifact.source_text,
            source_paths: artifact.source_paths,
            source_mtimes: artifact.source_mtimes,
            newest_mtime: artifact.newest_mtime,
            query: Arc::new(query),
        });
        self.compiled_query_cache.insert(cache_key, Arc::clone(&compiled));
        Ok(Some(compiled))
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
            file_path: file_path.map(Path::to_path_buf),
            detection_source: resolved.as_ref().map(|language| language.detection_source),
            language_id: resolved.as_ref().map(|language| language.canonical_id.clone()),
            display_name: resolved.as_ref().map(|language| language.display_name.clone()),
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

        let crate_specs = selected_languages
            .iter()
            .map(|language| {
                let crate_name = language.grammar_library_name().ok_or_else(|| {
                    RuntimeOperationError::config_merge(format!(
                        "language `{}` has no configured grammar package",
                        language.canonical_id()
                    ))
                })?;
                Ok(GrammarCrateSpec {
                    crate_name: crate_name.to_string(),
                    version: language.grammar_crate_version().map(str::to_string),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

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
        for language in selected_languages {
            let crate_name = language.grammar_library_name().expect("checked above");
            let source_dir = source_root.join(crate_name);
            if force && source_dir.exists() {
                fs::remove_dir_all(&source_dir).map_err(|error| {
                    RuntimeOperationError::grammar_source(format!(
                        "failed clearing grammar source {}: {error}",
                        source_dir.display()
                    ))
                })?;
            }
            if !source_dir.exists() {
                let registry_source = source_dirs.get(crate_name).ok_or_else(|| {
                    RuntimeOperationError::grammar_source(format!(
                        "grammar crate source for `{crate_name}` not found in cargo registry"
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
            results.push(RuntimeFetchedGrammar {
                language_id: language.canonical_id().to_string(),
                crate_name: crate_name.to_string(),
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
            self.runtime_roots.grammar_dir_for(RuntimeConfigSource::Workspace),
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
            self.runtime_roots
                .query_dir_for(RuntimeConfigSource::Workspace, language.query_language()),
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
            resolved.insert(language.canonical_id().to_string(), language.clone());
        }
        Ok(resolved.into_values().collect())
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

fn append_unique(target: &mut Vec<String>, additions: &[String]) {
    let mut seen: BTreeSet<String> =
        target.iter().map(|value| normalize_lookup_key(value)).collect();
    for addition in additions {
        let normalized = normalize_lookup_key(addition);
        if seen.insert(normalized) {
            target.push(addition.clone());
        }
    }
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
) -> io::Result<Vec<PathBuf>> {
    let source_query_dir = source_dir.join("queries");
    let destination_query_dir = output_root.join(QUERIES_DIR_NAME).join(language.query_language());
    fs::create_dir_all(&destination_query_dir)?;

    let mut copied = Vec::new();
    for kind in RuntimeQueryKind::STANDARD {
        let source_path = source_query_dir.join(kind.file_name());
        if !source_path.exists() {
            continue;
        }
        let destination_path = destination_query_dir.join(kind.file_name());
        fs::copy(&source_path, &destination_path)?;
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

#[derive(Debug, Deserialize)]
struct TreeSitterPackageGrammar {
    name: String,
    path: Option<String>,
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
    let manifest: TreeSitterPackageManifest =
        serde_json::from_str(&manifest_text).map_err(|error| {
            RuntimeOperationError::grammar_source(format!(
                "failed parsing tree-sitter manifest {}: {error}",
                manifest_path.display()
            ))
        })?;
    if manifest.grammars.is_empty() {
        return Ok(Some(source_dir.to_path_buf()));
    }

    let target_names = [language.grammar_id(), language.canonical_id(), language.query_language()]
        .into_iter()
        .map(normalize_lookup_key)
        .collect::<Vec<_>>();
    let grammar = manifest
        .grammars
        .iter()
        .find(|grammar| {
            target_names.iter().any(|target| *target == normalize_lookup_key(&grammar.name))
        })
        .or_else(|| manifest.grammars.first())
        .expect("checked non-empty grammars");

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
                $name.to_string(),
                RuntimeLanguageLayer {
                    grammar_library_name: Some($grammar.to_string()),
                    grammar_crate_version: Some($version.to_string()),
                    grammar_symbol_name: Some($symbol.to_string()),
                    aliases: $aliases.iter().map(|value| (*value).to_string()).collect(),
                    supported_query_kinds: standard_and_ee.clone(),
                    metadata: Some($metadata),
                    ..RuntimeLanguageLayer::default()
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
        "TypeScript".to_string(),
        RuntimeLanguageLayer {
            grammar_library_name: Some("tree-sitter-typescript".to_string()),
            grammar_crate_version: Some("0.23.2".to_string()),
            grammar_symbol_name: Some("tree_sitter_typescript".to_string()),
            aliases: vec![
                "ts".to_string(),
                "typescript".to_string(),
                "tsx".to_string(),
                "typescriptreact".to_string(),
            ],
            supported_query_kinds: standard_and_ee,
            metadata: Some(LanguageMetadata {
                line_comment: LineCommentStyle::Token("//"),
                block_comment: BlockCommentStyle::Tokens { open: "/*", close: "*/" },
                indentation: IndentationStrategy::TreeSitter,
                unsupported_semantic_targets: &[],
            }),
            ..RuntimeLanguageLayer::default()
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
    let (languages, overrides) = builtin_runtime_components();
    loader
        .reload_merged_languages(&languages, &overrides, None)
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
    let (_, overrides) = builtin_runtime_components();
    with_default_runtime_loader_mut(|loader| {
        loader.reload_merged_languages(languages, &overrides, None)?;
        loader.invalidate_all();
        Ok(())
    })
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
            "Rust".to_string(),
            RuntimeLanguageLayer {
                canonical_id: Some("rust".to_string()),
                grammar_library_name: Some("tree-sitter-rust".to_string()),
                aliases: vec!["rscript".to_string()],
                shebangs: vec!["#!/usr/bin/env rust-script".to_string()],
                supported_query_kinds: BTreeSet::from([
                    RuntimeQueryKind::Highlights,
                    RuntimeQueryKind::Locals,
                    RuntimeQueryKind::Indents,
                ]),
                ..RuntimeLanguageLayer::default()
            },
        );

        let mut workspace_overrides = RuntimeLanguageOverrides::new();
        workspace_overrides.insert(
            "Rust".to_string(),
            RuntimeLanguageLayer {
                file_types: vec!["rs.in".to_string()],
                globs: vec!["*.rs.in".to_string()],
                match_priority: Some(20),
                ..RuntimeLanguageLayer::default()
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
        assert_eq!(language.display_name(), "Rust");
        assert_eq!(language.grammar_library_name(), Some("tree-sitter-rust"));
        assert_eq!(language.asset_source(), RuntimeConfigSource::User);
        assert!(language.file_types().iter().any(|value| value == "rs"));
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
            RuntimeLanguageLayer {
                file_types: vec!["workspace-rs".to_string()],
                ..RuntimeLanguageLayer::default()
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
            RuntimeLanguageLayer {
                grammar_library_name: Some("tree-sitter-rust-user".to_string()),
                ..RuntimeLanguageLayer::default()
            },
        );
        let mut workspace_overrides = RuntimeLanguageOverrides::new();
        workspace_overrides.insert(
            "Rust".to_string(),
            RuntimeLanguageLayer {
                grammar_library_name: Some("tree-sitter-rust-workspace".to_string()),
                query_language: Some("rust-workspace".to_string()),
                ..RuntimeLanguageLayer::default()
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
            RuntimeLanguageLayer {
                supported_query_kinds: BTreeSet::from([RuntimeQueryKind::Indents]),
                ..RuntimeLanguageLayer::default()
            },
        );
        loader.reload_merged_languages(&languages, &overrides, None).unwrap();

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

    fn test_runtime_language(name: &str) -> RuntimeLanguage {
        RuntimeLanguage {
            canonical_id: name.to_string(),
            display_name: name.to_string(),
            grammar_id: name.to_string(),
            grammar_library_name: Some(format!("tree-sitter-{}", normalize_lookup_key(name))),
            grammar_crate_version: Some("0.0.0".to_string()),
            grammar_symbol_name: Some(format!("tree_sitter_{}", normalize_lookup_key(name))),
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
            "Reason".to_string(),
            RuntimeLanguageLayer {
                canonical_id: Some("reason".to_string()),
                match_priority: Some(10),
                ..RuntimeLanguageLayer::default()
            },
        );

        loader.reload_merged_languages(&languages, &user_overrides, None).unwrap();

        assert_eq!(
            loader.language_for_path(Path::new("main.rs")).map(RuntimeLanguage::canonical_id),
            Some("reason")
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
            "Rust".to_string(),
            RuntimeLanguageLayer {
                canonical_id: Some("rust".to_string()),
                globs: vec!["*.rs.in".to_string()],
                content_regex: Some(String::from("\\bfn\\s+main\\b")),
                match_priority: Some(20),
                ..RuntimeLanguageLayer::default()
            },
        );
        overrides.insert(
            "Shell".to_string(),
            RuntimeLanguageLayer {
                canonical_id: Some("shell".to_string()),
                shebangs: vec!["#!/usr/bin/env bash".to_string()],
                ..RuntimeLanguageLayer::default()
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
        assert_eq!(shebang.canonical_id, "shell");
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
            RuntimeLanguageLayer {
                grammar_library_name: Some("tree-sitter-rust".to_string()),
                grammar_symbol_name: Some("tree_sitter_rust".to_string()),
                ..RuntimeLanguageLayer::default()
            },
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
            RuntimeLanguageLayer {
                grammar_library_name: Some("tree-sitter-rust".to_string()),
                grammar_symbol_name: Some("tree_sitter_rust".to_string()),
                supported_query_kinds: BTreeSet::from([RuntimeQueryKind::Highlights]),
                ..RuntimeLanguageLayer::default()
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
            RuntimeLanguageLayer {
                grammar_library_name: Some("tree-sitter-rust".to_string()),
                grammar_symbol_name: Some("tree_sitter_rust".to_string()),
                supported_query_kinds: BTreeSet::from([RuntimeQueryKind::Tags]),
                ..RuntimeLanguageLayer::default()
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
            RuntimeLanguageLayer {
                grammar_library_name: Some("tree-sitter-rust".to_string()),
                grammar_symbol_name: Some("tree_sitter_rust".to_string()),
                supported_query_kinds: BTreeSet::from([RuntimeQueryKind::Textobjects]),
                ..RuntimeLanguageLayer::default()
            },
        );
        overrides.insert(
            "Base".to_string(),
            RuntimeLanguageLayer {
                grammar_library_name: Some("tree-sitter-rust".to_string()),
                grammar_symbol_name: Some("tree_sitter_rust".to_string()),
                supported_query_kinds: BTreeSet::from([RuntimeQueryKind::Textobjects]),
                ..RuntimeLanguageLayer::default()
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
            RuntimeLanguageLayer {
                grammar_library_name: Some("tree-sitter-rust".to_string()),
                grammar_symbol_name: Some("tree_sitter_rust".to_string()),
                supported_query_kinds: BTreeSet::from([
                    RuntimeQueryKind::Highlights,
                    RuntimeQueryKind::Locals,
                ]),
                ..RuntimeLanguageLayer::default()
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
            RuntimeLanguageLayer {
                grammar_library_name: Some("tree-sitter-rust".to_string()),
                grammar_symbol_name: Some("tree_sitter_rust".to_string()),
                supported_query_kinds: BTreeSet::from([
                    RuntimeQueryKind::Highlights,
                    RuntimeQueryKind::Textobjects,
                    RuntimeQueryKind::Indents,
                ]),
                ..RuntimeLanguageLayer::default()
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
            RuntimeLanguageLayer {
                grammar_library_name: Some("tree-sitter-rust".to_string()),
                grammar_symbol_name: Some("tree_sitter_rust".to_string()),
                supported_query_kinds: BTreeSet::from([
                    RuntimeQueryKind::Highlights,
                    RuntimeQueryKind::Locals,
                    RuntimeQueryKind::Tags,
                ]),
                ..RuntimeLanguageLayer::default()
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
            RuntimeLanguageLayer {
                grammar_library_name: Some("tree-sitter-rust".to_string()),
                grammar_symbol_name: Some("tree_sitter_rust".to_string()),
                supported_query_kinds: BTreeSet::from([
                    RuntimeQueryKind::Highlights,
                    RuntimeQueryKind::Locals,
                    RuntimeQueryKind::Tags,
                ]),
                ..RuntimeLanguageLayer::default()
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
            RuntimeLanguageLayer {
                grammar_library_name: Some("tree-sitter-rust".to_string()),
                grammar_symbol_name: Some("tree_sitter_rust".to_string()),
                ..RuntimeLanguageLayer::default()
            },
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

        let report =
            loader.runtime_health_report(Some("Rust"), Some(Path::new("main.rs")), None, None);
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
            RuntimeLanguageLayer {
                grammar_library_name: Some("tree-sitter-rust".to_string()),
                grammar_symbol_name: Some("tree_sitter_rust".to_string()),
                supported_query_kinds: BTreeSet::from([
                    RuntimeQueryKind::Highlights,
                    RuntimeQueryKind::Indents,
                ]),
                ..RuntimeLanguageLayer::default()
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

        let report =
            loader.runtime_health_report(Some("Rust"), Some(Path::new("main.rs")), None, None);
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
            RuntimeLanguageLayer {
                grammar_library_name: Some("tree-sitter-demo".to_string()),
                grammar_crate_version: Some("1.2.3".to_string()),
                ..RuntimeLanguageLayer::default()
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
}
