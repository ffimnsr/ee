use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::syntax::Languages;

use super::errors::RuntimeLoaderError;
use super::helpers::{canonicalize_or_original, normalize_lookup_key};
use super::loader::RuntimeLoader;
use super::types::{RuntimeLanguage, RuntimeLanguageOverrides, WorkspaceRuntimeOverrides};

impl RuntimeLoader {
    pub(crate) fn reload_merged_languages_and_invalidate_changed(
        &mut self,
        languages: &Languages,
        user_overrides: &RuntimeLanguageOverrides,
        workspace_overrides: Option<WorkspaceRuntimeOverrides<'_>>,
    ) -> Result<(), RuntimeLoaderError> {
        let previous = self.language_configuration();
        self.reload_merged_languages(languages, user_overrides, workspace_overrides)?;
        let current = self.language_configuration();

        let changed = previous
            .keys()
            .chain(current.keys())
            .filter(|language_id| previous.get(*language_id) != current.get(*language_id))
            .cloned()
            .collect::<BTreeSet<_>>();
        self.query_cache.clear();
        self.compiled_query_cache.clear();

        let grammar_paths = changed
            .iter()
            .flat_map(|language_id| [previous.get(language_id), current.get(language_id)])
            .flatten()
            .filter_map(|language| language.grammar_library_path(self.runtime_roots()))
            .map(canonicalize_or_original)
            .collect::<BTreeSet<PathBuf>>();
        self.grammar_cache.retain(|path, _| !grammar_paths.contains(path));
        Ok(())
    }

    fn language_configuration(&self) -> BTreeMap<String, RuntimeLanguage> {
        self.languages()
            .map(|language| (normalize_lookup_key(language.canonical_id()), language.clone()))
            .collect()
    }
}
