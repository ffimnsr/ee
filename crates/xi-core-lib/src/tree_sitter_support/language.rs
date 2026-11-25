use super::*;

use super::parse::{QueryWarmup, resolve_runtime_language_name, runtime_language};

// ---------------------------------------------------------------------------
// Language registry
// ---------------------------------------------------------------------------

/// Returns canonical xi language name for a requested alias.
pub fn canonical_language_name(requested: &str) -> Option<String> {
    #[cfg(any(test, feature = "test-grammars"))]
    crate::runtime_loader::ensure_default_runtime_loader_has_test_grammars();

    with_default_runtime_loader(|loader| loader.canonical_language_name(requested))
}

/// Returns canonical xi language name resolved from a path or file name.
pub fn language_name_for_path(path: &Path) -> Option<String> {
    #[cfg(any(test, feature = "test-grammars"))]
    crate::runtime_loader::ensure_default_runtime_loader_has_test_grammars();

    with_default_runtime_loader(|loader| {
        loader.detect_language(Some(path), None, None).map(|language| language.canonical_id)
    })
}

pub(crate) fn language_metadata_for_name(language_name: &str) -> Option<LanguageMetadata> {
    #[cfg(any(test, feature = "test-grammars"))]
    crate::runtime_loader::ensure_default_runtime_loader_has_test_grammars();

    with_default_runtime_loader(|loader| {
        loader.language_for_name(language_name).map(|language| language.metadata())
    })
}

pub(crate) fn syntax_feature_availability(
    language_name: Option<&str>,
    file_path: Option<&Path>,
    mode: DocumentMode,
) -> SyntaxFeatureAvailability {
    #[cfg(any(test, feature = "test-grammars"))]
    crate::runtime_loader::ensure_default_runtime_loader_has_test_grammars();

    with_default_runtime_loader_mut(|loader| {
        let gates = mode.feature_gates();
        let Some(resolved_language_name) =
            resolve_runtime_language_name(loader, language_name, file_path)
        else {
            return SyntaxFeatureAvailability {
                syntax_spans: false,
                semantic_motions: false,
                line_comments: false,
                block_comments: false,
                reindent: false,
            };
        };
        let Some(language) = loader.language_for_name(&resolved_language_name) else {
            return SyntaxFeatureAvailability {
                syntax_spans: false,
                semantic_motions: false,
                line_comments: false,
                block_comments: false,
                reindent: false,
            };
        };
        let metadata = language.metadata();
        let has_grammar = loader.load_language_for_name(&resolved_language_name).is_ok();
        let has_semantic_target = [
            SemanticTargetKind::Function,
            SemanticTargetKind::Class,
            SemanticTargetKind::Parameter,
            SemanticTargetKind::Comment,
            SemanticTargetKind::Test,
        ]
        .iter()
        .any(|target| !metadata.unsupported_semantic_targets.contains(target));

        SyntaxFeatureAvailability {
            syntax_spans: gates.syntax
                && has_grammar
                && loader.supports_any_query_kind(
                    &resolved_language_name,
                    &[
                        RuntimeQueryKind::Highlights,
                        RuntimeQueryKind::Injections,
                        RuntimeQueryKind::Locals,
                    ],
                ),
            semantic_motions: gates.syntax
                && has_grammar
                && has_semantic_target
                && loader
                    .supports_query_kind(&resolved_language_name, RuntimeQueryKind::Textobjects),
            line_comments: gates.editing
                && matches!(metadata.line_comment, LineCommentStyle::Token(_)),
            block_comments: gates.editing
                && matches!(metadata.block_comment, BlockCommentStyle::Tokens { .. }),
            reindent: gates.whole_doc_ops
                && has_grammar
                && matches!(metadata.indentation, IndentationStrategy::TreeSitter)
                && loader.supports_query_kind(&resolved_language_name, RuntimeQueryKind::Indents),
        }
    })
}

pub(crate) fn language_supports_semantic_target(
    language_name: &str,
    target: SemanticTargetKind,
) -> bool {
    #[cfg(any(test, feature = "test-grammars"))]
    crate::runtime_loader::ensure_default_runtime_loader_has_test_grammars();

    with_default_runtime_loader(|loader| {
        loader.language_for_name(language_name).is_some_and(|language| {
            !language.metadata().unsupported_semantic_targets.contains(&target)
                && loader.supports_query_kind(language_name, RuntimeQueryKind::Textobjects)
        })
    })
}

pub(crate) fn resolve_ts_language(
    language_name: Option<&str>,
    file_path: Option<&Path>,
) -> Option<Language> {
    runtime_language(language_name, file_path, QueryWarmup::None)
}

#[allow(dead_code)]
pub(crate) fn ts_language_for_path(path: &Path) -> Option<Language> {
    runtime_language(None, Some(path), QueryWarmup::None)
}
