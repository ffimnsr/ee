use std::ops::ControlFlow;
use std::path::Path;
#[cfg(test)]
use std::sync::MutexGuard;
use std::time::{Duration, Instant};

use tree_sitter::{
    Node, ParseOptions, Parser, Point, Query, QueryCursor, QueryPredicateArg, StreamingIterator,
    Tree,
};
use xi_rope::Rope;

use crate::runtime_loader::{IndentQueryCapture, with_default_runtime_loader_mut};
use crate::selection::SelRegion;
use crate::text_store::DocumentMode;
use crate::tree_sitter_support::resolve_ts_language;

pub(crate) const DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IndentOutcome {
    Inherit,
    IndentOneLevel,
    DedentOneLevel,
    /// Absolute alignment to a column (from an `@align`/`@anchor` match).
    AlignTo(usize),
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SyntaxIndentContext<'a> {
    pub(crate) language_name: &'a str,
    pub(crate) file_path: Option<&'a Path>,
    pub(crate) document_mode: DocumentMode,
}

impl<'a> SyntaxIndentContext<'a> {
    pub(crate) fn new(
        language_name: &'a str,
        file_path: Option<&'a Path>,
        document_mode: DocumentMode,
    ) -> Self {
        Self { language_name, file_path, document_mode }
    }
}

pub(crate) fn syntax_indent_outcome(
    text: &Rope,
    region: &SelRegion,
    context: &SyntaxIndentContext<'_>,
) -> Option<IndentOutcome> {
    if context.language_name.is_empty() || !context.document_mode.feature_gates().whole_doc_ops {
        return None;
    }

    let anchor = region.min().min(text.len());
    let end = region.max().min(text.len());
    if text.line_of_offset(anchor) != text.line_of_offset(end) {
        return None;
    }

    let snapshot = text.slice_to_cow(0..text.len()).into_owned();
    syntax_indent_outcome_for_text(
        context.language_name,
        context.file_path,
        &snapshot,
        anchor,
        DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
    )
}

fn syntax_indent_outcome_for_text(
    language_name: &str,
    file_path: Option<&Path>,
    text: &str,
    anchor: usize,
    timeout: Duration,
) -> Option<IndentOutcome> {
    let compiled =
        with_default_runtime_loader_mut(|loader| loader.compile_indent_query(language_name).ok())
            .flatten()?;
    let tree = parse_tree_with_timeout(language_name, file_path, text, timeout)?;
    let anchor = anchor.min(text.len());
    let anchor_line = line_of_offset(text, anchor);
    let line_start = line_start_offset(text, anchor_line);
    let line_end = line_end_offset(text, line_start);
    let line_text = &text[line_start..line_end];
    let anchor_in_line = anchor.saturating_sub(line_start).min(line_text.len());
    let indent_end = line_text
        .char_indices()
        .find_map(|(idx, ch)| (!ch.is_whitespace()).then_some(idx))
        .unwrap_or(line_text.len());
    let dedent_allowed = anchor_in_line <= indent_end;

    let (indent, dedent, align) =
        query_indent_signals(&compiled.query, &tree, text, anchor, anchor_line, dedent_allowed);

    if indent {
        Some(IndentOutcome::IndentOneLevel)
    } else if dedent {
        Some(IndentOutcome::DedentOneLevel)
    } else if let Some(column) = align {
        Some(IndentOutcome::AlignTo(column))
    } else {
        Some(IndentOutcome::Inherit)
    }
}

