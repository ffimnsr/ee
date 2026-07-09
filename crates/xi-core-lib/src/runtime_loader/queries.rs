use std::path::{Path, PathBuf};
use std::sync::Arc;

use tree_sitter::Query;

use super::errors::RuntimeLoaderError;
use super::helpers::{canonicalize_or_original, current_source_mtimes};
use super::loader::RuntimeLoader;
use super::types::{
    CompiledQueryArtifact, IndentQueryCapture, QueryArtifactCacheEntry, RuntimeGrammarHealth,
    RuntimeHealthReport, RuntimeLanguageDetectionSource, RuntimeLanguageMatch,
    RuntimeLanguageQuerySummary, RuntimeQueryHealth, RuntimeQueryHealthReport, RuntimeQueryKind,
    SemanticQuerySet, SyntaxQuerySet,
};

pub fn map_query_error(
    kind: RuntimeQueryKind,
    error: tree_sitter::QueryError,
    ranges: &[(PathBuf, std::ops::Range<usize>)],
) -> RuntimeLoaderError {
    let file = ranges
        .iter()
        .find(|(_, range)| range.contains(&error.offset))
        .map(|(path, _)| path.clone())
        .or_else(|| ranges.last().map(|(path, _)| path.clone()));
    RuntimeLoaderError::QueryCompile { kind, file, error }
}

pub fn validate_query_contract(
    kind: RuntimeQueryKind,
    source_paths: &[PathBuf],
    query: &Query,
) -> Result<(), RuntimeLoaderError> {
    match kind {
        RuntimeQueryKind::Indents => validate_indent_query_contract(source_paths, query),
        _ => Ok(()),
    }
}

pub fn validate_indent_query_contract(
    source_paths: &[PathBuf],
    query: &Query,
) -> Result<(), RuntimeLoaderError> {
    for capture in query.capture_names() {
        if IndentQueryCapture::from_capture_name(capture).is_none() {
            return Err(RuntimeLoaderError::InvalidQueryCapture {
                kind: RuntimeQueryKind::Indents,
                file: source_paths.first().cloned(),
                capture: capture.to_string(),
                allowed: IndentQueryCapture::allowed_names(),
            });
        }
    }

    Ok(())
}

pub fn inherited_languages(query: &str) -> Vec<String> {
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

pub fn query_artifact_is_fresh(entry: &QueryArtifactCacheEntry) -> bool {
    current_source_mtimes(&entry.source_paths) == entry.source_mtimes
}

impl RuntimeLoader {
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

    pub fn resolve_indent_query_source(
        &mut self,
        language_name: &str,
    ) -> Result<Option<&QueryArtifactCacheEntry>, RuntimeLoaderError> {
        self.resolve_query_source(language_name, RuntimeQueryKind::Indents)
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

    pub fn compile_indent_query(
        &mut self,
        language_name: &str,
    ) -> Result<Option<Arc<CompiledQueryArtifact>>, RuntimeLoaderError> {
        self.compile_query_kind(language_name, RuntimeQueryKind::Indents)
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
            match self.resolve_query_source_uncached(&canonical_id, kind, &mut Vec::new())? {
                Some((source_text, source_paths, path_ranges)) => {
                    let source_mtimes = current_source_mtimes(&source_paths);
                    let newest_mtime = source_mtimes.iter().flatten().copied().max();
                    Some(QueryArtifactCacheEntry {
                        language_id: canonical_id.clone(),
                        kind,
                        source_text,
                        source_paths,
                        source_mtimes,
                        path_ranges,
                        newest_mtime,
                    })
                }
                None => None,
            }
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
        validate_query_contract(kind, &artifact.source_paths, &query)?;
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

    /// Report query health for every registered language.
    pub fn language_query_diagnostics(&mut self) -> Vec<RuntimeLanguageQuerySummary> {
        let language_names: Vec<String> =
            self.languages().map(|l| l.canonical_id().to_string()).collect();

        let mut results = Vec::with_capacity(language_names.len());
        for name in language_names {
            // Clone language before mutating self below
            let Some(language) = self.language_for_name(&name).cloned() else {
                continue;
            };

            let grammar_status = match self.load_language_for_name(&name) {
                Ok(_) => RuntimeGrammarHealth::Loaded,
                Err(RuntimeLoaderError::MissingGrammar { .. }) => RuntimeGrammarHealth::Missing,
                Err(error) => RuntimeGrammarHealth::Error(error.to_string()),
            };

            let mut query_reports = Vec::new();
            for kind in RuntimeQueryKind::STANDARD.into_iter().chain(RuntimeQueryKind::EE_OWNED) {
                if !language.supported_query_kinds().contains(&kind) {
                    query_reports.push(RuntimeQueryHealthReport {
                        kind,
                        status: RuntimeQueryHealth::Unsupported,
                        source_paths: Vec::new(),
                    });
                    continue;
                }

                match self.resolve_query_source(&name, kind).map(|artifact| artifact.cloned()) {
                    Ok(Some(artifact)) => {
                        let status = match self.compile_query_kind(&name, kind) {
                            Ok(Some(_)) => RuntimeQueryHealth::Loaded,
                            Ok(None) => RuntimeQueryHealth::Missing,
                            Err(error) => RuntimeQueryHealth::Error(error.to_string()),
                        };
                        query_reports.push(RuntimeQueryHealthReport {
                            kind,
                            status,
                            source_paths: artifact.source_paths,
                        });
                    }
                    Ok(None) => {
                        query_reports.push(RuntimeQueryHealthReport {
                            kind,
                            status: RuntimeQueryHealth::Missing,
                            source_paths: self.query_source_paths(&language, kind),
                        });
                    }
                    Err(error) => {
                        query_reports.push(RuntimeQueryHealthReport {
                            kind,
                            status: RuntimeQueryHealth::Error(error.to_string()),
                            source_paths: Vec::new(),
                        });
                    }
                }
            }

            results.push(RuntimeLanguageQuerySummary {
                language_name: language.canonical_id().to_string(),
                display_name: language.display_name().to_string(),
                grammar_status,
                query_reports,
            });
        }

        results
    }
}
