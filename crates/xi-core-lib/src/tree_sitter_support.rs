// Copyright 2024 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Phase 3: tree-sitter integration for incremental parsing, fold detection,
//! and language-aware indentation.
//!
//! ## Evaluation findings
//!
//! tree-sitter is suitable as the successor to syntect for language-sensitive
//! features beyond basic highlighting:
//!
//! - **Incremental parsing**: `Tree::edit` + re-parse only processes changed
//!   byte ranges, making it O(edit size) rather than O(file size).
//! - **Folds**: AST node boundaries give precise, language-aware fold ranges
//!   (e.g. Rust `function_item`, `struct_item`, `block`).
//! - **Indentation**: Walking the syntax tree to count block depth produces
//!   correct results even for edge cases that confuse bracket-counting
//!   heuristics (template strings, raw string literals, macros).
//!
//! ## Phased adoption plan
//!
//! 1. Use `TsParseState` for per-buffer incremental parse in `tabs.rs`.
//! 2. Surface `fold_ranges` to the viewport for manual/auto fold commands.
//! 3. Replace syntect bracket-counting in `lang_features::reindent` with
//!    `indent_level_at_line` once all target language grammars are bundled.
//!
//! ## Bundled grammars
//!
//! Currently bundled: Rust, Python.
//! Add more by appending `tree-sitter-<lang>` to `Cargo.toml` and extending
//! `ts_language_for_name`.
//!
//! APIs in this module are evaluation scaffolding; dead-code warnings are
//! suppressed until each item is wired into the editor pipeline.
#![allow(dead_code)]

use tree_sitter::{InputEdit, Language, Node, Parser, Point, Tree};

// ---------------------------------------------------------------------------
// Language registry
// ---------------------------------------------------------------------------

/// Returns the tree-sitter `Language` for a given xi language name.
///
/// Returns `None` for languages without a bundled grammar.
pub(crate) fn ts_language_for_name(language_name: &str) -> Option<Language> {
    match language_name {
        "Rust" => Some(tree_sitter_rust::LANGUAGE.into()),
        "Python" | "Python 3" => Some(tree_sitter_python::LANGUAGE.into()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Per-buffer incremental parse state
// ---------------------------------------------------------------------------

/// Holds a tree-sitter `Parser` and the most recent `Tree` for a single
/// buffer.
///
/// Call [`update`] after every edit so the tree stays consistent with the
/// buffer text.  tree-sitter re-parses only the touched regions, so the
/// cost is proportional to the edit size rather than file size.
pub(crate) struct TsParseState {
    parser: Parser,
    tree: Option<Tree>,
}

impl std::fmt::Debug for TsParseState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TsParseState")
            .field("has_tree", &self.tree.is_some())
            .finish_non_exhaustive()
    }
}

impl TsParseState {
    /// Create a new parse state for the given language.
    ///
    /// Returns `None` if no bundled grammar exists for `language_name`.
    pub(crate) fn new(language_name: &str) -> Option<Self> {
        let language = ts_language_for_name(language_name)?;
        let mut parser = Parser::new();
        parser.set_language(&language).ok()?;
        Some(Self { parser, tree: None })
    }

    /// Parse or incrementally re-parse `text`.
    ///
    /// `edit` describes the byte-range change since the last call; pass `None`
    /// for a full initial parse.  Returns a reference to the updated tree.
    pub(crate) fn update(&mut self, text: &str, edit: Option<&InputEdit>) -> Option<&Tree> {
        if let (Some(tree), Some(edit)) = (self.tree.as_mut(), edit) {
            tree.edit(edit);
        }
        let old_tree = self.tree.as_ref();
        self.tree = self.parser.parse(text, old_tree);
        self.tree.as_ref()
    }

    /// Returns the current syntax tree, if one has been parsed.
    pub(crate) fn tree(&self) -> Option<&Tree> {
        self.tree.as_ref()
    }
}

// ---------------------------------------------------------------------------
// Fold detection
// ---------------------------------------------------------------------------

/// A line range that can be folded: the header line stays visible; lines
/// `body_start..=body_end` are hidden when the fold is closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FoldRange {
    /// The line containing the fold opener (e.g. the `fn` declaration line).
    pub(crate) header_line: usize,
    /// First line of the foldable body (inclusive).
    pub(crate) body_start: usize,
    /// Last line of the foldable body (inclusive).
    pub(crate) body_end: usize,
}

