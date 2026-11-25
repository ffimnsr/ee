use super::*;

use super::parse::{QueryWarmup, runtime_language};

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

pub(crate) fn fold_ranges_for_text(
    language_name: Option<&str>,
    file_path: Option<&Path>,
    text: &str,
    timeout: Duration,
) -> Vec<FoldRange> {
    if timeout.is_zero() || text.is_empty() {
        return Vec::new();
    }

    let Some(language) =
        runtime_language(language_name, file_path, QueryWarmup::Kind(RuntimeQueryKind::Folds))
    else {
        return Vec::new();
    };
    let started = Instant::now();
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }

    let mut progress = |_: &tree_sitter::ParseState| {
        if started.elapsed() >= timeout {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let bytes = text.as_bytes();
    let mut read = |offset: usize, _: Point| bytes.get(offset..).unwrap_or_default();
    let options = ParseOptions { progress_callback: Some(&mut progress) };
    let Some(tree) = parser.parse_with_options(&mut read, None, Some(options)) else {
        return Vec::new();
    };
    if started.elapsed() >= timeout {
        return Vec::new();
    }

    fold_ranges(&tree)
}

#[cfg(test)]
pub(super) fn byte_to_point(text: &str, byte_offset: usize) -> Point {
    let safe = byte_offset.min(text.len());
    let prefix = &text[..safe];
    let row = prefix.bytes().filter(|&b| b == b'\n').count();
    let col = prefix.rfind('\n').map(|idx| safe - idx - 1).unwrap_or(safe);
    Point::new(row, col)
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
