//! Bounded Tree-sitter symbol dependency analysis for editor snapshots.
//!
//! This module owns dependency facts derived from open editor buffers. It never
//! falls back to disk, regex, or an LSP, so callers cannot receive stale or
//! semantically stronger results than this syntax-only index can prove.

use std::time::{Duration, Instant};

use serde::Serialize;
use tree_sitter::{Node, ParseOptions, Parser, Point};

use crate::text_store::{
    ByteOffset, DocumentMode, FullTextPolicy, LogicalLine, TextChunkResult, TextStore, Utf16Lookup,
    Utf16Offset,
};
use crate::tree_sitter_support::resolve_ts_language;

const MAX_SOURCE_BYTES: usize = 2 * 1024 * 1024;
const MAX_RELATIONS: usize = 64;
const PARSE_TIMEOUT: Duration = Duration::from_millis(50);
const GRAPH_VERSION: &str = "tree-sitter-symbol-map-v1";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolDependencyMap {
    pub path: String,
    pub line: u32,
    pub character: u32,
    pub symbol: SymbolLocation,
    pub definition: SymbolLocation,
    pub callers: Vec<SymbolRelation>,
    pub callees: Vec<SymbolRelation>,
    pub implementations: Vec<SymbolRelation>,
    pub tests: Vec<SymbolLocation>,
    pub module_hints: Vec<ModuleHint>,
    pub related_files: Vec<RelatedFile>,
    pub totals: SymbolDependencyTotals,
    pub truncated: bool,
    pub freshness: String,
    pub graph_version: String,
    pub indexed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolLocation {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub line: u32,
    pub character: u32,
    pub end_line: u32,
    pub end_character: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolRelation {
    pub symbol: SymbolLocation,
    pub relation: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModuleHint {
    pub name: String,
    pub kind: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelatedFile {
    pub path: String,
    pub relation: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolDependencyTotals {
    pub callers: u32,
    pub callees: u32,
    pub implementations: u32,
    pub tests: u32,
    pub module_hints: u32,
    pub related_files: u32,
    pub omitted_callers: u32,
    pub omitted_callees: u32,
    pub omitted_implementations: u32,
    pub omitted_tests: u32,
    pub omitted_module_hints: u32,
    pub omitted_related_files: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolIndexError {
    Unavailable(&'static str),
    Stale(&'static str),
}

impl SymbolIndexError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unavailable(_) => "dependency_index_unavailable",
            Self::Stale(_) => "dependency_index_stale",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Unavailable(reason) => format!("{}: {reason}", self.code()),
            Self::Stale(reason) => format!("{}: {reason}", self.code()),
        }
    }
}

pub fn symbol_dependency_map(
    store: &dyn TextStore,
    revision: u64,
    path: String,
    line: u32,
    character: u32,
    language_id: &str,
) -> Result<SymbolDependencyMap, SymbolIndexError> {
    if store.mode() == DocumentMode::Vlf || store.full_text_policy() != FullTextPolicy::Allowed {
        return Err(SymbolIndexError::Unavailable("full document text is unavailable"));
    }
    if line == 0 {
        return Err(SymbolIndexError::Unavailable("line must be one-based"));
    }
    if store.len_bytes() > MAX_SOURCE_BYTES as u64 {
        return Err(SymbolIndexError::Unavailable("source exceeds dependency-index byte budget"));
    }
    // Dependency queries are deliberately narrow until each language defines a
    // complete query contract. Rust has a stable grammar shape used below.
    if language_id != "rust" {
        return Err(SymbolIndexError::Unavailable("language has no dependency query"));
    }
    let TextChunkResult::Ready(chunk) = store.read_full_text() else {
        return Err(SymbolIndexError::Unavailable("source snapshot is unavailable"));
    };
    let source = chunk.text;
    let start = match store.line_to_byte(LogicalLine(u64::from(line - 1))) {
        crate::text_store::LineLookup::Exact(offset) => offset,
        _ => return Err(SymbolIndexError::Unavailable("line is unavailable")),
    };
    let Some(global_utf16) =
        store.byte_to_utf16(start).and_then(|offset| offset.0.checked_add(u64::from(character)))
    else {
        return Err(SymbolIndexError::Unavailable("position is outside UTF-16 range"));
    };
    let byte = match store.utf16_to_byte(Utf16Offset(global_utf16)) {
        Utf16Lookup::Exact(offset)
            if store.byte_to_line(offset) == Some(LogicalLine(u64::from(line - 1))) =>
        {
            offset.0 as usize
        }
        _ => {
            return Err(SymbolIndexError::Unavailable(
                "position splits a UTF-16 character or is outside line",
            ));
        }
    };
    if !source.is_char_boundary(byte) {
        return Err(SymbolIndexError::Unavailable("position is not a UTF-8 boundary"));
    }

    let language = resolve_ts_language(Some(language_id), None)
        .ok_or(SymbolIndexError::Unavailable("Tree-sitter grammar is unavailable"))?;
    let started = Instant::now();
    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .map_err(|_| SymbolIndexError::Unavailable("Tree-sitter grammar is invalid"))?;
    let bytes = source.as_bytes();
    let mut progress = |_: &tree_sitter::ParseState| {
        if started.elapsed() >= PARSE_TIMEOUT {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        }
    };
    let mut read = |offset: usize, _: Point| bytes.get(offset..).unwrap_or_default();
    let options = ParseOptions { progress_callback: Some(&mut progress) };
    let tree = parser
        .parse_with_options(&mut read, None, Some(options))
        .ok_or(SymbolIndexError::Unavailable("Tree-sitter parse timed out"))?;
    if started.elapsed() >= PARSE_TIMEOUT {
        return Err(SymbolIndexError::Unavailable("Tree-sitter parse timed out"));
    }
    if store.snapshot_id() != revision {
        return Err(SymbolIndexError::Stale("buffer changed while dependency index was built"));
    }

    let root = tree.root_node();
    if root.has_error() {
        return Err(SymbolIndexError::Unavailable("Tree-sitter parse contains syntax errors"));
    }
    let symbol_node = identifier_at(root, byte, source.as_bytes())
        .ok_or(SymbolIndexError::Unavailable("no unambiguous symbol at position"))?;
    let name = node_text(symbol_node, source.as_bytes()).to_owned();
    let definition_node = find_definition(root, &name, source.as_bytes())
        .ok_or(SymbolIndexError::Unavailable("symbol definition is ambiguous or unavailable"))?;
    let definition_name = definition_node
        .child_by_field_name("name")
        .ok_or(SymbolIndexError::Unavailable("definition name is unavailable"))?;
    let definition = location(
        definition_name,
        &name,
        kind_for_definition(definition_node.kind()),
        &path,
        store,
    )?;

    let mut callers = Vec::new();
    let mut callees = Vec::new();
    let mut implementations = Vec::new();
    let mut tests = Vec::new();
    let mut modules = owning_module_hints(definition_node, source.as_bytes(), &path);
    walk(root, &mut |node| {
        if node.kind() == "call_expression"
            && called_name(node, source.as_bytes()) == Some(name.as_str())
        {
            if let Some(function) = enclosing_function(node) {
                let function_name = function.child_by_field_name("name").unwrap_or(function);
                if let Ok(location) = location(
                    function_name,
                    node_text(function_name, source.as_bytes()),
                    "function",
                    &path,
                    store,
                ) {
                    callers
                        .push(SymbolRelation { symbol: location, relation: String::from("calls") });
                }
            }
        }
        if node.kind() == "function_item"
            && node.start_byte() <= definition_node.start_byte()
            && definition_node.end_byte() <= node.end_byte()
        {
            collect_calls(node, source.as_bytes(), &path, store, &mut callees);
        }
        if node.kind() == "impl_item" {
            if let Some(method) = named_function_in(node, &name, source.as_bytes()) {
                let method_name = method.child_by_field_name("name").unwrap_or(method);
                if let Ok(location) = location(method_name, &name, "implementation", &path, store) {
                    implementations.push(SymbolRelation {
                        symbol: location,
                        relation: String::from("implements"),
                    });
                }
            }
        }
        if node.kind() == "function_item" && is_test(node, source.as_bytes()) {
            if let Some(test_name) = node.child_by_field_name("name") {
                if let Ok(location) = location(
                    test_name,
                    node_text(test_name, source.as_bytes()),
                    "test",
                    &path,
                    store,
                ) {
                    tests.push(location);
                }
            }
        }
    });

    let caller_total = dedupe_relations(&mut callers);
    let callee_total = dedupe_relations(&mut callees);
    let implementation_total = dedupe_relations(&mut implementations);
    let test_total = dedupe_locations(&mut tests);
    let module_total = dedupe_modules(&mut modules);
    let related_files =
        vec![RelatedFile { path: path.clone(), relation: String::from("definition") }];
    let related_total = related_files.len();
    callers.truncate(MAX_RELATIONS);
    callees.truncate(MAX_RELATIONS);
    implementations.truncate(MAX_RELATIONS);
    tests.truncate(MAX_RELATIONS);
    modules.truncate(MAX_RELATIONS);
    let totals = SymbolDependencyTotals {
        callers: caller_total as u32,
        callees: callee_total as u32,
        implementations: implementation_total as u32,
        tests: test_total as u32,
        module_hints: module_total as u32,
        related_files: related_total as u32,
        omitted_callers: caller_total.saturating_sub(callers.len()) as u32,
        omitted_callees: callee_total.saturating_sub(callees.len()) as u32,
        omitted_implementations: implementation_total.saturating_sub(implementations.len()) as u32,
        omitted_tests: test_total.saturating_sub(tests.len()) as u32,
        omitted_module_hints: module_total.saturating_sub(modules.len()) as u32,
        omitted_related_files: 0,
    };
    let truncated = [
        totals.omitted_callers,
        totals.omitted_callees,
        totals.omitted_implementations,
        totals.omitted_tests,
        totals.omitted_module_hints,
        totals.omitted_related_files,
    ]
    .iter()
    .any(|count| *count > 0);
    Ok(SymbolDependencyMap {
        path,
        line,
        character,
        symbol: definition.clone(),
        definition,
        callers,
        callees,
        implementations,
        tests,
        module_hints: modules,
        related_files,
        totals,
        truncated,
        freshness: String::from("fresh"),
        graph_version: format!("{GRAPH_VERSION}-{revision}"),
        indexed_at: None,
    })
}

fn identifier_at<'a>(node: Node<'a>, byte: usize, bytes: &[u8]) -> Option<Node<'a>> {
    if is_symbol_identifier(node.kind())
        && node.start_byte() <= byte
        && byte <= node.end_byte()
        && !node_text(node, bytes).is_empty()
    {
        return Some(node);
    }
    let mut cursor = node.walk();
    node.children(&mut cursor).find_map(|child| identifier_at(child, byte, bytes))
}

fn find_definition<'a>(root: Node<'a>, name: &str, bytes: &[u8]) -> Option<Node<'a>> {
    let mut definitions = Vec::new();
    collect_definitions(root, name, bytes, &mut definitions);
    (definitions.len() == 1).then(|| definitions[0])
}

fn collect_definitions<'a>(node: Node<'a>, name: &str, bytes: &[u8], output: &mut Vec<Node<'a>>) {
    if matches!(
        node.kind(),
        "function_item"
            | "struct_item"
            | "enum_item"
            | "trait_item"
            | "type_item"
            | "const_item"
            | "static_item"
    ) && node
        .child_by_field_name("name")
        .is_some_and(|candidate| node_text(candidate, bytes) == name)
    {
        output.push(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_definitions(child, name, bytes, output);
    }
}

fn location(
    node: Node<'_>,
    name: &str,
    kind: &str,
    path: &str,
    store: &dyn TextStore,
) -> Result<SymbolLocation, SymbolIndexError> {
    let point = node.start_position();
    let end = node.end_position();
    let start_line = u32::try_from(point.row + 1)
        .map_err(|_| SymbolIndexError::Unavailable("symbol position exceeds protocol range"))?;
    let end_line = u32::try_from(end.row + 1)
        .map_err(|_| SymbolIndexError::Unavailable("symbol position exceeds protocol range"))?;
    let start_byte = store.line_to_byte(LogicalLine(point.row as u64));
    let end_byte = store.line_to_byte(LogicalLine(end.row as u64));
    let crate::text_store::LineLookup::Exact(start_line_byte) = start_byte else {
        return Err(SymbolIndexError::Unavailable("symbol line is unavailable"));
    };
    let crate::text_store::LineLookup::Exact(end_line_byte) = end_byte else {
        return Err(SymbolIndexError::Unavailable("symbol line is unavailable"));
    };
    let start = store
        .byte_to_utf16(ByteOffset(start_line_byte.0 + point.column as u64))
        .ok_or(SymbolIndexError::Unavailable("symbol UTF-16 position unavailable"))?
        .0
        - store
            .byte_to_utf16(start_line_byte)
            .ok_or(SymbolIndexError::Unavailable("symbol UTF-16 position unavailable"))?
            .0;
    let finish = store
        .byte_to_utf16(ByteOffset(end_line_byte.0 + end.column as u64))
        .ok_or(SymbolIndexError::Unavailable("symbol UTF-16 position unavailable"))?
        .0
        - store
            .byte_to_utf16(end_line_byte)
            .ok_or(SymbolIndexError::Unavailable("symbol UTF-16 position unavailable"))?
            .0;
    Ok(SymbolLocation {
        name: name.to_owned(),
        kind: kind.to_owned(),
        path: path.to_owned(),
        line: start_line,
        character: start as u32,
        end_line,
        end_character: finish as u32,
    })
}

fn node_text<'a>(node: Node<'_>, bytes: &'a [u8]) -> &'a str {
    std::str::from_utf8(&bytes[node.start_byte()..node.end_byte()]).unwrap_or_default()
}
fn kind_for_definition(kind: &str) -> &'static str {
    match kind {
        "function_item" => "function",
        "struct_item" => "struct",
        "enum_item" => "enum",
        "trait_item" => "trait",
        "type_item" => "type",
        "const_item" => "constant",
        "static_item" => "static",
        _ => "symbol",
    }
}
fn walk(node: Node<'_>, f: &mut impl FnMut(Node<'_>)) {
    f(node);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, f);
    }
}
fn called_name<'a>(node: Node<'_>, bytes: &'a [u8]) -> Option<&'a str> {
    let function = node.child_by_field_name("function")?;
    (function.kind() == "identifier").then(|| node_text(function, bytes))
}
fn enclosing_function(node: Node<'_>) -> Option<Node<'_>> {
    let mut current = node.parent();
    while let Some(candidate) = current {
        if candidate.kind() == "function_item" {
            return Some(candidate);
        }
        current = candidate.parent();
    }
    None
}
fn is_symbol_identifier(kind: &str) -> bool {
    matches!(kind, "identifier" | "type_identifier" | "field_identifier")
}

fn owning_module_hints(node: Node<'_>, bytes: &[u8], path: &str) -> Vec<ModuleHint> {
    let mut modules = Vec::new();
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == "mod_item" {
            if let Some(name) = parent.child_by_field_name("name") {
                modules.push(ModuleHint {
                    name: node_text(name, bytes).to_owned(),
                    kind: String::from("module"),
                    path: path.to_owned(),
                });
            }
        }
        current = parent.parent();
    }
    modules.reverse();
    modules
}

