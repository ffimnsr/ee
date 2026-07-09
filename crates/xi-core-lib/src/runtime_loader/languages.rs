use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::syntax::LanguageDefinition;
use crate::tree_sitter_support::{
    BlockCommentStyle, IndentationStrategy, LanguageMetadata, LineCommentStyle,
};

use super::errors::RuntimeLoaderError;
use super::grammar::grammar_fetch_plan_for_language;
use super::helpers::{
    normalize_lookup_key, shared_library_filename, validate_runtime_grammar_source,
};
use super::types::{
    RuntimeConfigSource, RuntimeGrammarConfig, RuntimeGrammarSource, RuntimeLanguage,
    RuntimeLanguageConfig, RuntimeLanguageOverrides, RuntimeQueryKind, RuntimeRoots,
};

impl RuntimeLanguage {
    pub(crate) fn from_definition(definition: &LanguageDefinition) -> Self {
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
            standard_query_paths: super::types::RuntimeStandardQueryPaths::default(),
        }
    }

    pub(crate) fn new_config_only(language_id: &str) -> Self {
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
            standard_query_paths: super::types::RuntimeStandardQueryPaths::default(),
        }
    }

    pub(crate) fn apply_config(
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

    pub(crate) fn validate_configured(&self) -> Result<(), RuntimeLoaderError> {
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

    pub(crate) fn standard_query_paths(&self, kind: RuntimeQueryKind) -> Option<&[PathBuf]> {
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

    pub fn staged_source_dir(&self, roots: &RuntimeRoots) -> Option<PathBuf> {
        if matches!(self.asset_source, RuntimeConfigSource::Bundled) {
            return None;
        }
        let plan = grammar_fetch_plan_for_language(self).ok()?;
        let source_root = roots.source_dir_for(self.asset_source)?;
        Some(source_root.join(plan.stage_dir_name(self)))
    }
}

pub fn lookup_runtime_language_config<'a>(
    overrides: &'a RuntimeLanguageOverrides,
    language_id: &str,
) -> Option<&'a RuntimeLanguageConfig> {
    overrides.get(language_id).or_else(|| {
        overrides.iter().find_map(|(candidate, config)| {
            (normalize_lookup_key(candidate) == language_id).then_some(config)
        })
    })
}

pub fn apply_runtime_language_config(
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

pub fn merge_runtime_language_overrides(
    target: &mut RuntimeLanguageOverrides,
    updates: &RuntimeLanguageOverrides,
) {
    for (language_id, update) in updates {
        merge_runtime_language_config(target.entry(language_id.clone()).or_default(), update);
    }
}

pub fn merge_runtime_language_config(
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
