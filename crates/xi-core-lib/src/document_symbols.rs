//! Bounded Tree-sitter document-symbol extraction from runtime `tags.scm` queries.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tree_sitter::{
    Language, Node, ParseOptions, Parser, Query, QueryCursor, StreamingIterator, Tree,
};

use crate::plugin_rpc::SymbolItem;
use crate::runtime_loader::{
    CompiledQueryArtifact, RuntimeQueryKind, with_default_runtime_loader_mut,
};

pub const MAX_DOCUMENT_SYMBOL_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_DOCUMENT_SYMBOLS: usize = 4_096;
pub const DEFAULT_DOCUMENT_SYMBOL_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Debug)]
struct RawSymbol {
    item: SymbolItem,
    start_byte: usize,
    end_byte: usize,
    heading_level: Option<u8>,
}

/// Extracts document symbols from current buffer text using runtime `tags.scm`.
///
/// Inputs and outputs use zero-based lines and UTF-8 byte columns. Missing
/// grammars, missing or invalid queries, timeouts, and oversized documents
/// produce an empty result so callers can degrade without stale disk reads.
pub fn document_symbols_for_text(
    language_name: Option<&str>,
    file_path: Option<&Path>,
    text: &str,
) -> Vec<SymbolItem> {
    document_symbols_for_text_bounded(
        language_name,
        file_path,
        text,
        DEFAULT_DOCUMENT_SYMBOL_TIMEOUT,
        MAX_DOCUMENT_SYMBOLS,
    )
}

fn document_symbols_for_text_bounded(
    language_name: Option<&str>,
    file_path: Option<&Path>,
    text: &str,
    timeout: Duration,
    limit: usize,
) -> Vec<SymbolItem> {
    if timeout.is_zero() || limit == 0 || text.is_empty() || text.len() > MAX_DOCUMENT_SYMBOL_BYTES
    {
        return Vec::new();
    }

    #[cfg(any(test, feature = "test-grammars"))]
    crate::runtime_loader::ensure_default_runtime_loader_has_test_grammars();

    let Some((language, tags_query)) = tags_backend(language_name, file_path) else {
        return Vec::new();
    };
    let started = Instant::now();
    let Some(tree) = parse_tree(&language, text, started, timeout) else {
        return Vec::new();
    };
    let path = file_path.map(|path| path.to_string_lossy().into_owned()).unwrap_or_default();

    raw_symbols_from_query(&tags_query.query, &tree, text, &path, started, timeout, limit)
        .map(build_hierarchy)
        .unwrap_or_default()
}