fn named_function_in<'a>(node: Node<'a>, name: &str, bytes: &[u8]) -> Option<Node<'a>> {
    if node.kind() == "function_item"
        && node
            .child_by_field_name("name")
            .is_some_and(|candidate| node_text(candidate, bytes) == name)
    {
        return Some(node);
    }
    let mut cursor = node.walk();
    node.children(&mut cursor).find_map(|child| named_function_in(child, name, bytes))
}

fn is_test(node: Node<'_>, bytes: &[u8]) -> bool {
    node.child_by_field_name("name").is_some_and(|name| node_text(name, bytes).starts_with("test"))
        || node.prev_named_sibling().is_some_and(|attribute| {
            attribute.kind() == "attribute_item" && node_text(attribute, bytes).contains("test")
        })
}
fn collect_calls(
    node: Node<'_>,
    bytes: &[u8],
    path: &str,
    store: &dyn TextStore,
    output: &mut Vec<SymbolRelation>,
) {
    walk(node, &mut |child| {
        if child.kind() == "call_expression" {
            if let Some(name) = called_name(child, bytes) {
                let function = child.child_by_field_name("function").unwrap_or(child);
                if let Ok(symbol) = location(function, name, "function", path, store) {
                    output.push(SymbolRelation { symbol, relation: String::from("calls") });
                }
            }
        }
    });
}
fn dedupe_relations(values: &mut Vec<SymbolRelation>) -> usize {
    values.sort_by(|a, b| {
        (a.symbol.path.as_str(), a.symbol.line, a.symbol.character, a.symbol.name.as_str()).cmp(&(
            b.symbol.path.as_str(),
            b.symbol.line,
            b.symbol.character,
            b.symbol.name.as_str(),
        ))
    });
    values.dedup_by(|a, b| {
        a.symbol.path == b.symbol.path
            && a.symbol.line == b.symbol.line
            && a.symbol.character == b.symbol.character
            && a.symbol.name == b.symbol.name
    });
    values.len()
}
fn dedupe_locations(values: &mut Vec<SymbolLocation>) -> usize {
    values.sort_by(|a, b| {
        (a.path.as_str(), a.line, a.character).cmp(&(b.path.as_str(), b.line, b.character))
    });
    values.dedup_by(|a, b| a.path == b.path && a.line == b.line && a.character == b.character);
    values.len()
}
fn dedupe_modules(values: &mut Vec<ModuleHint>) -> usize {
    values.sort_by(|a, b| {
        (a.path.as_str(), a.name.as_str()).cmp(&(b.path.as_str(), b.name.as_str()))
    });
    values.dedup_by(|a, b| a.path == b.path && a.name == b.name);
    values.len()
}