// Evaluate the user-defined predicates shared with Helix/nvim indent queries:
// `#one-line?`/`#not-one-line?` and `#same-line?`/`#not-same-line?`. A failed
// predicate rejects the whole match (its indent *and* dedent captures). Unknown
// operators and malformed or absent arguments leave the match intact, matching
// Helix's lenient handling.
fn match_predicates_satisfied(
    query: &Query,
    query_match: &tree_sitter::QueryMatch<'_, '_>,
) -> bool {
    for predicate in query.general_predicates(query_match.pattern_index) {
        // The `#` prefix is consumed by the C parser, so operators arrive as
        // e.g. `not-same-line?`.
        match predicate.operator.as_ref() {
            "one-line?" | "not-one-line?" => {
                let Some(QueryPredicateArg::Capture(capture_id)) = predicate.args.first() else {
                    continue;
                };
                let Some(node) = capture_node_for_id(query_match, *capture_id) else {
                    continue;
                };
                // `#not-one-line?`: reject single-line nodes so single-line
                // yaml sequence items (`- a`) never indent.
                let one_line = node.start_position().row == node.end_position().row;
                if one_line == (predicate.operator.as_ref() == "not-one-line?") {
                    return false;
                }
            }
            "same-line?" | "not-same-line?" => {
                let mut args = predicate.args.iter().filter_map(|arg| match arg {
                    QueryPredicateArg::Capture(id) => Some(*id),
                    QueryPredicateArg::String(_) => None,
                });
                let (Some(first), Some(second)) = (args.next(), args.next()) else {
                    continue;
                };
                let (Some(node1), Some(node2)) = (
                    capture_node_for_id(query_match, first),
                    capture_node_for_id(query_match, second),
                ) else {
                    continue;
                };
                // `#not-same-line?`: reject pairs whose key and value start on
                // the same line so `foo: bar` never indents while `foo:` with a
                // block beneath does.
                let same_line = node1.start_position().row == node2.start_position().row;
                if same_line == (predicate.operator.as_ref() == "not-same-line?") {
                    return false;
                }
            }
            _ => {
                // Unknown user-defined predicate: does not gate the match.
            }
        }
    }
    true
}

fn capture_node_for_id<'tree>(
    query_match: &tree_sitter::QueryMatch<'_, 'tree>,
    capture_id: u32,
) -> Option<Node<'tree>> {
    query_match
        .captures
        .iter()
        .find(|capture| capture.index == capture_id)
        .map(|capture| capture.node)
}

fn query_indent_signals(
    query: &Query,
    tree: &Tree,
    text: &str,
    anchor: usize,
    anchor_line: usize,
    dedent_allowed: bool,
) -> (bool, bool, Option<usize>) {
    let bytes = text.as_bytes();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), bytes);
    let capture_names = query.capture_names();
    let mut indent = false;
    let mut dedent = false;
    let mut align = None;
    // (start_byte, end_byte, start_row, end_row) of every `@opaque` node.
    let mut opaque_ranges: Vec<(usize, usize, usize, usize)> = Vec::new();

    loop {
        matches.advance();
        let Some(query_match) = matches.get() else {
            break;
        };
        if !match_predicates_satisfied(query, query_match) {
            continue;
        }
        // Resolve the match's `@anchor` first so an `@align` in the same match
        // sees it regardless of capture order.
        let match_anchor = query_match.captures.iter().find_map(|capture| {
            let is_anchor = capture_names
                .get(capture.index as usize)
                .and_then(|name| IndentQueryCapture::from_capture_name(name))
                == Some(IndentQueryCapture::Anchor);
            is_anchor.then_some(capture.node)
        });
        for capture in query_match.captures.iter().copied() {
            let Some(kind) = capture_names
                .get(capture.index as usize)
                .and_then(|name| IndentQueryCapture::from_capture_name(name))
            else {
                continue;
            };

            match kind {
                // `@indent.always` uses the same opening-line gate as `@indent`:
                // the delta model computes one level relative to the caret's
                // line, and a node opens a scope exactly on the line it starts
                // on (see `IndentQueryCapture` docs).
                IndentQueryCapture::Indent | IndentQueryCapture::IndentAlways => {
                    if indent_capture_applies(capture.node, anchor, anchor_line) {
                        indent = true;
                    }
                }
                // `@outdent`/`@outdent.always` are Helix spellings of the same
                // token-style dedent.
                IndentQueryCapture::Dedent
                | IndentQueryCapture::Outdent
                | IndentQueryCapture::OutdentAlways => {
                    if dedent_allowed && dedent_capture_applies(capture.node, anchor, anchor_line) {
                        dedent = true;
                    }
                }
                IndentQueryCapture::Extend
                | IndentQueryCapture::ExtendPreventOnce
                | IndentQueryCapture::Anchor => {
                    // Accepted for Helix query compatibility: `@extend` needs
                    // no repositioning in this caret-gated engine (and
                    // `@extend.prevent-once` stops no propagation of it) while
                    // `@anchor` only feeds `@align` in the same match
                    // (resolved above).
                }
                IndentQueryCapture::Align => {
                    if let Some(anchor_node) = match_anchor {
                        if anchor_node.start_position().row == anchor_line && align.is_none() {
                            align = Some(anchor_node.start_position().column);
                        }
                    }
                }
                IndentQueryCapture::Opaque => {
                    opaque_ranges.push((
                        capture.node.start_byte(),
                        capture.node.end_byte(),
                        capture.node.start_position().row,
                        capture.node.end_position().row,
                    ));
                }
            }
        }
        // No early exit on `indent`: a later match may still carry an `@opaque`
        // range containing the caret, which suppresses every signal below.
    }

    // Lines strictly inside an `@opaque` node are literal content (strings,
    // heredocs, block comments): suppress every signal there.
    if opaque_ranges.iter().any(|&(start_byte, end_byte, start_row, end_row)| {
        start_byte <= anchor
            && anchor < end_byte
            && start_row < anchor_line
            && anchor_line <= end_row
    }) {
        return (false, false, None);
    }

    (indent, dedent, align)
}