fn tags_backend(
    language_name: Option<&str>,
    file_path: Option<&Path>,
) -> Option<(Language, Arc<CompiledQueryArtifact>)> {
    with_default_runtime_loader_mut(|loader| {
        let resolved_language_name =
            language_name.and_then(|name| loader.canonical_language_name(name)).or_else(|| {
                file_path.and_then(|path| {
                    loader
                        .detect_language(Some(path), None, None)
                        .map(|language| language.canonical_id)
                })
            })?;
        let tags_query =
            loader.compile_query_kind(&resolved_language_name, RuntimeQueryKind::Tags).ok()??;
        let language = loader.load_language_for_name(&resolved_language_name).ok()?.language();
        Some((language, tags_query))
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

fn raw_symbols_from_query(
    query: &Query,
    tree: &Tree,
    text: &str,
    path: &str,
    started: Instant,
    timeout: Duration,
    limit: usize,
) -> Option<Vec<RawSymbol>> {
    let capture_names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), text.as_bytes());
    let mut symbols: Vec<RawSymbol> = Vec::new();
    let mut symbol_indices: HashMap<(usize, usize, String, String), usize> = HashMap::new();

    loop {
        if started.elapsed() >= timeout {
            return None;
        }
        matches.advance();
        let Some(query_match) = matches.get() else {
            break;
        };

        let mut definition = None;
        let mut name_node = None;
        let mut heading_level = None;
        for capture in query_match.captures {
            let capture_name =
                capture_names.get(capture.index as usize).copied().unwrap_or_default();
            if let Some(kind) = capture_name.strip_prefix("definition.") {
                definition = Some((capture.node, kind));
            } else if capture_name == "name" {
                name_node = Some(capture.node);
            } else if let Some(level) = capture_name.strip_prefix("heading.") {
                heading_level = level.parse::<u8>().ok().filter(|level| (1..=6).contains(level));
            }
        }

        let Some((definition_node, kind)) = definition else {
            continue;
        };
        let Some(name_node) = name_node else {
            continue;
        };
        let Some(name) = node_text(name_node, text).map(str::trim).filter(|name| !name.is_empty())
        else {
            continue;
        };
        let raw = raw_symbol(definition_node, name_node, name, kind, path, heading_level);
        let key = (raw.start_byte, raw.end_byte, raw.item.name.clone(), raw.item.kind.clone());
        if let Some(index) = symbol_indices.get(&key).copied() {
            if symbols[index].heading_level.is_none() {
                symbols[index].heading_level = raw.heading_level;
            }
        } else if symbols.len() < limit {
            symbol_indices.insert(key, symbols.len());
            symbols.push(raw);
        }
    }

    symbols.sort_by_key(|symbol| (symbol.start_byte, Reverse(symbol.end_byte)));
    Some(symbols)
}

fn node_text<'a>(node: Node<'_>, text: &'a str) -> Option<&'a str> {
    text.get(node.byte_range())
}

fn raw_symbol(
    definition_node: Node<'_>,
    name_node: Node<'_>,
    name: &str,
    kind: &str,
    path: &str,
    heading_level: Option<u8>,
) -> RawSymbol {
    let position = name_node.start_position();
    RawSymbol {
        item: SymbolItem {
            name: name.to_owned(),
            kind: kind.to_owned(),
            path: path.to_owned(),
            line: position.row,
            column: position.column,
            children: Vec::new(),
        },
        start_byte: definition_node.start_byte(),
        end_byte: definition_node.end_byte(),
        heading_level,
    }
}

fn build_hierarchy(symbols: Vec<RawSymbol>) -> Vec<SymbolItem> {
    if symbols.is_empty() {
        return Vec::new();
    }

    let parents = if symbols.iter().any(|symbol| symbol.heading_level.is_some()) {
        heading_parents(&symbols)
    } else {
        containment_parents(&symbols)
    };
    let mut children = vec![Vec::new(); symbols.len()];
    let mut roots = Vec::new();
    for (index, parent) in parents.into_iter().enumerate() {
        if let Some(parent) = parent {
            children[parent].push(index);
        } else {
            roots.push(index);
        }
    }

    roots.into_iter().map(|index| materialize_symbol(index, &symbols, &children)).collect()
}

fn heading_parents(symbols: &[RawSymbol]) -> Vec<Option<usize>> {
    let mut parents = containment_parents(symbols);
    let mut stack: Vec<(u8, usize)> = Vec::new();

    for (index, symbol) in symbols.iter().enumerate() {
        let Some(level) = symbol.heading_level else {
            continue;
        };
        while stack.last().is_some_and(|(parent_level, _)| *parent_level >= level) {
            stack.pop();
        }
        parents[index] = stack.last().map(|(_, parent)| *parent);
        stack.push((level, index));
    }
    parents
}

fn containment_parents(symbols: &[RawSymbol]) -> Vec<Option<usize>> {
    let mut parents = vec![None; symbols.len()];
    let mut stack: Vec<usize> = Vec::new();

    for (index, symbol) in symbols.iter().enumerate() {
        while stack.last().is_some_and(|parent| {
            let candidate = &symbols[*parent];
            let strictly_contains = candidate.start_byte <= symbol.start_byte
                && candidate.end_byte >= symbol.end_byte
                && (candidate.start_byte < symbol.start_byte
                    || candidate.end_byte > symbol.end_byte);
            !strictly_contains
        }) {
            stack.pop();
        }
        parents[index] = stack.last().copied();
        stack.push(index);
    }
    parents
}

fn materialize_symbol(index: usize, symbols: &[RawSymbol], children: &[Vec<usize>]) -> SymbolItem {
    let mut item = symbols[index].item.clone();
    item.children = children[index]
        .iter()
        .copied()
        .map(|child| materialize_symbol(child, symbols, children))
        .collect();
    item
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_loader::runtime_loader_test_guard;

    fn flatten_names(symbols: &[SymbolItem], out: &mut Vec<String>) {
        for symbol in symbols {
            out.push(symbol.name.clone());
            flatten_names(&symbol.children, out);
        }
    }

    #[test]
    fn markdown_atx_and_setext_headings_nest_by_level() {
        let _guard = runtime_loader_test_guard();
        let source = "# Parent\nintro\n\n## Child\nbody\n\nDeep Setext\n-----------\n\n#### Skipped Level\n\nSibling Setext\n==============\n";

        let symbols =
            document_symbols_for_text(Some("markdown"), Some(Path::new("README.md")), source);

        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "Parent");
        assert_eq!(symbols[0].line, 0);
        assert_eq!(symbols[0].column, 2);
        assert_eq!(symbols[0].children.len(), 2);
        assert_eq!(symbols[0].children[0].name, "Child");
        assert_eq!(symbols[0].children[1].name, "Deep Setext");
        assert_eq!(symbols[0].children[1].children[0].name, "Skipped Level");
        assert_eq!(symbols[1].name, "Sibling Setext");
        assert_eq!(symbols[1].line, 11);
    }

    #[test]
    fn rust_tags_create_containment_hierarchy() {
        let _guard = runtime_loader_test_guard();
        let source = "enum Shape {\n    Circle,\n    Square,\n}\n\nfn area() {}\n";

        let symbols = document_symbols_for_text(Some("rust"), Some(Path::new("shape.rs")), source);

        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "Shape");
        assert_eq!(symbols[0].children.len(), 2);
        assert_eq!(symbols[0].children[0].name, "Circle");
        assert_eq!(symbols[0].children[1].name, "Square");
        assert_eq!(symbols[1].name, "area");
    }

    #[test]
    fn query_requires_definition_and_name_captures() {
        let language = ee_ts_test_grammars::markdown();
        let source = "# Heading\n";
        let mut parser = Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let started = Instant::now();

        let reference_query =
            Query::new(&language, "(atx_heading (inline) @name) @reference.section").unwrap();
        assert!(
            raw_symbols_from_query(
                &reference_query,
                &tree,
                source,
                "README.md",
                started,
                Duration::from_secs(1),
                10,
            )
            .unwrap()
            .is_empty()
        );

        let missing_name_query =
            Query::new(&language, "(atx_heading) @definition.section").unwrap();
        assert!(
            raw_symbols_from_query(
                &missing_name_query,
                &tree,
                source,
                "README.md",
                Instant::now(),
                Duration::from_secs(1),
                10,
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn extraction_honors_timeout_result_and_size_bounds() {
        let _guard = runtime_loader_test_guard();
        let source = "# One\n## Two\n### Three\n";

        assert!(
            document_symbols_for_text_bounded(
                Some("markdown"),
                Some(Path::new("README.md")),
                source,
                Duration::ZERO,
                10,
            )
            .is_empty()
        );

        let limited = document_symbols_for_text_bounded(
            Some("markdown"),
            Some(Path::new("README.md")),
            source,
            Duration::from_secs(1),
            2,
        );
        let mut names = Vec::new();
        flatten_names(&limited, &mut names);
        assert_eq!(names, ["One", "Two"]);

        let oversized = "x".repeat(MAX_DOCUMENT_SYMBOL_BYTES + 1);
        assert!(
            document_symbols_for_text(Some("markdown"), Some(Path::new("README.md")), &oversized)
                .is_empty()
        );
    }
}
