use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tree_sitter_loader::{Config as LoaderConfig, Loader};

use crate::syntax::Languages;

use super::errors::RuntimeLoaderError;
use super::grammar::{
    cargo_fetch_locked, cargo_fetch_runtime_crates, compile_runtime_grammar,
    copy_bundled_ee_owned_queries_to_runtime, copy_standard_queries_to_runtime,
    dedupe_grammar_crate_specs, fetch_git_grammar_source, grammar_fetch_plan_for_language,
    locate_grammar_crate_source, resolve_staged_grammar_build_dir, validate_built_grammar_symbol,
};
use super::helpers::{
    canonicalize_or_original, default_symbol_name, grammar_handle_is_fresh, normalize_lookup_key,
    runtime_loading_disabled_reason, runtime_query_dir_name, shared_library_filename,
};
use super::languages::apply_runtime_language_config;
use super::types::{
    CompiledQueryArtifact, FileTypeOwner, GRAMMARS_DIR_NAME, GrammarFetchPlan, GrammarHandle,
    QUERIES_DIR_NAME, QueryArtifactCacheEntry, ResolvedQuerySource, RuntimeBuiltGrammar,
    RuntimeConfigSource, RuntimeFetchedGrammar, RuntimeInjectionMatch, RuntimeLanguage,
    RuntimeLanguageDetectionSource, RuntimeLanguageMatch, RuntimeOperationError, RuntimeQueryKind,
    RuntimeRoots, RuntimeStandardQueryPaths, WorkspaceRuntimeOverrides,
};

pub struct RuntimeLoader {
    pub(crate) runtime_roots: RuntimeRoots,
    loader_config: LoaderConfig,
    loader: Loader,
    workspace_runtime_trusted: bool,
    languages: BTreeMap<String, RuntimeLanguage>,
    alias_index: HashMap<String, String>,
    file_type_index: HashMap<String, FileTypeOwner>,
    pub(crate) preloaded_grammars: HashMap<String, GrammarHandle>,
    pub(crate) grammar_cache: HashMap<PathBuf, GrammarHandle>,
    pub(crate) query_cache: HashMap<(String, RuntimeQueryKind), QueryArtifactCacheEntry>,
    pub(crate) compiled_query_cache:
        HashMap<(String, RuntimeQueryKind), Arc<CompiledQueryArtifact>>,
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
            .unwrap_or_else(|| self.runtime_roots.user_root().join("sources"))
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
                .filter(|pattern| super::helpers::regex_matches(pattern, injection_language))
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
        user_overrides: &super::types::RuntimeLanguageOverrides,
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

            if let Some(user_config) =
                super::languages::lookup_runtime_language_config(user_overrides, &language_id)
            {
                language = apply_runtime_language_config(
                    language,
                    &language_id,
                    user_config,
                    RuntimeConfigSource::User,
                );
            }

            if let Some(workspace) = workspace_overrides.filter(|workspace| workspace.trusted)
                && let Some(workspace_config) = super::languages::lookup_runtime_language_config(
                    workspace.overrides,
                    &language_id,
                )
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
        self.grammar_cache.insert(handle.canonical_library_path().to_path_buf(), handle);
    }

    pub fn cached_grammar_handle(&self, library_path: &Path) -> Option<&GrammarHandle> {
        self.grammar_cache
            .get(&canonicalize_or_original(library_path.to_path_buf()))
            .filter(|handle| grammar_handle_is_fresh(handle))
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
                        super::helpers::copy_dir_recursive(registry_source, &source_dir).map_err(
                            |error| {
                                RuntimeOperationError::grammar_source(format!(
                                    "failed copying grammar source from {} to {}: {error}",
                                    registry_source.display(),
                                    source_dir.display()
                                ))
                            },
                        )?;
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
            let mut query_paths =
                copy_standard_queries_to_runtime(&fetched.source_dir, output_root, &language)
                    .map_err(|error| {
                        RuntimeOperationError::runtime_asset(format!(
                            "failed copying queries for `{}`: {error}",
                            language.canonical_id()
                        ))
                    })?;
            query_paths.extend(
                copy_bundled_ee_owned_queries_to_runtime(output_root, &language).map_err(
                    |error| {
                        RuntimeOperationError::runtime_asset(format!(
                            "failed copying bundled ee-owned queries for `{}`: {error}",
                            language.canonical_id()
                        ))
                    },
                )?,
            );
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

    // -----------------------------------------------------------------------
    // Private methods
    // -----------------------------------------------------------------------
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

    pub(crate) fn resolve_query_source_uncached(
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
            let inherited = super::queries::inherited_languages(&content);
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
            self.workspace_runtime_root().map(|root| {
                root.join(QUERIES_DIR_NAME).join(runtime_query_dir_name(language.query_language()))
            }),
        ]
        .into_iter()
        .flatten()
        .map(|dir| dir.join(kind.file_name()))
        .filter(|path| path.exists())
        .collect()
    }

    pub(crate) fn query_source_paths(
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
                language
                    .shebangs()
                    .iter()
                    .any(|marker| super::helpers::shebang_matches(marker, first_line))
            }) {
                return Some((language, RuntimeLanguageDetectionSource::Shebang));
            }
        }

        if let Some(path) = file_path {
            if let Some(language) = ordered_languages.iter().copied().find(|language| {
                language.globs().iter().any(|glob| super::helpers::path_matches_glob(path, glob))
            }) {
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
                    .is_some_and(|pattern| super::helpers::regex_matches(pattern, first_line))
            }) {
                return Some((language, RuntimeLanguageDetectionSource::FirstLineRegex));
            }
        }

        if let Some(content) = content {
            if let Some(language) = ordered_languages.iter().copied().find(|language| {
                language
                    .content_regex
                    .as_deref()
                    .is_some_and(|pattern| super::helpers::regex_matches(pattern, content))
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

    pub(crate) fn resolve_languages_for_operation(
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