fn indent_capture_applies(node: Node<'_>, anchor: usize, anchor_line: usize) -> bool {
    node.start_position().row == anchor_line
        && anchor <= node.end_byte()
        && node.end_position().row >= anchor_line
}

fn dedent_capture_applies(node: Node<'_>, _anchor: usize, anchor_line: usize) -> bool {
    node.start_position().row == anchor_line && node.end_position().row >= anchor_line
}

fn parse_tree_with_timeout(
    language_name: &str,
    file_path: Option<&Path>,
    text: &str,
    timeout: Duration,
) -> Option<Tree> {
    if timeout.is_zero() {
        return None;
    }

    let language = resolve_ts_language(Some(language_name), file_path)?;
    let started = Instant::now();
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;

    let mut progress = |_: &tree_sitter::ParseState| {
        if started.elapsed() > timeout { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
    };
    let bytes = text.as_bytes();
    let mut read = |offset: usize, _: Point| bytes.get(offset..).unwrap_or_default();
    let options = ParseOptions { progress_callback: Some(&mut progress) };
    let tree = parser.parse_with_options(&mut read, None, Some(options))?;
    (started.elapsed() <= timeout).then_some(tree)
}

fn line_of_offset(text: &str, offset: usize) -> usize {
    text.as_bytes()[..offset.min(text.len())].iter().filter(|&&byte| byte == b'\n').count()
}

fn line_start_offset(text: &str, target_line: usize) -> usize {
    if target_line == 0 {
        return 0;
    }

    let mut line = 0usize;
    for (idx, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            line += 1;
            if line == target_line {
                return idx + 1;
            }
        }
    }
    text.len()
}

