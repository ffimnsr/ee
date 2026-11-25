use super::*;

use super::parse::{QueryWarmup, parse_tree_with_timeout_and_queries};
use super::visible::line_segments;
use super::visible::line_start_offsets;

// ---------------------------------------------------------------------------
// Indentation
// ---------------------------------------------------------------------------

pub(crate) fn indentation_levels_for_text(
    language_name: &str,
    text: &str,
    max_line: usize,
) -> Option<Vec<usize>> {
    // Whole-buffer parses share the scaled fold budget: a flat sub-250ms
    // wall-clock budget flakes on shared CI runners, where a descheduled
    // test thread can blow the budget before parsing a tiny buffer.
    indentation_levels_for_text_with_timeout(
        language_name,
        text,
        max_line,
        fold_parse_timeout(text.len()),
    )
}

pub(super) fn indentation_levels_for_text_with_timeout(
    language_name: &str,
    text: &str,
    max_line: usize,
    timeout: Duration,
) -> Option<Vec<usize>> {
    let metadata = language_metadata_for_name(language_name)?;
    if metadata.indentation != IndentationStrategy::TreeSitter {
        return None;
    }

    if canonical_language_name(language_name).as_deref() == Some("python") {
        return Some(python_indentation_levels(text, max_line));
    }

    let tree = parse_tree_with_timeout_and_queries(
        language_name,
        text,
        timeout,
        QueryWarmup::Kind(RuntimeQueryKind::Indents),
    )?;
    let line_starts = line_start_offsets(text);
    let line_segments = line_segments(text, &line_starts);
    let mut levels = Vec::with_capacity(max_line + 1);

    for line in 0..=max_line {
        let line_text = line_segments.get(line).map(|segment| &text[segment.clone()]).unwrap_or("");
        let mut level = indent_level_at_line(&tree, line);
        if line_requires_dedent(language_name, line_text) {
            level = level.saturating_sub(1);
        }
        levels.push(level);
    }

    Some(levels)
}

fn python_indentation_levels(text: &str, max_line: usize) -> Vec<usize> {
    let line_starts = line_start_offsets(text);
    let line_segments = line_segments(text, &line_starts);
    let mut levels = Vec::with_capacity(max_line + 1);
    let mut current_level = 0usize;

    for line in 0..=max_line {
        let line_text = line_segments.get(line).map(|segment| &text[segment.clone()]).unwrap_or("");
        let trimmed = line_text.trim_start();
        let is_dedent_clause = trimmed.starts_with("elif ")
            || trimmed.starts_with("else:")
            || trimmed.starts_with("except")
            || trimmed.starts_with("finally:");

        if is_dedent_clause {
            current_level = current_level.saturating_sub(1);
        }

        let line_level = current_level;
        levels.push(line_level);

        if !trimmed.is_empty() && trimmed.ends_with(':') {
            current_level = line_level + 1;
        }
    }

    levels
}

/// Returns brace/block-style indent depth for `line` by walking the syntax
/// tree to find enclosing block containers.
///
/// Prefer [`indentation_levels_for_text`] for editor-facing reindent because it
/// also applies language-specific rules such as Python clause dedents.
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
        "statement_block",
        "compound_statement",
        "class_body",
        "declaration_list",
        "field_declaration_list",
        "enum_variant_list",
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

fn line_requires_dedent(language_name: &str, line_text: &str) -> bool {
    let trimmed = line_text.trim_start();
    if trimmed.starts_with('}') || trimmed.starts_with(']') || trimmed.starts_with(')') {
        return true;
    }

    canonical_language_name(language_name).as_deref() == Some("python")
        && (trimmed.starts_with("elif ")
            || trimmed.starts_with("else:")
            || trimmed.starts_with("except")
            || trimmed.starts_with("finally:"))
}
