//! Editor configuration loading for ee.
//!
//! Settings are resolved by merging layers in priority order (lowest first):
//!   1. built-in defaults
//!   2. `/etc/ee/config.toml`
//!   3. `$XDG_CONFIG_HOME/ee/config.toml` or `~/.config/ee/config.toml`
//!   4. fallback `~/.ee.toml` when XDG user config is missing
//!   5. every ancestor `.ee.toml` from outermost to innermost
//!   6. `.editorconfig` (walked up from the open file, per spec)
//!
//! Later layers override earlier ones for any key that is explicitly set.

use super::discovery::{ConfigEnvironment, ConfigLayerKind, discover_config_layers_with_env};
use super::lsp::normalize_lsp_server_ids;
use super::raw::{EeToml, parse_ee_toml};
use std::collections::BTreeMap;
use std::path::Path;

use xi_core_lib::runtime_loader::{RuntimeLanguageConfig, RuntimeLanguageOverrides};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RuntimeLanguageSettings {
    pub user_overrides: RuntimeLanguageOverrides,
    pub workspace_overrides: RuntimeLanguageOverrides,
}

#[derive(Debug, Clone, Default)]
struct RuntimeLanguageSettingsBuilder {
    user_overrides: RuntimeLanguageOverrides,
    workspace_overrides: RuntimeLanguageOverrides,
}

impl RuntimeLanguageSettingsBuilder {
    fn merge_toml(&mut self, patch: &EeToml, kind: ConfigLayerKind) {
        let target = match kind {
            ConfigLayerKind::Ancestor => &mut self.workspace_overrides,
            ConfigLayerKind::System | ConfigLayerKind::UserXdg | ConfigLayerKind::UserLegacy => {
                &mut self.user_overrides
            }
        };

        for (language_id, language_patch) in &patch.languages {
            let normalized_id = normalize_runtime_language_id(language_id);
            merge_runtime_language_patch(
                target.entry(normalized_id).or_default(),
                language_patch,
                language_id,
            );
        }
    }

    fn finalize(self) -> RuntimeLanguageSettings {
        RuntimeLanguageSettings {
            user_overrides: self.user_overrides,
            workspace_overrides: self.workspace_overrides,
        }
    }
}

pub(super) fn normalize_runtime_language_id(language_id: &str) -> String {
    language_id.trim().to_ascii_lowercase()
}

fn normalize_runtime_file_types(language_id: &str, file_types: &[String]) -> Vec<String> {
    file_types
        .iter()
        .filter_map(|file_type| {
            let normalized = file_type.trim().trim_start_matches('.').to_string();
            if normalized.is_empty() {
                eprintln!(
                    "ee: warning: invalid runtime language config for {}: empty file type ignored",
                    language_id
                );
                None
            } else {
                Some(normalized)
            }
        })
        .collect()
}

fn merge_runtime_language_patch(
    target: &mut RuntimeLanguageConfig,
    patch: &RuntimeLanguageConfig,
    language_id: &str,
) {
    if let Some(enabled) = patch.enabled {
        target.enabled = Some(enabled);
    }
    if let Some(lsp) = &patch.lsp {
        target.lsp = Some(normalize_lsp_server_ids(language_id, lsp));
    }
    if let Some(name) = &patch.name {
        target.name = Some(name.clone());
    }
    if let Some(query_language) = &patch.query_language {
        target.query_language = Some(query_language.clone());
    }
    if let Some(scope) = &patch.scope {
        target.scope = Some(scope.clone());
    }
    if let Some(content_regex) = &patch.content_regex {
        target.content_regex = Some(content_regex.clone());
    }
    if let Some(first_line_regex) = &patch.first_line_regex {
        target.first_line_regex = Some(first_line_regex.clone());
    }
    if let Some(injection_regex) = &patch.injection_regex {
        target.injection_regex = Some(injection_regex.clone());
    }
    if let Some(aliases) = &patch.aliases {
        target.aliases = Some(aliases.clone());
    }
    if let Some(file_types) = &patch.file_types {
        target.file_types = Some(normalize_runtime_file_types(language_id, file_types));
    }
    if let Some(globs) = &patch.globs {
        target.globs = Some(globs.clone());
    }
    if let Some(shebangs) = &patch.shebangs {
        target.shebangs = Some(shebangs.clone());
    }
    if let Some(supported_query_kinds) = &patch.supported_query_kinds {
        target.supported_query_kinds = Some(supported_query_kinds.clone());
    }
    if let Some(match_priority) = patch.match_priority {
        target.match_priority = Some(match_priority);
    }
    if let Some(grammar_patch) = &patch.grammar {
        let grammar = target.grammar.get_or_insert_with(Default::default);
        if let Some(library) = &grammar_patch.library {
            grammar.library = Some(library.clone());
        }
        if let Some(symbol) = &grammar_patch.symbol {
            grammar.symbol = Some(symbol.clone());
        }
        if let Some(source) = &grammar_patch.source {
            grammar.source = Some(source.clone());
        }
    }
}

pub(super) fn runtime_languages_with_env(
    file_path: Option<&Path>,
    env: &ConfigEnvironment,
) -> RuntimeLanguageSettings {
    let mut runtime_languages = RuntimeLanguageSettingsBuilder::default();

    for layer in discover_config_layers_with_env(env, file_path).layers {
        if let Some(patch) = parse_ee_toml(&layer.path) {
            runtime_languages.merge_toml(&patch, layer.kind);
        }
    }

    runtime_languages.finalize()
}

pub(super) fn runtime_languages_to_toml(
    runtime_languages: RuntimeLanguageSettings,
) -> BTreeMap<String, RuntimeLanguageConfig> {
    let mut merged = runtime_languages.user_overrides;
    for (language_id, patch) in runtime_languages.workspace_overrides {
        merge_runtime_language_patch(
            merged.entry(language_id.clone()).or_default(),
            &patch,
            &language_id,
        );
    }
    merged
}