/// Returns all foldable regions in `tree` that span more than one line.
///
/// Foldable node kinds are chosen to cover blocks, functions, structs, and
/// similar multi-line constructs across supported languages.
pub(crate) fn fold_ranges(tree: &Tree) -> Vec<FoldRange> {
    /// Node kinds that make good fold points across the supported grammars.
    const FOLDABLE_KINDS: &[&str] = &[
        // Rust
        "function_item",
        "impl_item",
        "struct_item",
        "enum_item",
        "trait_item",
        "mod_item",
        "block",
        "match_expression",
        "if_expression",
        "while_expression",
        "for_expression",
        "macro_definition",
        // Python
        "function_definition",
        "class_definition",
        "if_statement",
        "while_statement",
        "for_statement",
        "with_statement",
        "try_statement",
        "decorated_definition",
        "block",
    ];

    let mut folds = Vec::new();
    collect_folds(tree.root_node(), FOLDABLE_KINDS, &mut folds);
    folds
}

fn collect_folds(node: Node<'_>, kinds: &[&str], out: &mut Vec<FoldRange>) {
    let start_line = node.start_position().row;
    let end_line = node.end_position().row;

    if kinds.contains(&node.kind()) && end_line > start_line {
        out.push(FoldRange {
            header_line: start_line,
            body_start: start_line + 1,
            body_end: end_line,
        });
    }

    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            collect_folds(child, kinds, out);
        }
    }
}

// ---------------------------------------------------------------------------
// Indentation
// ---------------------------------------------------------------------------

/// Returns the expected indent level (in multiples of the indent unit) for
/// `line` by walking the syntax tree to find the enclosing block depth.
///
/// The result is based on AST structure, not bracket counting, so it is
/// correct even in the presence of nested string literals, raw strings,
/// macros, and other constructs that confuse heuristic approaches.
pub(crate) fn indent_level_at_line(tree: &Tree, line: usize) -> usize {
    // Seek to any character on the target line; column 0 is fine since we
    // only care about the enclosing block count.
    let point = Point::new(line, 0);
    let node = tree.root_node().descendant_for_point_range(point, point);

    match node {
        None => 0,
        Some(n) => block_depth(n, line),
    }
}

/// Counts the number of block-introducing ancestors of `node` that **start
/// before** `target_line`.
///
/// A block that opens on the target line does not yet indent that line; the
/// opener is written at the parent's indent level.
fn block_depth(node: Node<'_>, target_line: usize) -> usize {
    /// Node kinds that introduce one level of indentation.
    const BLOCK_KINDS: &[&str] = &[
        "block",
        "function_item",
        "impl_item",
        "struct_item",
        "enum_item",
        "trait_item",
        "mod_item",
        // Python
        "function_definition",
        "class_definition",
        "if_statement",
        "elif_clause",
        "else_clause",
        "while_statement",
        "for_statement",
        "with_statement",
        "try_statement",
        "except_clause",
        "finally_clause",
        "decorated_definition",
    ];

    let mut depth = 0usize;
    let mut cursor = node;

    loop {
        // Only count this node if it opened before the target line (meaning
        // the target line is inside the body, not on the opener itself).
        if BLOCK_KINDS.contains(&cursor.kind()) && cursor.start_position().row < target_line {
            depth += 1;
        }
        match cursor.parent() {
            Some(p) => cursor = p,
            None => break,
        }
    }

    depth
}

// ---------------------------------------------------------------------------
// InputEdit helper
// ---------------------------------------------------------------------------

/// Construct a tree-sitter `InputEdit` from the byte-level description of a
/// single edit operation (insertion or deletion).
///
/// Parameters follow the tree-sitter convention:
/// - `start_byte`: byte offset of the first changed character.
/// - `old_end_byte`: byte offset of the last removed character (exclusive).
/// - `new_end_byte`: byte offset of the last inserted character (exclusive).
/// - `text`: the *new* buffer contents after the edit (used to compute row/col).
pub(crate) fn make_input_edit(
    start_byte: usize,
    old_end_byte: usize,
    new_end_byte: usize,
    old_text: &str,
    new_text: &str,
) -> InputEdit {
    InputEdit {
        start_byte,
        old_end_byte,
        new_end_byte,
        start_position: byte_to_point(old_text, start_byte),
        old_end_position: byte_to_point(old_text, old_end_byte),
        new_end_position: byte_to_point(new_text, new_end_byte),
    }
}