#[cfg(test)]
mod tests {
    use xi_rope::Rope;

    use super::*;
    use crate::text_store::rope_store::RopeTextStore;

    fn map(
        source: &str,
        line: u32,
        character: u32,
    ) -> Result<SymbolDependencyMap, SymbolIndexError> {
        let store = RopeTextStore::new(Rope::from(source), 7);
        symbol_dependency_map(
            &store,
            store.snapshot_id(),
            String::from("/workspace/src/lib.rs"),
            line,
            character,
            "rust",
        )
    }

    #[test]
    fn resolves_definition_and_callers_from_current_snapshot() {
        let result = map("fn helper() {}\nfn caller() { helper(); }\n", 1, 3).expect("map");
        assert_eq!(result.definition.name, "helper");
        assert_eq!(result.definition.kind, "function");
        assert_eq!(result.callers.len(), 1);
        assert_eq!(result.callers[0].symbol.name, "caller");
        assert_eq!(result.freshness, "fresh");
    }

    #[test]
    fn resolves_type_identifiers_and_annotated_tests() {
        let result = map("struct Widget;\n#[test]\nfn verifies_widget() {}\n", 1, 7).expect("map");
        assert_eq!(result.definition.kind, "struct");
        assert_eq!(result.definition.character, 7);
        assert_eq!(result.tests.len(), 1);
        assert_eq!(result.tests[0].name, "verifies_widget");
    }

    #[test]
    fn returns_only_definition_ownership_modules() {
        let result = map("mod api {\n    struct Widget;\n}\nmod other {}\n", 2, 11).expect("map");
        assert_eq!(result.module_hints.len(), 1);
        assert_eq!(result.module_hints[0].name, "api");
    }

    #[test]
    fn rejects_syntax_errors() {
        let error = map("fn broken( {\n", 1, 3).expect_err("syntax error must fail closed");
        assert_eq!(error.code(), "dependency_index_unavailable");
    }

    #[test]
    fn rejects_non_boundary_utf16_position() {
        let error = map("fn 😀helper() {}\n", 1, 4).expect_err("surrogate split must fail");
        assert_eq!(error.code(), "dependency_index_unavailable");
    }

    #[test]
    fn rejects_ambiguous_definitions() {
        let error = map("fn same() {}\nfn same() {}\n", 1, 3).expect_err("ambiguous symbol");
        assert_eq!(error.code(), "dependency_index_unavailable");
    }
}
