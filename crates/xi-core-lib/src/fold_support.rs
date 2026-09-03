use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tree_sitter::{Language, ParseOptions, Parser, Query, QueryCursor, StreamingIterator, Tree};

use crate::runtime_loader::{
    CompiledQueryArtifact, RuntimeQueryKind, with_default_runtime_loader_mut,
};
use crate::tree_sitter_support::FoldRange;

pub(crate) const DEFAULT_FOLD_PARSE_TIMEOUT: Duration = Duration::from_millis(250);

/// Returns backend-authoritative fold ranges for one complete document.
///
/// Runtime `folds.scm` captures are authoritative when present. Languages
/// without a fold query retain legacy AST-kind folding.
pub(crate) fn fold_ranges_for_text(
    language_name: Option<&str>,
    file_path: Option<&Path>,
    text: &str,
    timeout: Duration,
) -> Vec<FoldRange> {
    if timeout.is_zero() || text.is_empty() {
        return Vec::new();
    }

    #[cfg(any(test, feature = "test-grammars"))]
    crate::runtime_loader::ensure_default_runtime_loader_has_test_grammars();

    let Some((language, fold_query)) = fold_backend(language_name, file_path) else {
        return Vec::new();
    };
    let Some(fold_query) = fold_query else {
        return crate::tree_sitter_support::fold_ranges_for_text(
            language_name,
            file_path,
            text,
            timeout,
        );
    };
    let started = Instant::now();
    let Some(tree) = parse_tree(&language, text, started, timeout) else {
        return Vec::new();
    };

    fold_ranges_from_query(&fold_query.query, &tree, text, started, timeout).unwrap_or_default()
}

fn fold_backend(
    language_name: Option<&str>,
    file_path: Option<&Path>,
) -> Option<(Language, Option<Arc<CompiledQueryArtifact>>)> {
    with_default_runtime_loader_mut(|loader| {
        let resolved_language_name =
            language_name.and_then(|name| loader.canonical_language_name(name)).or_else(|| {
                file_path.and_then(|path| {
                    loader
                        .detect_language(Some(path), None, None)
                        .map(|language| language.canonical_id)
                })
            })?;
        let fold_query =
            if loader.supports_query_kind(&resolved_language_name, RuntimeQueryKind::Folds) {
                loader.compile_query_kind(&resolved_language_name, RuntimeQueryKind::Folds).ok()?
            } else {
                None
            };
        let language = loader.load_language_for_name(&resolved_language_name).ok()?.language();
        Some((language, fold_query))
    })
}

fn parse_tree(
    language: &Language,
    text: &str,
    started: Instant,
    timeout: Duration,
) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(language).ok()?;

    let mut progress = |_: &tree_sitter::ParseState| {
        if started.elapsed() >= timeout {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        }
    };
    let bytes = text.as_bytes();
    let mut read = |offset: usize, _: tree_sitter::Point| bytes.get(offset..).unwrap_or_default();
    let options = ParseOptions { progress_callback: Some(&mut progress) };
    let tree = parser.parse_with_options(&mut read, None, Some(options))?;
    (started.elapsed() < timeout).then_some(tree)
}

fn fold_ranges_from_query(
    query: &Query,
    tree: &Tree,
    text: &str,
    started: Instant,
    timeout: Duration,
) -> Option<Vec<FoldRange>> {
    let fold_capture = query.capture_index_for_name("fold")?;
    let mut cursor = QueryCursor::new();
    let mut captures = cursor.captures(query, tree.root_node(), text.as_bytes());
    let mut folds = Vec::new();

    loop {
        if started.elapsed() >= timeout {
            return None;
        }
        captures.advance();
        let Some((query_match, capture_index)) = captures.get() else {
            break;
        };
        let capture = query_match.captures[*capture_index];
        if capture.index != fold_capture {
            continue;
        }

        let start = capture.node.start_position();
        let end = capture.node.end_position();
        let body_end = if end.column == 0 && end.row > start.row { end.row - 1 } else { end.row };
        if body_end > start.row {
            folds.push(FoldRange { header_line: start.row, body_start: start.row + 1, body_end });
        }
    }

    folds.sort_unstable_by_key(|fold| (fold.header_line, fold.body_end));
    folds.dedup();
    Some(folds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_loader::runtime_loader_test_guard;

    #[test]
    fn markdown_sections_fold_content_by_heading_level() {
        let _guard = runtime_loader_test_guard();
        let source = "# Parent\nparent body\n\n## Child\nchild body\n\n### Grandchild\ngrandchild body\n\n## Sibling\nsibling body\n\n# Next\nnext body\n";

        let folds = fold_ranges_for_text(
            Some("markdown"),
            Some(Path::new("README.md")),
            source,
            Duration::from_secs(1),
        );

        assert_eq!(
            folds,
            vec![
                FoldRange { header_line: 0, body_start: 1, body_end: 11 },
                FoldRange { header_line: 3, body_start: 4, body_end: 8 },
                FoldRange { header_line: 6, body_start: 7, body_end: 8 },
                FoldRange { header_line: 9, body_start: 10, body_end: 11 },
                FoldRange { header_line: 12, body_start: 13, body_end: 13 },
            ]
        );
    }

    #[test]
    fn legacy_folding_remains_available_without_fold_query() {
        let _guard = runtime_loader_test_guard();
        let folds = fold_ranges_for_text(
            Some("rust"),
            Some(Path::new("main.rs")),
            "fn main() {\n    println!(\"hello\");\n}\n",
            Duration::from_secs(1),
        );

        assert!(folds.iter().any(|fold| fold.header_line == 0 && fold.body_end >= 2));
    }
}
