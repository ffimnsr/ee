//! Edit-ops tests: harness (loader guard and config fixtures).
use super::{
    align_it, align_selections, delete_backward, expand_tabs_in_lines, insert_newline,
    insert_newline_with_context, insert_preserving_case, reflow_lines, reverse_selection_contents,
    rotate_selection_contents, sort_lines, transpose,
};
use crate::config::BufferItems;
use crate::indent::SyntaxIndentContext;
use crate::runtime_loader::{
    RuntimeLanguageConfig, RuntimeLanguageOverrides, configure_default_runtime_loader_overrides,
    ensure_default_runtime_loader_has_test_grammars, with_default_runtime_loader_mut,
};
use crate::selection::SelRegion;
use crate::text_store::DocumentMode;
use std::collections::BTreeSet;
use std::sync::MutexGuard;
use xi_rope::Rope;

fn runtime_loader_test_guard() -> MutexGuard<'static, ()> {
    crate::runtime_loader::runtime_loader_test_guard()
}

struct RuntimeLoaderOverrideGuard;

impl RuntimeLoaderOverrideGuard {
    fn install(languages: &[&str]) -> Self {
        Self::install_with_query_language(languages, None)
    }

    fn install_with_query_language(languages: &[&str], query_language: Option<&str>) -> Self {
        let mut overrides = RuntimeLanguageOverrides::new();
        for language in languages {
            overrides.insert(
                (*language).to_string(),
                RuntimeLanguageConfig {
                    supported_query_kinds: Some(BTreeSet::from([
                        crate::runtime_loader::RuntimeQueryKind::Indents,
                    ])),
                    query_language: query_language.map(str::to_string),
                    ..RuntimeLanguageConfig::default()
                },
            );
        }
        configure_default_runtime_loader_overrides(
            overrides,
            RuntimeLanguageOverrides::new(),
            false,
        )
        .expect("configure runtime loader overrides");
        ensure_default_runtime_loader_has_test_grammars();
        Self
    }
}

impl Drop for RuntimeLoaderOverrideGuard {
    fn drop(&mut self) {
        let _ = configure_default_runtime_loader_overrides(
            RuntimeLanguageOverrides::new(),
            RuntimeLanguageOverrides::new(),
            false,
        );
        ensure_default_runtime_loader_has_test_grammars();
        with_default_runtime_loader_mut(|loader| {
            for language in ["rust", "json", "python"] {
                loader.invalidate_language(language);
            }
        });
    }
}

fn install_indent_query(language: &str, query: &str) {
    with_default_runtime_loader_mut(|loader| {
        loader.invalidate_language(language);
        loader.record_query_artifact(
            language,
            crate::runtime_loader::RuntimeQueryKind::Indents,
            query.to_string(),
            Vec::new(),
            Vec::new(),
        );
    });
}

fn test_config() -> BufferItems {
    BufferItems {
        line_ending: "\n".to_owned(),
        tab_size: 4,
        translate_tabs_to_spaces: false,
        use_tab_stops: true,
        font_face: String::new(),
        font_size: 12.0,
        auto_indent: true,
        smart_indent: true,
        scroll_past_end: false,
        wrap_width: 0,
        word_wrap: false,
        autodetect_whitespace: false,
        surrounding_pairs: Vec::new(),
        save_with_newline: false,
    }
}

mod align_tests;
mod basic_edit_tests;
mod newline_tests;
mod transform_tests;