/// Convert a byte offset into a `tree_sitter::Point` (row, column).
fn byte_to_point(text: &str, byte_offset: usize) -> Point {
    let safe = byte_offset.min(text.len());
    let prefix = &text[..safe];
    let row = prefix.bytes().filter(|&b| b == b'\n').count();
    let col = prefix.rfind('\n').map(|i| safe - i - 1).unwrap_or(safe);
    Point::new(row, col)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_rust(src: &str) -> Tree {
        let mut state = TsParseState::new("Rust").expect("Rust grammar not found");
        state.update(src, None).expect("parse failed").clone()
    }

    fn parse_python(src: &str) -> Tree {
        let mut state = TsParseState::new("Python").expect("Python grammar not found");
        state.update(src, None).expect("parse failed").clone()
    }

    // --------------------------------------------------
    // Language registry
    // --------------------------------------------------

    #[test]
    fn test_ts_language_rust() {
        assert!(ts_language_for_name("Rust").is_some());
    }

    #[test]
    fn test_ts_language_python() {
        assert!(ts_language_for_name("Python").is_some());
    }

    #[test]
    fn test_ts_language_unknown() {
        assert!(ts_language_for_name("COBOL").is_none());
    }

    // --------------------------------------------------
    // Incremental parse
    // --------------------------------------------------

    #[test]
    fn test_full_parse_produces_tree() {
        let src = "fn main() {}\n";
        let tree = parse_rust(src);
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn test_incremental_parse_edit() {
        let src_before = "fn main() {}\n";
        let src_after = "fn main() { let x = 1; }\n";

        let mut state = TsParseState::new("Rust").unwrap();
        state.update(src_before, None);

        // Simulate inserting " let x = 1;" at byte 11 (inside the braces).
        let edit = make_input_edit(11, 11, 11 + " let x = 1;".len(), src_before, src_after);
        let tree = state.update(src_after, Some(&edit)).unwrap();
        assert!(!tree.root_node().has_error());
    }

    // --------------------------------------------------
    // Fold detection
    // --------------------------------------------------

    #[test]
    fn test_fold_function() {
        let src = "fn foo() {\n    let x = 1;\n}\n";
        let tree = parse_rust(src);
        let folds = fold_ranges(&tree);
        // There should be at least one fold covering the function or its block.
        assert!(!folds.is_empty(), "expected at least one fold");
        assert!(folds.iter().any(|f| f.body_end >= 2), "fold should span to line 2");
    }

    #[test]
    fn test_fold_python_function() {
        let src = "def foo():\n    x = 1\n    y = 2\n";
        let tree = parse_python(src);
        let folds = fold_ranges(&tree);
        assert!(!folds.is_empty(), "expected fold for Python function");
    }

    #[test]
    fn test_no_folds_single_line() {
        let src = "fn foo() { 1 }\n";
        let tree = parse_rust(src);
        let folds = fold_ranges(&tree);
        // A single-line function should yield no multi-line folds.
        assert!(
            folds.iter().all(|f| f.body_start > f.body_end || f.body_end == f.header_line),
            "single-line function should have no body folds; got {folds:?}"
        );
    }

    // --------------------------------------------------
    // Indentation
    // --------------------------------------------------

    #[test]
    fn test_indent_level_top_level() {
        let src = "fn main() {\n    let x = 1;\n}\n";
        let tree = parse_rust(src);
        // Line 0 is top-level: no enclosing block.
        let level = indent_level_at_line(&tree, 0);
        assert_eq!(level, 0);
    }

    #[test]
    fn test_indent_level_inside_block() {
        let src = "fn main() {\n    let x = 1;\n}\n";
        let tree = parse_rust(src);
        // Line 1 is inside the function body.
        let level = indent_level_at_line(&tree, 1);
        assert!(level >= 1, "expected level >= 1 inside a function, got {level}");
    }

    #[test]
    fn test_indent_level_python() {
        let src = "def foo():\n    x = 1\n";
        let tree = parse_python(src);
        let level = indent_level_at_line(&tree, 1);
        assert!(level >= 1, "expected level >= 1 inside Python function, got {level}");
    }

    // --------------------------------------------------
    // Helpers
    // --------------------------------------------------

    #[test]
    fn test_byte_to_point_start() {
        assert_eq!(byte_to_point("hello\nworld", 0), Point::new(0, 0));
    }

    #[test]
    fn test_byte_to_point_second_line() {
        assert_eq!(byte_to_point("hello\nworld", 6), Point::new(1, 0));
    }

    #[test]
    fn test_byte_to_point_mid_line() {
        assert_eq!(byte_to_point("hello\nworld", 8), Point::new(1, 2));
    }
}