fn line_end_offset(text: &str, line_start: usize) -> usize {
    text[line_start.min(text.len())..]
        .find('\n')
        .map(|offset| line_start + offset)
        .unwrap_or(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_loader::{
        RuntimeLanguageConfig, RuntimeLanguageOverrides, RuntimeQueryKind,
        configure_default_runtime_loader_overrides,
        ensure_default_runtime_loader_has_test_grammars, with_default_runtime_loader_mut,
    };
    use std::collections::BTreeSet;

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
                        supported_query_kinds: Some(BTreeSet::from([RuntimeQueryKind::Indents])),
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
                for language in ["rust", "json", "python", "yaml"] {
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
                RuntimeQueryKind::Indents,
                query.to_string(),
                Vec::new(),
                Vec::new(),
            );
        });
    }

    const BUNDLED_YAML_INDENTS_QUERY: &str =
        include_str!("../../../runtime/queries/yaml/indents.scm");
    const BUNDLED_PYTHON_INDENTS_QUERY: &str =
        include_str!("../../../runtime/queries/python/indents.scm");

    #[test]
    fn yaml_indent_outcome_indents_after_key_without_value() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["yaml"]);
        install_indent_query("yaml", BUNDLED_YAML_INDENTS_QUERY);

        // `a:` (value vacant) must indent; `- a` single-line must not.
        let text = "a:\n";
        let anchor = text.find(':').expect("find colon") + 1;
        let outcome = syntax_indent_outcome_for_text(
            "yaml",
            None,
            text,
            anchor,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::IndentOneLevel));
    }

    #[test]
    fn yaml_indent_outcome_does_not_indent_single_line_pair() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["yaml"]);
        install_indent_query("yaml", BUNDLED_YAML_INDENTS_QUERY);

        // `key`/`value` on the same line fails `#not-same-line?`.
        let text = "a: b\n";
        let anchor = text.find('b').expect("find value") + 1;
        let outcome = syntax_indent_outcome_for_text(
            "yaml",
            None,
            text,
            anchor,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::Inherit));
    }

    #[test]
    fn yaml_indent_outcome_indents_on_multiline_pair_key_line() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["yaml"]);
        install_indent_query("yaml", BUNDLED_YAML_INDENTS_QUERY);

        let text = "a:\n  b: c\n";
        let anchor = text.find(':').expect("find colon") + 1;
        let outcome = syntax_indent_outcome_for_text(
            "yaml",
            None,
            text,
            anchor,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::IndentOneLevel));
    }

    #[test]
    fn yaml_indent_outcome_keeps_indent_inside_multiline_pair() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["yaml"]);
        install_indent_query("yaml", BUNDLED_YAML_INDENTS_QUERY);

        // A new line inside the pair's body (after `b: c`) inherits the line's
        // own level; `@indent.always` must not over-indent it.
        let text = "a:\n  b: c\n";
        let anchor = text.find('c').expect("find value") + 1;
        let outcome = syntax_indent_outcome_for_text(
            "yaml",
            None,
            text,
            anchor,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::Inherit));
    }

    #[test]
    fn yaml_indent_outcome_does_not_indent_single_line_sequence_item() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["yaml"]);
        install_indent_query("yaml", BUNDLED_YAML_INDENTS_QUERY);

        // `- a` fits on one line, so `#not-one-line?` rejects the match.
        let text = "- a\n";
        let anchor = text.find('a').expect("find item") + 1;
        let outcome = syntax_indent_outcome_for_text(
            "yaml",
            None,
            text,
            anchor,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::Inherit));
    }

    #[test]
    fn yaml_indent_outcome_indents_on_multiline_sequence_item_key_line() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["yaml"]);
        install_indent_query("yaml", BUNDLED_YAML_INDENTS_QUERY);

        let text = "- a:\n  b: c\n";
        let anchor = text.find(':').expect("find colon") + 1;
        let outcome = syntax_indent_outcome_for_text(
            "yaml",
            None,
            text,
            anchor,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::IndentOneLevel));
    }

    #[test]
    fn yaml_indent_outcome_indents_after_anchored_pair_value() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["yaml"]);
        install_indent_query("yaml", BUNDLED_YAML_INDENTS_QUERY);

        // `foo: &anchor` with a block beneath matches the anchor/tag rule.
        let text = "a: &x\n  b: 1\n";
        let anchor = text.find('x').expect("find anchor") + 1;
        let outcome = syntax_indent_outcome_for_text(
            "yaml",
            None,
            text,
            anchor,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::IndentOneLevel));
    }

    #[test]
    fn yaml_indent_outcome_indents_after_tagged_pair_value() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["yaml"]);
        install_indent_query("yaml", BUNDLED_YAML_INDENTS_QUERY);

        let text = "a: !!str\n  b: 1\n";
        let anchor = text.find('r').expect("find tag end") + 1;
        let outcome = syntax_indent_outcome_for_text(
            "yaml",
            None,
            text,
            anchor,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::IndentOneLevel));
    }

    #[test]
    fn yaml_indent_outcome_indents_after_block_scalar_opener() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["yaml"]);
        install_indent_query("yaml", BUNDLED_YAML_INDENTS_QUERY);

        let text = "a: |\n  x\n";
        let anchor = text.find('|').expect("find pipe") + 1;
        let outcome = syntax_indent_outcome_for_text(
            "yaml",
            None,
            text,
            anchor,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::IndentOneLevel));
    }

    #[test]
    fn yaml_indent_outcome_keeps_block_scalar_content_indent() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["yaml"]);
        install_indent_query("yaml", BUNDLED_YAML_INDENTS_QUERY);

        // Lines inside the scalar are literal content: a newline there inherits
        // the line's current indent.
        let text = "a: |\n  x\n";
        let anchor = text.find('x').expect("find content") + 1;
        let outcome = syntax_indent_outcome_for_text(
            "yaml",
            None,
            text,
            anchor,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::Inherit));
    }

    #[test]
    fn syntax_indent_outcome_uses_outdent_capture_before_closer() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["rust"]);
        install_indent_query("rust", "(_) @outdent");

        let text = "    }";
        let outcome = syntax_indent_outcome_for_text(
            "rust",
            None,
            text,
            4,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::DedentOneLevel));
    }

    #[test]
    fn syntax_indent_outcome_uses_outdent_always_capture_before_closer() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["rust"]);
        install_indent_query("rust", "(_) @outdent.always");

        let text = "    }";
        let outcome = syntax_indent_outcome_for_text(
            "rust",
            None,
            text,
            4,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::DedentOneLevel));
    }

    #[test]
    fn syntax_indent_outcome_same_line_predicate_passes_for_single_line_pair() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["yaml"]);
        install_indent_query(
            "yaml",
            "((block_mapping_pair key: (_) @key value: (_) @val (#same-line? @key @val)) @indent)",
        );

        // Positive form: key and value on the same line satisfy `#same-line?`.
        let text = "a: b\n";
        let anchor = text.find('b').expect("find value") + 1;
        let outcome = syntax_indent_outcome_for_text(
            "yaml",
            None,
            text,
            anchor,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::IndentOneLevel));
    }

    #[test]
    fn syntax_indent_outcome_one_line_predicate_passes_for_single_line_item() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["yaml"]);
        install_indent_query("yaml", "((block_sequence_item) @indent (#one-line? @indent))");

        // Positive form: a single-line item satisfies `#one-line?`.
        let text = "- a\n";
        let anchor = text.find('a').expect("find item") + 1;
        let outcome = syntax_indent_outcome_for_text(
            "yaml",
            None,
            text,
            anchor,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::IndentOneLevel));

        // Control: the same query must reject the multi-line variant.
        let text = "- a:\n  b: c\n";
        let anchor = text.find(':').expect("find colon") + 1;
        let outcome = syntax_indent_outcome_for_text(
            "yaml",
            None,
            text,
            anchor,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::Inherit));
    }

    #[test]
    fn syntax_indent_outcome_error_node_capture_indents_opening_line() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["rust"]);
        install_indent_query("rust", "(ERROR) @indent");

        // `foo(` does not recover in the rust grammar: the whole span becomes
        // one ERROR node starting on the caret line, so `@indent` fires.
        let text = "  foo(";
        let anchor = text.len();
        let outcome = syntax_indent_outcome_for_text(
            "rust",
            None,
            text,
            anchor,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::IndentOneLevel));
    }

    #[test]
    fn python_indent_outcome_error_recovery_rule_indents_incomplete_def() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["python"]);
        install_indent_query("python", BUNDLED_PYTHON_INDENTS_QUERY);

        // Mid-typing `def foo(` the grammar folds everything into an ERROR
        // node; the `(ERROR . "def") @indent @extend` recovery rule keeps the
        // newline indenting.
        let text = "def foo(";
        let anchor = text.len();
        let outcome = syntax_indent_outcome_for_text(
            "python",
            None,
            text,
            anchor,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::IndentOneLevel));
    }

    #[test]
    fn python_indent_outcome_regular_rules_still_fire_with_recovery_query() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["python"]);
        install_indent_query("python", BUNDLED_PYTHON_INDENTS_QUERY);

        // Regression: the added recovery rules must not disturb the normal
        // `@indent` path on a well-formed construct.
        let text = "if ok:\n    pass\n";
        let anchor = text.find(':').expect("find colon") + 1;
        let outcome = syntax_indent_outcome_for_text(
            "python",
            None,
            text,
            anchor,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::IndentOneLevel));
    }

    #[test]
    fn syntax_indent_outcome_aligns_to_anchor_column() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["rust"]);
        install_indent_query("rust", "(call_expression (identifier) @anchor) @align");

        let text = "  foo()";
        let anchor = text.len(); // caret at end of the line
        let outcome = syntax_indent_outcome_for_text(
            "rust",
            None,
            text,
            anchor,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::AlignTo(2)));
    }

    #[test]
    fn syntax_indent_outcome_ignores_align_without_anchor() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["rust"]);
        install_indent_query("rust", "(call_expression) @align");

        let text = "foo()";
        let anchor = text.len();
        let outcome = syntax_indent_outcome_for_text(
            "rust",
            None,
            text,
            anchor,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::Inherit));
    }

    #[test]
    fn syntax_indent_outcome_indents_inside_block_without_opaque() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["rust"]);
        install_indent_query("rust", "(_) @indent");

        // The `a` node starts on the caret line, so plain `@indent` fires.
        let text = "{\n  a\n}\n";
        let anchor = text.find('a').expect("find node") + 1;
        let outcome = syntax_indent_outcome_for_text(
            "rust",
            None,
            text,
            anchor,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::IndentOneLevel));
    }

    #[test]
    fn syntax_indent_outcome_opaque_suppresses_signals_inside_block() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["rust"]);
        install_indent_query("rust", "(block) @opaque\n(_) @indent");

        // Same node layout as above, but the caret line lies strictly inside
        // the opaque `block`: all signals are suppressed.
        let text = "{\n  a\n}\n";
        let anchor = text.find('a').expect("find node") + 1;
        let outcome = syntax_indent_outcome_for_text(
            "rust",
            None,
            text,
            anchor,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::Inherit));
    }

    #[test]
    fn syntax_indent_outcome_opaque_does_not_suppress_opening_line() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["rust"]);
        install_indent_query("rust", "(block) @opaque\n(_) @indent");

        // The caret sits on the block's own first line: not opaque-interior,
        // so `@indent` still fires.
        let text = "{}\n";
        let anchor = 1;
        let outcome = syntax_indent_outcome_for_text(
            "rust",
            None,
            text,
            anchor,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::IndentOneLevel));
    }

    #[test]
    fn syntax_indent_outcome_uses_indent_capture_for_block_open_line() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["rust"]);
        install_indent_query("rust", "(_) @indent");

        let text = "fn main() {}";
        let anchor = text.find('{').expect("find opener") + 1;
        let outcome = syntax_indent_outcome_for_text(
            "rust",
            None,
            text,
            anchor,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::IndentOneLevel));
    }

    #[test]
    fn syntax_indent_outcome_uses_dedent_capture_before_closer() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["rust"]);
        install_indent_query("rust", "(_) @dedent");

        let text = "    }";
        let outcome = syntax_indent_outcome_for_text(
            "rust",
            None,
            text,
            4,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::DedentOneLevel));
    }

    #[test]
    fn syntax_indent_outcome_returns_none_when_mode_disables_whole_doc_syntax_work() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["rust"]);
        install_indent_query("rust", "(_) @indent");

        let text: Rope = "fn main() {}".into();
        let region = SelRegion::caret(10);
        let context = SyntaxIndentContext::new("rust", None, DocumentMode::ConstrainedNormal);

        assert_eq!(syntax_indent_outcome(&text, &region, &context), None);
    }

    #[test]
    fn syntax_indent_outcome_returns_none_when_query_missing() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install_with_query_language(
            &["rust"],
            Some("rust-missing-indent-query"),
        );
        with_default_runtime_loader_mut(|loader| loader.invalidate_language("rust"));

        let text = "fn main() {}";
        let anchor = text.find('{').expect("find opener") + 1;

        assert_eq!(
            syntax_indent_outcome_for_text(
                "rust",
                None,
                text,
                anchor,
                DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
            ),
            None
        );
    }

    #[test]
    fn syntax_indent_outcome_supports_json_indent_query() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["json"]);
        install_indent_query("json", "(_) @indent");

        let text = "{}";
        let anchor = 1;
        let outcome = syntax_indent_outcome_for_text(
            "json",
            None,
            text,
            anchor,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::IndentOneLevel));
    }

    #[test]
    fn syntax_indent_outcome_supports_python_indent_query_when_semantics_ready() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["python"]);
        install_indent_query("python", "(_) @indent");

        let text = "if ok:\n    pass\n";
        let anchor = text.find(':').expect("find colon") + 1;
        let outcome = syntax_indent_outcome_for_text(
            "python",
            None,
            text,
            anchor,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::IndentOneLevel));
    }
}
